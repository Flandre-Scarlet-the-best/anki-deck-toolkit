//! Forvo-пакет сохраняет полученные байты до импорта, используя единственный SafeBatchRuntime.
use super::brave::fetch::{self as brave_fetch, BraveOptions, BraveSession, BrowserBinding};
use super::{
    Acquisition, ForvoEvidence, ForvoSession, acquire, import_acquisition, pauses_batch,
    provider_code,
};
use crate::{
    AssetError, AssetStore, ErrorCode,
    batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime},
    browser_diagnostics::{BrowserItemContext, BrowserItemTimer},
    pronunciation::{MAX_AUDIO_BYTES, Pronunciation, batch::MAX_BATCH_ITEMS},
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Браузерный транспорт, которым пакет получает записи. Это не происхождение
/// аудио: оно для обоих транспортов одно — `provider=forvo_web` и единый
/// `ForvoEvidence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Собственный постоянный профиль Chromium через CDP (`forvo batch-*`).
    CdpChromium,
    /// Обычный браузер пользователя через MV3-расширение (`forvo brave|browser
    /// batch-*`). Имя историческое: транспорт один для всех Chromium-целей,
    /// а конкретная сессия закреплена полем `browser` пакета.
    BraveExtension,
}
impl Transport {
    pub fn resume_command(self) -> &'static str {
        match self {
            Self::CdpChromium => "pronunciation-assets forvo batch-resume",
            Self::BraveExtension => "pronunciation-assets forvo brave batch-resume",
        }
    }
}

/// Источник для `resume`. Транспорт пакета закреплён в состоянии, и resume
/// другим транспортом отвергается до запуска браузера.
pub enum Backend<'a> {
    Cdp { profile: &'a Path },
    Brave(&'a BraveOptions),
}
impl Backend<'_> {
    pub fn transport(&self) -> Transport {
        match self {
            Self::Cdp { .. } => Transport::CdpChromium,
            Self::Brave(_) => Transport::BraveExtension,
        }
    }
}

