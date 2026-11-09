//! Операции пакета выбора поверх `SafeBatchRuntime` хранилища произношений.
//!
//! Каждая операция держит исключительную блокировку пакета, проверяет всё до
//! единственной атомарной записи состояния и не трогает `decks/**`.
//! Канонический `pronunciation_audio` меняется только явным [`promote`] через
//! штатные импорт, CAS и техническую проверку домена.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use super::engine::{self, SelectionIndex};
use super::model::*;
use crate::batch_runtime::{RuntimeBatchState, SafeBatchRuntime};
use crate::domain::validate_safe_consumer_filename;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::DetectedFormat;
use crate::pronunciation::recognizer::{
    Evidence, RecognitionAction, RecognitionOutcome, TargetIdentity, TargetSource, TargetSourceKind,
};
use crate::pronunciation::recognizer_batch::{self, ResultCache, WorkItem};
use crate::pronunciation::recognizer_pipeline::{ProcessingMode, RecognitionResult};
use crate::pronunciation::{MAX_AUDIO_BYTES, Pronunciation, PronunciationDomainPolicy, probe_mp3};
use crate::store::{AssetStore, HumanAttestationRequest};
use crate::{AssetDomainPolicy, AssetRecord, HumanDecision};

fn open(store_root: &Path, batch_id: &str) -> Result<SafeBatchRuntime, AssetError> {
    SafeBatchRuntime::open(store_root, batch_id)
}

fn load_existing(runtime: &mut SafeBatchRuntime) -> Result<SelectionBatch, AssetError> {
    runtime
        .load::<SelectionBatch>()?
        .ok_or_else(|| invalid("пакет выбора не найден; начните с selection intake"))
}

fn save(runtime: &mut SafeBatchRuntime, batch: &mut SelectionBatch) -> Result<(), AssetError> {
    batch.revision += 1;
    batch.validate()?;
    runtime.save(batch)
}

fn short_hash(value: &impl Serialize) -> String {
    sha256_hex(serde_json::to_vec(value).expect("значение сериализуемо"))[..16].to_owned()
}

pub(crate) fn recorded_at(time: std::time::SystemTime) -> Result<u64, AssetError> {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("системные часы раньше UNIX epoch"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| invalid("системное время вне допустимого диапазона"))
}

// ---------------------------------------------------------------------------
// Приём исходников
// ---------------------------------------------------------------------------

/// Файл для приёма: имя без пути и точные байты.
pub struct IntakeFile {
    pub source_name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
pub struct IntakeRejection {
    pub source_name: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub struct IntakeReport {
    pub added: Vec<String>,
    pub already_present: Vec<String>,
    pub rejected: Vec<IntakeRejection>,
    /// Написание-подсказка не заменено: у тех же байтов уже есть другое.
    pub surface_hint_conflicts: Vec<String>,
    pub sources: usize,
    pub revision: u64,
}

fn add_source(
    runtime: &SafeBatchRuntime,
    batch: &mut SelectionBatch,
    bytes: &[u8],
    origin: SourceOrigin,
    surface_hint: Option<&str>,
    report: &mut IntakeReport,
) -> Result<(), AssetError> {
    let sha = sha256_hex(bytes);
    if let Some(existing) = batch.sources.iter_mut().find(|s| s.sha256 == sha) {
        if !existing.origins.contains(&origin) {
            existing.origins.push(origin);
        }
        match (&existing.surface_hint, surface_hint) {
            (None, Some(hint)) => existing.surface_hint = Some(hint.into()),
            (Some(current), Some(hint)) if current != hint => {
                report.surface_hint_conflicts.push(sha.clone());
            }
            _ => {}
        }
        report.already_present.push(sha);
        return Ok(());
    }
    if batch.sources.len() >= MAX_SOURCES {
        return Err(invalid(format!(
            "пакет выбора ограничен {MAX_SOURCES} исходниками"
        )));
    }
    let probe = probe_mp3(bytes)?;
    let blob = runtime.persist_blob_with_limit(bytes, "mp3", MAX_AUDIO_BYTES)?;
    batch.sources.push(AudioSource {
        sha256: sha.clone(),
        byte_length: bytes.len() as u64,
        blob,
        probe,
        origins: vec![origin],
        surface_hint: surface_hint.map(str::to_owned),
        free_analysis: None,
    });
    report.added.push(sha);
    Ok(())
}

/// Один вход приёма: прочитанный файл или отказ чтения, который не мешает
/// остальным файлам пакета.
enum IntakeInput {
    File(IntakeFile),
    Unreadable {
        source_name: String,
        error: AssetError,
    },
}

/// Общий приём: вход перебирается лениво, поэтому одновременно в памяти
/// находится не больше одного MP3, а состояние сохраняется одной записью.
fn intake_inputs(
    store: &AssetStore,
    batch_id: &str,
    inputs: impl IntoIterator<Item = IntakeInput>,
    hint: Option<&str>,
) -> Result<IntakeReport, AssetError> {
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = runtime
        .load::<SelectionBatch>()?
        .unwrap_or_else(|| SelectionBatch::new(batch_id));
    let mut report = IntakeReport::default();
    for input in inputs {
        let (source_name, bytes) = match input {
            IntakeInput::File(file) => (file.source_name, file.bytes),
            IntakeInput::Unreadable { source_name, error } => {
                // Попытка выйти за защищённую границу — не отказ одного файла,
                // а ошибка операции: пакет не принимается вовсе.
                if matches!(
                    error.code,
                    ErrorCode::BoundaryViolation | ErrorCode::PathTraversal
                ) {
                    return Err(error);
                }
                report.rejected.push(IntakeRejection {
                    source_name,
                    code: error.code.as_str().into(),
                    message: error.message,
                });
                continue;
            }
        };
        if !valid_text(&source_name, 255) || source_name.contains(['/', '\\']) {
            report.rejected.push(IntakeRejection {
                source_name,
                code: "invalid_source_name".into(),
                message: "нужно одноуровневое имя файла".into(),
            });
            continue;
        }
        let origin = SourceOrigin::LocalFile {
            source_name: source_name.clone(),
        };
        if let Err(error) = add_source(&runtime, &mut batch, &bytes, origin, hint, &mut report) {
            if error.code == ErrorCode::InvalidTransition {
                return Err(error);
            }
            report.rejected.push(IntakeRejection {
                source_name,
                code: error.code.as_str().into(),
                message: error.message,
            });
        }
    }
    save(&mut runtime, &mut batch)?;
    report.sources = batch.sources.len();
    report.revision = batch.revision;
    Ok(report)
}

/// Принимает локальные MP3 без чтения и без связи с карточкой. Технически
/// непригодный файл получает собственный исход и не сохраняется; остальные
/// файлы принимаются.
pub fn intake_files(
    store: &AssetStore,
    batch_id: &str,
    files: Vec<IntakeFile>,
    surface_hint: Option<&str>,
) -> Result<IntakeReport, AssetError> {
    let hint = surface_hint.map(normalize_surface).transpose()?;
    intake_inputs(
        store,
        batch_id,
        files.into_iter().map(IntakeInput::File),
        hint.as_deref(),
    )
}

/// Потоковый приём по путям: файл читается ровно перед своей обработкой через
/// защищённую границу вызывающего, поэтому набор из тысяч MP3 не требует
/// держать их байты в памяти одновременно. Нечитаемый файл получает
/// собственный отказ в отчёте, а не отменяет остальные.
pub fn intake_paths(
    store: &AssetStore,
    batch_id: &str,
    paths: &[std::path::PathBuf],
    read: &mut dyn FnMut(&std::path::Path) -> Result<Vec<u8>, AssetError>,
    surface_hint: Option<&str>,
) -> Result<IntakeReport, AssetError> {
    let hint = surface_hint.map(normalize_surface).transpose()?;
    let inputs = paths.iter().map(|path| {
        let Some(source_name) = path.file_name().and_then(|name| name.to_str()) else {
            return IntakeInput::Unreadable {
                source_name: "без_имени".into(),
                error: AssetError::new(ErrorCode::InvalidIdentity, "нет имени файла UTF-8"),
            };
        };
        match read(path) {
            Ok(bytes) => IntakeInput::File(IntakeFile {
                source_name: source_name.to_owned(),
                bytes,
            }),
            Err(error) => IntakeInput::Unreadable {
                source_name: source_name.to_owned(),
                error,
            },
        }
    });
    intake_inputs(store, batch_id, inputs, hint.as_deref())
}

/// Принимает исходники завершённых получений Forvo-пакета того же хранилища.
/// Чтение плана сохраняется только как подсказка.
pub fn intake_forvo_batch(
    store: &AssetStore,
    batch_id: &str,
    forvo_batch_id: &str,
) -> Result<IntakeReport, AssetError> {
    if forvo_batch_id == batch_id {
        return Err(invalid(
            "пакет выбора и Forvo-пакет должны иметь разные batch_id",
        ));
    }
    let mut forvo_runtime = open(store.root(), forvo_batch_id)?;
    let forvo: crate::forvo::batch::ForvoBatch = forvo_runtime
        .load()?
        .ok_or_else(|| invalid("Forvo-пакет не найден"))?;
    // Свидетельства всех получений проверяются до первой записи, а байты
    // читаются по одному: набор не требует держать все MP3 в памяти.
    let mut acquisitions = Vec::new();
    for item in &forvo.items {
        if let Some(acquisition) = &item.acquisition {
            acquisition
                .evidence
                .validate(&item.request, &acquisition.blob.sha256)?;
            acquisitions.push((
                item.request.clone(),
                acquisition.evidence.clone(),
                acquisition.blob.clone(),
            ));
        }
    }
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = runtime
        .load::<SelectionBatch>()?
        .unwrap_or_else(|| SelectionBatch::new(batch_id));
    let mut report = IntakeReport::default();
    for (request, evidence, blob) in acquisitions {
        let bytes = forvo_runtime.read_blob_with_limit(&blob, MAX_AUDIO_BYTES)?;
        let origin = SourceOrigin::ForvoBatch {
            batch_id: forvo_batch_id.into(),
            evidence,
            plan_reading: request.reading.clone(),
            surface: Some(request.surface.clone()),
        };
        if let Err(error) = add_source(
            &runtime,
            &mut batch,
            &bytes,
            origin,
            Some(&request.surface),
            &mut report,
        ) {
            if error.code == ErrorCode::InvalidTransition {
                return Err(error);
            }
            report.rejected.push(IntakeRejection {
                source_name: format!("forvo:{}", request.surface),
                code: error.code.as_str().into(),
                message: error.message,
            });
        }
    }
    save(&mut runtime, &mut batch)?;
    report.sources = batch.sources.len();
    report.revision = batch.revision;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Связь с целью
// ---------------------------------------------------------------------------

pub(crate) fn link_id(source_sha: &str, surface: &str, reading: &str) -> String {
    format!("l-{}", short_hash(&(source_sha, surface, reading)))
}

/// Идентификатор кандидата: связь, вид, точные байты и интервал.
///
/// Байты, вид и интервал — это и есть идентичность кандидата, поэтому
/// `occurrence` в хеш не входит: приход ASR-свидетельства без изменения
/// контекста не должен превращать уже выбранного кандидата в другого и
/// отменять точное решение пользователя. Вхождение остаётся в поле
/// кандидата и учитывается при оценке, а интервал различает варианты
/// `OccurrenceInterval` своими границами.
fn candidate_id(
    link_id: &str,
    kind: CandidateKind,
    sha: &str,
    span: Option<&CandidateSpan>,
) -> String {
    format!("c-{}", short_hash(&(link_id, kind.as_str(), sha, span)))
}

/// Явная связь исходника с целью без распознавания (например, чтение пришло
/// позже). Распознавание можно добавить позже `link-recognizer`.
pub fn link_target(
    store: &AssetStore,
    batch_id: &str,
    source_sha256: &str,
    surface: &str,
    reading: &str,
    competing_readings: Vec<String>,
    particle_morae: Vec<usize>,
) -> Result<TargetLink, AssetError> {
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    let request = Pronunciation::new(surface, reading)?;
    let source = batch
        .source(source_sha256)
        .ok_or_else(|| invalid("исходник с таким SHA не принят в пакет выбора"))?
        .clone();
    let target = TargetIdentity {
        surface: request.surface.clone(),
        reading: request.reading.clone(),
        source: TargetSource {
            kind: TargetSourceKind::Request,
            key: format!("selection-link:{source_sha256}"),
        },
        competing_readings,
    };
    target.validate()?;
    let mora_count = crate::pronunciation::kana::morae(&crate::pronunciation::kana::to_hiragana(
        &request.reading,
    ))
    .map(|m| m.len())
    .unwrap_or(0);
    if particle_morae.iter().any(|&i| i >= mora_count) {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "позиция частицы вне мор чтения",
        ));
    }
    let id = link_id(&source.sha256, &request.surface, &request.reading);
    let link = match batch.links.iter_mut().find(|l| l.link_id == id) {
        Some(existing) => {
            if existing.competing_readings != target.competing_readings
                || existing.particle_morae != particle_morae
            {
                existing.generation = existing
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| invalid("исчерпаны поколения связи"))?;
                existing.recognition = None;
                existing.occurrences.clear();
                for candidate in &mut existing.candidates {
                    candidate.occurrence = None;
                }
            }
            existing.competing_readings = target.competing_readings.clone();
            existing.particle_morae = particle_morae;
            existing.clone()
        }
        None => {
            let link = TargetLink {
                link_id: id.clone(),
                source_sha256: source.sha256.clone(),
                surface: request.surface.clone(),
                reading: request.reading.clone(),
                generation: 0,
                competing_readings: target.competing_readings.clone(),
                particle_morae,
                origin: TargetOrigin {
                    kind: TargetSourceKind::Request,
                    key: target.source.key.clone(),
                },
                recognition: None,
                occurrences: Vec::new(),
                candidates: vec![PronunciationCandidate {
                    candidate_id: candidate_id(
                        &id,
                        CandidateKind::SourceOriginal,
                        &source.sha256,
                        None,
                    ),
                    kind: CandidateKind::SourceOriginal,
                    audio: Some(CandidateAudio {
                        sha256: source.sha256.clone(),
                        byte_length: source.byte_length,
                        blob: source.blob.clone(),
                        probe: source.probe.clone(),
                    }),
                    span: None,
                    occurrence: None,
                    technical: TechnicalGate {
                        usable: true,
                        code: "source_probe_ok".into(),
                    },
                }],
            };
            batch.links.push(link.clone());
            link
        }
    };
    save(&mut runtime, &mut batch)?;
    Ok(link)
}

