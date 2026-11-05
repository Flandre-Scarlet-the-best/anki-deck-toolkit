//! Транзакционная замена файлов распакованного расширения в его каталоге.
//!
//! Каталог установки не переименовывается и не пересоздаётся: строка его
//! пути определяет ID расширения, а браузер держит его загруженным. Замена идёт
//! пофайлово под журналом. Все пути к файлам разрешаются относительно открытых
//! дескрипторов каталогов с NOFOLLOW, поэтому symlink не может перенаправить
//! staging, замену или восстановление за пределы управляемого дерева.
//!
//! Имена временных файлов сначала резервируются через create_new, затем журнал
//! связывает их с транзакцией до записи содержимого. Staging не меняет файлы
//! установки; при восстановлении его временные файлы сохраняются, поскольку
//! содержимое журнала не доказывает владение ими. Смешанный swapping из
//! публичного recovery безопасно блокируется; установщик может откатить свою
//! транзакцию в том же процессе, пока у него есть контекст владения. Committed
//! проверяет целевые файлы и сохраняет временные копии. Без журнала файлы с
//! временными суффиксами также не удаляются.
use super::bridge_error;
use super::package::{self, PackageFile, write_order};
use crate::AssetError;
use crate::hashing::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

pub const JOURNAL: &str = ".toolkit-install-journal.json";
const NEW_SUFFIX: &str = ".update-new";
const OLD_SUFFIX: &str = ".update-old";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const MAX_VERSION_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Staging,
    Swapping,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    new_sha256: String,
    /// null — файла до установки не было.
    old_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    phase: Phase,
    from_version: Option<String>,
    to_version: Option<String>,
    entries: Vec<Entry>,
    /// Старые журналы без идентификатора используют прежние детерминированные
    /// имена. Новые журналы связывают временные файлы со своей транзакцией.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
}

/// Шаг транзакции для внедрения отказов в тестах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Staged(usize),
    Swapped(usize),
    Verified,
}

/// Реакция внедрённого отказа: ошибка ввода-вывода или немедленный выход
/// без какой-либо уборки.
#[derive(Debug)]
pub enum Fault {
    Io(std::io::Error),
    Crash,
}

fn io_error(context: &str, error: std::io::Error) -> AssetError {
    let mut e = AssetError::io(context.to_owned(), error);
    e.details["code"] = json!("install_io_failed");
    e
}

fn unique_id() -> std::io::Result<String> {
    use aws_lc_rs::rand::{SecureRandom, SystemRandom};

    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| std::io::Error::other("secure random generator failed"))?;
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").expect("writing to String");
    }
    Ok(id)
}

fn valid_transaction_id(id: &str) -> bool {
    (1..=80).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_entry_path(path: &str) -> bool {
    path == package::PAIRING_FILE || package::valid_package_path(path)
}

fn validate_journal(journal: &Journal) -> Result<(), &'static str> {
    if journal.schema_version != 1
        || journal.entries.is_empty()
        || journal.entries.len() > package::MAX_FILES
        || journal
            .transaction_id
            .as_deref()
            .is_some_and(|id| !valid_transaction_id(id))
        || journal.from_version.as_deref().is_some_and(|version| {
            version.is_empty()
                || version.len() > MAX_VERSION_BYTES
                || !version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        })
        || journal.to_version.as_deref().is_some_and(|version| {
            version.is_empty()
                || version.len() > MAX_VERSION_BYTES
                || !version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        })
    {
        return Err("journal_header_invalid");
    }

    let mut seen = std::collections::BTreeSet::new();
    for entry in &journal.entries {
        if !valid_entry_path(&entry.path)
            || !seen.insert(&entry.path)
            || !valid_sha256(&entry.new_sha256)
            || entry
                .old_sha256
                .as_deref()
                .is_some_and(|hash| !valid_sha256(hash))
        {
            return Err("journal_entry_invalid");
        }
    }
    Ok(())
}

fn open_dir_at(parent: &File, name: &str) -> std::io::Result<File> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

