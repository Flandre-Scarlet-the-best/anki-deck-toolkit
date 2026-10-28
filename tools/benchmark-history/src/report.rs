//! Раздел `## Производительность` для описания PR.
//!
//! Все значения берутся из проверенных результатов истории, а не из памяти
//! агента. Формат согласован с проверкой `.github/scripts/check_pr_body.py`:
//! строка `Статус:`, обязательные поля и таблица метрик. CI проверяет только
//! форму; подлинность цифр подтверждается ссылками на исходные результаты.

use crate::analysis::{self, Assessment, Comparison, MetricRow, Side};
use crate::format;
use benchmark_contract::comparability;
use benchmark_contract::result::{Availability, BenchmarkResult, Fact, OutcomeStatus, RunStatus};
use std::fmt::Write;

pub const HEADING: &str = "## Производительность";
pub const STATUS_MEASURED: &str = "измерено";
pub const STATUS_NOT_MEASURED: &str = "не измерено";
pub const NO_EVIDENCE: &str = "Доказательство производительности: отсутствует";

/// База проверяемого результата.
pub enum Base<'a> {
    /// Новая функциональность: состояния «до» не существует.
    First,
    Chosen {
        id: &'a str,
        result: &'a BenchmarkResult,
        comparison: Comparison,
        /// Как выбрана база: правило или явное указание.
        how: String,
        notes: Vec<String>,
    },
    /// Пригодной базы в истории нет.
    Missing { reasons: Vec<String> },
}

pub struct Item<'a> {
    pub id: &'a str,
    pub result: &'a BenchmarkResult,
    pub base: Base<'a>,
}

fn measured(result: &BenchmarkResult) -> bool {
    matches!(
        result.outcome.status,
        OutcomeStatus::Success | OutcomeStatus::Partial
    )
}

fn status_text(status: OutcomeStatus) -> String {
    analysis::wire_name(&status)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(16)]
}

/// Ячейка таблицы Markdown: без переводов строк и с экранированной чертой.
fn cell(text: &str) -> String {
    text.replace(['\n', '\r'], " ").replace('|', "\\|")
}

fn availability(a: Availability) -> String {
    analysis::wire_name(&a)
}

fn side(side: &Side, unit: benchmark_contract::vocabulary::Unit) -> String {
    match side {
        Side::Measured {
            availability: a,
            stats,
        } => {
            let spread = stats
                .stddev
                .map(|s| format!(" ± {}", format::quantity(s, unit)))
                .unwrap_or_default();
            let partial = if *a == Availability::Partial {
                " · partial"
            } else {
                ""
            };
            format!(
                "{}{spread} (n={}){partial}",
                format::quantity(stats.mean, unit),
                stats.count
            )
        }
        Side::Missing {
            availability: a,
            reason,
        } => cell(&format!(
            "`{}`: {}",
            availability(*a),
            reason.as_deref().unwrap_or("причина не записана")
        )),
        Side::Absent => "не записана".into(),
    }
}

fn assessment(a: &Assessment) -> String {
    match a {
        Assessment::NotCompared { reason } => format!("— ({})", cell(reason)),
        Assessment::InsufficientRepeats => "мало повторов".into(),
        Assessment::WithinSpread => "в пределах разброса".into(),
        Assessment::BeyondSpread { better: Some(true) } => "лучше сверх разброса".into(),
        Assessment::BeyondSpread {
            better: Some(false),
        } => "хуже сверх разброса".into(),
        Assessment::BeyondSpread { better: None } => "изменение сверх разброса".into(),
    }
}

/// Описание измеренного кода. Коммит и дерево исходников уместны в PR, но
/// не в постоянной документации.
fn code(result: &BenchmarkResult) -> String {
    let code = &result.provenance.code;
    let mut parts = vec![format!(
        "`{}` {}",
        result.scenario.module, code.module_version
    )];
    if let Some(commit) = &code.git_commit {
        parts.push(format!("коммит `{}`", &commit[..commit.len().min(12)]));
    }
    parts.push(match code.dirty {
        Some(true) => "рабочая копия **с изменениями**".into(),
        Some(false) => "рабочая копия без изменений".into(),
        None => "состояние рабочей копии не записано".into(),
    });
    if let Some(tree) = &code.source_tree_sha256 {
        parts.push(format!("дерево исходников `{}`", &tree[..12]));
    }
    parts.join(" · ")
}

