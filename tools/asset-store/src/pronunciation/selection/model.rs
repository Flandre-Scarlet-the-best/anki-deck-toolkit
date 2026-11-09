//! Типы пакета выбора: исходный ассет, связь с целью, кандидат, рекомендация,
//! аудиоэкспертиза и решение с полномочиями. Сущности намеренно раздельны:
//! источник не знает чтения, кандидат не знает потребителя, решение не меняет
//! байтов и не является `HumanAttestation`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use super::phonetic::Comparison;
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef};
use crate::error::{AssetError, ErrorCode};
use crate::forvo::ForvoEvidence;
use crate::pronunciation::recognizer::{
    CompetitorScore, Evidence, RecognitionAction, RecognitionOutcome, TargetSourceKind,
};
use crate::pronunciation::{AudioProbe, MAX_AUDIO_BYTES, Pronunciation};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const STATE_KIND: &str = "pronunciation_selection";
/// Версия детерминированной политики рекомендаций; входит в отпечаток группы.
pub const POLICY_VERSION: &str = "pronunciation-selection-policy/1";
/// Явная политика агентского выбора по документированным свидетельствам.
pub const AGENT_EVIDENCE_POLICY: &str = "agent-evidence-v1";
pub const MAX_SOURCES: usize = 8192;
pub const MAX_LINKS: usize = 16384;
pub const MAX_DECISIONS: usize = 65536;
pub const MAX_TEXT: usize = 512;

pub(crate) fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn corrupt(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::ManifestCorrupt, message)
}

pub(crate) fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Короткая строка без управляющих символов и краевых пробелов.
pub(crate) fn valid_text(text: &str, maximum: usize) -> bool {
    !text.is_empty()
        && text.len() <= maximum
        && text.trim() == text
        && !text.chars().any(char::is_control)
}

/// Безопасный идентификатор: ASCII буквы, цифры, `-`, `_`, `.`, `:`.
pub(crate) fn valid_token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

// ---------------------------------------------------------------------------
// Исходный ассет
// ---------------------------------------------------------------------------

/// Откуда получен исходник. Чтение здесь не хранится как факт: `plan_reading`
/// Forvo — только подсказка плана, а не доказанное произношение.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceOrigin {
    LocalFile {
        /// Одноуровневое имя файла без пути (свидетельство, не идентичность).
        source_name: String,
    },
    ForvoBatch {
        batch_id: String,
        /// Написание страницы данного происхождения. В старых пакетах оно
        /// хранится в общем surface_hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<String>,
        /// Полное проверенное свидетельство Forvo для точного SHA.
        evidence: ForvoEvidence,
        /// Чтение из плана загрузки: подсказка, не подтверждение.
        plan_reading: String,
    },
}

/// Неизменяемые байты исходника, SHA и проверка MP3 при приёме.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSource {
    pub sha256: String,
    pub byte_length: u64,
    pub blob: RuntimeBlobRef,
    pub probe: AudioProbe,
    /// Все известные происхождения тех же байтов (повторный приём добавляет).
    pub origins: Vec<SourceOrigin>,
    /// Известное написание (страница, имя файла). Не чтение.
    pub surface_hint: Option<String>,
    /// Свободные гипотезы моделей без подсказки цели, если анализ был.
    pub free_analysis: Option<FreeAnalysis>,
}

impl AudioSource {
    /// Лучшее доступное свидетельство Forvo (выбранный диктор и голоса).
    pub fn forvo(&self) -> Option<(&ForvoEvidence, &str)> {
        self.origins.iter().find_map(|origin| match origin {
            SourceOrigin::ForvoBatch {
                evidence, batch_id, ..
            } => Some((evidence, batch_id.as_str())),
            SourceOrigin::LocalFile { .. } => None,
        })
    }

    /// Голоса выбранного диктора Forvo — приор выбора, не доказательство фонем.
    pub fn forvo_votes(&self) -> Option<i64> {
        let (evidence, _) = self.forvo()?;
        evidence
            .candidates
            .iter()
            .find(|candidate| candidate.record_id == evidence.selected_record_id)
            .and_then(|candidate| candidate.votes)
    }

