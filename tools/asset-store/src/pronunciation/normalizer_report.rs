//! Отчёты нормализатора: агрегаты корпуса, построчные таблицы, воспроизводимая
//! стратифицированная выборка, сводка ручных решений и офлайн HTML «до/после».
//!
//! Отчёт пишется только в новый каталог, явно заданный пользователем. HTML не
//! загружает внешних ресурсов (CSP запрещает сеть), а все строки из данных
//! вставляются через `textContent`: имена файлов не интерпретируются как HTML.
//! Медиа копируются под именами `<sha256>.mp3`, без исходных имён.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::normalizer::{GateOutcome, NormalizationResult, Severity, Variant};
use super::normalizer_batch::{self, Inventory, NormalizerBatch, ResultCache};
use crate::hashing::sha256_hex;
use crate::{AssetError, ErrorCode};

const QUANTILES: [f64; 11] = [
    0.0, 0.01, 0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95, 0.99, 1.0,
];

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

/// Метрика строки: имя и извлечение из результата.
type Metric = (&'static str, fn(&NormalizationResult) -> Option<f64>);

fn metrics() -> Vec<Metric> {
    vec![
        ("input_bytes", |r| Some(r.input.byte_length as f64)),
        ("duration_ms", |r| {
            r.analysis.as_ref().map(|a| a.duration_ms)
        }),
        ("integrated_lufs", |r| {
            r.analysis.as_ref().and_then(|a| a.integrated_lufs)
        }),
        ("short_window_lufs", |r| {
            r.analysis.as_ref().and_then(|a| a.short_window_lufs)
        }),
        ("measured_lufs", |r| {
            r.plan.as_ref().and_then(|p| p.gain.measured_lufs)
        }),
        ("true_peak_dbtp", |r| {
            r.analysis.as_ref().map(|a| a.true_peak_dbtp)
        }),
        ("sample_peak_dbfs", |r| {
            r.analysis.as_ref().map(|a| a.sample_peak_dbfs)
        }),
        ("rms_dbfs", |r| r.analysis.as_ref().map(|a| a.rms_dbfs)),
        ("noise_floor_dbfs", |r| {
            r.analysis.as_ref().map(|a| a.noise_floor_dbfs)
        }),
        ("speech_level_dbfs", |r| {
            r.analysis.as_ref().map(|a| a.speech_level_dbfs)
        }),
        ("leading_below_threshold_ms", |r| {
            r.analysis.as_ref().map(|a| a.leading_below_threshold_ms)
        }),
        ("trailing_below_threshold_ms", |r| {
            r.analysis.as_ref().map(|a| a.trailing_below_threshold_ms)
        }),
        ("leading_below_weak_ms", |r| {
            r.analysis.as_ref().map(|a| a.leading_below_weak_ms)
        }),
        ("trailing_below_weak_ms", |r| {
            r.analysis.as_ref().map(|a| a.trailing_below_weak_ms)
        }),
        ("active_ms", |r| r.analysis.as_ref().map(|a| a.active_ms)),
        ("longest_internal_pause_ms", |r| {
            r.analysis.as_ref().map(|a| a.longest_internal_pause_ms)
        }),
        ("near_full_scale_ratio", |r| {
            r.analysis
                .as_ref()
                .map(|a| a.clipping.near_full_scale_ratio)
        }),
        ("dc_offset_abs", |r| {
            r.analysis.as_ref().map(|a| a.dc_offset.abs())
        }),
        ("stereo_difference_db", |r| {
            r.analysis.as_ref().and_then(|a| a.stereo_difference_db)
        }),
        ("desired_gain_db", |r| {
            r.plan.as_ref().and_then(|p| p.gain.desired_db)
        }),
        ("applied_gain_db", |r| {
            r.plan.as_ref().map(|p| p.gain.applied_db)
        }),
        ("shortfall_db", |r| {
            r.plan.as_ref().and_then(|p| p.gain.shortfall_db)
        }),
        ("trim_leading_ms", |r| {
            r.plan.as_ref().map(|p| p.leading.trim_ms)
        }),
        ("trim_trailing_ms", |r| {
            r.plan.as_ref().map(|p| p.trailing.trim_ms)
        }),
        ("output_bytes", |r| {
            r.output.as_ref().map(|o| o.byte_length as f64)
        }),
        ("codec_snr_db", |r| {
            r.post_qc.as_ref().and_then(|q| q.codec_snr_db)
        }),
        ("after_true_peak_dbtp", |r| {
            r.post_qc
                .as_ref()
                .and_then(|q| q.analysis.as_ref())
                .map(|a| a.true_peak_dbtp)
        }),
        ("after_measured_lufs", |r| {
            let after = r.post_qc.as_ref()?.analysis.as_ref()?;
            match r.plan.as_ref()?.gain.method {
                super::normalizer::LoudnessMethod::Integrated => {
                    after.integrated_lufs.or(after.short_window_lufs)
                }
                _ => after.short_window_lufs,
            }
        }),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Distribution {
    pub count: usize,
    pub missing: usize,
    pub mean: Option<f64>,
    pub quantiles: BTreeMap<String, f64>,
}

fn distribution(values: &[Option<f64>]) -> Distribution {
    let present: Vec<f64> = values
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    let mut quantiles = BTreeMap::new();
    for q in QUANTILES {
        if let Some(value) = super::dsp::percentile(&present, q) {
            quantiles.insert(format!("p{:02}", (q * 100.0).round() as u32), value);
        }
    }
    Distribution {
        count: present.len(),
        missing: values.len() - present.len(),
        mean: (!present.is_empty()).then(|| present.iter().sum::<f64>() / present.len() as f64),
        quantiles,
    }
}

fn paths_by_sha(inventory: &Inventory) -> BTreeMap<&str, Vec<&str>> {
    let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in &inventory.entries {
        if let Some(sha) = &entry.sha256 {
            map.entry(sha.as_str())
                .or_default()
                .push(entry.path.as_str());
        }
    }
    map
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub schema_version: u32,
    pub batch_id: String,
    pub mode: super::normalizer::Mode,
    pub profile: super::normalizer::NormalizerProfile,
    pub toolchain: super::normalizer::Toolchain,
    pub config_sha256: String,
    pub inventory_sha256: String,
    pub inventory: InventorySummary,
    pub results: usize,
    pub missing_results: usize,
    pub outcomes: BTreeMap<String, usize>,
    pub proposed_variants: BTreeMap<String, usize>,
    pub reasons: BTreeMap<String, usize>,
    pub loudness_methods: BTreeMap<String, usize>,
    pub edge_confidence: BTreeMap<String, usize>,
    pub post_qc_failed_checks: BTreeMap<String, usize>,
    pub sample_rates: BTreeMap<String, usize>,
    pub channels: BTreeMap<String, usize>,
    pub metrics: BTreeMap<String, Distribution>,
    pub source_errors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct InventorySummary {
    pub mp3_files: usize,
    pub readable_files: usize,
    pub unreadable_files: usize,
    pub unique_sha256: usize,
    pub duplicate_groups: usize,
    pub files_in_duplicate_groups: usize,
    pub total_bytes: u64,
    pub skipped: BTreeMap<String, usize>,
}

pub fn summarize(
    batch: &NormalizerBatch,
    inventory: &Inventory,
    results: &[NormalizationResult],
) -> Summary {
    let groups = paths_by_sha(inventory);
    let mut skipped = BTreeMap::new();
    for entry in &inventory.skipped {
        *skipped.entry(entry.reason.clone()).or_default() += 1;
    }
    let count =
        |items: &mut BTreeMap<String, usize>, key: String| *items.entry(key).or_default() += 1;
    let (mut outcomes, mut variants, mut reasons, mut methods, mut confidence, mut failed) = (
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let (mut rates, mut channels) = (BTreeMap::new(), BTreeMap::new());
    for outcome in GateOutcome::ALL {
        outcomes.insert(outcome.as_str().to_owned(), 0);
    }
    for result in results {
        count(&mut outcomes, result.gate.outcome.as_str().into());
        count(
            &mut variants,
            match result.gate.proposed_variant {
                Some(Variant::Original) => "original",
                Some(Variant::Processed) => "processed",
                None => "none",
            }
            .into(),
        );
        for reason in &result.gate.reasons {
            count(&mut reasons, reason.code.clone());
        }
        if let Some(plan) = &result.plan {
            count(
                &mut methods,
                format!("{:?}", plan.gain.method).to_lowercase(),
            );
            count(
                &mut confidence,
                format!("leading_{:?}", plan.leading.confidence).to_lowercase(),
            );
            count(
                &mut confidence,
                format!("trailing_{:?}", plan.trailing.confidence).to_lowercase(),
            );
        }
        if let Some(qc) = &result.post_qc {
            for check in qc.checks.iter().filter(|c| !c.passed) {
                count(&mut failed, check.id.clone());
            }
        }
        if let Some(probe) = &result.input.probe {
            count(&mut rates, probe.sample_rate.to_string());
            count(&mut channels, probe.channels.to_string());
        }
    }
    let metrics = metrics()
        .into_iter()
        .map(|(name, extract)| {
            (
                name.to_owned(),
                distribution(&results.iter().map(extract).collect::<Vec<_>>()),
            )
        })
        .collect();
    let unique = groups.len();
    Summary {
        schema_version: 1,
        batch_id: batch.batch_id.clone(),
        mode: batch.mode,
        profile: batch.profile.clone(),
        toolchain: batch.toolchain.clone(),
        config_sha256: batch.config_sha256.clone(),
        inventory_sha256: batch.inventory_sha256.clone(),
        inventory: InventorySummary {
            mp3_files: inventory.entries.len(),
            readable_files: inventory
                .entries
                .iter()
                .filter(|e| e.sha256.is_some())
                .count(),
            unreadable_files: inventory
                .entries
                .iter()
                .filter(|e| e.sha256.is_none())
                .count(),
            unique_sha256: unique,
            duplicate_groups: groups.values().filter(|p| p.len() > 1).count(),
            files_in_duplicate_groups: groups.values().filter(|p| p.len() > 1).map(Vec::len).sum(),
            total_bytes: inventory.entries.iter().map(|e| e.byte_length).sum(),
            skipped,
        },
        results: results.len(),
        missing_results: unique.saturating_sub(results.len()),
        outcomes,
        proposed_variants: variants,
        reasons,
        loudness_methods: methods,
        edge_confidence: confidence,
        post_qc_failed_checks: failed,
        sample_rates: rates,
        channels,
        metrics,
        source_errors: batch.progress.source_errors.len(),
    }
}

fn reason_codes(result: &NormalizationResult) -> String {
    result
        .gate
        .reasons
        .iter()
        .filter(|r| r.severity >= Severity::Warning)
        .map(|r| r.code.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn tsv_cell(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

fn number(value: Option<f64>) -> String {
    value.map_or_else(String::new, |v| format!("{v:.3}"))
}

/// Строки TSV: одна на уникальный вход; дубли перечислены в `paths`.
pub fn tsv(inventory: &Inventory, results: &[NormalizationResult]) -> String {
    let paths = paths_by_sha(inventory);
    let metrics = metrics();
    let mut out = String::from("input_sha256\tpaths\toutcome\tproposed\treasons");
    for (name, _) in &metrics {
        out.push('\t');
        out.push_str(name);
    }
    out.push('\n');
    for result in results {
        let names = paths
            .get(result.input.sha256.as_str())
            .map(|p| p.join(" | "))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}",
            result.input.sha256,
            tsv_cell(&names),
            result.gate.outcome.as_str(),
            match result.gate.proposed_variant {
                Some(Variant::Original) => "original",
                Some(Variant::Processed) => "processed",
                None => "",
            },
            reason_codes(result)
        ));
        for (_, extract) in &metrics {
            out.push('\t');
            out.push_str(&number(extract(result)));
        }
        out.push('\n');
    }
    out
}

pub fn jsonl(inventory: &Inventory, results: &[NormalizationResult]) -> String {
    let paths = paths_by_sha(inventory);
    let mut out = String::new();
    for result in results {
        let row = serde_json::json!({
            "paths": paths.get(result.input.sha256.as_str()).cloned().unwrap_or_default(),
            "result": result,
        });
        out.push_str(&serde_json::to_string(&row).expect("строка сериализуема"));
        out.push('\n');
    }
    out
}

// ---------- Стратифицированная выборка ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stratum {
    pub id: String,
    pub description: String,
    /// Элементы страты, реально пригодные для отбора (для страт крайних
    /// значений — только с конечной метрикой сортировки).
    pub available: usize,
    pub selected: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleItem {
    pub input_sha256: String,
    pub paths: Vec<String>,
    pub strata: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleSet {
    pub schema_version: u32,
    pub kind: String,
    pub batch_id: String,
    pub config_sha256: String,
    pub inventory_sha256: String,
    pub seed: u64,
    pub per_stratum: usize,
    pub strata: Vec<Stratum>,
    pub items: Vec<SampleItem>,
}

enum Order {
    Random,
    Ascending(fn(&NormalizationResult) -> Option<f64>),
    Descending(fn(&NormalizationResult) -> Option<f64>),
}

type Filter = fn(&NormalizationResult) -> bool;

fn has_reason(result: &NormalizationResult, prefix: &str) -> bool {
    result
        .gate
        .reasons
        .iter()
        .any(|r| r.code.starts_with(prefix))
}

fn strata() -> Vec<(&'static str, &'static str, Filter, Order)> {
    use GateOutcome::*;
    let analysis = |r: &NormalizationResult| r.analysis.is_some();
    vec![
        (
            "random_accept",
            "случайные ACCEPT",
            |r| r.gate.outcome == Accept,
            Order::Random,
        ),
        (
            "random_keep_original",
            "случайные KEEP_ORIGINAL",
            |r| r.gate.outcome == KeepOriginal,
            Order::Random,
        ),
        (
            "random_review",
            "случайные REVIEW_REQUIRED",
            |r| r.gate.outcome == ReviewRequired,
            Order::Random,
        ),
        (
            "reject",
            "REJECT",
            |r| r.gate.outcome == Reject,
            Order::Random,
        ),
        (
            "quietest",
            "самые тихие",
            analysis,
            Order::Ascending(|r| r.plan.as_ref()?.gain.measured_lufs),
        ),
        (
            "loudest",
            "самые громкие",
            analysis,
            Order::Descending(|r| r.plan.as_ref()?.gain.measured_lufs),
        ),
        (
            "largest_gain",
            "наибольшее усиление",
            |r| r.plan.as_ref().is_some_and(|p| p.gain.applied_db > 0.0),
            Order::Descending(|r| r.plan.as_ref().map(|p| p.gain.applied_db)),
        ),
        (
            "largest_attenuation",
            "наибольшее ослабление",
            |r| r.plan.as_ref().is_some_and(|p| p.gain.applied_db < 0.0),
            Order::Ascending(|r| r.plan.as_ref().map(|p| p.gain.applied_db)),
        ),
        (
            "longest_leading_trim",
            "наибольшая обрезка начала",
            |r| r.plan.as_ref().is_some_and(|p| p.leading.trim_frames > 0),
            Order::Descending(|r| r.plan.as_ref().map(|p| p.leading.trim_ms)),
        ),
        (
            "longest_trailing_trim",
            "наибольшая обрезка конца",
            |r| r.plan.as_ref().is_some_and(|p| p.trailing.trim_frames > 0),
            Order::Descending(|r| r.plan.as_ref().map(|p| p.trailing.trim_ms)),
        ),
        (
            "shortest_kept_lead",
            "минимальная тишина перед речью после обрезки (риск начала)",
            |r| r.plan.as_ref().is_some_and(|p| p.leading.trim_frames > 0),
            Order::Ascending(|r| {
                r.analysis
                    .as_ref()
                    .map(|a| a.leading_below_threshold_ms - a.leading_below_weak_ms)
            }),
        ),
        (
            "uncertain_boundary",
            "неуверенные границы",
            |r| has_reason(r, "uncertain_"),
            Order::Random,
        ),
        (
            "weak_edge_activity",
            "слабая активность у края",
            |r| has_reason(r, "leading_weak") || has_reason(r, "trailing_weak"),
            Order::Random,
        ),
        (
            "longest_internal_pause",
            "длинные внутренние паузы",
            analysis,
            Order::Descending(|r| r.analysis.as_ref().map(|a| a.longest_internal_pause_ms)),
        ),
        (
            "clipping",
            "признаки перегрузки",
            |r| has_reason(r, "source_clipping"),
            Order::Descending(|r| {
                r.analysis
                    .as_ref()
                    .map(|a| a.clipping.near_full_scale_ratio)
            }),
        ),
        (
            "noisiest_floor",
            "самый высокий шумовой пол",
            analysis,
            Order::Descending(|r| r.analysis.as_ref().map(|a| a.noise_floor_dbfs)),
        ),
        (
            "shortest",
            "самые короткие",
            analysis,
            Order::Ascending(|r| r.analysis.as_ref().map(|a| a.duration_ms)),
        ),
        (
            "longest",
            "самые длинные",
            analysis,
            Order::Descending(|r| r.analysis.as_ref().map(|a| a.duration_ms)),
        ),
        (
            "short_clip_loudness",
            "громкость по коротким окнам",
            |r| has_reason(r, "short_clip_loudness"),
            Order::Random,
        ),
        (
            "lowest_codec_snr",
            "наименьший SNR кодирования",
            |r| r.post_qc.as_ref().is_some_and(|q| q.codec_snr_db.is_some()),
            Order::Ascending(|r| r.post_qc.as_ref()?.codec_snr_db),
        ),
        (
            "post_qc_failed",
            "пост-QC не пройден",
            |r| has_reason(r, "post_qc_"),
            Order::Random,
        ),
    ]
}

/// Детерминированная выборка: одинаковые seed, конфигурация и результаты дают
/// одинаковый набор. Случайный порядок — сортировка по SHA-256(seed, вход).
pub fn sample(
    batch: &NormalizerBatch,
    inventory: &Inventory,
    results: &[NormalizationResult],
    seed: u64,
    per_stratum: usize,
) -> SampleSet {
    let paths = paths_by_sha(inventory);
    let key = |sha: &str| sha256_hex(format!("{seed}:{sha}").as_bytes());
    let mut membership: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut first_seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (id, description, filter, order) in strata() {
        let mut candidates: Vec<&NormalizationResult> =
            results.iter().filter(|r| filter(r)).collect();
        match order {
            Order::Random => candidates.sort_by_cached_key(|r| key(&r.input.sha256)),
            Order::Ascending(metric) | Order::Descending(metric) => {
                // Без конечной метрики сортировки элемент нельзя отобрать в
                // страту крайних значений: он не считается доступным.
                candidates.retain(|r| metric(r).is_some_and(f64::is_finite));
                candidates.sort_by(|a, b| {
                    let (x, y) = (metric(a).unwrap_or(0.0), metric(b).unwrap_or(0.0));
                    let ordering = x.total_cmp(&y);
                    let ordering = if matches!(order, Order::Descending(_)) {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    ordering.then_with(|| a.input.sha256.cmp(&b.input.sha256))
                });
            }
        }
        let available = candidates.len();
        let selected: Vec<String> = candidates
            .iter()
            .take(per_stratum)
            .map(|r| r.input.sha256.clone())
            .collect();
        for sha in &selected {
            let entry = membership.entry(sha.clone()).or_default();
            if entry.is_empty() {
                first_seen.push(sha.clone());
            }
            entry.push(id.into());
        }
        out.push(Stratum {
            id: id.into(),
            description: description.into(),
            available,
            selected,
        });
    }
    SampleSet {
        schema_version: 1,
        kind: "normalizer_listening_sample".into(),
        batch_id: batch.batch_id.clone(),
        config_sha256: batch.config_sha256.clone(),
        inventory_sha256: batch.inventory_sha256.clone(),
        seed,
        per_stratum,
        strata: out,
        items: first_seen
            .into_iter()
            .map(|sha| SampleItem {
                paths: paths
                    .get(sha.as_str())
                    .map(|p| p.iter().map(|s| s.to_string()).collect())
                    .unwrap_or_default(),
                strata: membership.remove(&sha).unwrap_or_default(),
                input_sha256: sha,
            })
            .collect(),
    }
}

// ---------- Ручные решения ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Обработанный (или предложенный) вариант звучит приемлемо.
    Acceptable,
    /// Обрезка задела звук.
    BadTrim,
    /// Обработка ухудшила звучание.
    Degraded,
    /// Проблема исходника, не обработки.
    SourceProblem,
    Unsure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub input_sha256: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decisions {
    pub schema_version: u32,
    pub kind: String,
    pub batch_id: String,
    pub config_sha256: String,
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewSummary {
    pub schema_version: u32,
    pub batch_id: String,
    pub config_sha256: String,
    pub decisions: usize,
    pub by_outcome_and_verdict: BTreeMap<String, BTreeMap<String, usize>>,
    pub by_stratum_and_verdict: BTreeMap<String, BTreeMap<String, usize>>,
    /// ACCEPT, который человек счёл плохой обрезкой или ухудшением.
    pub false_accepts: Vec<String>,
    /// REVIEW_REQUIRED/REJECT, который человек счёл приемлемым.
    pub conservative_flags: Vec<String>,
    pub metrics_by_verdict: BTreeMap<String, BTreeMap<String, Distribution>>,
}

/// Выборка принадлежит пакету, только если совпали ID, конфигурация и
/// инвентаризация: одинаковый ID бывает у пакетов разных рабочих каталогов.
fn sample_matches(sample: &SampleSet, batch: &NormalizerBatch) -> bool {
    sample.batch_id == batch.batch_id
        && sample.config_sha256 == batch.config_sha256
        && sample.inventory_sha256 == batch.inventory_sha256
}

pub fn review_summary(
    batch: &NormalizerBatch,
    results: &[NormalizationResult],
    decisions: &Decisions,
    sample: Option<&SampleSet>,
) -> Result<ReviewSummary, AssetError> {
    if decisions.schema_version != 1
        || decisions.kind != "normalizer_review_decisions"
        || decisions.batch_id != batch.batch_id
        || decisions.config_sha256 != batch.config_sha256
    {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "решения относятся к другой схеме, пакету или конфигурации",
        ));
    }
    if let Some(sample) = sample
        && !sample_matches(sample, batch)
    {
        return Err(invalid(
            "выборка относится к другому пакету, инвентаризации или конфигурации",
        ));
    }
    let by_sha: BTreeMap<&str, &NormalizationResult> = results
        .iter()
        .map(|r| (r.input.sha256.as_str(), r))
        .collect();
    let mut seen = BTreeSet::new();
    let mut summary = ReviewSummary {
        schema_version: 1,
        batch_id: batch.batch_id.clone(),
        config_sha256: batch.config_sha256.clone(),
        decisions: decisions.decisions.len(),
        by_outcome_and_verdict: BTreeMap::new(),
        by_stratum_and_verdict: BTreeMap::new(),
        false_accepts: Vec::new(),
        conservative_flags: Vec::new(),
        metrics_by_verdict: BTreeMap::new(),
    };
    let strata: BTreeMap<&str, &Vec<String>> = sample
        .map(|s| {
            s.items
                .iter()
                .map(|i| (i.input_sha256.as_str(), &i.strata))
                .collect()
        })
        .unwrap_or_default();
    let mut grouped: BTreeMap<String, Vec<&NormalizationResult>> = BTreeMap::new();
    for decision in &decisions.decisions {
        if !seen.insert(decision.input_sha256.as_str()) {
            return Err(invalid("повтор решения для одного входа"));
        }
        let result = by_sha
            .get(decision.input_sha256.as_str())
            .ok_or_else(|| invalid("решение ссылается на вход вне пакета"))?;
        let verdict = serde_json::to_value(decision.verdict)
            .expect("вердикт сериализуем")
            .as_str()
            .expect("строка")
            .to_owned();
        *summary
            .by_outcome_and_verdict
            .entry(result.gate.outcome.as_str().into())
            .or_default()
            .entry(verdict.clone())
            .or_default() += 1;
        for stratum in strata
            .get(decision.input_sha256.as_str())
            .into_iter()
            .flat_map(|s| s.iter())
        {
            *summary
                .by_stratum_and_verdict
                .entry(stratum.clone())
                .or_default()
                .entry(verdict.clone())
                .or_default() += 1;
        }
        match (result.gate.outcome, decision.verdict) {
            (GateOutcome::Accept, Verdict::BadTrim | Verdict::Degraded) => {
                summary.false_accepts.push(decision.input_sha256.clone())
            }
            (GateOutcome::ReviewRequired | GateOutcome::Reject, Verdict::Acceptable) => summary
                .conservative_flags
                .push(decision.input_sha256.clone()),
            _ => {}
        }
        grouped.entry(verdict).or_default().push(result);
    }
    for (verdict, items) in grouped {
        summary.metrics_by_verdict.insert(
            verdict,
            metrics()
                .into_iter()
                .map(|(name, extract)| {
                    (
                        name.to_owned(),
                        distribution(&items.iter().map(|r| extract(r)).collect::<Vec<_>>()),
                    )
                })
                .collect(),
        );
    }
    Ok(summary)
}

// ---------- Запись отчёта ----------

pub enum Include<'a> {
    All,
    Problems,
    Sample(&'a SampleSet),
}

pub struct ReportRequest<'a> {
    pub out_dir: &'a Path,
    pub html: bool,
    pub include: Include<'a>,
    pub media_limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportOutcome {
    pub files: Vec<String>,
    pub html_items: usize,
    pub media_files: usize,
    pub media_skipped: usize,
}

fn create_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), AssetError> {
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(dir.join(name))
        .map_err(|e| AssetError::io("запись файла отчёта", e))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| AssetError::io("запись файла отчёта", e))
}

/// Безопасная вставка JSON в `<script type="application/json">`.
fn embed_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value)
        .expect("данные отчёта сериализуемы")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Атомарная публикация отчёта: всё пишется во временный каталог рядом с
/// целевым (`.<имя>.partial-<pid>-<время>`), который затем переименовывается
/// в целевое имя без замены существующего (`RENAME_NOREPLACE`). После ошибки
/// временный каталог этого запуска удаляется; после аварийной остановки
/// остаётся только он, под другим именем, и повтор с тем же `--out` возможен.
pub fn write_report(
    batch: &NormalizerBatch,
    inventory: &Inventory,
    cache: &ResultCache,
    results: &[NormalizationResult],
    request: ReportRequest<'_>,
) -> Result<ReportOutcome, AssetError> {
    use rustix::fs::{Mode, OFlags, RenameFlags, mkdirat, open, renameat_with};
    let out = normalizer_batch::resolve_path(request.out_dir)?;
    let input_root = Path::new(&batch.input_root);
    if out.starts_with(input_root) || input_root.starts_with(&out) {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "каталог отчёта не может пересекаться с каталогом входа",
        ));
    }
    let (Some(parent), Some(name)) = (out.parent(), out.file_name()) else {
        return Err(invalid("у каталога отчёта нет родительского каталога"));
    };
    if std::fs::symlink_metadata(&out).is_ok() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "каталог отчёта должен быть новым (не существовать заранее)",
        ));
    }
    let parent_fd = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| AssetError::io("родительский каталог отчёта", e.into()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut temporary = std::ffi::OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".partial-{}-{nanos}", std::process::id()));
    mkdirat(
        &parent_fd,
        temporary.as_os_str(),
        Mode::from_raw_mode(0o700),
    )
    .map_err(|e| AssetError::io("временный каталог отчёта", e.into()))?;
    let staging = parent.join(&temporary);
    let written =
        write_contents(batch, inventory, cache, results, &request, &staging).and_then(|outcome| {
            File::open(&staging)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| AssetError::io("синхронизация отчёта", e))?;
            renameat_with(
                &parent_fd,
                temporary.as_os_str(),
                &parent_fd,
                name,
                RenameFlags::NOREPLACE,
            )
            .map_err(|e| {
                if e == rustix::io::Errno::EXIST {
                    AssetError::new(
                        ErrorCode::IdentityConflict,
                        "каталог отчёта появился во время записи; отчёт не опубликован",
                    )
                } else {
                    AssetError::io("публикация каталога отчёта", e.into())
                }
            })?;
            Ok(outcome)
        });
    match written {
        Ok(outcome) => {
            let _ = File::from(parent_fd).sync_all();
            Ok(outcome)
        }
        Err(error) => {
            // Удаляется только временный каталог этого запуска; ссылки внутри
            // не разыменовываются (`remove_dir_all` не следует ссылкам).
            if let Err(cleanup) = std::fs::remove_dir_all(&staging) {
                tracing::warn!(
                    operation = "normalizer_report",
                    code = "report_staging_cleanup_failed",
                    error = %cleanup,
                    "Не удалось удалить временный каталог отчёта"
                );
            }
            Err(error)
        }
    }
}

