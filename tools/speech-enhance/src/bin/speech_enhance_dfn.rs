//! Процесс-воркер DeepFilterNet3. Сеть не используется; веса читаются только
//! из явно указанного каталога и сверяются с закреплённым манифестом.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use speech_enhance::enhance::{EnhanceErrorKind, Enhancer, InferenceParams};
use speech_enhance::model::{self, ModelError};
use speech_enhance::protocol::{self, ErrorReport, Identity, ProcessResult, StreamInfo};

#[derive(Parser)]
#[command(
    name = "speech-enhance-dfn",
    about = "Локальный CPU-воркер DeepFilterNet3"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Проверить веса и напечатать идентичность backend.
    Identify {
        #[arg(long)]
        model_dir: PathBuf,
    },
    /// Улучшить один клип PCM f32 48 kHz.
    Process {
        #[arg(long)]
        model_dir: PathBuf,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        channels: usize,
        #[arg(long, default_value_t = 100.0)]
        atten_lim_db: f32,
        #[arg(long, default_value_t = 0.0)]
        post_filter_beta: f32,
        #[arg(long, default_value_t = 0)]
        context_ms: u32,
    },
    /// Установить веса из локального архива upstream или каталога.
    InstallModel {
        /// Архив `DeepFilterNet3_onnx.tar.gz` или каталог с четырьмя файлами.
        #[arg(long, required_unless_present = "from_cargo_registry")]
        source: Option<PathBuf>,
        /// Взять копию весов crate `deep-filter-rt` из локального реестра Cargo.
        #[arg(long, conflicts_with = "source")]
        from_cargo_registry: bool,
        #[arg(long)]
        models_root: PathBuf,
    },
}

struct Failure {
    exit: i32,
    code: String,
    message: String,
}

fn model_failure(e: ModelError) -> Failure {
    Failure {
        exit: protocol::EXIT_MODEL,
        code: e.kind.code().into(),
        message: e.message,
    }
}

fn failure(exit: i32, code: &str, message: impl Into<String>) -> Failure {
    Failure {
        exit,
        code: code.into(),
        message: message.into(),
    }
}

fn print(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value).expect("JSON сериализуем")
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_input(path: &Path) -> Result<Vec<f32>, Failure> {
    let file = std::fs::File::open(path)
        .map_err(|e| failure(protocol::EXIT_INPUT, "input_unreadable", e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(protocol::MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(protocol::EXIT_INPUT, "input_unreadable", e.to_string()))?;
    if bytes.len() as u64 > protocol::MAX_INPUT_BYTES || !bytes.len().is_multiple_of(4) {
        return Err(failure(
            protocol::EXIT_INPUT,
            "input_invalid",
            "вход больше предела или не кратен 4 байтам",
        ));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

fn run(command: Command) -> Result<(), Failure> {
    match command {
        Command::Identify { model_dir } => {
            let (identity, files) = model::load_dir(&model_dir).map_err(model_failure)?;
            let enhancer = Enhancer::new(&files, &InferenceParams::default())
                .map_err(|e| failure(protocol::EXIT_MODEL, "model_load_failed", e.message))?;
            print(&Identity {
                schema_version: 1,
                kind: "speech_enhance_identity".into(),
                backend: model::MODEL_VARIANT.into(),
                worker: protocol::worker_info(),
                runtime: protocol::runtime_info(),
                model: identity,
                stream: StreamInfo {
                    sample_rate: model::SAMPLE_RATE,
                    hop_samples: enhancer.hop() as u64,
                    delay_samples: enhancer.delay() as u64,
                    max_channels: 2,
                },
            });
            Ok(())
        }
        Command::Process {
            model_dir,
            input,
            output,
            channels,
            atten_lim_db,
            post_filter_beta,
            context_ms,
        } => {
            let (identity, files) = model::load_dir(&model_dir).map_err(model_failure)?;
            if context_ms > 5_000 {
                return Err(failure(
                    protocol::EXIT_INPUT,
                    "input_invalid",
                    "--context-ms больше 5000",
                ));
            }
            let params = InferenceParams {
                atten_lim_db,
                post_filter_beta,
                context_ms,
            };
            let samples = read_input(&input)?;
            let enhancer = Enhancer::new(&files, &params)
                .map_err(|e| failure(protocol::EXIT_MODEL, "model_load_failed", e.message))?;
            let (enhanced, diagnostics) =
                enhancer
                    .enhance(channels, &samples)
                    .map_err(|e| match e.kind {
                        EnhanceErrorKind::Input => {
                            failure(protocol::EXIT_INPUT, "input_invalid", e.message)
                        }
                        // Сбой инференса — не свойство входа: хост повторяет его
                        // как сбой среды.
                        EnhanceErrorKind::Runtime => {
                            failure(protocol::EXIT_RUNTIME, "inference_failed", e.message)
                        }
                    })?;
            let bytes: Vec<u8> = enhanced.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
                .map_err(|e| failure(protocol::EXIT_RUNTIME, "output_io_failure", e.to_string()))?;
            print(&ProcessResult {
                schema_version: 1,
                kind: "speech_enhance_result".into(),
                model_manifest_sha256: identity.manifest_sha256,
                params,
                diagnostics,
                output_sha256: sha256_hex(&bytes),
            });
            Ok(())
        }
        Command::InstallModel {
            source,
            from_cargo_registry,
            models_root,
        } => {
            let source = match source {
                Some(source) => source,
                None if from_cargo_registry => model::cargo_registry_copy().ok_or_else(|| {
                    failure(
                        protocol::EXIT_MODEL,
                        "model_missing",
                        format!(
                            "в локальном реестре Cargo нет проверяемой копии deep-filter-rt {}",
                            protocol::DEEP_FILTER_RT_VERSION
                        ),
                    )
                })?,
                None => unreachable!("clap требует --source или --from-cargo-registry"),
            };
            let (path, identity) = model::install(&source, &models_root).map_err(model_failure)?;
            print(&serde_json::json!({
                "schema_version": 1,
                "kind": "speech_enhance_install",
                "path": path,
                "model": identity,
            }));
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            print(&ErrorReport {
                schema_version: 1,
                kind: "speech_enhance_error".into(),
                code: f.code,
                message: f.message,
            });
            ExitCode::from(f.exit as u8)
        }
    }
}
