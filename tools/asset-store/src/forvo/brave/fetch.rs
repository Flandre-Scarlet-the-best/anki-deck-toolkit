//! Production-получение японских MP3 через обычный Brave.
//!
//! [`BraveSession`] — источник кандидатов для общего порядка попыток
//! `acquire_from`: рейтинг, переход к следующему кандидату, provenance
//! (`ForvoEvidence`) и проверка MP3 те же, что у CDP-провайдера. Сессия держит
//! одно логическое сопряжение с расширением на всё выполнение (одиночный fetch
//! или пакет) и при необходимости переподключается; каждая загрузка — одна
//! штатная загрузка из собственной вкладки, которая затем освобождается.
//! Smoke-отчёты и копии в `evidence/` этот путь не создаёт.
use super::acceptance::{self, Connection};
use super::download::{self, Downloaded, Limits};
use super::{Bridge, Platform, bridge_error};
use crate::forvo::candidates::DomResult;
use crate::forvo::evidence::{Acquisition, CandidateSource, acquire_from, import_acquisition};
use crate::forvo::{Candidate, provider_code};
use crate::pronunciation::Pronunciation;
use crate::{AssetError, AssetStore, ErrorCode};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Сколько ждать повторного подключения расширения после разрыва между командами.
const RECONNECT_WAIT: Duration = Duration::from_secs(45);
/// Предел освобождения собственной операции.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct BraveOptions {
    pub state_dir: PathBuf,
    /// Сколько ждать первого подключения расширения.
    pub wait: Duration,
    pub limits: Limits,
    /// Закрыть собственную вкладку адаптера по завершении.
    pub close_tab: bool,
}

/// Свежая проверка строки выбранного кандидата и одна штатная загрузка до
/// проверенных байтов файла. Общая узкая операция production-получения и
/// диагностического `smoke`; операцию не освобождает.
pub async fn download_candidate(
    bridge: &mut Bridge,
    platform: Platform,
    request: &Pronunciation,
    candidate: &Candidate,
    op_id: &str,
    limits: &Limits,
    interrupt: &mut Option<watch::Receiver<bool>>,
) -> Result<Downloaded, AssetError> {
    let before = bridge
        .call(
            "dom",
            json!({"surface":request.surface,"action":"inspect"}),
            Duration::from_secs(15),
        )
        .await?;
    let before: DomResult = serde_json::from_value(before)
        .map_err(|_| bridge_error("dom_changed", "Неизвестная структура кандидатов Forvo"))?;
    if before.code != "ok" || !before.candidates.contains(candidate) {
        return Err(bridge_error(
            "candidate_changed",
            "Строка кандидата изменилась после выбора; запись пропущена",
        ));
    }
    download::acquire_bytes(
        bridge, platform, request, candidate, op_id, limits, interrupt,
    )
    .await
}

/// Ошибка входа/проверки сайта с действием для обычного Brave, а не для CDP-профиля.
fn brave_auth_error(mut error: AssetError) -> AssetError {
    let action = match provider_code(&error) {
        "challenge_required" => {
            "Откройте forvo.com в Brave, пройдите проверку сайта вручную и повторите команду (пакет — forvo brave batch-resume)"
        }
        "login_required" | "session_expired" => {
            "Войдите в Forvo в обычном Brave и повторите команду (пакет — forvo brave batch-resume)"
        }
        "access_denied" | "rate_limited" => {
            "Forvo ограничил доступ; проверьте страницу в Brave и продолжите позже"
        }
        _ => return error,
    };
    error.message = action.into();
    error.details["provider"] = json!("forvo_brave");
    error.details["next_action"] = json!(action);
    error
}

fn reconnectable(error: &AssetError) -> bool {
    matches!(
        provider_code(error),
        "extension_disconnected" | "extension_timeout"
    )
}

fn interrupted_error() -> AssetError {
    AssetError::with_details(
        ErrorCode::Interrupted,
        download::failure_message("interrupted"),
        json!({"provider":"forvo_brave","code":"interrupted"}),
    )
}

/// Одно логическое сопряжение с расширением на всё выполнение.
pub struct BraveSession {
    connection: Connection,
    limits: Limits,
    close_tab: bool,
    interrupt: Option<watch::Receiver<bool>>,
    /// Слово, страница которого открыта в свежей (ещё не скачивавшей) вкладке.
    fresh_page: Option<String>,
    operations: Vec<Value>,
    recovered: Option<Value>,
    reconnects: u32,
}

