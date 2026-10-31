//! Операция `qa`: детерминированные content-level замечания по экспорту.
//!
//! Команда собирает замечания всех применимых правил ([`crate::qa::RULES`]) и
//! отдаёт их в каноническом порядке, ограничивая объём вывода по каждому коду.
//! Она ничего не решает за ревьюера: ни `validate`-валидность, ни блокировку
//! `edit` замечания не меняют. Даже замечание серьёзности `error` (например,
//! устаревшая белая `<span>`-обёртка) — это содержание, а не структурная
//! поломка экспорта.
//!
//! Ограничение вывода сделано по кодам, а не по всему списку: ревьюер должен
//! видеть все найденные *классы* проблем сразу, даже когда один класс массовый.
//! `findings_total` всегда считает полное число, `truncated` показывает,
//! попало ли в вывод всё.
//!
//! Второе ограничение вывода задано не размером, а downstream-контрактом:
//! замечание на заметку, которую нельзя адресовать (`guid` отсутствует, пуст или
//! повторяется), помечается `addressable: false`. Такое замечание остаётся
//! диагностикой содержимого, но не выдаётся за editable target: предложение по
//! нему нечем исполнить. Структурные причины неадресуемости остаются
//! собственностью [`crate::ops::validate`]; `qa` их не дублирует и не превращает
//! в отказ.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::qa::{self, QaSeverity, RULES, RuleContext};

/// Параметры запроса `qa`.
#[derive(Debug, Clone)]
pub struct QaQuery {
    /// Коды правил; пустой список означает «все правила».
    pub codes: Vec<String>,
    /// Предел числа замечаний на один код.
    pub max_per_code: usize,
}

/// Результат `qa`.
#[derive(Debug)]
pub struct QaResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Всего заметок в экспорте.
    pub notes_total: usize,
    /// Сколько замечаний найдено (после фильтра по кодам, до ограничения вывода).
    pub findings_total: usize,
    /// Сколько замечаний попало в вывод.
    pub findings_returned: usize,
    /// Есть ли замечания, не попавшие в вывод из-за предела на код.
    pub truncated: bool,
    /// Применённый предел на один код.
    pub max_per_code: usize,
    /// Применённый фильтр по кодам; пустой список означает «все правила».
    pub codes: Vec<String>,
    /// Реестр правил с их применимостью и полным числом замечания.
    pub rules: Vec<RuleSummary>,
    /// Распределение по выбранным кодам в порядке реестра.
    pub by_code: Vec<CodeCount>,
    /// Замечания в каноническом порядке.
    pub findings: Vec<FindingView>,
    /// Сколько выбранных замечаний нельзя адресовать однозначно.
    pub unaddressable_findings: usize,
}

/// Реестр одного правила в результате.
#[derive(Debug)]
pub struct RuleSummary {
    /// Код правила.
    pub code: &'static str,
    /// Серьёзность.
    pub severity: QaSeverity,
    /// Описание.
    pub description: &'static str,
    /// Применимо ли правило к этому экспорту.
    pub applicable: bool,
    /// Сколько замечаний правило нашло по всему экспорту (без учёта фильтра).
    pub findings: usize,
}

/// Число замечаний одного кода.
#[derive(Debug)]
pub struct CodeCount {
    /// Код правила.
    pub code: &'static str,
    /// Серьёзность.
    pub severity: QaSeverity,
    /// Число замечаний по этому коду среди выбранных.
    pub count: usize,
}

/// Одно замечание в выводе.
#[derive(Debug)]
pub struct FindingView {
    /// Код правила.
    pub code: &'static str,
    /// Серьёзность.
    pub severity: QaSeverity,
    /// Позиция заметки в порядке экспорта.
    pub note_index: usize,
    /// `guid` заметки.
    pub guid: Option<String>,
    /// Путь колоды заметки.
    pub deck_path: String,
    /// Имя модели заметки.
    pub note_model: Option<String>,
    /// Идентичность модели заметки.
    pub note_model_uuid: Option<String>,
    /// Имя поля, если замечание относится к полю.
    pub field: Option<String>,
    /// Позиция поля в модели, если применима.
    pub field_ord: Option<i64>,
    /// Человекочитаемое описание.
    pub message: String,
    /// Machine-readable детали в границах, установленных правилом.
    pub evidence: Value,
    /// Можно ли адресовать заметку замечания однозначно.
    pub addressable: bool,
    /// `guid` остальных участников группы в порядке экспорта.
    pub related_guids: Vec<String>,
    /// Позиции всех участников группы, включая саму заметку замечания.
    pub related_note_indices: Vec<usize>,
    /// Был ли список участников обрезан.
    pub related_truncated: bool,
    /// Размер группы целиком, если замечание — про группу.
    pub group_size: Option<usize>,
}

