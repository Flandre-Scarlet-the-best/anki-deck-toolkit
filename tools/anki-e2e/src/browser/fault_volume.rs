//! Владение выделенным томом `cli_batch_enospc`: доказательство, что заполняемая
//! файловая система — отдельная ограниченная tmpfs, исключительная межпроцессная
//! блокировка тома и очистка только созданного этим запуском — при успехе,
//! ошибке и отмене future.
//!
//! Граница доверия: проверяется пространство имён монтирования текущего процесса.
//! Копии того же суперблока в чужих пространствах имён отсюда не видны; рецепт
//! монтирования в документации создаёт tmpfs под приватным родителем, чтобы
//! таких копий не возникало. `Drop` не выполняется при SIGKILL или аварии
//! процесса: тогда том остаётся непустым, следующий запуск отказывает, а
//! восстановление — размонтирование выделенной tmpfs оператором.

use super::scenarios::Result;
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Верхняя граница ёмкости: заполняется только маленький выделенный том.
pub(super) const MAX_CAPACITY: u64 = 64 * 1024 * 1024;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const STORE: &str = "store";
const RESERVATION: &str = "reservation";
/// Сколько ждать завершения отслеживаемого CLI после SIGKILL при аварийной очистке.
const CHILD_EXIT_LIMIT: Duration = Duration::from_secs(10);
/// Общие системные tmpfs не заполняются, даже если выглядят отдельными.
const SHARED_MOUNTS: [&str; 8] = [
    "/",
    "/tmp",
    "/var/tmp",
    "/dev",
    "/dev/shm",
    "/run",
    "/run/lock",
    "/run/shm",
];
const SHARED_PREFIXES: [&str; 4] = ["/run/user/", "/proc/", "/sys/", "/dev/"];

/// Строка `/proc/self/mountinfo` (proc(5)) — только нужные поля.
#[derive(Debug, PartialEq)]
struct MountEntry {
    device: (u32, u32),
    root: String,
    mount_point: String,
    propagation: Vec<String>,
    fs_type: String,
}

/// Декодирование `\ooo`, которым ядро экранирует пробел, табуляцию, перевод строки и `\`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(code) = bytes
                .get(i + 1..i + 4)
                .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)))
                .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok())
        {
            out.push(code);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_line(line: &str) -> Option<MountEntry> {
    let fields: Vec<&str> = line.split(' ').collect();
    let separator = fields.iter().position(|f| *f == "-")?;
    if separator < 6 {
        return None;
    }
    let (major, minor) = fields[2].split_once(':')?;
    Some(MountEntry {
        device: (major.parse().ok()?, minor.parse().ok()?),
        root: unescape(fields[3]),
        mount_point: unescape(fields[4]),
        propagation: fields[6..separator].iter().map(|f| f.to_string()).collect(),
        fs_type: fields.get(separator + 1)?.to_string(),
    })
}

/// Доказательство по mountinfo, что `mount_point` — корень единственного видимого
/// монтирования отдельной tmpfs с устройством `device`, без распространения.
fn assess_mountinfo(mountinfo: &str, mount_point: &Path, device: (u32, u32)) -> Result<()> {
    let point = mount_point
        .to_str()
        .ok_or("путь fault-volume не UTF-8: mountinfo не сопоставляется надёжно")?;
    if SHARED_MOUNTS.contains(&point)
        || SHARED_PREFIXES
            .iter()
            .any(|p| point.starts_with(p) || format!("{point}/") == *p)
    {
        return Err(format!(
            "fault-volume {point} — общий системный том; заполняется только выделенная tmpfs"
        ));
    }
    let entries: Vec<MountEntry> = mountinfo.lines().filter_map(parse_line).collect();
    let Some(entry) = entries
        .iter()
        .find(|e| e.mount_point == point && e.device == device)
    else {
        return Err(format!(
            "{point} не является корнем собственного монтирования (нет записи mountinfo с тем же устройством)"
        ));
    };
    if entry.fs_type != "tmpfs" {
        return Err(format!(
            "fault-volume смонтирован как {}, нужна отдельная tmpfs",
            entry.fs_type
        ));
    }
    if entry.root != "/" {
        return Err(format!(
            "fault-volume — bind-монтирование каталога {} другой tmpfs; её место делят посторонние файлы",
            entry.root
        ));
    }
    let views = entries.iter().filter(|e| e.device == device).count();
    if views != 1 {
        return Err(format!(
            "файловая система fault-volume видна в {views} точках монтирования; заполняется только единственное монтирование"
        ));
    }
    if let Some(tag) = entry
        .propagation
        .iter()
        .find(|t| t.starts_with("shared:") || t.starts_with("master:"))
    {
        return Err(format!(
            "fault-volume распространяется между пространствами имён ({tag}); смонтируйте его под приватным родителем"
        ));
    }
    Ok(())
}

fn errno(e: rustix::io::Errno) -> String {
    std::io::Error::from(e).to_string()
}

/// Исключительная межпроцессная блокировка открытого описания каталога: файлов не создаёт.
fn lock_exclusive(dir: &OwnedFd) -> Result<()> {
    rustix::fs::flock(dir, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            "fault-volume занят другим запуском; параллельное заполнение одного тома запрещено"
                .to_owned()
        } else {
            format!("блокировка fault-volume: {}", errno(e))
        }
    })
}

