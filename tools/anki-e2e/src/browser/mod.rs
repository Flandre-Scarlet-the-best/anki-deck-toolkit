//! Браузерный набор Forvo-Brave: настоящий тестовый Chromium с установленным
//! MV3-расширением адаптера, настоящий loopback bridge и Rust-контракты
//! `asset-store` против локального fixture-сайта.
//!
//! Набор не требует Docker, Anki Desktop, персонального Brave, профиля или
//! аккаунта Forvo и не обращается к внешней сети. Он переиспользует статусы,
//! коды завершения, прерывание и формат отчётов стенда `anki-e2e`, но не его
//! Docker-жизненный цикл: тот привязан к образу Anki и не нужен браузеру.

mod chrome;
mod fixture;
mod scenarios;

use crate::report::{self, Status, Stop};
use crate::util;
use asset_store::forvo::brave;
use clap::Args;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const SCENARIOS: &[&str] = &[
    "extension_bridge_tab",
    "auth_and_candidates",
    "full_download",
    "concurrent_downloads",
    "download_failures",
    "service_worker_restart",
    "bridge_reconnect",
    "own_tab_closed",
    "interrupt",
    "save_as_prompt",
];

#[derive(Debug, Clone, Args)]
pub struct BrowserArgs {
    /// Сценарии через запятую; по умолчанию — весь набор.
    #[arg(long, value_delimiter = ',')]
    pub scenarios: Vec<String>,
    /// Исполняемый файл совместимого Chromium (Chromium или Chrome for Testing,
    /// принимающий `--load-extension`); иначе CHROME_BIN, CHROMIUM_BIN, PATH.
    #[arg(long)]
    pub chrome: Option<PathBuf>,
    /// Каталог отчётов (по умолчанию <репозиторий>/target/anki-e2e).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    /// Сохранить тестовые профили Chromium и каталог сопряжения после прогона.
    #[arg(long)]
    pub keep_profiles: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub what: String,
    pub ok: bool,
    pub evidence: Value,
}

/// Итог сценария: PASS только у завершённого сценария, где все проверки истинны.
#[derive(Debug, Default)]
pub struct Outcome {
    pub checks: Vec<Check>,
    pub completed: bool,
    pub evidence: serde_json::Map<String, Value>,
}

impl Outcome {
    pub fn check(&mut self, what: impl Into<String>, ok: bool, evidence: Value) -> bool {
        let what = what.into();
        println!("  {} {what}", if ok { "✔" } else { "✘" });
        self.checks.push(Check { what, ok, evidence });
        ok
    }
    pub fn status(&self) -> Status {
        if self.completed && !self.checks.is_empty() && self.checks.iter().all(|c| c.ok) {
            Status::Pass
        } else {
            Status::Fail
        }
    }
}

pub struct Ctx {
    pub chrome: PathBuf,
    pub run_dir: PathBuf,
    pub fixture: fixture::Fixture,
    pub state_dir: PathBuf,
    pub extension_dir: PathBuf,
    pub interrupt: tokio::sync::watch::Receiver<bool>,
}

fn tool_available(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn synthetic_mp3(path: &Path, frequency: u32, seconds: &str) -> Result<Vec<u8>, String> {
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={frequency}:duration={seconds}:sample_rate=44100"
        ))
        .args(["-c:a", "libmp3lame", "-q:a", "4"])
        .arg(path)
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !status.success() {
        return Err("ffmpeg не создал синтетический MP3".into());
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn certificate(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let (cert, key) = (dir.join("fixture-cert.pem"), dir.join("fixture-key.pem"));
    let status = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=forvo.com",
        ])
        .args([
            "-addext",
            &format!("subjectAltName=DNS:forvo.com,DNS:{}", fixture::USER_HOST),
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("openssl: {e}"))?;
    if !status.success() {
        return Err("openssl не создал сертификат fixture".into());
    }
    Ok((cert, key))
}

fn chrome_version(path: &Path) -> Option<String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    Ok(listener.local_addr().map_err(|e| e.to_string())?.port())
}

fn revision(repo: &Path) -> Value {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    json!({"commit":git(&["rev-parse","HEAD"]),"dirty":git(&["status","--porcelain"]).map(|s| !s.is_empty())})
}

