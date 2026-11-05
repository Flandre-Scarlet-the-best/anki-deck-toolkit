//! Транзакционная замена файлов распакованного расширения в его каталоге.
//!
//! Каталог установки не переименовывается и не пересоздаётся: от строки его
//! пути зависит ID расширения, а браузер держит его загруженным. Замена идёт
//! пофайлово, но как одна транзакция с журналом `.toolkit-install-journal.json`
//! в самом каталоге:
//!
//! 1. `staging` — журнал записан; рядом с каждым меняемым файлом создаётся
//!    полная новая копия `<имя>.update-new` и копия прежнего `<имя>.update-old`
//!    (здесь же проявляется нехватка места — до любой замены);
//! 2. `swapping` — атомарные `rename` новых копий на место, `manifest.json`
//!    последним; затем SHA-256 каждого файла сверяется с ожидаемым;
//! 3. `committed` — прежние копии удаляются, журнал удаляется.
//!
//! Восстановление ([`recover`]) после сбоя питания, `SIGKILL`, ошибки записи
//! или нехватки места: `staging` отбрасывается, `swapping` откатывается из
//! `.update-old` с проверкой прежних SHA, `committed` дочищается. Повторный
//! запуск идемпотентен. Служебные имена начинаются с точки или содержат
//! `.update-`, поэтому не совпадают ни с одним допустимым файлом пакета.
use super::bridge_error;
use super::package::{PackageFile, write_order};
use crate::AssetError;
use crate::hashing::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const JOURNAL: &str = ".toolkit-install-journal.json";
const NEW_SUFFIX: &str = ".update-new";
const OLD_SUFFIX: &str = ".update-old";

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
    /// `null` — файла до установки не было.
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
}

/// Шаг транзакции для внедрения отказов в тестах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Staged(usize),
    Swapped(usize),
    Verified,
}

/// Реакция внедрённого отказа: ошибка ввода-вывода (обычная обработка) или
/// «сбой питания» — немедленный выход без какой-либо уборки.
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

