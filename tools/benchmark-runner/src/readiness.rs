//! Реальная готовность окружения к серии: проверки действием.
//!
//! В отличие от `benchmark-contract preflight`, который только читает файлы,
//! здесь запускаются настоящие пробы: Hyperfine измеряет пустую команду с
//! запрошенным набором метрик, Docker отвечает демоном, создаётся пробная
//! подгруппа cgroup, офлайн-проба смотрит интерфейсы изнутри нового сетевого
//! пространства. Отказ обязательной возможности делает серию `unsupported`,
//! отказ необязательной — заранее известную причину отсутствия метрики.

use crate::cgroup::{self, Delegated, IoControl};
use crate::environment::docker_server_version;
use crate::hyperfine_exec::{self, Hyperfine, which};
use crate::offline::{self, Isolation};
use crate::plan::Selection;
use crate::procs;
use benchmark_contract::scenario::{MetricRequest, Program, Requirement};
use benchmark_contract::vocabulary::{MetricId, Os, Scope, Source};
use benchmark_history::reason;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Переменная окружения, по которой исполнитель знает, что уже перезапущен
/// в делегированной группе systemd, и не перезапускается повторно.
pub const SCOPE_ENV: &str = "BENCHMARK_RUNNER_DELEGATED_SCOPE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Available,
    Missing,
    Restricted,
    Unsupported,
    NotChecked,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub requirement: String,
    pub status: Status,
    pub detail: String,
}

/// Кто снимает метрику.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Collector {
    Hyperfine,
    HostProcfs,
    OwnCgroup,
    ContainerCgroup,
    ModuleReport,
    ModuleLog,
    Filesystem,
    ProcessProcfs,
}

pub fn collector(selection: &Selection<'_>, request: &MetricRequest) -> Collector {
    match (request.source, request.scope) {
        (Source::Hyperfine, _) => Collector::Hyperfine,
        (Source::Procfs, Scope::Host) => Collector::HostProcfs,
        (Source::Procfs, _) => Collector::ProcessProcfs,
        (Source::CgroupV2, _) if selection.item.scenario.container.is_some() => {
            Collector::ContainerCgroup
        }
        (Source::CgroupV2, _) => Collector::OwnCgroup,
        (Source::ModuleReport, _) => Collector::ModuleReport,
        (Source::ModuleLog, _) => Collector::ModuleLog,
        (Source::Filesystem, _) => Collector::Filesystem,
    }
}

