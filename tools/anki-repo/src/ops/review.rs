//! Операция `review`: bounded batch'и карточек для внешнего LLM/coding agent.
//!
//! Команда не анализирует содержимое и не вызывает никаких LLM API. Её задача —
//! детерминированно выбрать заметки и отдать их компактным постраничным
//! batch'ем с разрешёнными именованными полями, чтобы агент не читал
//! многомегабайтный `deck.json` ради нескольких карточек.
//!
//! Выбор заметок полностью делегирован общему слою ([`crate::selection`]):
//! `--guid`, `--field/--value/--match` и `--deck` означают здесь ровно то же,
//! что и в `find`.
//!
//! Контракт пустого результата:
//!
//! - `--guid` — это identity lookup: ненайденный `guid` даёт exit 4, повторённый
//!   в экспорте — exit 5, как в `find`;
//! - критерий-фильтр (`all`, поле, QA-код) и `--offset` за концом выборки дают
//!   валидную пустую страницу с exit 0: без этого невозможна пагинация.
//!
//! Второй контракт — адресуемость batch'а. Batch существует, чтобы внешний агент
//! предложил правки, а предложение исполнимо только для заметки, которую можно
//! однозначно назвать (`guid` есть, он уникален, модель разрешается). Поэтому
//! заметка, которую нельзя адресовать, в batch не попадает, а её число
//! сообщается отдельно ([`ReviewResult::excluded_unaddressable`]) — молча
//! выбросить её нельзя. Структурные причины такой заметки остаются предметом
//! [`crate::ops::validate`], `review` их не диагностирует.
//!
//! Третий контракт — контекст группы. Для групповых QA-замечаний
//! (например, `duplicate_note_content`) в batch попадают все
//! участники группы, а не только та заметка, к которой приписано замечание: иначе
//! агент не может судить о дубликате, не прочитав `deck.json`. Участники берутся
//! из domain-результата ([`qa::Finding::related`]), а не собираются здесь
//! повторно; границы списка сообщаются полями `group_size` и
//! `related_truncated`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::ops::NoteSummary;
use crate::qa::{self, QaSeverity, RuleContext};
use crate::selection::{self, MatchMode};

/// Максимум QA-замечаний, приложенных к одной карточке batch'а.
pub const MAX_FINDINGS_PER_ITEM: usize = 20;

/// Критерий выбора заметок для review.
#[derive(Debug, Clone)]
pub enum ReviewCriteria {
    /// Все заметки области.
    All,
    /// Одна заметка по `guid`.
    Guid {
        /// Искомый идентификатор.
        guid: String,
    },
    /// Заметки, у которых поле совпадает со значением.
    Field {
        /// Имя поля модели.
        field: String,
        /// Искомое значение.
        value: String,
        /// Режим сопоставления.
        mode: MatchMode,
    },
    /// Заметки, у которых есть QA-замечание указанного кода.
    QaCode {
        /// Код правила.
        code: String,
    },
}

/// Параметры запроса `review`.
#[derive(Debug, Clone)]
pub struct ReviewQuery {
    /// Критерий выбора.
    pub criteria: ReviewCriteria,
    /// Необязательное ограничение по пути колоды.
    pub deck: Option<String>,
    /// Сколько выбранных заметок пропустить.
    pub offset: usize,
    /// Предел числа заметок в одной странице.
    pub limit: usize,
}

/// Machine-readable описание применённого критерия.
#[derive(Debug)]
pub struct SelectionSummary {
    /// `all`, `guid`, `field` или `qa_code`.
    pub kind: &'static str,
    /// Искомый `guid`.
    pub guid: Option<String>,
    /// Имя поля.
    pub field: Option<String>,
    /// Искомое значение.
    pub value: Option<String>,
    /// Режим сопоставления.
    pub match_mode: Option<&'static str>,
    /// Код QA-правила.
    pub qa_code: Option<String>,
    /// Ограничение по колоде.
    pub deck: Option<String>,
}

/// Другой участник группы в контексте замечания.
#[derive(Debug, Clone)]
pub struct RelatedNote {
    /// Позиция заметки в порядке экспорта.
    pub note_index: usize,
    /// `guid` участника; `None`, если участник неадресуем и его нельзя назвать.
    pub guid: Option<String>,
}

