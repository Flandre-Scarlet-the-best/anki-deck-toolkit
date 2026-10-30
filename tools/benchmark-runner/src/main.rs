//! CLI исполнителя бенчмарков: `list`, `check`, `run`.

use benchmark_contract::catalog::{self, Catalog};
use benchmark_contract::diagnostic::Severity;
use benchmark_contract::vocabulary::ScenarioStatus;
use benchmark_runner::plan::{self, Selection};
use benchmark_runner::readiness::{self, Collector, OwnCgroup, SCOPE_ENV};
use benchmark_runner::series::{self, Exit, HistoryOff, HistoryStatus, Request};
use benchmark_runner::{cgroup, offline, procs};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_CATALOG: &str = "benchmarks";

#[derive(Debug, Parser)]
#[command(
    name = "benchmark-runner",
    version,
    about = "Запуск сценариев каталога бенчмарков через Hyperfine 2.0 со сбором системных метрик Linux."
)]
struct Cli {
    /// Каталог с catalog.toml; по умолчанию — benchmarks/ ближайшего родителя текущего каталога.
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    /// Машинный JSON в stdout; ход серии по-прежнему пишется в stderr.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Args)]
struct Target {
    /// ID сценария, например anki-repo.validate.
    id: String,
    /// Профиль условий выполнения из привязок сценария.
    #[arg(long)]
    profile: String,
    /// Значение объявленного параметра: --param <имя>=<значение>.
    #[arg(long = "param", value_parser = parse_param)]
    params: Vec<(String, String)>,
    /// Число измеряемых прогонов; по умолчанию min_runs, для long-e2e обязательно.
    #[arg(long)]
    runs: Option<u32>,
    /// Не опрашивать /proc и cgroup во время прогона: сэмплируемые метрики будут unavailable.
    #[arg(long)]
    no_sampling: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Сценарии, которые исполнитель может запускать.
    List,
    /// Проверить реальную готовность окружения к серии (с пробными запусками).
    Check(Target),
    /// Выполнить серию и записать проверенный результат.
    Run {
        #[command(flatten)]
        target: Target,
        /// Файл результата; по умолчанию result.json в каталоге артефактов. Существующий файл не перезаписывается.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Корень артефактов; по умолчанию target/benchmark-runner репозитория.
        #[arg(long)]
        artifacts_dir: Option<PathBuf>,
        /// Предел сборки каждого cargo-bin, секунд.
        #[arg(long, default_value_t = 3600)]
        build_timeout: u64,
        /// Не сохранять принятый результат в локальную историю benchmark-history.
        #[arg(long)]
        no_history: bool,
    },
    #[command(name = "__enter-cgroup", hide = true)]
    EnterCgroup {
        group: PathBuf,
        started_at: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(name = "__net-probe", hide = true)]
    NetProbe,
    #[command(name = "__cgroup-probe", hide = true)]
    CgroupProbe,
    #[command(name = "__noop", hide = true)]
    Noop,
}

fn parse_param(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "ожидается <имя>=<значение>".to_owned())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err((exit, message)) => {
            if cli.json {
                print_json(
                    &json!({"ok": false, "exit": exit, "exit_code": exit as i32, "error": message}),
                );
            } else {
                eprintln!("benchmark-runner: {message}");
            }
            ExitCode::from(exit as u8)
        }
    }
}

type Failure = (Exit, String);

