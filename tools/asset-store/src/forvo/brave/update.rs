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
const CHECK_LOCK_FILE: &str = "check.lock";
const MAX_LISTING_BYTES: usize = 2 * 1024 * 1024;
const LISTING_PAGES: u32 = 3;
const RELOAD_WAIT: Duration = Duration::from_secs(60);
const AUTO_CHECK_ERROR_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

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
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(5).min(channel.timeout))
        .timeout(channel.timeout);
    // Тестовые каналы — локальные серверы на 127.0.0.1: системный прокси из
    // окружения разработчика не должен в них вмешиваться.
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|_| unavailable("client"))
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
            // Отсутствующие или неверно типизированные флаги не доказывают,
            // что выпуск относится к стабильному каналу.
            if item["draft"].as_bool() != Some(false) || item["prerelease"].as_bool() != Some(false)
            {
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
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
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

/// Общая межпроцессная блокировка чтения, проверки и записи кэша. Проверки
/// выполняются последовательно: медленный ответ старого запроса не может
/// перезаписать более новый результат. Файл блокировки остаётся в приватном
/// каталоге состояния, а flock автоматически освобождается при завершении
/// процесса или отмене задачи.
async fn check_lock(state_dir: &Path, wait: bool) -> Result<Option<std::fs::File>, AssetError> {
    use std::os::unix::fs::OpenOptionsExt;

    let state_dir = state_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let dir = state_dir.join("updates");
        private_dir(&dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(dir.join(CHECK_LOCK_FILE))
            .map_err(|e| AssetError::io("блокировка проверки выпусков адаптера", e))?;
        if !file
            .metadata()
            .map_err(|e| AssetError::io("проверка блокировки выпусков адаптера", e))?
            .is_file()
        {
            return Err(bridge_error(
                "update_check_lock_invalid",
                "Файл блокировки проверки выпусков адаптера не является обычным файлом",
            ));
        }
        let operation = if wait {
            rustix::fs::FlockOperation::LockExclusive
        } else {
            rustix::fs::FlockOperation::NonBlockingLockExclusive
        };
        match rustix::fs::flock(&file, operation) {
            Ok(()) => Ok(Some(file)),
            Err(error) => {
                let error: std::io::Error = error.into();
                if !wait && error.kind() == std::io::ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(AssetError::io(
                        "блокировка проверки выпусков адаптера",
                        error,
                    ))
                }
            }
        }
    })
    .await
    .map_err(|_| {
        bridge_error(
            "update_check_lock_failed",
            "Не удалось получить блокировку проверки выпусков адаптера",
        )
    })?
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

/// Сетевая проверка доступной версии без управления кэшем. Архив не скачивается;
/// неподтверждённая подпись или отсутствие корня доверия — ошибка, а не успешный
/// результат с видом проверенного выпуска.
async fn check_uncached(
    state: Option<&AdapterState>,
    source: &Source,
    trust: &TrustRoot,
    channel: &Channel,
) -> Result<Value, AssetError> {
    // Отсутствие корня доверия блокирует явную проверку до запроса списка:
    // пустой канал не должен выглядеть как успешно подтверждённое отсутствие
    // обновлений.
    if trust.keys().is_empty() {
        return Err(release::no_trusted_keys());
    }
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
        let (metadata, key_id) =
            release::verify_metadata(&fetched.metadata, &fetched.signature, trust, tag.as_deref())?;
        Ok(json!({"status":"verified","origin":fetched.origin,"key_id":key_id,
            "version":metadata.version,"tag":metadata.tag,"commit":metadata.commit,"compatibility":metadata.compatibility,
            "comparison":compare(Some(&metadata.version), installed.as_deref())}))
    }
    .await;
    let mut value = result?;
    value["trust_root"] = trust.describe();
    Ok(value)
}

fn cache_for_result(result: &Result<Value, AssetError>) -> CheckCache {
    CheckCache {
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
    }
}

/// Сетевая проверка доступной версии: наибольший стабильный тег канала и
/// подпись его метаданных. Архив не скачивается. Запрос и запись кэша
/// сериализованы между процессами.
pub async fn check(
    state_dir: &Path,
    state: Option<&AdapterState>,
    source: &Source,
    trust: &TrustRoot,
    channel: &Channel,
) -> Result<Value, AssetError> {
    let _lock = if matches!(source, Source::GitHub) {
        Some(check_lock(state_dir, true).await?.ok_or_else(|| {
            bridge_error(
                "update_check_lock_failed",
                "Не удалось получить блокировку проверки выпусков адаптера",
            )
        })?)
    } else {
        None
    };
    let result = check_uncached(state, source, trust, channel).await;
    if matches!(source, Source::GitHub) {
        save_check(state_dir, &cache_for_result(&result));
    }
    result
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
    auto_check_with_channel(
        state_dir,
        state,
        max_age,
        timeout,
        Channel::default(),
        TrustRoot::embedded(),
    )
    .await
}

