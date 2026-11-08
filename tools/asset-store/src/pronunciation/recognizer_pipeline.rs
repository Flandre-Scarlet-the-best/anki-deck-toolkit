//! Конвейер распознавания одной записи для одной цели: строгая проверка MP3 →
//! исходный PCM → свободный предварительный анализ → сопоставление с целью →
//! осведомлённая о цели обработка (Balanced, улучшение речи с контекстом,
//! одно кодирование) → повторное распознавание фактически декодированного
//! результата → машинное решение.
//!
//! Этап 1 ([`stage_one`]) заканчивается байтами итогового кандидата (или
//! решением без него), этап 2 ([`stage_two`]) — повторной проверкой и
//! результатом. Пакет вызывает этапы раздельно, чтобы группировать вызовы
//! моделей; одиночный анализ — [`recognize_one`].
//!
//! Прежние гарантии не ослабляются: Balanced post-QC, Enhancement QC (13
//! проверок, поканально) и MP3-валидатор v2 обязательны для сохраняемого
//! материала. Новое — только то, что именно считается сохраняемым материалом:
//! при извлечении это фрагмент с целевым словом и защитными полями, а
//! нецелевые чтения, которые выбрасываются, проверку не проходят и не
//! проваливают.

use serde::{Deserialize, Serialize};

use super::AudioProbe;
use super::ctc::Emissions;
use super::dsp::{self, Pcm};
use super::enhancer::{
    self, EnhancementProfile, EnhancementQc, InferenceRecord, ItemContext, Triage,
};
use super::enhancer_backend::{self as ebackend, BackendIdentity};
use super::normalizer::{
    self, Check, InputRef, NormalizationResult, NormalizerProfile, OutputRef, ProfileRef, Reason,
    ReasonOrigin, Severity, Toolchain, Variant,
};
use super::probe;
use super::recognizer::{
    self, ExtractionPlan, Hypothesis, Level, Matching, ModelView, Occurrence, PreAnalysis,
    RecognitionAction, RecognitionOutcome, RecognitionProfile, SampleSpan, Selection,
    TargetIdentity, Timeline, Uncertainty, recognition_reason,
};
use super::recognizer_backend::{self as rbackend, ModelHandle, ModelIdentity};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

/// Источник эмиссий: прямой вызов воркера или кэш пакета поверх него.
pub trait EmissionSource: Sync {
    fn emissions(
        &self,
        model: &ModelHandle,
        inputs: &[&[f32]],
    ) -> Result<Vec<Emissions>, AssetError>;
}

/// Прямые вызовы воркера группами по `call_items`.
pub struct DirectEmissions {
    pub timeout: std::time::Duration,
    pub call_items: usize,
}

impl EmissionSource for DirectEmissions {
    fn emissions(
        &self,
        model: &ModelHandle,
        inputs: &[&[f32]],
    ) -> Result<Vec<Emissions>, AssetError> {
        let mut out = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(self.call_items.max(1)) {
            out.extend(rbackend::emissions(model, chunk, self.timeout)?.0);
        }
        Ok(out)
    }
}

pub struct EnhancerSetup<'a> {
    pub profile: &'a EnhancementProfile,
    pub backend: &'a BackendIdentity,
    pub worker: &'a std::path::Path,
    pub model_dir: &'a std::path::Path,
    pub config_sha256: &'a str,
}

pub struct Context<'a> {
    pub profile: &'a RecognitionProfile,
    pub primary: &'a ModelHandle,
    pub verifier: Option<&'a ModelHandle>,
    pub base_profile: &'a NormalizerProfile,
    pub base_toolchain: &'a Toolchain,
    pub enhancer: Option<EnhancerSetup<'a>>,
    pub config_sha256: &'a str,
    pub source: &'a dyn EmissionSource,
}

impl Context<'_> {
    fn enhancer_item(&self) -> Option<ItemContext<'_>> {
        self.enhancer.as_ref().map(|e| ItemContext {
            profile: e.profile,
            backend: e.backend,
            worker: e.worker,
            model_dir: e.model_dir,
            base_profile: self.base_profile,
            base_toolchain: self.base_toolchain,
            config_sha256: e.config_sha256,
        })
    }
}

/// Отпечаток всей конфигурации, от которой зависит решение пары SHA × цель
/// (сама цель входит в ключ кэша отдельно).
pub fn config_sha256(
    profile: &RecognitionProfile,
    primary: &ModelIdentity,
    verifier: Option<&ModelIdentity>,
    base_config: &str,
    enhancer_config: Option<&str>,
) -> String {
    let value = serde_json::json!({
        "schema_version": recognizer::RESULT_SCHEMA_VERSION,
        "algorithm": recognizer::ALGORITHM_VERSION,
        "profile": profile,
        "primary": primary.fingerprint(),
        "verifier": verifier.map(ModelIdentity::fingerprint),
        "balanced": base_config,
        "enhancer": enhancer_config,
    });
    sha256_hex(serde_json::to_vec(&value).expect("конфигурация сериализуема"))
}

// ---------------------------------------------------------------------------
// Декодирование и шкала анализа
// ---------------------------------------------------------------------------

/// Проверенная запись и её шкала анализа.
pub struct Decoded {
    pub probe: AudioProbe,
    pub pcm: Pcm,
    /// Моно 16 kHz для моделей.
    pub analysis: Vec<f32>,
    pub timeline: Timeline,
    /// Уровни окон 10 мс моно-сведения исходника, dBFS.
    pub levels: Vec<f64>,
    pub window_frames: u64,
    pub signal: normalizer::SignalAnalysis,
}

fn mono(pcm: &Pcm) -> Pcm {
    if pcm.channels == 1 {
        return pcm.clone();
    }
    Pcm {
        sample_rate: pcm.sample_rate,
        channels: 1,
        samples: pcm
            .samples
            .chunks_exact(pcm.channels)
            .map(|f| (f.iter().map(|s| f64::from(*s)).sum::<f64>() / f.len() as f64) as f32)
            .collect(),
    }
}

/// Отказ декодирования как свойство файла (`Ok(Err)`) отличается от сбоя
/// среды (`Err`).
pub fn decode(
    bytes: &[u8],
    base_profile: &NormalizerProfile,
) -> Result<Result<Decoded, String>, AssetError> {
    let probe = match probe::probe_mp3(bytes) {
        Ok(probe) => probe,
        Err(error) if normalizer::is_infrastructure_failure(&error) => return Err(error),
        Err(error) => return Ok(Err(error.message)),
    };
    let samples = match probe::decode_f32(bytes, &probe) {
        Ok(samples) => samples,
        Err(error) if normalizer::is_infrastructure_failure(&error) => return Err(error),
        Err(error) => return Ok(Err(error.message)),
    };
    let pcm = Pcm {
        sample_rate: probe.sample_rate,
        channels: probe.channels as usize,
        samples,
    };
    Ok(Ok(decoded_from_pcm(
        sha256_hex(bytes),
        probe,
        pcm,
        base_profile,
    )?))
}

