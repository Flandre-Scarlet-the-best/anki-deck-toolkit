//! Детерминированный QA-слой над содержимым карточек.
//!
//! QA отвечает на вопрос «что здесь стоит отревьюить», а не «структурно ли
//! валиден экспорт». Это разные контракты, и смешивать их нельзя:
//!
//! - [`crate::ops::validate`] владеет структурной целостностью CrowdAnki;
//!   его ERROR означает, что экспорт сломан.
//! - QA-правила ищут review-worthy проблемы содержимого. Их `error`
//!   (например, обязательный к устранению устаревший white-span) не делает
//!   экспорт структурно невалидным и не блокирует `edit`.
//!
//! Правила живут в одном реестре ([`RULES`]) и вызываются только через него.
//! Новое правило добавляется одной записью и одной функцией: ни диспетчер, ни
//! renderers менять не нужно.
//!
//! Каждое правило обязано быть детерминированным: одинаковый экспорт даёт
//! одинаковый набор замечаний в одинаковом порядке. Порядок замечаний
//! канонический и не зависит от порядка обхода хеш-таблиц:
//!
//! ```text
//! порядок правила в реестре
//! → позиция заметки в порядке экспорта
//! → позиция поля в модели
//! ```

pub mod duplicate_rules;
pub mod field_rules;
pub mod white_span;

use serde_json::Value;

use crate::index::{ExportIndex, ResolvedField, resolve_named_fields};
use crate::model::{Note, NoteModel};

/// Серьёзность QA-замечания.
///
/// Отдельный тип, а не [`crate::ops::validate::Severity`]: QA-серьёзность не
/// участвует в решении о валидности экспорта и не должна случайно начать
/// участвовать.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QaSeverity {
    /// Обязательный к устранению дефект содержимого.
    Error,
    /// Подозрительное состояние, требующее решения человека или агента.
    Warning,
    /// Диагностика: может быть нормой.
    Info,
}

impl QaSeverity {
    /// Стабильное machine-readable имя серьёзности.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// Заметка, подготовленная для правил.
#[derive(Debug)]
pub struct NoteView<'a> {
    /// Позиция заметки в порядке экспорта; она же `note_index` в выводе.
    pub position: usize,
    /// Сама заметка.
    pub note: &'a Note,
    /// Путь колоды заметки.
    pub deck_path: &'a str,
    /// Разрешённая модель заметки, если `note_model_uuid` разрешается.
    pub model: Option<&'a NoteModel>,
    /// Поля заметки в порядке `ord` модели.
    pub fields: Vec<ResolvedField<'a>>,
    /// Можно ли однозначно адресовать заметку в этом экспорте.
    ///
    /// Адресуемость — это свойство *экспорта*, а не заметки: `guid` может
    /// отсутствовать, быть пустым или повторяться, а модель — не разрешаться.
    /// Вычисляет его тот слой, у которого есть индексы ([`RuleContext::build`]),
    /// чтобы правила и команды не повторяли одну и ту же проверку по-разному.
    pub addressable: bool,
}

impl NoteView<'_> {
    /// Строковые значения полей вместе с их определениями.
    pub fn text_fields(&self) -> impl Iterator<Item = (&ResolvedField<'_>, &str)> {
        self.fields
            .iter()
            .filter_map(|field| Some((field, field.value?.as_text()?)))
    }

    /// Разрешённое значение поля по имени.
    pub fn field(&self, name: &str) -> Option<&ResolvedField<'_>> {
        self.fields.iter().find(|field| field.name == name)
    }

    /// Строковое значение поля по имени.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.field(name)?.value?.as_text()
    }
}

/// Контекст выполнения правил: заметки в порядке экспорта.
#[derive(Debug)]
pub struct RuleContext<'a> {
    /// Подготовленные заметки.
    pub views: Vec<NoteView<'a>>,
}

