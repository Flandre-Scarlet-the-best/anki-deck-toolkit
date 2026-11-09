//! Материализация выбранного интервала: самостоятельный MP3 точных границ.
//!
//! Интервал (`occurrence_interval`) адресно указывает фрагмент исходника, но
//! байтов не имеет. Материализация декодирует точные исходные байты, вырезает
//! кадры интервала, накладывает короткие линейные затухания против щелчков и
//! кодирует результат тем же контролируемым путём libmp3lame, что и остальные
//! производные. Результат проходит полную техническую проверку `probe_mp3`,
//! получает собственный SHA и цепочку обработки (`CandidateDerivation`) и
//! становится кандидатом `materialized_interval` той же связи. Отказ
//! фиксируется в журнале и оставляет цель в явном ожидающем состоянии.
//!
//! Фонетическую проверку полученных байтов выполняет штатная
//! `selection audio-review` материализованного кандидата; готовность к выдаче
//! решает `ops::selection_readiness`.

use serde::Serialize;

use super::engine::SelectionIndex;
use super::model::*;
use super::ops::{self, candidate_id, materialized_of};
use crate::batch_runtime::{RuntimeBatchState, SafeBatchRuntime};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::pronunciation::{MAX_AUDIO_BYTES, Pronunciation, probe, probe_mp3};
use crate::store::AssetStore;

pub const TRANSFORM: &str = "interval_extraction_v1";
/// Линейное затухание на краях среза, мс (не больше трети длины).
const FADE_MS: f64 = 5.0;
/// Минимальная длина материализуемого интервала, мс.
const MIN_INTERVAL_MS: f64 = 60.0;
const ENCODER: [&str; 2] = ["-q:a", "2"];

#[derive(Debug, Clone, Serialize)]
pub struct MaterializedItem {
    pub surface: String,
    pub reading: String,
    pub interval_candidate_id: String,
    pub candidate_id: String,
    pub sha256: String,
    pub span_ms: (f64, f64),
}

#[derive(Debug, Serialize)]
pub struct FailedItem {
    pub surface: String,
    pub reading: String,
    pub interval_candidate_id: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub struct MaterializeReport {
    pub materialized: Vec<MaterializedItem>,
    pub failed: Vec<FailedItem>,
    /// У выбранного интервала уже есть проверенные байты.
    pub already_materialized: usize,
    /// Выбран не интервал: у кандидата свои байты, материализация не нужна.
    pub not_interval: usize,
    pub revision: u64,
    /// Следующее действие для байтов агентского решения.
    pub next_action: Option<String>,
}

/// Байты и цепочка обработки одного интервала.
pub(crate) struct Extraction {
    pub bytes: Vec<u8>,
    pub chain: Vec<String>,
    pub toolchain: Vec<String>,
    pub config_sha256: String,
}

fn failure(code: &str, message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        message,
        serde_json::json!({"blocker": code}),
    )
}