async fn auto_check_with_channel(
    state_dir: &Path,
    state: Option<&AdapterState>,
    max_age: Duration,
    timeout: Duration,
    mut channel: Channel,
    trust: TrustRoot,
) -> Option<Value> {
    // Обычный fetch не ждёт уже запущенную явную проверку и не создаёт второй
    // сетевой запрос. Проверка кэша и его обновление выполняются под одной
    // блокировкой, включая запись отрицательного результата по тайм-ауту.
    let _lock = check_lock(state_dir, false).await.ok().flatten()?;
    let installed = state.and_then(|s| peek_version(&s.install_dir));
    let cache = if let Some(cache) = cached_check(state_dir).filter(|c| {
        let cache_ttl = if c.error.is_some() {
            max_age.min(AUTO_CHECK_ERROR_CACHE_TTL)
        } else {
            max_age
        };
        now().saturating_sub(c.checked_at) < cache_ttl.as_secs()
    }) {
        cache
    } else {
        // Внешний предел остаётся точным: запас не даёт reqwest успеть
        // сохранить собственную ошибку вместо записи timeout при отмене.
        channel.timeout = timeout.saturating_add(Duration::from_millis(250));
        let result = tokio::time::timeout(
            timeout,
            check_uncached(state, &Source::GitHub, &trust, &channel),
        )
        .await;
        let cache = match result {
            Ok(result) => cache_for_result(&result),
            Err(_) => CheckCache {
                schema_version: 1,
                checked_at: now(),
                latest_version: None,
                latest_tag: None,
                error: Some("timeout".into()),
            },
        };
        save_check(state_dir, &cache);
        cache
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
    files.push(targets::pairing_file(state.port, &state.secret));
    files
}

fn identity_changed(cause: AssetError, command_prefix: &str) -> AssetError {
    let mut error = bridge_error(
        "extension_identity_changed",
        "После перезагрузки подключился другой ID или экземпляр расширения; обновление не подтверждено. Проверьте профиль браузера и выполните doctor",
    );
    error.details["cause"] = json!(crate::forvo::provider_code(&cause));
    error.details["user_action"] = json!(format!(
        "Проверьте профиль браузера и выполните {command_prefix} doctor"
    ));
    error
}

fn installation_mismatch(differs: &[String]) -> AssetError {
    let mut error = bridge_error(
        "installed_files_differ",
        "Файлы установленного выпуска отличаются от подписанного архива той же версии; безопасное обновление не выполнено. Для восстановления явно укажите --reinstall",
    );
    error.details["status"] = json!("REPAIR_REQUIRED");
    error.details["files_match_release"] = json!(false);
    error.details["differs"] = json!(differs);
    error.details["next_action"] =
        json!("Повторите update с --reinstall для восстановления файлов подписанного выпуска");
    error
}

/// Проверяет выпуск, затем под блокировкой цели устанавливает его в прежний
/// каталог и штатно перезагружает подключённое расширение.
pub async fn update(options: &UpdateOptions) -> Result<Value, AssetError> {
    if options.trust.keys().is_empty() {
        return Err(release::no_trusted_keys());
    }
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
        std::cmp::Ordering::Equal if !differs.is_empty() && !options.reinstall => {
            return Err(installation_mismatch(&differs));
        }
        _ => {}
    }
    // 3. Подключение обязательно и для совпадающих файлов: версия на диске
    // сама по себе не доказывает, что браузер уже перечитал manifest.
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
    let files_match = differs.is_empty();
    let disk_matches = to == from && files_match;
    let runtime_matches = peer.version == verified.metadata.version;
    if disk_matches && runtime_matches {
        let mut value = base;
        value["status"] = json!("up_to_date");
        value["installed_version"] = json!(installed);
        value["disk_version"] = json!(installed);
        value["runtime_version"] = json!(peer.version);
        value["files_match_release"] = json!(true);
        value["runtime_confirmed"] = json!(true);
        value["extension"] = json!({"extension_id":peer.extension_id,
            "same_extension_id":state.extension_id.as_deref().map(|id| id == peer.extension_id),
            "same_instance":state.instance_id.as_deref().zip(peer.instance.as_deref()).map(|(a,b)| a == b),
            "instance_available":peer.instance.is_some(),"version":peer.version});
        return Ok(value);
    }

    let needs_install = to > from || (to == from && !files_match && options.reinstall);
    let needs_reload = needs_install || !runtime_matches;
    if !needs_reload {
        // Это ветка лишь для исчерпывающего перечисления будущих вариантов;
        // на текущем контракте все оставшиеся состояния требуют обновления.
        return Err(installation_mismatch(&differs));
    }
    if state.extension_id.is_none() {
        let mut e = bridge_error(
            "extension_not_paired",
            "Расширение цели не закреплено; автоматическое обновление не может подтвердить его ID после перезагрузки. Выполните setup и doctor",
        );
        e.details["files_changed"] = json!(false);
        e.details["user_action"] = json!(state.user_action());
        return Err(e);
    }
    let Some(instance) = peer.instance.as_deref() else {
        bridge.disconnect().await;
        let mut e = bridge_error(
            "extension_identity_unverifiable",
            "Подключённая версия адаптера не сообщает устойчивый ID экземпляра; обновление отложено до ручной проверки. Файлы не изменены",
        );
        e.details["files_changed"] = json!(false);
        e.details["runtime_version"] = json!(peer.version);
        e.details["user_action"] = json!(format!(
            "Обновите расширение вручную на странице {} и выполните {} doctor",
            state.browser.extensions_uri(),
            state.command_prefix()
        ));
        return Err(e);
    };
    // Для legacy state без сохранённого instance_id закрепляем экземпляр,
    // подтверждённый первым соединением, и требуем тот же при reload.
    bridge.pin(&peer.extension_id, Some(instance));

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

    // 4. При прерванной прошлой операции файлы могут быть уже зафиксированы,
    // но runtime — старым. В таком случае повтор выполняет только reload.
    let changed = if needs_install {
        install::install(
            &state.install_dir,
            &files,
            (
                Some(installed.clone()),
                Some(verified.metadata.version.clone()),
            ),
        )?
    } else {
        Vec::new()
    };
    if needs_install {
        tracing::info!(event = "extension_release_installed", stage = "update", target = %state.target,
            from = %installed, to = %verified.metadata.version, files = changed.len(), "Выпуск адаптера установлен");
    }
    // 5. Перезагрузка и обязательная проверка версии, ID и того же instance.
    let started = Instant::now();
    let command_prefix = state.command_prefix();
    let reloaded = async {
        bridge
            .call("reload", json!({}), Duration::from_secs(5))
            .await?;
        bridge
            .accept(Instant::now() + options.wait.min(RELOAD_WAIT))
            .await
            .map_err(|e| {
                if matches!(
                    crate::forvo::provider_code(&e),
                    "instance_mismatch" | "extension_id_mismatch"
                ) {
                    identity_changed(e, &command_prefix)
                } else {
                    e
                }
            })
    }
    .await;
    bridge.disconnect().await;
    let mut value = base;
    value["from_version"] = json!(installed);
    value["to_version"] = json!(verified.metadata.version);
    value["disk_version"] = json!(verified.metadata.version);
    value["runtime_version_before"] = json!(peer.version);
    value["files_match_release"] = json!(true);
    value["files_changed"] = json!(changed.clone());
    value["pairing"] = json!("kept");
    match reloaded {
        Ok(after)
            if after.version == verified.metadata.version
                && after.extension_id == peer.extension_id
                && after.instance == peer.instance =>
        {
            value["status"] = json!("updated");
            value["runtime_confirmed"] = json!(true);
            value["reload_ms"] = json!(started.elapsed().as_millis() as u64);
            value["extension"] = json!({"extension_id":after.extension_id,"same_extension_id":after.extension_id == peer.extension_id,
                "same_instance":after.instance == peer.instance,"version":after.version});
            Ok(value)
        }
        Ok(after) if after.extension_id != peer.extension_id || after.instance != peer.instance => {
            let mut e = identity_changed(
                bridge_error(
                    "instance_mismatch",
                    "После перезагрузки изменился ID или экземпляр расширения",
                ),
                &command_prefix,
            );
            e.details["files_installed"] = json!(true);
            e.details["files_changed_this_attempt"] = json!(changed);
            e.details["extension_id"] = json!(after.extension_id);
            e.details["extension_instance"] = json!(after.instance);
            e.details["update"] = value;
            Err(e)
        }
        Ok(after) => {
            let mut e = bridge_error(
                "extension_reload_ineffective",
                "В браузере не подтверждена версия подписанного выпуска; нажмите «Обновить» у расширения вручную",
            );
            e.details["files_installed"] = json!(true);
            e.details["files_changed_this_attempt"] = json!(changed);
            e.details["disk_version"] = json!(verified.metadata.version);
            e.details["extension_version"] = json!(after.version);
            e.details["update"] = value;
            e.details["user_action"] = json!(format!(
                "Нажмите «Обновить» у расширения на {}",
                state.browser.extensions_uri()
            ));
            Err(e)
        }
        Err(reload) => {
            if crate::forvo::provider_code(&reload) == "extension_identity_changed" {
                let mut reload = reload;
                reload.details["files_installed"] = json!(true);
                reload.details["files_changed_this_attempt"] = json!(changed);
                reload.details["disk_version"] = json!(verified.metadata.version);
                reload.details["update"] = value;
                return Err(reload);
            }
            let mut e = bridge_error(
                "extension_reload_unconfirmed",
                "Перезагрузка расширения не подтверждена; выполните update ещё раз после проверки страницы расширений браузера",
            );
            e.details["files_installed"] = json!(true);
            e.details["files_changed_this_attempt"] = json!(changed);
            e.details["disk_version"] = json!(verified.metadata.version);
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