/// QA-замечание в компактной форме для batch'а.
#[derive(Debug, Clone)]
pub struct FindingSummary {
    /// Код правила.
    pub code: &'static str,
    /// Серьёзность.
    pub severity: QaSeverity,
    /// Имя поля, если замечание относится к полю.
    pub field: Option<String>,
    /// Человекочитаемое описание.
    pub message: String,
    /// Размер группы целиком, если замечание — про группу.
    pub group_size: Option<usize>,
    /// Остальные участники группы, включая неадресуемых.
    pub related: Vec<RelatedNote>,
    /// Был ли список участников обрезан до предела правила.
    pub related_truncated: bool,
}

/// Роль заметки в групповом замечании, найденном в этом batch'е.
#[derive(Debug, Clone)]
pub struct GroupMembership {
    /// Код правила группы.
    pub code: &'static str,
    /// Позиция заметки, к которой приписано замечание группы.
    pub owner_note_index: usize,
    /// `guid` этой заметки.
    pub owner_guid: Option<String>,
    /// Размер группы целиком.
    pub group_size: usize,
}

/// Одна карточка batch'а.
#[derive(Debug)]
pub struct ReviewItem {
    /// Позиция заметки в порядке экспорта.
    pub note_index: usize,
    /// Сводка заметки с именованными полями.
    pub note: NoteSummary,
    /// QA-замечания этой заметки.
    pub qa_findings: Vec<FindingSummary>,
    /// Есть ли замечания, не попавшие в карточку.
    pub qa_findings_truncated: bool,
    /// Группы из этого batch'а, в которых заметка — участник, а не владелец.
    pub group_membership: Vec<GroupMembership>,
}

/// Результат `review`.
#[derive(Debug)]
pub struct ReviewResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Применённый критерий.
    pub selection: SelectionSummary,
    /// Всего заметок в экспорте.
    pub notes_total: usize,
    /// Сколько заметок выбрано до применения `offset`/`limit`.
    pub total_selected: usize,
    /// Применённое смещение.
    pub offset: usize,
    /// Применённый предел страницы.
    pub limit: usize,
    /// Сколько заметок в этой странице.
    pub returned: usize,
    /// Есть ли ещё заметки после этой страницы.
    pub truncated: bool,
    /// Смещение следующей страницы, если она есть.
    pub next_offset: Option<usize>,
    /// Карточки этой страницы в порядке экспорта.
    pub items: Vec<ReviewItem>,
    /// Сколько заметок критерий отобрал, но batch не взял из-за неадресуемости.
    pub excluded_unaddressable: usize,
}

/// Выполняет `review`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownDeck`], [`ErrorCode::UnknownField`],
/// [`ErrorCode::UnknownQaCode`], [`ErrorCode::NotFound`] и
/// [`ErrorCode::Ambiguous`] для неразрешимых критериев.
pub fn review(
    export_dir: &Path,
    index: &ExportIndex<'_>,
    query: &ReviewQuery,
) -> Result<ReviewResult, DomainError> {
    let scope = selection::resolve_deck_scope(index, query.deck.as_deref())?;
    let context = RuleContext::build(index);
    let findings = qa::collect(&context);
    let matched = select(index, &findings, query, scope)?;

    // Адресуемость — свойство экспорта, и оно уже вычислено QA-слоем один раз
    // для всех команд. Здесь заметка только отсеивается, но не диагностируется:
    // структурные причины принадлежат `validate`.
    let mut excluded_unaddressable = 0;
    let mut selected = Vec::with_capacity(matched.len());
    for position in matched {
        if context.views[position].addressable {
            selected.push(position);
        } else {
            excluded_unaddressable += 1;
        }
    }

    let total_selected = selected.len();
    let start = query.offset.min(total_selected);
    let end = start.saturating_add(query.limit).min(total_selected);
    let page = &selected[start..end];

    let mut by_position: BTreeMap<usize, Vec<FindingSummary>> = BTreeMap::new();
    let mut memberships: BTreeMap<usize, Vec<GroupMembership>> = BTreeMap::new();
    for finding in &findings {
        by_position
            .entry(finding.note_position)
            .or_default()
            .push(FindingSummary {
                code: finding.code,
                severity: finding.severity,
                field: finding.field.clone(),
                message: finding.message.clone(),
                group_size: finding.group_size,
                related: related_notes(index, &finding.related, finding.note_position),
                related_truncated: finding.related_truncated,
            });
        // Участник группы тоже должен понимать, почему он в batch'е: сам
        // замечание приписано владельцу, и без этой ссылки batch выглядел бы
        // набором несвязанных заметок.
        if finding.group_size.is_some() {
            for position in &finding.related {
                if *position == finding.note_position {
                    continue;
                }
                memberships
                    .entry(*position)
                    .or_default()
                    .push(GroupMembership {
                        code: finding.code,
                        owner_note_index: finding.note_position,
                        owner_guid: guid_at(index, finding.note_position),
                        group_size: finding.group_size.unwrap_or(1),
                    });
            }
        }
    }

    let items: Vec<ReviewItem> = page
        .iter()
        .map(|position| {
            let empty: &[FindingSummary] = &[];
            let note_findings = by_position.get(position).map_or(empty, Vec::as_slice);
            build_item(
                index,
                *position,
                note_findings,
                &memberships,
                MAX_FINDINGS_PER_ITEM,
            )
        })
        .collect();

    let truncated = end < total_selected;
    Ok(ReviewResult {
        export_dir: export_dir.display().to_string(),
        selection: summarize(query),
        notes_total: index.notes.len(),
        total_selected,
        offset: query.offset,
        limit: query.limit,
        returned: items.len(),
        truncated,
        next_offset: truncated.then_some(end),
        items,
        excluded_unaddressable,
    })
}

