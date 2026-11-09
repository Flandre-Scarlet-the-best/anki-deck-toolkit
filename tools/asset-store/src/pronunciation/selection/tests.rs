use std::path::Path;

use super::engine;
use super::ops::{
    self, AgentPolicy, ConsumerTrustPolicy, DecideAction, DecideRequest, GroupFilter,
};
use super::review;
use super::*;
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::forvo::{Attempt, Candidate, ForvoEvidence, RANKING, word_url};
use crate::hashing::sha256_hex;
use crate::pronunciation::recognizer::{
    CompetitorScore, Evidence, RecognitionOutcome, TargetSourceKind, action_for,
};
use crate::pronunciation::{
    AudioProbe, Pronunciation, PronunciationDomainPolicy, probe_mp3, read_trusted,
};
use crate::temp_workspace::TempWorkspace;
use crate::{AssetStore, ErrorCode, LifecycleState, StoreOptions};

// ---------------------------------------------------------------------------
// Построители синтетических свидетельств
// ---------------------------------------------------------------------------

fn sha(n: u32) -> String {
    sha256_hex(n.to_le_bytes())
}

fn fake_probe(sha: &str) -> AudioProbe {
    AudioProbe {
        schema_version: 2,
        codec: "mp3".into(),
        sha256: sha.into(),
        byte_length: 1000,
        sample_rate: 44100,
        channels: 1,
        frames: 40,
        declared_duration_seconds: 1.0,
        decoded_duration_seconds: 1.0,
        decoded_samples: 44100,
        gapless: None,
    }
}

fn fake_audio(sha: &str) -> CandidateAudio {
    CandidateAudio {
        sha256: sha.into(),
        byte_length: 1000,
        blob: RuntimeBlobRef {
            sha256: sha.into(),
            storage_path: format!("candidates/{sha}.mp3"),
        },
        probe: fake_probe(sha),
    }
}

fn forvo_evidence(surface: &str, sha: &str, votes: i64) -> ForvoEvidence {
    ForvoEvidence {
        schema_version: 1,
        page_url: word_url(surface),
        downloaded_sha256: sha.into(),
        selected_record_id: "123".into(),
        ranking_policy: RANKING.into(),
        candidates: vec![Candidate {
            record_id: "123".into(),
            username: Some("speaker".into()),
            votes: Some(votes),
            gender: None,
            country: Some("Japan".into()),
            download_available: true,
        }],
        attempts: vec![Attempt {
            record_id: "123".into(),
            code: "downloaded".into(),
        }],
        acquisition_mode: None,
    }
}

fn fake_source(n: u32, surface: &str, forvo: bool) -> AudioSource {
    let sha = sha(n);
    let origin = if forvo {
        SourceOrigin::ForvoBatch {
            batch_id: "forvo".into(),
            surface: Some(surface.into()),
            evidence: forvo_evidence(surface, &sha, 4),
            plan_reading: Some("なま".into()),
        }
    } else {
        SourceOrigin::LocalFile {
            source_name: format!("{surface}.mp3"),
        }
    };
    AudioSource {
        sha256: sha.clone(),
        byte_length: 1000,
        blob: fake_audio(&sha).blob,
        probe: fake_probe(&sha),
        origins: vec![origin],
        surface_hint: Some(surface.into()),
        free_analysis: None,
    }
}

fn span(start_ms: f64, end_ms: f64) -> CandidateSpan {
    CandidateSpan {
        start_frame: (start_ms * 44.1) as u64,
        end_frame: (end_ms * 44.1) as u64,
        sample_rate: 44100,
    }
}

fn occurrence(
    index: usize,
    evidence: Evidence,
    verifier: Option<&str>,
    primary_distance: usize,
    window: (f64, f64),
) -> OccurrenceEvidence {
    OccurrenceEvidence {
        index,
        evidence,
        primary_free: "n a m a".into(),
        primary_distance,
        verifier_text: verifier.map(str::to_owned),
        verifier_distance: verifier.map(|_| 0),
        best_competitor: None,
        competitor_margin: Some(3.0),
        snr_db: Some(30.0),
        core_ms: window,
        span: span(window.0, window.1),
        reasons: Vec::new(),
    }
}

fn recognition(outcome: RecognitionOutcome) -> RecognitionSummary {
    RecognitionSummary {
        recognizer_batch_id: "rb".into(),
        config_sha256: sha(9999),
        outcome,
        next_action: action_for(outcome, true),
        reason_codes: Vec::new(),
        primary_free: None,
        verifier_free: None,
        occurrences: 1,
        selected_occurrence: Some(0),
        post_validation_passed: Some(true),
        post_validation_failed_checks: Vec::new(),
        quality_details: None,
        fragment_mode: outcome == RecognitionOutcome::TargetExtracted,
        unrecognized_activity: false,
    }
}

fn candidate(
    link_id: &str,
    kind: CandidateKind,
    audio: Option<&str>,
    usable: bool,
    occurrence: Option<usize>,
    window: Option<(f64, f64)>,
) -> PronunciationCandidate {
    PronunciationCandidate {
        candidate_id: format!(
            "c-{}",
            &sha256_hex(format!(
                "{link_id}{kind:?}{audio:?}{window:?}{occurrence:?}"
            ))[..16]
        ),
        kind,
        audio: audio.map(fake_audio),
        span: window.map(|w| span(w.0, w.1)),
        occurrence,
        derivation: None,
        technical: TechnicalGate {
            usable,
            code: if usable {
                "ok"
            } else {
                "post_validation_failed"
            }
            .into(),
        },
    }
}

/// Связь «вся запись — цель» с оригиналом и обработанной записью.
fn whole_link(source: &AudioSource, surface: &str, reading: &str, verifier: &str) -> TargetLink {
    let id = ops::link_id(&source.sha256, surface, reading);
    let derived = sha256_hex(format!("derived-{}", source.sha256));
    TargetLink {
        link_id: id.clone(),
        source_sha256: source.sha256.clone(),
        surface: surface.into(),
        reading: reading.into(),
        generation: 0,
        competing_readings: Vec::new(),
        particle_morae: Vec::new(),
        origin: TargetOrigin {
            kind: TargetSourceKind::Manifest,
            key: "manifest:test".into(),
        },
        recognition: Some(recognition(RecognitionOutcome::TargetWhole)),
        occurrences: vec![occurrence(
            0,
            Evidence::Match,
            Some(verifier),
            0,
            (100.0, 600.0),
        )],
        candidates: vec![
            candidate(
                &id,
                CandidateKind::SourceOriginal,
                Some(&source.sha256),
                true,
                Some(0),
                None,
            ),
            candidate(
                &id,
                CandidateKind::ProcessedWhole,
                Some(&derived),
                true,
                Some(0),
                None,
            ),
        ],
    }
}

fn batch_with(sources: Vec<AudioSource>, links: Vec<TargetLink>) -> SelectionBatch {
    let mut batch = SelectionBatch::new("selection");
    batch.sources = sources;
    batch.links = links;
    batch
}

// ---------------------------------------------------------------------------
// Движок рекомендаций
// ---------------------------------------------------------------------------

#[test]
fn whole_target_with_asr_consensus_is_auto_qualified_and_prefers_processed_bytes() {
    let source = fake_source(1, "本", false);
    let link = whole_link(&source, "本", "ほん", "ほん");
    let batch = batch_with(vec![source], vec![link.clone()]);
    let recommendation = engine::recommend(&batch, "本", "ほん");
    assert_eq!(recommendation.class, SelectionClass::AutoQualified);
    assert_eq!(
        recommendation.candidates[0].proof,
        PhoneticProof::AsrConsensus
    );
    assert_eq!(
        recommendation.recommended.as_deref(),
        Some(link.candidates[1].candidate_id.as_str())
    );
    assert!(
        recommendation
            .reasons
            .contains(&ReasonCode::PitchAccentNotAssessed)
    );
    assert!(
        recommendation
            .reasons
            .contains(&ReasonCode::HomonymsNotDistinguished)
    );
}

#[test]
fn verifier_voicing_slip_is_tolerated_only_as_agent_selectable_with_forvo_provenance() {
    let forvo = fake_source(1, "生", true);
    let link = whole_link(&forvo, "生", "しょう", "じょう");
    let batch = batch_with(vec![forvo], vec![link]);
    let recommendation = engine::recommend(&batch, "生", "しょう");
    assert_eq!(recommendation.class, SelectionClass::AgentSelectable);
    assert_eq!(
        recommendation.candidates[0].proof,
        PhoneticProof::AsrToleratedDivergence
    );
    assert!(
        recommendation
            .reasons
            .contains(&ReasonCode::AsrToleratedDivergence)
    );

    let local = fake_source(2, "生", false);
    let link = whole_link(&local, "生", "しょう", "じょう");
    let batch = batch_with(vec![local], vec![link]);
    assert_eq!(
        engine::recommend(&batch, "生", "しょう").class,
        SelectionClass::AgentProposed
    );
}

#[test]
fn hashi_heard_as_hachi_is_not_forgiven_even_when_recognizer_accepted() {
    let source = fake_source(1, "橋", true);
    let link = whole_link(&source, "橋", "はし", "はち");
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "橋", "はし");
    assert_eq!(recommendation.class, SelectionClass::NeedsAudioAnalysis);
    assert_eq!(
        recommendation.candidates[0].proof,
        PhoneticProof::AsrUnconfirmed
    );
    assert!(
        recommendation
            .reasons
            .contains(&ReasonCode::DistinctMoraDivergence)
    );
    assert_eq!(recommendation.next_action, SelectionAction::RunAudioReview);
}

#[test]
fn e_and_he_are_tolerated_only_in_an_explicit_particle_position() {
    let source = fake_source(1, "へ", true);
    let mut link = whole_link(&source, "へ", "へ", "え");
    let batch = batch_with(vec![source.clone()], vec![link.clone()]);
    assert_eq!(
        engine::recommend(&batch, "へ", "へ").class,
        SelectionClass::NeedsAudioAnalysis
    );
    link.particle_morae = vec![0];
    let batch = batch_with(vec![source], vec![link]);
    assert_eq!(
        engine::recommend(&batch, "へ", "へ").class,
        SelectionClass::AgentSelectable
    );
}

#[test]
fn heard_competing_reading_contradicts_the_target() {
    let source = fake_source(1, "生", true);
    let mut link = whole_link(&source, "生", "なま", "せい");
    link.competing_readings = vec!["せい".into(), "しょう".into()];
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "生", "なま");
    assert_eq!(
        recommendation.candidates[0].proof,
        PhoneticProof::AsrContradicted
    );
    assert!(
        recommendation
            .reasons
            .contains(&ReasonCode::HeardCompetingReading)
    );
    assert_eq!(recommendation.class, SelectionClass::NeedsAudioAnalysis);

    // Победивший явный конкурент на кадрах вхождения — то же самое.
    let source = fake_source(2, "生", true);
    let mut link = whole_link(&source, "生", "なま", "なま");
    link.competing_readings = vec!["せい".into()];
    link.occurrences[0].best_competitor = Some(CompetitorScore {
        phonemes: "s e i".into(),
        origin: "explicit:せい".into(),
        score: 0.0,
    });
    link.occurrences[0].competitor_margin = Some(-0.5);
    let batch = batch_with(vec![source], vec![link]);
    assert_eq!(
        engine::recommend(&batch, "生", "なま").candidates[0].proof,
        PhoneticProof::AsrContradicted
    );
}

#[test]
fn failed_post_validation_derivative_is_never_recommended() {
    let source = fake_source(1, "本", false);
    let mut link = whole_link(&source, "本", "ほん", "ほん");
    link.recognition = Some(RecognitionSummary {
        post_validation_passed: Some(false),
        ..recognition(RecognitionOutcome::PostValidationFailed)
    });
    link.candidates[1].technical = TechnicalGate {
        usable: false,
        code: "post_validation_failed".into(),
    };
    let failed = link.candidates[1].candidate_id.clone();
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "本", "ほん");
    assert_ne!(recommendation.recommended.as_deref(), Some(failed.as_str()));
    let failed = recommendation
        .candidates
        .iter()
        .find(|c| c.candidate_id == failed)
        .unwrap();
    assert_eq!(failed.class, SelectionClass::Rejected);
    assert!(failed.reasons.contains(&ReasonCode::PostValidationFailed));
    // Полная исходная запись остаётся и может быть выбрана агентом.
    assert_eq!(recommendation.class, SelectionClass::AgentProposed);
}