fn run(cli: &Cli) -> Result<ExitCode, Failure> {
    match &cli.command {
        Command::Noop => Ok(ExitCode::SUCCESS),
        Command::NetProbe => {
            println!("{}", offline::list_interfaces());
            Ok(ExitCode::SUCCESS)
        }
        Command::CgroupProbe => match cgroup::probe() {
            Ok(group) => {
                if group.has_io() {
                    println!("io");
                }
                Ok(ExitCode::SUCCESS)
            }
            Err(reason) => {
                println!("{reason}");
                Ok(ExitCode::FAILURE)
            }
        },
        Command::EnterCgroup {
            group,
            started_at,
            command,
        } => {
            let (program, args) = match command.split_first() {
                Some((first, rest)) if first == "--" => match rest.split_first() {
                    Some((program, args)) => (program, args),
                    None => return Err((Exit::Usage, "после -- нужна команда".into())),
                },
                Some((program, args)) => (program, args),
                None => return Err((Exit::Usage, "нужна команда".into())),
            };
            let error = cgroup::enter_and_exec(group, started_at, program, args);
            eprintln!("benchmark-runner: {error}");
            Ok(ExitCode::from(125))
        }
        Command::List => {
            let catalog = load(cli)?;
            let rows: Vec<_> = catalog
                .scenarios
                .iter()
                .filter(|s| s.scenario.status == ScenarioStatus::Executable)
                .map(|item| {
                    let s = &item.scenario;
                    json!({
                        "id": s.id,
                        "kind": s.kind,
                        "revision": s.revision,
                        "title": s.title,
                        "profiles": s.profiles.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
                        "runs": {"min": s.runs.min_runs, "max": s.runs.max_runs, "warmup": s.runs.warmup,
                                 "single_run_allowed": s.runs.single_run_allowed,
                                 "run_timeout_seconds": s.runs.run_timeout_seconds,
                                 "total_budget_seconds": s.runs.total_budget_seconds},
                        "parameters": s.parameters.iter().map(|p| json!({"name": p.name, "required": p.required, "record": p.record})).collect::<Vec<_>>(),
                    })
                })
                .collect();
            if cli.json {
                print_json(&json!({"ok": true, "scenarios": rows}));
            } else {
                for row in &rows {
                    println!(
                        "{}  {}  [{}]  прогонов {}…{}  {}",
                        row["id"].as_str().unwrap_or_default(),
                        row["kind"].as_str().unwrap_or_default(),
                        row["profiles"]
                            .as_array()
                            .map(|p| p
                                .iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", "))
                            .unwrap_or_default(),
                        row["runs"]["min"],
                        row["runs"]["max"],
                        row["title"].as_str().unwrap_or_default()
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check(target) => {
            let catalog = load(cli)?;
            let selection = select(&catalog, target)?;
            let runner = current_exe()?;
            let ready = readiness::assess(&selection, &runner, !target.no_sampling);
            let metrics: Vec<_> = selection
                .item
                .scenario
                .metrics
                .iter()
                .map(|m| {
                    let key = m.key();
                    let missing = ready.unavailable.get(&key);
                    json!({
                        "key": key,
                        "required": m.required,
                        "collector": readiness::collector(&selection, m),
                        "plan": match missing {
                            None => "measure",
                            Some(m) if m.unsupported => "unsupported",
                            Some(_) => "unavailable",
                        },
                        "reason": missing.map(|m| m.reason.clone()),
                    })
                })
                .collect();
            if cli.json {
                print_json(&json!({
                    "ok": ready.ready(),
                    "scenario": selection.item.scenario.id,
                    "profile": selection.binding.id,
                    "runs": selection.runs,
                    "checks": ready.checks,
                    "blockers": ready.blockers,
                    "metrics": metrics,
                }));
            } else {
                for check in &ready.checks {
                    let status = serde_json::to_value(check.status).unwrap_or_default();
                    println!(
                        "{:<28} {:<12} {}",
                        check.requirement,
                        status.as_str().unwrap_or_default(),
                        check.detail
                    );
                }
                println!();
                for metric in &metrics {
                    println!(
                        "{:<48} {:<12} {}",
                        metric["key"].as_str().unwrap_or_default(),
                        metric["plan"].as_str().unwrap_or_default(),
                        metric["reason"].as_str().unwrap_or("")
                    );
                }
                println!(
                    "\n{}",
                    if ready.ready() {
                        "Серия может измеряться здесь.".to_owned()
                    } else {
                        format!("Серия здесь не измеряется: {}", ready.blockers.join("; "))
                    }
                );
            }
            Ok(ExitCode::from(if ready.ready() {
                0
            } else {
                Exit::Unsupported as u8
            }))
        }
        Command::Run {
            target,
            output,
            artifacts_dir,
            build_timeout,
            no_history,
        } => {
            let catalog = load(cli)?;
            let selection = select(&catalog, target)?;
            if let Some(path) = output
                && path.exists()
            {
                return Err((
                    Exit::Usage,
                    format!(
                        "файл результата {} уже существует: прежние результаты не перезаписываются",
                        path.display()
                    ),
                ));
            }
            let runner = current_exe()?;
            procs::install_signal_handler().map_err(|e| (Exit::Error, e))?;
            if !procs::become_subreaper() {
                eprintln!(
                    "benchmark-runner: предупреждение: subreaper не установлен; потомки, покинувшие группу процессов, не будут найдены"
                );
            }
            let ready = readiness::assess(&selection, &runner, !target.no_sampling);
            if let OwnCgroup::ViaScope(systemd_run, _) = &ready.own_cgroup {
                return Err(reexec_in_scope(systemd_run));
            }
            let needs_own = selection
                .item
                .scenario
                .metrics
                .iter()
                .any(|m| readiness::collector(&selection, m) == Collector::OwnCgroup);
            if needs_own {
                eprintln!("benchmark-runner: прогоны измеряются в собственных подгруппах cgroup");
            }
            let history = if *no_history {
                Err(HistoryOff::Disabled)
            } else {
                benchmark_history::location::resolve(None)
                    .and_then(|found| {
                        benchmark_history::location::refuse_inside_repository(
                            &found.root,
                            selection.repository_root(),
                        )
                        .map(|()| found.root)
                    })
                    .map_err(HistoryOff::Unavailable)
            };
            if let Err(HistoryOff::Unavailable(reason)) = &history {
                eprintln!(
                    "benchmark-runner: предупреждение: результат не попадёт в локальную историю: {reason}"
                );
            }
            let request = Request {
                sampling: !target.no_sampling,
                output: output.clone(),
                history,
                artifacts_root: artifacts_dir
                    .clone()
                    .unwrap_or_else(|| selection.repository_root().join("target/benchmark-runner")),
                build_timeout: Duration::from_secs(*build_timeout),
            };
            let summary = series::execute(&selection, &ready, &request, &runner)
                .map_err(|e| (Exit::Error, e))?;
            if cli.json {
                print_json(&serde_json::to_value(&summary).unwrap_or_default());
            } else {
                for diagnostic in summary
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                {
                    eprintln!("{diagnostic}");
                }
                println!(
                    "{}: {}{}",
                    summary
                        .outcome
                        .map(|o| format!("{o:?}").to_lowercase())
                        .unwrap_or_else(|| "нет результата".into()),
                    summary.reason.as_deref().unwrap_or("все прогоны успешны"),
                    summary
                        .result
                        .as_ref()
                        .map(|p| format!("\nрезультат: {}", p.display()))
                        .unwrap_or_default()
                );
                if let Some(dir) = &summary.artifacts {
                    println!("артефакты: {}", dir.display());
                }
                match summary.history.status {
                    HistoryStatus::Archived | HistoryStatus::AlreadyArchived => println!(
                        "история: {}",
                        summary.history.id.as_deref().unwrap_or_default()
                    ),
                    HistoryStatus::Failed => eprintln!(
                        "benchmark-runner: история: результат НЕ сохранён — {}",
                        summary.history.reason.as_deref().unwrap_or_default()
                    ),
                    HistoryStatus::Disabled | HistoryStatus::NotRecorded => {}
                }
            }
            Ok(ExitCode::from(summary.exit_code as u8))
        }
    }
}

/// Перезапустить исполнитель в делегированной группе systemd без root.
fn reexec_in_scope(systemd_run: &Path) -> Failure {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return (Exit::Error, format!("путь исполнителя не определён: {e}")),
    };
    eprintln!(
        "benchmark-runner: перезапуск в systemd-run --user --scope -p Delegate=yes для собственной cgroup прогонов"
    );
    let error = std::process::Command::new(systemd_run)
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
        ])
        .arg(exe)
        .args(std::env::args_os().skip(1))
        .env(SCOPE_ENV, "1")
        .exec();
    (
        Exit::Error,
        format!("перезапуск через systemd-run не удался: {error}"),
    )
}