fn file_sha(path: &Path) -> Result<Option<String>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_hex(bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().expect("имя файла").to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn sync_dir(dir: &Path) {
    // DrvFs (каталог Windows из WSL) может не поддерживать fsync каталога;
    // атомарность rename от этого не зависит.
    if let Ok(file) = std::fs::File::open(dir) {
        let _ = file.sync_all();
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn write_journal(dir: &Path, journal: &Journal) -> std::io::Result<()> {
    let path = dir.join(JOURNAL);
    let tmp = with_suffix(&path, NEW_SUFFIX);
    write_new(&tmp, &serde_json::to_vec_pretty(journal).expect("журнал"))?;
    std::fs::rename(&tmp, &path)?;
    sync_dir(dir);
    Ok(())
}

fn read_journal(dir: &Path) -> Result<Option<Journal>, AssetError> {
    match std::fs::read(dir.join(JOURNAL)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .ok()
            .filter(|j: &Journal| j.schema_version == 1)
            .map(Some)
            .ok_or_else(|| {
                bridge_error(
                    "install_journal_corrupt",
                    "Журнал установки расширения повреждён; восстановите каталог расширения командой setup",
                )
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("журнал установки расширения", e)),
    }
}

/// Служебные файлы прерванной до записи журнала подготовки: только наши имена.
fn sweep_stray(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut removed = Vec::new();
    let mut visit = |base: &Path, prefix: &str| -> std::io::Result<Vec<PathBuf>> {
        let mut subdirs = Vec::new();
        let entries = match std::fs::read_dir(base) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(subdirs),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                subdirs.push(entry.path());
            } else if kind.is_file() && (name.ends_with(NEW_SUFFIX) || name.ends_with(OLD_SUFFIX)) {
                std::fs::remove_file(entry.path())?;
                removed.push(format!("{prefix}{name}"));
            }
        }
        Ok(subdirs)
    };
    for sub in visit(dir, "")? {
        let prefix = format!("{}/", sub.file_name().unwrap_or_default().to_string_lossy());
        visit(&sub, &prefix)?;
    }
    Ok(removed)
}

/// Доводит каталог до согласованного состояния после прерванной установки.
/// `None` — восстанавливать было нечего.
pub fn recover(dir: &Path) -> Result<Option<Value>, AssetError> {
    let Some(journal) = read_journal(dir)? else {
        let removed = sweep_stray(dir).map_err(|e| io_error("уборка каталога расширения", e))?;
        return Ok((!removed.is_empty())
            .then(|| json!({"recovered":"stray_files_removed","files":removed})));
    };
    let failed = |reason: &str, path: Option<&str>| {
        let mut e = bridge_error(
            "install_recovery_failed",
            "Не удалось вернуть каталог расширения к согласованной версии; файлы оставлены как есть. Повторите команду или setup",
        );
        e.details["reason"] = json!(reason);
        if let Some(path) = path {
            e.details["path"] = json!(path);
        }
        e
    };
    let outcome = match journal.phase {
        Phase::Staging => {
            for entry in &journal.entries {
                let path = dir.join(&entry.path);
                remove_if_exists(&with_suffix(&path, NEW_SUFFIX))
                    .and_then(|()| remove_if_exists(&with_suffix(&path, OLD_SUFFIX)))
                    .map_err(|_| failed("cleanup", Some(&entry.path)))?;
            }
            "discarded_staging"
        }
        Phase::Swapping => {
            for entry in journal.entries.iter().rev() {
                let path = dir.join(&entry.path);
                let old = with_suffix(&path, OLD_SUFFIX);
                if old.exists() {
                    std::fs::rename(&old, &path)
                        .map_err(|_| failed("restore", Some(&entry.path)))?;
                } else if entry.old_sha256.is_none() {
                    remove_if_exists(&path).map_err(|_| failed("remove_new", Some(&entry.path)))?;
                }
                remove_if_exists(&with_suffix(&path, NEW_SUFFIX))
                    .map_err(|_| failed("cleanup", Some(&entry.path)))?;
            }
            sync_dir(dir);
            for entry in &journal.entries {
                let actual = file_sha(&dir.join(&entry.path))
                    .map_err(|_| failed("verify", Some(&entry.path)))?;
                if actual != entry.old_sha256 {
                    return Err(failed("old_sha_mismatch", Some(&entry.path)));
                }
            }
            "rolled_back"
        }
        Phase::Committed => {
            for entry in &journal.entries {
                let path = dir.join(&entry.path);
                remove_if_exists(&with_suffix(&path, OLD_SUFFIX))
                    .and_then(|()| remove_if_exists(&with_suffix(&path, NEW_SUFFIX)))
                    .map_err(|_| failed("cleanup", Some(&entry.path)))?;
            }
            "completed_commit"
        }
    };
    remove_if_exists(&dir.join(JOURNAL)).map_err(|_| failed("journal_remove", None))?;
    sync_dir(dir);
    tracing::warn!(event = "extension_install_recovered", stage = "install", outcome,
        from = ?journal.from_version, to = ?journal.to_version, "Прерванная установка расширения восстановлена");
    Ok(Some(
        json!({"recovered":outcome,"from_version":journal.from_version,"to_version":journal.to_version,
        "files":journal.entries.iter().map(|e| &e.path).collect::<Vec<_>>()}),
    ))
}

/// Файлы, чьё содержимое на диске отличается от `files`.
pub fn differing(dir: &Path, files: &[PackageFile]) -> Vec<String> {
    files
        .iter()
        .filter(|f| std::fs::read(dir.join(&f.path)).map_or(true, |b| b != f.bytes))
        .map(|f| f.path.clone())
        .collect()
}

/// Приводит каталог к `files` одной транзакцией. Файлы каталога, не названные
/// в `files`, не трогаются. Возвращает имена заменённых файлов.
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
    recover(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| io_error("каталог расширения", e))?;
    let mut ordered = files.to_vec();
    write_order(&mut ordered);
    let changed: Vec<PackageFile> = ordered
        .into_iter()
        .filter(|f| std::fs::read(dir.join(&f.path)).map_or(true, |b| b != f.bytes))
        .collect();
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::with_capacity(changed.len());
    for file in &changed {
        entries.push(Entry {
            path: file.path.clone(),
            new_sha256: sha256_hex(&file.bytes),
            old_sha256: file_sha(&dir.join(&file.path))
                .map_err(|e| io_error("файл расширения", e))?,
        });
    }
    let mut journal = Journal {
        schema_version: 1,
        phase: Phase::Staging,
        from_version,
        to_version,
        entries,
    };
    let crashed = || bridge_error("install_interrupted", "Установка прервана внедрённым сбоем");
    // 1. Подготовка: ни один рабочий файл ещё не изменён.
    let staged = (|| -> Result<(), Fault> {
        write_journal(dir, &journal).map_err(Fault::Io)?;
        for (index, file) in changed.iter().enumerate() {
            let path = dir.join(&file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(Fault::Io)?;
            }
            write_new(&with_suffix(&path, NEW_SUFFIX), &file.bytes).map_err(Fault::Io)?;
            if journal.entries[index].old_sha256.is_some() {
                let old = std::fs::read(&path).map_err(Fault::Io)?;
                write_new(&with_suffix(&path, OLD_SUFFIX), &old).map_err(Fault::Io)?;
            }
            fault(Step::Staged(index))?;
        }
        Ok(())
    })();
    match staged {
        Ok(()) => {}
        Err(Fault::Crash) => return Err(crashed()),
        Err(Fault::Io(error)) => {
            let cleanup = recover(dir);
            let mut e = io_error("подготовка файлов расширения", error);
            e.details["changed"] = json!(false);
            if let Err(cleanup) = cleanup {
                e.details["recovery"] = json!(crate::forvo::provider_code(&cleanup));
            }
            return Err(e);
        }
    }
    // 2. Замена: rename атомарен для каждого файла; manifest последним.
    journal.phase = Phase::Swapping;
    let swapped = (|| -> Result<(), Fault> {
        write_journal(dir, &journal).map_err(Fault::Io)?;
        for (index, file) in changed.iter().enumerate() {
            let path = dir.join(&file.path);
            std::fs::rename(with_suffix(&path, NEW_SUFFIX), &path).map_err(Fault::Io)?;
            fault(Step::Swapped(index))?;
        }
        sync_dir(dir);
        for entry in &journal.entries {
            let actual = file_sha(&dir.join(&entry.path)).map_err(Fault::Io)?;
            if actual.as_deref() != Some(entry.new_sha256.as_str()) {
                return Err(Fault::Io(std::io::Error::other("sha_mismatch_after_swap")));
            }
        }
        fault(Step::Verified)?;
        Ok(())
    })();
    match swapped {
        Ok(()) => {}
        Err(Fault::Crash) => return Err(crashed()),
        Err(Fault::Io(error)) => {
            let rollback = recover(dir);
            let mut e = io_error("замена файлов расширения", error);
            match rollback {
                Ok(_) => e.details["rolled_back"] = json!(true),
                Err(cleanup) => {
                    e.details["rolled_back"] = json!(false);
                    e.details["recovery"] = json!(crate::forvo::provider_code(&cleanup));
                }
            }
            return Err(e);
        }
    }
    // 3. Фиксация: новое содержимое на месте и проверено.
    journal.phase = Phase::Committed;
    write_journal(dir, &journal).map_err(|e| io_error("журнал установки расширения", e))?;
    for entry in &journal.entries {
        remove_if_exists(&with_suffix(&dir.join(&entry.path), OLD_SUFFIX))
            .map_err(|e| io_error("уборка прежних файлов расширения", e))?;
    }
    remove_if_exists(&dir.join(JOURNAL)).map_err(|e| io_error("журнал установки расширения", e))?;
    sync_dir(dir);
    Ok(changed.into_iter().map(|f| f.path).collect())
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