/// Связь записи `せい → しょう → なま` с одной из целей и интервалами.
fn multi_reading_link(
    source: &AudioSource,
    reading: &str,
    outcome: RecognitionOutcome,
    windows: &[(f64, f64)],
) -> TargetLink {
    let id = ops::link_id(&source.sha256, "生", reading);
    let occurrences: Vec<_> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| occurrence(i, Evidence::Match, Some(reading), 0, *w))
        .collect();
    let mut candidates = vec![candidate(
        &id,
        CandidateKind::SourceOriginal,
        Some(&source.sha256),
        true,
        Some(0),
        None,
    )];
    for (i, w) in windows.iter().enumerate() {
        candidates.push(candidate(
            &id,
            CandidateKind::OccurrenceInterval,
            None,
            true,
            Some(i),
            Some(*w),
        ));
    }
    TargetLink {
        link_id: id,
        source_sha256: source.sha256.clone(),
        surface: "生".into(),
        reading: reading.into(),
        generation: 0,
        competing_readings: Vec::new(),
        particle_morae: Vec::new(),
        origin: TargetOrigin {
            kind: TargetSourceKind::Manifest,
            key: "manifest:test".into(),
        },
        recognition: Some(recognition(outcome)),
        occurrences,
        candidates,
    }
}

#[test]
fn each_reading_of_one_recording_is_a_separate_target_and_intervals_need_extraction() {
    let source = fake_source(1, "生", true);
    let sei = multi_reading_link(
        &source,
        "せい",
        RecognitionOutcome::TargetExtracted,
        &[(0.0, 400.0)],
    );
    let nama = multi_reading_link(
        &source,
        "なま",
        RecognitionOutcome::MultipleOccurrences,
        &[(1200.0, 1600.0), (2000.0, 2400.0)],
    );
    let batch = batch_with(vec![source], vec![sei, nama.clone()]);
    let index = engine::SelectionIndex::new(&batch);
    let all = engine::recommend_all_with_index(&index, &batch);
    assert_eq!(all.len(), 2, "каждое чтение — отдельная цель");
    let nama_rec = &all[&("生".to_string(), "なま".to_string())];
    let interval = nama_rec
        .candidates
        .iter()
        .find(|c| c.candidate_id == nama.candidates[1].candidate_id)
        .unwrap();
    assert!(!interval.consumable);
    assert!(
        interval
            .reasons
            .contains(&ReasonCode::IntervalRequiresExtraction)
    );
    // Повтор одного чтения целиком: агент может выбрать исходник с риском.
    assert_eq!(nama_rec.class, SelectionClass::AgentSelectable);
    assert!(nama_rec.reasons.contains(&ReasonCode::RepeatedTarget));
}

#[test]
fn interval_overlapping_another_confirmed_reading_requires_the_user() {
    let source = fake_source(1, "剣", true);
    let mut ken = multi_reading_link(
        &source,
        "けん",
        RecognitionOutcome::Ambiguous,
        &[(100.0, 500.0)],
    );
    ken.surface = "剣".into();
    let mut tsurugi = multi_reading_link(
        &source,
        "つるぎ",
        RecognitionOutcome::Ambiguous,
        &[(300.0, 900.0)],
    );
    tsurugi.surface = "剣".into();
    ken.link_id = ops::link_id(&source.sha256, "剣", "けん");
    tsurugi.link_id = ops::link_id(&source.sha256, "剣", "つるぎ");
    let interval = ken.candidates[1].candidate_id.clone();
    let batch = batch_with(vec![source], vec![ken, tsurugi]);
    let recommendation = engine::recommend(&batch, "剣", "けん");
    let assessment = recommendation
        .candidates
        .iter()
        .find(|c| c.candidate_id == interval)
        .unwrap();
    assert_eq!(assessment.class, SelectionClass::NeedsUserConfirmation);
    assert!(
        assessment
            .reasons
            .contains(&ReasonCode::OverlapsOtherReading)
    );
}

#[test]
fn absent_target_is_deferred_and_unanalysed_link_is_not_proven_by_its_name() {
    let source = fake_source(1, "本", true);
    let mut link = whole_link(&source, "本", "ほん", "ほん");
    link.recognition = Some(recognition(RecognitionOutcome::TargetAbsent));
    link.occurrences.clear();
    for c in &mut link.candidates {
        c.occurrence = None;
    }
    let batch = batch_with(vec![source.clone()], vec![link.clone()]);
    let recommendation = engine::recommend(&batch, "本", "ほん");
    assert_eq!(recommendation.class, SelectionClass::Deferred);
    assert_eq!(recommendation.recommended, None);

    // Имя файла и чтение плана Forvo не дают фонетического доказательства.
    link.recognition = None;
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "本", "ほん");
    assert_eq!(recommendation.class, SelectionClass::NeedsAudioAnalysis);
    assert!(
        recommendation
            .candidates
            .iter()
            .all(|c| c.proof == PhoneticProof::NotAnalyzed)
    );
}

fn audio_review(
    candidate: &PronunciationCandidate,
    sha: &str,
    verdict: AudioVerdict,
) -> AudioReview {
    AudioReview {
        review_id: "a-1".into(),
        backend: ops::LOCAL_AUDIO_BACKEND.into(),
        model_fingerprints: vec![sha256_hex("model")],
        config_sha256: sha256_hex("config"),
        audio_sha256: sha.into(),
        candidate_id: candidate.candidate_id.clone(),
        surface: "橋".into(),
        reading: "はし".into(),
        link_generation: 0,
        competitors: vec!["はち".into()],
        outcome: RecognitionOutcome::TargetWhole,
        verdict,
        reason_codes: Vec::new(),
        local: true,
        independent_of_triage: false,
    }
}

#[test]
fn audio_review_on_exact_bytes_changes_only_awaiting_classes() {
    let source = fake_source(1, "橋", true);
    let link = whole_link(&source, "橋", "はし", "はち");
    let processed = link.candidates[1].clone();
    let processed_sha = processed.audio.as_ref().unwrap().sha256.clone();
    let mut batch = batch_with(vec![source], vec![link]);
    batch.audio_reviews = vec![audio_review(
        &processed,
        &processed_sha,
        AudioVerdict::Supports,
    )];
    let supported = engine::recommend(&batch, "橋", "はし");
    assert_eq!(supported.class, SelectionClass::AgentSelectable);
    assert_eq!(
        supported.recommended.as_deref(),
        Some(processed.candidate_id.as_str())
    );
    // Экспертиза других байтов не относится к кандидату.
    batch.audio_reviews[0].audio_sha256 = sha(77);
    assert_eq!(
        engine::recommend(&batch, "橋", "はし").class,
        SelectionClass::NeedsAudioAnalysis
    );
    batch.audio_reviews = vec![audio_review(
        &processed,
        &processed_sha,
        AudioVerdict::Contradicts,
    )];
    let contradicted = engine::recommend(&batch, "橋", "はし");
    assert_eq!(
        contradicted.class,
        SelectionClass::NeedsAudioAnalysis,
        "лучший — оригинал без экспертизы"
    );
    let processed_assessment = contradicted
        .candidates
        .iter()
        .find(|c| c.candidate_id == processed.candidate_id)
        .unwrap();
    assert_eq!(
        processed_assessment.class,
        SelectionClass::NeedsUserConfirmation
    );
}

/// 500 записей: 487 бесспорных и 13 неоднозначных. Автоматическая часть
/// завершается без ожидания пользователя; спорные сохраняют варианты.
#[test]
fn batch_of_500_completes_487_automatically_and_keeps_13_for_later() {
    let mut sources = Vec::new();
    let mut links = Vec::new();
    for n in 0..500u32 {
        let surface = format!("語{n}");
        let source = fake_source(n, &surface, false);
        let link = if n < 487 {
            whole_link(&source, &surface, "ご", "ご")
        } else {
            let mut link = whole_link(&source, &surface, "ご", "ご");
            link.recognition = Some(recognition(RecognitionOutcome::Ambiguous));
            link.occurrences[0].evidence = Evidence::Weak;
            link
        };
        sources.push(source);
        links.push(link);
    }
    let mut batch = batch_with(sources, links);
    let (decisions, report) = ops::plan_auto_select(&batch, None);
    assert_eq!(report.selected_deterministic, 487);
    assert_eq!(decisions.len(), 487);
    assert!(decisions.iter().all(|d| matches!(
        d.authority,
        DecisionAuthority::DeterministicAutomation { .. }
    )));
    let pending = &report.pending["needs_audio_analysis"];
    assert_eq!(pending.len(), 13);
    // Спорные цели сохраняют все варианты для последующего решения.
    for (surface, reading) in pending {
        let recommendation = engine::recommend(&batch, surface, reading);
        assert_eq!(recommendation.candidates.len(), 2);
        assert!(recommendation.recommended.is_some());
    }
    // Адресный ответ разрешает одну спорную цель, не меняя остальные 487
    // и не теряя варианты двенадцати оставшихся целей.
    batch.decisions.extend(decisions);
    let (surface, reading) = &pending[0];
    let recommendation = engine::recommend(&batch, surface, reading);
    let candidate = &recommendation.candidates[1].candidate_id;
    let (link, value) = batch.find_candidate(candidate).unwrap();
    let choice = DecideAction::Select {
        candidate_id: candidate.clone(),
        expected_sha256: value.addressed_sha(&link.source_sha256).into(),
    };
    let authority = user("выберите второй вариант", false);
    let action = ops::validate_decision(&batch, &recommendation, &choice, &authority).unwrap();
    batch.decisions.push(SelectionDecision {
        decision_id: "manual".into(),
        surface: surface.clone(),
        reading: reading.clone(),
        action,
        authority,
        reason: "адресный ответ".into(),
        group_fingerprint: recommendation.fingerprint,
        state_revision: 0,
        recorded_at_unix_ms: None,
    });
    let (later, report) = ops::plan_auto_select(&batch, None);
    assert!(later.is_empty());
    assert_eq!(report.already_decided, 488);
    assert_eq!(report.pending["needs_audio_analysis"].len(), 12);
    for (surface, reading) in &report.pending["needs_audio_analysis"] {
        assert_eq!(
            engine::recommend(&batch, surface, reading).candidates.len(),
            2
        );
    }
}

#[test]
fn agent_policy_is_explicit_and_recorded_as_agent_evidence() {
    let source = fake_source(1, "生", true);
    let link = whole_link(&source, "生", "しょう", "じょう");
    let batch = batch_with(vec![source], vec![link]);
    let (decisions, report) = ops::plan_auto_select(&batch, None);
    assert!(decisions.is_empty());
    assert_eq!(report.pending["agent_selectable"].len(), 1);
    let policy = AgentPolicy {
        agent: "codex-agent".into(),
    };
    let (decisions, report) = ops::plan_auto_select(&batch, Some(&policy));
    assert_eq!(report.selected_agent_evidence, 1);
    assert_eq!(
        decisions[0].authority,
        DecisionAuthority::AgentEvidence {
            agent: "codex-agent".into(),
            policy: AGENT_EVIDENCE_POLICY.into(),
        }
    );
    assert!(!decisions[0].authority.human_listened());
}

#[test]
fn deterministic_authority_expires_when_only_agent_policy_remains() {
    let source = fake_source(1, "生", true);
    let link = whole_link(&source, "生", "しょう", "しょう");
    let mut batch = batch_with(vec![source], vec![link]);
    let (decisions, _) = ops::plan_auto_select(&batch, None);
    assert_eq!(decisions.len(), 1);
    let index = engine::SelectionIndex::new(&batch);
    assert!(ops::decision_is_effective(&index, &batch, &decisions[0]));
    batch.links[0].occurrences[0].verifier_text = Some("じょう".into());
    assert_eq!(
        engine::recommend(&batch, "生", "しょう").class,
        SelectionClass::AgentSelectable
    );
    let index = engine::SelectionIndex::new(&batch);
    assert!(!ops::decision_is_effective(&index, &batch, &decisions[0]));
}

