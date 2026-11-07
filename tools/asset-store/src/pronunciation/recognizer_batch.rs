//! Пакетное распознавание: инвентаризация входа, явный манифест целей,
//! пары SHA × цель, кэш, остановка и возобновление.
//!
//! Акустический анализ уникальных байтов и решения по парам разделены:
//! эмиссии кэшируются по SHA-256 PCM анализа и отпечатку модели (одна
//! запись обслуживает все свои цели), а результат — по SHA входа, ключу цели
//! и отпечатку всей конфигурации. Решения разных чтений одного файла не
//! склеиваются: каждая пара — отдельный результат.
//!
//! Вызовы моделей группируются: для группы пар сначала одним вызовом воркера
//! на модель считаются эмиссии исходников, затем параллельно выполняется
//! этап 1, затем одним вызовом — эмиссии итоговых кандидатов и этап 2.
//! Сбой среды не превращается в исход пары: пара остаётся незавершённой и
//! повторяется следующим запуском.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ctc::Emissions;
use super::enhancer_backend as ebackend;
use super::normalizer::{NormalizerProfile, Toolchain};
use super::normalizer_batch::{self as nbatch, Inventory, Limits};
use super::recognizer::{RecognitionProfile, TargetIdentity, TargetSource, TargetSourceKind};
use super::recognizer_backend::ModelHandle;
use super::recognizer_pipeline::{self as pipeline, EmissionSource, RecognitionResult, Recognized};
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::hashing::sha256_hex;
use crate::pronunciation::MAX_AUDIO_BYTES;
use crate::{AssetError, ErrorCode};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const CACHE_ID: &str = "recognizer-cache";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TARGETS_PER_ENTRY: usize = 32;
const MAX_CONSECUTIVE_INFRA_FAILURES: usize = 8;
const MAX_RECORDED_ERRORS: usize = 1000;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

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
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

// ---------------------------------------------------------------------------
// Манифест целей
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTarget {
    pub surface: String,
    pub reading: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub competing_readings: Vec<String>,
}

/// Запись манифеста: файл (по SHA-256 и/или относительному пути внутри
/// входа) и одна или несколько целевых пар.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub targets: Vec<ManifestTarget>,
}

/// Явный манифест целей. Его происхождение (`provenance`) — например,
/// идентификатор плана Forvo — переносится в ключ происхождения каждой цели;
/// второго реестра доверия манифест не создаёт.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetManifest {
    pub schema_version: u32,
    pub kind: String,
    pub provenance: String,
    pub entries: Vec<ManifestEntry>,
}

impl TargetManifest {
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION
            || self.kind != "pronunciation_target_manifest"
            || self.provenance.is_empty()
            || self.provenance.len() > 256
        {
            return Err(invalid(
                "манифест целей: ожидается schema_version 1, kind pronunciation_target_manifest и непустой provenance",
            ));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            let id_ok = !entry.id.is_empty()
                && entry.id.len() <= 128
                && entry
                    .id
                    .chars()
                    .all(|c| c.is_alphanumeric() || "._-:".contains(c));
            if !id_ok || !ids.insert(entry.id.as_str()) {
                return Err(invalid(format!(
                    "манифест целей: идентификатор записи «{}» пуст, повторяется или содержит недопустимые символы",
                    entry.id
                )));
            }
            if entry.sha256.is_none() && entry.path.is_none() {
                return Err(invalid(format!(
                    "запись {}: нужен sha256 или path",
                    entry.id
                )));
            }
            if entry.sha256.as_deref().is_some_and(|s| !valid_sha(s)) {
                return Err(invalid(format!(
                    "запись {}: sha256 не в нижнем hex",
                    entry.id
                )));
            }
            if entry.targets.is_empty() || entry.targets.len() > MAX_TARGETS_PER_ENTRY {
                return Err(invalid(format!(
                    "запись {}: от 1 до {MAX_TARGETS_PER_ENTRY} целей",
                    entry.id
                )));
            }
            for target in &entry.targets {
                self.identity(entry, target, "0".repeat(64).as_str())
                    .validate()?;
            }
        }
        Ok(())
    }

    fn identity(
        &self,
        entry: &ManifestEntry,
        target: &ManifestTarget,
        manifest_sha: &str,
    ) -> TargetIdentity {
        TargetIdentity {
            surface: target.surface.clone(),
            reading: target.reading.clone(),
            source: TargetSource {
                kind: TargetSourceKind::Manifest,
                key: format!("manifest:{manifest_sha}:{}", entry.id),
            },
            competing_readings: target.competing_readings.clone(),
        }
    }
}

pub fn read_manifest(
    path: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<TargetManifest, AssetError> {
    let bytes = crate::pronunciation_cli::read_local(path, protected, MAX_MANIFEST_BYTES)?;
    let manifest: TargetManifest = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("манифест целей не разбирается: {e}")))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Пара SHA × цель (без цели — только свободный анализ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    pub sha256: String,
    pub path: String,
    pub target: Option<TargetIdentity>,
}

