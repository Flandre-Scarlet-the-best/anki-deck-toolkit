//! Balanced Normalizer: анализ MP3, план бережной обработки и технический QualityGate.
//!
//! Обработка ограничена тремя операциями: удаление доказанно избыточной тишины
//! только по краям, одно линейное усиление и, по явной политике, сведение
//! идентичных каналов в моно. Внутренние паузы, темп, высота тона, динамика
//! и спектр не меняются. Итог проверяется по фактически декодированному
//! результату. `ACCEPT` означает техническую пригодность варианта, а не
//! правильность чтения: фонетику без человека этот модуль не оценивает.

use serde::{Deserialize, Serialize};

use super::dsp::{self, Pcm};
use super::probe::{self, AudioProbe};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const RESULT_SCHEMA_VERSION: u32 = 1;

fn invalid_profile(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

/// Версионированный профиль. Любое изменение значения меняет отпечаток
/// конфигурации, поэтому результаты другого профиля не переиспользуются.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizerProfile {
    pub schema_version: u32,
    pub id: String,
    pub version: u32,
    pub analysis: AnalysisPolicy,
    pub trim: TrimPolicy,
    pub loudness: LoudnessPolicy,
    pub output: OutputPolicy,
    pub gate: GatePolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisPolicy {
    /// Окно уровней для поиска тишины, мс.
    pub frame_ms: f64,
    /// Ниже этого максимального уровня окна сигнал считается отсутствующим.
    pub signal_floor_dbfs: f64,
    /// Записи короче требуют прослушивания: громкость и границы ненадёжны.
    pub min_duration_ms: f64,
    /// Внутренние паузы не короче этого значения учитываются в диагностике.
    pub internal_pause_report_ms: f64,
    /// Внутренняя пауза длиннее — предупреждение (возможны два слова/дубль).
    pub long_internal_pause_ms: f64,
    /// Постоянная составляющая выше — предупреждение.
    pub dc_offset_warning: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrimPolicy {
    pub enabled: bool,
    /// Порог речи: окна не ниже считаются уверенным сигналом.
    pub silence_threshold_dbfs: f64,
    /// Окна в пределах этого запаса ниже порога считаются возможным слабым
    /// звуком (глухие согласные, придыхание) и никогда не удаляются.
    pub weak_sound_margin_db: f64,
    /// Край короче не обрезается вовсе.
    pub min_edge_silence_ms: f64,
    /// Сохраняемый запас тишины перед первым возможным звуком.
    pub guard_leading_ms: f64,
    /// Сохраняемый запас после последнего возможного звука.
    pub guard_trailing_ms: f64,
    /// Полукосинусный фейд внутри сохранённого запаса на обрезанном крае.
    pub fade_ms: f64,
    /// Если медианный уровень удаляемого края ближе этого к порогу слабого
    /// звука, граница неуверенна: край не обрезается и нужен человек.
    pub uncertain_floor_margin_db: f64,
    /// Минимальный запас речевого уровня (p95 окон) над порогом.
    pub min_speech_margin_db: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoudnessPolicy {
    pub target_lufs: f64,
    /// Отклонение от цели меньше этого не исправляется.
    pub gain_tolerance_db: f64,
    pub max_gain_db: f64,
    pub max_attenuation_db: f64,
    pub true_peak_ceiling_dbtp: f64,
    /// Запас под рост true peak при lossy-кодировании.
    pub encode_headroom_db: f64,
    /// Усиление не поднимает оценку шумового пола выше этого уровня.
    pub max_noise_floor_after_dbfs: f64,
    /// Недобор цели больше этого требует прослушивания.
    pub review_shortfall_db: f64,
    /// Минимум прошедших стробирование блоков 400 мс для интегральной громкости.
    pub min_integrated_blocks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SampleRatePolicy {
    Source,
    Fixed { hz: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelPolicy {
    Source,
    /// Сводит стерео в моно только при фактически одинаковых каналах.
    MonoIfIdentical,
    Mono,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum EncoderPolicy {
    /// libmp3lame VBR `-q:a` (0 — лучшее качество, 9 — худшее).
    Vbr { quality: u8 },
    /// libmp3lame CBR, кбит/с.
    Cbr { kbps: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPolicy {
    pub sample_rate: SampleRatePolicy,
    pub channels: ChannelPolicy,
    /// Порог разности каналов для `mono_if_identical`, dB.
    pub identical_channels_max_db: f64,
    pub encoder: EncoderPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatePolicy {
    pub max_true_peak_overshoot_db: f64,
    pub max_duration_error_ms: f64,
    pub max_active_change_ms: f64,
    pub max_active_change_ratio: f64,
    pub min_codec_snr_db: f64,
    pub max_loudness_error_db: f64,
    pub max_new_clipped_samples: u64,
    /// Доля отсчётов у полной шкалы, начиная с которой исходник требует прослушивания.
    pub review_clip_ratio: f64,
    pub max_alignment_lag_ms: f64,
}

impl NormalizerProfile {
    /// Профиль Balanced: исходные ориентиры (−23 LUFS, −2 dBTP, порог −50 dBFS,
    /// край от 150 мс, запасы 60/80 мс) — гипотеза для калибровки, не истина.
    pub fn balanced() -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: "balanced".into(),
            version: 1,
            analysis: AnalysisPolicy {
                frame_ms: 10.0,
                signal_floor_dbfs: -70.0,
                min_duration_ms: 120.0,
                internal_pause_report_ms: 150.0,
                long_internal_pause_ms: 1200.0,
                dc_offset_warning: 0.02,
            },
            trim: TrimPolicy {
                enabled: true,
                silence_threshold_dbfs: -50.0,
                weak_sound_margin_db: 10.0,
                min_edge_silence_ms: 150.0,
                guard_leading_ms: 60.0,
                guard_trailing_ms: 80.0,
                fade_ms: 5.0,
                uncertain_floor_margin_db: 6.0,
                min_speech_margin_db: 15.0,
            },
            loudness: LoudnessPolicy {
                target_lufs: -23.0,
                gain_tolerance_db: 1.0,
                max_gain_db: 12.0,
                max_attenuation_db: 20.0,
                true_peak_ceiling_dbtp: -2.0,
                encode_headroom_db: 0.5,
                max_noise_floor_after_dbfs: -55.0,
                review_shortfall_db: 6.0,
                min_integrated_blocks: 3,
            },
            output: OutputPolicy {
                sample_rate: SampleRatePolicy::Source,
                channels: ChannelPolicy::Source,
                identical_channels_max_db: -60.0,
                encoder: EncoderPolicy::Vbr { quality: 2 },
            },
            gate: GatePolicy {
                max_true_peak_overshoot_db: 0.5,
                max_duration_error_ms: 10.0,
                max_active_change_ms: 30.0,
                max_active_change_ratio: 0.05,
                min_codec_snr_db: 15.0,
                max_loudness_error_db: 1.0,
                max_new_clipped_samples: 0,
                review_clip_ratio: 0.0005,
                max_alignment_lag_ms: 5.0,
            },
        }
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != PROFILE_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "неподдерживаемая версия схемы профиля нормализации",
            ));
        }
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || self.version == 0
        {
            return Err(invalid_profile(
                "id профиля — 1–64 символа [a-z0-9-], version — положительное число",
            ));
        }
        let a = &self.analysis;
        let t = &self.trim;
        let l = &self.loudness;
        let g = &self.gate;
        let finite = [
            a.frame_ms,
            a.signal_floor_dbfs,
            a.min_duration_ms,
            a.internal_pause_report_ms,
            a.long_internal_pause_ms,
            a.dc_offset_warning,
            t.silence_threshold_dbfs,
            t.weak_sound_margin_db,
            t.min_edge_silence_ms,
            t.guard_leading_ms,
            t.guard_trailing_ms,
            t.fade_ms,
            t.uncertain_floor_margin_db,
            t.min_speech_margin_db,
            l.target_lufs,
            l.gain_tolerance_db,
            l.max_gain_db,
            l.max_attenuation_db,
            l.true_peak_ceiling_dbtp,
            l.encode_headroom_db,
            l.max_noise_floor_after_dbfs,
            l.review_shortfall_db,
            self.output.identical_channels_max_db,
            g.max_true_peak_overshoot_db,
            g.max_duration_error_ms,
            g.max_active_change_ms,
            g.max_active_change_ratio,
            g.min_codec_snr_db,
            g.max_loudness_error_db,
            g.review_clip_ratio,
            g.max_alignment_lag_ms,
        ];
        let checks = [
            finite.iter().all(|v| v.is_finite()),
            (1.0..=50.0).contains(&a.frame_ms),
            (-120.0..=-20.0).contains(&a.signal_floor_dbfs),
            (0.0..=5000.0).contains(&a.min_duration_ms),
            a.internal_pause_report_ms >= a.frame_ms,
            a.long_internal_pause_ms >= a.internal_pause_report_ms,
            (0.0..=1.0).contains(&a.dc_offset_warning),
            (-90.0..=-20.0).contains(&t.silence_threshold_dbfs),
            (0.0..=40.0).contains(&t.weak_sound_margin_db),
            t.silence_threshold_dbfs - t.weak_sound_margin_db >= a.signal_floor_dbfs - 30.0,
            t.min_edge_silence_ms >= 0.0,
            (0.0..=1000.0).contains(&t.guard_leading_ms),
            (0.0..=1000.0).contains(&t.guard_trailing_ms),
            t.min_edge_silence_ms >= t.guard_leading_ms.max(t.guard_trailing_ms),
            t.fade_ms >= 0.0 && t.fade_ms <= t.guard_leading_ms.min(t.guard_trailing_ms),
            (0.0..=40.0).contains(&t.uncertain_floor_margin_db),
            (0.0..=80.0).contains(&t.min_speech_margin_db),
            (-60.0..=-5.0).contains(&l.target_lufs),
            (0.0..=10.0).contains(&l.gain_tolerance_db),
            (0.0..=40.0).contains(&l.max_gain_db),
            (0.0..=60.0).contains(&l.max_attenuation_db),
            (-20.0..=0.0).contains(&l.true_peak_ceiling_dbtp),
            (0.0..=6.0).contains(&l.encode_headroom_db),
            (-120.0..=0.0).contains(&l.max_noise_floor_after_dbfs),
            l.review_shortfall_db >= 0.0,
            (1..=1000).contains(&l.min_integrated_blocks),
            self.output.identical_channels_max_db <= 0.0,
            (0.0..=6.0).contains(&g.max_true_peak_overshoot_db),
            (0.0..=1000.0).contains(&g.max_duration_error_ms),
            g.max_active_change_ms >= 0.0,
            (0.0..=1.0).contains(&g.max_active_change_ratio),
            (0.0..=120.0).contains(&g.min_codec_snr_db),
            (0.0..=20.0).contains(&g.max_loudness_error_db),
            (0.0..=1.0).contains(&g.review_clip_ratio),
            (0.0..=100.0).contains(&g.max_alignment_lag_ms),
            match self.output.sample_rate {
                SampleRatePolicy::Source => true,
                SampleRatePolicy::Fixed { hz } => SUPPORTED_RATES.contains(&hz),
            },
            match self.output.encoder {
                EncoderPolicy::Vbr { quality } => quality <= 9,
                EncoderPolicy::Cbr { kbps } => [
                    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                ]
                .contains(&kbps),
            },
        ];
        if checks.iter().all(|ok| *ok) {
            Ok(())
        } else {
            Err(invalid_profile(
                "значения профиля нормализации вне допустимых диапазонов",
            ))
        }
    }

    fn encoder_args(&self) -> Vec<String> {
        match self.output.encoder {
            EncoderPolicy::Vbr { quality } => vec!["-q:a".into(), quality.to_string()],
            EncoderPolicy::Cbr { kbps } => vec!["-b:a".into(), format!("{kbps}k")],
        }
    }
}

const SUPPORTED_RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Только измерения и план, без кодирования.
    Analyze,
    /// План, одно кодирование и пост-QC фактического результата.
    Normalize,
}

/// Окружение, от которого зависят байты результата.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    pub ffmpeg: String,
    pub encoder: String,
    pub analyzer: String,
}

impl Toolchain {
    pub fn detect() -> Result<Self, AssetError> {
        Ok(Self {
            ffmpeg: probe::ffmpeg_identity()?,
            encoder: "libmp3lame".into(),
            analyzer: ANALYZER_VERSION.into(),
        })
    }
}

/// Версия собственных алгоритмов анализа, плана и QualityGate.
pub const ANALYZER_VERSION: &str = "balanced-analyzer/1";

/// Отпечаток всего, что определяет результат для данного входа.
pub fn config_sha256(profile: &NormalizerProfile, mode: Mode, toolchain: &Toolchain) -> String {
    sha256_hex(
        serde_json::to_vec(&serde_json::json!({
            "schema_version": RESULT_SCHEMA_VERSION,
            "mode": mode,
            "profile": profile,
            "toolchain": toolchain,
        }))
        .expect("профиль сериализуем")
        .as_slice(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClippingSummary {
    pub near_full_scale_samples: u64,
    pub over_full_scale_samples: u64,
    pub clip_runs: u64,
    pub near_full_scale_ratio: f64,
}

/// Измерения одного сигнала. Неопределённые величины — `null`, а не 0/NaN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalAnalysis {
    pub sample_rate: u32,
    pub channels: u32,
    pub frames: u64,
    pub duration_ms: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
    pub rms_dbfs: f64,
    pub integrated_lufs: Option<f64>,
    pub integrated_blocks: usize,
    pub short_window_lufs: Option<f64>,
    pub short_window_blocks: usize,
    pub max_window_dbfs: f64,
    /// Оценка шумового пола (не измерение шума): медиана окон ниже порога речи,
    /// а без таких окон — консервативная p10 всех окон.
    pub noise_floor_dbfs: f64,
    pub noise_floor_method: NoiseFloorMethod,
    /// p95 уровней окон: оценка уровня речи.
    pub speech_level_dbfs: f64,
    pub leading_below_threshold_ms: f64,
    pub trailing_below_threshold_ms: f64,
    pub leading_below_weak_ms: f64,
    pub trailing_below_weak_ms: f64,
    pub active_ms: f64,
    pub longest_internal_pause_ms: f64,
    pub internal_pauses: usize,
    pub clipping: ClippingSummary,
    pub dc_offset: f64,
    pub stereo_difference_db: Option<f64>,
    pub digital_silence: bool,
}

/// Минимум окон ниже порога речи для оценки шума по паузам и краям.
const MIN_NOISE_WINDOWS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseFloorMethod {
    QuietWindows,
    /// Пауз нет: оценка завышена и ограничивает усиление сильнее.
    AllWindowsP10,
}

struct Levels {
    frame: usize,
    levels: Vec<f64>,
}

fn levels(pcm: &Pcm, profile: &NormalizerProfile) -> Levels {
    let frame = pcm.samples_for_ms(profile.analysis.frame_ms).max(1);
    Levels {
        frame,
        levels: dsp::frame_levels(pcm, frame),
    }
}

fn ms(pcm: &Pcm, frames: usize) -> f64 {
    frames as f64 * 1000.0 / f64::from(pcm.sample_rate)
}

pub fn analyze_pcm(pcm: &Pcm, profile: &NormalizerProfile) -> SignalAnalysis {
    let Levels { frame, levels } = levels(pcm, profile);
    let total = pcm.frames();
    let threshold = profile.trim.silence_threshold_dbfs;
    let weak = threshold - profile.trim.weak_sound_margin_db;
    let edges = |limit: f64| -> (usize, usize) {
        match (
            levels.iter().position(|l| *l >= limit),
            levels.iter().rposition(|l| *l >= limit),
        ) {
            (Some(first), Some(last)) => (
                (first * frame).min(total),
                total.saturating_sub((last + 1) * frame),
            ),
            _ => (total, total),
        }
    };
    let (lead_threshold, trail_threshold) = edges(threshold);
    let (lead_weak, trail_weak) = edges(weak);
    let mut longest_pause = 0usize;
    let mut pauses = 0usize;
    if let (Some(first), Some(last)) = (
        levels.iter().position(|l| *l >= weak),
        levels.iter().rposition(|l| *l >= weak),
    ) {
        let mut run = 0usize;
        let report = pcm.samples_for_ms(profile.analysis.internal_pause_report_ms);
        for level in &levels[first..=last] {
            if *level < threshold {
                run += 1;
            } else {
                if run > 0 {
                    longest_pause = longest_pause.max(run * frame);
                    if run * frame >= report {
                        pauses += 1;
                    }
                }
                run = 0;
            }
        }
    }
    let quiet: Vec<f64> = levels.iter().copied().filter(|l| *l < threshold).collect();
    let noise_floor = if quiet.len() >= MIN_NOISE_WINDOWS {
        (
            dsp::percentile(&quiet, 0.5).unwrap_or(dsp::LEVEL_FLOOR_DB),
            NoiseFloorMethod::QuietWindows,
        )
    } else {
        (
            dsp::percentile(&levels, 0.10).unwrap_or(dsp::LEVEL_FLOOR_DB),
            NoiseFloorMethod::AllWindowsP10,
        )
    };
    let loudness = dsp::loudness(pcm);
    let clip = dsp::clipping(pcm);
    let digital_silence = pcm.samples.iter().all(|s| *s == 0.0);
    SignalAnalysis {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels as u32,
        frames: total as u64,
        duration_ms: ms(pcm, total),
        sample_peak_dbfs: dsp::amplitude_db(dsp::sample_peak(pcm)),
        true_peak_dbtp: dsp::amplitude_db(dsp::true_peak(pcm)),
        rms_dbfs: dsp::amplitude_db(dsp::rms(pcm)),
        integrated_lufs: loudness.integrated.map(|l| l.lufs),
        integrated_blocks: loudness.integrated.map_or(0, |l| l.blocks_gated),
        short_window_lufs: loudness.short_window.map(|l| l.lufs),
        short_window_blocks: loudness.short_window.map_or(0, |l| l.blocks_gated),
        max_window_dbfs: levels.iter().copied().fold(dsp::LEVEL_FLOOR_DB, f64::max),
        noise_floor_dbfs: noise_floor.0,
        noise_floor_method: noise_floor.1,
        speech_level_dbfs: dsp::percentile(&levels, 0.95).unwrap_or(dsp::LEVEL_FLOOR_DB),
        leading_below_threshold_ms: ms(pcm, lead_threshold),
        trailing_below_threshold_ms: ms(pcm, trail_threshold),
        leading_below_weak_ms: ms(pcm, lead_weak),
        trailing_below_weak_ms: ms(pcm, trail_weak),
        active_ms: ms(
            pcm,
            (levels.iter().filter(|l| **l >= threshold).count() * frame).min(total),
        ),
        longest_internal_pause_ms: ms(pcm, longest_pause),
        internal_pauses: pauses,
        clipping: ClippingSummary {
            near_full_scale_samples: clip.near_full_scale_samples,
            over_full_scale_samples: clip.over_full_scale_samples,
            clip_runs: clip.clip_runs,
            near_full_scale_ratio: if pcm.samples.is_empty() {
                0.0
            } else {
                clip.near_full_scale_samples as f64 / pcm.samples.len() as f64
            },
        },
        dc_offset: dsp::dc_offset(pcm),
        stereo_difference_db: dsp::stereo_difference_db(pcm),
        digital_silence,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgePlan {
    /// Длительность края ниже порога слабого звука.
    pub quiet_ms: f64,
    pub trim_frames: u64,
    pub trim_ms: f64,
    /// Медианный уровень удаляемого края, если он есть.
    pub edge_level_dbfs: Option<f64>,
    pub confidence: Confidence,
    /// Отдельная слабая активность (дыхание, слабый звук) сохранена.
    pub weak_activity_kept: bool,
    /// Наибольший уровень окна внутри фактически удаляемой части.
    pub removed_max_dbfs: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoudnessMethod {
    /// BS.1770 интегральная: блоки 400 мс.
    Integrated,
    /// Короткие окна 100 мс с теми же порогами — для коротких слов.
    ShortWindow,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GainLimit {
    MaxGain,
    MaxAttenuation,
    TruePeak,
    NoiseFloor,
    Tolerance,
    LoudnessUnavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GainPlan {
    pub method: LoudnessMethod,
    pub measured_lufs: Option<f64>,
    pub desired_db: Option<f64>,
    pub applied_db: f64,
    pub limited_by: Vec<GainLimit>,
    /// Насколько результат останется тише цели (положительное — тише).
    pub shortfall_db: Option<f64>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub leading: EdgePlan,
    pub trailing: EdgePlan,
    pub gain: GainPlan,
    pub output_sample_rate: u32,
    pub output_channels: u32,
    pub fade_in_frames: u64,
    pub fade_out_frames: u64,
    pub needs_processing: bool,
    /// Метрики подготовленного PCM до кодирования: с ними сравнивается результат.
    pub expected: ExpectedSignal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedSignal {
    pub duration_ms: f64,
    pub true_peak_dbtp: f64,
    pub measured_lufs: Option<f64>,
    pub active_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateOutcome {
    /// Обработанный вариант технически пригоден.
    Accept,
    /// Изменение не нужно или его преимущество не доказано.
    KeepOriginal,
    /// Нужен человек; предлагаемый вариант указан отдельно.
    ReviewRequired,
    /// Сигнал технически непригоден.
    Reject,
}

impl GateOutcome {
    pub const ALL: [GateOutcome; 4] = [
        GateOutcome::Accept,
        GateOutcome::KeepOriginal,
        GateOutcome::ReviewRequired,
        GateOutcome::Reject,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPT",
            Self::KeepOriginal => "KEEP_ORIGINAL",
            Self::ReviewRequired => "REVIEW_REQUIRED",
            Self::Reject => "REJECT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    Original,
    Processed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Review,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub code: String,
    pub severity: Severity,
    pub message: String,
}

fn reason(code: &str, severity: Severity, message: impl Into<String>) -> Reason {
    Reason {
        code: code.into(),
        severity,
        message: message.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDecision {
    pub outcome: GateOutcome,
    pub proposed_variant: Option<Variant>,
    /// `false` в режиме analyze: исход — план без проверки фактического результата.
    pub post_qc_verified: bool,
    pub reasons: Vec<Reason>,
    /// Что этот этап принципиально не проверяет.
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    pub passed: bool,
    pub value: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostQc {
    pub passed: bool,
    pub checks: Vec<Check>,
    pub analysis: Option<SignalAnalysis>,
    pub alignment_lag_frames: Option<i64>,
    pub codec_snr_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRef {
    pub sha256: String,
    pub byte_length: u64,
    pub probe: Option<AudioProbe>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRef {
    pub sha256: String,
    pub byte_length: u64,
    pub probe: Option<AudioProbe>,
    /// Путь внутри runtime каталога вывода; задаётся пакетным исполнителем.
    pub storage_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationResult {
    pub schema_version: u32,
    pub mode: Mode,
    pub config_sha256: String,
    pub profile: ProfileRef,
    pub toolchain: Toolchain,
    pub input: InputRef,
    pub analysis: Option<SignalAnalysis>,
    pub plan: Option<Plan>,
    pub output: Option<OutputRef>,
    pub post_qc: Option<PostQc>,
    pub gate: GateDecision,
}

/// Результат вместе с байтами обработанного варианта (они не сериализуются).
#[derive(Debug)]
pub struct Processed {
    pub result: NormalizationResult,
    pub output_bytes: Option<Vec<u8>>,
}

const INFRASTRUCTURE_BLOCKERS: [&str; 4] = [
    "audio_tool_missing",
    "audio_tool_start_failed",
    "audio_temp_io_failure",
    "audio_tool_runtime_failed",
];

/// Сбой среды (нет FFmpeg, ошибка временных файлов, сигнал) не является
/// свойством входного файла и не превращается в исход `REJECT`.
pub fn is_infrastructure_failure(error: &AssetError) -> bool {
    error.details["blocker"]
        .as_str()
        .is_some_and(|code| INFRASTRUCTURE_BLOCKERS.contains(&code))
}

fn unresolved() -> Vec<String> {
    vec![
        "phonetic_content_not_verified: соответствие чтению и фонетике требует прослушивания человеком".into(),
        "speech_boundaries_heuristic: границы речи оценены по уровню сигнала, без ASR/forced alignment".into(),
    ]
}

fn plan_edges(
    pcm: &Pcm,
    analysis: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) -> (EdgePlan, EdgePlan) {
    let Levels { frame, levels } = levels(pcm, profile);
    let total = pcm.frames();
    let trim = &profile.trim;
    let threshold = trim.silence_threshold_dbfs;
    let weak = threshold - trim.weak_sound_margin_db;
    let none = |quiet: usize, confidence| EdgePlan {
        quiet_ms: ms(pcm, quiet),
        trim_frames: 0,
        trim_ms: 0.0,
        edge_level_dbfs: None,
        confidence,
        weak_activity_kept: false,
        removed_max_dbfs: None,
    };
    let (Some(first_weak), Some(last_weak), Some(first_strong), Some(last_strong)) = (
        levels.iter().position(|l| *l >= weak),
        levels.iter().rposition(|l| *l >= weak),
        levels.iter().position(|l| *l >= threshold),
        levels.iter().rposition(|l| *l >= threshold),
    ) else {
        return (none(0, Confidence::Low), none(0, Confidence::Low));
    };
    let speech_margin_ok = analysis.speech_level_dbfs - threshold >= trim.min_speech_margin_db;
    let edge = |leading: bool, reasons: &mut Vec<Reason>| -> EdgePlan {
        let (quiet_frames, region, activity, guard, name) = if leading {
            (
                (first_weak * frame).min(total),
                &levels[..first_weak],
                levels[first_weak..first_strong].iter().any(|l| *l < weak),
                pcm.samples_for_ms(trim.guard_leading_ms),
                "leading",
            )
        } else {
            (
                total.saturating_sub((last_weak + 1) * frame),
                &levels[last_weak + 1..],
                levels[last_strong + 1..=last_weak]
                    .iter()
                    .any(|l| *l < weak),
                pcm.samples_for_ms(trim.guard_trailing_ms),
                "trailing",
            )
        };
        let mut plan = none(quiet_frames, Confidence::High);
        plan.edge_level_dbfs = dsp::percentile(region, 0.5);
        plan.weak_activity_kept = activity;
        if activity {
            reasons.push(reason(
                &format!("{name}_weak_activity_kept"),
                Severity::Warning,
                "отдельная слабая активность у края сохранена: возможны дыхание, шум или слабый звук",
            ));
        }
        if !trim.enabled || ms(pcm, quiet_frames) < trim.min_edge_silence_ms {
            return plan;
        }
        if !speech_margin_ok {
            plan.confidence = Confidence::Low;
            return plan;
        }
        if plan
            .edge_level_dbfs
            .is_some_and(|level| level >= weak - trim.uncertain_floor_margin_db)
        {
            plan.confidence = Confidence::Low;
            reasons.push(reason(
                &format!("uncertain_{name}_boundary"),
                Severity::Review,
                "уровень края близок к порогу слабого звука: граница речи неуверенна, край не обрезан",
            ));
            return plan;
        }
        if activity {
            plan.confidence = Confidence::Medium;
        }
        let trim_frames = quiet_frames.saturating_sub(guard);
        plan.trim_frames = trim_frames as u64;
        plan.trim_ms = ms(pcm, trim_frames);
        let windows = trim_frames.div_ceil(frame).min(levels.len());
        let removed = if leading {
            &levels[..windows]
        } else {
            &levels[levels.len() - windows..]
        };
        plan.removed_max_dbfs = removed.iter().copied().reduce(f64::max);
        plan
    };
    if !speech_margin_ok {
        reasons.push(reason(
            "low_signal_margin",
            Severity::Review,
            "уровень речи слабо отличается от порога тишины: границы не определяются надёжно",
        ));
    }
    let leading = edge(true, reasons);
    let trailing = edge(false, reasons);
    (leading, trailing)
}

fn plan_gain(
    trimmed_analysis: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) -> GainPlan {
    let policy = &profile.loudness;
    let (method, measured, confidence) = match (
        trimmed_analysis.integrated_lufs,
        trimmed_analysis.short_window_lufs,
    ) {
        (Some(value), _) if trimmed_analysis.integrated_blocks >= policy.min_integrated_blocks => {
            (LoudnessMethod::Integrated, Some(value), Confidence::High)
        }
        (_, Some(value)) => (LoudnessMethod::ShortWindow, Some(value), Confidence::Medium),
        (Some(value), None) => (LoudnessMethod::Integrated, Some(value), Confidence::Medium),
        (None, None) => (LoudnessMethod::Unavailable, None, Confidence::Low),
    };
    let Some(measured) = measured else {
        reasons.push(reason(
            "loudness_unavailable",
            Severity::Review,
            "громкость не измерима: усиление не применяется",
        ));
        return GainPlan {
            method,
            measured_lufs: None,
            desired_db: None,
            applied_db: 0.0,
            limited_by: vec![GainLimit::LoudnessUnavailable],
            shortfall_db: None,
            confidence,
        };
    };
    if method == LoudnessMethod::ShortWindow {
        reasons.push(reason(
            "short_clip_loudness",
            Severity::Info,
            "запись короче устойчивого измерения 400 мс: громкость оценена окнами 100 мс",
        ));
    }
    let desired = policy.target_lufs - measured;
    let mut limited = Vec::new();
    let mut applied = desired;
    if desired > 0.0 {
        let caps = [
            (policy.max_gain_db, GainLimit::MaxGain),
            (
                policy.true_peak_ceiling_dbtp
                    - policy.encode_headroom_db
                    - trimmed_analysis.true_peak_dbtp,
                GainLimit::TruePeak,
            ),
            (
                policy.max_noise_floor_after_dbfs - trimmed_analysis.noise_floor_dbfs,
                GainLimit::NoiseFloor,
            ),
        ];
        let bound = caps.iter().map(|(cap, _)| *cap).fold(desired, f64::min);
        // Ограничение не превращает тихую запись в ещё более тихую.
        applied = bound.max(0.0);
        limited = caps
            .iter()
            .filter(|(cap, _)| *cap < desired && *cap <= bound + 1e-9)
            .map(|(_, limit)| *limit)
            .collect();
    } else if applied < -policy.max_attenuation_db {
        applied = -policy.max_attenuation_db;
        limited.push(GainLimit::MaxAttenuation);
    }
    if applied.abs() < policy.gain_tolerance_db {
        if applied != 0.0 {
            limited.push(GainLimit::Tolerance);
        }
        applied = 0.0;
    }
    let shortfall = desired - applied;
    if shortfall > policy.review_shortfall_db {
        reasons.push(reason(
            "target_shortfall",
            Severity::Review,
            format!(
                "цель недостижима без нарушения ограничений: останется тише на {shortfall:.1} dB"
            ),
        ));
    } else if !limited.is_empty() && !limited.contains(&GainLimit::Tolerance) {
        reasons.push(reason(
            "gain_limited",
            Severity::Info,
            "усиление ограничено политикой профиля",
        ));
    }
    GainPlan {
        method,
        measured_lufs: Some(measured),
        desired_db: Some(desired),
        applied_db: applied,
        limited_by: limited,
        shortfall_db: Some(shortfall),
        confidence,
    }
}

fn source_reasons(
    analysis: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) {
    let a = &profile.analysis;
    if analysis.duration_ms < a.min_duration_ms {
        reasons.push(reason(
            "very_short_clip",
            Severity::Review,
            "очень короткая запись: возможно усечение слова",
        ));
    }
    if analysis.clipping.near_full_scale_ratio > profile.gate.review_clip_ratio
        || analysis.clipping.clip_runs > 0
    {
        reasons.push(reason(
            "source_clipping",
            Severity::Review,
            "в исходнике есть признаки перегрузки; этап их не исправляет",
        ));
    }
    if analysis.true_peak_dbtp > profile.loudness.true_peak_ceiling_dbtp {
        reasons.push(reason(
            "source_true_peak_above_ceiling",
            Severity::Warning,
            "true peak исходника выше потолка профиля",
        ));
    }
    if analysis.longest_internal_pause_ms >= a.long_internal_pause_ms {
        reasons.push(reason(
            "long_internal_pause",
            Severity::Warning,
            "длинная внутренняя пауза сохранена без изменений: возможны два слова или дубль",
        ));
    }
    if analysis.dc_offset.abs() > a.dc_offset_warning {
        reasons.push(reason(
            "dc_offset",
            Severity::Warning,
            "заметная постоянная составляющая; этап её не исправляет",
        ));
    }
}

fn downmix(pcm: &Pcm) -> Pcm {
    Pcm {
        sample_rate: pcm.sample_rate,
        channels: 1,
        samples: pcm
            .samples
            .chunks_exact(pcm.channels)
            .map(|frame| {
                (frame.iter().map(|s| f64::from(*s)).sum::<f64>() / frame.len() as f64) as f32
            })
            .collect(),
    }
}

/// План и подготовленный PCM без кодирования.
pub struct Prepared {
    pub analysis: SignalAnalysis,
    pub plan: Plan,
    pub processed: Pcm,
    pub processed_analysis: SignalAnalysis,
    pub reasons: Vec<Reason>,
    pub rejected: bool,
}

pub fn prepare(pcm: &Pcm, profile: &NormalizerProfile) -> Prepared {
    let analysis = analyze_pcm(pcm, profile);
    let mut reasons = Vec::new();
    let rejected = analysis.digital_silence
        || analysis.frames == 0
        || analysis.max_window_dbfs < profile.analysis.signal_floor_dbfs;
    if rejected {
        reasons.push(reason(
            "no_signal",
            Severity::Reject,
            "сигнал отсутствует или ниже порога обнаружения",
        ));
    }
    source_reasons(&analysis, profile, &mut reasons);
    let (leading, trailing) = if rejected {
        let empty = EdgePlan {
            quiet_ms: 0.0,
            trim_frames: 0,
            trim_ms: 0.0,
            edge_level_dbfs: None,
            confidence: Confidence::Low,
            weak_activity_kept: false,
            removed_max_dbfs: None,
        };
        (empty.clone(), empty)
    } else {
        plan_edges(pcm, &analysis, profile, &mut reasons)
    };
    let start = leading.trim_frames as usize;
    let end = pcm.frames() - trailing.trim_frames as usize;
    let mut channels_out = pcm.channels as u32;
    let mut working = pcm.slice_frames(start, end);
    if pcm.channels == 2 {
        let identical = analysis
            .stereo_difference_db
            .is_some_and(|d| d <= profile.output.identical_channels_max_db);
        if profile.output.channels == ChannelPolicy::Mono
            || (profile.output.channels == ChannelPolicy::MonoIfIdentical && identical)
        {
            working = downmix(&working);
            channels_out = 1;
        }
    }
    let trimmed_analysis = analyze_pcm(&working, profile);
    let gain = if rejected {
        GainPlan {
            method: LoudnessMethod::Unavailable,
            measured_lufs: None,
            desired_db: None,
            applied_db: 0.0,
            limited_by: vec![GainLimit::LoudnessUnavailable],
            shortfall_db: None,
            confidence: Confidence::Low,
        }
    } else {
        plan_gain(&trimmed_analysis, profile, &mut reasons)
    };
    let fade = pcm.samples_for_ms(profile.trim.fade_ms);
    let fade_in = if start > 0 { fade } else { 0 };
    let fade_out = if trailing.trim_frames > 0 { fade } else { 0 };
    dsp::apply_gain_and_fades(
        &mut working,
        dsp::db_to_gain(gain.applied_db),
        fade_in,
        fade_out,
    );
    let output_sample_rate = match profile.output.sample_rate {
        SampleRatePolicy::Source => pcm.sample_rate,
        SampleRatePolicy::Fixed { hz } => hz,
    };
    let needs_processing = !rejected
        && (leading.trim_frames > 0
            || trailing.trim_frames > 0
            || gain.applied_db != 0.0
            || channels_out != pcm.channels as u32
            || output_sample_rate != pcm.sample_rate);
    let processed_analysis = analyze_pcm(&working, profile);
    let expected_lufs = gain.measured_lufs.map(|lufs| lufs + gain.applied_db);
    Prepared {
        analysis,
        plan: Plan {
            leading,
            trailing,
            gain,
            output_sample_rate,
            output_channels: channels_out,
            fade_in_frames: fade_in as u64,
            fade_out_frames: fade_out as u64,
            needs_processing,
            expected: ExpectedSignal {
                duration_ms: processed_analysis.duration_ms,
                true_peak_dbtp: processed_analysis.true_peak_dbtp,
                measured_lufs: expected_lufs,
                active_ms: processed_analysis.active_ms,
            },
        },
        processed: working,
        processed_analysis,
        reasons,
        rejected,
    }
}

fn check(id: &str, passed: bool, value: Option<f64>, limit: Option<f64>) -> Check {
    Check {
        id: id.into(),
        passed,
        value: value.filter(|v| v.is_finite()),
        limit,
    }
}

/// Пост-QC сравнивает фактически декодированный результат с подготовленным PCM.
pub fn post_qc(
    prepared: &Prepared,
    output: &[u8],
    profile: &NormalizerProfile,
) -> Result<(PostQc, Option<AudioProbe>), AssetError> {
    let gate = &profile.gate;
    let reference = &prepared.processed;
    let probe = match probe::probe_mp3(output) {
        Ok(probe) => probe,
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(_) => {
            return Ok((
                PostQc {
                    passed: false,
                    checks: vec![check("output_technical_probe", false, None, None)],
                    analysis: None,
                    alignment_lag_frames: None,
                    codec_snr_db: None,
                },
                None,
            ));
        }
    };
    let decoded = Pcm {
        sample_rate: probe.sample_rate,
        channels: probe.channels as usize,
        samples: probe::decode_f32(output, &probe)?,
    };
    let after = analyze_pcm(&decoded, profile);
    let mut checks = vec![check("output_technical_probe", true, None, None)];
    checks.push(check(
        "output_layout",
        probe.channels == prepared.plan.output_channels
            && probe.sample_rate == prepared.plan.output_sample_rate,
        None,
        None,
    ));
    let expected_ms = reference.duration_seconds() * 1000.0;
    let duration_error = (after.duration_ms - expected_ms).abs();
    checks.push(check(
        "duration_matches_plan",
        decoded.frames() > 0 && duration_error <= gate.max_duration_error_ms,
        Some(duration_error),
        Some(gate.max_duration_error_ms),
    ));
    let ceiling = profile.loudness.true_peak_ceiling_dbtp + gate.max_true_peak_overshoot_db;
    // Потолок обязателен, только если сами операции не начинались выше него.
    let tp_limit =
        ceiling.max(prepared.processed_analysis.true_peak_dbtp + gate.max_true_peak_overshoot_db);
    checks.push(check(
        "true_peak_within_ceiling",
        after.true_peak_dbtp <= tp_limit,
        Some(after.true_peak_dbtp),
        Some(tp_limit),
    ));
    let new_clipped = after
        .clipping
        .near_full_scale_samples
        .saturating_sub(prepared.processed_analysis.clipping.near_full_scale_samples);
    checks.push(check(
        "no_new_clipping",
        new_clipped <= gate.max_new_clipped_samples,
        Some(new_clipped as f64),
        Some(gate.max_new_clipped_samples as f64),
    ));
    let planned = prepared
        .plan
        .gain
        .measured_lufs
        .map(|lufs| lufs + prepared.plan.gain.applied_db);
    let actual = match prepared.plan.gain.method {
        LoudnessMethod::Integrated => after.integrated_lufs.or(after.short_window_lufs),
        _ => after.short_window_lufs,
    };
    if let (Some(planned), Some(actual)) = (planned, actual) {
        let error = (actual - planned).abs();
        checks.push(check(
            "loudness_matches_plan",
            error <= gate.max_loudness_error_db,
            Some(error),
            Some(gate.max_loudness_error_db),
        ));
    }
    let threshold = profile.trim.silence_threshold_dbfs;
    let edge_frame = decoded.samples_for_ms(profile.analysis.frame_ms).max(1);
    let edge_levels = dsp::frame_levels(&decoded, edge_frame);
    if prepared.plan.leading.trim_frames > 0 {
        let first = edge_levels.first().copied().unwrap_or(dsp::LEVEL_FLOOR_DB);
        checks.push(check(
            "leading_edge_quiet",
            first < threshold,
            Some(first),
            Some(threshold),
        ));
    }
    if prepared.plan.trailing.trim_frames > 0 {
        let last = edge_levels.last().copied().unwrap_or(dsp::LEVEL_FLOOR_DB);
        checks.push(check(
            "trailing_edge_quiet",
            last < threshold,
            Some(last),
            Some(threshold),
        ));
    }
    // Активная часть сравнивается с подготовленным сигналом в том же масштабе,
    // поэтому усиление само по себе не меняет её длительность.
    let active_change = (after.active_ms - prepared.processed_analysis.active_ms).abs();
    let active_limit = gate
        .max_active_change_ms
        .max(prepared.processed_analysis.active_ms * gate.max_active_change_ratio);
    checks.push(check(
        "active_duration_preserved",
        active_change <= active_limit,
        Some(active_change),
        Some(active_limit),
    ));
    let mut lag = None;
    let mut snr = None;
    if decoded.sample_rate == reference.sample_rate && decoded.channels == reference.channels {
        let max_lag = reference.samples_for_ms(gate.max_alignment_lag_ms.max(30.0));
        if let Some(alignment) = dsp::aligned_snr(reference, &decoded, max_lag) {
            lag = Some(alignment.lag_frames);
            snr = Some(alignment.snr_db);
            let lag_ms = alignment.lag_frames.unsigned_abs() as f64 * 1000.0
                / f64::from(reference.sample_rate);
            checks.push(check(
                "alignment_lag",
                lag_ms <= gate.max_alignment_lag_ms,
                Some(lag_ms),
                Some(gate.max_alignment_lag_ms),
            ));
            checks.push(check(
                "codec_snr",
                alignment.snr_db >= gate.min_codec_snr_db,
                Some(alignment.snr_db),
                Some(gate.min_codec_snr_db),
            ));
        }
    }
    let passed = checks.iter().all(|c| c.passed);
    Ok((
        PostQc {
            passed,
            checks,
            analysis: Some(after),
            alignment_lag_frames: lag,
            codec_snr_db: snr,
        },
        Some(probe),
    ))
}

/// Полный путь одного входа. `Err` возвращается только при сбое среды;
/// любой отказ самого файла становится объяснимым исходом QualityGate.
pub fn process(
    bytes: &[u8],
    profile: &NormalizerProfile,
    mode: Mode,
    toolchain: &Toolchain,
) -> Result<Processed, AssetError> {
    profile.validate()?;
    let config = config_sha256(profile, mode, toolchain);
    let base = |input: InputRef| NormalizationResult {
        schema_version: RESULT_SCHEMA_VERSION,
        mode,
        config_sha256: config.clone(),
        profile: ProfileRef {
            id: profile.id.clone(),
            version: profile.version,
        },
        toolchain: toolchain.clone(),
        input,
        analysis: None,
        plan: None,
        output: None,
        post_qc: None,
        gate: GateDecision {
            outcome: GateOutcome::Reject,
            proposed_variant: None,
            post_qc_verified: false,
            reasons: Vec::new(),
            unresolved: unresolved(),
        },
    };
    let input = InputRef {
        sha256: sha256_hex(bytes),
        byte_length: bytes.len() as u64,
        probe: None,
        error: None,
    };
    let mut strict_failure = None;
    let probe = match super::probe_mp3(bytes) {
        Ok(probe) => Ok(probe),
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(error) if error.details["check"] == "duration_consistency" => {
            match probe::probe_mp3_lame_gapless(bytes) {
                Ok(probe) => {
                    strict_failure = Some(error.message.clone());
                    Ok(probe)
                }
                Err(lenient) if is_infrastructure_failure(&lenient) => return Err(lenient),
                Err(_) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            let mut result = base(InputRef {
                error: Some(error.message.clone()),
                ..input
            });
            result.gate.reasons.push(reason(
                "source_invalid",
                Severity::Reject,
                format!("техническая проверка MP3 не пройдена: {}", error.message),
            ));
            return Ok(Processed {
                result,
                output_bytes: None,
            });
        }
    };
    let pcm = Pcm {
        sample_rate: probe.sample_rate,
        channels: probe.channels as usize,
        samples: probe::decode_f32(bytes, &probe)?,
    };
    let prepared = prepare(&pcm, profile);
    let mut result = base(InputRef {
        probe: Some(probe),
        ..input
    });
    result.analysis = Some(prepared.analysis.clone());
    result.plan = Some(prepared.plan.clone());
    let mut reasons = prepared.reasons.clone();
    if let Some(message) = strict_failure {
        result.input.error = Some(message);
        reasons.push(reason(
            "source_rejected_by_store_validator",
            Severity::Review,
            "исходник декодируется полностью, а расхождение длительностей точно объясняет тег LAME (задержка/дополнение), но текущая техническая проверка хранилища такой файл не принимает",
        ));
    }
    let mut output_bytes = None;
    let has = |reasons: &[Reason], severity| reasons.iter().any(|r| r.severity == severity);
    let (outcome, variant) = if prepared.rejected {
        (GateOutcome::Reject, None)
    } else if !prepared.plan.needs_processing {
        reasons.push(reason(
            "already_within_profile",
            Severity::Info,
            "исходник уже в допусках профиля: перекодирование не нужно",
        ));
        if has(&reasons, Severity::Review) {
            (GateOutcome::ReviewRequired, Some(Variant::Original))
        } else {
            (GateOutcome::KeepOriginal, Some(Variant::Original))
        }
    } else if mode == Mode::Analyze {
        if has(&reasons, Severity::Review) {
            (GateOutcome::ReviewRequired, Some(Variant::Processed))
        } else {
            (GateOutcome::Accept, Some(Variant::Processed))
        }
    } else {
        let encoded = probe::encode_mp3(
            &prepared.processed.samples,
            prepared.processed.sample_rate,
            prepared.plan.output_channels,
            prepared.plan.output_sample_rate,
            &profile.encoder_args(),
        );
        let encoded = match encoded {
            Ok(bytes) => bytes,
            Err(error) if is_infrastructure_failure(&error) => return Err(error),
            Err(error) => {
                reasons.push(reason(
                    "encode_failed",
                    Severity::Review,
                    format!("кодирование не выполнено: {}", error.message),
                ));
                result.gate.outcome = GateOutcome::ReviewRequired;
                result.gate.proposed_variant = Some(Variant::Original);
                result.gate.reasons = sorted(reasons);
                return Ok(Processed {
                    result,
                    output_bytes: None,
                });
            }
        };
        let (qc, output_probe) = post_qc(&prepared, &encoded, profile)?;
        result.output = Some(OutputRef {
            sha256: sha256_hex(&encoded),
            byte_length: encoded.len() as u64,
            probe: output_probe,
            storage_path: None,
        });
        result.gate.post_qc_verified = true;
        let passed = qc.passed;
        for failed in qc.checks.iter().filter(|c| !c.passed) {
            reasons.push(reason(
                &format!("post_qc_{}", failed.id),
                Severity::Review,
                "фактический результат не прошёл проверку; предложен исходник",
            ));
        }
        result.post_qc = Some(qc);
        output_bytes = Some(encoded);
        if !passed {
            (GateOutcome::ReviewRequired, Some(Variant::Original))
        } else if has(&reasons, Severity::Review) {
            (GateOutcome::ReviewRequired, Some(Variant::Processed))
        } else {
            (GateOutcome::Accept, Some(Variant::Processed))
        }
    };
    result.gate.outcome = outcome;
    result.gate.proposed_variant = variant;
    result.gate.reasons = sorted(reasons);
    Ok(Processed {
        result,
        output_bytes,
    })
}

fn sorted(mut reasons: Vec<Reason>) -> Vec<Reason> {
    reasons.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.code.cmp(&b.code)));
    reasons.dedup_by(|a, b| a.code == b.code);
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44100;

    /// Детерминированный сигнал из сегментов: (длительность мс, вид, уровень dBFS).
    #[derive(Clone, Copy)]
    enum Kind {
        Silence,
        Tone,
        Noise,
    }

    fn signal(segments: &[(f64, Kind, f64)]) -> Pcm {
        let mut samples = Vec::new();
        let mut state = 0x1234_5678u32;
        for &(ms, kind, level) in segments {
            let frames = (f64::from(RATE) * ms / 1000.0) as usize;
            let amplitude = dsp::db_to_gain(level);
            for i in 0..frames {
                let value = match kind {
                    Kind::Silence => 0.0,
                    Kind::Tone => {
                        amplitude
                            * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(RATE))
                                .sin()
                    }
                    Kind::Noise => {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        // Равномерный шум с RMS, равным заданному уровню.
                        amplitude
                            * 3f64.sqrt()
                            * ((state >> 8) as f64 / f64::from(1u32 << 24) * 2.0 - 1.0)
                    }
                };
                samples.push(value as f32);
            }
        }
        Pcm {
            sample_rate: RATE,
            channels: 1,
            samples,
        }
    }

    fn codes(prepared: &Prepared) -> Vec<&str> {
        prepared.reasons.iter().map(|r| r.code.as_str()).collect()
    }

    fn profile() -> NormalizerProfile {
        NormalizerProfile::balanced()
    }

    #[test]
    fn default_profile_is_valid_and_fingerprint_tracks_parameters() {
        let base = profile();
        base.validate().unwrap();
        let toolchain = Toolchain {
            ffmpeg: "ffmpeg test".into(),
            encoder: "libmp3lame".into(),
            analyzer: ANALYZER_VERSION.into(),
        };
        let mut changed = base.clone();
        changed.trim.guard_leading_ms = 70.0;
        assert_ne!(
            config_sha256(&base, Mode::Normalize, &toolchain),
            config_sha256(&changed, Mode::Normalize, &toolchain)
        );
        assert_ne!(
            config_sha256(&base, Mode::Normalize, &toolchain),
            config_sha256(&base, Mode::Analyze, &toolchain)
        );
        for broken in [
            {
                let mut p = base.clone();
                p.trim.fade_ms = 100.0;
                p
            },
            {
                let mut p = base.clone();
                p.loudness.target_lufs = f64::NAN;
                p
            },
            {
                let mut p = base.clone();
                p.output.encoder = EncoderPolicy::Cbr { kbps: 100 };
                p
            },
            {
                let mut p = base.clone();
                p.id = "Balanced!".into();
                p
            },
        ] {
            assert!(broken.validate().is_err());
        }
        let json = serde_json::to_value(&base).unwrap();
        let mut unknown = json.clone();
        unknown["trim"]["aggressive"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NormalizerProfile>(unknown).is_err());
        assert_eq!(
            serde_json::from_value::<NormalizerProfile>(json).unwrap(),
            base
        );
    }

    #[test]
    fn trims_only_excess_edge_silence_with_guards() {
        let pcm = signal(&[
            (500.0, Kind::Silence, 0.0),
            (400.0, Kind::Tone, -20.0),
            (600.0, Kind::Silence, 0.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        let plan = &prepared.plan;
        assert!(
            (plan.leading.trim_ms - 440.0).abs() < 1.0,
            "{}",
            plan.leading.trim_ms
        );
        assert!(
            (plan.trailing.trim_ms - 520.0).abs() < 1.0,
            "{}",
            plan.trailing.trim_ms
        );
        assert_eq!(plan.leading.confidence, Confidence::High);
        assert!(plan.needs_processing);
        // Запасы 60/80 мс и вся речь сохранены.
        assert!((prepared.processed.duration_seconds() * 1000.0 - 540.0).abs() < 2.0);
        assert!((prepared.processed_analysis.active_ms - prepared.analysis.active_ms).abs() < 1e-9);
        assert!(
            prepared
                .reasons
                .iter()
                .all(|r| r.severity < Severity::Review)
        );
    }

    #[test]
    fn short_edge_silence_is_not_trimmed() {
        let pcm = signal(&[
            (100.0, Kind::Silence, 0.0),
            (600.0, Kind::Tone, -20.0),
            (120.0, Kind::Silence, 0.0),
        ]);
        let plan = prepare(&pcm, &profile()).plan;
        assert_eq!(plan.leading.trim_frames, 0);
        assert_eq!(plan.trailing.trim_frames, 0);
    }

    #[test]
    fn weak_consonant_adjacent_to_speech_is_kept() {
        // Шум −55 dBFS перед тоном: ниже порога речи, но в зоне слабого звука.
        let pcm = signal(&[
            (400.0, Kind::Silence, 0.0),
            (120.0, Kind::Noise, -55.0),
            (400.0, Kind::Tone, -20.0),
            (300.0, Kind::Silence, 0.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        let lead = &prepared.plan.leading;
        assert!((lead.quiet_ms - 400.0).abs() <= 10.0, "{}", lead.quiet_ms);
        assert!(lead.trim_ms <= 400.0 - 60.0 + 10.0);
        assert!(lead.removed_max_dbfs.unwrap() < -60.0);
        // Слабый звук остался внутри результата целиком.
        assert!(prepared.processed.duration_seconds() * 1000.0 >= 120.0 + 400.0 + 60.0);
    }

    #[test]
    fn separated_weak_activity_is_preserved_and_flagged() {
        let pcm = signal(&[
            (400.0, Kind::Silence, 0.0),
            (100.0, Kind::Noise, -57.0),
            (200.0, Kind::Silence, 0.0),
            (400.0, Kind::Tone, -20.0),
            (300.0, Kind::Silence, 0.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        assert!(codes(&prepared).contains(&"leading_weak_activity_kept"));
        assert!(prepared.plan.leading.weak_activity_kept);
        assert_eq!(prepared.plan.leading.confidence, Confidence::Medium);
        assert!((prepared.plan.leading.quiet_ms - 400.0).abs() <= 10.0);
    }

    #[test]
    fn internal_pause_like_sokuon_is_never_shortened() {
        let pcm = signal(&[
            (300.0, Kind::Silence, 0.0),
            (250.0, Kind::Tone, -20.0),
            (400.0, Kind::Silence, 0.0),
            (250.0, Kind::Tone, -20.0),
            (300.0, Kind::Silence, 0.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        assert!((prepared.analysis.longest_internal_pause_ms - 400.0).abs() <= 10.0);
        assert!(
            (prepared.processed_analysis.longest_internal_pause_ms
                - prepared.analysis.longest_internal_pause_ms)
                .abs()
                < 1e-9
        );
        let kept = prepared.processed.duration_seconds() * 1000.0;
        let removed = prepared.plan.leading.trim_ms + prepared.plan.trailing.trim_ms;
        assert!((kept + removed - 1500.0).abs() < 1.0);
        assert!((prepared.plan.leading.trim_ms - 240.0).abs() < 1.0);
    }

    #[test]
    fn noisy_edge_near_threshold_is_uncertain_and_not_trimmed() {
        let pcm = signal(&[
            (400.0, Kind::Noise, -63.0),
            (400.0, Kind::Tone, -20.0),
            (400.0, Kind::Noise, -63.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        assert_eq!(prepared.plan.leading.trim_frames, 0);
        assert_eq!(prepared.plan.leading.confidence, Confidence::Low);
        assert!(codes(&prepared).contains(&"uncertain_leading_boundary"));
        assert!(codes(&prepared).contains(&"uncertain_trailing_boundary"));
    }

    #[test]
    fn linear_gain_reaches_target_within_peak_and_noise_limits() {
        let quiet = signal(&[
            (100.0, Kind::Silence, 0.0),
            (1500.0, Kind::Tone, -35.0),
            (100.0, Kind::Silence, 0.0),
        ]);
        let prepared = prepare(&quiet, &profile());
        let gain = &prepared.plan.gain;
        assert_eq!(gain.method, LoudnessMethod::Integrated);
        assert!(
            gain.applied_db > 10.0 && gain.applied_db <= 12.0,
            "{}",
            gain.applied_db
        );
        let after = prepared.processed_analysis.integrated_lufs.unwrap();
        assert!((after - (gain.measured_lufs.unwrap() + gain.applied_db)).abs() < 0.05);
        // Линейное усиление сохраняет форму: пик растёт ровно на ту же величину.
        assert!(
            (prepared.processed_analysis.sample_peak_dbfs
                - prepared.analysis.sample_peak_dbfs
                - gain.applied_db)
                .abs()
                < 0.01
        );
        let loud = signal(&[(1500.0, Kind::Tone, -3.0)]);
        let prepared = prepare(&loud, &profile());
        assert!(prepared.plan.gain.applied_db < -15.0);
        // Шумная запись не усиливается выше потолка шумового пола.
        let noisy = signal(&[(1500.0, Kind::Noise, -45.0)]);
        let prepared = prepare(&noisy, &profile());
        assert_eq!(
            prepared.analysis.noise_floor_method,
            NoiseFloorMethod::AllWindowsP10
        );
        let gain = prepared.plan.gain;
        assert!(gain.limited_by.contains(&GainLimit::NoiseFloor) || gain.applied_db == 0.0);
        assert!(gain.applied_db <= 0.0);
    }

    #[test]
    fn true_peak_limits_gain_instead_of_compressing() {
        // Короткий громкий щелчок на фоне тихой речи: цель недостижима без лимитера.
        let mut pcm = signal(&[
            (100.0, Kind::Silence, 0.0),
            (1500.0, Kind::Tone, -40.0),
            (100.0, Kind::Silence, 0.0),
        ]);
        pcm.samples[22050] = 0.5;
        let prepared = prepare(&pcm, &profile());
        let gain = &prepared.plan.gain;
        assert!(
            gain.limited_by.contains(&GainLimit::TruePeak),
            "{:?}",
            gain.limited_by
        );
        assert!(prepared.processed_analysis.true_peak_dbtp <= -2.0 - 0.5 + 0.01);
        assert!(gain.shortfall_db.unwrap() > 6.0);
        assert!(codes(&prepared).contains(&"target_shortfall"));
    }

    #[test]
    fn already_good_source_needs_no_processing() {
        let pcm = signal(&[(1500.0, Kind::Tone, -20.3)]);
        let prepared = prepare(&pcm, &profile());
        assert!(!prepared.plan.needs_processing, "{:?}", prepared.plan.gain);
        assert_eq!(prepared.plan.gain.applied_db, 0.0);
    }

    #[test]
    fn silence_is_rejected_and_tiny_clip_needs_review() {
        let silence = signal(&[(800.0, Kind::Silence, 0.0)]);
        let prepared = prepare(&silence, &profile());
        assert!(prepared.rejected);
        assert!(codes(&prepared).contains(&"no_signal"));
        assert!(!prepared.plan.needs_processing);
        let tiny = signal(&[(80.0, Kind::Tone, -20.0)]);
        let prepared = prepare(&tiny, &profile());
        assert!(!prepared.rejected);
        assert!(codes(&prepared).contains(&"very_short_clip"));
        assert_ne!(prepared.plan.gain.method, LoudnessMethod::Integrated);
        assert!(prepared.analysis.integrated_lufs.is_none());
        for value in [
            prepared.analysis.true_peak_dbtp,
            prepared.analysis.noise_floor_dbfs,
            prepared.analysis.rms_dbfs,
        ] {
            assert!(value.is_finite());
        }
    }

    #[test]
    fn clipped_source_requires_review_and_is_not_amplified() {
        let mut pcm = signal(&[(1000.0, Kind::Tone, 3.0)]);
        for sample in &mut pcm.samples {
            *sample = sample.clamp(-1.0, 1.0);
        }
        let prepared = prepare(&pcm, &profile());
        assert!(codes(&prepared).contains(&"source_clipping"));
        assert!(prepared.plan.gain.applied_db < 0.0);
    }

    #[test]
    fn identical_stereo_is_downmixed_only_by_explicit_policy() {
        let mono = signal(&[(1000.0, Kind::Tone, -20.0)]);
        let stereo = Pcm {
            sample_rate: RATE,
            channels: 2,
            samples: mono.samples.iter().flat_map(|s| [*s, *s]).collect(),
        };
        assert_eq!(prepare(&stereo, &profile()).plan.output_channels, 2);
        let mut policy = profile();
        policy.output.channels = ChannelPolicy::MonoIfIdentical;
        let prepared = prepare(&stereo, &policy);
        assert_eq!(prepared.plan.output_channels, 1);
        assert!(prepared.plan.needs_processing);
    }
}
