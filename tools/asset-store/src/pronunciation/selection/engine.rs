//! Детерминированная рекомендация по совокупности свидетельств.
//!
//! Правила консервативны и перечислены явно, без суммы баллов:
//!
//! - техническая пригодность — фильтр, а не балл: провалившая повторную
//!   проверку или недоступная производная запись не выбирается никогда;
//! - `auto_qualified` требует действующего решения распознавателя
//!   `use_final_candidate` **и** точного совпадения обеих моделей без подсказки;
//! - одно расхождение второй модели класса, допускающего ошибку
//!   распознавателя, при точной основной модели даёт только
//!   `agent_selectable` и только при происхождении Forvo, а фонетика
//!   помечается `asr_tolerated_divergence`, а не совпадением;
//! - другая мора (`はし`/`はち`), услышанное конкурирующее чтение, конфликт и
//!   неоднозначность требуют аудиоанализа, а после неубедительного или
//!   противоречащего аудиоанализа — ответа пользователя;
//! - голоса Forvo упорядочивают уже пригодных кандидатов и никогда не
//!   повышают класс; имя файла и словарь в рекомендации не участвуют;
//! - найденный интервал без извлечённых байтов можно выбрать, но не
//!   потребить; интервал, пересекающийся с вхождением другого чтения той же
//!   записи, требует пользователя.

use std::collections::{BTreeMap, BTreeSet};

use super::model::*;
use super::phonetic::{self, Comparison, DivergenceClass};
use crate::hashing::sha256_hex;
use crate::pronunciation::recognizer::{Evidence, RecognitionOutcome};

/// Фонетическая оценка вхождения цели.
pub(crate) fn assess_occurrence(
    link: &TargetLink,
    occurrence: &OccurrenceEvidence,
) -> (PhoneticProof, Option<Comparison>, Vec<ReasonCode>) {
    let mut reasons = Vec::new();
    let comparison = occurrence
        .verifier_text
        .as_deref()
        .map(|text| phonetic::compare(&link.reading, text, &link.particle_morae));
    // Вторая модель услышала одно из явно конкурирующих чтений.
    let heard_competitor = occurrence.verifier_text.as_deref().is_some_and(|text| {
        let heard = phonetic::hiragana_only(text);
        link.competing_readings
            .iter()
            .any(|alt| phonetic::hiragana_only(alt) == heard)
    }) || occurrence
        .best_competitor
        .as_ref()
        .is_some_and(|c| c.origin.starts_with("explicit:"))
        && occurrence.competitor_margin.is_some_and(|m| m <= 0.0);
    if heard_competitor {
        reasons.push(ReasonCode::HeardCompetingReading);
    }
    let proof = match occurrence.evidence {
        Evidence::Conflict => {
            reasons.push(ReasonCode::ConflictEvidence);
            PhoneticProof::AsrContradicted
        }
        _ if heard_competitor => PhoneticProof::AsrContradicted,
        Evidence::Weak => {
            reasons.push(ReasonCode::WeakEvidence);
            PhoneticProof::AsrUnconfirmed
        }
        Evidence::Match => match (&comparison, occurrence.primary_distance) {
            (_, distance) if distance > 0 => {
                reasons.push(ReasonCode::WeakEvidence);
                PhoneticProof::AsrUnconfirmed
            }
            (None, _) => {
                reasons.push(ReasonCode::VerifierAbsent);
                PhoneticProof::AsrUnconfirmed
            }
            (Some(Comparison::Exact), _) => {
                reasons.push(ReasonCode::AsrConsensus);
                PhoneticProof::AsrConsensus
            }
            (Some(Comparison::Unparseable), _) => {
                reasons.push(ReasonCode::VerifierUnparseable);
                PhoneticProof::AsrUnconfirmed
            }
            (Some(comparison @ Comparison::Diverges { divergences }), _) => {
                if comparison.single_possible_slip().is_some() {
                    reasons.push(ReasonCode::AsrToleratedDivergence);
                    PhoneticProof::AsrToleratedDivergence
                } else if divergences.len() == 1
                    && divergences[0].class == DivergenceClass::DistinctMora
                {
                    reasons.push(ReasonCode::DistinctMoraDivergence);
                    PhoneticProof::AsrUnconfirmed
                } else {
                    reasons.push(ReasonCode::MultipleDivergences);
                    PhoneticProof::AsrUnconfirmed
                }
            }
        },
    };
    (proof, comparison, reasons)
}