#[derive(Debug, Default, Serialize)]
pub struct LinkReport {
    pub recognizer_batch_id: String,
    pub results: usize,
    pub links_created: usize,
    pub links_updated: usize,
    pub free_analyses: usize,
    pub sources_intaken: usize,
    /// Результаты по SHA, которых нет в пакете выбора (без `--intake-missing`).
    pub missing_sources: Vec<String>,
    pub candidates: usize,
    pub revision: u64,
}

fn rejected_source(result: &RecognitionResult) -> bool {
    result.outcome == RecognitionOutcome::SourceRejected
}

fn build_link(
    runtime: &SafeBatchRuntime,
    cache: &ResultCache,
    recognizer_batch_id: &str,
    source: &AudioSource,
    target: &TargetIdentity,
    result: &RecognitionResult,
    previous_particles: Vec<usize>,
) -> Result<TargetLink, AssetError> {
    let id = link_id(&source.sha256, &target.surface, &target.reading);
    let sample_rate = result
        .pre_analysis
        .as_ref()
        .map(|p| p.timeline.sample_rate)
        .unwrap_or(source.probe.sample_rate);
    let span_of = |start: u64, end: u64| CandidateSpan {
        start_frame: start,
        end_frame: end,
        sample_rate,
    };
    let occurrences: Vec<OccurrenceEvidence> = result
        .matching
        .iter()
        .flat_map(|m| m.occurrences.iter())
        .filter(|o| o.start_uncertainty.start < o.end_uncertainty.end)
        .map(|o| OccurrenceEvidence {
            index: o.index,
            evidence: o.evidence,
            primary_free: o.free_phonemes.clone(),
            primary_distance: o.free_distance,
            verifier_text: o.verifier_text.clone(),
            verifier_distance: o.verifier_distance,
            best_competitor: o.best_competitor.clone(),
            competitor_margin: o.competitor_margin,
            snr_db: o.snr_db,
            core_ms: (
                o.core.start as f64 * 1000.0 / f64::from(sample_rate),
                o.core.end as f64 * 1000.0 / f64::from(sample_rate),
            ),
            span: span_of(o.start_uncertainty.start, o.end_uncertainty.end),
            reasons: o.reasons.clone(),
        })
        .collect();
    let selected = result
        .selected_occurrence
        .and_then(|i| occurrences.iter().find(|o| o.index == i).map(|o| o.index))
        .or_else(|| occurrences.first().map(|o| o.index));
    let fragment_mode = result
        .processing
        .as_ref()
        .is_some_and(|p| p.mode == ProcessingMode::TargetFragment);
    let recognition = RecognitionSummary {
        recognizer_batch_id: recognizer_batch_id.into(),
        config_sha256: result.config_sha256.clone(),
        outcome: result.outcome,
        next_action: result.next_action,
        reason_codes: result.reasons.iter().map(|r| r.code.clone()).collect(),
        primary_free: result
            .pre_analysis
            .as_ref()
            .map(|p| p.primary.raw_text.clone()),
        verifier_free: result
            .pre_analysis
            .as_ref()
            .and_then(|p| p.verifier.as_ref().map(|v| v.raw_text.clone())),
        occurrences: occurrences.len(),
        selected_occurrence: result.selected_occurrence,
        post_validation_passed: result.post_validation.as_ref().map(|p| p.passed),
        post_validation_failed_checks: result
            .post_validation
            .iter()
            .flat_map(|p| p.checks.iter())
            .filter(|c| !c.passed)
            .map(|c| c.id.clone())
            .collect(),
        quality_details: Some(serde_json::json!({
            "pre_analysis": result.pre_analysis.as_ref().map(|p| serde_json::json!({
                "timeline": p.timeline, "noise_floor_dbfs": p.noise_floor_dbfs,
                "activity": p.activity, "unrecognized_activity": p.unrecognized_activity,
            })),
            "full_record": result.full_record.as_ref().map(|f| serde_json::json!({
                "balanced_config_sha256": f.balanced_config_sha256,
                "balanced_outcome": f.balanced_outcome, "proposed_variant": f.proposed_variant,
                "selected_sha256": f.selected_sha256,
            })),
            "processing": result.processing.as_ref().map(|p| serde_json::json!({
                "mode": p.mode, "extraction": p.extraction, "context": p.context,
                "legacy_enhancement_outcome": p.legacy_enhancement_outcome,
                "enhancement": p.enhancement.as_ref().map(|e| serde_json::json!({
                    "triage": e.triage, "applied": e.applied, "qc": e.qc, "reasons": e.reasons,
                })),
                "balanced": p.balanced.as_ref().map(|b| serde_json::json!({
                    "analysis": b.analysis, "plan": b.plan, "post_qc": b.post_qc, "gate": b.gate,
                })),
                "trim_preserves_core": p.trim_preserves_core, "output_span": p.output_span,
            })),
            "post_validation": result.post_validation,
            "uncertainty": result.uncertainty,
        })),
        fragment_mode,
        unrecognized_activity: result
            .pre_analysis
            .as_ref()
            .is_some_and(|p| !p.unrecognized_activity.is_empty()),
    };
    let mut candidates = Vec::new();
    let source_usable = !rejected_source(result);
    candidates.push(PronunciationCandidate {
        candidate_id: candidate_id(&id, CandidateKind::SourceOriginal, &source.sha256, None),
        kind: CandidateKind::SourceOriginal,
        audio: Some(CandidateAudio {
            sha256: source.sha256.clone(),
            byte_length: source.byte_length,
            blob: source.blob.clone(),
            probe: source.probe.clone(),
        }),
        span: None,
        occurrence: selected,
        technical: TechnicalGate {
            usable: source_usable,
            code: if source_usable {
                "source_probe_ok"
            } else {
                "source_rejected"
            }
            .into(),
        },
    });
    let stored_audio = |bytes: Vec<u8>, expected: &str| -> Result<CandidateAudio, AssetError> {
        if sha256_hex(&bytes) != expected {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты кандидата распознавателя не совпадают с SHA результата",
            ));
        }
        let probe = probe_mp3(&bytes)?;
        let blob = runtime.persist_blob_with_limit(&bytes, "mp3", MAX_AUDIO_BYTES)?;
        Ok(CandidateAudio {
            sha256: expected.into(),
            byte_length: bytes.len() as u64,
            blob,
            probe,
        })
    };
    if let Some(bytes) = cache.full_record_bytes(result)?
        && let Some(full) = result.full_record.as_ref().and_then(|f| f.output.as_ref())
        && full.sha256 != source.sha256
    {
        let audio = stored_audio(bytes, &full.sha256)?;
        candidates.push(PronunciationCandidate {
            candidate_id: candidate_id(
                &id,
                CandidateKind::ProcessedFullRecord,
                &audio.sha256,
                None,
            ),
            kind: CandidateKind::ProcessedFullRecord,
            audio: Some(audio),
            span: None,
            occurrence: selected,
            technical: TechnicalGate {
                usable: true,
                code: "balanced_selected".into(),
            },
        });
    }
    if let Some(output) = &result.output
        && let Some(bytes) = cache.output_bytes(result)?
    {
        let audio = stored_audio(bytes, &output.sha256)?;
        let passed = result.next_action == RecognitionAction::UseFinalCandidate
            && result.post_validation.as_ref().is_some_and(|p| p.passed);
        let kind = if fragment_mode {
            CandidateKind::ExtractedFragment
        } else {
            CandidateKind::ProcessedWhole
        };
        let span = result
            .processing
            .as_ref()
            .and_then(|p| p.output_span)
            .filter(|s| s.start < s.end)
            .map(|s| span_of(s.start, s.end));
        candidates.push(PronunciationCandidate {
            candidate_id: candidate_id(&id, kind, &audio.sha256, span.as_ref()),
            kind,
            audio: Some(audio),
            span,
            occurrence: result
                .selected_occurrence
                .filter(|i| occurrences.iter().any(|o| o.index == *i)),
            technical: TechnicalGate {
                usable: passed,
                code: if passed {
                    "post_validation_passed"
                } else {
                    "post_validation_failed"
                }
                .into(),
            },
        });
    }
    if matches!(
        result.outcome,
        RecognitionOutcome::MultipleOccurrences | RecognitionOutcome::Ambiguous
    ) {
        for occurrence in &occurrences {
            candidates.push(PronunciationCandidate {
                candidate_id: candidate_id(
                    &id,
                    CandidateKind::OccurrenceInterval,
                    &source.sha256,
                    Some(&occurrence.span),
                ),
                kind: CandidateKind::OccurrenceInterval,
                audio: None,
                span: Some(occurrence.span),
                occurrence: Some(occurrence.index),
                technical: TechnicalGate {
                    usable: true,
                    code: "interval_not_extracted".into(),
                },
            });
        }
    }
    Ok(TargetLink {
        link_id: id,
        source_sha256: source.sha256.clone(),
        surface: target.surface.clone(),
        reading: target.reading.clone(),
        generation: 0,
        competing_readings: target.competing_readings.clone(),
        particle_morae: previous_particles,
        origin: TargetOrigin {
            kind: target.source.kind,
            key: target.source.key.clone(),
        },
        recognition: Some(recognition),
        occurrences,
        candidates,
    })
}

