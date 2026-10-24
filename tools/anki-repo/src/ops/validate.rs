//! Операция `validate`: детерминированная проверка структурной целостности.
//!
//! Validator собирает все issues за один run, а не останавливается на первой
//! обычной структурной проблеме. Серьёзность разделена на ERROR / WARNING /
//! INFO: отсутствие физического media-файла само по себе никогда не делает
//! экспорт невалидным.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::details;
use crate::error::DomainError;
use crate::index::ExportIndex;
use crate::loader::{DECK_JSON, DECK_TYPE, ensure_deck_root, read_deck_json};
use crate::media::{MEDIA_DIR, collect_media, extract_media_references, normalize_media_name};
use crate::model::{DeckNode, FieldValue, NoteModel, Ord};

/// Максимум issue одного кода одной серьёзности, попадающих в результат.
pub const MAX_ISSUES_PER_CODE: usize = 50;

/// Специальные токены Anki, не являющиеся именами полей.
pub const SPECIAL_TEMPLATE_TOKENS: [&str; 7] = [
    "FrontSide",
    "Tags",
    "Type",
    "Deck",
    "Subdeck",
    "Card",
    "CardFlag",
];

/// Стабильные коды ERROR.
pub mod error_codes {
    /// `deck.json` не является валидным JSON.
    pub const INVALID_JSON: &str = "invalid_json";
    /// Корень нельзя интерпретировать как CrowdAnki `Deck`.
    pub const ROOT_NOT_DECK: &str = "root_not_deck";
    /// Валидный JSON не соответствует типизированному ядру.
    pub const SCHEMA_INVALID: &str = "schema_invalid";
    /// Вложенный узел не является `Deck`.
    pub const NODE_NOT_DECK: &str = "node_not_deck";
    /// Дублирующаяся идентичность модели заметок в одном объявлении.
    pub const DUPLICATE_NOTE_MODEL_UUID: &str = "duplicate_note_model_uuid";
    /// Дублирующаяся идентичность конфигурации колоды в одном объявлении.
    pub const DUPLICATE_DECK_CONFIG_UUID: &str = "duplicate_deck_config_uuid";
    /// У модели заметок нет идентичности CrowdAnki.
    pub const NOTE_MODEL_IDENTITY_MISSING: &str = "note_model_identity_missing";
    /// У конфигурации колоды нет идентичности CrowdAnki.
    pub const DECK_CONFIG_IDENTITY_MISSING: &str = "deck_config_identity_missing";
    /// У заметки нет `note_model_uuid`.
    pub const NOTE_MODEL_UUID_MISSING: &str = "note_model_uuid_missing";
    /// `note_model_uuid` не разрешается.
    pub const NOTE_MODEL_UNRESOLVED: &str = "note_model_unresolved";
    /// `deck_config_uuid` не разрешается.
    pub const DECK_CONFIG_UNRESOLVED: &str = "deck_config_unresolved";
    /// Число значений `fields` не совпадает с числом полей модели.
    pub const NOTE_FIELDS_COUNT_MISMATCH: &str = "note_fields_count_mismatch";
    /// Значение поля заметки не является строкой.
    pub const NOTE_FIELD_VALUE_NOT_STRING: &str = "note_field_value_not_string";
    /// У заметки нет `guid`.
    pub const NOTE_GUID_MISSING: &str = "note_guid_missing";
    /// `guid` заметки повторяется.
    pub const DUPLICATE_NOTE_GUID: &str = "duplicate_note_guid";
    /// Имя поля повторяется внутри модели.
    pub const FIELD_NAME_DUPLICATE: &str = "field_name_duplicate";
    /// `flds[].ord` не является целым числом.
    pub const FIELD_ORD_INVALID: &str = "field_ord_invalid";
    /// `flds[].ord` отрицателен.
    pub const FIELD_ORD_NEGATIVE: &str = "field_ord_negative";
    /// Два поля имеют одинаковый `ord`.
    pub const FIELD_ORD_DUPLICATE: &str = "field_ord_duplicate";
    /// `flds[].ord` выходит за пределы числа полей.
    pub const FIELD_ORD_OUT_OF_RANGE: &str = "field_ord_out_of_range";
    /// В нумерации `flds[].ord` есть пропуски.
    pub const FIELD_ORD_GAP: &str = "field_ord_gap";
    /// Очевидная ссылка шаблона на несуществующее поле.
    pub const TEMPLATE_FIELD_UNRESOLVED: &str = "template_field_unresolved";
}