impl WorkItem {
    pub fn key(&self) -> String {
        format!(
            "{}-{}",
            self.sha256,
            self.target
                .as_ref()
                .map(TargetIdentity::matching_key)
                .unwrap_or_else(|| "none".into())
        )
    }
}

/// Пары работы в порядке инвентаризации. Цели, которые не удалось сопоставить
/// с файлами входа, перечисляются отдельно (без изобретения соответствия).
pub fn work_items(
    inventory: &Inventory,
    manifest: Option<&TargetManifest>,
    manifest_sha: Option<&str>,
) -> (Vec<WorkItem>, Vec<String>) {
    let unique = inventory.unique_inputs();
    let by_path: BTreeMap<&str, &str> = inventory
        .entries
        .iter()
        .filter_map(|e| e.sha256.as_deref().map(|sha| (e.path.as_str(), sha)))
        .collect();
    let known: BTreeSet<&str> = unique.iter().map(|(sha, _)| *sha).collect();
    let mut targets: BTreeMap<String, Vec<TargetIdentity>> = BTreeMap::new();
    let mut unresolved = Vec::new();
    if let (Some(manifest), Some(msha)) = (manifest, manifest_sha) {
        for entry in &manifest.entries {
            let via_path = entry.path.as_deref().and_then(|p| by_path.get(p).copied());
            let sha = match (entry.sha256.as_deref(), via_path) {
                (Some(sha), Some(p)) if sha != p => None,
                (Some(sha), _) if known.contains(sha) => Some(sha.to_string()),
                (None, Some(p)) => Some(p.to_string()),
                _ => None,
            };
            let Some(sha) = sha else {
                unresolved.push(entry.id.clone());
                continue;
            };
            let list = targets.entry(sha).or_default();
            for target in &entry.targets {
                let identity = manifest.identity(entry, target, msha);
                if !list
                    .iter()
                    .any(|t| t.matching_key() == identity.matching_key())
                {
                    list.push(identity);
                }
            }
        }
    }
    let mut items = Vec::new();
    for (sha, path) in unique {
        match targets.get(sha) {
            Some(list) => items.extend(list.iter().map(|t| WorkItem {
                sha256: sha.into(),
                path: path.into(),
                target: Some(t.clone()),
            })),
            None => items.push(WorkItem {
                sha256: sha.into(),
                path: path.into(),
                target: None,
            }),
        }
    }
    (items, unresolved)
}

// ---------------------------------------------------------------------------
// Состояние пакета
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub unique_inputs: u64,
    pub pairs: u64,
    pub pairs_with_target: u64,
    pub completed_pairs: u64,
    /// Уникальные входы, у которых завершены все пары.
    pub completed_inputs: u64,
    pub processed: u64,
    pub by_outcome: BTreeMap<String, u64>,
    pub by_action: BTreeMap<String, u64>,
    /// Сбои среды по ключу пары: пара повторяется следующим запуском.
    pub infrastructure_failures: BTreeMap<String, u32>,
    pub source_errors: BTreeMap<String, String>,
    /// Записи манифеста без соответствующего файла входа.
    pub unresolved_manifest_entries: Vec<String>,
    /// Вызовы моделей и их время (диагностика, не детерминирована).
    pub model_calls: u64,
    pub model_items: u64,
    pub model_wall_ms: f64,
    pub wall_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognizerBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub cancelled: bool,
    pub profile: RecognitionProfile,
    pub base_profile: NormalizerProfile,
    pub with_enhancer: bool,
    pub toolchain: Toolchain,
    /// Отпечатки моделей и воркеров на момент старта: продолжение с другой
    /// моделью или другим воркером запрещено (несопоставимые результаты).
    pub primary_fingerprint: String,
    pub verifier_fingerprint: Option<String>,
    pub enhancer_identity: Option<ebackend::BackendIdentity>,
    pub config_sha256: String,
    pub input_root: String,
    pub inventory_sha256: String,
    pub manifest_sha256: Option<String>,
    pub limits: Limits,
    pub progress: Progress,
}

impl RuntimeBatchState for RecognizerBatch {
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
            return Err(corrupt("неподдерживаемая схема пакета распознавания"));
        }
        self.profile.validate()?;
        self.base_profile.validate()?;
        if !valid_sha(&self.config_sha256)
            || !valid_sha(&self.inventory_sha256)
            || self
                .manifest_sha256
                .as_deref()
                .is_some_and(|s| !valid_sha(s))
            || !Path::new(&self.input_root).is_absolute()
            || self.limits.jobs == 0
            || self.limits.jobs > nbatch::MAX_JOBS
            || self.with_enhancer != self.enhancer_identity.is_some()
        {
            return Err(corrupt("противоречивое состояние пакета распознавания"));
        }
        Ok(())
    }
}