/// Шкала анализа для уже декодированного PCM.
pub fn decoded_from_pcm(
    sha: String,
    probe: AudioProbe,
    pcm: Pcm,
    base_profile: &NormalizerProfile,
) -> Result<Decoded, AssetError> {
    let mono = mono(&pcm);
    let analysis = if mono.sample_rate == rbackend::SAMPLE_RATE {
        mono.samples.clone()
    } else {
        probe::resample_f32(&mono.samples, mono.sample_rate, 1, rbackend::SAMPLE_RATE)?
    };
    let window = (pcm.sample_rate / 100).max(1) as usize;
    let levels = dsp::frame_levels(&mono, window);
    let signal = normalizer::analyze_pcm(&pcm, base_profile);
    let timeline = Timeline {
        original_sha256: sha,
        sample_rate: pcm.sample_rate,
        channels: pcm.channels as u32,
        frames: pcm.frames() as u64,
        analysis_sample_rate: rbackend::SAMPLE_RATE,
        analysis_frames: analysis.len() as u64,
        analysis_downmix: "mean_of_channels".into(),
        analysis_resampler: if pcm.sample_rate == rbackend::SAMPLE_RATE {
            "none".into()
        } else {
            "libswresample".into()
        },
    };
    Ok(Decoded {
        probe,
        pcm,
        analysis,
        timeline,
        levels,
        window_frames: window as u64,
        signal,
    })
}

/// Свободный анализ записи по готовым эмиссиям.
pub fn pre_analysis(
    decoded: &Decoded,
    primary: (&ModelHandle, &Emissions),
    verifier: Option<(&ModelHandle, &Emissions)>,
    profile: &RecognitionProfile,
) -> PreAnalysis {
    fn view<'a>((h, e): (&'a ModelHandle, &'a Emissions)) -> ModelView<'a> {
        ModelView {
            identity: &h.identity,
            classes: &h.classes,
            emissions: e,
        }
    }
    let noise = decoded.signal.noise_floor_dbfs;
    let activity = recognizer::activity_regions(
        &decoded.levels,
        decoded.window_frames,
        decoded.timeline.frames,
        noise,
        &profile.analysis,
        decoded.timeline.sample_rate,
    );
    recognizer::pre_analyze(
        decoded.timeline.clone(),
        view(primary),
        verifier.map(view),
        activity,
        noise,
        &profile.analysis,
    )
}

// ---------------------------------------------------------------------------
// Контракт результата
// ---------------------------------------------------------------------------

