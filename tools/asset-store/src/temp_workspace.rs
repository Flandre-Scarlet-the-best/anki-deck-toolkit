//! Владелец временного дерева и консервативная очистка собственных осиротевших запусков.
//!
//! Все операции обхода и удаления привязаны к дескрипторам каталогов. Маркер
//! подтверждает назначение, а UID, пространство имён, NOFOLLOW и идентичность процесса ограничивают очистку.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, RenameFlags, StatxFlags, mkdirat, open, openat, readlinkat,
    renameat_with, statat, statx, unlinkat,
};
use serde::{Deserialize, Serialize};

const TEMP_ROOT: &str = "/tmp";
pub(crate) const CONFIGURED_TEMP_ROOT_ENV: &str = "ASSET_STORE_TEMP_ROOT";
const NAMESPACE_PREFIX: &str = "anki-decks";
const MARKER: &str = ".anki-decks-owner.json";
const REPOSITORY: &str = "anki-decks";
const TOOL: &str = "asset-store";
const SCHEMA: u32 = 2;
const LEGACY_SCHEMA: u32 = 1;
const MARKER_LIMIT: u64 = 16 * 1024;
type CachedGcResult = Result<(), (io::ErrorKind, String)>;
static STARTUP_GC: OnceLock<Result<(), (io::ErrorKind, String)>> = OnceLock::new();
static CONFIGURED_ROOT_GC: OnceLock<Mutex<HashMap<PathBuf, CachedGcResult>>> = OnceLock::new();
pub const DEFAULT_ORPHAN_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipMarker {
    schema: u32,
    repository: String,
    tool: String,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_ticks: Option<u64>,
    created_unix_ms: u64,
    run_id: String,
    purpose: String,
}

/// Объект обязан жить дольше всех пользователей `path()`.
#[derive(Debug)]
pub struct TempWorkspace {
    path: PathBuf,
    run_id: String,
    parent: File,
    directory: File,
    closed: bool,
}

/// Ошибка создания, при которой безопасную очистку уже созданного дерева подтвердить не удалось.
#[derive(Debug, thiserror::Error)]
#[error("{original}; temp_workspace_cleanup_failed: {cleanup}")]
pub(crate) struct WorkspaceCreationCleanupFailure {
    pub(crate) original: io::Error,
    pub(crate) cleanup: io::Error,
}

fn creation_cleanup_failure(original: io::Error, cleanup: io::Error) -> io::Error {
    io::Error::new(
        original.kind(),
        WorkspaceCreationCleanupFailure { original, cleanup },
    )
}

/// Один раз за время жизни процесса очищает осиротевшие запуски перед работой с временными данными.
/// Вызывающий код может выполнить очистку без создания нового временного дерева.
pub(crate) fn cleanup_orphans_on_startup() -> io::Result<()> {
    let root = selected_temp_root()?;
    if root != Path::new(TEMP_ROOT) {
        return cleanup_configured_root_once(&root);
    }
    STARTUP_GC
        .get_or_init(|| {
            cleanup_orphans(DEFAULT_ORPHAN_MIN_AGE)
                .and_then(|report| {
                    tracing::debug!(
                        removed = report.removed,
                        skipped = report.skipped,
                        errors = report.errors,
                        "начальная очистка временных деревьев"
                    );
                    startup_gc_result(&report)
                })
                .map_err(|error| (error.kind(), error.to_string()))
        })
        .as_ref()
        .map(|_| ())
        .map_err(|(kind, message)| io::Error::new(*kind, message.clone()))
}

/// Корень неявных операций: ASSET_STORE_TEMP_ROOT либо /tmp.
pub fn selected_temp_root() -> io::Result<PathBuf> {
    let root = std::env::var_os(CONFIGURED_TEMP_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(TEMP_ROOT));
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{CONFIGURED_TEMP_ROOT_ENV} должен быть абсолютным путём"),
        ));
    }
    Ok(root)
}

impl TempWorkspace {
    /// Создаёт закрытое дерево после начальной очистки; TTL действует только для схемы 1.
    pub fn create(purpose: &str) -> io::Result<Self> {
        let root = selected_temp_root()?;
        cleanup_orphans_on_startup()?;
        Self::create_under(&root, purpose)
    }

