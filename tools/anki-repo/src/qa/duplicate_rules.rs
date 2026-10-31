//! Правило дубликатов содержимого: несколько заметок одной модели имеют
//! полностью одинаковые значения `fields`.
//!
//! Правило групповое: замечание адресуется группе, а не каждой её заметке, и
//! несёт структурированный список участников ([`RawFinding::related`]), чтобы
//! ревьюер видел всех участников сразу и не собирал тот же факт из нескольких
//! записей. Участники — часть domain-результата, а не украшение evidence:
//! по ним `review --qa-code` расширяет batch, поэтому renderers не имеют права
//! вычислять группу заново.
//!
//! Группировка идёт по разрешённой модели (`note_model_uuid`) и по сырым
//! значениям полей. Никакого «похоже» здесь нет: сравнение точное, поэтому
//! правило детерминировано и не требует эвристик.
//!
//! Правило сравнивает заметку целиком, а не отдельное «главное» поле: понятие
//! главного поля принадлежит конкретной модели, а не формату CrowdAnki, поэтому
//! у toolkit'а нет корректной универсальной семантики для него. Разделять
//! заметки по значению выбранного поля агент может явным `--field`/`--value`
//! в `find` и `review` — там имя поля называет вызывающая сторона.

use std::collections::BTreeMap;

use serde_json::json;

use crate::qa::{NoteView, RawFinding, RuleContext};

/// Предел числа участников группы в замечании.
pub const MAX_RELATED_NOTES: usize = 20;

/// Заметки двух разных участников группы.
type Group<'a> = Vec<&'a NoteView<'a>>;

/// Несколько заметок одной модели имеют полностью одинаковые значения `fields`.
///
/// Одинаковым считается набор значений всех полей заметки. Порядок полей —
/// порядок `ord` модели, поэтому две заметки с одним содержимым, но разным
/// расположением значений, дубликатами не считаются.
pub fn duplicate_note_content(context: &RuleContext<'_>) -> Vec<RawFinding> {
    let mut groups: BTreeMap<(String, String), Group<'_>> = BTreeMap::new();
    for view in context.addressable() {
        let Some(uuid) = view.note.note_model_uuid.as_deref() else {
            continue;
        };
        groups
            .entry((uuid.to_string(), content_fingerprint(view)))
            .or_default()
            .push(view);
    }

    let mut findings = Vec::new();
    for members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        let first = members[0];
        let related = Related::new(&members);
        findings.push(RawFinding {
            note_position: first.position,
            field: None,
            field_ord: None,
            message: format!(
                "{} заметок модели {} имеют полностью одинаковые значения fields",
                members.len(),
                model_label(first)
            ),
            evidence: json!({
                "fields_total": first.note.fields.len(),
            }),
            related: related.positions,
            related_truncated: related.truncated,
            group_size: Some(members.len()),
        });
    }

    findings.sort_by_key(|finding| finding.note_position);
    findings
}

/// Ограниченный список участников группы.
///
/// Позиции хранятся в порядке экспорта и включают саму заметку замечания:
/// потребитель (batch `review`) не должен досчитывать её сам.
struct Related {
    positions: Vec<usize>,
    truncated: bool,
}