fn workload(result: &BenchmarkResult) -> String {
    let mut parts: Vec<String> = result
        .inputs
        .iter()
        .map(|input| {
            let sizes: Vec<String> = input
                .size
                .iter()
                .map(|(name, q)| format!("`{name}` {}", format::quantity(q.value, q.unit)))
                .collect();
            format!(
                "вход `{}` ({}, `{}` `{}`){}",
                input.id,
                serde_json::to_value(input.role)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                serde_json::to_value(input.identity.method)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                // Объявленная версия — произвольный текст: срез по символам,
                // а не по байтам.
                input.identity.value.chars().take(12).collect::<String>(),
                if sizes.is_empty() {
                    String::new()
                } else {
                    format!(": {}", sizes.join(", "))
                }
            )
        })
        .collect();
    parts.extend(
        result
            .parameters
            .iter()
            .map(|(name, value)| format!("параметр `{name}` = `{value}`")),
    );
    if parts.is_empty() {
        "сценарий не объявляет входов и записываемых параметров".into()
    } else {
        parts.join("; ")
    }
}

fn environment(result: &BenchmarkResult) -> String {
    result
        .environment
        .iter()
        .map(|(name, fact)| match fact {
            Fact::Known { value } => format!("`{name}`={value}"),
            Fact::Unavailable { .. } => format!("`{name}` недоступен"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn runs(result: &BenchmarkResult) -> String {
    let ok = result
        .runs
        .iter()
        .filter(|r| r.status == RunStatus::Success)
        .count();
    let tool = |t: &benchmark_contract::result::Tool| format!("{} {}", t.name, t.version);
    let mut text = format!(
        "{ok} успешных из {}, прогрев {}; итог `{}`",
        result.runs.len(),
        result.warmup_runs,
        status_text(result.outcome.status)
    );
    if let Some(reason) = &result.outcome.reason {
        let _ = write!(text, " ({})", reason);
    }
    let _ = write!(text, "; {}", tool(&result.provenance.measurement));
    if let Some(runner) = &result.provenance.runner {
        let _ = write!(text, ", {}", tool(runner));
    }
    let _ = write!(text, "; записан {}", result.provenance.recorded_at);
    text
}

fn table(out: &mut String, rows: &[MetricRow], with_base: bool) {
    if with_base {
        out.push_str("| Метрика | Scope / источник | До: среднее ± σ (n) | После: среднее ± σ (n) | Δ | Δ % | Оценка |\n|---|---|---|---|---|---|---|\n");
    } else {
        out.push_str("| Метрика | Scope / источник | Значение: среднее ± σ (n) | Медиана | Мин–макс |\n|---|---|---|---|---|\n");
    }
    for row in rows {
        let name = match row.key.split_once('/') {
            Some((_, label)) => format!("`{}` `{label}`", row.metric),
            None => format!("`{}`", row.metric),
        };
        let scope = format!("{} / {}", row.scope, row.source);
        if with_base {
            let (delta, relative) = match &row.delta {
                Some(d) => (
                    format::signed(d.absolute, row.unit),
                    d.relative
                        .map(format::percent)
                        .unwrap_or_else(|| "— (база равна нулю)".into()),
                ),
                None => ("—".into(), "—".into()),
            };
            let before = row
                .before
                .as_ref()
                .map(|s| side(s, row.unit))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| {name} | {scope} | {before} | {} | {delta} | {relative} | {} |",
                side(&row.after, row.unit),
                assessment(&row.assessment)
            );
        } else {
            let (median, range) = match row.after.stats() {
                Some(s) => (
                    format::quantity(s.median, row.unit),
                    format!(
                        "{} – {}",
                        format::quantity(s.min, row.unit),
                        format::quantity(s.max, row.unit)
                    ),
                ),
                None => ("—".into(), "—".into()),
            };
            let _ = writeln!(
                out,
                "| {name} | {scope} | {} | {median} | {range} |",
                side(&row.after, row.unit)
            );
        }
    }
}

fn conclusion(item: &Item<'_>, rows: &[MetricRow]) -> String {
    let partial = if item.result.outcome.status == OutcomeStatus::Partial {
        "серия неполна (итог `partial`), выводы ограничены успешными прогонами; "
    } else {
        ""
    };
    match &item.base {
        Base::First => format!(
            "{partial}первое измерение новой функциональности: значения фиксируют исходное состояние, состояния «до» нет, вывода об улучшении или ухудшении нет."
        ),
        Base::Missing { .. } => format!(
            "{partial}сравнение до/после не выполнено: в локальной истории нет пригодной базы; значения приведены без вывода об изменении."
        ),
        Base::Chosen { comparison, .. } if !comparison.verdict.comparable => format!(
            "{partial}вывод об изменении не делается: результаты несопоставимы ({}).",
            comparison.verdict.blockers.join("; ")
        ),
        Base::Chosen { .. } => {
            let mut parts = Vec::new();
            for row in rows {
                let change = row
                    .delta
                    .as_ref()
                    .and_then(|d| d.relative)
                    .map(|r| format!(" ({})", format::percent(r)))
                    .unwrap_or_default();
                let text = match &row.assessment {
                    Assessment::BeyondSpread { better: Some(true) } => {
                        format!("улучшение сверх разброса повторов{change}")
                    }
                    Assessment::BeyondSpread {
                        better: Some(false),
                    } => {
                        format!("ухудшение сверх разброса повторов{change}")
                    }
                    Assessment::BeyondSpread { better: None } => {
                        format!(
                            "изменение сверх разброса повторов{change}, направление не оценивается"
                        )
                    }
                    Assessment::WithinSpread => "изменение в пределах разброса повторов".into(),
                    Assessment::InsufficientRepeats => {
                        format!(
                            "меньше {} измерений с одной из сторон: разброс не оценивается, вывода нет",
                            comparability::MIN_STATISTICAL_VALUES
                        )
                    }
                    Assessment::NotCompared { reason } => format!("не сравнивается: {reason}"),
                };
                parts.push(format!("`{}` — {text}", row.key));
            }
            format!(
                "{partial}{}. Оценка по разбросу повторов не является проверкой статистической значимости. Разность относится к состояниям кода базы и результата: {}.",
                parts.join("; "),
                analysis::IMPLEMENTATION_CAVEAT
            )
        }
    }
}

fn limitations(item: &Item<'_>, rows: &[MetricRow]) -> Vec<String> {
    let mut list = Vec::new();
    let r = item.result;
    if r.provenance.code.dirty == Some(true) {
        list.push("измерена рабочая копия с изменениями".into());
    }
    for row in rows {
        match &row.after {
            Side::Missing {
                availability: a,
                reason,
            } => list.push(format!(
                "`{}` не измерена (`{}`): {}",
                row.key,
                availability(*a),
                reason.as_deref().unwrap_or("причина не записана")
            )),
            Side::Measured {
                availability: Availability::Partial,
                ..
            } => list.push(format!("`{}` измерена не во всех прогонах", row.key)),
            _ => {}
        }
    }
    if r.runs
        .iter()
        .filter(|x| x.status == RunStatus::Success)
        .count()
        < 2
    {
        list.push("одно успешное наблюдение: разброс не оценивается".into());
    }
    if let Base::Chosen {
        comparison, notes, ..
    } = &item.base
    {
        list.extend(
            comparison
                .verdict
                .subject_differences
                .iter()
                .map(|d| format!("{d} — это ожидаемое изменение кода, а не нагрузки")),
        );
        list.extend(notes.iter().map(|n| format!("база: {n}")));
    }
    list
}

fn measured_item(out: &mut String, item: &Item<'_>) {
    let r = item.result;
    let _ = writeln!(
        out,
        "### `{}` · профиль `{}`\n",
        r.scenario.id, r.profile.id
    );
    let _ = writeln!(
        out,
        "- Сценарий: `{}`, ревизия {}, `{}`; профиль `{}` (кэш `{}`, сеть `{}`)",
        r.scenario.id,
        r.scenario.revision,
        r.scenario.kind,
        r.profile.id,
        r.profile.cache,
        r.profile.network
    );
    let _ = writeln!(out, "- Измеренный код: {}", code(r));
    let _ = writeln!(out, "- Нагрузка: {}", workload(r));
    let _ = writeln!(out, "- Окружение: {}", environment(r));
    let _ = writeln!(out, "- Прогоны: {}", runs(r));
    let rows = match &item.base {
        Base::First => {
            let _ = writeln!(
                out,
                "- База: нет — новая функциональность, первое измерение"
            );
            let _ = writeln!(
                out,
                "- Сопоставимость: не оценивается — сравнивать не с чем"
            );
            analysis::alone(r, "первое измерение")
        }
        Base::Missing { reasons } => {
            let _ = writeln!(
                out,
                "- База: не найдена — {}",
                if reasons.is_empty() {
                    "в истории нет других результатов этого сценария и профиля".to_owned()
                } else {
                    reasons.join("; ")
                }
            );
            let _ = writeln!(out, "- Сопоставимость: не оценивается — базы нет");
            analysis::alone(r, "базы нет")
        }
        Base::Chosen {
            id,
            result: base,
            comparison,
            how,
            ..
        } => {
            let _ = writeln!(
                out,
                "- База: результат `{}` · {} · записан {} — {how}",
                short(id),
                code(base),
                base.provenance.recorded_at
            );
            let verdict = &comparison.verdict;
            let text = if verdict.comparable {
                let mut text = format!(
                    "сопоставимы по контракту `benchmark-contract` по метрикам {}",
                    verdict
                        .metrics
                        .iter()
                        .map(|m| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if verdict.statistically_supported {
                    let _ = write!(
                        text,
                        "; по каждой не меньше {} успешных измерений с обеих сторон",
                        comparability::MIN_STATISTICAL_VALUES
                    );
                } else {
                    text.push_str(
                        "; повторов недостаточно для оценки разброса хотя бы по одной метрике",
                    );
                }
                text
            } else {
                format!("**несопоставимы**: {}", verdict.blockers.join("; "))
            };
            let _ = writeln!(out, "- Сопоставимость: {text}");
            comparison.rows.clone()
        }
    };
    out.push('\n');
    table(out, &rows, matches!(item.base, Base::Chosen { .. }));
    out.push('\n');
    let _ = writeln!(out, "- Вывод: {}", conclusion(item, &rows));
    let limits = limitations(item, &rows);
    let _ = writeln!(
        out,
        "- Ограничения: {}",
        if limits.is_empty() {
            "существенных нет".to_owned()
        } else {
            limits.join("; ")
        }
    );
    let mut sources = format!("проверяемый результат `{}`", item.id);
    if let Base::Chosen { id, .. } = &item.base {
        let _ = write!(sources, ", база `{id}`");
    }
    let _ = writeln!(
        out,
        "- Источники: {sources} в локальной истории `benchmark-history` (`benchmark-history show <ID>`)"
    );
}

fn unmeasured_item(out: &mut String, item: &Item<'_>) {
    let r = item.result;
    let _ = writeln!(
        out,
        "### `{}` · профиль `{}` — не измерено\n",
        r.scenario.id, r.profile.id
    );
    let _ = writeln!(
        out,
        "- Сценарий: `{}`, ревизия {}, `{}`; профиль `{}`",
        r.scenario.id, r.scenario.revision, r.scenario.kind, r.profile.id
    );
    let _ = writeln!(out, "- Измеренный код: {}", code(r));
    let _ = writeln!(
        out,
        "- Причина: {}",
        r.outcome
            .reason
            .as_deref()
            .unwrap_or("итог без причины в результате")
    );
    let _ = writeln!(
        out,
        "- Диагностика: исполнитель записал итог `{}`: прогонов {}, успешных 0; результат `{}`",
        status_text(r.outcome.status),
        r.runs.len(),
        item.id
    );
    let _ = writeln!(out, "- {NO_EVIDENCE}");
}

/// Полный раздел для PR.
pub fn pr_section(items: &[Item<'_>]) -> String {
    let mut out = String::new();
    let any_measured = items.iter().any(|i| measured(i.result));
    let _ = writeln!(out, "{HEADING}\n");
    if any_measured {
        let _ = writeln!(out, "Статус: {STATUS_MEASURED}\n");
    } else {
        let _ = writeln!(out, "Статус: {STATUS_NOT_MEASURED}\n");
        let reasons: Vec<String> = items
            .iter()
            .map(|i| {
                format!(
                    "`{}` · `{}`: {}",
                    i.result.scenario.id,
                    i.result.profile.id,
                    i.result
                        .outcome
                        .reason
                        .as_deref()
                        .unwrap_or("итог без причины")
                )
            })
            .collect();
        let _ = writeln!(out, "Причина: {}", reasons.join("; "));
        let _ = writeln!(
            out,
            "Диагностика: итоги исполнителя и результаты истории по каждому сценарию приведены ниже"
        );
        let _ = writeln!(out, "{NO_EVIDENCE}\n");
    }
    out.push_str(
        "Раздел подготовлен `benchmark-history report` из проверенных результатов `benchmark-runner` в локальной истории; значения не правились вручную.\n",
    );
    for item in items {
        out.push('\n');
        if measured(item.result) {
            measured_item(&mut out, item);
        } else {
            unmeasured_item(&mut out, item);
        }
    }
    out
}
