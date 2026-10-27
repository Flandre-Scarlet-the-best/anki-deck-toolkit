//! Запуск дочерних процессов серии с пределом времени, отменой и уборкой.
//!
//! Каждая команда получает собственную группу процессов: таймаут и отмена
//! завершают всю группу ступенчато (SIGINT, SIGTERM, SIGKILL), а не только
//! прямого потомка. Исполнитель объявляет себя subreaper-ом, поэтому
//! осиротевшие потомки, даже вышедшие из группы через `setsid`, становятся
//! его детьми: после команды они находятся, завершаются и дожидаются, а их
//! число сообщается как свидетельство нарушения границы прогона.

use rustix::process::{
    Pid, Signal, WaitOptions, getpid, kill_process, kill_process_group, set_child_subreaper,
    test_kill_process_group, waitpid,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Число полученных сигналов завершения: первый прерывает серию, второй —
/// и положенную уборку.
static SIGNALS: AtomicU32 = AtomicU32::new(0);
/// Дети, которых исполнитель запустил сам и ждёт через `std::process::Child`;
/// уборка сирот их не трогает.
static TRACKED: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

pub fn install_signal_handler() -> Result<(), String> {
    ctrlc::set_handler(|| {
        SIGNALS.fetch_add(1, Ordering::SeqCst);
    })
    .map_err(|e| format!("обработчик сигналов не установлен: {e}"))
}

/// Объявить исполнитель subreaper-ом для своих потомков.
pub fn become_subreaper() -> bool {
    set_child_subreaper(Some(getpid())).is_ok()
}

pub fn interrupted() -> bool {
    SIGNALS.load(Ordering::SeqCst) > 0
}

pub fn forced() -> bool {
    SIGNALS.load(Ordering::SeqCst) > 1
}

/// Чем закончилась команда.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Interrupted,
}

impl Ending {
    pub fn describe(self) -> String {
        match self {
            Ending::Exited(code) => format!("код завершения {code}"),
            Ending::Signaled(signal) => format!("завершён сигналом {signal}"),
            Ending::TimedOut => "превышен предел времени".to_owned(),
            Ending::Interrupted => "прерван сигналом исполнителю".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Finished {
    pub ending: Ending,
    pub elapsed: Duration,
    /// Сколько процессов команды пережили её и были завершены уборкой.
    pub leftovers: usize,
}

/// Как ждать команду.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub timeout: Duration,
    /// Сколько ждать штатного завершения после SIGINT и после SIGTERM.
    pub grace: Duration,
    /// Прерывать ли команду первым сигналом исполнителю. Уборка прерывается
    /// только повторным сигналом.
    pub interruptible: bool,
}

/// Запустить команду в собственной группе процессов и дождаться её.
pub fn run(command: &mut Command, bounds: Bounds) -> io::Result<Finished> {
    run_observed(command, bounds, |_| {})
}

/// То же, но сообщить PID сразу после запуска (например, сэмплеру).
pub fn run_observed(
    command: &mut Command,
    bounds: Bounds,
    on_spawn: impl FnOnce(i32),
) -> io::Result<Finished> {
    command.process_group(0);
    let started = Instant::now();
    let mut child = spawn_tracked(command)?;
    let pid = child.id() as i32;
    on_spawn(pid);
    let result = wait(&mut child, pid, started, bounds);
    untrack(pid);
    let ending = result?;
    let leftovers = sweep(pid);
    Ok(Finished {
        ending,
        elapsed: started.elapsed(),
        leftovers,
    })
}

/// Короткая служебная команда (Docker CLI, `--version`): вывод в память,
/// предел времени, без уборки чужих процессов.
pub fn output(command: &mut Command, timeout: Duration) -> io::Result<(Ending, Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let started = Instant::now();
    let mut child = spawn_tracked(command)?;
    let pid = child.id() as i32;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let bounds = Bounds {
        timeout,
        grace: Duration::from_secs(2),
        interruptible: true,
    };
    let result = wait(&mut child, pid, started, bounds);
    untrack(pid);
    let ending = result?;
    signal_group(pid, Signal::KILL);
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    Ok((ending, out, err))
}

/// Запуск и регистрация под одной блокировкой: иначе уборка из другого
/// потока успела бы принять только что запущенный процесс за сироту.
fn spawn_tracked(command: &mut Command) -> io::Result<Child> {
    let mut set = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    let child = command.spawn()?;
    set.insert(child.id() as i32);
    Ok(child)
}

fn untrack(pid: i32) {
    TRACKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&pid);
}

fn wait(child: &mut Child, pgid: i32, started: Instant, bounds: Bounds) -> io::Result<Ending> {
    let poll = Duration::from_millis(10);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(match (status.code(), status.signal()) {
                (Some(code), _) => Ending::Exited(code),
                (None, Some(signal)) => Ending::Signaled(signal),
                (None, None) => Ending::Signaled(0),
            });
        }
        let stop = if (bounds.interruptible && interrupted()) || forced() {
            Some(Ending::Interrupted)
        } else if started.elapsed() >= bounds.timeout {
            Some(Ending::TimedOut)
        } else {
            None
        };
        if let Some(ending) = stop {
            terminate(child, pgid, bounds.grace)?;
            return Ok(ending);
        }
        std::thread::sleep(poll);
    }
}