impl Related {
    fn new(members: &[&NoteView<'_>]) -> Self {
        Self {
            positions: members
                .iter()
                .map(|view| view.position)
                .take(MAX_RELATED_NOTES)
                .collect(),
            truncated: members.len() > MAX_RELATED_NOTES,
        }
    }
}

/// Отпечаток содержимого заметки: значения полей в порядке `fields`.
///
/// Каждое значение предваряется собственной длиной, поэтому склейка разных
/// наборов значений не может дать одинаковый отпечаток.
fn content_fingerprint(view: &NoteView<'_>) -> String {
    let mut fingerprint = String::new();
    for field in &view.note.fields {
        let rendered = field.rendered();
        fingerprint.push_str(&rendered.len().to_string());
        fingerprint.push(':');
        fingerprint.push_str(&rendered);
        fingerprint.push('\u{1}');
    }
    fingerprint
}

/// Имя модели заметки для human/message-текста.
fn model_label(view: &NoteView<'_>) -> String {
    view.model
        .and_then(|model| model.name.as_deref())
        .unwrap_or("(без имени)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ExportIndex;
    use crate::qa::{Finding, RuleContext, collect};
    use crate::test_support::{MINIMAL_EXPORT, deck_node, export_with};

    fn findings_for(json: &str, code: &str) -> Vec<Finding> {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        collect(&context)
            .into_iter()
            .filter(|finding| finding.code == code)
            .collect()
    }

    /// Экспорт с тремя заметками: вторая повторяет первую, третья уникальна.
    fn duplicated_export() -> String {
        export_with(MINIMAL_EXPORT, |value| {
            let first = value["notes"][0].clone();
            let mut copy = first.clone();
            copy["guid"] = json!("guid-3");
            value["notes"].as_array_mut().expect("notes").push(copy);
            let mut other = first;
            other["guid"] = json!("guid-4");
            other["fields"] = json!(["偶然", "случайность"]);
            value["notes"].as_array_mut().expect("notes").push(other);
        })
    }

    #[test]
    fn identical_fields_are_grouped_once() {
        let found = findings_for(&duplicated_export(), "duplicate_note_content");
        assert_eq!(found.len(), 1, "одна группа, а не три замечания");
        assert_eq!(found[0].note_position, 0);
        assert_eq!(found[0].field, None);
        assert_eq!(found[0].group_size, Some(2));
        assert_eq!(found[0].related, vec![0, 2]);
        assert!(!found[0].related_truncated);
    }

    #[test]
    fn same_values_in_different_models_are_not_duplicates() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut model = value["note_models"][0].clone();
            model["crowdanki_uuid"] = json!("model-2");
            model["name"] = json!("Другая модель");
            value["note_models"]
                .as_array_mut()
                .expect("models")
                .push(model);

            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-other-model");
            copy["note_model_uuid"] = json!("model-2");
            value["notes"].as_array_mut().expect("notes").push(copy);
        });

        assert!(
            findings_for(&json, "duplicate_note_content").is_empty(),
            "разные модели — разные наборы совместимости"
        );
    }

    #[test]
    fn different_field_order_is_not_a_duplicate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-3");
            copy["fields"] = json!(["случайность", "[sound:a.mp3]偶然"]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        });
        assert!(findings_for(&json, "duplicate_note_content").is_empty());
    }

    #[test]
    fn notes_without_guid_do_not_participate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!(null);
            value["notes"].as_array_mut().expect("notes").push(copy);
        });
        assert!(
            findings_for(&json, "duplicate_note_content").is_empty(),
            "группу нельзя адресовать без guid"
        );
    }

    #[test]
    fn unresolved_models_do_not_participate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            for note in value["notes"].as_array_mut().expect("notes") {
                note["note_model_uuid"] = json!("нет-такой-модели");
            }
        });
        assert!(findings_for(&json, "duplicate_note_content").is_empty());
    }

    #[test]
    fn related_lists_are_bounded() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let template = value["notes"][0].clone();
            for index in 0..(MAX_RELATED_NOTES + 5) {
                let mut copy = template.clone();
                copy["guid"] = json!(format!("guid-bulk-{index}"));
                value["notes"].as_array_mut().expect("notes").push(copy);
            }
        });

        let found = findings_for(&json, "duplicate_note_content");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].group_size,
            Some(MAX_RELATED_NOTES + 6),
            "размер группы считается целиком"
        );
        assert_eq!(
            found[0].related.len(),
            MAX_RELATED_NOTES,
            "список участников обрезан до предела"
        );
        assert!(found[0].related_truncated);
    }
}