fn mkdir_at(parent: &File, name: &str) -> std::io::Result<()> {
    rustix::fs::mkdirat(parent, name, rustix::fs::Mode::from_raw_mode(0o700))
        .map_err(std::io::Error::from)
}

/// Открывает корневой каталог покомпонентно, не следуя symlink ни в одном
/// компоненте пути. При create=true создаёт только отсутствующие каталоги.
struct InstallRoot {
    directory: File,
}

struct Reservation {
    new_path: String,
    new_file: File,
    old_path: Option<String>,
    old_file: Option<File>,
}

impl InstallRoot {
    fn open(path: &Path, create: bool) -> std::io::Result<Option<Self>> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "parent component in install root",
            ));
        }

        // Каталог расширения может быть задан через прежний symlink-alias:
        // Anki хранит буквальный путь, поэтому сохраняем его в state, а для
        // операций разрешаем alias один раз и дальше работаем только через
        // дескриптор корня. `canonicalize` здесь не является защитой подмены:
        // после него каждый компонент открывается с NOFOLLOW.
        let mut unresolved = absolute;
        let mut missing = Vec::new();
        let absolute = loop {
            match std::fs::canonicalize(&unresolved) {
                Ok(mut resolved) => {
                    for component in missing.into_iter().rev() {
                        resolved.push(component);
                    }
                    break resolved;
                }
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let name = unresolved.file_name().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "missing install root component",
                        )
                    })?;
                    missing.push(name.to_os_string());
                    if !unresolved.pop() {
                        return Err(error);
                    }
                }
                Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        };
        let mut current = File::open("/")?;
        for component in absolute.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name.to_str().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF8 path")
                })?,
                Component::ParentDir => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "parent component in install root",
                    ));
                }
                Component::Prefix(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "unsupported install root prefix",
                    ));
                }
            };
            match open_dir_at(&current, name) {
                Ok(next) => current = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    match mkdir_at(&current, name) {
                        Ok(()) => {}
                        Err(create_error)
                            if create_error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(create_error) => return Err(create_error),
                    }
                    current = open_dir_at(&current, name)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Ok(Some(Self { directory: current }))
    }

    fn parent(&self, path: &str, create: bool) -> std::io::Result<Option<(File, String)>> {
        let components: Vec<&str> = path.split('/').collect();
        if !(1..=2).contains(&components.len())
            || components.iter().any(|component| {
                component.is_empty()
                    || *component == "."
                    || *component == ".."
                    || component.len() > 180
                    || !component.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || b.is_ascii_uppercase()
                            || b.is_ascii_digit()
                            || matches!(b, b'.' | b'_' | b'-')
                    })
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe install path",
            ));
        }
        let mut parent = self.directory.try_clone()?;
        if components.len() == 2 {
            match open_dir_at(&parent, components[0]) {
                Ok(next) => parent = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    match mkdir_at(&parent, components[0]) {
                        Ok(()) => {}
                        Err(create_error)
                            if create_error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(create_error) => return Err(create_error),
                    }
                    parent = open_dir_at(&parent, components[0])?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Ok(Some((parent, components[components.len() - 1].to_owned())))
    }

    fn read_file(&self, path: &str) -> std::io::Result<Option<Vec<u8>>> {
        let Some((parent, leaf)) = self.parent(path, false)? else {
            return Ok(None);
        };
        read_regular_at(&parent, &leaf, package::MAX_FILE_BYTES)
    }

    fn write_new(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        let (parent, leaf) = self
            .parent(path, true)?
            .ok_or_else(|| std::io::Error::other("install parent disappeared"))?;
        let descriptor = rustix::fs::openat(
            &parent,
            leaf.as_str(),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;
        let mut file = File::from(descriptor);
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        if result.is_err() {
            drop(file);
            let _ = rustix::fs::unlinkat(&parent, leaf.as_str(), rustix::fs::AtFlags::empty());
        }
        result
    }

    fn reserve_file(&self, path: &str) -> std::io::Result<File> {
        let (parent, leaf) = self
            .parent(path, true)?
            .ok_or_else(|| std::io::Error::other("install parent disappeared"))?;
        rustix::fs::openat(
            &parent,
            leaf.as_str(),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map(File::from)
        .map_err(std::io::Error::from)
    }

    fn remove(&self, path: &str) -> std::io::Result<()> {
        let Some((parent, leaf)) = self.parent(path, false)? else {
            return Ok(());
        };
        match rustix::fs::unlinkat(&parent, leaf.as_str(), rustix::fs::AtFlags::empty()) {
            Ok(()) => Ok(()),
            Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn rename_sibling(&self, from: &str, to: &str) -> std::io::Result<()> {
        let (from_parent, from_leaf) = self
            .parent(from, false)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "source parent"))?;
        let (to_parent, to_leaf) = self
            .parent(to, false)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "target parent"))?;
        let from_parent_meta = from_parent.metadata()?;
        let to_parent_meta = to_parent.metadata()?;
        use std::os::unix::fs::MetadataExt;
        if from_parent_meta.dev() != to_parent_meta.dev()
            || from_parent_meta.ino() != to_parent_meta.ino()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "rename parents differ",
            ));
        }
        rustix::fs::renameat(&from_parent, from_leaf, &to_parent, to_leaf)
            .map_err(std::io::Error::from)
    }

    fn read_journal(&self) -> Result<Option<Journal>, AssetError> {
        let bytes = match read_regular_at(&self.directory, JOURNAL, MAX_JOURNAL_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => {
                let reason = error.kind().to_string();
                let mut failure = bridge_error(
                    "install_journal_corrupt",
                    "Не удалось безопасно прочитать журнал установки; восстановление остановлено",
                );
                failure.details["reason"] = json!(reason);
                return Err(failure);
            }
        };
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|_| {
            bridge_error(
                "install_journal_corrupt",
                "Журнал установки расширения повреждён; восстановите каталог расширения командой setup",
            )
        })?;
        validate_journal(&journal).map_err(|reason| {
            let mut error = bridge_error(
                "install_journal_corrupt",
                "Журнал установки расширения содержит недопустимые записи; восстановление остановлено",
            );
            error.details["reason"] = json!(reason);
            error
        })?;
        Ok(Some(journal))
    }

    fn write_journal(&self, journal: &Journal) -> std::io::Result<()> {
        let temporary = format!("{JOURNAL}{NEW_SUFFIX}-{}", unique_id()?);
        let bytes = serde_json::to_vec_pretty(journal).expect("журнал");
        self.write_new(&temporary, &bytes)?;
        self.rename_sibling(&temporary, JOURNAL)?;
        sync_file(&self.directory);
        Ok(())
    }

    fn sha256(&self, path: &str) -> std::io::Result<Option<String>> {
        self.read_file(path).map(|bytes| bytes.map(sha256_hex))
    }

    fn remove_if_hash(&self, path: &str, expected: &str) -> std::io::Result<()> {
        match self.sha256(path)? {
            Some(actual) if actual == expected => self.remove(path),
            None => Ok(()),
            Some(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "temporary file hash mismatch",
            )),
        }
    }
}

