//! cgroup v2: делегированная группа исполнителя и чтение счётчиков групп.
//!
//! Наличие `/sys/fs/cgroup/cgroup.controllers` значит лишь, что единая
//! иерархия смонтирована. Исполнителю нужно больше: создать подгруппу в своей
//! группе, включить в ней контроллеры `cpu` и `memory` и поместить туда
//! процесс. Каждая ступень проверяется действием, а её отказ называется
//! отдельно. Корень и `sudo` не используются: подходит группа, которую
//! делегировал пользователю systemd (`systemd-run --user --scope -p Delegate=yes`)
//! или окружение контейнера.

use rustix::time::{ClockId, clock_gettime};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/sys/fs/cgroup";

/// Имя подгруппы, в которую исполнитель переносит себя: в cgroup v2 группа
/// с процессами не может раздавать контроллеры подгруппам.
const SELF_LEAF: &str = "benchmark-runner";

pub fn mounted() -> bool {
    Path::new(ROOT).join("cgroup.controllers").is_file()
}

/// Путь группы процесса в единой иерархии (`0::<путь>`).
pub fn of_process(pid: &str) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
}

/// Каталог группы по её пути, если путь виден из пространства имён
/// исполнителя (путь вне его cgroup namespace начинается с `/..`).
pub fn dir_of(path: &str) -> Option<PathBuf> {
    if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return None;
    }
    let dir = Path::new(ROOT).join(path.trim_start_matches('/'));
    dir.is_dir().then_some(dir)
}