fn proof_class(proof: PhoneticProof, forvo: bool) -> SelectionClass {
    match proof {
        PhoneticProof::AsrConsensus => SelectionClass::AutoQualified,
        PhoneticProof::AsrToleratedDivergence if forvo => SelectionClass::AgentSelectable,
        PhoneticProof::AsrToleratedDivergence => SelectionClass::AgentProposed,
        _ => SelectionClass::NeedsAudioAnalysis,
    }
}

/// Индексы пакета, построенные один раз на прогон рекомендаций.
///
/// Без них оценка каждого кандидата делает линейные проходы по всем связям,
/// исходникам и аудиоэкспертизам пакета: на корпусе из тысяч исходников это
/// миллионы сравнений строк на одну команду. Порядок элементов пакета
/// сохраняется, поэтому выбор кандидата не меняется.
pub(crate) struct SelectionIndex<'a> {
    sources: BTreeMap<&'a str, &'a AudioSource>,
    candidates: BTreeMap<&'a str, (&'a TargetLink, &'a PronunciationCandidate)>,
    links_by_surface: BTreeMap<&'a str, Vec<&'a TargetLink>>,
    links_by_source: BTreeMap<&'a str, Vec<&'a TargetLink>>,
    reviews_by_candidate: BTreeMap<&'a str, Vec<&'a AudioReview>>,
    reviews_by_surface: BTreeMap<&'a str, Vec<&'a AudioReview>>,
}

impl<'a> SelectionIndex<'a> {
    pub(crate) fn new(batch: &'a SelectionBatch) -> Self {
        let mut sources: BTreeMap<&str, &AudioSource> = BTreeMap::new();
        for source in &batch.sources {
            // Первый исходник с этим SHA — как и последовательный поиск.
            sources.entry(source.sha256.as_str()).or_insert(source);
        }
        let mut links_by_surface: BTreeMap<&str, Vec<&TargetLink>> = BTreeMap::new();
        let mut links_by_source: BTreeMap<&str, Vec<&TargetLink>> = BTreeMap::new();
        let mut candidates: BTreeMap<&str, (&TargetLink, &PronunciationCandidate)> =
            BTreeMap::new();
        for link in &batch.links {
            links_by_surface
                .entry(link.surface.as_str())
                .or_default()
                .push(link);
            links_by_source
                .entry(link.source_sha256.as_str())
                .or_default()
                .push(link);
            for candidate in &link.candidates {
                // Первый кандидат с этим идентификатором — как у `find_candidate`.
                candidates
                    .entry(candidate.candidate_id.as_str())
                    .or_insert((link, candidate));
            }
        }
        let mut reviews_by_candidate: BTreeMap<&str, Vec<&AudioReview>> = BTreeMap::new();
        let mut reviews_by_surface: BTreeMap<&str, Vec<&AudioReview>> = BTreeMap::new();
        for review in &batch.audio_reviews {
            reviews_by_candidate
                .entry(review.candidate_id.as_str())
                .or_default()
                .push(review);
            reviews_by_surface
                .entry(review.surface.as_str())
                .or_default()
                .push(review);
        }
        Self {
            sources,
            candidates,
            links_by_surface,
            links_by_source,
            reviews_by_candidate,
            reviews_by_surface,
        }
    }

