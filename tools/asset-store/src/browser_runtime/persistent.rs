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
const ACTIVE_WORKSPACE: &str = ".forvo-browser-workspace";
const ACTIVE_WORKSPACE_TMP: &str = ".forvo-browser-workspace.tmp";

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
        // После аварии предыдущего владельца flock уже освобождён, но Chromium
        // мог пережить процесс CLI. Его профиль нельзя открыть повторно.
        crate::temp_workspace::TempWorkspace::ensure_no_live_process_references(path).map_err(
            |_| {
                io::Error::other("browser_profile_busy: предыдущий Chromium ещё использует профиль")
            },
        )?;
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
        let profile = Self {
            path: path.to_path_buf(),
            _lock: lock,
        };
        profile.check_previous_workspace()?;
        match fs::remove_file(profile.path.join(ACTIVE_WORKSPACE_TMP)) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        Ok(profile)
    }

    /// Маркер создаётся до запуска процесса и переживает аварийное завершение CLI.
    /// Он связывает профиль с TMPDIR Chromium, включая оставшихся потомков.
    pub(super) fn register_workspace(&self, workspace: &Path) -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.path.join(ACTIVE_WORKSPACE_TMP))?;
        file.write_all(workspace.as_os_str().as_encoded_bytes())?;
        file.sync_all()?;
        fs::rename(
            self.path.join(ACTIVE_WORKSPACE_TMP),
            self.path.join(ACTIVE_WORKSPACE),
        )?;
        File::open(&self.path)?.sync_all()
    }

    fn check_previous_workspace(&self) -> io::Result<()> {
        let marker = self.path.join(ACTIVE_WORKSPACE);
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&marker)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::getuid().as_raw()
            || meta.mode() & 0o077 != 0
            || meta.len() == 0
            || meta.len() > 4096
        {
            return Err(io::Error::other(
                "browser_profile_busy: недостоверный маркер прежнего браузера",
            ));
        }
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        let workspace = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        if !workspace.is_absolute()
            || workspace
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(io::Error::other(
                "browser_profile_busy: недостоверный путь прежнего браузера",
            ));
        }
        match fs::symlink_metadata(&workspace) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                crate::temp_workspace::TempWorkspace::ensure_no_live_process_references(&workspace)
                    .map_err(|_| {
                        io::Error::other(
                            "browser_profile_busy: прежний Chromium ещё использует временные файлы",
                        )
                    })?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => {
                return Err(io::Error::other(
                    "browser_profile_busy: недостоверная временная область",
                ));
            }
        }
        fs::remove_file(marker)?;
        File::open(&self.path)?.sync_all()
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

    #[test]
    fn previous_workspace_blocks_reopening_after_lock_owner_is_gone() {
        let workspace = TempWorkspace::create("persistent-profile-stale-owner").unwrap();
        let browser_tree = TempWorkspace::create("persistent-browser-tree").unwrap();
        let path = workspace.path().join("profile");
        let profile = PersistentProfile::open(&path).unwrap();
        profile.register_workspace(browser_tree.path()).unwrap();
        let used = browser_tree.path().join("in-use");
        fs::write(&used, "active").unwrap();
        let mut process = std::process::Command::new("sh")
            .args(["-c", "exec 3< \"$1\"; read line", "forvo-test"])
            .arg(&used)
            .arg(format!("--user-data-dir={}", browser_tree.path().display()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !Path::new(&format!("/proc/{}/fd/3", process.id())).exists()
            || !TempWorkspace::has_live_process_references(browser_tree.path()).unwrap()
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(profile);
        assert!(PersistentProfile::open(&path).is_err());
        process.kill().unwrap();
        process.wait().unwrap();
        let reopened = PersistentProfile::open(&path).unwrap();
        assert!(!path.join(ACTIVE_WORKSPACE).exists());
        drop(reopened);
        browser_tree.close().unwrap();
        workspace.close().unwrap();
    }
}