pub struct StartRequest<'a> {
    pub batch_id: &'a str,
    pub workspace: &'a Path,
    pub input_root: &'a Path,
    pub manifest: Option<TargetManifest>,
    pub profile: RecognitionProfile,
    pub base_profile: NormalizerProfile,
    pub with_enhancer: bool,
    pub jobs: usize,
    pub recognizer_bin: Option<&'a Path>,
    pub enhancer_bin: Option<&'a Path>,
    pub models_root: Option<&'a Path>,
}

fn reject_reserved(batch_id: &str) -> Result<(), AssetError> {
    if batch_id == CACHE_ID {
        return Err(invalid(format!(
            "batch_id {CACHE_ID} зарезервирован для кэша"
        )));
    }
    Ok(())
}

fn open_runtime(workspace: &Path, batch_id: &str) -> Result<SafeBatchRuntime, AssetError> {
    reject_reserved(batch_id)?;
    SafeBatchRuntime::open(workspace, batch_id)
}

/// Фиксирует вход, манифест и конфигурацию. Модели проверяются при старте,
/// и их отпечатки становятся частью пакета; обработку выполняет [`resume`].
pub fn start(request: StartRequest<'_>) -> Result<RecognizerBatch, AssetError> {
    reject_reserved(request.batch_id)?;
    request.profile.validate()?;
    request.base_profile.validate()?;
    if request.jobs == 0 || request.jobs > nbatch::MAX_JOBS {
        return Err(invalid(format!(
            "--jobs должен быть 1–{}",
            nbatch::MAX_JOBS
        )));
    }
    let setup = crate::pronunciation_recognizer_cli::setup_from(
        request.profile.clone(),
        request.base_profile.clone(),
        request.with_enhancer,
        request.recognizer_bin,
        request.enhancer_bin,
        request.models_root,
    )?;
    let limits = Limits {
        jobs: request.jobs,
        max_files: nbatch::DEFAULT_MAX_FILES,
        max_total_bytes: nbatch::DEFAULT_MAX_TOTAL_BYTES,
    };
    let inventory = nbatch::inventory(request.input_root, &limits)?;
    let inventory_bytes = serde_json::to_vec(&inventory).expect("инвентаризация сериализуема");
    let manifest_bytes = request
        .manifest
        .as_ref()
        .map(|m| serde_json::to_vec(m).expect("манифест сериализуем"));
    let manifest_sha = manifest_bytes.as_deref().map(sha256_hex);
    let (items, unresolved) = work_items(
        &inventory,
        request.manifest.as_ref(),
        manifest_sha.as_deref(),
    );
    let mut runtime = open_runtime(request.workspace, request.batch_id)?;
    if runtime.load::<RecognizerBatch>()?.is_some() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "пакет уже существует; используйте batch-resume",
        ));
    }
    let data = runtime.artifact_directory("data")?;
    data.write("inventory.json", &inventory_bytes)?;
    if let Some(bytes) = &manifest_bytes {
        data.write("targets.json", bytes)?;
    }
    let batch = RecognizerBatch {
        schema_version: STATE_SCHEMA_VERSION,
        batch_id: request.batch_id.into(),
        revision: 0,
        cancelled: false,
        profile: request.profile,
        base_profile: request.base_profile,
        with_enhancer: request.with_enhancer,
        toolchain: setup.toolchain.clone(),
        primary_fingerprint: setup.primary.identity.fingerprint(),
        verifier_fingerprint: setup.verifier.as_ref().map(|v| v.identity.fingerprint()),
        enhancer_identity: setup.enhancer.as_ref().map(|e| e.identity.clone()),
        config_sha256: setup.config_sha256.clone(),
        input_root: request
            .input_root
            .to_str()
            .ok_or_else(|| invalid("путь входа должен быть UTF-8"))?
            .into(),
        inventory_sha256: sha256_hex(&inventory_bytes),
        manifest_sha256: manifest_sha,
        limits,
        progress: Progress {
            unique_inputs: inventory.unique_inputs().len() as u64,
            pairs: items.len() as u64,
            pairs_with_target: items.iter().filter(|i| i.target.is_some()).count() as u64,
            unresolved_manifest_entries: unresolved,
            ..Progress::default()
        },
    };
    runtime.save(&batch)?;
    Ok(batch)
}

pub fn load(
    workspace: &Path,
    batch_id: &str,
) -> Result<(RecognizerBatch, Inventory, Option<TargetManifest>), AssetError> {
    let mut runtime = open_runtime(workspace, batch_id)?;
    let batch: RecognizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет распознавания отсутствует"))?;
    let data = runtime.artifact_directory("data")?;
    let bytes = data.read("inventory.json")?;
    if sha256_hex(&bytes) != batch.inventory_sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "инвентаризация пакета изменена",
        ));
    }
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
    let manifest = match &batch.manifest_sha256 {
        Some(expected) => {
            let bytes = data.read("targets.json")?;
            if sha256_hex(&bytes) != *expected {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "манифест целей пакета изменён",
                ));
            }
            let manifest: TargetManifest =
                serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
            manifest.validate()?;
            Some(manifest)
        }
        None => None,
    };
    Ok((batch, inventory, manifest))
}