/// `guid` заметки по позиции в порядке экспорта.
fn guid_at(index: &ExportIndex<'_>, position: usize) -> Option<String> {
    index
        .notes
        .get(position)
        .and_then(|entry| entry.note.guid.clone())
        .filter(|guid| !guid.is_empty())
}

/// Участники группы как адресуемые ссылки.
///
/// Список включает саму заметку замечания: это позиции всей группы, как их
/// вычислило правило, и потребитель не должен досчитывать владельца сам.
/// Неадресуемый участник остаётся в списке с `guid: None`: это контекст группы,
/// а не editable target.
fn related_notes(index: &ExportIndex<'_>, related: &[usize], owner: usize) -> Vec<RelatedNote> {
    related
        .iter()
        .filter(|position| **position != owner)
        .map(|position| RelatedNote {
            note_index: *position,
            guid: guid_at(index, *position),
        })
        .collect()
}

/// Собирает одну карточку batch'а с ограничением числа замечания.
fn build_item(
    index: &ExportIndex<'_>,
    position: usize,
    findings: &[FindingSummary],
    memberships: &BTreeMap<usize, Vec<GroupMembership>>,
    limit: usize,
) -> ReviewItem {
    ReviewItem {
        note_index: position,
        note: NoteSummary::build(index, &index.notes[position]),
        qa_findings: findings.iter().take(limit).cloned().collect(),
        qa_findings_truncated: findings.len() > limit,
        group_membership: memberships.get(&position).cloned().unwrap_or_default(),
    }
}

/// Выбирает позиции заметок в порядке экспорта.
fn select(
    index: &ExportIndex<'_>,
    findings: &[qa::Finding],
    query: &ReviewQuery,
    scope: Option<usize>,
) -> Result<Vec<usize>, DomainError> {
    match &query.criteria {
        ReviewCriteria::All => Ok((0..index.notes.len())
            .filter(|position| selection::in_scope(index, &index.notes[*position], scope))
            .collect()),

        ReviewCriteria::Guid { guid } => {
            let positions = selection::positions_by_guid(index, guid, scope);
            match positions.len() {
                0 => Err(DomainError::with_details(
                    ErrorCode::NotFound,
                    format!("заметка с guid {guid:?} не найдена"),
                    details! {
                        "guid" => guid,
                        "deck" => query.deck,
                    },
                )),
                1 => Ok(positions),
                count => Err(DomainError::with_details(
                    ErrorCode::Ambiguous,
                    format!(
                        "guid {guid:?} встречается в экспорте {count} раз; идентичность неоднозначна"
                    ),
                    details! {
                        "guid" => guid,
                        "matches" => count,
                    },
                )),
            }
        }

        ReviewCriteria::Field { field, value, mode } => {
            selection::ensure_known_field(index, field)?;
            Ok(selection::positions_matching_field(
                index, field, value, *mode, scope,
            ))
        }

        ReviewCriteria::QaCode { code } => {
            if !qa::is_known_code(code) {
                let available: Vec<&str> = qa::codes().collect();
                return Err(DomainError::with_details(
                    ErrorCode::UnknownQaCode,
                    format!("неизвестный код QA {code:?}"),
                    details! {
                        "code" => code,
                        "available_codes" => available,
                    },
                ));
            }
            // Групповое замечание отбирает всю группу: иначе агент не может
            // судить о дубликате, не читая экспорт целиком. Список участников
            // приходит из domain-результата и уже ограничен правилом.
            let matching: BTreeSet<usize> = findings
                .iter()
                .filter(|finding| finding.code == code)
                .flat_map(|finding| finding.related.iter().copied())
                .chain(
                    findings
                        .iter()
                        .filter(|finding| finding.code == code)
                        .map(|finding| finding.note_position),
                )
                .collect();
            Ok(matching
                .into_iter()
                .filter(|position| selection::in_scope(index, &index.notes[*position], scope))
                .collect())
        }
    }
}