    pub fn forvo_for(&self, surface: &str) -> Option<(&ForvoEvidence, &str)> {
        self.origins.iter().find_map(|origin| match origin {
            SourceOrigin::ForvoBatch {
                evidence,
                batch_id,
                surface: origin_surface,
                ..
            } if origin_surface.as_deref().or(self.surface_hint.as_deref()) == Some(surface) => {
                Some((evidence, batch_id.as_str()))
            }
            _ => None,
        })
    }

    pub fn forvo_votes_for(&self, surface: &str) -> Option<i64> {
        let (evidence, _) = self.forvo_for(surface)?;
        evidence
            .candidates
            .iter()
            .find(|c| c.record_id == evidence.selected_record_id)
            .and_then(|c| c.votes)
    }
}

/// Свободный анализ без цели: что услышали модели во всей записи.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreeAnalysis {
    pub config_sha256: String,
    pub primary: Option<String>,
    pub verifier: Option<String>,
    pub outcome: RecognitionOutcome,
}

// ---------------------------------------------------------------------------
// Связь источника с целью и кандидаты
// ---------------------------------------------------------------------------

/// Откуда известна пара `surface + reading` для этой связи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetOrigin {
    /// Источник цели распознавателя или явная привязка (`request`).
    pub kind: TargetSourceKind,
    pub key: String,
}

/// Итог распознавания пары «источник × цель»: только машинные свидетельства.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionSummary {
    pub recognizer_batch_id: String,
    pub config_sha256: String,
    pub outcome: RecognitionOutcome,
    pub next_action: RecognitionAction,
    pub reason_codes: Vec<String>,
    pub primary_free: Option<String>,
    pub verifier_free: Option<String>,
    pub occurrences: usize,
    pub selected_occurrence: Option<usize>,
    pub post_validation_passed: Option<bool>,
    pub post_validation_failed_checks: Vec<String>,
    /// Доступные метрики обработки; в старых состояниях их могло не быть.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_details: Option<serde_json::Value>,
    /// Выдаваемый кандидат — фрагмент записи, а не вся запись.
    pub fragment_mode: bool,
    /// Модели не услышали речи на части активности: другая речь не исключена.
    pub unrecognized_activity: bool,
}

/// Свидетельство одного найденного вхождения цели (шкала исходника).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceEvidence {
    pub index: usize,
    pub evidence: Evidence,
    pub primary_free: String,
    pub primary_distance: usize,
    pub verifier_text: Option<String>,
    pub verifier_distance: Option<usize>,
    pub best_competitor: Option<CompetitorScore>,
    pub competitor_margin: Option<f32>,
    pub snr_db: Option<f64>,
    /// Ядро и безопасный интервал (с неопределённостью границ), мс исходника.
    pub core_ms: (f64, f64),
    pub span: CandidateSpan,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// Извлечённый и повторно проверенный фрагмент цели.
    ExtractedFragment,
    /// Обработанная вся запись, прошедшая повторную проверку.
    ProcessedWhole,
    /// Безопасная полная запись Balanced (`full_record`).
    ProcessedFullRecord,
    /// Точные исходные байты.
    SourceOriginal,
    /// Найденный интервал исходника без извлечённых байтов.
    OccurrenceInterval,
}

impl CandidateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExtractedFragment => "extracted_fragment",
            Self::ProcessedWhole => "processed_whole",
            Self::ProcessedFullRecord => "processed_full_record",
            Self::SourceOriginal => "source_original",
            Self::OccurrenceInterval => "occurrence_interval",
        }
    }
}

/// Полуоткрытый интервал кадров исходника и его миллисекунды.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSpan {
    pub start_frame: u64,
    pub end_frame: u64,
    pub sample_rate: u32,
}

impl CandidateSpan {
    pub fn start_ms(&self) -> f64 {
        self.start_frame as f64 * 1000.0 / f64::from(self.sample_rate)
    }
    pub fn end_ms(&self) -> f64 {
        self.end_frame as f64 * 1000.0 / f64::from(self.sample_rate)
    }
    pub fn overlaps(&self, other: &CandidateSpan) -> bool {
        self.sample_rate == other.sample_rate
            && self.start_frame < other.end_frame
            && other.start_frame < self.end_frame
    }
}

