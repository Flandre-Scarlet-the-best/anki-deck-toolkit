//! Пакетное улучшение речи поверх завершённого пакета Balanced того же
//! рабочего каталога.
//!
//! Новый механизм инвентаризации не вводится: этап берёт неизменяемую
//! инвентаризацию, корень входа и результаты пакета Balanced, читает входы
//! тем же безопасным обходом (`NOFOLLOW` относительно дескриптора корня) и
//! исполняется тем же ограниченным исполнителем [`normalizer_batch::run_items`]
//! с контрольными точками `SafeBatchRuntime`:
//!
//! - `.runtime/batches/<batch-id>/` — состояние пакета улучшения;
//! - `.runtime/batches/enhancer-cache/results/<input-sha>.<config-sha>.json`
//!   и `candidates/<sha>.mp3` — кэш результатов и улучшенных MP3.
//!
//! Отпечаток конфигурации включает профиль, SHA воркера, манифест весов,
//! параметры инференса, режим исполнения и отпечаток Balanced: смена любого
//! из них — другой ключ, прежний кэш не переиспользуется.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::MAX_AUDIO_BYTES;
use super::enhancer::{self, EnhancementProfile, EnhancementResult, ItemContext};
use super::enhancer_backend::{self as backend, BackendIdentity};
use super::normalizer::{self, Toolchain, Variant};
use super::normalizer_batch::{
    self as nbatch, Inventory, ItemError, NormalizerBatch, Progress, ResultCache,
};
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_ID: &str = "enhancer-cache";
const MAX_CONSECUTIVE_INFRA_FAILURES: usize = 8;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RECORDED_ERRORS: usize = 1000;

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn corrupt(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::ManifestCorrupt, message)
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Расход ресурсов инференса за все запуски пакета (по контрольным точкам).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub inference_runs: u64,
    /// Длительность аудио, прошедшего через модель, мс.
    pub inferred_audio_ms: f64,
    /// Время процессов воркера (стена), мс.
    pub inference_wall_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancerBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub cancelled: bool,
    pub base_batch_id: String,
    pub base_config_sha256: String,
    pub base_inventory_sha256: String,
    pub input_root: String,
    pub profile: EnhancementProfile,
    pub backend: BackendIdentity,
    /// Где были воркер и веса при старте; на ключ не влияют (влияют их SHA).
    pub worker_path: String,
    pub model_dir: String,
    pub config_sha256: String,
    pub jobs: usize,
    pub progress: Progress,
    pub resources: Resources,
}

impl RuntimeBatchState for EnhancerBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        Vec::new()
    }
    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != STATE_SCHEMA_VERSION || self.revision > i64::MAX as u64 {
            return Err(corrupt("неподдерживаемая схема пакета улучшения"));
        }
        self.profile.validate()?;
        if enhancer::config_sha256(&self.profile, &self.backend, &self.base_config_sha256)
            != self.config_sha256
            || !valid_sha(&self.base_config_sha256)
            || !valid_sha(&self.base_inventory_sha256)
            || !valid_sha(&self.backend.worker_sha256)
            || !Path::new(&self.input_root).is_absolute()
            || self.jobs == 0
            || self.jobs > nbatch::MAX_JOBS
        {
            return Err(corrupt("противоречивое состояние пакета улучшения"));
        }
        Ok(())
    }
}

fn reject_reserved(batch_id: &str) -> Result<(), AssetError> {
    if batch_id == CACHE_ID || batch_id == nbatch::CACHE_ID {
        return Err(invalid(format!(
            "batch_id {batch_id} зарезервирован для кэша результатов"
        )));
    }
    Ok(())
}

fn open_runtime(workspace: &Path, batch_id: &str) -> Result<SafeBatchRuntime, AssetError> {
    reject_reserved(batch_id)?;
    SafeBatchRuntime::open(workspace, batch_id)
}

/// Кэш результатов улучшения. Держит исключительную блокировку.
pub struct EnhancerCache {
    runtime: SafeBatchRuntime,
}

fn result_name(input_sha: &str, config_sha: &str) -> String {
    format!("{input_sha}.{config_sha}.json")
}