/// Почему метрика не будет измерена. `unsupported` — окружение или модуль
/// не дают источника; иначе значение не получено по решению запуска.
#[derive(Debug, Clone)]
pub struct Missing {
    pub unsupported: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum OwnCgroup {
    NotNeeded,
    Ready(Delegated),
    /// Делегированная группа доступна после перезапуска через systemd-run;
    /// состояние io проверено активацией в одноразовой группе.
    ViaScope(PathBuf, IoControl),
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct Readiness {
    pub checks: Vec<Check>,
    /// Причины, по которым серия здесь не может измеряться.
    pub blockers: Vec<String>,
    /// Заранее известные причины отсутствия метрик по ключу метрики.
    pub unavailable: BTreeMap<String, Missing>,
    pub hyperfine: Option<Hyperfine>,
    pub hyperfine_metrics: Vec<&'static str>,
    pub isolation: Option<Isolation>,
    pub own_cgroup: OwnCgroup,
    pub docker: bool,
}

impl Readiness {
    pub fn ready(&self) -> bool {
        self.blockers.is_empty()
    }

    fn push(&mut self, requirement: &str, status: Status, detail: impl Into<String>) {
        self.checks.push(Check {
            requirement: requirement.to_owned(),
            status,
            detail: detail.into(),
        });
    }

    fn block(&mut self, requirement: &str, status: Status, detail: impl Into<String>) {
        let detail = detail.into();
        self.blockers.push(format!("{requirement}: {detail}"));
        self.push(requirement, status, detail);
    }
}

pub fn assess(selection: &Selection<'_>, runner: &Path, sampling: bool) -> Readiness {
    let scenario = &selection.item.scenario;
    let mut r = Readiness {
        checks: Vec::new(),
        blockers: Vec::new(),
        unavailable: BTreeMap::new(),
        hyperfine: None,
        hyperfine_metrics: Vec::new(),
        isolation: None,
        own_cgroup: OwnCgroup::NotNeeded,
        docker: false,
    };

    if std::env::consts::OS == "linux" && scenario.platforms.contains(&Os::Linux) {
        r.push(
            "platform",
            Status::Available,
            "linux входит в platforms сценария",
        );
    } else {
        r.block(
            "platform",
            Status::Unsupported,
            format!(
                "исполнитель измеряет только на Linux, а сценарий объявлен для: {}",
                scenario
                    .platforms
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        return r;
    }

    match hyperfine_exec::detect() {
        Ok(found) => {
            r.push(
                "hyperfine",
                Status::Available,
                format!("Hyperfine {} (формат экспорта 2)", found.version),
            );
            probe_hyperfine_metrics(&mut r, selection, &found, runner);
            r.hyperfine = Some(found);
        }
        Err(reason) => r.block("hyperfine", Status::Missing, reason),
    }

    for program in selection.programs() {
        match program {
            Program::CargoBin { .. } => {
                if which("cargo").is_none() {
                    r.block(
                        "executable:cargo",
                        Status::Missing,
                        "cargo не найден в PATH: cargo-bin не собирается",
                    );
                }
            }
            Program::Executable { name } => {
                if which(name).is_none() {
                    r.block(
                        &format!("executable:{name}"),
                        Status::Missing,
                        format!("{name} не найден в PATH"),
                    );
                }
            }
        }
    }

    let needs_docker =
        scenario.requires.contains(&Requirement::Docker) || scenario.container.is_some();
    let mut cgroup_restricted = None;
    for requirement in &scenario.requires {
        match requirement {
            Requirement::Executable { name } => {
                if which(name).is_none() {
                    r.block(
                        &format!("executable:{name}"),
                        Status::Missing,
                        format!("{name} не найден в PATH"),
                    );
                }
            }
            // Демон проверяется с окружением цели: цель и сэмплер обращаются
            // к Docker только с ним, и запасного демона исполнителя нет.
            Requirement::Docker => match selection
                .environment(&scenario.target.env)
                .map_err(|e| format!("окружение цели не разрешается: {e}"))
                .and_then(|env| docker_server_version(&env))
            {
                Ok(version) => {
                    r.docker = true;
                    r.push(
                        "docker",
                        Status::Available,
                        format!("демон Docker отвечает, версия {version}"),
                    );
                }
                Err(reason) => r.block(
                    "docker",
                    Status::Missing,
                    format!("демон Docker недоступен в окружении цели: {reason}"),
                ),
            },
            Requirement::LinuxCgroupV2 => match cgroup::probe() {
                Ok(_) => r.push(
                    "linux-cgroup-v2",
                    Status::Available,
                    "подгруппы с cpu и memory создаются",
                ),
                Err(reason) if scenario.container.is_some() && cgroup::mounted() => r.push(
                    "linux-cgroup-v2",
                    Status::Available,
                    format!("иерархия смонтирована; измеряется группа контейнера ({reason})"),
                ),
                // Решается после оценки собственной группы: run может получить
                // делегированную группу перезапуском в systemd-run.
                Err(reason) => cgroup_restricted = Some(reason),
            },
            Requirement::PerfEvents => match perf_paranoid() {
                Some(level) if level <= 2 => r.push(
                    "perf-events",
                    Status::Available,
                    format!("perf_event_paranoid = {level}"),
                ),
                Some(level) => r.block(
                    "perf-events",
                    Status::Restricted,
                    format!(
                        "perf_event_paranoid = {level}: счётчики процессора недоступны без прав"
                    ),
                ),
                None => r.block("perf-events", Status::Missing, "perf_event недоступен"),
            },
            Requirement::Network { purpose, profiles } => {
                if profiles.contains(&selection.binding.id) {
                    r.push(
                        "network",
                        Status::NotChecked,
                        format!("{purpose}; сеть не проверяется без обращения к ней"),
                    );
                }
            }
        }
    }

    if selection.offline() {
        if needs_docker {
            r.block(
                "offline-isolation",
                Status::Unsupported,
                "сетевое пространство имён серии не охватывает демона Docker и BuildKit, которые работают в сети хоста: отсутствие сети у нагрузки не обеспечено, поэтому офлайн-серия не запускается",
            );
        } else {
            match offline::probe(runner) {
                Ok(isolation) => {
                    r.push(
                        "offline-isolation",
                        Status::Available,
                        "серия исполняется в новом сетевом пространстве имён: виден только выключенный lo",
                    );
                    r.isolation = Some(isolation);
                }
                Err(reason) => r.block("offline-isolation", Status::Unsupported, reason),
            }
        }
    }

    let mut own_needed = false;
    for request in &scenario.metrics {
        let key = request.key();
        let reason = match collector(selection, request) {
            Collector::Hyperfine => None,
            Collector::HostProcfs if !sampling => Some(
                "сэмплирование отключено параметром --no-sampling".to_owned(),
            ),
            Collector::HostProcfs => (!Path::new("/proc/stat").is_file())
                .then(|| "/proc/stat недоступен".to_owned()),
            Collector::ProcessProcfs => Some(
                "исполнитель не читает счётчики процесса из /proc: процесс цели дожидается Hyperfine, после выхода его /proc уже недоступен".to_owned(),
            ),
            Collector::ModuleLog => Some(
                "модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста".to_owned(),
            ),
            Collector::ModuleReport | Collector::Filesystem => None,
            Collector::ContainerCgroup if !sampling => Some(reason::CONTAINER_NO_SAMPLING.text()),
            Collector::ContainerCgroup if !cgroup::mounted() => {
                Some(reason::CGROUP_NOT_MOUNTED.text())
            }
            Collector::ContainerCgroup if !r.docker => {
                Some(reason::DOCKER_UNAVAILABLE.text())
            }
            Collector::ContainerCgroup => None,
            Collector::OwnCgroup => {
                own_needed = true;
                // Пик загрузки — единственная метрика группы из выборки;
                // остальные читаются после прогона.
                (!sampling && request.metric == MetricId::CpuUtilizationPeak).then(|| {
                    "сэмплирование отключено параметром --no-sampling".to_owned()
                })
            }
        };
        if let Some(reason) = reason {
            if request.required {
                r.block(
                    &format!("metric:{key}"),
                    Status::Unsupported,
                    reason.clone(),
                );
            }
            let unsupported = !reason.contains("--no-sampling");
            r.unavailable.insert(
                key,
                Missing {
                    unsupported,
                    reason,
                },
            );
        }
    }
    if own_needed {
        r.own_cgroup = own_cgroup(runner);
        match &r.own_cgroup {
            OwnCgroup::Ready(_) => r.push(
                "cgroup-delegation",
                Status::Available,
                "подгруппы прогонов создаются в группе исполнителя",
            ),
            OwnCgroup::ViaScope(_, _) => r.push(
                "cgroup-delegation",
                Status::Available,
                "группа исполнителя не делегирована; run перезапустится в systemd-run --user --scope -p Delegate=yes",
            ),
            OwnCgroup::Unavailable(reason) => {
                let reason = reason.clone();
                for request in scenario
                    .metrics
                    .iter()
                    .filter(|m| collector(selection, m) == Collector::OwnCgroup)
                {
                    if request.required {
                        r.block(
                            &format!("metric:{}", request.key()),
                            Status::Restricted,
                            reason.clone(),
                        );
                    }
                    r.unavailable.insert(
                        request.key(),
                        Missing {
                            unsupported: true,
                            reason: reason.clone(),
                        },
                    );
                }
                r.push("cgroup-delegation", Status::Restricted, reason);
            }
            OwnCgroup::NotNeeded => {}
        }
    }
    let io_refusal = io_refusal(&r.own_cgroup);
    let io_requests: Vec<&MetricRequest> = scenario
        .metrics
        .iter()
        .filter(|m| m.metric.is_io() && collector(selection, m) == Collector::OwnCgroup)
        .collect();
    if !io_requests.is_empty() {
        match &io_refusal {
            Some(reason) => r.push("cgroup-io", Status::Unsupported, reason.clone()),
            None if matches!(r.own_cgroup, OwnCgroup::ViaScope(_, _)) => r.push(
                "cgroup-io",
                Status::Available,
                "контроллер io включается для подгрупп в делегированной группе systemd",
            ),
            None if matches!(r.own_cgroup, OwnCgroup::Ready(_)) => r.push(
                "cgroup-io",
                Status::Available,
                "контроллер io делегирован; включение для подгрупп проверяется активацией перед серией",
            ),
            None => {}
        }
    }
    if let Some(reason) = io_refusal {
        for request in io_requests {
            if request.required {
                r.block(
                    &format!("metric:{}", request.key()),
                    Status::Unsupported,
                    reason.clone(),
                );
            }
            r.unavailable.insert(
                request.key(),
                Missing {
                    unsupported: true,
                    reason: reason.clone(),
                },
            );
        }
    }
    if let Some(reason) = cgroup_restricted {
        if matches!(r.own_cgroup, OwnCgroup::ViaScope(_, _)) {
            r.push(
                "linux-cgroup-v2",
                Status::Available,
                format!("подгруппы будут доступны после перезапуска в systemd-run ({reason})"),
            );
        } else {
            r.block("linux-cgroup-v2", Status::Restricted, reason);
        }
    }
    r
}

/// Почему I/O-метрики собственной группы не будут измерены. Делегирование
/// io прямо доступной группе — необходимое условие; включение для подгрупп
/// серия проверяет активацией и при отказе записывает точную причину. Через
/// systemd-run включение уже проверено активацией в одноразовой группе.
fn io_refusal(own: &OwnCgroup) -> Option<String> {
    match own {
        OwnCgroup::Ready(group) if !group.io_delegated() => Some(reason::IO_NOT_DELEGATED.text()),
        OwnCgroup::ViaScope(_, IoControl::Unavailable(reason)) => Some(reason.clone()),
        _ => None,
    }
}

/// Состояние io по выводу `__cgroup-probe`: `io` или причина отказа.
fn scope_io(out: &str) -> IoControl {
    match out.trim() {
        "io" => IoControl::Enabled,
        "" => IoControl::Unavailable(reason::IO_PROBE_SILENT.text()),
        refusal => IoControl::Unavailable(refusal.to_owned()),
    }
}

fn perf_paranoid() -> Option<i32> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Группа исполнителя или перезапуск через systemd: проверяется действием.
fn own_cgroup(runner: &Path) -> OwnCgroup {
    let direct = match cgroup::probe() {
        Ok(delegated) => return OwnCgroup::Ready(delegated),
        Err(reason) => reason,
    };
    if std::env::var_os(SCOPE_ENV).is_some() {
        return OwnCgroup::Unavailable(format!("даже в делегированной группе systemd: {direct}"));
    }
    let Some(systemd_run) = which("systemd-run") else {
        return OwnCgroup::Unavailable(format!("{direct}; systemd-run не найден"));
    };
    let mut command = Command::new(&systemd_run);
    command
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
        ])
        .arg(runner)
        .arg("__cgroup-probe");
    match procs::output(&mut command, Duration::from_secs(20)) {
        Ok((procs::Ending::Exited(0), out, _)) => {
            OwnCgroup::ViaScope(systemd_run, scope_io(&String::from_utf8_lossy(&out)))
        }
        Ok((ending, out, err)) => OwnCgroup::Unavailable(format!(
            "{direct}; systemd-run --user --scope не дал делегированной группы ({}): {}",
            ending.describe(),
            String::from_utf8_lossy(if out.is_empty() { &err } else { &out })
                .lines()
                .next()
                .unwrap_or("")
                .trim()
        )),
        Err(error) => {
            OwnCgroup::Unavailable(format!("{direct}; systemd-run не запускается: {error}"))
        }
    }
}

/// Измерить пустую команду с запрошенными метриками: так видно, какие из них
/// Hyperfine действительно экспортирует здесь (счётчики процессора требуют
/// perf_event, RSS есть не на всех платформах).
fn probe_hyperfine_metrics(
    r: &mut Readiness,
    selection: &Selection<'_>,
    hyperfine: &Hyperfine,
    runner: &Path,
) {
    let requests: Vec<&MetricRequest> = selection
        .item
        .scenario
        .metrics
        .iter()
        .filter(|m| m.source == Source::Hyperfine)
        .collect();
    let all = hyperfine_exec::metric_names(&selection.item.scenario.metrics);
    let target = vec![runner.to_string_lossy().into_owned(), "__noop".to_owned()];
    let dir = std::env::temp_dir();
    let attempt = |names: &[&str]| -> Result<serde_json::Value, String> {
        let export = dir.join(format!(
            "benchmark-runner-probe-{}-{}.json",
            std::process::id(),
            crate::util::random_suffix()
        ));
        let mut command = hyperfine_exec::command(
            hyperfine,
            &target,
            names,
            &export,
            hyperfine_exec::Output::Null,
        );
        let result = procs::output(&mut command, Duration::from_secs(30));
        let text = std::fs::read_to_string(&export);
        let _ = std::fs::remove_file(&export);
        match result {
            Ok((procs::Ending::Exited(0), _, _)) => text
                .map_err(|e| format!("экспорт пробы не прочитан: {e}"))
                .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())),
            Ok((ending, _, err)) => Err(format!(
                "{}: {}",
                ending.describe(),
                String::from_utf8_lossy(&err)
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
            )),
            Err(error) => Err(error.to_string()),
        }
    };
    let (export, counters_refused) = match attempt(&all) {
        Ok(export) => (export, None),
        Err(reason) => {
            let basic: Vec<&str> = all
                .iter()
                .copied()
                .filter(|name| {
                    !benchmark_contract::vocabulary::MetricId::ALL
                        .iter()
                        .any(|m| m.as_str() == *name && m.is_hardware_counter())
                })
                .collect();
            match attempt(&basic) {
                Ok(export) => (export, Some(reason)),
                Err(second) => {
                    r.block(
                        "hyperfine-metrics",
                        Status::Restricted,
                        format!("Hyperfine не измеряет пробную команду: {second}"),
                    );
                    return;
                }
            }
        }
    };
    let measurement = &export["results"][0]["measurements"][0];
    let mut accepted = Vec::new();
    for name in &all {
        if measurement.get(*name).is_some() {
            accepted.push(*name);
        }
    }
    for request in requests {
        let name = request.metric.as_str();
        if accepted.contains(&name) {
            continue;
        }
        let reason = match (&counters_refused, request.metric.is_hardware_counter()) {
            (Some(refusal), true) => {
                format!("Hyperfine не открывает счётчики процессора (perf_event): {refusal}")
            }
            _ => format!(
                "Hyperfine {} не экспортирует {name} в этом окружении",
                hyperfine.version
            ),
        };
        if request.required {
            r.block(
                &format!("metric:{}", request.key()),
                Status::Restricted,
                reason.clone(),
            );
        }
        r.unavailable.insert(
            request.key(),
            Missing {
                unsupported: true,
                reason,
            },
        );
    }
    r.push(
        "hyperfine-metrics",
        Status::Available,
        format!("экспортируются: {}", accepted.join(", ")),
    );
    r.hyperfine_metrics = accepted;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_refusal_follows_delegation_and_the_scope_activation() {
        let base =
            std::env::temp_dir().join(format!("benchmark-io-readiness-{}", std::process::id()));
        std::fs::create_dir(&base).unwrap();
        let ready = OwnCgroup::Ready(Delegated { base: base.clone() });
        std::fs::write(base.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        assert_eq!(io_refusal(&ready), Some(reason::IO_NOT_DELEGATED.text()));
        std::fs::write(base.join("cgroup.controllers"), "cpu io memory pids\n").unwrap();
        assert_eq!(io_refusal(&ready), None);
        std::fs::remove_dir_all(&base).unwrap();

        let scope = |io| OwnCgroup::ViaScope(PathBuf::from("systemd-run"), io);
        assert_eq!(io_refusal(&scope(IoControl::Enabled)), None);
        let refused = reason::IO_NOT_ENABLED.with("Device or resource busy (os error 16)");
        assert_eq!(
            scope_io(&format!("{refused}\n")),
            IoControl::Unavailable(refused.clone())
        );
        assert_eq!(io_refusal(&scope(scope_io(&refused))), Some(refused));
        assert_eq!(scope_io("io\n"), IoControl::Enabled);
        assert_eq!(
            scope_io(""),
            IoControl::Unavailable(reason::IO_PROBE_SILENT.text())
        );
        assert_eq!(io_refusal(&OwnCgroup::NotNeeded), None);
    }
}