fn current_exe() -> Result<PathBuf, Failure> {
    std::env::current_exe()
        .map_err(|e| (Exit::Error, format!("путь исполнителя не определён: {e}")))
}

fn select<'c>(catalog: &'c Catalog, target: &Target) -> Result<Selection<'c>, Failure> {
    plan::select(
        catalog,
        &target.id,
        &target.profile,
        &target.params,
        target.runs,
    )
    .map_err(|e| (Exit::Usage, e.0))
}

fn load(cli: &Cli) -> Result<Catalog, Failure> {
    let root = match &cli.catalog {
        Some(path) => path.clone(),
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| (Exit::Error, format!("текущий каталог недоступен: {e}")))?;
            cwd.ancestors()
                .map(|dir| dir.join(DEFAULT_CATALOG))
                .find(|dir| dir.join(catalog::CATALOG_FILE).is_file())
                .ok_or_else(|| {
                    (
                        Exit::Error,
                        format!("каталог {DEFAULT_CATALOG}/ не найден выше текущего каталога; укажите --catalog"),
                    )
                })?
        }
    };
    let loaded = catalog::load(&root);
    loaded.catalog.ok_or_else(|| {
        let errors: Vec<String> = loaded
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(ToString::to_string)
            .collect();
        (
            Exit::Error,
            format!(
                "каталог некорректен, проверьте benchmark-contract validate:\n{}",
                errors.join("\n")
            ),
        )
    })
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON сериализуется")
    );
}