    fn source(&self, sha: &str) -> Option<&'a AudioSource> {
        self.sources.get(sha).copied()
    }

    /// Связь и кандидат по идентификатору, в порядке пакета.
    pub(crate) fn find_candidate(
        &self,
        candidate_id: &str,
    ) -> Option<(&'a TargetLink, &'a PronunciationCandidate)> {
        self.candidates.get(candidate_id).copied()
    }

    /// Связи цели в порядке пакета.
    fn links_of<'b>(
        &'b self,
        surface: &str,
        reading: &'b str,
    ) -> impl Iterator<Item = &'a TargetLink> + 'b {
        self.links_by_surface
            .get(surface)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |link| link.reading == reading)
    }

    /// Аудиоэкспертизы цели в порядке пакета.
    fn reviews_of<'b>(
        &'b self,
        surface: &str,
        reading: &'b str,
    ) -> impl Iterator<Item = &'a AudioReview> + 'b {
        self.reviews_by_surface
            .get(surface)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |review| review.reading == reading)
    }

    /// Последняя экспертиза точных байтов кандидата для этой цели.
    fn audio_review_for(
        &self,
        link: &TargetLink,
        candidate: &PronunciationCandidate,
    ) -> Option<&'a AudioReview> {
        let sha = candidate.addressed_sha(&link.source_sha256);
        self.reviews_by_candidate
            .get(candidate.candidate_id.as_str())?
            .iter()
            .rev()
            .find(|review| {
                review.audio_sha256 == sha
                    && review.surface == link.surface
                    && review.reading == link.reading
                    && review.link_generation == link.generation
            })
            .copied()
    }

    /// Вхождение другого чтения той же записи пересекает интервал кандидата.
    fn overlaps_other_reading(&self, link: &TargetLink, span: &CandidateSpan) -> bool {
        self.links_by_source
            .get(link.source_sha256.as_str())
            .is_some_and(|links| {
                links.iter().any(|other| {
                    (other.surface != link.surface || other.reading != link.reading)
                        && other
                            .occurrences
                            .iter()
                            .any(|o| o.evidence == Evidence::Match && o.span.overlaps(span))
                })
            })
    }
}

/// Оценка кандидата по уже построенным индексам пакета.
pub(crate) fn assess_candidate_with(
    index: &SelectionIndex<'_>,
    link: &TargetLink,
    candidate: &PronunciationCandidate,
) -> CandidateAssessment {
    let source = index.source(&link.source_sha256);
    let forvo = source.is_some_and(|s| s.forvo_for(&link.surface).is_some());
    let mut reasons = vec![if forvo {
        ReasonCode::ProvenanceForvo
    } else {
        ReasonCode::ProvenanceUnverified
    }];
    if source
        .and_then(|s| s.forvo_votes_for(&link.surface))
        .is_some()
    {
        reasons.push(ReasonCode::ForvoVotesPrior);
    }
    let occurrence = candidate
        .occurrence
        .and_then(|i| link.occurrences.iter().find(|o| o.index == i));
    let (proof, comparison, phonetic_reasons) = match occurrence {
        Some(occurrence) => assess_occurrence(link, occurrence),
        None => (PhoneticProof::NotAnalyzed, None, Vec::new()),
    };
    reasons.extend(phonetic_reasons);
    let consumable = candidate.technical.usable && candidate.audio.is_some();
    let mut class = if !candidate.technical.usable {
        reasons.push(match candidate.technical.code.as_str() {
            "post_validation_failed" => ReasonCode::PostValidationFailed,
            "output_bytes_unavailable" => ReasonCode::OutputBytesUnavailable,
            _ => ReasonCode::SourceRejected,
        });
        SelectionClass::Rejected
    } else {
        match &link.recognition {
            None => {
                reasons.push(ReasonCode::NoAcousticEvidence);
                SelectionClass::NeedsAudioAnalysis
            }
            Some(recognition) => {
                classify_by_outcome(recognition, link, candidate, proof, forvo, &mut reasons)
            }
        }
    };
    if let Some(span) = candidate.span.as_ref().filter(|_| {
        matches!(
            candidate.kind,
            CandidateKind::OccurrenceInterval | CandidateKind::ExtractedFragment
        )
    }) && index.overlaps_other_reading(link, span)
        && class < SelectionClass::NeedsUserConfirmation
    {
        reasons.push(ReasonCode::OverlapsOtherReading);
        class = SelectionClass::NeedsUserConfirmation;
    }
    // Реальная аудиоэкспертиза точных байтов меняет только классы, где её ждут.
    if let Some(review) = index.audio_review_for(link, candidate)
        && candidate.technical.usable
    {
        match review.verdict {
            AudioVerdict::Supports if !super::ops::trustworthy_audio_review(review) => {
                reasons.push(ReasonCode::AudioReviewInvalidProvenance);
                if class <= SelectionClass::NeedsAudioAnalysis {
                    class = SelectionClass::NeedsUserConfirmation;
                }
            }
            AudioVerdict::Supports => {
                reasons.push(ReasonCode::AudioReviewSupports);
                if matches!(
                    class,
                    SelectionClass::NeedsAudioAnalysis | SelectionClass::AgentProposed
                ) && proof != PhoneticProof::AsrContradicted
                    && !reasons.contains(&ReasonCode::OtherSpeechPresent)
                {
                    class = SelectionClass::AgentSelectable;
                } else if class == SelectionClass::NeedsAudioAnalysis {
                    class = SelectionClass::NeedsUserConfirmation;
                }
            }
            AudioVerdict::Contradicts => {
                reasons.push(ReasonCode::AudioReviewContradicts);
                if class <= SelectionClass::NeedsAudioAnalysis {
                    class = SelectionClass::NeedsUserConfirmation;
                }
            }
            AudioVerdict::Inconclusive => {
                reasons.push(ReasonCode::AudioReviewInconclusive);
                if class == SelectionClass::NeedsAudioAnalysis {
                    class = SelectionClass::NeedsUserConfirmation;
                }
            }
        }
    }
    if class <= SelectionClass::NeedsUserConfirmation {
        reasons.push(ReasonCode::PitchAccentNotAssessed);
        reasons.push(ReasonCode::HomonymsNotDistinguished);
    }
    reasons.sort();
    reasons.dedup();
    CandidateAssessment {
        candidate_id: candidate.candidate_id.clone(),
        link_id: link.link_id.clone(),
        class,
        proof,
        verifier_comparison: comparison,
        reasons,
        consumable,
    }
}

