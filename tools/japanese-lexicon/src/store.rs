//! Локальный каталог данных подсистемы: расположение, принадлежность,
//! блокировки, реестр активных версий и манифесты.
//!
//! ```text
//! <корень>/
//! ├── owner.json                     # маркер: каталог принадлежит japanese-lexicon
//! ├── operations.lock                # flock: одна изменяющая операция за раз
//! ├── active.lock                    # flock: чтение реестра и смена активных версий
//! ├── registry.json                  # активная версия каждого установленного словаря
//! ├── dictionaries/<id>/<версия>/    # archive.zip, index.sqlite, manifest.json
//! └── staging/<случайное имя>/       # незавершённые загрузки и сборки
//! ```
//!
//! Новая версия собирается в `staging`, переносится в `dictionaries/<id>/`
//! и становится видимой только атомарной заменой `registry.json`. Поиск
//! берёт разделяемую блокировку `active.lock` лишь на время чтения реестра
//! и открытия индексов; смена версии и удаление ждут её исключительно.

use crate::error::{Code, Error, Result};
use crate::index::BuildStats;
use crate::upstream::{FileDigest, RemoteArtifact};
use crate::yomitan::IndexMeta;
use rustix::fs::FlockOperation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsFd;
use std::path::{Component, Path, PathBuf};

pub const DIR_ENV: &str = "JAPANESE_LEXICON_DIR";
pub const DATA_SUBDIR: &str = "anki-deck-toolkit/japanese-lexicon";
pub const OWNER_FILE: &str = "owner.json";
pub const REGISTRY_FILE: &str = "registry.json";
pub const OPERATIONS_LOCK: &str = "operations.lock";
pub const ACTIVE_LOCK: &str = "active.lock";
pub const DICTIONARIES_DIR: &str = "dictionaries";
pub const STAGING_DIR: &str = "staging";
pub const ARCHIVE_FILE: &str = "archive.zip";
pub const INDEX_FILE: &str = "index.sqlite";
pub const MANIFEST_FILE: &str = "manifest.json";

const OWNER_SCHEMA: &str = "japanese-lexicon.owner/1";
pub const REGISTRY_SCHEMA: &str = "japanese-lexicon.registry/1";
pub const MANIFEST_SCHEMA: &str = "japanese-lexicon.manifest/1";

/// Откуда взят корень: виден пользователю в `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Option,
    Environment,
    XdgDataHome,
    Home,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub root: PathBuf,
    pub origin: Origin,
}

/// Корень: `--data-dir`, затем [`DIR_ENV`], затем XDG.
pub fn resolve(explicit: Option<&Path>) -> Result<Location> {
    resolve_with(explicit, &|name| std::env::var_os(name))
}

pub fn resolve_with(
    explicit: Option<&Path>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Location> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let location = if let Some(path) = explicit {
        Location {
            root: path.to_path_buf(),
            origin: Origin::Option,
        }
    } else if let Some(path) = set(DIR_ENV) {
        Location {
            root: path,
            origin: Origin::Environment,
        }
    } else if let Some(data) = set("XDG_DATA_HOME").filter(|p| p.is_absolute()) {
        Location {
            root: data.join(DATA_SUBDIR),
            origin: Origin::XdgDataHome,
        }
    } else if let Some(home) = set("HOME").filter(|p| p.is_absolute()) {
        Location {
            root: home.join(".local/share").join(DATA_SUBDIR),
            origin: Origin::Home,
        }
    } else {
        return Err(Error::new(
            Code::InvalidInput,
            format!(
                "каталог данных не определён: нет XDG_DATA_HOME и HOME; укажите --data-dir или {DIR_ENV}"
            ),
        ));
    };
    if !location.root.is_absolute() {
        return Err(Error::new(
            Code::InvalidInput,
            format!(
                "каталог данных должен быть абсолютным: {}",
                location.root.display()
            ),
        ));
    }
    if location
        .root
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return Err(Error::new(
            Code::InvalidInput,
            format!("каталог данных {} содержит «..»", location.root.display()),
        ));
    }
    Ok(location)
}

