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
use super::normalizer::{self, Mode as RunMode, NormalizationResult, NormalizerProfile, Toolchain};
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_ID: &str = "normalizer-cache";
pub const DEFAULT_MAX_FILES: usize = 20_000;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_JOBS: usize = 32;
const MAX_DEPTH: usize = 16;
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
    /// SHA-256 файла, пригодного к обработке; у записи с ошибкой — `None`.
    pub sha256: Option<String>,
    pub error: Option<String>,
    /// SHA-256 всех байтов файла, который прочитан, но к обработке не допущен
    /// (больше 8 MiB). Нужен только проверке неизменности входа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
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
    /// Входы, которые этот пакет обработал сам, а не взял готовыми из кэша,
    /// за все свои запуски (по последней контрольной точке).
    #[serde(default)]
    pub processed: u64,
    pub by_outcome: BTreeMap<String, u64>,
    /// Сбои среды по SHA входа: вход остаётся незавершённым и повторяется
    /// следующим запуском; в исход входа и кэш сбой не превращается.
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

/// Каталог относительно корня; каждый компонент — без перехода по ссылкам.
fn open_relative_dir(root: &File, relative: &str) -> Result<File, AssetError> {
    let mut current = root
        .try_clone()
        .map_err(|e| AssetError::io("дескриптор", e))?;
    for part in relative.split('/').filter(|p| !p.is_empty()) {
        current = File::from(
            openat(
                &current,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| AssetError::io("открытие каталога входа", e.into()))?,
        );
    }
    Ok(current)
}

/// Чтение одного MP3 по уже открытому дескриптору: длина по `fstat`,
/// байты — не больше предела; у слишком большого файла хеш считается
/// потоково, чтобы проверка неизменности могла его сверить.
fn read_entry(relative: String, file: File) -> (InventoryEntry, u64) {
    let length = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => {
            return (
                InventoryEntry {
                    path: relative,
                    byte_length: 0,
                    sha256: None,
                    error: Some("вход не является обычным файлом".into()),
                    content_sha256: None,
                },
                0,
            );
        }
        Err(error) => {
            return (
                InventoryEntry {
                    path: relative,
                    byte_length: 0,
                    sha256: None,
                    error: Some(AssetError::io("метаданные входа", error).message),
                    content_sha256: None,
                },
                0,
            );
        }
    };
    let mut file = file;
    let mut head = Vec::new();
    let read = (&mut file)
        .take(MAX_AUDIO_BYTES + 1)
        .read_to_end(&mut head)
        .map_err(|e| AssetError::io("чтение входа", e));
    match read {
        Err(error) => (
            InventoryEntry {
                path: relative,
                byte_length: length,
                sha256: None,
                error: Some(error.message),
                content_sha256: None,
            },
            length,
        ),
        Ok(_) if head.len() as u64 <= MAX_AUDIO_BYTES => {
            // Длина — по фактически прочитанным байтам, а не по метаданным.
            let actual = head.len() as u64;
            (
                InventoryEntry {
                    path: relative,
                    byte_length: actual,
                    sha256: Some(sha256_hex(&head)),
                    error: None,
                    content_sha256: None,
                },
                actual.max(length),
            )
        }
        Ok(_) => {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(&head);
            let mut total = head.len() as u64;
            let mut buffer = vec![0u8; 1024 * 1024];
            let rest = loop {
                match file.read(&mut buffer) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        hasher.update(&buffer[..n]);
                        total += n as u64;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            };
            let content = rest
                .is_ok()
                .then(|| crate::hashing::encode_lower_hex(hasher.finalize()));
            (
                InventoryEntry {
                    path: relative,
                    byte_length: total,
                    sha256: None,
                    error: Some("файл превышает предел 8 MiB".into()),
                    content_sha256: content,
                },
                total.max(length),
            )
        }
    }
}