fn classify_by_outcome(
    recognition: &RecognitionSummary,
    link: &TargetLink,
    candidate: &PronunciationCandidate,
    proof: PhoneticProof,
    forvo: bool,
    reasons: &mut Vec<ReasonCode>,
) -> SelectionClass {
    use CandidateKind as K;
    use RecognitionOutcome as O;
    match recognition.outcome {
        O::TargetWhole | O::TargetExtracted => {
            reasons.push(ReasonCode::RecognizerUseFinalCandidate);
            let whole_material = match candidate.kind {
                K::ExtractedFragment | K::ProcessedWhole => true,
                K::SourceOriginal | K::ProcessedFullRecord => {
                    recognition.outcome == O::TargetWhole && !recognition.unrecognized_activity
                }
                K::OccurrenceInterval => false,
            };
            if candidate.kind == K::OccurrenceInterval {
                reasons.push(ReasonCode::IntervalRequiresExtraction);
                return SelectionClass::AgentProposed;
            }
            if !whole_material {
                reasons.push(ReasonCode::OtherSpeechPresent);
                return proof_class(proof, forvo).max(SelectionClass::AgentProposed);
            }
            proof_class(proof, forvo)
        }
        O::MultipleOccurrences => {
            let all_consensus = link.occurrences.iter().all(|o| {
                matches!(
                    assess_occurrence(link, o).0,
                    PhoneticProof::AsrConsensus | PhoneticProof::AsrToleratedDivergence
                )
            });
            match candidate.kind {
                K::OccurrenceInterval => {
                    reasons.push(ReasonCode::IntervalRequiresExtraction);
                    if matches!(
                        proof,
                        PhoneticProof::AsrConsensus | PhoneticProof::AsrToleratedDivergence
                    ) {
                        SelectionClass::AgentProposed
                    } else {
                        SelectionClass::NeedsAudioAnalysis
                    }
                }
                _ => {
                    reasons.push(ReasonCode::RepeatedTarget);
                    if all_consensus && !recognition.unrecognized_activity && forvo {
                        SelectionClass::AgentSelectable
                    } else if all_consensus {
                        SelectionClass::AgentProposed
                    } else {
                        SelectionClass::NeedsAudioAnalysis
                    }
                }
            }
        }
        O::Ambiguous => {
            reasons.push(ReasonCode::AmbiguousRecognition);
            if candidate.kind == K::OccurrenceInterval {
                reasons.push(ReasonCode::IntervalRequiresExtraction);
            }
            SelectionClass::NeedsAudioAnalysis
        }
        O::BoundariesUnresolved | O::PostValidationFailed => {
            reasons.push(if recognition.outcome == O::BoundariesUnresolved {
                ReasonCode::BoundariesUnresolved
            } else {
                ReasonCode::PostValidationFailed
            });
            let whole = recognition.outcome == O::PostValidationFailed
                && !recognition.fragment_mode
                && !recognition.unrecognized_activity;
            match proof {
                PhoneticProof::AsrConsensus if whole && forvo => SelectionClass::AgentSelectable,
                PhoneticProof::AsrConsensus | PhoneticProof::AsrToleratedDivergence => {
                    if !whole {
                        reasons.push(ReasonCode::OtherSpeechPresent);
                    }
                    SelectionClass::AgentProposed
                }
                _ => SelectionClass::NeedsAudioAnalysis,
            }
        }
        O::TargetAbsent => {
            reasons.push(ReasonCode::TargetAbsent);
            SelectionClass::Deferred
        }
        O::NoSpeech => {
            reasons.push(ReasonCode::NoSpeech);
            SelectionClass::Deferred
        }
        O::TargetIdentityUnavailable => {
            reasons.push(ReasonCode::TargetIdentityUnavailable);
            SelectionClass::Deferred
        }
        O::SourceRejected => {
            reasons.push(ReasonCode::SourceRejected);
            SelectionClass::Rejected
        }
    }
}