impl<'a> RuleContext<'a> {
    /// Готовит контекст по индексам экспорта.
    ///
    /// Разрешение полей выполняется один раз: правила только читают готовые
    /// значения и не повторяют `note_model_uuid` → `flds[].ord` → `fields`.
    #[must_use]
    pub fn build(index: &'a ExportIndex<'a>) -> Self {
        let views = index
            .notes
            .iter()
            .enumerate()
            .map(|(position, entry)| {
                let model = entry
                    .note
                    .note_model_uuid
                    .as_deref()
                    .and_then(|uuid| index.model_by_uuid(uuid));
                let fields =
                    model.map_or_else(Vec::new, |model| resolve_named_fields(entry.note, model));
                NoteView {
                    position,
                    note: entry.note,
                    deck_path: index.note_deck_path(entry),
                    model,
                    fields,
                    addressable: is_addressable(index, entry),
                }
            })
            .collect();
        Self { views }
    }

    /// Заметки, которые можно адресовать однозначно.
    ///
    /// Правила о группах заметок работают только через этот итератор: находить
    /// группу по `guid`, который нельзя процитировать внешнему агенту, значило бы
    /// выдавать неисполнимую рекомендацию.
    pub fn addressable(&self) -> impl Iterator<Item = &NoteView<'a>> {
        self.views.iter().filter(|view| view.addressable)
    }
}

/// Разрешается ли заметка в этом экспорте однозначно.
///
/// Уникальность `guid` не проверяется здесь заново: индекс экспорта уже знает
/// все позиции каждого `guid`, и повторный `guid` для адресации неоднозначен
/// ровно так же, как для `find` и `review`.
fn is_addressable(index: &ExportIndex<'_>, entry: &crate::index::NoteRef<'_>) -> bool {
    let model_resolves = entry
        .note
        .note_model_uuid
        .as_deref()
        .is_some_and(|uuid| index.model_by_uuid(uuid).is_some());
    let guid = entry.note.guid.as_deref().unwrap_or("");
    model_resolves && !guid.is_empty() && index.note_positions_by_guid(guid).len() == 1
}

/// Ненормализованный факт, найденный правилом.
#[derive(Debug)]
pub struct RawFinding {
    /// Позиция заметки в порядке экспорта.
    pub note_position: usize,
    /// Имя поля, если замечание относится к полю.
    pub field: Option<String>,
    /// Позиция поля в модели, если применима.
    pub field_ord: Option<i64>,
    /// Человекочитаемое описание.
    pub message: String,
    /// Machine-readable детали; объём значения здесь не ограничен, размер
    /// выборок ограничивает само правило.
    pub evidence: Value,
    /// Позиции остальных заметок группы, если замечание — про группу.
    ///
    /// Ограничено [`duplicate_rules::MAX_RELATED_NOTES`]: это структурированный
    /// ответ на вопрос «кто ещё в группе», и его читают и `qa`, и `review`.
    pub related: Vec<usize>,
    /// Была ли группа обрезана до [`duplicate_rules::MAX_RELATED_NOTES`].
    pub related_truncated: bool,
    /// Размер группы целиком, если замечание — про группу.
    pub group_size: Option<usize>,
}