#[test]
fn text_only_agent_cannot_select_what_needs_audio_and_user_answer_is_addressed() {
    let source = fake_source(1, "橋", true);
    let other = fake_source(2, "本", true);
    let link = whole_link(&source, "橋", "はし", "はち");
    let foreign = whole_link(&other, "本", "ほん", "ほん");
    let batch = batch_with(
        vec![source.clone(), other],
        vec![link.clone(), foreign.clone()],
    );
    let recommendation = engine::recommend(&batch, "橋", "はし");
    let target = &link.candidates[1];
    let sha = target.audio.as_ref().unwrap().sha256.clone();
    let select = DecideAction::Select {
        candidate_id: target.candidate_id.clone(),
        expected_sha256: sha.clone(),
    };
    let agent = DecisionAuthority::AgentEvidence {
        agent: "codex-agent".into(),
        policy: AGENT_EVIDENCE_POLICY.into(),
    };
    let refused = ops::validate_decision(&batch, &recommendation, &select, &agent).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let user = DecisionAuthority::UserViaAgent {
        agent: "codex-agent".into(),
        channel: "agent-dialog".into(),
        statement_sha256: sha256_hex("выберите второй вариант"),
        user_listened: false,
    };
    assert!(ops::validate_decision(&batch, &recommendation, &select, &user).is_ok());
    // Одна реплика не подтверждает чужую пару или другие байты.
    let foreign_select = DecideAction::Select {
        candidate_id: foreign.candidates[0].candidate_id.clone(),
        expected_sha256: other_sha(&foreign),
    };
    assert_eq!(
        ops::validate_decision(&batch, &recommendation, &foreign_select, &user)
            .unwrap_err()
            .code,
        ErrorCode::IdentityConflict
    );
    let wrong_sha = DecideAction::Select {
        candidate_id: target.candidate_id.clone(),
        expected_sha256: source.sha256.clone(),
    };
    assert_eq!(
        ops::validate_decision(&batch, &recommendation, &wrong_sha, &user)
            .unwrap_err()
            .code,
        ErrorCode::IntegrityMismatch
    );
}

fn other_sha(link: &TargetLink) -> String {
    link.candidates[0].audio.as_ref().unwrap().sha256.clone()
}

#[test]
fn technically_failed_candidate_cannot_be_selected_by_anyone() {
    let source = fake_source(1, "本", false);
    let mut link = whole_link(&source, "本", "ほん", "ほん");
    link.candidates[1].technical = TechnicalGate {
        usable: false,
        code: "post_validation_failed".into(),
    };
    let failed = link.candidates[1].clone();
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "本", "ほん");
    let user = DecisionAuthority::UserDirectReview {
        review_id: sha(5),
        user_listened: true,
    };
    let refused = ops::validate_decision(
        &batch,
        &recommendation,
        &DecideAction::Select {
            candidate_id: failed.candidate_id.clone(),
            expected_sha256: failed.audio.unwrap().sha256,
        },
        &user,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
}

#[test]
fn group_filter_parses_classes() {
    assert_eq!(
        GroupFilter::parse("needs-user-confirmation").unwrap(),
        GroupFilter::Class(SelectionClass::NeedsUserConfirmation)
    );
    assert!(GroupFilter::parse("everything").is_err());
}

// ---------------------------------------------------------------------------
// Хранилище, runtime и обзор с настоящими MP3
// ---------------------------------------------------------------------------

fn mp3(frequency: u32) -> Vec<u8> {
    let workspace = TempWorkspace::create("selection-mp3").unwrap();
    let path = workspace.path().join("fixture.mp3");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={frequency}:duration=0.6:sample_rate=44100"
        ))
        .args(["-c:a", "libmp3lame", "-q:a", "4"])
        .arg(&path)
        .status()
        .expect("для тестов нужен FFmpeg");
    assert!(status.success());
    std::fs::read(path).unwrap()
}

fn store() -> (TempWorkspace, AssetStore) {
    let workspace = TempWorkspace::create("selection-store").unwrap();
    let store = AssetStore::open_with_policy(
        StoreOptions::new(workspace.path().join("store")),
        PronunciationDomainPolicy,
    )
    .unwrap();
    (workspace, store)
}

fn mutate(
    store: &AssetStore,
    batch_id: &str,
    f: impl FnOnce(&mut SelectionBatch, &SafeBatchRuntime),
) {
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id).unwrap();
    let mut batch: SelectionBatch = runtime.load().unwrap().unwrap();
    f(&mut batch, &runtime);
    batch.revision += 1;
    batch.validate().unwrap();
    runtime.save(&batch).unwrap();
}

fn load(store: &AssetStore, batch_id: &str) -> SelectionBatch {
    SafeBatchRuntime::open(store.root(), batch_id)
        .unwrap()
        .load()
        .unwrap()
        .unwrap()
}

fn real_audio(runtime: &SafeBatchRuntime, bytes: &[u8]) -> CandidateAudio {
    let blob = runtime
        .persist_blob_with_limit(bytes, "mp3", crate::pronunciation::MAX_AUDIO_BYTES)
        .unwrap();
    CandidateAudio {
        sha256: blob.sha256.clone(),
        byte_length: bytes.len() as u64,
        blob,
        probe: probe_mp3(bytes).unwrap(),
    }
}

fn user(statement: &str, listened: bool) -> DecisionAuthority {
    DecisionAuthority::UserViaAgent {
        agent: "codex-agent".into(),
        channel: "agent-dialog".into(),
        statement_sha256: sha256_hex(statement),
        user_listened: listened,
    }
}

/// Сценарий без карточки: приём без чтения → отдельная связь с чтением →
/// ответ пользователя через агента → чтение по политике → явная публикация.
#[test]
fn audio_is_staged_without_reading_or_deck_and_promoted_only_explicitly() {
    let (workspace, store) = store();
    let bytes = mp3(440);
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "生.mp3".into(),
            bytes: bytes.clone(),
        }],
        Some("生"),
    )
    .unwrap();
    assert_eq!(intake.added.len(), 1);
    let source_sha = intake.added[0].clone();
    let rejected = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "broken.mp3".into(),
            bytes: b"<html>".to_vec(),
        }],
        None,
    )
    .unwrap();
    assert_eq!(
        rejected.rejected.len(),
        1,
        "непригодный файл получает свой исход"
    );
    assert_eq!(rejected.sources, 1);
    let status = status_value(&store, "sel", GroupFilter::All, 10).unwrap();
    assert_eq!(status["groups"], 0, "исходник хранится без цели");
    assert!(!workspace.path().join("decks").exists());

    let nama = link_target(
        &store,
        "sel",
        &source_sha,
        "生",
        "なま",
        vec!["せい".into()],
        vec![],
    )
    .unwrap();
    let sei = link_target(&store, "sel", &source_sha, "生", "せい", vec![], vec![]).unwrap();
    let candidate = &nama.candidates[0];
    let agent = DecideRequest {
        surface: "生".into(),
        reading: "なま".into(),
        action: DecideAction::Select {
            candidate_id: candidate.candidate_id.clone(),
            expected_sha256: source_sha.clone(),
        },
        authority: DecisionAuthority::AgentEvidence {
            agent: "codex-agent".into(),
            policy: AGENT_EVIDENCE_POLICY.into(),
        },
        reason: "по тексту".into(),
        expected_fingerprint: None,
    };
    assert_eq!(
        decide(&store, "sel", agent).unwrap_err().code,
        ErrorCode::InvalidValidationEvidence,
        "без распознавания текстовый агент не выбирает"
    );
    let foreign = DecideRequest {
        surface: "生".into(),
        reading: "なま".into(),
        action: DecideAction::Select {
            candidate_id: sei.candidates[0].candidate_id.clone(),
            expected_sha256: source_sha.clone(),
        },
        authority: user("выберите этот файл для なま", false),
        reason: "пользователь".into(),
        expected_fingerprint: None,
    };
    assert_eq!(
        decide(&store, "sel", foreign).unwrap_err().code,
        ErrorCode::IdentityConflict
    );
    let answer = || DecideRequest {
        surface: "生".into(),
        reading: "なま".into(),
        action: DecideAction::Select {
            candidate_id: candidate.candidate_id.clone(),
            expected_sha256: source_sha.clone(),
        },
        authority: user("выберите этот файл для なま", false),
        reason: "ответ пользователя в диалоге".into(),
        expected_fingerprint: None,
    };
    assert!(decide(&store, "sel", answer()).unwrap().changed);
    assert!(
        !decide(&store, "sel", answer()).unwrap().changed,
        "повтор идемпотентен"
    );

    let requests = [Pronunciation::new("生", "なま").unwrap()];
    let selected = read_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap();
    assert_eq!(selected[0].bytes, bytes);
    assert!(!selected[0].info.authority.human_listened());
    // Строгий потребитель человеческого Verified ничего не получает.
    assert!(read_trusted(store.root(), &requests).is_err());

    // HumanAttestation без заявленного прослушивания не создаётся.
    let refused = promote(
        &store,
        "sel",
        "生",
        "なま",
        None,
        true,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let promoted = promote(
        &store,
        "sel",
        "生",
        "なま",
        None,
        false,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert!(!promoted.human_attestation_created);
    assert_ne!(promoted.asset.lifecycle, LifecycleState::Verified);
    assert!(promoted.asset.human_attestation.is_none());
    assert!(read_trusted(store.root(), &requests).is_err());

    // Пользователь сам сказал, что прослушал: только тогда — attestation.
    decide(
        &store,
        "sel",
        DecideRequest {
            authority: user("я прослушал, выберите этот файл", true),
            ..answer()
        },
    )
    .unwrap();
    let attested = promote(
        &store,
        "sel",
        "生",
        "なま",
        None,
        true,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert!(attested.human_attestation_created);
    assert_eq!(attested.asset.lifecycle, LifecycleState::Verified);
    assert!(
        attested
            .asset
            .human_attestation
            .as_ref()
            .unwrap()
            .reason
            .contains("пользователь заявил прослушивание")
    );
    assert_eq!(
        read_trusted(store.root(), &requests).unwrap()[0].bytes,
        bytes
    );
}

/// Связь с распознаванием «вся запись — цель» на настоящих байтах.
fn add_recognized_link(
    store: &AssetStore,
    batch_id: &str,
    source_sha: &str,
    surface: &str,
    reading: &str,
    verifier: &str,
    processed: &[u8],
) -> TargetLink {
    let mut created = None;
    mutate(store, batch_id, |batch, runtime| {
        let source = batch.source(source_sha).unwrap().clone();
        let mut link = whole_link(&source, surface, reading, verifier);
        link.candidates[1].audio = Some(real_audio(runtime, processed));
        batch.links.push(link.clone());
        created = Some(link);
    });
    created.unwrap()
}

/// Экспорт выбранных байтов — отдельное явное действие: без разрешающей
/// политики ничего не пишется, повтор идемпотентен, а чужой файл, symlink и
/// путь с `..` отвергаются.
#[test]
fn explicit_export_writes_only_authorized_bytes_and_never_replaces_files() {
    let (workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "本.mp3".into(),
            bytes: mp3(440),
        }],
        None,
    )
    .unwrap();
    let processed = mp3(660);
    add_recognized_link(
        &store,
        "sel",
        &intake.added[0],
        "本",
        "ほん",
        "ほん",
        &processed,
    );
    assert_eq!(
        auto_select(&store, "sel", None)
            .unwrap()
            .selected_deterministic,
        1
    );
    let requests = [Pronunciation::new("本", "ほん").unwrap()];
    let directory = workspace.path().join("out");
    let refused = export_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
        &directory,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    assert!(!directory.exists(), "отказ не создаёт каталог вывода");

    let exported = export_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::AgentSelected,
        &directory,
    )
    .unwrap();
    assert_eq!(exported.len(), 1);
    assert!(exported[0].written);
    assert_eq!(exported[0].consumer_filename, "本_ほん.mp3");
    let written = std::fs::read(directory.join("本_ほん.mp3")).unwrap();
    assert_eq!(
        written, processed,
        "экспортированы байты выбранного варианта"
    );
    assert_eq!(sha256_hex(&written), exported[0].sha256);
    let again = export_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::AgentSelected,
        &directory,
    )
    .unwrap();
    assert!(
        !again[0].written,
        "повторный экспорт не перезаписывает файл"
    );

    let foreign = mp3(990);
    std::fs::write(directory.join("本_ほん.mp3"), &foreign).unwrap();
    let conflict = export_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::AgentSelected,
        &directory,
    )
    .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::IdentityConflict);
    assert_eq!(
        std::fs::read(directory.join("本_ほん.mp3")).unwrap(),
        foreign,
        "чужой файл с тем же именем остаётся нетронутым"
    );
    std::fs::remove_file(directory.join("本_ほん.mp3")).unwrap();
    #[cfg(unix)]
    {
        let outside = workspace.path().join("outside.mp3");
        std::fs::write(&outside, &processed).unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("本_ほん.mp3")).unwrap();
        assert_eq!(
            export_selected(
                store.root(),
                "sel",
                &requests,
                ConsumerTrustPolicy::AgentSelected,
                &directory,
            )
            .unwrap_err()
            .code,
            ErrorCode::PathTraversal
        );
        std::fs::remove_file(directory.join("本_ほん.mp3")).unwrap();
    }
    let traversal = workspace.path().join("out").join("..");
    assert_eq!(
        export_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::AgentSelected,
            &traversal,
        )
        .unwrap_err()
        .code,
        ErrorCode::PathTraversal
    );
}