impl BraveSession {
    /// Подключает расширение и освобождает операцию, оставшуюся от прерванного
    /// запуска (только связанную собственную загрузку).
    pub async fn connect(options: &BraveOptions) -> Result<Self, AssetError> {
        let mut connection = acceptance::connect(&options.state_dir, options.wait).await?;
        let ping = connection
            .bridge
            .call("ping", json!({}), Duration::from_secs(5))
            .await?;
        let recovered = match acceptance::recover_previous(&mut connection.bridge, &ping).await {
            Some(Ok(value)) => Some(value),
            Some(Err(mut e)) => {
                e.details["next_action"] = json!(
                    "Незавершённая операция прошлого запуска не освобождена; повторите команду"
                );
                return Err(e);
            }
            None => None,
        };
        tracing::info!(event = "brave_session_connected", stage = "transport", extension_version = %connection.peer.version,
            recovered_previous = recovered.is_some(), "Сессия Brave подключена");
        Ok(Self {
            connection,
            limits: options.limits,
            close_tab: options.close_tab,
            interrupt: None,
            fresh_page: None,
            operations: Vec::new(),
            recovered,
            reconnects: 0,
        })
    }

    /// Сигнал прерывания: загрузка останавливается, собственная операция освобождается.
    pub fn set_interrupt(&mut self, interrupt: Option<watch::Receiver<bool>>) {
        self.interrupt = interrupt;
    }

    /// Сводка без секретов и путей: расширение, операции загрузок, восстановление.
    pub fn summary(&self) -> Value {
        json!({"transport":"brave_extension","extension_id":self.connection.peer.extension_id,
            "extension_version":self.connection.peer.version,"downloads":self.operations.len(),
            "operations":self.operations,"recovered_previous":self.recovered,"reconnects":self.reconnects})
    }

    pub async fn snapshot(&mut self) -> Result<Value, AssetError> {
        self.connection
            .bridge
            .call("snapshot", json!({}), Duration::from_secs(5))
            .await
            .map(|v| acceptance::structure(&v))
    }

    async fn reconnect(&mut self) -> Result<(), AssetError> {
        self.reconnects += 1;
        tracing::warn!(
            event = "brave_session_reconnect",
            stage = "transport",
            attempt = self.reconnects,
            "Соединение с расширением потеряно; ожидается переподключение"
        );
        self.fresh_page = None;
        self.connection
            .bridge
            .accept(Instant::now() + RECONNECT_WAIT)
            .await
            .map(|_| ())
    }

    /// Свежая собственная вкладка на точной странице слова с подтверждённым входом.
    /// Использованная вкладка заменяется расширением в той же группе.
    async fn open_page(&mut self, surface: &str) -> Result<(), AssetError> {
        for attempt in 0..2 {
            match acceptance::open_word(&mut self.connection.bridge, surface).await {
                Ok(_) => {
                    self.fresh_page = Some(surface.to_owned());
                    return Ok(());
                }
                Err(e) if attempt == 0 && reconnectable(&e) => self.reconnect().await?,
                Err(e) => return Err(brave_auth_error(e)),
            }
        }
        unreachable!("ограниченные попытки")
    }

    async fn ranked(&mut self, request: &Pronunciation) -> Result<Vec<Candidate>, AssetError> {
        for attempt in 0..2 {
            self.open_page(&request.surface).await?;
            match acceptance::candidates(&mut self.connection.bridge, request).await {
                Err(e) if attempt == 0 && reconnectable(&e) => self.reconnect().await?,
                other => return other,
            }
        }
        unreachable!("ограниченные попытки")
    }

    async fn release(&mut self, op_id: &str) -> Result<Value, AssetError> {
        for attempt in 0..2 {
            let result = tokio::time::timeout(
                RELEASE_TIMEOUT,
                download::release(&mut self.connection.bridge, op_id, false),
            )
            .await
            .unwrap_or_else(|_| {
                Err(bridge_error(
                    "extension_timeout",
                    "Расширение не освободило операцию вовремя",
                ))
            });
            match result {
                Err(e) if attempt == 0 && reconnectable(&e) => self.reconnect().await?,
                other => return other,
            }
        }
        unreachable!("ограниченные попытки")
    }