fn controllers(dir: &Path, file: &str) -> Vec<String> {
    fs::read_to_string(dir.join(file))
        .map(|t| t.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Группа исполнителя, в которой он может создавать измеряемые подгруппы.
#[derive(Debug, Clone)]
pub struct Delegated {
    pub base: PathBuf,
}

/// Проверить, можно ли создавать подгруппы с `cpu` и `memory`, не меняя
/// ничего, кроме пробной подгруппы, которая сразу удаляется.
pub fn probe() -> Result<Delegated, String> {
    if !mounted() {
        return Err("единая иерархия cgroup v2 не смонтирована в /sys/fs/cgroup".into());
    }
    let path = of_process("self").ok_or("группа процесса не читается из /proc/self/cgroup")?;
    let base = dir_of(&path).ok_or(
        "группа исполнителя вне видимой иерархии (другое cgroup namespace): её счётчики недоступны",
    )?;
    let available = if base.join(SELF_LEAF).is_dir() {
        controllers(&base, "cgroup.subtree_control")
    } else {
        controllers(&base, "cgroup.controllers")
    };
    for needed in ["cpu", "memory"] {
        if !available.iter().any(|c| c == needed) {
            return Err(format!(
                "контроллер {needed} не делегирован группе исполнителя (есть: {})",
                available.join(" ")
            ));
        }
    }
    let probe = base.join(format!("benchmark-probe-{}", std::process::id()));
    fs::create_dir(&probe).map_err(|e| {
        format!("нет права создавать подгруппы в группе исполнителя: {e}; без root группу делегирует systemd-run --user --scope -p Delegate=yes")
    })?;
    let _ = fs::remove_dir(&probe);
    // activate() включает контроллеры для подгрупп, а в некорневой группе это
    // возможно, только если в ней не останется процессов, кроме исполнителя.
    if !base.join(SELF_LEAF).is_dir() {
        let me = std::process::id().to_string();
        let others = fs::read_to_string(base.join("cgroup.procs"))
            .map(|t| t.lines().filter(|p| p.trim() != me).count())
            .unwrap_or(usize::MAX);
        if others > 0 {
            return Err("в группе исполнителя есть посторонние процессы: контроллеры для подгрупп не включатся; группу без них даёт systemd-run --user --scope -p Delegate=yes".into());
        }
    }
    Ok(Delegated { base })
}

impl Delegated {
    /// Перенести исполнитель в собственную подгруппу и включить контроллеры
    /// для измеряемых подгрупп.
    pub fn activate(&self) -> Result<(), String> {
        let leaf = self.base.join(SELF_LEAF);
        if !leaf.is_dir() {
            fs::create_dir(&leaf).map_err(|e| format!("подгруппа исполнителя не создана: {e}"))?;
        }
        fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()).map_err(|e| {
            format!("исполнитель не переносится в свою подгруппу (нет права на cgroup.procs): {e}")
        })?;
        fs::write(self.base.join("cgroup.subtree_control"), "+cpu +memory").map_err(|e| {
            format!("контроллеры cpu и memory не включаются для подгрупп: {e}; в группе есть посторонние процессы или контроллеры не делегированы")
        })
    }

    pub fn create(&self, name: &str) -> io::Result<RunGroup> {
        let dir = self.base.join(name);
        fs::create_dir(&dir)?;
        Ok(RunGroup { dir })
    }
}

/// Измеряемая подгруппа одного прогона.
#[derive(Debug, Clone)]
pub struct RunGroup {
    pub dir: PathBuf,
}

impl RunGroup {
    pub fn populated(&self) -> bool {
        events(&self.dir).is_some_and(|populated| populated)
    }

    /// Завершить все процессы группы (`cgroup.kill`) и дождаться пустоты.
    pub fn kill(&self) -> bool {
        if !self.populated() {
            return false;
        }
        let _ = fs::write(self.dir.join("cgroup.kill"), "1");
        for _ in 0..500 {
            if !self.populated() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    pub fn remove(&self) {
        let _ = fs::remove_dir(&self.dir);
    }
}

fn events(dir: &Path) -> Option<bool> {
    let text = fs::read_to_string(dir.join("cgroup.events")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("populated "))
        .map(|v| v.trim() == "1")
}

/// Счётчики группы в момент чтения.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Counters {
    pub usage_usec: Option<u64>,
    pub memory_peak: Option<u64>,
}

/// `None` — группы уже нет.
pub fn read(dir: &Path) -> Option<Counters> {
    let stat = fs::read_to_string(dir.join("cpu.stat")).ok()?;
    let usage_usec = stat
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok());
    let memory_peak = fs::read_to_string(dir.join("memory.peak"))
        .ok()
        .and_then(|v| v.trim().parse().ok());
    Some(Counters {
        usage_usec,
        memory_peak,
    })
}

/// Монотонное время в наносекундах, общее для родителя и его процессов.
pub fn monotonic_ns() -> u128 {
    let now = clock_gettime(ClockId::Monotonic);
    (now.tv_sec as u128) * 1_000_000_000 + (now.tv_nsec as u128)
}

/// Длительность от метки, записанной обёрткой после входа в cgroup.
pub fn span_since_marker(marker: &Path, ended_ns: u128) -> Option<f64> {
    let started_at = fs::read_to_string(marker).ok()?;
    span_from_marker_text(&started_at, ended_ns)
}

fn span_from_marker_text(started_at: &str, ended_ns: u128) -> Option<f64> {
    let started_ns = started_at.trim().parse().ok()?;
    span_seconds(started_ns, ended_ns)
}

fn span_seconds(started_ns: u128, ended_ns: u128) -> Option<f64> {
    Some(ended_ns.checked_sub(started_ns)? as f64 / 1_000_000_000.0)
}

/// Обёртка измеряемой цели: поместить себя в группу и заменить себя целью.
/// PID сохраняется, поэтому Hyperfine измеряет тот же процесс.
pub fn enter_and_exec(
    group: &Path,
    started_at: &Path,
    program: &str,
    args: &[String],
) -> io::Error {
    use std::os::unix::process::CommandExt;
    if let Err(error) = fs::write(group.join("cgroup.procs"), "0") {
        return io::Error::new(
            error.kind(),
            format!("процесс не помещён в группу прогона: {error}"),
        );
    }
    let mut marker = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(started_at)
    {
        Ok(marker) => marker,
        Err(error) => {
            return io::Error::new(
                error.kind(),
                format!("время входа в группу не записано: {error}"),
            );
        }
    };
    if let Err(error) = writeln!(marker, "{}", monotonic_ns()) {
        return io::Error::new(
            error.kind(),
            format!("время входа в группу не записано: {error}"),
        );
    }
    drop(marker);
    std::process::Command::new(program).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_uses_the_monotonic_start_marker() {
        assert_eq!(
            span_from_marker_text("1000000000\n", 2_250_000_000),
            Some(1.25)
        );
        assert_eq!(span_from_marker_text("invalid", 2_250_000_000), None);
        assert_eq!(span_seconds(2, 1), None);
    }
}
