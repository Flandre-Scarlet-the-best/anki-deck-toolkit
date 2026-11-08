//! Распознавание японского произношения: свободная гипотеза записи, поиск
//! вхождений целевого чтения, границы на шкале исходных отсчётов и машинное
//! решение.
//!
//! Модуль чистый: он получает эмиссии моделей и уровни сигнала и ничего не
//! запускает. Оркестрация с декодером, Balanced, улучшением речи и
//! пост-проверкой — `recognizer_pipeline`.
//!
//! Главные правила:
//! - свободная гипотеза (без подсказки) и проверка с подсказкой хранятся
//!   раздельно; совпадение засчитывается, только если свободная гипотеза
//!   основной модели на кадрах вхождения уже равна целевому чтению (с точностью
//!   до явно перечисленных вариантов реализации), выравнивание с подсказкой
//!   почти не уступает свободному лучшему пути, похожие чтения проигрывают с
//!   запасом, а независимая вторая модель слышит то же;
//! - совпадение чтения не доказывает написание или лексему: это фиксируется
//!   в неопределённостях, а не скрывается;
//! - confidence не калибрована: результат хранит измеримые признаки и
//!   `confidence: uncalibrated`, а не вероятность;
//! - неуверенность — это исход (`ambiguous`, `multiple_occurrences`,
//!   `boundaries_unresolved`…), а не сбой; сбой среды — ошибка, не исход.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::ctc::{self, Alignment, Classes, Emissions, TokenSpan};
use super::kana;
use super::normalizer::{Reason, ReasonOrigin, Severity};
use super::recognizer_backend::ModelIdentity;
use crate::{AssetError, ErrorCode};

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const RESULT_SCHEMA_VERSION: u32 = 1;
/// Версия алгоритма анализа и решений: входит в отпечаток конфигурации.
pub const ALGORITHM_VERSION: &str = "japanese-pronunciation-recognition/1";

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn identity_invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

