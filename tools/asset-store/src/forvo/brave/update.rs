//! Проверка доступной версии и явное обновление установленного адаптера.
//!
//! Канал — GitHub Releases репозитория [`release::REPOSITORY`] с тегами
//! `forvo-extension-v<версия>`. Общий `/releases/latest` не используется: он
//! выбирает последний выпуск всего репозитория, а не адаптера. Доступная
//! версия — наибольшая версия среди опубликованных (не черновых и не
//! предварительных) выпусков этого канала. Подлинность выпуска доказывает
//! подпись метаданных закреплённым ключом ([`release::verify`]), а не адрес
//! загрузки: те же проверки проходят и файлы из `--from-dir`.
//!
//! Установка выполняется только под занятым портом bridge цели — той же
//! блокировкой, что у `fetch`, пакетов, `setup` и `doctor`, — и только когда
//! подключённое расширение не держит незавершённую собственную операцию.
//! Каталог, ID, экземпляр, `pairing.json`, порт и секрет сохраняются.
use super::install;
use super::package::{self, PackageFile, Version};
use super::release::{self, TrustRoot, VerifiedRelease};
use super::targets::{self, AdapterState};
use super::{Bridge, Trust, bridge_error, private_dir, write_atomic};
use crate::AssetError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

const USER_AGENT: &str = "anki-deck-toolkit-forvo-updater";
const API_BASE: &str = "https://api.github.com";
const DOWNLOAD_BASE: &str = "https://github.com";
const CHECK_FILE: &str = "check.json";
const MAX_LISTING_BYTES: usize = 2 * 1024 * 1024;
const LISTING_PAGES: u32 = 3;
const RELOAD_WAIT: Duration = Duration::from_secs(60);

/// Откуда берётся выпуск.
#[derive(Debug, Clone)]
pub enum Source {
    /// GitHub Releases канала адаптера.
    GitHub,
    /// Каталог с файлами выпуска (офлайн): метаданные, подпись и ZIP.
    Directory(PathBuf),
}

/// Адреса канала; в тестах подменяются локальным сервером.
#[derive(Debug, Clone)]
pub struct Channel {
    pub api_base: String,
    pub download_base: String,
    pub timeout: Duration,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            api_base: API_BASE.into(),
            download_base: DOWNLOAD_BASE.into(),
            timeout: Duration::from_secs(30),
        }
    }
}

fn unavailable(reason: &str) -> AssetError {
    let mut e = bridge_error(
        "update_source_unavailable",
        "Канал выпусков адаптера недоступен; повторите позже или установите выпуск из скачанного каталога: update --from-dir <каталог>",
    );
    e.details["reason"] = json!(reason);
    e
}

fn client(channel: &Channel) -> Result<reqwest::Client, AssetError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(5).min(channel.timeout))
        .timeout(channel.timeout)
        .build()
        .map_err(|_| unavailable("client"))
}