pub fn run(args: &BrowserArgs, interrupted: Arc<AtomicBool>) -> u8 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = match crate::source::repository_root(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Запускайте из рабочей копии anki-deck-toolkit: {e}");
            return Status::Error.exit_code();
        }
    };
    let selected: Vec<String> = if args.scenarios.is_empty() {
        SCENARIOS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        args.scenarios.clone()
    };
    if let Some(unknown) = selected.iter().find(|s| !SCENARIOS.contains(&s.as_str())) {
        eprintln!(
            "Неизвестный сценарий {unknown}; доступны: {}",
            SCENARIOS.join(", ")
        );
        return Status::Error.exit_code();
    }
    let work = args
        .work_dir
        .clone()
        .unwrap_or_else(|| repo.join("target").join("anki-e2e"));
    let started = util::unix_now();
    let id = format!(
        "{}-{}",
        util::utc_compact(started),
        util::random_hex(3).unwrap_or_default()
    );
    let dir = work.join("browser-runs").join(&id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("{}: {e}", dir.display());
        return Status::Error.exit_code();
    }
    println!("anki-e2e browser: прогон {id}, отчёт {}", dir.display());
    let mut report = json!({
        "schema_version": 1, "suite": "browser", "run_id": id, "scenarios_requested": selected,
        "started_at": util::utc_rfc3339(started), "command": std::env::args().collect::<Vec<_>>(),
        "host": {"os":std::env::consts::OS,"arch":std::env::consts::ARCH},
        "source": revision(&repo), "paths": {"run_dir": dir},
        "boundary": "изолированный тестовый Chromium и локальный fixture; живой Forvo, авторизация и профиль пользователя не используются",
    });
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio: {e}");
            return Status::Error.exit_code();
        }
    };
    let status = runtime.block_on(execute(args, &dir, &selected, &mut report, interrupted));
    finish(&dir, &mut report, status, started)
}

fn finish(dir: &Path, report: &mut Value, status: Status, started: f64) -> u8 {
    let finished = util::unix_now();
    report["status"] = json!(status);
    report["finished_at"] = json!(util::utc_rfc3339(finished));
    report["duration_s"] = json!(finished - started);
    let mut errors = Vec::new();
    if let Err(e) = util::write_json(&dir.join("report.json"), report) {
        errors.push(e);
    }
    if let Err(e) = util::write_atomic(
        &dir.join("report.md"),
        report::render_browser_markdown(report).as_bytes(),
    ) {
        errors.push(e);
    }
    if errors.is_empty() {
        println!(
            "Итог: {} — {}",
            status.as_str(),
            dir.join("report.md").display()
        );
        return status.exit_code();
    }
    eprintln!("Отчёт не сохранён: {}", errors.join("; "));
    if status == Status::Pass {
        Status::Error
    } else {
        status
    }
    .exit_code()
}

