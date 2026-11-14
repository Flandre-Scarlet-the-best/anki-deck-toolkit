//! Получение японских MP3 через обычный Brave.
//!
//! [`BraveSession`] — источник кандидатов для общего порядка попыток
//! `acquire_from`: рейтинг, переход к следующему кандидату, сведения о происхождении
//! (`ForvoEvidence`) и проверка MP3 те же, что у CDP-провайдера. Сессия держит
//! одно логическое сопряжение с расширением на всё выполнение (одиночный fetch
//! или пакет) и при необходимости переподключается; каждая загрузка — одна
//! штатная загрузка из собственной вкладки, которая затем освобождается.
//! Smoke-отчёты и копии в `evidence/` этот путь не создаёт.
use super::acceptance::{self, Connection};
use super::download::{self, Downloaded, Limits};
use super::{Bridge, Platform, bridge_error};
use crate::forvo::candidates::DomResult;
use crate::forvo::evidence::{Acquisition, CandidateSource, acquire_from};
use crate::forvo::{Candidate, provider_code};
use crate::{AssetError, AssetStore, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Сколько ждать повторного подключения расширения после разрыва между командами.
const RECONNECT_WAIT: Duration = Duration::from_secs(45);
/// Предел освобождения собственной операции.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// Конкретная авторизованная браузерная сессия, за которой закреплён пакет:
/// цель адаптера, браузер, ID расширения и его экземпляр в профиле.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserBinding {
    pub target: String,
    pub browser: super::BrowserKind,
    pub extension_id: String,
    /// `null` — пакет привязан расширением до 0.3; дополняется при первом
    /// подключении того же расширения новой версии.
    pub instance_id: Option<String>,
}

impl BrowserBinding {
    /// Совпадает ли сессия с привязкой. Пустой экземпляр привязки допускает
    /// только то же расширение (ID) той же цели.
    pub fn accepts(&self, actual: &BrowserBinding) -> bool {
        self.target == actual.target
            && self.browser == actual.browser
            && self.extension_id == actual.extension_id
            && self
                .instance_id
                .as_ref()
                .is_none_or(|id| actual.instance_id.as_ref() == Some(id))
    }
}

#[derive(Debug, Clone)]
pub struct BraveOptions {
    pub state_dir: PathBuf,
    /// Цель адаптера; `None` — цель по умолчанию (или единственная).
    pub target: Option<String>,
    /// Привязка пакета: подключение к другой сессии отклоняется до первой команды.
    pub binding: Option<BrowserBinding>,
    /// Сколько ждать первого подключения расширения.
    pub wait: Duration,
    pub limits: Limits,
    /// Закрыть собственную вкладку адаптера по завершении.
    pub close_tab: bool,
}

/// Свежая проверка строки выбранного кандидата и одна штатная загрузка до
/// проверенных байтов файла. Общая узкая операция получения и
/// диагностического `smoke`; операцию не освобождает.
pub async fn download_candidate(
    bridge: &mut Bridge,
    platform: Platform,
    surface: &str,
    candidate: &Candidate,
    op_id: &str,
    limits: &Limits,
    interrupt: &mut Option<watch::Receiver<bool>>,
) -> Result<Downloaded, AssetError> {
    let before = bridge
        .call(
            "dom",
            json!({"surface":surface,"action":"inspect"}),
            Duration::from_secs(15),
        )
        .await?;
    let before: DomResult = serde_json::from_value(before)
        .map_err(|_| bridge_error("dom_changed", "Неизвестная структура кандидатов Forvo"))?;
    if before.code != "ok" || !before.candidates.contains(candidate) {
        return Err(bridge_error(
            "candidate_changed",
            "Строка кандидата изменилась после выбора; проверьте страницу слова и повторите команду",
        ));
    }
    download::acquire_bytes(
        bridge, platform, surface, candidate, op_id, limits, interrupt,
    )
    .await
}