/// Готовит machine-readable описание критерия.
fn summarize(query: &ReviewQuery) -> SelectionSummary {
    let deck = query.deck.clone();
    match &query.criteria {
        ReviewCriteria::All => SelectionSummary {
            kind: "all",
            guid: None,
            field: None,
            value: None,
            match_mode: None,
            qa_code: None,
            deck,
        },
        ReviewCriteria::Guid { guid } => SelectionSummary {
            kind: "guid",
            guid: Some(guid.clone()),
            field: None,
            value: None,
            match_mode: None,
            qa_code: None,
            deck,
        },
        ReviewCriteria::Field { field, value, mode } => SelectionSummary {
            kind: "field",
            guid: None,
            field: Some(field.clone()),
            value: Some(value.clone()),
            match_mode: Some(mode.as_str()),
            qa_code: None,
            deck,
        },
        ReviewCriteria::QaCode { code } => SelectionSummary {
            kind: "qa_code",
            guid: None,
            field: None,
            value: None,
            match_mode: None,
            qa_code: Some(code.clone()),
            deck,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, deck_node, export_with};

    fn query(criteria: ReviewCriteria) -> ReviewQuery {
        ReviewQuery {
            criteria,
            deck: None,
            offset: 0,
            limit: 50,
        }
    }

    fn run(json: &str, query: &ReviewQuery) -> ReviewResult {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        review(Path::new("."), &index, query).expect("review")
    }

    fn run_error(json: &str, query: &ReviewQuery) -> DomainError {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        review(Path::new("."), &index, query).expect_err("ожидалась ошибка")
    }

    #[test]
    fn all_notes_are_paginated_in_export_order() {
        let result = run(
            MINIMAL_EXPORT,
            &ReviewQuery {
                limit: 1,
                ..query(ReviewCriteria::All)
            },
        );
        assert_eq!(result.total_selected, 2);
        assert_eq!(result.returned, 1);
        assert!(result.truncated);
        assert_eq!(result.next_offset, Some(1));
        assert_eq!(result.items[0].note_index, 0);
        assert_eq!(result.items[0].note.guid.as_deref(), Some("guid-1"));

        let second = run(
            MINIMAL_EXPORT,
            &ReviewQuery {
                offset: 1,
                limit: 1,
                ..query(ReviewCriteria::All)
            },
        );
        assert_eq!(second.items[0].note_index, 1);
        assert!(!second.truncated);
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn offset_past_the_end_is_an_empty_page() {
        let result = run(
            MINIMAL_EXPORT,
            &ReviewQuery {
                offset: 99,
                limit: 5,
                ..query(ReviewCriteria::All)
            },
        );
        assert_eq!(result.returned, 0);
        assert!(result.items.is_empty());
        assert!(!result.truncated);
        assert_eq!(result.total_selected, 2);
    }

    #[test]
    fn items_carry_named_fields() {
        let result = run(MINIMAL_EXPORT, &query(ReviewCriteria::All));
        let second = &result.items[1];
        assert_eq!(
            second.note.note_model_name.as_deref(),
            Some("Тестовая модель")
        );
        let names: Vec<&str> = second
            .note
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, vec!["Заголовок", "Толкование"]);
        assert_eq!(
            second.note.fields[1].value.as_deref(),
            Some(""),
            "пустое значение остаётся видимым"
        );
        assert_eq!(second.qa_findings.len(), 1);
        assert_eq!(second.qa_findings[0].code, "empty_field_value");
    }

    #[test]
    fn item_findings_are_bounded_with_flag() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let findings: Vec<FindingSummary> = (0..3)
            .map(|index| FindingSummary {
                code: "empty_field_value",
                severity: QaSeverity::Warning,
                field: None,
                message: format!("finding {index}"),
                group_size: None,
                related: Vec::new(),
                related_truncated: false,
            })
            .collect();

        let memberships = BTreeMap::new();
        let bounded = build_item(&index, 0, &findings, &memberships, 2);
        assert_eq!(bounded.qa_findings.len(), 2);
        assert!(bounded.qa_findings_truncated);

        let complete = build_item(&index, 0, &findings, &memberships, 3);
        assert_eq!(complete.qa_findings.len(), 3);
        assert!(!complete.qa_findings_truncated);

        let none = build_item(&index, 0, &[], &memberships, MAX_FINDINGS_PER_ITEM);
        assert!(none.qa_findings.is_empty());
        assert!(!none.qa_findings_truncated);
        assert!(bounded.group_membership.is_empty());
    }

    #[test]
    fn qa_code_selection_returns_exactly_the_flagged_notes() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][0] = serde_json::json!(" 偶然");
        });
        let result = run(
            &json,
            &query(ReviewCriteria::QaCode {
                code: "leading_whitespace".to_string(),
            }),
        );
        assert_eq!(result.total_selected, 1);
        assert_eq!(result.items[0].note_index, 0);
        assert_eq!(
            result.items[0].qa_findings[0].code, "leading_whitespace",
            "карточка сообщает, почему попала в batch"
        );
    }

    #[test]
    fn unknown_qa_code_is_rejected() {
        let error = run_error(
            MINIMAL_EXPORT,
            &query(ReviewCriteria::QaCode {
                code: "нет_такого".to_string(),
            }),
        );
        assert_eq!(error.code, ErrorCode::UnknownQaCode);
    }

    #[test]
    fn guid_selection_keeps_find_semantics() {
        let node = deck_node(&export_with(MINIMAL_EXPORT, |value| {
            value["notes"][1]["guid"] = serde_json::json!("guid-1");
        }));
        let index = ExportIndex::build(&node);
        let ambiguous = review(
            Path::new("."),
            &index,
            &query(ReviewCriteria::Guid {
                guid: "guid-1".to_string(),
            }),
        )
        .expect_err("неоднозначный guid");
        assert_eq!(ambiguous.code, ErrorCode::Ambiguous);
        assert_eq!(ambiguous.exit_code(), 5);

        let missing = run_error(
            MINIMAL_EXPORT,
            &query(ReviewCriteria::Guid {
                guid: "нет-такого".to_string(),
            }),
        );
        assert_eq!(missing.code, ErrorCode::NotFound);
        assert_eq!(missing.exit_code(), 4);
    }

    #[test]
    fn field_and_deck_selection_reuse_find_semantics() {
        let result = run(
            MINIMAL_EXPORT,
            &ReviewQuery {
                criteria: ReviewCriteria::Field {
                    field: "Заголовок".to_string(),
                    value: "偶然".to_string(),
                    mode: MatchMode::Exact,
                },
                ..query(ReviewCriteria::All)
            },
        );
        assert_eq!(
            result.total_selected, 0,
            "точное совпадение не учитывает sound-тег"
        );
        assert_eq!(result.selection.kind, "field");
        assert_eq!(result.selection.match_mode, Some("exact"));

        let contains = run(
            MINIMAL_EXPORT,
            &ReviewQuery {
                criteria: ReviewCriteria::Field {
                    field: "Заголовок".to_string(),
                    value: "偶然".to_string(),
                    mode: MatchMode::Contains,
                },
                ..query(ReviewCriteria::All)
            },
        );
        assert_eq!(contains.total_selected, 1);

        let nested = run(
            NESTED_EXPORT,
            &ReviewQuery {
                criteria: ReviewCriteria::All,
                deck: Some("Root::Child".to_string()),
                offset: 0,
                limit: 50,
            },
        );
        assert_eq!(nested.total_selected, 2);
        assert_eq!(nested.selection.deck.as_deref(), Some("Root::Child"));

        let unknown_deck = run_error(
            NESTED_EXPORT,
            &ReviewQuery {
                criteria: ReviewCriteria::All,
                deck: Some("Child".to_string()),
                offset: 0,
                limit: 50,
            },
        );
        assert_eq!(unknown_deck.code, ErrorCode::UnknownDeck);
    }

    #[test]
    fn unknown_field_is_rejected_before_selection() {
        let error = run_error(
            MINIMAL_EXPORT,
            &query(ReviewCriteria::Field {
                field: "НетТакого".to_string(),
                value: "x".to_string(),
                mode: MatchMode::Contains,
            }),
        );
        assert_eq!(error.code, ErrorCode::UnknownField);
    }

    #[test]
    fn page_content_is_stable_between_runs() {
        let first = run(MINIMAL_EXPORT, &query(ReviewCriteria::All));
        let second = run(MINIMAL_EXPORT, &query(ReviewCriteria::All));
        assert_eq!(first.items.len(), second.items.len());
        for (left, right) in first.items.iter().zip(second.items.iter()) {
            assert_eq!(left.note_index, right.note_index);
            assert_eq!(left.note.guid, right.note.guid);
            assert_eq!(left.note.fields.len(), right.note.fields.len());
        }
    }
}
