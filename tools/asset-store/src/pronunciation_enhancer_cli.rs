//! Подкоманды `pronunciation-assets enhancer`: установка и проверка весов,
//! анализ одного MP3, пакетное улучшение поверх пакета Balanced и отчёт.
//!
//! Обработка не обращается к сети; установка весов — отдельная явная
//! операция из локального архива или каталога. Вход только читается, в
//! основное хранилище ничего не публикуется.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use crate::pronunciation::enhancer::{self, EnhancementProfile, ItemContext};
use crate::pronunciation::enhancer_backend as backend;
use crate::pronunciation::enhancer_batch as ebatch;
use crate::pronunciation::enhancer_report as ereport;
use crate::pronunciation::normalizer::{self, Mode, NormalizerProfile, Toolchain, Variant};
use crate::pronunciation::normalizer_batch as nbatch;
use crate::pronunciation_normalizer_cli::{create_new_file, ok, read_json, stop_flag};
use crate::{AssetError, ErrorCode};

#[derive(clap::Args, Clone)]
pub(crate) struct ModelArgs {
    /// Исполняемый файл воркера (по умолчанию — рядом с этой программой или в PATH).
    #[arg(long)]
    backend_bin: Option<PathBuf>,
    /// Каталог проверенных весов; по умолчанию — установка в корне моделей.
    #[arg(long)]
    model_dir: Option<PathBuf>,
    /// Корень моделей (по умолчанию `$XDG_DATA_HOME/anki-deck-toolkit/speech-models`).
    #[arg(long)]
    models_root: Option<PathBuf>,
}

/// Вид backend в аргументах CLI.
#[derive(clap::ValueEnum, Clone, Copy)]
pub(crate) enum BackendArg {
    Deepfilternet3,
    /// Экспериментальный: только denoiser Resemble Enhance.
    ResembleEnhanceDenoise,
}

impl From<BackendArg> for backend::BackendKind {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Deepfilternet3 => Self::DeepFilterNet3,
            BackendArg::ResembleEnhanceDenoise => Self::ResembleEnhanceDenoise,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum EnhancerAction {
    /// Печатает встроенный профиль backend (по умолчанию — DeepFilterNet3).
    Profile {
        #[arg(long, value_enum, default_value_t = BackendArg::Deepfilternet3)]
        backend: BackendArg,
    },
    /// Явная установка весов без сети. DeepFilterNet3: из архива upstream,
    /// каталога с четырьмя файлами или копии crate в локальном реестре Cargo;
    /// Resemble Enhance: только из локального каталога `--source`.
    ModelInstall {
        #[arg(long, value_enum, default_value_t = BackendArg::Deepfilternet3)]
        backend: BackendArg,
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        models_root: Option<PathBuf>,
        #[arg(long)]
        backend_bin: Option<PathBuf>,
    },
    /// Проверяет воркер и веса и печатает идентичность backend.
    ModelIdentify {
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        profile: Option<PathBuf>,
    },
    /// Balanced (normalize) и улучшение одного MP3 без записи; `--out`
    /// дополнительно сохраняет улучшенный MP3, если он выбран.
    Analyze {
        #[arg(long)]
        file: PathBuf,
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        normalizer_profile: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Фиксирует конфигурацию поверх завершённого пакета Balanced и обрабатывает.
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        base_batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        #[arg(long)]
        max_items: Option<usize>,
    },
    BatchResume {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        jobs: Option<usize>,
        #[arg(long)]
        max_items: Option<usize>,
    },
    BatchStatus {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
    },
    BatchCancel {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
    },
    /// Новый каталог отчёта: summary.json, items.jsonl, items.tsv, base_deferred.tsv.
    BatchReport {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Повторно хеширует вход: корпус не изменён.
    BatchVerifySources {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
    },
}

fn load_profile(
    path: Option<&Path>,
    protected: &BTreeSet<PathBuf>,
) -> Result<EnhancementProfile, AssetError> {
    let profile = match path {
        Some(path) => read_json(path, protected)?,
        None => EnhancementProfile::conservative(),
    };
    profile.validate()?;
    Ok(profile)
}

fn models_root(explicit: Option<&Path>) -> Result<PathBuf, AssetError> {
    explicit
        .map(Path::to_path_buf)
        .or_else(backend::default_models_root)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::InvalidTransition,
                "не задан корень моделей: укажите --models-root",
            )
        })
}

