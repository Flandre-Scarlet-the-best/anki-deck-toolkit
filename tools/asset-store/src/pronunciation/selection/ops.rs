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

/// Возвращает `true`, если состояние пакета изменилось: повторный приём тех же
/// байтов с тем же происхождением не создаёт нового состояния.
fn add_source(
    runtime: &SafeBatchRuntime,
    batch: &mut SelectionBatch,
    bytes: &[u8],
    origin: SourceOrigin,
    surface_hint: Option<&str>,
    report: &mut IntakeReport,
) -> Result<bool, AssetError> {
    let sha = sha256_hex(bytes);
    if let Some(existing) = batch.sources.iter_mut().find(|s| s.sha256 == sha) {
        let mut changed = false;
        if !existing.origins.contains(&origin) {
            existing.origins.push(origin);
            changed = true;
        }
        match (&existing.surface_hint, surface_hint) {
            (None, Some(hint)) => {
                existing.surface_hint = Some(hint.into());
                changed = true;
            }
            (Some(current), Some(hint)) if current != hint => {
                report.surface_hint_conflicts.push(sha.clone());
            }
            _ => {}
        }
        report.already_present.push(sha);
        return Ok(changed);
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
    Ok(true)
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
    let stored = runtime.load::<SelectionBatch>()?;
    // Новый пакет сохраняется всегда; существующий — только при изменении,
    // иначе повторный приём на тот же каталог обесценивал бы готовый обзор.
    let existed = stored.is_some();
    let mut changed = false;
    let mut batch = stored.unwrap_or_else(|| SelectionBatch::new(batch_id));
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
        match add_source(&runtime, &mut batch, &bytes, origin, hint, &mut report) {
            Ok(true) => changed = true,
            Ok(false) => {}
            Err(error) if error.code == ErrorCode::InvalidTransition => return Err(error),
            Err(error) => report.rejected.push(IntakeRejection {
                source_name,
                code: error.code.as_str().into(),
                message: error.message,
            }),
        }
    }
    if changed || !existed {
        save(&mut runtime, &mut batch)?;
    }
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

/// Принимает исходники Forvo-пакета того же хранилища: и запись прежнего
/// пути с парой, и все записи полного сбора по написанию. Чтение плана
/// прежнего пути сохраняется только как подсказка; у полного сбора чтения нет.
/// Одинаковые байты разных записей становятся одним исходником с несколькими
/// происхождениями (ID записи, диктор, свидетельство каждой сохраняются).
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
        let surface = item.surface().to_owned();
        let plan_reading = item.request.as_ref().map(|r| r.reading.clone());
        for acquisition in item.acquisition.iter().chain(&item.collected) {
            acquisition
                .evidence
                .validate(&surface, &acquisition.blob.sha256)?;
            acquisitions.push((
                surface.clone(),
                plan_reading.clone(),
                acquisition.evidence.clone(),
                acquisition.blob.clone(),
            ));
        }
    }
    let mut runtime = open(store.root(), batch_id)?;
    let stored = runtime.load::<SelectionBatch>()?;
    let existed = stored.is_some();
    let mut changed = false;
    let mut batch = stored.unwrap_or_else(|| SelectionBatch::new(batch_id));
    let mut report = IntakeReport::default();
    for (surface, plan_reading, evidence, blob) in acquisitions {
        let bytes = forvo_runtime.read_blob_with_limit(&blob, MAX_AUDIO_BYTES)?;
        let record_id = evidence.selected_record_id.clone();
        let origin = SourceOrigin::ForvoBatch {
            batch_id: forvo_batch_id.into(),
            evidence,
            plan_reading,
            surface: Some(surface.clone()),
        };
        match add_source(
            &runtime,
            &mut batch,
            &bytes,
            origin,
            Some(&surface),
            &mut report,
        ) {
            Ok(true) => changed = true,
            Ok(false) => {}
            Err(error) if error.code == ErrorCode::InvalidTransition => return Err(error),
            Err(error) => report.rejected.push(IntakeRejection {
                source_name: format!("forvo:{surface}:{record_id}"),
                code: error.code.as_str().into(),
                message: error.message,
            }),
        }
    }
    if changed || !existed {
        save(&mut runtime, &mut batch)?;
    }
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
pub(crate) fn candidate_id(
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
    // Повторная связь с теми же параметрами не меняет состояние: иначе
    // новый номер ревизии обесценивал бы уже готовый обзор.
    let changed;
    let link = match batch.links.iter_mut().find(|l| l.link_id == id) {
        Some(existing) => {
            let before = existing.clone();
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
            changed = *existing != before;
            existing.clone()
        }
        None => {
            if batch.links.len() >= MAX_LINKS {
                return Err(invalid(format!(
                    "пакет выбора ограничен {MAX_LINKS} связями"
                )));
            }
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
                    derivation: None,
                    technical: TechnicalGate {
                        usable: true,
                        code: "source_probe_ok".into(),
                    },
                }],
            };
            changed = true;
            batch.links.push(link.clone());
            link
        }
    };
    if changed {
        save(&mut runtime, &mut batch)?;
    }
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
    /// Отказы отдельных исходников при `--intake-missing`: остальные результаты
    /// обрабатываются, а не отменяются целиком.
    pub sources_rejected: Vec<IntakeRejection>,
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
        derivation: None,
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
            derivation: None,
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
        let technical = output_gate(kind, span.as_ref(), passed);
        candidates.push(PronunciationCandidate {
            candidate_id: candidate_id(&id, kind, &audio.sha256, span.as_ref()),
            kind,
            audio: Some(audio),
            span,
            occurrence: result
                .selected_occurrence
                .filter(|i| occurrences.iter().any(|o| o.index == *i)),
            derivation: None,
            technical,
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
                derivation: None,
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

/// Попытка выйти за защищённую область — ошибка операции, а не исход файла.
/// Технический гейт итогового кандидата распознавателя. Фрагмент без границ
/// не доказывает свою производную и не может быть продвинут, поэтому он
/// отклоняется до выбора, а не после него; исправляет это только пересчёт
/// распознавания.
pub(crate) fn output_gate(
    kind: CandidateKind,
    span: Option<&CandidateSpan>,
    post_validation_passed: bool,
) -> TechnicalGate {
    let code = if kind == CandidateKind::ExtractedFragment && span.is_none() {
        "fragment_span_missing"
    } else if post_validation_passed {
        "post_validation_passed"
    } else {
        "post_validation_failed"
    };
    TechnicalGate {
        usable: code == "post_validation_passed",
        code: code.into(),
    }
}

/// Материализованные кандидаты прежней связи, интервал которых остался тем же
/// кандидатом (те же связь, исходник и границы): их байты не теряются при
/// повторном связывании. Идентификатор интервала не зависит от номера
/// вхождения, а новый пакет распознавания может пронумеровать вхождения иначе,
/// поэтому номер берётся из нового интервала.
pub(crate) fn carried_materialized(
    previous: &[PronunciationCandidate],
    fresh: &[PronunciationCandidate],
) -> Vec<PronunciationCandidate> {
    previous
        .iter()
        .filter_map(|candidate| {
            let derivation = candidate.derivation.as_ref()?;
            let interval = fresh
                .iter()
                .find(|n| n.candidate_id == derivation.from_candidate_id)?;
            let mut kept = candidate.clone();
            kept.occurrence = interval.occurrence;
            Some(kept)
        })
        .collect()
}

pub(crate) fn is_boundary_refusal(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::BoundaryViolation | ErrorCode::PathTraversal
    )
}

/// Отказ, относящийся ко всему пакету, а не к одному файлу: граница, предел
/// пакета, целостность хранилища и его отсутствие.
pub(crate) fn aborts_linking(code: ErrorCode) -> bool {
    is_boundary_refusal(code)
        || matches!(
            code,
            ErrorCode::InvalidTransition | ErrorCode::IntegrityMismatch | ErrorCode::StoreMissing
        )
}

/// Принимает исходник входа распознавания, которого ещё нет в пакете.
///
/// Ошибка границы и расхождение SHA прерывают операцию; отказ отдельного файла
/// (чтение, проверка MP3) попадает в `sources_rejected`, а остальные результаты
/// продолжают обрабатываться. Возвращает `true`, если состояние изменилось.
pub(crate) fn accept_missing_source(
    runtime: &SafeBatchRuntime,
    batch: &mut SelectionBatch,
    input_root: &str,
    path: &str,
    sha256: &str,
    read: &mut SourceReader<'_>,
    report: &mut LinkReport,
) -> Result<bool, AssetError> {
    let source_name = valid_source_name(path).unwrap_or_else(|| "source.mp3".to_owned());
    let bytes = match read(&Path::new(input_root).join(path)) {
        Ok(bytes) if sha256_hex(&bytes) == sha256 => bytes,
        Ok(_) => {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "исходник входа распознавания изменился после инвентаризации",
            ));
        }
        Err(error) if is_boundary_refusal(error.code) => return Err(error),
        Err(error) => {
            report.sources_rejected.push(IntakeRejection {
                source_name,
                code: error.code.as_str().into(),
                message: error.message,
            });
            return Ok(false);
        }
    };
    // Имя проверяется так же, как при обычном приёме: иначе негодное имя
    // отвергло бы сохранение всего пакета вместо одного исходника.
    let mut intake = IntakeReport::default();
    match add_source(
        runtime,
        batch,
        &bytes,
        SourceOrigin::LocalFile {
            source_name: valid_source_name(path).unwrap_or_else(|| "source.mp3".to_owned()),
        },
        None,
        &mut intake,
    ) {
        Ok(added) => {
            report.sources_intaken += intake.added.len();
            Ok(added)
        }
        Err(error) if aborts_linking(error.code) => Err(error),
        Err(error) => {
            report.sources_rejected.push(IntakeRejection {
                source_name,
                code: error.code.as_str().into(),
                message: error.message,
            });
            Ok(false)
        }
    }
}

