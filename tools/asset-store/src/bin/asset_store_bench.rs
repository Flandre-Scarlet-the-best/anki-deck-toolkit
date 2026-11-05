//! Исполнитель измеряемой нагрузки `asset-store` для `benchmark-runner`.
//!
//! Каждая подкоманда — один шаг жизненного цикла сценария каталога
//! `benchmarks/scenarios/asset-store.toml`; логика — `asset_store::bench_workload`.
//! Итог шага — одна строка JSON в stdout, ошибка — текст в stderr. Коды
//! завершения: 0 — успех, 1 — отказ операции или постусловия, 2 — неверное
//! использование.

use std::path::PathBuf;

use asset_store::bench_workload::{self, BenchError, Operation, WorkloadSpec};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "asset-store-bench",
    about = "Измеряемая нагрузка asset-store для сценариев benchmark-runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Location {
    /// Файл спецификации нагрузки (TOML).
    #[arg(long)]
    spec: PathBuf,
    /// Рабочий каталог нагрузки; удаляется только с маркером asset-store-bench.
    #[arg(long)]
    workdir: PathBuf,
}

#[derive(Debug, Args)]
struct OperationArgs {
    /// verify-integrity, read-batch, read-single, ingest, ingest-contention,
    /// batch-reopen, batch-resume-reload или batch-checkpoint.
    operation: String,
    #[command(flatten)]
    location: Location,
    /// Число независимых писателей ingest-contention.
    #[arg(long)]
    writers: Option<u32>,
    /// Вид checkpoint пакета: unchanged или modified.
    #[arg(long)]
    change: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Объём нагрузки в JSON без записи на диск (проба входа сценария).
    Describe {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Построить эталонное хранилище серии.
    Setup(Location),
    /// Восстановить рабочую копию из эталона перед изменяющим прогоном.
    Prepare(Location),
    /// Измеряемая операция.
    Run(OperationArgs),
    /// Постусловия операции вне измеряемого интервала.
    Verify(OperationArgs),
    /// Синтетический корпус MP3 для `asset-store.audio-normalizer-batch` (новый каталог).
    AudioCorpus {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        count: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Привести помеченный рабочий каталог нормализатора к исходному состоянию.
    AudioReset {
        #[arg(long)]
        workdir: PathBuf,
        /// Сохранить кэш результатов (тёплый повторный запуск).
        #[arg(long)]
        keep_cache: bool,
        /// Удалить рабочий каталог целиком.
        #[arg(long)]
        remove: bool,
    },
    /// Постусловие: пакет нормализатора завершён, вход не изменён.
    AudioVerify {
        #[arg(long)]
        workdir: PathBuf,
        #[arg(long)]
        batch_id: String,
    },
    /// Удалить помеченный рабочий каталог.
    Clean {
        #[arg(long)]
        workdir: PathBuf,
        /// Удалить только рабочую копию, сохранив эталон для следующей серии.
        #[arg(long)]
        keep_template: bool,
    },
}

fn operation(args: &OperationArgs) -> Result<Operation, BenchError> {
    Operation::parse(&args.operation, args.writers, args.change.as_deref())
}

fn audio(command: &Command) -> Option<Result<serde_json::Value, String>> {
    use asset_store::bench_audio;
    let result = match command {
        Command::AudioCorpus { out, count, seed } => {
            bench_audio::generate_corpus(out, *count, *seed)
        }
        Command::AudioReset {
            workdir,
            keep_cache,
            remove,
        } => bench_audio::reset_workspace(workdir, *keep_cache, *remove),
        Command::AudioVerify { workdir, batch_id } => bench_audio::verify(workdir, batch_id),
        _ => return None,
    };
    Some(result.map_err(|error| error.to_string()))
}

fn execute(command: Command) -> Result<String, BenchError> {
    let summary = match command {
        Command::AudioCorpus { .. } | Command::AudioReset { .. } | Command::AudioVerify { .. } => {
            unreachable!("аудио-подкоманды обработаны отдельно")
        }
        Command::Describe { spec } => {
            let spec = WorkloadSpec::load(&spec)?;
            return Ok(serde_json::to_string(&bench_workload::describe(&spec))
                .expect("описание сериализуется"));
        }
        Command::Setup(location) => bench_workload::setup(&location.spec, &location.workdir)?,
        Command::Prepare(location) => bench_workload::prepare(&location.spec, &location.workdir)?,
        Command::Run(args) => {
            let operation = operation(&args)?;
            bench_workload::run(&args.location.spec, &args.location.workdir, operation)?
        }
        Command::Verify(args) => {
            let operation = operation(&args)?;
            bench_workload::verify(&args.location.spec, &args.location.workdir, operation)?
        }
        Command::Clean {
            workdir,
            keep_template,
        } => bench_workload::clean(&workdir, keep_template)?,
    };
    Ok(serde_json::to_string(&summary).expect("итог сериализуется"))
}

fn main() {
    let cli = Cli::parse();
    if let Some(result) = audio(&cli.command) {
        match result {
            Ok(value) => println!("{value}"),
            Err(error) => {
                eprintln!("asset-store-bench: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    match execute(cli.command) {
        Ok(line) => println!("{line}"),
        Err(error) => {
            eprintln!("asset-store-bench: {error}");
            std::process::exit(error.exit_code());
        }
    }
}