/// Воркер и каталог весов для backend профиля.
fn resolve_model(
    model: &ModelArgs,
    profile: &EnhancementProfile,
) -> Result<(PathBuf, PathBuf), AssetError> {
    let kind = profile.backend.kind;
    let worker = backend::locate_worker(model.backend_bin.as_deref(), kind)?;
    let dir = match &model.model_dir {
        Some(dir) => dir.clone(),
        None => backend::installed_model_dir(&models_root(model.models_root.as_deref())?, kind),
    };
    Ok((worker, dir))
}

fn open_batch(
    workspace: &Path,
    batch_id: &str,
    protected: &BTreeSet<PathBuf>,
) -> Result<
    (
        PathBuf,
        ebatch::EnhancerBatch,
        nbatch::NormalizerBatch,
        nbatch::Inventory,
    ),
    AssetError,
> {
    let workspace = nbatch::existing_workspace(workspace, protected)?;
    let (batch, base, inventory) = ebatch::load(&workspace, batch_id)?;
    nbatch::check_boundaries(Path::new(&batch.input_root), &workspace, protected)?;
    Ok((workspace, batch, base, inventory))
}

fn run_json(summary: ebatch::RunSummary) -> serde_json::Value {
    let interrupted = summary.outcome == "interrupted";
    let cancelled = summary.outcome == "cancelled";
    serde_json::json!({
        "schema_version": 1,
        "outcome": if interrupted { "paused" } else if cancelled { "cancelled" } else { "ok" },
        "result": {
            "run": {
                "outcome": summary.outcome,
                "processed_now": summary.processed_now,
                "reused": summary.reused,
                "remaining": summary.remaining,
                "infrastructure_failed_now": summary.infrastructure_failed_now,
                "wall_ms": summary.wall_ms,
            },
            "batch": summary.batch,
            "paused": if interrupted { serde_json::json!({"code":"interrupted"}) } else { serde_json::Value::Null },
        },
    })
}

