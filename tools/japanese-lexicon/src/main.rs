//! CLI `japanese-lexicon`: управление локальными словарями и поиск чтений.

use clap::{Args, Parser, Subcommand};
use japanese_lexicon::error::{Code, Error, code_name};
use japanese_lexicon::lookup::{self, Lexicon, LookupResult, SourceState};
use japanese_lexicon::manager::{self, OperationReport, Options, State, StatusReport};
use japanese_lexicon::store;
use japanese_lexicon::upstream::HttpTransport;
use serde::Serialize;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// Стабильные коды возврата.
#[derive(Debug, Clone, Copy)]
enum Exit {
    Success = 0,
    /// Команда выполнена, но не для всех словарей успешно или проверка нашла проблемы.
    Partial = 1,
    /// Неверный вызов или нет подтверждения разрушительного действия.
    Usage = 2,
    Error = 3,
    /// Другая изменяющая операция уже выполняется.
    Busy = 4,
}

#[derive(Debug, Parser)]
#[command(
    name = "japanese-lexicon",
    version,
    about = "Japanese Lexicon: локальные словари Yomitan (Jitendex, JMnedict, KANJIDIC, JPDB), их установка и обновление, офлайн-поиск чтений по написанию с происхождением."
)]
struct Cli {
    /// Каталог данных; по умолчанию JAPANESE_LEXICON_DIR или каталог данных XDG.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Стабильный JSON вместо текста для человека.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Selection {
    /// Весь стандартный набор.
    #[arg(long)]
    all: bool,
    /// ID словарей: jitendex, jmnedict, kanjidic-english, jpdb-v2.2-freq-kana.
    ids: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Весь поддерживаемый набор: что установлено, версии, записи, пути, состояние.
    #[command(visible_alias = "list")]
    Status,
    /// Установить словари с живого upstream (или один — из локального ZIP).
    Install {
        #[command(flatten)]
        selection: Selection,
        /// Ручной импорт локального Yomitan ZIP для одного словаря (восстановление без сети).
        #[arg(long)]
        from_file: Option<PathBuf>,
    },
    /// Проверить upstream на новые версии; установленные данные не меняются.
    CheckUpdates {
        #[command(flatten)]
        selection: Selection,
    },
    /// Получить и активировать новые версии установленных словарей.
    Update {
        #[command(flatten)]
        selection: Selection,
    },
    /// Удалить словари из каталога данных подсистемы.
    #[command(visible_alias = "uninstall")]
    Remove {
        #[command(flatten)]
        selection: Selection,
        /// Подтверждение удаления всего набора без вопроса (для автоматизации).
        #[arg(long)]
        yes: bool,
    },
    /// Локальная проверка контрольных сумм, индексов и реестра без сети и исправлений.
    #[command(visible_alias = "doctor")]
    Verify {
        /// ID словарей; по умолчанию все.
        ids: Vec<String>,
    },
    /// Допустимые чтения точного написания с происхождением (офлайн).
    Lookup {
        /// Японское написание.
        #[arg(long)]
        surface: String,
        /// Пропустить N групп чтений (пагинация).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Не больше N групп чтений.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Не больше N свидетельств в группе.
        #[arg(long, default_value_t = 10)]
        max_evidence: usize,
    },
}

fn emit_json<T: Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    );
}

fn fail(error: &Error, json: bool) -> ExitCode {
    if json {
        emit_json(&serde_json::json!({
            "schema": "japanese-lexicon.error/1",
            "code": error.code,
            "message": error.message,
        }));
    } else {
        eprintln!("ошибка [{}]: {}", code_name(error.code), error.message);
    }
    ExitCode::from(match error.code {
        Code::Busy => Exit::Busy,
        Code::InvalidInput | Code::UnknownDictionary | Code::ConfirmationRequired => Exit::Usage,
        _ => Exit::Error,
    } as u8)
}

