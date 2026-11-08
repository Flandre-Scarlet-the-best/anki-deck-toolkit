//! Оценка распознавания на размеченном наборе.
//!
//! Разметка задаёт для каждой записи все произнесённые слова с их чтениями и
//! истинными интервалами (из процедуры построения набора, а не из модели) и,
//! если нужно, цель. Оценка запускает новый конвейер и прежние Balanced и
//! улучшение речи на тех же входах и считает только объективные величины:
//! ложные совпадения (кандидат принят, а цели нет или выбран не тот
//! фрагмент), пропуски, обрезку внутрь цели, лишние слова внутри извлечения
//! и погрешность границ относительно разметки.
//!
//! Обрезка и лишние слова считаются по фактически выданному файлу: по
//! интервалу исходника, который несёт кандидат после всех обрезок
//! (`processing.output_span`), а не по первичному плану извлечения. Если
//! длина декодированного кандидата не согласуется с этим интервалом, связь
//! шкал не доказана и метрика отмечается неразрешимой, а не успешной.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::ctc::Emissions;
use super::enhancer::{self, ItemContext};
use super::kana;
use super::normalizer::{self, Variant};
use super::recognizer::{
    RecognitionAction, SampleSpan, TargetIdentity, TargetSource, TargetSourceKind,
};
use super::recognizer_backend::ModelHandle;
use super::recognizer_pipeline::{self as pipeline, EmissionSource, RecognitionResult};
use super::recognizer_report::{Distribution, distribution};
use crate::hashing::sha256_hex;
use crate::pronunciation_recognizer_cli::Setup;
use crate::{AssetError, ErrorCode};

