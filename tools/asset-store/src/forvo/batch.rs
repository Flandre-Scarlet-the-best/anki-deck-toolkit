//! Forvo-пакет сохраняет полученные байты до импорта, используя единственный SafeBatchRuntime.
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
    pub schema_version: u32,
    pub provider: String,
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
        if self.schema_version != 1
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
pub fn start(store: &AssetStore, id: &str, plan: FetchPlan) -> Result<ForvoBatch, AssetError> {
    if plan.schema_version != 1 || plan.items.is_empty() || plan.items.len() > MAX_BATCH_ITEMS {
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
        schema_version: 1,
        provider: "forvo_web".into(),
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
            "пакет существует; используйте forvo batch-resume",
        ));
    }
    runtime.save(&batch)?;
    Ok(batch)
}
fn load(runtime: &mut SafeBatchRuntime) -> Result<ForvoBatch, AssetError> {
    runtime.load()?.ok_or_else(invalid)
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
    profile: &Path,
    max_items: usize,
) -> Result<ForvoBatch, AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), id)?;
    let _lease = runtime.execution_lease()?;
    let mut batch = load(&mut runtime)?;
    if batch.cancelled {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "пакет отменён",
        ));
    }
    if batch.paused.is_some() && batch.items.iter().all(|item| item.outcome.is_some()) {
        batch.paused = None;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    let mut session = None;
    let result=async {
        let mut done=0;
        while let Some(index)=batch.items.iter().position(|i|i.outcome.is_none()) {
            if done>=max_items||batch.cancelled{break;}
            let timer = BrowserItemTimer::new(BrowserItemContext::without_generation("forvo_web", &batch.items[index].request.surface, 1));
            if batch.items[index].acquisition.is_none(){
                let stage = timer.stage("acquire");
                runtime.release_lock()?;
                let launched=if session.is_none() {
                    enum LaunchResult { Finished(Box<Result<ForvoSession,AssetError>>), Interrupted(AssetError) }
                    let mut launching=Box::pin(ForvoSession::launch(profile));
                    match tokio::select! {
                        result=&mut launching => LaunchResult::Finished(Box::new(result)),
                        _=tokio::signal::ctrl_c()=>LaunchResult::Interrupted(AssetError::new(ErrorCode::Interrupted,"Запуск Forvo прерван; продолжите через batch-resume")),
                        result=cancellation(store,id)=>LaunchResult::Interrupted(result.err().unwrap_or_else(||AssetError::new(ErrorCode::Interrupted,"Forvo-пакет отменён"))),
                    } {
                        LaunchResult::Finished(result)=>match *result {Ok(s)=>{session=Some(s);Ok(())},Err(e)=>Err(e)},
                        LaunchResult::Interrupted(mut interrupted)=>{
                            let cleanup=match launching.await {Ok(s)=>s.close().await,Err(e) if provider_code(&e)=="browser_cleanup_failed"=>Err(e),Err(_)=>Ok(())};
                            if let Err(e)=cleanup {interrupted.details["browser_cleanup"]=serde_json::json!({"code":provider_code(&e),"message":e.message});}
                            Err(interrupted)
                        }
                    }
                } else {Ok(())};
                let acquisition=match launched {
                    Err(e)=>Err(e),
                    Ok(())=>tokio::select! {
                    r=acquire(session.as_mut().expect("session"),&batch.items[index].request)=>r,
                    _=tokio::signal::ctrl_c()=>Err(AssetError::new(ErrorCode::Interrupted,"Forvo-пакет прерван; продолжите через batch-resume")),
                    r=cancellation(store,id)=>{r?;Err(AssetError::new(ErrorCode::Interrupted,"Forvo-пакет отменён"))},
                    }
                };
                match &acquisition { Ok(_) => stage.finish_success(), Err(e) => stage.finish_failure(provider_code(e), pauses_batch(e), None) }
                runtime.reacquire_lock()?;
                batch=load(&mut runtime)?;
                if let Err(e)=&acquisition && !e.details["browser_cleanup"].is_null() {
                    return Err(AssetError::with_details(e.code, e.message.clone(), e.details.clone()));
                }
                if batch.cancelled {
                    break;
                }
                let failure_code = acquisition.as_ref().err().map(|e| provider_code(e).to_owned());
                match checkpoint_acquisition(&mut runtime, &mut batch, index, acquisition)? {
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
            let item=&batch.items[index];
            let stage = timer.stage("import_mp3");
            let saved=item.acquisition.as_ref().expect("checkpoint");
            let result=runtime.read_blob(&saved.blob).and_then(|bytes|import_acquisition(store,&item.request,&Acquisition{bytes,evidence:saved.evidence.clone()},item.replace_expected_sha256.clone()));
            if let Err(e) = &result && retryable_import(e) {
                let failure=Failure::from(AssetError::with_details(e.code, e.message.clone(), e.details.clone()));
                stage.finish_failure(&failure.code, true, None);
                timer.finish_failure(&failure.code, true, None);
                tracing::warn!(event="forvo_batch_paused", stage="import_mp3", code=failure.code, "Импорт сохранённого MP3 приостановлен");
                batch.paused=Some(failure);
                batch.revision+=1;runtime.save(&batch)?;break;
            }
            match &result { Ok(_) => stage.finish_success(), Err(e) => stage.finish_failure(provider_code(e), false, None) }
            match &result { Ok(_) => timer.finish_success(), Err(e) => timer.finish_failure(provider_code(e), false, None) }
            batch.items[index].outcome=Some(match result {
                Ok(_)=>Failure{code:"imported".into(),message:"Оригинальный MP3 сохранён; техническая проверка не даёт фонетического доверия".into(),report:None},
                Err(e)=>e.into(),
            });
            batch.paused=None;batch.revision+=1;runtime.save(&batch)?;done+=1;
        }
        Ok::<_,AssetError>(())
    }.await;
    let stopped = if let Some(session) = session {
        session.close().await
    } else {
        Ok(())
    };
    match (result, stopped) {
        (Err(mut original), Err(cleanup)) => {
            original.details["browser_cleanup"] =
                serde_json::json!({"code":provider_code(&cleanup),"message":cleanup.message});
            return Err(original);
        }
        (Err(original), _) => return Err(original),
        (_, Err(cleanup)) => return Err(cleanup),
        _ => (),
    }
    Ok(batch)
}
