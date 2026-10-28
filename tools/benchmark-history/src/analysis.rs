//! Сравнение «до/после» и выбор базы поверх контракта сопоставимости.
//!
//! Допустимость сравнения решает только `benchmark_contract::comparability`.
//! Здесь добавляются разности, направление улучшения по виду метрики и
//! грубая оценка относительно разброса повторов. Эта оценка — не проверка
//! статистической значимости: она лишь не даёт выдать за изменение то, что
//! меньше наблюдаемого разброса.

use benchmark_contract::comparability::{self, Verdict};
use benchmark_contract::result::{
    Aggregate, Availability, BenchmarkResult, CodeState, OutcomeStatus,
};
use benchmark_contract::scenario::Scenario;
use benchmark_contract::vocabulary::{MetricId, Scope, Source, Unit};
use serde::Serialize;

/// Имя значения перечисления так, как оно записано в JSON результата.
pub fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Какое изменение метрики желательно.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    LowerIsBetter,
    /// Направление не оценивается: больше — не обязательно хуже
    /// (загрузка ядер, счётчики модуля, обращения к кэшу).
    Neutral,
}

pub fn direction(metric: MetricId) -> Direction {
    use MetricId::*;
    match metric {
        TimeWallClock
        | TimeUser
        | TimeSystem
        | TimeCpu
        | CpuTimeCgroup
        | PhaseDuration
        | MemoryPeakResident
        | MemoryPeakCgroup
        | MemoryHostPeakIncrease
        | DiskUsage
        | CpuCycles
        | Instructions
        | CacheMisses
        | BranchMisses => Direction::LowerIsBetter,
        CpuUtilizationMean | CpuUtilizationPeak | CacheReferences | ModuleCounter => {
            Direction::Neutral
        }
    }
}

/// Значения одной метрики одного результата.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Side {
    /// Есть значения успешных прогонов (`availability` может быть `partial`).
    Measured {
        availability: Availability,
        #[serde(flatten)]
        stats: Aggregate,
    },
    /// Значений нет: статус и причина из результата.
    Missing {
        availability: Availability,
        reason: Option<String>,
    },
    /// Метрика в результате не записана.
    Absent,
}

impl Side {
    pub fn of(result: &BenchmarkResult, key: &str) -> Side {
        let Some(record) = result.metrics.iter().find(|m| m.key == key) else {
            return Side::Absent;
        };
        match Aggregate::compute(&result.successful_values(key)) {
            Some(stats) => Side::Measured {
                availability: record.availability,
                stats,
            },
            None => Side::Missing {
                availability: record.availability,
                reason: record.reason.clone(),
            },
        }
    }