fn open_locked(path: &Path) -> Result<(PathBuf, OwnedFd)> {
    if path
        .symlink_metadata()
        .map_err(|e| format!("fault-volume: {e}"))?
        .file_type()
        .is_symlink()
    {
        return Err("fault-volume не должен быть символической ссылкой".into());
    }
    let path = std::fs::canonicalize(path).map_err(|e| format!("fault-volume: {e}"))?;
    let dir = rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("fault-volume: {}", errno(e)))?;
    // Блокировка берётся до проверки пустоты и держится до конца очистки.
    lock_exclusive(&dir)?;
    Ok((path, dir))
}

fn ensure_empty(dir: &OwnedFd) -> Result<()> {
    let entries = rustix::fs::Dir::read_from(dir).map_err(errno)?;
    for entry in entries {
        let entry = entry.map_err(errno)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            return Err("fault-volume не пуст; чужие файлы не изменяются. После прерванного прогона размонтируйте выделенную tmpfs и смонтируйте заново".into());
        }
    }
    Ok(())
}

/// Путь всё ещё указывает на тот же каталог, что открыт и заблокирован.
fn same_directory(path: &Path, dir: &OwnedFd) -> bool {
    match (rustix::fs::stat(path), rustix::fs::fstat(dir)) {
        (Ok(a), Ok(b)) => a.st_dev == b.st_dev && a.st_ino == b.st_ino,
        _ => false,
    }
}

