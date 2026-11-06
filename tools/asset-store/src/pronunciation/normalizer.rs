//! Balanced Normalizer: анализ MP3, план бережной обработки и технический QualityGate.
//!
//! Обработка ограничена тремя операциями: удаление доказанно избыточной тишины
//! только по краям, одно линейное усиление и, по явной политике, сведение
//! идентичных каналов в моно. Внутренние паузы, темп, высота тона, динамика
//! и спектр не меняются. Итог проверяется по фактически декодированному
//! результату. `ACCEPT` означает техническую пригодность варианта, а не
//! правильность чтения: фонетику этот модуль не оценивает.
//!
//! QualityGate не требует человека: каждый вход получает машинное решение —
//! исход, выбранные байты (`proposed_variant`) и следующее автоматическое
//! действие (`next_action`). Техническая неопределённость, которую этап не
//! может устранить без риска для сигнала, фиксируется причинами важности
//! `limitation` с источником (`origin`) и откладывается до будущей
//! автоматической обработки, а не превращается в задание прослушать запись.

use serde::{Deserialize, Serialize};

use super::dsp::{self, Pcm};
use super::probe::{self, AudioProbe};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
/// Схема новых результатов. `2`: исходы с ограничениями, прогноз
/// `PROCESSING_PLANNED` режима analyze и обязательный `next_action`,
/// однозначно согласованный с исходом и выбранным вариантом.
pub const RESULT_SCHEMA_VERSION: u32 = 2;
/// Прежняя схема (`REVIEW_REQUIRED`, `next_action` мог отсутствовать):
/// результаты и свидетельства этой версии только читаются и проверяются по
/// своим правилам, новые не создаются.
pub const LEGACY_RESULT_SCHEMA_VERSION: u32 = 1;

fn unsupported_result_schema() -> AssetError {
    AssetError::new(
        ErrorCode::UnsupportedSchemaVersion,
        "неподдерживаемая версия схемы результата нормализации",
    )
}

fn inconsistent_gate(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

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

/// Окружение, от которого зависят байты и решение результата: сборки
/// `ffmpeg` и `ffprobe` с версиями библиотек, функциональный отпечаток
/// кодировщика libmp3lame и версия собственного анализатора.
///
/// Поле `ffprobe` появилось позже остальных: в записях прежних версий его нет,
/// оно читается пустым и не сериализуется, поэтому их отпечатки конфигурации
/// не меняются. `detect` всегда заполняет его, и такая конфигурация с прежней
/// не совпадает.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    pub ffmpeg: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ffprobe: String,
    pub encoder: String,
    pub analyzer: String,
}

impl Toolchain {
    pub fn detect() -> Result<Self, AssetError> {
        Ok(Self {
            ffmpeg: probe::ffmpeg_identity()?,
            ffprobe: probe::ffprobe_identity()?,
            encoder: probe::encoder_identity()?,
            analyzer: ANALYZER_VERSION.into(),
        })
    }
}

/// Версия собственных алгоритмов анализа, плана и QualityGate.
/// `/2`: пост-QC не пропускает неразрешимые проверки, громкость проверяется
/// в обе стороны от цели, выравнивание учитывает каналы по отдельности.
/// `/3`: автоматическая маршрутизация без `REVIEW_REQUIRED` (исходы с
/// ограничениями и `next_action`) и вход через техническую проверку MP3
/// версии 2 со структурным gapless-тегом.
pub const ANALYZER_VERSION: &str = "balanced-analyzer/3";

/// Отпечаток всего, что определяет новый результат для данного входа.
pub fn config_sha256(profile: &NormalizerProfile, mode: Mode, toolchain: &Toolchain) -> String {
    config_sha256_for(RESULT_SCHEMA_VERSION, profile, mode, toolchain)
}