/// Ошибка входа/проверки сайта с действием для обычного браузера цели, а не для CDP-профиля.
/// `resume` — команда продолжения, только если операция идёт в существующем пакете;
/// одиночные `fetch`/`smoke` получают совет повторить команду.
pub(crate) fn browser_auth_error(
    mut error: AssetError,
    state: &super::AdapterState,
    resume: Option<&str>,
) -> AssetError {
    let browser = state.browser.display();
    let retry = match resume {
        Some(resume) => format!("продолжите пакет: {resume}"),
        None => "повторите команду".to_owned(),
    };
    let code = provider_code(&error).to_owned();
    let action = match code.as_str() {
        "challenge_required" => {
            format!("Откройте forvo.com в {browser}, пройдите проверку сайта вручную и {retry}")
        }
        "login_required" | "session_expired" => {
            format!("Войдите в Forvo в обычном {browser} и {retry}")
        }
        "access_denied" | "rate_limited" => {
            format!("Forvo ограничил доступ; проверьте страницу в {browser} и продолжите позже")
        }
        "save_as_required" | "save_as_suspected" => format!(
            "Откройте {} в {browser}, выключите «Спрашивать, куда сохранять каждый файл» и {retry}",
            state.browser.downloads_settings_uri()
        ),
        // Vivaldi по умолчанию спрашивает подтверждение каждой загрузки в своём интерфейсе;
        // расширению этот запрос не виден, загрузка просто не начинается.
        "download_not_started" if state.browser == super::BrowserKind::Vivaldi => format!(
            "Откройте {} в {browser}, включите «Начинать загрузки автоматически», закройте окно подтверждения загрузки, если оно открыто, и {retry}",
            state.browser.downloads_settings_uri()
        ),
        "candidate_changed" => format!(
            "Строка выбранной записи изменилась после выбора; проверьте страницу слова в {browser} и {retry}"
        ),
        _ => return error,
    };
    if matches!(
        code.as_str(),
        "challenge_required" | "login_required" | "session_expired"
    ) {
        crate::forvo::interaction::extension_required(&code, &action);
    }
    error.message = action.clone();
    error.details["provider"] = json!("forvo_brave");
    error.details["next_action"] = json!(action);
    if code == "download_not_started" {
        error.details["setting_required"] = json!(true);
    }
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

fn binding_of(connection: &Connection) -> BrowserBinding {
    BrowserBinding {
        target: connection.state.target.clone(),
        browser: connection.state.browser,
        extension_id: connection.peer.extension_id.clone(),
        instance_id: connection.peer.instance.clone(),
    }
}

/// Пакет закреплён за другой браузерной сессией: продолжение невыбранным
/// браузером, другим профилем или переустановленным расширением не допускается.
pub fn browser_mismatch(expected: &BrowserBinding, actual: &BrowserBinding) -> AssetError {
    AssetError::with_details(
        ErrorCode::InvalidTransition,
        "Пакет закреплён за другой браузерной сессией; продолжите его в исходном браузере и профиле",
        json!({"provider":"forvo_brave","code":"browser_mismatch","batch_browser":expected,"connected_browser":actual,
            "next_action":format!("{} batch-resume --batch-id <id>", super::targets::command_prefix(&expected.target))}),
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
    /// Команда продолжения, если сессия обслуживает существующий пакет.
    resume: Option<String>,
}

impl BraveSession {
    /// Подключает расширение и освобождает операцию, оставшуюся от прерванного
    /// запуска (только связанную собственную загрузку).
    pub async fn connect(options: &BraveOptions) -> Result<Self, AssetError> {
        let mut connection = acceptance::connect_with_binding(
            &options.state_dir,
            options.target.as_deref(),
            options.wait,
            options.binding.as_ref(),
        )
        .await?;
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
            resume: None,
        })
    }

    /// Сигнал прерывания: загрузка останавливается, собственная операция освобождается.
    pub fn set_interrupt(&mut self, interrupt: Option<watch::Receiver<bool>>) {
        self.interrupt = interrupt;
    }

    /// Сессия обслуживает пакет: советы пользователю ведут к его продолжению.
    pub fn set_batch(&mut self, batch_id: &str) {
        self.resume = Some(format!(
            "{} batch-resume --batch-id {batch_id}",
            self.connection.state.command_prefix()
        ));
    }

    fn guided(&self, error: AssetError) -> AssetError {
        browser_auth_error(error, &self.connection.state, self.resume.as_deref())
    }

    pub fn binding(&self) -> BrowserBinding {
        binding_of(&self.connection)
    }

    /// Сводка без секретов и путей: расширение, операции загрузок, восстановление.
    pub fn summary(&self) -> Value {
        json!({"transport":"brave_extension","target":self.connection.state.target,"browser":self.connection.state.browser,
            "update":self.connection.update,"extension_id":self.connection.peer.extension_id,
            "extension_version":self.connection.peer.version,"downloads":self.operations.len(),
            "recovery_required":self.recovery_required(),"operations":self.operations,"recovered_previous":self.recovered,"reconnects":self.reconnects})
    }

    pub fn recovery_required(&self) -> bool {
        self.operations
            .iter()
            .any(|op| !op["release"]["error"].is_null())
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
                Err(e) => return Err(self.guided(e)),
            }
        }
        unreachable!("ограниченные попытки")
    }

    async fn ranked(&mut self, surface: &str) -> Result<Vec<Candidate>, AssetError> {
        for attempt in 0..2 {
            self.open_page(surface).await?;
            match acceptance::candidates(&mut self.connection.bridge, surface).await {
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
        surface: &str,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        if self.fresh_page.as_deref() != Some(surface) {
            self.open_page(surface).await?;
        }
        // Вкладка, нажавшая кнопку, больше не считается свежей.
        self.fresh_page = None;
        let op_id = download::new_operation_id()?;
        let platform = self.connection.state.platform;
        let mut interrupt = self.interrupt.clone();
        let outcome = download_candidate(
            &mut self.connection.bridge,
            platform,
            surface,
            candidate,
            &op_id,
            &self.limits,
            &mut interrupt,
        )
        .await;
        let outcome = outcome.map_err(|e| self.guided(e));
        // Освобождение — при любом исходе: собственная связанная загрузка
        // отменяется или удаляется; несвязанная операция ничего не трогает.
        let released = self.release(&op_id).await.and_then(|value| {
            if outcome.is_ok() {
                download::validate_release(&value, false, true)?;
            }
            Ok(value)
        });
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
        let result = if self.close_tab {
            acceptance::close_owned(&mut self.connection.bridge).await
        } else {
            Ok(json!({"tab_closed":false,"recovery_required":self.recovery_required()}))
        };
        self.connection.bridge.disconnect().await;
        result
    }
}

impl CandidateSource for BraveSession {
    async fn candidates(&mut self, surface: &str) -> Result<Vec<Candidate>, AssetError> {
        let mut interrupt = self.interrupt.clone();
        tokio::select! {
            result = self.ranked(surface) => result,
            _ = download::interrupted(&mut interrupt) => Err(interrupted_error()),
        }
    }
    async fn download(
        &mut self,
        surface: &str,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        if self.interrupt.as_ref().is_some_and(|rx| *rx.borrow()) {
            return Err(interrupted_error());
        }
        // Прерывание наблюдает сама операция загрузки и освобождает её до выхода.
        self.download_one(surface, candidate).await
    }
}

/// Тот же результат получения, что `forvo::acquire`: `Acquisition` с единым
/// `ForvoEvidence`. Вход и проверку сайта пользователь выполняет в Brave сам.
pub async fn acquire(session: &mut BraveSession, surface: &str) -> Result<Acquisition, AssetError> {
    acquire_from(session, surface).await
}

/// Оригинал без чтения сохраняется в Forvo-пакете для Selection.
pub async fn fetch(
    store: &AssetStore,
    surface: &str,
    options: &BraveOptions,
    interrupt: Option<watch::Receiver<bool>>,
) -> Result<Value, AssetError> {
    let normalized = crate::pronunciation::selection::normalize_forvo_surface(surface)?;
    let surface = normalized.as_str();
    let mut session = BraveSession::connect(options).await?;
    session.set_interrupt(interrupt);
    let before = session.snapshot().await;
    let acquired = match crate::forvo::batch::saved_original(
        store,
        surface,
        crate::forvo::batch::Transport::BraveExtension,
        Some(&session.binding()),
    ) {
        Ok(Some(source)) => serde_json::from_value(source["source"]["evidence"].clone())
            .map(|evidence| (source, evidence, true))
            .map_err(|_| {
                super::super::error(
                    "invalid_forvo_evidence",
                    "Некорректный сохранённый оригинал",
                )
            }),
        Err(error) => Err(error),
        Ok(None) => acquire(&mut session, surface)
            .await
            .and_then(|acquisition| {
                let evidence = acquisition.evidence.clone();
                let source = crate::forvo::batch::save_original_pending_cleanup(
                    store,
                    surface,
                    acquisition,
                    crate::forvo::batch::Transport::BraveExtension,
                    Some(session.binding()),
                )?;
                Ok((source, evidence, false))
            }),
    };
    let after = session.snapshot().await;
    let summary = session.summary();
    let binding = session.binding();
    let closed = session.close().await;
    let mut browser = summary;
    browser["user_tabs_untouched"] = match (&before, &after) {
        (Ok(b), Ok(a)) => json!(b == a),
        _ => Value::Null,
    };
    browser["close"] = match &closed {
        Ok(value) => value.clone(),
        Err(e) => json!({"error":provider_code(e),"recovery_required":true}),
    };
    browser["recovery_required"] = json!(match &closed {
        Ok(v) if options.close_tab => v["recovery_required"] == true,
        _ => browser["recovery_required"] == true || closed.is_err(),
    });
    let (source, evidence, reused) = acquired.map_err(|mut e| {
        e.details["browser"] = browser.clone();
        e
    })?;
    if browser["recovery_required"] == true || closed.is_err() {
        let mut e = closed.err().unwrap_or_else(|| {
            super::super::error(
                "cleanup_incomplete",
                "Оригинал сохранён; требуется освобождение собственной загрузки",
            )
        });
        e.details["recovery_required"] = json!(true);
        e.details["reused"] = json!(reused);
        e.details["next_action"] = json!(crate::forvo::batch::resume_action(
            source["batch_id"].as_str().expect("пакет"),
            Some(&binding)
        ));
        e.details["browser"] = browser;
        e.details["source"] = json!(source);
        e.details["acquisition"] = json!(evidence);
        if let Err(checkpoint) = crate::forvo::batch::require_recovery(
            store,
            source["batch_id"].as_str().expect("пакет"),
            Some(binding),
        ) {
            e.details["checkpoint_error"] =
                json!({"code":provider_code(&checkpoint),"message":checkpoint.message});
        }
        return Err(e);
    }
    crate::forvo::batch::complete_original_cleanup(
        store,
        source["batch_id"].as_str().expect("пакет"),
    )
    .map_err(|mut error| {
        error.details["source"] = source.clone();
        error.details["acquisition"] = json!(evidence);
        error
    })?;
    tracing::info!(event = "brave_fetch_acquired", stage = "save_original", sha256 = %evidence.downloaded_sha256,
        record_id = %evidence.selected_record_id, "Оригинальный MP3 сохранён без чтения");
    Ok(
        json!({"transport":"brave_extension","source":source,"acquisition":evidence,"browser":browser,"reused":reused}),
    )
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
