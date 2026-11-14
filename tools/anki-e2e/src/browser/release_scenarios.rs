//! Выпуски адаптера и выбор транспорта в настоящем браузере через настоящий CLI.
//!
//! `cli_release_update` — подписанный тестовым ключом выпуск новой
//! версии устанавливается `forvo browser update --from-dir` в прежний каталог
//! цели с перезагрузкой расширения; проверяются сохранённые путь, ID,
//! экземпляр, `pairing.json` и `state.json`, отказы без изменения каталога
//! (понижение, подмена архива, чужой ключ, занятая цель, незавершённая
//! загрузка), отсутствие отката обычным `setup` и продолжение пакета после
//! обновления без повторной загрузки. Ключ создаётся на прогон: боевого ключа
//! и настоящего GitHub Release стенд не использует.
//!
//! `cli_transport_routing` — общий путь `forvo fetch|batch-*`: режим
//! `extension_only` не запускает собственный Chromium, `chromium_first` при
//! отказе запуска Chromium (исполняемый файл `/bin/false`) переходит к
//! адаптеру до побочных эффектов, пакет выбирает транспорт до создания и
//! продолжается по сохранённой привязке после смены конфигурации.
use super::chrome::download_files;
use super::cli_scenarios::{Cli, MISSING_CLI, codes, item, requests};
use super::fixture::{self, Behavior, Gate};
use super::scenarios::{Env, Result, err, files_json};
use crate::util::sha256_hex;
use asset_store::forvo::brave::package::{self, PackageFile};
use asset_store::forvo::brave::release::{self, ReleaseMetadata};
use asset_store::forvo::brave::{acceptance, download, install, targets};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const COMMIT: &str = "e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0";

