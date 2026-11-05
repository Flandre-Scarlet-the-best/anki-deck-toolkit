//! Пакетный анализ и нормализация каталога MP3 с контрольными точками.
//!
//! Каталог входа открывается только на чтение: обход не следует символическим
//! ссылкам, каждый файл открывается относительно дескриптора корня с `NOFOLLOW`.
//! Всё состояние и все результаты живут в отдельном рабочем каталоге в штатной
//! области `SafeBatchRuntime`:
//!
//! - `.runtime/batches/<batch-id>/` — состояние пакета и неизменяемая инвентаризация;
//! - `.runtime/batches/normalizer-cache/` — общий для рабочего каталога кэш:
//!   `results/<input-sha>.<config-sha>.json` и обработанные MP3 в `candidates/`.
//!
//! Результат каждого входа — отдельная атомарная контрольная точка. Повторный
//! запуск с тем же `(SHA входа, профиль, версия, параметры, инструменты)` не
//! повторяет работу ни в том же, ни в новом пакете того же рабочего каталога.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open, openat};
use serde::{Deserialize, Serialize};

use super::MAX_AUDIO_BYTES;
use super::normalizer::{
    self, GateDecision, GateOutcome, InputRef, Mode as RunMode, NormalizationResult,
    NormalizerProfile, ProfileRef, Reason, Severity, Toolchain, Variant,
};
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_ID: &str = "normalizer-cache";
pub const DEFAULT_MAX_FILES: usize = 20_000;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_JOBS: usize = 32;
const MAX_DEPTH: usize = 16;
const MAX_INFRA_ATTEMPTS: u32 = 3;
const MAX_CONSECUTIVE_INFRA_FAILURES: usize = 8;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RECORDED_ERRORS: usize = 1000;

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn corrupt(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::ManifestCorrupt, message)
}

/// Запись инвентаризации: относительный путь внутри корня входа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub path: String,
    pub byte_length: u64,
    pub sha256: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema_version: u32,
    pub entries: Vec<InventoryEntry>,
    pub skipped: Vec<SkippedEntry>,
}

impl Inventory {
    /// Уникальные SHA читаемых записей в порядке первого появления.
    pub fn unique_inputs(&self) -> Vec<(&str, &str)> {
        let mut seen = BTreeSet::new();
        self.entries
            .iter()
            .filter_map(|e| e.sha256.as_deref().map(|sha| (sha, e.path.as_str())))
            .filter(|(sha, _)| seen.insert(*sha))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub jobs: usize,
    pub max_files: usize,
    pub max_total_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub unique_inputs: u64,
    /// Входы с сохранённым результатом (по последней контрольной точке).
    pub completed: u64,
    pub by_outcome: BTreeMap<String, u64>,
    /// Повторяемые сбои среды по SHA; после предела вход получает REVIEW_REQUIRED.
    pub infrastructure_failures: BTreeMap<String, u32>,
    /// Входы, байты которых больше не совпадают с инвентаризацией.
    pub source_errors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizerBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub cancelled: bool,
    pub mode: RunMode,
    pub profile: NormalizerProfile,
    pub toolchain: Toolchain,
    pub config_sha256: String,
    /// Канонический абсолютный корень входа; хранится только в приватном рабочем каталоге.
    pub input_root: String,
    pub inventory_sha256: String,
    pub limits: Limits,
    pub progress: Progress,
}

impl RuntimeBatchState for NormalizerBatch {
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
            return Err(corrupt("неподдерживаемая схема пакета нормализации"));
        }
        self.profile.validate()?;
        if normalizer::config_sha256(&self.profile, self.mode, &self.toolchain)
            != self.config_sha256
            || !valid_sha(&self.inventory_sha256)
            || !Path::new(&self.input_root).is_absolute()
            || self.limits.jobs == 0
            || self.limits.jobs > MAX_JOBS
            || self
                .progress
                .infrastructure_failures
                .keys()
                .any(|k| !valid_sha(k))
        {
            return Err(corrupt("противоречивое состояние пакета нормализации"));
        }
        Ok(())
    }
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn result_name(input_sha: &str, config_sha: &str) -> String {
    format!("{input_sha}.{config_sha}.json")
}

fn is_mp3_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("mp3"))
}