/// Безопасная полная запись: решение прежнего Balanced по всему файлу.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullRecord {
    pub balanced_config_sha256: String,
    pub balanced_outcome: String,
    pub proposed_variant: Option<Variant>,
    /// SHA байтов, которые выбирает Balanced (исходник или его вариант).
    pub selected_sha256: Option<String>,
    pub output: Option<OutputRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementRecord {
    pub triage: Triage,
    /// Улучшенный сигнал выбран (прошёл Enhancement QC на сохраняемом материале).
    pub applied: bool,
    pub inference: Option<InferenceRecord>,
    pub qc: Option<EnhancementQc>,
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingMode {
    /// Вся запись: прежние Balanced и улучшение речи без изменений.
    WholeRecord,
    /// Фрагмент цели: план по фрагменту, улучшение с контекстом, одно кодирование.
    TargetFragment,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessingRecord {
    pub mode: ProcessingMode,
    pub extraction: Option<SampleSpan>,
    pub context: Option<SampleSpan>,
    pub enhancement: Option<EnhancementRecord>,
    /// Решение прежнего этапа улучшения по всей записи (режим всей записи).
    pub legacy_enhancement_outcome: Option<String>,
    /// Balanced итогового кандидата (план, одно кодирование, пост-QC).
    pub balanced: Option<NormalizationResult>,
    /// Обрезка Balanced не задела ядро слова.
    pub trim_preserves_core: bool,
    /// Ожидаемое положение ядра слова в итоговом кандидате (его кадры).
    pub expected_core: Option<SampleSpan>,
    /// Интервал исходной записи (кадры исходной частоты), который несёт
    /// итоговый кандидат после всех обрезок: связь шкалы выдаваемого MP3 со
    /// шкалой исходника для проверок и оценки.
    pub output_span: Option<SampleSpan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackRecord {
    /// Почему понадобилась ветка: речь не распознана до обработки.
    pub trigger: String,
    pub enhancement_outcome: Option<String>,
    pub pre_analysis: Option<PreAnalysis>,
    pub matching: Option<Matching>,
    pub outcome_after: Option<RecognitionOutcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostValidation {
    pub passed: bool,
    pub checks: Vec<Check>,
    pub output_probe: Option<AudioProbe>,
    pub hypothesis: Option<Hypothesis>,
    pub verifier_hypothesis: Option<Hypothesis>,
    pub events: usize,
    pub occurrence: Option<Occurrence>,
    pub expected_core: Option<SampleSpan>,
    /// Наибольшая из ошибок начала и конца ядра (её сравнивает
    /// `timing_consistent`).
    pub timing_error_ms: Option<f64>,
    pub start_error_ms: Option<f64>,
    pub end_error_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionResult {
    pub schema_version: u32,
    pub algorithm_version: String,
    pub config_sha256: String,
    pub profile: ProfileRef,
    pub models: Vec<ModelIdentity>,
    pub input: InputRef,
    pub target: Option<TargetIdentity>,
    /// Ключ цели в кэше (`None` — цель неизвестна).
    pub target_key: Option<String>,
    pub pre_analysis: Option<PreAnalysis>,
    pub matching: Option<Matching>,
    pub selected_occurrence: Option<usize>,
    pub extraction: Option<ExtractionPlan>,
    pub fallback: Option<FallbackRecord>,
    pub full_record: Option<FullRecord>,
    pub processing: Option<ProcessingRecord>,
    pub post_validation: Option<PostValidation>,
    /// Итоговый кандидат (выбранный или отклонённый — как диагностика).
    pub output: Option<OutputRef>,
    pub outcome: RecognitionOutcome,
    pub next_action: RecognitionAction,
    pub reasons: Vec<Reason>,
    pub uncertainty: Uncertainty,
}

impl RecognitionResult {
    /// Согласованность решения: действие следует из исхода, у использования
    /// итогового кандидата есть пройденная повторная проверка и вывод.
    pub fn validate(&self) -> Result<(), AssetError> {
        let inconsistent = |m: &str| AssetError::new(ErrorCode::ManifestCorrupt, m.to_string());
        if self.schema_version != recognizer::RESULT_SCHEMA_VERSION
            || self.algorithm_version != recognizer::ALGORITHM_VERSION
        {
            return Err(inconsistent(
                "неподдерживаемая схема результата распознавания",
            ));
        }
        let has_candidates = self
            .matching
            .as_ref()
            .is_some_and(|m| !m.occurrences.is_empty());
        if recognizer::action_for(self.outcome, has_candidates) != self.next_action {
            return Err(inconsistent("действие не следует из исхода"));
        }
        if self.next_action == RecognitionAction::UseFinalCandidate
            && (self.output.is_none() || !self.post_validation.as_ref().is_some_and(|p| p.passed))
        {
            return Err(inconsistent(
                "итоговый кандидат без пройденной повторной проверки",
            ));
        }
        if self.target.is_none()
            && self.outcome != RecognitionOutcome::TargetIdentityUnavailable
            && self.outcome != RecognitionOutcome::SourceRejected
        {
            return Err(inconsistent("исход сопоставления без цели"));
        }
        Ok(())
    }
}

/// Результат и байты итогового кандидата (не сериализуются).
pub struct Recognized {
    pub result: RecognitionResult,
    pub output_bytes: Option<Vec<u8>>,
    /// Байты безопасной полной записи, если это вариант Balanced.
    pub full_record_bytes: Option<Vec<u8>>,
}

/// Состояние между этапами.
pub struct StageOne {
    result: RecognitionResult,
    output_bytes: Option<Vec<u8>>,
    full_record_bytes: Option<Vec<u8>>,
    /// Ядро исходного вхождения и его свободная гипотеза для сравнения.
    original_occurrence: Option<Occurrence>,
    pending: Option<RecognitionOutcome>,
}

impl StageOne {
    /// Итоговые байты, которым нужна повторная проверка (для группировки).
    pub fn needs_post_validation(&self) -> Option<&[u8]> {
        self.pending.and(self.output_bytes.as_deref())
    }
}

fn uncertainty(result: &RecognitionResult) -> Uncertainty {
    use RecognitionOutcome as O;
    let selected_match = result
        .selected_occurrence
        .and_then(|i| result.matching.as_ref()?.occurrences.get(i))
        .is_some_and(|o| o.evidence == recognizer::Evidence::Match);
    let post = result.post_validation.as_ref().is_some_and(|p| p.passed);
    Uncertainty {
        reading_match: match result.outcome {
            _ if result.target.is_none() => Level::NotApplicable,
            O::TargetWhole | O::TargetExtracted if post => Level::Confirmed,
            O::BoundariesUnresolved if selected_match => Level::Confirmed,
            O::TargetAbsent => Level::Failed,
            _ => Level::Uncertain,
        },
        segmentation: match result.outcome {
            O::TargetExtracted if post => Level::Confirmed,
            O::TargetWhole => Level::NotApplicable,
            O::BoundariesUnresolved | O::PostValidationFailed => Level::Failed,
            _ if result.target.is_none() => Level::NotApplicable,
            _ => Level::Uncertain,
        },
        signal: if result
            .processing
            .as_ref()
            .and_then(|p| p.balanced.as_ref())
            .and_then(|b| b.post_qc.as_ref())
            .is_some_and(|q| q.passed)
            || (post && result.output.is_some())
        {
            Level::Confirmed
        } else {
            Level::Uncertain
        },
        pitch_accent: Level::NotAssessed,
        lexical_identity: if result.target.is_some() {
            Level::NotAssessed
        } else {
            Level::NotApplicable
        },
        provenance: if result.target.is_some() {
            Level::Confirmed
        } else {
            Level::NotApplicable
        },
        confidence: "uncalibrated".into(),
    }
}

fn finish(
    mut result: RecognitionResult,
    outcome: RecognitionOutcome,
    reasons: Vec<Reason>,
) -> RecognitionResult {
    let has_candidates = result
        .matching
        .as_ref()
        .is_some_and(|m| !m.occurrences.is_empty());
    result.outcome = outcome;
    result.next_action = recognizer::action_for(outcome, has_candidates);
    let mut reasons = reasons;
    reasons.sort_by(|a, b| (a.severity, &a.code).cmp(&(b.severity, &b.code)).reverse());
    reasons.dedup_by(|a, b| a.code == b.code);
    result.reasons = reasons;
    result.uncertainty = uncertainty(&result);
    result
}

fn emissions_for(
    ctx: &Context<'_>,
    analysis: &[f32],
) -> Result<(Emissions, Option<Emissions>), AssetError> {
    let primary = ctx
        .source
        .emissions(ctx.primary, &[analysis])?
        .pop()
        .ok_or_else(|| AssetError::new(ErrorCode::ValidatorFailure, "нет эмиссий"))?;
    let verifier = match ctx.verifier {
        Some(v) => ctx.source.emissions(v, &[analysis])?.pop(),
        None => None,
    };
    Ok((primary, verifier))
}

fn analyze_decoded(
    ctx: &Context<'_>,
    decoded: &Decoded,
) -> Result<(PreAnalysis, Emissions, Option<Emissions>), AssetError> {
    let (p, v) = emissions_for(ctx, &decoded.analysis)?;
    let pre = pre_analysis(
        decoded,
        (ctx.primary, &p),
        ctx.verifier.zip(v.as_ref()),
        ctx.profile,
    );
    Ok((pre, p, v))
}

fn match_decoded(
    ctx: &Context<'_>,
    decoded: &Decoded,
    pre: &PreAnalysis,
    p: &Emissions,
    v: Option<&Emissions>,
    target: &TargetIdentity,
) -> Result<(Matching, Selection), recognizer::UnmatchableTarget> {
    let primary = ModelView {
        identity: &ctx.primary.identity,
        classes: &ctx.primary.classes,
        emissions: p,
    };
    let verifier = ctx.verifier.zip(v).map(|(h, e)| ModelView {
        identity: &h.identity,
        classes: &h.classes,
        emissions: e,
    });
    let matching = recognizer::match_target(
        pre,
        primary,
        verifier,
        target,
        &decoded.levels,
        decoded.window_frames,
        ctx.profile,
    )?;
    let selection = recognizer::select(
        pre,
        &matching,
        &decoded.levels,
        decoded.window_frames,
        ctx.profile,
    );
    Ok((matching, selection))
}

pub fn is_infrastructure_failure(error: &AssetError) -> bool {
    enhancer::is_infrastructure_failure(error) || rbackend::is_backend_failure(error)
}

/// Паника рабочего потока — нарушение инварианта кода, а не свойство записи
/// и не сбой среды для повтора: исполнение останавливается явной ошибкой с
/// текстом паники, а элемент остаётся незавершённым.
pub(crate) fn worker_panic(stage: &str, payload: Box<dyn std::any::Any + Send>) -> AssetError {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "без сообщения".into());
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        format!("{stage}: рабочий поток аварийно завершился ({detail}); результат не сохранён"),
        serde_json::json!({"blocker": "worker_panicked", "stage": stage}),
    )
}

/// Выполняет `work`, превращая панику в [`worker_panic`].
pub(crate) fn catch_panic<T>(
    stage: &str,
    work: impl FnOnce() -> Result<T, AssetError>,
) -> Result<T, AssetError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|payload| Err(worker_panic(stage, payload)))
}

/// Результаты параллельного этапа по индексам входов и ошибка потока,
/// упавшего вне отдельного элемента.
pub(crate) type ParallelResults<T> = (Vec<(usize, Result<T, AssetError>)>, Option<AssetError>);

/// Параллельно применяет `work` к входам (`jobs` потоков, очередь по
/// индексу). Паника элемента становится его ошибкой [`worker_panic`], а не
/// пропажей элемента; после `stop` новые входы не выдаются. Каждый выданный
/// вход присутствует в результате ровно один раз.
pub(crate) fn run_parallel<I: Sync, T: Send>(
    stage: &str,
    inputs: &[I],
    jobs: usize,
    stop: &std::sync::atomic::AtomicBool,
    work: impl Fn(&I) -> Result<T, AssetError> + Sync,
) -> ParallelResults<T> {
    use std::sync::atomic::Ordering;
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.min(inputs.len()).max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let Some(input) = inputs.get(i) else {
                            break;
                        };
                        local.push((i, catch_panic(stage, || work(input))));
                    }
                    local
                })
            })
            .collect();
        let mut all = Vec::new();
        let mut panicked = None;
        for worker in workers {
            match worker.join() {
                Ok(local) => all.extend(local),
                Err(payload) => {
                    panicked.get_or_insert(worker_panic(stage, payload));
                }
            }
        }
        all.sort_by_key(|(i, _)| *i);
        (all, panicked)
    })
}

