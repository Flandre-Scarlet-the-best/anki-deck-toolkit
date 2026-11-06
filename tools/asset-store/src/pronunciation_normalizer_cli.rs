//! Подкоманды `pronunciation-assets normalizer`: анализ, пробная и пакетная
//! нормализация, отчёты, необязательная выборка для сравнения и явная публикация.
//!
//! Ни одна подкоманда не обращается к Forvo, браузеру или сети. Каталог входа
//! только читается; всё вывод пишется в явно заданные пользователем места.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Subcommand, ValueEnum};

use crate::pronunciation::normalizer::{self, Mode, NormalizerProfile, Toolchain};
use crate::pronunciation::normalizer_batch::{self as nbatch, Limits, ResultCache};
use crate::pronunciation::normalizer_report::{self as report, Decisions, Include, SampleSet};
use crate::pronunciation::{self as audio, Pronunciation, PronunciationDomainPolicy, derivation};
use crate::{AssetError, AssetStore, ErrorCode, StoreOptions};

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ModeArg {
    Analyze,
    Normalize,
}

impl From<ModeArg> for Mode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Analyze => Mode::Analyze,
            ModeArg::Normalize => Mode::Normalize,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum IncludeArg {
    All,
    Problems,
    Sample,
}

#[derive(Subcommand)]
pub(crate) enum NormalizerAction {
    /// Печатает профиль Balanced по умолчанию (основа для собственного профиля).
    Profile,
    /// Анализ одного MP3 без записи; `--mode normalize` дополнительно кодирует
    /// во временный каталог и выполняет пост-QC, но вариант не сохраняет.
    Analyze {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "analyze")]
        mode: ModeArg,
    },
    /// Пробная нормализация одного MP3: вариант записывается в новый файл
    /// вне каталога исходника, только если QualityGate его предлагает.
    Trial {
        #[arg(long)]
        file: PathBuf,
        /// Новый MP3 вне каталога исходника.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        profile: Option<PathBuf>,
    },
    /// Инвентаризация каталога и фиксация конфигурации, затем обработка.
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        input_dir: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, value_enum, default_value = "normalize")]
        mode: ModeArg,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        /// Ограничивает число новых обработок этой командой (0 — только инвентаризация).
        #[arg(long)]
        max_items: Option<usize>,
        #[arg(long, default_value_t = nbatch::DEFAULT_MAX_FILES)]
        max_files: usize,
        #[arg(long, default_value_t = nbatch::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
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
    /// Сводка, JSONL и TSV по всем результатам; `--html` — офлайн сравнение до/после.
    BatchReport {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        /// Новый каталог отчёта (не должен существовать).
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        html: bool,
        #[arg(long, value_enum, default_value = "problems")]
        include: IncludeArg,
        #[arg(long)]
        sample: Option<PathBuf>,
        /// Не больше стольких записей HTML получают копии MP3.
        #[arg(long, default_value_t = 400)]
        media_limit: usize,
    },
    /// Воспроизводимая стратифицированная выборка для необязательного сравнения.
    BatchSample {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        seed: u64,
        #[arg(long, default_value_t = 5)]
        per_stratum: usize,
        /// Новый файл выборки JSON.
        #[arg(long)]
        out: PathBuf,
    },
    /// Сопоставляет ручные решения (экспорт HTML) с метриками и исходами.
    BatchReviewSummary {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        decisions: PathBuf,
        #[arg(long)]
        sample: Option<PathBuf>,
    },
    /// Повторно хеширует вход: корпус не изменён после обработки.
    BatchVerifySources {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
    },
    /// Явно публикует обработанный вариант вместо текущей записи пары (CAS).
    Publish {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        input_sha256: String,
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        replace_expected_sha256: String,
        #[arg(long)]
        allow_replace_verified: bool,
    },
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<T, AssetError> {
    let bytes = crate::pronunciation_cli::read_local(path, protected, 16 * 1024 * 1024)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AssetError::new(ErrorCode::InvalidIdentity, format!("JSON: {e}")))
}

fn load_profile(
    path: Option<&Path>,
    protected: &BTreeSet<PathBuf>,
) -> Result<NormalizerProfile, AssetError> {
    let profile = match path {
        Some(path) => read_json(path, protected)?,
        None => NormalizerProfile::balanced(),
    };
    profile.validate()?;
    Ok(profile)
}