pub fn cancel(workspace: &Path, batch_id: &str) -> Result<RecognizerBatch, AssetError> {
    let mut runtime = open_runtime(workspace, batch_id)?;
    let mut batch: RecognizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет распознавания отсутствует"))?;
    if !batch.cancelled {
        batch.cancelled = true;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}

// ---------------------------------------------------------------------------
// Кэш
// ---------------------------------------------------------------------------

/// Кэш рабочего каталога: эмиссии и результаты. Держит исключительную блокировку.
pub struct ResultCache {
    runtime: SafeBatchRuntime,
}

fn result_name(config_sha: &str, item_key: &str) -> String {
    format!("{}-{item_key}.json", &config_sha[..16])
}

fn emission_name(model: &ModelHandle, input_sha: &str) -> String {
    format!("{}-{input_sha}.f32", &model.identity.fingerprint()[..16])
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmissionMeta {
    model_fingerprint: String,
    input_sha256: String,
    frames: usize,
    vocab: usize,
    sha256: String,
}

impl ResultCache {
    pub fn open(workspace: &Path) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(workspace, CACHE_ID)?,
        })
    }

    /// Готовый результат пары или `None`. Повреждённая или несогласованная
    /// запись считается отсутствующей и пересчитывается.
    pub fn get(
        &self,
        config_sha: &str,
        item: &WorkItem,
    ) -> Result<Option<RecognitionResult>, AssetError> {
        let bytes = match self
            .runtime
            .artifact_directory("results")?
            .read(&result_name(config_sha, &item.key()))
        {
            Ok(bytes) => bytes,
            Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(None),
            Err(error) => return Err(error),
        };
        let Ok(result) = serde_json::from_slice::<RecognitionResult>(&bytes) else {
            return Ok(None);
        };
        let key_ok = result.input.sha256 == item.sha256
            && result.config_sha256 == config_sha
            && result.target_key == item.target.as_ref().map(TargetIdentity::matching_key);
        if !key_ok || result.validate().is_err() {
            return Ok(None);
        }
        if let Some(output) = &result.output
            && let Some(path) = &output.storage_path
            && self
                .runtime
                .read_blob(&RuntimeBlobRef {
                    sha256: output.sha256.clone(),
                    storage_path: path.clone(),
                })
                .is_err()
        {
            return Ok(None);
        }
        Ok(Some(result))
    }

    fn put(&self, mut recognized: Recognized) -> Result<RecognitionResult, AssetError> {
        recognized.result.validate()?;
        if let (Some(bytes), Some(output)) = (
            recognized.output_bytes.as_deref(),
            recognized.result.output.as_mut(),
        ) {
            let blob = self
                .runtime
                .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)?;
            if blob.sha256 != output.sha256 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "SHA кандидата не совпал",
                ));
            }
            output.storage_path = Some(blob.storage_path);
        }
        if let (Some(bytes), Some(full)) = (
            recognized.full_record_bytes.as_deref(),
            recognized
                .result
                .full_record
                .as_mut()
                .and_then(|f| f.output.as_mut()),
        ) {
            let blob = self
                .runtime
                .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)?;
            if blob.sha256 == full.sha256 {
                full.storage_path = Some(blob.storage_path);
            }
        }
        let item_key = format!(
            "{}-{}",
            recognized.result.input.sha256,
            recognized
                .result
                .target_key
                .clone()
                .unwrap_or_else(|| "none".into())
        );
        let bytes = serde_json::to_vec_pretty(&recognized.result).expect("результат сериализуем");
        self.runtime.artifact_directory("results")?.write(
            &result_name(&recognized.result.config_sha256, &item_key),
            &bytes,
        )?;
        Ok(recognized.result)
    }

    fn emissions_get(
        &self,
        model: &ModelHandle,
        input_sha: &str,
    ) -> Result<Option<Emissions>, AssetError> {
        let dir = self.runtime.artifact_directory("emissions")?;
        let name = emission_name(model, input_sha);
        let (bytes, meta) = match (dir.read(&name), dir.read(&format!("{name}.json"))) {
            (Ok(b), Ok(m)) => (b, m),
            _ => return Ok(None),
        };
        let Ok(meta) = serde_json::from_slice::<EmissionMeta>(&meta) else {
            return Ok(None);
        };
        if meta.model_fingerprint != model.identity.fingerprint()
            || meta.input_sha256 != input_sha
            || sha256_hex(&bytes) != meta.sha256
            || bytes.len() != meta.frames * meta.vocab * 4
        {
            return Ok(None);
        }
        let values = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        Ok(Emissions::new(meta.frames, meta.vocab, values))
    }

    fn emissions_put(
        &self,
        model: &ModelHandle,
        input_sha: &str,
        e: &Emissions,
    ) -> Result<(), AssetError> {
        let dir = self.runtime.artifact_directory("emissions")?;
        let name = emission_name(model, input_sha);
        let bytes: Vec<u8> = e.log_probs.iter().flat_map(|v| v.to_le_bytes()).collect();
        let meta = EmissionMeta {
            model_fingerprint: model.identity.fingerprint(),
            input_sha256: input_sha.into(),
            frames: e.frames,
            vocab: e.vocab,
            sha256: sha256_hex(&bytes),
        };
        dir.write(&name, &bytes)?;
        dir.write(
            &format!("{name}.json"),
            &serde_json::to_vec(&meta).expect("сериализуемо"),
        )
    }

    /// Байты сохранённого итогового кандидата.
    pub fn output_bytes(&self, result: &RecognitionResult) -> Result<Option<Vec<u8>>, AssetError> {
        let Some(output) = &result.output else {
            return Ok(None);
        };
        let Some(path) = &output.storage_path else {
            return Ok(None);
        };
        Ok(Some(self.runtime.read_blob(&RuntimeBlobRef {
            sha256: output.sha256.clone(),
            storage_path: path.clone(),
        })?))
    }
}

