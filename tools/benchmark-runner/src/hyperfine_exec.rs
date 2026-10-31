//! Вызов Hyperfine 2.0 для одного прогона.
//!
//! Hyperfine 2.0 при отказе цели, шага `--prepare` или прерывании не пишет
//! `--export-json` вовсе: серия одним вызовом потеряла бы все прежние
//! измерения и не дала бы честного `partial`. Поэтому исполнитель вызывает
//! Hyperfine на каждый прогон (`--runs 1`), а прогрев, `prepare`, `conclude`,
//! таймауты и остановку серии ведёт сам. Измерение, единицы и формат
//! экспорта — Hyperfine; статистику серии считает контракт.

use crate::environment::tool_line;
use benchmark_contract::scenario::MetricRequest;
use benchmark_contract::vocabulary::Source;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Hyperfine {
    pub path: PathBuf,
    pub version: String,
}

/// Найти Hyperfine и проверить, что это 2.x: экспорт 1.x несовместим.
pub fn detect() -> Result<Hyperfine, String> {
    let path = which("hyperfine").ok_or(
        "hyperfine не найден в PATH: установите Hyperfine 2.0 (см. docs/ru/benchmark-runner/README.md)",
    )?;
    let line = tool_line(
        &path.to_string_lossy(),
        &["--version"],
        Duration::from_secs(10),
    )?;
    let version = line
        .strip_prefix("hyperfine ")
        .map(str::trim)
        .ok_or_else(|| format!("неожиданный ответ hyperfine --version: «{line}»"))?
        .to_owned();
    match version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
    {
        Some(2) => Ok(Hyperfine { path, version }),
        Some(major) => Err(format!(
            "установлен Hyperfine {version}: нужен 2.x (экспорт --export-json schema_version 2); версия {major}.x не поддерживается"
        )),
        None => Err(format!("версия Hyperfine «{version}» не разбирается")),
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Имена метрик Hyperfine, которые запросил сценарий; время по часам — первым.
pub fn metric_names(requests: &[MetricRequest]) -> Vec<&'static str> {
    let mut names = vec!["time_wall_clock"];
    for request in requests.iter().filter(|r| r.source == Source::Hyperfine) {
        let name = request.metric.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Куда направить вывод цели.
pub enum Output<'a> {
    Null,
    File(&'a Path),
}

/// argv Hyperfine для одного прогона. Цель — argv; Hyperfine исполняет её
/// без shell (`-N`) и делит строку по правилам POSIX, поэтому argv
/// экранируется здесь.
pub fn argv(
    hyperfine: &Hyperfine,
    target: &[String],
    metrics: &[&str],
    export: &Path,
    output: Output<'_>,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        hyperfine.path.to_string_lossy().as_ref(),
        "-N",
        "--runs",
        "1",
        "--warmup",
        "0",
        "--style",
        "none",
        "--ignore-failure",
        "--input",
        "null",
        "--metrics",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    argv.push(metrics.join(","));
    argv.push("--export-json".to_owned());
    argv.push(export.to_string_lossy().into_owned());
    argv.push("--output".to_owned());
    argv.push(match output {
        Output::Null => "null".to_owned(),
        Output::File(path) => path.to_string_lossy().into_owned(),
    });
    argv.extend(["--command-name".to_owned(), "target".to_owned()]);
    argv.push(crate::util::shell_join(target));
    argv
}

pub fn command(
    hyperfine: &Hyperfine,
    target: &[String],
    metrics: &[&str],
    export: &Path,
    output: Output<'_>,
) -> Command {
    let argv = argv(hyperfine, target, metrics, export, output);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}