/// Рабочий каталог существующего пакета: фактический путь вне защищённых
/// корней, а вход пакета не пересекается ни с ним, ни с `decks/**`.
fn open_batch(
    workspace: &Path,
    batch_id: &str,
    protected: &BTreeSet<PathBuf>,
) -> Result<(PathBuf, nbatch::NormalizerBatch, nbatch::Inventory), AssetError> {
    let workspace = nbatch::existing_workspace(workspace, protected)?;
    let (batch, inventory) = nbatch::load(&workspace, batch_id)?;
    nbatch::check_boundaries(Path::new(&batch.input_root), &workspace, protected)?;
    Ok((workspace, batch, inventory))
}

/// Запрещённые для вывода деревья пакета: вход, служебная область рабочего
/// каталога и защищённые корни — все в фактической форме.
fn forbidden_outputs(
    workspace: &Path,
    batch: &nbatch::NormalizerBatch,
    protected: &BTreeSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = nbatch::protected_roots(protected);
    roots.push(PathBuf::from(&batch.input_root));
    roots.push(workspace.join(".runtime"));
    roots
}

pub(crate) fn create_new_file(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    use std::io::Write;
    let mut file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| AssetError::io("создание файла вывода (файл не должен существовать)", e))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| AssetError::io("запись файла вывода", e))
}

/// Флаг остановки по SIGINT/SIGTERM. Регистрация заменяет стандартное
/// завершение процесса: пакет сохраняет контрольные точки и выходит сам.
pub(crate) fn stop_flag() -> &'static AtomicBool {
    static STOP: AtomicBool = AtomicBool::new(false);
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let (ready, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use tokio::signal::unix::{SignalKind, signal};
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    let _ = ready.send(());
                    return;
                }
            };
            runtime.block_on(async move {
                let (Ok(mut interrupt), Ok(mut terminate)) = (
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::terminate()),
                ) else {
                    let _ = ready.send(());
                    return;
                };
                let _ = ready.send(());
                tokio::select! {
                    _ = interrupt.recv() => {},
                    _ = terminate.recv() => {},
                }
                STOP.store(true, Ordering::SeqCst);
            });
        });
        let _ = wait.recv();
    });
    &STOP
}

fn run_json(summary: nbatch::RunSummary) -> serde_json::Value {
    let interrupted = summary.outcome == "interrupted";
    let cancelled = summary.outcome == "cancelled";
    let value = serde_json::json!({
        "run": {
            "outcome": summary.outcome,
            "processed_now": summary.processed_now,
            "reused": summary.reused,
            "remaining": summary.remaining,
            "infrastructure_failed_now": summary.infrastructure_failed_now,
        },
        "batch": summary.batch,
        "paused": if interrupted { serde_json::json!({"code":"interrupted"}) } else { serde_json::Value::Null },
    });
    serde_json::json!({
        "schema_version": 1,
        "outcome": if interrupted { "paused" } else if cancelled { "cancelled" } else { "ok" },
        "result": value,
    })
}

pub(crate) fn ok(result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"schema_version":1,"outcome":"ok","result":result})
}