/// Проверенные байты кандидата (копия в blob пакета выбора).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAudio {
    pub sha256: String,
    pub byte_length: u64,
    pub blob: RuntimeBlobRef,
    pub probe: AudioProbe,
}

/// Техническая пригодность по действующим проверкам владельцев.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TechnicalGate {
    pub usable: bool,
    /// `source_probe_ok`, `post_validation_passed`, `balanced_selected`,
    /// `post_validation_failed`, `output_bytes_unavailable`, `interval_not_extracted`.
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PronunciationCandidate {
    pub candidate_id: String,
    pub kind: CandidateKind,
    /// `None` только у интервала: его байты не извлечены.
    pub audio: Option<CandidateAudio>,
    /// Интервал исходника (у интервала и фрагмента).
    pub span: Option<CandidateSpan>,
    /// Вхождение, к которому относится фонетическое свидетельство.
    pub occurrence: Option<usize>,
    pub technical: TechnicalGate,
}

impl PronunciationCandidate {
    /// SHA, который подтверждает решение: байты кандидата или исходника
    /// интервала.
    pub fn addressed_sha<'a>(&'a self, source_sha: &'a str) -> &'a str {
        self.audio
            .as_ref()
            .map_or(source_sha, |a| a.sha256.as_str())
    }
}

/// Связь исходника с конкретной целью `surface + reading`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetLink {
    pub link_id: String,
    pub source_sha256: String,
    pub surface: String,
    pub reading: String,
    /// Поколение контекста распознавания и идентичности кандидатов.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub generation: u64,
    pub competing_readings: Vec<String>,
    /// Моры чтения, явно отмеченные как грамматические частицы.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub particle_morae: Vec<usize>,
    pub origin: TargetOrigin,
    pub recognition: Option<RecognitionSummary>,
    pub occurrences: Vec<OccurrenceEvidence>,
    pub candidates: Vec<PronunciationCandidate>,
}

impl TargetLink {
    pub fn pronunciation(&self) -> Result<Pronunciation, AssetError> {
        Pronunciation::new(&self.surface, &self.reading)
    }
}

// ---------------------------------------------------------------------------
// Аудиоэкспертиза
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioVerdict {
    Supports,
    Contradicts,
    Inconclusive,
}

/// Результат реального вызова аудиомодели на точных байтах.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioReview {
    pub review_id: String,
    pub backend: String,
    /// Отпечатки моделей/воркера и конфигурации вызова.
    pub model_fingerprints: Vec<String>,
    pub config_sha256: String,
    /// SHA байтов, которые реально переданы модели.
    pub audio_sha256: String,
    pub candidate_id: String,
    pub surface: String,
    pub reading: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub link_generation: u64,
    pub competitors: Vec<String>,
    pub outcome: RecognitionOutcome,
    pub verdict: AudioVerdict,
    pub reason_codes: Vec<String>,
    /// Данные не покидали машину.
    pub local: bool,
    /// Модели те же, что у первичного распознавания: это не независимое
    /// подтверждение, а целевая проверка с явными конкурентами.
    pub independent_of_triage: bool,
}

// ---------------------------------------------------------------------------
// Рекомендация
// ---------------------------------------------------------------------------

/// Степень доказанности фонетики; не путать с выбором кандидата.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneticProof {
    /// Обе модели без подсказки точно услышали целевое чтение.
    AsrConsensus,
    /// Основная модель точна, вторая расходится на одну мору класса,
    /// допускающего ошибку распознавателя; конкуренты проиграли.
    AsrToleratedDivergence,
    /// Свидетельств недостаточно (одна модель, слабое совпадение, другая мора).
    AsrUnconfirmed,
    /// Свидетельства указывают на другое чтение.
    AsrContradicted,
    /// Распознавание не выполнялось.
    NotAnalyzed,
}