/// Сколько исполнитель Brave ждёт штатного освобождения собственной загрузки
/// после Ctrl+C или `batch-cancel`, прежде чем оставить её на восстановление.
const BRAVE_INTERRUPT_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchPlan {
    pub schema_version: u32,
    pub items: Vec<FetchItem>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchItem {
    pub surface: String,
    pub reading: String,
    pub replace_expected_sha256: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recovery_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<CandidateReport>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateReport {
    pub candidates: Vec<super::Candidate>,
    pub attempts: Vec<super::Attempt>,
}

impl From<AssetError> for Failure {
    fn from(e: AssetError) -> Self {
        let report = match (
            serde_json::from_value(e.details["candidates"].clone()),
            serde_json::from_value(e.details["attempts"].clone()),
        ) {
            (Ok(candidates), Ok(attempts)) => Some(CandidateReport {
                candidates,
                attempts,
            }),
            _ => None,
        };
        Self {
            code: e.details["blocker"]
                .as_str()
                .or_else(|| {
                    e.details["blockers"]
                        .as_array()
                        .and_then(|values| values.first())
                        .and_then(|value| value.as_str())
                })
                .unwrap_or_else(|| provider_code(&e))
                .into(),
            recovery_required: e.details["recovery_required"] == true,
            message: e.message,
            report,
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAcquisition {
    pub blob: RuntimeBlobRef,
    pub evidence: ForvoEvidence,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub request: Pronunciation,
    pub replace_expected_sha256: Option<String>,
    pub acquisition: Option<StoredAcquisition>,
    pub outcome: Option<Failure>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForvoBatch {
    /// 1 — пакет без поля `transport` (до Brave): всегда CDP, состояние не
    /// переписывается в новую версию. 2 — транспорт закреплён явно. 3 —
    /// транспорт расширения с закреплённой браузерной сессией (`browser`). 4 —
    /// цель расширения (`target`) сохраняется ещё до первого подключения.
    pub schema_version: u32,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// Браузерная сессия пакета расширения: заполняется при первом подключении
    /// и дальше не меняется (кроме дополнения экземпляра того же расширения).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserBinding>,
    /// Выбранная цель пакета версии 4; сохраняется до подключения расширения.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub batch_id: String,
    pub revision: u64,
    pub cancelled: bool,
    pub paused: Option<Failure>,
    pub items: Vec<Item>,
}
fn invalid() -> AssetError {
    AssetError::new(
        ErrorCode::ManifestCorrupt,
        "некорректное состояние Forvo-пакета",
    )
}
fn valid_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl ForvoBatch {
    /// Фактический транспорт: у состояния версии 1 — CDP.
    pub fn transport(&self) -> Transport {
        self.transport.unwrap_or(Transport::CdpChromium)
    }
}
impl RuntimeBatchState for ForvoBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn maximum_blob_bytes(&self) -> u64 {
        MAX_AUDIO_BYTES
    }
    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.items
            .iter()
            .filter_map(|i| i.acquisition.as_ref().map(|a| a.blob.clone()))
            .collect()
    }
    fn validate(&self) -> Result<(), AssetError> {
        let transport_ok = match (
            self.schema_version,
            self.transport,
            &self.browser,
            self.target.as_deref(),
        ) {
            (1, None, None, None) | (2, Some(_), None, None) => true,
            (3, Some(Transport::BraveExtension), _, None) => true,
            (4, Some(Transport::BraveExtension), browser, Some(target)) => {
                super::brave::targets::valid_target_name(target)
                    && browser
                        .as_ref()
                        .is_none_or(|binding| binding.target == target)
            }
            _ => false,
        };
        if !transport_ok
            || self.provider != "forvo_web"
            || self.items.is_empty()
            || self.items.len() > MAX_BATCH_ITEMS
            || self.revision > i64::MAX as u64
        {
            return Err(invalid());
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            if Pronunciation::from_identity(&item.request.identity())? != item.request
                || !seen.insert(item.request.identity())
                || item
                    .replace_expected_sha256
                    .as_deref()
                    .is_some_and(|s| !valid_sha(s))
            {
                return Err(invalid());
            }
            if let Some(acquisition) = &item.acquisition {
                acquisition
                    .evidence
                    .validate(&item.request, &acquisition.blob.sha256)?;
            }
            if let Some(outcome) = &item.outcome
                && (outcome.code.is_empty()
                    || outcome.code.len() > 80
                    || outcome.message.is_empty()
                    || outcome.message.len() > 2048
                    || outcome.code == "imported" && item.acquisition.is_none())
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}
pub fn start(
    store: &AssetStore,
    id: &str,
    plan: FetchPlan,
    transport: Transport,
) -> Result<ForvoBatch, AssetError> {
    start_inner(store, id, plan, transport, None)
}

/// Создаёт пакет расширения с уже разрешённой целью до первого подключения.
pub(crate) fn start_with_browser_target(
    store: &AssetStore,
    id: &str,
    plan: FetchPlan,
    target: String,
) -> Result<ForvoBatch, AssetError> {
    if !super::brave::targets::valid_target_name(&target) {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "некорректная цель браузера для Forvo-пакета",
        ));
    }
    start_inner(store, id, plan, Transport::BraveExtension, Some(target))
}

fn start_inner(
    store: &AssetStore,
    id: &str,
    plan: FetchPlan,
    transport: Transport,
    target: Option<String>,
) -> Result<ForvoBatch, AssetError> {
    if plan.schema_version != 1 || plan.items.is_empty() || plan.items.len() > MAX_BATCH_ITEMS {
        return Err(invalid());
    }
    if transport != Transport::BraveExtension && target.is_some() {
        return Err(invalid());
    }
    let items = plan
        .items
        .into_iter()
        .map(|i| {
            Ok(Item {
                request: Pronunciation::new(&i.surface, &i.reading)?,
                replace_expected_sha256: i.replace_expected_sha256,
                acquisition: None,
                outcome: None,
            })
        })
        .collect::<Result<Vec<_>, AssetError>>()?;
    let batch = ForvoBatch {
        schema_version: match transport {
            Transport::BraveExtension if target.is_some() => 4,
            Transport::BraveExtension => 3,
            Transport::CdpChromium => 2,
        },
        provider: "forvo_web".into(),
        transport: Some(transport),
        browser: None,
        target,
        batch_id: id.into(),
        revision: 0,
        cancelled: false,
        paused: None,
        items,
    };
    batch.validate()?;
    let mut runtime = SafeBatchRuntime::open(store.root(), id)?;
    if runtime.load::<ForvoBatch>()?.is_some() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "пакет существует; используйте batch-resume его транспорта",
        ));
    }
    runtime.save(&batch)?;
    Ok(batch)
}
fn load(runtime: &mut SafeBatchRuntime) -> Result<ForvoBatch, AssetError> {
    runtime.load()?.ok_or_else(invalid)
}
pub(super) fn reload_after_acquisition(
    runtime: &mut SafeBatchRuntime,
) -> Result<ForvoBatch, AssetError> {
    runtime.reload_cached()?.ok_or_else(invalid)
}
pub fn status(store: &AssetStore, id: &str) -> Result<ForvoBatch, AssetError> {
    load(&mut SafeBatchRuntime::open(store.root(), id)?)
}
pub fn cancel(store: &AssetStore, id: &str) -> Result<ForvoBatch, AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), id)?;
    let mut batch = load(&mut runtime)?;
    if !batch.cancelled || batch.paused.is_some() {
        batch.cancelled = true;
        batch.paused = None;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}