/// Ёмкость тома после всех проверок выделенности.
fn verify_dedicated(path: &Path, dir: &OwnedFd) -> Result<u64> {
    let stat = rustix::fs::fstat(dir).map_err(errno)?;
    if !same_directory(path, dir) {
        return Err("fault-volume заменён во время проверки".into());
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
        return Err("корень fault-volume должен принадлежать текущему пользователю с режимом 0700: посторонние процессы не должны писать на том".into());
    }
    let fs = rustix::fs::fstatfs(dir).map_err(errno)?;
    let capacity = (fs.f_bsize as u64).checked_mul(fs.f_blocks as u64);
    #[allow(clippy::unnecessary_cast)]
    if fs.f_type as u64 != TMPFS_MAGIC || !capacity.is_some_and(|n| n > 0 && n <= MAX_CAPACITY) {
        return Err("fault-volume должен быть отдельным tmpfs с ёмкостью <=64 MiB; обычный диск не заполняется".into());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
    let device = (
        rustix::fs::major(stat.st_dev),
        rustix::fs::minor(stat.st_dev),
    );
    assess_mountinfo(&mountinfo, path, device)?;
    Ok(capacity.unwrap_or_default())
}

pub(super) struct FaultVolume {
    path: PathBuf,
    /// Открытый каталог тома: держит flock и привязывает заполнение к проверенной ФС.
    dir: OwnedFd,
    capacity: u64,
    reservation: bool,
    store_cleared: bool,
    /// pidfd дочерних CLI, которые пишут в `store`.
    children: Vec<OwnedFd>,
    evidence: PathBuf,
}

impl FaultVolume {
    /// Исключительно занимает пустую выделенную tmpfs. Всё, что появится в ней
    /// далее, создано этим запуском; `evidence` — куда сохранить store перед удалением.
    pub(super) fn acquire(path: &Path, evidence: PathBuf) -> Result<Self> {
        let (path, dir) = open_locked(path)?;
        let capacity = verify_dedicated(&path, &dir)?;
        ensure_empty(&dir)?;
        Ok(Self::owned(path, dir, capacity, evidence))
    }

    fn owned(path: PathBuf, dir: OwnedFd, capacity: u64, evidence: PathBuf) -> Self {
        Self {
            path,
            dir,
            capacity,
            reservation: false,
            store_cleared: false,
            children: Vec::new(),
            evidence,
        }
    }

    pub(super) fn capacity(&self) -> u64 {
        self.capacity
    }

    pub(super) fn store(&self) -> PathBuf {
        self.path.join(STORE)
    }

    /// pidfd берётся, пока процесс не дождан: PID ещё не может быть переиспользован.
    pub(super) fn track(&mut self, child: &tokio::process::Child) -> Result<()> {
        let pid = child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32))
            .ok_or("процесс CLI уже завершён")?;
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(|e| format!("pidfd CLI: {}", errno(e)))?;
        self.children.push(pidfd);
        Ok(())
    }

    /// Заполняет том до настоящей ошибки записи; возвращает её. Перед заполнением
    /// выделенность проверяется повторно, а запись идёт через открытый каталог.
    pub(super) fn fill(&mut self) -> Result<std::io::Error> {
        verify_dedicated(&self.path, &self.dir)?;
        self.fill_up_to(self.capacity + 1024 * 1024)
    }

    /// Запись ограничена `limit`: если том принимает больше заявленной ёмкости,
    /// заполнение останавливается, а не продолжается в неизвестную ФС.
    fn fill_up_to(&mut self, limit: u64) -> Result<std::io::Error> {
        let fd = rustix::fs::openat(
            &self.dir,
            RESERVATION,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| format!("reservation: {}", errno(e)))?;
        self.reservation = true;
        let mut file = std::fs::File::from(fd);
        let block = [0_u8; 4096];
        let mut written = 0_u64;
        loop {
            match file.write(&block) {
                Ok(n) => {
                    written += n as u64;
                    if written > limit {
                        return Err(format!(
                            "том принял больше {limit} байт без ENOSPC; заполнение остановлено"
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(e) => return Ok(e),
            }
        }
    }

    pub(super) fn release_reservation(&mut self) -> Result<()> {
        if self.reservation {
            rustix::fs::unlinkat(&self.dir, RESERVATION, AtFlags::empty())
                .map_err(|e| format!("удаление reservation: {}", errno(e)))?;
            self.reservation = false;
        }
        Ok(())
    }

    /// Штатное завершение: store копируется в каталог прогона и удаляется с тома.
    pub(super) fn save_and_clear_store(&mut self) -> Result<()> {
        if !self.save_store() {
            return Err("не удалось сохранить store ENOSPC в каталоге прогона".into());
        }
        if !same_directory(&self.path, &self.dir) {
            return Err("fault-volume заменён: store не удаляется".into());
        }
        std::fs::remove_dir_all(self.store()).map_err(|e| e.to_string())?;
        self.store_cleared = true;
        Ok(())
    }

    fn save_store(&self) -> bool {
        !self.evidence.exists()
            && std::process::Command::new("cp")
                .args(["-a", "--"])
                .arg(self.store())
                .arg(&self.evidence)
                .status()
                .is_ok_and(|s| s.success())
    }

    pub(super) fn is_empty(&self) -> Result<bool> {
        Ok(ensure_empty(&self.dir).is_ok())
    }

    /// Все отслеживаемые CLI завершены: SIGKILL через pidfd (без риска переиспользования PID)
    /// и ожидание выхода с ограничением.
    fn stop_children(&self) -> bool {
        self.children.iter().all(|pidfd| {
            match rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => (),
                Err(_) => return false,
            }
            let mut fds = [rustix::event::PollFd::new(
                pidfd,
                rustix::event::PollFlags::IN,
            )];
            let limit = rustix::time::Timespec {
                tv_sec: CHILD_EXIT_LIMIT.as_secs() as i64,
                tv_nsec: 0,
            };
            rustix::event::poll(&mut fds, Some(&limit)).is_ok_and(|n| n == 1)
        })
    }
}

impl Drop for FaultVolume {
    fn drop(&mut self) {
        // reservation пишет только этот процесс, поэтому удаляется всегда.
        if let Err(e) = self.release_reservation() {
            eprintln!("cli_batch_enospc: {e}");
        }
        if self.store_cleared
            || rustix::fs::statat(self.dir.as_fd(), STORE, AtFlags::SYMLINK_NOFOLLOW).is_err()
        {
            return;
        }
        if !self.stop_children() {
            eprintln!(
                "cli_batch_enospc: CLI не завершился за {} с; store на fault-volume оставлен",
                CHILD_EXIT_LIMIT.as_secs()
            );
            return;
        }
        if !same_directory(&self.path, &self.dir) {
            eprintln!("cli_batch_enospc: fault-volume заменён; store не удаляется");
            return;
        }
        if !self.save_store() {
            eprintln!(
                "cli_batch_enospc: store не сохранён в {}; оставлен на fault-volume как свидетельство",
                self.evidence.display()
            );
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(self.store()) {
            eprintln!("cli_batch_enospc: удаление store: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEDICATED: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
30 22 0:40 / /tmp rw,nosuid,nodev shared:5 - tmpfs tmpfs rw
41 30 0:52 / /tmp/fault\\040volume rw,nosuid,nodev - tmpfs tmpfs rw,size=8192k,mode=700
";

    fn point() -> &'static Path {
        Path::new("/tmp/fault volume")
    }

    struct TempDirectory(PathBuf);
    impl TempDirectory {
        fn create(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "anki-e2e-fault-volume-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Каталог на обычной ФС без проверок выделенности: заполнение в тестах
    /// ограничено несколькими КиБ.
    fn test_volume(dir: &Path, evidence: PathBuf) -> FaultVolume {
        let (path, fd) = open_locked(dir).unwrap();
        ensure_empty(&fd).unwrap();
        FaultVolume::owned(path, fd, 0, evidence)
    }

    #[test]
    fn dedicated_private_tmpfs_with_escaped_path_is_accepted() {
        assert_eq!(assess_mountinfo(DEDICATED, point(), (0, 52)), Ok(()));
    }

    #[test]
    fn bind_mount_of_a_directory_on_shared_tmpfs_is_rejected() {
        let info = "30 22 0:40 / /tmp rw - tmpfs tmpfs rw\n\
                    41 30 0:40 /shared/sub /mnt/fault rw - tmpfs tmpfs rw\n";
        let error = assess_mountinfo(info, Path::new("/mnt/fault"), (0, 40)).unwrap_err();
        assert!(error.contains("bind-монтирование"), "{error}");
    }

    #[test]
    fn bind_mount_of_a_whole_tmpfs_visible_elsewhere_is_rejected() {
        let info = "30 22 0:40 / /srv/data rw - tmpfs tmpfs rw\n\
                    41 30 0:40 / /mnt/fault rw - tmpfs tmpfs rw\n";
        let error = assess_mountinfo(info, Path::new("/mnt/fault"), (0, 40)).unwrap_err();
        assert!(error.contains("2 точках"), "{error}");
    }

    #[test]
    fn propagated_mounts_are_rejected() {
        for tag in ["shared:7", "master:3"] {
            let info = format!("41 30 0:52 / /mnt/fault rw {tag} - tmpfs tmpfs rw\n");
            let error = assess_mountinfo(&info, Path::new("/mnt/fault"), (0, 52)).unwrap_err();
            assert!(error.contains(tag), "{error}");
        }
    }

    #[test]
    fn system_shared_tmpfs_is_rejected_even_when_it_looks_dedicated() {
        for point in ["/tmp", "/dev/shm", "/run/user/1000", "/run/user"] {
            let info = format!("41 30 0:52 / {point} rw - tmpfs tmpfs rw\n");
            let error = assess_mountinfo(&info, Path::new(point), (0, 52)).unwrap_err();
            assert!(error.contains("общий системный"), "{point}: {error}");
        }
    }

    #[test]
    fn non_tmpfs_and_foreign_device_are_rejected() {
        let ext4 = "41 30 8:2 / /mnt/fault rw - ext4 /dev/sda2 rw\n";
        assert!(
            assess_mountinfo(ext4, Path::new("/mnt/fault"), (8, 2))
                .unwrap_err()
                .contains("ext4")
        );
        // Каталог внутри родительской ФС: путь не корень собственного монтирования.
        let error = assess_mountinfo(DEDICATED, Path::new("/tmp/other"), (0, 40)).unwrap_err();
        assert!(error.contains("не является корнем"), "{error}");
        // Переподключённая поверх точка: запись с иным устройством не подходит.
        let error = assess_mountinfo(DEDICATED, point(), (0, 99)).unwrap_err();
        assert!(error.contains("не является корнем"), "{error}");
    }

    #[test]
    fn current_process_mountinfo_is_parseable() {
        let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(
            info.lines()
                .filter_map(parse_line)
                .any(|e| e.mount_point == "/")
        );
    }

    #[test]
    fn regular_directory_is_refused_before_any_file_is_created() {
        let dir = TempDirectory::create("regular");
        let error = FaultVolume::acquire(&dir.0, dir.0.join("evidence"))
            .err()
            .unwrap();
        assert!(!error.is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn second_holder_of_the_same_volume_is_refused_until_release() {
        let dir = TempDirectory::create("lock");
        let first = test_volume(&dir.0, dir.0.join("unused"));
        // flock принадлежит открытому описанию файла, как у другого процесса.
        let error = open_locked(&dir.0).err().unwrap();
        assert!(error.contains("занят другим запуском"), "{error}");
        // Сама блокировка файлов не создаёт: проверка пустоты не ломается.
        assert!(first.is_empty().unwrap());
        drop(first);
        assert!(open_locked(&dir.0).is_ok());
    }

    #[test]
    fn nonempty_volume_is_refused_and_foreign_files_are_kept() {
        let dir = TempDirectory::create("foreign");
        std::fs::write(dir.0.join("foreign"), b"x").unwrap();
        let (_, fd) = open_locked(&dir.0).unwrap();
        assert!(ensure_empty(&fd).unwrap_err().contains("не пуст"));
        drop(fd);
        assert_eq!(std::fs::read(dir.0.join("foreign")).unwrap(), b"x");
    }

    #[test]
    fn fill_stops_at_the_limit_instead_of_running_away() {
        let dir = TempDirectory::create("limit");
        let mut volume = test_volume(&dir.0, dir.0.join("unused"));
        let error = volume.fill_up_to(8192).unwrap_err();
        assert!(error.contains("заполнение остановлено"), "{error}");
        drop(volume);
        assert!(!dir.0.join(RESERVATION).exists());
    }

    #[tokio::test]
    async fn cancelled_scenario_stops_the_child_and_removes_only_its_own_files() {
        let dir = TempDirectory::create("cancel");
        let evidence_root = TempDirectory::create("cancel-evidence");
        let evidence = evidence_root.0.join("store-evidence");
        let mut volume = test_volume(&dir.0, evidence.clone());
        let store = volume.store();
        let pid = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = pid.clone();
        let scenario = async move {
            let _ = volume.fill_up_to(8192);
            std::fs::create_dir_all(store.join("nested")).unwrap();
            std::fs::write(store.join("nested/state.json"), b"{}").unwrap();
            let mut child = tokio::process::Command::new("sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            *observed.lock().unwrap() = child.id();
            volume.track(&child).unwrap();
            let _ = child.wait().await;
            unreachable!("сценарий отменяется до завершения CLI");
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(200), scenario)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(evidence.join("nested/state.json")).unwrap(),
            b"{}"
        );
        let pid = pid.lock().unwrap().unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        assert!(
            status.is_empty() || status.contains("State:\tZ"),
            "CLI ещё работает: {status}"
        );
    }

    #[test]
    fn error_after_reservation_releases_space_even_without_store() {
        let dir = TempDirectory::create("error");
        let scenario = || -> Result<()> {
            let mut volume = test_volume(&dir.0, dir.0.join("unused"));
            let _ = volume.fill_up_to(8192);
            assert!(dir.0.join(RESERVATION).exists());
            Err("ранняя ошибка сценария".into())
        };
        let result = scenario();
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
