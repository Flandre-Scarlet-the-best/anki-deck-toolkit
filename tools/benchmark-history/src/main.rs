//! CLI локальной истории бенчмарков.

use benchmark_contract::catalog::{self, Catalog};
use benchmark_contract::diagnostic::Severity;
use benchmark_contract::result::{BenchmarkResult, OutcomeStatus};
use benchmark_history::analysis::{self, Candidate};
use benchmark_history::index::{self, Filter};
use benchmark_history::report::{self, Base, Item};
use benchmark_history::store::{self, Error, Store};
use benchmark_history::{format, location, snapshot};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_CATALOG: &str = "benchmarks";

/// Стабильные коды возврата.
#[derive(Debug, Clone, Copy)]
enum Exit {
    Success = 0,
    /// Ответ отрицательный: несопоставимо, базы нет, найдены проблемы.
    Negative = 1,
    Usage = 2,
    Error = 3,
    /// Хотя бы один документ отвергнут контрактом.
    Rejected = 4,
    /// Архивная копия записана, индекс не обновлён.
    Index = 5,
}

#[derive(Debug, Parser)]
#[command(
    name = "benchmark-history",
    version,
    about = "Локальная история проверенных результатов бенчмарков: архив, поиск, база, сравнение и отчёты. Ничего не измеряет и не публикует."
)]
struct Cli {
    /// Корень хранилища; по умолчанию BENCHMARK_HISTORY_DIR или каталог данных XDG.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// Каталог с catalog.toml; по умолчанию — benchmarks/ ближайшего родителя текущего каталога.
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    /// Стабильный JSON вместо текста для человека.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Args, Default)]
struct Filters {
    /// ID сценария.
    #[arg(long)]
    scenario: Option<String>,
    /// Модуль сценария.
    #[arg(long)]
    module: Option<String>,
    /// Ревизия сценария.
    #[arg(long)]
    revision: Option<u32>,
    /// Профиль.
    #[arg(long)]
    profile: Option<String>,
    /// Итог серии: success, partial, failure, unsupported; можно повторять.
    #[arg(long = "outcome")]
    outcomes: Vec<String>,
    /// Записан не раньше: YYYY-MM-DD или метка UTC.
    #[arg(long)]
    since: Option<String>,
    /// Записан не позже: YYYY-MM-DD (весь день) или метка UTC.
    #[arg(long)]
    until: Option<String>,
    /// Версия модуля из provenance.
    #[arg(long)]
    module_version: Option<String>,
    /// Префикс коммита измеренного кода.
    #[arg(long)]
    commit: Option<String>,
    /// Префикс идентичности любого входа (например, экспорта).
    #[arg(long)]
    input: Option<String>,
    /// Известный факт окружения: --env <имя>=<значение>; можно повторять.
    #[arg(long = "env", value_parser = parse_pair)]
    environment: Vec<(String, String)>,
    /// Не больше N строк.
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Где хранилище и что в нём.
    Status,
    /// Принять result.json или каталоги с ними после проверки контрактом и каталогом.
    Import {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Результаты истории от новых к старым.
    List(Filters),
    /// Подробности результата.
    Show {
        id: String,
        /// Исходный JSON архивной копии.
        #[arg(long)]
        raw: bool,
    },
    /// Кандидаты в базу для результата и почему каждый подходит или нет.
    Baseline {
        id: String,
        /// Выбирать только среди результатов коммита с этим префиксом.
        #[arg(long)]
        commit: Option<String>,
    },
    /// Сравнить результат с базой: до/после, разности, оценка относительно разброса.
    Compare { base: String, target: String },
    /// Раздел «## Производительность» для описания PR в Markdown.
    Report {
        #[arg(required = true)]
        ids: Vec<String>,
        /// База для результата того же сценария и профиля; можно повторять.
        #[arg(long = "baseline")]
        baselines: Vec<String>,
        /// Новая функциональность: первое измерение без состояния «до».
        #[arg(long, conflicts_with = "baselines")]
        first: bool,
        /// Автоматическая база только среди коммита с этим префиксом.
        #[arg(long, conflicts_with_all = ["baselines", "first"])]
        commit: Option<String>,
    },
    /// Обновить снимок на странице модуля (RU, EN, zh-CN); без --apply только показать.
    Snapshot {
        id: String,
        /// Русская страница модуля, например docs/ru/anki-repo/performance.md.
        #[arg(long)]
        page: PathBuf,
        /// Записать страницы.
        #[arg(long)]
        apply: bool,
        /// Подтвердить смену эталонного окружения снимка.
        #[arg(long)]
        replace_environment: bool,
        /// Подтвердить смену эталонной нагрузки снимка.
        #[arg(long)]
        replace_workload: bool,
        /// Разрешить результат рабочей копии с изменениями.
        #[arg(long)]
        allow_dirty: bool,
    },
    /// Проверить целостность каждой архивной копии и сверить индекс с архивом; архив не меняет, исправления индекса при открытии считаются расхождением.
    Verify,
    /// Пересобрать индекс SQLite из архива.
    Reindex,
    /// Скопировать архив в новый или пустой каталог для переноса.
    Backup { destination: PathBuf },
    /// Принять записи резервной копии, созданной backup.
    Restore { source: PathBuf },
}

/// Как выбрана база одного проверяемого результата.
enum Plan {
    Unmeasured,
    First,
    Chosen {
        id: String,
        base: Box<BenchmarkResult>,
        how: String,
        notes: Vec<String>,
    },
    Missing(Vec<String>),
}

fn parse_pair(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "ожидается <имя>=<значение>".to_owned())
}

type Failure = (Exit, String);

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => ExitCode::from(code as u8),
        Err((exit, message)) => {
            if cli.json {
                print_json(&json!({"ok": false, "exit_code": exit as u8, "error": message}));
            } else {
                eprintln!("benchmark-history: {message}");
            }
            ExitCode::from(exit as u8)
        }
    }
}