fn read_regular_at(parent: &File, leaf: &str, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let descriptor = match rustix::fs::openat(
        parent,
        leaf,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let file = File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a bounded regular install file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "install file too large",
        ));
    }
    Ok(Some(bytes))
}

fn sync_file(directory: &File) {
    let _ = directory.sync_all();
}

fn write_reserved(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

fn cleanup_reservations(root: &InstallRoot, reservations: Vec<Reservation>) -> std::io::Result<()> {
    for reservation in reservations {
        drop(reservation.new_file);
        if let Some(file) = reservation.old_file {
            drop(file);
        }
        root.remove(&reservation.new_path)?;
        if let Some(path) = reservation.old_path {
            root.remove(&path)?;
        }
    }
    Ok(())
}

fn verify_staged(root: &InstallRoot, journal: &Journal) -> std::io::Result<()> {
    for entry in &journal.entries {
        let new_temp = temp_path(&entry.path, NEW_SUFFIX, journal.transaction_id.as_deref());
        if root.sha256(&new_temp)?.as_deref() != Some(entry.new_sha256.as_str()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "staged new file hash mismatch",
            ));
        }
        if root.sha256(&entry.path)? != entry.old_sha256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "install target changed during staging",
            ));
        }
        if let Some(expected_old) = entry.old_sha256.as_deref() {
            let old_temp = temp_path(&entry.path, OLD_SUFFIX, journal.transaction_id.as_deref());
            if root.sha256(&old_temp)?.as_deref() != Some(expected_old) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "staged backup hash mismatch",
                ));
            }
        }
    }
    Ok(())
}