pub(super) async fn cancellation(store: &AssetStore, id: &str) -> Result<(), AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), id)?;
    let mut batch = load(&mut runtime)?;
    loop {
        if batch.cancelled {
            return Ok(());
        }
        runtime.release_lock()?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        runtime.reacquire_lock()?;
        batch = runtime.reload_cached()?.ok_or_else(invalid)?;
    }
}

fn retryable_import(error: &AssetError) -> bool {
    const BLOCKERS: &[&str] = &[
        "audio_tool_missing",
        "audio_tool_start_failed",
        "audio_tool_runtime_failed",
        "audio_temp_io_failure",
        "io_failure",
    ];
    error.code == ErrorCode::IoFailure
        || error.details["blocker"]
            .as_str()
            .is_some_and(|code| BLOCKERS.contains(&code))
        || error.details["blockers"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|code| BLOCKERS.contains(&code)))
        })
}

fn transport_mismatch(id: &str, batch: &ForvoBatch, requested: Transport) -> AssetError {
    let command = match batch.transport() {
        Transport::CdpChromium => Transport::CdpChromium.resume_command().to_owned(),
        Transport::BraveExtension => batch
            .browser
            .as_ref()
            .map(|binding| binding.target.as_str())
            .or(batch.target.as_deref())
            .map(|target| {
                format!(
                    "{} batch-resume",
                    super::brave::targets::command_prefix(target)
                )
            })
            .unwrap_or_else(|| Transport::BraveExtension.resume_command().to_owned()),
    };
    AssetError::with_details(
        ErrorCode::InvalidTransition,
        "Пакет закреплён за другим браузерным транспортом; продолжите его командой исходного транспорта",
        serde_json::json!({"provider":"forvo_web","code":"transport_mismatch","batch_id":id,
            "batch_transport":batch.transport(),"requested_transport":requested,
            "next_action":format!("{command} --batch-id {id}")}),
    )
}

/// Цель и привязка для продолжения пакета расширения. Пакет версии 2 (до
/// выбора браузера) выполнялся только целью `brave`; привязанный пакет —
/// только своей целью и своим экземпляром расширения. Несовпадение — отказ
/// до подключения к браузеру.
fn bound_options(
    id: &str,
    batch: &ForvoBatch,
    options: &BraveOptions,
) -> Result<BraveOptions, AssetError> {
    let required = match (&batch.browser, batch.schema_version) {
        (Some(binding), _) => Some(binding.target.clone()),
        (None, 2) => Some(super::brave::LEGACY_TARGET.to_owned()),
        (None, 4) => batch.target.clone(),
        (None, _) => None,
    };
    if let (Some(required), Some(requested)) = (&required, &options.target)
        && required != requested
    {
        return Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            "Пакет закреплён за другой целью браузера; продолжите его исходной целью",
            serde_json::json!({"provider":"forvo_brave","code":"browser_mismatch","batch_id":id,
                "batch_target":required,"requested_target":requested,
                "next_action":format!("{} batch-resume --batch-id {id}", super::brave::targets::command_prefix(required))}),
        ));
    }
    let mut options = options.clone();
    options.target = options.target.or(required);
    options.binding = batch.browser.clone();
    Ok(options)
}

/// Сессия выбранного транспорта; открывается лениво, только если элементу нужны байты.
enum Session {
    Cdp(Box<ForvoSession>),
    Brave(Box<BraveSession>),
}

impl Session {
    async fn close(self) -> Result<(), AssetError> {
        match self {
            Self::Cdp(session) => (*session).close().await,
            Self::Brave(session) => session.close().await.map(|_| ()),
        }
    }
}

fn with_batch_resume_action(
    mut error: AssetError,
    id: &str,
    binding: Option<&BrowserBinding>,
) -> AssetError {
    if provider_code(&error) == "browser_mismatch"
        && let Some(binding) = binding
    {
        error.details["next_action"] = serde_json::json!(format!(
            "{} batch-resume --batch-id {id}",
            super::brave::targets::command_prefix(&binding.target)
        ));
    }
    error
}