/// Стабильные коды WARNING.
pub mod warning_codes {
    /// Заметка ссылается на media, которого нет в `media_files`.
    pub const MEDIA_REFERENCE_UNDECLARED: &str = "media_reference_undeclared";
    /// Имя объявлено в `media_files` повторно.
    pub const MEDIA_FILES_DUPLICATE: &str = "media_files_duplicate";
    /// Объявленный media отсутствует физически.
    pub const MEDIA_PHYSICAL_MISSING: &str = "media_physical_missing";
    /// Каталог `media/` отсутствует при непустом `media_files`.
    pub const MEDIA_DIR_MISSING: &str = "media_dir_missing";
    /// Объявленное media-имя не является простым basename.
    pub const MEDIA_NAME_NOT_BASENAME: &str = "media_name_not_basename";
    /// Сложный template construct нельзя проверить ограниченным парсером.
    pub const TEMPLATE_CONSTRUCT_UNCHECKED: &str = "template_construct_unchecked";
    /// У узла колоды нет `deck_config_uuid`.
    pub const DECK_CONFIG_UUID_MISSING: &str = "deck_config_uuid_missing";
    /// У узла колоды нет имени.
    pub const DECK_NAME_MISSING: &str = "deck_name_missing";
    /// Одинаковая идентичность модели объявлена с разными определениями.
    pub const CONFLICTING_NOTE_MODEL_DEFINITION: &str = "conflicting_note_model_definition";
    /// Одинаковая идентичность конфигурации объявлена с разными определениями.
    pub const CONFLICTING_DECK_CONFIG_DEFINITION: &str = "conflicting_deck_config_definition";
    /// `tmpls[].ord` не является целым числом.
    pub const TEMPLATE_ORD_INVALID: &str = "template_ord_invalid";
    /// Два шаблона имеют одинаковый `ord`.
    pub const TEMPLATE_ORD_DUPLICATE: &str = "template_ord_duplicate";
}

/// Стабильные коды INFO.
pub mod info_codes {
    /// Сводка counts/models/configs.
    pub const EXPORT_SUMMARY: &str = "export_summary";
    /// Физический media-файл не объявлен ни в одном `media_files`.
    pub const MEDIA_PHYSICAL_UNUSED: &str = "media_physical_unused";
    /// Модель заметок не используется ни одной заметкой.
    pub const NOTE_MODEL_UNUSED: &str = "note_model_unused";
    /// Конфигурация колоды не используется ни одним узлом.
    pub const DECK_CONFIG_UNUSED: &str = "deck_config_unused";
    /// Пустые значения по полям.
    pub const EMPTY_FIELD_VALUES: &str = "empty_field_values";
    /// Слишком много однотипных issues: остальные опущены.
    pub const ISSUES_TRUNCATED: &str = "issues_truncated";
}

/// Серьёзность issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Структурная проблема: экспорт нельзя считать валидным.
    Error,
    /// Подозрительное, но допустимое состояние.
    Warning,
    /// Диагностика, не влияющая на validity.
    Info,
}

impl Severity {
    /// Стабильное machine-readable имя серьёзности.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// Одна найденная проблема.
#[derive(Debug)]
pub struct Issue {
    /// Серьёзность.
    pub severity: Severity,
    /// Стабильный код.
    pub code: &'static str,
    /// Путь колоды, если проблема привязана к узлу.
    pub deck_path: Option<String>,
    /// Структурная координата внутри узла или модели.
    pub location: String,
    /// Человекочитаемое описание.
    pub message: String,
    /// Дополнительные machine-readable детали.
    pub details: Value,
}

/// Счётчики по серьёзностям.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeverityCounts {
    /// Сколько ERROR.
    pub errors: usize,
    /// Сколько WARNING.
    pub warnings: usize,
    /// Сколько INFO.
    pub info: usize,
}

/// Результат `validate`.
#[derive(Debug)]
pub struct ValidateResult {
    /// True, если ERROR нет.
    pub valid: bool,
    /// Счётчики по серьёзностям.
    pub summary: SeverityCounts,
    /// Отсортированные issues.
    pub issues: Vec<Issue>,
}

impl ValidateResult {
    /// Число ERROR.
    pub const fn errors(&self) -> usize {
        self.summary.errors
    }
}

/// Выполняет `validate`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InputUnreadable`] или [`ErrorCode::DeckJsonMissing`],
/// если команда не может быть выполнена вообще: нечитаемый каталог или
/// отсутствующий `deck.json`. Проблемы содержимого становятся issues
/// результата, а не ошибкой.
pub fn validate(export_dir: &Path) -> Result<ValidateResult, DomainError> {
    let (deck_json, raw) = read_deck_json(export_dir)?;

    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return Ok(finish(vec![Issue {
                severity: Severity::Error,
                code: error_codes::INVALID_JSON,
                deck_path: None,
                location: DECK_JSON.to_string(),
                message: format!("{DECK_JSON} не является валидным JSON: {error}"),
                details: details! {
                    "message" => error.to_string(),
                    "line" => error.line(),
                    "column" => error.column(),
                },
            }]));
        }
    };

    if let Err(root_error) = ensure_deck_root(&value, &deck_json) {
        return Ok(finish(vec![Issue {
            severity: Severity::Error,
            code: error_codes::ROOT_NOT_DECK,
            deck_path: None,
            location: DECK_JSON.to_string(),
            message: root_error.message,
            details: root_error.details,
        }]));
    }

    let root: DeckNode = match serde_json::from_value(value) {
        Ok(root) => root,
        Err(error) => {
            return Ok(finish(vec![Issue {
                severity: Severity::Error,
                code: error_codes::SCHEMA_INVALID,
                deck_path: None,
                location: DECK_JSON.to_string(),
                message: format!(
                    "валидный JSON не соответствует типизированному ядру CrowdAnki: {error}"
                ),
                details: details! {
                    "message" => error.to_string(),
                },
            }]));
        }
    };

    Ok(validate_document(&root, export_dir))
}