fn write_contents(
    batch: &NormalizerBatch,
    inventory: &Inventory,
    cache: &ResultCache,
    results: &[NormalizationResult],
    request: &ReportRequest<'_>,
    absolute: &Path,
) -> Result<ReportOutcome, AssetError> {
    let summary = summarize(batch, inventory, results);
    let mut files = vec![
        "summary.json".to_owned(),
        "items.jsonl".into(),
        "items.tsv".into(),
    ];
    create_new(
        absolute,
        "summary.json",
        &serde_json::to_vec_pretty(&summary).expect("сводка"),
    )?;
    create_new(
        absolute,
        "items.jsonl",
        jsonl(inventory, results).as_bytes(),
    )?;
    create_new(absolute, "items.tsv", tsv(inventory, results).as_bytes())?;
    let mut outcome = ReportOutcome {
        files: Vec::new(),
        html_items: 0,
        media_files: 0,
        media_skipped: 0,
    };
    if request.html {
        let selected: Vec<&NormalizationResult> = match &request.include {
            Include::All => results.iter().collect(),
            Include::Problems => results
                .iter()
                .filter(|r| {
                    r.gate.outcome != GateOutcome::KeepOriginal
                        && (r.gate.outcome != GateOutcome::Accept
                            || r.gate
                                .reasons
                                .iter()
                                .any(|x| x.severity >= Severity::Warning))
                })
                .collect(),
            Include::Sample(sample) => {
                if !sample_matches(sample, batch) {
                    return Err(invalid(
                        "выборка относится к другому пакету, инвентаризации или конфигурации",
                    ));
                }
                let wanted: BTreeSet<&str> = sample
                    .items
                    .iter()
                    .map(|i| i.input_sha256.as_str())
                    .collect();
                results
                    .iter()
                    .filter(|r| wanted.contains(r.input.sha256.as_str()))
                    .collect()
            }
        };
        let media = absolute.join("media");
        std::fs::create_dir(&media).map_err(|e| AssetError::io("каталог media отчёта", e))?;
        let root = normalizer_batch::open_input_root(batch)?;
        let paths = paths_by_sha(inventory);
        let strata: BTreeMap<&str, &Vec<String>> = match &request.include {
            Include::Sample(sample) => sample
                .items
                .iter()
                .map(|i| (i.input_sha256.as_str(), &i.strata))
                .collect(),
            _ => BTreeMap::new(),
        };
        let mut rows = Vec::new();
        let mut copied = BTreeSet::new();
        for (index, result) in selected.iter().enumerate() {
            let within = index < request.media_limit;
            let mut original = None;
            let mut processed = None;
            if within {
                let sha = &result.input.sha256;
                if let Some(path) = paths.get(sha.as_str()).and_then(|p| p.first()) {
                    let bytes = normalizer_batch::read_input(&root, path, sha)?;
                    if copied.insert(sha.clone()) {
                        create_new(&media, &format!("{sha}.mp3"), &bytes)?;
                        outcome.media_files += 1;
                    }
                    original = Some(format!("media/{sha}.mp3"));
                }
                if let Some(output) = result.output.as_ref().filter(|o| o.storage_path.is_some()) {
                    let bytes = cache.variant_bytes(output)?;
                    if copied.insert(output.sha256.clone()) {
                        create_new(&media, &format!("{}.mp3", output.sha256), &bytes)?;
                        outcome.media_files += 1;
                    }
                    processed = Some(format!("media/{}.mp3", output.sha256));
                }
            } else {
                outcome.media_skipped += 1;
            }
            let metric_values: serde_json::Map<String, serde_json::Value> = metrics()
                .into_iter()
                .map(|(name, extract)| (name.to_owned(), serde_json::json!(extract(result))))
                .collect();
            rows.push(serde_json::json!({
                "sha": result.input.sha256,
                "paths": paths.get(result.input.sha256.as_str()).cloned().unwrap_or_default(),
                "outcome": result.gate.outcome.as_str(),
                "proposed": result.gate.proposed_variant,
                "verified": result.gate.post_qc_verified,
                "reasons": result.gate.reasons,
                "checks": result.post_qc.as_ref().map(|q| &q.checks),
                "strata": strata.get(result.input.sha256.as_str()),
                "metrics": metric_values,
                "original": original,
                "processed": processed,
            }));
        }
        outcome.html_items = rows.len();
        let data = serde_json::json!({
            "batch_id": batch.batch_id,
            "config_sha256": batch.config_sha256,
            "profile": {"id": batch.profile.id, "version": batch.profile.version},
            "mode": batch.mode,
            "outcomes": summary.outcomes,
            "missing_results": summary.missing_results,
            "unique_inputs": summary.inventory.unique_sha256,
            "rows": rows,
        });
        create_new(absolute, "report.html", html(&embed_json(&data)).as_bytes())?;
        files.push("report.html".into());
        files.push("media/".into());
    }
    outcome.files = files;
    Ok(outcome)
}