    async fn download_one(
        &mut self,
        request: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        if self.fresh_page.as_deref() != Some(request.surface.as_str()) {
            self.open_page(&request.surface).await?;
        }
        // Вкладка, нажавшая кнопку, больше не считается свежей.
        self.fresh_page = None;
        let op_id = download::new_operation_id()?;
        let platform = self.connection.state.platform;
        let mut interrupt = self.interrupt.clone();
        let outcome = download_candidate(
            &mut self.connection.bridge,
            platform,
            request,
            candidate,
            &op_id,
            &self.limits,
            &mut interrupt,
        )
        .await;
        // Освобождение — при любом исходе: собственная связанная загрузка
        // отменяется или удаляется; несвязанная операция ничего не трогает.
        let released = self.release(&op_id).await;
        let mut entry = json!({"operation_id":op_id,"record_id":candidate.record_id,"code":match &outcome {
            Ok(_) => "downloaded".to_owned(), Err(e) => provider_code(e).to_owned() }});
        if let Ok(d) = &outcome {
            entry["download_id"] = json!(d.item.id);
            entry["sha256"] = json!(d.sha256);
            entry["byte_length"] = json!(d.bytes.len());
            entry["reconnects"] = json!(d.reconnects);
        }
        entry["release"] = match &released {
            Ok(value) => value.clone(),
            Err(e) => json!({"error":provider_code(e)}),
        };
        self.operations.push(entry);
        match (outcome, released) {
            (Ok(downloaded), Ok(_)) => Ok(downloaded.bytes),
            (Ok(downloaded), Err(e)) => {
                // Байты уже прочитаны и проверены; неосвобождённая операция
                // остаётся в расширении и освобождается при следующем подключении.
                tracing::warn!(event = "brave_release_deferred", stage = "cleanup", operation_id = %op_id, code = provider_code(&e),
                    "Собственная операция будет освобождена при следующем подключении");
                Ok(downloaded.bytes)
            }
            (Err(mut e), released) => {
                if let Err(release) = released {
                    e.details["release"] = json!({"code":provider_code(&release),
                        "next_action":"собственная операция будет освобождена при следующем подключении"});
                }
                Err(e)
            }
        }
    }

    /// Закрывает только соединение bridge и, по контракту `close_tab`,
    /// собственную вкладку адаптера. Brave и вкладки пользователя не трогаются.
    pub async fn close(mut self) -> Result<Value, AssetError> {
        let mut result = json!({"tab_closed":false});
        if self.close_tab {
            match self
                .connection
                .bridge
                .call("close", json!({}), Duration::from_secs(10))
                .await
            {
                Ok(value) => result["tab_closed"] = value["closed"].clone(),
                Err(e) => result["tab_close_error"] = json!(provider_code(&e)),
            }
        }
        self.connection.bridge.disconnect().await;
        Ok(result)
    }
}

impl CandidateSource for BraveSession {
    async fn candidates(&mut self, request: &Pronunciation) -> Result<Vec<Candidate>, AssetError> {
        let mut interrupt = self.interrupt.clone();
        tokio::select! {
            result = self.ranked(request) => result,
            _ = download::interrupted(&mut interrupt) => Err(interrupted_error()),
        }
    }
    async fn download(
        &mut self,
        request: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        if self.interrupt.as_ref().is_some_and(|rx| *rx.borrow()) {
            return Err(interrupted_error());
        }
        // Прерывание наблюдает сама операция загрузки и освобождает её до выхода.
        self.download_one(request, candidate).await
    }
}

/// Тот же результат получения, что `forvo::acquire`: `Acquisition` с единым
/// `ForvoEvidence`. Вход и проверку сайта пользователь выполняет в Brave сам.
pub async fn acquire(
    session: &mut BraveSession,
    request: &Pronunciation,
) -> Result<Acquisition, AssetError> {
    acquire_from(session, request).await
}

/// Одиночный production fetch: одно сопряжение, кандидаты, загрузка выбранного
/// оригинального MP3 и импорт через `import_acquisition` с прежним уровнем доверия.
pub async fn fetch(
    store: &AssetStore,
    request: &Pronunciation,
    replace_expected: Option<String>,
    options: &BraveOptions,
    interrupt: Option<watch::Receiver<bool>>,
) -> Result<Value, AssetError> {
    let mut session = BraveSession::connect(options).await?;
    session.set_interrupt(interrupt);
    let before = session.snapshot().await;
    let acquired = acquire(&mut session, request).await;
    let after = session.snapshot().await;
    let summary = session.summary();
    let closed = session.close().await;
    let mut browser = summary;
    browser["user_tabs_untouched"] = match (&before, &after) {
        (Ok(b), Ok(a)) => json!(b == a),
        _ => Value::Null,
    };
    browser["close"] = closed.unwrap_or(Value::Null);
    let acquisition = acquired.map_err(|mut e| {
        e.details["browser"] = browser.clone();
        e
    })?;
    let asset =
        import_acquisition(store, request, &acquisition, replace_expected).map_err(|mut e| {
            e.details["browser"] = browser.clone();
            e
        })?;
    tracing::info!(event = "brave_fetch_imported", stage = "import", sha256 = %acquisition.evidence.downloaded_sha256,
        record_id = %acquisition.evidence.selected_record_id, "Оригинальный MP3 импортирован");
    Ok(
        json!({"transport":"brave_extension","asset":asset,"acquisition":acquisition.evidence,"browser":browser}),
    )
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