async fn launch(
    store: &AssetStore,
    id: &str,
    backend: &Backend<'_>,
) -> Result<Session, AssetError> {
    match backend {
        Backend::Cdp { profile } => {
            enum LaunchResult {
                Finished(Box<Result<ForvoSession, AssetError>>),
                Interrupted(AssetError),
            }
            let mut launching = Box::pin(ForvoSession::launch(profile));
            match tokio::select! {
                result = &mut launching => LaunchResult::Finished(Box::new(result)),
                _ = tokio::signal::ctrl_c() => LaunchResult::Interrupted(AssetError::new(ErrorCode::Interrupted, "Запуск Forvo прерван; продолжите через batch-resume")),
                result = cancellation(store, id) => LaunchResult::Interrupted(result.err().unwrap_or_else(|| AssetError::new(ErrorCode::Interrupted, "Forvo-пакет отменён"))),
            } {
                LaunchResult::Finished(result) => (*result).map(|s| Session::Cdp(Box::new(s))),
                LaunchResult::Interrupted(mut interrupted) => {
                    let cleanup = match launching.await {
                        Ok(s) => s.close().await,
                        Err(e) if provider_code(&e) == "browser_cleanup_failed" => Err(e),
                        Err(_) => Ok(()),
                    };
                    if let Err(e) = cleanup {
                        interrupted.details["browser_cleanup"] =
                            serde_json::json!({"code":provider_code(&e),"message":e.message});
                    }
                    Err(interrupted)
                }
            }
        }
        // Подключение к расширению безопасно прервать: до первой команды в
        // браузере ничего не изменено.
        Backend::Brave(options) => tokio::select! {
            result = BraveSession::connect(options) => result
                .map(|mut s| {
                    s.set_batch(id);
                    Session::Brave(Box::new(s))
                })
                .map_err(|error| with_batch_resume_action(error, id, options.binding.as_ref())),
            _ = tokio::signal::ctrl_c() => Err(AssetError::new(ErrorCode::Interrupted, "Подключение к браузеру прервано; продолжите через batch-resume цели")),
            result = cancellation(store, id) => Err(result.err().unwrap_or_else(|| AssetError::new(ErrorCode::Interrupted, "Forvo-пакет отменён"))),
        },
    }
}

/// Получение одного элемента с наблюдением Ctrl+C и `batch-cancel`.
///
/// CDP сохраняет прежнее поведение: future получения сбрасывается, а браузер
/// закрывается вместе с сессией. Brave не бросает операцию посреди загрузки:
/// сигнал передаётся сессии, которая отменяет и стирает только собственную
/// связанную загрузку; если она не успела за [`BRAVE_INTERRUPT_GRACE`],
/// незавершённая операция освобождается при следующем подключении.
/// Второе значение — сигнал остановки, пришедший во время получения.
async fn acquire_item(
    session: &mut Session,
    store: &AssetStore,
    id: &str,
    request: &Pronunciation,
) -> (Result<Acquisition, AssetError>, Option<AssetError>) {
    match session {
        Session::Cdp(session) => (
            tokio::select! {
                r = acquire(session, request) => r,
                _ = tokio::signal::ctrl_c() => Err(AssetError::new(ErrorCode::Interrupted, "Forvo-пакет прерван; продолжите через batch-resume")),
                r = cancellation(store, id) => { if let Err(e) = r { Err(e) } else { Err(AssetError::new(ErrorCode::Interrupted, "Forvo-пакет отменён")) } },
            },
            None,
        ),
        Session::Brave(session) => {
            let (tx, rx) = tokio::sync::watch::channel(false);
            session.set_interrupt(Some(rx));
            let work = brave_fetch::acquire(session, request);
            tokio::pin!(work);
            let stop = tokio::select! {
                r = &mut work => return (r, None),
                _ = tokio::signal::ctrl_c() => AssetError::with_details(ErrorCode::Interrupted, "Forvo-пакет прерван; продолжите через batch-resume цели",
                    serde_json::json!({"provider":"forvo_brave","code":"interrupted"})),
                r = cancellation(store, id) => match r {
                    Err(e) => e,
                    Ok(()) => AssetError::with_details(ErrorCode::Interrupted, "Forvo-пакет отменён",
                        serde_json::json!({"provider":"forvo_brave","code":"interrupted"})),
                },
            };
            tracing::warn!(
                event = "brave_batch_stop_requested",
                stage = "acquire",
                code = provider_code(&stop),
                "Получена остановка; собственная операция загрузки освобождается"
            );
            let _ = tx.send(true);
            finish_interrupted(&mut work, stop, BRAVE_INTERRUPT_GRACE).await
        }
    }
}