#[test]
fn agent_selected_audio_needs_explicit_consumer_policy_and_stays_out_of_verified() {
    let (_workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![
            IntakeFile {
                source_name: "本.mp3".into(),
                bytes: mp3(440),
            },
            IntakeFile {
                source_name: "橋.mp3".into(),
                bytes: mp3(550),
            },
        ],
        None,
    )
    .unwrap();
    let processed = mp3(660);
    let hon = add_recognized_link(
        &store,
        "sel",
        &intake.added[0],
        "本",
        "ほん",
        "ほん",
        &processed,
    );
    add_recognized_link(
        &store,
        "sel",
        &intake.added[1],
        "橋",
        "はし",
        "はち",
        &mp3(770),
    );
    let report = auto_select(&store, "sel", None).unwrap();
    assert_eq!(report.selected_deterministic, 1);
    assert_eq!(report.pending["needs_audio_analysis"].len(), 1);
    assert_eq!(auto_select(&store, "sel", None).unwrap().already_decided, 1);

    let requests = [Pronunciation::new("本", "ほん").unwrap()];
    let refused = read_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let refused = read_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanOrAudioModel,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let selected = read_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert_eq!(selected[0].bytes, processed);
    assert_eq!(selected[0].info.kind, CandidateKind::ProcessedWhole);
    assert_eq!(selected[0].info.proof, PhoneticProof::AsrConsensus);
    assert!(read_trusted(store.root(), &requests).is_err());
    // Технически готовый производный MP3 публикуется по явной политике:
    // со свидетельством цепочки, без Verified и без прослушивания.
    assert_eq!(
        promote(
            &store,
            "sel",
            "本",
            "ほん",
            None,
            false,
            ConsumerTrustPolicy::HumanDecision
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidValidationEvidence
    );
    let promoted = promote(
        &store,
        "sel",
        "本",
        "ほん",
        None,
        false,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert_eq!(promoted.kind, CandidateKind::ProcessedWhole);
    assert_eq!(
        promoted.asset.provenance.source_kind,
        "selection_derivative"
    );
    assert_eq!(promoted.asset.consumer_filename, "本_ほん.mp3");
    assert_eq!(promoted.asset.sha256, sha256_hex(&processed));
    assert_ne!(promoted.asset.lifecycle, LifecycleState::Verified);
    assert!(promoted.asset.human_attestation.is_none());
    let metadata = promoted.asset.domain_metadata.as_ref().unwrap();
    assert_eq!(
        metadata["selection_derivative"]["source_sha256"],
        serde_json::json!(intake.added[0])
    );
    assert_eq!(
        metadata["selection_derivative"]["selection"]["authority"],
        "deterministic_automation"
    );
    assert!(read_trusted(store.root(), &requests).is_err());

    // Новое противоречащее свидетельство отзывает агентское право чтения.
    let processed_candidate = hon.candidates[1].clone();
    let processed_sha = sha256_hex(&processed);
    mutate(&store, "sel", |batch, _| {
        let mut review = audio_review(
            &processed_candidate,
            &processed_sha,
            AudioVerdict::Contradicts,
        );
        review.surface = "本".into();
        review.reading = "ほん".into();
        batch.audio_reviews.push(review);
    });
    let refused = read_selected(
        store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
}

#[test]
fn foreign_batch_kind_is_refused() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let dir = TempWorkspace::create("selection-import").unwrap();
    let plan = crate::pronunciation::batch::ImportPlan {
        schema_version: 1,
        items: vec![crate::pronunciation::batch::ImportItem {
            surface: "本".into(),
            reading: "ほん".into(),
            file: "a.mp3".into(),
            expected_source_sha256: sha256_hex(&bytes),
            replace_expected_sha256: None,
        }],
    };
    std::fs::write(dir.path().join("a.mp3"), &bytes).unwrap();
    crate::pronunciation::batch::start(&store, "import", plan, dir.path(), |p: &Path| {
        Ok(std::fs::read(p).unwrap())
    })
    .unwrap();
    let error = intake_files(
        &store,
        "import",
        vec![IntakeFile {
            source_name: "a.mp3".into(),
            bytes,
        }],
        None,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::IdentityConflict);
}

/// Потоковый приём: файл читается ровно перед своей обработкой, поэтому
/// отказ чтения одного MP3 не отменяет остальные и не требует держать байты
/// всего набора в памяти.
#[test]
fn streamed_intake_reads_one_file_at_a_time_and_isolates_read_failures() {
    let (workspace, store) = store();
    let directory = workspace.path().join("in");
    std::fs::create_dir_all(&directory).unwrap();
    let first = mp3(440);
    let third = mp3(660);
    std::fs::write(directory.join("a.mp3"), &first).unwrap();
    std::fs::write(directory.join("bad.mp3"), &third).unwrap();
    std::fs::write(directory.join("b.mp3"), &third).unwrap();
    let paths = vec![
        directory.join("a.mp3"),
        directory.join("bad.mp3"),
        directory.join("b.mp3"),
    ];
    let blobs = store
        .root()
        .join(".runtime")
        .join("batches")
        .join("sel")
        .join("candidates");
    let mut order: Vec<String> = Vec::new();
    let mut blobs_at_read: Vec<usize> = Vec::new();
    let mut reader = |path: &Path| {
        order.push(path.file_name().unwrap().to_string_lossy().into_owned());
        blobs_at_read.push(
            std::fs::read_dir(&blobs)
                .map(|entries| entries.filter_map(Result::ok).count())
                .unwrap_or(0),
        );
        if path.file_name().unwrap() == "bad.mp3" {
            return Err(crate::AssetError::new(
                ErrorCode::InvalidTransition,
                "файл не читается",
            ));
        }
        Ok(std::fs::read(path).unwrap())
    };
    let report = intake_paths(&store, "sel", &paths, &mut reader, Some("生")).unwrap();
    assert_eq!(
        order,
        ["a.mp3", "bad.mp3", "b.mp3"],
        "порядок входа сохранён"
    );
    assert_eq!(
        blobs_at_read,
        [0, 1, 1],
        "к моменту чтения следующего файла байты предыдущего уже сохранены как blob"
    );
    assert_eq!(report.added, [sha256_hex(&first), sha256_hex(&third)]);
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.rejected[0].source_name, "bad.mp3");
    assert_eq!(report.rejected[0].code, "invalid_transition");
    assert_eq!(report.sources, 2);
    // Предел одного файла остаётся у защищённой границы: слишком большая
    // запись отвергается адресно, а не как отказ всего пакета.
    let mut oversized = |_path: &Path| {
        Err(crate::AssetError::new(
            ErrorCode::InvalidTransition,
            "файл больше предела",
        ))
    };
    let only = vec![directory.join("a.mp3")];
    let report = intake_paths(&store, "sel", &only, &mut oversized, None).unwrap();
    assert_eq!(report.added.len(), 0);
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.sources, 2);
    // Попытка выйти за защищённую границу — ошибка операции, а не отказ файла.
    let mut forbidden = |_path: &Path| {
        Err(crate::AssetError::new(
            ErrorCode::BoundaryViolation,
            "decks/** не читается",
        ))
    };
    assert_eq!(
        intake_paths(&store, "sel", &only, &mut forbidden, None)
            .unwrap_err()
            .code,
        ErrorCode::BoundaryViolation
    );
    assert_eq!(load(&store, "sel").sources.len(), 2, "пакет не изменён");
}

// ---------------------------------------------------------------------------
// Обзор
// ---------------------------------------------------------------------------

struct ReviewFixture {
    _workspace: TempWorkspace,
    store: AssetStore,
    nama: TargetLink,
}

fn review_fixture() -> ReviewFixture {
    let (workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "生.mp3".into(),
            bytes: mp3(440),
        }],
        Some("生"),
    )
    .unwrap();
    let failed = mp3(330);
    let mut nama = None;
    mutate(&store, "sel", |batch, runtime| {
        let source = batch.source(&intake.added[0]).unwrap().clone();
        let mut link = multi_reading_link(
            &source,
            "なま",
            RecognitionOutcome::MultipleOccurrences,
            &[(50.0, 250.0), (300.0, 550.0)],
        );
        // Отклонённый повторной проверкой фрагмент виден, но не выбирается.
        let mut rejected = candidate(
            &link.link_id,
            CandidateKind::ExtractedFragment,
            None,
            false,
            Some(0),
            Some((50.0, 250.0)),
        );
        rejected.audio = Some(real_audio(runtime, &failed));
        link.candidates.push(rejected);
        let sei = multi_reading_link(
            &source,
            "せい",
            RecognitionOutcome::Ambiguous,
            &[(0.0, 40.0)],
        );
        batch.links.push(link.clone());
        batch.links.push(sei);
        nama = Some(link);
    });
    ReviewFixture {
        _workspace: workspace,
        store,
        nama: nama.unwrap(),
    }
}

fn snapshot_rows(directory: &Path) -> Vec<serde_json::Value> {
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("snapshot.json")).unwrap()).unwrap();
    snapshot["rows"].as_array().unwrap().clone()
}

