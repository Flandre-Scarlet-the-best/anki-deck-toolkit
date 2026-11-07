//! Подкоманды `pronunciation-assets recognizer`: установка и проверка
//! акустических моделей, распознавание одного MP3 (с целью или без неё),
//! пакетная обработка с манифестом целей, отчёт и эталонная оценка.
//!
//! Обработка не обращается к сети; установка весов — отдельная явная
//! операция из локального снимка. Вход только читается, `decks/**` не
//! открывается, в основное хранилище ничего не публикуется, `Verified` и
//! `HumanAttestation` не создаются.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use crate::pronunciation::enhancer::{self, EnhancementProfile};
use crate::pronunciation::enhancer_backend as ebackend;
use crate::pronunciation::normalizer::{self, NormalizerProfile, Toolchain};
use crate::pronunciation::normalizer_batch as nbatch;
use crate::pronunciation::recognizer::{
    RecognitionProfile, TargetIdentity, TargetSource, TargetSourceKind,
};
use crate::pronunciation::recognizer_backend as rbackend;
use crate::pronunciation::recognizer_batch as rbatch;
use crate::pronunciation::recognizer_eval as reval;
use crate::pronunciation::recognizer_pipeline::{self as pipeline, DirectEmissions, EnhancerSetup};
use crate::pronunciation::recognizer_report as rreport;
use crate::pronunciation_normalizer_cli::{create_new_file, ok, read_json, stop_flag};
use crate::{AssetError, ErrorCode};