async fn finish_interrupted(
    work: impl std::future::Future<Output = Result<Acquisition, AssetError>>,
    stop: AssetError,
    grace: Duration,
) -> (Result<Acquisition, AssetError>, Option<AssetError>) {
    match tokio::time::timeout(grace, work).await {
        Ok(result) => (result, Some(stop)),
        Err(_) => {
            let mut stop = stop;
            stop.details["recovery_required"] = serde_json::json!(true);
            stop.message = format!(
                "{}; собственная загрузка не освобождена вовремя и будет освобождена при следующем подключении",
                stop.message
            );
            (
                Err(AssetError::with_details(
                    stop.code,
                    stop.message.clone(),
                    stop.details.clone(),
                )),
                Some(stop),
            )
        }
    }
}

/// Единая граница фиксации результата получения; до Ready импорт невозможен.
pub(super) enum AcquisitionProgress {
    Ready,
    Paused,
    Rejected,
}
pub(super) fn checkpoint_acquisition(
    runtime: &mut SafeBatchRuntime,
    batch: &mut ForvoBatch,
    index: usize,
    acquisition: Result<Acquisition, AssetError>,
) -> Result<AcquisitionProgress, AssetError> {
    let progress = match acquisition {
        Ok(acquisition) => {
            let blob =
                runtime.persist_blob_with_limit(&acquisition.bytes, "mp3", MAX_AUDIO_BYTES)?;
            batch.items[index].acquisition = Some(StoredAcquisition {
                blob,
                evidence: acquisition.evidence,
            });
            batch.paused = None;
            AcquisitionProgress::Ready
        }
        Err(e) if pauses_batch(&e) || e.code == ErrorCode::Interrupted => {
            tracing::warn!(
                event = "forvo_batch_paused",
                stage = "acquire",
                code = provider_code(&e),
                "Пакет Forvo приостановлен"
            );
            batch.paused = Some(e.into());
            AcquisitionProgress::Paused
        }
        Err(e) => {
            batch.items[index].outcome = Some(e.into());
            batch.paused = None;
            AcquisitionProgress::Rejected
        }
    };
    batch.revision += 1;
    runtime.save(batch)?;
    Ok(progress)
}

pub async fn resume(
    store: &AssetStore,
    id: &str,
    backend: Backend<'_>,
    max_items: usize,
) -> Result<ForvoBatch, AssetError> {
    resume_prepared(store, id, backend, max_items, None).await
}