/// Эмиссии через кэш: промахи считаются группами одним вызовом воркера.
struct CachedEmissions<'a> {
    cache: &'a ResultCache,
    direct: pipeline::DirectEmissions,
    stats: std::sync::Mutex<(u64, u64, f64)>,
}

impl EmissionSource for CachedEmissions<'_> {
    fn emissions(
        &self,
        model: &ModelHandle,
        inputs: &[&[f32]],
    ) -> Result<Vec<Emissions>, AssetError> {
        let keys: Vec<String> = inputs
            .iter()
            .map(|s| sha256_hex(s.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()))
            .collect();
        let mut out: Vec<Option<Emissions>> = keys
            .iter()
            .map(|k| self.cache.emissions_get(model, k))
            .collect::<Result<_, _>>()?;
        let missing: Vec<usize> = (0..inputs.len()).filter(|i| out[*i].is_none()).collect();
        if !missing.is_empty() {
            let started = Instant::now();
            let batch: Vec<&[f32]> = missing.iter().map(|&i| inputs[i]).collect();
            let computed = self.direct.emissions(model, &batch)?;
            {
                let mut stats = self.stats.lock().expect("статистика");
                stats.0 += batch.len().div_ceil(self.direct.call_items.max(1)) as u64;
                stats.1 += batch.len() as u64;
                stats.2 += started.elapsed().as_secs_f64() * 1000.0;
            }
            for (&i, e) in missing.iter().zip(computed) {
                self.cache.emissions_put(model, &keys[i], &e)?;
                out[i] = Some(e);
            }
        }
        Ok(out.into_iter().map(|e| e.expect("заполнено")).collect())
    }
}

// ---------------------------------------------------------------------------
// Исполнение
// ---------------------------------------------------------------------------

pub struct ResumeOptions {
    pub jobs: Option<usize>,
    pub max_items: Option<usize>,
    pub recognizer_bin: Option<PathBuf>,
    pub enhancer_bin: Option<PathBuf>,
    pub models_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub outcome: &'static str,
    pub processed_now: u64,
    pub reused: u64,
    pub remaining: u64,
    pub infrastructure_failed_now: u64,
    pub wall_ms: f64,
    pub batch: RecognizerBatch,
}