pub(crate) fn execute(
    action: &NormalizerAction,
    store_root: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<serde_json::Value, AssetError> {
    match action {
        NormalizerAction::Profile => Ok(ok(serde_json::json!(NormalizerProfile::balanced()))),
        NormalizerAction::Analyze {
            file,
            profile,
            mode,
        } => {
            let profile = load_profile(profile.as_deref(), protected)?;
            let bytes =
                crate::pronunciation_cli::read_local(file, protected, audio::MAX_AUDIO_BYTES)?;
            let processed =
                normalizer::process(&bytes, &profile, (*mode).into(), &Toolchain::detect()?)?;
            Ok(ok(serde_json::json!(processed.result)))
        }
        NormalizerAction::Trial { file, out, profile } => {
            let output = out;
            let profile = load_profile(profile.as_deref(), protected)?;
            let source = std::fs::canonicalize(file).map_err(|e| AssetError::io("исходник", e))?;
            let mut forbidden = nbatch::protected_roots(protected);
            forbidden.push(source.parent().unwrap_or(Path::new("/")).to_path_buf());
            let output = nbatch::ensure_outside(
                output,
                &forbidden,
                "вариант пишется только в отдельный каталог вне каталога исходника и decks/**",
            )?;
            let bytes =
                crate::pronunciation_cli::read_local(file, protected, audio::MAX_AUDIO_BYTES)?;
            let processed =
                normalizer::process(&bytes, &profile, Mode::Normalize, &Toolchain::detect()?)?;
            let written = match (
                &processed.output_bytes,
                processed.result.gate.proposed_variant,
            ) {
                (Some(bytes), Some(normalizer::Variant::Processed)) => {
                    create_new_file(&output, bytes)?;
                    true
                }
                _ => false,
            };
            Ok(ok(
                serde_json::json!({"written": written, "result": processed.result}),
            ))
        }
        NormalizerAction::BatchStart {
            batch_id,
            input_dir,
            workspace,
            mode,
            profile,
            jobs,
            max_items,
            max_files,
            max_total_bytes,
        } => {
            let profile = load_profile(profile.as_deref(), protected)?;
            let input = nbatch::canonical_directory(input_dir)?;
            // Границы проверяются по фактическим путям до первой записи:
            // отказ не оставляет следов ни во входе, ни в decks/**.
            nbatch::check_boundaries(&input, workspace, protected)?;
            let workspace = nbatch::create_workspace(workspace, &input, protected)?;
            let started = nbatch::start(nbatch::StartRequest {
                batch_id,
                workspace: &workspace,
                input_root: &input,
                mode: (*mode).into(),
                profile,
                limits: Limits {
                    jobs: *jobs,
                    max_files: *max_files,
                    max_total_bytes: *max_total_bytes,
                },
            })?;
            Ok(run_json(nbatch::resume_started(
                &workspace,
                &started,
                *max_items,
                stop_flag(),
            )?))
        }
        NormalizerAction::BatchResume {
            batch_id,
            workspace,
            jobs,
            max_items,
        } => {
            let (workspace, _, _) = open_batch(workspace, batch_id, protected)?;
            Ok(run_json(nbatch::resume(
                &workspace,
                batch_id,
                *jobs,
                *max_items,
                stop_flag(),
            )?))
        }
        NormalizerAction::BatchStatus {
            batch_id,
            workspace,
        } => {
            let (_, batch, _) = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(batch)))
        }
        NormalizerAction::BatchCancel {
            batch_id,
            workspace,
        } => {
            let (workspace, _, _) = open_batch(workspace, batch_id, protected)?;
            Ok(ok(serde_json::json!(nbatch::cancel(&workspace, batch_id)?)))
        }
        NormalizerAction::BatchReport {
            batch_id,
            workspace,
            out,
            html,
            include,
            sample,
            media_limit,
        } => {
            let (workspace, batch, inventory) = open_batch(workspace, batch_id, protected)?;
            let out = nbatch::ensure_outside(
                out,
                &forbidden_outputs(&workspace, &batch, protected),
                "каталог отчёта не может находиться во входе, в .runtime или в decks/**",
            )?;
            let sample: Option<SampleSet> = sample
                .as_deref()
                .map(|path| read_json(path, protected))
                .transpose()?;
            let include = match (include, &sample) {
                (IncludeArg::Sample, Some(sample)) => Include::Sample(sample),
                (IncludeArg::Sample, None) => {
                    return Err(AssetError::new(
                        ErrorCode::InvalidIdentity,
                        "--include sample требует --sample",
                    ));
                }
                (IncludeArg::All, _) => Include::All,
                (IncludeArg::Problems, _) => Include::Problems,
            };
            let cache = ResultCache::open(&workspace)?;
            let results = nbatch::collect_results(&cache, &batch, &inventory)?;
            let outcome = report::write_report(
                &batch,
                &inventory,
                &cache,
                &results,
                report::ReportRequest {
                    out_dir: &out,
                    html: *html,
                    include,
                    media_limit: *media_limit,
                },
            )?;
            Ok(ok(serde_json::json!({
                "report": outcome,
                "summary": report::summarize(&batch, &inventory, &results),
            })))
        }
        NormalizerAction::BatchSample {
            batch_id,
            workspace,
            seed,
            per_stratum,
            out,
        } => {
            let (workspace, batch, inventory) = open_batch(workspace, batch_id, protected)?;
            let out = nbatch::ensure_outside(
                out,
                &forbidden_outputs(&workspace, &batch, protected),
                "файл выборки не может находиться во входе, в .runtime или в decks/**",
            )?;
            let cache = ResultCache::open(&workspace)?;
            let results = nbatch::collect_results(&cache, &batch, &inventory)?;
            // Выборка по неполному пакету не репрезентативна для инвентаризации.
            let expected = inventory.unique_inputs().len();
            if results.len() < expected {
                return Err(AssetError::with_details(
                    ErrorCode::InvalidTransition,
                    "пакет обработан не полностью: выборка по части результатов не репрезентативна; завершите batch-resume",
                    serde_json::json!({
                        "blocker": "batch_incomplete",
                        "unique_inputs": expected,
                        "results": results.len(),
                        "missing_results": expected - results.len(),
                    }),
                ));
            }
            let sample = report::sample(&batch, &inventory, &results, *seed, *per_stratum);
            create_new_file(
                &out,
                &serde_json::to_vec_pretty(&sample).expect("выборка сериализуема"),
            )?;
            Ok(ok(serde_json::json!({
                "items": sample.items.len(),
                "strata": sample.strata.iter().map(|s| serde_json::json!({"id": s.id, "available": s.available, "selected": s.selected.len()})).collect::<Vec<_>>(),
            })))
        }
        NormalizerAction::BatchReviewSummary {
            batch_id,
            workspace,
            decisions,
            sample,
        } => {
            let (workspace, batch, inventory) = open_batch(workspace, batch_id, protected)?;
            let decisions: Decisions = read_json(decisions, protected)?;
            let sample: Option<SampleSet> = sample
                .as_deref()
                .map(|path| read_json(path, protected))
                .transpose()?;
            let cache = ResultCache::open(&workspace)?;
            let results = nbatch::collect_results(&cache, &batch, &inventory)?;
            Ok(ok(serde_json::json!(report::review_summary(
                &batch,
                &results,
                &decisions,
                sample.as_ref()
            )?)))
        }
        NormalizerAction::BatchVerifySources {
            batch_id,
            workspace,
        } => {
            let (_, batch, inventory) = open_batch(workspace, batch_id, protected)?;
            let check = nbatch::verify_sources(&batch, &inventory)?;
            if !check.verified {
                return Err(AssetError::with_details(
                    ErrorCode::IntegrityMismatch,
                    "неизменность входа не доказана: вход изменился или часть записей непроверяема",
                    serde_json::json!(check),
                ));
            }
            Ok(ok(serde_json::json!(check)))
        }
        NormalizerAction::Publish {
            batch_id,
            workspace,
            input_sha256,
            surface,
            reading,
            replace_expected_sha256,
            allow_replace_verified,
        } => {
            let (workspace, batch, inventory) = open_batch(workspace, batch_id, protected)?;
            let request = Pronunciation::new(surface, reading)?;
            let cache = ResultCache::open(&workspace)?;
            let result = cache
                .get(input_sha256, &batch.config_sha256)?
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::InvalidTransition,
                        "для входа нет результата пакета",
                    )
                })?;
            let output = result.output.as_ref().ok_or_else(|| {
                AssetError::new(
                    ErrorCode::InvalidTransition,
                    "у результата нет обработанного варианта",
                )
            })?;
            let output_bytes = cache.variant_bytes(output)?;
            let path = inventory
                .entries
                .iter()
                .find(|e| e.sha256.as_deref() == Some(input_sha256.as_str()))
                .map(|e| e.path.clone())
                .ok_or_else(|| AssetError::new(ErrorCode::InvalidTransition, "вход не найден"))?;
            let source_bytes =
                nbatch::read_input(&nbatch::open_input_root(&batch)?, &path, input_sha256)?;
            let options = protected
                .iter()
                .fold(StoreOptions::new(store_root), |o, p| o.protect_from(p));
            let store = AssetStore::open_existing_with_policy(options, PronunciationDomainPolicy)?;
            // Исходные байты остаются в рабочем каталоге до записи в хранилище.
            let retained = cache.retain_original(&source_bytes)?;
            let published = derivation::publish_with_outcome(derivation::PublishRequest {
                store: &store,
                request: &request,
                result: &result,
                profile: &batch.profile,
                output_bytes: &output_bytes,
                source_bytes: &source_bytes,
                replace_expected_sha256,
                allow_replace_verified: *allow_replace_verified,
            })?;
            let record = published.record;
            // Источник истины — запись хранилища и его журнал; файл
            // publications/ — вспомогательная выписка для рабочего каталога.
            let publication = serde_json::json!({
                "schema_version": 1,
                "batch_id": batch.batch_id,
                "config_sha256": batch.config_sha256,
                "identity": request.identity(),
                "source_sha256": input_sha256,
                "retained_source": retained,
                "published_sha256": record.sha256,
            });
            if let Err(error) = cache.write_record(
                "publications",
                &format!("{input_sha256}.{}.json", record.sha256),
                &serde_json::to_vec_pretty(&publication).expect("запись публикации"),
            ) {
                return Err(AssetError::with_details(
                    error.code,
                    "вариант опубликован в хранилище, но выписка publications/ не сохранена: повторите publish с теми же аргументами — повтор не заменит запись второй раз",
                    serde_json::json!({
                        "blocker": "publication_record_failed",
                        "store_committed": true,
                        "published_sha256": record.sha256,
                        "cause": error.message,
                    }),
                ));
            }
            Ok(ok(serde_json::json!({
                "asset": record,
                "already_published": published.already_published,
                "publication": publication,
            })))
        }
    }
}