    /// Создаёт такое же закрытое дерево, принадлежащее проекту, под выбранным временным корнем.
    /// Корень `temp_root` должен существовать и не быть symlink; API поддерживает изолированные
    /// песочницы и не ослабляет проверки маркера, UID, mount и процессов.
    pub fn create_under(temp_root: &Path, purpose: &str) -> io::Result<Self> {
        if purpose.is_empty() || purpose.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "назначение временного каталога должно содержать от 1 до 4096 байт",
            ));
        }
        let pid = std::process::id();
        let boot_id = read_boot_id(Path::new("/proc/sys/kernel/random/boot_id"))?;
        let process_start_ticks = read_process_start_ticks(Path::new("/proc"), pid)?;
        let uid = current_uid()?;
        let namespace_name = namespace_directory_name(uid);
        let parent = namespace(temp_root, uid, true)?.ok_or_else(|| {
            io::Error::other("не удалось создать пространство имён временных каталогов")
        })?;
        let mut random = [0_u8; 16];
        for _ in 0..32 {
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let run_id = format!("run-{}", crate::hashing::encode_lower_hex(random));
            match mkdirat(&parent, run_id.as_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    let directory = directory_at(&parent, OsStr::new(&run_id)).map_err(|error| {
                        creation_cleanup_failure(error, io::Error::other(
                            "не удалось подтвердить владение уже созданным временным каталогом",
                        ))
                    })?;
                    let owner = Self {
                        path: temp_root.join(&namespace_name).join(&run_id),
                        run_id: run_id.clone(),
                        parent,
                        directory,
                        closed: false,
                    };
                    let initialized = (|| {
                        let marker = OwnershipMarker {
                            schema: SCHEMA,
                            repository: REPOSITORY.into(),
                            tool: TOOL.into(),
                            pid,
                            boot_id: Some(boot_id),
                            process_start_ticks: Some(process_start_ticks),
                            created_unix_ms: unix_ms()?,
                            run_id,
                            purpose: purpose.into(),
                        };
                        let fd = openat(
                            &owner.directory,
                            ".anki-decks-owner.tmp",
                            OFlags::WRONLY
                                | OFlags::CREATE
                                | OFlags::EXCL
                                | OFlags::NOFOLLOW
                                | OFlags::CLOEXEC,
                            Mode::from_raw_mode(0o600),
                        )?;
                        let mut file = File::from(fd);
                        serde_json::to_writer(&mut file, &marker).map_err(io::Error::other)?;
                        file.write_all(b"\n")?;
                        file.sync_all()?;
                        renameat_with(
                            &owner.directory,
                            ".anki-decks-owner.tmp",
                            &owner.directory,
                            MARKER,
                            RenameFlags::NOREPLACE,
                        )?;
                        owner.directory.sync_all()?;
                        Ok(())
                    })();
                    return owner.finish_creation(initialized);
                }
                Err(error) if error == rustix::io::Errno::EXIST => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "исчерпаны попытки создания временного каталога запуска",
        ))
    }

    fn finish_creation(self, initialized: io::Result<()>) -> io::Result<Self> {
        match initialized {
            Ok(()) => Ok(self),
            Err(original) => match self.close() {
                Ok(()) => Err(original),
                Err(cleanup) => Err(creation_cleanup_failure(original, cleanup)),
            },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn ensure_no_live_process_references(run_path: &Path) -> io::Result<()> {
        if Self::has_live_process_references(run_path)? {
            return Err(io::Error::other(
                "живой процесс всё ещё использует временное дерево",
            ));
        }
        Ok(())
    }

    pub(crate) fn has_live_process_references(run_path: &Path) -> io::Result<bool> {
        let uid = current_uid()?;
        workspace_has_process_references(Path::new("/proc"), run_path, uid)
    }

    /// Ошибка очистки возвращается вызывающему коду; `Drop` повторяет попытку при возможности.
    pub fn close(mut self) -> io::Result<()> {
        self.remove()?;
        self.closed = true;
        Ok(())
    }

    fn remove(&self) -> io::Result<()> {
        self.remove_with_proc(Path::new("/proc"))
    }

    fn remove_with_proc(&self, proc_root: &Path) -> io::Result<()> {
        verify_identity(&self.parent, OsStr::new(&self.run_id), &self.directory)?;
        if has_browser_artifacts(&self.directory)?
            && workspace_has_process_references(proc_root, &self.path, current_uid()?)?
        {
            return Err(io::Error::other(
                "живой процесс всё ещё использует временное дерево",
            ));
        }
        remove_contents(&self.directory)?;
        verify_identity(&self.parent, OsStr::new(&self.run_id), &self.directory)?;
        unlinkat(&self.parent, self.run_id.as_str(), AtFlags::REMOVEDIR).map_err(Into::into)
    }
}

fn cleanup_configured_root_once(root: &Path) -> io::Result<()> {
    cleanup_configured_root_once_with(root, || {
        let report = cleanup_orphans_under(root, DEFAULT_ORPHAN_MIN_AGE)?;
        tracing::debug!(
            removed = report.removed,
            skipped = report.skipped,
            errors = report.errors,
            "начальная очистка временных деревьев"
        );
        startup_gc_result(&report)
    })
}

fn startup_gc_result(report: &GcReport) -> io::Result<()> {
    if report.errors != 0 {
        return Err(io::Error::other(format!(
            "начальная очистка временного корня {} завершилась с {} ошибками",
            report.root.display(),
            report.errors
        )));
    }
    Ok(())
}

fn cleanup_configured_root_once_with(
    root: &Path,
    cleanup: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut results = CONFIGURED_ROOT_GC
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| io::Error::other("кэш начальной очистки временных корней повреждён"))?;
    if let Some(result) = results.get(root) {
        return result
            .as_ref()
            .map(|_| ())
            .map_err(|(kind, message)| io::Error::new(*kind, message.clone()));
    }

    let result = cleanup();
    let cached = result
        .as_ref()
        .map(|_| ())
        .map_err(|error| (error.kind(), error.to_string()));
    results.insert(root.to_path_buf(), cached);
    result
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if !self.closed
            && let Err(error) = self.remove()
        {
            tracing::error!(
                stage = "temp_cleanup",
                code = "temp_workspace_cleanup_failed",
                path_category = "run_workspace",
                message = %crate::diagnostics::safe_message(&error.to_string()),
                "Не удалось удалить временное дерево"
            );
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub root: PathBuf,
    pub removed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub removed_bytes: u64,
    pub entries: Vec<GcEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcEntry {
    pub path: PathBuf,
    pub outcome: String,
    pub reason: String,
    pub bytes: u64,
    pub ownership: String,
    pub live: Option<bool>,
}

/// Удаляет подтверждённые маркером осиротевшие запуски с заведомо завершившимися владельцами.
/// Для схемы 1 дополнительно требуется TTL; схема 2 сверяет точную идентичность процесса.
/// Ошибки отдельных деревьев отражаются в `errors` и `entries`, после чего GC продолжается.
pub fn cleanup_orphans(min_age: Duration) -> io::Result<GcReport> {
    cleanup_orphans_under(&selected_temp_root()?, min_age)
}

/// Строит план для рабочей области, принадлежность которой подтверждена маркером, не меняя файловую систему.
/// Каждый кандидат повторно проверяется при последующем `cleanup_orphans`.
pub fn plan_orphans(min_age: Duration) -> io::Result<GcReport> {
    plan_orphans_under(&selected_temp_root()?, min_age)
}

/// Та же очистка по маркеру и идентичности процесса для явно выбранного временного корня.
/// Публичный параметр `root` нужен для песочниц и не ослабляет проверки владения.
pub fn cleanup_orphans_under(root: &Path, min_age: Duration) -> io::Result<GcReport> {
    cleanup_under_mode(root, min_age, true)
}

/// Строит план для явно выбранного временного корня без изменения файловой системы.
pub fn plan_orphans_under(root: &Path, min_age: Duration) -> io::Result<GcReport> {
    cleanup_under_mode(root, min_age, false)
}

#[cfg(test)]
fn cleanup_under(temp_root: &Path, min_age: Duration) -> io::Result<GcReport> {
    cleanup_under_mode(temp_root, min_age, true)
}

fn cleanup_under_mode(temp_root: &Path, min_age: Duration, apply: bool) -> io::Result<GcReport> {
    cleanup_under_with_proc_mode(temp_root, Path::new("/proc"), min_age, apply)
}

#[cfg(test)]
fn cleanup_under_with_proc(
    temp_root: &Path,
    proc_root: &Path,
    min_age: Duration,
) -> io::Result<GcReport> {
    cleanup_under_with_proc_mode(temp_root, proc_root, min_age, true)
}

fn cleanup_under_with_proc_mode(
    temp_root: &Path,
    proc_root: &Path,
    min_age: Duration,
    apply: bool,
) -> io::Result<GcReport> {
    cleanup_under_with_sources(
        temp_root,
        proc_root,
        &proc_root.join("sys/kernel/random/boot_id"),
        min_age,
        apply,
    )
}

fn cleanup_under_with_sources(
    temp_root: &Path,
    proc_root: &Path,
    boot_id_path: &Path,
    min_age: Duration,
    apply: bool,
) -> io::Result<GcReport> {
    let min_age = min_age.max(DEFAULT_ORPHAN_MIN_AGE);
    let mut report = GcReport {
        root: temp_root.to_path_buf(),
        ..GcReport::default()
    };
    let uid = current_uid()?;
    let Some(parent) = namespace(temp_root, uid, false)? else {
        return Ok(report);
    };
    let namespace_name = namespace_directory_name(uid);
    let now = unix_ms()?;
    for name in names(&parent)? {
        let path = temp_root.join(&namespace_name).join(&name);
        let result = inspect_orphan(
            &parent,
            &ProcessSources {
                proc_root,
                boot_id_path,
            },
            &name,
            &path,
            uid,
            now,
            min_age,
        );
        let (outcome, reason, bytes, ownership, live) = match result {
            Ok(Inspection::Skip(reason, ownership, live)) => {
                report.skipped += 1;
                ("deferred", reason, 0, ownership, live)
            }
            Ok(Inspection::Delete(directory, bytes, reference_policy)) => {
                if !apply {
                    (
                        "delete",
                        "маркер подтверждает, что рабочая область проекта устарела".into(),
                        bytes,
                        "project-owned",
                        Some(false),
                    )
                } else {
                    match remove_inspected_orphan(
                        &parent,
                        proc_root,
                        &name,
                        &path,
                        &directory,
                        uid,
                        reference_policy,
                    ) {
                        Ok(()) => {
                            report.removed += 1;
                            report.removed_bytes = report.removed_bytes.saturating_add(bytes);
                            (
                                "removed",
                                "осиротевший каталог принадлежал завершившемуся владельцу".into(),
                                bytes,
                                "project-owned",
                                Some(false),
                            )
                        }
                        Err(DeleteFailure::Skip { reason, live }) => {
                            report.skipped += 1;
                            ("deferred", reason.into(), 0, "project-owned", live)
                        }
                        Err(DeleteFailure::Io(error))
                            if error.kind() == io::ErrorKind::NotFound =>
                        {
                            report.skipped += 1;
                            (
                                "deferred",
                                "дерево уже удалено параллельной уборкой".into(),
                                0,
                                "project-owned",
                                None,
                            )
                        }
                        Err(DeleteFailure::Io(error)) => {
                            report.errors += 1;
                            ("error", error.to_string(), 0, "project-owned", None)
                        }
                    }
                }
            }
            Err(error) => {
                report.skipped += 1;
                ("deferred", error.to_string(), 0, "unknown", None)
            }
        };
        tracing::debug!(path = %path.display(), outcome, %reason, bytes, "очистка осиротевших временных каталогов");
        report.entries.push(GcEntry {
            path,
            outcome: outcome.into(),
            reason,
            bytes,
            ownership: ownership.into(),
            live,
        });
    }
    Ok(report)
}

enum Inspection {
    Skip(String, &'static str, Option<bool>),
    Delete(File, u64, ProcessReferencePolicy),
}

fn unknown_workspace(reason: impl Into<String>) -> Inspection {
    Inspection::Skip(reason.into(), "unknown", None)
}

fn owned_workspace(reason: impl Into<String>, live: Option<bool>) -> Inspection {
    Inspection::Skip(reason.into(), "project-owned", live)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessReferencePolicy {
    None,
    Browser,
    AllSameUid,
}

enum DeleteFailure {
    Skip {
        reason: &'static str,
        live: Option<bool>,
    },
    Io(io::Error),
}

fn remove_inspected_orphan(
    parent: &File,
    proc_root: &Path,
    name: &OsStr,
    run_path: &Path,
    directory: &File,
    uid: u32,
    reference_policy: ProcessReferencePolicy,
) -> Result<(), DeleteFailure> {
    verify_identity(parent, name, directory).map_err(DeleteFailure::Io)?;
    if reference_policy != ProcessReferencePolicy::None {
        match workspace_process_references(
            proc_root,
            run_path,
            uid,
            reference_policy == ProcessReferencePolicy::AllSameUid,
            &[directory.as_raw_fd()],
        ) {
            Ok(false) => (),
            Ok(true) => {
                return Err(DeleteFailure::Skip {
                    reason: "временную рабочую область использует живой процесс",
                    live: Some(true),
                });
            }
            Err(_) => {
                return Err(DeleteFailure::Skip {
                    reason: "не удалось подтвердить отсутствие ссылок процессов на каталог",
                    live: None,
                });
            }
        }
    }
    verify_identity(parent, name, directory).map_err(DeleteFailure::Io)?;
    remove_contents(directory).map_err(DeleteFailure::Io)?;
    verify_identity(parent, name, directory).map_err(DeleteFailure::Io)?;
    unlinkat(parent, name, AtFlags::REMOVEDIR)
        .map_err(io::Error::from)
        .map_err(DeleteFailure::Io)
}

fn inspect_orphan(
    parent: &File,
    sources: &ProcessSources<'_>,
    name: &OsStr,
    run_path: &Path,
    uid: u32,
    now: u64,
    min_age: Duration,
) -> io::Result<Inspection> {
    let Some(run_id) = name.to_str().filter(|name| valid_run_id(name)) else {
        return Ok(unknown_workspace("недопустимый префикс имени запуска"));
    };
    let directory = directory_at(parent, name)?;
    let metadata = directory.metadata()?;
    same_mount(parent, &directory)?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Ok(unknown_workspace(
            "каталог принадлежит другому UID или имеет небезопасные права",
        ));
    }
    let fd = openat(
        &directory,
        MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = File::from(fd);
    let marker_metadata = file.metadata()?;
    if !marker_metadata.is_file()
        || marker_metadata.uid() != uid
        || marker_metadata.len() > MARKER_LIMIT
        || marker_metadata.nlink() != 1
        || marker_metadata.mode() & 0o022 != 0
    {
        return Ok(unknown_workspace("небезопасный маркер владения"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    let marker: OwnershipMarker = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if !matches!(marker.schema, LEGACY_SCHEMA | SCHEMA)
        || marker.repository != REPOSITORY
        || marker.tool != TOOL
        || marker.run_id != run_id
        || marker.purpose.is_empty()
        || marker.pid == 0
    {
        return Ok(unknown_workspace(
            "маркер не принадлежит проекту или имеет неподдерживаемую версию",
        ));
    }
    match owner_is_dead(&marker, sources) {
        Ok(true) => (),
        Ok(false) => {
            return Ok(owned_workspace(
                "исходный владелец ещё работает",
                Some(true),
            ));
        }
        Err(_) => {
            return Ok(owned_workspace(
                "не удалось подтвердить, что исходный владелец завершился",
                None,
            ));
        }
    }
    let browser_artifacts = has_browser_artifacts(&directory)?;
    let reference_policy = if marker.schema == SCHEMA {
        ProcessReferencePolicy::AllSameUid
    } else if browser_artifacts {
        ProcessReferencePolicy::Browser
    } else {
        ProcessReferencePolicy::None
    };
    if reference_policy != ProcessReferencePolicy::None {
        match workspace_process_references(
            sources.proc_root,
            run_path,
            uid,
            reference_policy == ProcessReferencePolicy::AllSameUid,
            &[directory.as_raw_fd(), file.as_raw_fd()],
        ) {
            Ok(true) => {
                return Ok(owned_workspace(
                    "временную рабочую область использует живой процесс",
                    Some(true),
                ));
            }
            Ok(false) => (),
            Err(_) => {
                return Ok(owned_workspace(
                    "не удалось подтвердить отсутствие ссылок процессов на каталог",
                    None,
                ));
            }
        }
    }
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    if marker.created_unix_ms > now || modified > u128::from(now) {
        return Ok(owned_workspace(
            "время запуска указано в будущем",
            Some(false),
        ));
    }
    // Для схемы 2 пропускается только проверка TTL; остальные проверки обязательны.
    if marker.schema == LEGACY_SCHEMA {
        let age_ms = u64::try_from(min_age.as_millis()).unwrap_or(u64::MAX);
        if now - marker.created_unix_ms < age_ms
            || modified > u128::from(now.saturating_sub(age_ms))
        {
            return Ok(owned_workspace(
                "запуск по схеме 1 ещё не достиг минимального возраста",
                Some(false),
            ));
        }
    }
    let size = inspect_orphan_tree(&directory, uid, run_path)?;
    verify_identity(parent, name, &directory)?;
    Ok(Inspection::Delete(directory, size, reference_policy))
}

struct ProcessSources<'a> {
    proc_root: &'a Path,
    boot_id_path: &'a Path,
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn read_boot_id(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let value = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let value = value.strip_suffix('\n').unwrap_or(value);
    if !valid_boot_id(value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "невалидный boot_id",
        ));
    }
    Ok(value.into())
}

fn parse_process_start_ticks(bytes: &[u8], expected_pid: u32) -> io::Result<u64> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "невалидный /proc/PID/stat");
    let opening = bytes
        .windows(2)
        .position(|part| part == b" (")
        .ok_or_else(malformed)?;
    let pid = std::str::from_utf8(&bytes[..opening]).map_err(|_| malformed())?;
    if parse_pid(pid) != Some(expected_pid) {
        return Err(malformed());
    }
    // Поле `/proc/.../comm` может содержать произвольные байты, пробелы, ')' и '('.
    // Последняя ')' завершает поле 2; UTF-8 требуется только для числовых полей.
    let closing = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(malformed)?;
    if closing < opening + 2 {
        return Err(malformed());
    }
    let tail = bytes[closing + 1..]
        .strip_prefix(b" ")
        .ok_or_else(malformed)?;
    let tail = std::str::from_utf8(tail).map_err(|_| malformed())?;
    let fields = tail.split_ascii_whitespace().collect::<Vec<_>>();
    // Хвост начинается с поля 3 (`state`), поэтому поле 22 имеет индекс 19.
    if fields.len() < 20
        || fields[0].len() != 1
        || !matches!(
            fields[0].as_bytes()[0],
            b'R' | b'S' | b'D' | b'Z' | b'T' | b't' | b'X' | b'x' | b'K' | b'W' | b'P' | b'I'
        )
        || fields[1..]
            .iter()
            .any(|field| field.parse::<i128>().is_err())
        || !decimal_bytes(fields[19].as_bytes())
    {
        return Err(malformed());
    }
    fields[19].parse().map_err(|_| malformed())
}

fn read_process_start_ticks(proc_root: &Path, pid: u32) -> io::Result<u64> {
    parse_process_start_ticks(
        &fs::read(proc_root.join(pid.to_string()).join("stat"))?,
        pid,
    )
}

fn owner_is_dead(marker: &OwnershipMarker, sources: &ProcessSources<'_>) -> io::Result<bool> {
    match marker.schema {
        LEGACY_SCHEMA => {
            if marker.boot_id.is_some() || marker.process_start_ticks.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "схема 1 содержит идентификатор процесса, предусмотренный для схемы 2",
                ));
            }
            if marker.pid == std::process::id() {
                return Ok(false);
            }
        }
        SCHEMA => {
            let boot_id = marker
                .boot_id
                .as_deref()
                .filter(|id| valid_boot_id(id))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "некорректное поле `boot_id` в маркере",
                    )
                })?;
            let start_ticks = marker.process_start_ticks.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "в маркере отсутствует поле `starttime`",
                )
            })?;
            if boot_id != read_boot_id(sources.boot_id_path)? {
                return Ok(true);
            }
            let process = sources.proc_root.join(marker.pid.to_string());
            match fs::symlink_metadata(&process) {
                Ok(metadata) if metadata.is_dir() => (),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "PID не является каталогом",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
                Err(error) => return Err(error),
            }
            return match read_process_start_ticks(sources.proc_root, marker.pid) {
                Ok(current) => Ok(current != start_ticks),
                // Исчезновение только stat не доказывает исчезновение процесса.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match fs::symlink_metadata(&process) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                        Err(error) => Err(error),
                        Ok(_) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "неизвестная схема маркера",
            ));
        }
    }
    match fs::symlink_metadata(sources.proc_root.join(marker.pid.to_string())) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn valid_run_id(name: &str) -> bool {
    name.len() == 36
        && name.starts_with("run-")
        && name[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn has_browser_artifacts(directory: &File) -> io::Result<bool> {
    Ok(names(directory)?
        .iter()
        .any(|name| valid_profile_name(name) || valid_browser_temp_name(name)))
}

fn current_uid() -> io::Result<u32> {
    Ok(fs::metadata("/proc/self")?.uid())
}

fn workspace_has_process_references(
    proc_root: &Path,
    run_path: &Path,
    uid: u32,
) -> io::Result<bool> {
    workspace_process_references(proc_root, run_path, uid, false, &[])
}

fn workspace_process_references(
    proc_root: &Path,
    run_path: &Path,
    uid: u32,
    include_generic: bool,
    ignored_current_fds: &[i32],
) -> io::Result<bool> {
    let run_path = fs::canonicalize(run_path)?;
    let entries = fs::read_dir(proc_root)?;
    let current_pid = std::process::id().to_string();
    for entry in entries {
        let entry = entry?;
        let pid = entry.file_name();
        if !pid.as_bytes().iter().all(u8::is_ascii_digit) || pid.as_bytes().is_empty() {
            continue;
        }
        let is_current_process = pid.as_bytes() == current_pid.as_bytes();
        if is_current_process && !include_generic {
            continue;
        }
        let process = entry.path();
        let metadata = match fs::metadata(&process) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) if process_has_disappeared(&process) => continue,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "нельзя проверить UID процесса",
                ));
            }
        };
        if metadata.uid() != uid {
            continue;
        }
        let comm = match fs::read(process.join("comm")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !process_has_disappeared(&process) {
                    return Err(error);
                }
                continue;
            }
            // Чтение `/proc/<pid>` исчезающего процесса бывает и `ESRCH`, а не только
            // `ENOENT`. Пропуск допустим лишь после доказательства исчезновения.
            Err(_) if process_has_disappeared(&process) => continue,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "не удалось проверить имя процесса с тем же UID",
                ));
            }
        };
        let cmdline = match fs::read(process.join("cmdline")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !process_has_disappeared(&process) {
                    return Err(error);
                }
                continue;
            }
            Err(_) if process_has_disappeared(&process) => continue,
            Err(_) if !include_generic && !is_chromium_process(&comm, &[]) => continue,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "не удалось проверить команду процесса с тем же UID",
                ));
            }
        };
        if command_uses_workspace(&cmdline, &run_path) {
            return Ok(true);
        }
        let chromium = is_chromium_process(&comm, &cmdline);
        if !chromium && !include_generic {
            continue;
        }
        if chromium {
            let environment = match fs::read(process.join("environ")) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !process_has_disappeared(&process) {
                        return Err(error);
                    }
                    continue;
                }
                Err(_) if process_has_disappeared(&process) => continue,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "не удалось проверить окружение процесса браузера",
                    ));
                }
            };
            let referenced = environment_uses_workspace(
                &environment,
                || fs::canonicalize(process.join("cwd")),
                &run_path,
            );
            match referenced {
                Ok(true) => return Ok(true),
                Ok(false) => (),
                Err(_) if process_has_disappeared(&process) => continue,
                Err(error) => return Err(error),
            }
        }
        if include_generic {
            if is_current_process {
                let environment = fs::read(process.join("environ"))?;
                if environment_uses_workspace(
                    &environment,
                    || fs::canonicalize(process.join("cwd")),
                    &run_path,
                )? {
                    return Ok(true);
                }
            }
            match fs::read_link(process.join("cwd")) {
                Ok(target) if path_is_within(&target, &run_path) => return Ok(true),
                Ok(_) => (),
                Err(_) if process_has_disappeared(&process) => continue,
                Err(error) => return Err(error),
            }
        }
        let descriptors = match fs::read_dir(process.join("fd")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !process_has_disappeared(&process) {
                    return Err(error);
                }
                continue;
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "нельзя проверить файловые дескрипторы процесса",
                ));
            }
        };
        for descriptor in descriptors {
            let descriptor = match descriptor {
                Ok(value) => value,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "нельзя перечислить файловые дескрипторы процесса",
                    ));
                }
            };
            if is_current_process
                && descriptor
                    .file_name()
                    .to_str()
                    .and_then(|value| value.parse::<i32>().ok())
                    .is_some_and(|fd| ignored_current_fds.contains(&fd))
            {
                continue;
            }
            let target = match fs::read_link(descriptor.path()) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "нельзя прочитать цель файлового дескриптора",
                    ));
                }
            };
            let mut target_bytes = target.as_os_str().as_bytes();
            if let Some(without_deleted) = target_bytes.strip_suffix(b" (deleted)") {
                target_bytes = without_deleted;
            }
            let target = PathBuf::from(OsString::from_vec(target_bytes.to_vec()));
            if path_is_within(&target, &run_path) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn command_uses_workspace(cmdline: &[u8], run_path: &Path) -> bool {
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    for (index, argument) in arguments.iter().enumerate() {
        let value = argument.strip_prefix(b"--user-data-dir=").or_else(|| {
            (*argument == b"--user-data-dir")
                .then(|| arguments.get(index + 1).copied())
                .flatten()
        });
        if let Some(value) = value {
            let path = PathBuf::from(OsString::from_vec(value.to_vec()));
            if path_is_within(&path, run_path) {
                return true;
            }
        }
    }
    false
}