#[cfg(test)]
pub(crate) fn build_link_for_test(
    runtime: &SafeBatchRuntime,
    cache: &ResultCache,
    recognizer_batch_id: &str,
    source: &AudioSource,
    result: &RecognitionResult,
) -> Result<TargetLink, AssetError> {
    let target = result.target.clone().unwrap_or_else(|| TargetIdentity {
        surface: "生".into(),
        reading: "なま".into(),
        source: TargetSource {
            kind: TargetSourceKind::Request,
            key: "test".into(),
        },
        competing_readings: Vec::new(),
    });
    build_link(
        runtime,
        cache,
        recognizer_batch_id,
        source,
        &target,
        result,
        Vec::new(),
    )
}

/// Чтение исходника входа распознавания (с границами вызывающего CLI).
pub type SourceReader<'a> = dyn FnMut(&Path) -> Result<Vec<u8>, AssetError> + 'a;

/// Связывает исходники пакета с готовыми результатами пакета распознавания.
/// Модели не вызываются: используются только кэшированные результаты и их
/// проверенные MP3.
pub fn link_recognizer(
    store: &AssetStore,
    batch_id: &str,
    workspace: &Path,
    recognizer_batch_id: &str,
    mut read_source: Option<&mut SourceReader<'_>>,
) -> Result<LinkReport, AssetError> {
    let (recognizer, inventory, manifest) = recognizer_batch::load(workspace, recognizer_batch_id)?;
    let cache = ResultCache::open(workspace)?;
    let results =
        recognizer_batch::collect_results(&cache, &recognizer, &inventory, manifest.as_ref())?;
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    let mut report = LinkReport {
        recognizer_batch_id: recognizer_batch_id.into(),
        results: results.len(),
        ..LinkReport::default()
    };
    let mut missing = BTreeSet::new();
    for (item, result) in &results {
        let WorkItem { sha256, path, .. } = item;
        if batch.source(sha256).is_none() {
            match read_source.as_mut() {
                Some(read) => {
                    let file = Path::new(&recognizer.input_root).join(path);
                    let bytes = read(&file)?;
                    if sha256_hex(&bytes) != *sha256 {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "исходник входа распознавания изменился после инвентаризации",
                        ));
                    }
                    let name = Path::new(path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("source.mp3")
                        .to_owned();
                    let mut intake = IntakeReport::default();
                    add_source(
                        &runtime,
                        &mut batch,
                        &bytes,
                        SourceOrigin::LocalFile { source_name: name },
                        None,
                        &mut intake,
                    )?;
                    report.sources_intaken += intake.added.len();
                }
                None => {
                    missing.insert(sha256.clone());
                    continue;
                }
            }
        }
        let source = batch.source(sha256).expect("исходник принят").clone();
        let Some(target) = &result.target else {
            let analysis = FreeAnalysis {
                config_sha256: result.config_sha256.clone(),
                primary: result
                    .pre_analysis
                    .as_ref()
                    .map(|p| p.primary.raw_text.clone()),
                verifier: result
                    .pre_analysis
                    .as_ref()
                    .and_then(|p| p.verifier.as_ref().map(|v| v.raw_text.clone())),
                outcome: result.outcome,
            };
            if let Some(source) = batch.sources.iter_mut().find(|s| s.sha256 == *sha256) {
                source.free_analysis = Some(analysis);
            }
            report.free_analyses += 1;
            continue;
        };
        let id = link_id(sha256, &target.surface, &target.reading);
        let particles = batch
            .links
            .iter()
            .find(|l| l.link_id == id)
            .map(|l| l.particle_morae.clone())
            .unwrap_or_default();
        let mut link = build_link(
            &runtime,
            &cache,
            recognizer_batch_id,
            &source,
            target,
            result,
            particles,
        )?;
        report.candidates += link.candidates.len();
        match batch.links.iter_mut().find(|l| l.link_id == id) {
            Some(existing) => {
                link.generation = existing.generation;
                if existing.competing_readings != link.competing_readings
                    || existing.particle_morae != link.particle_morae
                {
                    link.generation = existing
                        .generation
                        .checked_add(1)
                        .ok_or_else(|| invalid("исчерпаны поколения связи"))?;
                }
                *existing = link;
                report.links_updated += 1;
            }
            None => {
                if batch.links.len() >= MAX_LINKS {
                    return Err(invalid(format!(
                        "пакет выбора ограничен {MAX_LINKS} связями"
                    )));
                }
                batch.links.push(link);
                report.links_created += 1;
            }
        }
    }
    report.missing_sources = missing.into_iter().collect();
    save(&mut runtime, &mut batch)?;
    report.revision = batch.revision;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Состояние для агента
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupFilter {
    All,
    Undecided,
    Unresolved,
    Class(SelectionClass),
}