/// Ставит действие в первую колонку строки цели `reading`.
fn set_action(decisions: &Path, rows: &[serde_json::Value], reading: &str, action: &str) {
    let text = std::fs::read_to_string(decisions).unwrap();
    let row = rows.iter().position(|r| r["reading"] == reading).unwrap();
    let mut seen = 0;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with('#') || line.starts_with("@review") {
                return line.to_owned();
            }
            let index = seen;
            seen += 1;
            let (_, rest) = line.split_once('\t').unwrap();
            if index == row {
                format!("{action}\t{rest}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(decisions, lines.join("\n") + "\n").unwrap();
}

fn option_number(rows: &[serde_json::Value], reading: &str, candidate_id: &str) -> u64 {
    rows.iter().find(|r| r["reading"] == reading).unwrap()["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["candidate_id"] == candidate_id)
        .unwrap()["number"]
        .as_u64()
        .unwrap()
}

#[test]
fn review_snapshot_plays_local_files_and_applies_atomically() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory).to_path_buf();
    for name in [
        "snapshot.json",
        "decisions.tsv",
        "index.html",
        "items.jsonl",
    ] {
        assert!(directory.join(name).is_file(), "{name}");
    }
    let html = std::fs::read_to_string(directory.join("index.html")).unwrap();
    assert!(html.contains("<audio"));
    assert!(
        html.contains("#t="),
        "интервал играется фрагментом исходника"
    );
    assert!(!html.contains("autoplay") && !html.contains("https://"));
    for part in html.split("src=\"").skip(1) {
        let src = part.split('"').next().unwrap();
        let file = src.split('#').next().unwrap();
        assert!(
            directory.join(file).is_file(),
            "плеер ссылается на существующий файл {file}"
        );
    }
    let rows = snapshot_rows(&directory);
    let decisions = directory.join("decisions.tsv");

    // Повторный export того же состояния не стирает заполненный файл.
    set_action(&decisions, &rows, "せい", "defer");
    let edited = std::fs::read(&decisions).unwrap();
    let again = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    assert_eq!(again.review_id, export.review_id);
    assert_eq!(std::fs::read(&decisions).unwrap(), edited);

    // Выбор технически отклонённого фрагмента отвергает весь файл.
    let rejected = fixture.nama.candidates.last().unwrap().candidate_id.clone();
    let rejected_number = option_number(&rows, "なま", &rejected);
    set_action(&decisions, &rows, "なま", &rejected_number.to_string());
    let error = review::apply(store, "sel", &export.review_id).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidValidationEvidence);
    assert!(
        load(store, "sel").decisions.is_empty(),
        "частичного применения нет"
    );

    // Неверный номер варианта и лишняя строка тоже отвергаются целиком.
    set_action(&decisions, &rows, "なま", "99");
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    let interval = fixture.nama.candidates[2].candidate_id.clone();
    let number = option_number(&rows, "なま", &interval);
    set_action(&decisions, &rows, "なま", &format!("heard:{number}"));
    let good = std::fs::read(&decisions).unwrap();
    let mut extra = good.clone();
    extra.extend_from_slice(b"1\textra\n");
    std::fs::write(&decisions, &extra).unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    std::fs::write(&decisions, &good).unwrap();

    // Подмена неизменяемой колонки.
    let tampered = String::from_utf8(good.clone()).unwrap().replacen(
        "needs_audio_analysis",
        "auto_qualified",
        1,
    );
    std::fs::write(&decisions, tampered).unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    std::fs::write(&decisions, &good).unwrap();

    let applied = review::apply(store, "sel", &export.review_id).unwrap();
    assert_eq!((applied.selected, applied.deferred), (1, 1));
    let batch = load(store, "sel");
    let decision = batch.current_decision("生", "なま").unwrap();
    assert_eq!(
        decision.authority,
        DecisionAuthority::UserDirectReview {
            review_id: export.review_id.clone(),
            user_listened: true,
        }
    );
    match &decision.action {
        DecisionAction::Select {
            candidate_id, span, ..
        } => {
            assert_eq!(candidate_id, &interval);
            assert!(span.is_some(), "выбран точный интервал");
        }
        other => panic!("ожидался выбор: {other:?}"),
    }
    let repeated = review::apply(store, "sel", &export.review_id).unwrap();
    assert_eq!(repeated.already_applied, 2);
    assert!(!repeated.changed);
    // Выбранный интервал нельзя потребить без извлечения.
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    assert!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .is_err()
    );

    // Чужой review_id.
    assert!(review::apply(store, "sel", &sha(4242)).is_err());

    // Новое свидетельство делает снимок устаревшим целиком.
    let source_candidate = fixture.nama.candidates[0].clone();
    let source_sha = fixture.nama.source_sha256.clone();
    mutate(store, "sel", |batch, _| {
        let mut review = audio_review(&source_candidate, &source_sha, AudioVerdict::Inconclusive);
        review.surface = "生".into();
        review.reading = "なま".into();
        batch.audio_reviews.push(review);
    });
    let before = load(store, "sel").decisions.len();
    let error = review::apply(store, "sel", &export.review_id).unwrap_err();
    assert!(error.message.contains("устарел"), "{}", error.message);
    assert_eq!(load(store, "sel").decisions.len(), before);
}

#[test]
fn review_copy_tampering_deletion_and_symlink_are_refused() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory).to_path_buf();
    let rows = snapshot_rows(&directory);
    let decisions = directory.join("decisions.tsv");
    set_action(&decisions, &rows, "せい", "defer");
    let copy = directory.join(rows[0]["options"][0]["file"].as_str().unwrap());
    let original = std::fs::read(&copy).unwrap();

    std::fs::write(&copy, b"tampered").unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    std::fs::remove_file(&copy).unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    let outside = directory.parent().unwrap().join("outside.mp3");
    std::fs::write(&outside, &original).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, &copy).unwrap();
        assert!(review::apply(store, "sel", &export.review_id).is_err());
        std::fs::remove_file(&copy).unwrap();
    }
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&outside, &copy).is_ok() {
        assert!(review::apply(store, "sel", &export.review_id).is_err());
        std::fs::remove_file(&copy).unwrap();
    }
    std::fs::write(&copy, &original).unwrap();
    assert_eq!(
        review::apply(store, "sel", &export.review_id)
            .unwrap()
            .deferred,
        1
    );

    // Подмена snapshot.json меняет его SHA и отвергается.
    let snapshot = directory.join("snapshot.json");
    let mut bytes = std::fs::read(&snapshot).unwrap();
    bytes.push(b' ');
    std::fs::write(&snapshot, bytes).unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
}

#[test]
fn unresolved_filter_and_pagination_select_only_disputed_targets() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let unresolved = review::export(store, "sel", GroupFilter::Unresolved, 0, 10).unwrap();
    assert_eq!((unresolved.rows, unresolved.matched), (2, 2));
    let first = review::export(
        store,
        "sel",
        GroupFilter::Class(SelectionClass::NeedsAudioAnalysis),
        0,
        1,
    )
    .unwrap();
    assert_eq!((first.rows, first.matched, first.pages), (1, 1, 1));
    assert!(
        review::export(
            store,
            "sel",
            GroupFilter::Class(SelectionClass::NeedsAudioAnalysis),
            1,
            1
        )
        .is_err()
    );
    let all = review::export(store, "sel", GroupFilter::All, 0, 1).unwrap();
    assert_eq!((all.rows, all.matched, all.pages), (1, 2, 2));
    // Одинаковые строки дают тот же снимок независимо от фильтра.
    assert_eq!(all.review_id, first.review_id);
    let second = review::export(store, "sel", GroupFilter::All, 1, 1).unwrap();
    assert_ne!(second.review_id, first.review_id);
}

// ---------------------------------------------------------------------------
// Приём из Forvo-пакета, построение связи и вердикт экспертизы
// ---------------------------------------------------------------------------

#[test]
fn forvo_batch_sources_keep_plan_reading_as_hint_and_promote_with_forvo_evidence() {
    use crate::forvo::batch::{ForvoBatch, Item, StoredAcquisition, Transport};
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let sha = sha256_hex(&bytes);
    let request = Pronunciation::new("生", "なま").unwrap();
    {
        let mut runtime = SafeBatchRuntime::open(store.root(), "forvo").unwrap();
        let blob = runtime
            .persist_blob_with_limit(&bytes, "mp3", crate::pronunciation::MAX_AUDIO_BYTES)
            .unwrap();
        runtime
            .save(&ForvoBatch {
                schema_version: 2,
                provider: "forvo_web".into(),
                transport: Some(Transport::CdpChromium),
                browser: None,
                target: None,
                batch_id: "forvo".into(),
                revision: 0,
                cancelled: false,
                paused: None,
                items: vec![Item {
                    request: Some(request.clone()),
                    surface: None,
                    collected: Vec::new(),
                    collection: None,
                    replace_expected_sha256: None,
                    acquisition: Some(StoredAcquisition {
                        blob,
                        evidence: forvo_evidence("生", &sha, 4),
                    }),
                    outcome: None,
                }],
            })
            .unwrap();
    }
    assert!(intake_forvo_batch(&store, "forvo", "forvo").is_err());
    let report = intake_forvo_batch(&store, "sel", "forvo").unwrap();
    assert_eq!(report.added, vec![sha.clone()]);
    let batch = load(&store, "sel");
    let source = batch.source(&sha).unwrap();
    assert_eq!(source.surface_hint.as_deref(), Some("生"));
    assert_eq!(source.forvo_votes(), Some(4));
    assert!(matches!(
        &source.origins[0],
        SourceOrigin::ForvoBatch { plan_reading, .. } if plan_reading.as_deref() == Some("なま")
    ));
    // Чтение плана — подсказка: без связи целей нет.
    let index = engine::SelectionIndex::new(&batch);
    assert_eq!(engine::recommend_all_with_index(&index, &batch).len(), 0);
    let link = link_target(&store, "sel", &sha, "生", "なま", vec![], vec![]).unwrap();
    decide(
        &store,
        "sel",
        DecideRequest {
            surface: "生".into(),
            reading: "なま".into(),
            action: DecideAction::Select {
                candidate_id: link.candidates[0].candidate_id.clone(),
                expected_sha256: sha.clone(),
            },
            authority: user("я прослушал, это なま", true),
            reason: "ответ пользователя".into(),
            expected_fingerprint: None,
        },
    )
    .unwrap();
    let promoted = promote(
        &store,
        "sel",
        "生",
        "なま",
        None,
        true,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert_eq!(promoted.asset.provenance.source_kind, "forvo_web");
    assert_eq!(promoted.asset.lifecycle, LifecycleState::Verified);

    // Приём не закреплял ни одного чтения как подтверждённое: тот же принятый
    // исходник независимо связывается со вторым чтением и получает собственное
    // решение, а копия байтов остаётся одна.
    let second = link_target(&store, "sel", &sha, "生", "せい", vec![], vec![]).unwrap();
    decide(
        &store,
        "sel",
        DecideRequest {
            surface: "生".into(),
            reading: "せい".into(),
            action: DecideAction::Select {
                candidate_id: second.candidates[0].candidate_id.clone(),
                expected_sha256: sha.clone(),
            },
            authority: user("для せい тоже этот файл", false),
            reason: "адресный ответ по второму чтению".into(),
            expected_fingerprint: None,
        },
    )
    .unwrap();
    let batch = load(&store, "sel");
    assert_eq!(batch.sources.len(), 1, "исходник сохранён один раз");
    assert_eq!(
        batch
            .links
            .iter()
            .filter(|link| link.source_sha256 == sha)
            .count(),
        2,
        "одна запись обслуживает два чтения независимыми связями"
    );
    let seiselected = read_selected(
        store.root(),
        "sel",
        &[Pronunciation::new("生", "せい").unwrap()],
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap();
    assert_eq!(seiselected[0].info.sha256, sha);
    assert!(
        seiselected[0].info.authority.is_user(),
        "второе чтение имеет собственное решение пользователя"
    );
    assert!(
        !seiselected[0].info.authority.human_listened(),
        "про второе чтение пользователь прослушивание не заявлял"
    );
}

fn recognizer_occurrence(
    index: usize,
    evidence: Evidence,
    window: (u64, u64),
) -> crate::pronunciation::recognizer::Occurrence {
    use crate::pronunciation::recognizer::SampleSpan;
    let span = SampleSpan {
        start: window.0,
        end: window.1,
    };
    crate::pronunciation::recognizer::Occurrence {
        index,
        phonemes: "n a m a".into(),
        frames: (1, 5),
        core: span,
        start_uncertainty: SampleSpan {
            start: window.0.saturating_sub(100),
            end: window.0,
        },
        end_uncertainty: SampleSpan {
            start: window.1,
            end: window.1 + 100,
        },
        score: -0.5,
        score_per_token: -0.1,
        free_phonemes: "n a m a".into(),
        free_distance: 0,
        best_competitor: None,
        competitor_margin: Some(3.0),
        verifier_text: Some("なま".into()),
        verifier_distance: Some(0),
        snr_db: Some(25.0),
        evidence,
        reasons: Vec::new(),
    }
}

fn recognition_result(
    outcome: RecognitionOutcome,
    occurrences: Vec<crate::pronunciation::recognizer::Occurrence>,
) -> crate::pronunciation::recognizer_pipeline::RecognitionResult {
    use crate::pronunciation::normalizer::{InputRef, ProfileRef};
    use crate::pronunciation::recognizer::{
        self, Level, Matching, TargetIdentity, TargetSource, Uncertainty,
    };
    let target = TargetIdentity {
        surface: "生".into(),
        reading: "なま".into(),
        source: TargetSource {
            kind: TargetSourceKind::Manifest,
            key: "manifest:test".into(),
        },
        competing_readings: vec!["せい".into()],
    };
    let has = !occurrences.is_empty();
    crate::pronunciation::recognizer_pipeline::RecognitionResult {
        schema_version: recognizer::RESULT_SCHEMA_VERSION,
        algorithm_version: recognizer::ALGORITHM_VERSION.into(),
        config_sha256: sha(1),
        profile: ProfileRef {
            id: "test".into(),
            version: 1,
        },
        models: Vec::new(),
        input: InputRef {
            sha256: sha(2),
            byte_length: 1,
            probe: None,
            error: None,
        },
        target_key: Some(target.matching_key()),
        target: Some(target),
        pre_analysis: None,
        matching: Some(Matching {
            target_variants: vec!["n a m a".into()],
            competitors: 1,
            occurrences,
        }),
        selected_occurrence: None,
        extraction: None,
        fallback: None,
        full_record: None,
        processing: None,
        post_validation: None,
        output: None,
        outcome,
        next_action: action_for(outcome, has),
        reasons: Vec::new(),
        uncertainty: Uncertainty {
            reading_match: Level::Uncertain,
            segmentation: Level::Uncertain,
            signal: Level::Confirmed,
            pitch_accent: Level::NotAssessed,
            lexical_identity: Level::NotAssessed,
            provenance: Level::Confirmed,
            confidence: "uncalibrated".into(),
        },
    }
}

#[test]
fn recognizer_occurrences_become_interval_candidates_without_inventing_bytes() {
    let workspace = TempWorkspace::create("selection-build-link").unwrap();
    let cache =
        crate::pronunciation::recognizer_batch::ResultCache::open(workspace.path()).unwrap();
    let (_store_ws, store) = self::store();
    let runtime = SafeBatchRuntime::open(store.root(), "sel").unwrap();
    let source = fake_source(2, "生", false);
    let result = recognition_result(
        RecognitionOutcome::MultipleOccurrences,
        vec![
            recognizer_occurrence(0, Evidence::Match, (4410, 8820)),
            recognizer_occurrence(1, Evidence::Match, (13230, 17640)),
        ],
    );
    let link = ops::build_link_for_test(&runtime, &cache, "rb", &source, &result).unwrap();
    assert_eq!(link.occurrences.len(), 2);
    let kinds: Vec<_> = link.candidates.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            CandidateKind::SourceOriginal,
            CandidateKind::OccurrenceInterval,
            CandidateKind::OccurrenceInterval
        ]
    );
    assert!(link.candidates[1].audio.is_none());
    assert_eq!(link.recognition.as_ref().unwrap().occurrences, 2);
    let details = link
        .recognition
        .as_ref()
        .unwrap()
        .quality_details
        .as_ref()
        .unwrap();
    assert_eq!(details["uncertainty"]["pitch_accent"], "not_assessed");
    assert!(details["processing"].is_null());
    assert!(details["post_validation"].is_null());
    let mut with_qc = result.clone();
    with_qc.post_validation = Some(crate::pronunciation::recognizer_pipeline::PostValidation {
        passed: false,
        checks: vec![crate::pronunciation::normalizer::Check {
            id: "timing_consistent".into(),
            passed: false,
            value: Some(40.0),
            limit: Some(20.0),
        }],
        output_probe: None,
        hypothesis: None,
        verifier_hypothesis: None,
        events: 0,
        occurrence: None,
        expected_core: None,
        timing_error_ms: Some(40.0),
        start_error_ms: Some(40.0),
        end_error_ms: Some(0.0),
    });
    let with_qc = ops::build_link_for_test(&runtime, &cache, "rb", &source, &with_qc).unwrap();
    let details = with_qc.recognition.unwrap().quality_details.unwrap();
    assert_eq!(
        details["post_validation"]["checks"][0]["id"],
        "timing_consistent"
    );
    assert_eq!(details["post_validation"]["checks"][0]["value"], 40.0);
    // Границы не разрешены: интервалы не создаются.
    let unresolved = recognition_result(
        RecognitionOutcome::BoundariesUnresolved,
        vec![recognizer_occurrence(0, Evidence::Match, (4410, 8820))],
    );
    let link = ops::build_link_for_test(&runtime, &cache, "rb", &source, &unresolved).unwrap();
    assert!(
        link.candidates
            .iter()
            .all(|c| c.kind != CandidateKind::OccurrenceInterval)
    );
    // Технически отвергнутый исходник хранится, но не пригоден.
    let rejected = recognition_result(RecognitionOutcome::SourceRejected, Vec::new());
    let link = ops::build_link_for_test(&runtime, &cache, "rb", &source, &rejected).unwrap();
    assert!(!link.candidates[0].technical.usable);
}