/// Одно зарегистрированное QA-правило.
pub struct Rule {
    /// Стабильный код замечания.
    pub code: &'static str,
    /// Серьёзность.
    pub severity: QaSeverity,
    /// Что правило ищет; попадает в `qa --json` как описание реестра.
    pub description: &'static str,
    /// Применимо ли правило к этому экспорту.
    ///
    /// Неприменимое правило не выдаёт замечания и не считается ошибкой: это
    /// документированный нейтральный результат, а не структурная проблема.
    pub applicable: fn(&RuleContext<'_>) -> bool,
    /// Выполнение правила; результат обязан быть детерминированным.
    pub run: fn(&RuleContext<'_>) -> Vec<RawFinding>,
}

impl Rule {
    /// Находит замечания правила.
    #[must_use]
    pub fn findings(&self, context: &RuleContext<'_>) -> Vec<RawFinding> {
        if !(self.applicable)(context) {
            return Vec::new();
        }
        (self.run)(context)
    }
}

/// Реестр QA-правил.
pub const RULES: &[Rule] = &[
    Rule {
        code: "empty_field_value",
        severity: QaSeverity::Warning,
        description: "значение поля пустое, хотя поле присутствует в модели",
        applicable: |_| true,
        run: field_rules::empty_field_value,
    },
    Rule {
        code: "leading_whitespace",
        severity: QaSeverity::Warning,
        description: "значение поля начинается с whitespace",
        applicable: |_| true,
        run: field_rules::leading_whitespace,
    },
    Rule {
        code: "trailing_whitespace",
        severity: QaSeverity::Warning,
        description: "значение поля заканчивается whitespace",
        applicable: |_| true,
        run: field_rules::trailing_whitespace,
    },
    Rule {
        code: "forbidden_white_span",
        severity: QaSeverity::Error,
        description: "значение содержит устаревшую белую <span>-обёртку",
        applicable: |_| true,
        run: field_rules::forbidden_white_span,
    },
    Rule {
        code: "duplicate_note_content",
        severity: QaSeverity::Warning,
        description: "несколько заметок одной модели имеют полностью одинаковые значения fields",
        applicable: |_| true,
        run: duplicate_rules::duplicate_note_content,
    },
];

/// Индекс правила в реестре; он же приоритет в каноническом порядке замечания.
#[must_use]
pub fn rule_index(code: &str) -> Option<usize> {
    RULES.iter().position(|rule| rule.code == code)
}

/// Известен ли такой код QA-замечания.
#[must_use]
pub fn is_known_code(code: &str) -> bool {
    rule_index(code).is_some()
}

/// Коды правил в порядке реестра.
pub fn codes() -> impl Iterator<Item = &'static str> {
    RULES.iter().map(|rule| rule.code)
}

/// Найденное QA-замечание в канонической форме.
#[derive(Debug)]
pub struct Finding {
    /// Стабильный код правила.
    pub code: &'static str,
    /// Серьёзность правила.
    pub severity: QaSeverity,
    /// Индекс правила в реестре.
    pub rule_index: usize,
    /// Позиция заметки в порядке экспорта.
    pub note_position: usize,
    /// Имя поля, если замечание относится к полю.
    pub field: Option<String>,
    /// Позиция поля в модели, если применима.
    pub field_ord: Option<i64>,
    /// Человекочитаемое описание.
    pub message: String,
    /// Machine-readable детали.
    pub evidence: Value,
    /// Можно ли адресовать заметку замечания однозначно.
    pub addressable: bool,
    /// Позиции остальных заметок группы.
    pub related: Vec<usize>,
    /// Была ли группа обрезана.
    pub related_truncated: bool,
    /// Размер группы целиком.
    pub group_size: Option<usize>,
}

/// Выполняет все применимые правила и возвращает замечания в каноническом
/// порядке.
///
/// Порядок стабилизируется явной сортировкой, а не порядком работы правил:
/// правило может собирать группы через хеш-таблицу, и его внутренний порядок
/// не должен влиять на контракт вывода.
#[must_use]
pub fn collect(context: &RuleContext<'_>) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    for (rule_index, rule) in RULES.iter().enumerate() {
        for raw in rule.findings(context) {
            findings.push(Finding {
                code: rule.code,
                severity: rule.severity,
                rule_index,
                note_position: raw.note_position,
                field: raw.field,
                field_ord: raw.field_ord,
                message: raw.message,
                evidence: raw.evidence,
                addressable: context.views[raw.note_position].addressable,
                related: raw.related,
                related_truncated: raw.related_truncated,
                group_size: raw.group_size,
            });
        }
    }