/// Как [`resume`], но с уже запущенной собственной CDP-сессией: её открыл
/// preflight общего пути до создания пакета, повторный запуск Chromium не нужен.
/// Сессия закрывается при любом исходе.
pub async fn resume_prepared(
    store: &AssetStore,
    id: &str,
    backend: Backend<'_>,
    max_items: usize,
    prepared: Option<ForvoSession>,
) -> Result<ForvoBatch, AssetError> {
    let mut prepared = prepared;
    let early = async {
        if prepared.is_some() && backend.transport() != Transport::CdpChromium {
            return Err(invalid());
        }
        let mut runtime = SafeBatchRuntime::open(store.root(), id)?;
        let lease = runtime.execution_lease()?;
        let batch = load(&mut runtime)?;
        if batch.transport() != backend.transport() {
            return Err(transport_mismatch(id, &batch, backend.transport()));
        }
        Ok((runtime, lease, batch))
    }
    .await;
    let (mut runtime, _lease, mut batch) = match early {
        Ok(parts) => parts,
        Err(mut error) => {
            if let Some(session) = prepared.take()
                && let Err(cleanup) = session.close().await
            {
                error.details["browser_cleanup"] = serde_json::json!({
                    "code": provider_code(&cleanup),
                    "message": cleanup.message,
                    "details": cleanup.details,
                });
            }
            return Err(error);
        }
    };
    let mut session: Option<Session> = prepared.take().map(|s| Session::Cdp(Box::new(s)));
    let result = async {
        let adjusted;
        let backend = match backend {
            Backend::Brave(options) => {
                adjusted = bound_options(id, &batch, options)?;
                Backend::Brave(&adjusted)
            }
            other => other,
        };
        if batch.cancelled {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "пакет отменён",
            ));
        }
        if batch.paused.as_ref().is_some_and(|p| !p.recovery_required)
            && batch.items.iter().all(|item| item.outcome.is_some())
        {
            batch.paused = None;
            batch.revision += 1;
            runtime.save(&batch)?;
        }
        // Сохранённые байты не требуют скачивания, но отложенное освобождение
        // всё равно требует сопряжения до импорта.
        if batch.paused.as_ref().is_some_and(|p| p.recovery_required) {
            runtime.release_lock()?;
            let connected = launch(store, id, &backend).await.map(|s| {
                session = Some(s);
            });
            runtime.reacquire_lock()?;
            batch = reload_after_acquisition(&mut runtime)?;
            connected?;
            batch.paused = None;
            batch.revision += 1;
            runtime.save(&batch)?;
        }
        let mut done = 0;
        while let Some(index) = batch.items.iter().position(|i| i.outcome.is_none()) {
            if done >= max_items || batch.cancelled {
                break;
            }
            let timer = BrowserItemTimer::new(BrowserItemContext::without_generation(
                "forvo_web",
                &batch.items[index].request.surface,
                1,
            ));
            if batch.items[index].acquisition.is_none() {
                let stage = timer.stage("acquire");
                runtime.release_lock()?;
                let launched = if session.is_none() {
                    launch(store, id, &backend).await.map(|s| {
                        session = Some(s);
                    })
                } else {
                    Ok(())
                };
                let (mut acquisition, mut stop) = match launched {
                    Err(e) => (Err(e), None),
                    Ok(()) => {
                        acquire_item(
                            session.as_mut().expect("session"),
                            store,
                            id,
                            &batch.items[index].request,
                        )
                        .await
                    }
                };
                if session.as_ref().is_some_and(|s| matches!(s, Session::Brave(s) if s.recovery_required())) {
                    if let Err(e) = &mut acquisition {
                        e.details["recovery_required"] = serde_json::json!(true);
                    }
                    if let Some(e) = &mut stop {
                        e.details["recovery_required"] = serde_json::json!(true);
                    } else if acquisition.is_ok() {
                        stop = Some(AssetError::with_details(
                            ErrorCode::Interrupted,
                            "Проверенные байты сохранены; требуется освобождение собственной загрузки при следующем сопряжении",
                            serde_json::json!({"code":"cleanup_incomplete","recovery_required":true}),
                        ));
                    }
                }
                match &acquisition {
                    Ok(_) => stage.finish_success(),
                    Err(e) => stage.finish_failure(provider_code(e), pauses_batch(e), None),
                }
                runtime.reacquire_lock()?;
                batch = reload_after_acquisition(&mut runtime)?;
                // Первая подключённая сессия закрепляется за пакетом.
                if let Some(Session::Brave(brave)) = &session {
                    let actual = brave.binding();
                    let record = match &batch.browser {
                        None => true,
                        Some(bound) => bound.instance_id.is_none() && actual.instance_id.is_some() && bound.accepts(&actual),
                    };
                    if record {
                        tracing::info!(event = "forvo_batch_browser_bound", stage = "acquire", target = %actual.target,
                            browser = actual.browser.id(), "Пакет закреплён за браузерной сессией");
                        batch.browser = Some(actual);
                        if batch.schema_version == 2 {
                            batch.schema_version = 3;
                        }
                        batch.revision += 1;
                        runtime.save(&batch)?;
                    }
                }
                if let Err(e) = &acquisition
                    && !e.details["browser_cleanup"].is_null()
                {
                    return Err(AssetError::with_details(
                        e.code,
                        e.message.clone(),
                        e.details.clone(),
                    ));
                }
                if batch.cancelled {
                    break;
                }
                // Остановка во время получения: проверенные байты всё равно
                // фиксируются, затем пакет приостанавливается сигналом остановки.
                let (acquisition, stop) = match (acquisition, stop) {
                    (Err(_), Some(stop)) => (Err(stop), None),
                    (acquisition, stop) => (acquisition, stop),
                };
                let mut failure_code =
                    acquisition.as_ref().err().map(|e| provider_code(e).to_owned());
                let mut progress =
                    checkpoint_acquisition(&mut runtime, &mut batch, index, acquisition)?;
                if let (AcquisitionProgress::Ready, Some(stop)) = (&progress, stop) {
                    failure_code = Some(provider_code(&stop).to_owned());
                    progress = checkpoint_acquisition(&mut runtime, &mut batch, index, Err(stop))?;
                }
                match progress {
                    AcquisitionProgress::Ready => (),
                    AcquisitionProgress::Paused => {
                        timer.finish_failure(failure_code.as_deref().expect("ошибка получения"), true, None);
                        break;
                    }
                    AcquisitionProgress::Rejected => {
                        timer.finish_failure(failure_code.as_deref().expect("ошибка получения"), false, None);
                        done += 1;
                        continue;
                    }
                }
            }
            let item = &batch.items[index];
            let stage = timer.stage("import_mp3");
            let saved = item.acquisition.as_ref().expect("checkpoint");
            let result = runtime.read_blob(&saved.blob).and_then(|bytes| {
                import_acquisition(
                    store,
                    &item.request,
                    &Acquisition {
                        bytes,
                        evidence: saved.evidence.clone(),
                    },
                    item.replace_expected_sha256.clone(),
                )
            });
            if let Err(e) = &result
                && retryable_import(e)
            {
                let failure = Failure::from(AssetError::with_details(
                    e.code,
                    e.message.clone(),
                    e.details.clone(),
                ));
                stage.finish_failure(&failure.code, true, None);
                timer.finish_failure(&failure.code, true, None);
                tracing::warn!(
                    event = "forvo_batch_paused",
                    stage = "import_mp3",
                    code = failure.code,
                    "Импорт сохранённого MP3 приостановлен"
                );
                batch.paused = Some(failure);
                batch.revision += 1;
                runtime.save(&batch)?;
                break;
            }
            match &result {
                Ok(_) => stage.finish_success(),
                Err(e) => stage.finish_failure(provider_code(e), false, None),
            }
            match &result {
                Ok(_) => timer.finish_success(),
                Err(e) => timer.finish_failure(provider_code(e), false, None),
            }
            batch.items[index].outcome = Some(match result {
                Ok(_) => Failure {
                    code: "imported".into(),
                    message: "Оригинальный MP3 сохранён; техническая проверка не даёт фонетического доверия".into(),
                    report: None,
                    recovery_required: false,
                },
                Err(e) => e.into(),
            });
            batch.paused = None;
            batch.revision += 1;
            runtime.save(&batch)?;
            done += 1;
        }
        Ok::<_, AssetError>(())
    }
    .await;
    let stopped = if let Some(session) = session {
        session.close().await
    } else {
        Ok(())
    };
    finish_resume(&mut runtime, batch, result, stopped)
}