fn print_operation(report: &OperationReport) {
    println!(
        "{} — каталог данных {}",
        report.operation,
        report.data_dir.display()
    );
    for path in &report.cleaned {
        println!("  убран остаток прерванной операции: {}", path.display());
    }
    for item in &report.items {
        let outcome = serde_json::to_value(item.outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let code = item
            .code
            .map(|c| format!(" [{}]", code_name(c)))
            .unwrap_or_default();
        println!("  {:<22} {outcome}{code}: {}", item.id, item.message);
        if let Some(i) = &item.installed {
            println!(
                "      установлено: {} (revision {}, release {}), sha256 {}…",
                i.title,
                i.revision,
                i.release.as_deref().unwrap_or("неизвестен"),
                &i.archive_sha256[..12.min(i.archive_sha256.len())]
            );
        }
        if let Some(p) = &item.previous {
            println!("      прежняя версия: revision {}", p.revision);
        }
        if let Some(r) = &item.remote {
            println!(
                "      upstream: {} {} {}",
                r.locator,
                r.release.as_deref().unwrap_or("?"),
                r.asset_name
            );
        }
    }
    for path in &report.untouched {
        println!("  посторонняя запись не тронута: {}", path.display());
    }
    println!(
        "итог: {}",
        if report.ok {
            "успешно"
        } else {
            "есть отказы"
        }
    );
}

fn print_status(report: &StatusReport) {
    println!(
        "каталог данных: {} ({:?}){}",
        report.data_dir.display(),
        report.data_dir_origin,
        if report.exists {
            ""
        } else {
            " — ещё не создан"
        }
    );
    for d in &report.dictionaries {
        let state = match d.state {
            State::Ok => "ok",
            State::Corrupt => "ПОВРЕЖДЁН",
            State::NotInstalled => "не установлен",
        };
        println!("\n{} [{:?}] — {}: {state}", d.id, d.role, d.title);
        if let Some(i) = &d.installed {
            println!(
                "  версия: {} (release {}), установлен {}",
                i.revision,
                i.release.as_deref().unwrap_or("неизвестен"),
                i.installed_at
            );
            println!(
                "  источник: {} {} ({})",
                i.provider, i.locator, i.asset_name
            );
            println!(
                "  записи: terms {}, readings {}, redirects {}, kanji {}, term_meta {}",
                i.stats.terms,
                i.stats.readings,
                i.stats.redirects,
                i.stats.kanji,
                i.stats.term_meta
            );
            println!("  sha256 архива: {}", i.archive_sha256);
            println!("  путь: {}", i.path.display());
        }
        for p in &d.problems {
            println!("  проблема [{}]: {}", code_name(p.code), p.message);
        }
        println!("  лицензия: {}", d.license);
    }
    if report.operation_in_progress {
        println!("\nсейчас выполняется изменяющая операция");
    }
    for path in &report.leftovers {
        println!(
            "остаток прерванной операции (уберёт следующая install/update/remove): {}",
            path.display()
        );
    }
    for path in &report.untouched {
        println!("посторонняя запись (не трогается): {}", path.display());
    }
    for id in &report.unknown_registry_entries {
        println!("реестр ссылается на неизвестный словарь: {id}");
    }
}

fn print_lookup(r: &LookupResult) {
    let status = serde_json::to_value(r.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    println!("{} — {status}", r.query.surface);
    if !r.distinct_readings.is_empty() {
        println!("различные чтения: {}", r.distinct_readings.join(", "));
    }
    for g in &r.readings {
        let class = serde_json::to_value(g.class)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let sources: Vec<String> = g
            .evidence
            .iter()
            .filter_map(|e| e.dictionary.as_ref().map(|d| d.id.clone()))
            .fold(Vec::new(), |mut acc, id| {
                if !acc.contains(&id) {
                    acc.push(id);
                }
                acc
            });
        let tags: Vec<String> = g
            .evidence
            .iter()
            .flat_map(|e| e.tags.iter().map(|t| t.name.replace('\u{a0}', " ")))
            .fold(Vec::new(), |mut acc, t| {
                if !acc.contains(&t) {
                    acc.push(t);
                }
                acc
            });
        let freq: Vec<String> = g
            .frequency
            .iter()
            .filter_map(|f| f.display.clone())
            .collect();
        println!(
            "  {:<14} {} [{}]{}{}{}",
            class,
            g.reading,
            g.normalized,
            if sources.is_empty() {
                String::new()
            } else {
                format!(" ← {}", sources.join(", "))
            },
            if tags.is_empty() {
                String::new()
            } else {
                format!("; пометы: {}", tags.join(", "))
            },
            if freq.is_empty() {
                String::new()
            } else {
                format!("; JPDB: {}", freq.join(", "))
            }
        );
        if g.evidence_total > g.evidence.len() {
            println!(
                "      (+{} свидетельств скрыто; --max-evidence)",
                g.evidence_total - g.evidence.len()
            );
        }
    }
    for d in &r.redirects {
        println!(
            "  указатель на форму {}{} ({}) — не чтение запрошенного написания",
            d.target,
            d.target_reading
                .as_deref()
                .map(|r| format!(" 【{r}】"))
                .unwrap_or_default(),
            d.dictionary.id
        );
    }
    if let Some(k) = &r.kana
        && !k.issues.is_empty()
    {
        println!("  кана не выводится уверенно: {:?}", k.issues);
    }
    if r.truncation.truncated {
        println!(
            "  показано {} из {} групп (offset {}); --offset/--limit для продолжения",
            r.truncation.groups_returned, r.truncation.groups_total, r.truncation.offset
        );
    }
    if !r.missing_sources.is_empty() {
        println!("  нет источников: {}", r.missing_sources.join(", "));
        // Повреждённый индекс не должен выглядеть как неустановленный словарь.
        for s in r
            .sources
            .iter()
            .filter(|s| s.state == SourceState::Unavailable && r.missing_sources.contains(&s.id))
        {
            let code = s.code.map(code_name).unwrap_or_default();
            println!(
                "  {} недоступен ({code}): {}; проверьте командой verify {}",
                s.id,
                s.message.as_deref().unwrap_or(""),
                s.id
            );
        }
    }
    println!("  {}", r.notice);
}

fn confirm_remove_all(root: &std::path::Path, yes: bool) -> Result<(), Error> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(Error::new(
            Code::ConfirmationRequired,
            "remove --all удаляет все словари подсистемы: в неинтерактивном режиме добавьте --yes",
        ));
    }
    eprint!(
        "Удалить все словари japanese-lexicon из {}? Посторонние файлы не трогаются. Введите yes: ",
        root.display()
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| Error::io("чтение подтверждения", e))?;
    if answer.trim() == "yes" {
        Ok(())
    } else {
        Err(Error::new(Code::ConfirmationRequired, "удаление отменено"))
    }
}