fn temp_path(path: &str, suffix: &str, transaction_id: Option<&str>) -> String {
    match transaction_id {
        Some(id) => format!("{path}{suffix}-{id}"),
        None => format!("{path}{suffix}"),
    }
}

fn recovery_failed(reason: &str, path: Option<&str>) -> AssetError {
    let mut error = bridge_error(
        "install_recovery_failed",
        "Безопасное восстановление каталога расширения не завершено; журнал и оставшиеся файлы сохранены. Исправьте причину и повторите команду или setup",
    );
    if reason == "swapping_recovery_requires_manual_repair" {
        error.message = "После прерывания замены каталог содержит файлы разных версий. Журнал и временные файлы сохранены; автоматическая замена заблокирована до ручного восстановления".into();
        error.details["status"] = json!("MANUAL_REPAIR_REQUIRED");
        error.details["user_action"] = json!(
            "Сохраните копию всего каталога расширения вместе с журналом и временными файлами. Вручную восстановите все файлы из одного полного доверенного выпуска, затем переместите .toolkit-install-journal.json из каталога расширения и выполните doctor. Не запускайте setup или update до восстановления целого выпуска."
        );
    }
    error.details["reason"] = json!(reason);
    if let Some(path) = path {
        error.details["path"] = json!(path);
    }
    error
}

