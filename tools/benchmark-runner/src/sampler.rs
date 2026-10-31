//! Опрос системных счётчиков во время одного прогона.
//!
//! Поток сэмплера снимает первое значение до запуска прогона и последнее
//! после его завершения. Хост — `/proc/stat` и `/proc/meminfo`: это нагрузка
//! всей системы, а не измеряемой цели. Группа — либо собственная подгруппа
//! прогона, либо cgroup контейнера нагрузки, найденного по метке и PID
//! создателя. Контейнер Docker живёт вне группы исполнителя и удаляется до
//! конца прогона, поэтому его счётчики — последнее наблюдение до удаления.

use crate::cgroup::{self, Counters};
use crate::environment;
use crate::procs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct HostSample {
    pub at: f64,
    /// Занятые такты всех CPU (без idle и iowait).
    pub busy_ticks: u64,
    pub mem_available: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct GroupSample {
    pub at: f64,
    pub counters: Counters,
}

/// Что опрашивать в прогоне.
#[derive(Debug, Clone)]
pub struct Plan {
    pub host: bool,
    pub io: bool,
    pub tick: Duration,
    pub group: GroupSource,
}

#[derive(Debug, Clone)]
pub enum GroupSource {
    None,
    /// Собственная подгруппа прогона: последний снимок точен, группа
    /// существует и после выхода цели.
    Own(PathBuf),
    Container(ContainerQuery),
}

#[derive(Debug, Clone)]
pub struct ContainerQuery {
    pub label: String,
    pub owner_pid_label: String,
    pub docker_env: Vec<(String, String)>,
}

/// Наблюдения одного прогона.
#[derive(Debug, Clone, Default)]
pub struct Samples {
    pub host: Vec<HostSample>,
    pub group: Vec<GroupSample>,
    /// Контейнер нагрузки, если найден.
    pub container: Option<String>,
    /// Группа контейнера исчезла до конца прогона: последний снимок — это
    /// последнее наблюдение, а не точное итоговое значение.
    pub group_vanished: bool,
    /// Почему группа не наблюдалась.
    pub group_problem: Option<String>,
}

pub struct Handle {
    stop: Arc<AtomicBool>,
    root: Arc<AtomicI32>,
    shared: Arc<Mutex<Samples>>,
    thread: Option<JoinHandle<()>>,
    clock: Instant,
}

/// Начать опрос; первый снимок хоста берётся сразу, до запуска прогона.
pub fn start(plan: Plan) -> Handle {
    let stop = Arc::new(AtomicBool::new(false));
    let root = Arc::new(AtomicI32::new(0));
    let shared = Arc::new(Mutex::new(Samples::default()));
    let clock = Instant::now();
    if plan.host
        && let Some(sample) = host_sample(clock)
    {
        lock(&shared).host.push(sample);
    }
    let thread = {
        let (stop, root, shared) = (stop.clone(), root.clone(), shared.clone());
        std::thread::spawn(move || run(plan, clock, &stop, &root, &shared))
    };
    Handle {
        stop,
        root,
        shared,
        thread: Some(thread),
        clock,
    }
}

impl Handle {
    /// PID корня процессов прогона (процесс Hyperfine).
    pub fn set_root(&self, pid: i32) {
        self.root.store(pid, Ordering::SeqCst);
    }

    pub fn now(&self) -> f64 {
        self.clock.elapsed().as_secs_f64()
    }

    /// Остановить опрос, сняв последний снимок.
    pub fn finish(mut self) -> Samples {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        lock(&self.shared).clone()
    }
}

fn lock(shared: &Mutex<Samples>) -> std::sync::MutexGuard<'_, Samples> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn run(plan: Plan, clock: Instant, stop: &AtomicBool, root: &AtomicI32, shared: &Mutex<Samples>) {
    let mut container: Option<(String, PathBuf)> = None;
    let mut last_discovery: Option<Instant> = None;
    let discovery_every = Duration::from_millis(500);
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        if plan.host
            && let Some(sample) = host_sample(clock)
        {
            lock(shared).host.push(sample);
        }
        match &plan.group {
            GroupSource::None => {}
            GroupSource::Own(dir) => {
                if let Some(counters) = cgroup::read(dir, plan.io) {
                    lock(shared).group.push(GroupSample {
                        at: clock.elapsed().as_secs_f64(),
                        counters,
                    });
                }
            }
            GroupSource::Container(query) => {
                if let Some((_, dir)) = &container {
                    match cgroup::read(dir, plan.io) {
                        Some(counters) => lock(shared).group.push(GroupSample {
                            at: clock.elapsed().as_secs_f64(),
                            counters,
                        }),
                        None => lock(shared).group_vanished = true,
                    }
                } else if !stopping && last_discovery.is_none_or(|t| t.elapsed() >= discovery_every)
                {
                    last_discovery = Some(Instant::now());
                    let pid = root.load(Ordering::SeqCst);
                    if pid > 0 {
                        match discover(query, pid) {
                            Ok(Some(found)) => {
                                lock(shared).container = Some(found.0.clone());
                                container = Some(found);
                            }
                            Ok(None) => {}
                            Err(problem) => lock(shared).group_problem = Some(problem),
                        }
                    }
                }
            }
        }
        if stopping {
            return;
        }
        // Сон короткими шагами: остановка не ждёт целый интервал.
        let deadline = Instant::now() + plan.tick;
        while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5).min(plan.tick));
        }
    }
}