impl GroupFilter {
    pub fn parse(text: &str) -> Result<Self, AssetError> {
        Ok(match text {
            "all" => Self::All,
            "undecided" => Self::Undecided,
            "unresolved" => Self::Unresolved,
            other => Self::Class(
                serde_json::from_value(serde_json::json!(other.replace('-', "_"))).map_err(
                    |_| {
                        AssetError::new(
                            ErrorCode::InvalidIdentity,
                            "фильтр: all, undecided, unresolved или класс выбора",
                        )
                    },
                )?,
            ),
        })
    }

    pub(crate) fn accepts(
        self,
        index: &SelectionIndex,
        batch: &SelectionBatch,
        recommendation: &GroupRecommendation,
        decision: Option<&SelectionDecision>,
    ) -> bool {
        let selected = decision.is_some_and(|d| decision_is_effective(index, batch, d));
        let deferred = decision.is_some_and(|d| matches!(d.action, DecisionAction::Defer));
        match self {
            Self::All => true,
            Self::Undecided => !selected,
            Self::Unresolved => {
                !selected && (deferred || recommendation.class > SelectionClass::AgentSelectable)
            }
            Self::Class(class) => recommendation.class == class,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CandidateView<'a> {
    pub assessment: &'a CandidateAssessment,
    pub kind: CandidateKind,
    pub addressed_sha256: &'a str,
    pub source_sha256: &'a str,
    pub span_ms: Option<(f64, f64)>,
    pub technical: &'a TechnicalGate,
}

#[derive(Debug, Serialize)]
pub struct GroupView<'a> {
    pub surface: &'a str,
    pub reading: &'a str,
    pub recommendation: &'a GroupRecommendation,
    pub decision: Option<&'a SelectionDecision>,
    pub selection_effective: bool,
    pub candidates: Vec<CandidateView<'a>>,
    pub links: Vec<serde_json::Value>,
    pub sources: Vec<SourceView<'a>>,
    pub audio_reviews: Vec<&'a AudioReview>,
}

/// Проверяемое происхождение без внутренних путей blob-хранилища.
#[derive(Debug, Serialize)]
pub struct SourceView<'a> {
    pub sha256: &'a str,
    pub byte_length: u64,
    pub probe: &'a crate::pronunciation::AudioProbe,
    pub origins: &'a [SourceOrigin],
    pub surface_hint: &'a Option<String>,
    pub free_analysis: &'a Option<FreeAnalysis>,
}

/// Машиночитаемый пакет свидетельств для агента.
#[derive(Debug, Serialize)]
pub struct StatusView<'a> {
    pub batch_id: &'a str,
    pub revision: u64,
    pub policy: &'static str,
    /// Пакет содержит текст ASR, метрики и SHA. Получатель, у которого есть
    /// только этот документ, запись не слушал и не вправе это утверждать.
    pub perception_notice: &'static str,
    pub sources: usize,
    pub links: usize,
    pub groups: usize,
    pub classes: BTreeMap<&'static str, usize>,
    /// Полномочия только выбранных целей.
    pub decided: BTreeMap<&'static str, usize>,
    pub dispositions: BTreeMap<&'static str, usize>,
    pub matched: usize,
    pub items: Vec<GroupView<'a>>,
}

pub const PERCEPTION_NOTICE: &str = "evidence_bundle_text_and_metrics_only: получатель видит транскрипции, метрики и SHA; это не прослушивание записи";

pub(crate) fn group_view<'a>(
    index: &SelectionIndex,
    batch: &'a SelectionBatch,
    recommendation: &'a GroupRecommendation,
) -> GroupView<'a> {
    let candidates = recommendation
        .candidates
        .iter()
        .filter_map(|assessment| {
            let (link, candidate) = batch.find_candidate(&assessment.candidate_id)?;
            Some(CandidateView {
                assessment,
                kind: candidate.kind,
                addressed_sha256: candidate.addressed_sha(&link.source_sha256),
                source_sha256: &link.source_sha256,
                span_ms: candidate.span.map(|s| (s.start_ms(), s.end_ms())),
                technical: &candidate.technical,
            })
        })
        .collect();
    GroupView {
        surface: &recommendation.surface,
        reading: &recommendation.reading,
        recommendation,
        decision: batch.current_decision(&recommendation.surface, &recommendation.reading),
        selection_effective: batch
            .current_decision(&recommendation.surface, &recommendation.reading)
            .is_some_and(|d| decision_is_effective(index, batch, d)),
        candidates,
        links: batch
            .links_of(&recommendation.surface, &recommendation.reading)
            .map(|link| {
                let mut public = serde_json::to_value(link).expect("связь сериализуема");
                if let Some(origin) = public.get_mut("origin").and_then(|v| v.as_object_mut()) {
                    origin.remove("key");
                }
                if let Some(candidates) =
                    public.get_mut("candidates").and_then(|v| v.as_array_mut())
                {
                    for candidate in candidates {
                        if let Some(audio) =
                            candidate.get_mut("audio").and_then(|v| v.as_object_mut())
                        {
                            audio.remove("blob");
                        }
                    }
                }
                public
            })
            .collect(),
        sources: batch
            .links_of(&recommendation.surface, &recommendation.reading)
            .map(|l| l.source_sha256.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|sha| batch.source(sha))
            .map(|s| SourceView {
                sha256: &s.sha256,
                byte_length: s.byte_length,
                probe: &s.probe,
                origins: &s.origins,
                surface_hint: &s.surface_hint,
                free_analysis: &s.free_analysis,
            })
            .collect(),
        audio_reviews: batch
            .audio_reviews
            .iter()
            .filter(|review| {
                review.surface == recommendation.surface && review.reading == recommendation.reading
            })
            .collect(),
    }
}

pub fn status_value(
    store: &AssetStore,
    batch_id: &str,
    filter: GroupFilter,
    limit: usize,
) -> Result<serde_json::Value, AssetError> {
    let mut runtime = open(store.root(), batch_id)?;
    let batch = load_existing(&mut runtime)?;
    let index = SelectionIndex::new(&batch);
    let recommendations = engine::recommend_all_with_index(&index, &batch);
    let mut classes = BTreeMap::new();
    let mut decided = BTreeMap::new();
    let mut dispositions = BTreeMap::new();
    let mut items = Vec::new();
    let mut matched = 0;
    for recommendation in recommendations.values() {
        *classes.entry(recommendation.class.as_str()).or_insert(0) += 1;
        let decision = batch.current_decision(&recommendation.surface, &recommendation.reading);
        if let Some(decision) = decision {
            let kind = match decision.action {
                DecisionAction::Select { .. } => {
                    if decision_is_effective(&index, &batch, decision) {
                        *decided.entry(decision.authority.kind()).or_insert(0) += 1;
                        "selected"
                    } else {
                        "selection_expired"
                    }
                }
                DecisionAction::RejectCandidate { .. } => "candidate_rejected",
                DecisionAction::Defer => "deferred",
            };
            *dispositions.entry(kind).or_insert(0) += 1;
        }
        if filter.accepts(&index, &batch, recommendation, decision) {
            matched += 1;
            if items.len() < limit {
                items.push(group_view(&index, &batch, recommendation));
            }
        }
    }
    let view = StatusView {
        batch_id: &batch.batch_id,
        revision: batch.revision,
        policy: POLICY_VERSION,
        perception_notice: PERCEPTION_NOTICE,
        sources: batch.sources.len(),
        links: batch.links.len(),
        groups: recommendations.len(),
        classes,
        decided,
        dispositions,
        matched,
        items,
    };
    serde_json::to_value(view).map_err(|e| invalid(e.to_string()))
}

// ---------------------------------------------------------------------------
// Решения
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize)]
pub struct AutoSelectReport {
    pub selected_deterministic: usize,
    pub selected_agent_evidence: usize,
    pub selected_agent_audio: usize,
    pub already_decided: usize,
    /// Цели, оставшиеся без решения, по классам.
    pub pending: BTreeMap<&'static str, Vec<(String, String)>>,
    pub revision: u64,
}

/// Явная политика агента для `agent_selectable`.
pub struct AgentPolicy {
    pub agent: String,
}