#[derive(clap::Args, Clone)]
pub(crate) struct ModelArgs {
    /// Исполняемый файл воркера распознавания (по умолчанию — рядом с программой или в PATH).
    #[arg(long)]
    recognizer_bin: Option<PathBuf>,
    /// Корень моделей (по умолчанию `$XDG_DATA_HOME/anki-deck-toolkit/speech-models`).
    #[arg(long)]
    models_root: Option<PathBuf>,
    /// Профиль распознавания JSON (по умолчанию — встроенный консервативный).
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Профиль Balanced JSON (по умолчанию — встроенный balanced).
    #[arg(long)]
    normalizer_profile: Option<PathBuf>,
    /// Улучшение речи для осведомлённой о цели обработки.
    #[arg(long, value_enum, default_value_t = EnhancerArg::Deepfilternet3)]
    enhancer: EnhancerArg,
    /// Исполняемый файл воркера улучшения речи.
    #[arg(long)]
    enhancer_bin: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnhancerArg {
    Deepfilternet3,
    None,
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub(crate) enum ModelArg {
    /// Основная фонемная модель HuBERT.
    HubertBasePhonemeCtcV4,
    /// Независимая модель второй гипотезы XLSR (хирагана).
    XlsrJapaneseHiraganaCtc,
}

impl From<ModelArg> for rbackend::ExpectedModel {
    fn from(arg: ModelArg) -> Self {
        match arg {
            ModelArg::HubertBasePhonemeCtcV4 => rbackend::PRIMARY_PHONEME,
            ModelArg::XlsrJapaneseHiraganaCtc => rbackend::VERIFIER_HIRAGANA,
        }
    }
}

#[derive(clap::Args, Clone)]
pub(crate) struct TargetArgs {
    /// Написание цели (вместе с --reading).
    #[arg(long, requires = "reading")]
    surface: Option<String>,
    /// Конкретное чтение цели каной.
    #[arg(long, requires = "surface")]
    reading: Option<String>,
    /// Явно известные другие чтения той же записи (повторяемый).
    #[arg(long = "competing-reading")]
    competing: Vec<String>,
}

#[derive(Subcommand)]
pub(crate) enum RecognizerAction {
    /// Печатает встроенный профиль распознавания.
    Profile,
    /// Явная установка весов модели из локального снимка репозитория без сети.
    ModelInstall {
        #[arg(long, value_enum)]
        model: ModelArg,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        models_root: Option<PathBuf>,
        #[arg(long)]
        recognizer_bin: Option<PathBuf>,
    },
    /// Проверяет воркер и веса моделей профиля и печатает идентичность.
    ModelIdentify {
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Распознавание одного MP3 без записи; `--out` дополнительно сохраняет
    /// итоговый кандидат, если он выбран (`use_final_candidate`).
    Analyze {
        #[arg(long)]
        file: PathBuf,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Фиксирует вход, необязательный манифест целей и конфигурацию, затем обрабатывает.
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        /// Манифест целей JSON (`docs/ru/asset-store/recognizer.md`).
        #[arg(long)]
        targets: Option<PathBuf>,
        #[command(flatten)]
        model: ModelArgs,
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
        #[arg(long)]
        recognizer_bin: Option<PathBuf>,
        #[arg(long)]
        enhancer_bin: Option<PathBuf>,
        #[arg(long)]
        models_root: Option<PathBuf>,
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
    /// Новый каталог отчёта: summary.json, items.jsonl, items.tsv, occurrences.tsv.
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
    /// Оценка на размеченном наборе: точность совпадений и отказов,
    /// погрешность границ и сравнение с прежним конвейером.
    Eval {
        /// Разметка набора JSON (`docs/ru/asset-store/recognizer.md`).
        #[arg(long)]
        labels: PathBuf,
        #[command(flatten)]
        model: ModelArgs,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
    },
}

fn models_root(explicit: Option<&Path>) -> Result<PathBuf, AssetError> {
    explicit
        .map(Path::to_path_buf)
        .or_else(ebackend::default_models_root)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::InvalidTransition,
                "не задан корень моделей: укажите --models-root",
            )
        })
}

pub(crate) fn load_profile(
    path: Option<&Path>,
    protected: &BTreeSet<PathBuf>,
) -> Result<RecognitionProfile, AssetError> {
    let profile = match path {
        Some(path) => read_json(path, protected)?,
        None => RecognitionProfile::conservative(),
    };
    profile.validate()?;
    Ok(profile)
}

/// Проверенные модели, профиль, Balanced и (необязательно) улучшение речи.
pub(crate) struct Setup {
    pub profile: RecognitionProfile,
    pub primary: rbackend::ModelHandle,
    pub verifier: Option<rbackend::ModelHandle>,
    pub base_profile: NormalizerProfile,
    pub toolchain: Toolchain,
    pub enhancer: Option<EnhancerParts>,
    pub config_sha256: String,
}

pub(crate) struct EnhancerParts {
    pub profile: EnhancementProfile,
    pub identity: ebackend::BackendIdentity,
    pub worker: PathBuf,
    pub model_dir: PathBuf,
    pub config_sha256: String,
}

pub(crate) fn handle_for(
    worker: &Path,
    root: &Path,
    id: &str,
    threads: u32,
) -> Result<rbackend::ModelHandle, AssetError> {
    let expected = rbackend::expected(id).ok_or_else(|| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            format!("модель {id} не закреплена"),
        )
    })?;
    rbackend::identify(
        worker,
        &rbackend::installed_model_dir(root, expected),
        expected,
        threads,
    )
}

pub(crate) fn setup(model: &ModelArgs, protected: &BTreeSet<PathBuf>) -> Result<Setup, AssetError> {
    let profile = load_profile(model.profile.as_deref(), protected)?;
    let base_profile: NormalizerProfile = match &model.normalizer_profile {
        Some(path) => read_json(path, protected)?,
        None => NormalizerProfile::balanced(),
    };
    base_profile.validate()?;
    setup_from(
        profile,
        base_profile,
        model.enhancer == EnhancerArg::Deepfilternet3,
        model.recognizer_bin.as_deref(),
        model.enhancer_bin.as_deref(),
        model.models_root.as_deref(),
    )
}