/// Выполняет проверки по уже разобранному документу.
///
/// Отделено от [`validate`] ради мутирующего пути: `edit` проверяет не только
/// исходный экспорт, но и кандидат, а читать `deck.json` с диска для этого не
/// нужно. Физический каталог `media/` при этом берётся реальный: значения
/// полей на набор media-имён не влияют, а media-проверки не дают ERROR.
pub fn validate_document(root: &DeckNode, export_dir: &Path) -> ValidateResult {
    let index = ExportIndex::build(root);
    let mut issues = structural_issues(&index);
    check_media(export_dir, &index, &mut issues);
    check_summary_diagnostics(&index, &mut issues);
    finish(issues)
}

/// Проверяет структуру уже разобранного CrowdAnki без обращения к файловой системе.
/// Используется также доверенным программным обменом для снимков в памяти.
pub fn validate_structure(root: &DeckNode) -> ValidateResult {
    let index = ExportIndex::build(root);
    let mut issues = structural_issues(&index);
    check_summary_diagnostics(&index, &mut issues);
    finish(issues)
}

fn structural_issues(index: &ExportIndex<'_>) -> Vec<Issue> {
    let mut issues: Vec<Issue> = Vec::new();

    check_deck_nodes(index, &mut issues);
    check_models(index, &mut issues);
    check_configs(index, &mut issues);
    check_note_guids(index, &mut issues);
    check_templates(index, &mut issues);
    issues
}

fn push(
    issues: &mut Vec<Issue>,
    severity: Severity,
    code: &'static str,
    deck_path: Option<&str>,
    location: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) {
    issues.push(Issue {
        severity,
        code,
        deck_path: deck_path.map(ToString::to_string),
        location: location.into(),
        message: message.into(),
        details,
    });
}

fn check_deck_nodes(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    for entry in &index.nodes {
        let node = entry.node;
        let path = entry.path;

        if node.type_name.as_deref() != Some(DECK_TYPE) {
            push(
                issues,
                Severity::Error,
                error_codes::NODE_NOT_DECK,
                Some(path),
                "__type__",
                format!(
                    "узел колоды имеет __type__ = {:?}, ожидался {DECK_TYPE:?}",
                    node.type_name
                ),
                details! {
                    "observed" => node.type_name,
                },
            );
        }

        if node.name.is_empty() {
            push(
                issues,
                Severity::Warning,
                warning_codes::DECK_NAME_MISSING,
                Some(path),
                "name",
                "у узла колоды пустое имя",
                details! {},
            );
        }

        check_deck_node_models(node, path, issues);
        check_deck_node_configs(node, path, issues);
        check_deck_node_notes(node, path, index, issues);
        check_deck_node_media(node, path, issues);

        match node.deck_config_uuid.as_deref() {
            None => push(
                issues,
                Severity::Warning,
                warning_codes::DECK_CONFIG_UUID_MISSING,
                Some(path),
                "deck_config_uuid",
                "у узла колоды нет deck_config_uuid",
                details! {},
            ),
            Some(uuid) if index.config_by_uuid(uuid).is_none() => push(
                issues,
                Severity::Error,
                error_codes::DECK_CONFIG_UNRESOLVED,
                Some(path),
                "deck_config_uuid",
                format!("deck_config_uuid {uuid:?} не разрешается в deck_configurations"),
                details! {
                    "deck_config_uuid" => uuid,
                },
            ),
            Some(_) => {}
        }
    }
}

fn check_deck_node_models(node: &DeckNode, path: &str, issues: &mut Vec<Issue>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, model) in node.note_models.iter().enumerate() {
        let location = format!("note_models[{position}]");
        match model.crowdanki_uuid.as_deref() {
            None => push(
                issues,
                Severity::Error,
                error_codes::NOTE_MODEL_IDENTITY_MISSING,
                Some(path),
                location,
                "модель заметок не имеет crowdanki_uuid",
                details! {
                    "model_name" => model.name,
                },
            ),
            Some(uuid) => {
                if let Some(first) = seen.insert(uuid, position) {
                    push(
                        issues,
                        Severity::Error,
                        error_codes::DUPLICATE_NOTE_MODEL_UUID,
                        Some(path),
                        location,
                        format!(
                            "crowdanki_uuid {uuid:?} уже объявлен в этой же колоде на позиции {first}"
                        ),
                        details! {
                            "crowdanki_uuid" => uuid,
                            "first_position" => first,
                        },
                    );
                }
            }
        }
    }
}

fn check_deck_node_configs(node: &DeckNode, path: &str, issues: &mut Vec<Issue>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, config) in node.deck_configurations.iter().enumerate() {
        let location = format!("deck_configurations[{position}]");
        match config.crowdanki_uuid.as_deref() {
            None => push(
                issues,
                Severity::Error,
                error_codes::DECK_CONFIG_IDENTITY_MISSING,
                Some(path),
                location,
                "конфигурация колоды не имеет crowdanki_uuid",
                details! {
                    "config_name" => config.name,
                },
            ),
            Some(uuid) => {
                if let Some(first) = seen.insert(uuid, position) {
                    push(
                        issues,
                        Severity::Error,
                        error_codes::DUPLICATE_DECK_CONFIG_UUID,
                        Some(path),
                        location,
                        format!(
                            "crowdanki_uuid {uuid:?} уже объявлен в этой же колоде на позиции {first}"
                        ),
                        details! {
                            "crowdanki_uuid" => uuid,
                            "first_position" => first,
                        },
                    );
                }
            }
        }
    }
}