fn html(data: &str) -> String {
    HTML_TEMPLATE.replace("__DATA__", data)
}

const HTML_TEMPLATE: &str = r#"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; media-src 'self' file:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<meta name="referrer" content="no-referrer">
<title>Нормализация: до и после</title>
<style>
:root { color-scheme: light dark; --bg:#fafafa; --fg:#1d1d1f; --muted:#6b6b70; --card:#fff; --line:#ddd;
  --accept:#1f7a3a; --keep:#3b5bab; --review:#9a6400; --reject:#b3261e; }
@media (prefers-color-scheme: dark) { :root { --bg:#161618; --fg:#ececf0; --muted:#a0a0a8; --card:#202024; --line:#38383e; } }
* { box-sizing: border-box; }
body { margin:0; padding:16px; background:var(--bg); color:var(--fg); font:14px/1.45 system-ui, sans-serif; }
header { display:flex; flex-wrap:wrap; gap:12px; align-items:center; margin-bottom:12px; }
h1 { font-size:18px; margin:0; }
.muted { color:var(--muted); }
.filters { display:flex; flex-wrap:wrap; gap:8px; align-items:center; margin-bottom:12px; }
.filters label { white-space:nowrap; }
input[type=search], select, textarea { font:inherit; color:inherit; background:var(--card); border:1px solid var(--line); border-radius:6px; padding:4px 6px; }
button { font:inherit; padding:4px 10px; border-radius:6px; border:1px solid var(--line); background:var(--card); color:inherit; cursor:pointer; }
.row { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:10px 12px; margin-bottom:10px; }
.top { display:flex; flex-wrap:wrap; gap:8px; align-items:baseline; }
.badge { font-weight:600; padding:1px 8px; border-radius:10px; color:#fff; font-size:12px; }
.ACCEPT { background:var(--accept);} .KEEP_ORIGINAL { background:var(--keep);} .REVIEW_REQUIRED { background:var(--review);} .REJECT { background:var(--reject);}
.name { font-weight:600; overflow-wrap:anywhere; }
.players { display:grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap:8px; margin:8px 0; }
.players audio { width:100%; }
.metrics { display:grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap:2px 12px; font-size:12px; font-variant-numeric: tabular-nums; }
.reasons { font-size:12px; margin:6px 0 0; padding-left:18px; }
.sev-review, .sev-reject { color:var(--reject); } .sev-warning { color:var(--review); }
.verdict { display:flex; flex-wrap:wrap; gap:8px; align-items:center; margin-top:8px; }
.verdict textarea { flex:1 1 220px; min-height:28px; }
.warn { color:var(--reject); font-weight:600; }
</style>
</head>
<body>
<header>
  <h1>Нормализация: до и после</h1>
  <span class="muted" id="meta"></span>
</header>
<p class="muted">ACCEPT означает лишь техническую пригодность варианта. Чтение и фонетику проверяет человек. Отчёт не обращается к сети.</p>
<p class="warn" id="incomplete" hidden></p>
<p class="muted">Оценки хранятся в этом браузере отдельно по каждой записи и сразу видны в других вкладках того же отчёта. Одну и ту же запись в двух вкладках не правьте одновременно: сохранится последнее изменение. Итог выгружайте кнопкой «Сохранить решения».</p>
<div class="filters">
  <label><input type="checkbox" value="ACCEPT" checked> ACCEPT</label>
  <label><input type="checkbox" value="KEEP_ORIGINAL" checked> KEEP_ORIGINAL</label>
  <label><input type="checkbox" value="REVIEW_REQUIRED" checked> REVIEW_REQUIRED</label>
  <label><input type="checkbox" value="REJECT" checked> REJECT</label>
  <select id="reason"><option value="">любая причина</option></select>
  <input type="search" id="search" placeholder="поиск по имени">
  <label><input type="checkbox" id="unrated"> без оценки</label>
  <button id="export" type="button">Сохранить решения (JSON)</button>
  <span class="muted" id="count"></span>
</div>
<main id="list"></main>
<script type="application/json" id="data">__DATA__</script>
<script>
"use strict";
const data = JSON.parse(document.getElementById("data").textContent);
// Каждое решение — отдельный ключ: правки разных записей в разных вкладках
// не перезаписывают друг друга (прежде весь набор сохранялся одним снимком).
const storeKey = "normalizer-decisions:" + data.batch_id + ":" + data.config_sha256;
const itemPrefix = storeKey + ":item:";
let decisions = {};
function readItem(raw) { try { const d = JSON.parse(raw); return d && d.verdict ? { verdict: String(d.verdict), note: String(d.note || "") } : null; } catch (e) { return null; } }
function loadDecisions() {
  const next = {};
  try {
    const legacy = JSON.parse(localStorage.getItem(storeKey) || "{}") || {};
    for (const [sha, d] of Object.entries(legacy)) { const item = readItem(JSON.stringify(d)); if (item) next[sha] = item; }
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key && key.startsWith(itemPrefix)) { const item = readItem(localStorage.getItem(key)); if (item) next[key.slice(itemPrefix.length)] = item; else delete next[key.slice(itemPrefix.length)]; }
    }
  } catch (e) {}
  decisions = next;
}
function persistItem(sha) {
  try {
    if (decisions[sha]) localStorage.setItem(itemPrefix + sha, JSON.stringify(decisions[sha]));
    else localStorage.setItem(itemPrefix + sha, "null");
  } catch (e) {}
}
loadDecisions();
window.addEventListener("storage", event => {
  if (!event.key || !(event.key === storeKey || event.key.startsWith(itemPrefix))) return;
  loadDecisions();
  // Не перерисовываем список под курсором: несохранённая заметка не теряется.
  const active = document.activeElement;
  if (!(active && document.getElementById("list").contains(active))) render();
});
if (data.missing_results > 0) {
  const note = document.getElementById("incomplete");
  note.hidden = false;
  note.textContent = "Пакет обработан не полностью: нет результатов для " + data.missing_results + " из " + data.unique_inputs + " входов. Отчёт не описывает весь корпус.";
}
const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text !== undefined) n.textContent = text; return n; };
document.getElementById("meta").textContent = data.batch_id + " · " + data.profile.id + " v" + data.profile.version + " · " + data.mode + " · " + data.rows.length + " записей";
const reasonSelect = document.getElementById("reason");
[...new Set(data.rows.flatMap(r => r.reasons.map(x => x.code)))].sort().forEach(code => reasonSelect.appendChild(el("option", null, code)));
const fmt = v => (v === null || v === undefined) ? "—" : (Math.abs(v) >= 1000 ? v.toFixed(0) : v.toFixed(2));
const verdicts = [["", "— оценка —"], ["acceptable", "приемлемо"], ["bad_trim", "плохая обрезка"], ["degraded", "ухудшение"], ["source_problem", "проблема исходника"], ["unsure", "не уверен"]];
const shown = ["duration_ms","measured_lufs","after_measured_lufs","applied_gain_db","shortfall_db","true_peak_dbtp","after_true_peak_dbtp","trim_leading_ms","trim_trailing_ms","noise_floor_dbfs","longest_internal_pause_ms","codec_snr_db","input_bytes","output_bytes"];
function player(label, src) {
  const box = el("div");
  box.appendChild(el("div", "muted", label));
  if (src) { const a = el("audio"); a.controls = true; a.preload = "none"; a.src = src; box.appendChild(a); }
  else box.appendChild(el("div", "muted", "нет файла"));
  return box;
}
function render() {
  const allowed = new Set([...document.querySelectorAll(".filters input[type=checkbox][value]")].filter(c => c.checked).map(c => c.value));
  const reason = reasonSelect.value; const query = document.getElementById("search").value.toLowerCase();
  const unrated = document.getElementById("unrated").checked;
  const list = document.getElementById("list"); list.replaceChildren();
  let count = 0;
  for (const row of data.rows) {
    if (!allowed.has(row.outcome)) continue;
    if (reason && !row.reasons.some(x => x.code === reason)) continue;
    if (query && !row.paths.join(" ").toLowerCase().includes(query)) continue;
    if (unrated && decisions[row.sha] && decisions[row.sha].verdict) continue;
    count++;
    const card = el("section", "row");
    const top = el("div", "top");
    top.appendChild(el("span", "badge " + row.outcome, row.outcome));
    top.appendChild(el("span", "name", row.paths[0] || row.sha));
    if (row.paths.length > 1) top.appendChild(el("span", "muted", "+" + (row.paths.length - 1) + " дубл."));
    top.appendChild(el("span", "muted", "предложен: " + (row.proposed || "нет") + (row.verified ? "" : " (без пост-QC)")));
    if (row.strata) top.appendChild(el("span", "muted", "страты: " + row.strata.join(", ")));
    card.appendChild(top);
    const players = el("div", "players");
    players.appendChild(player("Оригинал", row.original));
    players.appendChild(player("Обработанный", row.processed));
    card.appendChild(players);
    const metrics = el("div", "metrics");
    for (const name of shown) metrics.appendChild(el("span", null, name + ": " + fmt(row.metrics[name])));
    card.appendChild(metrics);
    const reasons = el("ul", "reasons");
    for (const r of row.reasons) reasons.appendChild(el("li", "sev-" + r.severity, r.code + " — " + r.message));
    for (const c of (row.checks || []).filter(c => !c.passed)) reasons.appendChild(el("li", "sev-review", "пост-QC " + c.id + ": " + fmt(c.value) + " (предел " + fmt(c.limit) + ")"));
    card.appendChild(reasons);
    const verdict = el("div", "verdict");
    const select = el("select");
    for (const [value, label] of verdicts) { const o = el("option", null, label); o.value = value; select.appendChild(o); }
    const note = el("textarea"); note.placeholder = "заметка";
    const current = decisions[row.sha] || {};
    select.value = current.verdict || ""; note.value = current.note || "";
    const save = () => { if (select.value) decisions[row.sha] = { verdict: select.value, note: note.value }; else delete decisions[row.sha]; persistItem(row.sha); };
    select.addEventListener("change", save); note.addEventListener("change", save);
    verdict.appendChild(select); verdict.appendChild(note);
    card.appendChild(verdict);
    list.appendChild(card);
  }
  document.getElementById("count").textContent = "показано: " + count;
}
document.querySelectorAll(".filters input, .filters select").forEach(n => n.addEventListener("input", render));
document.getElementById("export").addEventListener("click", () => {
  loadDecisions();
  const payload = { schema_version: 1, kind: "normalizer_review_decisions", batch_id: data.batch_id, config_sha256: data.config_sha256,
    decisions: Object.entries(decisions).map(([sha, d]) => ({ input_sha256: sha, verdict: d.verdict, note: d.note || "" })) };
  const url = URL.createObjectURL(new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" }));
  const a = el("a"); a.href = url; a.download = "decisions-" + data.batch_id + ".json"; document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
render();
</script>
</body>
</html>
"#;
