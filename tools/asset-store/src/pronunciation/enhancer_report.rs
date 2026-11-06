//! Машинный отчёт пакета улучшения речи.
//!
//! Отвечает на вопросы: сколько входов действительно улучшено, сколько
//! оставлено без изменений, сколько нейровариантов отклонено и почему,
//! сколько отложено по классам причин, сколько сбоев backend, какие модель,
//! воркер и профиль использованы и каков расход ресурсов. Полнота считается
//! отдельно по уникальным SHA и по физическим файлам. HTML нет: сравнение
//! на слух — добровольная диагностика, не условие обработки.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::enhancer::{EnhancementOutcome, EnhancementResult, Selection};
use super::enhancer_batch::EnhancerBatch;
use super::normalizer::{NextAction, Severity};
use super::normalizer_batch::Inventory;
use super::normalizer_report::{self as nreport, Distribution};
use crate::AssetError;
use crate::hashing::sha256_hex;

pub const SUMMARY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct Completeness {
    pub unique_sha256: usize,
    pub readable_files: usize,
    pub results: usize,
    pub missing_results: usize,
    pub results_files: usize,
    pub missing_results_files: usize,
    pub complete: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub outcomes: BTreeMap<String, usize>,
    pub next_actions: BTreeMap<String, usize>,
    pub outcomes_by_files: BTreeMap<String, usize>,
    pub next_actions_by_files: BTreeMap<String, usize>,
    pub selected: BTreeMap<String, usize>,
    pub reasons: BTreeMap<String, usize>,
    pub limitation_origins: BTreeMap<String, usize>,
    /// Коды ограничений у входов с `deferred_automatic_processing`.
    pub deferred_reasons: BTreeMap<String, usize>,
    pub qc_failures: BTreeMap<String, usize>,
    pub triage_signals: BTreeMap<String, usize>,
    /// Исход Balanced → исход улучшения.
    pub transitions: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub schema_version: u32,
    pub batch_id: String,
    pub base_batch_id: String,
    pub config_sha256: String,
    pub base_config_sha256: String,
    pub inventory_sha256: String,
    pub completeness: Completeness,
    pub counts: Counts,
    /// То же по входам, которые Balanced отложил (`deferred_automatic_processing`).
    pub base_deferred: Counts,
    pub base_deferred_unique: usize,
    pub backend_errors: BackendErrors,
    pub model: serde_json::Value,
    pub profile: serde_json::Value,
    pub resources: Resources,
    pub metrics: BTreeMap<String, Distribution>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendErrors {
    /// Уникальные входы, у которых последний запуск завершился сбоем среды.
    pub inputs: usize,
    pub attempts: u64,
    pub source_errors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resources {
    pub inference_runs: u64,
    pub inferred_audio_seconds: f64,
    pub inference_wall_seconds: f64,
    /// Время процесса воркера на секунду аудио (включая запуск и загрузку модели).
    pub real_time_factor: Option<f64>,
}

fn bump(map: &mut BTreeMap<String, usize>, key: impl Into<String>, by: usize) {
    *map.entry(key.into()).or_default() += by;
}

fn qc_value(result: &EnhancementResult, id: &str) -> Option<f64> {
    result
        .qc
        .as_ref()
        .and_then(|q| q.checks.iter().find(|c| c.id == id))
        .and_then(|c| c.value)
}

fn count(results: &[&EnhancementResult], files: &BTreeMap<&str, Vec<&str>>) -> Counts {
    let mut c = Counts::default();
    for r in results {
        let n = files.get(r.input.sha256.as_str()).map_or(1, Vec::len);
        let outcome = r.gate.outcome.as_str();
        let action = r.gate.next_action.as_str();
        bump(&mut c.outcomes, outcome, 1);
        bump(&mut c.next_actions, action, 1);
        bump(&mut c.outcomes_by_files, outcome, n);
        bump(&mut c.next_actions_by_files, action, n);
        bump(
            &mut c.selected,
            match r.gate.selected {
                Some(Selection::Enhanced) => "enhanced",
                Some(Selection::Input) => "input",
                None => "none",
            },
            1,
        );
        bump(
            &mut c.transitions,
            format!("{} -> {}", r.base.outcome.as_str(), outcome),
            1,
        );
        for reason in &r.gate.reasons {
            bump(&mut c.reasons, reason.code.clone(), 1);
            if reason.severity == Severity::Limitation {
                let origin = reason
                    .origin
                    .map(|o| serde_json::to_value(o).expect("источник сериализуем"))
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "unknown".into());
                bump(&mut c.limitation_origins, origin, 1);
                if r.gate.next_action.as_str() == "deferred_automatic_processing" {
                    bump(&mut c.deferred_reasons, reason.code.clone(), 1);
                }
            }
        }
        if let Some(qc) = &r.qc {
            for check in qc.checks.iter().filter(|c| !c.passed || !c.resolved) {
                bump(&mut c.qc_failures, check.id.clone(), 1);
            }
        }
        if let Some(t) = &r.triage {
            for signal in &t.signals {
                let key = signal.split(':').next().unwrap_or(signal);
                bump(&mut c.triage_signals, key, 1);
            }
        }
    }
    c
}

pub fn summarize(
    batch: &EnhancerBatch,
    inventory: &Inventory,
    results: &[EnhancementResult],
) -> Summary {
    let files = nreport::paths_by_sha(inventory);
    let unique = inventory.unique_inputs().len();
    let readable = inventory
        .entries
        .iter()
        .filter(|e| e.sha256.is_some())
        .count();
    let results_files: usize = results
        .iter()
        .map(|r| files.get(r.input.sha256.as_str()).map_or(0, Vec::len))
        .sum();
    let all: Vec<&EnhancementResult> = results.iter().collect();
    let deferred: Vec<&EnhancementResult> = results
        .iter()
        .filter(|r| r.base.next_action == Some(NextAction::DeferredAutomaticProcessing))
        .collect();
    let metric = |f: &dyn Fn(&EnhancementResult) -> Option<f64>| {
        nreport::distribution(&results.iter().map(f).collect::<Vec<_>>())
    };
    let inferred: Vec<&EnhancementResult> =
        results.iter().filter(|r| r.inference.is_some()).collect();
    let mut metrics = BTreeMap::new();
    metrics.insert(
        "triage_estimated_snr_db".into(),
        metric(&|r| r.triage.as_ref().map(|t| t.estimated_snr_db)),
    );
    metrics.insert(
        "triage_noise_floor_dbfs".into(),
        metric(&|r| r.triage.as_ref().map(|t| t.noise_floor_dbfs)),
    );
    for id in [
        "noise_reduction",
        "strong_speech_preserved",
        "strong_speech_p90",
        "weak_sounds_preserved",
        "edges_preserved",
        "no_added_energy",
        "spectral_balance",
        "loudness_preserved",
    ] {
        let values: Vec<Option<f64>> = inferred.iter().map(|r| qc_value(r, id)).collect();
        metrics.insert(format!("qc_{id}"), nreport::distribution(&values));
    }
    metrics.insert(
        "model_lsnr_mean_db".into(),
        metric(&|r| r.inference.as_ref().and_then(|i| i.run.lsnr_mean_db)),
    );
    metrics.insert(
        "enhanced_output_lufs".into(),
        metric(&|r| {
            (r.gate.outcome == EnhancementOutcome::Enhanced)
                .then(|| {
                    r.balanced
                        .as_ref()
                        .and_then(|b| b.post_qc.as_ref())
                        .and_then(|q| q.analysis.as_ref())
                        .and_then(|a| a.integrated_lufs.or(a.short_window_lufs))
                })
                .flatten()
        }),
    );
    let audio_s = batch.resources.inferred_audio_ms / 1000.0;
    let wall_s = batch.resources.inference_wall_ms / 1000.0;
    Summary {
        schema_version: SUMMARY_SCHEMA_VERSION,
        batch_id: batch.batch_id.clone(),
        base_batch_id: batch.base_batch_id.clone(),
        config_sha256: batch.config_sha256.clone(),
        base_config_sha256: batch.base_config_sha256.clone(),
        inventory_sha256: batch.base_inventory_sha256.clone(),
        completeness: Completeness {
            unique_sha256: unique,
            readable_files: readable,
            results: results.len(),
            missing_results: unique - results.len(),
            results_files,
            missing_results_files: readable - results_files,
            complete: results.len() == unique,
        },
        counts: count(&all, &files),
        base_deferred: count(&deferred, &files),
        base_deferred_unique: deferred.len(),
        backend_errors: BackendErrors {
            inputs: batch.progress.infrastructure_failures.len(),
            attempts: batch
                .progress
                .infrastructure_failures
                .values()
                .map(|v| u64::from(*v))
                .sum(),
            source_errors: batch.progress.source_errors.len(),
        },
        model: serde_json::json!({
            "backend": batch.backend.backend,
            "model_name": batch.backend.model_name,
            "model_variant": batch.backend.model_variant,
            "model_manifest_sha256": batch.backend.model_manifest_sha256,
            "worker_sha256": batch.backend.worker_sha256,
            "delay_samples": batch.backend.delay_samples,
            "params": batch.backend.params,
            "runtime": batch.backend.report["runtime"],
            "worker": batch.backend.report["worker"],
            "source": batch.backend.report["model"]["source"],
            "license": batch.backend.report["model"]["license"],
        }),
        profile: serde_json::json!({"id": batch.profile.id, "version": batch.profile.version}),
        resources: Resources {
            inference_runs: batch.resources.inference_runs,
            inferred_audio_seconds: audio_s,
            inference_wall_seconds: wall_s,
            real_time_factor: (audio_s > 0.0).then(|| wall_s / audio_s),
        },
        metrics,
    }
}

fn number(value: Option<f64>) -> String {
    value.map_or_else(String::new, |v| format!("{v:.3}"))
}

fn cell(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

pub fn tsv(inventory: &Inventory, results: &[EnhancementResult]) -> String {
    let files = nreport::paths_by_sha(inventory);
    let mut out = String::from(
        "input_sha256\tpaths\tbase_outcome\tbase_next_action\toutcome\tnext_action\tselected\tselected_sha256\ttriage_snr_db\tnoise_floor_dbfs\tnoise_reduction_db\tstrong_drop_median_db\tweak_drop_median_db\tlsnr_mean_db\treasons\n",
    );
    for r in results {
        let selected_sha = match r.gate.selected {
            Some(Selection::Enhanced) => r.output.as_ref().map(|o| o.sha256.clone()),
            Some(Selection::Input) => r.selected_input.as_ref().map(|s| s.sha256.clone()),
            None => None,
        };
        let row = [
            r.input.sha256.clone(),
            files
                .get(r.input.sha256.as_str())
                .map(|p| p.join(" | "))
                .unwrap_or_default(),
            r.base.outcome.as_str().into(),
            r.base
                .next_action
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default(),
            r.gate.outcome.as_str().into(),
            r.gate.next_action.as_str().into(),
            match r.gate.selected {
                Some(Selection::Enhanced) => "enhanced".into(),
                Some(Selection::Input) => "input".into(),
                None => String::new(),
            },
            selected_sha.unwrap_or_default(),
            number(r.triage.as_ref().map(|t| t.estimated_snr_db)),
            number(r.triage.as_ref().map(|t| t.noise_floor_dbfs)),
            number(qc_value(r, "noise_reduction")),
            number(qc_value(r, "strong_speech_preserved")),
            number(qc_value(r, "weak_sounds_preserved")),
            number(r.inference.as_ref().and_then(|i| i.run.lsnr_mean_db)),
            r.gate
                .reasons
                .iter()
                .map(|x| x.code.as_str())
                .collect::<Vec<_>>()
                .join(","),
        ];
        out.push_str(&row.iter().map(|v| cell(v)).collect::<Vec<_>>().join("\t"));
        out.push('\n');
    }
    out
}

pub fn jsonl(inventory: &Inventory, results: &[EnhancementResult]) -> String {
    let files = nreport::paths_by_sha(inventory);
    let mut out = String::new();
    for r in results {
        let line = serde_json::json!({
            "paths": files.get(r.input.sha256.as_str()).cloned().unwrap_or_default(),
            "result": r,
        });
        out.push_str(&serde_json::to_string(&line).expect("JSON сериализуем"));
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportOutcome {
    pub out_dir: String,
    pub summary: Summary,
    pub files: BTreeMap<String, String>,
}

/// Новый каталог отчёта: `summary.json`, `items.jsonl`, `items.tsv` и
/// `base_deferred.tsv` (входы, отложенные Balanced). Публикация атомарна.
pub fn write_report(
    batch: &EnhancerBatch,
    inventory: &Inventory,
    results: &[EnhancementResult],
    out_dir: &Path,
) -> Result<ReportOutcome, AssetError> {
    let summary = summarize(batch, inventory, results);
    let deferred: Vec<EnhancementResult> = results
        .iter()
        .filter(|r| r.base.next_action == Some(NextAction::DeferredAutomaticProcessing))
        .cloned()
        .collect();
    let contents: Vec<(&str, Vec<u8>)> = vec![
        (
            "summary.json",
            serde_json::to_vec_pretty(&summary).expect("сводка сериализуема"),
        ),
        ("items.jsonl", jsonl(inventory, results).into_bytes()),
        ("items.tsv", tsv(inventory, results).into_bytes()),
        ("base_deferred.tsv", tsv(inventory, &deferred).into_bytes()),
    ];
    let files: BTreeMap<String, String> = contents
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), sha256_hex(bytes)))
        .collect();
    let resolved = super::normalizer_batch::resolve_path(out_dir)?;
    nreport::publish_new_directory(
        out_dir,
        Path::new(&batch.input_root),
        "enhancer_report",
        |staging| {
            for (name, bytes) in &contents {
                nreport::create_new(staging, name, bytes)?;
            }
            Ok(())
        },
    )?;
    Ok(ReportOutcome {
        out_dir: resolved.to_string_lossy().into_owned(),
        summary,
        files,
    })
}