/// Следующая версия после `version`: четвёртая часть или её увеличение.
fn next(version: &str) -> String {
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let mut parts = parts;
    if parts.len() < 4 {
        parts.push(1);
    } else {
        *parts.last_mut().expect("часть") += 1;
    }
    parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn files_with_version(version: &str) -> Vec<PackageFile> {
    let mut files = package::bundled_files();
    let manifest = files
        .iter_mut()
        .find(|f| f.path == "manifest.json")
        .expect("manifest");
    let mut value: Value = serde_json::from_slice(&manifest.bytes).expect("manifest JSON");
    value["version"] = json!(version);
    manifest.bytes = serde_json::to_vec_pretty(&value).expect("JSON");
    files
}

/// Каталог выпуска как у GitHub Release: метаданные, подпись, ZIP.
fn write_release(root: &Path, name: &str, version: &str, pkcs8: &[u8]) -> Result<PathBuf> {
    let files = files_with_version(version);
    let zip = package::build_zip(&files).map_err(err)?;
    let metadata = ReleaseMetadata::new(&files, &zip, release::REPOSITORY, COMMIT)
        .map_err(err)?
        .to_bytes();
    let signature = release::sign(&metadata, pkcs8).map_err(err)?.to_bytes();
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (file, bytes) in [
        (release::METADATA_ASSET.to_owned(), metadata),
        (release::SIGNATURE_ASSET.to_owned(), signature),
        (release::archive_name(version), zip),
    ] {
        std::fs::write(dir.join(file), bytes).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// SHA-256 каждого файла каталога расширения (служебные файлы тоже видны).
fn tree(dir: &Path) -> Value {
    let mut entries: Vec<(String, String)> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read(e.path())
                            .map(|b| sha256_hex(&b))
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    json!(entries)
}

fn disk_version(dir: &Path) -> String {
    std::fs::read(dir.join("manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|m| m["version"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

async fn disconnect(env: &mut Env<'_>) {
    if let Some(mut conn) = env.conn.take() {
        conn.bridge.disconnect().await;
    }
}

/// Возвращает каталог расширения к встроенной версии (гигиена стенда: каталог
/// общий для сценариев прогона, а setup более новую версию не откатывает).
pub fn restore_bundled(state_dir: &Path, target: &str) -> std::result::Result<(), String> {
    let state = targets::load_target(state_dir, Some(target)).map_err(err)?;
    if disk_version(&state.install_dir) == package::bundled_version() {
        return Ok(());
    }
    let mut files = package::bundled_files();
    files.push(PackageFile {
        path: package::PAIRING_FILE.into(),
        bytes: serde_json::to_vec(
            &json!({"schema_version":1,"port":state.port,"secret":state.secret}),
        )
        .expect("JSON"),
    });
    install::install(&state.install_dir, &files, (None, None))
        .map(|_| ())
        .map_err(err)
}

pub async fn extension_release_update(env: &mut Env<'_>) -> Result<()> {
    let cli = Cli::new(env).ok_or(MISSING_CLI)?;
    let bundled = package::bundled_version();
    let (v1, v2) = (next(&bundled), next(&next(&bundled)));
    let key = release::generate_key().map_err(err)?;
    let public = env.dir.join("release-public.hex");
    std::fs::write(&public, release::public_key_of(&key).map_err(err)?)
        .map_err(|e| e.to_string())?;
    let releases = env.dir.join("releases");
    let new = write_release(&releases, "new", &v1, &key)?;
    let newer = write_release(&releases, "newer", &v2, &key)?;
    let older = write_release(&releases, "bundled", &bundled, &key)?;
    let foreign_key = release::generate_key().map_err(err)?;
    let foreign = write_release(&releases, "foreign", &v2, &foreign_key)?;
    let tampered = write_release(&releases, "tampered", &v2, &key)?;
    {
        let path = tampered.join(release::archive_name(&v2));
        let mut zip = std::fs::read(&path).map_err(|e| e.to_string())?;
        let middle = zip.len() / 2;
        zip[middle] ^= 0x20;
        std::fs::write(&path, zip).map_err(|e| e.to_string())?;
    }
    let public_arg = public.to_string_lossy().into_owned();
    let update = |dir: &Path| {
        let dir = dir.to_string_lossy().into_owned();
        let public_arg = public_arg.clone();
        let cli = &cli;
        async move {
            cli.run(
                true,
                &[
                    "update",
                    "--from-dir",
                    &dir,
                    "--trusted-public-key",
                    &public_arg,
                    "--wait-seconds",
                    "30",
                ],
            )
            .await
        }
    };
    // Первое подключение закрепляет ID и экземпляр (как у пользователя после установки).
    env.connect().await?;
    let first = env.conn.as_ref().expect("bridge").peer.clone();
    disconnect(env).await;
    let state_path = env.ctx.state_dir.join("state.json");
    let dir = env.ctx.extension_dir.clone();
    let state_before = std::fs::read(&state_path).map_err(|e| e.to_string())?;
    let pairing_before = std::fs::read(dir.join("pairing.json")).map_err(|e| e.to_string())?;
    // Пакет до обновления: оригинал одного элемента сохранён, второй ждёт продолжения.
    let plan = cli.plan(&env.dir, "before-update", &[fixture::WORD_3, fixture::WORD])?;
    let started = cli
        .run(
            true,
            &[
                "batch-start",
                "--batch-id",
                "upd",
                "--plan",
                &plan.to_string_lossy(),
                "--max-items",
                "1",
                "--wait-seconds",
                "30",
            ],
        )
        .await?;
    env.out.check(
        "до обновления пакет расширения обработал один элемент из двух",
        started.code == 0 && codes(&started) == [json!("acquired"), Value::Null],
        started.summary(),
    );
    let binding = started.out["result"]["browser"].clone();

    // 1. Обновление на новую совместимую версию через настоящий CLI.
    let updated = update(&new).await?;
    let state_after = std::fs::read(&state_path).map_err(|e| e.to_string())?;
    let pairing_after = std::fs::read(dir.join("pairing.json")).map_err(|e| e.to_string())?;
    let result = &updated.out["result"];
    env.out.check(
        "update установил подписанный выпуск в прежний каталог и перезагрузил расширение: оно отвечает новой версией",
        updated.code == 0
            && result["status"] == "updated"
            && result["from_version"] == bundled.as_str()
            && result["extension"]["version"] == v1.as_str()
            && disk_version(&dir) == v1,
        json!({"run":updated.summary(),"result":result}),
    );
    env.out.check(
        "сохранены путь, ID расширения, экземпляр, pairing.json и state.json (порт, секрет, привязка)",
        result["extension"]["extension_id"] == first.extension_id.as_str()
            && result["extension"]["same_instance"] == true
            && pairing_after == pairing_before
            && state_after == state_before
            && targets::load_target(&env.ctx.state_dir, Some(&env.ctx.target))
                .map(|s| s.install_dir == dir)
                .unwrap_or(false),
        json!({"pairing_unchanged":pairing_after == pairing_before,"state_unchanged":state_after == state_before}),
    );
    let doctor = cli.run(true, &["doctor", "--wait-seconds", "30"]).await?;
    let version_step = doctor.out["result"]["steps"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "extension_version").cloned())
        .unwrap_or(Value::Null);
    env.out.check(
        "doctor подтверждает установленную версию выпуска без отката на встроенную",
        version_step["status"] == "PASS"
            && version_step["evidence"]["installed_version"] == v1.as_str()
            && disk_version(&dir) == v1,
        version_step.clone(),
    );
    // 2. Повтор, та же версия и обычный setup — no-op, без отката.
    let again = update(&new).await?;
    // Тот же production-setup цели, что стенд выполняет перед сценарием.
    let setup = asset_store::forvo::brave::setup(&env.ctx.setup).map_err(err)?;
    env.out.check(
        "повтор update той же версии — no-op; обычный setup не откатывает выпуск на встроенную версию",
        again.out["result"]["status"] == "up_to_date"
            && setup["extension_files"]["version"] == v1.as_str()
            && setup["extension_files"]["changed"] == json!([])
            && setup["pairing"] == "kept"
            && disk_version(&dir) == v1,
        json!({"again":again.out["result"]["status"],"setup_files":setup["extension_files"]}),
    );
    // 3. Отказы до изменения каталога.
    let before = tree(&dir);
    let downgrade = update(&older).await?;
    let untrusted = update(&foreign).await?;
    let corrupt = update(&tampered).await?;
    env.out.check(
        "понижение версии, чужой ключ и подменённый байт архива отклонены; каталог расширения не изменён",
        downgrade.error_code() == "downgrade_refused"
            && untrusted.error_code() == "release_key_untrusted"
            && corrupt.error_code() == "release_archive_mismatch"
            && tree(&dir) == before,
        json!({"downgrade":downgrade.summary(),"untrusted":untrusted.summary(),"corrupt":corrupt.summary()}),
    );
    // 4. Занятая цель: порт bridge удерживает другая команда.
    env.connect().await?;
    let busy = update(&newer).await?;
    let still_connected = env.call("ping", json!({})).await.is_ok();
    env.out.check(
        "update при занятой цели отказывает adapter_busy, не трогая файлы и соединение другой команды",
        busy.error_code() == "adapter_busy" && tree(&dir) == before && still_connected,
        busy.summary(),
    );
    // 5. Незавершённая собственная загрузка: обновление откладывается.
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let op = download::new_operation_id().map_err(err)?;
    let begun = env.begin(&op, fixture::WORD, &candidate.record_id).await?;
    let bound = env
        .poll(&op, Duration::from_secs(15), |f| f.bound_id.is_some())
        .await?;
    disconnect(env).await;
    let deferred = update(&newer).await?;
    env.out.check(
        "при незавершённой собственной загрузке update отложен: файлы не заменены, reload не вызван",
        begun["code"] == "activated"
            && bound.bound_id.is_some()
            && deferred.error_code() == "update_deferred_operation_pending"
            && tree(&dir) == before,
        deferred.summary(),
    );
    env.connect().await?;
    let ping = env.call("ping", json!({})).await.map_err(err)?;
    let recovered =
        acceptance::recover_previous(&mut env.conn.as_mut().expect("bridge").bridge, &ping).await;
    gate.open();
    env.ctx.fixture.set(fixture::RECORD_A, Behavior::Normal);
    tokio::time::sleep(Duration::from_secs(1)).await;
    env.out.check(
        "отложенная операция освобождена штатным восстановлением; собственный файл не оставлен",
        recovered.as_ref().is_some_and(|r| r.is_ok())
            && download_files(&env.downloads()).is_empty(),
        json!({"downloads":files_json(&env.downloads())}),
    );
    disconnect(env).await;
    let finished = update(&newer).await?;
    env.out.check(
        "после освобождения операции обновление выполнено; идентичность прежняя",
        finished.out["result"]["status"] == "updated"
            && finished.out["result"]["extension"]["version"] == v2.as_str()
            && finished.out["result"]["extension"]["same_instance"] == true,
        finished.summary(),
    );
    // 6. Пакет продолжается после обновления адаптера по сохранённой привязке.
    let records = [fixture::RECORD_3, fixture::RECORD_A];
    let before_resume = requests(env, &records);
    let resumed = cli
        .run(
            true,
            &["batch-resume", "--batch-id", "upd", "--wait-seconds", "30"],
        )
        .await?;
    let after_resume = requests(env, &records);
    env.out.check(
        "batch-resume после обновления завершает пакет той же сессией; сохранённый элемент не скачивается повторно",
        resumed.code == 0
            && codes(&resumed) == [json!("acquired"), json!("acquired")]
            && resumed.out["result"]["browser"] == binding
            && after_resume[fixture::RECORD_3] == before_resume[fixture::RECORD_3]
            && item(&resumed, fixture::WORD)["outcome"]["code"] == "acquired",
        json!({"run":resumed.summary(),"requests_before":before_resume,"requests_after":after_resume,
            "binding_before":binding,"binding_after":resumed.out["result"]["browser"]}),
    );
    env.connect().await?;
    env.residual_clean().await?;
    disconnect(env).await;
    restore_bundled(&env.ctx.state_dir, &env.ctx.target)
}

pub async fn cli_transport_routing(env: &mut Env<'_>) -> Result<()> {
    let mut cli = Cli::new(env).ok_or(MISSING_CLI)?;
    let target = env.ctx.target.clone();
    let config = |name: &str, mode: &str| -> Result<String> {
        let path = env.dir.join(name);
        std::fs::write(
            &path,
            format!("[forvo]\nmode = \"{mode}\"\nbrowser_target = \"{target}\"\nupdate_check = \"off\"\n"),
        )
        .map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    };
    let extension_only = config("extension-only.toml", "extension_only")?;
    let chromium_first = config("chromium-first.toml", "chromium_first")?;
    // Отказ запуска собственного Chromium до любых побочных эффектов.
    let broken_cdp = [("CHROME_BIN", "/bin/false")];
    // Для доменного отказа запускаем production CDP против того же HTTPS fixture,
    // что использует тестовый браузер. Остальной трафик браузера остаётся закрыт.
    let fixture_chrome = env.dir.join("fixture-chromium.sh");
    let chrome = env.ctx.chrome.to_string_lossy().replace('\'', "'\\''");
    let resolver = format!(
        "--host-resolver-rules=MAP forvo.com 127.0.0.1:{}, MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
        env.ctx.fixture.port
    );
    let resolver = resolver.replace('\'', "'\\''");
    std::fs::write(
        &fixture_chrome,
        format!(
            "#!/bin/sh\nexec '{chrome}' --no-proxy-server --ignore-certificate-errors '{resolver}' \"$@\"\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fixture_chrome)
            .map_err(|e| e.to_string())?
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture_chrome, permissions).map_err(|e| e.to_string())?;
    }
    let fixture_chrome = fixture_chrome.to_string_lossy().into_owned();
    let fixture_cdp = [("CHROME_BIN", fixture_chrome.as_str())];

    // CDP получает оригинал и повторно читает его даже при недоступном Chromium.
    let downloads_before = env.ctx.fixture.requests_for(fixture::RECORD_A);
    let cdp = cli
        .generic(
            &["--config", &chromium_first],
            &["fetch", "--surface", fixture::WORD],
            &fixture_cdp,
        )
        .await?;
    let cdp_repeat = cli
        .generic(
            &["--config", &chromium_first],
            &["fetch", "--surface", fixture::WORD],
            &broken_cdp,
        )
        .await?;
    env.out.check("CDP fetch и повтор reused=true без запуска Chromium и второй загрузки",
        cdp.code == 0 && cdp_repeat.code == 0 && cdp.out["result"]["reused"] == false
            && cdp_repeat.out["result"]["reused"] == true
            && cdp.out["result"]["source"] == cdp_repeat.out["result"]["source"]
            && env.ctx.fixture.requests_for(fixture::RECORD_A) == downloads_before + 1
            && cli.canonical_audio()? == json!([]),
        json!({"first":cdp.out,"repeat":cdp_repeat.out,"downloads":env.ctx.fixture.requests_for(fixture::RECORD_A)-downloads_before}));

    // 1. extension_only: CDP не запускается даже для проверки.
    cli.profile = env.dir.join("cdp-profile-extension-only");
    let only = cli
        .generic(
            &["--config", &extension_only],
            &["fetch", "--surface", fixture::WORD],
            &[],
        )
        .await?;
    let decision = &only.out["result"]["transport_decision"];
    env.out.check(
        "extension_only: общий fetch сохранил оригинальный MP3 через адаптер, собственный Chromium не запускался",
        only.code == 0
            && only.out["result"]["acquisition"]["downloaded_sha256"] == sha256_hex(&env.ctx.fixture.bytes_for(fixture::RECORD_A))
            && decision["mode"] == "extension_only"
            && decision["cdp_attempted"] == false
            && decision["selected"] == "brave_extension"
            && !cli.profile.exists(),
        json!({"run":only.summary(),"decision":decision}),
    );
    // 2. chromium_first: отказ запуска CDP → переход к адаптеру.
    cli.profile = env.dir.join("cdp-profile-broken");
    let fallback = cli
        .generic(
            &["--config", &chromium_first],
            &["fetch", "--surface", fixture::WORD_3],
            &broken_cdp,
        )
        .await?;
    let decision = &fallback.out["result"]["transport_decision"];
    env.out.check(
        "chromium_first: отказ запуска CDP до побочных эффектов → тот же MP3 через адаптер",
        fallback.code == 0
            && fallback.out["result"]["acquisition"]["downloaded_sha256"]
                == sha256_hex(&env.ctx.fixture.bytes_for(fixture::RECORD_3))
            && decision["fallback"]["used"] == true
            && decision["fallback"]["reason"] == "cdp_launch_failed"
            && decision["fallback"]["failure_kind"] == "backend"
            && decision["attempts"][0]["stage"] == "launch"
            && decision["selected"] == "brave_extension",
        json!({"run":fallback.summary(),"decision":decision}),
    );
    // 3. Предметная ошибка CDP не запускает переход к адаптеру, хотя он допустим
    // после отказа запуска. Chromium открывает синтетическую изменённую страницу.
    let mp3_requests_before = env
        .ctx
        .fixture
        .log()
        .into_iter()
        .filter(|path| path.starts_with("/download/mp3/"))
        .count();
    cli.profile = env.dir.join("cdp-profile-domain");
    let domain = cli
        .generic(
            &["--config", &chromium_first],
            &["fetch", "--surface", fixture::WORD_DOM_CHANGED],
            &fixture_cdp,
        )
        .await?;
    let decision = &domain.out["error"]["details"]["transport_decision"];
    let attempts = decision["attempts"].as_array();
    let mp3_requests_after = env
        .ctx
        .fixture
        .log()
        .into_iter()
        .filter(|path| path.starts_with("/download/mp3/"))
        .count();
    env.out.check(
        "chromium_first при dom_changed не переходит с CDP на адаптер и не загружает MP3",
        domain.code != 0
            && domain.error_code() == "dom_changed"
            && decision["mode"] == "chromium_first"
            && decision["cdp_attempted"] == true
            && decision["selected"].is_null()
            && decision["fallback"]["used"] == false
            && decision["fallback"]["allowed"] == false
            && decision["fallback"]["failure_kind"] == "domain_or_unsafe"
            && decision["fallback"]["refused_because"] == "dom_changed"
            && attempts.is_some_and(|a| {
                a.len() == 1
                    && a[0]["transport"] == "cdp_chromium"
                    && a[0]["stage"] == "before_download"
                    && a[0]["outcome"] == "failed"
                    && a[0]["code"] == "dom_changed"
            })
            && mp3_requests_after == mp3_requests_before,
        json!({"run":domain.summary(),"decision":decision,
            "mp3_requests_before":mp3_requests_before,"mp3_requests_after":mp3_requests_after}),
    );
    // 4. Новый пакет: транспорт выбирается до создания; preflight CDP не удался → пакет адаптера.
    cli.profile = env.dir.join("cdp-profile-batch");
    let plan = cli.plan(&env.dir, "routed", &[fixture::WORD_2, fixture::WORD])?;
    let records = [fixture::RECORD_2_TOP, fixture::RECORD_A];
    let started = cli
        .generic(
            &["--config", &chromium_first],
            &[
                "batch-start",
                "--batch-id",
                "routed",
                "--plan",
                &plan.to_string_lossy(),
                "--max-items",
                "1",
            ],
            &broken_cdp,
        )
        .await?;
    env.out.check(
        "общий batch-start: CDP не запустился до записи состояния → пакет адаптера с целью, сохранённой до подключения",
        started.code == 0
            && started.out["result"]["effective_transport"] == "brave_extension"
            && started.out["result"]["schema_version"] == 5
            && started.out["result"]["target"] == target.as_str()
            && started.out["result"]["transport_decision"]["fallback"]["used"] == true
            && codes(&started) == [json!("acquired"), Value::Null],
        json!({"run":started.summary(),"decision":started.out["result"]["transport_decision"]}),
    );
    let before = requests(env, &records);
    // 5. Конфигурация сменилась (CDP теперь исправен): пакет всё равно продолжается своей привязкой.
    cli.profile = env.dir.join("cdp-profile-resume");
    let resumed = cli
        .generic(
            &["--config", &chromium_first],
            &["batch-resume", "--batch-id", "routed"],
            &[],
        )
        .await?;
    let after = requests(env, &records);
    env.out.check(
        "batch-resume после смены конфигурации идёт по сохранённой привязке; Chromium не запускается, повторной загрузки нет",
        resumed.code == 0
            && codes(&resumed) == [json!("acquired"), json!("acquired")]
            && resumed.out["result"]["transport_decision"]["source"] == "batch_binding"
            && resumed.out["result"]["browser"] == started.out["result"]["browser"]
            && after[fixture::RECORD_2_TOP] == before[fixture::RECORD_2_TOP]
            && !cli.profile.exists(),
        json!({"run":resumed.summary(),"requests_before":before,"requests_after":after}),
    );
    // 6. Явная команда другого транспорта и режим без CDP пакет не мигрируют.
    let explicit = cli
        .generic(&["--mode", "extension_only"], &["login"], &[])
        .await?;
    env.out.check(
        "extension_only отказывает командам собственного Chromium (cdp_disabled_by_mode) без запуска браузера",
        explicit.error_code() == "cdp_disabled_by_mode" && !cli.profile.exists(),
        explicit.summary(),
    );
    env.connect().await?;
    env.residual_clean().await
}