/// Выполняет `qa`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownQaCode`], если запрошен неизвестный код
/// правила. Проблемы содержимого ошибкой не являются.
pub fn qa(
    export_dir: &Path,
    index: &ExportIndex<'_>,
    query: &QaQuery,
) -> Result<QaResult, DomainError> {
    ensure_known_codes(&query.codes)?;

    let context = RuleContext::build(index);
    let all = qa::collect(&context);
    let selected: Vec<&qa::Finding> = if query.codes.is_empty() {
        all.iter().collect()
    } else {
        all.iter()
            .filter(|finding| query.codes.iter().any(|code| code == finding.code))
            .collect()
    };

    let mut per_code: BTreeMap<&'static str, usize> = BTreeMap::new();
    for finding in &selected {
        *per_code.entry(finding.code).or_default() += 1;
    }

    let mut by_code = Vec::new();
    let mut findings = Vec::new();
    for rule in RULES {
        let selected_by_rule =
            query.codes.is_empty() || query.codes.iter().any(|code| code == rule.code);
        if !selected_by_rule {
            continue;
        }
        let count = per_code.get(rule.code).copied().unwrap_or(0);
        by_code.push(CodeCount {
            code: rule.code,
            severity: rule.severity,
            count,
        });

        findings.extend(
            selected
                .iter()
                .filter(|finding| finding.code == rule.code)
                .take(query.max_per_code)
                .map(|finding| finding_view(index, finding, rule.severity)),
        );
    }

    let truncated = by_code.iter().any(|entry| entry.count > query.max_per_code);
    let returned = findings.len();

    let rules = RULES
        .iter()
        .map(|rule| RuleSummary {
            code: rule.code,
            severity: rule.severity,
            description: rule.description,
            applicable: (rule.applicable)(&context),
            findings: all
                .iter()
                .filter(|finding| finding.code == rule.code)
                .count(),
        })
        .collect();

    let unaddressable_findings = selected
        .iter()
        .filter(|finding| !finding.addressable)
        .count();

    Ok(QaResult {
        export_dir: export_dir.display().to_string(),
        notes_total: index.notes.len(),
        findings_total: selected.len(),
        findings_returned: returned,
        truncated,
        max_per_code: query.max_per_code,
        codes: query.codes.clone(),
        rules,
        by_code,
        findings,
        unaddressable_findings,
    })
}

/// Проверяет, что все запрошенные коды известны.
fn ensure_known_codes(codes: &[String]) -> Result<(), DomainError> {
    let unknown: Vec<&str> = codes
        .iter()
        .map(String::as_str)
        .filter(|code| !qa::is_known_code(code))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }

    let available: Vec<&str> = qa::codes().collect();
    Err(DomainError::with_details(
        ErrorCode::UnknownQaCode,
        format!("неизвестные коды QA: {}", unknown.join(", ")),
        details! {
            "unknown_codes" => unknown,
            "available_codes" => available,
        },
    ))
}

/// Приводит замечание к выводу, разрешая адрес заметки.
fn finding_view(
    index: &ExportIndex<'_>,
    finding: &qa::Finding,
    severity: QaSeverity,
) -> FindingView {
    let entry = index.notes.get(finding.note_position);
    let note = entry.map(|entry| entry.note);
    let model = note
        .and_then(|note| note.note_model_uuid.as_deref())
        .and_then(|uuid| index.model_by_uuid(uuid));

    FindingView {
        code: finding.code,
        severity,
        note_index: finding.note_position,
        guid: note.and_then(|note| note.guid.clone()),
        deck_path: entry.map_or_else(String::new, |entry| index.note_deck_path(entry).to_string()),
        note_model: model.and_then(|model| model.name.clone()),
        note_model_uuid: note.and_then(|note| note.note_model_uuid.clone()),
        field: finding.field.clone(),
        field_ord: finding.field_ord,
        message: finding.message.clone(),
        evidence: finding.evidence.clone(),
        addressable: finding.addressable,
        related_guids: related_guids(index, &finding.related),
        related_note_indices: finding.related.clone(),
        related_truncated: finding.related_truncated,
        group_size: finding.group_size,
    }
}