impl EnhancerCache {
    pub fn open(workspace: &Path) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(workspace, CACHE_ID)?,
        })
    }

    /// Готовый годный результат: JSON соответствует ключу и согласован, а
    /// улучшенный MP3, если он указан, существует и совпадает по SHA и длине.
    /// Иначе вход считается необработанным (кэш не доверяет подделке).
    pub fn get(
        &self,
        input_sha: &str,
        config_sha: &str,
    ) -> Result<Option<EnhancementResult>, AssetError> {
        let bytes = match self
            .runtime
            .artifact_directory("results")?
            .read(&result_name(input_sha, config_sha))
        {
            Ok(bytes) => bytes,
            Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(None),
            Err(error) => return Err(error),
        };
        let stale = |why: String| {
            tracing::warn!(
                operation = "enhancer_cache",
                code = "cache_entry_stale",
                input_sha256 = input_sha,
                reason = %why,
                "Результат кэша улучшения непригоден и будет пересчитан"
            );
            Ok(None)
        };
        let Ok(result) = serde_json::from_slice::<EnhancementResult>(&bytes) else {
            return stale("результат не разбирается".into());
        };
        if result.input.sha256 != input_sha || result.config_sha256 != config_sha {
            return stale("результат не соответствует своему ключу".into());
        }
        if let Err(error) = result.validate() {
            return stale(format!("результат противоречив: {}", error.message));
        }
        if let Some(output) = &result.output {
            match self.variant_bytes(output) {
                Ok(bytes) if bytes.len() as u64 == output.byte_length => {}
                Ok(_) => return stale("длина улучшенного MP3 не совпала".into()),
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::MissingAssetFile
                            | ErrorCode::IntegrityMismatch
                            | ErrorCode::InvalidTransition
                            | ErrorCode::UnexpectedPath
                            | ErrorCode::PathTraversal
                    ) =>
                {
                    return stale(format!("улучшенный MP3 недоступен: {}", error.message));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Some(result))
    }

    fn put(
        &self,
        mut result: EnhancementResult,
        output: Option<&[u8]>,
    ) -> Result<EnhancementResult, AssetError> {
        result.validate()?;
        if let (Some(bytes), Some(reference)) = (output, result.output.as_mut()) {
            let existing = RuntimeBlobRef {
                sha256: sha256_hex(bytes),
                storage_path: format!("candidates/{}.mp3", sha256_hex(bytes)),
            };
            let blob = match self
                .runtime
                .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)
            {
                Err(error)
                    if error.code == ErrorCode::IntegrityMismatch
                        || (error.code == ErrorCode::BoundaryViolation
                            && self
                                .runtime
                                .blob_is_oversized_regular_file(&existing, MAX_AUDIO_BYTES)?) =>
                {
                    self.runtime.remove_blob(&existing)?;
                    self.runtime
                        .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)?
                }
                other => other?,
            };
            if blob.sha256 != reference.sha256 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "SHA сохранённого улучшенного MP3 не совпал",
                ));
            }
            reference.storage_path = Some(blob.storage_path);
        }
        let bytes = serde_json::to_vec_pretty(&result).expect("результат сериализуем");
        self.runtime.artifact_directory("results")?.write(
            &result_name(&result.input.sha256, &result.config_sha256),
            &bytes,
        )?;
        Ok(result)
    }

    pub fn variant_bytes(&self, output: &normalizer::OutputRef) -> Result<Vec<u8>, AssetError> {
        let storage_path = output
            .storage_path
            .clone()
            .ok_or_else(|| invalid("у результата нет сохранённого улучшенного MP3"))?;
        self.runtime.read_blob(&RuntimeBlobRef {
            sha256: output.sha256.clone(),
            storage_path,
        })
    }
}

pub struct StartRequest<'a> {
    pub batch_id: &'a str,
    pub workspace: &'a Path,
    pub base_batch_id: &'a str,
    pub profile: EnhancementProfile,
    pub worker: &'a Path,
    pub model_dir: &'a Path,
    pub jobs: usize,
}