fn check_deck_node_notes(
    node: &DeckNode,
    path: &str,
    index: &ExportIndex<'_>,
    issues: &mut Vec<Issue>,
) {
    for (position, note) in node.notes.iter().enumerate() {
        let location = format!("notes[{position}]");

        match note.guid.as_deref() {
            None | Some("") => push(
                issues,
                Severity::Error,
                error_codes::NOTE_GUID_MISSING,
                Some(path),
                location.clone(),
                "у заметки нет guid",
                details! {},
            ),
            // Повторяющиеся guid собирает check_note_guids по всему экспорту;
            // отдельная проверка внутри узла давала бы вторую issue на тот же
            // дефект.
            Some(_) => {}
        }

        for (field_position, value) in note.fields.iter().enumerate() {
            if matches!(value, FieldValue::Other(_)) {
                push(
                    issues,
                    Severity::Error,
                    error_codes::NOTE_FIELD_VALUE_NOT_STRING,
                    Some(path),
                    format!("{location}.fields[{field_position}]"),
                    "значение поля заметки не является строкой",
                    details! {
                        "position" => field_position,
                    },
                );
            }
        }

        let Some(uuid) = note.note_model_uuid.as_deref() else {
            push(
                issues,
                Severity::Error,
                error_codes::NOTE_MODEL_UUID_MISSING,
                Some(path),
                location,
                "у заметки нет note_model_uuid",
                details! {},
            );
            continue;
        };

        let Some(model) = index.model_by_uuid(uuid) else {
            push(
                issues,
                Severity::Error,
                error_codes::NOTE_MODEL_UNRESOLVED,
                Some(path),
                location,
                format!("note_model_uuid {uuid:?} не разрешается в note_models"),
                details! {
                    "note_model_uuid" => uuid,
                },
            );
            continue;
        };

        if note.fields.len() != model.flds.len() {
            push(
                issues,
                Severity::Error,
                error_codes::NOTE_FIELDS_COUNT_MISMATCH,
                Some(path),
                location,
                format!(
                    "у заметки {actual} значений fields, а модель {model:?} объявляет {expected} полей",
                    actual = note.fields.len(),
                    model = model.name,
                    expected = model.flds.len(),
                ),
                details! {
                    "fields" => note.fields.len(),
                    "model_fields" => model.flds.len(),
                    "note_model_uuid" => uuid,
                },
            );
        }
    }
}

fn check_deck_node_media(node: &DeckNode, path: &str, issues: &mut Vec<Issue>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, name) in node.media_files.iter().enumerate() {
        let location = format!("media_files[{position}]");
        if let Some(first) = seen.insert(name, position) {
            push(
                issues,
                Severity::Warning,
                warning_codes::MEDIA_FILES_DUPLICATE,
                Some(path),
                location.clone(),
                format!("имя media {name:?} уже объявлено в этом узле на позиции {first}"),
                details! {
                    "name" => name,
                    "first_position" => first,
                },
            );
        }
        if normalize_media_name(name) != *name {
            push(
                issues,
                Severity::Warning,
                warning_codes::MEDIA_NAME_NOT_BASENAME,
                Some(path),
                location,
                format!("объявленное имя media {name:?} не является простым basename"),
                details! {
                    "name" => name,
                    "basename" => normalize_media_name(name),
                },
            );
        }
    }
}

fn check_models(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    let mut declarations: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for entry in &index.nodes {
        for model in &entry.node.note_models {
            if let Some(uuid) = model.crowdanki_uuid.as_deref() {
                declarations
                    .entry(uuid)
                    .or_default()
                    .insert(model_fingerprint(model));
            }
        }
    }

    for (uuid, fingerprints) in &declarations {
        if fingerprints.len() > 1 {
            push(
                issues,
                Severity::Warning,
                warning_codes::CONFLICTING_NOTE_MODEL_DEFINITION,
                None,
                format!("note_model[{uuid}]"),
                format!(
                    "идентичность {uuid:?} объявлена с {count} различающимися определениями",
                    count = fingerprints.len()
                ),
                details! {
                    "crowdanki_uuid" => uuid,
                    "variants" => fingerprints.len(),
                },
            );
        }
    }

    for &model in &index.models {
        let Some(uuid) = model.crowdanki_uuid.as_deref() else {
            continue;
        };
        let base = format!("note_models[{uuid}]");
        check_model_fields(model, &base, issues);
    }
}

fn model_fingerprint(model: &NoteModel) -> String {
    let fields: Vec<String> = model
        .flds
        .iter()
        .map(|field| format!("{}:{}", field.name, field.ord))
        .collect();
    format!(
        "{}|{}",
        model.name.clone().unwrap_or_default(),
        fields.join(",")
    )
}