/// Одноуровневое имя файла из пути входа; `None`, если оно не годится.
fn valid_source_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_text(name, 255) && !name.contains(['/', '\\']))
        .map(str::to_owned)
}

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
    let mut rechanged = false;
    for (item, result) in &results {
        let WorkItem { sha256, path, .. } = item;
        if batch.source(sha256).is_none() {
            match read_source.as_mut() {
                Some(read) => {
                    rechanged |= accept_missing_source(
                        &runtime,
                        &mut batch,
                        &recognizer.input_root,
                        path,
                        sha256,
                        read,
                        &mut report,
                    )?;
                    // Отказ отдельного файла: исходника в пакете нет, поэтому
                    // этот результат не связывается, а остальные обрабатываются.
                    if batch.source(sha256).is_none() {
                        continue;
                    }
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
            if let Some(source) = batch.sources.iter_mut().find(|s| s.sha256 == *sha256)
                && source.free_analysis != Some(analysis.clone())
            {
                source.free_analysis = Some(analysis);
                rechanged = true;
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
                let kept = carried_materialized(&existing.candidates, &link.candidates);
                link.candidates.extend(kept);
                if *existing != link {
                    rechanged = true;
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
                rechanged = true;
                report.links_created += 1;
            }
        }
    }
    report.missing_sources = missing.into_iter().collect();
    // Повторный прогон того же пакета распознавания не меняет состояние и не
    // обесценивает готовый обзор своим новым номером ревизии.
    if rechanged {
        save(&mut runtime, &mut batch)?;
    }
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
        let ready = selected
            && decision
                .and_then(|d| selection_readiness(index, batch, d))
                .is_some_and(|r| r.is_ready());
        let deferred = decision.is_some_and(|d| matches!(d.action, DecisionAction::Defer));
        match self {
            Self::All => true,
            Self::Undecided => !selected,
            // Выбранная, но ещё не готовая к выдаче цель (интервал без
            // проверенного MP3, отказ материализации, отозванная готовность)
            // остаётся нерешённой.
            Self::Unresolved => {
                !ready
                    && (selected
                        || deferred
                        || !batch.automatically_resolvable(
                            recommendation.class,
                            recommendation
                                .reasons
                                .contains(&ReasonCode::AudioReviewSupports),
                        ))
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
    /// Решение действительно (точный кандидат, полномочия, режим пакета).
    pub selection_effective: bool,
    /// Готовность к выдаче: существует ли проверенный MP3 этой цели.
    pub readiness: Option<Readiness>,
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
    /// Действующий режим проверки пакета (`null` — не закреплён).
    pub review_mode: Option<&'a ReviewModeGrant>,
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
    index: &SelectionIndex<'a>,
    batch: &'a SelectionBatch,
    recommendation: &'a GroupRecommendation,
) -> GroupView<'a> {
    let candidates = recommendation
        .candidates
        .iter()
        .filter_map(|assessment| {
            let (link, candidate) = index.find_candidate(&assessment.candidate_id)?;
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
        readiness: batch
            .current_decision(&recommendation.surface, &recommendation.reading)
            .and_then(|d| selection_readiness(index, batch, d)),
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
                    match selection_readiness(&index, &batch, decision) {
                        Some(readiness) if readiness.is_ready() => {
                            *decided.entry(decision.authority.kind()).or_insert(0) += 1;
                            "selected"
                        }
                        Some(readiness) => readiness.disposition(),
                        None => "selection_expired",
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
        review_mode: batch.review_mode(),
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
    /// Выбранные, но не готовые к выдаче цели по состоянию готовности
    /// (например, интервал ждёт `selection materialize`).
    pub not_ready: BTreeMap<&'static str, Vec<(String, String)>>,
    /// Действующий режим проверки, по которому принимались решения.
    pub review_mode: Option<ReviewMode>,
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
        if let Some(current) = batch.current_decision(surface, reading).filter(|d| {
            matches!(d.action, DecisionAction::Defer) || decision_is_effective(&index, batch, d)
        }) {
            report.already_decided += 1;
            if let Some(readiness) =
                selection_readiness(&index, batch, current).filter(|r| !r.is_ready())
            {
                report
                    .not_ready
                    .entry(readiness.disposition())
                    .or_default()
                    .push((surface.clone(), reading.clone()));
            }
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
                let (link, candidate) = index
                    .find_candidate(&best.candidate_id)
                    .expect("рекомендация ссылается на кандидата пакета");
                let review = best
                    .reasons
                    .contains(&ReasonCode::AudioReviewSupports)
                    .then(|| index.supporting_review(link, candidate))
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
        // Режим проверки пакета ограничивает полномочия: решение, которого
        // он не разрешает, остаётся ожидающим, а не принимается молча.
        let authority = authority.filter(|a| {
            let permitted = batch.mode_permits(a);
            if !permitted {
                match a {
                    DecisionAuthority::DeterministicAutomation { .. } => {
                        report.selected_deterministic -= 1
                    }
                    DecisionAuthority::AgentEvidence { .. } => report.selected_agent_evidence -= 1,
                    DecisionAuthority::AgentAudioModel { .. } => report.selected_agent_audio -= 1,
                    _ => {}
                }
            }
            permitted
        });
        match (authority, best) {
            (Some(authority), Some(best)) => {
                let (link, candidate) = index
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
    // Делегированный режим сам называет агента; явный `--agent` обязан с ним
    // совпадать, а в ручном режиме агентских решений нет вовсе.
    let delegated;
    let agent = match (batch.review_mode(), agent) {
        (Some(grant), Some(policy)) if grant.mode == ReviewMode::ManualUser => {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                format!(
                    "пакет в ручном режиме проверки: агент {} не принимает решений; используйте review-export",
                    policy.agent
                ),
            ));
        }
        (Some(grant), Some(policy)) if grant.agent.as_deref() != Some(policy.agent.as_str()) => {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "полномочия пакета делегированы другому агенту",
            ));
        }
        (Some(grant), None) if grant.mode == ReviewMode::DelegatedAgent => {
            delegated = AgentPolicy {
                agent: grant.agent.clone().expect("делегирование называет агента"),
            };
            Some(&delegated)
        }
        (_, agent) => agent,
    };
    let (decisions, mut report) = plan_auto_select(&batch, agent);
    report.review_mode = batch.review_mode().map(|g| g.mode);
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

/// Выбор режима проверки пакета пользователем.
pub struct ReviewModeRequest {
    pub mode: ReviewMode,
    pub agent: Option<String>,
    pub agent_trust: Option<ConsumerTrustPolicy>,
    /// Канал, по которому пользователь выразил выбор (`agent-dialog`).
    pub channel: String,
    /// Точная реплика пользователя: хранится только её SHA-256.
    pub statement: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewModeReport {
    pub grant: ReviewModeGrant,
    pub changed: bool,
    pub revision: u64,
    /// Агентские решения, которые перестали действовать в новом режиме.
    pub agent_decisions_suspended: usize,
    /// Краткие следующие шаги для агента.
    pub next_steps: Vec<String>,
}

/// Фиксирует выбранный пользователем режим проверки пакета. Режим проверяется
/// при каждом решении, а также при чтении, экспорте и публикации: смена
/// режима сразу приостанавливает агентские решения, которых он не разрешает
/// (журнал решений не переписывается).
pub fn set_review_mode(
    store: &AssetStore,
    batch_id: &str,
    request: ReviewModeRequest,
) -> Result<ReviewModeReport, AssetError> {
    if request.statement.trim().is_empty() || request.statement.len() > 4096 {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "нужна точная реплика пользователя (1–4096 байт); хранится только её SHA-256",
        ));
    }
    let mut runtime = open(store.root(), batch_id)?;
    let mut batch = load_existing(&mut runtime)?;
    let mut grant = ReviewModeGrant {
        mode: request.mode,
        agent: request.agent,
        agent_trust: request.agent_trust,
        channel: request.channel,
        statement_sha256: sha256_hex(request.statement.as_bytes()),
        state_revision: batch.revision + 1,
        recorded_at_unix_ms: recorded_at(std::time::SystemTime::now())?,
    };
    let same = batch.review_mode().is_some_and(|current| {
        current.mode == grant.mode
            && current.agent == grant.agent
            && current.agent_trust == grant.agent_trust
            && current.statement_sha256 == grant.statement_sha256
    });
    let index = SelectionIndex::new(&batch);
    let effective_before: Vec<(String, String)> = batch
        .identities()
        .into_iter()
        .filter(|(s, r)| {
            batch
                .current_decision(s, r)
                .is_some_and(|d| !d.authority.is_user() && decision_is_effective(&index, &batch, d))
        })
        .collect();
    let changed = !same;
    if changed {
        let mut probe = batch.clone();
        probe.review_modes.push(grant.clone());
        probe.revision += 1;
        probe.validate().map_err(|_| {
            AssetError::new(
                ErrorCode::InvalidIdentity,
                "delegated_agent требует --agent и --agent-trust human-or-audio-model|agent-selected; manual_user — без них",
            )
        })?;
        batch.review_modes.push(grant.clone());
        save(&mut runtime, &mut batch)?;
    } else {
        grant = batch.review_mode().expect("режим совпал").clone();
    }
    let index = SelectionIndex::new(&batch);
    let suspended = effective_before
        .iter()
        .filter(|(s, r)| {
            batch
                .current_decision(s, r)
                .is_some_and(|d| !decision_is_effective(&index, &batch, d))
        })
        .count();
    let next_steps = match grant.mode {
        ReviewMode::DelegatedAgent => vec![
            format!(
                "selection auto-select --batch-id {batch_id}: агент {} принимает разрешённые решения без вопроса по каждому кандидату",
                grant.agent.as_deref().unwrap_or_default()
            ),
            format!("selection materialize --batch-id {batch_id}: готовые MP3 выбранных интервалов"),
            format!(
                "selection status --batch-id {batch_id} --filter unresolved: остаток с причинами и следующими действиями"
            ),
        ],
        ReviewMode::ManualUser => vec![
            format!(
                "selection review-export --batch-id {batch_id}: локальная страница прослушивания и выбора"
            ),
            "передайте пользователю путь index.html; после выбора он сохранит файл решений и сообщит его путь".into(),
            format!(
                "selection review-apply --batch-id {batch_id} --review-id <id> --decisions-file <путь>: применение только явного выбора"
            ),
        ],
    };
    Ok(ReviewModeReport {
        grant,
        changed,
        revision: batch.revision,
        agent_decisions_suspended: suspended,
        next_steps,
    })
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
    // Экспертиза, на которую ссылается полномочие, проверяется до записи для
    // любого действия: иначе отказ приходит из `validate` как порча состояния.
    if let DecisionAuthority::AgentAudioModel {
        audio_review_id, ..
    } = authority
        && !batch
            .audio_reviews
            .iter()
            .any(|review| &review.review_id == audio_review_id)
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "аудиоэкспертиза с таким идентификатором отсутствует",
        ));
    }
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
    index: &SelectionIndex<'a>,
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
    let (link, candidate) = index
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
    if !batch.mode_permits(&decision.authority) {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            format!(
                "действующий режим проверки пакета не признаёт решение вида {}",
                decision.authority.kind()
            ),
        ));
    }
    let assessment = engine::assess_candidate_with(index, link, candidate);
    if !decision.authority.is_user() {
        validate_agent_authority(batch, link, candidate, &decision.authority, &assessment)?;
    }
    Ok((link, candidate, assessment))
}