#[test]
fn expert_verdict_follows_targeted_reanalysis() {
    let supports = recognition_result(
        RecognitionOutcome::TargetWhole,
        vec![recognizer_occurrence(0, Evidence::Match, (4410, 8820))],
    );
    assert_eq!(
        ops::verdict_of(&supports, None, 44100).0,
        AudioVerdict::Supports
    );
    // Интервал без пересечения с подтверждённым вхождением не поддержан.
    let far = span(1000.0, 1200.0);
    assert_eq!(
        ops::verdict_of(&supports, Some(&far), 44100).0,
        AudioVerdict::Inconclusive
    );
    let near = span(120.0, 180.0);
    assert_eq!(
        ops::verdict_of(&supports, Some(&near), 44100).0,
        AudioVerdict::Supports
    );
    let conflict = recognition_result(
        RecognitionOutcome::Ambiguous,
        vec![recognizer_occurrence(0, Evidence::Conflict, (4410, 8820))],
    );
    assert_eq!(
        ops::verdict_of(&conflict, None, 44100).0,
        AudioVerdict::Contradicts
    );
    let mut competitor = supports.clone();
    let occurrence = &mut competitor.matching.as_mut().unwrap().occurrences[0];
    occurrence.best_competitor = Some(CompetitorScore {
        phonemes: "s e i".into(),
        origin: "explicit:せい".into(),
        score: 0.0,
    });
    occurrence.competitor_margin = Some(-1.0);
    assert_eq!(
        ops::verdict_of(&competitor, None, 44100).0,
        AudioVerdict::Contradicts
    );
    let absent = recognition_result(RecognitionOutcome::TargetAbsent, Vec::new());
    assert_eq!(
        ops::verdict_of(&absent, None, 44100).0,
        AudioVerdict::Contradicts
    );
    let silence = recognition_result(RecognitionOutcome::NoSpeech, Vec::new());
    let (verdict, reasons) = ops::verdict_of(&silence, None, 44100);
    assert_eq!(verdict, AudioVerdict::Inconclusive);
    assert!(reasons.contains(&"expert_inconclusive".to_string()));
}

#[test]
fn expert_competitors_include_heard_forms_but_never_the_target() {
    let source = fake_source(1, "橋", true);
    let mut link = whole_link(&source, "橋", "はし", "はち");
    link.competing_readings = vec!["きょう".into()];
    let competitors = ops::expert_competitors(&link, &["ハシ".into(), "はさ".into(), "abc".into()]);
    assert_eq!(
        competitors,
        vec!["きょう".to_string(), "はさ".into(), "はち".into()]
    );
}

#[test]
fn agent_proposal_is_not_a_consumable_selection() {
    let source = fake_source(1, "生", false);
    let link = whole_link(&source, "生", "しょう", "じょう");
    let batch = batch_with(vec![source], vec![link.clone()]);
    let recommendation = engine::recommend(&batch, "生", "しょう");
    assert_eq!(recommendation.class, SelectionClass::AgentProposed);
    let candidate = &link.candidates[0];
    let action = DecideAction::Select {
        candidate_id: candidate.candidate_id.clone(),
        expected_sha256: candidate.audio.as_ref().unwrap().sha256.clone(),
    };
    let authority = DecisionAuthority::AgentEvidence {
        agent: "agent".into(),
        policy: AGENT_EVIDENCE_POLICY.into(),
    };
    assert!(ops::validate_decision(&batch, &recommendation, &action, &authority).is_err());
    assert!(
        ops::validate_decision(&batch, &recommendation, &action, &user("выберите", false)).is_ok()
    );
}

#[test]
fn revoked_agent_selection_cannot_be_read_or_promoted_but_user_can_reconfirm() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "hon.mp3".into(),
            bytes: bytes.clone(),
        }],
        None,
    )
    .unwrap();
    let sha = &intake.added[0];
    let link = link_target(&store, "sel", sha, "本", "ほん", vec![], vec![]).unwrap();
    mutate(&store, "sel", |batch, _| {
        let existing = batch
            .links
            .iter_mut()
            .find(|l| l.link_id == link.link_id)
            .unwrap();
        existing.recognition = Some(recognition(RecognitionOutcome::TargetWhole));
        existing.occurrences = vec![occurrence(
            0,
            Evidence::Match,
            Some("ほん"),
            0,
            (100.0, 600.0),
        )];
        existing.candidates[0].occurrence = Some(0);
    });
    let select = || DecideRequest {
        surface: "本".into(),
        reading: "ほん".into(),
        action: DecideAction::Select {
            candidate_id: link.candidates[0].candidate_id.clone(),
            expected_sha256: sha.clone(),
        },
        authority: DecisionAuthority::AgentEvidence {
            agent: "agent".into(),
            policy: AGENT_EVIDENCE_POLICY.into(),
        },
        reason: "выбор по свидетельствам".into(),
        expected_fingerprint: None,
    };
    let selected_decision = decide(&store, "sel", select()).unwrap();
    assert!(selected_decision.decision.recorded_at_unix_ms.is_some());
    let requests = [Pronunciation::new("本", "ほん").unwrap()];
    assert_eq!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::AgentSelected
        )
        .unwrap()[0]
            .bytes,
        bytes
    );
    let promoted = promote(
        &store,
        "sel",
        "本",
        "ほん",
        None,
        false,
        ConsumerTrustPolicy::AgentSelected,
    )
    .unwrap();
    assert!(!promoted.human_attestation_created);
    mutate(&store, "sel", |batch, _| {
        let mut review = audio_review(&link.candidates[0], sha, AudioVerdict::Contradicts);
        review.surface = "本".into();
        review.reading = "ほん".into();
        batch.audio_reviews.push(review);
    });
    assert!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::AgentSelected
        )
        .is_err()
    );
    assert!(
        promote(
            &store,
            "sel",
            "本",
            "ほん",
            Some(sha.clone()),
            false,
            ConsumerTrustPolicy::AgentSelected
        )
        .is_err()
    );
    let pending = ops::status_value(&store, "sel", GroupFilter::Unresolved, 10).unwrap();
    assert_eq!(pending["matched"], 1);
    assert_eq!(pending["items"][0]["selection_effective"], false);
    let mut explicit = select();
    explicit.authority = user("выберите эту запись", false);
    decide(&store, "sel", explicit).unwrap();
    mutate(&store, "sel", |batch, _| {
        let mut review = audio_review(&link.candidates[0], sha, AudioVerdict::Contradicts);
        review.review_id = "a-2".into();
        review.surface = "本".into();
        review.reading = "ほん".into();
        batch.audio_reviews.push(review);
    });
    assert_eq!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .unwrap()[0]
            .bytes,
        bytes
    );
    assert!(
        promote(
            &store,
            "sel",
            "本",
            "ほん",
            Some(sha.clone()),
            true,
            ConsumerTrustPolicy::AgentSelected
        )
        .is_err()
    );
}

#[test]
fn expert_verdict_only_uses_overlapping_occurrences() {
    let result = recognition_result(
        RecognitionOutcome::MultipleOccurrences,
        vec![
            recognizer_occurrence(0, Evidence::Match, (4410, 8820)),
            recognizer_occurrence(1, Evidence::Conflict, (13230, 17640)),
        ],
    );
    assert_eq!(
        ops::verdict_of(&result, Some(&span(100.0, 200.0)), 44100).0,
        AudioVerdict::Supports
    );
    assert_eq!(
        ops::verdict_of(&result, Some(&span(300.0, 400.0)), 44100).0,
        AudioVerdict::Contradicts
    );
    assert_eq!(
        ops::verdict_of(&result, Some(&span(200.0, 300.0)), 44100).0,
        AudioVerdict::Inconclusive
    );
    assert_eq!(
        ops::verdict_of(&result, None, 44100).0,
        AudioVerdict::Contradicts
    );
    assert_eq!(
        ops::verdict_of(&result, Some(&span(150.0, 150.0)), 44100).0,
        AudioVerdict::Inconclusive
    );
    let mut unresolved = result.clone();
    unresolved.outcome = RecognitionOutcome::BoundariesUnresolved;
    assert_eq!(
        ops::verdict_of(&unresolved, Some(&span(300.0, 400.0)), 44100).0,
        AudioVerdict::Inconclusive
    );
}

#[test]
fn invalid_zero_rate_intervals_are_rejected_in_state() {
    let source = fake_source(1, "生", false);
    let link = multi_reading_link(
        &source,
        "なま",
        RecognitionOutcome::MultipleOccurrences,
        &[(50.0, 250.0)],
    );
    let mut batch = batch_with(vec![source], vec![link]);
    assert!(batch.validate().is_ok());
    batch.links[0].occurrences[0].span.sample_rate = 0;
    assert!(batch.validate().is_err());
    batch.links[0].occurrences[0].span.sample_rate = 44100;
    batch.links[0]
        .candidates
        .last_mut()
        .unwrap()
        .span
        .as_mut()
        .unwrap()
        .sample_rate = 0;
    assert!(batch.validate().is_err());
}