    pub fn stats(&self) -> Option<&Aggregate> {
        match self {
            Side::Measured { stats, .. } => Some(stats),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    /// После минус до, по средним.
    pub absolute: f64,
    /// Отношение к среднему базы; нет при нулевой базе.
    pub relative: Option<f64>,
}

/// Чем можно назвать разность.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Assessment {
    /// Сравнение недопустимо или значения есть не с обеих сторон.
    NotCompared { reason: String },
    /// Меньше двух измерений хотя бы с одной стороны: разброс не оценить.
    InsufficientRepeats,
    /// |Δ| не больше суммы стандартных отклонений обеих сторон.
    WithinSpread,
    /// |Δ| больше суммы стандартных отклонений. `better` — по направлению
    /// метрики; `None` — направление не оценивается.
    BeyondSpread { better: Option<bool> },
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricRow {
    pub key: String,
    pub metric: MetricId,
    pub scope: Scope,
    pub source: Source,
    pub unit: Unit,
    pub direction: Direction,
    pub before: Option<Side>,
    pub after: Side,
    pub delta: Option<Delta>,
    pub assessment: Assessment,
}

/// Сравнение результата с базой.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub verdict: Verdict,
    pub rows: Vec<MetricRow>,
}

/// Строки метрик проверяемого результата без базы (первое измерение).
pub fn alone(target: &BenchmarkResult, reason: &str) -> Vec<MetricRow> {
    target
        .metrics
        .iter()
        .map(|m| MetricRow {
            key: m.key.clone(),
            metric: m.metric,
            scope: m.scope,
            source: m.source,
            unit: m.unit,
            direction: direction(m.metric),
            before: None,
            after: Side::of(target, &m.key),
            delta: None,
            assessment: Assessment::NotCompared {
                reason: reason.to_owned(),
            },
        })
        .collect()
}

pub fn compare(
    scenario: &Scenario,
    base: &BenchmarkResult,
    target: &BenchmarkResult,
) -> Comparison {
    let verdict = comparability::compare(scenario, base, target);
    let rows = target
        .metrics
        .iter()
        .map(|m| {
            let before = Side::of(base, &m.key);
            let after = Side::of(target, &m.key);
            let same_definition = base
                .metrics
                .iter()
                .find(|b| b.key == m.key)
                .is_some_and(|b| {
                    b.metric == m.metric
                        && b.scope == m.scope
                        && b.source == m.source
                        && b.unit == m.unit
                });
            let delta = match (before.stats(), after.stats()) {
                (Some(b), Some(a)) if verdict.comparable && same_definition => Some(Delta {
                    absolute: a.mean - b.mean,
                    relative: (b.mean != 0.0).then(|| (a.mean - b.mean) / b.mean),
                }),
                _ => None,
            };
            let assessment = assess(
                &verdict,
                m.key.as_str(),
                direction(m.metric),
                &before,
                &after,
                delta.as_ref(),
            );
            MetricRow {
                key: m.key.clone(),
                metric: m.metric,
                scope: m.scope,
                source: m.source,
                unit: m.unit,
                direction: direction(m.metric),
                before: Some(before),
                after,
                delta,
                assessment,
            }
        })
        .collect();
    Comparison { verdict, rows }
}

fn assess(
    verdict: &Verdict,
    key: &str,
    direction: Direction,
    before: &Side,
    after: &Side,
    delta: Option<&Delta>,
) -> Assessment {
    if !verdict.comparable {
        return Assessment::NotCompared {
            reason: "результаты несопоставимы".into(),
        };
    }
    let (Some(b), Some(a), Some(delta)) = (before.stats(), after.stats(), delta) else {
        return Assessment::NotCompared {
            reason: "значения есть не в обоих результатах".into(),
        };
    };
    if !verdict.statistical_metrics.iter().any(|m| m == key) {
        return Assessment::InsufficientRepeats;
    }
    let spread = b.stddev.unwrap_or(0.0) + a.stddev.unwrap_or(0.0);
    if delta.absolute.abs() <= spread {
        return Assessment::WithinSpread;
    }
    Assessment::BeyondSpread {
        better: match direction {
            Direction::LowerIsBetter => Some(delta.absolute < 0.0),
            Direction::Neutral => None,
        },
    }
}

/// Что известно о различии кода кандидата и проверяемого результата.
///
/// `source_tree_sha256` и коммит описывают состояние всего репозитория, а не
/// измеряемый исполняемый артефакт: результат не содержит идентичности
/// собранной программы. Поэтому даже доказанное различие состояний не
/// доказывает, что изменилась сама измеряемая реализация.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CodeChange {
    /// Отличие не доказано: тот же код или его состояние не записано.
    NotProven,
    /// Между коммитами изменена только документация (Markdown) или ничего:
    /// та же реализация, состоянием «до» оптимизации это не является.
    DocumentationOnly { changed: usize },
    /// Между коммитами изменены и другие файлы; относятся ли они к
    /// измеряемому артефакту, записанные данные не доказывают.
    Files {
        changed: usize,
        documentation: usize,
    },
    /// Состояния различаются, но состав изменений не проверен.
    Unverified { reason: String },
}