pub(crate) fn setup_from(
    profile: RecognitionProfile,
    base_profile: NormalizerProfile,
    with_enhancer: bool,
    recognizer_bin: Option<&Path>,
    enhancer_bin: Option<&Path>,
    models_root_arg: Option<&Path>,
) -> Result<Setup, AssetError> {
    let root = models_root(models_root_arg)?;
    let worker = rbackend::locate_worker(recognizer_bin)?;
    let threads = profile.models.threads;
    let primary = handle_for(&worker, &root, &profile.models.primary, threads)?;
    let verifier = match &profile.models.verifier {
        Some(id) => Some(handle_for(&worker, &root, id, threads)?),
        None => None,
    };
    let toolchain = Toolchain::detect()?;
    let base_config =
        normalizer::config_sha256(&base_profile, normalizer::Mode::Normalize, &toolchain);
    let enhancer = if with_enhancer {
        let eprofile = EnhancementProfile::conservative();
        let kind = eprofile.backend.kind;
        let eworker = ebackend::locate_worker(enhancer_bin, kind)?;
        let dir = ebackend::installed_model_dir(&root, kind);
        let identity = ebackend::identify(&eworker, &dir, kind, &eprofile.backend.inference)?;
        let config = enhancer::config_sha256(&eprofile, &identity, &base_config);
        Some(EnhancerParts {
            profile: eprofile,
            identity,
            worker: eworker,
            model_dir: dir,
            config_sha256: config,
        })
    } else {
        None
    };
    let config_sha256 = pipeline::config_sha256(
        &profile,
        &primary.identity,
        verifier.as_ref().map(|v| &v.identity),
        &base_config,
        enhancer.as_ref().map(|e| e.config_sha256.as_str()),
    );
    Ok(Setup {
        profile,
        primary,
        verifier,
        base_profile,
        toolchain,
        enhancer,
        config_sha256,
    })
}

impl Setup {
    pub(crate) fn context<'a>(
        &'a self,
        source: &'a dyn pipeline::EmissionSource,
    ) -> pipeline::Context<'a> {
        pipeline::Context {
            profile: &self.profile,
            primary: &self.primary,
            verifier: self.verifier.as_ref(),
            base_profile: &self.base_profile,
            base_toolchain: &self.toolchain,
            enhancer: self.enhancer.as_ref().map(|e| EnhancerSetup {
                profile: &e.profile,
                backend: &e.identity,
                worker: &e.worker,
                model_dir: &e.model_dir,
                config_sha256: &e.config_sha256,
            }),
            config_sha256: &self.config_sha256,
            source,
        }
    }

    pub(crate) fn direct(&self) -> DirectEmissions {
        DirectEmissions {
            timeout: std::time::Duration::from_secs(self.profile.models.timeout_seconds),
            call_items: self.profile.models.call_items,
        }
    }
}

fn request_target(args: &TargetArgs, file_sha: &str) -> Result<Option<TargetIdentity>, AssetError> {
    let (Some(surface), Some(reading)) = (&args.surface, &args.reading) else {
        return Ok(None);
    };
    let target = TargetIdentity {
        surface: surface.clone(),
        reading: reading.clone(),
        source: TargetSource {
            kind: TargetSourceKind::Request,
            key: format!("request:{file_sha}"),
        },
        competing_readings: args.competing.clone(),
    };
    target.validate()?;
    Ok(Some(target))
}