/// Обрабатывает недостающие пары. `stop` выставляется обработчиком сигналов.
pub fn resume(
    workspace: &Path,
    batch_id: &str,
    options: ResumeOptions,
    stop: &AtomicBool,
) -> Result<RunSummary, AssetError> {
    let started = Instant::now();
    let mut runtime = open_runtime(workspace, batch_id)?;
    let mut batch: RecognizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет распознавания отсутствует"))?;
    if batch.cancelled {
        return Err(invalid("пакет распознавания отменён"));
    }
    let _lease = runtime.execution_lease()?;
    runtime.release_lock()?;
    let (_, inventory, manifest) = load(workspace, batch_id)?;
    runtime.reacquire_lock()?;
    let setup = crate::pronunciation_recognizer_cli::setup_from(
        batch.profile.clone(),
        batch.base_profile.clone(),
        batch.with_enhancer,
        options.recognizer_bin.as_deref(),
        options.enhancer_bin.as_deref(),
        options.models_root.as_deref(),
    )?;
    if setup.config_sha256 != batch.config_sha256
        || setup.toolchain != batch.toolchain
        || setup.primary.identity.fingerprint() != batch.primary_fingerprint
        || setup.verifier.as_ref().map(|v| v.identity.fingerprint()) != batch.verifier_fingerprint
    {
        return Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            "модель, воркер, FFmpeg или конфигурация изменились с начала пакета: результаты несопоставимы, начните новый пакет",
            serde_json::json!({"blocker": "recognizer_backend_changed"}),
        ));
    }
    let worker_snapshot = ebackend::worker_snapshot(&setup.primary.worker)?;
    let jobs = options.jobs.unwrap_or(batch.limits.jobs);
    if jobs == 0 || jobs > nbatch::MAX_JOBS {
        return Err(invalid(format!(
            "--jobs должен быть 1–{}",
            nbatch::MAX_JOBS
        )));
    }
    runtime.release_lock()?;
    let cache = ResultCache::open(workspace)?;
    let source = CachedEmissions {
        cache: &cache,
        direct: setup.direct(),
        stats: std::sync::Mutex::new((0, 0, 0.0)),
    };
    let ctx = setup.context(&source);
    let root = nbatch::open_dir(Path::new(&batch.input_root))?;
    let (items, _) = work_items(
        &inventory,
        manifest.as_ref(),
        batch.manifest_sha256.as_deref(),
    );
    let mut done: BTreeMap<String, RecognitionResult> = BTreeMap::new();
    let mut pending = Vec::new();
    for item in &items {
        match cache.get(&batch.config_sha256, item)? {
            Some(result) => {
                done.insert(item.key(), result);
            }
            None => pending.push(item.clone()),
        }
    }
    let reused = done.len() as u64;
    pending.truncate(options.max_items.unwrap_or(usize::MAX));
    let mut infra = batch.progress.infrastructure_failures.clone();
    let mut source_errors = batch.progress.source_errors.clone();
    let mut processed_now = 0u64;
    let mut infra_now = 0u64;
    let mut consecutive_infra = 0usize;
    let mut failure: Option<AssetError> = None;
    let mut cancelled = false;
    let mut last_checkpoint = Instant::now();
    let processed_before = batch.progress.processed;
    let checkpoint = |batch: &mut RecognizerBatch,
                      runtime: &mut SafeBatchRuntime,
                      done: &BTreeMap<String, RecognitionResult>,
                      processed: u64,
                      infra: &BTreeMap<String, u32>,
                      source_errors: &BTreeMap<String, String>,
                      stats: (u64, u64, f64)|
     -> Result<bool, AssetError> {
        runtime.reacquire_lock()?;
        let mut stored: RecognizerBatch = runtime
            .reload_cached()?
            .ok_or_else(|| invalid("пакет распознавания исчез"))?;
        let mut by_outcome = BTreeMap::new();
        let mut by_action = BTreeMap::new();
        for result in done.values() {
            *by_outcome
                .entry(result.outcome.as_str().to_string())
                .or_insert(0) += 1;
            *by_action
                .entry(result.next_action.as_str().to_string())
                .or_insert(0) += 1;
        }
        let complete_inputs: BTreeSet<&str> = items
            .iter()
            .map(|i| i.sha256.as_str())
            .filter(|sha| {
                items
                    .iter()
                    .filter(|i| i.sha256 == *sha)
                    .all(|i| done.contains_key(&i.key()))
            })
            .collect();
        stored.progress.completed_pairs = done.len() as u64;
        stored.progress.completed_inputs = complete_inputs.len() as u64;
        stored.progress.processed = processed;
        stored.progress.by_outcome = by_outcome;
        stored.progress.by_action = by_action;
        stored.progress.infrastructure_failures = infra.clone();
        stored.progress.source_errors = source_errors.clone();
        stored.progress.model_calls += stats.0;
        stored.progress.model_items += stats.1;
        stored.progress.model_wall_ms += stats.2;
        stored.revision += 1;
        runtime.save(&stored)?;
        runtime.release_lock()?;
        *batch = stored;
        Ok(batch.cancelled)
    };
    let take_stats = |source: &CachedEmissions<'_>| {
        let mut s = source.stats.lock().expect("статистика");
        std::mem::replace(&mut *s, (0, 0, 0.0))
    };
    'chunks: for chunk in pending.chunks(ctx.profile.models.call_items.max(1)) {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Чтение и сверка входов группы.
        let mut inputs: Vec<(WorkItem, Vec<u8>)> = Vec::new();
        for item in chunk {
            match nbatch::read_input(&root, &item.path, &item.sha256) {
                Ok(bytes) => inputs.push((item.clone(), bytes)),
                Err(error) => {
                    if source_errors.len() < MAX_RECORDED_ERRORS {
                        source_errors.insert(item.path.clone(), error.message);
                    }
                }
            }
        }
        // Фаза A: эмиссии исходников группы одним вызовом на модель.
        let mut unique: BTreeMap<&str, &[u8]> = BTreeMap::new();
        for (item, bytes) in &inputs {
            unique.insert(item.sha256.as_str(), bytes.as_slice());
        }
        let decoded: Vec<Vec<f32>> = std::thread::scope(|scope| {
            let handles: Vec<_> = unique
                .values()
                .map(|bytes| {
                    scope.spawn(|| match pipeline::decode(bytes, ctx.base_profile) {
                        Ok(Ok(d)) => Some(d.analysis),
                        _ => None,
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect()
        });
        let refs: Vec<&[f32]> = decoded.iter().map(Vec::as_slice).collect();
        let prefetch = source
            .emissions(ctx.primary, &refs)
            .and_then(|_| match ctx.verifier {
                Some(v) => source.emissions(v, &refs).map(|_| ()),
                None => Ok(()),
            });
        if let Err(error) = prefetch {
            if pipeline::is_infrastructure_failure(&error) {
                infra_now += inputs.len() as u64;
                for (item, _) in &inputs {
                    *infra.entry(item.key()).or_default() += 1;
                }
                consecutive_infra += inputs.len();
                if consecutive_infra >= MAX_CONSECUTIVE_INFRA_FAILURES {
                    failure = Some(error);
                    break 'chunks;
                }
                continue;
            }
            failure = Some(error);
            break;
        }
        // Фаза B: этап 1 параллельно.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stage_one: Vec<(usize, Result<pipeline::StageOne, AssetError>)> =
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..jobs.min(inputs.len()).max(1))
                    .map(|_| {
                        scope.spawn(|| {
                            let mut local = Vec::new();
                            loop {
                                if stop.load(Ordering::SeqCst) {
                                    break;
                                }
                                let i = next.fetch_add(1, Ordering::SeqCst);
                                let Some((item, bytes)) = inputs.get(i) else {
                                    break;
                                };
                                local.push((
                                    i,
                                    pipeline::stage_one(bytes, item.target.as_ref(), &ctx),
                                ));
                            }
                            local
                        })
                    })
                    .collect();
                let mut all: Vec<_> = workers
                    .into_iter()
                    .flat_map(|w| w.join().unwrap_or_default())
                    .collect();
                all.sort_by_key(|(i, _)| *i);
                all
            });
        // Фаза C: эмиссии итоговых кандидатов одним вызовом на модель.
        let finals: Vec<Vec<f32>> = stage_one
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok()?.needs_post_validation())
            .filter_map(|bytes| match pipeline::decode(bytes, ctx.base_profile) {
                Ok(Ok(d)) => Some(d.analysis),
                _ => None,
            })
            .collect();
        let refs: Vec<&[f32]> = finals.iter().map(Vec::as_slice).collect();
        let _ = source
            .emissions(ctx.primary, &refs)
            .and_then(|_| match ctx.verifier {
                Some(v) => source.emissions(v, &refs).map(|_| ()),
                None => Ok(()),
            });
        // Фаза D: этап 2 и сохранение.
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if ebackend::worker_snapshot(&setup.primary.worker)? != worker_snapshot {
            failure = Some(AssetError::with_details(
                ErrorCode::ValidatorFailure,
                "воркер распознавания заменён во время пакета: результат не сохранён",
                serde_json::json!({"blocker": "recognizer_backend_changed"}),
            ));
            break;
        }
        for (i, stage) in stage_one {
            let item = &inputs[i].0;
            let outcome = stage.and_then(|s| pipeline::stage_two(s, &ctx));
            match outcome {
                Ok(recognized) => match cache.put(recognized) {
                    Ok(result) => {
                        consecutive_infra = 0;
                        processed_now += 1;
                        infra.remove(&item.key());
                        source_errors.remove(&item.path);
                        done.insert(item.key(), result);
                    }
                    Err(error) => {
                        failure = Some(error);
                        break 'chunks;
                    }
                },
                Err(error) if pipeline::is_infrastructure_failure(&error) => {
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    infra_now += 1;
                    consecutive_infra += 1;
                    *infra.entry(item.key()).or_default() += 1;
                    if consecutive_infra >= MAX_CONSECUTIVE_INFRA_FAILURES {
                        failure = Some(error);
                        break 'chunks;
                    }
                }
                Err(error) => {
                    failure = Some(error);
                    break 'chunks;
                }
            }
        }
        if last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            last_checkpoint = Instant::now();
            match checkpoint(
                &mut batch,
                &mut runtime,
                &done,
                processed_before + processed_now,
                &infra,
                &source_errors,
                take_stats(&source),
            ) {
                Ok(true) => {
                    cancelled = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
    }
    let final_checkpoint = checkpoint(
        &mut batch,
        &mut runtime,
        &done,
        processed_before + processed_now,
        &infra,
        &source_errors,
        take_stats(&source),
    );
    if let Some(error) = failure {
        return Err(error);
    }
    cancelled |= final_checkpoint?;
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    runtime.reacquire_lock()?;
    if let Some(mut stored) = runtime.reload_cached::<RecognizerBatch>()? {
        stored.progress.wall_ms += wall_ms;
        stored.revision += 1;
        runtime.save(&stored)?;
        batch = stored;
    }
    runtime.release_lock()?;
    let remaining = (items.len() as u64).saturating_sub(done.len() as u64);
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
        infrastructure_failed_now: infra_now,
        wall_ms,
        batch,
    })
}