/// Чистое планирование авто-выбора: решения без записи состояния.
pub(crate) fn plan_auto_select(
    batch: &SelectionBatch,
    agent: Option<&AgentPolicy>,
) -> (Vec<SelectionDecision>, AutoSelectReport) {
    let index = SelectionIndex::new(batch);
    let recommendations = engine::recommend_all_with_index(&index, batch);
    let mut report = AutoSelectReport::default();
    let mut decisions = Vec::new();
    for recommendation in recommendations.values() {
        let (surface, reading) = (&recommendation.surface, &recommendation.reading);
        if batch.current_decision(surface, reading).is_some_and(|d| {
            matches!(d.action, DecisionAction::Defer) || decision_is_effective(&index, batch, d)
        }) {
            report.already_decided += 1;
            continue;
        }
        let best = recommendation.candidates.first().filter(|c| {
            c.consumable && recommendation.recommended.as_ref() == Some(&c.candidate_id)
        });
        let authority = match (recommendation.class, best, agent) {
            (SelectionClass::AutoQualified, Some(_), _) => {
                report.selected_deterministic += 1;
                Some(DecisionAuthority::DeterministicAutomation {
                    policy: POLICY_VERSION.into(),
                })
            }
            (SelectionClass::AgentSelectable, Some(best), Some(policy)) => {
                let (link, candidate) = batch
                    .find_candidate(&best.candidate_id)
                    .expect("рекомендация ссылается на кандидата пакета");
                let sha = candidate.addressed_sha(&link.source_sha256);
                let review = best
                    .reasons
                    .contains(&ReasonCode::AudioReviewSupports)
                    .then(|| {
                        batch.audio_reviews.iter().rev().find(|r| {
                            r.candidate_id == best.candidate_id
                                && r.audio_sha256 == sha
                                && r.surface == link.surface
                                && r.reading == link.reading
                                && r.link_generation == link.generation
                                && r.verdict == AudioVerdict::Supports
                        })
                    })
                    .flatten();
                Some(match review {
                    Some(review) => {
                        report.selected_agent_audio += 1;
                        DecisionAuthority::AgentAudioModel {
                            agent: policy.agent.clone(),
                            audio_review_id: review.review_id.clone(),
                            backend: review.backend.clone(),
                        }
                    }
                    None => {
                        report.selected_agent_evidence += 1;
                        DecisionAuthority::AgentEvidence {
                            agent: policy.agent.clone(),
                            policy: AGENT_EVIDENCE_POLICY.into(),
                        }
                    }
                })
            }
            _ => None,
        };
        match (authority, best) {
            (Some(authority), Some(best)) => {
                let (link, candidate) = batch
                    .find_candidate(&best.candidate_id)
                    .expect("рекомендация ссылается на кандидата пакета");
                decisions.push(SelectionDecision {
                    decision_id: String::new(),
                    surface: surface.clone(),
                    reading: reading.clone(),
                    action: DecisionAction::Select {
                        candidate_id: candidate.candidate_id.clone(),
                        addressed_sha256: candidate.addressed_sha(&link.source_sha256).into(),
                        source_sha256: link.source_sha256.clone(),
                        span: candidate.span,
                        link_generation: link.generation,
                    },
                    authority,
                    reason: format!(
                        "{}; proof={}",
                        recommendation.class.as_str(),
                        serde_json::to_value(best.proof)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    ),
                    group_fingerprint: recommendation.fingerprint.clone(),
                    state_revision: 0,
                    recorded_at_unix_ms: None,
                });
            }
            _ => report
                .pending
                .entry(recommendation.class.as_str())
                .or_default()
                .push((surface.clone(), reading.clone())),
        }
    }
    (decisions, report)
}

/// Принимает все бесспорные решения и оставляет остальные без остановки.
pub fn auto_select(
    store: &AssetStore,
    batch_id: &str,
    agent: Option<&AgentPolicy>,
) -> Result<AutoSelectReport, AssetError> {
    if let Some(policy) = agent
        && !valid_token(&policy.agent)
    {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "идентификатор агента: ASCII буквы, цифры, -, _, ., :",
        ));
    }
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    let (decisions, mut report) = plan_auto_select(&batch, agent);
    let index = SelectionIndex::new(&batch);
    if !decisions.is_empty() {
        // До записи всех решений проверяем реальные blob для экспертиз,
        // использованных автоматическим планом, и их действующие полномочия.
        for decision in &decisions {
            if matches!(
                decision.authority,
                DecisionAuthority::AgentAudioModel { .. }
            ) {
                let request = Pronunciation::new(&decision.surface, &decision.reading)?;
                let (link, candidate, _) =
                    validate_effective_selection(&index, &batch, decision, &request)?;
                let bytes = candidate_bytes(&runtime, &batch, link, candidate)?;
                if sha256_hex(&bytes) != candidate.addressed_sha(&link.source_sha256) {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "байты аудиоэкспертизы изменились",
                    ));
                }
                probe_mp3(&bytes)?;
            }
        }
        let revision = batch.revision + 1;
        let timestamp = recorded_at(std::time::SystemTime::now())?;
        for mut decision in decisions {
            decision.decision_id = batch.next_decision_id();
            decision.state_revision = revision;
            decision.recorded_at_unix_ms = Some(timestamp);
            batch.decisions.push(decision);
        }
        save(&mut runtime, &mut batch)?;
    }
    report.revision = batch.revision;
    Ok(report)
}

/// Адресное решение: одна цель, один точный кандидат и SHA.
pub struct DecideRequest {
    pub surface: String,
    pub reading: String,
    pub action: DecideAction,
    pub authority: DecisionAuthority,
    pub reason: String,
    pub expected_fingerprint: Option<String>,
}

pub enum DecideAction {
    Select {
        candidate_id: String,
        expected_sha256: String,
    },
    Reject {
        candidate_id: String,
        expected_sha256: String,
    },
    Defer,
}

#[derive(Debug, Serialize)]
pub struct DecideReport {
    pub decision: SelectionDecision,
    pub changed: bool,
    pub revision: u64,
}

/// Проверяет решение относительно текущего состояния без записи.
pub(crate) fn validate_decision(
    batch: &SelectionBatch,
    recommendation: &GroupRecommendation,
    action: &DecideAction,
    authority: &DecisionAuthority,
) -> Result<DecisionAction, AssetError> {
    let resolve = |candidate_id: &str, expected: &str| {
        let assessment = recommendation
            .candidates
            .iter()
            .find(|c| c.candidate_id == candidate_id)
            .ok_or_else(|| {
                AssetError::new(
                    ErrorCode::IdentityConflict,
                    format!(
                        "кандидат {candidate_id} не принадлежит цели {}/{}",
                        recommendation.surface, recommendation.reading
                    ),
                )
            })?;
        let (link, candidate) = batch
            .find_candidate(candidate_id)
            .expect("оценка ссылается на кандидата пакета");
        let sha = candidate.addressed_sha(&link.source_sha256);
        if sha != expected {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "SHA решения не совпадает с байтами кандидата",
            ));
        }
        Ok((assessment, link, candidate))
    };
    match action {
        DecideAction::Defer => Ok(DecisionAction::Defer),
        DecideAction::Reject {
            candidate_id,
            expected_sha256,
        } => {
            if matches!(authority, DecisionAuthority::DeterministicAutomation { .. }) {
                return Err(invalid("автоматика не отклоняет кандидатов адресно"));
            }
            let (_, _, candidate) = resolve(candidate_id, expected_sha256)?;
            Ok(DecisionAction::RejectCandidate {
                candidate_id: candidate.candidate_id.clone(),
                addressed_sha256: expected_sha256.clone(),
            })
        }
        DecideAction::Select {
            candidate_id,
            expected_sha256,
        } => {
            let (assessment, link, candidate) = resolve(candidate_id, expected_sha256)?;
            if !candidate.technical.usable || assessment.class == SelectionClass::Rejected {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "технически отклонённого кандидата выбрать нельзя: решение не обходит проверки",
                ));
            }
            match authority {
                DecisionAuthority::DeterministicAutomation { .. } => {
                    return Err(invalid("детерминированный выбор делает только auto-select"));
                }
                DecisionAuthority::AgentEvidence { .. } => {
                    if assessment.class > SelectionClass::AgentSelectable {
                        return Err(AssetError::new(
                            ErrorCode::InvalidValidationEvidence,
                            "agent_proposed — только предложение: для выбора нужны аудиоэкспертиза или явное решение пользователя",
                        ));
                    }
                }
                DecisionAuthority::AgentAudioModel { .. } => {
                    validate_agent_authority(batch, link, candidate, authority, assessment)?;
                }
                DecisionAuthority::UserViaAgent { .. }
                | DecisionAuthority::UserDirectReview { .. } => {}
            }
            Ok(DecisionAction::Select {
                candidate_id: candidate.candidate_id.clone(),
                addressed_sha256: expected_sha256.clone(),
                source_sha256: link.source_sha256.clone(),
                span: candidate.span,
                link_generation: link.generation,
            })
        }
    }
}

/// Проверяет действующее право агента, включая последнюю экспертизу точного
/// кандидата. Историческое Supports не отменяет более поздний вердикт.
fn validate_agent_authority(
    batch: &SelectionBatch,
    link: &TargetLink,
    candidate: &PronunciationCandidate,
    authority: &DecisionAuthority,
    assessment: &CandidateAssessment,
) -> Result<(), AssetError> {
    let permitted = match authority {
        DecisionAuthority::DeterministicAutomation { .. } => {
            assessment.class == SelectionClass::AutoQualified
        }
        _ => assessment.class <= SelectionClass::AgentSelectable,
    };
    if !permitted {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "текущие свидетельства не разрешают агентский выбор кандидата",
        ));
    }
    if let DecisionAuthority::AgentAudioModel {
        audio_review_id,
        backend,
        ..
    } = authority
    {
        let sha = candidate.addressed_sha(&link.source_sha256);
        let latest = batch.audio_reviews.iter().rev().find(|r| {
            r.candidate_id == candidate.candidate_id
                && r.audio_sha256 == sha
                && r.surface == link.surface
                && r.reading == link.reading
                && r.link_generation == link.generation
        });
        if !latest.is_some_and(|r| {
            &r.review_id == audio_review_id
                && &r.backend == backend
                && trustworthy_audio_review(r)
                && r.verdict == AudioVerdict::Supports
        }) {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "нет актуальной поддерживающей аудиоэкспертизы точного кандидата и backend",
            ));
        }
    }
    Ok(())
}