// ---------------------------------------------------------------------------
// Профиль
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicy {
    pub primary: String,
    /// Независимая модель второй гипотезы; `None` — совпадение без
    /// независимого подтверждения не бывает строгим.
    pub verifier: Option<String>,
    pub threads: u32,
    pub timeout_seconds: u64,
    /// Входов на один вызов воркера (веса проверяются один раз на вызов).
    pub call_items: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisPolicy {
    /// Токены свободной гипотезы, разделённые паузой не короче, — разные
    /// речевые события.
    pub event_gap_ms: f64,
    /// Окна 10 мс выше шумового пола на столько — акустическая активность.
    pub activity_above_noise_db: f64,
    pub activity_merge_gap_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchPolicy {
    /// Оценка на элемент (≤ 0) не ниже — кандидат во вхождения.
    pub candidate_min_score_per_token: f64,
    /// Оценка на элемент не ниже — достаточна для строгого совпадения.
    pub match_min_score_per_token: f64,
    /// Наименьший запас целевого выравнивания над лучшим похожим чтением.
    pub min_competitor_margin: f64,
    pub max_occurrences: usize,
    /// Кадры вокруг найденного вхождения, запрещённые следующему поиску.
    pub separation_frames: usize,
    /// Свободная гипотеза второй модели должна совпасть с чтением.
    pub verifier_required: bool,
    pub verifier_max_distance: usize,
    /// Независимая модель ошибается сама (CER порядка 10 %): расхождение не
    /// больше этого числа фонем ещё подтверждает цель, если свободная
    /// гипотеза основной модели точно совпала с целью, а отрыв от всех
    /// конкурентов не меньше `verifier_relaxed_min_margin`.
    pub verifier_relaxed_distance: usize,
    pub verifier_relaxed_min_margin: f64,
    /// Допуск (кадры) центра токена свободной гипотезы вокруг вхождения.
    pub overlap_tolerance_frames: usize,
    /// Автоматический выбор одного из нескольких совпадений: лучший должен
    /// превосходить следующий хотя бы по одному признаку на столько.
    pub selection_min_snr_gap_db: f64,
    pub selection_min_score_gap: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryPolicy {
    /// Неопределённость границы выравнивания CTC (пиковые эмиссии).
    pub start_uncertainty_ms: f64,
    pub end_uncertainty_ms: f64,
    /// Защитное поле перед словом по классу первой фонемы.
    pub guard_pre_fricative_ms: f64,
    pub guard_pre_plosive_ms: f64,
    pub guard_pre_other_ms: f64,
    /// Защитное поле после слова по классу последней фонемы.
    pub guard_post_weak_ms: f64,
    pub guard_post_other_ms: f64,
    /// Меньшего поля после ограничения соседями недостаточно: границы
    /// неразрешимы.
    pub min_guard_ms: f64,
    /// Продление по непрерывной активности от края слова (слабые звуки).
    pub energy_extension_max_ms: f64,
    /// Тихий промежуток между словом и соседним материалом не короче.
    pub min_quiet_gap_ms: f64,
    /// Контекст исходного сигнала для улучшения речи вокруг фрагмента.
    pub context_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostPolicy {
    /// Допустимое расхождение положения слова в результате с ожидаемым.
    pub timing_tolerance_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionProfile {
    pub schema_version: u32,
    pub id: String,
    pub version: u32,
    pub models: ModelPolicy,
    pub analysis: AnalysisPolicy,
    pub matching: MatchPolicy,
    pub boundaries: BoundaryPolicy,
    pub post: PostPolicy,
}

impl RecognitionProfile {
    /// Консервативный профиль по умолчанию. Пороги выбраны по размеченному
    /// эталонному набору (`docs/ru/asset-store/recognizer.md`) и не
    /// заучивают SHA корпуса.
    pub fn conservative() -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: "japanese-recognition-conservative".into(),
            version: 1,
            models: ModelPolicy {
                primary: super::recognizer_backend::PRIMARY_PHONEME.id.into(),
                verifier: Some(super::recognizer_backend::VERIFIER_HIRAGANA.id.into()),
                threads: 2,
                timeout_seconds: 600,
                call_items: 16,
            },
            analysis: AnalysisPolicy {
                event_gap_ms: 160.0,
                activity_above_noise_db: 10.0,
                activity_merge_gap_ms: 50.0,
            },
            matching: MatchPolicy {
                candidate_min_score_per_token: -3.0,
                match_min_score_per_token: -0.7,
                min_competitor_margin: 1.5,
                max_occurrences: 8,
                separation_frames: 2,
                verifier_required: true,
                verifier_max_distance: 0,
                verifier_relaxed_distance: 1,
                verifier_relaxed_min_margin: 3.0,
                overlap_tolerance_frames: 0,
                selection_min_snr_gap_db: 6.0,
                selection_min_score_gap: 1.0,
            },
            boundaries: BoundaryPolicy {
                // Смещение ядра CTC относительно энергетических краёв слова на
                // размеченном наборе: начало до +132 мс, конец до −201 мс
                // (пиковые эмиссии конечной гласной); защитное поле
                // откладывается от ядра, расширенного на эту неопределённость.
                start_uncertainty_ms: 80.0,
                end_uncertainty_ms: 120.0,
                guard_pre_fricative_ms: 120.0,
                guard_pre_plosive_ms: 80.0,
                guard_pre_other_ms: 60.0,
                guard_post_weak_ms: 150.0,
                guard_post_other_ms: 100.0,
                min_guard_ms: 30.0,
                energy_extension_max_ms: 200.0,
                min_quiet_gap_ms: 40.0,
                context_ms: 400.0,
            },
            post: PostPolicy {
                timing_tolerance_ms: 80.0,
            },
        }
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        let m = &self.matching;
        let b = &self.boundaries;
        let finite_nonneg = [
            self.analysis.event_gap_ms,
            self.analysis.activity_above_noise_db,
            self.analysis.activity_merge_gap_ms,
            m.min_competitor_margin,
            m.verifier_relaxed_min_margin,
            m.selection_min_snr_gap_db,
            m.selection_min_score_gap,
            b.start_uncertainty_ms,
            b.end_uncertainty_ms,
            b.guard_pre_fricative_ms,
            b.guard_pre_plosive_ms,
            b.guard_pre_other_ms,
            b.guard_post_weak_ms,
            b.guard_post_other_ms,
            b.min_guard_ms,
            b.energy_extension_max_ms,
            b.min_quiet_gap_ms,
            b.context_ms,
            self.post.timing_tolerance_ms,
        ];
        let valid = self.schema_version == PROFILE_SCHEMA_VERSION
            && !self.id.is_empty()
            && self.version > 0
            && finite_nonneg.iter().all(|v| v.is_finite() && *v >= 0.0)
            && m.candidate_min_score_per_token.is_finite()
            && m.match_min_score_per_token.is_finite()
            && m.candidate_min_score_per_token <= m.match_min_score_per_token
            && m.match_min_score_per_token <= 0.0
            && (1..=32).contains(&m.max_occurrences)
            && (1..=4).contains(&self.models.threads)
            && (1..=super::recognizer_backend::MAX_CALL_ITEMS).contains(&self.models.call_items)
            && (1..=3600).contains(&self.models.timeout_seconds)
            && super::recognizer_backend::expected(&self.models.primary)
                .is_some_and(|e| e.output == "phoneme")
            && self.models.verifier.as_deref().is_none_or(|v| {
                super::recognizer_backend::expected(v).is_some() && v != self.models.primary
            })
            && (!m.verifier_required || self.models.verifier.is_some())
            && b.min_guard_ms <= b.guard_pre_other_ms.min(b.guard_post_other_ms);
        if valid {
            Ok(())
        } else {
            Err(invalid(
                "профиль распознавания противоречив: проверьте схему, пороги (конечные, кандидат ≤ совпадение ≤ 0), модели и поля границ",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Целевая идентичность
// ---------------------------------------------------------------------------

/// Откуда известна пара `surface + reading`. Имя файла, путь, ранжирование
/// Forvo и транскрипция модели источником не бывают.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TargetSourceKind {
    /// Явная пара в запросе одного файла (`analyze --surface --reading`).
    Request,
    /// Запись явного манифеста целей пакета.
    Manifest,
    /// План загрузки Forvo, в котором пара задана явно.
    ForvoPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct TargetSource {
    pub kind: TargetSourceKind,
    /// Ключ происхождения: SHA манифеста и идентификатор записи и т. п.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    pub surface: String,
    pub reading: String,
    pub source: TargetSource,
    /// Явно известные из того же источника другие чтения той же записи
    /// словаря (например, `生`: `せい`, `しょう` при цели `なま`). Не
    /// порождаются автоматически из всех чтений кандзи.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub competing_readings: Vec<String>,
}

impl TargetIdentity {
    /// Строгая проверка: пара проходит `Pronunciation::new`, альтернативы —
    /// кана и отличаются от цели.
    pub fn validate(&self) -> Result<(), AssetError> {
        let pronunciation = super::Pronunciation::new(&self.surface, &self.reading)?;
        if pronunciation.surface != self.surface || pronunciation.reading != self.reading {
            return Err(identity_invalid("цель должна быть канонической NFC-парой"));
        }
        if self.source.key.is_empty() || self.source.key.len() > 512 {
            return Err(identity_invalid("у цели нет ключа происхождения"));
        }
        let mut seen = BTreeSet::new();
        for alt in &self.competing_readings {
            if !kana::is_phonetic_reading(alt)
                || kana::to_hiragana(alt) == kana::to_hiragana(&self.reading)
                || !seen.insert(kana::to_hiragana(alt))
                || self.competing_readings.len() > 16
            {
                return Err(identity_invalid(
                    "конкурирующие чтения — до 16 различных каноничных строк каны, не равных цели",
                ));
            }
        }
        Ok(())
    }

    /// Ключ цели для кэша: решение зависит от пары и альтернатив, но не от
    /// того, каким пакетом цель пришла. Происхождение остаётся в результате.
    pub fn matching_key(&self) -> String {
        crate::hashing::sha256_hex(
            serde_json::to_vec(&(&self.surface, &self.reading, &self.competing_readings))
                .expect("цель сериализуема"),
        )
    }
}

// ---------------------------------------------------------------------------
// Временная шкала
// ---------------------------------------------------------------------------

/// Полуоткрытый интервал кадров PCM (`[start, end)`) исходного сигнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SampleSpan {
    pub start: u64,
    pub end: u64,
}

impl SampleSpan {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
    pub fn overlaps(&self, other: &SampleSpan) -> bool {
        self.start < other.end && other.start < self.end
    }
    pub fn contains(&self, other: &SampleSpan) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// Шкала исходной записи и явные преобразования к шкале анализа. Границы
/// хранятся в кадрах PCM исходной частоты; миллисекунды — только для отчётов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeline {
    pub original_sha256: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// Кадров PCM после декодирования с учётом gapless (как у `probe`).
    pub frames: u64,
    pub analysis_sample_rate: u32,
    pub analysis_frames: u64,
    /// Сведение каналов к моно для анализа (среднее каналов).
    pub analysis_downmix: String,
    /// Ресэмплер шкалы анализа (libswresample через FFmpeg).
    pub analysis_resampler: String,
}

impl Timeline {
    /// Отсчёт шкалы анализа → кадр исходника (вниз для начала, вверх для конца).
    pub fn from_analysis(&self, sample: i64, round_up: bool) -> u64 {
        let sample = sample.max(0) as u128;
        let num = sample * u128::from(self.sample_rate);
        let den = u128::from(self.analysis_sample_rate);
        let value = if round_up {
            num.div_ceil(den)
        } else {
            num / den
        };
        (value as u64).min(self.frames)
    }

    /// Кадр исходника → отсчёт шкалы анализа (вниз).
    pub fn to_analysis(&self, frame: u64) -> i64 {
        (u128::from(frame) * u128::from(self.analysis_sample_rate) / u128::from(self.sample_rate))
            as i64
    }

    pub fn ms(&self, frames: u64) -> f64 {
        frames as f64 * 1000.0 / f64::from(self.sample_rate)
    }

    pub fn frames_for_ms(&self, ms: f64) -> u64 {
        (ms.max(0.0) * f64::from(self.sample_rate) / 1000.0).round() as u64
    }
}

/// Кадры модели `[first, last]` → интервал исходника. Кадр `t` описывает
/// окно `[t·stride, t·stride + receptive)` дополненного входа; ему ставится
/// в соответствие ячейка шириной `stride` в центре окна, за вычетом
/// дополнения тишиной.
pub fn frames_to_span(
    timeline: &Timeline,
    model: &ModelIdentity,
    first: usize,
    last: usize,
) -> SampleSpan {
    let offset = (model.receptive_field - model.frame_stride) as i64 / 2;
    let lead = model.lead_pad_samples as i64;
    let stride = model.frame_stride as i64;
    let start = first as i64 * stride + offset - lead;
    let end = (last as i64 + 1) * stride + offset - lead;
    SampleSpan {
        start: timeline.from_analysis(start, false),
        end: timeline.from_analysis(end, true),
    }
}

/// Интервал исходника → кадры модели, пересекающиеся с ним.
pub fn span_to_frames(
    timeline: &Timeline,
    model: &ModelIdentity,
    span: SampleSpan,
    frames: usize,
) -> (usize, usize) {
    let offset = (model.receptive_field - model.frame_stride) as i64 / 2;
    let lead = model.lead_pad_samples as i64;
    let stride = model.frame_stride as i64;
    let a = timeline.to_analysis(span.start) + lead - offset;
    let b = timeline.to_analysis(span.end) + lead - offset;
    let first = (a.max(0) / stride) as usize;
    let last = ((b.max(0) + stride - 1) / stride) as usize;
    (first.min(frames), last.min(frames))
}

// ---------------------------------------------------------------------------
// Свободная гипотеза и речевые события
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HypothesisToken {
    /// Токен словаря модели как есть.
    pub text: String,
    pub start_frame: usize,
    pub end_frame: usize,
    pub span: SampleSpan,
    pub mean_log_prob: f32,
}

/// Свободная (без подсказки) гипотеза одной модели по всей записи.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    pub model_id: String,
    pub output: String,
    /// Сырые токены жадного декодирования (без слияния вариантов).
    pub tokens: Vec<HypothesisToken>,
    /// Сырой текст: фонемы через пробел или хирагана как есть.
    pub raw_text: String,
    /// Нормализованная для сравнения форма (фонемы, `I/U` → `i/u`, без пауз).
    pub normalized: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechEvent {
    pub index: usize,
    /// Кадры основной модели (включительно).
    pub frames: (usize, usize),
    pub span: SampleSpan,
    pub phonemes: Vec<String>,
    pub mean_log_prob: f32,
    /// Хирагана второй модели на тех же кадрах (если она есть).
    pub verifier_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreAnalysis {
    pub timeline: Timeline,
    pub primary: Hypothesis,
    pub verifier: Option<Hypothesis>,
    pub events: Vec<SpeechEvent>,
    /// Области акустической активности (уровень выше шума), независимо от модели.
    pub activity: Vec<SampleSpan>,
    /// Активность, на которой модель не услышала речи. Отсутствие события
    /// не доказывает тишину: VAD-подобный признак — не источник истины.
    pub unrecognized_activity: Vec<SampleSpan>,
    pub noise_floor_dbfs: f64,
}

/// Модель вместе с её эмиссиями для одной записи.
#[derive(Clone, Copy)]
pub struct ModelView<'a> {
    pub identity: &'a ModelIdentity,
    pub classes: &'a Classes,
    pub emissions: &'a Emissions,
}

fn hypothesis(timeline: &Timeline, view: ModelView<'_>) -> (Hypothesis, Vec<TokenSpan>) {
    let spans = ctc::greedy(view.emissions, view.identity.blank_id);
    let tokens: Vec<HypothesisToken> = spans
        .iter()
        .map(|t| HypothesisToken {
            text: view.identity.vocab[t.token].clone(),
            start_frame: t.start,
            end_frame: t.end,
            span: frames_to_span(timeline, view.identity, t.start, t.end),
            mean_log_prob: t.mean_log_prob,
        })
        .collect();
    let phoneme = view.identity.output == "phoneme";
    let raw_text = if phoneme {
        tokens
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        tokens.iter().map(|t| t.text.as_str()).collect::<String>()
    };
    let normalized = if phoneme {
        tokens
            .iter()
            .filter_map(|t| kana::comparable_phoneme(&t.text))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        kana::hiragana_hypothesis_phonemes(&raw_text)
            .and_then(|v| v.into_iter().next())
            .map(|v| v.join(" "))
            .unwrap_or_default()
    };
    (
        Hypothesis {
            model_id: view.identity.model_id.clone(),
            output: view.identity.output.clone(),
            tokens,
            raw_text,
            normalized,
        },
        spans,
    )
}

/// Окна 10 мс с уровнем выше `noise + threshold` → интервалы активности.
pub fn activity_regions(
    levels_db: &[f64],
    window_frames: u64,
    total_frames: u64,
    noise_floor_dbfs: f64,
    policy: &AnalysisPolicy,
    sample_rate: u32,
) -> Vec<SampleSpan> {
    let threshold = (noise_floor_dbfs + policy.activity_above_noise_db).max(-70.0);
    let merge = (policy.activity_merge_gap_ms * f64::from(sample_rate) / 1000.0) as u64;
    let mut out: Vec<SampleSpan> = Vec::new();
    for (i, level) in levels_db.iter().enumerate() {
        if *level < threshold {
            continue;
        }
        let span = SampleSpan {
            start: i as u64 * window_frames,
            end: ((i as u64 + 1) * window_frames).min(total_frames),
        };
        match out.last_mut() {
            Some(last) if span.start <= last.end + merge => last.end = span.end,
            _ => out.push(span),
        }
    }
    out
}

/// Свободный анализ записи без знания цели.
pub fn pre_analyze(
    timeline: Timeline,
    primary: ModelView<'_>,
    verifier: Option<ModelView<'_>>,
    activity: Vec<SampleSpan>,
    noise_floor_dbfs: f64,
    policy: &AnalysisPolicy,
) -> PreAnalysis {
    let (primary_hypothesis, spans) = hypothesis(&timeline, primary);
    let verifier_hypothesis = verifier.map(|v| hypothesis(&timeline, v).0);
    let gap_frames = (policy.event_gap_ms / 1000.0 * f64::from(primary.identity.sample_rate)
        / primary.identity.frame_stride as f64)
        .round() as usize;
    let mut events: Vec<SpeechEvent> = Vec::new();
    let mut current: Vec<&TokenSpan> = Vec::new();
    let flush = |current: &mut Vec<&TokenSpan>, events: &mut Vec<SpeechEvent>| {
        if current.is_empty() {
            return;
        }
        let first = current[0].start;
        let last = current[current.len() - 1].end;
        let span = frames_to_span(&timeline, primary.identity, first, last);
        let mean = current
            .iter()
            .map(|t| f64::from(t.mean_log_prob))
            .sum::<f64>()
            / current.len() as f64;
        events.push(SpeechEvent {
            index: events.len(),
            frames: (first, last),
            span,
            phonemes: current
                .iter()
                .filter_map(|t| kana::comparable_phoneme(&primary.identity.vocab[t.token]))
                .map(String::from)
                .collect(),
            mean_log_prob: mean as f32,
            verifier_text: None,
        });
        current.clear();
    };
    for token in &spans {
        let text = &primary.identity.vocab[token.token];
        if kana::comparable_phoneme(text).is_none() {
            // Пауза или служебный токен модели разделяет события.
            flush(&mut current, &mut events);
            continue;
        }
        if let Some(last) = current.last()
            && token.start.saturating_sub(last.end) > gap_frames
        {
            flush(&mut current, &mut events);
        }
        current.push(token);
    }
    flush(&mut current, &mut events);
    if let Some(v) = &verifier_hypothesis {
        for event in &mut events {
            let text: String = v
                .tokens
                .iter()
                .filter(|t| t.span.overlaps(&event.span))
                .map(|t| t.text.as_str())
                .collect();
            event.verifier_text = Some(text);
        }
    }
    let unrecognized_activity = activity
        .iter()
        .filter(|a| !events.iter().any(|e| e.span.overlaps(a)))
        .copied()
        .collect();
    PreAnalysis {
        timeline,
        primary: primary_hypothesis,
        verifier: verifier_hypothesis,
        events,
        activity,
        unrecognized_activity,
        noise_floor_dbfs,
    }
}

// ---------------------------------------------------------------------------
// Конкуренты
// ---------------------------------------------------------------------------

/// Похожее чтение, с которым сравнивается цель.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Competitor {
    pub phonemes: Vec<String>,
    /// `minimal_pair:<правило>`, `explicit:<чтение>` или `free_hypothesis`.
    pub origin: String,
}

const VOICELESS_ONSETS: [&str; 7] = ["k", "s", "sh", "t", "ch", "ts", "p"];
const VOICING: [(&str, &str); 12] = [
    ("k", "g"),
    ("s", "z"),
    ("sh", "j"),
    ("t", "d"),
    ("ch", "j"),
    ("ts", "z"),
    ("h", "b"),
    ("p", "b"),
    ("ky", "gy"),
    ("hy", "by"),
    ("py", "by"),
    ("f", "b"),
];
const PALATAL: [(&str, &str); 8] = [
    ("ky", "k"),
    ("gy", "g"),
    ("ny", "n"),
    ("hy", "h"),
    ("by", "b"),
    ("py", "p"),
    ("my", "m"),
    ("ry", "r"),
];

/// Минимальные пары канонической реализации: различия, которые меняют
/// слово (`っ`, `ん`, долгота, звонкость, мягкость). Это контролируемый
/// перечень правил, а не все чтения всех кандзи.
pub fn minimal_pairs(phonemes: &[String]) -> Vec<Competitor> {
    let mut out: Vec<Competitor> = Vec::new();
    let mut push = |seq: Vec<String>, rule: &str| {
        if !seq.is_empty() {
            out.push(Competitor {
                phonemes: seq,
                origin: format!("minimal_pair:{rule}"),
            });
        }
    };
    let p = phonemes;
    for i in 0..p.len() {
        let cur = p[i].as_str();
        if cur == "cl" || cur == "N" {
            let mut seq = p.to_vec();
            seq.remove(i);
            push(
                seq,
                if cur == "cl" {
                    "drop_sokuon"
                } else {
                    "drop_moraic_n"
                },
            );
        }
        if kana::is_vowel(cur) {
            if p.get(i + 1).is_some_and(|n| n == cur) {
                let mut seq = p.to_vec();
                seq.remove(i + 1);
                push(seq, "shorten_vowel");
            } else if i == 0 || p[i - 1] != cur {
                let mut seq = p.to_vec();
                seq.insert(i + 1, cur.to_string());
                push(seq, "lengthen_vowel");
            }
        }
        if i > 0 && VOICELESS_ONSETS.contains(&cur) && p[i - 1] != "cl" && kana::is_vowel(&p[i - 1])
        {
            let mut seq = p.to_vec();
            seq.insert(i, "cl".into());
            push(seq, "insert_sokuon");
        }
        for (a, b) in VOICING {
            let other = if cur == a {
                Some(b)
            } else if cur == b {
                Some(a)
            } else {
                None
            };
            if let Some(other) = other {
                let mut seq = p.to_vec();
                seq[i] = other.to_string();
                push(seq, "voicing");
            }
        }
        for (soft, plain) in PALATAL {
            if cur == soft && p.get(i + 1).is_some_and(|v| kana::is_vowel(v)) {
                let mut seq = p.to_vec();
                seq.splice(i..=i, [plain.to_string(), "i".into(), "y".into()]);
                push(seq, "split_palatal");
            }
            if cur == plain
                && p.get(i + 1).is_some_and(|v| v == "i")
                && p.get(i + 2).is_some_and(|v| v == "y")
                && p.get(i + 3).is_some_and(|v| kana::is_vowel(v))
            {
                let mut seq = p.to_vec();
                seq.splice(i..i + 3, [soft.to_string()]);
                push(seq, "merge_palatal");
            }
        }
    }
    if let Some(last) = p.last()
        && kana::is_vowel(last)
    {
        let mut seq = p.to_vec();
        seq.push("N".into());
        push(seq, "append_moraic_n");
    }
    out.sort();
    out.dedup_by(|a, b| a.phonemes == b.phonemes);
    out
}

// ---------------------------------------------------------------------------
// Вхождения цели
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Свободная гипотеза равна чтению, выравнивание почти не уступает
    /// лучшему пути, похожие чтения проигрывают с запасом, вторая модель
    /// согласна.
    Match,
    /// Похоже, но хотя бы одного свидетельства недостаточно.
    Weak,
    /// Свидетельства указывают на другое чтение.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompetitorScore {
    pub phonemes: String,
    pub origin: String,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Occurrence {
    pub index: usize,
    /// Реализация чтения, выровненная лучше всех.
    pub phonemes: String,
    pub frames: (usize, usize),
    /// Предполагаемые границы самого чтения.
    pub core: SampleSpan,
    /// Интервалы неопределённости начала и конца.
    pub start_uncertainty: SampleSpan,
    pub end_uncertainty: SampleSpan,
    /// Сумма и среднее на элемент `log p(путь) − log p(лучший токен)`.
    pub score: f32,
    pub score_per_token: f32,
    /// Свободная гипотеза основной модели на кадрах вхождения.
    pub free_phonemes: String,
    pub free_distance: usize,
    /// Лучшее похожее чтение и запас цели над ним (`None` — сравнивать не с чем).
    pub best_competitor: Option<CompetitorScore>,
    pub competitor_margin: Option<f32>,
    /// Свободная гипотеза второй модели на тех же кадрах.
    pub verifier_text: Option<String>,
    pub verifier_distance: Option<usize>,
    /// Уровень речи во вхождении над шумовым полом записи, dB.
    pub snr_db: Option<f64>,
    pub evidence: Evidence,
    pub reasons: Vec<String>,
}

/// Классы последовательности для модели; `None`, если в словаре модели нет
/// какой-то фонемы.
fn class_sequence(classes: &Classes, phonemes: &[String]) -> Option<Vec<usize>> {
    phonemes.iter().map(|p| classes.id(p)).collect()
}

/// Токены свободной гипотезы, чей центр лежит в кадрах `[start, end]` с
/// допуском `tolerance`: соседнее слово вплотную к вхождению в его свободную
/// гипотезу не попадает.
fn overlapping_phonemes(
    hypothesis: &Hypothesis,
    frames: (usize, usize),
    tolerance: usize,
) -> Vec<String> {
    let lo = 2 * frames.0.saturating_sub(tolerance);
    let hi = 2 * (frames.1 + tolerance);
    hypothesis
        .tokens
        .iter()
        .filter(|t| (lo..=hi).contains(&(t.start_frame + t.end_frame)))
        .filter_map(|t| kana::comparable_phoneme(&t.text).map(String::from))
        .collect()
}

/// Результат сопоставления записи с одной целью.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matching {
    /// Реализации чтения, которыми выполнялся поиск.
    pub target_variants: Vec<String>,
    /// Похожие чтения, с которыми сравнивалась цель.
    pub competitors: usize,
    pub occurrences: Vec<Occurrence>,
}

/// Уровень речи во вхождении (p95 окон 10 мс) над шумовым полом.
pub fn span_snr(
    levels_db: &[f64],
    window_frames: u64,
    span: SampleSpan,
    noise_floor: f64,
) -> Option<f64> {
    let first = (span.start / window_frames) as usize;
    let last = (span.end.div_ceil(window_frames) as usize).min(levels_db.len());
    let mut values: Vec<f64> = levels_db.get(first..last)?.to_vec();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let p95 = values[((values.len() - 1) as f64 * 0.95).round() as usize];
    Some(p95 - noise_floor)
}

/// Все вхождения цели с признаками и классификацией свидетельств.
pub fn match_target(
    pre: &PreAnalysis,
    primary: ModelView<'_>,
    verifier: Option<ModelView<'_>>,
    target: &TargetIdentity,
    levels_db: &[f64],
    window_frames: u64,
    profile: &RecognitionProfile,
) -> Result<Matching, UnmatchableTarget> {
    let policy = &profile.matching;
    let variants = kana::phoneme_variants(&target.reading).map_err(|e| UnmatchableTarget(e.0))?;
    let sequences: Vec<(Vec<String>, Vec<usize>)> = variants
        .iter()
        .filter_map(|v| class_sequence(primary.classes, v).map(|c| (v.clone(), c)))
        .collect();
    if sequences.is_empty() {
        return Err(UnmatchableTarget(target.reading.clone()));
    }
    let blank = primary.identity.blank_id;
    let emissions = primary.emissions;
    // Кандидаты всех реализаций; перекрывающиеся — лучшая реализация.
    let mut found: Vec<(Alignment, usize)> = Vec::new();
    for (vi, (_, seq)) in sequences.iter().enumerate() {
        for alignment in ctc::find_occurrences(
            emissions,
            primary.classes,
            blank,
            seq,
            policy.candidate_min_score_per_token as f32,
            policy.max_occurrences,
            policy.separation_frames,
        ) {
            found.push((alignment, vi));
        }
    }
    found.sort_by(|a, b| b.0.score_per_token().total_cmp(&a.0.score_per_token()));
    let mut kept: Vec<(Alignment, usize)> = Vec::new();
    for (alignment, vi) in found {
        let overlaps = kept
            .iter()
            .any(|(k, _)| alignment.start <= k.end && k.start <= alignment.end);
        if !overlaps && kept.len() < policy.max_occurrences {
            kept.push((alignment, vi));
        }
    }
    kept.sort_by_key(|(a, _)| a.start);
    // Конкуренты: минимальные пары каждой реализации и явные альтернативы.
    let target_set: BTreeSet<Vec<String>> = variants.iter().cloned().collect();
    let mut competitors: Vec<Competitor> = variants
        .iter()
        .flat_map(|v| minimal_pairs(v))
        .filter(|c| !target_set.contains(&c.phonemes))
        .collect();
    for alt in &target.competing_readings {
        if let Ok(alt_variants) = kana::phoneme_variants(alt) {
            for v in alt_variants {
                if !target_set.contains(&v) {
                    competitors.push(Competitor {
                        phonemes: v,
                        origin: format!("explicit:{alt}"),
                    });
                }
            }
        }
    }
    competitors.sort();
    competitors.dedup_by(|a, b| a.phonemes == b.phonemes);
    let explicit_sets: Vec<Vec<Vec<String>>> = target
        .competing_readings
        .iter()
        .filter_map(|alt| kana::phoneme_variants(alt).ok())
        .collect();
    let timeline = &pre.timeline;
    let ms_frames = |ms: f64| timeline.frames_for_ms(ms);
    let mut occurrences = Vec::new();
    for (index, (alignment, vi)) in kept.into_iter().enumerate() {
        let frames = (alignment.start, alignment.end);
        let core = frames_to_span(timeline, primary.identity, frames.0, frames.1);
        let start_u = ms_frames(profile.boundaries.start_uncertainty_ms);
        let end_u = ms_frames(profile.boundaries.end_uncertainty_ms);
        let free = overlapping_phonemes(&pre.primary, frames, policy.overlap_tolerance_frames);
        let free_distance = kana::min_distance(std::slice::from_ref(&free), &variants);
        // Цель и конкуренты сравниваются на одних и тех же кадрах вхождения.
        let target_forced = sequences
            .iter()
            .filter_map(|(_, seq)| {
                ctc::forced_score(emissions, primary.classes, blank, seq, frames)
            })
            .fold(f32::NEG_INFINITY, f32::max);
        let mut candidates = competitors.clone();
        if free_distance > 0 && !free.is_empty() {
            candidates.push(Competitor {
                phonemes: free.clone(),
                origin: "free_hypothesis".into(),
            });
        }
        let best_competitor = candidates
            .iter()
            .filter_map(|c| {
                let seq = class_sequence(primary.classes, &c.phonemes)?;
                let score = ctc::forced_score(emissions, primary.classes, blank, &seq, frames)?;
                Some(CompetitorScore {
                    phonemes: c.phonemes.join(" "),
                    origin: c.origin.clone(),
                    score,
                })
            })
            .max_by(|a, b| a.score.total_cmp(&b.score));
        let margin = best_competitor.as_ref().map(|c| target_forced - c.score);
        let (verifier_text, verifier_distance, verifier_prefers_alternative) =
            match (verifier, &pre.verifier) {
                (Some(_), Some(vh)) => {
                    // Токен второй модели относится к вхождению, если его центр
                    // лежит в ядре с допуском 40 мс (рассинхронизация моделей).
                    let lo = core.start.saturating_sub(ms_frames(40.0));
                    let hi = core.end + ms_frames(40.0);
                    let text: String = vh
                        .tokens
                        .iter()
                        .filter(|t| {
                            let center = (t.span.start + t.span.end) / 2;
                            (lo..hi).contains(&center)
                        })
                        .map(|t| t.text.as_str())
                        .collect();
                    // Неразбираемая гипотеза (сочетание вне таблицы мор) —
                    // расстояния нет, подтверждения тоже.
                    match kana::hiragana_hypothesis_phonemes(&text) {
                        Some(phon) => {
                            let d = kana::min_distance(&phon, &variants);
                            let prefers = explicit_sets
                                .iter()
                                .any(|alt| kana::min_distance(&phon, alt) < d);
                            (Some(text), Some(d), prefers)
                        }
                        None => (Some(text), None, false),
                    }
                }
                _ => (None, None, false),
            };
        let free_is_alternative = explicit_sets
            .iter()
            .any(|alt| kana::min_distance(std::slice::from_ref(&free), alt) == 0);
        let mut reasons = Vec::new();
        let evidence = if margin.is_some_and(|m| m < 0.0)
            || free_is_alternative
            || verifier_prefers_alternative
        {
            if margin.is_some_and(|m| m < 0.0) {
                reasons.push("competitor_scores_higher".to_string());
            }
            if free_is_alternative {
                reasons.push("free_hypothesis_is_competing_reading".to_string());
            }
            if verifier_prefers_alternative {
                reasons.push("verifier_prefers_competing_reading".to_string());
            }
            Evidence::Conflict
        } else {
            let mut ok = true;
            if free_distance != 0 {
                reasons.push("free_hypothesis_differs".to_string());
                ok = false;
            }
            if f64::from(alignment.score_per_token()) < policy.match_min_score_per_token {
                reasons.push("alignment_below_match_threshold".to_string());
                ok = false;
            }
            if margin.is_some_and(|m| f64::from(m) < policy.min_competitor_margin) {
                reasons.push("competitor_margin_too_small".to_string());
                ok = false;
            }
            let relaxed = free_distance == 0
                && margin.is_some_and(|m| f64::from(m) >= policy.verifier_relaxed_min_margin);
            match verifier_distance {
                Some(d) if d > policy.verifier_max_distance => {
                    if relaxed && d <= policy.verifier_relaxed_distance {
                        reasons.push("verifier_near_match".to_string());
                    } else {
                        reasons.push("verifier_disagrees".to_string());
                        ok = false;
                    }
                }
                None if verifier_text.is_some() => {
                    reasons.push("verifier_hypothesis_unparseable".to_string());
                    ok = false;
                }
                None if policy.verifier_required => {
                    reasons.push("verifier_unavailable".to_string());
                    ok = false;
                }
                _ => {}
            }
            if ok { Evidence::Match } else { Evidence::Weak }
        };
        occurrences.push(Occurrence {
            index,
            phonemes: sequences[vi].0.join(" "),
            frames,
            core,
            start_uncertainty: SampleSpan {
                start: core.start.saturating_sub(start_u),
                end: (core.start + start_u).min(core.end),
            },
            end_uncertainty: SampleSpan {
                start: core.end.saturating_sub(end_u).max(core.start),
                end: (core.end + end_u).min(timeline.frames),
            },
            score: alignment.score,
            score_per_token: alignment.score_per_token(),
            free_phonemes: free.join(" "),
            free_distance,
            best_competitor,
            competitor_margin: margin,
            verifier_text,
            verifier_distance,
            snr_db: span_snr(levels_db, window_frames, core, pre.noise_floor_dbfs),
            evidence,
            reasons,
        });
    }
    Ok(Matching {
        target_variants: variants.iter().map(|v| v.join(" ")).collect(),
        competitors: competitors.len(),
        occurrences,
    })
}

/// Чтение не представимо фонемами основной модели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmatchableTarget(pub String);

// ---------------------------------------------------------------------------
// Границы извлечения
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionPlan {
    /// Ядро слова, защитные поля и итоговый фрагмент (исходные кадры PCM).
    pub core: SampleSpan,
    pub guard: SampleSpan,
    pub extraction: SampleSpan,
    /// Контекст исходного сигнала для улучшения речи (шире фрагмента).
    pub context: SampleSpan,
    /// Контекст ограничен краем записи: модель получит меньше, чем просит
    /// профиль (выдуманные отсчёты не добавляются).
    pub context_limited: bool,
    pub pre_guard_ms: f64,
    pub post_guard_ms: f64,
}

fn onset_class(phoneme: &str) -> &'static str {
    match phoneme {
        "s" | "sh" | "z" | "j" | "ts" | "ch" | "h" | "hy" | "f" | "fy" => "fricative",
        "k" | "ky" | "g" | "gy" | "t" | "ty" | "d" | "dy" | "p" | "py" | "b" | "by" | "kw"
        | "gw" => "plosive",
        _ => "other",
    }
}

/// Конечная фонема, после которой энергия слабая и легко теряется: оглушаемые
/// гласные, `ん`, долгие гласные, фрикативы.
fn weak_coda(phonemes: &[&str]) -> bool {
    match phonemes {
        [.., a, b] if a == b && kana::is_vowel(b) => true,
        [.., "N"] | [.., "cl"] => true,
        [.., c, "i" | "u"] => onset_class(c) == "fricative" || onset_class(c) == "plosive",
        _ => false,
    }
}

/// Материал записи вне вхождения: речевые токены основной модели, лежащие
/// целиком вне кадров вхождения (с допуском), и активность, на которой модель
/// речи не услышала.
pub fn other_material(
    pre: &PreAnalysis,
    occurrence: &Occurrence,
    tolerance: usize,
) -> Vec<SampleSpan> {
    let lo = occurrence.frames.0.saturating_sub(tolerance);
    let hi = occurrence.frames.1 + tolerance;
    pre.primary
        .tokens
        .iter()
        .filter(|t| kana::comparable_phoneme(&t.text).is_some())
        .filter(|t| t.end_frame < lo || t.start_frame > hi)
        .map(|t| t.span)
        .chain(
            pre.unrecognized_activity
                .iter()
                .filter(|a| !a.overlaps(&occurrence.core))
                .copied(),
        )
        .collect()
}

/// Причина, по которой безопасного отделения нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved(pub &'static str);

/// Границы фрагмента для выбранного вхождения: защитные поля по классу
/// крайних фонем, продление по непрерывной активности, ограничение соседним
/// материалом с требованием тихого промежутка. Если после ограничения поле
/// меньше минимального или промежутка нет — границы неразрешимы.
pub fn plan_extraction(
    pre: &PreAnalysis,
    occurrence: &Occurrence,
    levels_db: &[f64],
    window_frames: u64,
    policy: &BoundaryPolicy,
    activity_threshold_db: f64,
) -> Result<ExtractionPlan, Unresolved> {
    let t = &pre.timeline;
    let ms = |v: f64| t.frames_for_ms(v);
    let phonemes: Vec<&str> = occurrence.phonemes.split(' ').collect();
    let pre_guard_ms = match onset_class(phonemes.first().copied().unwrap_or("")) {
        "fricative" => policy.guard_pre_fricative_ms,
        "plosive" => policy.guard_pre_plosive_ms,
        _ => policy.guard_pre_other_ms,
    };
    let post_guard_ms = if weak_coda(&phonemes) {
        policy.guard_post_weak_ms
    } else {
        policy.guard_post_other_ms
    };
    let core = occurrence.core;
    // Продление по непрерывной активности от краёв ядра.
    let active = |frame: u64| {
        let w = (frame / window_frames) as usize;
        levels_db
            .get(w)
            .is_some_and(|l| *l >= activity_threshold_db)
    };
    let max_ext = ms(policy.energy_extension_max_ms);
    let mut ext_start = core.start;
    while ext_start > 0 && core.start - ext_start < max_ext && active(ext_start.saturating_sub(1)) {
        ext_start = ext_start.saturating_sub(window_frames.max(1));
    }
    let mut ext_end = core.end;
    while ext_end < t.frames && ext_end - core.end < max_ext && active(ext_end) {
        ext_end = (ext_end + window_frames.max(1)).min(t.frames);
    }
    // Соседний материал: токены свободной гипотезы вне вхождения (в том
    // числе внутри того же речевого события) и нераспознанная активность.
    let others = other_material(pre, occurrence, 1);
    let prev_end = others
        .iter()
        .filter(|s| s.end <= core.start)
        .map(|s| s.end)
        .max();
    let next_start = others
        .iter()
        .filter(|s| s.start >= core.end)
        .map(|s| s.start)
        .min();
    if others.iter().any(|s| s.overlaps(&core)) {
        return Err(Unresolved("overlapping_material"));
    }
    let gap = ms(policy.min_quiet_gap_ms);
    // Поля откладываются от ядра, расширенного на неопределённость CTC.
    let pre_total = ms(policy.start_uncertainty_ms + pre_guard_ms);
    let post_total = ms(policy.end_uncertainty_ms + post_guard_ms);
    let mut start = core.start.saturating_sub(pre_total).min(ext_start);
    if let Some(prev) = prev_end {
        if ext_start < prev + gap {
            return Err(Unresolved("no_quiet_gap_before"));
        }
        start = start.max(prev + (ext_start - prev) / 2);
    }
    let mut end = (core.end + post_total).max(ext_end).min(t.frames);
    if let Some(next) = next_start {
        if ext_end + gap > next {
            return Err(Unresolved("no_quiet_gap_after"));
        }
        end = end.min(ext_end + (next - ext_end) / 2);
    }
    let min_guard = ms(policy.min_guard_ms);
    let edge_before = core.start == 0 || start == 0;
    let edge_after = core.end >= t.frames || end >= t.frames;
    if (core.start - start < min_guard && !edge_before)
        || (end - core.end < min_guard && !edge_after)
    {
        return Err(Unresolved("guard_too_small"));
    }
    let extraction = SampleSpan { start, end };
    let context_frames = ms(policy.context_ms);
    let context = SampleSpan {
        start: start.saturating_sub(context_frames),
        end: (end + context_frames).min(t.frames),
    };
    Ok(ExtractionPlan {
        core,
        guard: SampleSpan {
            start: core.start.saturating_sub(pre_total),
            end: (core.end + post_total).min(t.frames),
        },
        extraction,
        context,
        context_limited: start - context.start < context_frames
            || context.end - end < context_frames,
        pre_guard_ms,
        post_guard_ms,
    })
}

// ---------------------------------------------------------------------------
// Решение
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RecognitionOutcome {
    /// Вся полезная запись — одно подтверждённое произношение цели.
    TargetWhole,
    /// Подтверждённое произношение цели безопасно извлечено из записи с
    /// другим материалом.
    TargetExtracted,
    /// Несколько равноправных подтверждённых вхождений.
    MultipleOccurrences,
    /// Речь есть, вхождения цели нет.
    TargetAbsent,
    /// Свидетельства противоречат друг другу или недостаточны.
    Ambiguous,
    /// Цель подтверждена, но безопасно отделить её нельзя.
    BoundariesUnresolved,
    /// Речи не найдено (в том числе после безопасного улучшения).
    NoSpeech,
    /// Цель неизвестна: выполнен только свободный анализ.
    TargetIdentityUnavailable,
    /// Обработанный фрагмент не прошёл повторную проверку.
    PostValidationFailed,
    /// Исходник технически непригоден (MP3 не прошёл проверку).
    SourceRejected,
}

impl RecognitionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TargetWhole => "TARGET_WHOLE",
            Self::TargetExtracted => "TARGET_EXTRACTED",
            Self::MultipleOccurrences => "MULTIPLE_OCCURRENCES",
            Self::TargetAbsent => "TARGET_ABSENT",
            Self::Ambiguous => "AMBIGUOUS",
            Self::BoundariesUnresolved => "BOUNDARIES_UNRESOLVED",
            Self::NoSpeech => "NO_SPEECH",
            Self::TargetIdentityUnavailable => "TARGET_IDENTITY_UNAVAILABLE",
            Self::PostValidationFailed => "POST_VALIDATION_FAILED",
            Self::SourceRejected => "SOURCE_REJECTED",
        }
    }
}

/// Следующее автоматическое действие. Состояний «ждём человека» нет.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionAction {
    /// Использовать проверенный итоговый фрагмент.
    UseFinalCandidate,
    /// Сохранить безопасную полную запись (выбор Balanced).
    KeepFullRecord,
    /// Передать все кандидаты будущему автоматическому выбору.
    PassCandidates,
    /// Отложить до следующего автоматического шага.
    Defer,
    /// Отбросить технически непригодный исходник.
    DiscardInput,
}

