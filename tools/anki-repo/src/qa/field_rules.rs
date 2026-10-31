//! Правила уровня значений полей: пустое значение, краевые пробелы и
//! устаревшая белая `<span>`-обёртка.
//!
//! Все три правила работают только со строковыми значениями. Нестроковое
//! значение или отсутствующая позиция — структурная проблема, и её владелец —
//! [`crate::ops::validate`]; QA не дублирует структурную диагностику и не
//! превращает её в content-замечание.

use serde_json::json;

use crate::qa::white_span::scan_white_spans;
use crate::qa::{RawFinding, RuleContext};
use crate::text::{bounded_sample, escape_whitespace};

/// Предел числа экранируемых whitespace-символов в evidence.
const WHITESPACE_SAMPLE_LIMIT: usize = 8;

/// Значение поля равно пустой строке.
pub fn empty_field_value(context: &RuleContext<'_>) -> Vec<RawFinding> {
    let mut findings = Vec::new();
    for view in &context.views {
        for (field, text) in view.text_fields() {
            if !text.is_empty() {
                continue;
            }
            findings.push(RawFinding {
                note_position: view.position,
                field: Some(field.name.to_string()),
                field_ord: field.ord.value(),
                message: format!("значение поля {:?} пустое", field.name),
                evidence: json!({
                    "sample": "",
                    "value_chars": 0,
                }),
                related: Vec::new(),
                related_truncated: false,
                group_size: None,
            });
        }
    }
    findings
}

/// Значение поля начинается с whitespace.
///
/// Значение целиком из whitespace даёт и `leading_whitespace`, и
/// `trailing_whitespace`: это два независимых свойства одного значения, и
/// скрывать одно из них значило бы терять информацию.
pub fn leading_whitespace(context: &RuleContext<'_>) -> Vec<RawFinding> {
    whitespace_rule(context, "leading", |text| {
        let trimmed = text.trim_start();
        (trimmed.len() != text.len()).then(|| &text[..text.len() - trimmed.len()])
    })
}

/// Значение поля заканчивается whitespace.
pub fn trailing_whitespace(context: &RuleContext<'_>) -> Vec<RawFinding> {
    whitespace_rule(context, "trailing", |text| {
        let trimmed = text.trim_end();
        (trimmed.len() != text.len()).then(|| &text[trimmed.len()..])
    })
}

/// Общая реализация краевых whitespace-правил.
///
/// `boundary` возвращает саму whitespace-последовательность, если она есть.
fn whitespace_rule(
    context: &RuleContext<'_>,
    kind: &'static str,
    boundary: impl Fn(&str) -> Option<&str>,
) -> Vec<RawFinding> {
    let mut findings = Vec::new();
    for view in &context.views {
        for (field, text) in view.text_fields() {
            let Some(whitespace) = boundary(text) else {
                continue;
            };
            findings.push(RawFinding {
                note_position: view.position,
                field: Some(field.name.to_string()),
                field_ord: field.ord.value(),
                message: format!(
                    "значение поля {:?} {} с whitespace ({})",
                    field.name,
                    if kind == "leading" {
                        "начинается"
                    } else {
                        "заканчивается"
                    },
                    escape_whitespace(whitespace, WHITESPACE_SAMPLE_LIMIT)
                ),
                evidence: json!({
                    "sample": bounded_sample(text),
                    "value_chars": text.chars().count(),
                    "boundary": {
                        "kind": kind,
                        "escaped": escape_whitespace(whitespace, WHITESPACE_SAMPLE_LIMIT),
                        "chars": whitespace.chars().count(),
                    },
                }),
                related: Vec::new(),
                related_truncated: false,
                group_size: None,
            });
        }
    }
    findings
}