fn kind_rank(kind: CandidateKind) -> u8 {
    match kind {
        CandidateKind::ExtractedFragment => 0,
        CandidateKind::ProcessedWhole => 1,
        CandidateKind::SourceOriginal => 2,
        CandidateKind::ProcessedFullRecord => 3,
        CandidateKind::OccurrenceInterval => 4,
    }
}

/// Рекомендация для одной цели по всем её связям.
type RejectedCandidates = BTreeSet<(String, String, String, String)>;

fn rejected_candidates(batch: &SelectionBatch) -> RejectedCandidates {
    let mut rejected = BTreeSet::new();
    for decision in &batch.decisions {
        let (id, sha, is_rejection) = match &decision.action {
            DecisionAction::RejectCandidate {
                candidate_id,
                addressed_sha256,
            } => (candidate_id, addressed_sha256, true),
            DecisionAction::Select {
                candidate_id,
                addressed_sha256,
                ..
            } => (candidate_id, addressed_sha256, false),
            DecisionAction::Defer => continue,
        };
        let key = (
            decision.surface.clone(),
            decision.reading.clone(),
            id.clone(),
            sha.clone(),
        );
        if is_rejection {
            rejected.insert(key);
        } else {
            rejected.remove(&key);
        }
    }
    rejected
}

pub(crate) fn recommend(
    batch: &SelectionBatch,
    surface: &str,
    reading: &str,
) -> GroupRecommendation {
    let index = SelectionIndex::new(batch);
    let rejected = rejected_candidates(batch);
    recommend_with_index(&index, &rejected, surface, reading)
}