impl RecognitionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UseFinalCandidate => "use_final_candidate",
            Self::KeepFullRecord => "keep_full_record",
            Self::PassCandidates => "pass_candidates",
            Self::Defer => "defer",
            Self::DiscardInput => "discard_input",
        }
    }
}

pub fn action_for(outcome: RecognitionOutcome, has_candidates: bool) -> RecognitionAction {
    use RecognitionOutcome as O;
    match outcome {
        O::TargetWhole | O::TargetExtracted => RecognitionAction::UseFinalCandidate,
        O::MultipleOccurrences => RecognitionAction::PassCandidates,
        O::Ambiguous if has_candidates => RecognitionAction::PassCandidates,
        O::BoundariesUnresolved | O::PostValidationFailed => RecognitionAction::KeepFullRecord,
        O::Ambiguous | O::TargetAbsent | O::NoSpeech | O::TargetIdentityUnavailable => {
            RecognitionAction::Defer
        }
        O::SourceRejected => RecognitionAction::DiscardInput,
    }
}

/// Уровень неопределённости одного аспекта.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Confirmed,
    Uncertain,
    Failed,
    NotApplicable,
    /// Принципиально не проверяется этим этапом.
    NotAssessed,
}

/// Отдельные неопределённости: совпадение чтения, сегментация, сигнал,
/// тональный акцент, лексическая идентичность, происхождение, калибровка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Uncertainty {
    pub reading_match: Level,
    pub segmentation: Level,
    pub signal: Level,
    /// Нормативная проверка акцента требует независимого эталона.
    pub pitch_accent: Level,
    /// Совпадение чтения не доказывает написание и лексему (омонимы).
    pub lexical_identity: Level,
    pub provenance: Level,
    /// `uncalibrated`: признаки измерены, вероятность не утверждается.
    pub confidence: String,
}