fn check_model_fields(model: &NoteModel, base: &str, issues: &mut Vec<Issue>) {
    let field_count = model.flds.len();

    let mut names: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (position, field) in model.flds.iter().enumerate() {
        names.entry(field.name.as_str()).or_default().push(position);
    }
    for (name, positions) in &names {
        if positions.len() > 1 && !name.is_empty() {
            push(
                issues,
                Severity::Error,
                error_codes::FIELD_NAME_DUPLICATE,
                None,
                format!("{base}.flds"),
                format!(
                    "имя поля {name:?} повторяется в модели {count} раз",
                    count = positions.len()
                ),
                details! {
                    "field" => name,
                    "positions" => positions,
                },
            );
        }
    }

    let mut ords: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    let mut valid_ords: BTreeSet<i64> = BTreeSet::new();

    for (position, field) in model.flds.iter().enumerate() {
        let location = format!("{base}.flds[{position}].ord");

        match field.ord {
            Ord::Invalid => {
                push(
                    issues,
                    Severity::Error,
                    error_codes::FIELD_ORD_INVALID,
                    None,
                    location,
                    format!("у поля {:?} нет корректного целочисленного ord", field.name),
                    details! {
                        "field" => field.name,
                        "position" => position,
                    },
                );
                continue;
            }
            Ord::Int(value) => {
                if value < 0 {
                    push(
                        issues,
                        Severity::Error,
                        error_codes::FIELD_ORD_NEGATIVE,
                        None,
                        location.clone(),
                        format!("у поля {:?} отрицательный ord {value}", field.name),
                        details! {
                            "field" => field.name,
                            "ord" => value,
                        },
                    );
                    continue;
                }
                if usize::try_from(value).is_ok_and(|value| value >= field_count) {
                    push(
                        issues,
                        Severity::Error,
                        error_codes::FIELD_ORD_OUT_OF_RANGE,
                        None,
                        location.clone(),
                        format!(
                            "ord {value} поля {name:?} выходит за пределы {field_count} полей модели",
                            name = field.name
                        ),
                        details! {
                            "field" => field.name,
                            "ord" => value,
                            "model_fields" => field_count,
                        },
                    );
                }
                if let Some(existing) = ords.get_mut(&value) {
                    existing.push(field.name.as_str());
                } else {
                    valid_ords.insert(value);
                    ords.insert(value, vec![field.name.as_str()]);
                }
            }
        }
    }

    for (value, fields) in &ords {
        if fields.len() > 1 {
            push(
                issues,
                Severity::Error,
                error_codes::FIELD_ORD_DUPLICATE,
                None,
                format!("{base}.flds"),
                format!(
                    "ord {value} используют сразу {count} поля: {fields:?}",
                    count = fields.len()
                ),
                details! {
                    "ord" => value,
                    "fields" => fields,
                },
            );
        }
    }

    let missing: Vec<i64> = (0..i64::try_from(field_count).unwrap_or(i64::MAX))
        .filter(|value| !valid_ords.contains(value))
        .collect();
    if !missing.is_empty() {
        push(
            issues,
            Severity::Error,
            error_codes::FIELD_ORD_GAP,
            None,
            format!("{base}.flds"),
            format!("в нумерации ord есть пропуски: {missing:?}"),
            details! {
                "missing" => missing,
                "model_fields" => field_count,
            },
        );
    }
}

fn check_configs(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    let mut declarations: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for entry in &index.nodes {
        for config in &entry.node.deck_configurations {
            if let Some(uuid) = config.crowdanki_uuid.as_deref() {
                declarations
                    .entry(uuid)
                    .or_default()
                    .insert(config.name.clone().unwrap_or_default());
            }
        }
    }

    for (uuid, names) in &declarations {
        if names.len() > 1 {
            push(
                issues,
                Severity::Warning,
                warning_codes::CONFLICTING_DECK_CONFIG_DEFINITION,
                None,
                format!("deck_config[{uuid}]"),
                format!(
                    "идентичность {uuid:?} объявлена с {count} различающимися именами",
                    count = names.len()
                ),
                details! {
                    "crowdanki_uuid" => uuid,
                    "variants" => names.len(),
                },
            );
        }
    }
}

fn check_note_guids(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    for (guid, positions) in &index.guids {
        if positions.len() < 2 {
            continue;
        }
        let locations: Vec<String> = positions
            .iter()
            .map(|position| {
                let entry = &index.notes[*position];
                format!("{}:notes[{}]", index.note_deck_path(entry), entry.order)
            })
            .collect();
        push(
            issues,
            Severity::Error,
            error_codes::DUPLICATE_NOTE_GUID,
            None,
            "notes".to_string(),
            format!(
                "guid {guid:?} встречается {count} раз в экспорте",
                count = positions.len()
            ),
            details! {
                "guid" => guid,
                "count" => positions.len(),
                "locations" => locations,
            },
        );
    }
}