/// Пакет Balanced, на который опирается этап: текущая схема, режим
/// normalize, результаты всех уникальных входов. Неполный пакет — отказ с
/// числом недостающих результатов, а не частичное улучшение.
fn load_complete_base(
    workspace: &Path,
    base_batch_id: &str,
) -> Result<(NormalizerBatch, Inventory), AssetError> {
    let (base, inventory) = nbatch::load(workspace, base_batch_id)?;
    if base.mode != normalizer::Mode::Normalize
        || base.result_schema_version != normalizer::RESULT_SCHEMA_VERSION
    {
        return Err(invalid(
            "этапу улучшения нужен пакет Balanced текущей схемы в режиме normalize",
        ));
    }
    let cache = ResultCache::open(workspace)?;
    let missing = inventory
        .unique_inputs()
        .iter()
        .filter(|(sha, _)| !matches!(cache.get(sha, &base.config_sha256), Ok(Some(_))))
        .count();
    if missing > 0 {
        return Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            "пакет Balanced не завершён: сначала batch-resume нормализатора",
            serde_json::json!({"blocker":"base_batch_incomplete","missing_results":missing}),
        ));
    }
    Ok((base, inventory))
}

pub fn start(request: StartRequest<'_>) -> Result<EnhancerBatch, AssetError> {
    reject_reserved(request.batch_id)?;
    request.profile.validate()?;
    if request.jobs == 0 || request.jobs > nbatch::MAX_JOBS {
        return Err(invalid(format!(
            "--jobs должен быть 1–{}",
            nbatch::MAX_JOBS
        )));
    }
    if request.batch_id == request.base_batch_id {
        return Err(invalid(
            "ID пакета улучшения должен отличаться от ID пакета Balanced",
        ));
    }
    let (base, inventory) = load_complete_base(request.workspace, request.base_batch_id)?;
    let identity = backend::identify(
        request.worker,
        request.model_dir,
        request.profile.backend.kind,
        &request.profile.backend.inference,
    )?;
    let mut runtime = open_runtime(request.workspace, request.batch_id)?;
    if runtime.load::<EnhancerBatch>()?.is_some() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "пакет уже существует; используйте resume",
        ));
    }
    let config_sha256 = enhancer::config_sha256(&request.profile, &identity, &base.config_sha256);
    let batch = EnhancerBatch {
        schema_version: STATE_SCHEMA_VERSION,
        batch_id: request.batch_id.into(),
        revision: 0,
        cancelled: false,
        base_batch_id: request.base_batch_id.into(),
        base_config_sha256: base.config_sha256.clone(),
        base_inventory_sha256: base.inventory_sha256.clone(),
        input_root: base.input_root.clone(),
        profile: request.profile,
        backend: identity,
        worker_path: request.worker.to_string_lossy().into_owned(),
        model_dir: request.model_dir.to_string_lossy().into_owned(),
        config_sha256,
        jobs: request.jobs,
        progress: Progress {
            unique_inputs: inventory.unique_inputs().len() as u64,
            ..Progress::default()
        },
        resources: Resources::default(),
    };
    runtime.save(&batch)?;
    Ok(batch)
}

/// Состояние пакета улучшения, его базовый пакет и инвентаризация.
pub fn load(
    workspace: &Path,
    batch_id: &str,
) -> Result<(EnhancerBatch, NormalizerBatch, Inventory), AssetError> {
    let mut runtime = open_runtime(workspace, batch_id)?;
    let batch: EnhancerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет улучшения отсутствует"))?;
    let (base, inventory) = nbatch::load(workspace, &batch.base_batch_id)?;
    if base.config_sha256 != batch.base_config_sha256
        || base.inventory_sha256 != batch.base_inventory_sha256
        || base.input_root != batch.input_root
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "базовый пакет Balanced изменился после старта пакета улучшения",
        ));
    }
    Ok((batch, base, inventory))
}