// ---------------------------------------------------------------------------
// Этап 1
// ---------------------------------------------------------------------------

/// Декодирование, анализ, сопоставление, выбор и обработка. `Err` — только
/// сбой среды; свойства записи становятся исходами.
pub fn stage_one(
    bytes: &[u8],
    target: Option<&TargetIdentity>,
    ctx: &Context<'_>,
) -> Result<StageOne, AssetError> {
    ctx.profile.validate()?;
    let mut models = vec![ctx.primary.identity.clone()];
    models.extend(ctx.verifier.map(|v| v.identity.clone()));
    let mut result = RecognitionResult {
        schema_version: recognizer::RESULT_SCHEMA_VERSION,
        algorithm_version: recognizer::ALGORITHM_VERSION.into(),
        config_sha256: ctx.config_sha256.into(),
        profile: ProfileRef {
            id: ctx.profile.id.clone(),
            version: ctx.profile.version,
        },
        models,
        input: InputRef {
            sha256: sha256_hex(bytes),
            byte_length: bytes.len() as u64,
            probe: None,
            error: None,
        },
        target: target.cloned(),
        target_key: target.map(TargetIdentity::matching_key),
        pre_analysis: None,
        matching: None,
        selected_occurrence: None,
        extraction: None,
        fallback: None,
        full_record: None,
        processing: None,
        post_validation: None,
        output: None,
        outcome: RecognitionOutcome::SourceRejected,
        next_action: RecognitionAction::DiscardInput,
        reasons: Vec::new(),
        uncertainty: Uncertainty {
            reading_match: Level::NotApplicable,
            segmentation: Level::NotApplicable,
            signal: Level::NotApplicable,
            pitch_accent: Level::NotAssessed,
            lexical_identity: Level::NotApplicable,
            provenance: Level::NotApplicable,
            confidence: "uncalibrated".into(),
        },
    };
    let done = |result: RecognitionResult, outcome, reasons, full: Option<Vec<u8>>| StageOne {
        result: finish(result, outcome, reasons),
        output_bytes: None,
        full_record_bytes: full,
        original_occurrence: None,
        pending: None,
    };
    if let Some(t) = target {
        t.validate()?;
    }
    let decoded = match decode(bytes, ctx.base_profile)? {
        Ok(decoded) => decoded,
        Err(message) => {
            result.input.error = Some(message.clone());
            let reasons = vec![Reason {
                code: "source_invalid".into(),
                severity: Severity::Reject,
                message: format!("техническая проверка MP3 не пройдена: {message}"),
                origin: Some(ReasonOrigin::SourceSignal),
            }];
            return Ok(done(
                result,
                RecognitionOutcome::SourceRejected,
                reasons,
                None,
            ));
        }
    };
    result.input.probe = Some(decoded.probe.clone());
    // Безопасная полная запись — прежний Balanced по всему файлу.
    let legacy = normalizer::process(
        bytes,
        ctx.base_profile,
        normalizer::Mode::Normalize,
        ctx.base_toolchain,
    )?;
    let full_bytes = legacy.output_bytes.clone();
    result.full_record = Some(FullRecord {
        balanced_config_sha256: legacy.result.config_sha256.clone(),
        balanced_outcome: legacy.result.gate.outcome.as_str().into(),
        proposed_variant: legacy.result.gate.proposed_variant,
        selected_sha256: match legacy.result.gate.proposed_variant {
            Some(Variant::Original) => Some(result.input.sha256.clone()),
            Some(Variant::Processed) => legacy.result.output.as_ref().map(|o| o.sha256.clone()),
            None => None,
        },
        output: legacy.result.output.clone(),
    });
    let (pre, p, v) = analyze_decoded(ctx, &decoded)?;
    result.pre_analysis = Some(pre.clone());
    let Some(target) = target else {
        let reasons = vec![recognition_reason(
            "target_identity_unavailable",
            Severity::Limitation,
            "пара surface + reading для записи не задана доказанным источником: выполнен только свободный анализ, сопоставления и извлечения нет",
        )
        .with_origin(ReasonOrigin::TargetIdentity)];
        return Ok(done(
            result,
            RecognitionOutcome::TargetIdentityUnavailable,
            reasons,
            full_bytes,
        ));
    };
    let (matching, selection) = match match_decoded(ctx, &decoded, &pre, &p, v.as_ref(), target) {
        Ok(found) => found,
        Err(recognizer::UnmatchableTarget(what)) => {
            let reasons = vec![recognition_reason(
                "target_reading_not_phonetic",
                Severity::Limitation,
                format!("чтение цели не представимо фонемами модели ({what}): сопоставление невозможно"),
            )
            .with_origin(ReasonOrigin::TargetIdentity)];
            return Ok(done(
                result,
                RecognitionOutcome::TargetIdentityUnavailable,
                reasons,
                full_bytes,
            ));
        }
    };
    result.matching = Some(matching.clone());
    let mut reasons = selection.reasons.clone();
    let mut selection = selection;
    // Ветка без распознанной речи: безопасное улучшение всей записи и
    // повторный анализ без какой-либо обрезки.
    if selection.outcome == RecognitionOutcome::NoSpeech
        && !pre.activity.is_empty()
        && let Some(item) = ctx.enhancer_item()
    {
        let mut fallback = FallbackRecord {
            trigger: "no_speech_recognized_with_activity".into(),
            enhancement_outcome: None,
            pre_analysis: None,
            matching: None,
            outcome_after: None,
        };
        let selected_bytes = match legacy.result.gate.proposed_variant {
            Some(Variant::Processed) => full_bytes.as_deref(),
            Some(Variant::Original) => Some(bytes),
            None => None,
        };
        let enhanced = enhancer::process_item(&item, bytes, &legacy.result, selected_bytes)?;
        fallback.enhancement_outcome = Some(enhanced.result.gate.outcome.as_str().into());
        if let (enhancer::EnhancementOutcome::Enhanced, Some(enhanced_bytes)) =
            (enhanced.result.gate.outcome, enhanced.output_bytes.as_ref())
            && let Ok(after) = decode(enhanced_bytes, ctx.base_profile)?
        {
            let (pre2, p2, v2) = analyze_decoded(ctx, &after)?;
            if let Ok((m2, s2)) = match_decoded(ctx, &after, &pre2, &p2, v2.as_ref(), target) {
                // Вхождения повторного анализа — в отсчётах улучшенной записи;
                // в `matching` результата они переводятся на шкалу исходника,
                // порядок и индексы сохраняются.
                let lead_trim = enhanced
                    .result
                    .balanced
                    .as_ref()
                    .and_then(|b| b.plan.as_ref())
                    .map_or(0, |p| p.leading.trim_frames);
                let m2_on_source = matching_on_source(
                    m2.clone(),
                    lead_trim,
                    after.timeline.sample_rate,
                    decoded.timeline.sample_rate,
                );
                fallback.outcome_after = Some(s2.outcome);
                fallback.pre_analysis = Some(pre2.clone());
                fallback.matching = Some(m2.clone());
                reasons.push(recognition_reason(
                    "pre_analysis_failed_before_enhancement",
                    Severity::Warning,
                    "до улучшения речь не распознана: применено консервативное улучшение всей записи и повторный анализ",
                ));
                if s2.outcome == RecognitionOutcome::TargetWhole {
                    // Вся улучшенная запись — цель: кандидат — улучшенный файл
                    // без какой-либо обрезки, после повторной проверки.
                    let occurrence = s2.selected.and_then(|i| m2.occurrences.get(i).cloned());
                    result.matching = Some(m2_on_source);
                    result.selected_occurrence = s2.selected;
                    result.fallback = Some(fallback);
                    result.output = enhanced.result.output.clone();
                    // Вхождение найдено на самом улучшенном файле, то есть
                    // уже после обрезки Balanced: ожидаемое ядро — его же
                    // ядро, повторный сдвиг на обрезку исказил бы проверку.
                    // До улучшения речь не распознана, поэтому ядра на шкале
                    // исходника, с которым можно сверить обрезку, нет.
                    let expected = occurrence.as_ref().map(|o| o.core);
                    let output_span = Some(kept_span(
                        SampleSpan {
                            start: 0,
                            end: decoded.timeline.frames,
                        },
                        enhanced.result.balanced.as_ref(),
                        true,
                    ));
                    result.processing = Some(ProcessingRecord {
                        mode: ProcessingMode::WholeRecord,
                        extraction: None,
                        context: None,
                        enhancement: None,
                        legacy_enhancement_outcome: Some(
                            enhanced.result.gate.outcome.as_str().into(),
                        ),
                        balanced: enhanced.result.balanced.clone(),
                        trim_preserves_core: true,
                        expected_core: expected,
                        output_span,
                    });
                    result.reasons = reasons;
                    return Ok(StageOne {
                        result,
                        output_bytes: Some(enhanced_bytes.clone()),
                        full_record_bytes: full_bytes,
                        original_occurrence: occurrence,
                        pending: Some(RecognitionOutcome::TargetWhole),
                    });
                }
                // Выбор ниже индексирует вхождения повторного анализа: список
                // кандидатов результата обязан быть тем же.
                result.matching = Some(m2_on_source);
                if matches!(s2.outcome, RecognitionOutcome::TargetExtracted) {
                    reasons.push(recognition_reason(
                        "target_found_only_after_enhancement",
                        Severity::Limitation,
                        "цель найдена только после улучшения: материал, нераспознанный до обработки, автоматически не режется",
                    ));
                    selection = Selection {
                        outcome: RecognitionOutcome::BoundariesUnresolved,
                        selected: s2.selected,
                        plan: None,
                        reasons: Vec::new(),
                    };
                } else {
                    selection = Selection {
                        reasons: Vec::new(),
                        ..s2
                    };
                }
            }
        }
        result.fallback = Some(fallback);
    }
    result.selected_occurrence = selection.selected;
    result.extraction = selection.plan.clone();
    let occurrence = selection
        .selected
        .and_then(|i| result.matching.as_ref()?.occurrences.get(i).cloned());
    match selection.outcome {
        RecognitionOutcome::TargetWhole => {
            let (output, mut record, derived) = process_whole(ctx, bytes, &legacy, &mut reasons)?;
            let expected = occurrence.as_ref().map(|o| {
                expected_in_output(
                    o.core,
                    record.balanced.as_ref(),
                    derived,
                    decoded.timeline.sample_rate,
                )
            });
            record.expected_core = expected;
            let whole = SampleSpan {
                start: 0,
                end: decoded.timeline.frames,
            };
            let kept = kept_span(whole, record.balanced.as_ref(), derived);
            record.output_span = output.as_ref().map(|_| kept);
            record.trim_preserves_core = occurrence.as_ref().is_none_or(|o| kept.contains(&o.core));
            result.output = output.as_ref().map(|(r, _)| r.clone());
            result.processing = Some(record);
            result.reasons = reasons;
            Ok(StageOne {
                result,
                output_bytes: output.map(|(_, b)| b),
                full_record_bytes: full_bytes,
                original_occurrence: occurrence,
                pending: Some(RecognitionOutcome::TargetWhole),
            })
        }
        RecognitionOutcome::TargetExtracted => {
            let plan = selection.plan.clone().expect("извлечение с планом");
            let core = occurrence
                .as_ref()
                .map(|o| o.core)
                .expect("выбранное вхождение");
            let (output, record) =
                process_fragment(ctx, &decoded, &result.input, &plan, core, &mut reasons)?;
            let failed = output.is_none();
            result.output = output.as_ref().map(|(r, _)| r.clone());
            result.processing = Some(record);
            if failed {
                result.reasons = reasons.clone();
                return Ok(done(
                    result,
                    RecognitionOutcome::PostValidationFailed,
                    reasons,
                    full_bytes,
                ));
            }
            result.reasons = reasons;
            Ok(StageOne {
                result,
                output_bytes: output.map(|(_, b)| b),
                full_record_bytes: full_bytes,
                original_occurrence: occurrence,
                pending: Some(RecognitionOutcome::TargetExtracted),
            })
        }
        other => Ok(done(result, other, reasons, full_bytes)),
    }
}