/// Отпечаток по правилам указанной версии схемы результата. Версия входит в
/// хешируемые данные, поэтому сохранённые результаты и свидетельства
/// проверяются по своей версии: отпечатки прежней схемы не пересчитываются
/// новыми правилами и не теряют проверяемость.
pub fn config_sha256_for(
    schema_version: u32,
    profile: &NormalizerProfile,
    mode: Mode,
    toolchain: &Toolchain,
) -> String {
    sha256_hex(
        serde_json::to_vec(&serde_json::json!({
            "schema_version": schema_version,
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
    /// Поправка стробирования BS.1770: громкость подготовленного PCM минус
    /// «измерено + усиление». Абсолютный порог −70 LUFS не сдвигается вместе
    /// с усилением, поэтому у записи с долгими тихими участками ослабление
    /// исключает часть блоков и результат громче простого прогноза. Усиление
    /// уточняется по фактической громкости подготовленного PCM; `desired_db`
    /// и `shortfall_db` уже учитывают поправку.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gating_correction_db: Option<f64>,
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
    /// Обработанный вариант прошёл обязательный пост-QC, профиль достигнут.
    Accept,
    /// Обработанный вариант прошёл обязательный пост-QC и лучше исходника, но
    /// свойства исходника не дают достичь профиля целиком; ограничения
    /// перечислены причинами `limitation`.
    AcceptWithLimitations,
    /// Исходник уже в допусках профиля; кодирования не было.
    KeepOriginal,
    /// Сохранён исходник, который профилю не соответствует: обработка
    /// небезопасна, не получена или не прошла пост-QC, либо у исходника есть
    /// недостаток, который этап не исправляет. Ограничения перечислены.
    OriginalWithLimitations,
    /// Сигнал технически непригоден: MP3 не прошёл проверку, нет сигнала.
    Reject,
    /// Прогноз режима analyze: исходнику нужна обработка, но обработанного
    /// MP3 ещё нет и пост-QC не выполнялся. Это не решение о байтах:
    /// вариант не выбран, следующий шаг — `normalize`.
    ProcessingPlanned,
    /// Только чтение прежних результатов и свидетельств (`balanced-analyzer/2`
    /// и раньше): новые результаты этот исход не выдают.
    ReviewRequired,
}

impl GateOutcome {
    /// Исходы, которые выдаёт текущая версия QualityGate.
    pub const ALL: [GateOutcome; 6] = [
        GateOutcome::Accept,
        GateOutcome::AcceptWithLimitations,
        GateOutcome::KeepOriginal,
        GateOutcome::OriginalWithLimitations,
        GateOutcome::Reject,
        GateOutcome::ProcessingPlanned,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPT",
            Self::AcceptWithLimitations => "ACCEPT_WITH_LIMITATIONS",
            Self::KeepOriginal => "KEEP_ORIGINAL",
            Self::OriginalWithLimitations => "ORIGINAL_WITH_LIMITATIONS",
            Self::Reject => "REJECT",
            Self::ProcessingPlanned => "PROCESSING_PLANNED",
            Self::ReviewRequired => "REVIEW_REQUIRED",
        }
    }
}

/// Следующий автоматический шаг для входа. Ни один не требует человека.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    /// Технический кандидат — обработанный вариант.
    UseProcessed,
    /// Технический кандидат — исходник без перекодирования.
    UseOriginal,
    /// Выбранные байты (`proposed_variant`) — лучший доступный сейчас
    /// кандидат; ограничения переданы будущей автоматической обработке
    /// (шумоподавление, улучшение, ASR).
    DeferredAutomaticProcessing,
    /// Вход технически непригоден и в дальнейшую обработку не идёт.
    DiscardInput,
    /// Прогноз analyze: выбор байтов возможен только после `normalize` с
    /// пост-QC фактического результата.
    RunNormalize,
}

impl NextAction {
    pub const ALL: [NextAction; 5] = [
        NextAction::UseProcessed,
        NextAction::UseOriginal,
        NextAction::DeferredAutomaticProcessing,
        NextAction::DiscardInput,
        NextAction::RunNormalize,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UseProcessed => "use_processed",
            Self::UseOriginal => "use_original",
            Self::DeferredAutomaticProcessing => "deferred_automatic_processing",
            Self::DiscardInput => "discard_input",
            Self::RunNormalize => "run_normalize",
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
    /// Ограничение, которое этап не устраняет: исход `*_WITH_LIMITATIONS` и
    /// `next_action: deferred_automatic_processing`.
    Limitation,
    /// Только чтение прежних результатов (`balanced-analyzer/2` и раньше).
    Review,
    Reject,
}

/// Откуда взялась причина: свойство исходного сигнала, ненадёжное измерение,
/// предел политики профиля, отказ кодировщика или провал пост-QC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ReasonOrigin {
    SourceSignal,
    Measurement,
    ProfilePolicy,
    Encoder,
    PostQc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    /// Есть у каждой причины `limitation` и `reject` текущей версии.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ReasonOrigin>,
}

fn reason(code: &str, severity: Severity, message: impl Into<String>) -> Reason {
    Reason {
        code: code.into(),
        severity,
        message: message.into(),
        origin: None,
    }
}

/// Ограничение с источником: исход с ограничениями и отложенная автоматическая обработка.
fn limitation(code: &str, origin: ReasonOrigin, message: impl Into<String>) -> Reason {
    Reason {
        origin: Some(origin),
        ..reason(code, Severity::Limitation, message)
    }
}

fn rejection(code: &str, origin: ReasonOrigin, message: impl Into<String>) -> Reason {
    Reason {
        origin: Some(origin),
        ..reason(code, Severity::Reject, message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDecision {
    pub outcome: GateOutcome,
    /// Какие байты использовать дальше; `None` у `REJECT` и у прогноза
    /// `PROCESSING_PLANNED`, где байты ещё не выбраны.
    pub proposed_variant: Option<Variant>,
    /// Следующий автоматический шаг. У результатов схемы 1 мог отсутствовать.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextAction>,
    /// Пост-QC фактически декодированного результата выполнялся (пройден он
    /// или нет — см. `post_qc.passed`). `false` в режиме analyze.
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

impl GateDecision {
    /// Согласованность решения по правилам версии схемы результата: исход,
    /// выбранный вариант, следующее действие, состояние пост-QC и важность
    /// причин. Граница доверия для решений, созданных не штатным
    /// [`process`]: прочитанных, изменённых или собранных программно.
    pub fn validate(&self, schema_version: u32, mode: Mode) -> Result<(), AssetError> {
        match schema_version {
            RESULT_SCHEMA_VERSION => self.validate_current(mode),
            LEGACY_RESULT_SCHEMA_VERSION => self.validate_legacy(),
            _ => Err(unsupported_result_schema()),
        }
    }

    fn has(&self, severity: Severity) -> bool {
        self.reasons.iter().any(|r| r.severity == severity)
    }

    fn validate_current(&self, mode: Mode) -> Result<(), AssetError> {
        let limited = self.has(Severity::Limitation);
        let verified_normalize = mode == Mode::Normalize && self.post_qc_verified;
        let (variant, consistent) = match self.outcome {
            GateOutcome::Accept => (Some(Variant::Processed), !limited && verified_normalize),
            GateOutcome::AcceptWithLimitations => {
                (Some(Variant::Processed), limited && verified_normalize)
            }
            GateOutcome::KeepOriginal => {
                (Some(Variant::Original), !limited && !self.post_qc_verified)
            }
            GateOutcome::OriginalWithLimitations => (Some(Variant::Original), limited),
            GateOutcome::Reject => (None, !self.post_qc_verified),
            GateOutcome::ProcessingPlanned => {
                (None, mode == Mode::Analyze && !self.post_qc_verified)
            }
            GateOutcome::ReviewRequired => {
                return Err(inconsistent_gate(
                    "REVIEW_REQUIRED допустим только в результатах прежней схемы",
                ));
            }
        };
        if !consistent
            || self.proposed_variant != variant
            || self.next_action != next_action_for(self.outcome)
            || self.has(Severity::Review)
            || self.has(Severity::Reject) != (self.outcome == GateOutcome::Reject)
        {
            return Err(inconsistent_gate(format!(
                "решение QualityGate противоречиво: исход {}, вариант {:?}, действие {:?}, пост-QC {}",
                self.outcome.as_str(),
                self.proposed_variant,
                self.next_action.map(NextAction::as_str),
                self.post_qc_verified
            )));
        }
        Ok(())
    }

    /// Схема 1: `REVIEW_REQUIRED` и отсутствующий `next_action` допустимы,
    /// но выбранный вариант всё равно обязан соответствовать исходу.
    fn validate_legacy(&self) -> Result<(), AssetError> {
        let variant_ok = match self.outcome {
            GateOutcome::Accept | GateOutcome::AcceptWithLimitations => {
                self.proposed_variant == Some(Variant::Processed)
            }
            GateOutcome::KeepOriginal | GateOutcome::OriginalWithLimitations => {
                self.proposed_variant == Some(Variant::Original)
            }
            GateOutcome::Reject => self.proposed_variant.is_none(),
            GateOutcome::ReviewRequired => self.proposed_variant.is_some(),
            GateOutcome::ProcessingPlanned => false,
        };
        let action_ok =
            self.next_action.is_none() || self.next_action == next_action_for(self.outcome);
        if !variant_ok || !action_ok {
            return Err(inconsistent_gate(
                "решение QualityGate прежней схемы противоречиво",
            ));
        }
        Ok(())
    }
}

impl NormalizationResult {
    /// Граница доверия для прочитанного, изменённого или собранного
    /// результата: поддерживаемая версия схемы, согласованность решения и,
    /// в текущей схеме, наличие фактических данных, на которые оно ссылается.
    pub fn validate(&self) -> Result<(), AssetError> {
        self.gate.validate(self.schema_version, self.mode)?;
        if self.schema_version != RESULT_SCHEMA_VERSION {
            return Ok(());
        }
        let qc_passed = self.post_qc.as_ref().is_some_and(|q| q.passed);
        // Вариант и его пост-QC появляются только вместе (`conclude_post_qc`):
        // вариант без пост-QC и пост-QC без варианта — подделанный результат.
        let consistent = self.gate.post_qc_verified == self.post_qc.is_some()
            && self.output.is_some() == self.post_qc.is_some()
            && match self.gate.outcome {
                GateOutcome::Accept | GateOutcome::AcceptWithLimitations => {
                    self.output.is_some() && qc_passed
                }
                GateOutcome::OriginalWithLimitations => !qc_passed,
                _ => self.output.is_none() && self.post_qc.is_none(),
            };
        if !consistent {
            return Err(inconsistent_gate(
                "решение QualityGate не соответствует фактическому результату и пост-QC",
            ));
        }
        Ok(())
    }
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
        "phonetic_content_not_verified: соответствие чтению и фонетике этот этап не проверяет; это задача будущего этапа выбора записи, техническую обработку оно не останавливает".into(),
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
            reasons.push(limitation(
                &format!("uncertain_{name}_boundary"),
                ReasonOrigin::Measurement,
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
        reasons.push(limitation(
            "low_signal_margin",
            ReasonOrigin::Measurement,
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
        reasons.push(limitation(
            "loudness_unavailable",
            ReasonOrigin::Measurement,
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
            gating_correction_db: None,
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
    // `shortfall_db = desired − applied`: положительное значение — результат
    // останется тише цели, отрицательное — громче (ослабление ограничено).
    let shortfall = desired - applied;
    if shortfall.abs() > policy.review_shortfall_db {
        reasons.push(target_deviation_reason(shortfall, "по плану"));
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
        gating_correction_db: None,
    }
}

/// Наибольшее число уточнений усиления по фактической громкости.
const MAX_GAIN_REFINEMENTS: usize = 3;
/// Уточнение прекращается, когда усиление меняется меньше этого, dB.
const GAIN_REFINEMENT_EPSILON_DB: f64 = 0.01;

/// План усиления с уточнением по фактической громкости подготовленного PCM.
/// Пределы профиля (максимум усиления/ослабления, true peak, шумовой
/// потолок, допуск) применяются на каждом шаге тем же `plan_gain`;
/// в причины попадает только итоговый шаг.
fn refine_gain(
    trimmed: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
    mut render: impl FnMut(f64) -> (Pcm, SignalAnalysis),
) -> (GainPlan, Pcm, SignalAnalysis) {
    let mut step_reasons = Vec::new();
    let mut gain = plan_gain(trimmed, profile, &mut step_reasons);
    let (mut out, mut analysis) = render(gain.applied_db);
    for _ in 0..MAX_GAIN_REFINEMENTS {
        let (Some(measured), Some(actual)) =
            (gain.measured_lufs, loudness_by(&analysis, gain.method))
        else {
            break;
        };
        if gain.applied_db == 0.0 {
            break;
        }
        let correction = actual - (measured + gain.applied_db);
        let mut corrected = trimmed.clone();
        corrected.integrated_lufs = corrected.integrated_lufs.map(|v| v + correction);
        corrected.short_window_lufs = corrected.short_window_lufs.map(|v| v + correction);
        let mut next_reasons = Vec::new();
        let mut next = plan_gain(&corrected, profile, &mut next_reasons);
        next.measured_lufs = gain.measured_lufs;
        next.gating_correction_db = Some(correction);
        let changed = (next.applied_db - gain.applied_db).abs() >= GAIN_REFINEMENT_EPSILON_DB;
        gain = next;
        step_reasons = next_reasons;
        if !changed {
            break;
        }
        (out, analysis) = render(gain.applied_db);
    }
    reasons.extend(step_reasons);
    (gain, out, analysis)
}

/// Громкость тем же методом, каким она измерена в плане.
fn loudness_by(analysis: &SignalAnalysis, method: LoudnessMethod) -> Option<f64> {
    match method {
        LoudnessMethod::Integrated => analysis.integrated_lufs.or(analysis.short_window_lufs),
        _ => analysis.short_window_lufs,
    }
}

/// Отклонение громкости от цели больше допуска профиля в любую сторону.
/// `deviation` — «цель минус результат»: больше нуля — тише цели.
fn target_deviation_reason(deviation: f64, basis: &str) -> Reason {
    if deviation > 0.0 {
        limitation(
            "target_shortfall",
            ReasonOrigin::ProfilePolicy,
            format!(
                "цель недостижима без нарушения ограничений: {basis} результат тише цели на {deviation:.1} dB"
            ),
        )
    } else {
        limitation(
            "target_exceeded",
            ReasonOrigin::ProfilePolicy,
            format!(
                "ослабление ограничено профилем: {basis} результат громче цели на {:.1} dB",
                -deviation
            ),
        )
    }
}

fn source_reasons(
    analysis: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) {
    let a = &profile.analysis;
    if analysis.duration_ms < a.min_duration_ms {
        reasons.push(limitation(
            "very_short_clip",
            ReasonOrigin::SourceSignal,
            "очень короткая запись: возможно усечение слова",
        ));
    }
    if analysis.clipping.near_full_scale_ratio > profile.gate.review_clip_ratio
        || analysis.clipping.clip_runs > 0
    {
        reasons.push(limitation(
            "source_clipping",
            ReasonOrigin::SourceSignal,
            "в исходнике есть признаки перегрузки; этап их не исправляет",
        ));
    }
    if analysis.true_peak_dbtp > profile.loudness.true_peak_ceiling_dbtp {
        reasons.push(reason(
            SOURCE_PEAK_CODE,
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

const SOURCE_PEAK_CODE: &str = "source_true_peak_above_ceiling";

/// Сохраняемый исходник выше потолка true peak профиля профилю не
/// соответствует: диагностическое предупреждение становится ограничением.
/// Ослаблять запись только ради потолка этап не обязан, поэтому исход —
/// исходник с ограничением, а не принудительное перекодирование.
fn original_ceiling_limitation(
    analysis: &SignalAnalysis,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) {
    if analysis.true_peak_dbtp > profile.loudness.true_peak_ceiling_dbtp {
        reasons.retain(|r| r.code != SOURCE_PEAK_CODE);
        reasons.push(limitation(
            SOURCE_PEAK_CODE,
            ReasonOrigin::SourceSignal,
            "сохраняемый исходник выше потолка true peak профиля; ослабление только ради потолка не выполняется",
        ));
    }
}

/// Обработанный вариант, унаследовавший превышение потолка (пост-QC
/// допускает его, если операции начинались выше потолка), тоже профилю не
/// соответствует. Вариант в пределах применимого потолка ограничения не
/// получает, даже если исходник был выше.
fn processed_ceiling_limitation(
    true_peak_dbtp: f64,
    profile: &NormalizerProfile,
    reasons: &mut Vec<Reason>,
) {
    let limit = profile.loudness.true_peak_ceiling_dbtp + profile.gate.max_true_peak_overshoot_db;
    if true_peak_dbtp > limit {
        reasons.push(limitation(
            "output_true_peak_above_ceiling",
            ReasonOrigin::SourceSignal,
            "обработанный вариант выше потолка true peak: исходник уже был выше, ослабление только ради потолка не выполняется",
        ));
    }
}

/// Наибольшая допустимая потеря энергии при сведении стерео в моно. У
/// одинаковых каналов потеря 0 dB, у некоррелированных — около 3 dB, у
/// противофазных сигнал исчезает.
const MAX_DOWNMIX_LOSS_DB: f64 = 6.0;

/// Средняя энергия канала источника относительно энергии сведённого моно, dB.
fn downmix_loss_db(stereo: &Pcm, mono: &Pcm) -> f64 {
    let source: f64 = stereo
        .samples
        .iter()
        .map(|s| f64::from(*s).powi(2))
        .sum::<f64>()
        / stereo.channels.max(1) as f64;
    let mixed: f64 = mono.samples.iter().map(|s| f64::from(*s).powi(2)).sum();
    if source <= 0.0 {
        0.0
    } else if mixed <= 0.0 {
        f64::INFINITY
    } else {
        10.0 * (source / mixed).log10()
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
        reasons.push(rejection(
            "no_signal",
            ReasonOrigin::SourceSignal,
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
    if pcm.channels == 2 && !rejected {
        let identical = analysis
            .stereo_difference_db
            .is_some_and(|d| d <= profile.output.identical_channels_max_db);
        if profile.output.channels == ChannelPolicy::Mono
            || (profile.output.channels == ChannelPolicy::MonoIfIdentical && identical)
        {
            let mixed = downmix(&working);
            // Сведение противофазных каналов уничтожило бы сигнал: это не
            // обоснованная политика вывода, а потеря содержимого.
            if downmix_loss_db(&working, &mixed) > MAX_DOWNMIX_LOSS_DB {
                reasons.push(limitation(
                    "downmix_phase_cancellation",
                    ReasonOrigin::SourceSignal,
                    format!(
                        "сведение в моно ослабило бы сигнал больше чем на {MAX_DOWNMIX_LOSS_DB:.0} dB (каналы в противофазе): раскладка источника сохранена"
                    ),
                ));
            } else {
                working = mixed;
                channels_out = 1;
            }
        }
    }
    let trimmed_analysis = analyze_pcm(&working, profile);
    let fade = pcm.samples_for_ms(profile.trim.fade_ms);
    let fade_in = if start > 0 { fade } else { 0 };
    let fade_out = if trailing.trim_frames > 0 { fade } else { 0 };
    let render = |gain_db: f64| {
        let mut out = working.clone();
        dsp::apply_gain_and_fades(&mut out, dsp::db_to_gain(gain_db), fade_in, fade_out);
        out
    };
    let (gain, working, processed_analysis) = if rejected {
        let gain = GainPlan {
            method: LoudnessMethod::Unavailable,
            measured_lufs: None,
            desired_db: None,
            applied_db: 0.0,
            limited_by: vec![GainLimit::LoudnessUnavailable],
            shortfall_db: None,
            confidence: Confidence::Low,
            gating_correction_db: None,
        };
        let out = render(0.0);
        let analysis = analyze_pcm(&out, profile);
        (gain, out, analysis)
    } else {
        refine_gain(&trimmed_analysis, profile, &mut reasons, |gain_db| {
            let out = render(gain_db);
            let analysis = analyze_pcm(&out, profile);
            (out, analysis)
        })
    };
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
    // Ожидаемая громкость — фактически измеренная у подготовленного PCM тем
    // же методом, а не «измерено + усиление»: пост-QC сравнивает результат
    // с эталоном и видит только влияние кодирования.
    let expected_lufs = gain
        .measured_lufs
        .and(loudness_by(&processed_analysis, gain.method));
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

fn unresolved_check(id: &str, limit: Option<f64>) -> Check {
    Check {
        id: id.into(),
        passed: false,
        value: None,
        limit,
    }
}

/// Пост-QC сравнивает фактически декодированный результат с эталоном —
/// тем PCM, который получил кодировщик (при фиксированной частоте вывода —
/// уже пересэмплированным). Каждая обязательная проверка присутствует в
/// `checks`: если величину нельзя корректно вычислить, проверка остаётся
/// с `passed: false` и `value: null` и не даёт `ACCEPT`.
pub fn post_qc(
    reference: &Pcm,
    reference_analysis: &SignalAnalysis,
    plan: &Plan,
    output: &[u8],
    profile: &NormalizerProfile,
) -> Result<(PostQc, Option<AudioProbe>), AssetError> {
    let gate = &profile.gate;
    let ceiling = profile.loudness.true_peak_ceiling_dbtp + gate.max_true_peak_overshoot_db;
    // Потолок обязателен, только если сами операции не начинались выше него.
    let tp_limit = ceiling.max(reference_analysis.true_peak_dbtp + gate.max_true_peak_overshoot_db);
    let active_limit = gate
        .max_active_change_ms
        .max(reference_analysis.active_ms * gate.max_active_change_ratio);
    let threshold = profile.trim.silence_threshold_dbfs;
    // Ожидаемая громкость — измеренная у самого эталона тем же методом:
    // проверка видит влияние кодирования, а не погрешность прогноза плана.
    let planned = plan
        .gain
        .measured_lufs
        .map(|_| actual_loudness(reference_analysis, plan));
    // Результат не прошёл техническую проверку или не декодировался:
    // остальные обязательные проверки не выполнимы и остаются неразрешёнными,
    // но присутствуют в `checks` с теми же пределами, что и при измерении.
    let failed = |failed_id: &str, probe_passed: bool| {
        let mut checks = Vec::new();
        if probe_passed {
            checks.push(check("output_technical_probe", true, None, None));
        }
        checks.push(check(failed_id, false, None, None));
        checks.push(unresolved_check("output_layout", None));
        checks.push(unresolved_check(
            "duration_matches_plan",
            Some(gate.max_duration_error_ms),
        ));
        checks.push(unresolved_check("true_peak_within_ceiling", Some(tp_limit)));
        checks.push(unresolved_check(
            "no_new_clipping",
            Some(gate.max_new_clipped_samples as f64),
        ));
        if planned.is_some() {
            checks.push(unresolved_check(
                "loudness_matches_plan",
                Some(gate.max_loudness_error_db),
            ));
        }
        if plan.leading.trim_frames > 0 {
            checks.push(unresolved_check("leading_edge_quiet", Some(threshold)));
        }
        if plan.trailing.trim_frames > 0 {
            checks.push(unresolved_check("trailing_edge_quiet", Some(threshold)));
        }
        checks.push(unresolved_check(
            "active_duration_preserved",
            Some(active_limit),
        ));
        checks.push(unresolved_check(
            "alignment_lag",
            Some(gate.max_alignment_lag_ms),
        ));
        checks.push(unresolved_check("codec_snr", Some(gate.min_codec_snr_db)));
        PostQc {
            passed: false,
            checks,
            analysis: None,
            alignment_lag_frames: None,
            codec_snr_db: None,
        }
    };
    let probe = match probe::probe_mp3(output) {
        Ok(probe) => probe,
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(_) => return Ok((failed("output_technical_probe", false), None)),
    };
    let samples = match probe::decode_f32(output, &probe) {
        Ok(samples) => samples,
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(_) => return Ok((failed("output_decode", true), Some(probe))),
    };
    let decoded = Pcm {
        sample_rate: probe.sample_rate,
        channels: probe.channels as usize,
        samples,
    };
    let after = analyze_pcm(&decoded, profile);
    let mut checks = vec![check("output_technical_probe", true, None, None)];
    let comparable =
        decoded.sample_rate == reference.sample_rate && decoded.channels == reference.channels;
    checks.push(check(
        "output_layout",
        probe.channels == plan.output_channels
            && probe.sample_rate == plan.output_sample_rate
            && comparable,
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
    checks.push(check(
        "true_peak_within_ceiling",
        after.true_peak_dbtp <= tp_limit,
        Some(after.true_peak_dbtp),
        Some(tp_limit),
    ));
    let new_clipped = after
        .clipping
        .near_full_scale_samples
        .saturating_sub(reference_analysis.clipping.near_full_scale_samples);
    checks.push(check(
        "no_new_clipping",
        new_clipped <= gate.max_new_clipped_samples,
        Some(new_clipped as f64),
        Some(gate.max_new_clipped_samples as f64),
    ));
    // Громкость сверяется с планом по фактически декодированному результату;
    // если план громкость задал, а измерить её у результата нельзя, проверка
    // неразрешима и не проходит.
    let actual = actual_loudness(&after, plan);
    if let Some(planned) = planned {
        checks.push(match actual.zip(planned) {
            Some((actual, planned)) => {
                let error = (actual - planned).abs();
                check(
                    "loudness_matches_plan",
                    error <= gate.max_loudness_error_db,
                    Some(error),
                    Some(gate.max_loudness_error_db),
                )
            }
            None => unresolved_check("loudness_matches_plan", Some(gate.max_loudness_error_db)),
        });
    }
    let edge_frame = decoded.samples_for_ms(profile.analysis.frame_ms).max(1);
    let edge_levels = dsp::frame_levels(&decoded, edge_frame);
    if plan.leading.trim_frames > 0 {
        let first = edge_levels.first().copied().unwrap_or(dsp::LEVEL_FLOOR_DB);
        checks.push(check(
            "leading_edge_quiet",
            first < threshold,
            Some(first),
            Some(threshold),
        ));
    }
    if plan.trailing.trim_frames > 0 {
        let last = edge_levels.last().copied().unwrap_or(dsp::LEVEL_FLOOR_DB);
        checks.push(check(
            "trailing_edge_quiet",
            last < threshold,
            Some(last),
            Some(threshold),
        ));
    }
    // Активная часть сравнивается с эталоном в том же масштабе,
    // поэтому усиление само по себе не меняет её длительность.
    let active_change = (after.active_ms - reference_analysis.active_ms).abs();
    checks.push(check(
        "active_duration_preserved",
        active_change <= active_limit,
        Some(active_change),
        Some(active_limit),
    ));
    let alignment = if comparable {
        let max_lag = reference.samples_for_ms(gate.max_alignment_lag_ms.max(30.0));
        dsp::aligned_snr(reference, &decoded, max_lag)
    } else {
        None
    };
    let (lag, snr) = match alignment {
        Some(alignment) => {
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
            (Some(alignment.lag_frames), Some(alignment.snr_db))
        }
        None => {
            checks.push(unresolved_check(
                "alignment_lag",
                Some(gate.max_alignment_lag_ms),
            ));
            checks.push(unresolved_check("codec_snr", Some(gate.min_codec_snr_db)));
            (None, None)
        }
    };
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

/// Громкость результата тем же методом, которым она измерена в плане.
fn actual_loudness(after: &SignalAnalysis, plan: &Plan) -> Option<f64> {
    loudness_by(after, plan.gain.method)
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
            next_action: Some(NextAction::DiscardInput),
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
    let probe = match super::probe_mp3(bytes) {
        Ok(probe) => Ok(probe),
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(error) => Err(error),
    };
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            let mut result = base(InputRef {
                error: Some(error.message.clone()),
                ..input
            });
            result.gate.reasons.push(rejection(
                "source_invalid",
                ReasonOrigin::SourceSignal,
                format!("техническая проверка MP3 не пройдена: {}", error.message),
            ));
            return Ok(Processed {
                result,
                output_bytes: None,
            });
        }
    };
    // Повторное декодирование уже проверенного входа: отказ декодера — свойство
    // файла (исход `REJECT`), а сбой среды — ошибка для повтора.
    let samples = match probe::decode_f32(bytes, &probe) {
        Ok(samples) => samples,
        Err(error) if is_infrastructure_failure(&error) => return Err(error),
        Err(error) => {
            let mut result = base(InputRef {
                probe: Some(probe),
                error: Some(error.message.clone()),
                ..input
            });
            result.gate.reasons.push(rejection(
                "source_invalid",
                ReasonOrigin::SourceSignal,
                format!("декодирование MP3 не выполнено: {}", error.message),
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
        samples,
    };
    let prepared = prepare(&pcm, profile);
    let mut result = base(InputRef {
        probe: Some(probe),
        ..input
    });
    result.analysis = Some(prepared.analysis.clone());
    result.plan = Some(prepared.plan.clone());
    let mut reasons = prepared.reasons.clone();
    let (outcome, variant) = if prepared.rejected {
        route(&reasons, Route::Rejected)
    } else if !prepared.plan.needs_processing {
        original_ceiling_limitation(&prepared.analysis, profile, &mut reasons);
        if !has_limitation(&reasons) {
            reasons.push(reason(
                "already_within_profile",
                Severity::Info,
                "исходник уже в допусках профиля: перекодирование не нужно",
            ));
        }
        route(&reasons, Route::Unchanged)
    } else if mode == Mode::Analyze {
        processed_ceiling_limitation(prepared.plan.expected.true_peak_dbtp, profile, &mut reasons);
        route(&reasons, Route::Forecast)
    } else {
        // Частота вывода меняется явным шагом до кодирования: кодировщик и
        // пост-QC получают один и тот же эталон на частоте вывода.
        let resampled = if prepared.plan.output_sample_rate != prepared.processed.sample_rate {
            match probe::resample_f32(
                &prepared.processed.samples,
                prepared.processed.sample_rate,
                prepared.plan.output_channels,
                prepared.plan.output_sample_rate,
            ) {
                Ok(samples) => {
                    let pcm = Pcm {
                        sample_rate: prepared.plan.output_sample_rate,
                        channels: prepared.processed.channels,
                        samples,
                    };
                    let analysis = analyze_pcm(&pcm, profile);
                    Some(Ok((pcm, analysis)))
                }
                Err(error) if is_infrastructure_failure(&error) => return Err(error),
                Err(error) => Some(Err(error)),
            }
        } else {
            None
        };
        let (reference, reference_analysis) = match &resampled {
            None => (&prepared.processed, &prepared.processed_analysis),
            Some(Ok((pcm, analysis))) => (pcm, analysis),
            Some(Err(error)) => {
                reasons.push(limitation(
                    "encode_failed",
                    ReasonOrigin::Encoder,
                    format!(
                        "пересэмплирование не выполнено: {}; сохранён исходник",
                        error.message
                    ),
                ));
                original_ceiling_limitation(&prepared.analysis, profile, &mut reasons);
                let (outcome, variant) = route(&reasons, Route::VariantDiscarded);
                set_gate(&mut result.gate, outcome, variant);
                result.gate.reasons = sorted(reasons);
                return Ok(Processed {
                    result,
                    output_bytes: None,
                });
            }
        };
        let encoded = probe::encode_mp3(
            &reference.samples,
            reference.sample_rate,
            prepared.plan.output_channels,
            prepared.plan.output_sample_rate,
            &profile.encoder_args(),
        );
        let encoded = match encoded {
            Ok(bytes) => bytes,
            Err(error) if is_infrastructure_failure(&error) => return Err(error),
            Err(error) => {
                reasons.push(limitation(
                    "encode_failed",
                    ReasonOrigin::Encoder,
                    format!(
                        "кодирование не выполнено: {}; сохранён исходник",
                        error.message
                    ),
                ));
                original_ceiling_limitation(&prepared.analysis, profile, &mut reasons);
                let (outcome, variant) = route(&reasons, Route::VariantDiscarded);
                set_gate(&mut result.gate, outcome, variant);
                result.gate.reasons = sorted(reasons);
                return Ok(Processed {
                    result,
                    output_bytes: None,
                });
            }
        };
        let (qc, output_probe) = post_qc(
            reference,
            reference_analysis,
            &prepared.plan,
            &encoded,
            profile,
        )?;
        return Ok(conclude_post_qc(
            result,
            reasons,
            PostQcOutcome {
                qc,
                output_probe,
                encoded,
                source_analysis: &prepared.analysis,
            },
            profile,
        ));
    };
    set_gate(&mut result.gate, outcome, variant);
    result.gate.reasons = sorted(reasons);
    Ok(Processed {
        result,
        output_bytes: None,
    })
}

/// Фактический результат кодирования и его пост-QC.
pub(crate) struct PostQcOutcome<'a> {
    pub qc: PostQc,
    pub output_probe: Option<AudioProbe>,
    pub encoded: Vec<u8>,
    pub source_analysis: &'a SignalAnalysis,
}

/// Решение после пост-QC: единственное место, где итог обязательных
/// проверок выбирает вариант. Вариант с проваленным пост-QC остаётся только
/// диагностикой (сравнение в отчёте): он не предлагается и не публикуется,
/// сохраняется исходник с ограничением.
pub(crate) fn conclude_post_qc(
    mut result: NormalizationResult,
    mut reasons: Vec<Reason>,
    outcome: PostQcOutcome<'_>,
    profile: &NormalizerProfile,
) -> Processed {
    let PostQcOutcome {
        qc,
        output_probe,
        encoded,
        source_analysis,
    } = outcome;
    let plan = result
        .plan
        .as_ref()
        .expect("план есть у закодированного варианта");
    // Фактическая громкость результата сверяется и с целью: план мог
    // уложиться в допуск, а декодированный вывод — нет.
    if let Some(actual) = qc
        .analysis
        .as_ref()
        .and_then(|after| actual_loudness(after, plan))
    {
        let deviation = profile.loudness.target_lufs - actual;
        if deviation.abs() > profile.loudness.review_shortfall_db {
            reasons.push(target_deviation_reason(deviation, "фактически"));
        }
    }
    let passed = qc.passed;
    reasons.extend(post_qc_reasons(&qc));
    let state = if passed {
        if let Some(after) = &qc.analysis {
            processed_ceiling_limitation(after.true_peak_dbtp, profile, &mut reasons);
        }
        Route::Verified
    } else {
        original_ceiling_limitation(source_analysis, profile, &mut reasons);
        Route::VariantDiscarded
    };
    result.output = Some(OutputRef {
        sha256: sha256_hex(&encoded),
        byte_length: encoded.len() as u64,
        probe: output_probe,
        storage_path: None,
    });
    result.gate.post_qc_verified = true;
    result.post_qc = Some(qc);
    let (outcome, variant) = route(&reasons, state);
    set_gate(&mut result.gate, outcome, variant);
    result.gate.reasons = sorted(reasons);
    Processed {
        result,
        output_bytes: Some(encoded),
    }
}

/// Каждая непройденная или неразрешимая обязательная проверка — отдельное
/// ограничение с источником `post_qc`.
fn post_qc_reasons(qc: &PostQc) -> Vec<Reason> {
    qc.checks
        .iter()
        .filter(|c| !c.passed)
        .map(|failed| {
            limitation(
                &format!("post_qc_{}", failed.id),
                ReasonOrigin::PostQc,
                if failed.value.is_none() && failed.limit.is_some() {
                    "проверка фактического результата неразрешима (величину нельзя корректно вычислить); вариант отброшен, сохранён исходник"
                } else {
                    "фактический результат не прошёл проверку; вариант отброшен, сохранён исходник"
                },
            )
        })
        .collect()
}

fn has_limitation(reasons: &[Reason]) -> bool {
    reasons.iter().any(|r| r.severity == Severity::Limitation)
}

/// Состояние обработки входа в момент решения QualityGate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Сигнал непригоден (`no_signal`).
    Rejected,
    /// Обработка не нужна: план ничего не меняет.
    Unchanged,
    /// Режим `analyze`: план без фактического результата. Даёт прогноз
    /// `PROCESSING_PLANNED`, а не решение о байтах.
    Forecast,
    /// Вариант получен и прошёл весь обязательный пост-QC.
    Verified,
    /// Вариант не получен (кодирование, пересэмплирование) или не прошёл
    /// пост-QC: он отброшен, сохраняется исходник.
    VariantDiscarded,
}

/// Решение QualityGate: исход и выбранные байты. Ограничение никогда не
/// превращается в `ACCEPT`/`KEEP_ORIGINAL`, отброшенный вариант — в
/// выбранный, а прогноз analyze — в решение о несуществующих байтах;
/// человек ни в одном исходе не нужен.
pub fn route(reasons: &[Reason], state: Route) -> (GateOutcome, Option<Variant>) {
    let limited = has_limitation(reasons);
    match state {
        Route::Rejected => (GateOutcome::Reject, None),
        Route::Unchanged if !limited => (GateOutcome::KeepOriginal, Some(Variant::Original)),
        Route::Unchanged | Route::VariantDiscarded => (
            GateOutcome::OriginalWithLimitations,
            Some(Variant::Original),
        ),
        Route::Forecast => (GateOutcome::ProcessingPlanned, None),
        Route::Verified if limited => {
            (GateOutcome::AcceptWithLimitations, Some(Variant::Processed))
        }
        Route::Verified => (GateOutcome::Accept, Some(Variant::Processed)),
    }
}

/// Следующее автоматическое действие однозначно следует из исхода.
pub fn next_action_for(outcome: GateOutcome) -> Option<NextAction> {
    match outcome {
        GateOutcome::Accept => Some(NextAction::UseProcessed),
        GateOutcome::KeepOriginal => Some(NextAction::UseOriginal),
        GateOutcome::AcceptWithLimitations | GateOutcome::OriginalWithLimitations => {
            Some(NextAction::DeferredAutomaticProcessing)
        }
        GateOutcome::Reject => Some(NextAction::DiscardInput),
        GateOutcome::ProcessingPlanned => Some(NextAction::RunNormalize),
        GateOutcome::ReviewRequired => None,
    }
}

fn set_gate(gate: &mut GateDecision, outcome: GateOutcome, variant: Option<Variant>) {
    gate.outcome = outcome;
    gate.proposed_variant = variant;
    gate.next_action = next_action_for(outcome);
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
            ffprobe: "ffprobe test".into(),
            encoder: "libmp3lame sha256:0000000000000000".into(),
            analyzer: ANALYZER_VERSION.into(),
        };
        // Любая составляющая окружения меняет отпечаток.
        for other in [
            Toolchain {
                ffprobe: "ffprobe other".into(),
                ..toolchain.clone()
            },
            Toolchain {
                encoder: "libmp3lame sha256:1111111111111111".into(),
                ..toolchain.clone()
            },
        ] {
            assert_ne!(
                config_sha256(&base, Mode::Normalize, &toolchain),
                config_sha256(&base, Mode::Normalize, &other)
            );
        }
        // Запись прежней версии без `ffprobe` читается и сохраняет свой отпечаток.
        let legacy: Toolchain = serde_json::from_value(serde_json::json!({
            "ffmpeg": "ffmpeg test", "encoder": "libmp3lame", "analyzer": "balanced-analyzer/1"
        }))
        .unwrap();
        assert!(legacy.ffprobe.is_empty());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            serde_json::json!({"ffmpeg": "ffmpeg test", "encoder": "libmp3lame", "analyzer": "balanced-analyzer/1"})
        );
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
                .all(|r| r.severity < Severity::Limitation)
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

    /// Слишком громкий источник при пределе ослабления: отклонение вверх от
    /// цели диагностируется так же, как недобор, с отрицательным `shortfall_db`.
    #[test]
    fn loud_source_beyond_attenuation_limit_requires_review() {
        let pcm = signal(&[
            (200.0, Kind::Silence, 0.0),
            (1200.0, Kind::Tone, -3.0),
            (200.0, Kind::Silence, 0.0),
        ]);
        let mut policy = profile();
        policy.loudness.max_attenuation_db = 5.0;
        let prepared = prepare(&pcm, &policy);
        let gain = &prepared.plan.gain;
        assert_eq!(gain.applied_db, -5.0);
        assert!(gain.limited_by.contains(&GainLimit::MaxAttenuation));
        let shortfall = gain.shortfall_db.unwrap();
        assert!(shortfall < -6.0, "{shortfall}");
        assert!(codes(&prepared).contains(&"target_exceeded"));
        assert!(!codes(&prepared).contains(&"target_shortfall"));
        // В пределах ослабления цель достигается без причины для прослушивания.
        let prepared = prepare(&pcm, &profile());
        assert!(prepared.plan.gain.shortfall_db.unwrap().abs() < 1e-9);
        assert!(!codes(&prepared).contains(&"target_exceeded"));
    }

    /// Принудительное моно не уничтожает противофазное стерео.
    #[test]
    fn opposite_phase_stereo_is_not_destroyed_by_mono_policy() {
        let mono = signal(&[
            (200.0, Kind::Silence, 0.0),
            (800.0, Kind::Tone, -20.0),
            (200.0, Kind::Silence, 0.0),
        ]);
        let stereo = Pcm {
            sample_rate: RATE,
            channels: 2,
            samples: mono.samples.iter().flat_map(|s| [*s, -*s]).collect(),
        };
        let mut policy = profile();
        policy.output.channels = ChannelPolicy::Mono;
        let prepared = prepare(&stereo, &policy);
        assert_eq!(prepared.plan.output_channels, 2);
        assert!(codes(&prepared).contains(&"downmix_phase_cancellation"));
        // Обычное стерео сводится, как требует политика.
        let normal = Pcm {
            sample_rate: RATE,
            channels: 2,
            samples: mono.samples.iter().flat_map(|s| [*s, *s * 0.5]).collect(),
        };
        assert_eq!(prepare(&normal, &policy).plan.output_channels, 1);
    }

    /// Речеподобный сигнал: слабое начало-шум, два «слога» с внутренней паузой,
    /// шумовой хвост, краевая тишина.
    fn speech_like(channels: usize, opposite_phase: bool) -> Pcm {
        let mono = signal(&[
            (400.0, Kind::Silence, 0.0),
            (40.0, Kind::Noise, -45.0),
            (300.0, Kind::Tone, -20.0),
            (120.0, Kind::Silence, 0.0),
            (250.0, Kind::Tone, -24.0),
            (60.0, Kind::Noise, -40.0),
            (500.0, Kind::Silence, 0.0),
        ]);
        if channels == 1 {
            return mono;
        }
        Pcm {
            sample_rate: RATE,
            channels: 2,
            samples: mono
                .samples
                .iter()
                .flat_map(|s| [*s, if opposite_phase { -*s } else { *s * 0.7 }])
                .collect(),
        }
    }

    fn mp3(pcm: &Pcm) -> Vec<u8> {
        probe::encode_mp3(
            &pcm.samples,
            pcm.sample_rate,
            pcm.channels as u32,
            pcm.sample_rate,
            &["-q:a".into(), "2".into()],
        )
        .expect("для теста нужен FFmpeg с libmp3lame")
    }

    fn check_of<'a>(result: &'a NormalizationResult, id: &str) -> &'a Check {
        result
            .post_qc
            .as_ref()
            .expect("пост-QC выполнен")
            .checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("нет обязательной проверки {id}"))
    }

    /// Разрешённые частоты и раскладки вывода проходят полную сопоставимую
    /// проверку: сдвиг и SNR вычислены по фактически декодированному выводу.
    #[test]
    fn allowed_rate_and_channel_policies_keep_alignment_and_snr_checks() {
        let tools = Toolchain::detect().expect("для теста нужен FFmpeg");
        let cases = [
            (1, false, SampleRatePolicy::Source, ChannelPolicy::Source),
            (
                1,
                false,
                SampleRatePolicy::Fixed { hz: 48000 },
                ChannelPolicy::Source,
            ),
            (
                2,
                false,
                SampleRatePolicy::Fixed { hz: 22050 },
                ChannelPolicy::Source,
            ),
            (2, false, SampleRatePolicy::Source, ChannelPolicy::Mono),
            (2, true, SampleRatePolicy::Source, ChannelPolicy::Source),
        ];
        for (channels, opposite, rate, layout) in cases {
            let bytes = mp3(&speech_like(channels, opposite));
            let mut policy = profile();
            policy.output.sample_rate = rate.clone();
            policy.output.channels = layout;
            let processed = process(&bytes, &policy, Mode::Normalize, &tools).unwrap();
            let result = &processed.result;
            let label = format!("{channels} {opposite} {rate:?} {layout:?}");
            let qc = result.post_qc.as_ref().unwrap_or_else(|| panic!("{label}"));
            for id in ["output_layout", "alignment_lag", "codec_snr"] {
                let check = check_of(result, id);
                assert!(check.passed, "{label}: {:#?}", qc.checks);
                if id != "output_layout" {
                    assert!(check.value.is_some_and(f64::is_finite), "{label}");
                }
            }
            assert!(qc.passed, "{label}: {:#?}", qc.checks);
            assert_eq!(
                result.gate.outcome,
                GateOutcome::Accept,
                "{label}: {:#?}",
                result.gate
            );
            let probe = result.output.as_ref().unwrap().probe.as_ref().unwrap();
            let expected_rate = match rate {
                SampleRatePolicy::Fixed { hz } => hz,
                SampleRatePolicy::Source => RATE,
            };
            assert_eq!(probe.sample_rate, expected_rate, "{label}");
            assert_eq!(
                probe.channels,
                result.plan.as_ref().unwrap().output_channels
            );
        }
    }

    /// Несопоставимый эталон: обязательные проверки остаются в списке как
    /// неразрешимые, и пост-QC не пройден.
    #[test]
    fn incomparable_reference_leaves_mandatory_checks_unresolved() {
        let pcm = speech_like(1, false);
        let policy = profile();
        let prepared = prepare(&pcm, &policy);
        let encoded = mp3(&prepared.processed);
        let mislabeled = Pcm {
            sample_rate: 48000,
            ..prepared.processed.clone()
        };
        let (qc, _) = post_qc(
            &mislabeled,
            &prepared.processed_analysis,
            &prepared.plan,
            &encoded,
            &policy,
        )
        .unwrap();
        assert!(!qc.passed);
        for id in ["alignment_lag", "codec_snr"] {
            let check = qc.checks.iter().find(|c| c.id == id).unwrap();
            assert!(!check.passed && check.value.is_none() && check.limit.is_some());
        }
        assert!(
            !qc.checks
                .iter()
                .find(|c| c.id == "output_layout")
                .unwrap()
                .passed
        );
        let text = serde_json::to_string(&qc).unwrap();
        assert!(!text.contains("NaN"));
    }

    /// Результат, не прошедший техническую проверку, получает тот же набор
    /// обязательных проверок, что и измеренный: невыполнимые — неразрешены.
    #[test]
    fn failed_output_probe_keeps_every_mandatory_check() {
        let pcm = speech_like(1, false);
        let policy = profile();
        let prepared = prepare(&pcm, &policy);
        let encoded = mp3(&prepared.processed);
        let (measured, _) = post_qc(
            &prepared.processed,
            &prepared.processed_analysis,
            &prepared.plan,
            &encoded,
            &policy,
        )
        .unwrap();
        let (failed, probe) = post_qc(
            &prepared.processed,
            &prepared.processed_analysis,
            &prepared.plan,
            b"not an mp3",
            &policy,
        )
        .unwrap();
        assert!(probe.is_none() && !failed.passed);
        let ids = |qc: &PostQc| qc.checks.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&failed), ids(&measured));
        assert!(failed.checks.iter().all(|c| !c.passed && c.value.is_none()));
        for (failed, measured) in failed.checks.iter().zip(&measured.checks) {
            assert_eq!(failed.limit, measured.limit, "{}", failed.id);
        }
    }

    /// Решение по каждой строке таблицы: какие байты сохраняются и куда
    /// вход передаётся дальше. Ни одна ветка не требует человека.
    fn decision(reasons: &[Reason], state: Route) -> (GateOutcome, Option<Variant>, NextAction) {
        let (outcome, variant) = route(reasons, state);
        (
            outcome,
            variant,
            next_action_for(outcome).expect("текущий исход"),
        )
    }

    /// Неустранимые этим этапом свойства входа — ограничения с источником:
    /// проверенный вариант остаётся кандидатом с отметкой ограничений, а
    /// отброшенный — сохраняет исходник; оба отложены автоматически.
    #[test]
    fn every_unresolvable_class_is_routed_automatically_without_false_accept() {
        let mut clipped = signal(&[(1000.0, Kind::Tone, 3.0)]);
        for sample in &mut clipped.samples {
            *sample = sample.clamp(-1.0, 1.0);
        }
        let mut click = signal(&[
            (100.0, Kind::Silence, 0.0),
            (1500.0, Kind::Tone, -40.0),
            (100.0, Kind::Silence, 0.0),
        ]);
        click.samples[22050] = 0.5;
        let loud = signal(&[
            (200.0, Kind::Silence, 0.0),
            (1200.0, Kind::Tone, -3.0),
            (200.0, Kind::Silence, 0.0),
        ]);
        let mut low_attenuation = profile();
        low_attenuation.loudness.max_attenuation_db = 5.0;
        let tone = signal(&[
            (200.0, Kind::Silence, 0.0),
            (800.0, Kind::Tone, -20.0),
            (200.0, Kind::Silence, 0.0),
        ]);
        let opposite = Pcm {
            sample_rate: RATE,
            channels: 2,
            samples: tone.samples.iter().flat_map(|s| [*s, -*s]).collect(),
        };
        let mut mono = profile();
        mono.output.channels = ChannelPolicy::Mono;
        let cases: Vec<(&str, Pcm, NormalizerProfile, ReasonOrigin)> = vec![
            (
                "uncertain_leading_boundary",
                signal(&[
                    (400.0, Kind::Noise, -63.0),
                    (400.0, Kind::Tone, -20.0),
                    (400.0, Kind::Noise, -63.0),
                ]),
                profile(),
                ReasonOrigin::Measurement,
            ),
            (
                "low_signal_margin",
                signal(&[
                    (300.0, Kind::Silence, 0.0),
                    (600.0, Kind::Tone, -45.0),
                    (300.0, Kind::Silence, 0.0),
                ]),
                profile(),
                ReasonOrigin::Measurement,
            ),
            (
                "very_short_clip",
                signal(&[(80.0, Kind::Tone, -20.0)]),
                profile(),
                ReasonOrigin::SourceSignal,
            ),
            (
                "source_clipping",
                clipped,
                profile(),
                ReasonOrigin::SourceSignal,
            ),
            (
                "target_shortfall",
                click,
                profile(),
                ReasonOrigin::ProfilePolicy,
            ),
            (
                "target_exceeded",
                loud,
                low_attenuation,
                ReasonOrigin::ProfilePolicy,
            ),
            (
                "downmix_phase_cancellation",
                opposite,
                mono,
                ReasonOrigin::SourceSignal,
            ),
        ];
        for (code, pcm, policy, origin) in cases {
            let prepared = prepare(&pcm, &policy);
            let found = prepared
                .reasons
                .iter()
                .find(|r| r.code == code)
                .unwrap_or_else(|| panic!("{code}: {:?}", codes(&prepared)));
            assert_eq!(found.severity, Severity::Limitation, "{code}");
            assert_eq!(found.origin, Some(origin), "{code}");
            assert!(!prepared.rejected, "{code}");
            assert_eq!(
                decision(&prepared.reasons, Route::Verified),
                (
                    GateOutcome::AcceptWithLimitations,
                    Some(Variant::Processed),
                    NextAction::DeferredAutomaticProcessing
                ),
                "{code}"
            );
            for state in [Route::VariantDiscarded, Route::Unchanged] {
                assert_eq!(
                    decision(&prepared.reasons, state),
                    (
                        GateOutcome::OriginalWithLimitations,
                        Some(Variant::Original),
                        NextAction::DeferredAutomaticProcessing
                    ),
                    "{code}: {state:?}"
                );
            }
        }
    }

    /// Абсолютный порог BS.1770 не сдвигается вместе с усилением: при
    /// сильном ослаблении записи с долгим тихим фоном часть блоков выпадает
    /// из стробирования, и простой прогноз «измерено + усиление» ошибается
    /// больше чем на допуск пост-QC. Усиление уточняется по фактической
    /// громкости подготовленного PCM, а ожидаемая громкость плана — её точное
    /// измерение.
    #[test]
    fn gain_is_refined_when_absolute_gate_drops_quiet_blocks() {
        // Длинный тихий фон выше −70 LUFS и короткая громкая речь: ослабление
        // на ~12 dB опускает блоки фона ниже абсолютного порога (7 → 5 блоков).
        let pcm = signal(&[
            (1300.0, Kind::Noise, -63.0),
            (200.0, Kind::Tone, -3.0),
            (300.0, Kind::Tone, -20.0),
            (1300.0, Kind::Noise, -63.0),
        ]);
        let mut policy = profile();
        policy.trim.enabled = false;
        let prepared = prepare(&pcm, &policy);
        let gain = &prepared.plan.gain;
        let naive = gain.measured_lufs.unwrap() + gain.applied_db;
        let actual = loudness_by(&prepared.processed_analysis, gain.method).unwrap();
        let correction = gain.gating_correction_db.expect("уточнение выполнено");
        assert!(
            correction.abs() > policy.gate.max_loudness_error_db,
            "{gain:?}"
        );
        assert!(
            (actual - policy.loudness.target_lufs).abs() < 0.05,
            "{actual}"
        );
        assert!((naive - actual).abs() > 0.5, "{naive} {actual}");
        assert_eq!(prepared.plan.expected.measured_lufs, Some(actual));
        assert!(gain.applied_db < 0.0 && gain.limited_by.is_empty());
        assert!(
            prepared.processed_analysis.integrated_blocks < prepared.analysis.integrated_blocks
        );
    }

    /// Шумный исходник: усиление ограничено шумовым потолком, а не
    /// поднимает шум; недостижимая цель — ограничение, а не `ACCEPT`.
    #[test]
    fn noisy_source_gain_is_capped_and_routed_as_limitation() {
        let pcm = signal(&[
            (300.0, Kind::Noise, -58.0),
            (800.0, Kind::Tone, -40.0),
            (300.0, Kind::Noise, -58.0),
        ]);
        let prepared = prepare(&pcm, &profile());
        let gain = &prepared.plan.gain;
        assert!(gain.limited_by.contains(&GainLimit::NoiseFloor), "{gain:?}");
        assert!(prepared.processed_analysis.noise_floor_dbfs <= -55.0 + 0.5);
        assert!(codes(&prepared).contains(&"target_shortfall"));
        assert_eq!(
            decision(&prepared.reasons, Route::Verified).0,
            GateOutcome::AcceptWithLimitations
        );
    }

    /// Бережные случаи без ограничений не становятся «проблемными»: слабый
    /// согласный у края и внутренняя пауза `っ` сохраняются, а вход получает
    /// обычный `ACCEPT`.
    #[test]
    fn preserved_weak_sounds_and_pauses_stay_plain_accept() {
        for pcm in [
            signal(&[
                (400.0, Kind::Silence, 0.0),
                (120.0, Kind::Noise, -55.0),
                (400.0, Kind::Tone, -20.0),
                (300.0, Kind::Silence, 0.0),
            ]),
            signal(&[
                (300.0, Kind::Silence, 0.0),
                (250.0, Kind::Tone, -20.0),
                (400.0, Kind::Silence, 0.0),
                (250.0, Kind::Tone, -20.0),
                (300.0, Kind::Silence, 0.0),
            ]),
        ] {
            let prepared = prepare(&pcm, &profile());
            assert!(!has_limitation(&prepared.reasons), "{:?}", codes(&prepared));
            assert_eq!(
                decision(&prepared.reasons, Route::Verified),
                (
                    GateOutcome::Accept,
                    Some(Variant::Processed),
                    NextAction::UseProcessed
                )
            );
            // Тот же вход с проваленным пост-QC сохраняет исходник.
            let mut failed = prepared.reasons.clone();
            failed.push(limitation(
                "post_qc_codec_snr",
                ReasonOrigin::PostQc,
                "тест",
            ));
            assert_eq!(
                decision(&failed, Route::VariantDiscarded),
                (
                    GateOutcome::OriginalWithLimitations,
                    Some(Variant::Original),
                    NextAction::DeferredAutomaticProcessing
                )
            );
        }
        let silence = prepare(&signal(&[(800.0, Kind::Silence, 0.0)]), &profile());
        assert_eq!(
            decision(&silence.reasons, Route::Rejected),
            (GateOutcome::Reject, None, NextAction::DiscardInput)
        );
        assert_eq!(silence.reasons[0].origin, Some(ReasonOrigin::SourceSignal));
    }

    /// Провал или неразрешимость каждой обязательной проверки пост-QC даёт
    /// отдельное ограничение `post_qc_<проверка>`, и решение принимает та же
    /// функция [`conclude_post_qc`], которую вызывает [`process`]: вариант
    /// отброшен, сохраняется исходник, `ACCEPT` невозможен. Тест падает, если
    /// связь `qc.passed` → маршрут заменить безусловным `Route::Verified`.
    #[test]
    fn each_failed_mandatory_post_qc_check_discards_the_variant() {
        let tools = Toolchain::detect().expect("для теста нужен FFmpeg");
        let policy = profile();
        let verified = process(
            &mp3(&speech_like(1, false)),
            &policy,
            Mode::Normalize,
            &tools,
        )
        .unwrap();
        let measured = verified.result.post_qc.clone().unwrap();
        assert!(measured.passed, "{:#?}", measured.checks);
        assert_eq!(
            verified.result.gate.proposed_variant,
            Some(Variant::Processed)
        );
        let encoded = verified.output_bytes.clone().unwrap();
        let source = verified.result.analysis.clone().unwrap();
        let output_probe = verified.result.output.as_ref().unwrap().probe.clone();
        // Состояние результата до решения пост-QC: тот же план и причины.
        let pending = || {
            let mut result = verified.result.clone();
            result.output = None;
            result.post_qc = None;
            result.gate.post_qc_verified = false;
            result
        };
        let conclude = |qc: PostQc| {
            conclude_post_qc(
                pending(),
                verified.result.gate.reasons.clone(),
                PostQcOutcome {
                    qc,
                    output_probe: output_probe.clone(),
                    encoded: encoded.clone(),
                    source_analysis: &source,
                },
                &policy,
            )
        };
        // Контроль: неизменённый пост-QC воспроизводит решение `process`.
        assert_eq!(conclude(measured.clone()).result, verified.result);
        for index in 0..measured.checks.len() {
            let mut qc = measured.clone();
            qc.checks[index].passed = false;
            qc.passed = false;
            let reasons = post_qc_reasons(&qc);
            assert_eq!(reasons.len(), 1);
            let code = format!("post_qc_{}", qc.checks[index].id);
            assert_eq!(reasons[0].code, code);
            assert_eq!(reasons[0].origin, Some(ReasonOrigin::PostQc));
            let failed = conclude(qc);
            let gate = &failed.result.gate;
            assert_eq!(
                (gate.outcome, gate.proposed_variant, gate.next_action),
                (
                    GateOutcome::OriginalWithLimitations,
                    Some(Variant::Original),
                    Some(NextAction::DeferredAutomaticProcessing)
                ),
                "{code}"
            );
            assert!(
                gate.reasons
                    .iter()
                    .any(|r| r.code == code && r.severity == Severity::Limitation),
                "{code}"
            );
            assert!(gate.post_qc_verified);
            failed.result.validate().unwrap();
            // Байты остаются только диагностикой для отчёта.
            assert!(failed.output_bytes.is_some());
            // Диагностический вариант без своего пост-QC и пост-QC без
            // варианта — противоречивый результат, а не исходник с ограничением.
            let mut unverified = failed.result.clone();
            unverified.post_qc = None;
            unverified.gate.post_qc_verified = false;
            let mut orphan_qc = failed.result.clone();
            orphan_qc.output = None;
            for forged in [unverified, orphan_qc] {
                assert!(forged.validate().is_err(), "{code}");
            }
        }
        // Нечитаемый результат: все проверки неразрешимы, каждая — ограничение.
        let prepared = prepare(&speech_like(1, false), &policy);
        let (broken, _) = post_qc(
            &prepared.processed,
            &prepared.processed_analysis,
            &prepared.plan,
            b"not an mp3",
            &policy,
        )
        .unwrap();
        let reasons = post_qc_reasons(&broken);
        assert_eq!(reasons.len(), broken.checks.len());
        assert!(reasons.iter().all(|r| r.severity == Severity::Limitation));
        assert_eq!(
            conclude(broken).result.gate.outcome,
            GateOutcome::OriginalWithLimitations
        );
    }

    /// Прогноз analyze — не решение о байтах: варианта нет, следующий шаг —
    /// normalize, а исполнительные исходы для него противоречивы.
    #[test]
    fn analyze_forecast_never_claims_processed_bytes() {
        let tools = Toolchain::detect().expect("для теста нужен FFmpeg");
        let forecast = process(
            &mp3(&speech_like(1, false)),
            &profile(),
            Mode::Analyze,
            &tools,
        )
        .unwrap();
        let result = &forecast.result;
        assert!(forecast.output_bytes.is_none() && result.output.is_none());
        assert!(result.post_qc.is_none() && !result.gate.post_qc_verified);
        assert_eq!(
            (
                result.gate.outcome,
                result.gate.proposed_variant,
                result.gate.next_action
            ),
            (
                GateOutcome::ProcessingPlanned,
                None,
                Some(NextAction::RunNormalize)
            )
        );
        result.validate().unwrap();
        for (outcome, variant) in [
            (GateOutcome::Accept, Some(Variant::Processed)),
            (GateOutcome::AcceptWithLimitations, Some(Variant::Processed)),
        ] {
            let mut forged = result.clone();
            set_gate(&mut forged.gate, outcome, variant);
            assert!(forged.validate().is_err(), "{outcome:?}");
        }
        // Прогноз в режиме normalize тоже противоречив.
        let mut normalize = result.clone();
        normalize.mode = Mode::Normalize;
        assert!(normalize.validate().is_err());
    }

    /// Решение из публичных десериализуемых полей проверяется целиком:
    /// исход, вариант, действие, пост-QC и важность причин.
    #[test]
    fn inconsistent_gate_combinations_are_rejected() {
        let tools = Toolchain::detect().expect("для теста нужен FFmpeg");
        let verified = process(
            &mp3(&speech_like(1, false)),
            &profile(),
            Mode::Normalize,
            &tools,
        )
        .unwrap()
        .result;
        verified.validate().unwrap();
        type Forge = Box<dyn Fn(&mut NormalizationResult)>;
        let forged: Vec<(&str, Forge)> = vec![
            (
                "discard_input",
                Box::new(|r| r.gate.next_action = Some(NextAction::DiscardInput)),
            ),
            (
                "use_original",
                Box::new(|r| r.gate.next_action = Some(NextAction::UseOriginal)),
            ),
            ("no_action", Box::new(|r| r.gate.next_action = None)),
            (
                "original_variant",
                Box::new(|r| r.gate.proposed_variant = Some(Variant::Original)),
            ),
            ("no_variant", Box::new(|r| r.gate.proposed_variant = None)),
            (
                "not_verified",
                Box::new(|r| r.gate.post_qc_verified = false),
            ),
            (
                "qc_failed",
                Box::new(|r| r.post_qc.as_mut().unwrap().passed = false),
            ),
            ("no_output", Box::new(|r| r.output = None)),
            (
                "review_outcome",
                Box::new(|r| {
                    set_gate(
                        &mut r.gate,
                        GateOutcome::ReviewRequired,
                        Some(Variant::Processed),
                    )
                }),
            ),
            (
                "keep_original",
                Box::new(|r| {
                    set_gate(
                        &mut r.gate,
                        GateOutcome::KeepOriginal,
                        Some(Variant::Original),
                    )
                }),
            ),
            (
                "reject_reason",
                Box::new(|r| {
                    r.gate
                        .reasons
                        .push(rejection("no_signal", ReasonOrigin::SourceSignal, "…"))
                }),
            ),
            (
                "hidden_limitation",
                Box::new(|r| {
                    r.gate
                        .reasons
                        .push(limitation("encode_failed", ReasonOrigin::Encoder, "…"))
                }),
            ),
            ("unsupported_schema", Box::new(|r| r.schema_version = 3)),
        ];
        for (label, forge) in forged {
            let mut result = verified.clone();
            forge(&mut result);
            assert!(result.validate().is_err(), "{label}");
        }
    }

    /// Отпечаток схемы 1 воспроизводится побайтно: сравнение — с
    /// зафиксированным историческим значением, а не с пересчётом тем же
    /// новым кодом. Новая схема даёт другой отпечаток.
    #[test]
    fn legacy_result_fingerprints_stay_reproducible() {
        // Профиль Balanced v1 в точности как при выпуске схемы 1: значения
        // заморожены, чтобы будущие изменения профиля по умолчанию не
        // подменяли историческую проверку.
        let frozen: NormalizerProfile = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "id": "balanced", "version": 1,
            "analysis": {"frame_ms": 10.0, "signal_floor_dbfs": -70.0, "min_duration_ms": 120.0,
                "internal_pause_report_ms": 150.0, "long_internal_pause_ms": 1200.0, "dc_offset_warning": 0.02},
            "trim": {"enabled": true, "silence_threshold_dbfs": -50.0, "weak_sound_margin_db": 10.0,
                "min_edge_silence_ms": 150.0, "guard_leading_ms": 60.0, "guard_trailing_ms": 80.0,
                "fade_ms": 5.0, "uncertain_floor_margin_db": 6.0, "min_speech_margin_db": 15.0},
            "loudness": {"target_lufs": -23.0, "gain_tolerance_db": 1.0, "max_gain_db": 12.0,
                "max_attenuation_db": 20.0, "true_peak_ceiling_dbtp": -2.0, "encode_headroom_db": 0.5,
                "max_noise_floor_after_dbfs": -55.0, "review_shortfall_db": 6.0, "min_integrated_blocks": 3},
            "output": {"sample_rate": {"mode": "source"}, "channels": "source",
                "identical_channels_max_db": -60.0, "encoder": {"mode": "vbr", "quality": 2}},
            "gate": {"max_true_peak_overshoot_db": 0.5, "max_duration_error_ms": 10.0,
                "max_active_change_ms": 30.0, "max_active_change_ratio": 0.05, "min_codec_snr_db": 15.0,
                "max_loudness_error_db": 1.0, "max_new_clipped_samples": 0, "review_clip_ratio": 0.0005,
                "max_alignment_lag_ms": 5.0}
        }))
        .unwrap();
        let analyzer2: Toolchain = serde_json::from_value(serde_json::json!({
            "ffmpeg": "ffmpeg version 6.1.1-3ubuntu5",
            "encoder": "libmp3lame sha256:0123456789abcdef",
            "analyzer": "balanced-analyzer/2"
        }))
        .unwrap();
        let analyzer3 = Toolchain {
            ffprobe: "ffprobe version 6.1.1-3ubuntu5".into(),
            analyzer: "balanced-analyzer/3".into(),
            ..analyzer2.clone()
        };
        assert_eq!(
            config_sha256_for(
                LEGACY_RESULT_SCHEMA_VERSION,
                &frozen,
                Mode::Normalize,
                &analyzer2
            ),
            "02012b1f1e1aa825e510d9f938147ed78d9340647fbda43c962c4e4d6020da6f"
        );
        assert_eq!(
            config_sha256_for(
                LEGACY_RESULT_SCHEMA_VERSION,
                &frozen,
                Mode::Normalize,
                &analyzer3
            ),
            "48999c7c10ad28a44d39bfd34768560ef6b9b8894eda8edb0a03eadb24c9e286"
        );
        assert_ne!(
            config_sha256(&frozen, Mode::Normalize, &analyzer3),
            "48999c7c10ad28a44d39bfd34768560ef6b9b8894eda8edb0a03eadb24c9e286"
        );
        assert_eq!(
            config_sha256(&frozen, Mode::Normalize, &analyzer3),
            config_sha256_for(RESULT_SCHEMA_VERSION, &frozen, Mode::Normalize, &analyzer3)
        );
    }

    /// Сохраняемый исходник выше потолка true peak — исходник с
    /// ограничением, а не `KEEP_ORIGINAL`; исходник в допусках остаётся
    /// `KEEP_ORIGINAL`, а обработанный вариант в пределах потолка не
    /// наследует ограничение за превышение на входе.
    #[test]
    fn true_peak_ceiling_follows_the_selected_variant() {
        let tools = Toolchain::detect().expect("для теста нужен FFmpeg");
        let tone = signal(&[(1500.0, Kind::Tone, -20.0)]);
        let mut peaky = tone.clone();
        for i in (4410..tone.samples.len()).step_by(4410) {
            peaky.samples[i] = 0.95;
        }
        let run = |pcm: &Pcm, policy: &NormalizerProfile| {
            process(&mp3(pcm), policy, Mode::Normalize, &tools)
                .unwrap()
                .result
        };
        // Цель — фактическая громкость самого сигнала: усиление не нужно.
        let target = |pcm: &Pcm| {
            let mut policy = profile();
            policy.loudness.target_lufs = analyze_pcm(pcm, &policy).integrated_lufs.unwrap();
            policy
        };
        let within = run(&tone, &target(&tone));
        assert_eq!(
            within.gate.outcome,
            GateOutcome::KeepOriginal,
            "{:#?}",
            within.gate
        );
        assert_eq!(within.gate.next_action, Some(NextAction::UseOriginal));

        let policy = target(&peaky);
        let above = run(&peaky, &policy);
        assert!(
            !above.plan.as_ref().unwrap().needs_processing,
            "{:#?}",
            above.plan
        );
        assert!(
            above.analysis.as_ref().unwrap().true_peak_dbtp
                > policy.loudness.true_peak_ceiling_dbtp
        );
        assert_eq!(
            (
                above.gate.outcome,
                above.gate.proposed_variant,
                above.gate.next_action
            ),
            (
                GateOutcome::OriginalWithLimitations,
                Some(Variant::Original),
                Some(NextAction::DeferredAutomaticProcessing)
            ),
            "{:#?}",
            above.gate
        );
        let peak = above
            .gate
            .reasons
            .iter()
            .find(|r| r.code == SOURCE_PEAK_CODE)
            .unwrap();
        assert_eq!(peak.severity, Severity::Limitation);
        assert!(
            above
                .gate
                .reasons
                .iter()
                .all(|r| r.code != "already_within_profile")
        );
        above.validate().unwrap();

        // Громкий исходник выше потолка ослабляется ради цели громкости:
        // результат ниже потолка — `ACCEPT`, превышение входа — лишь диагностика.
        let loud = signal(&[
            (200.0, Kind::Silence, 0.0),
            (1200.0, Kind::Tone, -1.0),
            (200.0, Kind::Silence, 0.0),
        ]);
        let attenuated = run(&loud, &profile());
        assert_eq!(
            attenuated.gate.outcome,
            GateOutcome::Accept,
            "{:#?}",
            attenuated.gate
        );
        let warning = attenuated
            .gate
            .reasons
            .iter()
            .find(|r| r.code == SOURCE_PEAK_CODE)
            .unwrap();
        assert_eq!(warning.severity, Severity::Warning);
        assert!(
            attenuated
                .gate
                .reasons
                .iter()
                .all(|r| r.code != "output_true_peak_above_ceiling")
        );
    }

    /// Прежний результат с `REVIEW_REQUIRED` и причиной `review` читается, а
    /// у нового результата исход и следующее действие всегда согласованы.
    #[test]
    fn legacy_review_results_stay_readable_and_new_actions_are_consistent() {
        let legacy = serde_json::json!({
            "outcome": "REVIEW_REQUIRED",
            "proposed_variant": "processed",
            "post_qc_verified": true,
            "reasons": [{"code":"uncertain_leading_boundary","severity":"review","message":"…"}],
            "unresolved": []
        });
        let gate: GateDecision = serde_json::from_value(legacy).unwrap();
        assert_eq!(gate.outcome, GateOutcome::ReviewRequired);
        assert_eq!(gate.next_action, None);
        assert_eq!(gate.reasons[0].severity, Severity::Review);
        // Допустим только по правилам схемы 1, а не как форма нового решения.
        gate.validate(LEGACY_RESULT_SCHEMA_VERSION, Mode::Normalize)
            .unwrap();
        assert!(
            gate.validate(RESULT_SCHEMA_VERSION, Mode::Normalize)
                .is_err()
        );
        assert!(gate.validate(3, Mode::Normalize).is_err());
        assert!(!GateOutcome::ALL.contains(&GateOutcome::ReviewRequired));
        for outcome in GateOutcome::ALL {
            assert!(next_action_for(outcome).is_some(), "{outcome:?}");
        }
    }
}