pub(crate) fn execute(
    action: &RecognizerAction,
    protected: &BTreeSet<PathBuf>,
) -> Result<serde_json::Value, AssetError> {
    match action {
        RecognizerAction::Profile => Ok(ok(serde_json::json!(RecognitionProfile::conservative()))),
        RecognizerAction::ModelInstall {
            model,
            source,
            models_root: root,
            recognizer_bin,
        } => {
            let worker = rbackend::locate_worker(recognizer_bin.as_deref())?;
            let root = models_root(root.as_deref())?;
            for path in [root.as_path(), source.as_path()] {
                nbatch::ensure_outside(
                    path,
                    &nbatch::protected_roots(protected),
                    "веса не читаются из decks/** и не пишутся туда",
                )?;
            }
            Ok(ok(rbackend::install_model(
                &worker,
                (*model).into(),
                source,
                &root,
            )?))
        }
        RecognizerAction::ModelIdentify { model } => {
            let setup = setup(model, protected)?;
            Ok(ok(serde_json::json!({
                "primary": setup.primary.identity,
                "verifier": setup.verifier.as_ref().map(|v| &v.identity),
                "enhancer": setup.enhancer.as_ref().map(|e| &e.identity),
                "config_sha256": setup.config_sha256,
            })))
        }
        RecognizerAction::Analyze {
            file,
            target,
            model,
            out,
        } => {
            let output = match out {
                Some(out) => {
                    let source =
                        std::fs::canonicalize(file).map_err(|e| AssetError::io("исходник", e))?;
                    let mut forbidden = nbatch::protected_roots(protected);
                    forbidden.push(source.parent().unwrap_or(Path::new("/")).to_path_buf());
                    Some(nbatch::ensure_outside(
                        out,
                        &forbidden,
                        "итоговый кандидат пишется только вне каталога исходника и decks/**",
                    )?)
                }
                None => None,
            };
            let bytes = crate::pronunciation_cli::read_local(
                file,
                protected,
                crate::pronunciation::MAX_AUDIO_BYTES,
            )?;
            let target = request_target(target, &crate::hashing::sha256_hex(&bytes))?;
            let setup = setup(model, protected)?;
            let direct = setup.direct();
            let ctx = setup.context(&direct);
            let recognized = pipeline::recognize_one(&bytes, target.as_ref(), &ctx)?;
            let written = match (&output, &recognized.output_bytes) {
                (Some(path), Some(bytes)) => {
                    create_new_file(path, bytes)?;
                    true
                }
                _ => false,
            };
            Ok(ok(serde_json::json!({
                "written": written,
                "result": recognized.result,
            })))
        }
        RecognizerAction::BatchStart {
            batch_id,
            input,
            workspace,
            targets,
            model,
            jobs,
            max_items,
        } => {
            let profile = load_profile(model.profile.as_deref(), protected)?;
            let base_profile: NormalizerProfile = match &model.normalizer_profile {
                Some(path) => read_json(path, protected)?,
                None => NormalizerProfile::balanced(),
            };
            let input = nbatch::canonical_directory(input)?;
            let workspace = nbatch::create_workspace(workspace, &input, protected)?;
            let manifest = match targets {
                Some(path) => Some(rbatch::read_manifest(path, protected)?),
                None => None,
            };
            let started = rbatch::start(rbatch::StartRequest {
                batch_id,
                workspace: &workspace,
                input_root: &input,
                manifest,
                profile,
                base_profile,
                with_enhancer: model.enhancer == EnhancerArg::Deepfilternet3,
                jobs: *jobs,
                recognizer_bin: model.recognizer_bin.as_deref(),
                enhancer_bin: model.enhancer_bin.as_deref(),
                models_root: model.models_root.as_deref(),
            })?;
            Ok(run_json(rbatch::resume(
                &workspace,
                &started.batch_id,
                rbatch::ResumeOptions {
                    jobs: None,
                    max_items: *max_items,
                    recognizer_bin: model.recognizer_bin.clone(),
                    enhancer_bin: model.enhancer_bin.clone(),
                    models_root: model.models_root.clone(),
                },
                stop_flag(),
            )?))
        }
        RecognizerAction::BatchResume {
            batch_id,
            workspace,
            jobs,
            max_items,
            recognizer_bin,
            enhancer_bin,
            models_root,
        } => {
            let workspace = open_batch(workspace, batch_id, protected)?;
            Ok(run_json(rbatch::resume(
                &workspace,
                batch_id,
                rbatch::ResumeOptions {
                    jobs: *jobs,
                    max_items: *max_items,
                    recognizer_bin: recognizer_bin.clone(),
                    enhancer_bin: enhancer_bin.clone(),
                    models_root: models_root.clone(),
                },
                stop_flag(),
            )?))
        }
        RecognizerAction::BatchStatus {
            batch_id,
            workspace,
        } => {
            let workspace = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(rbatch::load(&workspace, batch_id)?.0)))
        }
        RecognizerAction::BatchCancel {
            batch_id,
            workspace,
        } => {
            let workspace = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(rbatch::cancel(&workspace, batch_id)?)))
        }
        RecognizerAction::BatchReport {
            batch_id,
            workspace,
            out,
        } => {
            let workspace = open_batch(workspace, batch_id, protected)?;
            let (batch, inventory, manifest) = rbatch::load(&workspace, batch_id)?;
            let mut forbidden = nbatch::protected_roots(protected);
            forbidden.push(PathBuf::from(&batch.input_root));
            forbidden.push(workspace.join(".runtime"));
            let out = nbatch::ensure_outside(
                out,
                &forbidden,
                "отчёт пишется вне входа, служебной области и decks/**",
            )?;
            let cache = rbatch::ResultCache::open(&workspace)?;
            let results = rbatch::collect_results(&cache, &batch, &inventory, manifest.as_ref())?;
            Ok(ok(serde_json::json!(rreport::write_report(
                &batch, &inventory, &results, &out
            )?)))
        }
        RecognizerAction::BatchVerifySources {
            batch_id,
            workspace,
        } => {
            let workspace = open_batch(workspace, batch_id, protected)?;
            let (batch, inventory, _) = rbatch::load(&workspace, batch_id)?;
            let check =
                nbatch::verify_inventory(Path::new(&batch.input_root), &batch.limits, &inventory)?;
            if !check.verified {
                return Err(AssetError::with_details(
                    ErrorCode::IntegrityMismatch,
                    "вход изменился после инвентаризации",
                    serde_json::json!(check),
                ));
            }
            Ok(ok(serde_json::json!(check)))
        }
        RecognizerAction::Eval {
            labels,
            model,
            out,
            jobs,
        } => {
            let setup = setup(model, protected)?;
            let mut forbidden = nbatch::protected_roots(protected);
            let labels_path =
                std::fs::canonicalize(labels).map_err(|e| AssetError::io("разметка", e))?;
            forbidden.push(labels_path.parent().unwrap_or(Path::new("/")).to_path_buf());
            let out = nbatch::ensure_outside(
                out,
                &forbidden,
                "отчёт оценки пишется вне каталога разметки и decks/**",
            )?;
            Ok(ok(serde_json::json!(reval::evaluate(
                &setup,
                &labels_path,
                protected,
                &out,
                *jobs
            )?)))
        }
    }
}

fn open_batch(
    workspace: &Path,
    batch_id: &str,
    protected: &BTreeSet<PathBuf>,
) -> Result<PathBuf, AssetError> {
    let workspace = nbatch::existing_workspace(workspace, protected)?;
    let (batch, _, _) = rbatch::load(&workspace, batch_id)?;
    nbatch::check_boundaries(Path::new(&batch.input_root), &workspace, protected)?;
    Ok(workspace)
}

fn run_json(summary: rbatch::RunSummary) -> serde_json::Value {
    let interrupted = summary.outcome == "interrupted";
    let cancelled = summary.outcome == "cancelled";
    serde_json::json!({
        "schema_version": 1,
        "outcome": if interrupted { "paused" } else if cancelled { "cancelled" } else { "ok" },
        "result": {
            "run": summary,
            "paused": if interrupted { serde_json::json!({"code":"interrupted"}) } else { serde_json::Value::Null },
        },
    })
}