#[test]
fn reject_excludes_only_the_candidate_and_defer_holds_the_group() {
    let source = fake_source(1, "本", false);
    let link = whole_link(&source, "本", "ほん", "ほん");
    let other_source = fake_source(2, "橋", false);
    let other = whole_link(&other_source, "橋", "はし", "はし");
    let mut batch = batch_with(vec![source, other_source], vec![link.clone(), other]);
    let first = engine::recommend(&batch, "本", "ほん").recommended.unwrap();
    let candidate = batch.find_candidate(&first).unwrap().1;
    let action = DecisionAction::RejectCandidate {
        candidate_id: first.clone(),
        addressed_sha256: candidate.audio.as_ref().unwrap().sha256.clone(),
    };
    batch.decisions.push(SelectionDecision {
        decision_id: "d-1".into(),
        surface: "本".into(),
        reading: "ほん".into(),
        action,
        authority: user("отклонить первый", false),
        reason: "отклонение".into(),
        group_fingerprint: sha(7),
        state_revision: 0,
        recorded_at_unix_ms: None,
    });
    let recommendation = engine::recommend(&batch, "本", "ほん");
    assert_ne!(recommendation.recommended.as_deref(), Some(first.as_str()));
    let (planned, _) = ops::plan_auto_select(&batch, None);
    assert!(planned.iter().any(|d| d.surface == "本"));
    assert!(planned.iter().any(|d| d.surface == "橋"));
    batch.decisions.push(SelectionDecision {
        decision_id: "d-2".into(),
        surface: "本".into(),
        reading: "ほん".into(),
        action: DecisionAction::Defer,
        authority: user("отложить", false),
        reason: "ожидание".into(),
        group_fingerprint: sha(8),
        state_revision: 0,
        recorded_at_unix_ms: None,
    });
    let (planned, _) = ops::plan_auto_select(&batch, None);
    assert!(!planned.iter().any(|d| d.surface == "本"));
    assert!(planned.iter().any(|d| d.surface == "橋"));
}

#[test]
fn relinking_invalidates_old_recognition_and_old_authority() {
    let (_workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "hon.mp3".into(),
            bytes: mp3(440),
        }],
        None,
    )
    .unwrap();
    let sha = &intake.added[0];
    let link = link_target(&store, "sel", sha, "本", "ほん", vec![], vec![]).unwrap();
    mutate(&store, "sel", |batch, _| {
        let existing = &mut batch.links[0];
        existing.recognition = Some(recognition(RecognitionOutcome::TargetWhole));
        existing.occurrences = vec![occurrence(
            0,
            Evidence::Match,
            Some("ほん"),
            0,
            (100.0, 600.0),
        )];
        existing.candidates[0].occurrence = Some(0);
    });
    decide(
        &store,
        "sel",
        DecideRequest {
            surface: "本".into(),
            reading: "ほん".into(),
            action: DecideAction::Select {
                candidate_id: link.candidates[0].candidate_id.clone(),
                expected_sha256: sha.clone(),
            },
            authority: user("подтверждаю", false),
            reason: "выбор".into(),
            expected_fingerprint: None,
        },
    )
    .unwrap();
    let changed = link_target(
        &store,
        "sel",
        sha,
        "本",
        "ほん",
        vec!["ぽん".into()],
        vec![0],
    )
    .unwrap();
    assert!(changed.recognition.is_none());
    assert!(changed.occurrences.is_empty());
    let requests = [Pronunciation::new("本", "ほん").unwrap()];
    assert!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .is_err()
    );
}

#[test]
fn skipped_review_row_must_also_be_current() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory);
    let rows = snapshot_rows(directory);
    set_action(&directory.join("decisions.tsv"), &rows, "せい", "defer");
    mutate(store, "sel", |batch, _| {
        let link = batch
            .links
            .iter_mut()
            .find(|l| l.reading == "なま")
            .unwrap();
        link.particle_morae.push(0);
    });
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    assert!(load(store, "sel").decisions.is_empty());
}

#[test]
fn review_rejects_revision_change_even_if_target_evidence_is_unchanged() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory);
    let rows = snapshot_rows(directory);
    set_action(&directory.join("decisions.tsv"), &rows, "せい", "defer");
    mutate(store, "sel", |_, _| {});
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    assert!(load(store, "sel").decisions.is_empty());
}

#[test]
fn review_escapes_untrusted_source_metadata() {
    let (_workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "<img onerror=alert(1)>.mp3".into(),
            bytes: mp3(440),
        }],
        None,
    )
    .unwrap();
    link_target(
        &store,
        "sel",
        &intake.added[0],
        "本",
        "ほん",
        vec![],
        vec![],
    )
    .unwrap();
    let report = review::export(&store, "sel", GroupFilter::All, 0, 10).unwrap();
    let html = std::fs::read_to_string(&report.index).unwrap();
    assert!(html.contains("&lt;img onerror=alert(1)&gt;"));
    assert!(!html.contains("<img onerror=alert(1)>"));
    assert_eq!(html.matches("<script>").count(), 1);
}

#[test]
fn review_rejects_missing_blob_even_for_skipped_row() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory);
    let rows = snapshot_rows(directory);
    set_action(&directory.join("decisions.tsv"), &rows, "せい", "defer");
    let removed = {
        let runtime = SafeBatchRuntime::open(store.root(), "sel").unwrap();
        let blob = fixture
            .nama
            .candidates
            .last()
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .blob
            .clone();
        let bytes = runtime.read_blob(&blob).unwrap();
        runtime.remove_blob(&blob).unwrap();
        bytes
    };
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    SafeBatchRuntime::open(store.root(), "sel")
        .unwrap()
        .persist_blob_with_limit(&removed, "mp3", crate::pronunciation::MAX_AUDIO_BYTES)
        .unwrap();
    assert!(load(store, "sel").decisions.is_empty());
}

#[test]
fn identical_forvo_bytes_keep_surface_specific_provenance() {
    use crate::forvo::batch::{ForvoBatch, Item, StoredAcquisition, Transport};
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let sha = sha256_hex(&bytes);
    {
        let mut runtime = SafeBatchRuntime::open(store.root(), "forvo").unwrap();
        let blob = runtime
            .persist_blob_with_limit(&bytes, "mp3", crate::pronunciation::MAX_AUDIO_BYTES)
            .unwrap();
        let items = [("生", "なま"), ("本", "ほん")]
            .into_iter()
            .map(|(surface, reading)| Item {
                request: Some(Pronunciation::new(surface, reading).unwrap()),
                surface: None,
                collected: Vec::new(),
                collection: None,
                replace_expected_sha256: None,
                acquisition: Some(StoredAcquisition {
                    blob: blob.clone(),
                    evidence: forvo_evidence(surface, &sha, 4),
                }),
                outcome: None,
            })
            .collect();
        runtime
            .save(&ForvoBatch {
                schema_version: 2,
                provider: "forvo_web".into(),
                transport: Some(Transport::CdpChromium),
                browser: None,
                target: None,
                batch_id: "forvo".into(),
                revision: 0,
                cancelled: false,
                paused: None,
                items,
            })
            .unwrap();
    }
    let report = intake_forvo_batch(&store, "sel", "forvo").unwrap();
    assert_eq!(report.added.len(), 1);
    assert_eq!(report.already_present.len(), 1);
    for (surface, reading) in [("生", "なま"), ("本", "ほん")] {
        let link = link_target(&store, "sel", &sha, surface, reading, vec![], vec![]).unwrap();
        let status = ops::status_value(&store, "sel", GroupFilter::All, 10).unwrap();
        let item = status["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["surface"] == surface)
            .unwrap();
        assert_eq!(item["sources"][0]["sha256"], sha);
        assert_eq!(item["sources"][0]["origins"].as_array().unwrap().len(), 2);
        assert!(item["sources"][0].get("blob").is_none());
        assert!(!item.to_string().contains("storage_path"));
        decide(
            &store,
            "sel",
            DecideRequest {
                surface: surface.into(),
                reading: reading.into(),
                action: DecideAction::Select {
                    candidate_id: link.candidates[0].candidate_id.clone(),
                    expected_sha256: sha.clone(),
                },
                authority: user("подтверждаю", false),
                reason: "выбор".into(),
                expected_fingerprint: None,
            },
        )
        .unwrap();
        let promoted = promote(
            &store,
            "sel",
            surface,
            reading,
            None,
            false,
            ConsumerTrustPolicy::AgentSelected,
        )
        .unwrap();
        assert_eq!(promoted.asset.provenance.source_kind, "forvo_web");
    }
}

#[test]
fn audio_authority_requires_matching_latest_backend_and_target() {
    let source = fake_source(1, "橋", true);
    let link = whole_link(&source, "橋", "はし", "はち");
    let candidate = &link.candidates[1];
    let sha = candidate.audio.as_ref().unwrap().sha256.clone();
    let mut batch = batch_with(vec![source], vec![link.clone()]);
    batch
        .audio_reviews
        .push(audio_review(candidate, &sha, AudioVerdict::Supports));
    let action = DecideAction::Select {
        candidate_id: candidate.candidate_id.clone(),
        expected_sha256: sha.clone(),
    };
    let authority = |backend: &str, id: &str| DecisionAuthority::AgentAudioModel {
        agent: "agent".into(),
        backend: backend.into(),
        audio_review_id: id.into(),
    };
    let recommendation = engine::recommend(&batch, "橋", "はし");
    assert!(
        ops::validate_decision(
            &batch,
            &recommendation,
            &action,
            &authority(ops::LOCAL_AUDIO_BACKEND, "a-1")
        )
        .is_ok()
    );
    assert!(
        ops::validate_decision(
            &batch,
            &recommendation,
            &action,
            &authority("other-backend", "a-1")
        )
        .is_err()
    );
    for corrupt in 0..3 {
        let mut bad = batch.clone();
        match corrupt {
            0 => bad.audio_reviews[0].audio_sha256 = sha256_hex("другие байты"),
            1 => bad.audio_reviews[0].reading = "はち".into(),
            _ => bad.audio_reviews[0].candidate_id = link.candidates[0].candidate_id.clone(),
        }
        let recommendation = engine::recommend(&bad, "橋", "はし");
        assert!(
            ops::validate_decision(
                &bad,
                &recommendation,
                &action,
                &authority(ops::LOCAL_AUDIO_BACKEND, "a-1")
            )
            .is_err()
        );
    }
    let mut newer = audio_review(candidate, &sha, AudioVerdict::Contradicts);
    newer.review_id = "a-2".into();
    batch.audio_reviews.push(newer);
    let recommendation = engine::recommend(&batch, "橋", "はし");
    assert!(
        ops::validate_decision(
            &batch,
            &recommendation,
            &action,
            &authority(ops::LOCAL_AUDIO_BACKEND, "a-1")
        )
        .is_err()
    );
}

#[test]
fn invalid_audio_review_does_not_authorize_selection_or_block_other_targets() {
    let source = fake_source(1, "橋", true);
    let link = whole_link(&source, "橋", "はし", "はち");
    let candidate = link.candidates[1].clone();
    let sha = candidate.audio.as_ref().unwrap().sha256.clone();
    let other = fake_source(2, "本", false);
    let other_link = whole_link(&other, "本", "ほん", "ほん");
    let mut batch = batch_with(vec![source, other], vec![link, other_link]);
    let mut review = audio_review(&candidate, &sha, AudioVerdict::Supports);
    review.backend = "other-backend".into();
    batch.audio_reviews.push(review);
    let recommendation = engine::recommend(&batch, "橋", "はし");
    assert!(recommendation.candidates.iter().any(|candidate| {
        candidate
            .reasons
            .contains(&ReasonCode::AudioReviewInvalidProvenance)
    }));
    let index = engine::SelectionIndex::new(&batch);
    let view = serde_json::to_value(ops::group_view(&index, &batch, &recommendation)).unwrap();
    assert_eq!(view["audio_reviews"][0]["backend"], "other-backend");
    let (decisions, report) = ops::plan_auto_select(
        &batch,
        Some(&AgentPolicy {
            agent: "agent".into(),
        }),
    );
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].surface, "本");
    assert_eq!(report.pending.values().map(Vec::len).sum::<usize>(), 1);
}