/// Рекурсивный обход без перехода по ссылкам. Каждый каталог и файл
/// открывается относительно дескриптора корня с `NOFOLLOW` на каждом
/// компоненте, а тип берётся у открытого дескриптора: подмена записи
/// ссылкой между перечислением и открытием не выводит обход за пределы входа.
/// Сортировка по байтам имени делает инвентаризацию воспроизводимой.
///
/// `--max-total-bytes` ограничивает суммарный размер **всех** MP3-записей,
/// включая не прошедшие предел 8 MiB и непрочитанные: учитывается наибольшее
/// из длины по `fstat` и фактически прочитанного.
pub fn inventory(root: &Path, limits: &Limits) -> Result<Inventory, AssetError> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    let root_fd = open_dir(root)?;
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![(String::new(), 0usize)];
    while let Some((prefix, depth)) = stack.pop() {
        let directory = open_relative_dir(&root_fd, &prefix)?;
        let mut names = Vec::new();
        for entry in Dir::read_from(&directory)
            .map_err(|e| AssetError::io("обход каталога входа", e.into()))?
        {
            let entry = entry.map_err(|e| AssetError::io("обход каталога входа", e.into()))?;
            let raw = entry.file_name().to_bytes().to_vec();
            if raw == b"." || raw == b".." {
                continue;
            }
            names.push((raw, entry.file_type()));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let mut subdirectories = Vec::new();
        for (raw, listed) in names {
            let Ok(name) = String::from_utf8(raw.clone()) else {
                skipped.push(SkippedEntry {
                    path: format!("{prefix}{}", String::from_utf8_lossy(&raw)),
                    reason: "non_utf8_name".into(),
                });
                continue;
            };
            let relative = format!("{prefix}{name}");
            let kind = if listed == FileType::Unknown {
                statat(&directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                    .map(|stat| FileType::from_raw_mode(stat.st_mode))
                    .map_err(|e| AssetError::io("тип записи каталога", e.into()))?
            } else {
                listed
            };
            let skip = |reason: &str| SkippedEntry {
                path: relative.clone(),
                reason: reason.into(),
            };
            if kind == FileType::Symlink {
                skipped.push(skip("symlink"));
            } else if kind == FileType::Directory {
                if depth + 1 >= MAX_DEPTH {
                    skipped.push(skip("depth_limit"));
                } else {
                    subdirectories.push((format!("{relative}/"), depth + 1));
                }
            } else if kind != FileType::RegularFile {
                skipped.push(skip("not_regular"));
            } else if name.contains(['\\', '\0']) || name.chars().any(char::is_control) {
                skipped.push(skip("unsafe_name"));
            } else if !is_mp3_name(&name) {
                skipped.push(skip("unsupported_extension"));
            } else {
                if entries.len() >= limits.max_files {
                    return Err(invalid(format!(
                        "каталог содержит больше {} MP3: увеличьте --max-files",
                        limits.max_files
                    )));
                }
                let opened = openat(
                    &directory,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                );
                let (entry, counted) = match opened {
                    Ok(descriptor) => {
                        let file = File::from(descriptor);
                        // Заведомое превышение по `fstat` отклоняется до чтения:
                        // иначе большой файл читался и хешировался бы целиком.
                        // При ошибке `fstat` `read_entry` вернёт ошибку записи,
                        // не читая байтов; итоговая проверка ниже учитывает рост
                        // файла во время чтения.
                        if let Ok(metadata) = file.metadata()
                            && total.saturating_add(metadata.len()) > limits.max_total_bytes
                        {
                            return Err(invalid(
                                "суммарный размер входа превышает --max-total-bytes",
                            ));
                        }
                        read_entry(relative, file)
                    }
                    // Запись стала ссылкой после перечисления: не следуем ей.
                    Err(rustix::io::Errno::LOOP) => {
                        skipped.push(skip("symlink"));
                        continue;
                    }
                    Err(error) => (
                        InventoryEntry {
                            path: relative,
                            byte_length: 0,
                            sha256: None,
                            error: Some(
                                AssetError::io("открытие файла входа", error.into()).message,
                            ),
                            content_sha256: None,
                        },
                        0,
                    ),
                };
                total = total.saturating_add(counted);
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

fn boundary(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::BoundaryViolation, message)
}

/// Фактический путь, в том числе ещё не существующий: наибольший
/// существующий предок канонизируется (символические ссылки разрешаются),
/// остальные компоненты присоединяются как есть. `.` и `..` в
/// несуществующей части отвергаются. Ничего не создаёт.
pub fn resolve_path(path: &Path) -> Result<PathBuf, AssetError> {
    use std::path::Component;
    let absolute = std::path::absolute(path).map_err(|e| AssetError::io("путь", e))?;
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| boundary("путь не имеет существующего предка"))?;
                rest.push(name.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| boundary("путь не имеет существующего предка"))?;
            }
            Err(error) => return Err(AssetError::io("путь", error)),
        }
    }
    if absolute.strip_prefix(existing).is_ok_and(|tail| {
        tail.components()
            .any(|c| !matches!(c, Component::Normal(_)))
    }) {
        return Err(boundary(
            "несуществующая часть пути не может содержать . и ..",
        ));
    }
    let mut resolved = std::fs::canonicalize(existing).map_err(|e| AssetError::io("путь", e))?;
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Защищённые корни в лексической и фактической форме.
pub fn protected_roots(protected: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in protected {
        roots.push(root.clone());
        if let Ok(resolved) = resolve_path(root) {
            roots.push(resolved);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Запрещает путь вывода внутри (или поверх) любого из `forbidden`. Путь
/// сравнивается в фактической форме ещё до первой записи.
pub fn ensure_outside(
    path: &Path,
    forbidden: &[PathBuf],
    message: &str,
) -> Result<PathBuf, AssetError> {
    let resolved = resolve_path(path)?;
    if forbidden.iter().any(|root| overlaps(&resolved, root)) {
        return Err(boundary(message));
    }
    Ok(resolved)
}

/// Пути входа и рабочего каталога (уже в фактической форме) не должны
/// пересекаться друг с другом и с защищёнными корнями (`decks/**`).
pub fn check_boundaries(
    input_root: &Path,
    workspace: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<(), AssetError> {
    let input = resolve_path(input_root)?;
    let work = resolve_path(workspace)?;
    if overlaps(&input, &work) {
        return Err(boundary(
            "каталог входа и рабочий каталог должны быть раздельными и невложенными",
        ));
    }
    let roots = protected_roots(protected);
    if roots
        .iter()
        .any(|root| overlaps(&input, root) || overlaps(&work, root))
    {
        return Err(boundary(
            "нормализатор не читает и не пишет в защищённые каталоги (decks/**)",
        ));
    }
    Ok(())
}

/// Создаёт рабочий каталог только после проверки его фактического пути.
/// Недостающие компоненты создаются относительно дескриптора проверенного
/// предка без перехода по ссылкам; итог сверяется с проверенным путём.
pub fn create_workspace(
    path: &Path,
    input_root: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<PathBuf, AssetError> {
    let resolved = resolve_path(path)?;
    check_boundaries(input_root, &resolved, protected)?;
    let mut ancestor = resolved.as_path();
    let mut missing = Vec::new();
    while std::fs::symlink_metadata(ancestor).is_err() {
        missing.push(
            ancestor
                .file_name()
                .ok_or_else(|| boundary("путь не имеет существующего предка"))?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| boundary("путь не имеет существующего предка"))?;
    }
    let mut current = open_dir(ancestor).map_err(|_| {
        boundary("предок рабочего каталога должен быть каталогом без символической ссылки")
    })?;
    for name in missing.into_iter().rev() {
        match rustix::fs::mkdirat(&current, name.as_os_str(), Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(AssetError::io("рабочий каталог", error.into())),
        }
        current = File::from(
            openat(
                &current,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| boundary("компонент рабочего каталога подменён ссылкой"))?,
        );
    }
    existing_workspace(&resolved, protected)
}

/// Открытие существующего рабочего каталога для любых подкоманд: фактический
/// путь — каталог вне защищённых корней. Связь со входом проверяет
/// [`check_boundaries`] после чтения состояния пакета.
pub fn existing_workspace(
    path: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<PathBuf, AssetError> {
    let resolved = resolve_path(path)?;
    let metadata =
        std::fs::symlink_metadata(&resolved).map_err(|e| AssetError::io("рабочий каталог", e))?;
    if !metadata.is_dir() {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "ожидается каталог",
        ));
    }
    if protected_roots(protected)
        .iter()
        .any(|root| overlaps(&resolved, root))
    {
        return Err(boundary("рабочий каталог не может пересекаться с decks/**"));
    }
    Ok(resolved)
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

/// Состояние пакета с ID кэша жило бы в каталоге кэша под той же блокировкой:
/// первая же контрольная точка ждала бы блокировку, которую держит сам кэш.
fn reject_reserved_batch_id(batch_id: &str) -> Result<(), AssetError> {
    if batch_id == CACHE_ID {
        return Err(invalid(format!(
            "batch_id {CACHE_ID} зарезервирован для кэша результатов"
        )));
    }
    Ok(())
}

fn open_batch_runtime(workspace: &Path, batch_id: &str) -> Result<SafeBatchRuntime, AssetError> {
    reject_reserved_batch_id(batch_id)?;
    SafeBatchRuntime::open(workspace, batch_id)
}

/// Фиксирует инвентаризацию и конфигурацию; обработку выполняет [`resume`].
pub fn start(request: StartRequest<'_>) -> Result<NormalizerBatch, AssetError> {
    reject_reserved_batch_id(request.batch_id)?;
    request.profile.validate()?;
    if request.limits.jobs == 0 || request.limits.jobs > MAX_JOBS {
        return Err(invalid(format!("--jobs должен быть 1–{MAX_JOBS}")));
    }
    let toolchain = Toolchain::detect()?;
    let inventory = inventory(request.input_root, &request.limits)?;
    let inventory_bytes = serde_json::to_vec(&inventory).expect("инвентаризация сериализуема");
    let mut runtime = open_batch_runtime(request.workspace, request.batch_id)?;
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
    let mut runtime = open_batch_runtime(workspace, batch_id)?;
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
    let mut runtime = open_batch_runtime(workspace, batch_id)?;
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

enum CacheLookup {
    Hit(Box<NormalizationResult>),
    Missing,
    /// Запись есть, но непригодна (повреждена, потерян вариант).
    Stale(String),
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

    /// Готовый результат входа или `None`. Результат годен, только если JSON
    /// соответствует ключу, а указанный им обработанный вариант существует и
    /// совпадает по SHA-256; иначе вход считается необработанным.
    pub fn get(
        &self,
        input_sha: &str,
        config_sha: &str,
    ) -> Result<Option<NormalizationResult>, AssetError> {
        Ok(match self.lookup(input_sha, config_sha)? {
            CacheLookup::Hit(result) => Some(*result),
            CacheLookup::Missing => None,
            CacheLookup::Stale(why) => {
                tracing::warn!(
                    operation = "normalizer_cache",
                    code = "cache_entry_stale",
                    input_sha256 = input_sha,
                    reason = %why,
                    "Результат кэша непригоден и будет пересчитан"
                );
                None
            }
        })
    }

    fn lookup(&self, input_sha: &str, config_sha: &str) -> Result<CacheLookup, AssetError> {
        let bytes = match self
            .runtime
            .artifact_directory("results")?
            .read(&result_name(input_sha, config_sha))
        {
            Ok(bytes) => bytes,
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                return Ok(CacheLookup::Missing);
            }
            Err(error) => return Err(error),
        };
        let Ok(result) = serde_json::from_slice::<NormalizationResult>(&bytes) else {
            return Ok(CacheLookup::Stale("результат кэша не разбирается".into()));
        };
        if result.input.sha256 != input_sha || result.config_sha256 != config_sha {
            return Ok(CacheLookup::Stale(
                "результат кэша не соответствует своему ключу".into(),
            ));
        }
        if let Some(output) = &result.output {
            match self.variant_bytes(output) {
                Ok(bytes) if bytes.len() as u64 == output.byte_length => {}
                Ok(_) => return Ok(CacheLookup::Stale("длина варианта не совпала".into())),
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
                    return Ok(CacheLookup::Stale(format!(
                        "обработанный вариант недоступен: {}",
                        error.message
                    )));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(CacheLookup::Hit(Box::new(result)))
    }

    /// Сохраняет результат и вариант. Повреждённый прежний результат того же
    /// ключа и повреждённые байты варианта заменяются атомарно: ключ
    /// детерминирован, а исключительная блокировка кэша не допускает
    /// параллельного писателя.
    fn put(
        &self,
        mut result: NormalizationResult,
        output: Option<&[u8]>,
    ) -> Result<NormalizationResult, AssetError> {
        if let (Some(bytes), Some(reference)) = (output, result.output.as_mut()) {
            let existing = RuntimeBlobRef {
                sha256: sha256_hex(bytes),
                storage_path: format!("candidates/{}.mp3", sha256_hex(bytes)),
            };
            let blob = match self
                .runtime
                .persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)
            {
                // По пути лежат другие байты либо обычный файл, раздутый
                // сверх предела: оба — повреждённый прежний вариант. Прочие
                // нарушения границы (ссылка, не файл) остаются ошибкой.
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
                    "SHA сохранённого варианта не совпал",
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
    /// Входы, обработка которых в этом запуске прервана сбоем среды; они
    /// остаются незавершёнными и повторяются при следующем запуске.
    pub infrastructure_failed_now: u64,
    pub batch: NormalizerBatch,
}

enum ItemError {
    /// Байты входа не совпали с инвентаризацией или вход не читается.
    Source(String),
    /// Сбой среды: восстановимо, результат не записывается.
    Infrastructure(AssetError),
    /// Нарушение инварианта самого нормализатора: пакет останавливается.
    Fatal(AssetError),
}

/// Обрабатывает недостающие результаты с ограниченной конкуренцией.
/// `stop` выставляется обработчиком SIGINT/SIGTERM вызывающего процесса.
///
/// Готовый результат переиспользуется, только если вход по-прежнему совпадает
/// с инвентаризацией и результат годен (см. [`ResultCache::get`]). Сбой среды
/// не превращается в исход входа: вход остаётся незавершённым, число сбоев
/// копится в `progress.infrastructure_failures`, а следующий запуск повторяет
/// обработку.
pub fn resume(
    workspace: &Path,
    batch_id: &str,
    jobs: Option<usize>,
    max_items: Option<usize>,
    stop: &AtomicBool,
) -> Result<RunSummary, AssetError> {
    resume_with(workspace, batch_id, jobs, max_items, stop, None)
}

/// [`resume`] сразу после [`start`] в том же процессе: окружение, только что
/// определённое и записанное в пакет при старте, повторно не определяется
/// (каждое определение запускает FFmpeg/ffprobe и пробное кодирование).
/// Сверка с `batch.toolchain` сохраняется.
pub fn resume_started(
    workspace: &Path,
    started: &NormalizerBatch,
    max_items: Option<usize>,
    stop: &AtomicBool,
) -> Result<RunSummary, AssetError> {
    resume_with(
        workspace,
        &started.batch_id,
        None,
        max_items,
        stop,
        Some(started.toolchain.clone()),
    )
}

fn resume_with(
    workspace: &Path,
    batch_id: &str,
    jobs: Option<usize>,
    max_items: Option<usize>,
    stop: &AtomicBool,
    detected: Option<Toolchain>,
) -> Result<RunSummary, AssetError> {
    let mut runtime = open_batch_runtime(workspace, batch_id)?;
    let mut batch: NormalizerBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет нормализации отсутствует"))?;
    if batch.cancelled {
        return Err(invalid("пакет нормализации отменён"));
    }
    let _lease = runtime.execution_lease()?;
    let bytes = runtime.artifact_directory("data")?.read("inventory.json")?;
    if sha256_hex(&bytes) != batch.inventory_sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "инвентаризация пакета изменена",
        ));
    }
    let inventory: Inventory =
        serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
    let toolchain = match detected {
        Some(toolchain) => toolchain,
        None => Toolchain::detect()?,
    };
    if toolchain != batch.toolchain {
        return Err(invalid(
            "версия FFmpeg/ffprobe/кодировщика/анализатора изменилась: результаты несопоставимы, начните новый пакет",
        ));
    }
    let jobs = jobs.unwrap_or(batch.limits.jobs);
    if jobs == 0 || jobs > MAX_JOBS {
        return Err(invalid(format!("--jobs должен быть 1–{MAX_JOBS}")));
    }
    runtime.release_lock()?;
    let cache = ResultCache::open(workspace)?;
    let root = open_dir(Path::new(&batch.input_root))?;
    let mut by_outcome: BTreeMap<String, u64> = BTreeMap::new();
    let mut pending = Vec::new();
    let mut reused = 0u64;
    let processed_before = batch.progress.processed;
    let mut infra = batch.progress.infrastructure_failures.clone();
    let mut source_errors = batch.progress.source_errors.clone();
    let unique = inventory.unique_inputs();
    for (sha, path) in &unique {
        match cache.get(sha, &batch.config_sha256)? {
            Some(result) => match read_input(&root, path, sha) {
                Ok(_) => {
                    reused += 1;
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
    let mut processed_now = 0u64;
    let mut infrastructure_failed_now = 0u64;
    let mut failure: Option<AssetError> = None;
    let mut cancelled = false;
    let mut last_checkpoint = Instant::now();
    let halt = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let mut consecutive_infra = 0usize;
    let checkpoint = |batch: &mut NormalizerBatch,
                      runtime: &mut SafeBatchRuntime,
                      completed: u64,
                      processed: u64,
                      by_outcome: &BTreeMap<String, u64>,
                      infra: &BTreeMap<String, u32>,
                      source_errors: &BTreeMap<String, String>|
     -> Result<bool, AssetError> {
        runtime.reacquire_lock()?;
        let mut stored: NormalizerBatch = runtime
            .reload_cached()?
            .ok_or_else(|| invalid("пакет нормализации исчез"))?;
        stored.progress.completed = completed;
        stored.progress.processed = processed;
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
                        Ok(bytes) => {
                            normalizer::process(&bytes, profile, mode, toolchain).map_err(|error| {
                                if normalizer::is_infrastructure_failure(&error) {
                                    ItemError::Infrastructure(error)
                                } else {
                                    ItemError::Fatal(error)
                                }
                            })
                        }
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
                    infrastructure_failed_now += 1;
                    consecutive_infra += 1;
                    *infra.entry(sha.clone()).or_default() += 1;
                    if consecutive_infra >= MAX_CONSECUTIVE_INFRA_FAILURES {
                        halt.store(true, Ordering::SeqCst);
                        failure.get_or_insert(error);
                    }
                    continue;
                }
                Err(ItemError::Fatal(error)) => {
                    halt.store(true, Ordering::SeqCst);
                    failure.get_or_insert(error);
                    continue;
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
                    continue;
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
        processed_before + processed_now,
        &by_outcome,
        &infra,
        &source_errors,
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

/// Итог повторной проверки входа против зафиксированной инвентаризации.
/// `verified: true` только если инвариант доказан полностью.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceCheck {
    pub verified: bool,
    /// MP3-записей в зафиксированной инвентаризации.
    pub checked: u64,
    pub unchanged: u64,
    /// Тот же путь, но другие байты, длина или ошибка чтения.
    pub changed: Vec<String>,
    /// Был прочитан, теперь не читается.
    pub unreadable: Vec<String>,
    /// Новые MP3, которых нет в инвентаризации.
    pub added: Vec<String>,
    /// MP3 инвентаризации, которых больше нет.
    pub removed: Vec<String>,
    /// В инвентаризации нет хеша содержимого (файл не читался): неизменность
    /// не доказуема, это не успех.
    pub unverifiable: Vec<String>,
    /// Пропущенные при обходе записи (ссылки, другие расширения, небезопасные
    /// имена) появились, исчезли или сменили причину пропуска.
    pub skipped_changed: Vec<String>,
}

/// Повторная полная инвентаризация того же корня тем же безопасным обходом и
/// сверка с зафиксированным снимком: набор путей MP3, длины, SHA-256 и ошибки
/// чтения, а также список пропущенных записей с причинами. Содержимое
/// посторонних (не MP3) файлов не хешируется: их изменение вне инварианта.
pub fn verify_sources(
    batch: &NormalizerBatch,
    inventory: &Inventory,
) -> Result<SourceCheck, AssetError> {
    let current =
        self::inventory(Path::new(&batch.input_root), &batch.limits).map_err(|error| {
            AssetError::with_details(
                ErrorCode::IntegrityMismatch,
                format!(
                    "повторная инвентаризация входа не выполнена: {}",
                    error.message
                ),
                serde_json::json!({"code": error.code.as_str()}),
            )
        })?;
    let now: BTreeMap<&str, &InventoryEntry> = current
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let mut check = SourceCheck {
        checked: inventory.entries.len() as u64,
        ..SourceCheck::default()
    };
    for entry in &inventory.entries {
        let content = entry.sha256.as_ref().or(entry.content_sha256.as_ref());
        let Some(after) = now.get(entry.path.as_str()) else {
            check.removed.push(entry.path.clone());
            continue;
        };
        let after_content = after.sha256.as_ref().or(after.content_sha256.as_ref());
        match (content, after_content) {
            (None, _) => check.unverifiable.push(entry.path.clone()),
            (Some(_), None) => check.unreadable.push(entry.path.clone()),
            (Some(a), Some(b))
                if a == b
                    && entry.byte_length == after.byte_length
                    && entry.sha256 == after.sha256 =>
            {
                check.unchanged += 1
            }
            _ => check.changed.push(entry.path.clone()),
        }
    }
    let known: BTreeSet<&str> = inventory.entries.iter().map(|e| e.path.as_str()).collect();
    check.added = current
        .entries
        .iter()
        .filter(|e| !known.contains(e.path.as_str()))
        .map(|e| e.path.clone())
        .collect();
    let before: BTreeSet<(&str, &str)> = inventory
        .skipped
        .iter()
        .map(|e| (e.path.as_str(), e.reason.as_str()))
        .collect();
    let after: BTreeSet<(&str, &str)> = current
        .skipped
        .iter()
        .map(|e| (e.path.as_str(), e.reason.as_str()))
        .collect();
    check.skipped_changed = before
        .symmetric_difference(&after)
        .map(|(path, _)| (*path).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    check.verified = check.changed.is_empty()
        && check.unreadable.is_empty()
        && check.added.is_empty()
        && check.removed.is_empty()
        && check.unverifiable.is_empty()
        && check.skipped_changed.is_empty()
        && check.unchanged == check.checked;
    Ok(check)
}

pub fn open_input_root(batch: &NormalizerBatch) -> Result<File, AssetError> {
    open_dir(Path::new(&batch.input_root))
}
