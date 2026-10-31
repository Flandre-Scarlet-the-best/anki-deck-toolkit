//! Машинный контракт результата измерения (JSON) и его проверка.
//!
//! Результат хранит первичные наблюдения каждого прогона; агрегаты —
//! необязательная производная, которая обязана совпадать с прогонами.
//! Отсутствие значения выражается статусом с причиной и никогда не нулём.

use crate::catalog::{Catalog, LoadedScenario};
use crate::diagnostic::{Diagnostic, Diagnostics, Sink, field};
use crate::scenario::{IdentityMethod, InputRole};
use crate::sensitive;
use crate::vocabulary::{
    CacheState, MANDATORY_FACTS, MetricId, Network, RESULT_SCHEMA_VERSION, Scope, Source, Unit,
    WorkloadKind, is_identifier, metric_key,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkResult {
    pub schema_version: u32,
    pub scenario: ScenarioRef,
    pub profile: ProfileRef,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputRecord>,
    pub provenance: Provenance,
    pub environment: BTreeMap<String, Fact>,
    pub outcome: Outcome,
    pub observation: Observation,
    /// Число выполненных прогревочных прогонов; их значения не хранятся.
    pub warmup_runs: u32,
    pub metrics: Vec<MetricRecord>,
    pub runs: Vec<RunRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aggregates: BTreeMap<String, Aggregate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioRef {
    pub id: String,
    pub module: String,
    pub revision: u32,
    pub kind: WorkloadKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub id: String,
    pub cache: CacheState,
    pub network: Network,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRecord {
    pub id: String,
    pub role: InputRole,
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub size: BTreeMap<String, Quantity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub method: IdentityMethod,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quantity {
    pub value: f64,
    pub unit: Unit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    pub version: String,
    /// Формат исходных данных, например `hyperfine-json-v2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// Технический provenance записи. SHA коммита — свойство записи результата,
/// а не постоянной документации: в журналы `docs/` он не переносится.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeState {
    pub module_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Время UTC в RFC 3339 с суффиксом `Z`.
    pub recorded_at: String,
    /// Измерительный движок или иной источник значений.
    pub measurement: Tool,
    /// Исполнитель, собравший запись (если отличается от движка).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<Tool>,
    pub code: CodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase", deny_unknown_fields)]
pub enum Fact {
    Known { value: String },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeStatus {
    /// Серия завершена, все обязательные метрики измерены в каждом прогоне.
    Success,
    /// Ни одного успешного прогона: отказ подготовки или каждого прогона.
    Failure,
    /// Есть успешные прогоны, но серия или набор метрик неполны.
    Partial,
    /// Сценарий не может измеряться в этом окружении; прогонов нет.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub status: OutcomeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Observation {
    /// Измеряемых прогонов нет.
    None,
    /// Одно наблюдение: валидно, но без статистических выводов.
    Single,
    /// Повторные прогоны: допустимы разброс и статистика.
    Repeated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Measured,
    Partial,
    Unavailable,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRecord {
    pub key: String,
    pub metric: MetricId,
    pub scope: Scope,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub unit: Unit,
    pub availability: Availability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Success,
    Failure,
    Timeout,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub index: u32,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub values: BTreeMap<String, MetricValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase", deny_unknown_fields)]
pub enum MetricValue {
    Measured { value: f64 },
    Unavailable { reason: String },
    Unsupported { reason: String },
    Failed { reason: String },
}

impl MetricValue {
    pub fn measured(&self) -> Option<f64> {
        match self {
            MetricValue::Measured { value } => Some(*value),
            _ => None,
        }
    }
}

/// Сводка по измеренным значениям успешных прогонов. `stddev` — выборочное
/// стандартное отклонение (n − 1), `null` для одного значения.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregate {
    pub count: u32,
    pub mean: f64,
    pub stddev: Option<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

impl Aggregate {
    /// Сводка значений или `None` для пустого набора.
    pub fn compute(values: &[f64]) -> Option<Aggregate> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let count = sorted.len();
        let mean = sorted.iter().sum::<f64>() / count as f64;
        let median = if count % 2 == 1 {
            sorted[count / 2]
        } else {
            (sorted[count / 2 - 1] + sorted[count / 2]) / 2.0
        };
        let stddev = (count > 1).then(|| {
            (sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count - 1) as f64).sqrt()
        });
        Some(Aggregate {
            count: count as u32,
            mean,
            stddev,
            median,
            min: sorted[0],
            max: sorted[count - 1],
        })
    }
}

impl BenchmarkResult {
    /// Значения метрики в успешных прогонах — основа агрегатов.
    pub fn successful_values(&self, key: &str) -> Vec<f64> {
        self.runs
            .iter()
            .filter(|run| run.status == RunStatus::Success)
            .filter_map(|run| run.values.get(key).and_then(MetricValue::measured))
            .collect()
    }

    /// Пересчитать агрегаты всех полностью или частично измеренных метрик.
    pub fn recompute_aggregates(&mut self) {
        self.aggregates = self
            .metrics
            .iter()
            .filter_map(|m| {
                Aggregate::compute(&self.successful_values(&m.key)).map(|a| (m.key.clone(), a))
            })
            .collect();
    }
}

/// Итог проверки документа результата.
#[derive(Debug)]
pub struct Checked {
    pub result: Option<BenchmarkResult>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Проверить JSON результата; с каталогом — также соответствие декларации.
pub fn check(text: &str, name: &str, catalog: Option<&Catalog>) -> Checked {
    let mut diagnostics = Diagnostics::default();
    let result = check_into(text, name, catalog, &mut diagnostics);
    let failed = diagnostics.has_errors();
    Checked {
        result: if failed { None } else { result },
        diagnostics: diagnostics.into_vec(),
    }
}

fn check_into(
    text: &str,
    name: &str,
    catalog: Option<&Catalog>,
    diagnostics: &mut Diagnostics,
) -> Option<BenchmarkResult> {
    let mut sink = diagnostics.at(name, None);
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            sink.error("json_syntax", None, error.to_string());
            return None;
        }
    };
    match value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(version) if version == u64::from(RESULT_SCHEMA_VERSION) => {}
        Some(version) => {
            sink.error(
                "unsupported_schema_version",
                field("schema_version"),
                format!(
                    "версия схемы результата {version} не поддерживается: этот инструмент понимает только {RESULT_SCHEMA_VERSION}. \
                     Обновите benchmark-contract или переведите результат по правилам эволюции схемы"
                ),
            );
            return None;
        }
        None => {
            sink.error(
                "unsupported_schema_version",
                field("schema_version"),
                format!("обязательное целое поле schema_version отсутствует; поддерживается {RESULT_SCHEMA_VERSION}"),
            );
            return None;
        }
    }
    scan_sensitive(&value, "", &mut sink);
    let result: BenchmarkResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => {
            sink.error("invalid_document", None, error.to_string());
            return None;
        }
    };
    let scenario_id = result.scenario.id.clone();
    let mut sink = diagnostics.at(name, Some(&scenario_id));
    check_standalone(&result, &mut sink);
    if let Some(catalog) = catalog {
        match catalog.scenario(&result.scenario.id) {
            None => sink.error("unknown_scenario", field("scenario.id"), "сценарий не найден в каталоге"),
            Some(declared) if declared.scenario.revision != result.scenario.revision => sink.warning(
                "historical_revision",
                field("scenario.revision"),
                format!(
                    "результат записан для ревизии {}, в каталоге ревизия {}: соответствие текущей декларации не проверяется, а сопоставление с результатами текущей ревизии запрещено",
                    result.scenario.revision, declared.scenario.revision
                ),
            ),
            Some(declared) => check_against_catalog(&result, declared, catalog, &mut sink),
        }
    }
    Some(result)
}

/// Чувствительные сведения проверяются по всем строкам и ключам документа.
fn scan_sensitive(value: &serde_json::Value, path: &str, sink: &mut Sink<'_>) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(reason) = sensitive::finding(text) {
                sink.error(
                    "sensitive_value",
                    field(path.trim_start_matches('.')),
                    format!("значение нельзя публиковать: {reason}"),
                );
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                scan_sensitive(item, &format!("{path}[{index}]"), sink);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let child = format!("{path}.{key}");
                if let Some(reason) = sensitive::finding(key) {
                    sink.error(
                        "sensitive_value",
                        field(child.trim_start_matches('.')),
                        format!("ключ нельзя публиковать: {reason}"),
                    );
                }
                scan_sensitive(item, &child, sink);
            }
        }
        _ => {}
    }
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Число дней в месяце григорианского календаря.
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction]Z` с существующей календарной датой.
pub fn is_utc_timestamp(text: &str) -> bool {
    let Some(body) = text.strip_suffix('Z') else {
        return false;
    };
    let (main, fraction) = match body.split_once('.') {
        Some((main, fraction)) => (main, Some(fraction)),
        None => (body, None),
    };
    if fraction
        .is_some_and(|f| f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let bytes = main.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    // parse::<u32> принимает ведущий «+», поэтому цифры проверяются отдельно.
    let number = |range: std::ops::Range<usize>| {
        let part = &main[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    match (
        number(0..4),
        number(5..7),
        number(8..10),
        number(11..13),
        number(14..16),
        number(17..19),
    ) {
        (Some(year), Some(month @ 1..=12), Some(day), Some(0..=23), Some(0..=59), Some(0..=60)) => {
            (1..=days_in_month(year, month)).contains(&day)
        }
        _ => false,
    }
}

fn check_standalone(result: &BenchmarkResult, sink: &mut Sink<'_>) {
    let scenario = &result.scenario;
    match scenario.id.split_once('.') {
        Some((module, name))
            if module == scenario.module && is_identifier(module) && is_identifier(name) => {}
        _ => sink.error(
            "invalid_scenario_id",
            field("scenario.id"),
            "ID сценария имеет вид «<модуль>.<имя>»",
        ),
    }
    if scenario.revision == 0 {
        sink.error(
            "invalid_revision",
            field("scenario.revision"),
            "ревизия начинается с 1",
        );
    }
    if !is_identifier(&result.profile.id) {
        sink.error(
            "invalid_identifier",
            field("profile.id"),
            "ID профиля — идентификатор",
        );
    }

    let provenance = &result.provenance;
    if !is_utc_timestamp(&provenance.recorded_at) {
        sink.error(
            "invalid_timestamp",
            field("provenance.recorded_at"),
            "время записи — UTC в RFC 3339 с суффиксом Z",
        );
    }
    for (path, tool) in std::iter::once(("provenance.measurement", &provenance.measurement))
        .chain(provenance.runner.as_ref().map(|t| ("provenance.runner", t)))
    {
        if tool.name.trim().is_empty() || tool.version.trim().is_empty() {
            sink.error(
                "missing_provenance",
                field(path),
                "укажите имя и версию источника",
            );
        }
    }
    let code = &provenance.code;
    if code.module_version.trim().is_empty() {
        sink.error(
            "missing_provenance",
            field("provenance.code.module_version"),
            "версия модуля обязательна",
        );
    }
    if code
        .source_tree_sha256
        .as_deref()
        .is_some_and(|v| !is_hex(v, 64))
    {
        sink.error(
            "invalid_identity",
            field("provenance.code.source_tree_sha256"),
            "ожидается SHA-256 в нижнем регистре",
        );
    }
    if code
        .git_commit
        .as_deref()
        .is_some_and(|v| !is_hex(v, 40) && !is_hex(v, 64))
    {
        sink.error(
            "invalid_identity",
            field("provenance.code.git_commit"),
            "ожидается полный идентификатор коммита в нижнем регистре",
        );
    }

    for fact in MANDATORY_FACTS {
        if !result.environment.contains_key(fact.as_str()) {
            sink.error(
                "missing_fact",
                field(format!("environment.{fact}")),
                "обязательный факт окружения отсутствует; неизвестное значение записывается статусом unavailable с причиной",
            );
        }
    }
    if let Some(Fact::Known { value }) = result.environment.get("module_version")
        && value != &code.module_version
    {
        sink.error(
            "inconsistent_provenance",
            field("environment.module_version"),
            "версия модуля в окружении расходится с provenance.code.module_version",
        );
    }
    for (name, fact) in &result.environment {
        if !is_identifier(name) {
            sink.error(
                "invalid_identifier",
                field(format!("environment.{name}")),
                "имя факта — идентификатор",
            );
        }
        match fact {
            Fact::Known { value } if value.trim().is_empty() => sink.error(
                "empty_text",
                field(format!("environment.{name}.value")),
                "пустое значение известного факта; используйте unavailable",
            ),
            Fact::Unavailable { reason } if reason.trim().is_empty() => sink.error(
                "missing_reason",
                field(format!("environment.{name}.reason")),
                "причина недоступности обязательна",
            ),
            _ => {}
        }
    }

    let mut input_ids = BTreeSet::new();
    for (index, input) in result.inputs.iter().enumerate() {
        let at = |name: &str| field(format!("inputs[{index}].{name}"));
        if !input_ids.insert(&input.id) {
            sink.error("duplicate_input", at("id"), "вход повторяется");
        }
        let valid_identity = match input.identity.method {
            IdentityMethod::ContentTreeSha256 | IdentityMethod::FileSha256 => {
                is_hex(&input.identity.value, 64)
            }
            IdentityMethod::DeclaredVersion => !input.identity.value.trim().is_empty(),
        };
        if !valid_identity {
            sink.error(
                "invalid_identity",
                at("identity.value"),
                "значение не соответствует способу идентификации",
            );
        }
        for (size_name, quantity) in &input.size {
            check_quantity(
                *quantity,
                &format!("inputs[{index}].size.{size_name}"),
                sink,
            );
        }
    }

    let mut keys = BTreeSet::new();
    for (index, metric) in result.metrics.iter().enumerate() {
        let at = |name: &str| field(format!("metrics[{index}].{name}"));
        if !keys.insert(metric.key.as_str()) {
            sink.error("duplicate_metric", at("key"), "метрика повторяется");
        }
        let expected = metric_key(metric.metric, metric.scope, metric.label.as_deref());
        if metric.key != expected {
            sink.error(
                "invalid_metric_key",
                at("key"),
                format!("канонический ключ — {expected}"),
            );
        }
        let definition = metric.metric.definition();
        if definition
            .measurement(metric.scope, metric.source)
            .is_none()
        {
            sink.error(
                "metric_scope_source",
                at("scope"),
                format!(
                    "{} не измеряется в сочетании {}/{}",
                    metric.metric, metric.scope, metric.source
                ),
            );
        }
        if metric.unit != definition.unit {
            sink.error(
                "unit_mismatch",
                at("unit"),
                format!("базовая единица {} — {}", metric.metric, definition.unit),
            );
        }
        if metric.metric.label_required() != metric.label.is_some() {
            sink.error(
                "invalid_label",
                at("label"),
                "метка обязательна только для метрик с экземплярами",
            );
        }
        let allowed = allowed_availability(result, &metric.key);
        if !allowed.contains(&metric.availability) {
            let names: Vec<&str> = allowed.iter().map(|a| availability_name(*a)).collect();
            sink.error(
                "inconsistent_availability",
                at("availability"),
                format!(
                    "доступность не соответствует значениям прогонов; допустимо: {}",
                    names.join(", ")
                ),
            );
        }
        if metric.availability != Availability::Measured
            && metric.reason.as_deref().is_none_or(|r| r.trim().is_empty())
        {
            sink.error(
                "missing_reason",
                at("reason"),
                "неполная или отсутствующая метрика требует причины",
            );
        }
    }

    for (position, run) in result.runs.iter().enumerate() {
        let at = |name: &str| field(format!("runs[{position}].{name}"));
        if run.index as usize != position {
            sink.error(
                "invalid_run_index",
                at("index"),
                "номера прогонов идут подряд с нуля",
            );
        }
        match run.status {
            RunStatus::Success if run.exit_code.is_none() => sink.error(
                "missing_exit_code",
                at("exit_code"),
                "успешный прогон фиксирует код завершения",
            ),
            RunStatus::Success => {}
            _ if run.reason.as_deref().is_none_or(|r| r.trim().is_empty()) => sink.error(
                "missing_reason",
                at("reason"),
                "неуспешный прогон требует причины",
            ),
            _ => {}
        }
        for key in &keys {
            if !run.values.contains_key(*key) {
                sink.error(
                    "missing_value",
                    at(&format!("values.{key}")),
                    "значение отсутствует: укажите измеренное значение или статус с причиной, а не пропускайте и не подменяйте нулём",
                );
            }
        }
        for (key, value) in &run.values {
            let value_at = format!("runs[{position}].values.{key}");
            let Some(metric) = result.metrics.iter().find(|m| &m.key == key) else {
                sink.error(
                    "unknown_metric",
                    field(value_at),
                    "метрика не объявлена в metrics",
                );
                continue;
            };
            match value {
                MetricValue::Measured { value } => check_quantity(
                    Quantity {
                        value: *value,
                        unit: metric.unit,
                    },
                    &value_at,
                    sink,
                ),
                MetricValue::Unavailable { reason }
                | MetricValue::Unsupported { reason }
                | MetricValue::Failed { reason } => {
                    if reason.trim().is_empty() {
                        sink.error(
                            "missing_reason",
                            field(value_at),
                            "причина отсутствия значения обязательна",
                        );
                    }
                }
            }
        }
    }

    let expected_observation = match result.runs.len() {
        0 => Observation::None,
        1 => Observation::Single,
        _ => Observation::Repeated,
    };
    if result.observation != expected_observation {
        sink.error(
            "inconsistent_observation",
            field("observation"),
            format!(
                "для {} прогонов вид наблюдения — {:?}",
                result.runs.len(),
                expected_observation
            )
            .to_lowercase(),
        );
    }

    let successes = result
        .runs
        .iter()
        .filter(|r| r.status == RunStatus::Success)
        .count();
    let primary = metric_key(MetricId::TimeWallClock, Scope::Command, None);
    let complete = result.runs.iter().all(|r| r.status == RunStatus::Success)
        && result.metrics.iter().any(|m| m.key == primary)
        && result.successful_values(&primary).len() == result.runs.len();
    let reason_missing = result
        .outcome
        .reason
        .as_deref()
        .is_none_or(|r| r.trim().is_empty());
    match result.outcome.status {
        OutcomeStatus::Success => {
            if result.runs.is_empty() || !complete {
                sink.error(
                    "inconsistent_outcome",
                    field("outcome.status"),
                    "success требует хотя бы одного прогона, успеха каждого прогона и первичной метрики time_wall_clock@command в каждом",
                );
            }
        }
        OutcomeStatus::Failure => {
            if successes > 0 {
                sink.error(
                    "inconsistent_outcome",
                    field("outcome.status"),
                    "failure при успешных прогонах: используйте partial",
                );
            }
        }
        OutcomeStatus::Partial => {
            if successes == 0 {
                sink.error(
                    "inconsistent_outcome",
                    field("outcome.status"),
                    "partial требует хотя бы одного успешного прогона",
                );
            }
        }
        OutcomeStatus::Unsupported => {
            if !result.runs.is_empty() {
                sink.error(
                    "inconsistent_outcome",
                    field("outcome.status"),
                    "unsupported означает, что прогонов не было",
                );
            }
        }
    }
    if result.outcome.status != OutcomeStatus::Success && reason_missing {
        sink.error(
            "missing_reason",
            field("outcome.reason"),
            "неуспешный итог требует причины",
        );
    }

    for (key, aggregate) in &result.aggregates {
        let at = |name: &str| field(format!("aggregates.{key}.{name}"));
        if !keys.contains(key.as_str()) {
            sink.error(
                "unknown_metric",
                field(format!("aggregates.{key}")),
                "агрегат метрики, которой нет в metrics",
            );
            continue;
        }
        let Some(expected) = Aggregate::compute(&result.successful_values(key)) else {
            sink.error(
                "aggregate_without_values",
                field(format!("aggregates.{key}")),
                "агрегат без первичных измерений: агрегаты считаются только по сохранённым значениям успешных прогонов",
            );
            continue;
        };
        if aggregate.count != expected.count {
            sink.error(
                "aggregate_mismatch",
                at("count"),
                format!("ожидается {}", expected.count),
            );
            continue;
        }
        for (name, got, want) in [
            ("mean", aggregate.mean, expected.mean),
            ("median", aggregate.median, expected.median),
            ("min", aggregate.min, expected.min),
            ("max", aggregate.max, expected.max),
        ] {
            if !close(got, want) {
                sink.error(
                    "aggregate_mismatch",
                    at(name),
                    format!("по прогонам получается {want}"),
                );
            }
        }
        match (aggregate.stddev, expected.stddev) {
            (None, None) => {}
            (Some(got), Some(want)) if close(got, want) => {}
            (_, None) => sink.error(
                "aggregate_mismatch",
                at("stddev"),
                "для одного значения stddev — null: разброс одиночного наблюдения неизвестен",
            ),
            (_, Some(want)) => sink.error(
                "aggregate_mismatch",
                at("stddev"),
                format!("по прогонам получается {want}"),
            ),
        }
    }
}

fn availability_name(value: Availability) -> &'static str {
    match value {
        Availability::Measured => "measured",
        Availability::Partial => "partial",
        Availability::Unavailable => "unavailable",
        Availability::Unsupported => "unsupported",
        Availability::Failed => "failed",
    }
}

/// Какие значения доступности согласуются со значениями прогонов.
fn allowed_availability(result: &BenchmarkResult, key: &str) -> Vec<Availability> {
    const MISSING: [Availability; 3] = [
        Availability::Unavailable,
        Availability::Unsupported,
        Availability::Failed,
    ];
    let values: Vec<&MetricValue> = result
        .runs
        .iter()
        .filter_map(|r| r.values.get(key))
        .collect();
    let measured = values.iter().filter(|v| v.measured().is_some()).count();
    if result.runs.is_empty() {
        return MISSING.to_vec();
    }
    if measured == result.runs.len() {
        return vec![Availability::Measured];
    }
    if measured > 0 {
        return vec![Availability::Partial];
    }
    let uniform =
        |wanted: fn(&MetricValue) -> bool| !values.is_empty() && values.iter().all(|v| wanted(v));
    if uniform(|v| matches!(v, MetricValue::Unsupported { .. })) {
        vec![Availability::Unsupported]
    } else if uniform(|v| matches!(v, MetricValue::Unavailable { .. })) {
        vec![Availability::Unavailable]
    } else if uniform(|v| matches!(v, MetricValue::Failed { .. })) {
        vec![Availability::Failed]
    } else {
        MISSING.to_vec()
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-3)
}

fn check_quantity(quantity: Quantity, at: &str, sink: &mut Sink<'_>) {
    if !quantity.value.is_finite() || quantity.value < 0.0 {
        sink.error(
            "invalid_value",
            field(at),
            "значение — конечное неотрицательное число",
        );
    } else if quantity.unit == Unit::Count && quantity.value.fract() != 0.0 {
        sink.error(
            "invalid_value",
            field(at),
            "количество событий — целое число",
        );
    }
}

fn check_against_catalog(
    result: &BenchmarkResult,
    declared: &LoadedScenario,
    catalog: &Catalog,
    sink: &mut Sink<'_>,
) {
    let scenario = &declared.scenario;
    if result.scenario.module != declared.module || result.scenario.kind != scenario.kind {
        sink.error(
            "catalog_mismatch",
            field("scenario"),
            "модуль или вид нагрузки расходятся с декларацией",
        );
    }
    match (
        catalog.profile(&result.profile.id),
        scenario.profiles.iter().any(|b| b.id == result.profile.id),
    ) {
        (Some(profile), true) => {
            if profile.cache != result.profile.cache || profile.network != result.profile.network {
                sink.error(
                    "catalog_mismatch",
                    field("profile"),
                    "состояние кэша или сеть расходятся с профилем каталога",
                );
            }
        }
        _ => sink.error(
            "unknown_profile",
            field("profile.id"),
            "профиль не привязан к сценарию",
        ),
    }

    for (name, value) in &result.parameters {
        match scenario.parameters.iter().find(|p| &p.name == name) {
            None => sink.error(
                "unknown_parameter",
                field(format!("parameters.{name}")),
                "параметр не объявлен",
            ),
            Some(param) if !param.record => sink.error(
                "unpublishable_parameter",
                field(format!("parameters.{name}")),
                "параметр объявлен с record = false и не публикуется",
            ),
            Some(param) if !param.choices.is_empty() && !param.choices.contains(value) => sink
                .error(
                    "invalid_parameter",
                    field(format!("parameters.{name}")),
                    "значение не входит в choices",
                ),
            Some(_) => {}
        }
    }
    // Профиль может сделать необязательный параметр обязательным через
    // required_parameters: без значения его args_append не подставляется.
    let profile_required: BTreeSet<&str> = scenario
        .profiles
        .iter()
        .find(|b| b.id == result.profile.id)
        .map(|b| b.required_parameters.iter().map(String::as_str).collect())
        .unwrap_or_default();
    for param in scenario.parameters.iter().filter(|p| p.record) {
        let required = param.required || profile_required.contains(param.name.as_str());
        if required && param.default.is_none() && !result.parameters.contains_key(&param.name) {
            sink.error(
                "missing_parameter",
                field(format!("parameters.{}", param.name)),
                "обязательный параметр не записан",
            );
        }
    }

    let recorded: BTreeSet<&str> = result.inputs.iter().map(|i| i.id.as_str()).collect();
    for input in &scenario.inputs {
        let Some(record) = result.inputs.iter().find(|r| r.id == input.id) else {
            // Без успешного прогона вход мог остаться неразрешённым:
            // идентичность загружаемого артефакта, например, сообщает только
            // отчёт прогона, а прервавшийся прогон её не получил. Значений,
            // которые сопоставлялись бы с другой нагрузкой, при этом нет.
            if result.runs.iter().any(|r| r.status == RunStatus::Success) {
                sink.error(
                    "missing_input",
                    field(format!("inputs.{}", input.id)),
                    "идентичность входа не записана",
                );
            }
            continue;
        };
        if record.role != input.role || record.identity.method != input.identity {
            sink.error(
                "catalog_mismatch",
                field(format!("inputs.{}", input.id)),
                "роль или способ идентификации расходятся с декларацией",
            );
        }
        for size in &input.size {
            match record.size.get(&size.name) {
                Some(quantity) if quantity.unit != size.unit => sink.error(
                    "unit_mismatch",
                    field(format!("inputs.{}.size.{}", input.id, size.name)),
                    format!("ожидается {}", size.unit),
                ),
                Some(_) => {}
                None => sink.error(
                    "missing_size",
                    field(format!("inputs.{}.size.{}", input.id, size.name)),
                    "объём входа не записан: без него результаты разных нагрузок неотличимы",
                ),
            }
        }
        for name in record.size.keys() {
            if !input.size.iter().any(|s| &s.name == name) {
                sink.error(
                    "unknown_size",
                    field(format!("inputs.{}.size.{name}", input.id)),
                    "характеристика не объявлена",
                );
            }
        }
    }
    for id in recorded {
        if !scenario.inputs.iter().any(|i| i.id == id) {
            sink.error(
                "unknown_input",
                field(format!("inputs.{id}")),
                "вход не объявлен",
            );
        }
    }

    let declared_keys: BTreeMap<String, _> =
        scenario.metrics.iter().map(|m| (m.key(), m)).collect();
    for metric in &result.metrics {
        match declared_keys.get(&metric.key) {
            None => sink.error(
                "unknown_metric",
                field(format!("metrics.{}", metric.key)),
                "метрика не запрошена сценарием",
            ),
            Some(request) => {
                if request.source != metric.source {
                    sink.error(
                        "catalog_mismatch",
                        field(format!("metrics.{}.source", metric.key)),
                        format!("сценарий запрашивает источник {}", request.source),
                    );
                }
                if request.required
                    && result.outcome.status == OutcomeStatus::Success
                    && metric.availability != Availability::Measured
                {
                    sink.error(
                        "inconsistent_outcome",
                        field(format!("metrics.{}", metric.key)),
                        "обязательная метрика измерена не во всех прогонах: итог не может быть success",
                    );
                }
            }
        }
    }
    for key in declared_keys.keys() {
        if !result.metrics.iter().any(|m| &m.key == key) {
            sink.error(
                "missing_metric",
                field(format!("metrics.{key}")),
                "запрошенная метрика отсутствует; недоступную метрику записывают со статусом и причиной",
            );
        }
    }

    let runs = result.runs.len() as u32;
    if runs > scenario.runs.max_runs {
        sink.error(
            "unbounded_runs",
            field("runs"),
            format!("прогонов больше max_runs = {}", scenario.runs.max_runs),
        );
    }
    if result.outcome.status == OutcomeStatus::Success && runs < scenario.runs.min_runs {
        sink.error(
            "inconsistent_outcome",
            field("runs"),
            format!(
                "success требует не меньше min_runs = {} прогонов; иначе итог partial",
                scenario.runs.min_runs
            ),
        );
    }
    // Успешен только прогон с кодом, который декларация считает ожидаемым.
    for (position, run) in result.runs.iter().enumerate() {
        if let (RunStatus::Success, Some(code)) = (run.status, run.exit_code)
            && !scenario.target.expect_exit_codes.contains(&code)
        {
            sink.error(
                "unexpected_exit_code",
                field(format!("runs[{position}].exit_code")),
                format!(
                    "код завершения {code} вне expect_exit_codes = {:?}: такой прогон — failure, а не success",
                    scenario.target.expect_exit_codes
                ),
            );
        }
    }
    // Прогревы идут до измерений: если измеряемые прогоны начались, все
    // объявленные прогревы завершены. Без прогонов серия могла оборваться
    // на setup или посреди прогрева, но лишних прогревов не бывает и тогда.
    // Тёплое состояние через setup профиля при warmup = 0 сюда не входит:
    // setup прогревом не считается.
    let declared_warmup = scenario.runs.warmup;
    let warmup_ok = if result.runs.is_empty() {
        result.warmup_runs <= declared_warmup
    } else {
        result.warmup_runs == declared_warmup
    };
    if !warmup_ok {
        sink.error(
            "warmup_mismatch",
            field("warmup_runs"),
            if result.profile.cache == CacheState::Cold && result.warmup_runs > 0 {
                "холодный профиль не допускает прогревов".to_owned()
            } else if result.runs.is_empty() {
                format!(
                    "прогревов {} больше объявленных runs.warmup = {declared_warmup}",
                    result.warmup_runs
                )
            } else {
                format!(
                    "измерения начинаются после всех runs.warmup = {declared_warmup} прогревов, записано {}",
                    result.warmup_runs
                )
            },
        );
    }

    let spec = &scenario.environment;
    for name in &spec.comparable {
        if !result.environment.contains_key(name) {
            sink.error(
                "missing_fact",
                field(format!("environment.{name}")),
                "факт сопоставимости не записан; неизвестное значение записывается статусом unavailable",
            );
        }
    }
    for name in result.environment.keys() {
        if !spec.record.contains(name) {
            sink.error(
                "unknown_fact",
                field(format!("environment.{name}")),
                "факт не объявлен в environment.record сценария",
            );
        }
    }
}