/// Только свидетельство штатного локального вызова может повышать доверие.
pub(crate) fn trustworthy_audio_review(review: &AudioReview) -> bool {
    review.backend == LOCAL_AUDIO_BACKEND
        && review.local
        && !review.independent_of_triage
        && !review.model_fingerprints.is_empty()
        && review
            .model_fingerprints
            .iter()
            .all(|fingerprint| valid_sha(fingerprint))
        && valid_sha(&review.config_sha256)
}

/// Общая проверка действующего решения перед чтением и публикацией.
fn validate_effective_selection<'a>(
    index: &SelectionIndex,
    batch: &'a SelectionBatch,
    decision: &SelectionDecision,
    request: &Pronunciation,
) -> Result<
    (
        &'a TargetLink,
        &'a PronunciationCandidate,
        CandidateAssessment,
    ),
    AssetError,
> {
    let DecisionAction::Select {
        candidate_id,
        addressed_sha256,
        source_sha256,
        span,
        link_generation,
    } = &decision.action
    else {
        return Err(invalid("текущее решение цели — не выбор кандидата"));
    };
    let (link, candidate) = batch
        .find_candidate(candidate_id)
        .ok_or_else(|| invalid("выбранный кандидат больше не существует; решение устарело"))?;
    if decision.surface != request.surface
        || decision.reading != request.reading
        || link.surface != request.surface
        || link.reading != request.reading
        || &link.source_sha256 != source_sha256
        || candidate.span != *span
        || link.generation != *link_generation
        || candidate.addressed_sha(&link.source_sha256) != addressed_sha256
        || !candidate.technical.usable
    {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "решение не относится к точной паре, кандидату, SHA и интервалу",
        ));
    }
    let assessment = engine::assess_candidate_with(index, link, candidate);
    if !decision.authority.is_user() {
        validate_agent_authority(batch, link, candidate, &decision.authority, &assessment)?;
    }
    Ok((link, candidate, assessment))
}

pub(crate) fn decision_is_effective(
    index: &SelectionIndex,
    batch: &SelectionBatch,
    decision: &SelectionDecision,
) -> bool {
    Pronunciation::new(&decision.surface, &decision.reading)
        .ok()
        .is_some_and(|request| {
            validate_effective_selection(index, batch, decision, &request).is_ok()
        })
}

pub fn decide(
    store: &AssetStore,
    batch_id: &str,
    request: DecideRequest,
) -> Result<DecideReport, AssetError> {
    let pronunciation = Pronunciation::new(&request.surface, &request.reading)?;
    if !valid_text(&request.reason, MAX_TEXT) {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "причина решения: 1–512 байт без управляющих символов",
        ));
    }
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    if batch
        .links_of(&pronunciation.surface, &pronunciation.reading)
        .next()
        .is_none()
    {
        return Err(invalid("у цели нет связей в пакете выбора"));
    }
    let recommendation = engine::recommend(&batch, &pronunciation.surface, &pronunciation.reading);
    if request
        .expected_fingerprint
        .as_ref()
        .is_some_and(|f| f != &recommendation.fingerprint)
    {
        return Err(invalid(
            "свидетельства цели изменились после показа агенту: получите статус заново",
        ));
    }
    let action = validate_decision(&batch, &recommendation, &request.action, &request.authority)?;
    if matches!(request.authority, DecisionAuthority::AgentAudioModel { .. })
        && let DecisionAction::Select {
            candidate_id,
            addressed_sha256,
            ..
        } = &action
    {
        let (link, candidate) = batch
            .find_candidate(candidate_id)
            .ok_or_else(|| invalid("кандидат аудиоэкспертизы отсутствует"))?;
        let bytes = candidate_bytes(&runtime, &batch, link, candidate)?;
        if sha256_hex(&bytes) != *addressed_sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты аудиоэкспертизы изменились",
            ));
        }
        probe_mp3(&bytes)?;
    }
    if let Some(current) = batch.current_decision(&pronunciation.surface, &pronunciation.reading)
        && current.action == action
        && current.authority == request.authority
    {
        return Ok(DecideReport {
            decision: current.clone(),
            changed: false,
            revision: batch.revision,
        });
    }
    let decision = SelectionDecision {
        decision_id: batch.next_decision_id(),
        surface: pronunciation.surface,
        reading: pronunciation.reading,
        action,
        authority: request.authority,
        reason: request.reason,
        group_fingerprint: recommendation.fingerprint,
        state_revision: batch.revision + 1,
        recorded_at_unix_ms: Some(recorded_at(std::time::SystemTime::now())?),
    };
    batch.decisions.push(decision.clone());
    save(&mut runtime, &mut batch)?;
    Ok(DecideReport {
        decision,
        changed: true,
        revision: batch.revision,
    })
}

// ---------------------------------------------------------------------------
// Аудиоэкспертиза
// ---------------------------------------------------------------------------

pub const LOCAL_AUDIO_BACKEND: &str = "local-ctc-targeted-reanalysis";

/// Байты, которые получает эксперт: кандидат или исходник интервала.
pub(crate) fn candidate_bytes(
    runtime: &SafeBatchRuntime,
    batch: &SelectionBatch,
    link: &TargetLink,
    candidate: &PronunciationCandidate,
) -> Result<Vec<u8>, AssetError> {
    let blob = match &candidate.audio {
        Some(audio) => &audio.blob,
        None => {
            &batch
                .source(&link.source_sha256)
                .ok_or_else(|| invalid("исходник кандидата отсутствует"))?
                .blob
        }
    };
    runtime.read_blob_with_limit(blob, MAX_AUDIO_BYTES)
}

/// Конкуренты экспертизы: явные чтения связи, переданные агентом и формы,
/// которые услышала вторая модель (если это кана, отличная от цели).
pub(crate) fn expert_competitors(link: &TargetLink, extra: &[String]) -> Vec<String> {
    let target = super::phonetic::hiragana_only(&link.reading);
    let mut seen = BTreeSet::new();
    let heard = link
        .occurrences
        .iter()
        .filter_map(|o| o.verifier_text.as_deref())
        .map(super::phonetic::hiragana_only);
    link.competing_readings
        .iter()
        .cloned()
        .chain(extra.iter().cloned())
        .chain(heard)
        .filter(|alt| {
            crate::pronunciation::kana::is_phonetic_reading(alt)
                && super::phonetic::hiragana_only(alt) != target
                && seen.insert(super::phonetic::hiragana_only(alt))
        })
        .take(16)
        .collect()
}

/// Сводит результат целевого повторного анализа к вердикту по кандидату.
pub(crate) fn verdict_of(
    result: &RecognitionResult,
    span: Option<&CandidateSpan>,
    sample_rate: u32,
) -> (AudioVerdict, Vec<String>) {
    let occurrences: Vec<_> = result
        .matching
        .iter()
        .flat_map(|m| m.occurrences.iter())
        .collect();
    let mut reasons: Vec<String> = result.reasons.iter().map(|r| r.code.clone()).collect();
    if span.is_some() && result.outcome == RecognitionOutcome::BoundariesUnresolved {
        reasons.push("expert_inconclusive".into());
        return (AudioVerdict::Inconclusive, reasons);
    }
    // Границы полуоткрытые; несовпадение шкалы не доказывает пересечения.
    // TARGET_ABSENT относится ко всей записи и поэтому противоречит и интервалу.
    let relevant = |o: &&&crate::pronunciation::recognizer::Occurrence| match span {
        None => true,
        Some(span) => {
            let own = CandidateSpan {
                start_frame: o.core.start,
                end_frame: o.core.end,
                sample_rate,
            };
            span.sample_rate != 0
                && span.start_frame < span.end_frame
                && own.start_frame < own.end_frame
                && own.overlaps(span)
        }
    };
    let conflict = occurrences
        .iter()
        .filter(relevant)
        .any(|o| o.evidence == Evidence::Conflict);
    let competitor_won = occurrences.iter().filter(relevant).any(|o| {
        o.best_competitor
            .as_ref()
            .is_some_and(|c| c.origin.starts_with("explicit:"))
            && o.competitor_margin.is_some_and(|m| m <= 0.0)
    });
    if conflict || competitor_won || result.outcome == RecognitionOutcome::TargetAbsent {
        reasons.push("expert_contradicts_target".into());
        return (AudioVerdict::Contradicts, reasons);
    }
    let supports = matches!(
        result.outcome,
        RecognitionOutcome::TargetWhole
            | RecognitionOutcome::TargetExtracted
            | RecognitionOutcome::MultipleOccurrences
    ) && occurrences.iter().filter(relevant).count() > 0
        && occurrences
            .iter()
            .filter(relevant)
            .all(|o| o.evidence == Evidence::Match);
    if supports {
        reasons.push("expert_supports_target".into());
        (AudioVerdict::Supports, reasons)
    } else {
        reasons.push("expert_inconclusive".into());
        (AudioVerdict::Inconclusive, reasons)
    }
}