/// Решение по сопоставлению до обработки сигнала.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub outcome: RecognitionOutcome,
    pub selected: Option<usize>,
    pub plan: Option<ExtractionPlan>,
    pub reasons: Vec<Reason>,
}

pub(crate) fn recognition_reason(
    code: &str,
    severity: Severity,
    message: impl Into<String>,
) -> Reason {
    Reason {
        code: code.into(),
        severity,
        message: message.into(),
        origin: Some(ReasonOrigin::Recognition),
    }
}

/// Выбор вхождения и вида результата по полному списку вхождений.
pub fn select(
    pre: &PreAnalysis,
    matching: &Matching,
    levels_db: &[f64],
    window_frames: u64,
    profile: &RecognitionProfile,
) -> Selection {
    let policy = &profile.matching;
    let threshold = pre.noise_floor_dbfs + profile.analysis.activity_above_noise_db;
    let mut reasons = Vec::new();
    let matches: Vec<&Occurrence> = matching
        .occurrences
        .iter()
        .filter(|o| o.evidence == Evidence::Match)
        .collect();
    if matches.is_empty() {
        let outcome = if matching
            .occurrences
            .iter()
            .any(|o| o.evidence != Evidence::Match)
        {
            reasons.push(recognition_reason(
                "target_not_confirmed",
                Severity::Limitation,
                "найдены похожие на цель фрагменты, но свидетельств строгого совпадения недостаточно или они противоречат друг другу",
            ));
            RecognitionOutcome::Ambiguous
        } else if pre.events.is_empty() {
            reasons.push(recognition_reason(
                "no_speech_recognized",
                Severity::Limitation,
                "основная модель не распознала речи в записи",
            ));
            RecognitionOutcome::NoSpeech
        } else {
            reasons.push(recognition_reason(
                "target_absent",
                Severity::Limitation,
                format!(
                    "распознана речь «{}», вхождений целевого чтения нет",
                    pre.primary.normalized
                ),
            ));
            RecognitionOutcome::TargetAbsent
        };
        return Selection {
            outcome,
            selected: None,
            plan: None,
            reasons,
        };
    }
    // Каждое совпадение: можно ли его безопасно отделить.
    let plans: Vec<Result<ExtractionPlan, Unresolved>> = matches
        .iter()
        .map(|o| {
            plan_extraction(
                pre,
                o,
                levels_db,
                window_frames,
                &profile.boundaries,
                threshold,
            )
        })
        .collect();
    let chosen = if matches.len() == 1 {
        Some(0)
    } else {
        let mut order: Vec<usize> = (0..matches.len()).collect();
        let snr = |i: usize| matches[i].snr_db.unwrap_or(f64::NEG_INFINITY);
        order.sort_by(|&a, &b| {
            plans[b]
                .is_ok()
                .cmp(&plans[a].is_ok())
                .then(snr(b).total_cmp(&snr(a)))
                .then(
                    matches[b]
                        .score_per_token
                        .total_cmp(&matches[a].score_per_token),
                )
        });
        let (best, second) = (order[0], order[1]);
        let separable_only_best = plans[best].is_ok() && plans[second].is_err();
        let snr_gap = snr(best) - snr(second);
        let score_gap = f64::from(matches[best].score_per_token - matches[second].score_per_token);
        if separable_only_best
            || snr_gap >= policy.selection_min_snr_gap_db
            || score_gap >= policy.selection_min_score_gap
        {
            reasons.push(recognition_reason(
                "occurrence_selected",
                Severity::Info,
                format!(
                    "из {} подтверждённых вхождений выбрано одно: отделимо только оно — {separable_only_best}, запас SNR {snr_gap:.1} dB, запас оценки {score_gap:.2}",
                    matches.len()
                ),
            ));
            Some(best)
        } else {
            None
        }
    };
    let Some(chosen) = chosen else {
        reasons.push(recognition_reason(
            "equivalent_occurrences",
            Severity::Limitation,
            format!(
                "{} равноправных подтверждённых вхождения: признаков для автоматического выбора недостаточно, кандидаты переданы дальше",
                matches.len()
            ),
        ));
        return Selection {
            outcome: RecognitionOutcome::MultipleOccurrences,
            selected: None,
            plan: None,
            reasons,
        };
    };
    let occurrence = matches[chosen];
    let others_present = !other_material(pre, occurrence, 1).is_empty();
    match &plans[chosen] {
        Ok(plan) => {
            let outcome = if others_present {
                RecognitionOutcome::TargetExtracted
            } else {
                RecognitionOutcome::TargetWhole
            };
            Selection {
                outcome,
                selected: Some(occurrence.index),
                plan: Some(plan.clone()),
                reasons,
            }
        }
        Err(Unresolved(why)) if !others_present && *why == "guard_too_small" => {
            // Вся запись — само слово, края которого у границ файла: резать
            // нечего, полная запись и есть целевое произношение.
            Selection {
                outcome: RecognitionOutcome::TargetWhole,
                selected: Some(occurrence.index),
                plan: None,
                reasons,
            }
        }
        Err(Unresolved(why)) => {
            reasons.push(recognition_reason(
                "boundaries_unresolved",
                Severity::Limitation,
                format!("целевое чтение подтверждено, но безопасно отделить его нельзя ({why}): сохранена полная запись"),
            ));
            Selection {
                outcome: RecognitionOutcome::BoundariesUnresolved,
                selected: Some(occurrence.index),
                plan: None,
                reasons,
            }
        }
    }
}

#[cfg(test)]
mod tests;