/// Положение ядра в кандидате: сдвиг на обрезку Balanced (если кандидат
/// производный) и пересчёт в частоту вывода.
fn expected_in_output(
    core: SampleSpan,
    balanced: Option<&NormalizationResult>,
    derived: bool,
    source_rate: u32,
) -> SampleSpan {
    let plan = balanced.and_then(|b| b.plan.as_ref()).filter(|_| derived);
    let trim = plan.map(|p| p.leading.trim_frames).unwrap_or(0);
    let out_rate = plan.map(|p| p.output_sample_rate).unwrap_or(source_rate);
    let shifted = shift(core, trim);
    let convert = |v: u64| v * u64::from(out_rate) / u64::from(source_rate);
    SampleSpan {
        start: convert(shifted.start),
        end: convert(shifted.end),
    }
}

/// Обратное к `expected_in_output` для производной записи: кадры записи с
/// частотой `out_rate`, из начала которой Balanced срезал `lead_trim` кадров
/// исходника, → кадры исходника.
fn to_source(span: SampleSpan, lead_trim: u64, out_rate: u32, source_rate: u32) -> SampleSpan {
    let convert = |v: u64| {
        let frames = u128::from(v) * u128::from(source_rate) / u128::from(out_rate.max(1));
        u64::try_from(frames)
            .unwrap_or(u64::MAX)
            .saturating_add(lead_trim)
    };
    SampleSpan {
        start: convert(span.start),
        end: convert(span.end),
    }
}

