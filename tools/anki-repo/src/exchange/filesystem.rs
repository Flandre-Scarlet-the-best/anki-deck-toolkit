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

pub(super) struct Directory(File);

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
