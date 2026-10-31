//! Встроенные факты окружения и версии инструментов.
//!
//! Факты читаются из стандартных интерфейсов Linux. Виртуализация
//! определяется во время выполнения (`systemd-detect-virt`, затем признаки
//! ядра и `/proc/cpuinfo`): WSL2, виртуальная машина, контейнер и голое железо
//! записываются разными значениями, поэтому результаты разных сред не
//! становятся сопоставимыми молча.

use crate::procs;
use benchmark_contract::result::Fact;
use std::fs;
use std::process::Command;
use std::time::Duration;

pub fn known(value: impl Into<String>) -> Fact {
    Fact::Known {
        value: value.into(),
    }
}

pub fn unavailable(reason: impl Into<String>) -> Fact {
    Fact::Unavailable {
        reason: reason.into(),
    }
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

pub fn kernel_release() -> Fact {
    read("/proc/sys/kernel/osrelease")
        .map(known)
        .unwrap_or_else(|| unavailable("/proc/sys/kernel/osrelease не читается"))
}

/// Первая строка вывода `program args…` при успешном завершении.
pub fn tool_line(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    first_line(program, &mut command, timeout)
}

/// Команда Docker CLI с окружением цели и только с ним. Цель запускается с
/// очищенным окружением, поэтому переменные исполнителя (`DOCKER_HOST`,
/// `DOCKER_CONTEXT`, `DOCKER_CONFIG`), которых цель не получила, не должны
/// направить проверку готовности, факт версии или сэмплер к другому демону.
pub fn docker_command(target_env: &[(String, String)], args: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command
        .args(args)
        .env_clear()
        .envs(target_env.iter().cloned());
    command
}

/// Версия демона Docker, к которому обращается цель.
pub fn docker_server_version(target_env: &[(String, String)]) -> Result<String, String> {
    let mut command = docker_command(target_env, &["version", "--format", "{{.Server.Version}}"]);
    first_line("docker", &mut command, Duration::from_secs(20))
}

fn first_line(program: &str, command: &mut Command, timeout: Duration) -> Result<String, String> {
    match procs::output(command, timeout) {
        Ok((procs::Ending::Exited(0), out, _)) => String::from_utf8_lossy(&out)
            .lines()
            .next()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .ok_or_else(|| format!("{program} не сообщил версию")),
        Ok((ending, _, err)) => Err(format!(
            "{program}: {}; {}",
            ending.describe(),
            String::from_utf8_lossy(&err)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
        )),
        Err(error) => Err(format!("{program} не запускается: {error}")),
    }
}

pub fn virtualization() -> Fact {
    let release = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let wsl2 = release
        .to_ascii_lowercase()
        .contains("microsoft-standard-wsl2");
    // systemd-detect-virt печатает «none» с кодом 1, поэтому код не проверяется.
    let mut command = Command::new("systemd-detect-virt");
    let detected = match procs::output(&mut command, Duration::from_secs(5)) {
        Ok((procs::Ending::Exited(_), out, _)) => {
            Some(String::from_utf8_lossy(&out).trim().to_owned()).filter(|s| !s.is_empty())
        }
        _ => None,
    };
    let value = match detected.as_deref() {
        Some("wsl") if wsl2 => "wsl2".to_owned(),
        Some(value) => value.to_owned(),
        None if wsl2 => "wsl2".to_owned(),
        None if release.to_ascii_lowercase().contains("microsoft") => "wsl".to_owned(),
        None if std::path::Path::new("/.dockerenv").exists()
            || std::path::Path::new("/run/.containerenv").exists() =>
        {
            "container".to_owned()
        }
        None if read("/proc/cpuinfo").is_some_and(|t| {
            t.lines()
                .any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor"))
        }) =>
        {
            "vm".to_owned()
        }
        None => {
            return unavailable(
                "systemd-detect-virt недоступен, а признаков WSL, контейнера и гипервизора нет: голое железо не доказано",
            );
        }
    };
    known(value)
}

pub fn cpu_model() -> Fact {
    read("/proc/cpuinfo")
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        })
        .filter(|v| !v.is_empty())
        .map(known)
        .unwrap_or_else(|| unavailable("model name отсутствует в /proc/cpuinfo"))
}

/// Число включённых логических CPU по `/sys/devices/system/cpu/online`.
pub fn logical_cpus() -> Fact {
    let Some(list) = read("/sys/devices/system/cpu/online") else {
        return unavailable("/sys/devices/system/cpu/online не читается");
    };
    let mut total = 0u64;
    for part in list.split(',') {
        let count = match part.split_once('-') {
            Some((a, b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) if b >= a => b - a + 1,
                _ => return unavailable("список CPU не разбирается"),
            },
            None if part.parse::<u64>().is_ok() => 1,
            None => return unavailable("список CPU не разбирается"),
        };
        total += count;
    }
    known(total.to_string())
}

pub fn memory_total_bytes() -> Fact {
    read("/proc/meminfo")
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map(|kib| known((kib * 1024).to_string()))
        .unwrap_or_else(|| unavailable("MemTotal отсутствует в /proc/meminfo"))
}

pub fn rustc_version() -> Fact {
    match tool_line("rustc", &["--version"], Duration::from_secs(20)) {
        Ok(line) => known(line),
        Err(reason) => unavailable(reason),
    }
}

pub fn docker_version(target_env: &[(String, String)]) -> Fact {
    match docker_server_version(target_env) {
        Ok(line) => known(line),
        Err(reason) => unavailable(format!("версия демона Docker не получена: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Подставной `docker` печатает своё окружение: видны только переменные
    /// цели, а не исполнителя.
    #[test]
    fn docker_runs_with_target_environment_only() {
        let dir = std::env::temp_dir().join(format!(
            "benchmark-runner-docker-env-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("docker");
        fs::write(&fake, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            ("PATH".to_owned(), dir.to_string_lossy().into_owned()),
            ("DOCKER_HOST".to_owned(), "unix:///target.sock".to_owned()),
        ];
        let out = docker_command(&env, &[]).output().unwrap();
        let _ = fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout);
        let docker: Vec<&str> = text.lines().filter(|l| l.starts_with("DOCKER_")).collect();
        assert_eq!(docker, ["DOCKER_HOST=unix:///target.sock"]);
        assert!(!text.lines().any(|l| l.starts_with("HOME=")), "{text}");
    }
}
