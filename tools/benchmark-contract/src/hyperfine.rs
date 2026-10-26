//! Импорт экспорта Hyperfine 2.0 (`--export-json`, `schema_version: 2`) в
//! прогоны и метрики контракта результата.
//!
//! Формат сверен с исходниками тега `v2.0.0` (`src/export/json.rs`,
//! `src/benchmark/measurement.rs`): времена — секунды, память — байты,
//! счётчики — целые без `unit`; недоступные метрики опускаются; `exit_code`
//! равен `null`, если процесс завершён сигналом; `stddev` — выборочный (n − 1)
//! и `null` для одного прогона; прогревы в `measurements` не входят.
//!
//! Hyperfine — один из источников, а не формат проекта: импорт возвращает
//! только значения источника `hyperfine`. Метрики других источников
//! (cgroup, журнал модуля) добавляет исполнитель.

use crate::result::{
    Aggregate, Availability, MetricRecord, MetricValue, RunRecord, RunStatus, Tool,
};
use crate::scenario::MetricRequest;
use crate::vocabulary::{MetricId, Scope, Source};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

pub const SUPPORTED_SCHEMA_VERSION: u64 = 2;
pub const FORMAT: &str = "hyperfine-json-v2";

#[derive(Debug, Clone, Deserialize)]
pub struct Export {
    pub schema_version: u64,
    pub primary_metric: String,
    pub metadata: Metadata,
    pub results: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub hyperfine_version: String,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub platform: Platform,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Platform {
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParameterValue {
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// Команда после подстановки параметров. В результат не переносится:
    /// она может содержать пути пользователя, а цель запуска однозначно
    /// задают сценарий, его ревизия и параметры.
    pub command: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParameterValue>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub measurements: Vec<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub summary: BTreeMap<String, Summary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Summary {
    #[serde(default)]
    pub unit: Option<String>,
    pub count: u64,
    pub mean: f64,
    pub stddev: Option<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportError {
    Syntax(String),
    UnsupportedVersion(u64),
    Invalid(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Syntax(error) => write!(f, "экспорт Hyperfine не разбирается: {error}"),
            ImportError::UnsupportedVersion(version) => write!(
                f,
                "экспорт Hyperfine schema_version {version} не поддерживается: ожидается {SUPPORTED_SCHEMA_VERSION} (Hyperfine 2.x); формат 1.x несовместим"
            ),
            ImportError::Invalid(reason) => write!(f, "экспорт Hyperfine некорректен: {reason}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// Разобрать экспорт, отказав в неподдерживаемой версии до разбора полей.
pub fn parse(text: &str) -> Result<Export, ImportError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ImportError::Syntax(e.to_string()))?;
    match value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(SUPPORTED_SCHEMA_VERSION) => {}
        Some(version) => return Err(ImportError::UnsupportedVersion(version)),
        // В Hyperfine 1.x поля нет вовсе.
        None => return Err(ImportError::UnsupportedVersion(1)),
    }
    serde_json::from_value(value).map_err(|e| ImportError::Syntax(e.to_string()))
}

/// Соответствие имени метрики Hyperfine метрике и области охвата контракта.
pub fn mapping(name: &str) -> Option<(MetricId, Scope)> {
    let metric = MetricId::ALL.iter().copied().find(|m| m.as_str() == name)?;
    let scope = match metric {
        MetricId::TimeWallClock => Scope::Command,
        MetricId::TimeUser | MetricId::TimeSystem | MetricId::TimeCpu => Scope::ProcessTreeWaited,
        MetricId::MemoryPeakResident => Scope::LargestProcessInTree,
        m if m.is_hardware_counter() => Scope::ProcessTreeWaited,
        _ => return None,
    };
    Some((metric, scope))
}

/// Прогоны, метрики и агрегаты одного элемента `results` экспорта.
#[derive(Debug, Clone)]
pub struct Imported {
    pub measurement: Tool,
    pub start_time: Option<String>,
    pub platform: Platform,
    /// Имена параметров Hyperfine и их значения (для сверки с параметрами сценария).
    pub parameters: BTreeMap<String, String>,
    pub metrics: Vec<MetricRecord>,
    pub runs: Vec<RunRecord>,
    pub aggregates: BTreeMap<String, Aggregate>,
    /// Метрики экспорта, которые сценарий не запрашивал: перечислены, а не
    /// потеряны молча.
    pub unrequested: Vec<String>,
}

/// Импортировать `results[index]` для метрик сценария с источником `hyperfine`.
pub fn import(
    export: &Export,
    index: usize,
    requests: &[MetricRequest],
    expected_exit_codes: &[i32],
) -> Result<Imported, ImportError> {
    let entry = export
        .results
        .get(index)
        .ok_or_else(|| ImportError::Invalid(format!("нет results[{index}]")))?;
    let requested: Vec<&MetricRequest> = requests
        .iter()
        .filter(|r| r.source == Source::Hyperfine)
        .collect();
    for request in &requested {
        if mapping(request.metric.as_str()) != Some((request.metric, request.scope)) {
            return Err(ImportError::Invalid(format!(
                "метрика {} не экспортируется Hyperfine со scope {}",
                request.metric, request.scope
            )));
        }
    }

    let mut unrequested = BTreeSet::new();
    let mut runs = Vec::with_capacity(entry.measurements.len());
    for (position, measurement) in entry.measurements.iter().enumerate() {
        let exit_code = match measurement.get("exit_code") {
            None => {
                return Err(ImportError::Invalid(format!(
                    "measurements[{position}] без exit_code"
                )));
            }
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| {
                        ImportError::Invalid(format!("measurements[{position}].exit_code не целое"))
                    })?,
            ),
        };
        let (status, reason) = match exit_code {
            Some(code) if expected_exit_codes.contains(&code) => (RunStatus::Success, None),
            Some(code) => (
                RunStatus::Failure,
                Some(format!("код завершения {code} вне ожидаемых")),
            ),
            None => (
                RunStatus::Failure,
                Some("процесс завершён сигналом".to_owned()),
            ),
        };
        let mut values = BTreeMap::new();
        for request in &requested {
            let value = match measurement.get(request.metric.as_str()) {
                Some(raw) => MetricValue::Measured {
                    value: read_value(raw, request.metric, position)?,
                },
                None => MetricValue::Unavailable {
                    reason: "Hyperfine не экспортировал метрику: она недоступна на платформе или не выбрана --metrics".to_owned(),
                },
            };
            values.insert(request.key(), value);
        }
        for name in measurement.keys().filter(|k| k.as_str() != "exit_code") {
            if !requested.iter().any(|r| r.metric.as_str() == name) {
                unrequested.insert(name.clone());
            }
        }
        runs.push(RunRecord {
            index: position as u32,
            status,
            exit_code,
            reason,
            values,
        });
    }

    let mut metrics = Vec::new();
    let mut aggregates = BTreeMap::new();
    for request in &requested {
        let key = request.key();
        let measured = runs
            .iter()
            .filter(|r| r.values[&key].measured().is_some())
            .count();
        let (availability, reason) = if runs.is_empty() {
            (Availability::Unavailable, Some("прогонов нет".to_owned()))
        } else if measured == runs.len() {
            (Availability::Measured, None)
        } else if measured > 0 {
            (
                Availability::Partial,
                Some("метрика есть не во всех прогонах".to_owned()),
            )
        } else {
            (
                Availability::Unavailable,
                Some("Hyperfine не экспортировал метрику ни в одном прогоне".to_owned()),
            )
        };
        metrics.push(MetricRecord {
            key: key.clone(),
            metric: request.metric,
            scope: request.scope,
            source: Source::Hyperfine,
            label: None,
            unit: request.metric.base_unit(),
            availability,
            reason,
        });
        // Hyperfine 2.0 (`Metric::summarize`) сводит метрику по всем
        // measurements, включая неуспешные, и только при полном непустом
        // наборе значений: тогда сводка обязана быть в экспорте.
        if measured == runs.len()
            && let Some(expected) = Aggregate::compute(
                &runs
                    .iter()
                    .filter_map(|r| r.values[&key].measured())
                    .collect::<Vec<_>>(),
            )
        {
            let summary = entry.summary.get(request.metric.as_str()).ok_or_else(|| {
                ImportError::Invalid(format!(
                    "summary.{} отсутствует, хотя метрика есть во всех measurements: Hyperfine 2.0 в этом случае всегда пишет сводку",
                    request.metric
                ))
            })?;
            verify_summary(request.metric, summary, &expected)?;
        }
        // Агрегаты контракта — только по успешным прогонам.
        let successful: Vec<f64> = runs
            .iter()
            .filter(|r| r.status == RunStatus::Success)
            .filter_map(|r| r.values[&key].measured())
            .collect();
        if let Some(aggregate) = Aggregate::compute(&successful) {
            aggregates.insert(key, aggregate);
        }
    }

    Ok(Imported {
        measurement: Tool {
            name: "hyperfine".to_owned(),
            version: export.metadata.hyperfine_version.clone(),
            format: Some(FORMAT.to_owned()),
        },
        start_time: export.metadata.start_time.clone(),
        platform: export.metadata.platform.clone(),
        parameters: entry
            .parameters
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect(),
        metrics,
        runs,
        aggregates,
        unrequested: unrequested.into_iter().collect(),
    })
}

fn expected_unit(metric: MetricId) -> Option<&'static str> {
    match metric.base_unit() {
        crate::vocabulary::Unit::Second => Some("second"),
        crate::vocabulary::Unit::Byte => Some("byte"),
        _ => None,
    }
}

fn read_value(
    raw: &serde_json::Value,
    metric: MetricId,
    position: usize,
) -> Result<f64, ImportError> {
    let at = format!("measurements[{position}].{metric}");
    let unit = raw.get("unit").and_then(serde_json::Value::as_str);
    if unit != expected_unit(metric) {
        return Err(ImportError::Invalid(format!(
            "{at}: единица {:?}, ожидается {:?}; JSON Hyperfine 2 всегда пишет базовые единицы",
            unit,
            expected_unit(metric)
        )));
    }
    let value = raw
        .get("value")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| ImportError::Invalid(format!("{at}: нет числового value")))?;
    if !value.is_finite() || value < 0.0 {
        return Err(ImportError::Invalid(format!(
            "{at}: значение не конечное или отрицательное"
        )));
    }
    Ok(value)
}

fn verify_summary(
    metric: MetricId,
    summary: &Summary,
    aggregate: &Aggregate,
) -> Result<(), ImportError> {
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-3);
    let stddev_ok = match (summary.stddev, aggregate.stddev) {
        (None, None) => true,
        (Some(a), Some(b)) => close(a, b),
        _ => false,
    };
    if summary.unit.as_deref() != expected_unit(metric)
        || summary.count != u64::from(aggregate.count)
        || !close(summary.mean, aggregate.mean)
        || !close(summary.median, aggregate.median)
        || !close(summary.min, aggregate.min)
        || !close(summary.max, aggregate.max)
        || !stddev_ok
    {
        return Err(ImportError::Invalid(format!(
            "summary.{metric} не совпадает со значениями measurements: первичные данные и сводка противоречат друг другу"
        )));
    }
    Ok(())
}