/// Результаты пакета в порядке пар.
pub fn collect_results(
    cache: &ResultCache,
    batch: &RecognizerBatch,
    inventory: &Inventory,
    manifest: Option<&TargetManifest>,
) -> Result<Vec<(WorkItem, RecognitionResult)>, AssetError> {
    let (items, _) = work_items(inventory, manifest, batch.manifest_sha256.as_deref());
    let mut out = Vec::new();
    for item in items {
        if let Some(result) = cache.get(&batch.config_sha256, &item)? {
            out.push((item, result));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pronunciation::normalizer_batch::InventoryEntry;

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn inventory() -> Inventory {
        Inventory {
            schema_version: 1,
            entries: vec![
                InventoryEntry {
                    path: "a.mp3".into(),
                    byte_length: 1,
                    sha256: Some(sha('a')),
                    error: None,
                    content_sha256: None,
                },
                InventoryEntry {
                    path: "copy-of-a.mp3".into(),
                    byte_length: 1,
                    sha256: Some(sha('a')),
                    error: None,
                    content_sha256: None,
                },
                InventoryEntry {
                    path: "b.mp3".into(),
                    byte_length: 1,
                    sha256: Some(sha('b')),
                    error: None,
                    content_sha256: None,
                },
            ],
            skipped: Vec::new(),
        }
    }

    fn manifest() -> TargetManifest {
        TargetManifest {
            schema_version: 1,
            kind: "pronunciation_target_manifest".into(),
            provenance: "test".into(),
            entries: vec![
                ManifestEntry {
                    id: "nama".into(),
                    sha256: Some(sha('a')),
                    path: None,
                    targets: vec![ManifestTarget {
                        surface: "生".into(),
                        reading: "なま".into(),
                        competing_readings: vec!["せい".into()],
                    }],
                },
                ManifestEntry {
                    id: "sei".into(),
                    sha256: None,
                    path: Some("copy-of-a.mp3".into()),
                    targets: vec![ManifestTarget {
                        surface: "生".into(),
                        reading: "せい".into(),
                        competing_readings: vec![],
                    }],
                },
                ManifestEntry {
                    id: "missing".into(),
                    sha256: Some(sha('c')),
                    path: None,
                    targets: vec![ManifestTarget {
                        surface: "木".into(),
                        reading: "き".into(),
                        competing_readings: vec![],
                    }],
                },
            ],
        }
    }

    #[test]
    fn one_sha_serves_several_targets_and_untargeted_inputs_stay_target_free() {
        let m = manifest();
        m.validate().unwrap();
        let (items, unresolved) = work_items(&inventory(), Some(&m), Some(&sha('f')));
        assert_eq!(unresolved, ["missing"]);
        let a: Vec<&WorkItem> = items.iter().filter(|i| i.sha256 == sha('a')).collect();
        assert_eq!(
            a.len(),
            2,
            "дубликат файла не удваивает пары, но обе цели сохранены"
        );
        let readings: BTreeSet<&str> = a
            .iter()
            .map(|i| i.target.as_ref().unwrap().reading.as_str())
            .collect();
        assert_eq!(readings, BTreeSet::from(["なま", "せい"]));
        assert_ne!(a[0].key(), a[1].key());
        let b: Vec<&WorkItem> = items.iter().filter(|i| i.sha256 == sha('b')).collect();
        assert_eq!(b.len(), 1);
        assert!(b[0].target.is_none(), "имя файла цель не задаёт");
    }

    #[test]
    fn manifest_validation_rejects_bad_entries() {
        let mut m = manifest();
        m.entries[0].targets[0].reading = "生".into();
        assert!(
            m.validate().is_ok(),
            "чтение-кандзи допустимо как пара, но не сопоставимо"
        );
        let mut m = manifest();
        m.entries[1].id = "nama".into();
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.entries[0].sha256 = Some("XYZ".into());
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.entries[0].targets.clear();
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.kind = "other".into();
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.entries[0].sha256 = None;
        m.entries[0].path = None;
        assert!(m.validate().is_err());
    }

    #[test]
    fn conflicting_sha_and_path_is_unresolved_not_guessed() {
        let mut m = manifest();
        m.entries[1].sha256 = Some(sha('b'));
        let (_, unresolved) = work_items(&inventory(), Some(&m), Some(&sha('f')));
        assert!(unresolved.contains(&"sei".to_string()));
    }
}
