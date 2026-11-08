//! Отчёт пакета распознавания: машиночитаемые `summary.json`, `items.jsonl`,
//! `items.tsv` и `occurrences.tsv` в новом каталоге.
//!
//! Отчёт различает полноту по уникальным SHA и по парам SHA × цель, долю
//! входов с известной целью, исходы и действия, все найденные вхождения с
//! интервалами и признаками, использованные модели и время. Распределения
//! «уверенности» здесь — измеренные признаки на реальном корпусе без
//! разметки; доказанная точность — только в отчёте `recognizer eval`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::normalizer_batch::Inventory;
use super::recognizer::Evidence;
use super::recognizer_batch::{RecognizerBatch, WorkItem};
use super::recognizer_pipeline::RecognitionResult;
use crate::AssetError;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Distribution {
    pub count: usize,
    pub min: Option<f64>,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
}

pub fn distribution(values: &mut [f64]) -> Distribution {
    values.sort_by(f64::total_cmp);
    let at = |q: f64| {
        values
            .get(((values.len().max(1) - 1) as f64 * q).round() as usize)
            .copied()
    };
    Distribution {
        count: values.len(),
        min: values.first().copied(),
        p50: at(0.5),
        p90: at(0.9),
        max: values.last().copied(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub schema_version: u32,
    pub batch_id: String,
    pub config_sha256: String,
    pub physical_files: usize,
    pub unique_inputs: u64,
    pub pairs: u64,
    pub pairs_with_target: u64,
    pub pairs_reported: usize,
    pub inputs_without_target: usize,
    pub unresolved_manifest_entries: usize,
    pub by_outcome: BTreeMap<String, u64>,
    pub by_action: BTreeMap<String, u64>,
    /// Число речевых событий свободной гипотезы на запись (уникальные SHA).
    pub speech_events_per_input: BTreeMap<usize, u64>,
    pub duration_ms: Distribution,
    pub occurrences_total: usize,
    pub occurrences_by_evidence: BTreeMap<String, u64>,
    /// Измеренные признаки подтверждённых вхождений (не калиброванная вероятность).
    pub match_score_per_token: Distribution,
    pub competitor_margin: Distribution,
    pub fallback_used: usize,
    pub extraction_enhanced: usize,
    pub post_validation_failed_checks: BTreeMap<String, u64>,
    /// Решения прежних Balanced и улучшения по всей записи для тех же входов
    /// (уникальные SHA).
    pub legacy_balanced_outcomes: BTreeMap<String, u64>,
    pub legacy_enhancement_outcomes: BTreeMap<String, u64>,
    pub models: Vec<serde_json::Value>,
    pub model_calls: u64,
    pub model_items: u64,
    pub model_wall_ms: f64,
    pub wall_ms: f64,
    pub confidence: &'static str,
}

fn tsv(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

/// Решения прежних Balanced и улучшения по всей записи — по одному на
/// уникальный SHA, независимо от числа целей этого входа.
pub(crate) fn legacy_outcomes(
    results: &[(WorkItem, RecognitionResult)],
) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let mut balanced = BTreeMap::new();
    let mut enhancement = BTreeMap::new();
    let mut seen_balanced = std::collections::BTreeSet::new();
    // Отдельный учёт: у первой пары SHA обработки всей записи может не быть.
    let mut seen_enhancement = std::collections::BTreeSet::new();
    for (item, result) in results {
        if let Some(full) = &result.full_record
            && seen_balanced.insert(item.sha256.as_str())
        {
            *balanced
                .entry(full.balanced_outcome.clone())
                .or_insert(0u64) += 1;
        }
        if let Some(o) = result
            .processing
            .as_ref()
            .and_then(|p| p.legacy_enhancement_outcome.as_ref())
            && seen_enhancement.insert(item.sha256.as_str())
        {
            *enhancement.entry(o.clone()).or_insert(0u64) += 1;
        }
    }
    (balanced, enhancement)
}

pub fn write_report(
    batch: &RecognizerBatch,
    inventory: &Inventory,
    results: &[(WorkItem, RecognitionResult)],
    out: &Path,
) -> Result<Summary, AssetError> {
    std::fs::create_dir(out).map_err(|e| AssetError::io("каталог отчёта", e))?;
    let mut by_outcome = BTreeMap::new();
    let mut by_action = BTreeMap::new();
    let mut events = BTreeMap::new();
    let mut durations = Vec::new();
    let mut evidence = BTreeMap::new();
    let mut scores = Vec::new();
    let mut margins = Vec::new();
    let mut failed_checks = BTreeMap::new();
    let (legacy_balanced, legacy_enhancement) = legacy_outcomes(results);
    let mut fallback = 0;
    let mut enhanced = 0;
    let mut occurrences_total = 0;
    let mut seen_inputs = std::collections::BTreeSet::new();
    let mut items_jsonl = String::new();
    let mut items_tsv = String::from(
        "sha256\tpath\tsurface\treading\toutcome\tnext_action\tevents\tfree_hypothesis\tverifier_hypothesis\toccurrences\tselected\textraction_start\textraction_end\tsample_rate\toutput_sha256\n",
    );
    let mut occ_tsv = String::from(
        "sha256\tsurface\treading\tindex\tevidence\tphonemes\tfree_phonemes\tcore_start\tcore_end\tstart_unc_start\tstart_unc_end\tend_unc_start\tend_unc_end\tscore_per_token\tcompetitor_margin\tbest_competitor\tverifier_text\tsnr_db\treasons\n",
    );
    for (item, result) in results {
        *by_outcome
            .entry(result.outcome.as_str().to_string())
            .or_insert(0u64) += 1;
        *by_action
            .entry(result.next_action.as_str().to_string())
            .or_insert(0u64) += 1;
        let pre = result.pre_analysis.as_ref();
        if seen_inputs.insert(item.sha256.clone())
            && let Some(pre) = pre
        {
            *events.entry(pre.events.len()).or_insert(0u64) += 1;
            durations.push(pre.timeline.ms(pre.timeline.frames));
        }
        if result.fallback.is_some() {
            fallback += 1;
        }
        if result
            .processing
            .as_ref()
            .and_then(|p| p.enhancement.as_ref())
            .is_some_and(|e| e.applied)
        {
            enhanced += 1;
        }
        if let Some(post) = &result.post_validation {
            for check in post.checks.iter().filter(|c| !c.passed) {
                *failed_checks.entry(check.id.clone()).or_insert(0u64) += 1;
            }
        }
        let target = result.target.as_ref();
        let (surface, reading) = target
            .map(|t| (t.surface.as_str(), t.reading.as_str()))
            .unwrap_or(("", ""));
        if let Some(matching) = &result.matching {
            for o in &matching.occurrences {
                occurrences_total += 1;
                let key = match o.evidence {
                    Evidence::Match => "match",
                    Evidence::Weak => "weak",
                    Evidence::Conflict => "conflict",
                };
                *evidence.entry(key.to_string()).or_insert(0u64) += 1;
                if o.evidence == Evidence::Match {
                    scores.push(f64::from(o.score_per_token));
                    if let Some(m) = o.competitor_margin {
                        margins.push(f64::from(m));
                    }
                }
                occ_tsv.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{key}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{}\t{}\t{}\t{}\t{}\n",
                    item.sha256,
                    tsv(surface),
                    tsv(reading),
                    o.index,
                    o.phonemes,
                    o.free_phonemes,
                    o.core.start,
                    o.core.end,
                    o.start_uncertainty.start,
                    o.start_uncertainty.end,
                    o.end_uncertainty.start,
                    o.end_uncertainty.end,
                    o.score_per_token,
                    o.competitor_margin.map(|m| format!("{m:.3}")).unwrap_or_default(),
                    o.best_competitor
                        .as_ref()
                        .map(|c| format!("{} ({})", c.phonemes, c.origin))
                        .unwrap_or_default(),
                    tsv(o.verifier_text.as_deref().unwrap_or("")),
                    o.snr_db.map(|v| format!("{v:.1}")).unwrap_or_default(),
                    o.reasons.join(","),
                ));
            }
        }
        let compact = serde_json::json!({
            "sha256": item.sha256,
            "path": item.path,
            "target": target,
            "outcome": result.outcome,
            "next_action": result.next_action,
            "free_hypothesis": pre.map(|p| &p.primary.normalized),
            "verifier_hypothesis": pre.and_then(|p| p.verifier.as_ref()).map(|v| &v.raw_text),
            "events": pre.map(|p| &p.events),
            "occurrences": result.matching.as_ref().map(|m| &m.occurrences),
            "selected_occurrence": result.selected_occurrence,
            "extraction": result.extraction,
            "output": result.output,
            "full_record": result.full_record,
            "post_validation": result.post_validation.as_ref().map(|p| serde_json::json!({
                "passed": p.passed, "checks": p.checks, "timing_error_ms": p.timing_error_ms,
                "start_error_ms": p.start_error_ms, "end_error_ms": p.end_error_ms,
            })),
            "reasons": result.reasons,
            "uncertainty": result.uncertainty,
            "config_sha256": result.config_sha256,
        });
        items_jsonl.push_str(&serde_json::to_string(&compact).expect("сериализуемо"));
        items_jsonl.push('\n');
        items_tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            item.sha256,
            tsv(&item.path),
            tsv(surface),
            tsv(reading),
            result.outcome.as_str(),
            result.next_action.as_str(),
            pre.map(|p| p.events.len()).unwrap_or(0),
            tsv(pre.map(|p| p.primary.normalized.as_str()).unwrap_or("")),
            tsv(pre
                .and_then(|p| p.verifier.as_ref())
                .map(|v| v.raw_text.as_str())
                .unwrap_or("")),
            result
                .matching
                .as_ref()
                .map(|m| m.occurrences.len())
                .unwrap_or(0),
            result
                .selected_occurrence
                .map(|s| s.to_string())
                .unwrap_or_default(),
            result
                .extraction
                .as_ref()
                .map(|e| e.extraction.start.to_string())
                .unwrap_or_default(),
            result
                .extraction
                .as_ref()
                .map(|e| e.extraction.end.to_string())
                .unwrap_or_default(),
            pre.map(|p| p.timeline.sample_rate.to_string())
                .unwrap_or_default(),
            result
                .output
                .as_ref()
                .map(|o| o.sha256.as_str())
                .unwrap_or(""),
        ));
    }
    let models = results
        .first()
        .map(|(_, r)| {
            r.models
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "model_id": m.model_id, "revision": m.revision, "license": m.license,
                        "manifest_sha256": m.manifest_sha256, "worker_sha256": m.worker_sha256,
                        "inference": m.inference, "device": m.device, "threads": m.threads,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let summary = Summary {
        schema_version: 1,
        batch_id: batch.batch_id.clone(),
        config_sha256: batch.config_sha256.clone(),
        physical_files: inventory.entries.len(),
        unique_inputs: batch.progress.unique_inputs,
        pairs: batch.progress.pairs,
        pairs_with_target: batch.progress.pairs_with_target,
        pairs_reported: results.len(),
        inputs_without_target: results.iter().filter(|(i, _)| i.target.is_none()).count(),
        unresolved_manifest_entries: batch.progress.unresolved_manifest_entries.len(),
        by_outcome,
        by_action,
        speech_events_per_input: events,
        duration_ms: distribution(&mut durations),
        occurrences_total,
        occurrences_by_evidence: evidence,
        match_score_per_token: distribution(&mut scores),
        competitor_margin: distribution(&mut margins),
        fallback_used: fallback,
        extraction_enhanced: enhanced,
        post_validation_failed_checks: failed_checks,
        legacy_balanced_outcomes: legacy_balanced,
        legacy_enhancement_outcomes: legacy_enhancement,
        models,
        model_calls: batch.progress.model_calls,
        model_items: batch.progress.model_items,
        model_wall_ms: batch.progress.model_wall_ms,
        wall_ms: batch.progress.wall_ms,
        confidence: "uncalibrated",
    };
    let write = |name: &str, bytes: &[u8]| {
        std::fs::write(out.join(name), bytes).map_err(|e| AssetError::io(name, e))
    };
    write(
        "summary.json",
        &serde_json::to_vec_pretty(&summary).expect("сериализуемо"),
    )?;
    write("items.jsonl", items_jsonl.as_bytes())?;
    write("items.tsv", items_tsv.as_bytes())?;
    write("occurrences.tsv", occ_tsv.as_bytes())?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_quantiles() {
        let d = distribution(&mut [5.0, 1.0, 3.0, 2.0, 4.0]);
        assert_eq!((d.min, d.p50, d.max), (Some(1.0), Some(3.0), Some(5.0)));
        let empty = distribution(&mut []);
        assert_eq!(empty.count, 0);
        assert_eq!(empty.p50, None);
    }
}