/// Вхождения анализа производной записи на шкале исходника.
fn matching_on_source(
    mut matching: Matching,
    lead_trim: u64,
    out_rate: u32,
    source_rate: u32,
) -> Matching {
    for o in &mut matching.occurrences {
        o.core = to_source(o.core, lead_trim, out_rate, source_rate);
        o.start_uncertainty = to_source(o.start_uncertainty, lead_trim, out_rate, source_rate);
        o.end_uncertainty = to_source(o.end_uncertainty, lead_trim, out_rate, source_rate);
    }
    matching
}

fn shift(span: SampleSpan, by: u64) -> SampleSpan {
    SampleSpan {
        start: span.start.saturating_sub(by),
        end: span.end.saturating_sub(by),
    }
}

/// Часть `available` (кадры исходника), оставшаяся после обрезки Balanced
/// производного кандидата; непроизводный кандидат — весь `available`.
pub(crate) fn kept_span(
    available: SampleSpan,
    balanced: Option<&NormalizationResult>,
    derived: bool,
) -> SampleSpan {
    let plan = balanced.and_then(|b| b.plan.as_ref()).filter(|_| derived);
    let lead = plan.map(|p| p.leading.trim_frames).unwrap_or(0);
    let trail = plan.map(|p| p.trailing.trim_frames).unwrap_or(0);
    let start = (available.start + lead).min(available.end);
    SampleSpan {
        start,
        end: available.end.saturating_sub(trail).max(start),
    }
}

trait WithOrigin {
    fn with_origin(self, origin: ReasonOrigin) -> Self;
}

impl WithOrigin for Reason {
    fn with_origin(mut self, origin: ReasonOrigin) -> Self {
        self.origin = Some(origin);
        self
    }
}

type Output = Option<(OutputRef, Vec<u8>)>;

/// Вся запись: прежние Balanced и улучшение речи по всему файлу. Кандидат —
/// байты, которые выбрали бы прежние этапы. Третий элемент — кандидат
/// производный (обрезка Balanced применена).
fn process_whole(
    ctx: &Context<'_>,
    bytes: &[u8],
    legacy: &normalizer::Processed,
    reasons: &mut Vec<Reason>,
) -> Result<(Output, ProcessingRecord, bool), AssetError> {
    let mut record = ProcessingRecord {
        mode: ProcessingMode::WholeRecord,
        extraction: None,
        context: None,
        enhancement: None,
        legacy_enhancement_outcome: None,
        balanced: Some(legacy.result.clone()),
        trim_preserves_core: true,
        expected_core: None,
        output_span: None,
    };
    let selected: Option<(OutputRef, Vec<u8>)> = match legacy.result.gate.proposed_variant {
        Some(Variant::Processed) => legacy
            .output_bytes
            .clone()
            .zip(legacy.result.output.clone())
            .map(|(b, r)| (r, b)),
        Some(Variant::Original) => Some((
            OutputRef {
                sha256: sha256_hex(bytes),
                byte_length: bytes.len() as u64,
                probe: legacy.result.input.probe.clone(),
                storage_path: None,
            },
            bytes.to_vec(),
        )),
        None => None,
    };
    let derived = legacy.result.gate.proposed_variant == Some(Variant::Processed);
    if let Some(item) = ctx.enhancer_item() {
        let selected_bytes = selected.as_ref().map(|(_, b)| b.as_slice());
        let enhanced = enhancer::process_item(&item, bytes, &legacy.result, selected_bytes)?;
        record.legacy_enhancement_outcome = Some(enhanced.result.gate.outcome.as_str().into());
        if enhanced.result.gate.outcome == enhancer::EnhancementOutcome::Enhanced
            && let (Some(out), Some(b)) = (enhanced.result.output.clone(), enhanced.output_bytes)
        {
            reasons.push(recognition_reason(
                "whole_record_enhanced",
                Severity::Info,
                "вся запись — целевое произношение: выбран нейровариант прежнего этапа улучшения",
            ));
            record.balanced = enhanced.result.balanced.clone();
            return Ok((Some((out, b)), record, true));
        }
    }
    Ok((selected, record, derived))
}