const MAX_LABELS_BYTES: u64 = 16 * 1024 * 1024;
/// Допустимое расхождение длины декодированного кандидата с длиной
/// интервала исходника, который он несёт (округления ресэмплинга).
const DELIVERED_LENGTH_TOLERANCE_MS: f64 = 5.0;
/// Перекрытие с чужим словом короче этого не считается его присутствием.
const OTHER_WORD_MIN_OVERLAP_MS: f64 = 20.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelWord {
    pub reading: String,
    pub start_ms: f64,
    pub end_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelTarget {
    pub surface: String,
    pub reading: String,
    #[serde(default)]
    pub competing_readings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelCase {
    pub id: String,
    /// Путь MP3 относительно файла разметки.
    pub file: String,
    pub category: String,
    pub target: Option<LabelTarget>,
    /// Все произнесённые слова записи (пусто — речи нет).
    pub words: Vec<LabelWord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    pub schema_version: u32,
    pub kind: String,
    pub license: String,
    pub provenance: String,
    pub cases: Vec<LabelCase>,
}

/// Эмиссии в памяти: сначала все входы группой, затем повторные запросы.
pub struct MemoryEmissions {
    pub direct: pipeline::DirectEmissions,
    store: Mutex<HashMap<(String, String), Emissions>>,
}

impl MemoryEmissions {
    pub fn new(direct: pipeline::DirectEmissions) -> Self {
        Self {
            direct,
            store: Mutex::new(HashMap::new()),
        }
    }
}

impl EmissionSource for MemoryEmissions {
    fn emissions(
        &self,
        model: &ModelHandle,
        inputs: &[&[f32]],
    ) -> Result<Vec<Emissions>, AssetError> {
        let fp = model.identity.fingerprint();
        let keys: Vec<(String, String)> = inputs
            .iter()
            .map(|s| {
                (
                    fp.clone(),
                    sha256_hex(s.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()),
                )
            })
            .collect();
        let missing: Vec<usize> = {
            let store = self.store.lock().expect("кэш");
            (0..inputs.len())
                .filter(|i| !store.contains_key(&keys[*i]))
                .collect()
        };
        if !missing.is_empty() {
            let batch: Vec<&[f32]> = missing.iter().map(|&i| inputs[i]).collect();
            let computed = self.direct.emissions(model, &batch)?;
            let mut store = self.store.lock().expect("кэш");
            for (&i, e) in missing.iter().zip(computed) {
                store.insert(keys[i].clone(), e);
            }
        }
        let store = self.store.lock().expect("кэш");
        Ok(keys.iter().map(|k| store[k].clone()).collect())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseReport {
    pub id: String,
    pub category: String,
    pub target_reading: Option<String>,
    pub target_present: bool,
    pub words: usize,
    pub outcome: String,
    pub next_action: String,
    pub accepted: bool,
    pub false_positive: bool,
    pub missed: bool,
    pub selected_overlaps_target: Option<bool>,
    pub start_error_ms: Option<f64>,
    pub end_error_ms: Option<f64>,
    /// По фактически выданному файлу (`None` — не принят или неразрешимо).
    pub extraction_cuts_target: Option<bool>,
    pub extraction_contains_other_words: Option<bool>,
    /// Связь шкалы выданного файла с исходником доказана (`None` — не принят).
    pub delivered_boundaries_resolved: Option<bool>,
    /// Интервал исходника, который несёт выданный файл.
    pub delivered_span: Option<SampleSpan>,
    /// Те же метрики по первичному плану извлечения — для диагностики.
    pub primary_extraction_cuts_target: Option<bool>,
    pub primary_extraction_contains_other_words: Option<bool>,
    pub free_hypothesis: String,
    pub verifier_hypothesis: Option<String>,
    pub legacy_balanced: Option<String>,
    pub legacy_enhancement: Option<String>,
    pub enhancement_applied: Option<bool>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalSummary {
    pub schema_version: u32,
    /// Записей в разметке: `cases + infrastructure_failures`.
    pub labelled_cases: usize,
    pub cases: usize,
    pub with_target: usize,
    pub target_present: usize,
    pub accepted: usize,
    pub true_accepts: usize,
    pub false_positives: usize,
    pub missed: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    /// По фактически выданным файлам.
    pub extraction_cuts_target: usize,
    pub extraction_contains_other_words: usize,
    /// Принятые кандидаты, для которых границы выданного файла не установлены.
    pub delivered_boundaries_unresolved: usize,
    pub primary_extraction_cuts_target: usize,
    pub primary_extraction_contains_other_words: usize,
    pub start_error_ms: Distribution,
    pub end_error_ms: Distribution,
    pub abs_boundary_error_ms: Distribution,
    pub by_category: BTreeMap<String, BTreeMap<String, usize>>,
    pub by_outcome: BTreeMap<String, usize>,
    pub legacy_balanced: BTreeMap<String, usize>,
    pub legacy_enhancement: BTreeMap<String, usize>,
    /// Записи с несколькими словами, где прежний конвейер оставил бы все слова.
    pub legacy_multi_word_kept: usize,
    pub new_multi_word_extracted: usize,
    pub infrastructure_failures: usize,
    pub confidence: &'static str,
    pub license: String,
    pub provenance: String,
}

fn read_labels(path: &Path, protected: &BTreeSet<PathBuf>) -> Result<Labels, AssetError> {
    let bytes = crate::pronunciation_cli::read_local(path, protected, MAX_LABELS_BYTES)?;
    let labels: Labels = serde_json::from_slice(&bytes).map_err(|e| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            format!("разметка не разбирается: {e}"),
        )
    })?;
    if labels.schema_version != 1 || labels.kind != "pronunciation_recognition_labels" {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "разметка: ожидается schema_version 1 и kind pronunciation_recognition_labels",
        ));
    }
    Ok(labels)
}

fn same_reading(a: &str, b: &str) -> bool {
    kana::to_hiragana(a) == kana::to_hiragana(b)
}

/// Интервал исходника, который несёт выданный кандидат, если связь шкал
/// доказана: длина декодированного кандидата совпала с длиной интервала в
/// его частоте. Иначе `None` — границы неразрешимы.
fn delivered_span(result: &RecognitionResult) -> Option<SampleSpan> {
    let span = result.processing.as_ref()?.output_span?;
    let probe = result.post_validation.as_ref()?.output_probe.as_ref()?;
    let source_rate = result.pre_analysis.as_ref()?.timeline.sample_rate;
    // `frames` пробы — число кадров MP3; длина PCM на канал — `decoded_samples`.
    span_matches_length(span, source_rate, probe.decoded_samples, probe.sample_rate).then_some(span)
}

fn span_matches_length(span: SampleSpan, source_rate: u32, frames: u64, rate: u32) -> bool {
    if source_rate == 0 || rate == 0 {
        return false;
    }
    let expected = span.len() as f64 * f64::from(rate) / f64::from(source_rate);
    (expected - frames as f64).abs() <= DELIVERED_LENGTH_TOLERANCE_MS * f64::from(rate) / 1000.0
}

/// Слово `[start, end)` (кадры исходника) не целиком внутри интервала.
fn cuts(span: SampleSpan, word: SampleSpan) -> bool {
    !span.contains(&word)
}

/// Чужое слово перекрывает интервал заметнее `min_overlap` кадров.
fn contains_other(span: SampleSpan, others: &[SampleSpan], min_overlap: u64) -> bool {
    others
        .iter()
        .any(|w| span.end.min(w.end).saturating_sub(span.start.max(w.start)) > min_overlap)
}

/// Сохранность целевого слова и присутствие чужих слов: по фактически
/// выданному файлу и, для диагностики, по первичному плану извлечения. Всё
/// `None`, если кандидат не принят.
pub(crate) struct BoundaryMetrics {
    pub delivered: Option<SampleSpan>,
    pub delivered_boundaries_resolved: Option<bool>,
    pub extraction_cuts_target: Option<bool>,
    pub extraction_contains_other_words: Option<bool>,
    pub primary_extraction_cuts_target: Option<bool>,
    pub primary_extraction_contains_other_words: Option<bool>,
}

/// `target` — индекс размеченного целевого слова, совпавшего с выбранным
/// вхождением; `rate` — частота исходника (кадры разметки).
pub(crate) fn boundary_metrics(
    result: &RecognitionResult,
    words: &[LabelWord],
    target: Option<usize>,
    rate: f64,
) -> BoundaryMetrics {
    let accepted = result.next_action == RecognitionAction::UseFinalCandidate;
    let to_frames = |ms: f64| (ms * rate / 1000.0).round().max(0.0) as u64;
    let span_of = |w: &LabelWord| SampleSpan {
        start: to_frames(w.start_ms),
        end: to_frames(w.end_ms),
    };
    let target_span = target.and_then(|i| words.get(i)).map(span_of);
    let others: Vec<SampleSpan> = words
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != target)
        .map(|(_, w)| span_of(w))
        .collect();
    let min_overlap = to_frames(OTHER_WORD_MIN_OVERLAP_MS);
    let delivered = delivered_span(result).filter(|_| accepted);
    let primary = result
        .processing
        .as_ref()
        .and_then(|p| p.extraction)
        .filter(|_| accepted);
    BoundaryMetrics {
        delivered,
        delivered_boundaries_resolved: accepted.then_some(delivered.is_some()),
        extraction_cuts_target: delivered.zip(target_span).map(|(d, w)| cuts(d, w)),
        extraction_contains_other_words: delivered.map(|d| contains_other(d, &others, min_overlap)),
        primary_extraction_cuts_target: primary.zip(target_span).map(|(e, w)| cuts(e, w)),
        primary_extraction_contains_other_words: primary
            .map(|e| contains_other(e, &others, min_overlap)),
    }
}