/// Значение поля содержит `<span>` с белым цветом текста.
pub fn forbidden_white_span(context: &RuleContext<'_>) -> Vec<RawFinding> {
    let mut findings = Vec::new();
    for view in &context.views {
        for (field, text) in view.text_fields() {
            let scan = scan_white_spans(text);
            if scan.occurrences == 0 {
                continue;
            }
            findings.push(RawFinding {
                note_position: view.position,
                field: Some(field.name.to_string()),
                field_ord: field.ord.value(),
                message: format!(
                    "значение поля {:?} содержит {} белых <span>-обёрток",
                    field.name, scan.occurrences
                ),
                evidence: json!({
                    "sample": bounded_sample(text),
                    "value_chars": text.chars().count(),
                    "occurrences": scan.occurrences,
                    "first_tag": scan.first_tag,
                }),
                related: Vec::new(),
                related_truncated: false,
                group_size: None,
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ExportIndex;
    use crate::qa::{RULES, RuleContext, collect};
    use crate::test_support::{MINIMAL_EXPORT, deck_node, export_with};

    fn findings_for(json: &str, code: &str) -> Vec<crate::qa::Finding> {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        collect(&context)
            .into_iter()
            .filter(|finding| finding.code == code)
            .collect()
    }

    #[test]
    fn empty_field_values_are_reported_per_field() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = json!("");
        });
        let found = findings_for(&json, "empty_field_value");
        assert_eq!(
            found.len(),
            2,
            "пустые «Толкование» первой и второй заметок"
        );
        assert_eq!(found[0].note_position, 0);
        assert_eq!(found[0].field.as_deref(), Some("Толкование"));
        assert_eq!(found[0].field_ord, Some(1));
        assert_eq!(found[0].evidence["value_chars"], 0);
    }

    #[test]
    fn whitespace_edges_are_reported_with_escaped_evidence() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][0] = json!(" 偶然");
            value["notes"][1]["fields"][0] = json!("必然 ");
        });

        let leading = findings_for(&json, "leading_whitespace");
        assert_eq!(leading.len(), 1);
        assert_eq!(leading[0].note_position, 0);
        assert_eq!(leading[0].evidence["boundary"]["kind"], "leading");
        assert_eq!(leading[0].evidence["boundary"]["escaped"], "\\u{20}");
        assert_eq!(leading[0].evidence["boundary"]["chars"], 1);

        let trailing = findings_for(&json, "trailing_whitespace");
        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].note_position, 1);
        assert_eq!(trailing[0].evidence["boundary"]["kind"], "trailing");
    }

    #[test]
    fn all_whitespace_value_reports_both_edges() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = json!("   ");
        });
        assert_eq!(findings_for(&json, "leading_whitespace").len(), 1);
        assert_eq!(findings_for(&json, "trailing_whitespace").len(), 1);
        assert!(
            findings_for(&json, "empty_field_value")
                .iter()
                .all(|finding| finding.note_position != 0),
            "значение из пробелов не является пустой строкой"
        );
    }

    #[test]
    fn inner_whitespace_is_not_an_edge() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][0] = json!("偶 然");
            value["notes"][1]["fields"][0] = json!("必 然");
        });
        assert!(
            findings_for(&json, "leading_whitespace").is_empty()
                && findings_for(&json, "trailing_whitespace").is_empty(),
            "пробел внутри значения не является краевым"
        );
    }

    #[test]
    fn unicode_whitespace_is_escaped_in_evidence() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][1]["fields"][0] = json!("\u{a0}必然 ");
        });
        let leading = findings_for(&json, "leading_whitespace");
        assert_eq!(leading.len(), 1);
        assert_eq!(leading[0].note_position, 1);
        assert_eq!(leading[0].evidence["boundary"]["escaped"], "\\u{a0}");

        let trailing = findings_for(&json, "trailing_whitespace");
        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].evidence["boundary"]["escaped"], "\\u{20}");
    }

    #[test]
    fn forbidden_white_span_is_reported_with_occurrences() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] =
                json!(r#"<span style="color: rgb(255, 255, 255);">скрыто</span>"#);
        });
        let found = findings_for(&json, "forbidden_white_span");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].note_position, 0);
        assert_eq!(found[0].severity, crate::qa::QaSeverity::Error);
        assert_eq!(found[0].evidence["occurrences"], 1);
        assert!(
            found[0].evidence["first_tag"]
                .as_str()
                .is_some_and(|tag| tag.contains("rgb(255, 255, 255)"))
        );
    }

    #[test]
    fn non_string_values_are_left_to_validate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = json!(42);
            value["notes"][1]["fields"] = json!(["только одно значение"]);
        });
        let node = deck_node(&json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);

        assert!(rs_is_empty(&context, "empty_field_value"));
        assert!(rs_is_empty(&context, "leading_whitespace"));
        assert!(rs_is_empty(&context, "forbidden_white_span"));
        assert_eq!(
            context.views[0].text("Заголовок"),
            Some("[sound:a.mp3]偶然"),
            "строковое поле остаётся доступным"
        );
        assert_eq!(
            context.views[0].text("Толкование"),
            None,
            "нестроковое значение не даёт текста"
        );
        assert_eq!(
            context.views[1].text("Толкование"),
            None,
            "отсутствующая позиция не даёт текста"
        );
    }

    fn rs_is_empty(context: &RuleContext<'_>, code: &str) -> bool {
        let rule = RULES
            .iter()
            .find(|rule| rule.code == code)
            .expect("правило");
        rule.findings(context).is_empty()
    }

    #[test]
    fn unresolved_model_produces_no_field_findings() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["note_model_uuid"] = json!("нет-такой-модели");
            value["notes"][0]["fields"] = json!(["", ""]);
        });
        let node = deck_node(&json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        assert!(context.views[0].model.is_none());
        assert!(context.views[0].fields.is_empty());
        assert_eq!(
            findings_for(&json, "empty_field_value")
                .iter()
                .filter(|finding| finding.note_position == 0)
                .count(),
            0,
            "неразрешённая модель — не content-замечание"
        );
    }

    #[test]
    fn note_without_guid_still_participates_in_field_rules() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["guid"] = json!(null);
            value["notes"][0]["fields"][1] = json!("");
        });
        // Правило описывает содержимое, а не адресуемость: заметка без guid
        // по-прежнему даёт замечание, но оно честно помечено как неисполнимое.
        let found = findings_for(&json, "empty_field_value");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].note_position, 0);
        assert!(!found[0].addressable);
        assert!(found[1].addressable);
        assert!(
            found.iter().any(|finding| finding.note_position == 0
                && finding.field.as_deref() == Some("Толкование")),
            "заметка без guid всё равно описывается позицией в экспорте"
        );
    }

    #[test]
    fn field_rule_evidence_samples_are_bounded() {
        let long = "я".repeat(crate::text::VALUE_SAMPLE_CHARS + 50);
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = json!(format!("{long} "));
        });
        let found = findings_for(&json, "trailing_whitespace");
        assert_eq!(found.len(), 1);
        let sample = found[0].evidence["sample"].as_str().expect("sample");
        assert!(
            sample.ends_with('…'),
            "выборка должна быть усечена: {sample}"
        );
        assert_eq!(found[0].evidence["value_chars"], long.chars().count() + 1);
    }

    #[test]
    fn several_matches_in_one_value_produce_one_finding() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] =
                json!(r#"<span style="color: #fff">a</span><span style="color: #ffffff">b</span>"#);
        });
        let found = findings_for(&json, "forbidden_white_span");
        assert_eq!(
            found.len(),
            1,
            "замечание адресуется полю, а не каждому span"
        );
        assert_eq!(found[0].evidence["occurrences"], 2);
    }
}