fn process_has_disappeared(process: &Path) -> bool {
    fs::symlink_metadata(process).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Разрешает существующие компоненты, включая symlink, до первого отсутствующего.
/// Остаток допустим только из обычных имён: `..` после отсутствующего предка,
/// оборванная symlink и отказ доступа не дают доказательства принадлежности.
fn resolve_possibly_missing_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "требуется абсолютный путь",
        ));
    }
    let mut resolved = PathBuf::from("/");
    let mut missing = false;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => (),
            Component::Normal(name) => {
                resolved.push(name);
                if !missing {
                    match fs::symlink_metadata(&resolved) {
                        Ok(_) => {
                            resolved = fs::canonicalize(&resolved)?;
                            if !fs::metadata(&resolved)?.is_dir() {
                                return Err(io::Error::other(
                                    "временный путь содержит компонент, не являющийся каталогом",
                                ));
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => missing = true,
                        Err(error) => return Err(error),
                    }
                }
            }
            Component::ParentDir if !missing => {
                resolved.pop();
            }
            _ => {
                return Err(io::Error::other(
                    "принадлежность временного пути не доказана",
                ));
            }
        }
    }
    Ok(resolved)
}

fn environment_uses_workspace(
    environment: &[u8],
    cwd: impl Fn() -> io::Result<PathBuf>,
    run_path: &Path,
) -> io::Result<bool> {
    for entry in environment.split(|byte| *byte == 0) {
        let Some(value) = entry.strip_prefix(b"TMPDIR=") else {
            continue;
        };
        let path = PathBuf::from(OsString::from_vec(value.to_vec()));
        let path = if path.is_absolute() {
            path
        } else {
            cwd()?.join(path)
        };
        if path_is_within(&resolve_possibly_missing_path(&path)?, run_path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_chromium_process(comm: &[u8], cmdline: &[u8]) -> bool {
    let mut identity = comm.to_ascii_lowercase();
    identity.extend_from_slice(&cmdline.to_ascii_lowercase());
    identity.windows(6).any(|part| part == b"chrome")
        || identity.windows(8).any(|part| part == b"chromium")
}

fn path_is_within(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

fn unix_ms() -> io::Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    )
    .map_err(io::Error::other)
}

fn namespace(temp_root: &Path, uid: u32, create: bool) -> io::Result<Option<File>> {
    // NOFOLLOW действует и для `root`; пути к каталогам запуска не разрешается задавать
    // как произвольные абсолютные пути за пределами выбранного корня.
    let temp = File::from(open(
        temp_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let namespace_name = namespace_directory_name(uid);
    if create {
        match mkdirat(&temp, namespace_name.as_str(), Mode::from_raw_mode(0o700)) {
            Ok(()) => (),
            Err(error) if error == rustix::io::Errno::EXIST => (),
            Err(error) => return Err(error.into()),
        }
    }
    let directory = match directory_at(&temp, OsStr::new(&namespace_name)) {
        Ok(directory) => directory,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = directory.metadata()?;
    same_mount(&temp, &directory)?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "пространство имён временных каталогов принадлежит другому UID или имеет небезопасные права",
        ));
    }
    // Получение канонического пути не служит доказательством: дескриптор файла остаётся границей.
    if fs::canonicalize(temp_root.join(&namespace_name))?
        != fs::canonicalize(temp_root)?.join(&namespace_name)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "пространство имён временных каталогов выходит за пределы `root`",
        ));
    }
    Ok(Some(directory))
}

fn namespace_directory_name(uid: u32) -> String {
    format!("{NAMESPACE_PREFIX}-{uid}")
}

fn directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            entries.push(OsStr::from_bytes(name).to_owned());
        }
    }
    entries.sort();
    Ok(entries)
}

fn verify_identity(parent: &File, name: &OsStr, directory: &File) -> io::Result<()> {
    let current = directory_at(parent, name)?;
    let before = directory.metadata()?;
    let after = current.metadata()?;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "временный каталог подменён",
        ));
    }
    Ok(())
}

// Идентификатор mount различает bind mounts даже на одном устройстве. Если
// идентификатор mount недоступен, GC отказывается обходить такое дерево.
fn same_mount(parent: &File, child: &File) -> io::Result<()> {
    let before = statx(parent, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    let after = statx(child, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if before.stx_mask & StatxFlags::MNT_ID.bits() == 0
        || after.stx_mask & StatxFlags::MNT_ID.bits() == 0
        || before.stx_mnt_id != after.stx_mnt_id
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "граница mount или неизвестный идентификатор mount",
        ));
    }
    Ok(())
}

fn inspect_tree(directory: &File, uid: u32) -> io::Result<u64> {
    let mut bytes = 0_u64;
    for name in names(directory)? {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        if stat.st_uid != uid
            || stat.st_mode & 0o022 != 0
            || stat.st_dev != directory.metadata()?.dev()
            || kind == rustix::fs::FileType::Symlink
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "дерево содержит объект другого UID, границу mount или symlink",
            ));
        }
        if kind == rustix::fs::FileType::Directory {
            let child = directory_at(directory, &name)?;
            same_mount(directory, &child)?;
            bytes = bytes.saturating_add(inspect_tree(&child, uid)?);
        } else if kind == rustix::fs::FileType::RegularFile {
            bytes = bytes.saturating_add(u64::try_from(stat.st_size).unwrap_or(0));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "дерево содержит файл специального типа",
            ));
        }
    }
    Ok(bytes)
}

fn inspect_orphan_tree(directory: &File, uid: u32, run_path: &Path) -> io::Result<u64> {
    fn inspect(
        directory: &File,
        uid: u32,
        run_path: &Path,
        relative: &mut Vec<OsString>,
    ) -> io::Result<u64> {
        let mut bytes = 0_u64;
        let device = directory.metadata()?.dev();
        for name in names(directory)? {
            let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
            let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            if stat.st_uid != uid
                || (kind != rustix::fs::FileType::Symlink && stat.st_mode & 0o022 != 0)
                || stat.st_dev != device
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "рабочая область содержит объект другого UID, доступную для записи запись или границу mount",
                ));
            }
            relative.push(name.clone());
            let browser_temp_path = path_is_browser_temp(relative);
            match kind {
                rustix::fs::FileType::Directory => {
                    if browser_temp_path
                        && (relative.len() == 3
                            || (relative.len() == 2
                                && !valid_chromium_socket_directory(relative.last().unwrap())))
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "временный каталог браузера содержит неизвестный каталог",
                        ));
                    }
                    let child = directory_at(directory, &name)?;
                    same_mount(directory, &child)?;
                    bytes = bytes.saturating_add(inspect(&child, uid, run_path, relative)?);
                }
                rustix::fs::FileType::RegularFile => {
                    if browser_temp_path && !valid_chromium_temp_file(relative) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "временный каталог браузера содержит неизвестный файл",
                        ));
                    }
                    bytes = bytes.saturating_add(u64::try_from(stat.st_size).unwrap_or(0));
                }
                rustix::fs::FileType::Symlink => {
                    let target = readlinkat(directory, &name, Vec::new())
                        .map_err(io::Error::from)?
                        .into_bytes();
                    if !allowed_browser_symlink(relative, &target, run_path) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "рабочая область содержит неизвестную ссылку типа symlink",
                        ));
                    }
                }
                rustix::fs::FileType::Socket => {
                    if !valid_browser_socket(relative) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "рабочая область содержит неизвестный файл специального типа",
                        ));
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "рабочая область содержит неизвестный файл специального типа",
                    ));
                }
            }
            relative.pop();
        }
        Ok(bytes)
    }

    inspect(directory, uid, run_path, &mut Vec::new())
}

fn path_is_browser_temp(relative: &[OsString]) -> bool {
    relative
        .first()
        .is_some_and(|name| valid_browser_temp_name(name))
}