fn check_templates(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    // Набор полей берётся у конкретной модели: объединение по всем моделям
    // скрывало бы ссылку шаблона на поле, которого у его собственной модели нет.
    for &model in &index.models {
        let Some(uuid) = model.crowdanki_uuid.as_deref() else {
            continue;
        };
        let base = format!("note_models[{uuid}]");
        let field_names: BTreeSet<&str> =
            model.flds.iter().map(|field| field.name.as_str()).collect();

        let mut ords: BTreeMap<i64, usize> = BTreeMap::new();
        for (position, template) in model.tmpls.iter().enumerate() {
            if let Some(value) = template.ord.value() {
                *ords.entry(value).or_insert(0) += 1;
            } else {
                push(
                    issues,
                    Severity::Warning,
                    warning_codes::TEMPLATE_ORD_INVALID,
                    None,
                    format!("{base}.tmpls[{position}].ord"),
                    format!(
                        "у шаблона {:?} нет корректного целочисленного ord",
                        template.name
                    ),
                    details! {
                        "template" => template.name,
                        "position" => position,
                    },
                );
            }
        }
        for (value, count) in &ords {
            if *count > 1 {
                push(
                    issues,
                    Severity::Warning,
                    warning_codes::TEMPLATE_ORD_DUPLICATE,
                    None,
                    format!("{base}.tmpls"),
                    format!("ord {value} используют {count} шаблона"),
                    details! {
                        "ord" => value,
                        "count" => count,
                    },
                );
            }
        }

        for (position, template) in model.tmpls.iter().enumerate() {
            for (side, text) in [("qfmt", &template.qfmt), ("afmt", &template.afmt)] {
                for construct in scan_template(text) {
                    // Незакрытый `{{` — это сломанный текст шаблона, а не
                    // доказанная ссылка на отсутствующее поле.
                    let classification = if is_closed(&construct) {
                        classify_template_token(&construct.token, &field_names)
                    } else {
                        TemplateToken::Unchecked
                    };
                    match classification {
                        TemplateToken::Known => {}
                        TemplateToken::Unresolved => push(
                            issues,
                            Severity::Error,
                            error_codes::TEMPLATE_FIELD_UNRESOLVED,
                            None,
                            format!("{base}.tmpls[{position}].{side}"),
                            format!(
                                "шаблон {:?} ссылается на несуществующее поле {name:?}",
                                template.name,
                                name = construct.token
                            ),
                            details! {
                                "template" => template.name,
                                "construct" => construct.raw,
                                "field" => construct.token,
                            },
                        ),
                        TemplateToken::Unchecked => push(
                            issues,
                            Severity::Warning,
                            warning_codes::TEMPLATE_CONSTRUCT_UNCHECKED,
                            None,
                            format!("{base}.tmpls[{position}].{side}"),
                            format!(
                                "template construct {:?} нельзя проверить ограниченным parser'ом",
                                construct.raw
                            ),
                            details! {
                                "template" => template.name,
                                "construct" => construct.raw,
                            },
                        ),
                    }
                }
            }
        }
    }
}

fn check_media(export_dir: &Path, index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    let report = collect_media(export_dir, index);

    if report.declared_total > 0 && !report.dir_present {
        push(
            issues,
            Severity::Warning,
            warning_codes::MEDIA_DIR_MISSING,
            None,
            MEDIA_DIR.to_string(),
            format!(
                "каталог {MEDIA_DIR}/ отсутствует, хотя объявлено {count} media-имён",
                count = report.declared_total
            ),
            details! {
                "declared" => report.declared_total,
            },
        );
    }

    for name in report.missing_physical() {
        push(
            issues,
            Severity::Warning,
            warning_codes::MEDIA_PHYSICAL_MISSING,
            None,
            format!("{MEDIA_DIR}/{name}"),
            format!("объявленный media {name:?} отсутствует физически в {MEDIA_DIR}/"),
            details! {
                "name" => name,
            },
        );
    }

    for name in report.undeclared_physical() {
        push(
            issues,
            Severity::Info,
            info_codes::MEDIA_PHYSICAL_UNUSED,
            None,
            format!("{MEDIA_DIR}/{name}"),
            format!("физический файл {name:?} не объявлен ни в одном media_files"),
            details! {
                "name" => name,
            },
        );
    }

    let mut undeclared: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
    for entry in &index.notes {
        let deck_path = index.note_deck_path(entry).to_string();
        for (position, value) in entry.note.fields.iter().enumerate() {
            let Some(text) = value.as_text() else {
                continue;
            };
            for reference in extract_media_references(text) {
                let basename = normalize_media_name(&reference);
                if report.declared.contains(&basename) {
                    continue;
                }
                let location = format!("notes[{}].fields[{position}]", entry.order);
                undeclared
                    .entry(basename)
                    .and_modify(|slot| slot.2 += 1)
                    .or_insert((deck_path.clone(), location.clone(), 1));
            }
        }
    }

    for (name, (deck_path, location, count)) in undeclared {
        push(
            issues,
            Severity::Warning,
            warning_codes::MEDIA_REFERENCE_UNDECLARED,
            Some(deck_path.as_str()),
            location,
            format!("заметка ссылается на media {name:?}, которого нет в media_files"),
            details! {
                "name" => name,
                "references" => count,
            },
        );
    }
}