/// Разрешает `guid` участников группы; неразрешимый участник не выдумывается.
fn related_guids(index: &ExportIndex<'_>, related: &[usize]) -> Vec<String> {
    related
        .iter()
        .filter_map(|position| index.notes.get(*position))
        .filter_map(|entry| entry.note.guid.clone())
        .filter(|guid| !guid.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, deck_node, export_with};

    fn run(json: &str, query: &QaQuery) -> QaResult {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        qa(Path::new("."), &index, query).expect("qa")
    }

    fn all_findings() -> QaQuery {
        QaQuery {
            codes: Vec::new(),
            max_per_code: 50,
        }
    }

    #[test]
    fn empty_value_finding_carries_address_and_evidence() {
        let result = run(MINIMAL_EXPORT, &all_findings());
        let finding = result
            .findings
            .iter()
            .find(|finding| finding.code == "empty_field_value")
            .expect("finding");

        assert_eq!(finding.note_index, 1);
        assert_eq!(finding.guid.as_deref(), Some("guid-2"));
        assert_eq!(finding.deck_path, "Test::Deck");
        assert_eq!(finding.note_model.as_deref(), Some("Тестовая модель"));
        assert_eq!(finding.note_model_uuid.as_deref(), Some("model-1"));
        assert_eq!(finding.field.as_deref(), Some("Толкование"));
        assert_eq!(finding.field_ord, Some(1));
        assert_eq!(finding.severity, QaSeverity::Warning);
        assert_eq!(result.notes_total, 2);
    }

    #[test]
    fn code_filter_selects_only_requested_rules() {
        let result = run(
            MINIMAL_EXPORT,
            &QaQuery {
                codes: vec!["empty_field_value".to_string()],
                max_per_code: 50,
            },
        );
        assert!(
            result
                .findings
                .iter()
                .all(|finding| finding.code == "empty_field_value")
        );
        assert_eq!(result.by_code.len(), 1);
        assert_eq!(result.by_code[0].code, "empty_field_value");
        assert_eq!(result.by_code[0].count, result.findings_total);
    }

    #[test]
    fn unknown_code_is_a_domain_error() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let error = qa(
            Path::new("."),
            &index,
            &QaQuery {
                codes: vec!["нет_такого".to_string()],
                max_per_code: 50,
            },
        )
        .expect_err("неизвестный код");

        assert_eq!(error.code, ErrorCode::UnknownQaCode);
        assert_eq!(error.exit_code(), 3);
        assert!(error.details["available_codes"].is_array());
        assert_eq!(error.details["unknown_codes"][0], "нет_такого");
    }

    #[test]
    fn per_code_limit_bounds_output_but_not_totals() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            for note in value["notes"].as_array_mut().expect("notes") {
                note["fields"][1] = serde_json::json!("");
            }
        });
        let result = run(
            &json,
            &QaQuery {
                codes: vec!["empty_field_value".to_string()],
                max_per_code: 1,
            },
        );

        assert_eq!(result.findings_total, 2);
        assert_eq!(result.findings_returned, 1);
        assert!(result.truncated);
        assert_eq!(result.by_code[0].count, 2);
        assert_eq!(result.findings.len(), 1);
        assert_eq!(
            result.findings[0].note_index, 0,
            "берутся первые по порядку"
        );
    }

    #[test]
    fn rules_registry_reports_applicability_and_full_counts() {
        let result = run(MINIMAL_EXPORT, &all_findings());
        assert_eq!(result.rules.len(), RULES.len());
        assert!(result.rules.iter().all(|rule| rule.applicable));
        let empty = result
            .rules
            .iter()
            .find(|rule| rule.code == "empty_field_value")
            .expect("правило");
        assert_eq!(empty.findings, 1);
        assert!(!empty.description.is_empty());
    }

    #[test]
    fn output_is_stable_between_runs() {
        let first = run(MINIMAL_EXPORT, &all_findings());
        let second = run(MINIMAL_EXPORT, &all_findings());
        assert_eq!(first.findings.len(), second.findings.len());
        for (left, right) in first.findings.iter().zip(second.findings.iter()) {
            assert_eq!(left.code, right.code);
            assert_eq!(left.note_index, right.note_index);
            assert_eq!(left.evidence, right.evidence);
        }
    }
}
