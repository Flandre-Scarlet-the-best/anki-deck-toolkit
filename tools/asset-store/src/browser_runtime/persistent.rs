//! Постоянный профиль приложения: отдельный каталог, маркер и advisory lock.
use rustix::fs::{FlockOperation, flock};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(super) struct PersistentProfile {
    pub path: PathBuf,
    _lock: File,
}
const OWNER: &str = "anki-asset-store/forvo-browser-profile/v1\n";

impl PersistentProfile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let fail = || {
            io::Error::other("browser_profile_not_owned: нужен отдельный приватный профиль Forvo")
        };
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(fail());
        }
        // Не следуем symlink ни одного компонента, в том числе при создании родителей.
        let mut current = PathBuf::from("/");
        for component in path.components().skip(1) {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => (),
                Ok(_) => return Err(fail()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::DirBuilder::new().mode(0o700).create(&current)?;
                }
                Err(e) => return Err(e),
            }
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| io::Error::other("browser_profile_busy: закройте другой процесс Forvo"))?;
        let metadata = lock.metadata()?;
        if metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(fail());
        }
        let marker = path.join(".forvo-owner");
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&marker)
        {
            Ok(file) => {
                let mut value = String::new();
                file.take(128).read_to_string(&mut value)?;
                if value != OWNER {
                    return Err(fail());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::read_dir(path)?.next().is_some() {
                    return Err(fail());
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(marker)?;
                file.write_all(OWNER.as_bytes())?;
                file.sync_all()?;
            }
            Err(e) => return Err(e),
        }
        Ok(Self {
            path: path.to_path_buf(),
            _lock: lock,
        })
    }
}

use std::os::unix::fs::DirBuilderExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp_workspace::TempWorkspace;
    #[test]
    fn profile_is_private_exclusive_persistent_and_never_adopts_a_normal_profile() {
        let workspace = TempWorkspace::create("persistent-profile-test").unwrap();
        let path = workspace.path().join("forvo");
        let profile = PersistentProfile::open(&path).unwrap();
        fs::write(path.join("session-marker"), b"not a cookie").unwrap();
        assert!(PersistentProfile::open(&path).is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(profile);
        let reopened = PersistentProfile::open(&path).unwrap();
        assert_eq!(
            fs::read(path.join("session-marker")).unwrap(),
            b"not a cookie"
        );
        drop(reopened);
        let normal = workspace.path().join("normal");
        fs::create_dir(&normal).unwrap();
        fs::set_permissions(&normal, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(normal.join("Preferences"), "{}").unwrap();
        assert!(PersistentProfile::open(&normal).is_err());
        let link = workspace.path().join("linked");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(PersistentProfile::open(&link).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PersistentProfile::open(&path).is_err());
        workspace.close().unwrap();
    }
}