fn open_dir(path: &Path) -> Result<File, AssetError> {
    Ok(File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| AssetError::io("открытие каталога входа", e.into()))?,
    ))
}

/// Открывает файл относительно корня; каждый компонент — без перехода по ссылкам.
fn open_relative(root: &File, relative: &str) -> Result<File, AssetError> {
    let parts: Vec<&str> = relative.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\0'))
    {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "некорректный относительный путь входа",
        ));
    }
    let mut current = root
        .try_clone()
        .map_err(|e| AssetError::io("дескриптор", e))?;
    for (index, part) in parts.iter().enumerate() {
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if index + 1 != parts.len() {
            flags |= OFlags::DIRECTORY;
        }
        current = File::from(
            openat(&current, *part, flags, Mode::empty())
                .map_err(|e| AssetError::io("открытие файла входа", e.into()))?,
        );
    }
    if !current
        .metadata()
        .map_err(|e| AssetError::io("метаданные входа", e))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "вход не является обычным файлом",
        ));
    }
    Ok(current)
}

fn read_limited(mut file: File) -> Result<Vec<u8>, AssetError> {
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_AUDIO_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| AssetError::io("чтение входа", e))?;
    if bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err(invalid("файл превышает предел 8 MiB"));
    }
    Ok(bytes)
}

/// Повторно читает вход и сверяет его SHA с инвентаризацией.
pub fn read_input(root: &File, relative: &str, expected_sha: &str) -> Result<Vec<u8>, AssetError> {
    let bytes = read_limited(open_relative(root, relative)?)?;
    if sha256_hex(&bytes) != expected_sha {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "файл входа изменился после инвентаризации",
        ));
    }
    Ok(bytes)
}

/// Рекурсивный обход без перехода по ссылкам. Сортировка по байтам имени
/// делает инвентаризацию воспроизводимой между запусками.
pub fn inventory(root: &Path, limits: &Limits) -> Result<Inventory, AssetError> {
    let root_fd = open_dir(root)?;
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((directory, prefix, depth)) = stack.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&directory)
            .map_err(|e| AssetError::io("обход каталога входа", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AssetError::io("обход каталога входа", e))?;
        names.sort_by_key(|e| e.file_name());
        let mut subdirectories = Vec::new();
        for entry in names {
            let raw = entry.file_name();
            let Some(name) = raw.to_str().map(str::to_owned) else {
                skipped.push(SkippedEntry {
                    path: format!("{prefix}{}", raw.to_string_lossy()),
                    reason: "non_utf8_name".into(),
                });
                continue;
            };
            let relative = format!("{prefix}{name}");
            let kind = entry
                .file_type()
                .map_err(|e| AssetError::io("тип записи каталога", e))?;
            if kind.is_symlink() {
                skipped.push(SkippedEntry {
                    path: relative,
                    reason: "symlink".into(),
                });
            } else if kind.is_dir() {
                if depth + 1 >= MAX_DEPTH {
                    skipped.push(SkippedEntry {
                        path: relative,
                        reason: "depth_limit".into(),
                    });
                } else {
                    subdirectories.push((entry.path(), format!("{relative}/"), depth + 1));
                }
            } else if !kind.is_file() {
                skipped.push(SkippedEntry {
                    path: relative,
                    reason: "not_regular".into(),
                });
            } else if name.contains(['\\', '\0']) || name.chars().any(char::is_control) {
                skipped.push(SkippedEntry {
                    path: relative,
                    reason: "unsafe_name".into(),
                });
            } else if !is_mp3_name(&name) {
                skipped.push(SkippedEntry {
                    path: relative,
                    reason: "unsupported_extension".into(),
                });
            } else {
                if entries.len() >= limits.max_files {
                    return Err(invalid(format!(
                        "каталог содержит больше {} MP3: увеличьте --max-files",
                        limits.max_files
                    )));
                }
                let read = open_relative(&root_fd, &relative).and_then(|file| {
                    let length = file
                        .metadata()
                        .map_err(|e| AssetError::io("метаданные входа", e))?
                        .len();
                    Ok((length, read_limited(file)))
                });
                let entry = match read {
                    // Длина — по фактически прочитанным байтам, а не по метаданным.
                    Ok((_, Ok(bytes))) => {
                        total = total.saturating_add(bytes.len() as u64);
                        InventoryEntry {
                            path: relative,
                            byte_length: bytes.len() as u64,
                            sha256: Some(sha256_hex(&bytes)),
                            error: None,
                        }
                    }
                    Ok((length, Err(error))) => InventoryEntry {
                        path: relative,
                        byte_length: length,
                        sha256: None,
                        error: Some(error.message),
                    },
                    Err(error) => InventoryEntry {
                        path: relative,
                        byte_length: 0,
                        sha256: None,
                        error: Some(error.message),
                    },
                };
                if total > limits.max_total_bytes {
                    return Err(invalid(
                        "суммарный размер входа превышает --max-total-bytes",
                    ));
                }
                entries.push(entry);
            }
        }
        for item in subdirectories.into_iter().rev() {
            stack.push(item);
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Inventory {
        schema_version: 1,
        entries,
        skipped,
    })
}