fn valid_browser_temp_name(name: &OsStr) -> bool {
    let value = name.as_bytes();
    value.len() == 4
        && value[0] == b't'
        && value[1..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn valid_profile_name(name: &OsStr) -> bool {
    let Some(value) = name.to_str() else {
        return false;
    };
    value.strip_prefix("browser-profile-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_chromium_socket_directory(name: &OsStr) -> bool {
    let Some(suffix) = name
        .to_str()
        .and_then(|value| value.strip_prefix("org.chromium.Chromium."))
    else {
        return false;
    };
    suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_chromium_temp_file(relative: &[OsString]) -> bool {
    if relative.len() != 2 {
        return false;
    }
    let Some(suffix) = relative[1]
        .to_str()
        .and_then(|value| value.strip_prefix(".org.chromium.Chromium."))
    else {
        return false;
    };
    path_is_browser_temp(relative)
        && suffix.len() == 6
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_browser_socket(relative: &[OsString]) -> bool {
    relative.len() == 3
        && path_is_browser_temp(relative)
        && valid_chromium_socket_directory(&relative[1])
        && relative[2] == OsStr::new("SingletonSocket")
}

fn allowed_browser_symlink(relative: &[OsString], target: &[u8], run_path: &Path) -> bool {
    if relative.len() == 2 && valid_profile_name(&relative[0]) {
        return match relative[1].to_str() {
            Some("SingletonCookie") => decimal_u64(target),
            Some("SingletonLock") => valid_singleton_lock_target(target),
            Some("SingletonSocket") => {
                let target_path = PathBuf::from(OsString::from_vec(target.to_vec()));
                valid_global_socket_path(&target_path)
                    || valid_local_socket_path(&target_path, run_path)
            }
            _ => false,
        };
    }
    relative.len() == 3
        && path_is_browser_temp(relative)
        && valid_chromium_socket_directory(&relative[1])
        && relative[2] == OsStr::new("SingletonCookie")
        && decimal_u64(target)
}

fn valid_global_socket_path(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    parent.parent() == Some(Path::new(TEMP_ROOT))
        && path.file_name() == Some(OsStr::new("SingletonSocket"))
        && parent
            .file_name()
            .is_some_and(valid_chromium_socket_directory)
}

fn valid_local_socket_path(path: &Path, run_path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(chromium_dir) = parent.file_name() else {
        return false;
    };
    let Some(temp_dir) = parent.parent() else {
        return false;
    };
    temp_dir.parent() == Some(run_path)
        && temp_dir.file_name().is_some_and(valid_browser_temp_name)
        && valid_chromium_socket_directory(chromium_dir)
        && path.file_name() == Some(OsStr::new("SingletonSocket"))
}

fn valid_singleton_lock_target(target: &[u8]) -> bool {
    let Some(separator) = target.iter().rposition(|byte| *byte == b'-') else {
        return false;
    };
    let (host, pid_with_separator) = target.split_at(separator);
    let pid = &pid_with_separator[1..];
    !host.is_empty()
        && host
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'))
        && decimal_bytes(pid)
        && std::str::from_utf8(pid)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .is_some_and(|pid| pid > 0)
}

fn decimal_bytes(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(u8::is_ascii_digit)
}

fn decimal_u64(value: &[u8]) -> bool {
    decimal_bytes(value)
        && std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .is_some()
}

// Измерение учитывает обычные файлы, в том числе в деревьях с symlink,
// но не переходит по самим symlink и не пересекает границы чужого mount.
fn measure_tree(directory: &File, uid: u32) -> io::Result<u64> {
    let mut bytes = 0_u64;
    let device = directory.metadata()?.dev();
    for name in names(directory)? {
        let stat = match statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => return Err(error.into()),
        };
        if stat.st_uid != uid || stat.st_dev != device {
            continue;
        }
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Directory => {
                let child = match directory_at(directory, &name) {
                    Ok(child) => child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if same_mount(directory, &child).is_err() {
                    continue;
                }
                match measure_tree(&child, uid) {
                    Ok(child_bytes) => bytes = bytes.saturating_add(child_bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
            }
            rustix::fs::FileType::RegularFile => {
                bytes = bytes.saturating_add(u64::try_from(stat.st_size).unwrap_or(0));
            }
            _ => (),
        }
    }
    Ok(bytes)
}

fn remove_contents(directory: &File) -> io::Result<()> {
    let mut entries = names(directory)?;
    // До удаления последнего файла сохраняем маркер для повторной очистки при ошибке.
    entries.sort_by_key(|name| name == OsStr::new(MARKER));
    for name in entries {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory {
            let child = directory_at(directory, &name)?;
            if same_mount(directory, &child).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "очистка запрещена: обнаружена вложенная граница mount",
                ));
            }
            remove_contents(&child)?;
            verify_identity(directory, &name, &child)?;
            unlinkat(directory, &name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(directory, &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Serialize)]
pub struct TempSnapshot {
    pub directories: usize,
    pub bytes: u64,
    pub paths: Vec<PathBuf>,
}

/// Только измерение: сомнительные записи не обходятся и не удаляются.
pub fn snapshot() -> io::Result<TempSnapshot> {
    snapshot_under(&selected_temp_root()?)
}

pub fn snapshot_under(root: &Path) -> io::Result<TempSnapshot> {
    let mut snapshot = TempSnapshot::default();
    let uid = current_uid()?;
    let namespace_name = namespace_directory_name(uid);
    if let Some(parent) = namespace(root, uid, false)? {
        for name in names(&parent)? {
            if name.to_str().is_some_and(valid_run_id) {
                measure_entry(
                    &parent,
                    &name,
                    root.join(&namespace_name).join(&name),
                    uid,
                    &mut snapshot,
                )?;
            }
        }
    }
    let root_file = root_directory(root)?;
    for name in names(&root_file)? {
        if name.to_str().is_some_and(|name| legacy_pid(name).is_some()) {
            measure_entry(&root_file, &name, root.join(&name), uid, &mut snapshot)?;
        }
    }
    snapshot.paths.sort();
    Ok(snapshot)
}

fn measure_entry(
    parent: &File,
    name: &OsStr,
    path: PathBuf,
    uid: u32,
    snapshot: &mut TempSnapshot,
) -> io::Result<()> {
    let directory = match directory_at(parent, name) {
        Ok(directory) => directory,
        Err(_) => return Ok(()), // Недоказанное или уже исчезнувшее дерево.
    };
    if directory.metadata()?.uid() != uid || same_mount(parent, &directory).is_err() {
        return Ok(());
    }
    let bytes = match measure_tree(&directory, uid)
        .and_then(|bytes| verify_identity(parent, name, &directory).map(|()| bytes))
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    snapshot.directories += 1;
    snapshot.paths.push(path);
    snapshot.bytes = snapshot.bytes.saturating_add(bytes);
    Ok(())
}

/// Одноразовая миграция строго известных старых шаблонов имён каталогов без маркера.
/// Вызывается явно: начальная очистка в тестовой фикстуре не удаляет старые деревья автоматически.
/// Для старых каталогов TTL составляет не менее 24 часов; известный живой PID блокирует удаление.
pub fn cleanup_legacy_orphans(min_age: Duration) -> io::Result<GcReport> {
    cleanup_legacy_orphans_under(&selected_temp_root()?, min_age)
}

/// Удаляет только распознанные старые формы имён каталогов внутри явно выбранного `root`.
pub fn cleanup_legacy_orphans_under(root: &Path, min_age: Duration) -> io::Result<GcReport> {
    cleanup_legacy_under(root, min_age.max(DEFAULT_ORPHAN_MIN_AGE), true)
}

/// Строит план для путей к строго распознанным старым каталогам без владельца.
pub fn plan_legacy_orphans_under(root: &Path, min_age: Duration) -> io::Result<GcReport> {
    cleanup_legacy_under(root, min_age.max(DEFAULT_ORPHAN_MIN_AGE), false)
}

fn cleanup_legacy_under(root: &Path, min_age: Duration, apply: bool) -> io::Result<GcReport> {
    let uid = current_uid()?;
    let parent = root_directory(root)?;
    let mut report = GcReport {
        root: root.to_path_buf(),
        ..GcReport::default()
    };
    for name in names(&parent)? {
        let Some(pid) = name.to_str().and_then(legacy_pid) else {
            continue;
        };
        let path = root.join(&name);
        let inspection = (|| {
            let directory = directory_at(&parent, &name)?;
            let metadata = directory.metadata()?;
            same_mount(&parent, &directory)?;
            if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "каталог принадлежит другому UID или старое дерево доступно для записи",
                ));
            }
            if let Some(pid) = pid {
                match fs::symlink_metadata(format!("/proc/{pid}")) {
                    Ok(_) => {
                        return Err(io::Error::other("процесс со старым PID всё ещё работает"));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
            }
            if metadata.modified()?.elapsed().map_err(io::Error::other)? < min_age {
                return Err(io::Error::other(
                    "старый каталог ещё не достиг минимального возраста",
                ));
            }
            let bytes = inspect_tree(&directory, uid)?;
            Ok((directory, bytes))
        })();
        let (outcome, reason, bytes, ownership, live) = match inspection {
            Err(error) => {
                report.skipped += 1;
                ("deferred", error.to_string(), 0, "unknown", None)
            }
            Ok((directory, bytes)) => {
                let live = pid.map(|_| false);
                if !apply {
                    (
                        "delete",
                        if pid.is_some() {
                            "старое имя каталога, UID, возраст и отсутствие живого PID подтверждены"
                        } else {
                            "старое имя каталога без PID, UID и возраст подтверждены"
                        }
                        .into(),
                        bytes,
                        "legacy-unverified",
                        live,
                    )
                } else {
                    match verify_identity(&parent, &name, &directory)
                        .and_then(|()| remove_contents(&directory))
                        .and_then(|()| verify_identity(&parent, &name, &directory))
                        .and_then(|()| {
                            unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(io::Error::from)
                        }) {
                        Ok(()) => {
                            report.removed += 1;
                            report.removed_bytes = report.removed_bytes.saturating_add(bytes);
                            (
                                "removed",
                                if pid.is_some() {
                                    "старое имя каталога с PID удалено после проверки его отсутствия"
                                } else {
                                    "старое имя каталога без PID удалено после проверки UID и возраста"
                                }
                                .into(),
                                bytes,
                                "legacy-unverified",
                                live,
                            )
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            report.skipped += 1;
                            (
                                "deferred",
                                "дерево уже удалено параллельной уборкой".into(),
                                0,
                                "legacy-unverified",
                                None,
                            )
                        }
                        Err(error) => {
                            report.errors += 1;
                            ("error", error.to_string(), 0, "legacy-unverified", None)
                        }
                    }
                }
            }
        };
        tracing::info!(path = %path.display(), outcome, %reason, bytes, "очистка старых временных каталогов");
        report.entries.push(GcEntry {
            path,
            outcome: outcome.into(),
            reason,
            bytes,
            ownership: ownership.into(),
            live,
        });
    }
    tracing::info!(
        removed = report.removed,
        removed_bytes = report.removed_bytes,
        skipped = report.skipped,
        errors = report.errors,
        "сводка очистки старых временных каталогов"
    );
    Ok(report)
}

fn root_directory(root: &Path) -> io::Result<File> {
    Ok(File::from(open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

/// `Some(None)`: подтверждено старое случайное имя каталога без PID.
fn legacy_pid(name: &str) -> Option<Option<u32>> {
    if let Some(hex) = name.strip_prefix("kanji-")
        && hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Some(None);
    }
    for prefix in [
        "asset-store-pitch-cli-",
        "asset-store-pitch-batch-",
        "jpdb-live-acceptance-",
        "yarxi-live-acceptance-",
        "jpdb-session-stop-test-",
        "jpdb-failure-evidence-",
        "jpdb-report-durability-",
        "jpdb-acceptance-test-",
        "jpdb-acceptance-html-test-",
        "yarxi-acceptance-test-",
        "yarxi-acceptance-html-test-",
        "yarxi-acceptance-evidence-test-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, sequence) = suffix.split_once('-')?;
            let valid_sequence = if prefix.starts_with("asset-store-") {
                counter_value(sequence)
            } else {
                decimal(sequence) && sequence.parse::<u128>().is_ok()
            };
            return valid_sequence.then(|| parse_pid(pid)).flatten().map(Some);
        }
    }
    for prefix in [
        "asset-store-generic-batch-",
        "anki-kanji-batch-",
        "kanji-batch-cli-",
        "kanji-assets-source-boundary-",
        "asset-store-unit-",
        "asset-trust-",
        "kanji-assets-cli-",
        "pitch-cli-contract-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, counter) = suffix.split_once('-')?;
            return counter_value(counter)
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    if let Some(suffix) = name.strip_prefix("asset-store-diagnostics-") {
        let (pid, timestamp_counter) = suffix.split_once('-')?;
        let (timestamp, counter) = timestamp_counter.split_once('-')?;
        return (decimal(timestamp) && timestamp.parse::<u128>().is_ok() && counter_value(counter))
            .then(|| parse_pid(pid))
            .flatten()
            .map(Some);
    }
    for prefix in [
        "anki-repo-unit-",
        "anki-repo-test-",
        "asset-store-contract-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, label_counter) = suffix.split_once('-')?;
            let (label, counter) = label_counter.rsplit_once('-')?;
            return (safe_label(label) && counter_value(counter))
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    for prefix in ["domain-semantics-", "pitch-corpus-trust-"] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (label_pid, counter) = suffix.rsplit_once('-')?;
            let (label, pid) = label_pid.rsplit_once('-')?;
            return (safe_label(label) && counter_value(counter))
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    let suffix = name.strip_prefix("anki-manifest-")?;
    let (label_pid, thread) = suffix.rsplit_once("-ThreadId(")?;
    let thread = thread.strip_suffix(')')?;
    if !decimal(thread) || thread.parse::<u64>().is_err() {
        return None;
    }
    let (label, pid) = label_pid.rsplit_once('-')?;
    const LABELS: &[&str] = &[
        "free",
        "foreign",
        "corrupt",
        "escape",
        "self-claim",
        "foreign-inside",
        "foreign-dir",
        "symlink-conflict",
        "symlink-conflict-outside",
        "symlink-at-file",
        "symlink-at-file-outside",
        "symlink-walk",
        "symlink-walk-outside",
        "symlink-out",
        "symlink-out-real",
        "repeat",
        "target-foreign",
        "interrupted",
        "parent-symlink",
        "parent-symlink-outside",
        "target-symlink",
        "abort",
        "missing-staged",
        "confined",
        "prune",
        "dir-on-file",
        "file-on-dir",
    ];
    if !LABELS.contains(&label) {
        return None;
    }
    parse_pid(pid).map(Some)
}

fn counter_value(value: &str) -> bool {
    decimal(value) && value.parse::<u64>().is_ok()
}
fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn decimal(value: &str) -> bool {
    !value.is_empty()
        && (value.len() == 1 || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn parse_pid(value: &str) -> Option<u32> {
    if decimal(value) {
        value.parse().ok().filter(|pid| *pid > 0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sandbox() -> TempWorkspace {
        TempWorkspace::create_under(Path::new(TEMP_ROOT), "temp-workspace-unit").unwrap()
    }

    #[test]
    fn configured_root_initial_gc_runs_once_under_concurrent_creation() {
        let root = std::env::temp_dir().join(format!("configured-root-gc-{}", random_hex()));
        let calls = Arc::new(AtomicUsize::new(0));
        let workers = (0..8)
            .map(|_| {
                let root = root.clone();
                let calls = Arc::clone(&calls);
                std::thread::spawn(move || {
                    cleanup_configured_root_once_with(&root, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let other_root = std::env::temp_dir().join(format!("configured-root-gc-{}", random_hex()));
        cleanup_configured_root_once_with(&other_root, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn configured_root_initial_gc_caches_and_propagates_first_error() {
        let root = std::env::temp_dir().join(format!("configured-root-gc-error-{}", random_hex()));
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let error = cleanup_configured_root_once_with(&root, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected configured-root GC failure",
                ))
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "injected configured-root GC failure");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn make_orphan(root: &Path, age: Duration) -> PathBuf {
        let mut owner = TempWorkspace::create_under(root, "gc-unit").unwrap();
        let marker_path = owner.path().join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.schema = LEGACY_SCHEMA;
        marker.boot_id = None;
        marker.process_start_ticks = None;
        marker.pid = u32::MAX;
        marker.created_unix_ms = unix_ms().unwrap() - u64::try_from(age.as_millis()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        owner
            .directory
            .set_times(FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
        owner.closed = true; // Имитация SIGKILL без уничтожения тестового процесса.
        owner.path().to_path_buf()
    }

    #[test]
    fn partial_creation_failure_reports_unproven_cleanup_and_preserves_replacement() {
        let owner =
            TempWorkspace::create_under(Path::new(TEMP_ROOT), "partial-create-error").unwrap();
        let owned_path = owner.path().to_path_buf();
        let moved_path = owned_path.with_extension("moved");
        fs::rename(&owned_path, &moved_path).unwrap();
        fs::create_dir(&owned_path).unwrap();
        let error = owner
            .finish_creation(Err(io::Error::other("сбой записи маркера")))
            .unwrap_err();
        let typed = error
            .get_ref()
            .unwrap()
            .downcast_ref::<WorkspaceCreationCleanupFailure>()
            .unwrap();
        assert_eq!(typed.original.to_string(), "сбой записи маркера");
        assert!(typed.cleanup.to_string().contains("подменён"));
        assert!(matches!(
            crate::browser_runtime::BrowserLaunchError::workspace_creation(
                error,
                "worker_workspace_create_failed"
            ),
            crate::browser_runtime::BrowserLaunchError::CleanupFailed { .. }
        ));
        assert!(owned_path.is_dir());
        assert!(moved_path.is_dir());
        fs::remove_dir(&owned_path).unwrap();
        fs::remove_dir_all(&moved_path).unwrap();
    }

    #[test]
    fn partial_creation_failure_with_proven_cleanup_remains_regular_io_error() {
        let owner =
            TempWorkspace::create_under(Path::new(TEMP_ROOT), "partial-create-error").unwrap();
        let path = owner.path().to_path_buf();
        let error = owner
            .finish_creation(Err(io::Error::other("сбой записи маркера")))
            .unwrap_err();
        assert_eq!(error.to_string(), "сбой записи маркера");
        assert!(
            error
                .get_ref()
                .and_then(|error| error.downcast_ref::<WorkspaceCreationCleanupFailure>())
                .is_none()
        );
        assert!(!path.exists());
        assert!(matches!(
            crate::browser_runtime::BrowserLaunchError::workspace_creation(
                error,
                "worker_workspace_create_failed"
            ),
            crate::browser_runtime::BrowserLaunchError::Setup(_)
        ));
    }

    #[test]
    fn namespace_is_scoped_to_uid() {
        assert_ne!(
            namespace_directory_name(1000),
            namespace_directory_name(1001)
        );
    }

    const TEST_BOOT_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const OTHER_BOOT_ID: &str = "fedcba98-7654-3210-fedc-ba9876543210";

    fn proc_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let proc_root = root.join("proc-fixture");
        let boot_id = proc_root.join("sys/kernel/random/boot_id");
        fs::create_dir_all(boot_id.parent().unwrap()).unwrap();
        fs::write(&boot_id, format!("{TEST_BOOT_ID}\n")).unwrap();
        (proc_root, boot_id)
    }

    fn update_marker(path: &Path, change: impl FnOnce(&mut OwnershipMarker)) {
        let marker_path = path.join(MARKER);
        let mut marker = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        change(&mut marker);
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
    }

    fn exact_orphan(root: &Path) -> (PathBuf, u32) {
        let path = make_orphan(root, Duration::ZERO);
        let pid = std::process::id() + 200_000;
        update_marker(&path, |marker| {
            marker.schema = SCHEMA;
            marker.pid = pid;
            marker.boot_id = Some(TEST_BOOT_ID.into());
            marker.process_start_ticks = Some(123_456);
        });
        (path, pid)
    }

    fn stat_fixture(pid: u32, comm: &[u8], start_ticks: &str) -> Vec<u8> {
        let mut bytes = format!("{pid} (").into_bytes();
        bytes.extend_from_slice(comm);
        bytes.extend_from_slice(b") S");
        for field in 4..22 {
            bytes.extend_from_slice(format!(" {field}").as_bytes());
        }
        bytes.extend_from_slice(format!(" {start_ticks} 0 0\n").as_bytes());
        bytes
    }

    fn fake_owner(proc_root: &Path, pid: u32, start_ticks: &str) -> PathBuf {
        let process = proc_root.join(pid.to_string());
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::write(process.join("comm"), b"fixture-owner\n").unwrap();
        fs::write(process.join("cmdline"), b"fixture-owner\0").unwrap();
        symlink(Path::new("/"), process.join("cwd")).unwrap();
        fs::write(
            process.join("stat"),
            stat_fixture(pid, b"owner", start_ticks),
        )
        .unwrap();
        process
    }

    #[test]
    fn stat_parser_handles_comm_parentheses_spaces_and_non_utf8() {
        let pid = std::process::id() + 200_000;
        for comm in [
            b"owner".as_slice(),
            b"name with ) and ( parentheses))",
            b"name\xff)",
        ] {
            assert_eq!(
                parse_process_start_ticks(&stat_fixture(pid, comm, "987654"), pid).unwrap(),
                987654
            );
        }
        let mut missing_tail = stat_fixture(pid, b"owner", "123");
        missing_tail.truncate(missing_tail.iter().rposition(|byte| *byte == b')').unwrap() + 1);
        for malformed in [
            Vec::new(),
            missing_tail,
            stat_fixture(pid + 1, b"owner", "123"),
            stat_fixture(pid, b"owner", "-1"),
            stat_fixture(pid, b"owner", "+1"),
            stat_fixture(pid, b"owner", "18446744073709551616"),
            stat_fixture(pid, b"owner", "nonnumeric"),
            stat_fixture(pid, b"owner", "123")
                .into_iter()
                .chain(b") S 0".iter().copied())
                .collect(),
        ] {
            assert!(
                parse_process_start_ticks(&malformed, pid).is_err(),
                "{malformed:?}"
            );
        }
        assert!(
            parse_process_start_ticks(format!("{pid} (owner) S 1 2\n").as_bytes(), pid).is_err()
        );
        assert!(
            parse_process_start_ticks(
                format!("{pid} (owner) ? {} 123", "0 ".repeat(18)).as_bytes(),
                pid
            )
            .is_err()
        );
    }

    #[test]
    fn new_marker_publishes_complete_exact_identity() {
        let owner = sandbox();
        let marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(owner.path().join(MARKER)).unwrap()).unwrap();
        assert_eq!(marker.schema, SCHEMA);
        assert_eq!(marker.pid, std::process::id());
        assert_eq!(
            marker.boot_id.unwrap(),
            read_boot_id(Path::new("/proc/sys/kernel/random/boot_id")).unwrap()
        );
        assert_eq!(
            marker.process_start_ticks.unwrap(),
            read_process_start_ticks(Path::new("/proc"), std::process::id()).unwrap()
        );
        assert!(!owner.path().join(".anki-decks-owner.tmp").exists());
    }

    #[test]
    fn exact_live_owner_survives_regardless_of_age() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (path, pid) = exact_orphan(root.path());
        fake_owner(&proc_root, pid, "123456");
        for age in [Duration::ZERO, DEFAULT_ORPHAN_MIN_AGE * 2] {
            update_marker(&path, |marker| {
                marker.created_unix_ms =
                    unix_ms().unwrap() - u64::try_from(age.as_millis()).unwrap()
            });
            set_old_directory_time(&path, age);
            let report =
                cleanup_under_with_proc(root.path(), &proc_root, DEFAULT_ORPHAN_MIN_AGE).unwrap();
            assert_eq!(report.removed, 0);
            assert_eq!(report.skipped, 1);
            assert!(path.exists());
        }
    }

    #[test]
    fn fresh_exact_dead_owner_collects_without_ttl_and_pid_alone_is_insufficient() {
        for death in ["missing", "reused", "reboot"] {
            let root = sandbox();
            let (proc_root, boot_id) = proc_fixture(root.path());
            let (path, pid) = exact_orphan(root.path());
            match death {
                "reused" => {
                    fake_owner(&proc_root, pid, "123457");
                }
                "reboot" => {
                    fake_owner(&proc_root, pid, "123456");
                    fs::write(boot_id, format!("{OTHER_BOOT_ID}\n")).unwrap();
                }
                _ => (),
            }
            let marker: OwnershipMarker =
                serde_json::from_slice(&fs::read(path.join(MARKER)).unwrap()).unwrap();
            assert!(unix_ms().unwrap() - marker.created_unix_ms < 60_000);
            if death == "reused" {
                assert!(proc_root.join(marker.pid.to_string()).exists());
                assert_ne!(
                    read_process_start_ticks(&proc_root, marker.pid).unwrap(),
                    marker.process_start_ticks.unwrap()
                );
            }
            let report =
                cleanup_under_with_proc(root.path(), &proc_root, DEFAULT_ORPHAN_MIN_AGE * 2)
                    .unwrap();
            assert_eq!(report.removed, 1);
            assert!(!path.exists());
        }
    }

    #[test]
    fn exact_owner_errors_and_incomplete_identity_fail_closed() {
        for failure in [
            "boot-missing",
            "boot-malformed",
            "boot-io",
            "proc-io",
            "stat-missing",
            "stat-malformed",
            "stat-io",
            "pid-symlink",
            "marker-boot-malformed",
            "marker-boot-missing",
            "marker-start-missing",
        ] {
            let root = sandbox();
            let (proc_root, boot_id) = proc_fixture(root.path());
            let (path, pid) = exact_orphan(root.path());
            let process = fake_owner(&proc_root, pid, "123457");
            match failure {
                "boot-missing" => fs::remove_file(&boot_id).unwrap(),
                "boot-malformed" => fs::write(&boot_id, b"not-a-boot-id\n").unwrap(),
                "boot-io" => {
                    fs::remove_file(&boot_id).unwrap();
                    fs::create_dir(&boot_id).unwrap();
                }
                "proc-io" => {
                    fs::remove_dir_all(&process).unwrap();
                    fs::write(&process, b"not a PID directory").unwrap();
                }
                "stat-missing" => fs::remove_file(process.join("stat")).unwrap(),
                "stat-malformed" => fs::write(process.join("stat"), b"malformed").unwrap(),
                "stat-io" => {
                    fs::remove_file(process.join("stat")).unwrap();
                    fs::create_dir(process.join("stat")).unwrap();
                }
                "pid-symlink" => {
                    fs::remove_dir_all(&process).unwrap();
                    symlink(root.path(), &process).unwrap();
                }
                "marker-boot-malformed" => {
                    update_marker(&path, |marker| marker.boot_id = Some("invalid".into()))
                }
                "marker-boot-missing" => update_marker(&path, |marker| marker.boot_id = None),
                "marker-start-missing" => {
                    update_marker(&path, |marker| marker.process_start_ticks = None)
                }
                _ => unreachable!(),
            }
            let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
            assert_eq!(report.removed, 0);
            assert_eq!(report.skipped, 1);
            assert!(path.exists());
        }
    }

    #[test]
    fn permission_denied_boot_and_proc_stat_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        if current_uid().unwrap() == 0 {
            return; // Пользователь `root` обходит ограничения DAC; остальные тесты ввода-вывода работают и с `root`.
        }
        for failure in ["boot", "pid", "stat", "proc"] {
            let root = sandbox();
            let (proc_root, boot_id) = proc_fixture(root.path());
            let (path, pid) = exact_orphan(root.path());
            let process = fake_owner(&proc_root, pid, "123457");
            let blocked = match failure {
                "boot" => boot_id,
                "pid" => process,
                "stat" => process.join("stat"),
                "proc" => proc_root.clone(),
                _ => unreachable!(),
            };
            let permissions = fs::metadata(&blocked).unwrap().permissions();
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
            let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
            fs::set_permissions(&blocked, permissions).unwrap();
            assert_eq!(report.removed, 0);
            assert_eq!(report.skipped, 1);
            assert!(path.exists());
        }
    }

    #[test]
    fn mount_identity_rejects_cross_mount_descriptors() {
        let root = sandbox();
        let proc = root_directory(Path::new("/proc")).unwrap();
        same_mount(&root.directory, &root.directory).unwrap();
        assert_eq!(
            same_mount(&root.directory, &proc).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn fresh_exact_owner_future_timestamps_fail_closed() {
        for future in ["marker", "directory"] {
            let root = sandbox();
            let (proc_root, _) = proc_fixture(root.path());
            let (path, _) = exact_orphan(root.path());
            if future == "marker" {
                update_marker(&path, |marker| {
                    marker.created_unix_ms = unix_ms().unwrap() + 60_000
                });
            } else {
                File::open(&path)
                    .unwrap()
                    .set_times(
                        FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(60)),
                    )
                    .unwrap();
            }
            assert_eq!(
                cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO)
                    .unwrap()
                    .removed,
                0
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn boot_id_parser_rejects_ambiguous_or_noncanonical_input() {
        let root = sandbox();
        let boot_id = root.path().join("boot-id");
        for value in [
            format!("{TEST_BOOT_ID}\n\n"),
            format!(" {TEST_BOOT_ID}\n"),
            format!("{TEST_BOOT_ID}\r\n"),
            TEST_BOOT_ID.to_uppercase(),
            "0".repeat(36),
        ] {
            fs::write(&boot_id, value).unwrap();
            assert!(read_boot_id(&boot_id).is_err());
        }
    }

    #[test]
    fn exact_dead_owner_preserves_browser_and_generic_process_references() {
        for reference in [
            "browser-profile",
            "browser-env",
            "browser-fd",
            "generic-fd",
            "generic-cwd",
        ] {
            let root = sandbox();
            let (proc_root, _) = proc_fixture(root.path());
            let (path, _) = exact_orphan(root.path());
            let profile = path.join(format!("browser-profile-{}", random_hex()));
            fs::create_dir(&profile).unwrap();
            let process = fake_chromium_process(
                &proc_root,
                300_000,
                if reference == "browser-profile" {
                    format!("chrome\0--user-data-dir={}\0", profile.display()).into_bytes()
                } else {
                    b"chrome\0".to_vec()
                }
                .as_slice(),
                if reference == "browser-env" {
                    format!("TMPDIR={}\0", profile.display()).into_bytes()
                } else {
                    b"TMPDIR=/tmp\0".to_vec()
                }
                .as_slice(),
                matches!(reference, "browser-fd" | "generic-fd").then_some(profile.as_path()),
            );
            if reference.starts_with("generic") {
                fs::write(process.join("comm"), b"generic\n").unwrap();
                fs::write(process.join("cmdline"), b"generic\0").unwrap();
            }
            if reference == "generic-cwd" {
                fs::remove_file(process.join("cwd")).unwrap();
                symlink(&profile, process.join("cwd")).unwrap();
            }
            let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
            assert_eq!(report.removed, 0);
            assert!(path.exists());
            fs::remove_dir_all(process).unwrap();
            assert_eq!(
                cleanup_under_with_proc(root.path(), &proc_root, DEFAULT_ORPHAN_MIN_AGE)
                    .unwrap()
                    .removed,
                1
            );
        }
    }

    #[test]
    fn exact_gc_checks_current_process_cwd_and_ignores_only_its_own_directory_fd() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (path, _) = exact_orphan(root.path());
        let process = proc_root.join(std::process::id().to_string());
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::write(process.join("comm"), b"gc-fixture\n").unwrap();
        fs::write(process.join("cmdline"), b"gc-fixture\0").unwrap();
        fs::write(process.join("environ"), b"TMPDIR=/tmp\0").unwrap();

        symlink(&path, process.join("cwd")).unwrap();
        assert!(
            workspace_process_references(&proc_root, &path, current_uid().unwrap(), true, &[7],)
                .unwrap()
        );
        fs::remove_file(process.join("cwd")).unwrap();
        symlink(Path::new("/"), process.join("cwd")).unwrap();

        symlink(&path, process.join("fd/7")).unwrap();
        assert!(
            !workspace_process_references(&proc_root, &path, current_uid().unwrap(), true, &[7],)
                .unwrap()
        );
        symlink(&path, process.join("fd/8")).unwrap();
        assert!(
            workspace_process_references(&proc_root, &path, current_uid().unwrap(), true, &[7],)
                .unwrap()
        );
    }

    #[test]
    fn browser_tmpdir_resolves_parent_components_before_workspace_check() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (path, _) = exact_orphan(root.path());
        let temp = path.join("browser-temp");
        fs::create_dir(&temp).unwrap();
        let cwd = root.path().join("sibling-cwd");
        fs::create_dir(&cwd).unwrap();
        let tmpdir = Path::new("..")
            .join(path.strip_prefix(root.path()).unwrap())
            .join("browser-temp");
        let environment = format!("TMPDIR={}\0", tmpdir.display());
        let process = fake_chromium_process(
            &proc_root,
            300_001,
            b"chrome\0",
            environment.as_bytes(),
            None,
        );
        fs::remove_file(process.join("cwd")).unwrap();
        symlink(&cwd, process.join("cwd")).unwrap();

        assert!(
            workspace_process_references(&proc_root, &path, current_uid().unwrap(), false, &[],)
                .unwrap()
        );
    }

    #[test]
    fn unrelated_missing_browser_tmpdir_does_not_block_gc() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (path, _) = exact_orphan(root.path());
        let process = fake_chromium_process(
            &proc_root,
            300_002,
            b"chrome\0",
            format!("TMPDIR={}/missing-browser-temp\0", root.path().display()).as_bytes(),
            None,
        );
        let cwd = root.path().join("sibling-cwd");
        fs::create_dir(&cwd).unwrap();
        fs::remove_file(process.join("cwd")).unwrap();
        symlink(&cwd, process.join("cwd")).unwrap();

        let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.skipped, 0);
        assert!(!path.exists());
    }

    #[test]
    fn tmpdir_path_proof_handles_symlinks_missing_tails_and_ambiguity() {
        let root = sandbox();
        let inside = root.path().join("inside");
        let outside = root.path().join("outside");
        fs::create_dir(&inside).unwrap();
        fs::create_dir(&outside).unwrap();
        let link = outside.join("link");
        symlink(&inside, &link).unwrap();
        let proof = |path: &Path| {
            environment_uses_workspace(
                format!("TMPDIR={}\0", path.display()).as_bytes(),
                || Ok(outside.clone()),
                &inside,
            )
        };
        assert!(!proof(&outside).unwrap());
        assert!(!proof(&outside.join("missing/tail")).unwrap());
        assert!(proof(&inside.join("missing/tail")).unwrap());
        assert!(proof(&link.join("missing")).unwrap());
        assert!(proof(Path::new("link/../inside/missing")).unwrap());
        assert!(proof(Path::new("../inside/missing")).unwrap());
        assert!(proof(&outside.join("missing/../inside")).is_err());
        symlink(outside.join("absent"), outside.join("dangling")).unwrap();
        assert!(proof(&outside.join("dangling")).is_err());
        symlink("loop", outside.join("loop")).unwrap();
        assert!(proof(&outside.join("loop")).is_err());
        fs::write(outside.join("file"), b"x").unwrap();
        assert!(proof(&outside.join("file/../inside")).is_err());
        let unreadable = outside.join("unreadable");
        fs::create_dir(&unreadable).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = proof(&unreadable.join("missing"));
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        if current_uid().unwrap() != 0 {
            assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
        let denied_cwd = || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "нет доступа к cwd",
            ))
        };
        assert!(
            !environment_uses_workspace(
                format!("TMPDIR={}\0", outside.display()).as_bytes(),
                denied_cwd,
                &inside
            )
            .unwrap()
        );
        assert!(environment_uses_workspace(b"TMPDIR=relative\0", denied_cwd, &inside).is_err());
    }

    #[test]
    fn unrelated_missing_tmpdir_allows_close_but_active_tree_and_unknown_paths_survive() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let mut free = TempWorkspace::create_under(root.path(), "close-free").unwrap();
        let held = TempWorkspace::create_under(root.path(), "close-held").unwrap();
        for run in [&free, &held] {
            fs::create_dir(run.path().join("t123")).unwrap();
        }
        let process = fake_chromium_process(
            &proc_root,
            300_003,
            b"chrome\0",
            format!("TMPDIR={}/missing\0", held.path().display()).as_bytes(),
            None,
        );
        // Абсолютному TMPDIR не нужен cwd, даже если каталог процесса удалён.
        fs::remove_file(process.join("cwd")).unwrap();
        symlink(root.path().join("gone (deleted)"), process.join("cwd")).unwrap();
        let free_path = free.path().to_path_buf();
        free.remove_with_proc(&proc_root).unwrap();
        free.closed = true;
        assert!(!free_path.exists());
        assert!(held.remove_with_proc(&proc_root).is_err());
        assert!(held.path().exists());
        fs::write(process.join("environ"), b"TMPDIR=relative\0").unwrap();
        assert!(held.remove_with_proc(&proc_root).is_err());
        fs::remove_dir_all(process).unwrap();
        assert!(
            !workspace_has_process_references(&proc_root, held.path(), current_uid().unwrap())
                .unwrap()
        );
        held.close().unwrap();
    }

    #[test]
    fn gc_collects_independent_tree_while_real_browser_tmpdir_holds_its_own_tree() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (held, _) = exact_orphan(root.path());
        let (free, _) = exact_orphan(root.path());
        let (unknown, _) = exact_orphan(root.path());
        let process = fake_chromium_process(
            &proc_root,
            300_004,
            b"chrome\0",
            format!("TMPDIR={}/missing\0", held.display()).as_bytes(),
            None,
        );
        let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(report.root, root.path());
        assert_eq!(report.removed, 2);
        assert!(held.exists());
        assert!(!free.exists());
        assert!(!unknown.exists());
        fs::remove_file(process.join("environ")).unwrap();
        fs::create_dir(process.join("environ")).unwrap();
        let (unknown, _) = exact_orphan(root.path());
        let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(report.removed, 0);
        assert!(unknown.exists());
        fs::remove_dir_all(process).unwrap();
        assert_eq!(
            cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO)
                .unwrap()
                .removed,
            2
        );
    }

    #[test]
    fn startup_gc_never_caches_reported_cleanup_errors_as_success() {
        let root = sandbox();
        let report = GcReport {
            root: root.path().to_path_buf(),
            errors: 1,
            ..GcReport::default()
        };
        let first = cleanup_configured_root_once_with(root.path(), || startup_gc_result(&report))
            .unwrap_err();
        let cached = cleanup_configured_root_once_with(root.path(), || Ok(())).unwrap_err();
        assert_eq!(first.to_string(), cached.to_string());
    }

    #[test]
    fn exact_gc_rechecks_process_references_immediately_before_removal() {
        let root = sandbox();
        let (proc_root, boot_id_path) = proc_fixture(root.path());
        let (path, _) = exact_orphan(root.path());
        let uid = current_uid().unwrap();
        let parent = namespace(root.path(), uid, false).unwrap().unwrap();
        let name = path.file_name().unwrap();
        let inspected = inspect_orphan(
            &parent,
            &ProcessSources {
                proc_root: &proc_root,
                boot_id_path: &boot_id_path,
            },
            name,
            &path,
            uid,
            unix_ms().unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        let Inspection::Delete(directory, _, reference_policy) = inspected else {
            panic!("ожидался осиротевший каталог, готовый к удалению");
        };
        assert!(matches!(
            reference_policy,
            ProcessReferencePolicy::AllSameUid
        ));

        let referenced_path = path.join("referenced-file");
        fs::write(&referenced_path, b"live process reference").unwrap();
        let process = fake_chromium_process(
            &proc_root,
            400_000,
            b"generic-process\0",
            b"TMPDIR=/tmp\0",
            Some(&referenced_path),
        );
        fs::write(process.join("comm"), b"generic\n").unwrap();

        assert!(matches!(
            remove_inspected_orphan(
                &parent,
                &proc_root,
                name,
                &path,
                &directory,
                uid,
                reference_policy,
            ),
            Err(DeleteFailure::Skip {
                reason: "временную рабочую область использует живой процесс",
                live: Some(true),
            })
        ));
        assert!(path.exists());

        fs::remove_dir_all(process).unwrap();
        drop(directory);
        assert_eq!(
            cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO)
                .unwrap()
                .removed,
            1
        );
        assert!(!path.exists());
    }

    #[test]
    fn missing_process_sources_do_not_prove_absence_of_references() {
        for source in ["comm", "cmdline", "environ", "fd", "cwd"] {
            let root = sandbox();
            let (proc_root, _) = proc_fixture(root.path());
            let (path, _) = exact_orphan(root.path());
            let process =
                fake_chromium_process(&proc_root, 300_000, b"chrome\0", b"TMPDIR=/tmp\0", None);
            if source == "fd" {
                fs::remove_dir(process.join(source)).unwrap();
            } else {
                fs::remove_file(process.join(source)).unwrap();
            }
            let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
            assert_eq!(report.removed, 0);
            assert!(path.exists());
        }
    }

    #[test]
    fn fresh_exact_orphan_tree_checks_preserve_unknown_entries_and_foreign_objects() {
        for unsafe_entry in [
            "browser-file",
            "browser-directory",
            "symlink",
            "socket",
            "writable",
        ] {
            let root = sandbox();
            let (proc_root, _) = proc_fixture(root.path());
            let (path, _) = exact_orphan(root.path());
            let foreign = root.path().join("foreign");
            fs::create_dir(&foreign).unwrap();
            fs::write(foreign.join("keep"), b"foreign").unwrap();
            let zip = root.path().join("user.zip");
            fs::write(&zip, b"user ZIP").unwrap();
            match unsafe_entry {
                "browser-file" | "browser-directory" => {
                    let temp = path.join("t123");
                    fs::create_dir(&temp).unwrap();
                    if unsafe_entry == "browser-file" {
                        fs::write(temp.join("unknown"), b"keep").unwrap();
                    } else {
                        fs::create_dir(temp.join("unknown")).unwrap();
                    }
                }
                "symlink" => symlink(&foreign, path.join("escape")).unwrap(),
                "socket" => {
                    drop(bind_socket_at(&path, "unknown-socket"));
                }
                "writable" => {
                    use std::os::unix::fs::PermissionsExt;
                    fs::write(path.join("writable"), b"keep").unwrap();
                    fs::set_permissions(path.join("writable"), fs::Permissions::from_mode(0o666))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(
                cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO)
                    .unwrap()
                    .removed,
                0,
                "{unsafe_entry}"
            );
            assert!(path.exists());
            assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"foreign");
            assert_eq!(fs::read(zip).unwrap(), b"user ZIP");
        }
    }

    #[test]
    fn fresh_exact_allowlisted_browser_orphan_preserves_unrelated_directory_and_zip() {
        let root = sandbox();
        let (proc_root, _) = proc_fixture(root.path());
        let (path, _) = exact_orphan(root.path());
        make_browser_temp_entries(&path);
        let foreign = root.path().join("unrelated");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep"), b"unrelated").unwrap();
        let zip = root.path().join("user.zip");
        fs::write(&zip, b"user ZIP").unwrap();
        let report =
            cleanup_under_with_proc(root.path(), &proc_root, DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!path.exists());
        assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"unrelated");
        assert_eq!(fs::read(zip).unwrap(), b"user ZIP");
    }

    #[test]
    fn schema_one_retains_ttl_and_rejects_schema_two_identity() {
        let root = sandbox();
        let (proc_root, boot_id) = proc_fixture(root.path());
        fs::remove_file(boot_id).unwrap(); // Схеме 1 не требуется новый источник.
        let fresh = make_orphan(root.path(), Duration::ZERO);
        let aged = make_orphan(root.path(), DEFAULT_ORPHAN_MIN_AGE * 2);
        let modified_fresh = make_orphan(root.path(), DEFAULT_ORPHAN_MIN_AGE * 2);
        set_old_directory_time(&modified_fresh, Duration::ZERO);
        let mixed = make_orphan(root.path(), DEFAULT_ORPHAN_MIN_AGE * 2);
        update_marker(&mixed, |marker| marker.boot_id = Some(TEST_BOOT_ID.into()));
        let report = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!aged.exists());
        for kept in [fresh, modified_fresh, mixed] {
            assert!(kept.exists());
        }
    }

    #[test]
    fn parallel_gc_never_deletes_active_exact_workspace() {
        let root = sandbox();
        let active = TempWorkspace::create_under(root.path(), "active-gc-unit").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..8 {
                        let report = cleanup_under(root.path(), Duration::ZERO).unwrap();
                        assert_eq!(report.removed, 0);
                        assert!(active.path().join(MARKER).exists());
                    }
                });
            }
        });
        active.close().unwrap();
    }

    fn random_hex() -> String {
        let mut random = [0_u8; 16];
        File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut random)
            .unwrap();
        crate::hashing::encode_lower_hex(random)
    }

    fn fake_chromium_process(
        proc_root: &Path,
        pid_suffix: u32,
        cmdline: &[u8],
        environment: &[u8],
        fd_target: Option<&Path>,
    ) -> PathBuf {
        let process = proc_root.join((std::process::id() + pid_suffix).to_string());
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::write(process.join("comm"), b"chrome\n").unwrap();
        fs::write(process.join("cmdline"), cmdline).unwrap();
        fs::write(process.join("environ"), environment).unwrap();
        symlink(Path::new("/"), process.join("cwd")).unwrap();
        if let Some(target) = fd_target {
            symlink(target, process.join("fd/7")).unwrap();
        }
        process
    }

    fn make_browser_temp_entries(run_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let profile = run_path.join(format!("browser-profile-{}", random_hex()));
        let legacy_profile = run_path.join(format!("browser-profile-{}", random_hex()));
        let temp = run_path.join(format!("t{}", &random_hex()[..3]));
        let suffix = &random_hex()[..6];
        let socket_directory = temp.join(format!("org.chromium.Chromium.{suffix}"));
        fs::create_dir(&profile).unwrap();
        fs::create_dir(&legacy_profile).unwrap();
        fs::create_dir(&temp).unwrap();
        fs::create_dir(&socket_directory).unwrap();
        fs::write(
            temp.join(format!(".org.chromium.Chromium.{suffix}")),
            b"deleted-open temp representation",
        )
        .unwrap();
        symlink("123456789", profile.join("SingletonCookie")).unwrap();
        symlink("host-name-12345", profile.join("SingletonLock")).unwrap();
        symlink("678901234", socket_directory.join("SingletonCookie")).unwrap();
        let socket = socket_directory.join("SingletonSocket");
        let listener = bind_socket_at(&socket_directory, "SingletonSocket");
        drop(listener);
        symlink(&socket, profile.join("SingletonSocket")).unwrap();
        let global_socket = Path::new(TEMP_ROOT)
            .join(format!("org.chromium.Chromium.{suffix}"))
            .join("SingletonSocket");
        symlink(global_socket, legacy_profile.join("SingletonSocket")).unwrap();
        (profile, temp, socket)
    }

    fn bind_socket_at(directory: &Path, name: &str) -> UnixListener {
        let descriptor = File::open(directory).unwrap();
        UnixListener::bind(format!("/proc/self/fd/{}/{}", descriptor.as_raw_fd(), name)).unwrap()
    }

    fn set_old_directory_time(path: &Path, age: Duration) {
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
    }

    #[test]
    fn scope_error_and_unwind_remove_owned_tree() {
        let path = {
            let owner = sandbox();
            fs::write(owner.path().join("payload"), b"data").unwrap();
            owner.path().to_path_buf()
        };
        assert!(!path.exists());
        fn fail_early(path: &mut Option<PathBuf>) -> io::Result<()> {
            let owner = sandbox();
            *path = Some(owner.path().to_path_buf());
            Err(io::Error::other("early error"))
        }
        let mut early_path = None;
        let result = fail_early(&mut early_path);
        assert!(result.is_err());
        assert!(!early_path.unwrap().exists());
        let mut panic_path = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let owner = sandbox();
            panic_path = Some(owner.path().to_path_buf());
            panic!("unwind");
        }));
        assert!(result.is_err());
        assert!(!panic_path.unwrap().exists());
    }

    #[test]
    fn explicit_close_and_parallel_creation() {
        let owners = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8).map(|_| scope.spawn(sandbox)).collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        let paths: std::collections::BTreeSet<_> = owners
            .iter()
            .map(|owner| owner.path().to_path_buf())
            .collect();
        assert_eq!(paths.len(), owners.len());
        for owner in owners {
            owner.close().unwrap();
        }
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn explicit_close_reports_replaced_directory_and_preserves_replacement() {
        let owner = sandbox();
        let path = owner.path().to_path_buf();
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("foreign"), b"preserve").unwrap();
        assert!(owner.close().is_err());
        assert!(path.join("foreign").exists());
        fs::remove_dir_all(path).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn old_orphan_removed_and_gc_is_idempotent() {
        let root = sandbox();
        let path = make_orphan(root.path(), Duration::from_secs(172800));
        fs::write(path.join("data"), b"payload").unwrap();
        File::open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let first = cleanup_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(first.removed, 1);
        assert!(first.removed_bytes >= 7);
        assert!(!path.exists());
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
    }

    #[test]
    fn orphan_gc_dry_run_reports_stale_tree_without_mutating_it() {
        let root = sandbox();
        let path = make_orphan(root.path(), Duration::from_secs(172800));
        fs::write(path.join("data"), b"payload").unwrap();
        File::open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();

        let report = plan_orphans_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();

        assert_eq!(report.removed, 0);
        assert!(path.join("data").exists());
        let candidate = report
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert_eq!(candidate.outcome, "delete");
        assert_eq!(candidate.ownership, "project-owned");
        assert_eq!(candidate.live, Some(false));
        assert!(candidate.bytes > 0);
    }

    #[test]
    fn orphan_gc_removes_only_allowlisted_browser_temp_objects() {
        let root = sandbox();
        let orphan = make_orphan(root.path(), Duration::from_secs(172800));
        let (_profile, _temp, _socket) = make_browser_temp_entries(&orphan);
        let unrelated = root
            .path()
            .join(format!("org.chromium.Chromium.{}", &random_hex()[..6]));
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("keep"), b"unrelated global object").unwrap();
        set_old_directory_time(&orphan, Duration::from_secs(172800));

        let report = cleanup_under(root.path(), Duration::ZERO).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!orphan.exists());
        assert_eq!(
            fs::read(unrelated.join("keep")).unwrap(),
            b"unrelated global object"
        );
        root.close().unwrap();
    }

    #[test]
    fn orphan_gc_waits_for_live_browser_references_then_collects() {
        let root = sandbox();
        let proc_root = root.path().join("proc-fixture");
        fs::create_dir(&proc_root).unwrap();
        let orphan = make_orphan(root.path(), Duration::from_secs(172800));
        let profile = orphan.join(format!("browser-profile-{}", random_hex()));
        fs::create_dir(&profile).unwrap();
        let candidate = fake_chromium_process(
            &proc_root,
            100_001,
            format!("/usr/bin/chrome\0--user-data-dir={}\0", profile.display()).as_bytes(),
            b"TMPDIR=/tmp\0",
            None,
        );
        set_old_directory_time(&orphan, Duration::from_secs(172800));

        let active = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(active.removed, 0);
        assert_eq!(active.skipped, 1);
        assert!(orphan.exists());

        fs::remove_dir_all(candidate).unwrap();
        let closed = cleanup_under_with_proc(root.path(), &proc_root, Duration::ZERO).unwrap();
        assert_eq!(closed.removed, 1);
        assert!(!orphan.exists());
        root.close().unwrap();
    }

    #[test]
    fn browser_process_environment_and_open_fds_are_workspace_references() {
        let run = sandbox();
        let proc_root = sandbox();
        let temp = run.path().join(format!("t{}", &random_hex()[..3]));
        fs::create_dir(&temp).unwrap();
        let env_process = fake_chromium_process(
            proc_root.path(),
            100_002,
            b"/usr/bin/chrome\0--type=renderer\0",
            format!("TMPDIR={}\0", temp.display()).as_bytes(),
            None,
        );
        assert!(
            workspace_has_process_references(proc_root.path(), run.path(), current_uid().unwrap())
                .unwrap()
        );
        fs::remove_dir_all(env_process).unwrap();

        let referenced_file = temp.join("open-file");
        fs::write(&referenced_file, b"held").unwrap();
        let fd_process = fake_chromium_process(
            proc_root.path(),
            100_003,
            b"/usr/bin/chrome\0--type=renderer\0",
            b"TMPDIR=/tmp\0",
            Some(&referenced_file),
        );
        assert!(
            workspace_has_process_references(proc_root.path(), run.path(), current_uid().unwrap())
                .unwrap()
        );
        fs::remove_dir_all(fd_process).unwrap();
        proc_root.close().unwrap();
        run.close().unwrap();
    }

    #[test]
    fn browser_process_relative_tmpdir_is_resolved_from_process_cwd() {
        let run = sandbox();
        let proc_root = sandbox();
        let temp = run.path().join(format!("t{}", &random_hex()[..3]));
        fs::create_dir(&temp).unwrap();
        let relative_temp = temp.strip_prefix(run.path()).unwrap();
        let process = fake_chromium_process(
            proc_root.path(),
            100_005,
            b"/usr/bin/chrome\0--type=renderer\0",
            format!("TMPDIR={}\0", relative_temp.display()).as_bytes(),
            None,
        );
        fs::remove_file(process.join("cwd")).unwrap();
        symlink(run.path(), process.join("cwd")).unwrap();

        assert!(
            workspace_has_process_references(proc_root.path(), run.path(), current_uid().unwrap())
                .unwrap()
        );

        fs::remove_dir_all(process).unwrap();
        proc_root.close().unwrap();
        run.close().unwrap();
    }

    #[test]
    fn inaccessible_browser_candidate_fails_closed() {
        let run = sandbox();
        let proc_root = sandbox();
        let process = proc_root
            .path()
            .join((std::process::id() + 100_004).to_string());
        fs::create_dir_all(process.join("cmdline")).unwrap();
        fs::write(process.join("comm"), b"chrome\n").unwrap();
        assert!(
            workspace_has_process_references(proc_root.path(), run.path(), current_uid().unwrap())
                .is_err()
        );
        proc_root.close().unwrap();
        run.close().unwrap();
    }

    #[test]
    fn orphan_gc_rejects_unrecognized_special_files() {
        let root = sandbox();
        let orphan = make_orphan(root.path(), Duration::from_secs(172800));
        let socket = orphan.join("unrelated-socket");
        let listener = bind_socket_at(&orphan, "unrelated-socket");
        drop(listener);
        set_old_directory_time(&orphan, Duration::from_secs(172800));
        let report = cleanup_under(root.path(), Duration::ZERO).unwrap();
        assert_eq!(report.removed, 0);
        assert!(orphan.exists());
        fs::remove_file(socket).unwrap();
        root.close().unwrap();
    }

    #[test]
    fn fresh_current_live_foreign_marker_and_missing_marker_are_skipped() {
        let root = sandbox();
        let current = TempWorkspace::create_under(root.path(), "current").unwrap();
        let fresh = make_orphan(root.path(), Duration::from_secs(1));
        let live = make_orphan(root.path(), Duration::from_secs(172800));
        let live_marker_path = live.join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&live_marker_path).unwrap()).unwrap();
        marker.pid = std::process::id();
        fs::write(&live_marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        let missing = make_orphan(root.path(), Duration::from_secs(172800));
        fs::remove_file(missing.join(MARKER)).unwrap();
        let foreign = make_orphan(root.path(), Duration::from_secs(172800));
        let foreign_marker_path = foreign.join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&foreign_marker_path).unwrap()).unwrap();
        marker.repository = "foreign".into();
        fs::write(foreign_marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        let foreign_prefix = root
            .path()
            .join(namespace_directory_name(current_uid().unwrap()))
            .join("other-application");
        fs::create_dir(&foreign_prefix).unwrap();
        let report = cleanup_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, 6);
        for path in [
            current.path(),
            &fresh,
            &live,
            &missing,
            &foreign,
            &foreign_prefix,
        ] {
            assert!(path.exists());
        }
    }

    #[test]
    fn marker_schema_and_run_id_cannot_escape_root() {
        for (schema, run_id) in [(SCHEMA + 1, "invalid"), (SCHEMA, "../escape")] {
            let root = sandbox();
            let path = make_orphan(root.path(), Duration::from_secs(172800));
            let marker_path = path.join(MARKER);
            let mut marker: OwnershipMarker =
                serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
            marker.schema = schema;
            marker.run_id = run_id.into();
            fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
            assert_eq!(
                cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
                0
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn symlinks_and_namespace_escape_never_delete_targets() {
        let root = sandbox();
        let target = sandbox();
        fs::write(target.path().join("keep"), b"keep").unwrap();
        let path = make_orphan(root.path(), Duration::from_secs(172800));
        symlink(target.path(), path.join("escape")).unwrap();
        let namespace_name = namespace_directory_name(current_uid().unwrap());
        let link = root
            .path()
            .join(&namespace_name)
            .join("run-11111111111111111111111111111111");
        symlink(target.path(), &link).unwrap();
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
        assert!(target.path().join("keep").exists());
        let escaped = sandbox();
        symlink(target.path(), escaped.path().join(namespace_name)).unwrap();
        assert!(cleanup_under(escaped.path(), Duration::ZERO).is_err());
        assert!(target.path().join("keep").exists());
    }

    #[test]
    fn legacy_parser_accepts_only_audited_basename_forms() {
        for name in [
            "anki-manifest-free-4294967295-ThreadId(17)",
            "anki-manifest-symlink-conflict-outside-4294967295-ThreadId(1)",
            "asset-store-pitch-cli-4294967295-9",
            "asset-store-pitch-batch-4294967295-9",
            "jpdb-live-acceptance-4294967295-1777777777777777777",
            "yarxi-live-acceptance-4294967295-1777777777777777777",
            "anki-repo-unit-4294967295-loader-bytes-0",
            "anki-repo-test-4294967295-cli-usage-0",
            "asset-store-generic-batch-4294967295-0",
            "anki-kanji-batch-4294967295-0",
            "kanji-batch-cli-4294967295-0",
            "kanji-assets-source-boundary-4294967295-0",
            "asset-store-unit-4294967295-0",
            "asset-trust-4294967295-0",
            "kanji-assets-cli-4294967295-0",
            "pitch-cli-contract-4294967295-0",
            "asset-store-diagnostics-4294967295-1777777777777777777-0",
            "asset-store-contract-4294967295-reopen-ingest-0",
            "domain-semantics-trust-automated-only-4294967295-0",
            "pitch-corpus-trust-gate-v4-approved-4294967295-0",
            "jpdb-session-stop-test-4294967295-1777777777777777777",
            "yarxi-acceptance-evidence-test-4294967295-1777777777777777777",
            "kanji-0123456789abcdef0123456789abcdef",
        ] {
            assert!(legacy_pid(name).is_some(), "{name}");
        }
        for name in [
            "foreign-app-1-2",
            "anki-manifest-custom-1-ThreadId(1)",
            "anki-manifest-free-0-ThreadId(1)",
            "anki-manifest-free-1-ThreadId(1)/escape",
            "asset-store-pitch-cli-1-2-more",
            "asset-store-pitch-batch-a-1",
            "kanji-0123456789ABCDEF0123456789ABCDEF",
            "kanji-0123456789abcdef0123456789abcde",
            "anki-repo-unit-1-../escape-0",
            "anki-repo-unit-1--0",
            "anki-repo-test-01-label-0",
            "asset-store-generic-batch-1-label-0",
            "asset-store-diagnostics-1-42-0-extra",
            "asset-store-contract-1-bad.label-0",
            "domain-semantics-label-0-0",
            "pitch-corpus-trust-label-1-18446744073709551616",
            "jpdb-session-stop-test-1-42-extra",
            "jpdb-live-acceptance-1-../escape",
        ] {
            assert!(legacy_pid(name).is_none(), "{name}");
        }
    }

    #[test]
    fn legacy_gc_skips_foreign_fresh_live_and_symlinks_and_is_idempotent() {
        let root = sandbox();
        let target = sandbox();
        fs::write(target.path().join("keep"), b"keep").unwrap();
        let old = root.path().join("asset-store-pitch-cli-4294967295-1");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("data"), b"old payload").unwrap();
        File::open(&old)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let fresh = root.path().join("asset-store-pitch-batch-4294967295-1");
        fs::create_dir(&fresh).unwrap();
        let live = root
            .path()
            .join(format!("jpdb-live-acceptance-{}-17", std::process::id()));
        fs::create_dir(&live).unwrap();
        File::open(&live)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let foreign = root.path().join("foreign-app-4294967295-1");
        fs::create_dir(&foreign).unwrap();
        let link = root.path().join("yarxi-live-acceptance-4294967295-1");
        symlink(target.path(), &link).unwrap();
        let inner_link = root.path().join("kanji-0123456789abcdef0123456789abcdef");
        fs::create_dir(&inner_link).unwrap();
        symlink(target.path(), inner_link.join("escape")).unwrap();
        File::open(&inner_link)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let report = cleanup_legacy_under(root.path(), DEFAULT_ORPHAN_MIN_AGE, true).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.removed_bytes, 11);
        assert_eq!(report.skipped, 4);
        let removed = report
            .entries
            .iter()
            .find(|entry| entry.outcome == "removed")
            .unwrap();
        assert_eq!(removed.ownership, "legacy-unverified");
        assert_eq!(removed.live, Some(false));
        assert!(!old.exists());
        assert!(target.path().join("keep").exists());
        for path in [&fresh, &live, &foreign, &link, &inner_link] {
            assert!(path.exists());
        }
        assert_eq!(
            cleanup_legacy_under(root.path(), DEFAULT_ORPHAN_MIN_AGE, true)
                .unwrap()
                .removed,
            0
        );
    }

    #[test]
    fn legacy_name_without_pid_keeps_liveness_unknown_in_plan_and_apply() {
        let root = sandbox();
        let path = root.path().join("kanji-0123456789abcdef0123456789abcdef");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("data"), b"legacy data").unwrap();
        File::open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();

        let plan = plan_legacy_orphans_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        let planned = plan
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert_eq!(planned.outcome, "delete");
        assert_eq!(planned.live, None);
        assert!(planned.reason.contains("без PID"));

        let applied = cleanup_legacy_orphans_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        let removed = applied
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert_eq!(removed.outcome, "removed");
        assert_eq!(removed.live, None);
        assert!(removed.reason.contains("без PID"));
        assert!(!path.exists());
    }

    #[test]
    fn marker_symlink_and_writable_entries_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        let target = sandbox();
        let marker_link = make_orphan(root.path(), Duration::from_secs(172800));
        let marker_bytes = fs::read(marker_link.join(MARKER)).unwrap();
        let outside = target.path().join("marker");
        fs::write(&outside, &marker_bytes).unwrap();
        fs::remove_file(marker_link.join(MARKER)).unwrap();
        symlink(&outside, marker_link.join(MARKER)).unwrap();
        let writable = make_orphan(root.path(), Duration::from_secs(172800));
        fs::write(writable.join("group-writable"), b"preserve").unwrap();
        fs::set_permissions(
            writable.join("group-writable"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
        assert_eq!(fs::read(&outside).unwrap(), marker_bytes);
        assert!(writable.join("group-writable").exists());
        let before = snapshot_under(root.path()).unwrap();
        assert_eq!(before.directories, 2);
        assert!(before.bytes >= 8);
    }

    #[test]
    fn snapshot_tolerates_concurrently_removed_workspaces() {
        let root = sandbox();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..64 {
                    let owner =
                        TempWorkspace::create_under(root.path(), "parallel-snapshot").unwrap();
                    fs::create_dir(owner.path().join("nested")).unwrap();
                    fs::write(owner.path().join("nested/data"), b"payload").unwrap();
                    std::thread::yield_now();
                    owner.close().unwrap();
                }
            });
            for _ in 0..64 {
                snapshot_under(root.path()).unwrap();
                std::thread::yield_now();
            }
        });
        assert_eq!(snapshot_under(root.path()).unwrap().directories, 0);
    }
}