/// Готовность выбранной цели к выдаче. Отдельна от действительности решения:
/// адресно выбранный интервал без проверенного MP3 — не готовый результат.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Readiness {
    /// Проверенные байты существуют: `candidate_id` — что именно выдаётся.
    Ready {
        candidate_id: String,
        sha256: String,
    },
    /// Выбран интервал; нужен `selection materialize`.
    PendingMaterialization,
    /// Последняя материализация не удалась; цель не выдаётся.
    MaterializationFailed { code: String },
    /// Байты есть, но решение агента требует поддерживающей аудиоэкспертизы
    /// именно материализованных байтов (`selection audio-review`).
    SemanticCheckRequired { candidate_id: String },
    /// Экспертиза материализованных байтов противоречит цели: готовность отозвана.
    SemanticContradicted { candidate_id: String },
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub(crate) fn disposition(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "selected",
            Self::PendingMaterialization => "selected_pending_materialization",
            Self::MaterializationFailed { .. } => "selected_materialization_failed",
            Self::SemanticCheckRequired { .. } => "selected_semantic_check_required",
            Self::SemanticContradicted { .. } => "selected_semantic_contradicted",
        }
    }

    fn refusal(&self, surface: &str, reading: &str) -> AssetError {
        let (code, text) = match self {
            Self::Ready { .. } => ("ready", "готово"),
            Self::PendingMaterialization => (
                "interval_not_materialized",
                "выбран интервал без извлечённого MP3: выполните selection materialize",
            ),
            Self::MaterializationFailed { .. } => (
                "materialization_failed",
                "материализация выбранного интервала не удалась: исходник и журнал сохранены, повторите materialize или выберите другой вариант",
            ),
            Self::SemanticCheckRequired { .. } => (
                "semantic_check_required",
                "решение агента требует поддерживающей аудиоэкспертизы материализованного MP3",
            ),
            Self::SemanticContradicted { .. } => (
                "semantic_contradicted",
                "аудиоэкспертиза материализованного MP3 противоречит цели: готовность отозвана",
            ),
        };
        AssetError::with_details(
            ErrorCode::InvalidTransition,
            format!("{surface}/{reading}: {text}"),
            serde_json::json!({"code": code, "readiness": self}),
        )
    }
}