#[test]
fn audio_authority_refuses_missing_candidate_bytes_before_deciding() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "hon.mp3".into(),
            bytes,
        }],
        None,
    )
    .unwrap();
    let sha = &intake.added[0];
    let link = link_target(&store, "sel", sha, "本", "ほん", vec![], vec![]).unwrap();
    mutate(&store, "sel", |batch, _| {
        let mut review = audio_review(&link.candidates[0], sha, AudioVerdict::Supports);
        review.surface = "本".into();
        review.reading = "ほん".into();
        batch.audio_reviews.push(review);
    });
    let removed = {
        let runtime = SafeBatchRuntime::open(store.root(), "sel").unwrap();
        let blob = link.candidates[0].audio.as_ref().unwrap().blob.clone();
        let bytes = runtime.read_blob(&blob).unwrap();
        runtime.remove_blob(&blob).unwrap();
        bytes
    };
    let result = decide(
        &store,
        "sel",
        DecideRequest {
            surface: "本".into(),
            reading: "ほん".into(),
            action: DecideAction::Select {
                candidate_id: link.candidates[0].candidate_id.clone(),
                expected_sha256: sha.clone(),
            },
            authority: DecisionAuthority::AgentAudioModel {
                agent: "agent".into(),
                audio_review_id: "a-1".into(),
                backend: ops::LOCAL_AUDIO_BACKEND.into(),
            },
            reason: "экспертиза".into(),
            expected_fingerprint: None,
        },
    );
    assert!(result.is_err());
    SafeBatchRuntime::open(store.root(), "sel")
        .unwrap()
        .persist_blob_with_limit(&removed, "mp3", crate::pronunciation::MAX_AUDIO_BYTES)
        .unwrap();
    assert!(load(&store, "sel").decisions.is_empty());
}

#[test]
fn review_reject_replay_is_idempotent_and_cannot_add_actions() {
    let fixture = review_fixture();
    let store = &fixture.store;
    let export = review::export(store, "sel", GroupFilter::All, 0, 10).unwrap();
    let directory = Path::new(&export.directory);
    let rows = snapshot_rows(directory);
    let decisions = directory.join("decisions.tsv");
    let candidate = &fixture.nama.candidates[0].candidate_id;
    let number = option_number(&rows, "なま", candidate);
    set_action(&decisions, &rows, "なま", &format!("r{number}"));
    assert_eq!(
        review::apply(store, "sel", &export.review_id)
            .unwrap()
            .rejected,
        1
    );
    let again = review::apply(store, "sel", &export.review_id).unwrap();
    assert_eq!(again.already_applied, 1);
    assert!(!again.changed);
    set_action(&decisions, &rows, "せい", "defer");
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    assert_eq!(load(store, "sel").decisions.len(), 1);
    assert!(review::export(store, "sel", GroupFilter::All, usize::MAX, 2).is_err());
}

#[test]
fn system_recording_time_is_explicit_and_legacy_decisions_deserialize() {
    assert_eq!(
        ops::recorded_at(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1234)).unwrap(),
        1234
    );
    let source = fake_source(1, "本", false);
    let link = whole_link(&source, "本", "ほん", "ほん");
    let batch = batch_with(vec![source], vec![link]);
    let (decisions, _) = ops::plan_auto_select(&batch, None);
    let json = serde_json::to_value(&decisions[0]).unwrap();
    assert!(json.get("recorded_at_unix_ms").is_none());
    assert_eq!(
        serde_json::from_value::<SelectionDecision>(json)
            .unwrap()
            .recorded_at_unix_ms,
        None
    );
}

/// Идентичность кандидата — связь, вид, точные байты и интервал, а не
/// вхождение: приход ASR-свидетельства без изменения контекста не отменяет
/// точное решение пользователя.
#[test]
fn user_selection_survives_asr_arrival_without_context_change() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "nama.mp3".into(),
            bytes: bytes.clone(),
        }],
        None,
    )
    .unwrap();
    let sha = intake.added[0].clone();
    let link = link_target(&store, "sel", &sha, "生", "なま", vec![], vec![]).unwrap();
    decide(
        &store,
        "sel",
        DecideRequest {
            surface: "生".into(),
            reading: "なま".into(),
            action: DecideAction::Select {
                candidate_id: link.candidates[0].candidate_id.clone(),
                expected_sha256: sha.clone(),
            },
            authority: user("я прослушал, это なま", true),
            reason: "ответ пользователя".into(),
            expected_fingerprint: None,
        },
    )
    .unwrap();
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    assert_eq!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .unwrap()[0]
            .bytes,
        bytes
    );

    // Тот же исходник получает результат распознавания: связь заменяется новой,
    // у исходного кандидата появляется выбранное вхождение.
    let rebuilt = {
        let workspace = TempWorkspace::create("selection-asr-arrival").unwrap();
        let cache =
            crate::pronunciation::recognizer_batch::ResultCache::open(workspace.path()).unwrap();
        let mut runtime = SafeBatchRuntime::open(store.root(), "sel").unwrap();
        let loaded: SelectionBatch = runtime.load().unwrap().unwrap();
        let source = loaded.source(&sha).expect("исходник принят").clone();
        let result = recognition_result(
            RecognitionOutcome::TargetWhole,
            vec![recognizer_occurrence(0, Evidence::Match, (4410, 8820))],
        );
        let mut rebuilt =
            ops::build_link_for_test(&runtime, &cache, "rb", &source, &result).unwrap();
        rebuilt.generation = link.generation;
        rebuilt
    };
    assert_eq!(
        rebuilt.candidates[0].candidate_id, link.candidates[0].candidate_id,
        "точные байты сохраняют идентичность"
    );
    mutate(&store, "sel", |batch, _| {
        let existing = batch
            .links
            .iter_mut()
            .find(|l| l.link_id == link.link_id)
            .expect("связь на месте");
        *existing = rebuilt;
    });

    assert_eq!(
        read_selected(
            store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .expect("решение пользователя действует после прихода ASR")[0]
            .bytes,
        bytes
    );
}

/// Аудиоэкспертиза — журнал свидетельств: когда новый результат распознавания
/// убирает кандидата, запись журнала остаётся, а состояние пакета сохраняемо.
#[test]
fn audio_review_of_replaced_candidate_keeps_state_savable() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "nama.mp3".into(),
            bytes: bytes.clone(),
        }],
        None,
    )
    .unwrap();
    let sha = intake.added[0].clone();
    let link = link_target(&store, "sel", &sha, "生", "なま", vec![], vec![]).unwrap();
    mutate(&store, "sel", |batch, _| {
        let mut review = audio_review(&link.candidates[0], &sha, AudioVerdict::Supports);
        review.surface = "生".into();
        review.reading = "なま".into();
        batch.audio_reviews.push(review);
    });
    mutate(&store, "sel", |batch, _| {
        let mut replacement = link.candidates[0].clone();
        replacement.candidate_id = "c-other-bytes".into();
        let existing = batch
            .links
            .iter_mut()
            .find(|l| l.link_id == link.link_id)
            .expect("связь на месте");
        existing.candidates = vec![replacement];
    });
    let batch = load(&store, "sel");
    assert_eq!(
        batch.audio_reviews.len(),
        1,
        "журнал свидетельств сохраняется при смене кандидатов"
    );
    assert_eq!(
        batch.audio_reviews[0].candidate_id,
        link.candidates[0].candidate_id
    );
}

/// Повторный приём тех же байтов и повторная та же связь не меняют состояние:
/// иначе новый номер ревизии обесценивал бы уже выгруженный обзор.
#[test]
fn repeated_intake_and_link_do_not_advance_revision() {
    let (_workspace, store) = store();
    let bytes = mp3(440);
    let intake = |store: &AssetStore| {
        intake_files(
            store,
            "sel",
            vec![IntakeFile {
                source_name: "nama.mp3".into(),
                bytes: bytes.clone(),
            }],
            None,
        )
        .unwrap()
    };
    let first = intake(&store);
    let sha = first.added[0].clone();
    let revision = first.revision;
    let again = intake(&store);
    assert!(
        again.added.is_empty(),
        "повторный приём не добавляет исходник"
    );
    assert_eq!(again.already_present.len(), 1);
    assert_eq!(
        again.revision, revision,
        "повторный приём не меняет ревизию"
    );

    let link = link_target(&store, "sel", &sha, "生", "なま", vec![], vec![]).unwrap();
    let linked = load(&store, "sel").revision;
    assert_eq!(linked, revision + 1, "новая связь меняет ревизию");
    link_target(&store, "sel", &sha, "生", "なま", vec![], vec![]).unwrap();
    assert_eq!(
        load(&store, "sel").revision,
        linked,
        "повторная связь с теми же параметрами не меняет ревизию"
    );

    let changed = link_target(
        &store,
        "sel",
        &sha,
        "生",
        "なま",
        vec!["せい".into()],
        vec![],
    )
    .unwrap();
    assert_eq!(changed.generation, link.generation + 1);
    assert_eq!(
        load(&store, "sel").revision,
        linked + 1,
        "изменение контекста связи меняет ревизию"
    );
}

/// Пустой список вхождений при `MultipleOccurrences` не доказывает консенсус:
/// иначе Forvo-запись стала бы `agent_selectable` без фонетического свидетельства
/// и автоматика записала бы выбор без экспертизы.
#[test]
fn empty_occurrences_do_not_produce_agent_selectable() {
    let source = fake_source(3, "生", true);
    let id = ops::link_id(&source.sha256, "生", "なま");
    let mut link = whole_link(&source, "生", "なま", "なま");
    link.recognition = Some(recognition(RecognitionOutcome::MultipleOccurrences));
    link.occurrences.clear();
    for candidate in &mut link.candidates {
        candidate.occurrence = None;
    }
    assert_eq!(link.link_id, id);
    let batch = batch_with(vec![source], vec![link]);
    let recommendation = engine::recommend(&batch, "生", "なま");
    assert_ne!(
        recommendation.class,
        SelectionClass::AgentSelectable,
        "без вхождений нет консенсуса: {:?}",
        recommendation.reasons
    );
    let (decisions, report) = ops::plan_auto_select(
        &batch,
        Some(&ops::AgentPolicy {
            agent: "agent".into(),
        }),
    );
    assert!(
        decisions.is_empty(),
        "автоматика не выбирает без свидетельств"
    );
    assert_eq!(report.selected_agent_evidence, 0);
    assert_eq!(report.selected_agent_audio, 0);
}

/// При `--intake-missing` ошибка границы прерывает операцию, а отказ чтения
/// одного файла остаётся его собственным исходом.
#[test]
fn missing_source_intake_aborts_on_boundary_and_records_single_file_refusal() {
    let (_workspace, store) = store();
    let mut runtime = SafeBatchRuntime::open(store.root(), "sel").unwrap();
    let mut batch: SelectionBatch = runtime
        .load()
        .unwrap()
        .unwrap_or_else(|| SelectionBatch::new("sel"));
    let bytes = mp3(440);
    let sha = sha256_hex(&bytes);
    let mut report = ops::LinkReport::default();

    let mut boundary = |_path: &Path| {
        Err(crate::AssetError::new(
            ErrorCode::BoundaryViolation,
            "источник в decks/** запрещён",
        ))
    };
    let error = ops::accept_missing_source(
        &runtime,
        &mut batch,
        "/input",
        "nama.mp3",
        &sha,
        &mut boundary,
        &mut report,
    )
    .expect_err("граница обязана прервать операцию");
    assert_eq!(error.code, ErrorCode::BoundaryViolation);
    assert!(
        report.sources_rejected.is_empty(),
        "граница не превращается в отказ файла"
    );

    let mut unreadable =
        |_path: &Path| Err(crate::AssetError::new(ErrorCode::IoFailure, "нет доступа"));
    let added = ops::accept_missing_source(
        &runtime,
        &mut batch,
        "/input",
        "nama.mp3",
        &sha,
        &mut unreadable,
        &mut report,
    )
    .expect("отказ файла не прерывает операцию");
    assert!(!added);
    assert_eq!(report.sources_rejected.len(), 1);
    assert_eq!(report.sources_rejected[0].code, "io_failure");

    let mut readable = move |_path: &Path| Ok(bytes.clone());
    let added = ops::accept_missing_source(
        &runtime,
        &mut batch,
        "/input",
        "nama.mp3",
        &sha,
        &mut readable,
        &mut report,
    )
    .unwrap();
    assert!(added, "исходник принят");
    assert!(report.sources_intaken >= 1);
    assert!(batch.source(&sha).is_some());
}

mod canonical;