/// Проверяет все файлы и резервные копии до первой операции восстановления.
fn preflight_recovery(root: &InstallRoot, journal: &Journal) -> Result<(), AssetError> {
    let transaction_id = journal.transaction_id.as_deref();
    for entry in &journal.entries {
        let new_temp = temp_path(&entry.path, NEW_SUFFIX, transaction_id);
        let old_temp = temp_path(&entry.path, OLD_SUFFIX, transaction_id);
        let current = root
            .sha256(&entry.path)
            .map_err(|_| recovery_failed("path_or_file_invalid", Some(&entry.path)))?;
        let staged = root
            .sha256(&new_temp)
            .map_err(|_| recovery_failed("new_temp_invalid", Some(&entry.path)))?;
        let backup = root
            .sha256(&old_temp)
            .map_err(|_| recovery_failed("backup_invalid", Some(&entry.path)))?;

        match journal.phase {
            Phase::Staging => {
                if current != entry.old_sha256 {
                    return Err(recovery_failed("staging_target_changed", Some(&entry.path)));
                }
                if staged
                    .as_deref()
                    .is_some_and(|hash| hash != entry.new_sha256)
                    && transaction_id.is_none()
                {
                    return Err(recovery_failed("new_temp_sha_mismatch", Some(&entry.path)));
                }
                if let (Some(actual), Some(expected)) =
                    (backup.as_deref(), entry.old_sha256.as_deref())
                    && actual != expected
                    && transaction_id.is_none()
                {
                    return Err(recovery_failed("old_temp_sha_mismatch", Some(&entry.path)));
                }
                if backup.is_some() && entry.old_sha256.is_none() {
                    return Err(recovery_failed("unexpected_backup", Some(&entry.path)));
                }
            }
            Phase::Swapping => {
                if staged
                    .as_deref()
                    .is_some_and(|hash| hash != entry.new_sha256)
                {
                    return Err(recovery_failed("new_temp_sha_mismatch", Some(&entry.path)));
                }
                match entry.old_sha256.as_deref() {
                    Some(expected_old) => {
                        if let Some(actual_backup) = backup.as_deref() {
                            if actual_backup != expected_old {
                                return Err(recovery_failed("old_sha_mismatch", Some(&entry.path)));
                            }
                        } else if current.as_deref() != Some(expected_old) {
                            return Err(recovery_failed("backup_missing", Some(&entry.path)));
                        }
                        if current
                            .as_deref()
                            .is_some_and(|hash| hash != expected_old && hash != entry.new_sha256)
                        {
                            return Err(recovery_failed("target_sha_mismatch", Some(&entry.path)));
                        }
                    }
                    None => {
                        if backup.is_some() {
                            return Err(recovery_failed("unexpected_backup", Some(&entry.path)));
                        }
                        if current
                            .as_deref()
                            .is_some_and(|hash| hash != entry.new_sha256)
                        {
                            return Err(recovery_failed("target_sha_mismatch", Some(&entry.path)));
                        }
                    }
                }
            }
            Phase::Committed => {
                if current.as_deref() != Some(entry.new_sha256.as_str()) {
                    return Err(recovery_failed(
                        "committed_target_sha_mismatch",
                        Some(&entry.path),
                    ));
                }
                if let (Some(actual_backup), Some(expected_old)) =
                    (backup.as_deref(), entry.old_sha256.as_deref())
                    && actual_backup != expected_old
                {
                    return Err(recovery_failed("old_sha_mismatch", Some(&entry.path)));
                }
                if backup.is_some() && entry.old_sha256.is_none() {
                    return Err(recovery_failed("unexpected_backup", Some(&entry.path)));
                }
                if staged
                    .as_deref()
                    .is_some_and(|hash| hash != entry.new_sha256)
                {
                    return Err(recovery_failed("new_temp_sha_mismatch", Some(&entry.path)));
                }
            }
        }
    }
    Ok(())
}

/// Проверяет каталог после прерванной установки. Staging отбрасывается без
/// изменения целей; полностью установленный Swapping/Committed подтверждается.
/// Смешанный Swapping блокируется, так как один локальный журнал не доказывает
/// владение резервными копиями. None — восстанавливать было нечего.
pub fn recover(dir: &Path) -> Result<Option<Value>, AssetError> {
    let Some(root) =
        InstallRoot::open(dir, false).map_err(|error| io_error("каталог расширения", error))?
    else {
        return Ok(None);
    };
    let Some(journal) = root.read_journal()? else {
        // Без журнала невозможно доказать владение файлами с временными
        // суффиксами. Оставляем их для диагностики и не трогаем.
        return Ok(None);
    };

    recover_loaded(&root, journal, false)
}