/// Вырезает интервал `span` (кадры шкалы `span.sample_rate`) из точных
/// исходных байтов и кодирует его как самостоятельный MP3.
pub(crate) fn extract_interval(
    source: &[u8],
    span: &CandidateSpan,
    toolchain: &[String],
) -> Result<Extraction, AssetError> {
    let probe = probe_mp3(source)?;
    let samples = probe::decode_f32(source, &probe)?;
    let channels = probe.channels as usize;
    let frames = samples.len() / channels.max(1);
    if span.sample_rate == 0 || span.start_frame >= span.end_frame {
        return Err(failure("interval_invalid", "интервал пуст или без частоты"));
    }
    let scale = |frame: u64| -> usize {
        (u128::from(frame) * u128::from(probe.sample_rate) / u128::from(span.sample_rate)) as usize
    };
    let start = scale(span.start_frame);
    let end = scale(span.end_frame);
    if end > frames || start >= end {
        return Err(failure(
            "interval_out_of_range",
            "границы интервала выходят за декодированный исходник",
        ));
    }
    let rate = f64::from(probe.sample_rate);
    let length_ms = (end - start) as f64 * 1000.0 / rate;
    if length_ms < MIN_INTERVAL_MS {
        return Err(failure(
            "interval_too_short",
            format!("интервал короче {MIN_INTERVAL_MS} мс"),
        ));
    }
    let mut slice = samples[start * channels..end * channels].to_vec();
    let fade = ((FADE_MS * rate / 1000.0) as usize)
        .min((end - start) / 3)
        .max(1);
    let length = end - start;
    for frame in 0..fade {
        let gain = frame as f32 / fade as f32;
        for channel in 0..channels {
            slice[frame * channels + channel] *= gain;
            slice[(length - 1 - frame) * channels + channel] *= gain;
        }
    }
    let encoder: Vec<String> = ENCODER.iter().map(|s| (*s).to_owned()).collect();
    let bytes = probe::encode_mp3(
        &slice,
        probe.sample_rate,
        probe.channels,
        probe.sample_rate,
        &encoder,
    )?;
    let output = probe_mp3(&bytes)?;
    if (output.decoded_duration_seconds * 1000.0 - length_ms).abs() > 60.0 {
        return Err(failure(
            "materialized_duration_mismatch",
            "длительность полученного MP3 не соответствует интервалу",
        ));
    }
    let chain = vec![
        format!(
            "decode:ffmpeg_pcm_f32le@{}Hz/{}ch",
            probe.sample_rate, probe.channels
        ),
        format!(
            "slice_frames:{start}-{end}@{}Hz (интервал {}-{}@{}Hz)",
            probe.sample_rate, span.start_frame, span.end_frame, span.sample_rate
        ),
        format!("fade_linear_ms:{FADE_MS}"),
        format!(
            "encode:libmp3lame {}@{}Hz bitexact",
            ENCODER.join(" "),
            probe.sample_rate
        ),
        "probe:pronunciation-mp3-technical".to_owned(),
    ];
    let config_sha256 = sha256_hex(
        serde_json::to_vec(&serde_json::json!({
            "transform": TRANSFORM,
            "fade_ms": FADE_MS,
            "min_interval_ms": MIN_INTERVAL_MS,
            "encoder": ENCODER,
            "toolchain": toolchain,
        }))
        .expect("конфигурация сериализуема"),
    );
    Ok(Extraction {
        bytes,
        chain,
        toolchain: toolchain.to_vec(),
        config_sha256,
    })
}

fn toolchain() -> Result<Vec<String>, AssetError> {
    Ok(vec![
        format!("ffmpeg:{}", probe::ffmpeg_identity()?),
        format!("encoder:{}", probe::encoder_identity()?),
    ])
}