/// Материализованный кандидат выбранного интервала: технически пригодные
/// байты той же связи и тех же границ.
pub(crate) fn materialized_of<'a>(
    link: &'a TargetLink,
    interval: &PronunciationCandidate,
) -> Option<&'a PronunciationCandidate> {
    link.candidates.iter().rev().find(|c| {
        c.kind == CandidateKind::MaterializedInterval
            && c.technical.usable
            && c.audio.is_some()
            && c.span == interval.span
            && c.derivation
                .as_ref()
                .is_some_and(|d| d.from_candidate_id == interval.candidate_id)
    })
}

/// Готовность действующего решения (`None`, если решение не действует или
/// это не выбор кандидата).
pub(crate) fn selection_readiness(
    index: &SelectionIndex,
    batch: &SelectionBatch,
    decision: &SelectionDecision,
) -> Option<Readiness> {
    let request = Pronunciation::new(&decision.surface, &decision.reading).ok()?;
    let (link, candidate, _) =
        validate_effective_selection(index, batch, decision, &request).ok()?;
    Some(readiness_of(batch, link, candidate, &decision.authority).0)
}

fn readiness_of<'a>(
    batch: &SelectionBatch,
    link: &'a TargetLink,
    candidate: &'a PronunciationCandidate,
    authority: &DecisionAuthority,
) -> (Readiness, Option<&'a PronunciationCandidate>) {
    if let Some(audio) = &candidate.audio {
        return (
            Readiness::Ready {
                candidate_id: candidate.candidate_id.clone(),
                sha256: audio.sha256.clone(),
            },
            Some(candidate),
        );
    }
    let Some(materialized) = materialized_of(link, candidate) else {
        let failed = batch
            .materialization_of(&candidate.candidate_id)
            .filter(|m| m.link_generation == link.generation && Some(m.span) == candidate.span)
            .and_then(|m| match &m.outcome {
                MaterializationOutcome::Failed { code, .. } => Some(code.clone()),
                MaterializationOutcome::Materialized { .. } => None,
            });
        return (
            match failed {
                Some(code) => Readiness::MaterializationFailed { code },
                None => Readiness::PendingMaterialization,
            },
            None,
        );
    };
    let audio = materialized.audio.as_ref().expect("материализованный MP3");
    let latest = batch.audio_reviews.iter().rev().find(|r| {
        r.candidate_id == materialized.candidate_id
            && r.audio_sha256 == audio.sha256
            && r.surface == link.surface
            && r.reading == link.reading
            && r.link_generation == link.generation
            && trustworthy_audio_review(r)
    });
    let id = materialized.candidate_id.clone();
    match latest.map(|r| r.verdict) {
        Some(AudioVerdict::Contradicts) => {
            (Readiness::SemanticContradicted { candidate_id: id }, None)
        }
        Some(AudioVerdict::Supports) => (
            Readiness::Ready {
                candidate_id: id,
                sha256: audio.sha256.clone(),
            },
            Some(materialized),
        ),
        // Пользователь сам выбрал интервал: детерминированный срез его же
        // границ готов после технической проверки. Агентскому решению нужна
        // поддерживающая экспертиза именно полученных байтов.
        _ if authority.is_user() => (
            Readiness::Ready {
                candidate_id: id,
                sha256: audio.sha256.clone(),
            },
            Some(materialized),
        ),
        _ => (Readiness::SemanticCheckRequired { candidate_id: id }, None),
    }
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
    if !batch.mode_permits(&request.authority) {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            format!(
                "режим проверки пакета не разрешает решение вида {}",
                request.authority.kind()
            ),
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

pub use super::model::ConsumerTrustPolicy;

#[derive(Debug, Serialize)]
pub struct PromoteReport {
    pub asset: AssetRecord,
    pub decision_id: String,
    pub authority: DecisionAuthority,
    /// Политика, по которой разрешена публикация решения.
    pub policy: ConsumerTrustPolicy,
    /// Что опубликовано: точные исходные байты или производный MP3.
    pub kind: CandidateKind,
    pub candidate_id: String,
    /// `HumanAttestation` создаётся только по явному флагу и только для
    /// решения пользователя, который сам заявил прослушивание.
    pub human_attestation_created: bool,
}

/// Явная публикация выбранного готового MP3 в канонический
/// `pronunciation_audio` через штатный импорт, CAS и техническую проверку.
///
/// Публикуются точные исходные байты (`source_original`) и технически готовые
/// производные (извлечённый или материализованный фрагмент, обработанная
/// запись) — последние со свидетельством `selection_derivative`: SHA
/// исходника, границы, цепочка обработки, отпечатки конфигурации и решение.
/// Полномочия решения проверяются явной [`ConsumerTrustPolicy`]; публикация
/// агентского решения не делает запись `Verified` и не означает прослушивания.
pub fn promote(
    store: &AssetStore,
    batch_id: &str,
    surface: &str,
    reading: &str,
    replace_expected_sha256: Option<String>,
    attest_heard_decision: bool,
    policy: ConsumerTrustPolicy,
) -> Result<PromoteReport, AssetError> {
    let request = Pronunciation::new(surface, reading)?;
    let mut runtime = open(store.root(), batch_id)?;
    let batch = load_existing(&mut runtime)?;
    let index = SelectionIndex::new(&batch);
    let item = check_selected(&index, &batch, std::slice::from_ref(&request), policy)?
        .pop()
        .expect("один запрос — одна проверенная цель");
    let decision = batch
        .current_decision(&request.surface, &request.reading)
        .expect("проверенная цель имеет решение")
        .clone();
    if attest_heard_decision
        && (!decision.authority.human_listened() || item.candidate_id != item.decision_candidate_id)
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            if decision.authority.human_listened() {
                "пользователь прослушал выбранный интервал исходника, а не полученный из него MP3: HumanAttestation требует прослушивания именно публикуемых байтов"
            } else {
                "HumanAttestation требует решения пользователя, который сам заявил прослушивание; агентский или текстовый выбор его не создаёт"
            },
        ));
    }
    let (link, consumed) = index
        .find_candidate(&item.candidate_id)
        .ok_or_else(|| invalid("выдаваемый кандидат отсутствует"))?;
    let source = batch
        .source(&link.source_sha256)
        .ok_or_else(|| invalid("исходник выбора отсутствует"))?;
    let bytes = runtime.read_blob_with_limit(&item.blob, MAX_AUDIO_BYTES)?;
    if sha256_hex(&bytes) != item.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты выбранного кандидата не совпадают с его SHA",
        ));
    }
    probe_mp3(&bytes)?;
    // Байты, совпадающие с исходником (Balanced выбрал исходный вариант без
    // улучшения), — это сам исходник: производной от них не бывает, и
    // свидетельство производной такую пару отвергло бы.
    let asset = if consumed.kind == CandidateKind::SourceOriginal
        || item.sha256 == link.source_sha256
    {
        match source
            .forvo_for(&request.surface)
            .map(|(evidence, _)| evidence)
        {
            Some(evidence) => crate::pronunciation::import_forvo_bytes(
                store,
                &request,
                &bytes,
                evidence,
                replace_expected_sha256,
            )?,
            None => {
                let name = local_source_name(source);
                crate::pronunciation::import_bytes(
                    store,
                    &request,
                    &bytes,
                    &name,
                    replace_expected_sha256,
                )?
            }
        }
    } else {
        let derivative = selection_derivative(batch_id, &decision, &item, link, consumed, source)?;
        crate::pronunciation::import_selection_derived_bytes(
            store,
            &request,
            &bytes,
            &derivative,
            replace_expected_sha256,
        )?
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
        policy,
        kind: consumed.kind,
        candidate_id: consumed.candidate_id.clone(),
        human_attestation_created: attested,
    })
}