/// Словари — локальные данные пользователя: каталог внутри рабочей копии
/// Git отвергается до создания файлов (проверяется ближайший существующий
/// предок с разрешёнными ссылками).
fn refuse_inside_git(path: &Path) -> Result<()> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent.to_path_buf(),
            None => break,
        }
    }
    let resolved = probe.canonicalize().unwrap_or(probe);
    if let Some(work_tree) = resolved
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
    {
        return Err(Error::new(
            Code::NotOwned,
            format!(
                "каталог данных {} лежит внутри рабочей копии Git {}: словари не должны попадать в Git; выберите каталог вне репозитория",
                path.display(),
                work_tree.display()
            ),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Owner {
    schema: String,
    owner: String,
}

/// Реестр активных версий: единственный источник истины об установленном.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registry {
    pub schema: String,
    pub dictionaries: BTreeMap<String, RegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryEntry {
    pub version: String,
    pub activated_at: String,
}

/// Описание установленной версии словаря.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub dictionary_id: String,
    pub role: crate::catalog::Role,
    /// Идентичность поставленного артефакта у upstream.
    pub artifact: RemoteArtifact,
    /// Хеши фактически полученного архива.
    pub archive: FileDigest,
    /// Сверена ли опубликованная upstream контрольная сумма.
    pub published_checksum_verified: bool,
    /// Совпала ли `revision` из `index.json` с тегом релиза (если источник это обещает).
    pub revision_matches_release: Option<bool>,
    pub index_json: IndexMeta,
    pub corpus: CorpusInfo,
    pub checked_at: String,
    pub installed_at: String,
    pub license: String,
    pub attribution: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusInfo {
    pub schema: u32,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub stats: BuildStats,
}

/// Удерживаемая блокировка; снимается при закрытии файла.
pub struct LockGuard {
    _file: File,
}

fn lock(path: &Path, operation: FlockOperation) -> Result<LockGuard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::io(path.display(), e))?;
    match rustix::fs::flock(file.as_fd(), operation) {
        Ok(()) => Ok(LockGuard { _file: file }),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(Error::new(
            Code::Busy,
            "другая операция установки, обновления или удаления уже выполняется над этим каталогом данных; повторите после её завершения",
        )),
        Err(e) => Err(Error::io(path.display(), e.into())),
    }
}

/// Записывает файл атомарно: временный файл, fsync, rename, fsync каталога.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::new(Code::Io, format!("{} без родителя", path.display())))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        random_id()?
    ));
    let result = (|| {
        let mut file = File::create_new(&tmp).map_err(|e| Error::io(tmp.display(), e))?;
        file.write_all(bytes)
            .map_err(|e| Error::io(tmp.display(), e))?;
        file.sync_all().map_err(|e| Error::io(tmp.display(), e))?;
        fs::rename(&tmp, path).map_err(|e| Error::io(path.display(), e))?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| Error::io(dir.display(), e))
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::new(Code::Io, format!("нет источника случайности: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Допустимое имя каталога версии: защищает от записи реестра с `..`.
fn valid_version(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Существующий каталог данных или `None`, если его ещё нет.
    /// Чужой каталог (без маркера) отвергается.
    pub fn open_existing(root: &Path) -> Result<Option<Store>> {
        if !root.exists() {
            return Ok(None);
        }
        let store = Store {
            root: root.to_path_buf(),
        };
        store.check_owner()?;
        Ok(Some(store))
    }

    /// Каталог для изменяющих операций: создаётся при первом использовании.
    /// Непустой каталог без маркера не присваивается.
    pub fn open_or_init(root: &Path) -> Result<Store> {
        refuse_inside_git(root)?;
        let store = Store {
            root: root.to_path_buf(),
        };
        if root.exists() {
            if !root.join(OWNER_FILE).exists() {
                // Временный файл маркера параллельной инициализации — не чужие данные.
                let owner_tmp = format!(".{OWNER_FILE}.");
                let empty = fs::read_dir(root)
                    .map_err(|e| Error::io(root.display(), e))?
                    .filter_map(|e| e.ok())
                    .all(|e| e.file_name().to_string_lossy().starts_with(&owner_tmp));
                if empty {
                    store.write_owner()?;
                } else if store.check_owner().is_err() {
                    // Маркер мог появиться после первой проверки от параллельной
                    // инициализации; без корректного маркера каталог чужой.
                    return Err(Error::new(
                        Code::NotOwned,
                        format!(
                            "каталог {} не пуст и не принадлежит japanese-lexicon (нет {OWNER_FILE}); выберите пустой или новый каталог",
                            root.display()
                        ),
                    ));
                }
            }
            store.check_owner()?;
        } else {
            fs::create_dir_all(root).map_err(|e| Error::io(root.display(), e))?;
            store.write_owner()?;
        }
        for dir in [DICTIONARIES_DIR, STAGING_DIR] {
            let path = root.join(dir);
            fs::create_dir_all(&path).map_err(|e| Error::io(path.display(), e))?;
        }
        Ok(store)
    }

    fn write_owner(&self) -> Result<()> {
        let owner = Owner {
            schema: OWNER_SCHEMA.into(),
            owner: "japanese-lexicon".into(),
        };
        write_atomic(
            &self.root.join(OWNER_FILE),
            &serde_json::to_vec_pretty(&owner).unwrap_or_default(),
        )
    }

    fn check_owner(&self) -> Result<()> {
        let path = self.root.join(OWNER_FILE);
        let owner: Option<Owner> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        match owner {
            Some(o) if o.schema == OWNER_SCHEMA && o.owner == "japanese-lexicon" => Ok(()),
            _ => Err(Error::new(
                Code::NotOwned,
                format!(
                    "каталог {} не принадлежит japanese-lexicon: нет корректного {OWNER_FILE}",
                    self.root.display()
                ),
            )),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dictionary_dir(&self, id: &str) -> PathBuf {
        self.root.join(DICTIONARIES_DIR).join(id)
    }

    pub fn version_dir(&self, id: &str, version: &str) -> Result<PathBuf> {
        if !valid_version(version) || crate::catalog::find(id).is_none() {
            return Err(Error::corrupt(format!(
                "реестр ссылается на недопустимую версию {id}/{version}"
            )));
        }
        Ok(self.dictionary_dir(id).join(version))
    }

    /// Исключительная блокировка изменяющих операций (без ожидания).
    pub fn lock_operations(&self) -> Result<LockGuard> {
        lock(
            &self.root.join(OPERATIONS_LOCK),
            FlockOperation::NonBlockingLockExclusive,
        )
    }

    /// Разделяемая блокировка чтения реестра (ждёт смену версии).
    pub fn lock_read(&self) -> Result<LockGuard> {
        lock(&self.root.join(ACTIVE_LOCK), FlockOperation::LockShared)
    }

    /// Исключительная блокировка смены активных версий (ждёт читателей).
    pub fn lock_activate(&self) -> Result<LockGuard> {
        lock(&self.root.join(ACTIVE_LOCK), FlockOperation::LockExclusive)
    }

    pub fn registry(&self) -> Result<Registry> {
        let path = self.root.join(REGISTRY_FILE);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Registry {
                    schema: REGISTRY_SCHEMA.into(),
                    dictionaries: BTreeMap::new(),
                });
            }
            Err(e) => return Err(Error::io(path.display(), e)),
        };
        let registry: Registry = serde_json::from_slice(&bytes)
            .map_err(|e| Error::corrupt(format!("{} не читается: {e}", path.display())))?;
        if registry.schema != REGISTRY_SCHEMA {
            return Err(Error::corrupt(format!(
                "{}: неизвестная схема {}",
                path.display(),
                registry.schema
            )));
        }
        Ok(registry)
    }

    pub fn write_registry(&self, registry: &Registry) -> Result<()> {
        let mut registry = registry.clone();
        registry.schema = REGISTRY_SCHEMA.into();
        write_atomic(
            &self.root.join(REGISTRY_FILE),
            &serde_json::to_vec_pretty(&registry).unwrap_or_default(),
        )
    }

    pub fn read_manifest(&self, id: &str, version: &str) -> Result<Manifest> {
        let path = self.version_dir(id, version)?.join(MANIFEST_FILE);
        let bytes = fs::read(&path)
            .map_err(|e| Error::corrupt(format!("манифест {} не читается: {e}", path.display())))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| Error::corrupt(format!("манифест {} повреждён: {e}", path.display())))?;
        if manifest.schema != MANIFEST_SCHEMA || manifest.dictionary_id != id {
            return Err(Error::corrupt(format!(
                "манифест {} не относится к словарю {id}",
                path.display()
            )));
        }
        Ok(manifest)
    }

    /// Новый пустой каталог в staging.
    pub fn new_staging(&self) -> Result<PathBuf> {
        let path = self.root.join(STAGING_DIR).join(random_id()?);
        fs::create_dir_all(&path).map_err(|e| Error::io(path.display(), e))?;
        Ok(path)
    }

    /// Остатки прерванных операций: каталоги staging и версии вне реестра.
    pub fn leftovers(&self, registry: &Registry) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let staging = self.root.join(STAGING_DIR);
        if staging.is_dir() {
            for entry in fs::read_dir(&staging).map_err(|e| Error::io(staging.display(), e))? {
                found.push(entry.map_err(|e| Error::io(staging.display(), e))?.path());
            }
        }
        for source in crate::catalog::standard() {
            let dir = self.dictionary_dir(source.id);
            if !dir.is_dir() {
                continue;
            }
            let active = registry
                .dictionaries
                .get(source.id)
                .map(|e| e.version.as_str());
            for entry in fs::read_dir(&dir).map_err(|e| Error::io(dir.display(), e))? {
                let entry = entry.map_err(|e| Error::io(dir.display(), e))?;
                if entry.file_name().to_str() != active {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Записи корня, которые не принадлежат раскладке подсистемы. Они не
    /// удаляются ни при каких операциях.
    pub fn foreign_entries(&self) -> Result<Vec<PathBuf>> {
        let known = [
            OWNER_FILE,
            REGISTRY_FILE,
            OPERATIONS_LOCK,
            ACTIVE_LOCK,
            DICTIONARIES_DIR,
            STAGING_DIR,
        ];
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|e| Error::io(self.root.display(), e))? {
            let entry = entry.map_err(|e| Error::io(self.root.display(), e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let atomic_tmp = name.starts_with('.') && name.ends_with(".tmp");
            if !known.contains(&name.as_ref()) && !atomic_tmp {
                found.push(entry.path());
            }
        }
        let dictionaries = self.root.join(DICTIONARIES_DIR);
        if dictionaries.is_dir() {
            for entry in
                fs::read_dir(&dictionaries).map_err(|e| Error::io(dictionaries.display(), e))?
            {
                let entry = entry.map_err(|e| Error::io(dictionaries.display(), e))?;
                if crate::catalog::find(&entry.file_name().to_string_lossy()).is_none() {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        Ok(found)
    }
}

/// Удаляет каталог, принадлежащий подсистеме. Символическая ссылка на месте
/// каталога удаляется как ссылка: обход её цели исключён.
pub fn remove_owned_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|e| Error::io(path.display(), e))
        }
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).map_err(|e| Error::io(path.display(), e))
        }
        Ok(_) => fs::remove_file(path).map_err(|e| Error::io(path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(path.display(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn root_resolution_order() {
        let all = env(&[(DIR_ENV, "/e"), ("XDG_DATA_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(
            resolve_with(Some(Path::new("/o")), &all).unwrap().root,
            PathBuf::from("/o")
        );
        assert_eq!(resolve_with(None, &all).unwrap().root, PathBuf::from("/e"));
        let xdg = env(&[("XDG_DATA_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(
            resolve_with(None, &xdg).unwrap().root,
            PathBuf::from("/x").join(DATA_SUBDIR)
        );
        let rel = env(&[("XDG_DATA_HOME", "x"), ("HOME", "/h")]);
        assert_eq!(
            resolve_with(None, &rel).unwrap().root,
            PathBuf::from("/h/.local/share").join(DATA_SUBDIR)
        );
        assert!(resolve_with(Some(Path::new("rel")), &all).is_err());
        assert!(resolve_with(Some(Path::new("/a/../b")), &all).is_err());
    }

    #[test]
    fn version_names_are_restricted() {
        assert!(valid_version("0123abcd-0011"));
        assert!(!valid_version("../x"));
        assert!(!valid_version(""));
        assert!(!valid_version("A"));
    }
}
