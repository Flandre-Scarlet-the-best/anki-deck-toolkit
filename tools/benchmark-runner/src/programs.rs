//! Разрешение программ сценария в исполняемые файлы.
//!
//! `cargo-bin` собирается до серии (`cargo build --locked`): сборка —
//! подготовка, она не входит ни в измерение, ни в бюджет серии и ограничена
//! собственным таймаутом. Путь результата сообщает сам Cargo
//! (`--message-format=json`), поэтому раскладка `target/` не угадывается.

use crate::hyperfine_exec::which;
use crate::procs::{self, Bounds, Ending};
use benchmark_contract::scenario::Program;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct Resolved {
    entries: Vec<(Program, PathBuf)>,
}

impl Resolved {
    pub fn path(&self, program: &Program) -> Option<&Path> {
        self.entries
            .iter()
            .find(|(p, _)| p == program)
            .map(|(_, path)| path.as_path())
    }
}

/// Имя программы для журнала и диагностики.
pub fn label(program: &Program) -> String {
    match program {
        Program::CargoBin { package, bin, .. } => format!("{package}/{bin}"),
        Program::Executable { name } => name.clone(),
    }
}

pub fn resolve(
    programs: &[&Program],
    repo: &Path,
    log_dir: &Path,
    timeout: Duration,
) -> Result<Resolved, String> {
    let mut resolved = Resolved::default();
    for program in programs {
        let path = match program {
            Program::Executable { name } => {
                which(name).ok_or_else(|| format!("исполняемый файл «{name}» не найден в PATH"))?
            }
            Program::CargoBin {
                package,
                bin,
                cargo_profile,
            } => build(package, bin, cargo_profile, repo, log_dir, timeout)?,
        };
        resolved.entries.push(((*program).clone(), path));
    }
    Ok(resolved)
}

fn build(
    package: &str,
    bin: &str,
    profile: &str,
    repo: &Path,
    log_dir: &Path,
    timeout: Duration,
) -> Result<PathBuf, String> {
    fs::create_dir_all(log_dir).map_err(|e| format!("каталог журналов сборки: {e}"))?;
    let stdout_path = log_dir.join(format!("build-{package}-{bin}.jsonl"));
    let stderr_path = log_dir.join(format!("build-{package}-{bin}.log"));
    let open = |path: &Path| fs::File::create(path).map_err(|e| format!("журнал сборки: {e}"));
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--locked",
            "--profile",
            profile,
            "-p",
            package,
            "--bin",
            bin,
        ])
        .arg("--message-format=json-render-diagnostics")
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(open(&stdout_path)?)
        .stderr(open(&stderr_path)?);
    let finished = procs::run(
        &mut command,
        Bounds {
            timeout,
            grace: Duration::from_secs(10),
            interruptible: true,
        },
    )
    .map_err(|e| format!("cargo не запускается: {e}"))?;
    match finished.ending {
        Ending::Exited(0) => {}
        ending => {
            return Err(format!(
                "сборка {package}/{bin} (профиль {profile}) не удалась: {}; журнал сборки — в каталоге артефактов",
                ending.describe()
            ));
        }
    }
    let messages = fs::read_to_string(&stdout_path).map_err(|e| format!("вывод cargo: {e}"))?;
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == bin)
        .filter(|m| {
            m["target"]["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "bin"))
        })
        .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
        .next_back()
        .ok_or_else(|| format!("cargo не сообщил путь исполняемого файла {package}/{bin}"))
}

/// Версия пакета рабочей области: пакет с именем модуля, иначе пакет цели.
pub fn module_version(repo: &Path, module: &str, target: &Program) -> Option<String> {
    let manifest = read_toml(&repo.join("Cargo.toml"))?;
    let workspace_version = manifest
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let members: Vec<String> = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut versions = std::collections::BTreeMap::new();
    for member in members {
        let Some(package) = read_toml(&repo.join(&member).join("Cargo.toml"))
            .and_then(|m| m.get("package").cloned())
        else {
            continue;
        };
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let version = match package.get("version") {
            Some(toml::Value::String(v)) => Some(v.clone()),
            Some(toml::Value::Table(t))
                if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
            {
                workspace_version.clone()
            }
            _ => None,
        };
        if let Some(version) = version {
            versions.insert(name.to_owned(), version);
        }
    }
    let target_package = match target {
        Program::CargoBin { package, .. } => Some(package.as_str()),
        Program::Executable { .. } => None,
    };
    versions
        .get(module)
        .or_else(|| target_package.and_then(|p| versions.get(p)))
        .cloned()
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    toml::from_str(&fs::read_to_string(path).ok()?).ok()
}