fn local_source_name(source: &AudioSource) -> String {
    source
        .origins
        .iter()
        .find_map(|origin| match origin {
            SourceOrigin::LocalFile { source_name } => Some(source_name.clone()),
            SourceOrigin::ForvoBatch { .. } => None,
        })
        .unwrap_or_else(|| format!("selection-{}.mp3", &source.sha256[..16]))
}

/// Свидетельство производного MP3 для канонического домена: копирует цепочку
/// обработки кандидата, не создавая второго источника истины.
fn selection_derivative(
    batch_id: &str,
    decision: &SelectionDecision,
    item: &CheckedSelection,
    link: &TargetLink,
    consumed: &PronunciationCandidate,
    source: &AudioSource,
) -> Result<crate::pronunciation::derivation::SelectionDerivative, AssetError> {
    use crate::pronunciation::derivation::{SelectionDerivative, SelectionReference};
    let (source_provenance, source_metadata) = match source.forvo_for(&link.surface) {
        Some((evidence, _)) => (
            crate::Provenance {
                source_kind: "forvo_web".into(),
                source_name: format!("forvo-ja-{}.mp3", evidence.selected_record_id),
            },
            serde_json::json!({"surface": link.surface, "forvo": evidence}),
        ),
        None => {
            let name = local_source_name(source);
            (
                crate::Provenance {
                    source_kind: "local_import".into(),
                    source_name: name.clone(),
                },
                serde_json::json!({"source_name": name}),
            )
        }
    };
    let (transform, chain, toolchain, config_sha256, processing) = match &consumed.derivation {
        Some(d) => (
            d.transform.clone(),
            d.chain.clone(),
            d.toolchain.clone(),
            d.config_sha256.clone(),
            None,
        ),
        None => {
            let recognition = link.recognition.as_ref().ok_or_else(|| {
                invalid("у производного кандидата нет свидетельства распознавателя")
            })?;
            let transform = match consumed.kind {
                CandidateKind::ExtractedFragment => "recognizer_target_fragment",
                CandidateKind::ProcessedWhole => "recognizer_processed_whole",
                CandidateKind::ProcessedFullRecord => "balanced_full_record",
                _ => return Err(invalid("вид кандидата не является производным MP3")),
            };
            let details = recognition.quality_details.clone().unwrap_or_default();
            let config = match consumed.kind {
                CandidateKind::ProcessedFullRecord => {
                    details["full_record"]["balanced_config_sha256"]
                        .as_str()
                        .filter(|s| valid_sha(s))
                        .unwrap_or(&recognition.config_sha256)
                        .to_owned()
                }
                _ => recognition.config_sha256.clone(),
            };
            (
                transform.to_owned(),
                vec![
                    format!("recognizer_batch:{}", recognition.recognizer_batch_id),
                    format!("recognizer_outcome:{}", json_word(&recognition.outcome)),
                    format!("candidate_kind:{}", consumed.kind.as_str()),
                ],
                vec![format!(
                    "recognizer_config_sha256:{}",
                    recognition.config_sha256
                )],
                config,
                Some(serde_json::json!({
                    "processing": details["processing"],
                    "full_record": details["full_record"],
                    "post_validation_passed": recognition.post_validation_passed,
                })),
            )
        }
    };
    let derivative = SelectionDerivative {
        schema_version: crate::pronunciation::derivation::SELECTION_DERIVATIVE_SCHEMA_VERSION,
        kind: consumed.kind.as_str().into(),
        transform,
        source_sha256: link.source_sha256.clone(),
        source_provenance,
        source_metadata,
        output_sha256: item.sha256.clone(),
        span: consumed
            .span
            .map(|s| crate::pronunciation::derivation::FrameSpan {
                start_frame: s.start_frame,
                end_frame: s.end_frame,
                sample_rate: s.sample_rate,
            }),
        chain,
        toolchain,
        config_sha256,
        processing,
        selection: SelectionReference {
            batch_id: batch_id.into(),
            decision_id: decision.decision_id.clone(),
            decision_candidate_id: item.decision_candidate_id.clone(),
            candidate_id: consumed.candidate_id.clone(),
            link_generation: link.generation,
            authority: decision.authority.kind().into(),
            proof: json_word(&item.proof),
        },
    };
    Ok(derivative)
}

