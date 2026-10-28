//! Хранилище истории: архив, индекс и их согласованность.
//!
//! Порядок записи: сначала атомарно архивная копия, затем строка индекса.
//! Прерывание между ними оставляет запись в архиве без строки индекса, и
//! следующее открытие хранилища её доиндексирует. Строка без архивной копии
//! невозможна при штатной записи; если файл удалили вручную, синхронизация
//! убирает строку и сообщает об этом. Все операции с хранилищем выполняются
//! под исключительной блокировкой файла `lock`.

use crate::archive::{self, Accepted, Entry, Put};
use crate::index::{Index, Opened};
use benchmark_contract::catalog::Catalog;
use benchmark_contract::diagnostic::Diagnostic;
use benchmark_contract::result::BenchmarkResult;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FORMAT_FILE: &str = "store.json";
pub const INDEX_FILE: &str = "index.sqlite";
pub const LOCK_FILE: &str = "lock";
pub const TMP_DIR: &str = "tmp";
pub const FORMAT_NAME: &str = "anki-deck-toolkit/benchmark-history";
/// Версия раскладки архива; индекс версионируется отдельно.
pub const ARCHIVE_VERSION: u32 = 1;
pub const BACKUP_MANIFEST: &str = "backup.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Format {
    format: String,
    archive_version: u32,
}

/// Отказ операции с понятной причиной и кодом возврата.
#[derive(Debug, Clone)]
pub enum Error {
    /// Неверный запрос: неизвестный ID, неоднозначный префикс.
    Usage(String),
    /// Хранилище или индекс недоступны.
    Store(String),
    /// Документ отвергнут контрактом и не принят.
    Rejected(Vec<Diagnostic>),
    /// Архивная копия записана, индекс не обновлён.
    Index { id: String, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage(m) | Error::Store(m) => f.write_str(m),
            Error::Rejected(diagnostics) => {
                let errors: Vec<String> = diagnostics
                    .iter()
                    .filter(|d| d.severity == benchmark_contract::diagnostic::Severity::Error)
                    .map(ToString::to_string)
                    .collect();
                write!(f, "документ отвергнут контрактом: {}", errors.join("; "))
            }
            Error::Index { id, message } => write!(
                f,
                "результат {id} сохранён в архиве, но индекс не обновлён: {message}; выполните reindex"
            ),
        }
    }
}

/// Итог приёма одного документа.
#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub id: String,
    pub scenario: String,
    pub profile: String,
    pub outcome: String,
    /// `false` — такой результат уже был в истории.
    pub created: bool,
    pub warnings: Vec<Diagnostic>,
}

/// Расхождения архива и индекса, найденные и устранённые при синхронизации.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Sync {
    /// Строки, удалённые из-за несовпадения контрольной суммы проекции.
    pub repaired: Vec<String>,
    pub indexed: Vec<String>,
    pub dropped: Vec<String>,
    pub problems: Vec<Problem>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// ID записи или путь постороннего файла относительно корня хранилища.
    pub item: String,
    pub reason: String,
}

pub struct Store {
    root: PathBuf,
    index: Index,
    sync: Sync,
    _lock: fs::File,
}

impl Store {
    /// Открыть хранилище, создав его при необходимости, и согласовать индекс
    /// с архивом. Занятое хранилище ожидается без предела.
    pub fn open(root: &Path) -> Result<Store, Error> {
        Store::open_with(root, None)
    }

    /// Как [`Store::open`], но блокировку ждёт не дольше `wait`: побочный
    /// шаг вроде сохранения результата серии не зависает за чужой долгой
    /// командой.
    pub fn open_waiting(root: &Path, wait: Duration) -> Result<Store, Error> {
        Store::open_with(root, Some(wait))
    }