/// Ступенчатое завершение группы: SIGINT даёт команде штатно убрать за
/// собой (например, удалить контейнер), затем SIGTERM и SIGKILL.
fn terminate(child: &mut Child, pgid: i32, grace: Duration) -> io::Result<()> {
    for signal in [Signal::INT, Signal::TERM] {
        signal_group(pgid, signal);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            let exited = child.try_wait()?.is_some();
            if exited && !group_alive(pgid) {
                return Ok(());
            }
            if forced() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    signal_group(pgid, Signal::KILL);
    child.wait()?;
    Ok(())
}

fn signal_group(pgid: i32, signal: Signal) {
    if let Some(pid) = Pid::from_raw(pgid) {
        let _ = kill_process_group(pid, signal);
    }
}

/// Есть ли в группе живые процессы. Зомби не считаются: `kill(-pgid, 0)`
/// успешен и для них, а осиротевших зомби исполнитель дожидается здесь же.
fn group_alive(pgid: i32) -> bool {
    if !Pid::from_raw(pgid).is_some_and(|pid| test_kill_process_group(pid).is_ok()) {
        return false;
    }
    let me = std::process::id() as i32;
    let tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut alive = false;
    for (pid, info) in process_table() {
        if info.pgid != pgid {
            continue;
        }
        if info.state != 'Z' {
            alive = true;
        } else if info.ppid == me
            && !tracked.contains(&pid)
            && let Some(pid) = Pid::from_raw(pid)
        {
            let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        }
    }
    alive
}

/// Завершить и дождаться процессы, переживших команду: членов её группы и
/// осиротевших потомков, переданных исполнителю как subreaper-у.
fn sweep(pgid: i32) -> usize {
    let me = std::process::id() as i32;
    let mut killed = BTreeSet::new();
    for _ in 0..200 {
        let table = process_table();
        let tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut busy = false;
        for (pid, info) in &table {
            let orphan = info.ppid == me && !tracked.contains(pid);
            if !(orphan || info.pgid == pgid) {
                continue;
            }
            busy = true;
            if info.state == 'Z' {
                if orphan && let Some(pid) = Pid::from_raw(*pid) {
                    let _ = waitpid(Some(pid), WaitOptions::NOHANG);
                }
                continue;
            }
            killed.insert(*pid);
            if let Some(pid) = Pid::from_raw(*pid) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        if !busy {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    killed.len()
}

#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    pub ppid: i32,
    pub pgid: i32,
    pub state: char,
}

/// Снимок `/proc`: PID → родитель, группа, состояние.
pub fn process_table() -> BTreeMap<i32, ProcInfo> {
    let mut table = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(info) = proc_info(pid) {
            table.insert(pid, info);
        }
    }
    table
}

pub fn proc_info(pid: i32) -> Option<ProcInfo> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    Some(ProcInfo { ppid, pgid, state })
}

/// Процесс и все его потомки по текущему снимку `/proc`.
pub fn descendants(root: i32) -> BTreeSet<i32> {
    let table = process_table();
    let mut found = BTreeSet::from([root]);
    loop {
        let before = found.len();
        for (pid, info) in &table {
            if found.contains(&info.ppid) {
                found.insert(*pid);
            }
        }
        if found.len() == before {
            return found;
        }
    }
}
