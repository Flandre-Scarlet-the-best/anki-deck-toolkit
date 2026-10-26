//! Можно ли сопоставлять два результата. Правила следуют из декларации:
//! сравнение допустимо только для одного сценария, ревизии и профиля, одной
//! нагрузки, одинаковых определений метрик и значимых фактов окружения.
//! Решения о регрессии здесь не принимаются.

use crate::result::{BenchmarkResult, Fact, OutcomeStatus};
use crate::scenario::{InputRole, Scenario};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub comparable: bool,
    /// Почему сопоставление запрещено.
    pub blockers: Vec<String>,
    /// Ожидаемые различия предмета сравнения (например, другое дерево кода).
    pub subject_differences: Vec<String>,
    /// Метрики, определение которых совпадает и которые измерены в обоих.
    pub metrics: Vec<String>,
    /// Подмножество `metrics`, у которого в каждом результате не меньше
    /// [`MIN_STATISTICAL_VALUES`] значений успешных прогонов.
    pub statistical_metrics: Vec<String>,
    /// Статистические выводы допустимы для каждой сопоставимой метрики.
    /// Сопоставимость значений от этого не зависит.
    pub statistically_supported: bool,
}

/// Наименьшее число успешных измерений, по которому оценивается разброс.
pub const MIN_STATISTICAL_VALUES: usize = 2;

pub fn compare(scenario: &Scenario, a: &BenchmarkResult, b: &BenchmarkResult) -> Verdict {
    let mut blockers = Vec::new();
    let mut subject_differences = Vec::new();

    for result in [a, b] {
        if result.scenario.id != scenario.id {
            blockers.push(format!(
                "результат относится к сценарию {}",
                result.scenario.id
            ));
        }
        if result.scenario.revision != scenario.revision {
            blockers.push(format!(
                "ревизия {} не совпадает с ревизией декларации {}",
                result.scenario.revision, scenario.revision
            ));
        }
        if matches!(
            result.outcome.status,
            OutcomeStatus::Failure | OutcomeStatus::Unsupported
        ) {
            blockers.push(
                format!("итог {:?} не содержит измерений", result.outcome.status).to_lowercase(),
            );
        }
    }
    if a.profile != b.profile {
        blockers.push(format!(
            "разные профили: {} и {}",
            a.profile.id, b.profile.id
        ));
    }

    for input in &scenario.inputs {
        let left = a.inputs.iter().find(|i| i.id == input.id);
        let right = b.inputs.iter().find(|i| i.id == input.id);
        let (Some(left), Some(right)) = (left, right) else {
            blockers.push(format!("вход {} записан не в обоих результатах", input.id));
            continue;
        };
        let same = left.identity == right.identity && left.size == right.size;
        match (input.role, same) {
            (_, true) => {}
            (InputRole::Workload, false) => blockers.push(format!(
                "нагрузка {} различается по идентичности или объёму: несходные нагрузки не эквивалентны",
                input.id
            )),
            (InputRole::Subject, false) => subject_differences.push(format!("предмет сравнения {} различается", input.id)),
        }
    }

    for param in scenario
        .parameters
        .iter()
        .filter(|p| p.affects_comparability)
    {
        let value = |r: &BenchmarkResult| {
            r.parameters
                .get(&param.name)
                .cloned()
                .or_else(|| param.default.clone())
        };
        if value(a) != value(b) {
            blockers.push(format!(
                "параметр {} влияет на сопоставимость и различается",
                param.name
            ));
        }
    }

    for name in &scenario.environment.comparable {
        match (a.environment.get(name), b.environment.get(name)) {
            (Some(Fact::Known { value: left }), Some(Fact::Known { value: right }))
                if left == right => {}
            (Some(Fact::Known { .. }), Some(Fact::Known { .. })) => {
                blockers.push(format!("факт окружения {name} различается"))
            }
            _ => blockers.push(format!(
                "факт окружения {name} неизвестен хотя бы в одном результате"
            )),
        }
    }

    let mut metrics = Vec::new();
    let mut statistical_metrics = Vec::new();
    for left in &a.metrics {
        let Some(right) = b.metrics.iter().find(|m| m.key == left.key) else {
            continue;
        };
        let same_definition = left.metric == right.metric
            && left.scope == right.scope
            && left.source == right.source
            && left.unit == right.unit;
        if !same_definition {
            blockers.push(format!("определение метрики {} различается", left.key));
        } else {
            // Число попыток (observation) не заменяет число измерений:
            // неуспешный прогон значения не даёт.
            let fewest = a
                .successful_values(&left.key)
                .len()
                .min(b.successful_values(&left.key).len());
            if fewest > 0 {
                metrics.push(left.key.clone());
            }
            if fewest >= MIN_STATISTICAL_VALUES {
                statistical_metrics.push(left.key.clone());
            }
        }
    }
    if metrics.is_empty() {
        blockers.push("нет ни одной метрики, измеренной в обоих результатах".to_owned());
    }
    blockers.sort();
    blockers.dedup();

    Verdict {
        comparable: blockers.is_empty(),
        blockers,
        subject_differences,
        statistically_supported: !metrics.is_empty() && statistical_metrics.len() == metrics.len(),
        metrics,
        statistical_metrics,
    }
}
