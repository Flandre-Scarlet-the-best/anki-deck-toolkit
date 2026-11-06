//! Speech Enhancement поверх Balanced Normalizer: триаж по измерениям,
//! локальный инференс DeepFilterNet3, Enhancement QC и маршрутизация.
//!
//! Конвейер одного входа:
//!
//! ```text
//! исходный MP3 + результат Balanced (schema 2, normalize)
//!   → REJECT Balanced → INPUT_REJECTED (discard_input)
//!   → probe_mp3 и декодирование исходника (не Balanced MP3: потерь второго
//!     поколения нет)
//!   → триаж по шуму: не нужен → NOT_NEEDED (точные байты выбранного входа)
//!   → неприменимо (длина) → DEFERRED
//!   → явный ресэмплинг в 48 kHz → DeepFilterNet3 по каналам → обратно
//!   → Enhancement QC улучшенного PCM против исходного PCM
//!   → Balanced над улучшенным PCM (края — по исходнику, громкость — по
//!     улучшенному сигналу), одно кодирование libmp3lame и обычный пост-QC
//!   → ENHANCED либо ENHANCEMENT_REJECTED (точные байты выбранного входа)
//! ```
//!
//! Снижение шума — необходимое, но не достаточное условие: обязательные
//! проверки сохранности сильной речи, коротких сегментов, слабых звуков у
//! речи, краёв, спектрального баланса, громкости, перегрузки и выравнивания
//! должны быть пройдены и разрешимы. Это сигнальные эвристики, а не
//! доказательство сохранности японской фонетики: сомнение всегда оставляет
//! вход. Сбой backend (нет модели, авария, таймаут) — не исход входа, а
//! ошибка среды для повтора.

use serde::{Deserialize, Serialize};

use super::dsp::{self, Pcm};
use super::enhancer_backend::{
    self as backend, BackendIdentity, BackendKind, InferenceParams, InferenceRun,
};
use super::normalizer::{
    self, GainLimit, GateOutcome, InputRef, NormalizationResult, NormalizerProfile, OutputRef,
    Reason, ReasonOrigin, Severity, Toolchain, Variant,
};
use super::probe;
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const RESULT_SCHEMA_VERSION: u32 = 1;
/// Версия собственных алгоритмов триажа, Enhancement QC и маршрутизации.
/// Входит в ключ кэша: любое изменение их логики обязано менять версию,
/// иначе прежние результаты будут ложно переиспользованы под новым кодом.
pub const ALGORITHM_VERSION: &str = "speech-enhancement-gate/1";
/// Режим исполнения инференса: влияет на байты, поэтому входит в отпечаток.
pub const EXECUTION_MODE: &str = "cpu-tract-single-thread";

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn inconsistent(message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        message,
        serde_json::json!({"blocker": "inconsistent_enhancement_gate"}),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriagePolicy {
    /// Оценка SNR (уровень речи p95 минус шумовой пол) ниже — кандидат.
    pub max_estimated_snr_db: f64,
    /// Шумовой пол выше — кандидат независимо от SNR.
    pub min_noise_floor_dbfs: f64,
    /// Короче — модель не применяется (нормировка модели не успевает
    /// установиться), запись откладывается.
    pub min_duration_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendPolicy {
    /// Вид backend. DeepFilterNet3 по умолчанию не сериализуется, поэтому
    /// профиль и отпечаток конфигурации DF3 от появления поля не меняются.
    #[serde(default, skip_serializing_if = "BackendKind::is_default")]
    pub kind: BackendKind,
    pub inference: InferenceParams,
    /// Предел времени и CPU одного вызова воркера.
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementGatePolicy {
    /// Минимальное снижение шумового пола (p10 окон 10 мс), dB.
    pub min_noise_reduction_db: f64,
    /// Окна не ниже «речь p95 минус это» — сильная речь.
    pub strong_window_margin_db: f64,
    pub max_strong_drop_median_db: f64,
    pub max_strong_drop_p90_db: f64,
    /// Окна речи (не ниже «речь p95 минус 20 dB»), ослабленные сильнее,
    /// считаются потерянными; их подряд идущая серия ограничена.
    pub segment_loss_drop_db: f64,
    pub max_segment_loss_ms: f64,
    /// Слабые окна: выше шумового пола хотя бы на это и ниже сильной речи,
    /// не дальше `weak_context_ms` от сильной речи (согласные, оглушённые
    /// гласные, переходы).
    pub weak_window_above_noise_db: f64,
    pub weak_context_ms: f64,
    pub max_weak_drop_median_db: f64,
    /// Первые и последние окна звука (выше шума на `weak_window_above_noise_db`).
    pub edge_windows: usize,
    pub max_edge_drop_db: f64,
    /// Тихие окна (не выше шумового пола плюс это) не должны получать энергию.
    pub quiet_window_above_noise_db: f64,
    pub max_quiet_rise_p95_db: f64,
    /// Изменение энергии полос сильной речи.
    pub max_band_attenuation_db: f64,
    pub max_band_boost_db: f64,
    /// Падение громкости (тем же методом BS.1770).
    pub max_loudness_drop_db: f64,
    pub max_alignment_lag_ms: f64,
}

/// Профиль этапа: строгая схема, собственные `id`/`version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementProfile {
    pub schema_version: u32,
    pub id: String,
    pub version: u32,
    pub triage: TriagePolicy,
    pub backend: BackendPolicy,
    pub gate: EnhancementGatePolicy,
}

impl EnhancementProfile {
    pub fn conservative() -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: "dfn3-conservative".into(),
            version: 1,
            triage: TriagePolicy {
                max_estimated_snr_db: 35.0,
                min_noise_floor_dbfs: -55.0,
                min_duration_ms: 250.0,
            },
            // Предел ослабления 18 dB и контекст прогрева 200 мс выбраны по
            // калибровке на реальных произношениях: без предела модель
            // полностью гасит слабые края и хвосты слов, без контекста —
            // начало клипа (старт потоковой модели с нулевых состояний).
            backend: BackendPolicy {
                kind: BackendKind::DeepFilterNet3,
                inference: InferenceParams {
                    atten_lim_db: 18.0,
                    post_filter_beta: 0.0,
                    context_ms: 200,
                },
                timeout_seconds: 60,
            },
            gate: EnhancementGatePolicy {
                min_noise_reduction_db: 6.0,
                strong_window_margin_db: 15.0,
                max_strong_drop_median_db: 1.5,
                max_strong_drop_p90_db: 3.0,
                segment_loss_drop_db: 10.0,
                max_segment_loss_ms: 30.0,
                weak_window_above_noise_db: 10.0,
                weak_context_ms: 150.0,
                max_weak_drop_median_db: 6.0,
                edge_windows: 3,
                max_edge_drop_db: 6.0,
                quiet_window_above_noise_db: 3.0,
                max_quiet_rise_p95_db: 3.0,
                max_band_attenuation_db: 6.0,
                max_band_boost_db: 3.0,
                max_loudness_drop_db: 4.0,
                max_alignment_lag_ms: 1.0,
            },
        }
    }

    /// Экспериментальный профиль с denoiser Resemble Enhance: тот же triage
    /// и тот же Enhancement QualityGate, что у DeepFilterNet3; у backend нет
    /// параметров инференса. Предел времени шире: процесс каждый раз
    /// загружает PyTorch и чекпойнт, а без CUDA считает на CPU.
    pub fn resemble_denoise() -> Self {
        let mut profile = Self::conservative();
        profile.id = "resemble-denoise-conservative".into();
        profile.version = 1;
        profile.backend = BackendPolicy {
            kind: BackendKind::ResembleEnhanceDenoise,
            inference: InferenceParams::default(),
            timeout_seconds: 300,
        };
        profile
    }

    /// Встроенный профиль для вида backend.
    pub fn for_backend(kind: BackendKind) -> Self {
        match kind {
            BackendKind::DeepFilterNet3 => Self::conservative(),
            BackendKind::ResembleEnhanceDenoise => Self::resemble_denoise(),
        }
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        let g = &self.gate;
        let finite = [
            self.triage.max_estimated_snr_db,
            self.triage.min_noise_floor_dbfs,
            self.triage.min_duration_ms,
            g.min_noise_reduction_db,
            g.strong_window_margin_db,
            g.max_strong_drop_median_db,
            g.max_strong_drop_p90_db,
            g.segment_loss_drop_db,
            g.max_segment_loss_ms,
            g.weak_window_above_noise_db,
            g.weak_context_ms,
            g.max_weak_drop_median_db,
            g.max_edge_drop_db,
            g.quiet_window_above_noise_db,
            g.max_quiet_rise_p95_db,
            g.max_band_attenuation_db,
            g.max_band_boost_db,
            g.max_loudness_drop_db,
            g.max_alignment_lag_ms,
        ];
        backend::check_params(self.backend.kind, &self.backend.inference)
            .map_err(|_| invalid("параметры инференса профиля не подходят к его backend"))?;
        if self.schema_version != PROFILE_SCHEMA_VERSION
            || self.id.trim().is_empty()
            || finite.iter().any(|v| !v.is_finite())
            || g.min_noise_reduction_db <= 0.0
            || g.edge_windows == 0
            || self.backend.timeout_seconds == 0
            || self.backend.timeout_seconds > 3600
        {
            return Err(invalid("профиль улучшения речи некорректен"));
        }
        Ok(())
    }
}

