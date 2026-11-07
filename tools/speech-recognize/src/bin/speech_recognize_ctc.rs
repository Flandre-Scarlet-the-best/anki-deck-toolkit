//! Процесс-воркер акустических CTC-моделей: `identify`, `process`,
//! `install-model`. Сеть не используется; веса проверяются по закреплённому
//! манифесту при каждом запуске. Контракт — `docs/ru/asset-store/recognizer.md`.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use rustix::fs::{Mode, OFlags, open};

use speech_recognize::infer;
use speech_recognize::model::{self, ModelError, PinnedModel, sha256_hex};
use speech_recognize::protocol::{
    EXIT_INPUT, EXIT_MODEL, EXIT_RUNTIME, ErrorReport, Identity, InstallReport, MAX_BATCH_ITEMS,
    MAX_INPUT_BYTES, ProcessItem, ProcessReport, runtime_info, worker_info,
};

#[derive(Parser)]
#[command(
    name = "speech-recognize-ctc",
    about = "Локальный CTC-инференс японских акустических моделей"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Проверяет веса и печатает идентичность воркера и модели.
    Identify {
        #[arg(long)]
        model: String,
        #[arg(long)]
        model_dir: PathBuf,
        /// Полная загрузка весов в модель (а не только сверка файлов).
        #[arg(long)]
        load: bool,
    },
    /// Эмиссии CTC для моно PCM f32 16 kHz. Пары `--input`/`--output`
    /// повторяются; веса проверяются и загружаются один раз на вызов.
    Process {
        #[arg(long)]
        model: String,
        #[arg(long)]
        model_dir: PathBuf,
        #[arg(long, required = true)]
        input: Vec<PathBuf>,
        #[arg(long, required = true)]
        output: Vec<PathBuf>,
    },
    /// Устанавливает проверенные файлы из локального снимка репозитория модели.
    InstallModel {
        #[arg(long)]
        model: String,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        models_root: PathBuf,
    },
}

struct Failure {
    exit: i32,
    code: String,
    message: String,
}

impl From<ModelError> for Failure {
    fn from(e: ModelError) -> Self {
        Self {
            exit: EXIT_MODEL,
            code: e.kind.code().into(),
            message: e.message,
        }
    }
}

fn failure(exit: i32, code: &str, message: impl Into<String>) -> Failure {
    Failure {
        exit,
        code: code.into(),
        message: message.into(),
    }
}

fn pinned(id: &str) -> Result<&'static PinnedModel, Failure> {
    model::find(id).ok_or_else(|| {
        failure(
            EXIT_MODEL,
            "model_unknown",
            format!("модель {id} не закреплена в этом воркере"),
        )
    })
}

fn read_input(path: &Path) -> Result<Vec<u8>, Failure> {
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| failure(EXIT_INPUT, "input_unreadable", format!("вход: {e}")))?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| failure(EXIT_INPUT, "input_unreadable", e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_INPUT_BYTES || meta.len() % 4 != 0 {
        return Err(failure(
            EXIT_INPUT,
            "input_invalid",
            "вход — не обычный файл, длиннее предела или не кратен f32",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(EXIT_INPUT, "input_unreadable", e.to_string()))?;
    if bytes.len() as u64 != meta.len() {
        return Err(failure(
            EXIT_INPUT,
            "input_invalid",
            "вход изменился при чтении",
        ));
    }
    Ok(bytes)
}

fn emit(value: &impl serde::Serialize) -> Result<(), Failure> {
    let text = serde_json::to_string(value).expect("отчёт сериализуем");
    println!("{text}");
    Ok(())
}

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.command {
        Command::Identify {
            model,
            model_dir,
            load,
        } => {
            let pinned = pinned(&model)?;
            let identity = if load {
                model::load_dir(pinned, &model_dir)?.identity
            } else {
                model::verify_dir(pinned, &model_dir)?
            };
            emit(&Identity {
                schema_version: 1,
                kind: "speech_recognize_identity".into(),
                worker: worker_info(),
                runtime: runtime_info(),
                model: identity,
            })
        }
        Command::Process {
            model,
            model_dir,
            input,
            output,
        } => {
            let start = Instant::now();
            let pinned = pinned(&model)?;
            if input.len() != output.len() || input.len() > MAX_BATCH_ITEMS {
                return Err(failure(
                    EXIT_INPUT,
                    "input_invalid",
                    format!("нужно поровну --input и --output, не больше {MAX_BATCH_ITEMS}"),
                ));
            }
            // Все входы читаются и проверяются до загрузки модели: отказ
            // входа не тратит время на веса и не оставляет частичных выводов.
            let mut inputs = Vec::with_capacity(input.len());
            for path in &input {
                let bytes = read_input(path)?;
                let samples: Vec<f32> = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                if samples.iter().any(|s| !s.is_finite()) {
                    return Err(failure(
                        EXIT_INPUT,
                        "input_invalid",
                        "вход содержит неконечные отсчёты",
                    ));
                }
                inputs.push((sha256_hex(&bytes), samples));
            }
            let loaded = model::load_dir(pinned, &model_dir)?;
            let load_ms = start.elapsed().as_secs_f64() * 1000.0;
            let mut items = Vec::with_capacity(inputs.len());
            for ((input_sha256, samples), out) in inputs.iter().zip(&output) {
                let emissions = infer::emissions(&loaded.model, &loaded.identity, samples);
                if emissions.log_probs.iter().any(|v| !v.is_finite()) {
                    return Err(failure(
                        EXIT_RUNTIME,
                        "inference_failed",
                        "неконечные эмиссии",
                    ));
                }
                let raw: Vec<u8> = emissions
                    .log_probs
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect();
                let mut file = File::create_new(out)
                    .map_err(|e| failure(EXIT_RUNTIME, "output_failed", format!("вывод: {e}")))?;
                file.write_all(&raw)
                    .and_then(|()| file.sync_all())
                    .map_err(|e| failure(EXIT_RUNTIME, "output_failed", format!("вывод: {e}")))?;
                items.push(ProcessItem {
                    input_samples: samples.len() as u64,
                    input_sha256: input_sha256.clone(),
                    frames: emissions.frames as u64,
                    vocab: emissions.vocab as u64,
                    output_sha256: sha256_hex(&raw),
                });
            }
            emit(&ProcessReport {
                schema_version: 1,
                kind: "speech_recognize_result".into(),
                worker: worker_info(),
                runtime: runtime_info(),
                model_id: loaded.identity.id.clone(),
                model_manifest_sha256: loaded.identity.manifest_sha256.clone(),
                items,
                load_ms,
                wall_ms: start.elapsed().as_secs_f64() * 1000.0,
            })
        }
        Command::InstallModel {
            model,
            source,
            models_root,
        } => {
            let pinned = pinned(&model)?;
            let identity = model::install(pinned, &source, &models_root)?;
            emit(&InstallReport {
                schema_version: 1,
                kind: "speech_recognize_install".into(),
                model: identity,
                path: model::installed_dir(&models_root, pinned)
                    .display()
                    .to_string(),
            })
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            let report = ErrorReport {
                schema_version: 1,
                kind: "speech_recognize_error".into(),
                code: f.code,
                message: f.message,
            };
            println!("{}", serde_json::to_string(&report).expect("сериализуемо"));
            let _ = std::io::stdout().flush();
            ExitCode::from(f.exit as u8)
        }
    }
}
