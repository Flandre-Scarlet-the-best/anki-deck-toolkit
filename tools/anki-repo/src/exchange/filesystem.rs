//! Linux dirfd + NOFOLLOW. `/proc/self/fd` адресует только собственный уже
//! проверенный дескриптор для std::read_dir и общего механизма атомарной записи проекта.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags, openat};

use super::{ExchangeError, Result};

const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 16384;

pub(crate) struct Directory(File);

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut current = Self(File::open("/")?);
        for component in absolute.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .ok_or_else(|| ExchangeError::UnsafePath("путь не UTF-8".into()))?;
                    current = current
                        .child(name)?
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                }
                _ => return Err(ExchangeError::UnsafePath("неоднозначный путь с ..".into())),
            }
        }
        Ok(current)
    }

    pub fn child(&self, name: &str) -> Result<Option<Self>> {
        plain_filename(name)?;
        match openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(Self(fd.into()))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(ExchangeError::UnsafePath(format!(
                "каталог {name:?}: {error}"
            ))),
        }
    }

    pub fn ensure_child(&self, name: &str) -> Result<(Self, bool)> {
        plain_filename(name)?;
        let created = match rustix::fs::mkdirat(&self.0, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        {
            Ok(()) => {
                self.0.sync_all()?;
                true
            }
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
        Ok((
            self.child(name)?
                .ok_or_else(|| ExchangeError::UnsafePath("исчез каталог".into()))?,
            created,
        ))
    }

    pub fn lock(&self) -> Result<File> {
        // Новое описание открытого файла: независимые ExchangeStore в одном процессе
        // сериализуются так же, как процессы, а Drop снимает ровно эту блокировку.
        let file: File = openat(
            &self.0,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?
        .into();
        file.lock()?;
        Ok(file)
    }

    /// Немедленный отказ при занятом владельце; общий lock сохраняет прежний контракт.
    pub fn try_lock(&self) -> Result<File> {
        let file: File = openat(
            &self.0,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?
        .into();
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                std::io::Error::from(std::io::ErrorKind::WouldBlock)
            }
            std::fs::TryLockError::Error(e) => e,
        })?;
        Ok(file)
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        plain_filename(name)?;
        let mut file: File = openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| ExchangeError::UnsafePath(format!("файл {name:?}: {e}")))?
        .into();
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_FILE_BYTES {
            return Err(ExchangeError::UnsafePath(format!(
                "не обычный единственный файл или превышен лимит: {name}"
            )));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(ExchangeError::UnsafePath("файл вырос сверх лимита".into()));
        }
        let after = file.metadata()?;
        let linked = fs::symlink_metadata(self.fd_path().join(name))?;
        if metadata.dev() != linked.dev()
            || metadata.ino() != linked.ino()
            || metadata.len() != after.len()
            || metadata.mtime_nsec() != after.mtime_nsec()
            || metadata.mtime() != after.mtime()
            || metadata.ctime_nsec() != after.ctime_nsec()
            || metadata.ctime() != after.ctime()
            || after.nlink() != 1
            || bytes.len() as u64 != after.len()
        {
            return Err(ExchangeError::UnsafePath(
                "файл изменился при чтении".into(),
            ));
        }
        Ok(bytes)
    }

    pub fn hash_files(&self) -> Result<BTreeMap<String, String>> {
        let mut hashes = BTreeMap::new();
        let mut total = 0;
        for entry in fs::read_dir(self.fd_path())? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ExchangeError::UnsafePath("имя media не UTF-8".into()))?;
            let bytes = self.read(&name)?;
            total += bytes.len() as u64;
            if total > MAX_TOTAL_BYTES || hashes.len() >= MAX_FILES {
                return Err(ExchangeError::UnsafePath(
                    "превышен лимит media снимка".into(),
                ));
            }
            hashes.insert(name, asset_store::hashing::sha256_hex(bytes));
        }
        Ok(hashes)
    }

    pub fn publish(&self, name: &str, bytes: &[u8]) -> Result<()> {
        plain_filename(name)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(ExchangeError::Blocked(format!(
                "метаданные требуют {} байт при лимите {MAX_FILE_BYTES} байт; состояние не записано",
                bytes.len()
            )));
        }
        let path = self.fd_path().join(name);
        // Не позволяем общему механизму записи следовать символической ссылке
        // даже при чтении прав доступа.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() || metadata.nlink() != 1 => {
                return Err(ExchangeError::UnsafePath(
                    "небезопасный файл метаданных".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        crate::write::replace_document_atomically(&path, bytes)?;
        // Обмен требует подтверждения синхронизации каталога после атомарной записи.
        // Ошибка после rename означает неизвестный результат: следующий шаг — read.
        self.0.sync_all()?;
        Ok(())
    }

    fn fd_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    pub fn is_same_child(&self, parent: &Self, name: &str) -> Result<bool> {
        let Some(current) = parent.child(name)? else {
            return Ok(false);
        };
        let current = current.0.metadata()?;
        let opened = self.0.metadata()?;
        Ok(current.dev() == opened.dev() && current.ino() == opened.ino())
    }

    pub fn require_private(&self) -> Result<()> {
        let metadata = self.0.metadata()?;
        let user = fs::metadata("/proc/self")?.uid();
        if metadata.mode() & 0o077 != 0 || metadata.uid() != user {
            return Err(ExchangeError::UnsafePath(
                "метаданные требуют собственного закрытого каталога (0700)".into(),
            ));
        }
        Ok(())
    }

    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(self.fd_path())? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| ExchangeError::UnsafePath("имя не UTF-8".into()))?;
            plain_filename(&name)?;
            names.push(name);
            if names.len() > MAX_FILES {
                return Err(ExchangeError::UnsafePath("слишком много файлов".into()));
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn sync(&self) -> Result<()> {
        self.0.sync_all()?;
        Ok(())
    }

    /// Только перемещение собственных детей; никогда rename поверх каталога.
    pub fn rename_child(&self, from: &str, to: &str) -> Result<()> {
        plain_filename(from)?;
        plain_filename(to)?;
        rustix::fs::renameat_with(
            &self.0,
            from,
            &self.0,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)?;
        self.sync()
    }

    pub fn remove_file(&self, name: &str) -> Result<()> {
        // read проверяет обычный файл и отсутствие hardlink/symlink.
        self.read(name)?;
        rustix::fs::unlinkat(&self.0, name, rustix::fs::AtFlags::empty())
            .map_err(std::io::Error::from)?;
        self.sync()
    }

    pub fn remove_tree(&self, name: &str) -> Result<()> {
        let Some(child) = self.child(name)? else {
            return Ok(());
        };
        for entry in child.names()? {
            // Каталоги снимка только media; никаких ссылок и специальных файлов.
            let path = child.fd_path().join(&entry);
            if fs::symlink_metadata(path)?.is_dir() {
                child.remove_tree(&entry)?;
            } else {
                child.remove_file(&entry)?;
            }
        }
        if !child.is_same_child(self, name)? {
            return Err(ExchangeError::UnsafePath("каталог подменён".into()));
        }
        rustix::fs::unlinkat(&self.0, name, rustix::fs::AtFlags::REMOVEDIR)
            .map_err(std::io::Error::from)?;
        self.sync()
    }
}

pub(super) fn plain_filename(name: &str) -> Result<()> {
    super::identity::validate_component(name)?;
    if name.len() > 255 {
        return Err(ExchangeError::UnsafePath(
            "слишком длинное имя файла".into(),
        ));
    }
    Ok(())
}