/// Классификация решения для цели. Сводится к `next_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionClass {
    /// Безусловно пригоден по действующим проверкам (детерминированно).
    AutoQualified,
    /// Пригоден для выбора агентом по явной политике доверия.
    AgentSelectable,
    /// Предложен, но политика не разрешает автоматического принятия.
    AgentProposed,
    /// Нужен дополнительный аудиоанализ.
    NeedsAudioAnalysis,
    /// Нужен адресный ответ пользователя.
    NeedsUserConfirmation,
    /// Нет пригодной цели/речи: отложено до нового свидетельства.
    Deferred,
    /// Пригодных кандидатов нет (технический отказ производных).
    Rejected,
}

impl SelectionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoQualified => "auto_qualified",
            Self::AgentSelectable => "agent_selectable",
            Self::AgentProposed => "agent_proposed",
            Self::NeedsAudioAnalysis => "needs_audio_analysis",
            Self::NeedsUserConfirmation => "needs_user_confirmation",
            Self::Deferred => "deferred",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionAction {
    SelectAutomatically,
    SelectWithAgentPolicy,
    AgentOrUserDecision,
    RunAudioReview,
    AskUser,
    Defer,
    NoneAvailable,
}

impl SelectionClass {
    pub fn next_action(self) -> SelectionAction {
        match self {
            Self::AutoQualified => SelectionAction::SelectAutomatically,
            Self::AgentSelectable => SelectionAction::SelectWithAgentPolicy,
            Self::AgentProposed => SelectionAction::AgentOrUserDecision,
            Self::NeedsAudioAnalysis => SelectionAction::RunAudioReview,
            Self::NeedsUserConfirmation => SelectionAction::AskUser,
            Self::Deferred => SelectionAction::Defer,
            Self::Rejected => SelectionAction::NoneAvailable,
        }
    }
}

/// Типизированная причина рекомендации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    RecognizerUseFinalCandidate,
    AsrConsensus,
    AsrToleratedDivergence,
    DistinctMoraDivergence,
    MultipleDivergences,
    VerifierAbsent,
    VerifierUnparseable,
    WeakEvidence,
    ConflictEvidence,
    HeardCompetingReading,
    OtherSpeechPresent,
    RepeatedTarget,
    IntervalRequiresExtraction,
    OverlapsOtherReading,
    PostValidationFailed,
    OutputBytesUnavailable,
    BoundariesUnresolved,
    AmbiguousRecognition,
    TargetAbsent,
    NoSpeech,
    TargetIdentityUnavailable,
    SourceRejected,
    NoAcousticEvidence,
    ProvenanceForvo,
    ProvenanceUnverified,
    ForvoVotesPrior,
    AudioReviewSupports,
    AudioReviewContradicts,
    AudioReviewInconclusive,
    RejectedByDecision,
    PitchAccentNotAssessed,
    HomonymsNotDistinguished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAssessment {
    pub candidate_id: String,
    pub link_id: String,
    pub class: SelectionClass,
    pub proof: PhoneticProof,
    pub verifier_comparison: Option<Comparison>,
    pub reasons: Vec<ReasonCode>,
    /// Байты доступны для потребителя (не интервал, техника пройдена).
    pub consumable: bool,
}

/// Рекомендация для цели `surface + reading` по всем её связям.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRecommendation {
    pub surface: String,
    pub reading: String,
    pub policy: String,
    pub class: SelectionClass,
    pub next_action: SelectionAction,
    pub recommended: Option<String>,
    /// Все кандидаты в порядке предпочтения.
    pub candidates: Vec<CandidateAssessment>,
    pub reasons: Vec<ReasonCode>,
    /// Отпечаток: связи, кандидаты, экспертизы и рекомендация.
    pub fingerprint: String,
}

// ---------------------------------------------------------------------------
// Решение и полномочия
// ---------------------------------------------------------------------------