fn failure(error: Error) -> Failure {
    let exit = match &error {
        Error::Usage(_) => Exit::Usage,
        Error::Store(_) => Exit::Error,
        Error::Rejected(_) => Exit::Rejected,
        Error::Index { .. } => Exit::Index,
    };
    (exit, error.to_string())
}

fn catalog_root(cli: &Cli) -> Result<Option<PathBuf>, Failure> {
    if let Some(path) = &cli.catalog {
        if !path.join(catalog::CATALOG_FILE).is_file() {
            return Err((
                Exit::Error,
                format!(
                    "в каталоге {} нет {}",
                    path.display(),
                    catalog::CATALOG_FILE
                ),
            ));
        }
        return Ok(Some(path.clone()));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| (Exit::Error, format!("текущий каталог недоступен: {e}")))?;
    Ok(cwd
        .ancestors()
        .map(|dir| dir.join(DEFAULT_CATALOG))
        .find(|dir| dir.join(catalog::CATALOG_FILE).is_file()))
}

fn load_catalog(root: &Path) -> Result<Catalog, Failure> {
    let loaded = catalog::load(root);
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

fn require_catalog(cli: &Cli) -> Result<Catalog, Failure> {
    match catalog_root(cli)? {
        Some(root) => load_catalog(&root),
        None => Err((
            Exit::Error,
            format!(
                "каталог {DEFAULT_CATALOG}/ не найден выше текущего каталога; укажите --catalog"
            ),
        )),
    }
}

fn store_root(cli: &Cli) -> Result<location::Location, Failure> {
    let found = location::resolve(cli.store.as_deref()).map_err(|e| (Exit::Usage, e))?;
    // Граница Git проверяется по самому пути и не зависит от каталога сценариев.
    location::refuse_inside_git(&found.root, "хранилище истории").map_err(|e| (Exit::Usage, e))?;
    if let Some(root) = catalog_root(cli)?
        && let Ok(catalog) = load_catalog(&root)
    {
        location::refuse_inside_repository(&found.root, &catalog.repository_root)
            .map_err(|e| (Exit::Usage, e))?;
    }
    Ok(found)
}

fn open(cli: &Cli) -> Result<Store, Failure> {
    let root = store_root(cli)?.root;
    let store = Store::open(&root).map_err(failure)?;
    report_sync(cli, store.opened_sync());
    Ok(store)
}

/// Исправления индекса при открытии видны человеку в stderr.
fn report_sync(cli: &Cli, sync: &store::Sync) {
    if cli.json {
        return;
    }
    for note in &sync.notes {
        eprintln!("benchmark-history: {note}");
    }
    for id in &sync.repaired {
        eprintln!(
            "benchmark-history: строки индекса {id} не совпали с контрольной суммой — перестроены из архива"
        );
    }
    if !sync.indexed.is_empty() {
        eprintln!(
            "benchmark-history: доиндексировано записей архива: {}",
            sync.indexed.len()
        );
    }
    for id in &sync.dropped {
        eprintln!("benchmark-history: архивной копии {id} нет — строка индекса удалена");
    }
    for problem in &sync.problems {
        eprintln!(
            "benchmark-history: запись не индексирована — {}: {}",
            problem.item, problem.reason
        );
    }
}

fn run(cli: &Cli) -> Result<Exit, Failure> {
    match &cli.command {
        Command::Status => {
            let found = store_root(cli)?;
            let exists = found.root.join(store::FORMAT_FILE).is_file();
            let count = if exists {
                Some(
                    open(cli)?
                        .index()
                        .count()
                        .map_err(|e| (Exit::Error, e.to_string()))?,
                )
            } else {
                None
            };
            if cli.json {
                print_json(&json!({
                    "ok": true,
                    "store": found.root,
                    "origin": found.origin,
                    "exists": exists,
                    "results": count,
                    "index_schema": index::SCHEMA_VERSION,
                    "archive_version": store::ARCHIVE_VERSION,
                }));
            } else {
                println!("хранилище: {} ({:?})", found.root.display(), found.origin);
                match count {
                    Some(n) => println!("результатов: {n}"),
                    None => println!(
                        "хранилище ещё не создано: его создаст первый import или run исполнителя"
                    ),
                }
            }
            Ok(Exit::Success)
        }
        Command::Import { paths } => {
            let catalog = require_catalog(cli)?;
            let mut store = open(cli)?;
            let mut files = Vec::new();
            let mut skipped = Vec::new();
            for path in paths {
                collect(path, true, &mut files, &mut skipped)
                    .map_err(|e| (Exit::Usage, format!("{}: {e}", path.display())))?;
            }
            if files.is_empty() {
                return Err((
                    Exit::Usage,
                    format!(
                        "не найдено ни одного файла результата (пропущено файлов, не похожих на результат: {})",
                        skipped.len()
                    ),
                ));
            }
            let mut reports = Vec::new();
            let mut worst = Exit::Success;
            for file in &files {
                let name = file.display().to_string();
                let outcome = std::fs::read_to_string(file)
                    .map_err(|e| Error::Usage(format!("{name}: {e}")))
                    .and_then(|text| store.import(&text, &name, Some(&catalog)));
                match outcome {
                    Ok(imported) => {
                        if !cli.json {
                            println!(
                                "{}  {}  {} · {} · {}{}",
                                if imported.created {
                                    "принят   "
                                } else {
                                    "уже есть "
                                },
                                &imported.id[..16],
                                imported.scenario,
                                imported.profile,
                                imported.outcome,
                                if imported.warnings.is_empty() {
                                    String::new()
                                } else {
                                    format!("  ({} предупр.)", imported.warnings.len())
                                }
                            );
                            for warning in &imported.warnings {
                                eprintln!("{warning}");
                            }
                        }
                        reports.push(json!({"file": name, "ok": true, "imported": imported}));
                    }
                    Err(error) => {
                        let (exit, message) = failure(error.clone());
                        if (exit as u8) > (worst as u8) {
                            worst = exit;
                        }
                        if !cli.json {
                            eprintln!("не принят {name}: {message}");
                        }
                        let diagnostics = match &error {
                            Error::Rejected(d) => json!(d),
                            _ => json!([]),
                        };
                        reports.push(json!({"file": name, "ok": false, "error": message, "diagnostics": diagnostics}));
                    }
                }
            }
            if cli.json {
                print_json(
                    &json!({"ok": matches!(worst, Exit::Success), "results": reports, "skipped": skipped}),
                );
            } else if !skipped.is_empty() {
                eprintln!(
                    "пропущено файлов, не похожих на результат: {}",
                    skipped.len()
                );
            }
            Ok(worst)
        }
        Command::List(filters) => {
            let store = open(cli)?;
            let filter = to_filter(filters)?;
            let rows = store
                .index()
                .query(&filter)
                .map_err(|e| (Exit::Error, e.to_string()))?;
            if cli.json {
                print_json(&json!({"ok": true, "results": rows}));
            } else {
                if rows.is_empty() {
                    println!("подходящих результатов нет");
                }
                for row in &rows {
                    println!(
                        "{}  {}  {} r{}  {}  {:<11} {}/{}  {} {}{}",
                        &row.id[..16],
                        row.recorded_at,
                        row.scenario,
                        row.revision,
                        row.profile,
                        row.outcome,
                        row.successful_runs,
                        row.runs,
                        row.module_version,
                        row.git_commit
                            .as_deref()
                            .map(|c| &c[..c.len().min(12)])
                            .unwrap_or("—"),
                        if row.dirty == Some(true) {
                            "+изменения"
                        } else {
                            ""
                        }
                    );
                }
            }
            Ok(Exit::Success)
        }
        Command::Show { id, raw } => {
            let store = open(cli)?;
            let id = store.resolve(id).map_err(failure)?;
            let (text, result) = store.load(&id).map_err(failure)?;
            if *raw {
                print!("{text}");
            } else if cli.json {
                print_json(&json!({"ok": true, "id": id, "result": result}));
            } else {
                show(&id, &result);
            }
            Ok(Exit::Success)
        }
        Command::Baseline { id, commit } => {
            let catalog = require_catalog(cli)?;
            let store = open(cli)?;
            let id = store.resolve(id).map_err(failure)?;
            let (_, target) = store.load(&id).map_err(failure)?;
            let scenario = declaration(&catalog, &target)?;
            let list = candidates(&store, &catalog, scenario, &id, &target)?;
            let chosen = analysis::choose(&list, commit.as_deref());
            if cli.json {
                print_json(&json!({
                    "ok": chosen.is_some(),
                    "target": id,
                    "rule": analysis::BASELINE_RULE,
                    "caveat": analysis::IMPLEMENTATION_CAVEAT,
                    "chosen": chosen.map(|c| &c.id),
                    "candidates": list,
                }));
            } else {
                println!("правило: {}", analysis::BASELINE_RULE);
                println!("оговорка: {}", analysis::IMPLEMENTATION_CAVEAT);
                if list.is_empty() {
                    println!(
                        "других результатов {} · {} в истории нет",
                        target.scenario.id, target.profile.id
                    );
                }
                for c in &list {
                    let mark = if chosen.is_some_and(|x| x.id == c.id) {
                        "→"
                    } else if c.eligible {
                        "+"
                    } else {
                        "−"
                    };
                    println!(
                        "{mark} {}  {}  {}  {}{}",
                        &c.id[..16],
                        c.recorded_at,
                        analysis::wire_name(&c.outcome),
                        c.git_commit
                            .as_deref()
                            .map(|g| &g[..g.len().min(12)])
                            .unwrap_or("—"),
                        if c.dirty == Some(true) {
                            " +изменения"
                        } else {
                            ""
                        }
                    );
                    for reason in &c.reasons {
                        println!("      не подходит: {reason}");
                    }
                    for note in &c.notes {
                        println!("      учтите: {note}");
                    }
                }
                match chosen {
                    Some(c) => println!("база: {}", c.id),
                    None => println!("пригодной базы нет"),
                }
            }
            Ok(if chosen.is_some() {
                Exit::Success
            } else {
                Exit::Negative
            })
        }
        Command::Compare { base, target } => {
            let catalog = require_catalog(cli)?;
            let store = open(cli)?;
            let base_id = store.resolve(base).map_err(failure)?;
            let target_id = store.resolve(target).map_err(failure)?;
            if base_id == target_id {
                return Err((
                    Exit::Usage,
                    format!("база {base_id} совпадает с проверяемым результатом"),
                ));
            }
            let (_, base) = store.load(&base_id).map_err(failure)?;
            let (_, target) = store.load(&target_id).map_err(failure)?;
            let scenario = declaration(&catalog, &target)?;
            let comparison = analysis::compare(scenario, &base, &target);
            if cli.json {
                print_json(
                    &json!({"ok": comparison.verdict.comparable, "base": base_id, "target": target_id, "comparison": comparison}),
                );
            } else {
                print_comparison(&comparison);
            }
            Ok(if comparison.verdict.comparable {
                Exit::Success
            } else {
                Exit::Negative
            })
        }
        Command::Report {
            ids,
            baselines,
            first,
            commit,
        } => {
            let catalog = require_catalog(cli)?;
            let store = open(cli)?;
            let mut targets = Vec::new();
            for id in ids {
                let id = store.resolve(id).map_err(failure)?;
                let (_, result) = store.load(&id).map_err(failure)?;
                if targets.iter().any(|(_, r): &(String, BenchmarkResult)| {
                    r.scenario.id == result.scenario.id && r.profile.id == result.profile.id
                }) {
                    return Err((
                        Exit::Usage,
                        format!(
                            "для {} · {} передано больше одного результата",
                            result.scenario.id, result.profile.id
                        ),
                    ));
                }
                targets.push((id, result));
            }
            let mut explicit: Vec<Option<(String, BenchmarkResult)>> = vec![None; targets.len()];
            for base in baselines {
                let id = store.resolve(base).map_err(failure)?;
                if targets.iter().any(|(target_id, _)| *target_id == id) {
                    return Err((
                        Exit::Usage,
                        format!("база {id} совпадает с проверяемым результатом"),
                    ));
                }
                let (_, result) = store.load(&id).map_err(failure)?;
                let slot = targets
                    .iter()
                    .position(|(_, t)| t.scenario.id == result.scenario.id && t.profile.id == result.profile.id)
                    .ok_or_else(|| (Exit::Usage, format!("база {id} не относится ни к одному из проверяемых сценариев и профилей")))?;
                if explicit[slot].is_some() {
                    return Err((
                        Exit::Usage,
                        format!(
                            "для {} · {} передано больше одной базы",
                            result.scenario.id, result.profile.id
                        ),
                    ));
                }
                explicit[slot] = Some((id, result));
            }
            let mut plans = Vec::new();
            for (n, (id, target)) in targets.iter().enumerate() {
                let scenario = declaration(&catalog, target)?;
                let pool = candidates(&store, &catalog, scenario, id, target)?;
                let measured = matches!(
                    target.outcome.status,
                    OutcomeStatus::Success | OutcomeStatus::Partial
                );
                let plan = if !measured {
                    Plan::Unmeasured
                } else if *first {
                    Plan::First
                } else if let Some((base_id, base)) = explicit[n].take() {
                    // Явная база сравнивается даже вне правила, но отступления видны в отчёте.
                    let notes = pool
                        .iter()
                        .find(|c| c.id == base_id)
                        .map(|c| {
                            c.reasons
                                .iter()
                                .map(|r| {
                                    format!("не соответствует правилу автоматического выбора — {r}")
                                })
                                .chain(c.notes.iter().cloned())
                                .collect()
                        })
                        .unwrap_or_default();
                    Plan::Chosen {
                        id: base_id,
                        base: Box::new(base),
                        how: "указана явно".into(),
                        notes,
                    }
                } else {
                    match analysis::choose(&pool, commit.as_deref()) {
                        Some(c) => Plan::Chosen {
                            id: c.id.clone(),
                            base: Box::new(store.load(&c.id).map_err(failure)?.1),
                            how: format!("выбрана автоматически: {}", analysis::BASELINE_RULE),
                            notes: c.notes.clone(),
                        },
                        None => Plan::Missing(missing_reasons(&pool, commit.as_deref())),
                    }
                };
                plans.push(plan);
            }
            let mut rendered = Vec::new();
            for ((id, target), plan) in targets.iter().zip(&plans) {
                let scenario = declaration(&catalog, target)?;
                let base = match plan {
                    Plan::Unmeasured => Base::Missing {
                        reasons: Vec::new(),
                    },
                    Plan::First => Base::First,
                    Plan::Missing(reasons) => Base::Missing {
                        reasons: reasons.clone(),
                    },
                    Plan::Chosen {
                        id,
                        base,
                        how,
                        notes,
                    } => Base::Chosen {
                        id,
                        result: base,
                        comparison: analysis::compare(scenario, base, target),
                        how: how.clone(),
                        notes: notes.clone(),
                    },
                };
                rendered.push(Item {
                    id,
                    result: target,
                    base,
                });
            }
            let markdown = report::pr_section(&rendered);
            if cli.json {
                print_json(&json!({"ok": true, "markdown": markdown}));
            } else {
                print!("{markdown}");
            }
            Ok(Exit::Success)
        }
        Command::Snapshot {
            id,
            page,
            apply,
            replace_environment,
            replace_workload,
            allow_dirty,
        } => {
            let catalog = require_catalog(cli)?;
            let store = open(cli)?;
            let id = store.resolve(id).map_err(failure)?;
            let (_, result) = store.load(&id).map_err(failure)?;
            let scenario = declaration(&catalog, &result)?;
            let name = page
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with(".en.md") || name.ends_with(".zh-CN.md") || !name.ends_with(".md") {
                return Err((Exit::Usage, "--page — русская страница модуля (docs/ru/<модуль>/performance.md); переводы не обновляются автоматически".into()));
            }
            let absolute = std::path::absolute(page).map_err(|e| (Exit::Usage, e.to_string()))?;
            let repository = catalog
                .repository_root
                .canonicalize()
                .unwrap_or(catalog.repository_root.clone());
            if !absolute
                .canonicalize()
                .unwrap_or(absolute.clone())
                .starts_with(&repository)
            {
                return Err((
                    Exit::Usage,
                    "страница снимка должна лежать в репозитории каталога".into(),
                ));
            }
            // Тождество нагрузки прежнего снимка доказывается результатами
            // той же пары в локальной истории: на странице идентичностей нет.
            // Публикуемый результат snapshot сам исключает из доказательства.
            let mut history = Vec::new();
            for other in store
                .index()
                .series(&result.scenario.id, &result.profile.id)
                .map_err(|e| (Exit::Error, e.to_string()))?
            {
                let (_, recorded) = store.load(&other).map_err(failure)?;
                history.push(snapshot::Recorded::of(scenario, &recorded));
            }
            let options = snapshot::Options {
                replace_environment: *replace_environment,
                replace_workload: *replace_workload,
                allow_dirty: *allow_dirty,
                history,
            };
            let plan = snapshot::plan(&absolute, scenario, &result, &options)
                .map_err(|e| (Exit::Negative, e))?;
            let notes = if *apply && plan.action != snapshot::Action::Unchanged {
                snapshot::apply(&plan).map_err(|e| (Exit::Error, e))?
            } else {
                Vec::new()
            };
            if cli.json {
                print_json(&json!({"ok": true, "applied": *apply, "plan": plan, "notes": notes}));
            } else {
                let action = match plan.action {
                    snapshot::Action::Added => "снимок будет добавлен",
                    snapshot::Action::Replaced => "снимок будет заменён",
                    snapshot::Action::Unchanged => "снимок уже актуален",
                };
                println!("{action}: {} · {}", result.scenario.id, result.profile.id);
                println!("нагрузка: {}", plan.workload.describe());
                for p in &plan.pages {
                    println!("  {}", p.path.display());
                }
                println!("\n{}", plan.pages[0].block);
                for note in &notes {
                    eprintln!("benchmark-history: {note}");
                }
                if plan.action != snapshot::Action::Unchanged {
                    println!(
                        "{}",
                        if *apply {
                            "страницы записаны"
                        } else {
                            "ничего не записано: для записи добавьте --apply"
                        }
                    );
                }
            }
            Ok(Exit::Success)
        }
        Command::Verify => {
            // Проверка не создаёт хранилище по опечатке в пути.
            let root = store_root(cli)?.root;
            if !root.join(store::FORMAT_FILE).is_file() {
                return Err((Exit::Error, format!("хранилища {} нет", root.display())));
            }
            let store = open(cli)?;
            // Открытие согласует индекс с архивом до сверки; найденное им —
            // тоже расхождение, и проверка не выдаёт его за целый индекс.
            let opened = store.opened_sync();
            let opened_clean = opened.repaired.is_empty()
                && opened.indexed.is_empty()
                && opened.dropped.is_empty()
                && opened.problems.is_empty()
                && opened.notes.is_empty();
            let report = store.verify().map_err(failure)?;
            let ok = opened_clean && report.problems.is_empty() && report.index_problems.is_empty();
            if cli.json {
                print_json(&json!({"ok": ok, "opened_sync": opened, "verification": report}));
            } else {
                if !opened_clean {
                    println!(
                        "индекс исправлен при открытии: перестроено записей {}, доиндексировано {}, удалено строк {}, не индексировано {}, заметок {}",
                        opened.repaired.len(),
                        opened.indexed.len(),
                        opened.dropped.len(),
                        opened.problems.len(),
                        opened.notes.len()
                    );
                }
                println!("проверено записей: {}", report.checked);
                for p in &report.problems {
                    println!("проблема {}: {}", p.item, p.reason);
                }
                for p in &report.index_problems {
                    println!("индекс {}: {}", p.item, p.reason);
                }
                if report.problems.is_empty() {
                    println!("архив цел");
                }
                if opened_clean && report.index_problems.is_empty() {
                    println!("индекс совпадает с архивом");
                }
            }
            Ok(if ok { Exit::Success } else { Exit::Negative })
        }
        Command::Reindex => {
            let root = store_root(cli)?.root;
            let (store, sync) = Store::rebuild(&root).map_err(failure)?;
            let count = store
                .index()
                .count()
                .map_err(|e| (Exit::Error, e.to_string()))?;
            if cli.json {
                print_json(
                    &json!({"ok": sync.problems.is_empty(), "results": count, "sync": sync}),
                );
            } else {
                println!("индекс пересобран: записей {count}");
                for p in &sync.problems {
                    println!("не индексирована {}: {}", p.item, p.reason);
                }
            }
            Ok(if sync.problems.is_empty() {
                Exit::Success
            } else {
                Exit::Negative
            })
        }
        Command::Backup { destination } => {
            let store = open(cli)?;
            let copied = store.backup(destination).map_err(failure)?;
            if cli.json {
                print_json(&json!({"ok": true, "copied": copied, "destination": destination}));
            } else {
                println!("скопировано записей: {copied} в {}", destination.display());
            }
            Ok(Exit::Success)
        }
        Command::Restore { source } => {
            let mut store = open(cli)?;
            let restored = store.restore(source).map_err(failure)?;
            let ok = restored.complete;
            if cli.json {
                print_json(&json!({"ok": ok, "restored": restored}));
            } else {
                println!(
                    "в инвентаре резервной копии: {}; добавлено: {}, уже было: {}, заменено повреждённых: {}",
                    restored.expected,
                    restored.added.len(),
                    restored.present.len(),
                    restored.replaced.len()
                );
                for id in &restored.replaced {
                    println!("заменена повреждённая копия {id}: прежний файл отложен в corrupt/");
                }
                for p in &restored.problems {
                    println!("не принято {}: {}", p.item, p.reason);
                }
                if !ok {
                    println!(
                        "резервная копия перенесена не полностью: исправные записи приняты, перечисленные выше — нет"
                    );
                }
            }
            Ok(if ok { Exit::Success } else { Exit::Rejected })
        }
    }
}

/// Файлы результатов: явно названный файл принимается всегда, в каталоге —
/// только документы, похожие на результат; отвергнутые исполнителем копии
/// (`*.rejected.json`) не принимаются. Символическая ссылка на каталог внутри
/// обхода пропускается: ссылка на предка иначе повторяет обход дерева.
fn collect(
    path: &Path,
    explicit: bool,
    files: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if path.is_dir() {
        if !explicit && std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            skipped.push(path.to_path_buf());
            return Ok(());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for entry in entries {
            collect(&entry, false, files, skipped)?;
        }
        return Ok(());
    }
    if explicit {
        files.push(path.to_path_buf());
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.ends_with(".json") || name.ends_with(".rejected.json") {
        skipped.push(path.to_path_buf());
        return Ok(());
    }
    let looks_like_result = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| {
            v.get("schema_version").is_some()
                && v.get("scenario").is_some()
                && v.get("provenance").is_some()
        });
    if looks_like_result {
        files.push(path.to_path_buf());
    } else {
        skipped.push(path.to_path_buf());
    }
    Ok(())
}

fn to_filter(f: &Filters) -> Result<Filter, Failure> {
    let outcomes = f
        .outcomes
        .iter()
        .map(|o| {
            serde_json::from_value::<OutcomeStatus>(json!(o)).map_err(|_| {
                (
                    Exit::Usage,
                    format!("итог «{o}»: ожидается success, partial, failure или unsupported"),
                )
            })
        })
        .collect::<Result<_, _>>()?;
    let bound = |text: &Option<String>, end| {
        text.as_deref()
            .map(|t| index::bound(t, end).map_err(|e| (Exit::Usage, e)))
            .transpose()
    };
    Ok(Filter {
        scenario: f.scenario.clone(),
        module: f.module.clone(),
        revision: f.revision,
        profile: f.profile.clone(),
        outcomes,
        since: bound(&f.since, false)?,
        until: bound(&f.until, true)?,
        module_version: f.module_version.clone(),
        commit: f.commit.clone(),
        input_identity: f.input.clone(),
        environment: f.environment.clone(),
        limit: f.limit,
    })
}

fn declaration<'c>(
    catalog: &'c Catalog,
    result: &BenchmarkResult,
) -> Result<&'c benchmark_contract::scenario::Scenario, Failure> {
    catalog
        .scenario(&result.scenario.id)
        .map(|s| &s.scenario)
        .ok_or_else(|| (Exit::Usage, format!("сценарий {} не найден в каталоге: сопоставимость оценивается только по декларации", result.scenario.id)))
}