/// Локальная аудиоэкспертиза: реальные байты MP3 → PCM → обе модели CTC с
/// явными конкурентами. Ничего не отправляется наружу.
pub(crate) fn audio_review_local(
    store: &AssetStore,
    batch_id: &str,
    candidate_id: &str,
    extra_competitors: &[String],
    setup: &crate::pronunciation_recognizer_cli::Setup,
) -> Result<AudioReview, AssetError> {
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    let (link, candidate) = batch
        .find_candidate(candidate_id)
        .map(|(l, c)| (l.clone(), c.clone()))
        .ok_or_else(|| invalid("кандидат не найден в пакете выбора"))?;
    let bytes = candidate_bytes(&runtime, &batch, &link, &candidate)?;
    let target = TargetIdentity {
        surface: link.surface.clone(),
        reading: link.reading.clone(),
        source: TargetSource {
            kind: TargetSourceKind::Request,
            key: format!("selection-audio-review:{candidate_id}"),
        },
        competing_readings: expert_competitors(&link, extra_competitors),
    };
    target.validate()?;
    let direct = setup.direct();
    let ctx = setup.context(&direct);
    let recognized =
        crate::pronunciation::recognizer_pipeline::recognize_one(&bytes, Some(&target), &ctx)?;
    let sample_rate = batch
        .source(&link.source_sha256)
        .ok_or_else(|| invalid("исходник аудиоэкспертизы отсутствует"))?
        .probe
        .sample_rate;
    // Интервал сверяется по шкале исходника; у байтов кандидата своя шкала.
    let span = if candidate.audio.is_none() {
        candidate.span.as_ref()
    } else {
        None
    };
    let (verdict, reason_codes) = verdict_of(&recognized.result, span, sample_rate);
    let review = AudioReview {
        review_id: format!("a-{}", batch.audio_reviews.len() + 1),
        backend: LOCAL_AUDIO_BACKEND.into(),
        model_fingerprints: std::iter::once(setup.primary.identity.fingerprint())
            .chain(setup.verifier.iter().map(|v| v.identity.fingerprint()))
            .collect(),
        config_sha256: recognized.result.config_sha256.clone(),
        audio_sha256: sha256_hex(&bytes),
        candidate_id: candidate_id.into(),
        surface: link.surface.clone(),
        reading: link.reading.clone(),
        link_generation: link.generation,
        competitors: target.competing_readings.clone(),
        outcome: recognized.result.outcome,
        verdict,
        reason_codes,
        local: true,
        independent_of_triage: false,
    };
    batch.audio_reviews.push(review.clone());
    save(&mut runtime, &mut batch)?;
    Ok(review)
}

// ---------------------------------------------------------------------------
// Публикация и чтение по явной политике
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PromoteReport {
    pub asset: AssetRecord,
    pub decision_id: String,
    pub authority: DecisionAuthority,
    /// `HumanAttestation` создаётся только по явному флагу и только для
    /// решения пользователя, который сам заявил прослушивание.
    pub human_attestation_created: bool,
}

/// Явный перенос выбранных **исходных** байтов в канонический
/// `pronunciation_audio` через штатный импорт, CAS и техническую проверку.
/// Производные фрагменты домен пока не принимает: их свидетельство обработки
/// (извлечение, улучшение речи) не представимо в `audio_derivative`.
pub fn promote(
    store: &AssetStore,
    batch_id: &str,
    surface: &str,
    reading: &str,
    replace_expected_sha256: Option<String>,
    attest_heard_decision: bool,
) -> Result<PromoteReport, AssetError> {
    let request = Pronunciation::new(surface, reading)?;
    let mut runtime = open(store.root(), batch_id)?;
    let batch = load_existing(&mut runtime)?;
    let decision = batch
        .current_decision(&request.surface, &request.reading)
        .ok_or_else(|| invalid("у цели нет текущего решения"))?
        .clone();
    let index = SelectionIndex::new(&batch);
    let (link, candidate, _) = validate_effective_selection(&index, &batch, &decision, &request)?;
    if candidate.kind != CandidateKind::SourceOriginal || !candidate.technical.usable {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "в pronunciation_audio публикуются только выбранные точные исходные байты; производный фрагмент доступен через selection read",
        ));
    }
    if attest_heard_decision && !decision.authority.human_listened() {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "HumanAttestation требует решения пользователя, который сам заявил прослушивание; агентский или текстовый выбор его не создаёт",
        ));
    }
    let source = batch
        .source(&link.source_sha256)
        .ok_or_else(|| invalid("исходник выбора отсутствует"))?;
    let bytes = runtime.read_blob_with_limit(&source.blob, MAX_AUDIO_BYTES)?;
    let forvo = source
        .forvo_for(&request.surface)
        .map(|(evidence, _)| evidence);
    let asset = match forvo {
        Some(evidence) => crate::pronunciation::import_forvo_bytes(
            store,
            &request,
            &bytes,
            evidence,
            replace_expected_sha256,
        )?,
        None => {
            let name = source
                .origins
                .iter()
                .find_map(|origin| match origin {
                    SourceOrigin::LocalFile { source_name } => Some(source_name.clone()),
                    SourceOrigin::ForvoBatch { .. } => None,
                })
                .unwrap_or_else(|| format!("selection-{}.mp3", &source.sha256[..16]));
            crate::pronunciation::import_bytes(
                store,
                &request,
                &bytes,
                &name,
                replace_expected_sha256,
            )?
        }
    };
    let mut attested = false;
    let asset = if attest_heard_decision {
        let outcome = store.attest(HumanAttestationRequest {
            identity: request.identity(),
            expected_sha256: asset.sha256.clone(),
            decision: HumanDecision::Approve,
            reason: format!(
                "selection:{batch_id}/{}; {}; пользователь заявил прослушивание",
                decision.decision_id,
                decision.authority.kind()
            ),
        })?;
        attested = true;
        outcome.asset
    } else {
        asset
    };
    Ok(PromoteReport {
        asset,
        decision_id: decision.decision_id.clone(),
        authority: decision.authority.clone(),
        human_attestation_created: attested,
    })
}

/// Явная политика потребителя для чтения выбранных кандидатов. Она не
/// заменяет `read_verified_with_policy`: строгий потребитель человеческого
/// `Verified` агентских решений не видит.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerTrustPolicy {
    /// Только решения пользователя (через агента или в обзоре).
    HumanDecision,
    /// Плюс агент с поддерживающей локальной аудиоэкспертизой.
    HumanOrAudioModel,
    /// Плюс детерминированная автоматика и агент по свидетельствам. Риск:
    /// фонетика может быть подтверждена только распознавателем.
    AgentSelected,
}

impl ConsumerTrustPolicy {
    fn accepts(self, authority: &DecisionAuthority) -> bool {
        match authority {
            DecisionAuthority::UserViaAgent { .. } | DecisionAuthority::UserDirectReview { .. } => {
                true
            }
            DecisionAuthority::AgentAudioModel { .. } => self != Self::HumanDecision,
            DecisionAuthority::DeterministicAutomation { .. }
            | DecisionAuthority::AgentEvidence { .. } => self == Self::AgentSelected,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectedAudioInfo {
    pub surface: String,
    pub reading: String,
    pub consumer_filename: String,
    pub candidate_id: String,
    pub kind: CandidateKind,
    pub sha256: String,
    pub decision_id: String,
    pub authority: DecisionAuthority,
    pub policy: ConsumerTrustPolicy,
    pub proof: PhoneticProof,
}

#[derive(Debug, Serialize)]
pub struct SelectedAudio {
    #[serde(flatten)]
    pub info: SelectedAudioInfo,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// Проверенный выбор без прочитанных байтов: всё, что подтверждает решение,
/// плюс blob, который читается непосредственно перед потреблением.
struct CheckedSelection {
    consumer_filename: String,
    surface: String,
    reading: String,
    candidate_id: String,
    kind: CandidateKind,
    sha256: String,
    blob: crate::batch_runtime::RuntimeBlobRef,
    decision_id: String,
    authority: DecisionAuthority,
    proof: PhoneticProof,
}

impl CheckedSelection {
    fn into_info(self, policy: ConsumerTrustPolicy) -> SelectedAudioInfo {
        SelectedAudioInfo {
            surface: self.surface,
            reading: self.reading,
            consumer_filename: self.consumer_filename,
            candidate_id: self.candidate_id,
            kind: self.kind,
            sha256: self.sha256,
            decision_id: self.decision_id,
            authority: self.authority,
            policy,
            proof: self.proof,
        }
    }
}

fn open_for_consumption(
    store_root: &Path,
    batch_id: &str,
) -> Result<(crate::batch_runtime::SafeBatchRuntime, SelectionBatch), AssetError> {
    crate::batch_runtime::validate_batch_id(batch_id)?;
    let state = store_root
        .join(".runtime")
        .join("batches")
        .join(batch_id)
        .join("state.json");
    if std::fs::symlink_metadata(&state).is_err() {
        return Err(invalid("пакет выбора не найден"));
    }
    let mut runtime = open(store_root, batch_id)?;
    let batch = load_existing(&mut runtime)?;
    Ok((runtime, batch))
}

/// Проверяет весь набор без чтения blob: отсутствие решения, интервал без
/// байтов, недостаточные полномочия, понижение класса после новых свидетельств
/// и конфликт имён потребителя отклоняют весь ответ.
fn check_selected(
    index: &SelectionIndex,
    batch: &SelectionBatch,
    requests: &[Pronunciation],
    policy: ConsumerTrustPolicy,
) -> Result<Vec<CheckedSelection>, AssetError> {
    let mut names = BTreeMap::new();
    let mut checked = Vec::with_capacity(requests.len());
    for request in requests {
        let request = Pronunciation::new(&request.surface, &request.reading)?;
        let name_key = PronunciationDomainPolicy.consumer_name_key(&request.consumer_filename());
        if let Some(previous) = names.insert(name_key, request.identity())
            && previous != request.identity()
        {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "разные произношения с одинаковым именем потребителя в одном наборе",
            ));
        }
        let decision = batch
            .current_decision(&request.surface, &request.reading)
            .ok_or_else(|| {
                invalid(format!(
                    "{}/{}: решения нет",
                    request.surface, request.reading
                ))
            })?;
        if !policy.accepts(&decision.authority) {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                format!(
                    "{}/{}: полномочия решения ({}) не разрешены политикой потребителя",
                    request.surface,
                    request.reading,
                    decision.authority.kind()
                ),
            ));
        }
        let (_link, candidate, assessment) =
            validate_effective_selection(index, batch, decision, &request)?;
        let audio = candidate.audio.as_ref().ok_or_else(|| {
            invalid("выбран интервал без извлечённых байтов: потребление требует извлечения")
        })?;
        checked.push(CheckedSelection {
            consumer_filename: request.consumer_filename(),
            surface: request.surface,
            reading: request.reading,
            candidate_id: candidate.candidate_id.clone(),
            kind: candidate.kind,
            sha256: audio.sha256.clone(),
            blob: audio.blob.clone(),
            decision_id: decision.decision_id.clone(),
            authority: decision.authority.clone(),
            proof: assessment.proof,
        });
    }
    Ok(checked)
}