/// Пути входа и рабочего каталога не должны пересекаться друг с другом и с
/// защищёнными корнями (`decks/**`).
pub fn check_boundaries(
    input_root: &Path,
    workspace: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<(), AssetError> {
    let overlaps = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
    if overlaps(input_root, workspace) {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "каталог входа и рабочий каталог должны быть раздельными и невложенными",
        ));
    }
    for root in protected {
        let resolved = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if overlaps(input_root, &resolved)
            || overlaps(workspace, &resolved)
            || overlaps(input_root, root)
            || overlaps(workspace, root)
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "нормализатор не читает и не пишет в защищённые каталоги (decks/**)",
            ));
        }
    }
    Ok(())
}

/// Канонизирует существующий каталог и запрещает ссылки в любом компоненте.
pub fn canonical_directory(path: &Path) -> Result<PathBuf, AssetError> {
    let absolute = std::path::absolute(path).map_err(|e| AssetError::io("путь", e))?;
    let canonical = std::fs::canonicalize(&absolute).map_err(|e| AssetError::io("путь", e))?;
    if canonical != absolute {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "путь каталога не должен содержать символических ссылок, . и ..",
        ));
    }
    if !canonical.is_dir() {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "ожидается каталог",
        ));
    }
    Ok(canonical)
}

pub struct StartRequest<'a> {
    pub batch_id: &'a str,
    pub workspace: &'a Path,
    pub input_root: &'a Path,
    pub mode: RunMode,
    pub profile: NormalizerProfile,
    pub limits: Limits,
}

/// Фиксирует инвентаризацию и конфигурацию; обработку выполняет [`resume`].
pub fn start(request: StartRequest<'_>) -> Result<NormalizerBatch, AssetError> {
    request.profile.validate()?;
    if request.limits.jobs == 0 || request.limits.jobs > MAX_JOBS {
        return Err(invalid(format!("--jobs должен быть 1–{MAX_JOBS}")));
    }
    let toolchain = Toolchain::detect()?;
    let inventory = inventory(request.input_root, &request.limits)?;
    let inventory_bytes = serde_json::to_vec(&inventory).expect("инвентаризация сериализуема");
    let mut runtime = SafeBatchRuntime::open(request.workspace, request.batch_id)?;
    if runtime.load::<NormalizerBatch>()?.is_some() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "пакет уже существует; используйте resume",
        ));
    }
    let data = runtime.artifact_directory("data")?;
    data.write("inventory.json", &inventory_bytes)?;
    let config_sha256 = normalizer::config_sha256(&request.profile, request.mode, &toolchain);
    let batch = NormalizerBatch {
        schema_version: STATE_SCHEMA_VERSION,
        batch_id: request.batch_id.into(),
        revision: 0,
        cancelled: false,
        mode: request.mode,
        profile: request.profile,
        toolchain,
        config_sha256,
        input_root: request
            .input_root
            .to_str()
            .ok_or_else(|| invalid("путь входа должен быть UTF-8"))?
            .into(),
        inventory_sha256: sha256_hex(&inventory_bytes),
        limits: request.limits,
        progress: Progress {
            unique_inputs: inventory.unique_inputs().len() as u64,
            ..Progress::default()
        },
    };
    runtime.save(&batch)?;
    Ok(batch)
}