fn json_word(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectedAudioInfo {
    pub surface: String,
    pub reading: String,
    pub consumer_filename: String,
    /// Выдаваемый кандидат (у выбранного интервала — его материализация).
    pub candidate_id: String,
    /// Кандидат, выбранный решением.
    pub decision_candidate_id: String,
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
    decision_candidate_id: String,
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
            decision_candidate_id: self.decision_candidate_id,
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

/// Проверяет весь набор без чтения blob: отсутствие решения, недостаточные
/// полномочия или режим пакета, понижение класса после новых свидетельств,
/// неготовность выдачи (интервал без проверенного MP3, отказ материализации,
/// отозванная экспертиза) и конфликт имён потребителя отклоняют весь ответ.
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
        let (link, candidate, assessment) =
            validate_effective_selection(index, batch, decision, &request)?;
        let (readiness, consumed) = readiness_of(batch, link, candidate, &decision.authority);
        let Some(consumed) = consumed else {
            return Err(readiness.refusal(&request.surface, &request.reading));
        };
        let audio = consumed
            .audio
            .as_ref()
            .ok_or_else(|| readiness.refusal(&request.surface, &request.reading))?;
        checked.push(CheckedSelection {
            consumer_filename: request.consumer_filename(),
            surface: request.surface,
            reading: request.reading,
            candidate_id: consumed.candidate_id.clone(),
            decision_candidate_id: candidate.candidate_id.clone(),
            kind: consumed.kind,
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
        let bytes = read_checked(&runtime, &item)?;
        out.push(SelectedAudio {
            info: item.into_info(policy),
            bytes,
        });
    }
    Ok(out)
}

/// Байты проверенного выбора: SHA и полная техническая проверка MP3.
fn read_checked(
    runtime: &SafeBatchRuntime,
    item: &CheckedSelection,
) -> Result<Vec<u8>, AssetError> {
    let bytes = runtime.read_blob_with_limit(&item.blob, MAX_AUDIO_BYTES)?;
    if sha256_hex(&bytes) != item.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты выбранного кандидата не совпадают с его SHA",
        ));
    }
    probe_mp3(&bytes)?;
    Ok(bytes)
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
        read_checked(&runtime, &item)?;
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
/// действующее решение, точная пара и SHA, готовность выдачи, целость blob.
/// Затем имя потребителя проверяется как одноуровневое безопасное имя, а файл
/// публикуется с семантикой «создать, только если отсутствует»: существующий
/// или конкурентно созданный чужой файл не заменяется и не удаляется ни при
/// каком исходе. Каталог вывода обязан быть вне `decks/**`: эту границу
/// проверяет вызывающий до вызова.
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
        let bytes = read_checked(&runtime, &item)?;
        let written =
            persist_selected_file(directory, &item.consumer_filename, &item.sha256, &bytes)?;
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