fn recommend_with_index(
    index: &SelectionIndex<'_>,
    rejected_candidates: &RejectedCandidates,
    surface: &str,
    reading: &str,
) -> GroupRecommendation {
    let mut ranked = Vec::new();
    for link in index.links_of(surface, reading) {
        let source = index.source(&link.source_sha256);
        let votes = source.and_then(|s| s.forvo_votes_for(surface));
        for candidate in &link.candidates {
            let mut assessment = assess_candidate_with(index, link, candidate);
            let rejected = rejected_candidates.contains(&(
                surface.to_owned(),
                reading.to_owned(),
                candidate.candidate_id.clone(),
                candidate.addressed_sha(&link.source_sha256).to_owned(),
            ));
            if rejected {
                assessment.reasons.push(ReasonCode::RejectedByDecision);
            }
            let snr = candidate
                .occurrence
                .and_then(|i| link.occurrences.iter().find(|o| o.index == i))
                .and_then(|o| o.snr_db)
                .unwrap_or(f64::NEG_INFINITY);
            ranked.push((assessment, kind_rank(candidate.kind), votes, snr, rejected));
        }
    }
    ranked.sort_by(|a, b| {
        a.4.cmp(&b.4)
            .then_with(|| a.0.class.cmp(&b.0.class))
            .then_with(|| b.0.consumable.cmp(&a.0.consumable))
            .then_with(|| a.0.proof.cmp(&b.0.proof))
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.total_cmp(&a.3))
            .then_with(|| a.0.candidate_id.cmp(&b.0.candidate_id))
    });
    let candidates: Vec<CandidateAssessment> = ranked.into_iter().map(|r| r.0).collect();
    let best = candidates
        .iter()
        .find(|c| !c.reasons.contains(&ReasonCode::RejectedByDecision));
    let class = best.map_or(
        if candidates.is_empty() {
            SelectionClass::Deferred
        } else {
            SelectionClass::Rejected
        },
        |c| c.class,
    );
    let recommended = best
        .filter(|c| c.class <= SelectionClass::NeedsUserConfirmation)
        .map(|c| c.candidate_id.clone());
    let reasons = best.map(|c| c.reasons.clone()).unwrap_or_else(|| {
        vec![if candidates.is_empty() {
            ReasonCode::NoAcousticEvidence
        } else {
            ReasonCode::RejectedByDecision
        }]
    });
    let fingerprint = group_fingerprint(index, surface, reading, &candidates, class);
    GroupRecommendation {
        surface: surface.into(),
        reading: reading.into(),
        policy: POLICY_VERSION.into(),
        class,
        next_action: class.next_action(),
        recommended,
        candidates,
        reasons,
        fingerprint,
    }
}

/// Отпечаток группы: связи, кандидаты (с байтами), экспертизы и итог.
/// Изменение любого из них делает прежний снимок и решение устаревшими.
fn group_fingerprint(
    index: &SelectionIndex<'_>,
    surface: &str,
    reading: &str,
    assessments: &[CandidateAssessment],
    class: SelectionClass,
) -> String {
    let links: Vec<&TargetLink> = index.links_of(surface, reading).collect();
    let sources: Vec<&AudioSource> = links
        .iter()
        .map(|link| link.source_sha256.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|sha| index.source(sha))
        .collect();
    let reviews: Vec<&AudioReview> = index.reviews_of(surface, reading).collect();
    sha256_hex(
        serde_json::to_vec(&(
            POLICY_VERSION,
            surface,
            reading,
            &links,
            &sources,
            &reviews,
            assessments,
            class,
        ))
        .expect("группа сериализуема"),
    )
}

/// Рекомендации всех целей пакета.
/// Рекомендации всех целей по индексам, уже построенным вызывающим: индекс
/// пакета не пересобирается на каждую цель и на каждую проверку решения.
pub(crate) fn recommend_all_with_index(
    index: &SelectionIndex<'_>,
    batch: &SelectionBatch,
) -> BTreeMap<(String, String), GroupRecommendation> {
    let rejected = rejected_candidates(batch);
    batch
        .identities()
        .into_iter()
        .map(|(surface, reading)| {
            let recommendation = recommend_with_index(index, &rejected, &surface, &reading);
            ((surface, reading), recommendation)
        })
        .collect()
}

/// Рекомендация одной цели по индексам, уже построенным вызывающим.
pub(crate) fn recommend_with(
    index: &SelectionIndex<'_>,
    batch: &SelectionBatch,
    surface: &str,
    reading: &str,
) -> GroupRecommendation {
    recommend_with_index(index, &rejected_candidates(batch), surface, reading)
}