fn operation(report: OperationReport, json: bool) -> ExitCode {
    if json {
        emit_json(&report);
    } else {
        print_operation(&report);
    }
    if report.ok {
        ExitCode::from(Exit::Success as u8)
    } else {
        ExitCode::from(Exit::Partial as u8)
    }
}

fn run(cli: Cli) -> Result<ExitCode, Error> {
    let location = store::resolve(cli.data_dir.as_deref())?;
    let root = location.root.clone();
    let options = Options::default();
    let json = cli.json;
    match cli.command {
        Command::Status => {
            let report = manager::status(&location)?;
            if json {
                emit_json(&report);
            } else {
                print_status(&report);
            }
            Ok(ExitCode::from(Exit::Success as u8))
        }
        Command::Verify { ids } => {
            let sources = manager::select(&ids, ids.is_empty())?;
            let report = manager::verify(&location, &sources)?;
            if json {
                emit_json(&report);
            } else {
                print_status(&report);
                println!(
                    "\nитог проверки: {}",
                    if report.ok {
                        "ok"
                    } else {
                        "найдены проблемы"
                    }
                );
            }
            Ok(ExitCode::from(if report.ok {
                Exit::Success
            } else {
                Exit::Partial
            } as u8))
        }
        Command::Install {
            selection,
            from_file,
        } => {
            let sources = manager::select(&selection.ids, selection.all)?;
            let transport = HttpTransport::new()?;
            Ok(operation(
                manager::install(&root, &sources, &transport, from_file.as_deref(), &options)?,
                json,
            ))
        }
        Command::CheckUpdates { selection } => {
            let sources = manager::select(&selection.ids, selection.all)?;
            let transport = HttpTransport::new()?;
            Ok(operation(
                manager::check_updates(&root, &sources, &transport)?,
                json,
            ))
        }
        Command::Update { selection } => {
            let sources = manager::select(&selection.ids, selection.all)?;
            let transport = HttpTransport::new()?;
            Ok(operation(
                manager::update(&root, &sources, &transport, &options)?,
                json,
            ))
        }
        Command::Remove { selection, yes } => {
            let sources = manager::select(&selection.ids, selection.all)?;
            if selection.all {
                confirm_remove_all(&root, yes)?;
            }
            Ok(operation(manager::remove(&root, &sources)?, json))
        }
        Command::Lookup {
            surface,
            offset,
            limit,
            max_evidence,
        } => {
            let lexicon = Lexicon::open(&root)?;
            let result = lexicon.lookup(
                &surface,
                &lookup::Options {
                    offset,
                    limit,
                    max_evidence,
                },
            )?;
            if json {
                emit_json(&result);
            } else {
                print_lookup(&result);
            }
            Ok(ExitCode::from(Exit::Success as u8))
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(code) => code,
        Err(error) => fail(&error, json),
    }
}
