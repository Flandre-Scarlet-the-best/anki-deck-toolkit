//! Контракт общего пути Forvo и релизного конвейера адаптера на настоящих
//! бинарях: выбор транспорта по конфигурации, отказ без тайного fallback,
//! продолжение пакета по сохранённой привязке, воспроизводимый ZIP без
//! `pairing.json`, подпись, проверка и офлайн-проверка версии из каталога.
//! Браузер не запускается: отказ CDP моделируется исполняемым файлом
//! `/bin/false` вместо Chromium, адаптер — отсутствующим сопряжением.
use asset_store::forvo::batch as web;
use asset_store::forvo::brave::{package, release};
use asset_store::temp_workspace::TempWorkspace;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    ws: TempWorkspace,
}

impl Env {
    fn new(name: &str) -> Self {
        let ws = TempWorkspace::create(name).unwrap();
        std::fs::create_dir_all(ws.path().join("xdg")).unwrap();
        Self { ws }
    }
    fn path(&self) -> &Path {
        self.ws.path()
    }
    fn profile(&self) -> PathBuf {
        self.path().join("cdp-profile")
    }
    fn adapter(&self) -> PathBuf {
        self.path().join("adapter")
    }
    fn config(&self, text: &str) -> PathBuf {
        let path = self.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }
    /// `forvo` общего пути с изолированными конфигурацией, профилем и адаптером.
    fn forvo(&self, generic: &[&str], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
            .arg("--repository-root")
            .arg(self.path())
            .arg("--store")
            .arg(self.path().join("store"))
            .arg("forvo")
            .arg("--profile")
            .arg(self.profile())
            .arg("--adapter-state-dir")
            .arg(self.adapter())
            .args(generic)
            .args(args)
            .env("XDG_CONFIG_HOME", self.path().join("xdg"))
            .env_remove(asset_store::user_config::ENV_PATH)
            // Отказ запуска собственного Chromium без браузера и GUI.
            .env("CHROME_BIN", "/bin/false")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn code(value: &Value) -> &str {
    value["error"]["details"]["code"]
        .as_str()
        .unwrap_or_default()
}

fn plan(env: &Env) -> PathBuf {
    let path = env.path().join("plan.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"schema_version":1,"items":[{"surface":"水","reading":"みず","replace_expected_sha256":null}]})).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn config_defaults_to_chromium_first_and_names_sources() {
    let env = Env::new("forvo-config-default");
    let value = json(&env.forvo(&[], &["config"]));
    assert_eq!(value["outcome"], "ok", "{value}");
    assert_eq!(value["result"]["forvo"]["mode"], "chromium_first");
    assert_eq!(value["result"]["forvo"]["sources"]["mode"], "default");
    assert_eq!(value["result"]["forvo"]["config_file_loaded"], false);
    assert_eq!(value["result"]["adapter"]["configured"], false);

    let file = env.config("[forvo]\nmode = \"extension_only\"\nbrowser_target = \"chrome\"\n");
    let from_file = json(&env.forvo(&["--config", file.to_str().unwrap()], &["config"]));
    assert_eq!(from_file["result"]["forvo"]["mode"], "extension_only");
    assert_eq!(from_file["result"]["forvo"]["sources"]["mode"], "file");
    assert_eq!(from_file["result"]["forvo"]["browser_target"], "chrome");
    let cli = json(&env.forvo(
        &[
            "--config",
            file.to_str().unwrap(),
            "--mode",
            "chromium_first",
        ],
        &["config"],
    ));
    assert_eq!(cli["result"]["forvo"]["mode"], "chromium_first");
    assert_eq!(cli["result"]["forvo"]["sources"]["mode"], "cli");

    let bad = env.config("[forvo]\nmode = \"slow\"\n");
    let output = env.forvo(&["--config", bad.to_str().unwrap()], &["config"]);
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(code(&json(&output)), "config_invalid");
    // Неизвестное значение CLI отвергает разбор аргументов, а не тайный выбор.
    let output = env.forvo(&["--mode", "fast"], &["config"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn extension_only_never_starts_cdp_and_names_the_next_action() {
    let env = Env::new("forvo-extension-only");
    let generic = ["--mode", "extension_only"];
    let fetch = json(&env.forvo(&generic, &["fetch", "--surface", "水", "--reading", "みず"]));
    assert_eq!(code(&fetch), "adapter_not_configured", "{fetch}");
    assert!(fetch["error"]["details"]["next_action"].is_string());
    let decision = &fetch["error"]["details"]["transport_decision"];
    assert_eq!(decision["mode"], "extension_only");
    assert_eq!(decision["cdp_attempted"], false);
    for args in [
        vec!["login"],
        vec!["check"],
        vec!["candidates", "--surface", "水", "--reading", "みず"],
    ] {
        let value = json(&env.forvo(&generic, &args));
        assert_eq!(code(&value), "cdp_disabled_by_mode", "{args:?}");
    }
    let plan = plan(&env);
    let start = json(&env.forvo(
        &generic,
        &[
            "batch-start",
            "--batch-id",
            "only",
            "--plan",
            plan.to_str().unwrap(),
        ],
    ));
    assert_eq!(code(&start), "adapter_not_configured", "{start}");
    // Ни профиля CDP, ни пакета: отказ до первой долговечной операции.
    assert!(!env.profile().exists());
    let status = json(&env.forvo(&[], &["batch-status", "--batch-id", "only"]));
    assert_eq!(status["outcome"], "error");
}

#[test]
fn chromium_first_reports_backend_failure_without_secret_fallback() {
    let env = Env::new("forvo-chromium-first");
    let fetch = json(&env.forvo(&[], &["fetch", "--surface", "水", "--reading", "みず"]));
    assert_eq!(code(&fetch), "browser_recovery_required", "{fetch}");
    let decision = &fetch["error"]["details"]["transport_decision"];
    assert_eq!(decision["mode"], "chromium_first");
    assert_eq!(decision["attempts"][0]["stage"], "launch");
    assert_eq!(decision["fallback"]["used"], false);
    assert_eq!(decision["fallback"]["allowed"], true);
    assert_eq!(decision["fallback"]["failure_kind"], "backend");
    assert_eq!(
        decision["fallback"]["adapter_unavailable"],
        "adapter_not_configured"
    );
    // Пакет без адаптера — прежнее поведение: пакет CDP приостановлен отказом запуска.
    let plan = plan(&env);
    let output = env.forvo(
        &[],
        &[
            "batch-start",
            "--batch-id",
            "cdp",
            "--plan",
            plan.to_str().unwrap(),
        ],
    );
    let start = json(&output);
    assert_eq!(output.status.code(), Some(3), "{start}");
    assert_eq!(start["outcome"], "paused");
    assert_eq!(start["result"]["effective_transport"], "cdp_chromium");
    assert_eq!(
        start["result"]["transport_decision"]["fallback"]["used"],
        false
    );
    // Пакет CDP продолжается сохранённым транспортом; режим extension_only его
    // не мигрирует и Chromium не запускает.
    let refused = json(&env.forvo(
        &["--mode", "extension_only"],
        &["batch-resume", "--batch-id", "cdp"],
    ));
    assert_eq!(code(&refused), "transport_disabled_by_mode", "{refused}");
    assert!(
        refused["error"]["details"]["next_action"]
            .as_str()
            .unwrap()
            .contains("--mode chromium_first")
    );
}

#[test]
fn generic_resume_follows_stored_extension_binding_and_keeps_mismatch_checks() {
    let env = Env::new("forvo-resume-binding");
    let options = asset_store::StoreOptions::new(env.path().join("store"));
    let store = asset_store::AssetStore::open_with_policy(
        options,
        asset_store::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    let plan: web::FetchPlan = serde_json::from_slice(&std::fs::read(plan(&env)).unwrap()).unwrap();
    web::start(&store, "ext", plan, web::Transport::BraveExtension).unwrap();
    // Цель пакета сохранена до первого подключения (схема 4), как у batch-start адаптера.
    let mut runtime =
        asset_store::batch_runtime::SafeBatchRuntime::open(store.root(), "ext").unwrap();
    let mut batch: web::ForvoBatch = runtime.load().unwrap().unwrap();
    batch.schema_version = 4;
    batch.target = Some("brave".into());
    batch.revision += 1;
    runtime.save(&batch).unwrap();
    drop(runtime);
    drop(store);
    // Явная другая цель для пакета, закреплённого за целью brave, — browser_mismatch.
    let mismatch = json(&env.forvo(
        &["--browser-target", "chrome"],
        &["batch-resume", "--batch-id", "ext"],
    ));
    assert_eq!(code(&mismatch), "browser_mismatch", "{mismatch}");
    assert_eq!(
        mismatch["error"]["details"]["transport_decision"]["source"],
        "batch_binding"
    );
    // Без явной цели — сохранённая цель пакета: адаптер не настроен, CDP не запускается.
    let resumed = json(&env.forvo(&[], &["batch-resume", "--batch-id", "ext"]));
    assert_eq!(resumed["outcome"], "paused", "{resumed}");
    assert_eq!(
        resumed["result"]["paused"]["code"],
        "adapter_not_configured"
    );
    assert_eq!(resumed["result"]["target"], "brave");
    assert_eq!(
        resumed["result"]["transport_decision"]["selected"],
        "brave_extension"
    );
    assert!(!env.profile().exists());
    // Явная команда другого транспорта по-прежнему отказывает.
    let explicit = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(env.path())
        .arg("--store")
        .arg(env.path().join("store"))
        .args(["forvo", "--profile"])
        .arg(env.profile())
        .args(["batch-status", "--batch-id", "ext"])
        .output()
        .unwrap();
    assert_eq!(
        json(&explicit)["result"]["effective_transport"],
        "brave_extension"
    );
}

// --- Релизный конвейер ---------------------------------------------------------

fn release_tool(args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_forvo-extension-release"))
        .args(args)
        .output()
        .unwrap();
    (output.status.success(), json(&output))
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn release_build_is_reproducible_complete_and_verifiable() {
    let ws = TempWorkspace::create("forvo-release-build").unwrap();
    let (a, b) = (ws.path().join("a"), ws.path().join("b"));
    for out in [&a, &b] {
        let (ok, value) = release_tool(&[
            "build",
            "--out",
            out.to_str().unwrap(),
            "--repository",
            release::REPOSITORY,
            "--commit",
            COMMIT,
        ]);
        assert!(ok, "{value}");
        assert_eq!(value["result"]["pairing_included"], false);
    }
    let version = package::bundled_version();
    let zip_name = release::archive_name(&version);
    let zip = std::fs::read(a.join(&zip_name)).unwrap();
    assert_eq!(
        zip,
        std::fs::read(b.join(&zip_name)).unwrap(),
        "ZIP воспроизводим побайтово"
    );
    let files = package::extract_zip(&zip).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"forvo_page.js") && paths.contains(&"background.js"));
    assert!(!paths.contains(&"pairing.json"));
    let manifest: Value = serde_json::from_slice(
        &files
            .iter()
            .find(|f| f.path == "manifest.json")
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(manifest["version"], version.as_str());
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(a.join(release::METADATA_ASSET)).unwrap()).unwrap();
    assert_eq!(metadata["tag"], release::tag(&version));
    assert_eq!(metadata["version"], version.as_str());

    // Подпись без секрета — BLOCKED, а не неподписанный выпуск.
    let (ok, missing) = release_tool(&[
        "sign",
        "--dir",
        a.to_str().unwrap(),
        "--key-env",
        "FORVO_TEST_KEY_ABSENT",
    ]);
    assert!(!ok);
    assert_eq!(missing["code"], "signing_key_missing");
    assert_eq!(missing["details"]["status"], "BLOCKED");

    // Ключ сопровождающего: keygen → подпись → проверка с явным ключом.
    let key_file = ws.path().join("signing.key");
    let (ok, key) = release_tool(&["keygen", "--out", key_file.to_str().unwrap()]);
    assert!(ok, "{key}");
    let public = ws.path().join("public.hex");
    std::fs::write(&public, key["result"]["public_key"].as_str().unwrap()).unwrap();
    let secret = std::fs::read_to_string(&key_file).unwrap();
    let untrusted = Command::new(env!("CARGO_BIN_EXE_forvo-extension-release"))
        .args([
            "sign",
            "--dir",
            a.to_str().unwrap(),
            "--key-env",
            "FORVO_TEST_KEY",
        ])
        .env("FORVO_TEST_KEY", &secret)
        .output()
        .unwrap();
    assert_eq!(json(&untrusted)["code"], "signing_key_untrusted");
    let signed = Command::new(env!("CARGO_BIN_EXE_forvo-extension-release"))
        .args([
            "sign",
            "--dir",
            a.to_str().unwrap(),
            "--key-env",
            "FORVO_TEST_KEY",
            "--allow-untrusted-key",
        ])
        .env("FORVO_TEST_KEY", &secret)
        .output()
        .unwrap();
    assert!(
        signed.status.success(),
        "{}",
        String::from_utf8_lossy(&signed.stdout)
    );
    let (ok, verified) = release_tool(&[
        "verify",
        "--dir",
        a.to_str().unwrap(),
        "--trusted-public-key",
        public.to_str().unwrap(),
    ]);
    assert!(ok, "{verified}");
    assert_eq!(verified["result"]["verified"]["version"], version.as_str());
    let (ok, embedded) = release_tool(&["verify", "--dir", a.to_str().unwrap()]);
    if release::TrustRoot::embedded().keys().is_empty() {
        assert!(!ok);
        assert_eq!(embedded["code"], "release_trust_root_missing");
    }

    // Офлайн-проверка версии тем же CLI, что устанавливает обновления.
    let check = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .args(["forvo", "browser", "--state-dir"])
        .arg(ws.path().join("adapter"))
        .args(["update-check", "--from-dir"])
        .arg(&a)
        .arg("--trusted-public-key")
        .arg(&public)
        .output()
        .unwrap();
    let check = json(&check);
    assert_eq!(check["result"]["status"], "verified", "{check}");
    assert_eq!(check["result"]["version"], version.as_str());
    assert_eq!(check["result"]["trust_root"]["source"], "explicit_key");
    // Обновление без настроенной цели — отказ без записи.
    let update = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .args(["forvo", "browser", "--state-dir"])
        .arg(ws.path().join("adapter"))
        .args(["update", "--from-dir"])
        .arg(&a)
        .arg("--trusted-public-key")
        .arg(&public)
        .output()
        .unwrap();
    assert_eq!(code(&json(&update)), "adapter_not_configured");

    // Сверка с «опубликованным» выпуском той же версии: совпадает побайтово.
    let (ok, same) = release_tool(&[
        "compare",
        "--dir",
        a.to_str().unwrap(),
        "--published",
        b.to_str().unwrap(),
    ]);
    assert!(ok, "{same}");
    // Изменённое содержимое под той же версией — отказ публикации.
    let mut forged: Value =
        serde_json::from_slice(&std::fs::read(b.join(release::METADATA_ASSET)).unwrap()).unwrap();
    forged["archive"]["sha256"] = json!("0".repeat(64));
    std::fs::write(
        b.join(release::METADATA_ASSET),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    let (ok, differs) = release_tool(&[
        "compare",
        "--dir",
        a.to_str().unwrap(),
        "--published",
        b.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert_eq!(differs["code"], "published_release_differs");
}
