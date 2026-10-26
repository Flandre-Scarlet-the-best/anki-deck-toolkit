//! CLI проверки каталога бенчмарков и результатов измерений.

use benchmark_contract::catalog::{self, Catalog, LoadedScenario};
use benchmark_contract::diagnostic::{Diagnostic, Severity};
use benchmark_contract::vocabulary::{self, BuiltinFact, WorkloadKind};
use benchmark_contract::{comparability, preflight, result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Каталог относительно корня репозитория, где его ищет CLI по умолчанию.
const DEFAULT_CATALOG: &str = "benchmarks";

#[derive(Debug, Parser)]
#[command(
    name = "benchmark-contract",
    version,
    about = "Проверка каталога сценариев бенчмаркинга и результатов измерений. Ничего не запускает и не измеряет."
)]
struct Cli {
    /// Каталог с catalog.toml; по умолчанию — benchmarks/ ближайшего родителя текущего каталога.
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    /// Стабильный JSON вместо текста для человека.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Загрузить каталог и проверить все декларации.
    Validate,
    /// Перечислить сценарии каталога.
    List,
    /// Показать типизированную декларацию сценария.
    Show { id: String },
    /// Проверить доступность зависимостей сценария в текущем окружении (только чтение).
    Preflight { id: String },
    /// Проверить документы результатов; по умолчанию — и по каталогу.
    CheckResult {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Проверить только сам документ, без сверки с декларацией.
        #[arg(long)]
        no_catalog: bool,
    },
    /// Ответить, допустимо ли сопоставлять два результата.
    Compare { left: PathBuf, right: PathBuf },
    /// Вывести словарь контракта: метрики, области охвата, источники, единицы, пределы.
    Vocabulary,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(message) => {
            if cli.json {
                print_json(&json!({"ok": false, "error": message}));
            } else {
                eprintln!("benchmark-contract: {message}");
            }
            ExitCode::from(3)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    match &cli.command {
        Command::Validate => {
            let (catalog, diagnostics) = load(cli)?;
            let ok = catalog.is_some();
            if cli.json {
                print_json(&json!({
                    "ok": ok,
                    "scenarios": catalog.as_ref().map_or(0, |c| c.scenarios.len()),
                    "diagnostics": diagnostics,
                }));
            } else {
                print_diagnostics(&diagnostics);
                if let Some(catalog) = &catalog {
                    println!(
                        "Каталог корректен: сценариев — {}, профилей — {}",
                        catalog.scenarios.len(),
                        catalog.profiles.len()
                    );
                }
            }
            Ok(code(ok))
        }
        Command::List => {
            let catalog = require_catalog(cli)?;
            if cli.json {
                let rows: Vec<_> = catalog.scenarios.iter().map(summary).collect();
                print_json(&json!({"ok": true, "scenarios": rows}));
            } else {
                for item in &catalog.scenarios {
                    let s = &item.scenario;
                    let profiles: Vec<&str> = s.profiles.iter().map(|p| p.id.as_str()).collect();
                    println!(
                        "{}  r{}  {}  {}  [{}]  {}",
                        s.id,
                        s.revision,
                        s.kind,
                        s.status,
                        profiles.join(", "),
                        s.title
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Show { id } => {
            let catalog = require_catalog(cli)?;
            let item = find(&catalog, id)?;
            if cli.json {
                print_json(&json!({"ok": true, "scenario": item}));
            } else {
                let text = toml::to_string_pretty(&item.scenario).map_err(|e| e.to_string())?;
                println!("# {} ({})\n{text}", item.scenario.id, item.file);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Preflight { id } => {
            let catalog = require_catalog(cli)?;
            let item = find(&catalog, id)?;
            let checks = preflight::run(&item.scenario);
            let ok = checks.iter().all(|c| {
                matches!(
                    c.status,
                    preflight::Status::Available | preflight::Status::NotChecked
                )
            });
            if cli.json {
                print_json(&json!({"ok": ok, "scenario": id, "checks": checks}));
            } else {
                for check in &checks {
                    let status = serde_json::to_value(check.status).expect("статус сериализуется");
                    println!(
                        "{:<28} {:<12} {}",
                        check.requirement,
                        status.as_str().unwrap_or_default(),
                        check.detail
                    );
                }
            }
            Ok(code(ok))
        }
        Command::CheckResult { files, no_catalog } => {
            let catalog = if *no_catalog {
                None
            } else {
                Some(require_catalog(cli)?)
            };
            let mut all_ok = true;
            let mut reports = Vec::new();
            for file in files {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| format!("не удалось прочитать {}: {e}", file.display()))?;
                let checked = result::check(&text, &display_name(file), catalog.as_ref());
                all_ok &= checked.result.is_some();
                if !cli.json {
                    print_diagnostics(&checked.diagnostics);
                    if checked.result.is_some() {
                        println!("{}: результат корректен", display_name(file));
                    }
                }
                reports.push(json!({
                    "file": display_name(file),
                    "ok": checked.result.is_some(),
                    "diagnostics": checked.diagnostics,
                }));
            }
            if cli.json {
                print_json(&json!({"ok": all_ok, "results": reports}));
            }
            Ok(code(all_ok))
        }
        Command::Compare { left, right } => {
            let catalog = require_catalog(cli)?;
            let mut parsed = Vec::new();
            for file in [left, right] {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| format!("не удалось прочитать {}: {e}", file.display()))?;
                let checked = result::check(&text, &display_name(file), Some(&catalog));
                match checked.result {
                    Some(result) => parsed.push(result),
                    None => {
                        if cli.json {
                            print_json(
                                &json!({"ok": false, "file": display_name(file), "diagnostics": checked.diagnostics}),
                            );
                        } else {
                            print_diagnostics(&checked.diagnostics);
                        }
                        return Ok(ExitCode::FAILURE);
                    }
                }
            }
            let item = find(&catalog, &parsed[0].scenario.id)?;
            let verdict = comparability::compare(&item.scenario, &parsed[0], &parsed[1]);
            if cli.json {
                print_json(&json!({"ok": verdict.comparable, "verdict": verdict}));
            } else {
                if verdict.comparable {
                    println!("Сопоставимы по метрикам: {}", verdict.metrics.join(", "));
                    if !verdict.statistically_supported {
                        let limited: Vec<&str> = verdict
                            .metrics
                            .iter()
                            .filter(|m| !verdict.statistical_metrics.contains(m))
                            .map(String::as_str)
                            .collect();
                        println!(
                            "Внимание: меньше {} успешных измерений хотя бы в одном результате — статистические выводы недопустимы по метрикам: {}",
                            comparability::MIN_STATISTICAL_VALUES,
                            limited.join(", ")
                        );
                    }
                } else {
                    println!("Несопоставимы:");
                    for blocker in &verdict.blockers {
                        println!("  - {blocker}");
                    }
                }
                for difference in &verdict.subject_differences {
                    println!("Предмет сравнения: {difference}");
                }
            }
            Ok(code(verdict.comparable))
        }
        Command::Vocabulary => {
            let limits: serde_json::Map<String, serde_json::Value> = WorkloadKind::ALL
                .iter()
                .map(|kind| (kind.as_str().to_owned(), json!(kind.limits())))
                .collect();
            let value = json!({
                "catalog_schema_version": vocabulary::CATALOG_SCHEMA_VERSION,
                "result_schema_version": vocabulary::RESULT_SCHEMA_VERSION,
                "metrics": vocabulary::REGISTRY,
                "run_limits": limits,
                "builtin_facts": BuiltinFact::ALL,
                "mandatory_facts": vocabulary::MANDATORY_FACTS,
                "mandatory_comparable_facts": vocabulary::MANDATORY_COMPARABLE_FACTS,
            });
            if cli.json {
                print_json(&value);
            } else {
                for definition in vocabulary::REGISTRY {
                    println!(
                        "{} [{}] — {}",
                        definition.metric, definition.unit, definition.summary
                    );
                    for m in definition.measurements {
                        let platforms: Vec<&str> = m.platforms.iter().map(|p| p.as_str()).collect();
                        println!(
                            "    {}/{} ({}){}: {}",
                            m.scope,
                            m.source,
                            platforms.join(", "),
                            if m.sampled {
                                ", сэмплируется"
                            } else {
                                ""
                            },
                            m.meaning
                        );
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[derive(Serialize)]
struct Summary<'a> {
    id: &'a str,
    module: &'a str,
    revision: u32,
    kind: WorkloadKind,
    status: vocabulary::ScenarioStatus,
    title: &'a str,
    profiles: Vec<&'a str>,
    file: &'a str,
}

fn summary(item: &LoadedScenario) -> Summary<'_> {
    let s = &item.scenario;
    Summary {
        id: &s.id,
        module: &item.module,
        revision: s.revision,
        kind: s.kind,
        status: s.status,
        title: &s.title,
        profiles: s.profiles.iter().map(|p| p.id.as_str()).collect(),
        file: &item.file,
    }
}

fn catalog_root(cli: &Cli) -> Result<PathBuf, String> {
    if let Some(path) = &cli.catalog {
        if !path.join(catalog::CATALOG_FILE).is_file() {
            return Err(format!(
                "в каталоге {} нет {}",
                path.display(),
                catalog::CATALOG_FILE
            ));
        }
        return Ok(path.clone());
    }
    let cwd = std::env::current_dir().map_err(|e| format!("текущий каталог недоступен: {e}"))?;
    cwd.ancestors()
        .map(|dir| dir.join(DEFAULT_CATALOG))
        .find(|dir| dir.join(catalog::CATALOG_FILE).is_file())
        .ok_or_else(|| {
            format!(
                "каталог {DEFAULT_CATALOG}/{} не найден выше текущего каталога; укажите --catalog",
                catalog::CATALOG_FILE
            )
        })
}

fn load(cli: &Cli) -> Result<(Option<Catalog>, Vec<Diagnostic>), String> {
    let root = catalog_root(cli)?;
    let loaded = catalog::load(&root);
    Ok((loaded.catalog, loaded.diagnostics))
}

fn require_catalog(cli: &Cli) -> Result<Catalog, String> {
    let (catalog, diagnostics) = load(cli)?;
    catalog.ok_or_else(|| {
        let errors: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(ToString::to_string)
            .collect();
        format!(
            "каталог некорректен, запустите validate:\n{}",
            errors.join("\n")
        )
    })
}

fn find<'a>(catalog: &'a Catalog, id: &str) -> Result<&'a LoadedScenario, String> {
    catalog
        .scenario(id)
        .ok_or_else(|| format!("сценарий «{id}» не найден; список — команда list"))
}

/// Путь так, как его передал вызывающий: одноимённые файлы из разных
/// каталогов в отчёте должны различаться.
fn display_name(path: &Path) -> String {
    path.display().to_string()
}

fn print_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        eprintln!("{diagnostic}");
    }
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON сериализуется")
    );
}
