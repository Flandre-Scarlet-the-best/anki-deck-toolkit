//! Наблюдения самого browser E2E для module-report benchmark-runner.
//! Это выборки /proc, а не замена системных метрик исполнителя: короткие
//! процессы могут не попасть в выборку, RSS дерева — сумма без дедупликации.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Clone)]
struct Process {
    pid: u32,
    parent: u32,
    start: u64,
    ticks: u64,
    rss: u64,
    hwm: u64,
}

fn processes() -> std::io::Result<Vec<Process>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        let number = |index: usize| fields.get(index).and_then(|v| v.parse::<u64>().ok());
        let (Some(parent), Some(user), Some(system), Some(start)) =
            (number(1), number(11), number(12), number(19))
        else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let kb = |name: &str| {
            status
                .lines()
                .find_map(|line| {
                    line.strip_prefix(name)?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
                .map(|v| v * 1024)
        };
        if let (Some(rss), Some(hwm)) = (kb("VmRSS:"), kb("VmHWM:")) {
            result.push(Process {
                pid,
                parent: parent as u32,
                start,
                ticks: user + system,
                rss,
                hwm,
            });
        }
    }
    Ok(result)
}

#[derive(Default)]
struct Tree {
    peaks: HashMap<(u32, u64), (u64, u64)>,
    rss_peak: u64,
    root_hwm: u64,
    samples: usize,
    rss_last: u64,
    processes_last: usize,
    trajectory: Vec<Value>,
}

impl Tree {
    fn sample(&mut self, root: u32, all: &[Process]) {
        let mut ids = HashSet::from([root]);
        loop {
            let previous = ids.len();
            for p in all {
                if ids.contains(&p.parent) {
                    ids.insert(p.pid);
                }
            }
            if ids.len() == previous {
                break;
            }
        }
        let mut rss = 0;
        let mut seen = false;
        for p in all.iter().filter(|p| ids.contains(&p.pid)) {
            seen = true;
            rss += p.rss;
            self.peaks
                .entry((p.pid, p.start))
                .and_modify(|(_, last)| *last = p.ticks)
                .or_insert((p.ticks, p.ticks));
            if p.pid == root {
                self.root_hwm = self.root_hwm.max(p.hwm);
            }
        }
        if seen {
            self.samples += 1;
            self.rss_peak = self.rss_peak.max(rss);
            self.rss_last = rss;
            self.processes_last = ids.len();
            if self.samples == 1 || self.samples.is_multiple_of(20) {
                self.trajectory
                    .push(json!({"sample":self.samples,"rss_sum_bytes":rss,"processes":ids.len()}));
            }
        }
    }

    fn report(&self) -> Value {
        if self.samples == 0 {
            return json!({"status":"unavailable","reason":"/proc не дал ни одной выборки живого процесса"});
        }
        json!({"status":"partial","samples":self.samples,
            "rss_sum_peak_bytes":self.rss_peak,"rss_sum_last_bytes":self.rss_last,"processes_last":self.processes_last,"trajectory":self.trajectory,"root_hwm_observed_bytes":self.root_hwm,
            "cpu_observed_ticks":self.peaks.values().map(|(first,last)|last-first).sum::<u64>(),
            "reason":"выборка 250 мс; CPU — прирост между первой и последней выборкой каждого PID/starttime; короткие процессы и края интервала могут отсутствовать; RSS суммирует shared pages"})
    }
}

pub(super) struct Observer {
    rust: u32,
    browser: u32,
    rust_tree: Tree,
    browser_tree: Tree,
    error: Option<String>,
}

impl Observer {
    pub fn new(rust: u32, browser: u32) -> Self {
        Self {
            rust,
            browser,
            rust_tree: Tree::default(),
            browser_tree: Tree::default(),
            error: None,
        }
    }
    pub fn sample(&mut self) {
        match processes() {
            Ok(all) => {
                self.rust_tree.sample(self.rust, &all);
                self.browser_tree.sample(self.browser, &all);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
    pub fn report(&self) -> Value {
        json!({"rust":self.rust_tree.report(),"browser":self.browser_tree.report(),"read_error":self.error,
            "cpu_unit":"CLK_TCK ядра; перевод в секунды требует getconf CLK_TCK"})
    }
}

pub(super) fn browser_snapshot(pid: u32) -> Value {
    match processes() {
        Ok(all) => {
            let mut tree = Tree::default();
            tree.sample(pid, &all);
            tree.report()
        }
        Err(e) => json!({"status":"unavailable","reason":e.to_string()}),
    }
}

/// Размер и состав только собственного каталога. Ошибка наблюдения — ошибка,
/// а не пустой каталог. Символические ссылки не обходятся.
pub(super) fn files(root: &Path) -> Result<Value, String> {
    fn walk(
        path: &Path,
        count: &mut u64,
        bytes: &mut u64,
        temporary: &mut u64,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.path().symlink_metadata()?;
            if metadata.is_dir() {
                walk(&entry.path(), count, bytes, temporary)?;
            } else if metadata.is_file() {
                *count += 1;
                *bytes += metadata.len();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".tmp") || name.contains(".tmp-") || name.ends_with(".crdownload")
                {
                    *temporary += 1;
                }
            }
        }
        Ok(())
    }
    let (mut count, mut bytes, mut temporary) = (0, 0, 0);
    walk(root, &mut count, &mut bytes, &mut temporary).map_err(|e| e.to_string())?;
    Ok(json!({"files":count,"bytes":bytes,"temporary":temporary}))
}