/// Восстанавливает проверенный журнал. `owned_transaction` разрешён только
/// вызывающему установщику, который создал temp-файлы в этом же процессе и
/// удерживает их исходный контекст; публичный recovery читает недоверенный JSON.
fn recover_loaded(
    root: &InstallRoot,
    journal: Journal,
    owned_transaction: bool,
) -> Result<Option<Value>, AssetError> {
    preflight_recovery(root, &journal)?;
    let transaction_id = journal.transaction_id.as_deref();
    let outcome = match journal.phase {
        Phase::Staging => {
            // Фаза staging не меняла установленные файлы. Даже SHA из
            // журнала нельзя использовать как подтверждение владения: сам
            // журнал локален и может быть подменён. Оставляем все temp-файлы.
            "discarded_staging"
        }
        Phase::Swapping => {
            if !owned_transaction {
                let mut fully_swapped = true;
                for entry in &journal.entries {
                    let current = root
                        .sha256(&entry.path)
                        .map_err(|_| recovery_failed("path_or_file_invalid", Some(&entry.path)))?;
                    if current.as_deref() != Some(entry.new_sha256.as_str()) {
                        fully_swapped = false;
                    }
                }
                if !fully_swapped {
                    return Err(recovery_failed(
                        "swapping_recovery_requires_manual_repair",
                        None,
                    ));
                }
                // Все целевые файлы уже соответствуют журналу. Считаем
                // операцию завершённой, но сохраняем temp-файлы: ownership
                // нельзя доказать данными из того же журнала.
                "completed_commit"
            } else {
                for entry in journal.entries.iter().rev() {
                    let new_temp = temp_path(&entry.path, NEW_SUFFIX, transaction_id);
                    let old_temp = temp_path(&entry.path, OLD_SUFFIX, transaction_id);
                    match entry.old_sha256.as_deref() {
                        Some(_)
                            if root
                                .sha256(&old_temp)
                                .map_err(|_| recovery_failed("backup_invalid", Some(&entry.path)))?
                                .is_some() =>
                        {
                            root.rename_sibling(&old_temp, &entry.path)
                                .map_err(|_| recovery_failed("restore", Some(&entry.path)))?
                        }
                        Some(_) => {}
                        None => {
                            if root
                                .sha256(&entry.path)
                                .map_err(|_| recovery_failed("verify", Some(&entry.path)))?
                                .as_deref()
                                == Some(entry.new_sha256.as_str())
                            {
                                root.remove(&entry.path).map_err(|_| {
                                    recovery_failed("remove_new", Some(&entry.path))
                                })?;
                            }
                        }
                    }
                    root.remove(&new_temp)
                        .map_err(|_| recovery_failed("cleanup", Some(&entry.path)))?;
                }
                sync_file(&root.directory);
                for entry in &journal.entries {
                    let actual = root
                        .sha256(&entry.path)
                        .map_err(|_| recovery_failed("verify", Some(&entry.path)))?;
                    if actual != entry.old_sha256 {
                        return Err(recovery_failed("old_sha_mismatch", Some(&entry.path)));
                    }
                }
                "rolled_back"
            }
        }
        // После фиксации все целевые SHA проверены; временные копии остаются,
        // потому что их владение нельзя подтвердить по локальному журналу.
        Phase::Committed => "completed_commit",
    };
    root.remove(JOURNAL)
        .map_err(|_| recovery_failed("journal_remove", None))?;
    sync_file(&root.directory);
    tracing::warn!(event = "extension_install_recovered", stage = "install", outcome,
        from = ?journal.from_version, to = ?journal.to_version, "Прерванная установка расширения восстановлена");
    Ok(Some(
        json!({"recovered":outcome,"from_version":journal.from_version,"to_version":journal.to_version,
        "files":journal.entries.iter().map(|e| &e.path).collect::<Vec<_>>()}),
    ))
}

/// Файлы, чьё содержимое на диске отличается от files.
pub fn differing(dir: &Path, files: &[PackageFile]) -> Vec<String> {
    let Ok(Some(root)) = InstallRoot::open(dir, false) else {
        return files.iter().map(|file| file.path.clone()).collect();
    };
    files
        .iter()
        .filter(|file| {
            !valid_entry_path(&file.path)
                || root.read_file(&file.path).ok().flatten().as_deref() != Some(&file.bytes)
        })
        .map(|file| file.path.clone())
        .collect()
}

fn validate_files(files: &[PackageFile]) -> Result<(), AssetError> {
    let mut seen = std::collections::BTreeSet::new();
    for file in files {
        if !valid_entry_path(&file.path)
            || !seen.insert(&file.path)
            || file.bytes.len() as u64 > package::MAX_FILE_BYTES
        {
            return Err(bridge_error(
                "install_package_invalid",
                "Внутренний список файлов расширения содержит недопустимый путь или размер",
            ));
        }
    }
    Ok(())
}

fn version_valid(version: Option<&str>) -> bool {
    version.is_none_or(|version| {
        !version.is_empty()
            && version.len() <= MAX_VERSION_BYTES
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    })
}