/// Кто и на каком основании принял решение. Разные варианты — разные права,
/// даже если выбран один и тот же SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionAuthority {
    /// Детерминированная автоматика по версии политики.
    DeterministicAutomation { policy: String },
    /// Агент по документированным свидетельствам (текст ASR и метрики).
    /// Агент запись не слушал.
    AgentEvidence { agent: String, policy: String },
    /// Агент по результату реального вызова аудиомодели на точных байтах.
    AgentAudioModel {
        agent: String,
        audio_review_id: String,
        backend: String,
    },
    /// Явный ответ пользователя, переданный агентом. Текст беседы не
    /// хранится: только SHA-256 реплики, канал и признак прослушивания,
    /// который пользователь сам заявил.
    UserViaAgent {
        agent: String,
        channel: String,
        statement_sha256: String,
        user_listened: bool,
    },
    /// Решение пользователя в файле решений локального обзора.
    UserDirectReview {
        review_id: String,
        user_listened: bool,
    },
}

impl DecisionAuthority {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DeterministicAutomation { .. } => "deterministic_automation",
            Self::AgentEvidence { .. } => "agent_evidence",
            Self::AgentAudioModel { .. } => "agent_audio_model",
            Self::UserViaAgent { .. } => "user_via_agent",
            Self::UserDirectReview { .. } => "user_direct_review",
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(
            self,
            Self::UserViaAgent { .. } | Self::UserDirectReview { .. }
        )
    }

    /// Кто-то из людей прослушал запись по собственному заявлению.
    pub fn human_listened(&self) -> bool {
        matches!(
            self,
            Self::UserViaAgent {
                user_listened: true,
                ..
            } | Self::UserDirectReview {
                user_listened: true,
                ..
            }
        )
    }

    fn validate(&self) -> Result<(), AssetError> {
        let ok = match self {
            Self::DeterministicAutomation { policy } => policy == POLICY_VERSION,
            Self::AgentEvidence { agent, policy } => {
                valid_token(agent) && policy == AGENT_EVIDENCE_POLICY
            }
            Self::AgentAudioModel {
                agent,
                audio_review_id,
                backend,
            } => valid_token(agent) && valid_token(audio_review_id) && valid_token(backend),
            Self::UserViaAgent {
                agent,
                channel,
                statement_sha256,
                ..
            } => valid_token(agent) && valid_token(channel) && valid_sha(statement_sha256),
            Self::UserDirectReview { review_id, .. } => valid_sha(review_id),
        };
        if ok {
            Ok(())
        } else {
            Err(corrupt("некорректные полномочия решения"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionAction {
    /// Выбран точный кандидат: его SHA (или SHA исходника интервала) и интервал.
    Select {
        candidate_id: String,
        addressed_sha256: String,
        source_sha256: String,
        span: Option<CandidateSpan>,
        #[serde(default, skip_serializing_if = "is_zero")]
        link_generation: u64,
    },
    /// Отклонён точный кандидат; исходник при этом не удаляется.
    RejectCandidate {
        candidate_id: String,
        addressed_sha256: String,
    },
    /// Решение отложено (например, пользователь недоступен).
    Defer,
}

/// Запись журнала решений; журнал только дополняется.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionDecision {
    pub decision_id: String,
    pub surface: String,
    pub reading: String,
    pub action: DecisionAction,
    pub authority: DecisionAuthority,
    pub reason: String,
    /// Отпечаток группы, по которой принято решение.
    pub group_fingerprint: String,
    pub state_revision: u64,
    /// Момент записи решения системой, не время исходной реплики пользователя.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_unix_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Состояние пакета
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionBatch {
    pub schema_version: u32,
    pub kind: String,
    pub batch_id: String,
    pub revision: u64,
    pub sources: Vec<AudioSource>,
    pub links: Vec<TargetLink>,
    pub audio_reviews: Vec<AudioReview>,
    pub decisions: Vec<SelectionDecision>,
}

impl SelectionBatch {
    pub fn new(batch_id: &str) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            kind: STATE_KIND.into(),
            batch_id: batch_id.into(),
            revision: 0,
            sources: Vec::new(),
            links: Vec::new(),
            audio_reviews: Vec::new(),
            decisions: Vec::new(),
        }
    }

    pub fn source(&self, sha: &str) -> Option<&AudioSource> {
        self.sources.iter().find(|s| s.sha256 == sha)
    }

    /// Все цели пакета в детерминированном порядке.
    pub fn identities(&self) -> Vec<(String, String)> {
        self.links
            .iter()
            .map(|l| (l.surface.clone(), l.reading.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn links_of<'a>(
        &'a self,
        surface: &'a str,
        reading: &'a str,
    ) -> impl Iterator<Item = &'a TargetLink> {
        self.links
            .iter()
            .filter(move |l| l.surface == surface && l.reading == reading)
    }

    /// Текущее решение цели: последняя запись журнала.
    pub fn current_decision(&self, surface: &str, reading: &str) -> Option<&SelectionDecision> {
        self.decisions
            .iter()
            .rev()
            .find(|d| d.surface == surface && d.reading == reading)
    }

    pub fn find_candidate(
        &self,
        candidate_id: &str,
    ) -> Option<(&TargetLink, &PronunciationCandidate)> {
        self.links.iter().find_map(|link| {
            link.candidates
                .iter()
                .find(|c| c.candidate_id == candidate_id)
                .map(|c| (link, c))
        })
    }

    pub fn next_decision_id(&self) -> String {
        format!("d-{}", self.decisions.len() + 1)
    }
}

/// NFC и правила `Pronunciation` для написания-подсказки.
pub(crate) fn normalize_surface(surface: &str) -> Result<String, AssetError> {
    let value: String = surface.nfc().collect();
    if !valid_text(&value, 180) {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "написание должно содержать 1–180 байт без краевых пробелов и управляющих символов",
        ));
    }
    Ok(value)
}

fn validate_blob(blob: &RuntimeBlobRef, sha: &str) -> bool {
    blob.sha256 == sha && blob.storage_path == format!("candidates/{sha}.mp3")
}

fn validate_probe(probe: &AudioProbe, sha: &str, length: u64) -> bool {
    probe.sha256 == sha && probe.byte_length == length && probe.codec == "mp3"
}

impl RuntimeBatchState for SelectionBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn maximum_blob_bytes(&self) -> u64 {
        MAX_AUDIO_BYTES
    }
    fn check_stored_compatibility(raw: &serde_json::Value) -> Result<(), AssetError> {
        if raw.get("kind").and_then(|v| v.as_str()) != Some(STATE_KIND) {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "batch_id занят пакетом другого вида; пакет выбора требует отдельного batch_id",
            ));
        }
        if raw.get("schema_version").and_then(|v| v.as_u64())
            != Some(u64::from(STATE_SCHEMA_VERSION))
        {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "неподдерживаемая схема пакета выбора",
            ));
        }
        Ok(())
    }
    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        let mut blobs: BTreeMap<String, RuntimeBlobRef> = BTreeMap::new();
        for source in &self.sources {
            blobs.insert(source.blob.sha256.clone(), source.blob.clone());
        }
        for link in &self.links {
            for candidate in &link.candidates {
                if let Some(audio) = &candidate.audio {
                    blobs.insert(audio.blob.sha256.clone(), audio.blob.clone());
                }
            }
        }
        blobs.into_values().collect()
    }
    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != STATE_SCHEMA_VERSION
            || self.kind != STATE_KIND
            || self.revision > i64::MAX as u64
            || self.sources.len() > MAX_SOURCES
            || self.links.len() > MAX_LINKS
            || self.decisions.len() > MAX_DECISIONS
        {
            return Err(corrupt("неподдерживаемая схема или размер пакета выбора"));
        }
        let mut sources = BTreeSet::new();
        for source in &self.sources {
            if !valid_sha(&source.sha256)
                || !sources.insert(source.sha256.as_str())
                || !validate_blob(&source.blob, &source.sha256)
                || !validate_probe(&source.probe, &source.sha256, source.byte_length)
                || source.origins.is_empty()
                || source
                    .surface_hint
                    .as_deref()
                    .is_some_and(|s| !valid_text(s, 180))
            {
                return Err(corrupt("некорректный исходник пакета выбора"));
            }
            for origin in &source.origins {
                match origin {
                    SourceOrigin::LocalFile { source_name } => {
                        if !valid_text(source_name, 255) || source_name.contains(['/', '\\']) {
                            return Err(corrupt("некорректное имя исходника"));
                        }
                    }
                    SourceOrigin::ForvoBatch {
                        batch_id,
                        evidence,
                        plan_reading,
                        surface: origin_surface,
                    } => {
                        let surface = origin_surface
                            .as_deref()
                            .or(source.surface_hint.as_deref())
                            .ok_or_else(|| corrupt("исходник Forvo без написания"))?;
                        let request = Pronunciation::new(surface, plan_reading)?;
                        evidence.validate(&request, &source.sha256)?;
                        if !valid_token(batch_id) {
                            return Err(corrupt("некорректный batch_id Forvo"));
                        }
                    }
                }
            }
        }
        let mut links = BTreeSet::new();
        let mut candidate_ids = BTreeSet::new();
        for link in &self.links {
            link.pronunciation()?;
            if !links.insert(link.link_id.as_str())
                || !sources.contains(link.source_sha256.as_str())
                || !valid_token(&link.link_id)
                || !valid_text(&link.origin.key, MAX_TEXT)
                || link.competing_readings.len() > 16
                || link
                    .occurrences
                    .iter()
                    .any(|o| o.span.sample_rate == 0 || o.span.start_frame >= o.span.end_frame)
            {
                return Err(corrupt("некорректная связь с целью"));
            }
            for candidate in &link.candidates {
                if !candidate_ids.insert(candidate.candidate_id.as_str())
                    || !valid_token(&candidate.candidate_id)
                    || (candidate.kind == CandidateKind::OccurrenceInterval)
                        != candidate.audio.is_none()
                    || candidate.audio.as_ref().is_some_and(|audio| {
                        !valid_sha(&audio.sha256)
                            || !validate_blob(&audio.blob, &audio.sha256)
                            || !validate_probe(&audio.probe, &audio.sha256, audio.byte_length)
                    })
                    || (candidate.kind == CandidateKind::SourceOriginal
                        && candidate.audio.as_ref().map(|a| a.sha256.as_str())
                            != Some(link.source_sha256.as_str()))
                    || candidate
                        .occurrence
                        .is_some_and(|i| !link.occurrences.iter().any(|o| o.index == i))
                    || candidate
                        .span
                        .is_some_and(|s| s.sample_rate == 0 || s.start_frame >= s.end_frame)
                {
                    return Err(corrupt("некорректный кандидат произношения"));
                }
            }
        }
        let mut review_ids = BTreeSet::new();
        for review in &self.audio_reviews {
            if !review_ids.insert(review.review_id.as_str())
                || !valid_token(&review.review_id)
                || !valid_sha(&review.audio_sha256)
                || !candidate_ids.contains(review.candidate_id.as_str())
            {
                return Err(corrupt("некорректная аудиоэкспертиза"));
            }
        }
        let mut decision_ids = BTreeSet::new();
        for decision in &self.decisions {
            decision.authority.validate()?;
            Pronunciation::new(&decision.surface, &decision.reading)?;
            if !decision_ids.insert(decision.decision_id.as_str())
                || !valid_text(&decision.reason, MAX_TEXT)
                || !valid_sha(&decision.group_fingerprint)
                || decision.state_revision > self.revision
            {
                return Err(corrupt("некорректная запись журнала решений"));
            }
            if let DecisionAuthority::AgentAudioModel {
                audio_review_id, ..
            } = &decision.authority
                && !review_ids.contains(audio_review_id.as_str())
            {
                return Err(corrupt(
                    "решение ссылается на отсутствующую аудиоэкспертизу",
                ));
            }
            match &decision.action {
                DecisionAction::Select {
                    addressed_sha256,
                    source_sha256,
                    ..
                } if !valid_sha(addressed_sha256) || !valid_sha(source_sha256) => {
                    return Err(corrupt("некорректный SHA решения"));
                }
                DecisionAction::RejectCandidate {
                    addressed_sha256, ..
                } if !valid_sha(addressed_sha256) => {
                    return Err(corrupt("некорректный SHA решения"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