    fn open_with(root: &Path, wait: Option<Duration>) -> Result<Store, Error> {
        fs::create_dir_all(root.join(archive::RESULTS_DIR))
            .map_err(|e| Error::Store(format!("хранилище {} не создано: {e}", root.display())))?;
        let lock = lock(root, wait)?;
        check_format(root)?;
        let mut notes = Vec::new();
        let index = open_index(root, &mut notes)?;
        let mut store = Store {
            root: root.to_path_buf(),
            index,
            sync: Sync::default(),
            _lock: lock,
        };
        let mut sync = store.synchronize()?;
        sync.notes.splice(0..0, notes);
        store.sync = sync;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Что исправила синхронизация при открытии.
    pub fn opened_sync(&self) -> &Sync {
        &self.sync
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Принять документ: проверка контрактом, архивная копия, строка индекса.
    pub fn import(
        &mut self,
        text: &str,
        name: &str,
        catalog: Option<&Catalog>,
    ) -> Result<Imported, Error> {
        let Accepted {
            id,
            result,
            warnings,
        } = archive::accept(text, name, catalog).map_err(Error::Rejected)?;
        let created =
            archive::put(&self.root, &id, text.as_bytes()).map_err(Error::Store)? == Put::Created;
        let path = archive::relative_path(&id).to_string_lossy().into_owned();
        self.index
            .insert(&id, &path, &result)
            .map_err(|e| Error::Index {
                id: id.clone(),
                message: e.to_string(),
            })?;
        Ok(Imported {
            id,
            scenario: result.scenario.id.clone(),
            profile: result.profile.id.clone(),
            outcome: crate::analysis::wire_name(&result.outcome.status),
            created,
            warnings,
        })
    }

    /// Доиндексировать архивные записи без строк и убрать строки без записей.
    ///
    /// Строки, не совпавшие с контрольной суммой проекции, сначала удаляются:
    /// дальше они доиндексируются из архива как недостающие.
    fn synchronize(&mut self) -> Result<Sync, Error> {
        let (entries, strays) = archive::scan(&self.root)
            .map_err(|e| Error::Store(format!("архив не читается: {e}")))?;
        let mut sync = Sync::default();
        let inconsistent = self
            .index
            .inconsistent()
            .map_err(|e| Error::Store(format!("индекс не читается: {e}")))?;
        for id in &inconsistent {
            self.index
                .remove(id)
                .map_err(|e| Error::Store(format!("индекс не обновлён: {e}")))?;
        }
        if !inconsistent.is_empty() {
            sync.notes.push(format!(
                "строки индекса не совпали с контрольной суммой проекции и перестраиваются из архива: {}",
                inconsistent.len()
            ));
        }
        sync.repaired = inconsistent;
        let indexed = self
            .index
            .ids()
            .map_err(|e| Error::Store(format!("индекс не читается: {e}")))?;
        for stray in strays {
            sync.problems.push(Problem {
                item: self.relative(&stray),
                reason: "посторонний файл в архиве: имя не является идентификатором записи".into(),
            });
        }
        let present: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.id.as_str()).collect();
        for entry in entries.iter().filter(|e| !indexed.contains(&e.id)) {
            match self.load_entry(entry) {
                Ok(result) => {
                    let path = archive::relative_path(&entry.id)
                        .to_string_lossy()
                        .into_owned();
                    self.index
                        .insert(&entry.id, &path, &result)
                        .map_err(|e| Error::Index {
                            id: entry.id.clone(),
                            message: e.to_string(),
                        })?;
                    sync.indexed.push(entry.id.clone());
                }
                Err(reason) => sync.problems.push(Problem {
                    item: entry.id.clone(),
                    reason,
                }),
            }
        }
        for id in indexed.iter().filter(|id| !present.contains(id.as_str())) {
            self.index
                .remove(id)
                .map_err(|e| Error::Store(format!("индекс не обновлён: {e}")))?;
            sync.dropped.push(id.clone());
        }
        Ok(sync)
    }

    fn load_entry(&self, entry: &Entry) -> Result<BenchmarkResult, String> {
        let text = fs::read_to_string(&entry.path).map_err(|e| format!("файл не читается: {e}"))?;
        archive::verify(&entry.id, &text)
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Найти ID по префиксу не короче шести символов.
    pub fn resolve(&self, prefix: &str) -> Result<String, Error> {
        let prefix = prefix.trim().to_ascii_lowercase();
        if prefix.len() < 6 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Usage(format!(
                "«{prefix}»: ID — не меньше шести шестнадцатеричных символов"
            )));
        }
        let found = self
            .index
            .matching(&prefix)
            .map_err(|e| Error::Store(e.to_string()))?;
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(Error::Usage(format!("в истории нет результата {prefix}"))),
            many => Err(Error::Usage(format!(
                "префикс {prefix} неоднозначен: {}",
                many.join(", ")
            ))),
        }
    }

    /// Исходный текст и проверенный результат записи.
    pub fn load(&self, id: &str) -> Result<(String, BenchmarkResult), Error> {
        let path = self.root.join(archive::relative_path(id));
        let text = fs::read_to_string(&path)
            .map_err(|e| Error::Store(format!("архивная копия {id} не читается: {e}")))?;
        let result = archive::verify(id, &text)
            .map_err(|e| Error::Store(format!("архивная копия {id} повреждена: {e}")))?;
        Ok((text, result))
    }

    /// Полная проверка целостности каждой архивной копии. Ничего не меняет.
    pub fn verify(&self) -> Result<Verification, Error> {
        let (entries, strays) = archive::scan(&self.root)
            .map_err(|e| Error::Store(format!("архив не читается: {e}")))?;
        let mut report = Verification {
            checked: entries.len(),
            problems: Vec::new(),
            index_problems: Vec::new(),
        };
        for stray in strays {
            report.problems.push(Problem {
                item: self.relative(&stray),
                reason: "посторонний файл в архиве".into(),
            });
        }
        let mut loaded = Vec::new();
        for entry in &entries {
            match self.load_entry(entry) {
                Ok(result) => loaded.push((
                    entry.id.as_str(),
                    archive::relative_path(&entry.id)
                        .to_string_lossy()
                        .into_owned(),
                    result,
                )),
                Err(reason) => report.problems.push(Problem {
                    item: entry.id.clone(),
                    reason,
                }),
            }
        }
        // Проекция сверяется с архивом: эталонные суммы строятся из JSON в
        // памяти, индекс на диске не меняется.
        let expected = Index::expected_digests(
            loaded
                .iter()
                .map(|(id, path, result)| (*id, path.as_str(), result)),
        )
        .map_err(|e| Error::Store(format!("эталон проекции не построен: {e}")))?;
        let stored = self
            .index
            .digests()
            .map_err(|e| Error::Store(format!("индекс не читается: {e}")))?;
        let unsealed: std::collections::BTreeSet<String> = self
            .index
            .inconsistent()
            .map_err(|e| Error::Store(format!("индекс не читается: {e}")))?
            .into_iter()
            .collect();
        let damaged: std::collections::BTreeSet<&str> =
            report.problems.iter().map(|p| p.item.as_str()).collect();
        for (id, digest) in &expected {
            if stored.get(id) != Some(digest) || unsealed.contains(id) {
                report.index_problems.push(Problem {
                    item: id.clone(),
                    reason: "строка индекса отсутствует или не совпадает с проекцией архивной копии; выполните reindex".into(),
                });
            }
        }
        for id in stored.keys().filter(|id| !expected.contains_key(*id)) {
            if !damaged.contains(id.as_str()) {
                report.index_problems.push(Problem {
                    item: id.clone(),
                    reason: "строка индекса без архивной копии; выполните reindex".into(),
                });
            }
        }
        Ok(report)
    }

    /// Пересобрать индекс из архива целиком: новый файл строится рядом и
    /// заменяет прежний одним переименованием. Прежний индекс не открывается,
    /// поэтому пересборка доступна и для повреждённого индекса, и для индекса
    /// более новой схемы.
    pub fn rebuild(root: &Path) -> Result<(Store, Sync), Error> {
        if !root.join(FORMAT_FILE).is_file() {
            return Err(Error::Store(format!("хранилища {} нет", root.display())));
        }
        let lock = lock(root, None)?;
        check_format(root)?;
        let fresh = root.join(format!("{INDEX_FILE}.rebuild"));
        let _ = fs::remove_file(&fresh);
        Index::open(&fresh).map_err(|e| Error::Store(format!("новый индекс не создан: {e}")))?;
        fs::rename(&fresh, root.join(INDEX_FILE))
            .map_err(|e| Error::Store(format!("индекс не заменён: {e}")))?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let _ = fs::remove_file(root.join(format!("{INDEX_FILE}{suffix}")));
        }
        let mut notes = Vec::new();
        let index = open_index(root, &mut notes)?;
        let mut store = Store {
            root: root.to_path_buf(),
            index,
            sync: Sync::default(),
            _lock: lock,
        };
        let sync = store.synchronize()?;
        Ok((store, sync))
    }

    /// Скопировать архив в новый каталог для переноса на другую машину.
    pub fn backup(&self, destination: &Path) -> Result<usize, Error> {
        crate::location::refuse_inside_git(destination, "каталог резервной копии")
            .map_err(Error::Usage)?;
        crate::location::refuse_overlap(&self.root, destination, "каталог резервной копии")
            .map_err(Error::Usage)?;
        if destination.exists()
            && fs::read_dir(destination)
                .map_err(|e| Error::Usage(format!("{}: {e}", destination.display())))?
                .next()
                .is_some()
        {
            return Err(Error::Usage(format!(
                "каталог {} не пуст: резервная копия пишется только в новый или пустой каталог",
                destination.display()
            )));
        }
        let (entries, _) = archive::scan(&self.root)
            .map_err(|e| Error::Store(format!("архив не читается: {e}")))?;
        let mut copied = Vec::new();
        for entry in &entries {
            let bytes = fs::read(&entry.path).map_err(|e| Error::Store(e.to_string()))?;
            let text = String::from_utf8_lossy(&bytes);
            archive::verify(&entry.id, &text).map_err(|reason| {
                Error::Store(format!(
                    "запись {} повреждена ({reason}): резервная копия не создана; проверьте verify",
                    entry.id
                ))
            })?;
            let target = destination.join(archive::relative_path(&entry.id));
            fs::create_dir_all(target.parent().expect("каталог записи"))
                .map_err(|e| Error::Store(e.to_string()))?;
            archive::write_new(&destination.join(TMP_DIR), &target, &bytes)
                .map_err(|e| Error::Store(format!("{}: {e}", target.display())))?;
            copied.push(entry.id.clone());
        }
        // Манифест пишется последним и атомарно: копия без него не считается
        // резервной, и прерванное копирование не выдаётся за завершённое.
        let manifest = Manifest {
            format: FORMAT_NAME.into(),
            archive_version: ARCHIVE_VERSION,
            results: copied,
        };
        archive::write_new(
            &destination.join(TMP_DIR),
            &destination.join(BACKUP_MANIFEST),
            (serde_json::to_string_pretty(&manifest).expect("JSON") + "\n").as_bytes(),
        )
        .map_err(|e| Error::Store(format!("{BACKUP_MANIFEST} не записан: {e}")))?;
        let _ = fs::remove_dir(destination.join(TMP_DIR));
        Ok(manifest.results.len())
    }

    /// Инвентарь резервной копии из её манифеста.
    fn manifest(source: &Path) -> Result<Manifest, Error> {
        let path = source.join(BACKUP_MANIFEST);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Usage(format!(
                    "в {} нет {BACKUP_MANIFEST}: это не завершённая резервная копия backup; отдельные result.json принимайте командой import",
                    source.display()
                )));
            }
            Err(e) => return Err(Error::Usage(format!("{}: {e}", path.display()))),
        };
        let manifest: Manifest = serde_json::from_str(&text).map_err(|e| {
            Error::Usage(format!(
                "{} не распознан ({e}): инвентарь резервной копии неизвестен",
                path.display()
            ))
        })?;
        if manifest.format != FORMAT_NAME {
            return Err(Error::Usage(format!(
                "{} принадлежит другому формату «{}»",
                path.display(),
                manifest.format
            )));
        }
        if manifest.archive_version != ARCHIVE_VERSION {
            return Err(Error::Usage(format!(
                "резервная копия раскладки версии {} не поддерживается: этот инструмент понимает {ARCHIVE_VERSION}",
                manifest.archive_version
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &manifest.results {
            if !archive::is_id(id) || !seen.insert(id.as_str()) {
                return Err(Error::Usage(format!(
                    "{}: в инвентаре недопустимый или повторный идентификатор «{id}»",
                    path.display()
                )));
            }
        }
        Ok(manifest)
    }

    /// Принять записи резервной копии. Имя каждой записи — её идентификатор,
    /// и содержимое обязано ему соответствовать: так доказывается, что копия не
    /// изменилась с момента приёма, и каталог повторно не требуется.
    ///
    /// Состав копии сверяется с инвентарём манифеста: запись, которой нет в
    /// копии, повреждённая или не перечисленная в инвентаре, — проблема, и
    /// перенос не считается полным. Исправные записи принимаются в любом случае.
    pub fn restore(&mut self, source: &Path) -> Result<Restored, Error> {
        crate::location::refuse_overlap(&self.root, source, "резервная копия")
            .map_err(Error::Usage)?;
        let manifest = Self::manifest(source)?;
        let (entries, strays) = archive::scan(source)
            .map_err(|e| Error::Usage(format!("{}: {e}", source.display())))?;
        let mut restored = Restored {
            expected: manifest.results.len(),
            ..Restored::default()
        };
        for stray in strays {
            restored.problems.push(Problem {
                item: stray.display().to_string(),
                reason: "не запись резервной копии".into(),
            });
        }
        let found: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.id.as_str()).collect();
        for id in manifest
            .results
            .iter()
            .filter(|id| !found.contains(id.as_str()))
        {
            restored.missing.push(id.clone());
            restored.problems.push(Problem {
                item: id.clone(),
                reason: format!("запись перечислена в {BACKUP_MANIFEST}, но её файла в копии нет"),
            });
        }
        let listed: std::collections::BTreeSet<&str> =
            manifest.results.iter().map(String::as_str).collect();
        for entry in &entries {
            if !listed.contains(entry.id.as_str()) {
                restored.problems.push(Problem {
                    item: entry.id.clone(),
                    reason: format!(
                        "запись не перечислена в {BACKUP_MANIFEST}: копия изменена после создания, запись не принята"
                    ),
                });
                continue;
            }
            let text = match fs::read_to_string(&entry.path) {
                Ok(text) => text,
                Err(e) => {
                    restored.problems.push(Problem {
                        item: entry.id.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let result = match archive::verify(&entry.id, &text) {
                Ok(result) => result,
                Err(reason) => {
                    restored.problems.push(Problem {
                        item: entry.id.clone(),
                        reason,
                    });
                    continue;
                }
            };
            match archive::put(&self.root, &entry.id, text.as_bytes()) {
                Ok(Put::Created) => restored.added.push(entry.id.clone()),
                Ok(Put::Existing) => restored.present.push(entry.id.clone()),
                // Локальная копия повреждена, копия из резервной проверена:
                // это и есть штатное восстановление, на которое указывает отказ.
                Err(damaged) if self.root.join(archive::relative_path(&entry.id)).is_file() => {
                    match archive::replace_damaged(&self.root, &entry.id, text.as_bytes()) {
                        Ok(_) => restored.replaced.push(entry.id.clone()),
                        Err(reason) => {
                            restored.problems.push(Problem {
                                item: entry.id.clone(),
                                reason: format!("{damaged}; {reason}"),
                            });
                            continue;
                        }
                    }
                }
                Err(reason) => {
                    restored.problems.push(Problem {
                        item: entry.id.clone(),
                        reason,
                    });
                    continue;
                }
            }
            let path = archive::relative_path(&entry.id)
                .to_string_lossy()
                .into_owned();
            self.index
                .insert(&entry.id, &path, &result)
                .map_err(|e| Error::Index {
                    id: entry.id.clone(),
                    message: e.to_string(),
                })?;
        }
        restored.complete = restored.problems.is_empty();
        Ok(restored)
    }
}

/// Манифест резервной копии: формат и полный инвентарь записей.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    archive_version: u32,
    results: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Restored {
    /// Записей в инвентаре резервной копии.
    pub expected: usize,
    pub added: Vec<String>,
    pub present: Vec<String>,
    /// Повреждённые локальные копии, заменённые проверенными из резервной;
    /// прежние файлы отложены в `corrupt/`.
    pub replaced: Vec<String>,
    /// Перечислены в инвентаре, но файлов в копии нет.
    pub missing: Vec<String>,
    pub problems: Vec<Problem>,
    /// Каждая запись инвентаря перенесена или уже была, посторонних нет.
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    pub checked: usize,
    /// Проблемы архива — первичных данных.
    pub problems: Vec<Problem>,
    /// Расхождения индекса с архивом; устраняются командой `reindex`.
    pub index_problems: Vec<Problem>,
}

fn check_format(root: &Path) -> Result<(), Error> {
    let path = root.join(FORMAT_FILE);
    let expected = Format {
        format: FORMAT_NAME.into(),
        archive_version: ARCHIVE_VERSION,
    };
    match fs::read_to_string(&path) {
        Ok(text) => {
            let found: Format = serde_json::from_str(&text).map_err(|e| {
                Error::Store(format!(
                    "{} не распознан ({e}): это не хранилище истории бенчмарков",
                    path.display()
                ))
            })?;
            if found.format != FORMAT_NAME {
                return Err(Error::Store(format!(
                    "{} принадлежит другому формату «{}»",
                    path.display(),
                    found.format
                )));
            }
            if found.archive_version != ARCHIVE_VERSION {
                return Err(Error::Store(format!(
                    "раскладка архива версии {} не поддерживается: этот инструмент понимает {ARCHIVE_VERSION}; обновите benchmark-history",
                    found.archive_version
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let text = serde_json::to_string_pretty(&expected).expect("JSON") + "\n";
            match archive::write_new(&root.join(TMP_DIR), &path, text.as_bytes()) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => check_format(root),
                Err(e) => Err(Error::Store(format!("{} не записан: {e}", path.display()))),
            }
        }
        Err(e) => Err(Error::Store(format!("{} не читается: {e}", path.display()))),
    }
}

/// Исключительная блокировка хранилища; с `wait` — не дольше этого срока.
fn lock(root: &Path, wait: Option<Duration>) -> Result<fs::File, Error> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(LOCK_FILE))
        .map_err(|e| Error::Store(format!("файл блокировки не открыт: {e}")))?;
    let Some(wait) = wait else {
        lock.lock()
            .map_err(|e| Error::Store(format!("блокировка хранилища не получена: {e}")))?;
        return Ok(lock);
    };
    let deadline = Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::Store(format!(
                    "хранилище занято другой командой дольше {wait:?}"
                )));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(Error::Store(format!(
                    "блокировка хранилища не получена: {e}"
                )));
            }
        }
    }
}

/// Открыть индекс; повреждённый — отложить в сторону и создать заново.
fn open_index(root: &Path, notes: &mut Vec<String>) -> Result<Index, Error> {
    let path = root.join(INDEX_FILE);
    match Index::open(&path) {
        Ok((index, opened)) => {
            match opened {
                Opened::Created
                    if root
                        .join(archive::RESULTS_DIR)
                        .read_dir()
                        .is_ok_and(|mut d| d.next().is_some()) =>
                {
                    notes.push("индекс создан заново и заполняется из архива".into());
                }
                Opened::Repaired { reason } => notes.push(format!(
                    "{reason}: схема индекса создана заново и заполняется из архива"
                )),
                Opened::Migrated { from } => notes.push(format!(
                    "схема индекса {from} устарела: индекс перестроен из архива по схеме {}",
                    crate::index::SCHEMA_VERSION
                )),
                _ => {}
            }
            Ok(index)
        }
        Err(rusqlite::Error::SqliteFailure(error, message))
            if error.code == rusqlite::ErrorCode::TypeMismatch =>
        {
            Err(Error::Store(format!(
                "{}: индекс создан более новой версией benchmark-history; обновите инструмент или пересоберите индекс командой reindex",
                message.unwrap_or_default()
            )))
        }
        Err(error) => {
            // Индекс — производная архива: повреждённый файл сохраняется для
            // разбора, а новый строится из источника истины.
            let aside = root.join(format!("{INDEX_FILE}.corrupt"));
            fs::rename(&path, &aside).map_err(|e| {
                Error::Store(format!("индекс повреждён ({error}) и не отложен: {e}"))
            })?;
            for suffix in ["-journal", "-wal", "-shm"] {
                let _ = fs::remove_file(root.join(format!("{INDEX_FILE}{suffix}")));
            }
            notes.push(format!(
                "индекс повреждён ({error}): он сохранён как {INDEX_FILE}.corrupt и перестроен из архива"
            ));
            Index::open(&path)
                .map(|(index, _)| index)
                .map_err(|e| Error::Store(format!("новый индекс не создан: {e}")))
        }
    }
}