/// Изменённые пути между двумя коммитами по локальному Git репозитория
/// каталога. Коммита может не быть в этом клоне: тогда состав изменений
/// неизвестен, и выбор базы это оговаривает.
fn git_changes(repository: &Path, a: &str, b: &str) -> Result<Vec<String>, String> {
    for commit in [a, b] {
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("«{commit}» не идентификатор коммита"));
        }
    }
    let mut command = std::process::Command::new("git");
    for name in analysis::GIT_LOCAL_ENV {
        command.env_remove(name);
    }
    let output = command
        .arg("-C")
        .arg(repository)
        .args(["diff", "--name-only", "-z", "--no-renames", a, b, "--"])
        .output()
        .map_err(|e| format!("Git не запущен: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff в репозитории каталога не выполнен: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

fn candidates(
    store: &Store,
    catalog: &Catalog,
    scenario: &benchmark_contract::scenario::Scenario,
    id: &str,
    target: &BenchmarkResult,
) -> Result<Vec<Candidate>, Failure> {
    let ids = store
        .index()
        .series(&target.scenario.id, &target.profile.id)
        .map_err(|e| (Exit::Error, e.to_string()))?;
    let mut pool = Vec::new();
    for other in ids.into_iter().filter(|o| o != id) {
        let (_, result) = store.load(&other).map_err(failure)?;
        pool.push((other, result));
    }
    let repository = &catalog.repository_root;
    Ok(analysis::candidates(
        scenario,
        id,
        target,
        &pool,
        &|a, b| git_changes(repository, a, b),
    ))
}

fn missing_reasons(list: &[Candidate], commit: Option<&str>) -> Vec<String> {
    if list.is_empty() {
        return Vec::new();
    }
    // Подходящие по правилу кандидаты сюда попадают, только если их отсёк `--commit`.
    let mut reasons = vec![match commit {
        Some(prefix) => format!(
            "кандидатов {}, ни один не подходит по правилу «{}» с коммитом, начинающимся с «{prefix}»",
            list.len(),
            analysis::BASELINE_RULE
        ),
        None => format!(
            "кандидатов {}, ни один не подходит по правилу «{}»",
            list.len(),
            analysis::BASELINE_RULE
        ),
    }];
    reasons.extend(list.iter().take(5).map(|c| {
        let why = if c.eligible {
            format!("коммит не начинается с «{}»", commit.unwrap_or_default())
        } else {
            c.reasons.join(", ")
        };
        format!("`{}`: {why}", &c.id[..16])
    }));
    reasons
}

fn show(id: &str, r: &BenchmarkResult) {
    println!("результат {id}");
    println!(
        "сценарий:   {} r{} ({}) · профиль {}",
        r.scenario.id, r.scenario.revision, r.scenario.kind, r.profile.id
    );
    println!(
        "итог:       {}{}",
        analysis::wire_name(&r.outcome.status),
        r.outcome
            .reason
            .as_deref()
            .map(|x| format!(" — {x}"))
            .unwrap_or_default()
    );
    println!("записан:    {}", r.provenance.recorded_at);
    let code = &r.provenance.code;
    println!(
        "код:        {} {} · коммит {} · {}",
        r.scenario.module,
        code.module_version,
        code.git_commit.as_deref().unwrap_or("—"),
        match code.dirty {
            Some(true) => "с изменениями",
            Some(false) => "без изменений",
            None => "состояние не записано",
        }
    );
    for input in &r.inputs {
        println!(
            "вход:       {} ({:?}) {}",
            input.id, input.identity.method, input.identity.value
        );
        for (name, q) in &input.size {
            println!("            {name} = {}", format::quantity(q.value, q.unit));
        }
    }
    for (name, fact) in &r.environment {
        match fact {
            benchmark_contract::result::Fact::Known { value } => {
                println!("окружение:  {name} = {value}")
            }
            benchmark_contract::result::Fact::Unavailable { reason } => {
                println!("окружение:  {name} недоступен: {reason}")
            }
        }
    }
    println!("прогоны:    {} (прогрев {})", r.runs.len(), r.warmup_runs);
    for metric in &r.metrics {
        match analysis::Side::of(r, &metric.key) {
            analysis::Side::Measured { stats, .. } => println!(
                "метрика:    {:<48} {} ± {} (n={}), медиана {}",
                metric.key,
                format::quantity(stats.mean, metric.unit),
                stats
                    .stddev
                    .map(|s| format::quantity(s, metric.unit))
                    .unwrap_or_else(|| "—".into()),
                stats.count,
                format::quantity(stats.median, metric.unit)
            ),
            _ => println!(
                "метрика:    {:<48} {:?}: {}",
                metric.key,
                metric.availability,
                metric.reason.as_deref().unwrap_or("")
            ),
        }
    }
}

fn print_comparison(c: &analysis::Comparison) {
    if c.verdict.comparable {
        println!("сопоставимы по метрикам: {}", c.verdict.metrics.join(", "));
    } else {
        println!("несопоставимы:");
        for b in &c.verdict.blockers {
            println!("  - {b}");
        }
    }
    for d in &c.verdict.subject_differences {
        println!("предмет сравнения: {d}");
    }
    for row in &c.rows {
        let side = |s: &analysis::Side| match s {
            analysis::Side::Measured { stats, .. } => format!(
                "{} ± {} (n={})",
                format::quantity(stats.mean, row.unit),
                stats
                    .stddev
                    .map(|v| format::quantity(v, row.unit))
                    .unwrap_or_else(|| "—".into()),
                stats.count
            ),
            analysis::Side::Missing {
                availability,
                reason,
            } => format!("{availability:?}: {}", reason.as_deref().unwrap_or("")),
            analysis::Side::Absent => "не записана".into(),
        };
        let delta = row
            .delta
            .as_ref()
            .map(|d| {
                format!(
                    "{} ({})",
                    format::signed(d.absolute, row.unit),
                    d.relative
                        .map(format::percent)
                        .unwrap_or_else(|| "база 0".into())
                )
            })
            .unwrap_or_else(|| "—".into());
        println!(
            "{:<48} до {}  после {}  Δ {}  {:?}",
            row.key,
            row.before.as_ref().map(side).unwrap_or_default(),
            side(&row.after),
            delta,
            row.assessment
        );
    }
    println!(
        "оценка относительно разброса повторов не является проверкой статистической значимости"
    );
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON сериализуется")
    );
}