/// Переменные окружения, привязывающие Git к конкретному репозиторию
/// (`git rev-parse --local-env-vars`). Унаследованные, например из Git-хука,
/// они увели бы `Changes` мимо нужного репозитория, поэтому их снимают.
pub const GIT_LOCAL_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Изменённые пути между двумя коммитами (относительно корня репозитория).
pub type Changes<'a> = &'a dyn Fn(&str, &str) -> Result<Vec<String>, String>;

/// Документация, которая не входит в сборку модулей: Markdown.
pub fn is_documentation(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("md"))
}

/// Кандидат в базу и почему он подходит или нет.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub id: String,
    pub recorded_at: String,
    pub git_commit: Option<String>,
    pub dirty: Option<bool>,
    pub outcome: OutcomeStatus,
    pub code: CodeChange,
    pub eligible: bool,
    /// Почему запись не может быть базой.
    pub reasons: Vec<String>,
    /// Ограничения подходящей записи.
    pub notes: Vec<String>,
}

/// Правило автоматического выбора базы — повторяется в отчётах.
pub const BASELINE_RULE: &str = "самый поздний успешный результат того же сценария и профиля, записанный раньше проверяемого, с доказанно другим состоянием кода (разные source_tree_sha256 либо разные коммиты чистых рабочих копий), если между коммитами изменена не только документация, и сопоставимый по контракту benchmark-contract";

/// Оговорка к любой базе: различие состояний не равно замене реализации.
pub const IMPLEMENTATION_CAVEAT: &str = "результаты не записывают идентичность собранной программы, поэтому различие состояний репозитория не доказывает замену измеряемой реализации";