pub(crate) fn evaluate(
    setup: &Setup,
    labels_path: &Path,
    protected: &BTreeSet<PathBuf>,
    out: &Path,
    jobs: usize,
) -> Result<EvalSummary, AssetError> {
    let labels = read_labels(labels_path, protected)?;
    let base_dir = labels_path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir(out).map_err(|e| AssetError::io("каталог оценки", e))?;
    let source = MemoryEmissions::new(setup.direct());
    let ctx = setup.context(&source);
    let mut cases = Vec::new();
    for case in &labels.cases {
        let path = base_dir.join(&case.file);
        let bytes = crate::pronunciation_cli::read_local(
            &path,
            protected,
            crate::pronunciation::MAX_AUDIO_BYTES,
        )?;
        let target = case.target.as_ref().map(|t| TargetIdentity {
            surface: t.surface.clone(),
            reading: t.reading.clone(),
            source: TargetSource {
                kind: TargetSourceKind::Manifest,
                key: format!("labels:{}", case.id),
            },
            competing_readings: t.competing_readings.clone(),
        });
        cases.push((case.clone(), bytes, target));
    }
    // Эмиссии всех исходников — группами по `call_items`.
    let analyses: Vec<Vec<f32>> = cases
        .iter()
        .filter_map(
            |(_, bytes, _)| match pipeline::decode(bytes, ctx.base_profile) {
                Ok(Ok(d)) => Some(d.analysis),
                _ => None,
            },
        )
        .collect();
    let refs: Vec<&[f32]> = analyses.iter().map(Vec::as_slice).collect();
    source.emissions(ctx.primary, &refs)?;
    if let Some(v) = ctx.verifier {
        source.emissions(v, &refs)?;
    }
    // Без сигнала остановки: оценка выполняется целиком или завершается ошибкой.
    let never = std::sync::atomic::AtomicBool::new(false);
    let (stage_one, panicked) = pipeline::run_parallel(
        "этап 1",
        &cases,
        jobs.clamp(1, 8),
        &never,
        |(_, bytes, target)| pipeline::stage_one(bytes, target.as_ref(), &ctx),
    );
    if let Some(error) = panicked {
        return Err(error);
    }
    let finals: Vec<Vec<f32>> = stage_one
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok()?.needs_post_validation())
        .filter_map(|b| match pipeline::decode(b, ctx.base_profile) {
            Ok(Ok(d)) => Some(d.analysis),
            _ => None,
        })
        .collect();
    let refs: Vec<&[f32]> = finals.iter().map(Vec::as_slice).collect();
    source.emissions(ctx.primary, &refs)?;
    if let Some(v) = ctx.verifier {
        source.emissions(v, &refs)?;
    }
    let mut reports = Vec::new();
    let mut infra = 0;
    let mut jsonl = String::new();
    for (i, stage) in stage_one {
        let (case, bytes, _) = &cases[i];
        let recognized = match stage
            .and_then(|s| pipeline::catch_panic("этап 2", || pipeline::stage_two(s, &ctx)))
        {
            Ok(r) => r,
            Err(error) if pipeline::is_infrastructure_failure(&error) => {
                infra += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let result = &recognized.result;
        jsonl.push_str(
            &serde_json::to_string(&serde_json::json!({"id": case.id, "result": result}))
                .expect("сериализуемо"),
        );
        jsonl.push('\n');
        // Прежний конвейер на том же входе.
        let legacy = normalizer::process(
            bytes,
            ctx.base_profile,
            normalizer::Mode::Normalize,
            ctx.base_toolchain,
        )?;
        let legacy_enhancement = match &setup.enhancer {
            Some(e) => {
                let item = ItemContext {
                    profile: &e.profile,
                    backend: &e.identity,
                    worker: &e.worker,
                    model_dir: &e.model_dir,
                    base_profile: ctx.base_profile,
                    base_toolchain: ctx.base_toolchain,
                    config_sha256: &e.config_sha256,
                };
                let selected = match legacy.result.gate.proposed_variant {
                    Some(Variant::Processed) => legacy.output_bytes.clone(),
                    Some(Variant::Original) => Some(bytes.clone()),
                    None => None,
                };
                match enhancer::process_item(&item, bytes, &legacy.result, selected.as_deref()) {
                    Ok(r) => Some(r.result.gate.outcome.as_str().to_string()),
                    Err(error) if enhancer::is_infrastructure_failure(&error) => None,
                    Err(error) => return Err(error),
                }
            }
            None => None,
        };
        let rate = result
            .pre_analysis
            .as_ref()
            .map(|p| f64::from(p.timeline.sample_rate))
            .unwrap_or(44_100.0);
        let to_frames = |ms: f64| (ms * rate / 1000.0).round().max(0.0) as u64;
        let target_reading = case.target.as_ref().map(|t| t.reading.clone());
        let target_words: Vec<&LabelWord> = case
            .words
            .iter()
            .filter(|w| {
                target_reading
                    .as_deref()
                    .is_some_and(|r| same_reading(&w.reading, r))
            })
            .collect();
        let target_present = !target_words.is_empty();
        let accepted = result.next_action == RecognitionAction::UseFinalCandidate;
        let selected = result
            .selected_occurrence
            .and_then(|s| result.matching.as_ref()?.occurrences.get(s));
        let overlap_word = selected.and_then(|o| {
            target_words.iter().find(|w| {
                let (ws, we) = (to_frames(w.start_ms), to_frames(w.end_ms));
                o.core.start < we && ws < o.core.end
            })
        });
        let selected_overlaps_target = selected.map(|_| overlap_word.is_some());
        let ms = |frames: i64| frames as f64 * 1000.0 / rate;
        let (start_error_ms, end_error_ms) = match (selected, overlap_word) {
            (Some(o), Some(w)) => (
                Some(ms(o.core.start as i64 - to_frames(w.start_ms) as i64)),
                Some(ms(o.core.end as i64 - to_frames(w.end_ms) as i64)),
            ),
            _ => (None, None),
        };
        let target_index =
            overlap_word.and_then(|t| case.words.iter().position(|w| std::ptr::eq(*t, w)));
        let BoundaryMetrics {
            delivered,
            delivered_boundaries_resolved,
            extraction_cuts_target,
            extraction_contains_other_words,
            primary_extraction_cuts_target,
            primary_extraction_contains_other_words,
        } = boundary_metrics(result, &case.words, target_index, rate);
        let false_positive =
            accepted && (!target_present || selected_overlaps_target == Some(false));
        let pre = result.pre_analysis.as_ref();
        reports.push(CaseReport {
            id: case.id.clone(),
            category: case.category.clone(),
            target_reading,
            target_present,
            words: case.words.len(),
            outcome: result.outcome.as_str().into(),
            next_action: result.next_action.as_str().into(),
            accepted,
            false_positive,
            missed: target_present && !accepted,
            selected_overlaps_target,
            start_error_ms,
            end_error_ms,
            extraction_cuts_target,
            extraction_contains_other_words,
            delivered_boundaries_resolved,
            delivered_span: delivered,
            primary_extraction_cuts_target,
            primary_extraction_contains_other_words,
            free_hypothesis: pre
                .map(|p| p.primary.normalized.clone())
                .unwrap_or_default(),
            verifier_hypothesis: pre
                .and_then(|p| p.verifier.as_ref())
                .map(|v| v.raw_text.clone()),
            legacy_balanced: Some(legacy.result.gate.outcome.as_str().into()),
            legacy_enhancement,
            enhancement_applied: result
                .processing
                .as_ref()
                .and_then(|p| p.enhancement.as_ref())
                .map(|e| e.applied),
            reasons: result.reasons.iter().map(|r| r.code.clone()).collect(),
        });
    }
    let count = |f: &dyn Fn(&CaseReport) -> bool| reports.iter().filter(|r| f(r)).count();
    let accepted = count(&|r| r.accepted);
    let false_positives = count(&|r| r.false_positive);
    let present = count(&|r| r.target_present);
    let true_accepts = count(&|r| r.accepted && !r.false_positive);
    let mut starts: Vec<f64> = reports
        .iter()
        .filter_map(|r| r.start_error_ms.filter(|_| r.accepted))
        .collect();
    let mut ends: Vec<f64> = reports
        .iter()
        .filter_map(|r| r.end_error_ms.filter(|_| r.accepted))
        .collect();
    let mut abs: Vec<f64> = starts.iter().chain(ends.iter()).map(|v| v.abs()).collect();
    let mut by_category: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut by_outcome = BTreeMap::new();
    let mut legacy_balanced = BTreeMap::new();
    let mut legacy_enhancement = BTreeMap::new();
    for r in &reports {
        *by_category
            .entry(r.category.clone())
            .or_default()
            .entry(r.outcome.clone())
            .or_insert(0) += 1;
        *by_outcome.entry(r.outcome.clone()).or_insert(0) += 1;
        if let Some(l) = &r.legacy_balanced {
            *legacy_balanced.entry(l.clone()).or_insert(0) += 1;
        }
        if let Some(l) = &r.legacy_enhancement {
            *legacy_enhancement.entry(l.clone()).or_insert(0) += 1;
        }
    }
    // Каждая запись разметки либо оценена, либо учтена как сбой среды:
    // иначе метрики считались бы по неполному набору незаметно.
    if reports.len() + infra != labels.cases.len() {
        return Err(AssetError::new(
            ErrorCode::ValidatorFailure,
            format!(
                "оценено {} и сбоев среды {infra} при {} записях разметки",
                reports.len(),
                labels.cases.len()
            ),
        ));
    }
    let summary = EvalSummary {
        schema_version: 1,
        labelled_cases: labels.cases.len(),
        cases: reports.len(),
        with_target: count(&|r| r.target_reading.is_some()),
        target_present: present,
        accepted,
        true_accepts,
        false_positives,
        missed: count(&|r| r.missed),
        precision: (accepted > 0).then(|| true_accepts as f64 / accepted as f64),
        recall: (present > 0).then(|| true_accepts as f64 / present as f64),
        extraction_cuts_target: count(&|r| r.extraction_cuts_target == Some(true)),
        extraction_contains_other_words: count(&|r| {
            r.extraction_contains_other_words == Some(true)
        }),
        delivered_boundaries_unresolved: count(&|r| r.delivered_boundaries_resolved == Some(false)),
        primary_extraction_cuts_target: count(&|r| r.primary_extraction_cuts_target == Some(true)),
        primary_extraction_contains_other_words: count(&|r| {
            r.primary_extraction_contains_other_words == Some(true)
        }),
        start_error_ms: distribution(&mut starts),
        end_error_ms: distribution(&mut ends),
        abs_boundary_error_ms: distribution(&mut abs),
        by_category,
        by_outcome,
        legacy_balanced,
        legacy_enhancement,
        legacy_multi_word_kept: count(&|r| r.words > 1),
        new_multi_word_extracted: count(&|r| {
            r.words > 1 && r.accepted && r.extraction_contains_other_words == Some(false)
        }),
        infrastructure_failures: infra,
        confidence: "uncalibrated",
        license: labels.license.clone(),
        provenance: labels.provenance.clone(),
    };
    let write = |name: &str, bytes: &[u8]| {
        std::fs::write(out.join(name), bytes).map_err(|e| AssetError::io(name, e))
    };
    write(
        "summary.json",
        &serde_json::to_vec_pretty(&summary).expect("сериализуемо"),
    )?;
    write(
        "cases.json",
        &serde_json::to_vec_pretty(&reports).expect("сериализуемо"),
    )?;
    write("results.jsonl", jsonl.as_bytes())?;
    Ok(summary)
}