/// Сверяет существующий файл с ожидаемым SHA, не трогая его.
/// `Ok(true)` — те же байты (идемпотентный повтор), `Ok(false)` — пути нет.
fn existing_export_matches(target: &Path, sha256: &str) -> Result<bool, AssetError> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AssetError::new(
            ErrorCode::PathTraversal,
            "symlink вместо файла вывода запрещён",
        )),
        Ok(metadata) if !metadata.is_file() => Err(AssetError::new(
            ErrorCode::PathTraversal,
            "по пути вывода не обычный файл",
        )),
        Ok(_) => {
            let existing =
                std::fs::read(target).map_err(|error| AssetError::io("файл вывода", error))?;
            if sha256_hex(&existing) == sha256 {
                Ok(true)
            } else {
                Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "в каталоге вывода уже есть другой файл с этим именем; он не изменён",
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AssetError::io("файл вывода", error)),
    }
}

/// Публикует байты под именем потребителя без замены чужого объекта.
///
/// Ни один шаг не удаляет и не переименовывает поверх `target`: публикация —
/// либо жёсткая ссылка на полностью записанный временный файл (атомарна и
/// отказывает, если имя занято), либо, где жёстких ссылок нет, создание файла
/// с `create_new` (`O_EXCL` / `CREATE_NEW`) — тоже атомарный отказ при занятом
/// имени. Обратная сверка байтов при несовпадении сообщает об ошибке, но
/// файл не удаляет: его принадлежность уже не доказана. Удаляется только
/// собственный временный файл с уникальным именем.
fn persist_selected_file(
    directory: &Path,
    filename: &str,
    sha256: &str,
    bytes: &[u8],
) -> Result<bool, AssetError> {
    let target = directory.join(filename);
    if existing_export_matches(&target, sha256)? {
        return Ok(false);
    }
    let temporary = unique_temporary(directory, sha256);
    if let Err(error) = write_export_temporary(&temporary, bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(test)]
    BEFORE_PUBLISH.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(&target);
        }
    });
    let published = publish_no_replace(&temporary, &target, bytes);
    let _ = std::fs::remove_file(&temporary);
    match published {
        Ok(()) => {}
        // Имя занято конкурентом между проверкой и публикацией: тот же SHA —
        // идемпотентный успех, иначе отказ; чужой файл не тронут.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return if existing_export_matches(&target, sha256)? {
                Ok(false)
            } else {
                Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "в каталоге вывода уже есть другой файл с этим именем; он не изменён",
                ))
            };
        }
        Err(error) => return Err(AssetError::io("запись файла вывода", error)),
    }
    #[cfg(test)]
    AFTER_PUBLISH.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(&target);
        }
    });
    let written = std::fs::read(&target).map_err(|error| AssetError::io("файл вывода", error))?;
    if sha256_hex(&written) != sha256 {
        return Err(AssetError::with_details(
            ErrorCode::IntegrityMismatch,
            "после публикации по пути вывода другие байты: файл не удалён, его принадлежность не доказана; проверьте каталог вручную",
            serde_json::json!({"code":"export_readback_mismatch","path": target.display().to_string()}),
        ));
    }
    probe_mp3(&written)?;
    Ok(true)
}