fn finish_resume(
    runtime: &mut SafeBatchRuntime,
    mut batch: ForvoBatch,
    result: Result<(), AssetError>,
    mut stopped: Result<(), AssetError>,
) -> Result<ForvoBatch, AssetError> {
    if let Err(cleanup) = &mut stopped {
        cleanup.details["paused"] = serde_json::json!(batch.paused);
        if let Some(original) = &mut batch.paused {
            original.recovery_required = true;
        } else {
            let mut failure = Failure::from(AssetError::with_details(
                cleanup.code,
                cleanup.message.clone(),
                cleanup.details.clone(),
            ));
            failure.recovery_required = true;
            batch.paused = Some(failure);
        }
        batch.revision += 1;
        if let Err(save) = runtime.save(&batch) {
            cleanup.details["checkpoint_error"] = serde_json::json!({
                "code": save.code.as_str(), "message": save.message, "details": save.details
            });
        }
    }
    match (result, stopped) {
        (Err(mut original), Err(cleanup)) => {
            original.details["browser_cleanup"] = serde_json::json!({"code":provider_code(&cleanup),"message":cleanup.message,"details":cleanup.details});
            return Err(original);
        }
        (Err(original), _) => return Err(original),
        (_, Err(cleanup)) => return Err(cleanup),
        _ => (),
    }
    Ok(batch)
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn cleanup_checkpoint_failure_preserves_both_errors() {
        let workspace =
            crate::temp_workspace::TempWorkspace::create("brave-cleanup-error").unwrap();
        let store = AssetStore::open_with_policy(
            crate::StoreOptions::new(workspace.path().join("store")),
            crate::pronunciation::PronunciationDomainPolicy,
        )
        .unwrap();
        start(
            &store,
            "failure",
            FetchPlan {
                schema_version: 1,
                items: vec![FetchItem {
                    surface: "水".into(),
                    reading: "みず".into(),
                    replace_expected_sha256: None,
                }],
            },
            Transport::BraveExtension,
        )
        .unwrap();
        let mut runtime = SafeBatchRuntime::open(store.root(), "failure").unwrap();
        let batch = load(&mut runtime).unwrap();
        runtime.release_lock().unwrap();
        let original = AssetError::new(ErrorCode::IoFailure, "исходная ошибка");
        let cleanup = AssetError::with_details(
            ErrorCode::InvalidTransition,
            "ошибка очистки",
            serde_json::json!({"code":"cleanup_incomplete"}),
        );
        let error = finish_resume(&mut runtime, batch, Err(original), Err(cleanup))
            .err()
            .unwrap();
        assert_eq!(error.message, "исходная ошибка");
        assert_eq!(
            error.details["browser_cleanup"]["code"],
            "cleanup_incomplete"
        );
        assert!(
            error.details["browser_cleanup"]["details"]["checkpoint_error"]["message"].is_string()
        );
        runtime.reacquire_lock().unwrap();
        let batch = load(&mut runtime).unwrap();
        let cleanup = AssetError::with_details(
            ErrorCode::InvalidTransition,
            "ошибка очистки CDP",
            serde_json::json!({"code":"browser_cleanup_failed"}),
        );
        assert!(finish_resume(&mut runtime, batch, Ok(()), Err(cleanup)).is_err());
        drop(runtime);
        let saved = status(&store, "failure").unwrap();
        assert_eq!(
            saved.paused.as_ref().unwrap().code,
            "browser_cleanup_failed"
        );
        assert!(saved.paused.unwrap().recovery_required);
    }

    #[tokio::test]
    async fn grace_timeout_survives_checkpoint_and_new_runtime() {
        let workspace = crate::temp_workspace::TempWorkspace::create("brave-grace").unwrap();
        let store = AssetStore::open_with_policy(
            crate::StoreOptions::new(workspace.path().join("store")),
            crate::pronunciation::PronunciationDomainPolicy,
        )
        .unwrap();
        start(
            &store,
            "timeout",
            FetchPlan {
                schema_version: 1,
                items: vec![FetchItem {
                    surface: "水".into(),
                    reading: "みず".into(),
                    replace_expected_sha256: None,
                }],
            },
            Transport::BraveExtension,
        )
        .unwrap();
        let (result, stop) = finish_interrupted(
            std::future::pending(),
            AssetError::new(ErrorCode::Interrupted, "остановка"),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(
            result.as_ref().err().unwrap().details["recovery_required"],
            true
        );
        assert!(Failure::from(stop.unwrap()).recovery_required);
        let mut runtime = SafeBatchRuntime::open(store.root(), "timeout").unwrap();
        let mut batch = load(&mut runtime).unwrap();
        checkpoint_acquisition(&mut runtime, &mut batch, 0, result).unwrap();
        drop(runtime);
        assert!(
            status(&store, "timeout")
                .unwrap()
                .paused
                .unwrap()
                .recovery_required
        );
        let old: Failure =
            serde_json::from_value(serde_json::json!({"code":"interrupted","message":"остановка"}))
                .unwrap();
        assert!(!old.recovery_required);
    }
}

#[cfg(test)]
mod browser_target_tests {
    use super::*;

    #[test]
    fn extension_batch_persists_target_before_first_connection() {
        let workspace = crate::temp_workspace::TempWorkspace::create("forvo-batch-target").unwrap();
        let store = AssetStore::open_with_policy(
            crate::StoreOptions::new(workspace.path().join("store")),
            crate::pronunciation::PronunciationDomainPolicy,
        )
        .unwrap();
        let plan = FetchPlan {
            schema_version: 1,
            items: vec![FetchItem {
                surface: "水".into(),
                reading: "みず".into(),
                replace_expected_sha256: None,
            }],
        };

        let batch = start_with_browser_target(&store, "targeted", plan, "chrome".into()).unwrap();

        assert_eq!(batch.schema_version, 4);
        assert_eq!(batch.target.as_deref(), Some("chrome"));
        assert!(batch.browser.is_none());
        let saved = status(&store, "targeted").unwrap();
        assert_eq!(saved.target.as_deref(), Some("chrome"));
        assert!(saved.browser.is_none());

        let options = BraveOptions {
            state_dir: workspace.path().join("adapter"),
            target: None,
            binding: None,
            wait: Duration::from_secs(1),
            limits: super::super::brave::download::Limits::default(),
            close_tab: false,
        };
        let adjusted = bound_options("targeted", &saved, &options).unwrap();
        assert_eq!(adjusted.target.as_deref(), Some("chrome"));

        let wrong_target = BraveOptions {
            target: Some("brave".into()),
            ..options
        };
        let error = bound_options("targeted", &saved, &wrong_target).unwrap_err();
        assert_eq!(provider_code(&error), "browser_mismatch");
        assert_eq!(
            error.details["next_action"],
            "pronunciation-assets forvo browser --target chrome batch-resume --batch-id targeted"
        );

        let error = transport_mismatch("targeted", &saved, Transport::CdpChromium);
        assert_eq!(
            error.details["next_action"],
            "pronunciation-assets forvo browser --target chrome batch-resume --batch-id targeted"
        );
    }
}
