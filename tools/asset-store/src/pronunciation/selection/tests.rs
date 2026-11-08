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
            code: "imported".into(),
        }],
    }
}

fn fake_source(n: u32, surface: &str, forvo: bool) -> AudioSource {
    let sha = sha(n);
    let origin = if forvo {
        SourceOrigin::ForvoBatch {
            batch_id: "forvo".into(),
            evidence: forvo_evidence(surface, &sha, 4),
            plan_reading: "なま".into(),
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
    let all = engine::recommend_all(&batch);
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
    let batch = batch_with(sources, links);
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
    assert!(!selected[0].authority.human_listened());
    // Строгий потребитель человеческого Verified ничего не получает.
    assert!(read_trusted(store.root(), &requests).is_err());

    // HumanAttestation без заявленного прослушивания не создаётся.
    let refused = promote(&store, "sel", "生", "なま", None, true).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let promoted = promote(&store, "sel", "生", "なま", None, false).unwrap();
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
    let attested = promote(&store, "sel", "生", "なま", None, true).unwrap();
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
    assert_eq!(selected[0].kind, CandidateKind::ProcessedWhole);
    assert_eq!(selected[0].proof, PhoneticProof::AsrConsensus);
    assert!(read_trusted(store.root(), &requests).is_err());
    // Производный фрагмент в канонический домен не переносится.
    assert_eq!(
        promote(&store, "sel", "本", "ほん", None, false)
            .unwrap_err()
            .code,
        ErrorCode::InvalidTransition
    );

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
    std::os::unix::fs::symlink(&outside, &copy).unwrap();
    assert!(review::apply(store, "sel", &export.review_id).is_err());
    std::fs::remove_file(&copy).unwrap();
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