/// Читает выбранные кандидаты по явной политике. Весь набор проверяется до
/// возврата, а байты каждого blob читаются по одному и сверяются `probe_mp3`;
/// байты возвращаются вызывающему, которому они нужны в памяти.
pub fn read_selected(
    store_root: &Path,
    batch_id: &str,
    requests: &[Pronunciation],
    policy: ConsumerTrustPolicy,
) -> Result<Vec<SelectedAudio>, AssetError> {
    let (runtime, batch) = open_for_consumption(store_root, batch_id)?;
    let index = SelectionIndex::new(&batch);
    let checked = check_selected(&index, &batch, requests, policy)?;
    let mut out = Vec::with_capacity(checked.len());
    for item in checked {
        let bytes = runtime.read_blob_with_limit(&item.blob, MAX_AUDIO_BYTES)?;
        probe_mp3(&bytes)?;
        out.push(SelectedAudio {
            info: item.into_info(policy),
            bytes,
        });
    }
    Ok(out)
}

/// Те же проверки и сверка байтов, но наружу отдаются только метаданные:
/// одновременно в памяти находится не больше одного blob, поэтому память не
/// зависит от размера набора запросов.
pub fn read_selected_info(
    store_root: &Path,
    batch_id: &str,
    requests: &[Pronunciation],
    policy: ConsumerTrustPolicy,
) -> Result<Vec<SelectedAudioInfo>, AssetError> {
    let (runtime, batch) = open_for_consumption(store_root, batch_id)?;
    let index = SelectionIndex::new(&batch);
    let checked = check_selected(&index, &batch, requests, policy)?;
    let mut out = Vec::with_capacity(checked.len());
    for item in checked {
        let bytes = runtime.read_blob_with_limit(&item.blob, MAX_AUDIO_BYTES)?;
        probe_mp3(&bytes)?;
        out.push(item.into_info(policy));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Явный экспорт выбранных байтов
// ---------------------------------------------------------------------------

/// Записанный файл выбранного кандидата: имя потребителя и подтверждённый SHA.
#[derive(Debug, Serialize)]
pub struct ExportedAudio {
    pub consumer_filename: String,
    pub sha256: String,
    /// `true` — файл создан этим вызовом; `false` — уже существовал с теми же байтами.
    pub written: bool,
}

/// Явный экспорт выбранных байтов в каталог потребителя. Запись сюда — само
/// разрешение потребить байты, поэтому отдельная команда, а не побочный эффект
/// просмотра метаданных.
///
/// Проверки те же, что у [`read_selected`]: явная [`ConsumerTrustPolicy`],
/// действующее решение, точная пара и SHA, техническая пригодность, целость
/// blob. Затем имя потребителя проверяется как одноуровневое безопасное имя,
/// файл пишется атомарно, а его байты на диске сверяются по SHA-256 и снова
/// декодируются. Существующий файл с другим содержимым не заменяется, symlink
/// и не-файл по пути вывода отвергаются. Каталог вывода обязан быть вне
/// `decks/**`: эту границу проверяет вызывающий до вызова.
pub fn export_selected(
    store_root: &Path,
    batch_id: &str,
    requests: &[Pronunciation],
    policy: ConsumerTrustPolicy,
    directory: &Path,
) -> Result<Vec<ExportedAudio>, AssetError> {
    let (runtime, batch) = open_for_consumption(store_root, batch_id)?;
    let index = SelectionIndex::new(&batch);
    let checked = check_selected(&index, &batch, requests, policy)?;
    prepare_export_directory(directory)?;
    let mut exported = Vec::with_capacity(checked.len());
    for item in checked {
        validate_safe_consumer_filename(&item.consumer_filename, DetectedFormat::Mp3)?;
        // Байты читаются непосредственно перед записью своего файла: набор не
        // удерживается в памяти целиком.
        let bytes = runtime.read_blob_with_limit(&item.blob, MAX_AUDIO_BYTES)?;
        probe_mp3(&bytes)?;
        let written = persist_selected_file(directory, &item, &bytes)?;
        exported.push(ExportedAudio {
            consumer_filename: item.consumer_filename,
            sha256: item.sha256,
            written,
        });
    }
    Ok(exported)
}

fn prepare_export_directory(directory: &Path) -> Result<(), AssetError> {
    if directory.is_empty()
        || directory.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
                && !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Prefix(_)
                )
        })
    {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "каталог вывода должен быть обычным путём без . и ..",
        ));
    }
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AssetError::new(
            ErrorCode::PathTraversal,
            "каталог вывода не может быть symlink",
        )),
        Ok(metadata) if !metadata.is_dir() => Err(AssetError::new(
            ErrorCode::PathTraversal,
            "по пути вывода уже есть не каталог",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(directory)
                .map_err(|error| AssetError::io("каталог вывода", error))
        }
        Err(error) => Err(AssetError::io("каталог вывода", error)),
    }
}

fn persist_selected_file(
    directory: &Path,
    item: &CheckedSelection,
    bytes: &[u8],
) -> Result<bool, AssetError> {
    let target = directory.join(&item.consumer_filename);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AssetError::new(
                ErrorCode::PathTraversal,
                "symlink вместо файла вывода запрещён",
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(AssetError::new(
                ErrorCode::PathTraversal,
                "по пути вывода не обычный файл",
            ));
        }
        Ok(_) => {
            let existing =
                std::fs::read(&target).map_err(|error| AssetError::io("файл вывода", error))?;
            if sha256_hex(&existing) == item.sha256 {
                return Ok(false);
            }
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "в каталоге вывода уже есть другой файл с этим именем",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(AssetError::io("файл вывода", error)),
    }
    let suffix: String = item.sha256.chars().take(16).collect();
    let temporary = directory.join(format!(".tmp-export-{}-{suffix}", std::process::id()));
    if let Err(error) = write_export_temporary(&temporary, bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    // `hard_link` не заменяет существующий файл: гонка двух экспортов не
    // приводит к тихой подмене байтов.
    let linked = publish_export_file(&temporary, &target);
    let _ = std::fs::remove_file(&temporary);
    linked.map_err(|error| AssetError::io("запись файла вывода", error))?;
    let written = std::fs::read(&target).map_err(|error| AssetError::io("файл вывода", error))?;
    if sha256_hex(&written) != item.sha256 {
        let _ = std::fs::remove_file(&target);
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "записанные байты не совпали с SHA-256 выбранного кандидата",
        ));
    }
    probe_mp3(&written)?;
    Ok(true)
}

fn write_export_temporary(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| AssetError::io("временный файл вывода", error))?;
    file.write_all(bytes)
        .map_err(|error| AssetError::io("временный файл вывода", error))?;
    file.sync_all()
        .map_err(|error| AssetError::io("временный файл вывода", error))
}

/// Публикует подготовленный файл под именем потребителя, не заменяя чужой
/// файл. Основной путь — жёсткая ссылка (атомарна и не перезаписывает);
/// на файловых системах без жёстких ссылок Windows используется переименование
/// с повторной проверкой отсутствия цели.
fn publish_export_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(temporary, target) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(_) if std::fs::symlink_metadata(target).is_err() => std::fs::rename(temporary, target),
        Err(error) => Err(error),
    }
}