pub fn cancel(workspace: &Path, batch_id: &str) -> Result<EnhancerBatch, AssetError> {
    let mut runtime = open_runtime(workspace, batch_id)?;
    let mut batch: EnhancerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет улучшения отсутствует"))?;
    if !batch.cancelled {
        batch.cancelled = true;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub outcome: &'static str,
    pub processed_now: u64,
    pub reused: u64,
    pub remaining: u64,
    pub infrastructure_failed_now: u64,
    /// Время этого запуска (стена), мс.
    pub wall_ms: f64,
    pub batch: EnhancerBatch,
}

struct Snapshots {
    tools: super::probe::ToolFiles,
    worker: backend::WorkerSnapshot,
    /// Исходник Python-воркера: запускатель не меняется, когда меняется
    /// исполняемый им код.
    source: Option<backend::WorkerSnapshot>,
}

fn snapshots(worker: &Path, identity: &BackendIdentity) -> Result<Snapshots, AssetError> {
    Ok(Snapshots {
        tools: super::probe::tool_files()?,
        worker: backend::worker_snapshot(worker)?,
        source: identity
            .worker_source()
            .map(|source| backend::worker_snapshot(&source))
            .transpose()?,
    })
}

/// Обрабатывает недостающие результаты. Сбой backend или FFmpeg не
/// превращается в исход входа: вход остаётся незавершённым и повторяется.
pub fn resume(
    workspace: &Path,
    batch_id: &str,
    jobs: Option<usize>,
    max_items: Option<usize>,
    stop: &AtomicBool,
) -> Result<RunSummary, AssetError> {
    let started = Instant::now();
    let mut runtime = open_runtime(workspace, batch_id)?;
    let mut batch: EnhancerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет улучшения отсутствует"))?;
    if batch.cancelled {
        return Err(invalid("пакет улучшения отменён"));
    }
    let _lease = runtime.execution_lease()?;
    let (base, inventory) = nbatch::load(workspace, &batch.base_batch_id)?;
    if base.config_sha256 != batch.base_config_sha256
        || base.inventory_sha256 != batch.base_inventory_sha256
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "базовый пакет Balanced изменился после старта пакета улучшения",
        ));
    }
    let worker = PathBuf::from(&batch.worker_path);
    let model_dir = PathBuf::from(&batch.model_dir);
    // Снимок до `identify`: исходник, изменённый между снимком и чтением
    // в `identify`, даст расхождение снимков после первого же входа.
    let snap = snapshots(&worker, &batch.backend)?;
    let toolchain = Toolchain::detect()?;
    if toolchain != base.toolchain {
        return Err(invalid(
            "версия FFmpeg/ffprobe/кодировщика изменилась: результаты несопоставимы, начните новый пакет",
        ));
    }
    // Веса и воркер проверяются заново: другая модель или сборка — другой
    // ключ кэша, смешивать результаты нельзя.
    let identity = backend::identify(
        &worker,
        &model_dir,
        batch.profile.backend.kind,
        &batch.profile.backend.inference,
    )?;
    if identity != batch.backend {
        return Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            "воркер или веса изменились после старта: начните новый пакет",
            serde_json::json!({"blocker":"enhancer_backend_changed"}),
        ));
    }
    let jobs = jobs.unwrap_or(batch.jobs);
    if jobs == 0 || jobs > nbatch::MAX_JOBS {
        return Err(invalid(format!(
            "--jobs должен быть 1–{}",
            nbatch::MAX_JOBS
        )));
    }
    runtime.release_lock()?;
    let base_cache = ResultCache::open(workspace)?;
    let cache = EnhancerCache::open(workspace)?;
    let root = nbatch::open_input_root(&base)?;
    let mut by_outcome: BTreeMap<String, u64> = BTreeMap::new();
    let mut pending = Vec::new();
    let mut reused = 0u64;
    let processed_before = batch.progress.processed;
    let mut infra = batch.progress.infrastructure_failures.clone();
    let mut source_errors = batch.progress.source_errors.clone();
    let mut resources = batch.resources.clone();
    let unique = inventory.unique_inputs();
    for (sha, path) in &unique {
        match cache.get(sha, &batch.config_sha256)? {
            Some(result) => match nbatch::read_input(&root, path, sha) {
                Ok(_) => {
                    reused += 1;
                    infra.remove(*sha);
                    source_errors.remove(*path);
                    *by_outcome
                        .entry(result.gate.outcome.as_str().into())
                        .or_default() += 1;
                }
                Err(error) => {
                    if source_errors.len() < MAX_RECORDED_ERRORS {
                        source_errors.insert((*path).to_owned(), error.message);
                    }
                }
            },
            None => pending.push((sha.to_string(), path.to_string())),
        }
    }
    let unique_count = unique.len() as u64;
    pending.truncate(max_items.unwrap_or(usize::MAX));
    // Результаты Balanced читаются до запуска, а байты выбранного ими
    // варианта — лениво в `work`: предзагрузка всех вариантов держала бы в
    // памяти весь пакет. Кэш Balanced остаётся открытым до конца прогона,
    // потому что его блобы читаются только под его исключительной блокировкой.
    let mut prepared: Vec<Option<normalizer::NormalizationResult>> =
        Vec::with_capacity(pending.len());
    for (sha, path) in &pending {
        let entry = base_cache.get(sha, &base.config_sha256)?;
        if entry.is_none() && source_errors.len() < MAX_RECORDED_ERRORS {
            source_errors.insert(path.clone(), "нет годного результата Balanced".into());
        }
        prepared.push(entry);
    }
    let profile = batch.profile.clone();
    let backend_identity = batch.backend.clone();
    let config_sha256 = batch.config_sha256.clone();
    let ctx = ItemContext {
        profile: &profile,
        backend: &backend_identity,
        worker: &worker,
        model_dir: &model_dir,
        base_profile: &base.profile,
        base_toolchain: &base.toolchain,
        config_sha256: &config_sha256,
    };
    let index_of: BTreeMap<&str, usize> = pending
        .iter()
        .enumerate()
        .map(|(i, (sha, _))| (sha.as_str(), i))
        .collect();
    let work = |sha: &str, path: &str| {
        let Some(base_result) = &prepared[index_of[sha]] else {
            return Err(ItemError::Source("нет годного результата Balanced".into()));
        };
        let original = nbatch::read_input(&root, path, sha)
            .map_err(|error| ItemError::Source(error.message))?;
        let processed_bytes = match (&base_result.gate.proposed_variant, &base_result.output) {
            (Some(Variant::Processed), Some(output)) => {
                Some(base_cache.variant_bytes(output).map_err(ItemError::Fatal)?)
            }
            _ => None,
        };
        let selected = match base_result.gate.proposed_variant {
            Some(Variant::Processed) => processed_bytes.as_deref(),
            Some(Variant::Original) => Some(original.as_slice()),
            None => None,
        };
        let outcome =
            enhancer::process_item(&ctx, &original, base_result, selected).map_err(|error| {
                if enhancer::is_infrastructure_failure(&error) {
                    ItemError::Infrastructure(error)
                } else {
                    ItemError::Fatal(error)
                }
            })?;
        match snapshots(&worker, &backend_identity) {
            Ok(now)
                if now.tools == snap.tools
                    && now.worker == snap.worker
                    && now.source == snap.source =>
            {
                Ok(outcome)
            }
            Ok(now) if now.tools != snap.tools => {
                Err(ItemError::Fatal(super::probe::toolchain_changed()))
            }
            Ok(_) => Err(ItemError::Fatal(backend::worker_changed())),
            Err(error) => Err(ItemError::Infrastructure(error)),
        }
    };
    let halt = AtomicBool::new(false);
    let mut processed_now = 0u64;
    let mut infrastructure_failed_now = 0u64;
    let mut failure: Option<AssetError> = None;
    let mut cancelled = false;
    let mut last_checkpoint = Instant::now();
    let mut consecutive_infra = 0usize;
    let checkpoint = |batch: &mut EnhancerBatch,
                      runtime: &mut SafeBatchRuntime,
                      completed: u64,
                      processed: u64,
                      by_outcome: &BTreeMap<String, u64>,
                      infra: &BTreeMap<String, u32>,
                      source_errors: &BTreeMap<String, String>,
                      resources: &Resources|
     -> Result<bool, AssetError> {
        runtime.reacquire_lock()?;
        let mut stored: EnhancerBatch = runtime
            .reload_cached()?
            .ok_or_else(|| invalid("пакет улучшения исчез"))?;
        stored.progress.completed = completed;
        stored.progress.processed = processed;
        stored.progress.by_outcome = by_outcome.clone();
        stored.progress.infrastructure_failures = infra.clone();
        stored.progress.source_errors = source_errors.clone();
        stored.resources = resources.clone();
        stored.revision += 1;
        runtime.save(&stored)?;
        runtime.release_lock()?;
        *batch = stored;
        Ok(batch.cancelled)
    };
    nbatch::run_items(&pending, jobs, stop, &halt, work, |index, outcome| {
        let (sha, path) = &pending[index];
        let stored = match outcome {
            Ok(item) => {
                consecutive_infra = 0;
                if let Some(record) = &item.result.inference {
                    resources.inference_runs += 1;
                    resources.inference_wall_ms += record.run.wall_ms;
                    resources.inferred_audio_ms +=
                        item.result.triage.as_ref().map_or(0.0, |t| t.duration_ms);
                }
                cache.put(item.result, item.output_bytes.as_deref())
            }
            Err(ItemError::Source(message)) => {
                if source_errors.len() < MAX_RECORDED_ERRORS {
                    source_errors.insert(path.clone(), message);
                }
                return;
            }
            Err(ItemError::Infrastructure(error)) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                tracing::warn!(
                    operation = "enhancer_batch",
                    code = error.details["blocker"].as_str().unwrap_or("infrastructure"),
                    input_sha256 = %sha,
                    message = %error.message,
                    "Сбой среды при улучшении: вход будет повторён"
                );
                infrastructure_failed_now += 1;
                consecutive_infra += 1;
                *infra.entry(sha.clone()).or_default() += 1;
                if consecutive_infra >= MAX_CONSECUTIVE_INFRA_FAILURES {
                    halt.store(true, Ordering::SeqCst);
                    failure.get_or_insert(error);
                }
                return;
            }
            Err(ItemError::Fatal(error)) => {
                halt.store(true, Ordering::SeqCst);
                failure.get_or_insert(error);
                return;
            }
        };
        match stored {
            Ok(result) => {
                processed_now += 1;
                infra.remove(sha);
                source_errors.remove(path);
                *by_outcome
                    .entry(result.gate.outcome.as_str().into())
                    .or_default() += 1;
            }
            Err(error) => {
                halt.store(true, Ordering::SeqCst);
                failure.get_or_insert(error);
                return;
            }
        }
        if last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            last_checkpoint = Instant::now();
            match checkpoint(
                &mut batch,
                &mut runtime,
                reused + processed_now,
                processed_before + processed_now,
                &by_outcome,
                &infra,
                &source_errors,
                &resources,
            ) {
                Ok(true) => {
                    cancelled = true;
                    halt.store(true, Ordering::SeqCst);
                }
                Ok(false) => {}
                Err(error) => {
                    halt.store(true, Ordering::SeqCst);
                    failure.get_or_insert(error);
                }
            }
        }
    });
    let final_checkpoint = checkpoint(
        &mut batch,
        &mut runtime,
        reused + processed_now,
        processed_before + processed_now,
        &by_outcome,
        &infra,
        &source_errors,
        &resources,
    );
    if let Some(error) = failure {
        return Err(error);
    }
    cancelled |= final_checkpoint?;
    let remaining = unique_count.saturating_sub(reused + processed_now);
    Ok(RunSummary {
        outcome: if stop.load(Ordering::SeqCst) {
            "interrupted"
        } else if cancelled {
            "cancelled"
        } else if remaining > 0 {
            "paused"
        } else {
            "completed"
        },
        processed_now,
        reused,
        remaining,
        infrastructure_failed_now,
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
        batch,
    })
}