pub(crate) fn execute(
    action: &EnhancerAction,
    protected: &BTreeSet<PathBuf>,
) -> Result<serde_json::Value, AssetError> {
    match action {
        EnhancerAction::Profile { backend } => Ok(ok(serde_json::json!(
            EnhancementProfile::for_backend((*backend).into())
        ))),
        EnhancerAction::ModelInstall {
            backend: kind,
            source,
            models_root: root,
            backend_bin,
        } => {
            let kind = backend::BackendKind::from(*kind);
            let worker = backend::locate_worker(backend_bin.as_deref(), kind)?;
            let root = models_root(root.as_deref())?;
            for path in [Some(root.as_path()), source.as_deref()]
                .into_iter()
                .flatten()
            {
                nbatch::ensure_outside(
                    path,
                    &nbatch::protected_roots(protected),
                    "веса не читаются из decks/** и не пишутся туда",
                )?;
            }
            Ok(ok(backend::install_model(
                &worker,
                kind,
                source.as_deref(),
                &root,
            )?))
        }
        EnhancerAction::ModelIdentify { model, profile } => {
            let profile = load_profile(profile.as_deref(), protected)?;
            let (worker, dir) = resolve_model(model, &profile)?;
            let identity = backend::identify(
                &worker,
                &dir,
                profile.backend.kind,
                &profile.backend.inference,
            )?;
            Ok(ok(serde_json::json!({
                "worker": worker,
                "model_dir": dir,
                "identity": identity,
            })))
        }
        EnhancerAction::Analyze {
            file,
            model,
            profile,
            normalizer_profile,
            out,
        } => {
            let profile = load_profile(profile.as_deref(), protected)?;
            let base_profile = match normalizer_profile {
                Some(path) => read_json(path, protected)?,
                None => NormalizerProfile::balanced(),
            };
            base_profile.validate()?;
            let output = match out {
                Some(out) => {
                    let source =
                        std::fs::canonicalize(file).map_err(|e| AssetError::io("исходник", e))?;
                    let mut forbidden = nbatch::protected_roots(protected);
                    forbidden.push(source.parent().unwrap_or(Path::new("/")).to_path_buf());
                    Some(nbatch::ensure_outside(
                        out,
                        &forbidden,
                        "улучшенный MP3 пишется только вне каталога исходника и decks/**",
                    )?)
                }
                None => None,
            };
            let (worker, dir) = resolve_model(model, &profile)?;
            let toolchain = Toolchain::detect()?;
            let identity = backend::identify(
                &worker,
                &dir,
                profile.backend.kind,
                &profile.backend.inference,
            )?;
            let bytes = crate::pronunciation_cli::read_local(
                file,
                protected,
                crate::pronunciation::MAX_AUDIO_BYTES,
            )?;
            let base = normalizer::process(&bytes, &base_profile, Mode::Normalize, &toolchain)?;
            let selected = match base.result.gate.proposed_variant {
                Some(Variant::Processed) => base.output_bytes.clone(),
                Some(Variant::Original) => Some(bytes.clone()),
                None => None,
            };
            let config = enhancer::config_sha256(&profile, &identity, &base.result.config_sha256);
            let ctx = ItemContext {
                profile: &profile,
                backend: &identity,
                worker: &worker,
                model_dir: &dir,
                base_profile: &base_profile,
                base_toolchain: &toolchain,
                config_sha256: &config,
            };
            let item = enhancer::process_item(&ctx, &bytes, &base.result, selected.as_deref())?;
            let written = match (&output, &item.output_bytes, item.result.gate.selected) {
                (Some(path), Some(bytes), Some(enhancer::Selection::Enhanced)) => {
                    create_new_file(path, bytes)?;
                    true
                }
                _ => false,
            };
            Ok(ok(serde_json::json!({
                "written": written,
                "base": base.result,
                "result": item.result,
            })))
        }
        EnhancerAction::BatchStart {
            batch_id,
            base_batch_id,
            workspace,
            model,
            profile,
            jobs,
            max_items,
        } => {
            let profile = load_profile(profile.as_deref(), protected)?;
            let workspace = nbatch::existing_workspace(workspace, protected)?;
            let (base, _) = nbatch::load(&workspace, base_batch_id)?;
            nbatch::check_boundaries(Path::new(&base.input_root), &workspace, protected)?;
            let (worker, dir) = resolve_model(model, &profile)?;
            let dir =
                ebatch::check_model_dir(&dir, Path::new(&base.input_root), &workspace, protected)?;
            let started = ebatch::start(ebatch::StartRequest {
                batch_id,
                workspace: &workspace,
                base_batch_id,
                profile,
                worker: &worker,
                model_dir: &dir,
                jobs: *jobs,
            })?;
            Ok(run_json(ebatch::resume(
                &workspace,
                &started.batch_id,
                None,
                *max_items,
                stop_flag(),
            )?))
        }
        EnhancerAction::BatchResume {
            batch_id,
            workspace,
            jobs,
            max_items,
        } => {
            let (workspace, _, _, _) = open_batch(workspace, batch_id, protected)?;
            Ok(run_json(ebatch::resume(
                &workspace,
                batch_id,
                *jobs,
                *max_items,
                stop_flag(),
            )?))
        }
        EnhancerAction::BatchStatus {
            batch_id,
            workspace,
        } => {
            let (_, batch, _, _) = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(batch)))
        }
        EnhancerAction::BatchCancel {
            batch_id,
            workspace,
        } => {
            let (workspace, _, _, _) = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(ebatch::cancel(&workspace, batch_id)?)))
        }
        EnhancerAction::BatchReport {
            batch_id,
            workspace,
            out,
        } => {
            let (workspace, batch, _, inventory) = open_batch(workspace, batch_id, protected)?;
            let mut forbidden = nbatch::protected_roots(protected);
            forbidden.push(PathBuf::from(&batch.input_root));
            forbidden.push(workspace.join(".runtime"));
            let out = nbatch::ensure_outside(
                out,
                &forbidden,
                "отчёт пишется вне входа, служебной области и decks/**",
            )?;
            let cache = ebatch::EnhancerCache::open(&workspace)?;
            let results = ebatch::collect_results(&cache, &batch, &inventory)?;
            Ok(ok(serde_json::json!(ereport::write_report(
                &batch, &inventory, &results, &out
            )?)))
        }
        EnhancerAction::BatchVerifySources {
            batch_id,
            workspace,
        } => {
            let (_, _, base, inventory) = open_batch(workspace, batch_id, protected)?;
            let check = ebatch::verify_sources(&base, &inventory)?;
            if !check.verified {
                return Err(AssetError::with_details(
                    ErrorCode::IntegrityMismatch,
                    "вход изменился после инвентаризации",
                    serde_json::json!(check),
                ));
            }
            Ok(ok(serde_json::json!(check)))
        }
    }
}