/// Материализует выбранные интервалы действующих решений пакета (или одной
/// цели). Решения не меняются: появляется только кандидат с байтами и запись
/// журнала. Повтор не создаёт второго кандидата для тех же границ.
pub fn materialize(
    store: &AssetStore,
    batch_id: &str,
    only: Option<(&str, &str)>,
) -> Result<MaterializeReport, AssetError> {
    let only = only
        .map(|(surface, reading)| Pronunciation::new(surface, reading))
        .transpose()?;
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    let mut batch: SelectionBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет выбора не найден"))?;
    let mut report = MaterializeReport::default();
    // План собирается по неизменному состоянию; запись — одна, в конце.
    let mut work = Vec::new();
    {
        let index = SelectionIndex::new(&batch);
        for (surface, reading) in batch.identities() {
            if only
                .as_ref()
                .is_some_and(|o| o.surface != surface || o.reading != reading)
            {
                continue;
            }
            let Some(decision) = batch.current_decision(&surface, &reading) else {
                continue;
            };
            let DecisionAction::Select { candidate_id, .. } = &decision.action else {
                continue;
            };
            if !ops::decision_is_effective(&index, &batch, decision) {
                continue;
            }
            let (link, candidate) = index
                .find_candidate(candidate_id)
                .expect("действующее решение ссылается на кандидата");
            if candidate.kind != CandidateKind::OccurrenceInterval {
                report.not_interval += 1;
                continue;
            }
            if materialized_of(link, candidate).is_some() {
                report.already_materialized += 1;
                continue;
            }
            work.push((
                surface.clone(),
                reading.clone(),
                link.link_id.clone(),
                link.generation,
                link.source_sha256.clone(),
                candidate.clone(),
                decision.authority.is_user(),
            ));
        }
    }
    if only.is_some() && work.is_empty() && report.already_materialized == 0 {
        return Err(invalid(
            "у цели нет действующего выбора интервала, ожидающего материализации",
        ));
    }
    let mut tools: Option<Vec<String>> = None;
    let mut agent_pending = false;
    let timestamp = ops::recorded_at(std::time::SystemTime::now())?;
    let revision = batch.revision + 1;
    for (surface, reading, link_id, generation, source_sha, interval, by_user) in work {
        let span = interval.span.expect("интервал имеет границы");
        let source = batch
            .source(&source_sha)
            .ok_or_else(|| invalid("исходник интервала отсутствует"))?;
        let source_bytes = runtime.read_blob_with_limit(&source.blob, MAX_AUDIO_BYTES)?;
        if sha256_hex(&source_bytes) != source_sha {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "исходные байты интервала не совпадают с SHA",
            ));
        }
        let extraction = match &tools {
            Some(tools) => extract_interval(&source_bytes, &span, tools),
            None => toolchain().and_then(|t| {
                let result = extract_interval(&source_bytes, &span, &t);
                tools = Some(t);
                result
            }),
        };
        let outcome = match extraction {
            Ok(extraction) => {
                let sha = sha256_hex(&extraction.bytes);
                let probe = probe_mp3(&extraction.bytes)?;
                let blob =
                    runtime.persist_blob_with_limit(&extraction.bytes, "mp3", MAX_AUDIO_BYTES)?;
                let id = candidate_id(
                    &link_id,
                    CandidateKind::MaterializedInterval,
                    &sha,
                    Some(&span),
                );
                let link = batch
                    .links
                    .iter_mut()
                    .find(|l| l.link_id == link_id)
                    .expect("связь интервала существует");
                if !link.candidates.iter().any(|c| c.candidate_id == id) {
                    link.candidates.push(PronunciationCandidate {
                        candidate_id: id.clone(),
                        kind: CandidateKind::MaterializedInterval,
                        audio: Some(CandidateAudio {
                            sha256: sha.clone(),
                            byte_length: extraction.bytes.len() as u64,
                            blob,
                            probe,
                        }),
                        span: Some(span),
                        occurrence: interval.occurrence,
                        technical: TechnicalGate {
                            usable: true,
                            code: "materialized_probe_ok".into(),
                        },
                        derivation: Some(CandidateDerivation {
                            transform: TRANSFORM.into(),
                            source_sha256: source_sha.clone(),
                            from_candidate_id: interval.candidate_id.clone(),
                            span,
                            chain: extraction.chain,
                            toolchain: extraction.toolchain,
                            config_sha256: extraction.config_sha256,
                            output_sha256: sha.clone(),
                        }),
                    });
                }
                agent_pending |= !by_user;
                report.materialized.push(MaterializedItem {
                    surface,
                    reading,
                    interval_candidate_id: interval.candidate_id.clone(),
                    candidate_id: id.clone(),
                    sha256: sha.clone(),
                    span_ms: (span.start_ms(), span.end_ms()),
                });
                MaterializationOutcome::Materialized {
                    candidate_id: id,
                    sha256: sha,
                }
            }
            // Нарушение границы или целостности хранилища — ошибка операции.
            Err(error)
                if matches!(
                    error.code,
                    ErrorCode::BoundaryViolation
                        | ErrorCode::PathTraversal
                        | ErrorCode::IntegrityMismatch
                ) =>
            {
                return Err(error);
            }
            Err(error) => {
                let code = error.details["blocker"]
                    .as_str()
                    .unwrap_or(error.code.as_str())
                    .to_owned();
                let message: String = error.message.chars().take(1024).collect();
                report.failed.push(FailedItem {
                    surface,
                    reading,
                    interval_candidate_id: interval.candidate_id.clone(),
                    code: code.clone(),
                    message: message.clone(),
                });
                MaterializationOutcome::Failed { code, message }
            }
        };
        batch.materializations.push(MaterializationRecord {
            interval_candidate_id: interval.candidate_id.clone(),
            link_id,
            link_generation: generation,
            source_sha256: source_sha,
            span,
            outcome,
            state_revision: revision,
            recorded_at_unix_ms: timestamp,
        });
    }
    if !report.materialized.is_empty() || !report.failed.is_empty() {
        batch.revision = revision;
        batch.validate()?;
        runtime.save(&batch)?;
    }
    report.revision = batch.revision;
    if agent_pending {
        report.next_action = Some(
            "решение агента по интервалу станет готовым после поддерживающей selection audio-review материализованного кандидата".into(),
        );
    }
    Ok(report)
}