async fn execute(
    args: &BrowserArgs,
    dir: &Path,
    selected: &[String],
    report: &mut Value,
    interrupted: Arc<AtomicBool>,
) -> Status {
    let stop = |report: &mut Value, stop: Stop| {
        eprintln!("{}: {}", stop.reason, stop.message);
        let status = stop.status;
        report["stop"] = json!(stop);
        status
    };
    let Some(chrome) = chrome::find_chrome(args.chrome.as_deref()) else {
        return stop(
            report,
            Stop::blocked(
                "prerequisites",
                "browser_not_found",
                "Нет совместимого Chromium: укажите --chrome или CHROME_BIN (Chromium или Chrome for Testing; брендированный Chrome не загружает --load-extension)",
            ),
        );
    };
    for tool in ["ffmpeg", "ffprobe", "openssl"] {
        let available = if tool == "openssl" {
            std::process::Command::new(tool)
                .arg("version")
                .output()
                .is_ok_and(|o| o.status.success())
        } else {
            tool_available(tool)
        };
        if !available {
            return stop(
                report,
                Stop::blocked(
                    "prerequisites",
                    format!("{tool}_not_found"),
                    format!(
                        "Нужен {tool}: fixture генерирует MP3 и сертификат, а Rust проверяет MP3 через ffprobe/ffmpeg"
                    ),
                ),
            );
        }
    }
    report["browser"] = json!({"executable": chrome, "version": chrome_version(&chrome), "headless": true, "profile": "временный, в каталоге прогона"});
    let fixture_dir = dir.join("fixture");
    let prepared = (|| {
        std::fs::create_dir_all(&fixture_dir).map_err(|e| e.to_string())?;
        let a = synthetic_mp3(&fixture_dir.join("record-a.mp3"), 440, "0.6")?;
        let b = synthetic_mp3(&fixture_dir.join("record-b.mp3"), 660, "0.8")?;
        let (cert, key) = certificate(&fixture_dir)?;
        Ok::<_, String>((a, b, cert, key))
    })();
    let (mp3_a, mp3_b, cert, key) = match prepared {
        Ok(v) => v,
        Err(e) => {
            return stop(
                report,
                Stop::error("fixture", "fixture_generation_failed", e),
            );
        }
    };
    let fixture = match fixture::Fixture::start(&cert, &key, mp3_a, mp3_b).await {
        Ok(f) => f,
        Err(e) => return stop(report, Stop::error("fixture", "fixture_start_failed", e)),
    };
    let _ = std::fs::remove_file(&key);
    report["fixture"] = json!({
        "origin": "https://forvo.com → 127.0.0.1 (только тестовый профиль, --host-resolver-rules)",
        "port": fixture.port, "tls": "самоподписанный сертификат на время прогона",
        "record_a": {"record_id": fixture::RECORD_A, "sha256": util::sha256_hex(&fixture.mp3_a), "bytes": fixture.mp3_a.len()},
        "record_b": {"record_id": fixture::RECORD_B, "sha256": util::sha256_hex(&fixture.mp3_b), "bytes": fixture.mp3_b.len()},
        "user_file_sha256": util::sha256_hex(&fixture.user_file),
        "generator": "ffmpeg lavfi sine → libmp3lame",
    });
    let adapter_dir = dir.join("adapter");
    let state_dir = adapter_dir.join("state");
    let extension_dir = adapter_dir.join("extension");
    let port = match free_port() {
        Ok(p) => p,
        Err(e) => return stop(report, Stop::error("adapter", "port_unavailable", e)),
    };
    // Production-расширение без изменений: тот же manifest и тот же код, что ставит setup.
    match brave::setup(&brave::SetupOptions {
        state_dir: state_dir.clone(),
        install_dir: Some(extension_dir.clone()),
        port: Some(port),
        rotate_secret: false,
        platform: Some(brave::Platform::Linux),
    }) {
        Ok(value) => {
            report["adapter"] = json!({"bridge": value["bridge"], "platform": value["platform"], "pairing": value["pairing"]})
        }
        Err(e) => return stop(report, Stop::error("adapter", "setup_failed", e.message)),
    }
    let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(false);
    let watcher = {
        let interrupted = interrupted.clone();
        tokio::spawn(async move {
            loop {
                if interrupted.load(Ordering::SeqCst) {
                    let _ = interrupt_tx.send(true);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let ctx = Ctx {
        chrome,
        run_dir: dir.to_owned(),
        fixture: fixture.clone(),
        state_dir,
        extension_dir,
        interrupt: interrupt_rx,
    };
    let mut results = Vec::new();
    let mut statuses = Vec::new();
    for name in selected {
        if interrupted.load(Ordering::SeqCst) {
            results.push(json!({"name":name,"status":Status::Interrupted,"duration_s":0.0,"reason":"прогон прерван до сценария","checks":[]}));
            statuses.push(Status::Interrupted);
            continue;
        }
        println!("▶ {name}");
        fixture.reset();
        let begun = util::unix_now();
        let (status, reason, outcome) = scenarios::run(&ctx, name).await;
        let duration = util::unix_now() - begun;
        println!(
            "  {} ({duration:.1} с){}",
            status.as_str(),
            reason
                .as_ref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        );
        results.push(json!({"name":name,"status":status,"duration_s":duration,"reason":reason,"checks":outcome.checks,"evidence":outcome.evidence}));
        statuses.push(status);
    }
    watcher.abort();
    fixture.shutdown();
    let mut cleanup = json!({"fixture_stopped": true});
    if !args.keep_profiles {
        // Каталог сопряжения содержит тестовый секрет прогона; профили — состояние Chromium.
        cleanup["adapter_removed"] = json!(std::fs::remove_dir_all(&adapter_dir).is_ok());
        let mut removed = 0;
        for entry in std::fs::read_dir(dir.join("scenarios"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if std::fs::remove_dir_all(entry.path().join("profile")).is_ok() {
                removed += 1;
            }
        }
        cleanup["profiles_removed"] = json!(removed);
    }
    report["scenarios"] = json!(results);
    report["cleanup"] = cleanup;
    report::overall(statuses)
}