fn check_summary_diagnostics(index: &ExportIndex<'_>, issues: &mut Vec<Issue>) {
    push(
        issues,
        Severity::Info,
        info_codes::EXPORT_SUMMARY,
        None,
        "export".to_string(),
        "сводка экспорта",
        details! {
            "deck_nodes" => index.nodes.len(),
            "notes" => index.notes.len(),
            "note_models" => index.models.len(),
            "deck_configurations" => index.configs.len(),
            "unique_guids" => index.guids.len(),
        },
    );

    for &model in &index.models {
        let Some(uuid) = model.crowdanki_uuid.as_deref() else {
            continue;
        };
        let used = index
            .notes
            .iter()
            .any(|entry| entry.note.note_model_uuid.as_deref() == Some(uuid));
        if !used {
            push(
                issues,
                Severity::Info,
                info_codes::NOTE_MODEL_UNUSED,
                None,
                format!("note_models[{uuid}]"),
                format!("модель {:?} не используется ни одной заметкой", model.name),
                details! {
                    "crowdanki_uuid" => uuid,
                },
            );
        }
    }

    for &config in &index.configs {
        let Some(uuid) = config.crowdanki_uuid.as_deref() else {
            continue;
        };
        let used = index
            .nodes
            .iter()
            .any(|entry| entry.node.deck_config_uuid.as_deref() == Some(uuid));
        if !used {
            push(
                issues,
                Severity::Info,
                info_codes::DECK_CONFIG_UNUSED,
                None,
                format!("deck_configurations[{uuid}]"),
                format!(
                    "конфигурация {:?} не используется ни одним узлом",
                    config.name
                ),
                details! {
                    "crowdanki_uuid" => uuid,
                },
            );
        }
    }

    let mut empty_fields: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &index.notes {
        let Some(model) = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid))
        else {
            continue;
        };
        for field in &model.flds {
            let value = field
                .ord
                .value()
                .and_then(|ord| usize::try_from(ord).ok())
                .and_then(|position| entry.note.fields.get(position));
            if value.is_none_or(FieldValue::is_empty) {
                *empty_fields.entry(field.name.clone()).or_insert(0) += 1;
            }
        }
    }
    if !empty_fields.is_empty() {
        push(
            issues,
            Severity::Info,
            info_codes::EMPTY_FIELD_VALUES,
            None,
            "notes".to_string(),
            "пустые значения по полям заметок",
            details! {
                "empty_by_field" => empty_fields,
            },
        );
    }
}

/// Найденный в шаблоне `{{ ... }}` construct.
#[derive(Debug, Clone)]
pub struct TemplateConstruct {
    /// Исходный текст construct'а вместе со скобками.
    ///
    /// Для закрытой конструкции это `{{` + содержимое + `}}`; для
    /// незакрытого хвоста шаблона — остаток текста, в котором закрывающих
    /// `}}` уже нет.
    pub raw: String,
    /// Содержимое без скобок и ведущих сигналов.
    pub token: String,
}

/// Проверяет, что construct шаблона был закрыт.
///
/// Незакрытый `{{` не даёт оснований утверждать, что шаблон ссылается именно
/// на поле: такой случай остаётся `template_construct_unchecked`.
fn is_closed(construct: &TemplateConstruct) -> bool {
    construct.raw.ends_with("}}")
}

/// Находит очевидные `{{ ... }}` constructs. Полноценный parser не используется.
pub fn scan_template(text: &str) -> Vec<TemplateConstruct> {
    let mut found = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            found.push(TemplateConstruct {
                raw: rest.to_string(),
                token: after.trim().to_string(),
            });
            return found;
        };
        let inner = &after[..end];
        let raw = format!("{{{{{inner}}}}}");
        found.push(TemplateConstruct {
            raw,
            token: strip_signals(inner).to_string(),
        });
        rest = &after[end + 2..];
    }

    found
}

fn strip_signals(token: &str) -> &str {
    token.trim_start_matches(['#', '/', '^', '!']).trim()
}

/// Итог классификации template construct'а.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateToken {
    /// Очевидная ссылка на существующее поле или специальный токен.
    Known,
    /// Очевидная ссылка на несуществующее поле.
    Unresolved,
    /// Сложный construct, который нельзя проверить консервативно.
    Unchecked,
}

/// Классифицирует construct шаблона консервативно.
pub fn classify_template_token(token: &str, field_names: &BTreeSet<&str>) -> TemplateToken {
    let token = token.trim();
    if token.is_empty() {
        return TemplateToken::Known;
    }
    if is_known_name(token, field_names) {
        return TemplateToken::Known;
    }
    if token.contains(':') {
        return if token
            .split(':')
            .map(str::trim)
            .any(|segment| is_known_name(segment, field_names))
        {
            TemplateToken::Known
        } else {
            TemplateToken::Unchecked
        };
    }
    TemplateToken::Unresolved
}

fn is_known_name(name: &str, field_names: &BTreeSet<&str>) -> bool {
    field_names.contains(name) || SPECIAL_TEMPLATE_TOKENS.contains(&name)
}

fn finish(issues: Vec<Issue>) -> ValidateResult {
    let mut issues = apply_caps(issues, MAX_ISSUES_PER_CODE);
    issues.sort_by(|left, right| {
        left.severity
            .cmp(&right.severity)
            .then_with(|| left.code.cmp(right.code))
            .then_with(|| left.deck_path.cmp(&right.deck_path))
            .then_with(|| left.location.cmp(&right.location))
            .then_with(|| left.message.cmp(&right.message))
    });

    let summary = SeverityCounts {
        errors: count_severity(&issues, Severity::Error),
        warnings: count_severity(&issues, Severity::Warning),
        info: count_severity(&issues, Severity::Info),
    };

    ValidateResult {
        valid: summary.errors == 0,
        summary,
        issues,
    }
}

fn count_severity(issues: &[Issue], severity: Severity) -> usize {
    issues
        .iter()
        .filter(|issue| issue.severity == severity)
        .count()
}