/// Отпечаток всего, что определяет результат входа: схема, алгоритмы,
/// профиль, идентичность backend (SHA воркера, веса, параметры), режим
/// исполнения и отпечаток конфигурации Balanced (его профиль, FFmpeg,
/// ffprobe, кодировщик и анализатор).
pub fn config_sha256(
    profile: &EnhancementProfile,
    backend: &BackendIdentity,
    base_config_sha256: &str,
) -> String {
    sha256_hex(
        serde_json::to_vec(&serde_json::json!({
            "schema_version": RESULT_SCHEMA_VERSION,
            "algorithm": ALGORITHM_VERSION,
            "execution": EXECUTION_MODE,
            "profile": profile,
            "backend": backend,
            "base_config_sha256": base_config_sha256,
        }))
        .expect("конфигурация сериализуема"),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EnhancementOutcome {
    /// Шум по измерениям не требует улучшения: вход не трогается.
    NotNeeded,
    /// Улучшенный вариант прошёл Enhancement QC и пост-QC Balanced.
    Enhanced,
    /// Нейровариант получен, но отклонён (польза не доказана, сохранность
    /// под сомнением или пост-QC не пройден): оставлен вход.
    EnhancementRejected,
    /// Модель к записи не применяется сейчас (например, слишком короткая):
    /// оставлен вход, запись передана следующей автоматической стадии.
    Deferred,
    /// Balanced признал вход технически непригодным.
    InputRejected,
}

impl EnhancementOutcome {
    pub const ALL: [Self; 5] = [
        Self::NotNeeded,
        Self::Enhanced,
        Self::EnhancementRejected,
        Self::Deferred,
        Self::InputRejected,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotNeeded => "NOT_NEEDED",
            Self::Enhanced => "ENHANCED",
            Self::EnhancementRejected => "ENHANCEMENT_REJECTED",
            Self::Deferred => "DEFERRED",
            Self::InputRejected => "INPUT_REJECTED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EnhancementAction {
    /// Технический кандидат — выбранный ранее вход (точные байты).
    UseInput,
    /// Технический кандидат — улучшенный вариант.
    UseEnhanced,
    /// Выбранные байты — лучший доступный кандидат; ограничения переданы
    /// будущей автоматической обработке. Человек не требуется.
    DeferredAutomaticProcessing,
    DiscardInput,
}

impl EnhancementAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UseInput => "use_input",
            Self::UseEnhanced => "use_enhanced",
            Self::DeferredAutomaticProcessing => "deferred_automatic_processing",
            Self::DiscardInput => "discard_input",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// Байты, выбранные Balanced (`processed` или `original`), без изменений.
    Input,
    Enhanced,
}

/// Байты, выбранные этапом Balanced для этого входа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedInput {
    pub variant: Variant,
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseRef {
    pub config_sha256: String,
    pub outcome: GateOutcome,
    pub next_action: Option<normalizer::NextAction>,
    pub proposed_variant: Option<Variant>,
    pub limitation_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triage {
    pub candidate: bool,
    pub estimated_snr_db: f64,
    pub noise_floor_dbfs: f64,
    pub noise_floor_method: normalizer::NoiseFloorMethod,
    pub duration_ms: f64,
    /// Измеренные признаки, по которым запись стала кандидатом.
    pub signals: Vec<String>,
}

/// Обязательная проверка Enhancement QC. `resolved: false` — величину нельзя
/// корректно вычислить; это отказ, а не пропуск. `samples: 0` с
/// `resolved: true` — проверяемого класса окон нет (проверка выполнена и
/// вакуумно пройдена).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementCheck {
    pub id: String,
    pub passed: bool,
    pub resolved: bool,
    pub value: Option<f64>,
    pub limit: Option<f64>,
    pub samples: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementQc {
    pub passed: bool,
    pub checks: Vec<EnhancementCheck>,
    pub noise_floor_before_dbfs: f64,
    pub noise_floor_after_dbfs: f64,
    pub loudness_before_lufs: Option<f64>,
    pub loudness_after_lufs: Option<f64>,
    pub band_change_db: Vec<BandChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BandChange {
    pub low_hz: f64,
    pub high_hz: f64,
    pub change_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRecord {
    pub source_sample_rate: u32,
    pub backend_sample_rate: u32,
    /// Ресэмплинг в частоту backend и обратно выполнялся (явные шаги libswresample).
    pub resampled: bool,
    pub channels: u32,
    pub run: InferenceRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementGate {
    pub outcome: EnhancementOutcome,
    pub selected: Option<Selection>,
    pub next_action: EnhancementAction,
    pub reasons: Vec<Reason>,
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhancementResult {
    pub schema_version: u32,
    pub config_sha256: String,
    pub profile: normalizer::ProfileRef,
    pub backend: BackendIdentity,
    pub base: BaseRef,
    /// Исходный неизменяемый MP3.
    pub input: InputRef,
    pub selected_input: Option<SelectedInput>,
    pub triage: Option<Triage>,
    pub inference: Option<InferenceRecord>,
    pub qc: Option<EnhancementQc>,
    /// Balanced над улучшенным PCM: план, одно кодирование и пост-QC.
    pub balanced: Option<NormalizationResult>,
    /// Улучшенный MP3 (выбранный или отклонённый — как диагностика).
    pub output: Option<OutputRef>,
    pub gate: EnhancementGate,
}

/// Результат и байты улучшенного MP3 (не сериализуются).
#[derive(Debug)]
pub struct EnhancedItem {
    pub result: EnhancementResult,
    pub output_bytes: Option<Vec<u8>>,
}

fn has_limitation(reasons: &[Reason]) -> bool {
    reasons.iter().any(|r| r.severity == Severity::Limitation)
}

/// Действие однозначно следует из исхода и наличия ограничений.
pub fn action_for(outcome: EnhancementOutcome, limited: bool) -> EnhancementAction {
    match outcome {
        EnhancementOutcome::InputRejected => EnhancementAction::DiscardInput,
        EnhancementOutcome::Deferred => EnhancementAction::DeferredAutomaticProcessing,
        _ if limited => EnhancementAction::DeferredAutomaticProcessing,
        EnhancementOutcome::Enhanced => EnhancementAction::UseEnhanced,
        EnhancementOutcome::NotNeeded | EnhancementOutcome::EnhancementRejected => {
            EnhancementAction::UseInput
        }
    }
}

impl EnhancementResult {
    /// Граница доверия для прочитанного или собранного результата: исход,
    /// выбор, действие и наличие фактических данных согласованы.
    pub fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != RESULT_SCHEMA_VERSION {
            return Err(inconsistent("неподдерживаемая схема результата улучшения"));
        }
        let g = &self.gate;
        let limited = has_limitation(&g.reasons);
        let qc_passed = self.qc.as_ref().is_some_and(|q| q.passed);
        let balanced_accepted = self.balanced.as_ref().is_some_and(|b| {
            b.validate().is_ok()
                && b.gate.proposed_variant == Some(Variant::Processed)
                && b.post_qc.as_ref().is_some_and(|q| q.passed)
                && b.output.as_ref().map(|o| &o.sha256) == self.output.as_ref().map(|o| &o.sha256)
        });
        let balanced_consistent = self
            .balanced
            .as_ref()
            .is_none_or(|b| b.validate().is_ok() && b.input.sha256 == self.input.sha256);
        let ok = match g.outcome {
            EnhancementOutcome::Enhanced => {
                g.selected == Some(Selection::Enhanced)
                    && self.inference.is_some()
                    && qc_passed
                    && balanced_accepted
                    && self.output.is_some()
                    && self.selected_input.is_some()
            }
            EnhancementOutcome::NotNeeded => {
                g.selected == Some(Selection::Input)
                    && self.triage.as_ref().is_some_and(|t| !t.candidate)
                    && self.inference.is_none()
                    && self.qc.is_none()
                    && self.balanced.is_none()
                    && self.output.is_none()
                    && self.selected_input.is_some()
            }
            EnhancementOutcome::EnhancementRejected => {
                g.selected == Some(Selection::Input)
                    && self.triage.as_ref().is_some_and(|t| t.candidate)
                    && !balanced_accepted
                    && self.selected_input.is_some()
                    && limited
            }
            EnhancementOutcome::Deferred => {
                g.selected == Some(Selection::Input)
                    && self.inference.is_none()
                    && self.output.is_none()
                    && self.selected_input.is_some()
                    && limited
            }
            EnhancementOutcome::InputRejected => {
                g.selected.is_none()
                    && self.base.outcome == GateOutcome::Reject
                    && self.selected_input.is_none()
                    && self.inference.is_none()
                    && self.output.is_none()
            }
        };
        if !ok
            || !balanced_consistent
            || g.next_action != action_for(g.outcome, limited)
            || g.reasons.iter().any(|r| r.severity == Severity::Review)
        {
            return Err(inconsistent(format!(
                "решение Enhancement QualityGate противоречиво: исход {}, выбор {:?}, действие {}",
                g.outcome.as_str(),
                g.selected,
                g.next_action.as_str()
            )));
        }
        Ok(())
    }
}

fn reason(code: &str, severity: Severity, message: impl Into<String>) -> Reason {
    Reason {
        code: code.into(),
        severity,
        message: message.into(),
        origin: None,
    }
}

fn limitation(code: &str, origin: ReasonOrigin, message: impl Into<String>) -> Reason {
    Reason {
        origin: Some(origin),
        ..reason(code, Severity::Limitation, message)
    }
}

fn unresolved() -> Vec<String> {
    vec![
        "phonetic_content_not_verified: соответствие чтению, фонетике и тональному акценту этап не проверяет".into(),
        "enhancement_qc_heuristic: сохранность слабых звуков, оглушённых гласных, фрикативов и пауз っ оценена по уровням окон и полосам, без ASR и без эталона; это эвристика, а не доказательство".into(),
    ]
}

fn sorted(mut reasons: Vec<Reason>) -> Vec<Reason> {
    reasons.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.code.cmp(&b.code)));
    reasons.dedup_by(|a, b| a.code == b.code);
    reasons
}

// ---------------------------------------------------------------------------
// Триаж
// ---------------------------------------------------------------------------

/// Решение о кандидате по измерениям исходника и плану Balanced (без имён
/// файлов и SHA корпуса).
pub fn triage(
    analysis: &normalizer::SignalAnalysis,
    base: &NormalizationResult,
    profile: &EnhancementProfile,
) -> Triage {
    let t = &profile.triage;
    let snr = analysis.speech_level_dbfs - analysis.noise_floor_dbfs;
    let mut signals = Vec::new();
    if snr < t.max_estimated_snr_db {
        signals.push(format!(
            "estimated_snr_below_threshold: {snr:.1} dB < {:.1} dB",
            t.max_estimated_snr_db
        ));
    }
    if analysis.noise_floor_dbfs > t.min_noise_floor_dbfs {
        signals.push(format!(
            "noise_floor_above_threshold: {:.1} dBFS > {:.1} dBFS",
            analysis.noise_floor_dbfs, t.min_noise_floor_dbfs
        ));
    }
    let noise_limited_gain = base
        .plan
        .as_ref()
        .is_some_and(|p| p.gain.limited_by.contains(&GainLimit::NoiseFloor));
    if noise_limited_gain {
        signals.push("balanced_gain_limited_by_noise_floor".into());
    }
    Triage {
        candidate: !signals.is_empty(),
        estimated_snr_db: snr,
        noise_floor_dbfs: analysis.noise_floor_dbfs,
        noise_floor_method: analysis.noise_floor_method,
        duration_ms: analysis.duration_ms,
        signals,
    }
}

// ---------------------------------------------------------------------------
// Enhancement QC
// ---------------------------------------------------------------------------

/// Полосовой биквад RBJ (пиковое усиление 0 dB).
fn bandpass(samples: &[f32], channels: usize, rate: f64, low: f64, high: f64) -> Vec<f64> {
    let center = (low * high).sqrt();
    let q = center / (high - low);
    let w0 = std::f64::consts::TAU * center / rate;
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    let (b0, b2) = (alpha / a0, -alpha / a0);
    let (a1, a2) = (-2.0 * w0.cos() / a0, (1.0 - alpha) / a0);
    let mut out = vec![0.0; samples.len()];
    for c in 0..channels {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for i in (c..samples.len()).step_by(channels) {
            let x = f64::from(samples[i]);
            let y = b0 * x + b2 * x2 - a1 * y1 - a2 * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y;
            out[i] = y;
        }
    }
    out
}

/// Уровень, ниже которого добавленная энергия не учитывается.
const INAUDIBLE_DBFS: f64 = -90.0;

/// Отделённая активность не тише «речь минус это» считается возможной
/// частью слова.
const SEPARATED_ACTIVITY_MARGIN_DB: f64 = 25.0;

fn median(values: &[f64]) -> Option<f64> {
    dsp::percentile(values, 0.5)
}

struct QcInput<'a> {
    original: &'a Pcm,
    enhanced: &'a Pcm,
    original_analysis: &'a normalizer::SignalAnalysis,
    enhanced_analysis: &'a normalizer::SignalAnalysis,
}

fn check(
    id: &str,
    value: Option<f64>,
    limit: f64,
    samples: usize,
    pass: impl Fn(f64) -> bool,
) -> EnhancementCheck {
    match value {
        Some(v) if v.is_finite() => EnhancementCheck {
            id: id.into(),
            passed: pass(v),
            resolved: true,
            value: Some(v),
            limit: Some(limit),
            samples,
        },
        _ if samples == 0 => EnhancementCheck {
            id: id.into(),
            passed: true,
            resolved: true,
            value: None,
            limit: Some(limit),
            samples: 0,
        },
        _ => EnhancementCheck {
            id: id.into(),
            passed: false,
            resolved: false,
            value: None,
            limit: Some(limit),
            samples,
        },
    }
}

fn unresolved_check(id: &str, limit: f64) -> EnhancementCheck {
    EnhancementCheck {
        id: id.into(),
        passed: false,
        resolved: false,
        value: None,
        limit: Some(limit),
        samples: 0,
    }
}

/// Порядок обязательных проверок Enhancement QC: каждая всегда присутствует.
pub const QC_CHECKS: [&str; 13] = [
    "length_preserved",
    "alignment_lag",
    "noise_reduction",
    "strong_speech_preserved",
    "strong_speech_p90",
    "no_segment_loss",
    "weak_sounds_preserved",
    "edges_preserved",
    "separated_activity_preserved",
    "no_added_energy",
    "spectral_balance",
    "loudness_preserved",
    "no_new_clipping",
];

fn enhancement_qc(input: &QcInput<'_>, profile: &EnhancementProfile) -> EnhancementQc {
    let g = &profile.gate;
    let (orig, enh) = (input.original, input.enhanced);
    let comparable = orig.sample_rate == enh.sample_rate
        && orig.channels == enh.channels
        && orig.frames() == enh.frames()
        && orig.frames() > 0;
    let frame = orig.samples_for_ms(10.0).max(1);
    let window_ms = frame as f64 * 1000.0 / f64::from(orig.sample_rate.max(1));
    let a = dsp::frame_levels(orig, frame);
    let b = dsp::frame_levels(enh, frame);
    let noise_before = dsp::percentile(&a, 0.10).unwrap_or(dsp::LEVEL_FLOOR_DB);
    let noise_after = dsp::percentile(&b, 0.10).unwrap_or(dsp::LEVEL_FLOOR_DB);
    let loudness = |an: &normalizer::SignalAnalysis| an.integrated_lufs.or(an.short_window_lufs);
    let (loud_before, loud_after) = (
        loudness(input.original_analysis),
        loudness(input.enhanced_analysis),
    );
    let mut bands = Vec::new();
    if !comparable {
        let checks = QC_CHECKS
            .iter()
            .map(|id| unresolved_check(id, 0.0))
            .collect();
        return EnhancementQc {
            passed: false,
            checks,
            noise_floor_before_dbfs: noise_before,
            noise_floor_after_dbfs: noise_after,
            loudness_before_lufs: loud_before,
            loudness_after_lufs: loud_after,
            band_change_db: bands,
        };
    }
    let speech = dsp::percentile(&a, 0.95).unwrap_or(dsp::LEVEL_FLOOR_DB);
    let drops: Vec<f64> = a.iter().zip(&b).map(|(x, y)| x - y).collect();
    // Сильная речь — окна у уровня речи и заметно выше шума: при шуме в
    // пределах `strong_window_margin_db` от речи окна чистого шума иначе
    // считались бы речью, и их законное удаление выглядело бы потерей.
    // При SNR ниже `weak_window_above_noise_db` таких окон нет, сохранность
    // доказать нечем, и проверки неразрешимы.
    let above_noise = noise_before + g.weak_window_above_noise_db;
    let strong: Vec<usize> = (0..a.len())
        .filter(|&i| {
            a[i] >= speech - g.strong_window_margin_db
                && a[i] >= above_noise
                && a[i] > dsp::LEVEL_FLOOR_DB
        })
        .collect();
    let strong_drops: Vec<f64> = strong.iter().map(|&i| drops[i]).collect();
    let mut checks = Vec::new();
    checks.push(check("length_preserved", Some(0.0), 0.0, 1, |v| v == 0.0));
    let max_lag = orig.samples_for_ms(10.0);
    let lag_ms = dsp::aligned_snr(orig, enh, max_lag)
        .map(|al| al.lag_frames.unsigned_abs() as f64 * 1000.0 / f64::from(orig.sample_rate));
    checks.push(check(
        "alignment_lag",
        lag_ms,
        g.max_alignment_lag_ms,
        1,
        |v| v <= g.max_alignment_lag_ms,
    ));
    checks.push(check(
        "noise_reduction",
        Some(noise_before - noise_after),
        g.min_noise_reduction_db,
        a.len(),
        |v| v >= g.min_noise_reduction_db,
    ));
    // Без сильной речи сохранность нечем подтвердить: проверка неразрешима.
    let strong_median = median(&strong_drops);
    checks.push(if strong.is_empty() {
        unresolved_check("strong_speech_preserved", g.max_strong_drop_median_db)
    } else {
        check(
            "strong_speech_preserved",
            strong_median,
            g.max_strong_drop_median_db,
            strong.len(),
            |v| v <= g.max_strong_drop_median_db,
        )
    });
    checks.push(if strong.is_empty() {
        unresolved_check("strong_speech_p90", g.max_strong_drop_p90_db)
    } else {
        check(
            "strong_speech_p90",
            dsp::percentile(&strong_drops, 0.90),
            g.max_strong_drop_p90_db,
            strong.len(),
            |v| v <= g.max_strong_drop_p90_db,
        )
    });
    let speech_floor = (speech - 20.0).max(above_noise);
    let (mut run, mut longest) = (0usize, 0usize);
    for i in 0..a.len() {
        if a[i] >= speech_floor && drops[i] > g.segment_loss_drop_db {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let speech_windows = a.iter().filter(|l| **l >= speech_floor).count();
    checks.push(check(
        "no_segment_loss",
        Some(longest as f64 * window_ms),
        g.max_segment_loss_ms,
        speech_windows,
        |v| v <= g.max_segment_loss_ms,
    ));
    let context = (g.weak_context_ms / window_ms).ceil() as usize;
    let near_strong = |i: usize| strong.iter().any(|&s| s.abs_diff(i) <= context && s != i);
    let weak_floor = noise_before + g.weak_window_above_noise_db;
    let weak_drops: Vec<f64> = (0..a.len())
        .filter(|&i| {
            a[i] >= weak_floor && a[i] < speech - g.strong_window_margin_db && near_strong(i)
        })
        .map(|i| drops[i])
        .collect();
    checks.push(check(
        "weak_sounds_preserved",
        median(&weak_drops),
        g.max_weak_drop_median_db,
        weak_drops.len(),
        |v| v <= g.max_weak_drop_median_db,
    ));
    // Края слова — границы непрерывной звучащей области вокруг сильной
    // речи (от первого сильного окна назад и от последнего вперёд, пока
    // окно выше шума на `weak_window_above_noise_db`): здесь начальные
    // согласные и хвосты слов. Значение — худшая из медиан падения первых и
    // последних `edge_windows` окон области.
    let (region_start, region_end) = match (strong.first(), strong.last()) {
        (Some(&first), Some(&last)) => {
            let (mut s0, mut s1) = (first, last);
            while s0 > 0 && a[s0 - 1] >= weak_floor {
                s0 -= 1;
            }
            while s1 + 1 < a.len() && a[s1 + 1] >= weak_floor {
                s1 += 1;
            }
            (s0, s1)
        }
        _ => (1, 0),
    };
    checks.push(if region_start > region_end {
        unresolved_check("edges_preserved", g.max_edge_drop_db)
    } else {
        let n = g.edge_windows.min(region_end - region_start + 1);
        let lead: Vec<f64> = (region_start..region_start + n).map(|i| drops[i]).collect();
        let trail: Vec<f64> = (region_end + 1 - n..=region_end)
            .map(|i| drops[i])
            .collect();
        let worst = match (median(&lead), median(&trail)) {
            (Some(x), Some(y)) => Some(x.max(y)),
            _ => None,
        };
        check(
            "edges_preserved",
            worst,
            g.max_edge_drop_db,
            lead.len() + trail.len(),
            |v| v <= g.max_edge_drop_db,
        )
    });
    // Отделённая тишиной активность вне области слова. Тихую (вдох, щелчок,
    // ниже «речь минус 25 dB») модель гасит законно; сопоставимую с речью
    // — возможно, часть слова — удалять нельзя.
    let separated: Vec<f64> = (0..a.len())
        .filter(|&i| {
            (i < region_start || i > region_end)
                && a[i] >= weak_floor
                && a[i] >= speech - SEPARATED_ACTIVITY_MARGIN_DB
        })
        .map(|i| drops[i])
        .collect();
    checks.push(check(
        "separated_activity_preserved",
        separated.iter().copied().reduce(f64::max),
        g.segment_loss_drop_db,
        separated.len(),
        |v| v <= g.segment_loss_drop_db,
    ));
    // Тихие окна вплотную к звуку (два окна, 20 мс) не учитываются: окно
    // STFT модели законно переносит туда часть энергии соседнего звука.
    let quiet_limit = noise_before + g.quiet_window_above_noise_db;
    let near_sound =
        |i: usize| (i.saturating_sub(2)..=(i + 2).min(a.len() - 1)).any(|j| a[j] > quiet_limit);
    // Рост считается от уровня окна, но не ниже шумового пола исходника и
    // −90 dBFS: энергия ниже уже существовавшего шума (например, в окнах
    // цифровой тишины) неслышима и «галлюцинацией» не является.
    let quiet_rise: Vec<f64> = (0..a.len())
        .filter(|&i| a[i] <= quiet_limit && !near_sound(i))
        .map(|i| b[i] - a[i].max(noise_before).max(INAUDIBLE_DBFS))
        .collect();
    checks.push(check(
        "no_added_energy",
        dsp::percentile(&quiet_rise, 0.95),
        g.max_quiet_rise_p95_db,
        quiet_rise.len(),
        |v| v <= g.max_quiet_rise_p95_db,
    ));
    // Энергия полос в окнах сильной речи: большое ослабление речевой части
    // полосы — признак потери согласных/фрикативов, подъём — артефактов.
    // Шум в полосе законно уходит, поэтому сравнивается не энергия полосы
    // исходника, а её речевая часть: мощность полосы в окнах сильной речи
    // минус мощность той же полосы в тихих окнах (оценка шума). Полоса, где
    // речь не выше шума (SNR полосы < 0 dB) или ниже −70 dBFS, не оценивается.
    let rate = f64::from(orig.sample_rate);
    let quiet_windows: Vec<usize> = (0..a.len()).filter(|&i| a[i] <= quiet_limit).collect();
    let band_power = |x: &[f64], windows: &[usize]| -> Option<f64> {
        let (mut sum, mut count) = (0.0, 0usize);
        for &w in windows {
            let s = w * frame * orig.channels;
            let e = ((w + 1) * frame * orig.channels).min(x.len());
            sum += x[s..e].iter().map(|v| v * v).sum::<f64>();
            count += e - s;
        }
        (count > 0).then(|| sum / count as f64)
    };
    let mut worst_attenuation = None::<f64>;
    let mut worst_boost = None::<f64>;
    for (low, high) in [
        (150.0, 500.0),
        (500.0, 2000.0),
        (2000.0, 5000.0),
        (5000.0, 9000.0),
    ] {
        if high > 0.45 * rate {
            continue;
        }
        let fa = bandpass(&orig.samples, orig.channels, rate, low, high);
        let fb = bandpass(&enh.samples, enh.channels, rate, low, high);
        let noise = band_power(&fa, &quiet_windows).unwrap_or(0.0);
        let change = match (band_power(&fa, &strong), band_power(&fb, &strong)) {
            (Some(pa), Some(pb)) => {
                let speech = pa - noise;
                (speech > noise && dsp::power_db(speech) > -70.0)
                    .then(|| dsp::power_db(pb) - dsp::power_db(speech))
            }
            _ => None,
        };
        if let Some(c) = change {
            worst_attenuation = Some(worst_attenuation.map_or(-c, |w: f64| w.max(-c)));
            worst_boost = Some(worst_boost.map_or(c, |w: f64| w.max(c)));
        }
        bands.push(BandChange {
            low_hz: low,
            high_hz: high,
            change_db: change,
        });
    }
    let spectral = match (worst_attenuation, worst_boost) {
        (Some(att), Some(boost)) => EnhancementCheck {
            id: "spectral_balance".into(),
            passed: att <= g.max_band_attenuation_db && boost <= g.max_band_boost_db,
            resolved: true,
            value: Some(att),
            limit: Some(g.max_band_attenuation_db),
            samples: bands.iter().filter(|b| b.change_db.is_some()).count(),
        },
        _ => unresolved_check("spectral_balance", g.max_band_attenuation_db),
    };
    checks.push(spectral);
    checks.push(match (loud_before, loud_after) {
        (Some(x), Some(y)) => check(
            "loudness_preserved",
            Some(x - y),
            g.max_loudness_drop_db,
            1,
            |v| v <= g.max_loudness_drop_db,
        ),
        _ => unresolved_check("loudness_preserved", g.max_loudness_drop_db),
    });
    let new_clipping = input.enhanced_analysis.clipping.near_full_scale_samples
        > input.original_analysis.clipping.near_full_scale_samples;
    if new_clipping {
        checks.push(EnhancementCheck {
            id: "no_new_clipping".into(),
            passed: false,
            resolved: true,
            value: Some(input.enhanced_analysis.clipping.near_full_scale_samples as f64),
            limit: Some(input.original_analysis.clipping.near_full_scale_samples as f64),
            samples: 1,
        });
    } else {
        checks.push(check("no_new_clipping", Some(0.0), 0.0, 1, |_| true));
    }
    EnhancementQc {
        passed: checks.iter().all(|c| c.passed && c.resolved),
        checks,
        noise_floor_before_dbfs: noise_before,
        noise_floor_after_dbfs: noise_after,
        loudness_before_lufs: loud_before,
        loudness_after_lufs: loud_after,
        band_change_db: bands,
    }
}

fn qc_reasons(qc: &EnhancementQc) -> Vec<Reason> {
    qc.checks
        .iter()
        .filter(|c| !c.passed || !c.resolved)
        .map(|c| {
            let code = if c.id == "noise_reduction" {
                "enhancement_benefit_insufficient".to_owned()
            } else {
                format!("enhancement_qc_{}", c.id)
            };
            limitation(
                &code,
                ReasonOrigin::EnhancementQc,
                if !c.resolved {
                    "проверка нейроварианта неразрешима; вариант отклонён, сохранён вход".to_owned()
                } else if c.id == "noise_reduction" {
                    "модель не дала измеримого снижения шума; вариант отклонён, шум передан следующей автоматической стадии".to_owned()
                } else {
                    "нейровариант не прошёл проверку сохранности; вариант отклонён, сохранён вход".to_owned()
                },
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Один вход
// ---------------------------------------------------------------------------

/// Всё, что нужно для обработки входа; одинаково для всех входов пакета.
pub struct ItemContext<'a> {
    pub profile: &'a EnhancementProfile,
    pub backend: &'a BackendIdentity,
    pub worker: &'a std::path::Path,
    pub model_dir: &'a std::path::Path,
    pub base_profile: &'a NormalizerProfile,
    pub base_toolchain: &'a Toolchain,
    pub config_sha256: &'a str,
}

fn is_audio_infrastructure(error: &AssetError) -> bool {
    normalizer::is_infrastructure_failure(error) || backend::is_backend_failure(error)
}

/// Сбой среды (FFmpeg или backend) — ошибка для повтора.
pub fn is_infrastructure_failure(error: &AssetError) -> bool {
    is_audio_infrastructure(error)
}

/// Полный путь одного входа. `Err` — только сбой среды или нарушение
/// контракта; любое свойство сигнала становится объяснимым исходом.
pub fn process_item(
    ctx: &ItemContext<'_>,
    original: &[u8],
    base: &NormalizationResult,
    selected_bytes: Option<&[u8]>,
) -> Result<EnhancedItem, AssetError> {
    ctx.profile.validate()?;
    base.validate()?;
    if base.schema_version != normalizer::RESULT_SCHEMA_VERSION
        || base.mode != normalizer::Mode::Normalize
    {
        return Err(invalid(
            "улучшению нужен результат Balanced текущей схемы в режиме normalize",
        ));
    }
    let input_sha = sha256_hex(original);
    if input_sha != base.input.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "исходник не совпадает с результатом Balanced",
        ));
    }
    let selected_input = match (base.gate.proposed_variant, selected_bytes) {
        (None, _) => None,
        (Some(variant), Some(bytes)) => {
            let expected = match variant {
                Variant::Original => base.input.sha256.as_str(),
                Variant::Processed => base
                    .output
                    .as_ref()
                    .map(|o| o.sha256.as_str())
                    .unwrap_or(""),
            };
            if sha256_hex(bytes) != expected {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "выбранные Balanced байты не совпали со своим SHA",
                ));
            }
            Some(SelectedInput {
                variant,
                sha256: expected.into(),
                byte_length: bytes.len() as u64,
            })
        }
        (Some(_), None) => return Err(invalid("нет байтов выбранного Balanced варианта")),
    };
    let base_limitations: Vec<Reason> = base
        .gate
        .reasons
        .iter()
        .filter(|r| r.severity == Severity::Limitation)
        .cloned()
        .collect();
    let mut result = EnhancementResult {
        schema_version: RESULT_SCHEMA_VERSION,
        config_sha256: ctx.config_sha256.into(),
        profile: normalizer::ProfileRef {
            id: ctx.profile.id.clone(),
            version: ctx.profile.version,
        },
        backend: ctx.backend.clone(),
        base: BaseRef {
            config_sha256: base.config_sha256.clone(),
            outcome: base.gate.outcome,
            next_action: base.gate.next_action,
            proposed_variant: base.gate.proposed_variant,
            limitation_codes: base_limitations.iter().map(|r| r.code.clone()).collect(),
        },
        input: InputRef {
            sha256: input_sha,
            byte_length: original.len() as u64,
            probe: None,
            error: None,
        },
        selected_input,
        triage: None,
        inference: None,
        qc: None,
        balanced: None,
        output: None,
        gate: EnhancementGate {
            outcome: EnhancementOutcome::InputRejected,
            selected: None,
            next_action: EnhancementAction::DiscardInput,
            reasons: Vec::new(),
            unresolved: unresolved(),
        },
    };
    let finish = |mut result: EnhancementResult,
                  outcome: EnhancementOutcome,
                  reasons: Vec<Reason>,
                  output_bytes: Option<Vec<u8>>| {
        let reasons = sorted(reasons);
        let limited = has_limitation(&reasons);
        result.gate.outcome = outcome;
        result.gate.selected = match outcome {
            EnhancementOutcome::InputRejected => None,
            EnhancementOutcome::Enhanced => Some(Selection::Enhanced),
            _ => Some(Selection::Input),
        };
        result.gate.next_action = action_for(outcome, limited);
        result.gate.reasons = reasons;
        EnhancedItem {
            result,
            output_bytes,
        }
    };
    if base.gate.outcome == GateOutcome::Reject {
        let reasons = vec![reason(
            "input_rejected_by_balanced",
            Severity::Info,
            "Balanced признал вход технически непригодным; улучшение не применяется",
        )];
        return Ok(finish(
            result,
            EnhancementOutcome::InputRejected,
            reasons,
            None,
        ));
    }
    let probe = probe::probe_mp3(original)?;
    let samples = probe::decode_f32(original, &probe)?;
    result.input.probe = Some(probe.clone());
    let pcm = Pcm {
        sample_rate: probe.sample_rate,
        channels: probe.channels as usize,
        samples,
    };
    let analysis = normalizer::analyze_pcm(&pcm, ctx.base_profile);
    let triage = triage(&analysis, base, ctx.profile);
    let candidate = triage.candidate;
    let duration_ms = triage.duration_ms;
    let unreliable = analysis.noise_floor_method == normalizer::NoiseFloorMethod::AllWindowsP10;
    result.triage = Some(triage);
    let mut reasons = base_limitations.clone();
    if unreliable {
        reasons.push(reason(
            "noise_estimate_unreliable",
            Severity::Warning,
            "пауз почти нет: шумовой пол оценён p10 всех окон и может быть завышен",
        ));
    }
    if !candidate {
        reasons.push(reason(
            "enhancement_not_needed",
            Severity::Info,
            "по измерениям шум не требует улучшения: вход оставлен без изменений",
        ));
        return Ok(finish(result, EnhancementOutcome::NotNeeded, reasons, None));
    }
    if duration_ms < ctx.profile.triage.min_duration_ms || pcm.channels > 2 {
        reasons.push(limitation(
            "enhancement_not_applicable",
            ReasonOrigin::EnhancementPolicy,
            format!(
                "запись шумная, но модель к ней сейчас не применяется (длительность {duration_ms:.0} мс, каналов {}): передано следующей автоматической стадии",
                pcm.channels
            ),
        ));
        return Ok(finish(result, EnhancementOutcome::Deferred, reasons, None));
    }
    // Явный ресэмплинг в частоту модели и обратно тем же libswresample.
    let resampled = pcm.sample_rate != backend::BACKEND_SAMPLE_RATE;
    let at_backend = if resampled {
        probe::resample_f32(
            &pcm.samples,
            pcm.sample_rate,
            pcm.channels as u32,
            backend::BACKEND_SAMPLE_RATE,
        )?
    } else {
        pcm.samples.clone()
    };
    let (enhanced_48k, run) = backend::enhance(
        ctx.worker,
        ctx.model_dir,
        ctx.backend,
        pcm.channels,
        &at_backend,
        std::time::Duration::from_secs(ctx.profile.backend.timeout_seconds),
    )?;
    let mut enhanced = if resampled {
        probe::resample_f32(
            &enhanced_48k,
            backend::BACKEND_SAMPLE_RATE,
            pcm.channels as u32,
            pcm.sample_rate,
        )?
    } else {
        enhanced_48k
    };
    result.inference = Some(InferenceRecord {
        source_sample_rate: pcm.sample_rate,
        backend_sample_rate: backend::BACKEND_SAMPLE_RATE,
        resampled,
        channels: pcm.channels as u32,
        run,
    });
    // Обратный ресэмплинг может дать на кадр больше или меньше из-за
    // округления: длина приводится к исходной явно, расхождение больше
    // кадра — нарушение контракта длины.
    let expected = pcm.samples.len();
    let length_error_frames = enhanced.len().abs_diff(expected) / pcm.channels;
    enhanced.resize(expected, 0.0);
    let enhanced = Pcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples: enhanced,
    };
    let enhanced_analysis = normalizer::analyze_pcm(&enhanced, ctx.base_profile);
    let mut qc = enhancement_qc(
        &QcInput {
            original: &pcm,
            enhanced: &enhanced,
            original_analysis: &analysis,
            enhanced_analysis: &enhanced_analysis,
        },
        ctx.profile,
    );
    if let Some(c) = qc.checks.iter_mut().find(|c| c.id == "length_preserved") {
        c.value = Some(length_error_frames as f64);
        c.limit = Some(1.0);
        c.passed = length_error_frames <= 1;
    }
    qc.passed = qc.checks.iter().all(|c| c.passed && c.resolved);
    let qc_failed = qc_reasons(&qc);
    let qc_passed = qc.passed;
    result.qc = Some(qc);
    if !qc_passed {
        reasons.extend(qc_failed);
        return Ok(finish(
            result,
            EnhancementOutcome::EnhancementRejected,
            reasons,
            None,
        ));
    }
    let processed = normalizer::balance_enhanced(
        result.input.clone(),
        &pcm,
        &enhanced,
        ctx.base_profile,
        ctx.base_toolchain,
    )?;
    let accepted = processed.result.gate.proposed_variant == Some(Variant::Processed);
    result.output = processed.result.output.clone();
    let output_bytes = processed.output_bytes;
    let balanced_reasons = processed.result.gate.reasons.clone();
    result.balanced = Some(processed.result);
    if accepted {
        let mut reasons: Vec<Reason> = balanced_reasons;
        if unreliable {
            reasons.push(reason(
                "noise_estimate_unreliable",
                Severity::Warning,
                "пауз почти нет: шумовой пол оценён p10 всех окон и может быть завышен",
            ));
        }
        let kind = ctx.profile.backend.kind;
        reasons.push(reason(
            kind.enhanced_reason(),
            Severity::Info,
            format!(
                "нейровариант {} прошёл Enhancement QC и пост-QC Balanced; одно кодирование из исходного PCM",
                kind.model_name()
            ),
        ));
        return Ok(finish(
            result,
            EnhancementOutcome::Enhanced,
            reasons,
            output_bytes,
        ));
    }
    reasons.push(limitation(
        "enhancement_balanced_post_qc_failed",
        ReasonOrigin::PostQc,
        "кодированный нейровариант не прошёл пост-QC Balanced; сохранён вход",
    ));
    Ok(finish(
        result,
        EnhancementOutcome::EnhancementRejected,
        reasons,
        output_bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 44_100;

    /// Детерминированный шум (xorshift), амплитуда `level`.
    fn noise(len: usize, seed: u64, level: f32) -> Vec<f32> {
        let mut x = seed.max(1);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * level
            })
            .collect()
    }

    #[derive(Clone, Copy)]
    enum Seg {
        Silence,
        Vowel,
        Fricative,
    }

    /// Чистый эталон: гласные (гармоники), фрикатив (высокочастотный шум
    /// с огибающей), паузы (っ) — с известными границами.
    fn clean(segments: &[(f64, Seg)]) -> Vec<f32> {
        let mut out = Vec::new();
        let mut seed = 17;
        for &(ms, kind) in segments {
            let n = (f64::from(SR) * ms / 1000.0) as usize;
            match kind {
                Seg::Silence => out.extend(std::iter::repeat_n(0.0, n)),
                Seg::Vowel => out.extend((0..n).map(|i| {
                    let t = i as f32 / SR as f32;
                    let env = (std::f32::consts::PI * i as f32 / n as f32).sin();
                    env * (0.2 * (std::f32::consts::TAU * 180.0 * t).sin()
                        + 0.1 * (std::f32::consts::TAU * 720.0 * t).sin()
                        + 0.05 * (std::f32::consts::TAU * 2500.0 * t).sin())
                })),
                Seg::Fricative => {
                    seed += 1;
                    let hiss = noise(n, seed, 0.06);
                    // Простейший фильтр верхних частот: разность соседних отсчётов.
                    let mut prev = 0.0;
                    out.extend(hiss.iter().enumerate().map(|(i, s)| {
                        let env = (std::f32::consts::PI * i as f32 / n as f32).sin();
                        let y = s - prev;
                        prev = *s;
                        env * y
                    }));
                }
            }
        }
        out
    }

    fn pcm(samples: Vec<f32>) -> Pcm {
        Pcm {
            sample_rate: SR,
            channels: 1,
            samples,
        }
    }

    fn word() -> Vec<f32> {
        // «ka-っ-sa»: гласная, пауза っ, фрикатив, гласная.
        clean(&[
            (300.0, Seg::Silence),
            (180.0, Seg::Vowel),
            (90.0, Seg::Silence),
            (110.0, Seg::Fricative),
            (220.0, Seg::Vowel),
            (300.0, Seg::Silence),
        ])
    }

    fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    fn run_qc(original: &[f32], enhanced: &[f32]) -> EnhancementQc {
        let profile = EnhancementProfile::conservative();
        let base = NormalizerProfile::balanced();
        let (o, e) = (pcm(original.to_vec()), pcm(enhanced.to_vec()));
        let (oa, ea) = (
            normalizer::analyze_pcm(&o, &base),
            normalizer::analyze_pcm(&e, &base),
        );
        enhancement_qc(
            &QcInput {
                original: &o,
                enhanced: &e,
                original_analysis: &oa,
                enhanced_analysis: &ea,
            },
            &profile,
        )
    }

    fn failed(qc: &EnhancementQc) -> Vec<&str> {
        qc.checks
            .iter()
            .filter(|c| !c.passed || !c.resolved)
            .map(|c| c.id.as_str())
            .collect()
    }

    #[test]
    fn ideal_denoising_passes_every_mandatory_check() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.006));
        // Идеальная модель вернула бы чистый эталон.
        let qc = run_qc(&noisy, &clean);
        assert!(qc.passed, "{:?}", failed(&qc));
        assert_eq!(qc.checks.len(), QC_CHECKS.len());
    }

    #[test]
    fn copy_of_input_has_no_measurable_benefit() {
        let noisy = add(&word(), &noise(word().len(), 3, 0.006));
        let qc = run_qc(&noisy, &noisy);
        assert_eq!(failed(&qc), vec!["noise_reduction"]);
    }

    #[test]
    fn lost_short_segment_is_detected() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.006));
        let mut damaged = clean.clone();
        // Модель «съела» вторую гласную целиком.
        let start = (f64::from(SR) * 0.68) as usize;
        let end = (f64::from(SR) * 0.90) as usize;
        damaged[start..end].iter_mut().for_each(|s| *s = 0.0);
        let qc = run_qc(&noisy, &damaged);
        assert!(
            failed(&qc).contains(&"no_segment_loss"),
            "{:?}",
            failed(&qc)
        );
    }

    #[test]
    fn suppressed_fricative_is_not_accepted() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.003));
        let mut damaged = clean.clone();
        let start = (f64::from(SR) * 0.57) as usize;
        let end = (f64::from(SR) * 0.68) as usize;
        damaged[start..end].iter_mut().for_each(|s| *s *= 0.05);
        let qc = run_qc(&noisy, &damaged);
        let failed = failed(&qc);
        assert!(!qc.passed);
        assert!(
            failed.iter().any(|f| [
                "weak_sounds_preserved",
                "no_segment_loss",
                "spectral_balance",
                "strong_speech_p90"
            ]
            .contains(f)),
            "{failed:?}"
        );
    }

    #[test]
    fn added_delay_is_rejected() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.006));
        let shift = (f64::from(SR) * 0.005) as usize;
        let mut delayed = vec![0.0; shift];
        delayed.extend_from_slice(&clean[..clean.len() - shift]);
        let qc = run_qc(&noisy, &delayed);
        assert!(failed(&qc).contains(&"alignment_lag"), "{:?}", failed(&qc));
    }

    #[test]
    fn hallucinated_energy_in_silence_is_rejected() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.002));
        let mut out = clean.clone();
        let tone: Vec<f32> = (0..out.len())
            .map(|i| 0.02 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin())
            .collect();
        for (i, s) in out
            .iter_mut()
            .enumerate()
            .take((f64::from(SR) * 0.25) as usize)
        {
            *s += tone[i];
        }
        let qc = run_qc(&noisy, &out);
        assert!(
            failed(&qc).contains(&"no_added_energy"),
            "{:?}",
            failed(&qc)
        );
    }

    #[test]
    fn attenuated_voice_is_rejected_even_with_less_noise() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.006));
        let quiet: Vec<f32> = clean.iter().map(|s| s * 0.5).collect();
        let qc = run_qc(&noisy, &quiet);
        let failed = failed(&qc);
        assert!(failed.contains(&"strong_speech_preserved"), "{failed:?}");
    }

    #[test]
    fn incomparable_output_leaves_every_check_unresolved() {
        let profile = EnhancementProfile::conservative();
        let base = NormalizerProfile::balanced();
        let o = pcm(word());
        let e = pcm(word()[..1000].to_vec());
        let (oa, ea) = (
            normalizer::analyze_pcm(&o, &base),
            normalizer::analyze_pcm(&e, &base),
        );
        let qc = enhancement_qc(
            &QcInput {
                original: &o,
                enhanced: &e,
                original_analysis: &oa,
                enhanced_analysis: &ea,
            },
            &profile,
        );
        assert!(!qc.passed);
        assert!(qc.checks.iter().all(|c| !c.resolved));
        assert_eq!(qc.checks.len(), QC_CHECKS.len());
    }

    #[test]
    fn digital_silence_output_cannot_prove_speech_preservation() {
        let clean = word();
        let noisy = add(&clean, &noise(clean.len(), 3, 0.006));
        let qc = run_qc(&noisy, &vec![0.0; clean.len()]);
        assert!(!qc.passed);
    }

    #[test]
    fn action_follows_outcome_and_limitations() {
        use EnhancementAction as A;
        use EnhancementOutcome as O;
        assert_eq!(action_for(O::NotNeeded, false), A::UseInput);
        assert_eq!(
            action_for(O::NotNeeded, true),
            A::DeferredAutomaticProcessing
        );
        assert_eq!(action_for(O::Enhanced, false), A::UseEnhanced);
        assert_eq!(
            action_for(O::Enhanced, true),
            A::DeferredAutomaticProcessing
        );
        assert_eq!(
            action_for(O::EnhancementRejected, true),
            A::DeferredAutomaticProcessing
        );
        assert_eq!(
            action_for(O::Deferred, false),
            A::DeferredAutomaticProcessing
        );
        assert_eq!(action_for(O::InputRejected, false), A::DiscardInput);
    }

    #[test]
    fn default_profile_is_valid_and_strict() {
        let profile = EnhancementProfile::conservative();
        profile.validate().unwrap();
        let mut bad = profile.clone();
        bad.gate.min_noise_reduction_db = f64::NAN;
        assert!(bad.validate().is_err());
        let json = serde_json::to_value(&profile).unwrap();
        let mut extra = json.clone();
        extra["unexpected"] = serde_json::json!(1);
        assert!(serde_json::from_value::<EnhancementProfile>(extra).is_err());
    }
}