#[cfg(test)]
type PublishHook = std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>>;

#[cfg(test)]
std::thread_local! {
    /// Детерминированная точка между проверкой цели и публикацией.
    pub(crate) static BEFORE_PUBLISH: PublishHook =
        const { std::cell::RefCell::new(None) };
    /// Детерминированная точка между публикацией и обратной сверкой.
    pub(crate) static AFTER_PUBLISH: PublishHook =
        const { std::cell::RefCell::new(None) };
}

/// Уникальное имя временного файла: процесс, поток, время и счётчик, чтобы
/// параллельные экспорты одного SHA не делили временный объект.
fn unique_temporary(directory: &Path, sha256: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let suffix: String = sha256.chars().take(16).collect();
    directory.join(format!(
        ".tmp-export-{}-{nanos:x}-{}-{suffix}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
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

/// Публикация «создать, если отсутствует» без проверки-затем-переименования.
///
/// Основной путь — `hard_link`: на Unix и NTFS он атомарно создаёт имя и
/// возвращает `AlreadyExists`, если оно занято. Если ФС жёстких ссылок не
/// поддерживает (FAT/exFAT, часть сетевых ФС), файл создаётся напрямую с
/// `create_new` — атомарным эксклюзивным созданием, которое тоже никогда не
/// заменяет существующий объект. `rename`, способный заменить цель, не
/// используется ни на одной платформе.
fn publish_no_replace(temporary: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match std::fs::hard_link(temporary, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => create_new_with(target, bytes),
    }
}

/// Эксклюзивно создаёт файл и записывает байты. Если запись не удалась,
/// созданный этим вызовом файл остаётся: удалять имя, которое к этому моменту
/// мог подменить другой процесс, небезопасно; следующий экспорт увидит
/// расхождение SHA и откажет без изменений.
fn create_new_with(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(target)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
pub(crate) fn publish_no_replace_for_test(
    temporary: &Path,
    target: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    publish_no_replace(temporary, target, bytes)
}

#[cfg(test)]
pub(crate) fn create_new_with_for_test(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    create_new_with(target, bytes)
}

#[cfg(test)]
pub(crate) fn persist_selected_file_for_test(
    directory: &Path,
    filename: &str,
    sha256: &str,
    bytes: &[u8],
) -> Result<bool, AssetError> {
    persist_selected_file(directory, filename, sha256, bytes)
}