/// Результаты в порядке инвентаризации (по одному на уникальный SHA).
pub fn collect_results(
    cache: &EnhancerCache,
    batch: &EnhancerBatch,
    inventory: &Inventory,
) -> Result<Vec<EnhancementResult>, AssetError> {
    let mut results = Vec::new();
    for (sha, _) in inventory.unique_inputs() {
        if let Some(result) = cache.get(sha, &batch.config_sha256)? {
            results.push(result);
        }
    }
    Ok(results)
}

/// Проверка неизменности входа — та же, что у пакета Balanced.
pub fn verify_sources(
    base: &NormalizerBatch,
    inventory: &Inventory,
) -> Result<nbatch::SourceCheck, AssetError> {
    nbatch::verify_sources(base, inventory)
}

/// Корни, которые не могут пересекаться с каталогом весов: вход, рабочий
/// каталог и защищённые деревья.
pub fn check_model_dir(
    model_dir: &Path,
    input_root: &Path,
    workspace: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<PathBuf, AssetError> {
    let resolved = nbatch::resolve_path(model_dir)?;
    nbatch::check_boundaries(input_root, &resolved, protected)?;
    if resolved.starts_with(workspace) || workspace.starts_with(&resolved) {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "каталог весов не может пересекаться с рабочим каталогом",
        ));
    }
    Ok(resolved)
}
