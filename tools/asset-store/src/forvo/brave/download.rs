//! Одна штатная загрузка выбранной записи Forvo в обычном Brave.
//!
//! Расширение нажимает обычную кнопку записи в собственной вкладке и сообщает
//! факты: запросы ожидаемого адреса из своей и чужих вкладок, совпавшие
//! загрузки и связанный `DownloadItem`. Решение о ходе операции принимает
//! [`classify`] — чистая функция над этими фактами; файл читается только после
//! `complete` и проверки размеров. Ничего, кроме связанной собственной загрузки,
//! расширение не отменяет и не удаляет.
use super::acceptance::observed_url;
use super::{Bridge, Platform, bridge_error, browser_path_to_local};
use crate::forvo::{Candidate, download::matches_download, provider_code};
use crate::hashing::sha256_hex;
use crate::pronunciation::{AudioProbe, MAX_AUDIO_BYTES, Pronunciation, probe_mp3};
use crate::{AssetError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Безопасные состояния `DownloadItem.danger`; остальные требуют решения пользователя.
const SAFE_DANGER: &[&str] = &["safe", "accepted", "allowlistedByPolicy", "deepScannedSafe"];
const POLL: Duration = Duration::from_millis(300);
const RECONNECT_WAIT: Duration = Duration::from_secs(45);

/// Идентификатор операции: Rust → bridge → расширение → загрузка → проверка.
pub fn new_operation_id() -> Result<String, AssetError> {
    Ok(super::encode_lower_hex(super::random_bytes()?)[..32].to_owned())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ItemFacts {
    pub id: i64,
    pub state: String,
    pub error: Option<String>,
    pub danger: String,
    pub exists: bool,
    #[serde(default)]
    pub paused: bool,
    pub has_filename: bool,
    /// Полный путь браузера нужен только для чтения файла; в отчёт и журнал не попадает.
    #[serde(skip_serializing)]
    pub filename: Option<String>,
    pub file_name: Option<String>,
    pub bytes_received: i64,
    pub total_bytes: i64,
    pub file_size: i64,
    pub mime: Option<String>,
    pub url: Value,
    pub final_url: Value,
    pub referrer_is_own_page: bool,
    pub referrer_origin_is_forvo: bool,
    pub by_extension: bool,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct StatusFacts {
    pub code: String,
    #[serde(default)]
    pub observer_alive: bool,
    #[serde(default)]
    pub own_requests: u32,
    #[serde(default)]
    pub foreign_requests: u32,
    #[serde(default)]
    pub matching_downloads: u32,
    #[serde(default)]
    pub conflict: bool,
    #[serde(default)]
    pub observer_lost: bool,
    #[serde(default)]
    pub bound_id: Option<i64>,
    #[serde(default)]
    pub bound_missing: bool,
    #[serde(default)]
    pub item: Option<ItemFacts>,
    #[serde(default)]
    pub tab_present: bool,
    #[serde(default)]
    pub page_unchanged: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Сколько ждать запроса из своей вкладки и появления загрузки.
    pub start: Duration,
    /// Сколько связанная загрузка может оставаться без имени файла (окно «Сохранить как»).
    pub prompt: Duration,
    /// Общий предел операции от нажатия кнопки.
    pub total: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(20),
            prompt: Duration::from_secs(8),
            total: Duration::from_secs(90),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Waiting(&'static str),
    Complete(Box<ItemFacts>),
    Failed(&'static str),
}

/// Человекочитаемое пояснение стабильного машинного кода отказа.
pub fn failure_message(code: &str) -> &'static str {
    match code {
        "operation_lost" => "Расширение потеряло состояние операции; загрузка не усыновлена",
        "download_conflict" => {
            "Параллельная загрузка той же записи делает владение неоднозначным; операция остановлена, чужие загрузки не тронуты"
        }
        "ownership_unproven" => {
            "Принадлежность загрузки не доказана (например, после рестарта service worker); файл не импортирован и не удалён"
        }
        "download_vanished" => "Связанная загрузка исчезла из истории браузера",
        "download_mismatch" => "Загрузка не связана однозначно с выбранной записью",
        "download_dangerous" => {
            "Браузер пометил загрузку как требующую решения пользователя; она не принята"
        }
        "save_as_required" => {
            "Браузер требует выбрать место сохранения: выключите «Спрашивать, куда сохранять каждый файл» в настройках загрузок. Окно сохранения после отмены загрузки остаётся открытым — закройте его кнопкой «Отмена». В новом профиле Brave для Linux этот запрос включён по умолчанию"
        }
        "download_canceled" => "Загрузка отменена в браузере",
        "download_interrupted" => "Загрузка прервана браузером",
        "download_timeout" => "Загрузка не завершилась вовремя",
        "download_not_started" => {
            "Обычная кнопка выбранной записи не начала загрузку (ответ не стал загрузкой или браузер заблокировал её)"
        }
        "own_tab_used" => {
            "Вкладка адаптера уже начинала загрузку; повторное открытие заменит её свежей вкладкой в той же группе"
        }
        "own_tab_closed" => "Собственная вкладка адаптера закрыта до начала загрузки",
        "download_incomplete" => "Размер или имя файла завершённой загрузки противоречивы",
        "file_missing" => "Файл завершённой загрузки отсутствует",
        "file_access_denied" => "Файл завершённой загрузки недоступен для чтения",
        "download_invalid" => "Вместо MP3 получено другое содержимое",
        "probe_failed" => "Файл не прошёл техническую проверку MP3",
        "interrupted" => "Операция прервана пользователем",
        "operation_pending" => "У расширения есть незавершённая операция прошлого запуска",
        _ => "Операция загрузки остановлена",
    }
}

fn failed(code: &str) -> AssetError {
    bridge_error(code, failure_message(code))
}

/// Решение по фактам расширения. Порядок важен: любой признак неоднозначности
/// владения останавливает операцию раньше, чем её состояние загрузки.
pub fn classify(
    facts: &StatusFacts,
    request: &Pronunciation,
    record_id: &str,
    limits: &Limits,
) -> Progress {
    if facts.code != "status" {
        return Progress::Failed("operation_lost");
    }
    if facts.conflict {
        return Progress::Failed("download_conflict");
    }
    if facts.observer_lost {
        return Progress::Failed("ownership_unproven");
    }
    if facts.bound_missing {
        return Progress::Failed("download_vanished");
    }
    let elapsed = Duration::from_millis(facts.elapsed_ms);
    let Some(item) = &facts.item else {
        if !facts.tab_present {
            return Progress::Failed("own_tab_closed");
        }
        if elapsed >= limits.start {
            return Progress::Failed(if facts.matching_downloads == 0 {
                "download_not_started"
            } else {
                "ownership_unproven"
            });
        }
        return Progress::Waiting(if facts.own_requests == 0 {
            "waiting_request"
        } else {
            "waiting_download"
        });
    };
    let address_ok = observed_url(&item.url)
        .is_some_and(|url| matches_download(&url, &request.surface, record_id));
    if !address_ok
        || item.by_extension
        || !(item.referrer_is_own_page || item.referrer_origin_is_forvo)
    {
        return Progress::Failed("download_mismatch");
    }
    if !SAFE_DANGER.contains(&item.danger.as_str()) {
        return Progress::Failed("download_dangerous");
    }
    match item.state.as_str() {
        "complete" => Progress::Complete(Box::new(item.clone())),
        "interrupted" if item.error.as_deref() == Some("USER_CANCELED") && !item.has_filename => {
            // Окно выбора пути закрыто без сохранения (headless закрывает его сам).
            Progress::Failed("save_as_required")
        }
        "interrupted" if item.error.as_deref() == Some("USER_CANCELED") => {
            Progress::Failed("download_canceled")
        }
        "interrupted" => Progress::Failed("download_interrupted"),
        "in_progress" if !item.has_filename && elapsed >= limits.prompt => {
            Progress::Failed("save_as_required")
        }
        "in_progress" if elapsed >= limits.total => Progress::Failed("download_timeout"),
        "in_progress" => Progress::Waiting("in_progress"),
        _ => Progress::Failed("download_interrupted"),
    }
}

pub(crate) fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, AssetError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| {
            failed(if e.kind() == std::io::ErrorKind::NotFound {
                "file_missing"
            } else {
                "file_access_denied"
            })
        })?;
    let meta = file.metadata().map_err(|_| failed("file_access_denied"))?;
    if !meta.is_file() {
        return Err(failed("download_invalid"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed("file_access_denied"))?;
    if bytes.len() as u64 > limit {
        return Err(failed("download_invalid"));
    }
    Ok(bytes)
}

/// Файл принимается только у завершённой загрузки с согласованными размерами.
pub fn verify_file(platform: Platform, item: &ItemFacts) -> Result<(PathBuf, Vec<u8>), AssetError> {
    let filename = item
        .filename
        .as_deref()
        .filter(|f| !f.is_empty())
        .ok_or_else(|| failed("file_missing"))?;
    let lower = filename.to_ascii_lowercase();
    if item.state != "complete"
        || [".crdownload", ".tmp", ".part"]
            .iter()
            .any(|s| lower.ends_with(s))
    {
        return Err(failed("download_incomplete"));
    }
    if !item.exists {
        return Err(failed("file_missing"));
    }
    let received = item.bytes_received;
    if received <= 0
        || received as u64 > MAX_AUDIO_BYTES
        || (item.file_size >= 0 && item.file_size != received)
        || (item.total_bytes > 0 && item.total_bytes != received)
    {
        return Err(failed("download_incomplete"));
    }
    let local = browser_path_to_local(platform, filename)?;
    let bytes = read_regular(&local, MAX_AUDIO_BYTES)?;
    if bytes.len() as i64 != received {
        return Err(failed("download_incomplete"));
    }
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<') {
        return Err(failed("download_invalid"));
    }
    Ok((local, bytes))
}

#[derive(Debug)]
pub struct Acquired {
    pub item: ItemFacts,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub probe: AudioProbe,
    pub reconnects: u32,
    pub elapsed_ms: u64,
}

/// Прерывание извне (Ctrl+C) или его отсутствие.
pub async fn interrupted(interrupt: &mut Option<watch::Receiver<bool>>) {
    match interrupt {
        Some(rx) => {
            if *rx.borrow() {
                return;
            }
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
                    return;
                }
            }
            std::future::pending::<()>().await
        }
        None => std::future::pending::<()>().await,
    }
}

async fn status(bridge: &mut Bridge, op_id: &str) -> Result<StatusFacts, AssetError> {
    let value = bridge
        .call(
            "download_status",
            json!({"op_id":op_id}),
            Duration::from_secs(10),
        )
        .await?;
    serde_json::from_value(value)
        .map_err(|_| bridge_error("protocol_error", "Неизвестная структура состояния загрузки"))
}

/// Нажимает кнопку выбранной записи и доводит собственную загрузку до
/// проверенных байтов. Операцию не освобождает: это делает вызывающий через
/// [`release`] при любом исходе, в том числе после ошибки.
pub async fn acquire(
    bridge: &mut Bridge,
    platform: Platform,
    request: &Pronunciation,
    candidate: &Candidate,
    op_id: &str,
    limits: &Limits,
    interrupt: &mut Option<watch::Receiver<bool>>,
) -> Result<Acquired, AssetError> {
    let started = Instant::now();
    let begun = bridge
        .call(
            "download_begin",
            json!({"op_id":op_id,"surface":request.surface,"record_id":candidate.record_id}),
            Duration::from_secs(20),
        )
        .await
        .map_err(|e| {
            if matches!(provider_code(&e), "own_tab_used" | "operation_pending") {
                failed(provider_code(&e))
            } else {
                e
            }
        })?;
    let code = begun["code"].as_str().unwrap_or("protocol_error");
    if code != "activated" {
        tracing::warn!(
            event = "brave_download_not_activated",
            stage = "download",
            operation_id = op_id,
            code,
            "Кнопка загрузки не нажата"
        );
        return Err(if code == "download_conflict" {
            failed(code)
        } else {
            bridge_error(code, "Обычная кнопка выбранной записи недоступна")
        });
    }
    tracing::info!(event = "brave_download_activated", stage = "download", operation_id = op_id, record_id = %candidate.record_id, "Нажата обычная кнопка выбранной записи");
    let mut reconnects = 0;
    let mut last = "";
    let item = loop {
        let facts = tokio::select! {
            facts = status(bridge, op_id) => facts,
            _ = interrupted(interrupt) => return Err(AssetError::with_details(ErrorCode::Interrupted, failure_message("interrupted"),
                json!({"provider":"forvo_brave","code":"interrupted"}))),
        };
        let facts = match facts {
            Ok(facts) => facts,
            Err(e)
                if matches!(
                    provider_code(&e),
                    "extension_disconnected" | "extension_timeout"
                ) && reconnects < 3 =>
            {
                // Состояние операции живёт в расширении; после переподключения опрос продолжается.
                reconnects += 1;
                tracing::warn!(
                    event = "brave_download_reconnect",
                    stage = "download",
                    operation_id = op_id,
                    attempt = reconnects,
                    "Соединение с расширением потеряно во время загрузки"
                );
                tokio::select! {
                    result = bridge.accept(Instant::now() + RECONNECT_WAIT) => { result?; }
                    _ = interrupted(interrupt) => return Err(AssetError::with_details(ErrorCode::Interrupted, failure_message("interrupted"),
                        json!({"provider":"forvo_brave","code":"interrupted"}))),
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        match classify(&facts, request, &candidate.record_id, limits) {
            Progress::Waiting(state) => {
                if state != last {
                    tracing::info!(event = "brave_download_state", stage = "download", operation_id = op_id, state,
                        own_requests = facts.own_requests, matching_downloads = facts.matching_downloads,
                        download_id = ?facts.bound_id, elapsed_ms = facts.elapsed_ms, "Состояние собственной загрузки");
                    last = state;
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL) => {}
                    _ = interrupted(interrupt) => return Err(AssetError::with_details(ErrorCode::Interrupted, failure_message("interrupted"),
                        json!({"provider":"forvo_brave","code":"interrupted"}))),
                }
            }
            Progress::Failed(code) => {
                tracing::warn!(event = "brave_download_failed", stage = "download", operation_id = op_id, code,
                    own_requests = facts.own_requests, foreign_requests = facts.foreign_requests,
                    matching_downloads = facts.matching_downloads, download_id = ?facts.bound_id,
                    item_state = ?facts.item.as_ref().map(|i| i.state.clone()),
                    item_error = ?facts.item.as_ref().and_then(|i| i.error.clone()),
                    elapsed_ms = facts.elapsed_ms, "Собственная загрузка остановлена");
                let mut error = failed(code);
                error.details["status"] = json!(facts);
                return Err(error);
            }
            Progress::Complete(item) => break *item,
        }
    };
    tracing::info!(event = "brave_download_complete", stage = "download", operation_id = op_id, download_id = item.id,
        file_name = ?item.file_name, bytes = item.bytes_received, "Собственная загрузка завершена");
    let (_, bytes) = verify_file(platform, &item)?;
    let sha256 = sha256_hex(&bytes);
    let probe = probe_mp3(&bytes).map_err(|e| {
        let mut error = failed("probe_failed");
        error.details["probe"] = json!({"message":e.message,"details":e.details});
        error
    })?;
    tracing::info!(event = "brave_download_validated", stage = "validation", operation_id = op_id, download_id = item.id,
        sha256 = %sha256, bytes = bytes.len(), codec = %probe.codec, "Файл собственной загрузки проверен");
    Ok(Acquired {
        item,
        bytes,
        sha256,
        probe,
        reconnects,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

/// Освобождает операцию. `remove` отменяет незавершённую собственную загрузку
/// или удаляет её файл и запись истории; `keep` оставляет всё на месте.
/// Несвязанная операция ничего не затрагивает.
pub async fn release(bridge: &mut Bridge, op_id: &str, keep: bool) -> Result<Value, AssetError> {
    let value = bridge
        .call(
            "download_release",
            json!({"op_id":op_id,"action":if keep {"keep"} else {"remove"}}),
            Duration::from_secs(10),
        )
        .await?;
    tracing::info!(event = "brave_download_released", stage = "cleanup", operation_id = op_id,
        bound = ?value["bound"].as_bool(), removed_file = ?value["removed_file"].as_bool(),
        canceled = ?value["canceled"].as_bool(), erased = ?value["erased"].as_bool(), "Операция загрузки освобождена");
    Ok(value)
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