async fn get_limited(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
    accept: &str,
) -> Result<Option<Vec<u8>>, AssetError> {
    let mut response = client
        .get(url)
        .header("accept", accept)
        .send()
        .await
        .map_err(|e| unavailable(if e.is_timeout() { "timeout" } else { "network" }))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        let mut e = unavailable("http_status");
        e.details["status"] = json!(response.status().as_u16());
        return Err(e);
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(unavailable("too_large"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable("network"))? {
        if body.len() + chunk.len() > limit {
            return Err(unavailable("too_large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Наибольшая опубликованная версия канала адаптера.
pub async fn latest_published(channel: &Channel) -> Result<Option<(Version, String)>, AssetError> {
    let client = client(channel)?;
    let mut best: Option<(Version, String)> = None;
    for page in 1..=LISTING_PAGES {
        let url = format!(
            "{}/repos/{}/releases?per_page=100&page={page}",
            channel.api_base,
            release::REPOSITORY
        );
        let Some(body) = get_limited(
            &client,
            &url,
            MAX_LISTING_BYTES,
            "application/vnd.github+json",
        )
        .await?
        else {
            return Err(unavailable("repository_not_found"));
        };
        let items: Vec<Value> =
            serde_json::from_slice(&body).map_err(|_| unavailable("listing"))?;
        for item in &items {
            let Some(tag) = item["tag_name"].as_str() else {
                continue;
            };
            if item["draft"] == true || item["prerelease"] == true {
                continue;
            }
            let Some(version) = tag
                .strip_prefix(release::TAG_PREFIX)
                .and_then(Version::parse)
            else {
                continue;
            };
            // Тег должен быть каноническим: `forvo-extension-v01` не принимается.
            if release::tag(&version.to_string()) != tag {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| version > *b) {
                best = Some((version, tag.to_owned()));
            }
        }
        if items.len() < 100 {
            break;
        }
    }
    Ok(best)
}

/// Непроверенные файлы выпуска и их происхождение.
pub struct Fetched {
    pub metadata: Vec<u8>,
    pub signature: Vec<u8>,
    pub archive: Option<Vec<u8>>,
    pub origin: Value,
}

async fn fetch_github(
    channel: &Channel,
    tag: &str,
    with_archive: bool,
) -> Result<Fetched, AssetError> {
    let client = client(channel)?;
    let asset = |name: &str| {
        format!(
            "{}/{}/releases/download/{tag}/{name}",
            channel.download_base,
            release::REPOSITORY
        )
    };
    let missing = || {
        let mut e = bridge_error(
            "release_not_found",
            "Выпуск адаптера с такой версией не опубликован",
        );
        e.details["tag"] = json!(tag);
        e
    };
    let metadata = get_limited(&client, &asset(release::METADATA_ASSET), 64 * 1024, "*/*")
        .await?
        .ok_or_else(missing)?;
    let signature = get_limited(&client, &asset(release::SIGNATURE_ASSET), 4096, "*/*")
        .await?
        .ok_or_else(missing)?;
    let archive = if with_archive {
        // Имя архива — функция версии из тега, а не из непроверенных метаданных.
        let version = tag.strip_prefix(release::TAG_PREFIX).unwrap_or_default();
        Some(
            get_limited(
                &client,
                &asset(&release::archive_name(version)),
                package::MAX_ARCHIVE_BYTES as usize,
                "*/*",
            )
            .await?
            .ok_or_else(missing)?,
        )
    } else {
        None
    };
    Ok(Fetched {
        metadata,
        signature,
        archive,
        origin: json!({"source":"github_releases","repository":release::REPOSITORY,"tag":tag}),
    })
}

/// Обычный файл без следования ссылкам и не больше `limit` байт.
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, AssetError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let failed = |reason: &str| {
        let mut e = bridge_error(
            "release_dir_invalid",
            "Каталог выпуска должен содержать обычные файлы метаданных, подписи и ZIP",
        );
        e.details["path"] = json!(path);
        e.details["reason"] = json!(reason);
        e
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|_| failed("open"))?;
    if !file.metadata().map_err(|_| failed("metadata"))?.is_file() {
        return Err(failed("not_regular"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed("read"))?;
    if bytes.len() as u64 > limit {
        return Err(failed("too_large"));
    }
    Ok(bytes)
}

fn fetch_dir(dir: &Path, version: Option<&str>) -> Result<Fetched, AssetError> {
    let metadata = read_regular(&dir.join(release::METADATA_ASSET), 64 * 1024)?;
    let signature = read_regular(&dir.join(release::SIGNATURE_ASSET), 4096)?;
    // Имя архива определяется заявленной версией; подлинность проверит подпись.
    let claimed = match version {
        Some(v) => v.to_owned(),
        None => serde_json::from_slice::<Value>(&metadata)
            .ok()
            .and_then(|v| v["version"].as_str().map(str::to_owned))
            .filter(|v| Version::parse(v).is_some())
            .ok_or_else(|| {
                bridge_error(
                    "release_metadata_mismatch",
                    "Метаданные выпуска не называют версию",
                )
            })?,
    };
    let archive = read_regular(
        &dir.join(release::archive_name(&claimed)),
        package::MAX_ARCHIVE_BYTES,
    )?;
    Ok(Fetched {
        metadata,
        signature,
        archive: Some(archive),
        origin: json!({"source":"directory","path":dir,"tag":release::tag(&claimed)}),
    })
}

fn no_release() -> AssetError {
    let mut e = bridge_error(
        "release_not_published",
        "В канале адаптера ещё нет опубликованных выпусков; используйте встроенную в CLI версию (setup)",
    );
    e.details["status"] = json!("NOT_TESTED");
    e
}

/// Тег, который будет установлен: явная версия или наибольшая опубликованная.
async fn resolve_tag(
    source: &Source,
    version: Option<&str>,
    channel: &Channel,
) -> Result<Option<String>, AssetError> {
    if let Some(v) = version {
        if Version::parse(v).is_none() {
            return Err(bridge_error(
                "invalid_version",
                "Версия: 1–4 числа через точку",
            ));
        }
        return Ok(Some(release::tag(v)));
    }
    match source {
        Source::Directory(_) => Ok(None),
        Source::GitHub => Ok(latest_published(channel).await?.map(|(_, tag)| tag)),
    }
}

async fn fetch(
    source: &Source,
    version: Option<&str>,
    channel: &Channel,
    with_archive: bool,
) -> Result<(Fetched, Option<String>), AssetError> {
    let tag = resolve_tag(source, version, channel).await?;
    let fetched = match source {
        Source::Directory(dir) => fetch_dir(dir, version)?,
        Source::GitHub => {
            let tag = tag.as_deref().ok_or_else(no_release)?;
            fetch_github(channel, tag, with_archive).await?
        }
    };
    Ok((fetched, tag))
}

// --- Кэш проверки ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckCache {
    pub schema_version: u32,
    pub checked_at: u64,
    pub latest_version: Option<String>,
    pub latest_tag: Option<String>,
    pub error: Option<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn check_path(state_dir: &Path) -> PathBuf {
    state_dir.join("updates").join(CHECK_FILE)
}

pub fn cached_check(state_dir: &Path) -> Option<CheckCache> {
    std::fs::read(check_path(state_dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<CheckCache>(&b).ok())
        .filter(|c| c.schema_version == 1)
}

fn save_check(state_dir: &Path, cache: &CheckCache) {
    let dir = state_dir.join("updates");
    if private_dir(&dir).is_ok() {
        let _ = write_atomic(
            &check_path(state_dir),
            &serde_json::to_vec_pretty(cache).expect("кэш"),
            0o600,
        );
    }
}

/// Сопоставление доступной версии с установленной и встроенной (без сети).
fn compare(available: Option<&str>, installed: Option<&str>) -> Value {
    let bundled = package::bundled_version();
    let current = installed.unwrap_or(&bundled);
    let newer = available
        .and_then(Version::parse)
        .zip(Version::parse(current))
        .is_some_and(|(a, c)| a > c);
    json!({"available_version":available,"installed_version":installed,"bundled_version":bundled,
        "update_available":newer})
}

/// Сетевая проверка доступной версии: наибольший тег канала и подпись его
/// метаданных. Архив не скачивается. Результат кэшируется в каталоге состояния.
pub async fn check(
    state_dir: &Path,
    state: Option<&AdapterState>,
    source: &Source,
    trust: &TrustRoot,
    channel: &Channel,
) -> Result<Value, AssetError> {
    let installed = state.and_then(|s| peek_version(&s.install_dir));
    let result = async {
        let (fetched, tag) = match fetch(source, None, channel, false).await {
            Ok(found) => found,
            Err(e) if crate::forvo::provider_code(&e) == "release_not_published" => {
                return Ok(json!({"status":"no_release_published","channel":release::CHANNEL,
                    "comparison":compare(None, installed.as_deref())}));
            }
            Err(e) => return Err(e),
        };
        let verified = release::verify_metadata(&fetched.metadata, &fetched.signature, trust, tag.as_deref());
        Ok(match verified {
            Ok((metadata, key_id)) => json!({"status":"verified","origin":fetched.origin,"key_id":key_id,
                "version":metadata.version,"tag":metadata.tag,"commit":metadata.commit,"compatibility":metadata.compatibility,
                "comparison":compare(Some(&metadata.version), installed.as_deref())}),
            Err(e) => json!({"status":"unverified","origin":fetched.origin,"code":crate::forvo::provider_code(&e),
                "message":e.message,"next_action":e.details["next_action"],
                "comparison":compare(None, installed.as_deref())}),
        })
    }
    .await;
    if matches!(source, Source::GitHub) {
        save_check(
            state_dir,
            &CheckCache {
                schema_version: 1,
                checked_at: now(),
                latest_version: result
                    .as_ref()
                    .ok()
                    .and_then(|v| v["version"].as_str().map(str::to_owned)),
                latest_tag: result
                    .as_ref()
                    .ok()
                    .and_then(|v| v["tag"].as_str().map(str::to_owned)),
                error: result
                    .as_ref()
                    .err()
                    .map(|e| crate::forvo::provider_code(e).to_owned()),
            },
        );
    }
    let mut value = result?;
    value["trust_root"] = trust.describe();
    Ok(value)
}

/// Версия на диске без восстановления журнала (чтение вне блокировки цели).
fn peek_version(dir: &Path) -> Option<String> {
    std::fs::read(dir.join("manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|m| m["version"].as_str().map(str::to_owned))
}

/// Редкая фоновая проверка при обычной работе: только если кэш старше
/// `max_age`, с коротким пределом времени; ошибки сети лишь записываются в кэш
/// и не влияют на результат команды. Установку не выполняет никогда.
pub async fn auto_check(
    state_dir: &Path,
    state: Option<&AdapterState>,
    max_age: Duration,
    timeout: Duration,
) -> Option<Value> {
    let installed = state.and_then(|s| peek_version(&s.install_dir));
    let fresh =
        cached_check(state_dir).filter(|c| now().saturating_sub(c.checked_at) < max_age.as_secs());
    let cache = match fresh {
        Some(cache) => cache,
        None => {
            let channel = Channel {
                timeout,
                ..Channel::default()
            };
            let trust = TrustRoot::embedded();
            let _ = tokio::time::timeout(
                timeout,
                check(state_dir, state, &Source::GitHub, &trust, &channel),
            )
            .await;
            cached_check(state_dir).unwrap_or(CheckCache {
                schema_version: 1,
                checked_at: now(),
                latest_version: None,
                latest_tag: None,
                error: Some("timeout".into()),
            })
        }
    };
    let comparison = compare(cache.latest_version.as_deref(), installed.as_deref());
    (comparison["update_available"] == true).then(|| {
        json!({"available_version":cache.latest_version,"installed_version":installed,
            "next_action":"Обновите адаптер: pronunciation-assets forvo browser update"})
    })
}

// --- Обновление -----------------------------------------------------------------

pub struct UpdateOptions {
    pub state_dir: PathBuf,
    pub target: Option<String>,
    pub source: Source,
    pub version: Option<String>,
    pub trust: TrustRoot,
    pub wait: Duration,
    /// Переустановить ту же версию, если файлы на диске отличаются (ремонт).
    pub reinstall: bool,
    pub channel: Channel,
}

fn busy() -> AssetError {
    bridge_error(
        "adapter_busy",
        "Цель занята: идёт fetch, пакет, setup или другое обновление этой цели. Обновление не выполнено; повторите после завершения",
    )
}

fn release_files(verified: &VerifiedRelease, state: &AdapterState) -> Vec<PackageFile> {
    let mut files = verified.files.clone();
    files.push(PackageFile {
        path: package::PAIRING_FILE.into(),
        bytes: serde_json::to_vec(
            &json!({"schema_version":1,"port":state.port,"secret":state.secret}),
        )
        .expect("JSON"),
    });
    files
}

/// Проверяет выпуск, затем под блокировкой цели устанавливает его в прежний
/// каталог и штатно перезагружает подключённое расширение.
pub async fn update(options: &UpdateOptions) -> Result<Value, AssetError> {
    let state = targets::load_target(&options.state_dir, options.target.as_deref())?;
    // 1. Скачивание и полная проверка — до блокировки и до любой записи.
    let (fetched, tag) = fetch(
        &options.source,
        options.version.as_deref(),
        &options.channel,
        true,
    )
    .await?;
    let archive = fetched.archive.as_deref().expect("архив запрошен");
    let verified = release::verify(
        &fetched.metadata,
        &fetched.signature,
        archive,
        &options.trust,
        tag.as_deref(),
    )?;
    let to = verified.version();
    // 2. Блокировка цели: порт bridge — общий замок команд этой цели.
    let mut bridge = Bridge::bind_with(
        state.port,
        state.secret_bytes()?,
        Trust {
            pinned: state.extension_id.clone(),
            expected: state.expected_extension_ids(),
            instance: state.instance_id.clone(),
        },
    )
    .await
    .map_err(|e| {
        if crate::forvo::provider_code(&e) == "bridge_port_busy" {
            busy()
        } else {
            e
        }
    })?;
    let base = json!({"target":state.target,"browser":state.browser,"release":verified.summary(),
        "origin":fetched.origin,"trust_root":options.trust.describe()});
    let installed = targets::installed_version(&state.install_dir)?.ok_or_else(|| {
        let mut e = bridge_error(
            "extension_files_missing",
            "Расширение цели не установлено; первичная установка — setup и ручная загрузка распакованного расширения",
        );
        e.details["next_action"] = json!(format!("{} setup", state.command_prefix()));
        e
    })?;
    let from = Version::parse(&installed).ok_or_else(|| {
        bridge_error(
            "installed_version_invalid",
            "Версия установленного расширения не распознана; восстановите его командой setup",
        )
    })?;
    let files = release_files(&verified, &state);
    let differs = install::differing(&state.install_dir, &files);
    match to.cmp(&from) {
        std::cmp::Ordering::Less => {
            let mut e = bridge_error(
                "downgrade_refused",
                "Выпуск старее установленной версии; понижение версии не выполняется",
            );
            e.details["installed_version"] = json!(installed);
            e.details["release_version"] = json!(verified.metadata.version);
            return Err(e);
        }
        std::cmp::Ordering::Equal if differs.is_empty() || !options.reinstall => {
            let mut value = base;
            value["status"] = json!("up_to_date");
            value["installed_version"] = json!(installed);
            value["files_match_release"] = json!(differs.is_empty());
            if !differs.is_empty() {
                value["differs"] = json!(differs);
                value["next_action"] = json!(
                    "Файлы отличаются от выпуска той же версии; восстановить: update --reinstall"
                );
            }
            return Ok(value);
        }
        _ => {}
    }
    if state.extension_id.is_none() {
        let mut e = bridge_error(
            "extension_not_paired",
            "Расширение цели ещё ни разу не подключалось: идентичность после перезагрузки нечем подтвердить. Загрузите его вручную и выполните doctor",
        );
        e.details["user_action"] = json!(state.user_action());
        return Err(e);
    }
    // 3. Подключение: перезагрузка допустима только у свободного расширения.
    let peer = bridge
        .accept(Instant::now() + options.wait)
        .await
        .map_err(|mut e| {
            e.details["files_changed"] = json!(false);
            e.details["user_action"] = json!(format!(
                "Запустите {} с включённым расширением и повторите update",
                state.browser.display()
            ));
            e
        })?;
    let ping = bridge
        .call("ping", json!({}), Duration::from_secs(5))
        .await?;
    if !ping["operation"].is_null() {
        bridge.disconnect().await;
        let mut e = bridge_error(
            "update_deferred_operation_pending",
            "У расширения незавершённая собственная загрузка; обновление отложено, файлы не изменены. Завершите или продолжите пакет и повторите update",
        );
        e.details["files_changed"] = json!(false);
        return Err(e);
    }
    // 4. Транзакционная замена файлов в прежнем каталоге.
    let changed = install::install(
        &state.install_dir,
        &files,
        (
            Some(installed.clone()),
            Some(verified.metadata.version.clone()),
        ),
    )?;
    tracing::info!(event = "extension_release_installed", stage = "update", target = %state.target,
        from = %installed, to = %verified.metadata.version, files = changed.len(), "Выпуск адаптера установлен");
    // 5. Штатная перезагрузка и подтверждение той же идентичности и новой версии.
    let started = Instant::now();
    let reloaded = async {
        bridge
            .call("reload", json!({}), Duration::from_secs(5))
            .await?;
        bridge.accept(Instant::now() + RELOAD_WAIT).await
    }
    .await;
    bridge.disconnect().await;
    let mut value = base;
    value["from_version"] = json!(installed);
    value["to_version"] = json!(verified.metadata.version);
    value["files_changed"] = json!(changed);
    value["pairing"] = json!("kept");
    match reloaded {
        Ok(after) if after.version == verified.metadata.version => {
            value["status"] = json!("updated");
            value["reload_ms"] = json!(started.elapsed().as_millis() as u64);
            value["extension"] = json!({"extension_id":after.extension_id,"same_extension_id":after.extension_id == peer.extension_id,
                "same_instance":after.instance == peer.instance,"version":after.version});
            Ok(value)
        }
        Ok(after) => {
            let mut e = bridge_error(
                "extension_reload_ineffective",
                "Файлы нового выпуска установлены, но браузер перезагрузил расширение прежней версии; нажмите «Обновить» у расширения вручную",
            );
            e.details["files_installed"] = json!(true);
            e.details["extension_version"] = json!(after.version);
            e.details["update"] = value;
            e.details["user_action"] = json!(format!(
                "Нажмите «Обновить» у расширения на {}",
                state.browser.extensions_uri()
            ));
            Err(e)
        }
        Err(reload) => {
            let mut e = bridge_error(
                "extension_reload_unconfirmed",
                "Файлы нового выпуска установлены, но перезагрузка расширения не подтверждена; нажмите «Обновить» у расширения вручную или выполните doctor",
            );
            e.details["files_installed"] = json!(true);
            e.details["reload_error"] = json!(crate::forvo::provider_code(&reload));
            e.details["update"] = value;
            e.details["user_action"] = json!(format!(
                "Нажмите «Обновить» у расширения на {} и выполните {} doctor",
                state.browser.extensions_uri(),
                state.command_prefix()
            ));
            Err(e)
        }
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