/// Фрагмент цели: улучшение речи с исходным контекстом, Enhancement QC на
/// сохраняемом фрагменте, Balanced по фрагменту с краями по исходнику и одно
/// кодирование. `None` в выводе — кандидат не получен (причины записаны).
fn process_fragment(
    ctx: &Context<'_>,
    decoded: &Decoded,
    input: &InputRef,
    plan: &ExtractionPlan,
    core: SampleSpan,
    reasons: &mut Vec<Reason>,
) -> Result<(Output, ProcessingRecord), AssetError> {
    let pcm = &decoded.pcm;
    let e = plan.extraction;
    let fragment = pcm.slice_frames(e.start as usize, e.end as usize);
    let mut record = ProcessingRecord {
        mode: ProcessingMode::TargetFragment,
        extraction: Some(e),
        context: Some(plan.context),
        enhancement: None,
        legacy_enhancement_outcome: None,
        balanced: None,
        trim_preserves_core: false,
        expected_core: None,
        output_span: None,
    };
    let mut signal = fragment.clone();
    if let Some(setup) = &ctx.enhancer {
        let analysis = normalizer::analyze_pcm(&fragment, ctx.base_profile);
        let prepared = normalizer::prepare(&fragment, ctx.base_profile);
        let noise_limited = prepared
            .plan
            .gain
            .limited_by
            .contains(&normalizer::GainLimit::NoiseFloor);
        let triage = enhancer::triage_signals(&analysis, noise_limited, setup.profile);
        let mut enh = EnhancementRecord {
            triage: triage.clone(),
            applied: false,
            inference: None,
            qc: None,
            reasons: Vec::new(),
        };
        if triage.candidate
            && fragment.channels <= 2
            && triage.duration_ms >= setup.profile.triage.min_duration_ms
        {
            let c = plan.context;
            let context = pcm.slice_frames(c.start as usize, c.end as usize);
            let resampled = context.sample_rate != ebackend::BACKEND_SAMPLE_RATE;
            let at_backend = if resampled {
                probe::resample_f32(
                    &context.samples,
                    context.sample_rate,
                    context.channels as u32,
                    ebackend::BACKEND_SAMPLE_RATE,
                )?
            } else {
                context.samples.clone()
            };
            let (out48, run) = ebackend::enhance(
                setup.worker,
                setup.model_dir,
                setup.backend,
                context.channels,
                &at_backend,
                std::time::Duration::from_secs(setup.profile.backend.timeout_seconds),
            )?;
            let mut back = if resampled {
                probe::resample_f32(
                    &out48,
                    ebackend::BACKEND_SAMPLE_RATE,
                    context.channels as u32,
                    context.sample_rate,
                )?
            } else {
                out48
            };
            back.resize(context.samples.len(), 0.0);
            let enhanced_context = Pcm {
                sample_rate: context.sample_rate,
                channels: context.channels,
                samples: back,
            };
            let offset = (e.start - c.start) as usize;
            let enhanced = enhanced_context.slice_frames(offset, offset + fragment.frames());
            let enhanced_analysis = normalizer::analyze_pcm(&enhanced, ctx.base_profile);
            let qc = enhancer::enhancement_qc(
                &enhancer::QcInput {
                    original: &fragment,
                    enhanced: &enhanced,
                    original_analysis: &analysis,
                    enhanced_analysis: &enhanced_analysis,
                },
                setup.profile,
            );
            enh.inference = Some(InferenceRecord {
                source_sample_rate: context.sample_rate,
                backend_sample_rate: ebackend::BACKEND_SAMPLE_RATE,
                resampled,
                channels: context.channels as u32,
                run,
            });
            if qc.passed {
                enh.applied = true;
                signal = enhanced;
                enh.reasons.push(recognition_reason(
                    "fragment_enhanced",
                    Severity::Info,
                    "улучшение с исходным контекстом прошло Enhancement QC на сохраняемом фрагменте",
                ));
            } else {
                enh.reasons.extend(enhancer::qc_reasons(&qc));
                enh.reasons.push(recognition_reason(
                    "fragment_enhancement_rejected",
                    Severity::Info,
                    "улучшенный фрагмент не прошёл Enhancement QC: кандидат строится из исходного PCM",
                ));
            }
            enh.qc = Some(qc);
        }
        reasons.extend(enh.reasons.iter().cloned());
        record.enhancement = Some(enh);
    }
    let processed = normalizer::balance_enhanced(
        input.clone(),
        &fragment,
        &signal,
        ctx.base_profile,
        ctx.base_toolchain,
    )?;
    let plan_b = processed.result.plan.clone();
    let lead = plan_b.as_ref().map(|p| p.leading.trim_frames).unwrap_or(0);
    let trail = plan_b.as_ref().map(|p| p.trailing.trim_frames).unwrap_or(0);
    let preserves = lead <= core.start - e.start && trail <= e.end - core.end;
    record.trim_preserves_core = preserves;
    let kept = kept_span(e, Some(&processed.result), true);
    record.output_span = Some(kept);
    let out_rate = plan_b
        .as_ref()
        .map(|p| p.output_sample_rate)
        .unwrap_or(decoded.timeline.sample_rate);
    let to_out =
        |frames: u64| frames * u64::from(out_rate) / u64::from(decoded.timeline.sample_rate);
    record.expected_core = Some(SampleSpan {
        start: to_out(core.start - e.start - lead.min(core.start - e.start)),
        end: to_out(core.end - e.start - lead.min(core.start - e.start)),
    });
    let accepted = processed.result.gate.proposed_variant == Some(Variant::Processed);
    let output = processed.result.output.clone();
    let bytes = processed.output_bytes;
    record.balanced = Some(processed.result);
    if !preserves {
        reasons.push(recognition_reason(
            "fragment_trim_would_cut_target",
            Severity::Limitation,
            "обрезка Balanced задела бы ядро целевого слова: фрагмент не используется",
        ));
        return Ok((None, record));
    }
    if !accepted {
        reasons.push(recognition_reason(
            "fragment_post_qc_failed",
            Severity::Limitation,
            "кодированный фрагмент не прошёл пост-QC Balanced: сохранена полная запись",
        ));
        return Ok((None, record));
    }
    Ok((output.zip(bytes), record))
}

// ---------------------------------------------------------------------------
// Этап 2
// ---------------------------------------------------------------------------