fn apply_caps(issues: Vec<Issue>, max: usize) -> Vec<Issue> {
    let mut kept_per_code: BTreeMap<(Severity, &'static str), usize> = BTreeMap::new();
    let mut dropped_per_code: BTreeMap<(Severity, &'static str), usize> = BTreeMap::new();
    let mut kept = Vec::new();

    for issue in issues {
        let key = (issue.severity, issue.code);
        let counter = kept_per_code.entry(key).or_insert(0);
        if *counter < max {
            *counter += 1;
            kept.push(issue);
        } else {
            *dropped_per_code.entry(key).or_insert(0) += 1;
        }
    }

    for ((severity, code), dropped) in dropped_per_code {
        kept.push(Issue {
            severity: Severity::Info,
            code: info_codes::ISSUES_TRUNCATED,
            deck_path: None,
            location: String::new(),
            message: format!(
                "issues с кодом {code} серьёзности {} опущены: {dropped}",
                severity.as_str()
            ),
            details: details! {
                "severity" => severity.as_str(),
                "code" => code,
                "dropped" => dropped,
            },
        });
    }

    kept
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn fields(names: &[&str]) -> BTreeSet<&'static str> {
        names
            .iter()
            .map(|name| Box::leak(name.to_string().into_boxed_str()) as &'static str)
            .collect()
    }

    #[test]
    fn scans_simple_and_braced_constructs() {
        let found = scan_template("{{Заголовок}}{{#Толкование}}А{{/Толкование}}");
        let tokens: Vec<&str> = found.iter().map(|item| item.token.as_str()).collect();
        assert_eq!(tokens, vec!["Заголовок", "Толкование", "Толкование"]);
        assert_eq!(found[0].raw, "{{Заголовок}}");
    }

    #[test]
    fn reports_unterminated_construct_once() {
        let found = scan_template("{{Заголовок");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].token, "Заголовок");
    }

    #[test]
    fn classifies_known_names_and_special_tokens() {
        let known = fields(&["Заголовок", "Толкование"]);
        assert_eq!(
            classify_template_token("Заголовок", &known),
            TemplateToken::Known
        );
        assert_eq!(
            classify_template_token("FrontSide", &known),
            TemplateToken::Known
        );
        assert_eq!(
            classify_template_token("Tags", &known),
            TemplateToken::Known
        );
        assert_eq!(classify_template_token("", &known), TemplateToken::Known);
    }

    #[test]
    fn classifies_filters_conservatively() {
        let known = fields(&["Заголовок"]);
        assert_eq!(
            classify_template_token("tts ja_JP:Заголовок", &known),
            TemplateToken::Known
        );
        assert_eq!(
            classify_template_token("tts ja_JP:Другое", &known),
            TemplateToken::Unchecked
        );
    }

    #[test]
    fn classifies_unknown_simple_reference_as_unresolved() {
        let known = fields(&["Заголовок"]);
        assert_eq!(
            classify_template_token("Пропавшее", &known),
            TemplateToken::Unresolved
        );
    }

    #[test]
    fn severity_has_stable_names_and_order() {
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(Severity::Warning.as_str(), "warning");
        assert_eq!(Severity::Info.as_str(), "info");
        assert!(Severity::Error < Severity::Warning);
        assert!(Severity::Warning < Severity::Info);
    }

    #[test]
    fn caps_drop_excess_issues_per_code_and_report_it() {
        let mut issues = Vec::new();
        for index in 0..(MAX_ISSUES_PER_CODE + 2) {
            push(
                &mut issues,
                Severity::Warning,
                warning_codes::MEDIA_PHYSICAL_MISSING,
                None,
                format!("media/{index}.mp3"),
                "нет файла",
                serde_json::Value::Null,
            );
        }
        let finished = finish(issues);
        let kept = finished
            .issues
            .iter()
            .filter(|issue| issue.code == warning_codes::MEDIA_PHYSICAL_MISSING)
            .count();
        assert_eq!(kept, MAX_ISSUES_PER_CODE);

        let truncated = finished
            .issues
            .iter()
            .find(|issue| issue.code == info_codes::ISSUES_TRUNCATED)
            .expect("должна быть отметка об усечении");
        assert_eq!(truncated.details["dropped"], serde_json::json!(2));
        assert!(finished.valid);
    }

    #[test]
    fn issues_are_sorted_by_severity_then_code() {
        let mut issues = Vec::new();
        push(
            &mut issues,
            Severity::Info,
            info_codes::EXPORT_SUMMARY,
            None,
            "export",
            "сводка",
            serde_json::Value::Null,
        );
        push(
            &mut issues,
            Severity::Error,
            error_codes::NOTE_GUID_MISSING,
            Some("D"),
            "notes[0]",
            "нет guid",
            serde_json::Value::Null,
        );
        push(
            &mut issues,
            Severity::Warning,
            warning_codes::MEDIA_DIR_MISSING,
            None,
            "media",
            "нет каталога",
            serde_json::Value::Null,
        );
        let finished = finish(issues);
        let order: Vec<&str> = finished
            .issues
            .iter()
            .map(|issue| issue.severity.as_str())
            .collect();
        assert_eq!(order, vec!["error", "warning", "info"]);
        assert_eq!(finished.summary.errors, 1);
        assert_eq!(finished.summary.warnings, 1);
        assert_eq!(finished.summary.info, 1);
        assert!(!finished.valid);
    }
}