    findings.sort_by(|left, right| {
        (
            left.rule_index,
            left.note_position,
            left.field_ord.unwrap_or(i64::MAX),
            left.field.as_deref(),
        )
            .cmp(&(
                right.rule_index,
                right.note_position,
                right.field_ord.unwrap_or(i64::MAX),
                right.field.as_deref(),
            ))
    });
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, deck_node};
    use serde_json::json;

    fn findings(json: &str) -> Vec<Finding> {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        collect(&context)
    }

    #[test]
    fn registry_codes_are_unique_and_snake_case() {
        for (index, rule) in RULES.iter().enumerate() {
            assert_eq!(rule_index(rule.code), Some(index), "{}", rule.code);
            assert!(
                rule.code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "код {} не snake_case",
                rule.code
            );
            assert!(!rule.description.is_empty(), "{} без описания", rule.code);
        }
        assert!(!is_known_code("нет_такого_кода"));
    }

    #[test]
    fn registry_matches_the_documented_rule_set() {
        let actual: Vec<(&str, QaSeverity)> = RULES
            .iter()
            .map(|rule| (rule.code, rule.severity))
            .collect();
        assert_eq!(
            actual,
            vec![
                ("empty_field_value", QaSeverity::Warning),
                ("leading_whitespace", QaSeverity::Warning),
                ("trailing_whitespace", QaSeverity::Warning),
                ("forbidden_white_span", QaSeverity::Error),
                ("duplicate_note_content", QaSeverity::Warning),
            ],
            "реестр QA-правил — публичный контракт: его изменение должно быть осознанным"
        );
    }

    /// QA-правила судят о значениях, а не об именах полей конкретной модели.
    ///
    /// Один и тот же набор значений под разными именами полей и моделей обязан
    /// дать один и тот же набор замечаний.
    #[test]
    fn rules_are_independent_of_field_and_model_names() {
        let renamed = crate::test_support::export_with(MINIMAL_EXPORT, |value| {
            value["note_models"][0]["name"] = json!("Другая модель");
            value["note_models"][0]["flds"] = json!([
                {"name": "Альфа", "ord": 0},
                {"name": "Бета", "ord": 1}
            ]);
        });
        let baseline = crate::test_support::export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"] = json!(["[sound:a.mp3]偶然", " значение "]);
        });
        let renamed = crate::test_support::export_with(&renamed, |value| {
            value["notes"][0]["fields"] = json!(["[sound:a.mp3]偶然", " значение "]);
        });

        let shape = |found: &[Finding]| -> Vec<(String, usize, Option<i64>)> {
            found
                .iter()
                .map(|finding| {
                    (
                        finding.code.to_string(),
                        finding.note_position,
                        finding.field_ord,
                    )
                })
                .collect()
        };
        let baseline_findings = shape(&findings(&baseline));
        assert!(
            !baseline_findings.is_empty(),
            "фикстура должна давать замечания, иначе равенство ниже тривиально"
        );
        assert_eq!(baseline_findings, shape(&findings(&renamed)));
    }

    #[test]
    fn findings_are_ordered_by_rule_position_and_field() {
        let json = crate::test_support::export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = serde_json::json!(" значение ");
            value["notes"][1]["fields"][1] = serde_json::json!("значение");
        });
        let found = findings(&json);

        let keys: Vec<(usize, usize, Option<i64>)> = found
            .iter()
            .map(|finding| (finding.rule_index, finding.note_position, finding.field_ord))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "порядок замечаний должен быть каноническим");

        assert_eq!(
            found
                .iter()
                .filter(|finding| finding.code == "empty_field_value")
                .count(),
            0,
            "оба значения «Толкование» заданы явно и не пусты"
        );
        assert!(
            found
                .iter()
                .any(|finding| finding.code == "leading_whitespace"),
            "ведущий пробел должен быть найден"
        );
        assert!(
            found
                .iter()
                .any(|finding| finding.code == "trailing_whitespace"),
            "замыкающий пробел должен быть найден"
        );
    }

    #[test]
    fn severity_is_independent_from_validate() {
        assert_eq!(QaSeverity::Error.as_str(), "error");
        assert_eq!(QaSeverity::Warning.as_str(), "warning");
        assert_eq!(QaSeverity::Info.as_str(), "info");
        assert_eq!(
            RULES
                .iter()
                .find(|rule| rule.code == "forbidden_white_span")
                .map(|rule| rule.severity),
            Some(QaSeverity::Error)
        );
    }

    #[test]
    fn every_rule_is_deterministic() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);

        let first = collect(&context);
        let second = collect(&context);
        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(second.iter()) {
            assert_eq!(left.code, right.code);
            assert_eq!(left.note_position, right.note_position);
            assert_eq!(left.evidence, right.evidence);
        }
    }
}