/// Приводит каталог к files одной транзакцией. Файлы каталога, не названные
/// в files, не трогаются. Возвращает имена заменённых файлов.
pub fn install(
    dir: &Path,
    files: &[PackageFile],
    versions: (Option<String>, Option<String>),
) -> Result<Vec<String>, AssetError> {
    install_with(dir, files, versions, &mut |_| Ok(()))
}

pub fn install_with(
    dir: &Path,
    files: &[PackageFile],
    (from_version, to_version): (Option<String>, Option<String>),
    fault: &mut dyn FnMut(Step) -> Result<(), Fault>,
) -> Result<Vec<String>, AssetError> {
    validate_files(files)?;
    if !version_valid(from_version.as_deref()) || !version_valid(to_version.as_deref()) {
        return Err(bridge_error(
            "install_version_invalid",
            "Версия установки содержит недопустимые символы",
        ));
    }
    recover(dir)?;
    let root = InstallRoot::open(dir, true)
        .map_err(|error| io_error("каталог расширения", error))?
        .ok_or_else(|| bridge_error("install_root_missing", "Каталог расширения не создан"))?;
    let mut ordered = files.to_vec();
    write_order(&mut ordered);
    let mut changed = Vec::new();
    for file in ordered {
        match root.read_file(&file.path) {
            Ok(Some(bytes)) if bytes == file.bytes => {}
            Ok(_) => changed.push(file),
            Err(error) => return Err(io_error("файл расширения", error)),
        }
    }
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::with_capacity(changed.len());
    for file in &changed {
        let old_sha256 = root
            .sha256(&file.path)
            .map_err(|error| io_error("файл расширения", error))?;
        entries.push(Entry {
            path: file.path.clone(),
            new_sha256: sha256_hex(&file.bytes),
            old_sha256,
        });
    }
    let transaction_id =
        unique_id().map_err(|error| io_error("идентификатор транзакции установки", error))?;
    let mut journal = Journal {
        schema_version: 1,
        phase: Phase::Staging,
        from_version,
        to_version,
        entries,
        transaction_id: Some(transaction_id),
    };
    let mut reservations = Vec::with_capacity(changed.len());
    let reserve_result = (|| -> std::io::Result<()> {
        for (file, entry) in changed.iter().zip(&journal.entries) {
            let new_path = temp_path(&file.path, NEW_SUFFIX, journal.transaction_id.as_deref());
            let new_file = root.reserve_file(&new_path)?;
            reservations.push(Reservation {
                new_path,
                new_file,
                old_path: None,
                old_file: None,
            });
            if entry.old_sha256.is_some() {
                let old_path = temp_path(&file.path, OLD_SUFFIX, journal.transaction_id.as_deref());
                let old_file = root.reserve_file(&old_path)?;
                let reservation = reservations.last_mut().expect("reservation was added");
                reservation.old_path = Some(old_path);
                reservation.old_file = Some(old_file);
            }
        }
        Ok(())
    })();
    if let Err(error) = reserve_result {
        // Коллизия имени не даёт права удалять существующий файл. Удаляем
        // только дескрипторы, открытые этим вызовом до столкновения.
        let _ = cleanup_reservations(&root, reservations);
        return Err(io_error("резервирование файлов расширения", error));
    }
    if let Err(error) = root.write_journal(&journal) {
        let _ = cleanup_reservations(&root, reservations);
        return Err(io_error("журнал установки расширения", error));
    }
    let failed_crash =
        || bridge_error("install_interrupted", "Установка прервана внедрённым сбоем");

    // Запись временных файлов: ни один рабочий файл ещё не изменён.
    let staged = (|| -> Result<(), Fault> {
        for (index, (file, reservation)) in changed.iter().zip(reservations.iter_mut()).enumerate()
        {
            write_reserved(&mut reservation.new_file, &file.bytes).map_err(Fault::Io)?;
            if let Some(expected_old) = journal.entries[index].old_sha256.as_deref() {
                let old = root
                    .read_file(&file.path)
                    .map_err(Fault::Io)?
                    .ok_or_else(|| {
                        Fault::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "old install file disappeared",
                        ))
                    })?;
                if sha256_hex(&old) != expected_old {
                    return Err(Fault::Io(std::io::Error::other(
                        "old install file changed during staging",
                    )));
                }
                let old_file = reservation.old_file.as_mut().ok_or_else(|| {
                    Fault::Io(std::io::Error::other("old reservation is missing"))
                })?;
                write_reserved(old_file, &old).map_err(Fault::Io)?;
            }
            fault(Step::Staged(index))?;
        }
        verify_staged(&root, &journal).map_err(Fault::Io)?;
        Ok(())
    })();
    match staged {
        Ok(()) => drop(reservations),
        Err(Fault::Crash) => return Err(failed_crash()),
        Err(Fault::Io(error)) => {
            let cleanup = cleanup_reservations(&root, reservations);
            if cleanup.is_ok() {
                let _ = root.remove(JOURNAL);
            }
            let mut error = io_error("подготовка файлов расширения", error);
            error.details["changed"] = json!(false);
            if let Err(cleanup) = cleanup {
                error.details["cleanup_error"] = json!(cleanup.kind().to_string());
            }
            return Err(error);
        }
    }

    // Замена: renameat выполняется относительно удерживаемого дескриптора
    // проверенного родительского каталога; manifest остаётся последним.
    journal.phase = Phase::Swapping;
    let swapped = (|| -> Result<(), Fault> {
        root.write_journal(&journal).map_err(Fault::Io)?;
        preflight_recovery(&root, &journal).map_err(|error| {
            Fault::Io(std::io::Error::other(crate::forvo::provider_code(&error)))
        })?;
        for (index, file) in changed.iter().enumerate() {
            let temp = temp_path(&file.path, NEW_SUFFIX, journal.transaction_id.as_deref());
            let entry = &journal.entries[index];
            if root.sha256(&temp).map_err(Fault::Io)?.as_deref() != Some(entry.new_sha256.as_str())
                || root.sha256(&entry.path).map_err(Fault::Io)? != entry.old_sha256
            {
                return Err(Fault::Io(std::io::Error::other(
                    "install inputs changed before swap",
                )));
            }
            root.rename_sibling(&temp, &file.path).map_err(Fault::Io)?;
            fault(Step::Swapped(index))?;
        }
        sync_file(&root.directory);
        for entry in &journal.entries {
            let actual = root.sha256(&entry.path).map_err(Fault::Io)?;
            if actual.as_deref() != Some(entry.new_sha256.as_str()) {
                return Err(Fault::Io(std::io::Error::other("sha_mismatch_after_swap")));
            }
        }
        fault(Step::Verified)?;
        Ok(())
    })();
    match swapped {
        Ok(()) => {}
        Err(Fault::Crash) => return Err(failed_crash()),
        Err(Fault::Io(error)) => {
            let rollback = recover_loaded(&root, journal.clone(), true);
            let mut error = io_error("замена файлов расширения", error);
            match rollback {
                Ok(_) => error.details["rolled_back"] = json!(true),
                Err(cleanup) => {
                    error.details["rolled_back"] = json!(false);
                    error.details["recovery"] = json!(crate::forvo::provider_code(&cleanup));
                }
            }
            return Err(error);
        }
    }

    journal.phase = Phase::Committed;
    root.write_journal(&journal)
        .map_err(|error| io_error("журнал установки расширения", error))?;
    for entry in &journal.entries {
        if let Some(old_sha) = entry.old_sha256.as_deref() {
            let old_temp = temp_path(&entry.path, OLD_SUFFIX, journal.transaction_id.as_deref());
            root.remove_if_hash(&old_temp, old_sha)
                .map_err(|error| io_error("уборка прежних файлов расширения", error))?;
        }
    }
    root.remove(JOURNAL)
        .map_err(|error| io_error("журнал установки расширения", error))?;
    sync_file(&root.directory);
    Ok(changed.into_iter().map(|file| file.path).collect())
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