/// Повторное распознавание фактически декодированного итогового кандидата
/// и окончательное решение.
pub fn stage_two(stage: StageOne, ctx: &Context<'_>) -> Result<Recognized, AssetError> {
    let StageOne {
        mut result,
        output_bytes,
        full_record_bytes,
        original_occurrence,
        pending,
    } = stage;
    let Some(outcome) = pending else {
        return Ok(Recognized {
            result,
            output_bytes,
            full_record_bytes,
        });
    };
    let bytes = output_bytes.clone().expect("кандидат этапа 1");
    let mut reasons = result.reasons.clone();
    let target = result.target.clone().expect("цель");
    let mut checks = Vec::new();
    let mut post = PostValidation {
        passed: false,
        checks: Vec::new(),
        output_probe: None,
        hypothesis: None,
        verifier_hypothesis: None,
        events: 0,
        occurrence: None,
        expected_core: result.processing.as_ref().and_then(|p| p.expected_core),
        timing_error_ms: None,
        start_error_ms: None,
        end_error_ms: None,
    };
    let check = |id: &str, passed: bool, value: Option<f64>, limit: Option<f64>| Check {
        id: id.into(),
        passed,
        value,
        limit,
    };
    match decode(&bytes, ctx.base_profile)? {
        Err(message) => {
            checks.push(check("decoded_output_valid", false, None, None));
            reasons.push(recognition_reason(
                "post_validation_decode_failed",
                Severity::Limitation,
                format!("итоговый кандидат не прошёл техническую проверку MP3: {message}"),
            ));
        }
        Ok(after) => {
            checks.push(check("decoded_output_valid", true, None, None));
            post.output_probe = Some(after.probe.clone());
            let (pre, p, v) = analyze_decoded(ctx, &after)?;
            post.events = pre.events.len();
            post.hypothesis = Some(pre.primary.clone());
            post.verifier_hypothesis = pre.verifier.clone();
            // Число событий — диагностика: пауза внутри слова (смычка перед
            // `ch`, оглушение) делит его на события, это не другая речь.
            // Другую речь ловит `no_other_speech` по токенам вне вхождения.
            let matched = match_decoded(ctx, &after, &pre, &p, v.as_ref(), &target).ok();
            // Повторная проверка — проверка сохранности: независимость уже
            // установлена на исходнике. Обязательны совпадение свободной
            // гипотезы основной модели, порог выравнивания и запас над
            // похожими чтениями; вторая модель на коротком фрагменте может
            // ослышаться и блокирует результат, только если слышит
            // конкурирующее чтение (`Conflict`).
            let policy = &ctx.profile.matching;
            let preserved_match = |o: &&Occurrence| {
                o.evidence != recognizer::Evidence::Conflict
                    && o.free_distance == 0
                    && f64::from(o.score_per_token) >= policy.match_min_score_per_token
                    && o.competitor_margin
                        .is_none_or(|m| f64::from(m) >= policy.min_competitor_margin)
            };
            let best = matched.as_ref().and_then(|(m, _)| {
                m.occurrences
                    .iter()
                    .filter(preserved_match)
                    .max_by(|a, b| a.score_per_token.total_cmp(&b.score_per_token))
                    .cloned()
            });
            let conflict = matched.as_ref().is_some_and(|(m, _)| {
                m.occurrences
                    .iter()
                    .any(|o| o.evidence == recognizer::Evidence::Conflict)
            });
            checks.push(check("no_competing_reading", !conflict, None, None));
            let all_occurrences = matched
                .as_ref()
                .map(|(m, _)| m.occurrences.len())
                .unwrap_or(0);
            checks.push(check(
                "target_reading_confirmed",
                best.is_some(),
                None,
                None,
            ));
            checks.push(check(
                "single_target_occurrence",
                all_occurrences == 1,
                Some(all_occurrences as f64),
                Some(1.0),
            ));
            let preserved = match (&best, &original_occurrence) {
                (Some(after), Some(before)) => after.free_phonemes == before.free_phonemes,
                _ => false,
            };
            checks.push(check("free_hypothesis_preserved", preserved, None, None));
            let extra = matched
                .as_ref()
                .zip(best.as_ref())
                .map(|(_, occ)| recognizer::other_material(&pre, occ, 1).len())
                .unwrap_or(usize::MAX);
            checks.push(check(
                "no_other_speech",
                extra == 0,
                (extra != usize::MAX).then_some(extra as f64),
                Some(0.0),
            ));
            // Сохранность обоих краёв: ядро в декодированном кандидате
            // сверяется с ожидаемым положением (сдвиг на обрезку Balanced и
            // пересчёт частоты уже учтены в `expected_core`) и по началу, и
            // по концу. Преждевременно обрезанный или продлённый конец при
            // верном начале этой проверки не проходит.
            let tolerance = ctx.profile.post.timing_tolerance_ms;
            if let (Some(occ), Some(expected)) = (&best, post.expected_core) {
                let ms =
                    |frames: u64| frames as f64 * 1000.0 / f64::from(after.timeline.sample_rate);
                let start_ms = ms(occ.core.start.abs_diff(expected.start));
                let end_ms = ms(occ.core.end.abs_diff(expected.end));
                let worst = start_ms.max(end_ms);
                post.start_error_ms = Some(start_ms);
                post.end_error_ms = Some(end_ms);
                post.timing_error_ms = Some(worst);
                checks.push(check(
                    "timing_consistent",
                    worst <= tolerance,
                    Some(worst),
                    Some(tolerance),
                ));
            } else {
                checks.push(check("timing_consistent", false, None, Some(tolerance)));
            }
            post.occurrence = best;
        }
    }
    post.passed = checks.iter().all(|c| c.passed);
    post.checks = checks;
    let passed = post.passed;
    result.post_validation = Some(post);
    let outcome = if passed {
        reasons.push(recognition_reason(
            "post_validation_passed",
            Severity::Info,
            "повторное распознавание декодированного результата подтвердило одно целевое произношение без другой речи",
        ));
        outcome
    } else {
        let failed: Vec<String> = result
            .post_validation
            .as_ref()
            .map(|p| {
                p.checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| c.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        reasons.push(recognition_reason(
            "post_validation_failed",
            Severity::Limitation,
            format!(
                "повторная проверка не пройдена ({}): сохранена безопасная полная запись",
                failed.join(", ")
            ),
        ));
        RecognitionOutcome::PostValidationFailed
    };
    let result = finish(result, outcome, reasons);
    Ok(Recognized {
        output_bytes: if passed { output_bytes } else { None },
        result,
        full_record_bytes,
    })
}

/// Полный путь одной записи с прямыми вызовами моделей.
pub fn recognize_one(
    bytes: &[u8],
    target: Option<&TargetIdentity>,
    ctx: &Context<'_>,
) -> Result<Recognized, AssetError> {
    let stage = stage_one(bytes, target, ctx)?;
    stage_two(stage, ctx)
}

#[cfg(test)]
mod tests;