fn host_sample(clock: Instant) -> Option<HostSample> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = cpu
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal; guest уже входит в user.
    let busy = [0usize, 1, 2, 5, 6, 7]
        .iter()
        .filter_map(|i| fields.get(*i))
        .sum();
    let mem_available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("MemAvailable:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map(|kib| kib * 1024);
    Some(HostSample {
        at: clock.elapsed().as_secs_f64(),
        busy_ticks: busy,
        mem_available,
    })
}

/// Найти контейнер нагрузки этого прогона: метка цели, PID создателя среди
/// процессов прогона и группа, видимая из пространства имён исполнителя.
/// `Ok(None)` — контейнера пока нет; `Err` — найден, но измерять нельзя.
fn discover(query: &ContainerQuery, root: i32) -> Result<Option<(String, PathBuf)>, String> {
    let filter = format!("label={}", query.label);
    let (_, out, _) = docker(query, &["ps", "-q", "--no-trunc", "--filter", &filter])?;
    let ids: Vec<String> = String::from_utf8_lossy(&out)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    let ours = procs::descendants(root);
    let format = format!(
        "{{{{.Id}}}} {{{{.State.Pid}}}} {{{{index .Config.Labels \"{}\"}}}}",
        query.owner_pid_label
    );
    let mut args = vec!["inspect", "--format", &format];
    args.extend(ids.iter().map(String::as_str));
    let (_, out, _) = docker(query, &args)?;
    let mut owned = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), Some(pid), owner) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if owner
            .and_then(|o| o.parse::<i32>().ok())
            .is_some_and(|o| ours.contains(&o))
        {
            owned.push((id.to_owned(), pid.to_owned()));
        }
    }
    match owned.as_slice() {
        [] => Ok(None),
        [(id, pid)] => {
            if pid == "0" {
                return Ok(None);
            }
            let path = cgroup::of_process(pid)
                .ok_or("группа процесса контейнера не читается из /proc".to_owned())?;
            if !path.contains(id.as_str()) {
                return Err("группа процесса контейнера не содержит его ID: охват контейнера не подтверждён (драйвер cgroup или namespace не опознаны)".to_owned());
            }
            let dir = cgroup::dir_of(&path).ok_or(
                "группа контейнера вне видимой иерархии cgroup исполнителя".to_owned(),
            )?;
            Ok(Some((id.clone(), dir)))
        }
        _ => Err(
            "прогону принадлежат несколько контейнеров с меткой нагрузки: какой измерять, не определено"
                .into(),
        ),
    }
}

fn docker(
    query: &ContainerQuery,
    args: &[&str],
) -> Result<(procs::Ending, Vec<u8>, Vec<u8>), String> {
    let mut command = environment::docker_command(&query.docker_env, args);
    match procs::output(&mut command, Duration::from_secs(10)) {
        Ok((procs::Ending::Exited(0), out, err)) => Ok((procs::Ending::Exited(0), out, err)),
        Ok((ending, _, err)) => Err(format!(
            "docker {}: {}; {}",
            args.first().unwrap_or(&""),
            ending.describe(),
            String::from_utf8_lossy(&err).trim()
        )),
        Err(e) => Err(format!("docker не запускается: {e}")),
    }
}

/// Наибольшая скорость роста счётчика за окно не короче `interval`.
/// `None` — наблюдений не хватает на одно окно.
pub fn peak_rate(points: &[(f64, f64)], interval: f64) -> Option<f64> {
    let mut best: Option<f64> = None;
    for (i, &(t_i, v_i)) in points.iter().enumerate() {
        let start = points[..i]
            .iter()
            .rev()
            .find(|(t_j, _)| t_i - t_j >= interval * 0.999);
        if let Some(&(t_j, v_j)) = start {
            let rate = (v_i - v_j).max(0.0) / (t_i - t_j);
            best = Some(best.map_or(rate, |b: f64| b.max(rate)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_rate_uses_full_windows_only() {
        let points = [(0.0, 0.0), (0.5, 1.0), (1.0, 1.0), (1.5, 3.0), (2.0, 3.0)];
        assert_eq!(peak_rate(&points, 1.0), Some(2.0));
        assert_eq!(peak_rate(&points[..2], 1.0), None);
    }
}