pub fn load(workspace: &Path, batch_id: &str) -> Result<(NormalizerBatch, Inventory), AssetError> {
    let mut runtime = SafeBatchRuntime::open(workspace, batch_id)?;
    let batch: NormalizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет нормализации отсутствует"))?;
    let bytes = runtime.artifact_directory("data")?.read("inventory.json")?;
    if sha256_hex(&bytes) != batch.inventory_sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "инвентаризация пакета изменена",
        ));
    }
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
    Ok((batch, inventory))
}

pub fn status(workspace: &Path, batch_id: &str) -> Result<NormalizerBatch, AssetError> {
    Ok(load(workspace, batch_id)?.0)
}

pub fn cancel(workspace: &Path, batch_id: &str) -> Result<NormalizerBatch, AssetError> {
    let mut runtime = SafeBatchRuntime::open(workspace, batch_id)?;
    let mut batch: NormalizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет нормализации отсутствует"))?;
    if !batch.cancelled {
        batch.cancelled = true;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}

/// Кэш результатов рабочего каталога. Держит исключительную блокировку.
pub struct ResultCache {
    runtime: SafeBatchRuntime,
}

impl ResultCache {
    pub fn open(workspace: &Path) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(workspace, CACHE_ID)?,
        })
    }

    pub fn get(
        &self,
        input_sha: &str,
        config_sha: &str,
    ) -> Result<Option<NormalizationResult>, AssetError> {
        match self
            .runtime
            .artifact_directory("results")?
            .read(&result_name(input_sha, config_sha))
        {
            Ok(bytes) => {
                let result: NormalizationResult =
                    serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
                if result.input.sha256 != input_sha || result.config_sha256 != config_sha {
                    return Err(corrupt("результат кэша не соответствует своему ключу"));
                }
                Ok(Some(result))
            }
            Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn put(
        &self,
        mut result: NormalizationResult,
        output: Option<&[u8]>,
    ) -> Result<NormalizationResult, AssetError> {
        if let (Some(bytes), Some(reference)) = (output, result.output.as_mut()) {
            let blob = self
                .runtime
                .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)?;
            if blob.sha256 != reference.sha256 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "SHA сохранённого варианта не совпал",
                ));
            }
            reference.storage_path = Some(blob.storage_path);
        }
        let bytes = serde_json::to_vec_pretty(&result).expect("результат сериализуем");
        self.runtime
            .artifact_directory("results")?
            .write_if_absent(
                &result_name(&result.input.sha256, &result.config_sha256),
                &bytes,
            )?;
        Ok(result)
    }

    /// Читает обработанный вариант и сверяет SHA.
    pub fn variant_bytes(&self, output: &normalizer::OutputRef) -> Result<Vec<u8>, AssetError> {
        let storage_path = output
            .storage_path
            .clone()
            .ok_or_else(|| invalid("у результата нет сохранённого варианта"))?;
        self.runtime.read_blob(&RuntimeBlobRef {
            sha256: output.sha256.clone(),
            storage_path,
        })
    }

    /// Сохраняет копию исходных байтов перед публикацией производного варианта.
    pub fn retain_original(&self, bytes: &[u8]) -> Result<RuntimeBlobRef, AssetError> {
        self.runtime
            .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)
    }

    pub fn write_record(
        &self,
        directory: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<bool, AssetError> {
        self.runtime
            .artifact_directory(directory)?
            .write_if_absent(name, bytes)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub outcome: &'static str,
    pub processed_now: u64,
    pub reused: u64,
    pub remaining: u64,
    pub batch: NormalizerBatch,
}

enum ItemError {
    Source(String),
    Infrastructure(AssetError),
}

fn failure_result(
    batch: &NormalizerBatch,
    sha: &str,
    length: u64,
    message: &str,
) -> NormalizationResult {
    NormalizationResult {
        schema_version: normalizer::RESULT_SCHEMA_VERSION,
        mode: batch.mode,
        config_sha256: batch.config_sha256.clone(),
        profile: ProfileRef {
            id: batch.profile.id.clone(),
            version: batch.profile.version,
        },
        toolchain: batch.toolchain.clone(),
        input: InputRef {
            sha256: sha.into(),
            byte_length: length,
            probe: None,
            error: Some(message.into()),
        },
        analysis: None,
        plan: None,
        output: None,
        post_qc: None,
        gate: GateDecision {
            outcome: GateOutcome::ReviewRequired,
            proposed_variant: Some(Variant::Original),
            post_qc_verified: false,
            reasons: vec![Reason {
                code: "processing_failed_repeatedly".into(),
                severity: Severity::Review,
                message: format!(
                    "обработка {MAX_INFRA_ATTEMPTS} раза завершилась сбоем среды: {message}"
                ),
            }],
            unresolved: Vec::new(),
        },
    }
}

/// Обрабатывает недостающие результаты с ограниченной конкуренцией.
/// `stop` выставляется обработчиком SIGINT/SIGTERM вызывающего процесса.
pub fn resume(
    workspace: &Path,
    batch_id: &str,
    jobs: Option<usize>,
    max_items: Option<usize>,
    stop: &AtomicBool,
) -> Result<RunSummary, AssetError> {
    let mut runtime = SafeBatchRuntime::open(workspace, batch_id)?;
    let mut batch: NormalizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет нормализации отсутствует"))?;
    if batch.cancelled {
        return Err(invalid("пакет нормализации отменён"));
    }
    let _lease = runtime.execution_lease()?;
    let (_, inventory) = {
        let bytes = runtime.artifact_directory("data")?.read("inventory.json")?;
        if sha256_hex(&bytes) != batch.inventory_sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "инвентаризация пакета изменена",
            ));
        }
        (
            (),
            serde_json::from_slice::<Inventory>(&bytes).map_err(|e| corrupt(e.to_string()))?,
        )
    };
    let toolchain = Toolchain::detect()?;
    if toolchain != batch.toolchain {
        return Err(invalid(
            "версия FFmpeg/анализатора изменилась: результаты несопоставимы, начните новый пакет",
        ));
    }
    let jobs = jobs.unwrap_or(batch.limits.jobs);
    if jobs == 0 || jobs > MAX_JOBS {
        return Err(invalid(format!("--jobs должен быть 1–{MAX_JOBS}")));
    }
    runtime.release_lock()?;
    let cache = ResultCache::open(workspace)?;
    let root = open_dir(Path::new(&batch.input_root))?;
    let lengths: BTreeMap<&str, u64> = inventory
        .entries
        .iter()
        .filter_map(|e| e.sha256.as_deref().map(|s| (s, e.byte_length)))
        .collect();
    let mut by_outcome: BTreeMap<String, u64> = BTreeMap::new();
    let mut pending = Vec::new();
    let mut reused = 0u64;
    for (sha, path) in inventory.unique_inputs() {
        match cache.get(sha, &batch.config_sha256)? {
            Some(result) => {
                reused += 1;
                *by_outcome
                    .entry(result.gate.outcome.as_str().into())
                    .or_default() += 1;
            }
            None => pending.push((sha.to_owned(), path.to_owned())),
        }
    }
    let total_pending = pending.len();
    pending.truncate(max_items.unwrap_or(usize::MAX));
    let mut processed_now = 0u64;
    let mut failure: Option<AssetError> = None;
    let mut cancelled = false;
    let mut last_checkpoint = Instant::now();
    let halt = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let mut consecutive_infra = 0usize;
    let mut infra = batch.progress.infrastructure_failures.clone();
    let mut source_errors = batch.progress.source_errors.clone();
    let checkpoint = |batch: &mut NormalizerBatch,
                      runtime: &mut SafeBatchRuntime,
                      completed: u64,
                      by_outcome: &BTreeMap<String, u64>,
                      infra: &BTreeMap<String, u32>,
                      source_errors: &BTreeMap<String, String>|
     -> Result<bool, AssetError> {
        runtime.reacquire_lock()?;
        let mut stored: NormalizerBatch = runtime
            .reload_cached()?
            .ok_or_else(|| invalid("пакет нормализации исчез"))?;
        stored.progress.completed = completed;
        stored.progress.by_outcome = by_outcome.clone();
        stored.progress.infrastructure_failures = infra.clone();
        stored.progress.source_errors = source_errors.clone();
        stored.revision += 1;
        runtime.save(&stored)?;
        runtime.release_lock()?;
        *batch = stored;
        Ok(batch.cancelled)
    };
    let profile = batch.profile.clone();
    let mode = batch.mode;
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(jobs);
        for _ in 0..jobs.min(pending.len().max(1)) {
            let sender = sender.clone();
            let (pending, next, halt, root, profile, toolchain) =
                (&pending, &next, &halt, &root, &profile, &toolchain);
            scope.spawn(move || {
                loop {
                    if stop.load(Ordering::SeqCst) || halt.load(Ordering::SeqCst) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some((sha, path)) = pending.get(index) else {
                        break;
                    };
                    let outcome = match read_input(root, path, sha) {
                        Err(error) => Err(ItemError::Source(error.message)),
                        Ok(bytes) => normalizer::process(&bytes, profile, mode, toolchain)
                            .map_err(ItemError::Infrastructure),
                    };
                    if sender.send((index, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (index, outcome) in receiver {
            let (sha, path) = &pending[index];
            let stored = match outcome {
                Ok(processed) => {
                    consecutive_infra = 0;
                    cache.put(processed.result, processed.output_bytes.as_deref())
                }
                Err(ItemError::Source(message)) => {
                    if source_errors.len() < MAX_RECORDED_ERRORS {
                        source_errors.insert(path.clone(), message);
                    }
                    continue;
                }
                Err(ItemError::Infrastructure(error)) => {
                    if stop.load(Ordering::SeqCst) {
                        continue;
                    }
                    consecutive_infra += 1;
                    let attempts = infra.entry(sha.clone()).or_default();
                    *attempts += 1;
                    if consecutive_infra >= MAX_CONSECUTIVE_INFRA_FAILURES {
                        halt.store(true, Ordering::SeqCst);
                        failure.get_or_insert(error);
                        continue;
                    }
                    if *attempts < MAX_INFRA_ATTEMPTS {
                        continue;
                    }
                    cache.put(
                        failure_result(&batch, sha, lengths[sha.as_str()], &error.message),
                        None,
                    )
                }
            };
            match stored {
                Ok(result) => {
                    processed_now += 1;
                    *by_outcome
                        .entry(result.gate.outcome.as_str().into())
                        .or_default() += 1;
                }
                Err(error) => {
                    halt.store(true, Ordering::SeqCst);
                    failure.get_or_insert(error);
                    continue;
                }
            }
            if last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
                last_checkpoint = Instant::now();
                match checkpoint(
                    &mut batch,
                    &mut runtime,
                    reused + processed_now,
                    &by_outcome,
                    &infra,
                    &source_errors,
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
        }
    });
    let final_checkpoint = checkpoint(
        &mut batch,
        &mut runtime,
        reused + processed_now,
        &by_outcome,
        &infra,
        &source_errors,
    );
    if let Some(error) = failure {
        return Err(error);
    }
    cancelled |= final_checkpoint?;
    let remaining = total_pending as u64 - processed_now;
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
        batch,
    })
}

/// Все результаты пакета в порядке инвентаризации (по одному на уникальный SHA).
pub fn collect_results(
    cache: &ResultCache,
    batch: &NormalizerBatch,
    inventory: &Inventory,
) -> Result<Vec<NormalizationResult>, AssetError> {
    let mut results = Vec::new();
    for (sha, _) in inventory.unique_inputs() {
        if let Some(result) = cache.get(sha, &batch.config_sha256)? {
            results.push(result);
        }
    }
    Ok(results)
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceCheck {
    pub checked: u64,
    pub unchanged: u64,
    pub changed: Vec<String>,
    pub unreadable: Vec<String>,
}

/// Повторно хеширует все входы инвентаризации: доказательство, что корпус не изменён.
pub fn verify_sources(
    batch: &NormalizerBatch,
    inventory: &Inventory,
) -> Result<SourceCheck, AssetError> {
    let root = open_dir(Path::new(&batch.input_root))?;
    let mut check = SourceCheck {
        checked: 0,
        unchanged: 0,
        changed: Vec::new(),
        unreadable: Vec::new(),
    };
    for entry in &inventory.entries {
        let Some(sha) = &entry.sha256 else { continue };
        check.checked += 1;
        match open_relative(&root, &entry.path).and_then(read_limited) {
            Ok(bytes) if &sha256_hex(&bytes) == sha => check.unchanged += 1,
            Ok(_) => check.changed.push(entry.path.clone()),
            Err(_) => check.unreadable.push(entry.path.clone()),
        }
    }
    Ok(check)
}

pub fn open_input_root(batch: &NormalizerBatch) -> Result<File, AssetError> {
    open_dir(Path::new(&batch.input_root))
}