/// Оценить каждого кандидата относительно проверяемого результата.
pub fn candidates(
    scenario: &Scenario,
    target_id: &str,
    target: &BenchmarkResult,
    pool: &[(String, BenchmarkResult)],
    changes: Changes<'_>,
) -> Vec<Candidate> {
    let target_key = crate::index::time_key(&target.provenance.recorded_at);
    let mut list: Vec<Candidate> = pool
        .iter()
        .filter(|(id, _)| id != target_id)
        .filter(|(_, r)| r.scenario.id == target.scenario.id && r.profile.id == target.profile.id)
        .map(|(id, r)| {
            let mut reasons = Vec::new();
            let mut notes = Vec::new();
            if r.outcome.status != OutcomeStatus::Success {
                reasons.push(format!(
                    "итог {}: неполная или неуспешная серия не служит базой",
                    wire_name(&r.outcome.status)
                ));
            }
            if crate::index::time_key(&r.provenance.recorded_at) >= target_key {
                reasons.push("записан не раньше проверяемого результата".into());
            }
            let code = match code_difference(&r.provenance.code, &target.provenance.code) {
                Ok(note) => {
                    notes.extend(note);
                    let code = change_of(&r.provenance.code, &target.provenance.code, changes);
                    match &code {
                        CodeChange::DocumentationOnly { changed: 0 } => reasons.push(
                            "между коммитами нет изменённых файлов: тот же код, это повтор измерения, а не состояние «до»".into(),
                        ),
                        CodeChange::DocumentationOnly { changed } => reasons.push(format!(
                            "между коммитами изменена только документация (файлов Markdown: {changed}): реализация та же, это повтор измерения, а не состояние «до»; осознанное сравнение — только явной базой"
                        )),
                        CodeChange::Files {
                            changed,
                            documentation,
                        } => notes.push(format!(
                            "между коммитами изменено файлов: {changed}, из них документации: {documentation}; затронута ли измеряемая реализация, записанные данные не доказывают"
                        )),
                        CodeChange::Unverified { reason } => notes.push(format!(
                            "состав изменений не проверен ({reason}): различие установлено только для состояния репозитория"
                        )),
                        CodeChange::NotProven => {}
                    }
                    code
                }
                Err(reason) => {
                    reasons.push(reason);
                    CodeChange::NotProven
                }
            };
            let verdict = comparability::compare(scenario, r, target);
            reasons.extend(
                verdict
                    .blockers
                    .iter()
                    .map(|b| format!("несопоставим: {b}")),
            );
            if r.provenance.code.dirty == Some(true) {
                notes.push("измерен в рабочей копии с изменениями".into());
            }
            if verdict.comparable && !verdict.statistically_supported {
                notes.push(format!(
                    "меньше {} успешных измерений хотя бы у одной метрики: разброс не оценивается",
                    comparability::MIN_STATISTICAL_VALUES
                ));
            }
            Candidate {
                id: id.clone(),
                recorded_at: r.provenance.recorded_at.clone(),
                git_commit: r.provenance.code.git_commit.clone(),
                dirty: r.provenance.code.dirty,
                outcome: r.outcome.status,
                code,
                eligible: reasons.is_empty(),
                reasons,
                notes,
            }
        })
        .collect();
    list.sort_by(|a, b| {
        crate::index::time_key(&b.recorded_at)
            .cmp(&crate::index::time_key(&a.recorded_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    list
}

/// Доказано ли, что исходный код кандидата отличается от проверяемого.
///
/// Отсутствие доказательства одинакового кода — не доказательство различия:
/// без положительного свидетельства кандидат базой автоматически не становится.
/// Свидетельство — разные `source_tree_sha256`, а если дерево записано не в
/// обоих результатах — разные коммиты, оба измеренные без изменений рабочей
/// копии. `Ok(Some(_))` — отличие доказано с оговоркой.
fn code_difference(base: &CodeState, target: &CodeState) -> Result<Option<String>, String> {
    if let (Some(a), Some(b)) = (&base.source_tree_sha256, &target.source_tree_sha256) {
        return if a == b {
            Err("тот же исходный код (source_tree_sha256): это повтор измерения, а не состояние «до»".into())
        } else {
            Ok(None)
        };
    }
    let clean = base.dirty == Some(false) && target.dirty == Some(false);
    match (&base.git_commit, &target.git_commit) {
        (Some(a), Some(b)) if clean && a == b => Err(
            "тот же коммит без изменений рабочей копии: это повтор измерения, а не состояние «до»".into(),
        ),
        (Some(_), Some(_)) if clean => Ok(Some(
            "source_tree_sha256 записан не в обоих результатах: отличие кода доказано только разными коммитами".into(),
        )),
        _ => Err("отличие исходного кода не доказано: source_tree_sha256 записан не в обоих результатах, а коммиты не записаны или измерены не в чистой рабочей копии".into()),
    }
}

/// Состав изменений между доказанно разными состояниями. Сверяется только
/// пара коммитов чистых рабочих копий: незафиксированные изменения Git
/// перечислить уже не может.
fn change_of(base: &CodeState, target: &CodeState, changes: Changes<'_>) -> CodeChange {
    let clean = base.dirty == Some(false) && target.dirty == Some(false);
    let (Some(a), Some(b), true) = (&base.git_commit, &target.git_commit, clean) else {
        return CodeChange::Unverified {
            reason: "коммиты не записаны или измерена рабочая копия с изменениями".into(),
        };
    };
    if a == b {
        return CodeChange::Unverified {
            reason: "коммит тот же, а дерево различается".into(),
        };
    }
    match changes(a, b) {
        Ok(paths) => {
            let documentation = paths.iter().filter(|p| is_documentation(p)).count();
            if documentation == paths.len() {
                CodeChange::DocumentationOnly {
                    changed: documentation,
                }
            } else {
                CodeChange::Files {
                    changed: paths.len(),
                    documentation,
                }
            }
        }
        Err(reason) => CodeChange::Unverified { reason },
    }
}

/// База по [`BASELINE_RULE`]; `commit` сужает выбор до коммита с этим префиксом.
pub fn choose<'a>(candidates: &'a [Candidate], commit: Option<&str>) -> Option<&'a Candidate> {
    candidates.iter().find(|c| {
        c.eligible
            && commit.is_none_or(|prefix| {
                c.git_commit
                    .as_deref()
                    .is_some_and(|g| g.starts_with(prefix))
            })
    })
}
