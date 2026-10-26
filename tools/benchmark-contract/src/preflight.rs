//! Предварительная проверка доступности зависимостей сценария в текущем
//! окружении. Она отделена от проверки каталога: отсутствие Docker, Anki или
//! Hyperfine не делает декларацию некорректной.
//!
//! Первой проверяется платформа: на ОС вне `platforms` сценарий не
//! измеряется, даже если все программы найдены.
//!
//! Проверка только читает `PATH`, `/sys/fs/cgroup` и `/proc/sys`: она ничего
//! не запускает, не обращается к сети и не требует прав на cgroup или
//! `perf_event`.

use crate::scenario::{Program, Requirement, Scenario};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Available,
    Missing,
    Restricted,
    NotChecked,
    /// Сценарий не объявлен для этой платформы.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub requirement: String,
    pub status: Status,
    pub detail: String,
}

pub fn run(scenario: &Scenario) -> Vec<Check> {
    let mut checks = vec![platform(scenario, std::env::consts::OS)];
    let mut executables = Vec::new();
    let programs = std::iter::once(&scenario.target.program)
        .chain(scenario.setup.iter().map(|s| &s.program))
        .chain(scenario.cleanup.iter().map(|s| &s.program))
        .chain(scenario.profiles.iter().flat_map(|p| {
            p.setup
                .iter()
                .chain(&p.prepare)
                .chain(&p.conclude)
                .map(|s| &s.program)
        }));
    for program in programs {
        let name = match program {
            Program::CargoBin { .. } => "cargo",
            Program::Executable { name } => name.as_str(),
        };
        if !executables.iter().any(|e: &String| e == name) {
            executables.push(name.to_owned());
        }
    }
    for requirement in &scenario.requires {
        if let Requirement::Executable { name } = requirement
            && !executables.contains(name)
        {
            executables.push(name.clone());
        }
    }
    for name in &executables {
        checks.push(executable(name));
    }
    for requirement in &scenario.requires {
        match requirement {
            Requirement::Executable { .. } => {}
            Requirement::Docker => {
                let mut check = executable("docker");
                check.requirement = "docker".to_owned();
                if check.status == Status::Available {
                    check.detail.push_str(
                        "; доступность демона не проверялась: preflight не запускает команд",
                    );
                }
                checks.push(check);
            }
            Requirement::LinuxCgroupV2 => checks.push(cgroup_v2()),
            Requirement::PerfEvents => checks.push(perf_events()),
            Requirement::Network { purpose, profiles } => checks.push(Check {
                requirement: "network".to_owned(),
                status: Status::NotChecked,
                detail: format!(
                    "{purpose} (профили: {}); сеть не проверяется без обращения к ней",
                    profiles.join(", ")
                ),
            }),
        }
    }
    checks
}

/// Сравнить `platforms` сценария с ОС в обозначении `std::env::consts::OS`.
pub fn platform(scenario: &Scenario, os: &str) -> Check {
    let declared: Vec<&str> = scenario.platforms.iter().map(|p| p.as_str()).collect();
    let supported = declared.contains(&os);
    Check {
        requirement: "platform".to_owned(),
        status: if supported {
            Status::Available
        } else {
            Status::Unsupported
        },
        detail: if supported {
            format!("{os} входит в platforms сценария")
        } else {
            format!(
                "{os} не входит в platforms сценария ({}): здесь сценарий не измеряется",
                declared.join(", ")
            )
        },
    }
}

fn executable(name: &str) -> Check {
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(name))));
    Check {
        requirement: format!("executable:{name}"),
        status: if found {
            Status::Available
        } else {
            Status::Missing
        },
        detail: if found {
            format!("{name} найден в PATH")
        } else {
            format!("{name} не найден в PATH")
        },
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    let mut with_suffix = path.as_os_str().to_owned();
    with_suffix.push(std::env::consts::EXE_SUFFIX);
    path.is_file() || Path::new(&with_suffix).is_file()
}

fn cgroup_v2() -> Check {
    let available = Path::new("/sys/fs/cgroup/cgroup.controllers").is_file();
    Check {
        requirement: "linux-cgroup-v2".to_owned(),
        status: if available {
            Status::Available
        } else {
            Status::Missing
        },
        detail: if available {
            "единая иерархия cgroup v2 смонтирована; права на создание групп не проверялись"
                .to_owned()
        } else {
            "нет /sys/fs/cgroup/cgroup.controllers: cgroup v2 недоступна".to_owned()
        },
    }
}

fn perf_events() -> Check {
    match std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid") {
        Ok(text) => match text.trim().parse::<i32>() {
            // Значение 2 и ниже разрешает непривилегированный подсчёт
            // пользовательского кода собственных процессов.
            Ok(level) if level <= 2 => Check {
                requirement: "perf-events".to_owned(),
                status: Status::Available,
                detail: format!("perf_event_paranoid = {level}"),
            },
            Ok(level) => Check {
                requirement: "perf-events".to_owned(),
                status: Status::Restricted,
                detail: format!(
                    "perf_event_paranoid = {level}: счётчики процессора недоступны без прав"
                ),
            },
            Err(_) => Check {
                requirement: "perf-events".to_owned(),
                status: Status::NotChecked,
                detail: "perf_event_paranoid не разбирается".to_owned(),
            },
        },
        Err(_) => Check {
            requirement: "perf-events".to_owned(),
            status: Status::Missing,
            detail: "нет /proc/sys/kernel/perf_event_paranoid: perf_event недоступен".to_owned(),
        },
    }
}
