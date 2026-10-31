//! Доменные ошибки и их отображение на process exit codes.
//!
//! Машиночитаемый контракт — только значения [`ErrorCode`] в snake_case.
//! Rust-имена типов наружу не выходят.

use std::fmt;

use serde_json::{Map, Value};

/// Стабильные машиночитаемые коды доменных ошибок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorCode {
    /// Некорректная комбинация аргументов, обнаруженная вне clap.
    Usage,
    /// Не удалось прочитать каталог экспорта.
    InputUnreadable,
    /// В каталоге экспорта нет `deck.json`.
    DeckJsonMissing,
    /// `deck.json` не является валидным JSON.
    InvalidJson,
    /// Корневую сущность нельзя интерпретировать как CrowdAnki `Deck`.
    RootNotDeck,
    /// Валидный JSON не соответствует ожидаемому типизированному ядру.
    SchemaInvalid,
    /// Запрошенное имя поля отсутствует во всех note models экспорта.
    UnknownField,
    /// Запрошенный deck path отсутствует в экспорте.
    UnknownDeck,
    /// Селектор колоды совпал более чем с одним узлом дерева.
    AmbiguousDeck,
    /// Селектор модели заметок не совпал ни с одной моделью экспорта.
    UnknownModel,
    /// Селектор модели заметок совпал более чем с одной моделью.
    AmbiguousModel,
    /// Модель заметок существует, но её схема полей и шаблонов непригодна
    /// для безопасной сборки значений полей.
    ModelSchemaUnusable,
    /// Модель требует значение поля, которого нет в запросе создания.
    MissingFieldValue,
    /// Новое значение поля создаваемой заметки содержит media-ссылку.
    MediaForbidden,
    /// `guid` новой заметки не свободен: он повторён в самом запросе либо
    /// (у сгенерированного) свободного `guid` не нашлось за отведённые попытки.
    GuidCollision,
    /// `guid` новой заметки уже занят существующей заметкой с другим
    /// содержимым, колодой или моделью.
    GuidConflict,
    /// `create --apply`/`retire --apply`: запрос не содержит разрешённой
    /// стабильной identity.
    UnresolvedGuid,
    /// `create --apply`: запрос не содержит разрешённой identity target deck.
    UnresolvedDeckIdentity,
    /// Селектор и ожидаемая identity колоды указывают на разные узлы.
    DeckIdentityMismatch,
    /// `qa`/`review`: запрошен неизвестный код QA-правила.
    UnknownQaCode,
    /// `visual-report`: before/after нельзя сравнить как один логический export.
    InvalidComparison,
    /// `find` по идентичности не нашёл ни одного совпадения.
    NotFound,
    /// `find` по идентичности нашёл больше одного совпадения.
    Ambiguous,
    /// `edit`: исходный `deck.json` не в канонической форме.
    SourceNotCanonical,
    /// `edit`: запрос правки структурно некорректен.
    InvalidRequest,
    /// `edit`: одна пара «guid — поле» запрошена дважды.
    DuplicateEditTarget,
    /// `edit`: в экспорте нет заметки с таким `guid`.
    NoteNotFound,
    /// Экспорт содержит ERROR (до или после предполагаемой правки).
    ExportInvalid,
    /// `edit`: экспорт пригоден по ERROR, но не подходит для правки.
    ExportNotMutable,
    /// `edit`: текущее значение поля не совпало с `expected`.
    ExpectedMismatch,
    /// `edit`: `deck.json` изменился между проверкой и заменой файла.
    SourceChanged,
    /// Не удалось записать/опубликовать результат или настроить системное обслуживание.
    WriteFailed,
    /// Внешняя программа, API или окружение не выполнили операцию.
    ExternalFailure,
    /// Непредвиденный внутренний сбой.
    Internal,
}

impl ErrorCode {
    /// Стабильное snake_case представление кода.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::InputUnreadable => "input_unreadable",
            Self::DeckJsonMissing => "deck_json_missing",
            Self::InvalidJson => "invalid_json",
            Self::RootNotDeck => "root_not_deck",
            Self::SchemaInvalid => "schema_invalid",
            Self::UnknownField => "unknown_field",
            Self::UnknownDeck => "unknown_deck",
            Self::AmbiguousDeck => "ambiguous_deck",
            Self::UnknownModel => "unknown_model",
            Self::AmbiguousModel => "ambiguous_model",
            Self::ModelSchemaUnusable => "model_schema_unusable",
            Self::MissingFieldValue => "missing_field_value",
            Self::MediaForbidden => "media_forbidden",
            Self::GuidCollision => "guid_collision",
            Self::GuidConflict => "guid_conflict",
            Self::UnresolvedGuid => "unresolved_guid",
            Self::UnresolvedDeckIdentity => "unresolved_deck_identity",
            Self::DeckIdentityMismatch => "deck_identity_mismatch",
            Self::UnknownQaCode => "unknown_qa_code",
            Self::InvalidComparison => "invalid_comparison",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::SourceNotCanonical => "source_not_canonical",
            Self::InvalidRequest => "invalid_request",
            Self::DuplicateEditTarget => "duplicate_edit_target",
            Self::NoteNotFound => "note_not_found",
            Self::ExportInvalid => "export_invalid",
            Self::ExportNotMutable => "export_not_mutable",
            Self::ExpectedMismatch => "expected_mismatch",
            Self::SourceChanged => "source_changed",
            Self::WriteFailed => "write_failed",
            Self::ExternalFailure => "external_failure",
            Self::Internal => "internal_error",
        }
    }

    /// Process exit code, соответствующий коду ошибки.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::InputUnreadable
            | Self::DeckJsonMissing
            | Self::InvalidJson
            | Self::RootNotDeck
            | Self::SchemaInvalid
            | Self::UnknownField
            | Self::UnknownDeck
            | Self::UnknownModel
            | Self::MissingFieldValue
            | Self::MediaForbidden
            | Self::UnresolvedGuid
            | Self::UnresolvedDeckIdentity
            | Self::DeckIdentityMismatch
            | Self::InvalidComparison
            | Self::UnknownQaCode
            | Self::SourceNotCanonical
            | Self::InvalidRequest
            | Self::DuplicateEditTarget => 3,
            Self::NotFound | Self::NoteNotFound => 4,
            Self::Ambiguous | Self::AmbiguousDeck | Self::AmbiguousModel => 5,
            Self::ExportInvalid
            | Self::ExportNotMutable
            | Self::ModelSchemaUnusable
            | Self::GuidCollision
            | Self::GuidConflict => 6,
            Self::ExpectedMismatch | Self::SourceChanged => 7,
            Self::WriteFailed => 8,
            Self::ExternalFailure => 9,
            Self::Internal => 70,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Доменная ошибка выполнения команды.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct DomainError {
    /// Стабильный машиночитаемый код.
    pub code: ErrorCode,
    /// Человекочитаемое объяснение (на русском языке).
    pub message: String,
    /// Дополнительные машиночитаемые детали.
    pub details: Value,
}

impl DomainError {
    /// Ошибка без дополнительных деталей.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Object(Map::new()),
        }
    }

    /// Ошибка с дополнительными деталями.
    pub fn with_details(code: ErrorCode, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }

    /// Process exit code этой ошибки.
    pub const fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}

/// Собирает JSON-объект деталей из пар «ключ — значение».
#[macro_export]
macro_rules! details {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = ::serde_json::Map::new();
        $(
            map.insert(
                ::std::string::String::from($key),
                ::serde_json::json!($value),
            );
        )*
        ::serde_json::Value::Object(map)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_process_contract() {
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::InputUnreadable.exit_code(), 3);
        assert_eq!(ErrorCode::DeckJsonMissing.exit_code(), 3);
        assert_eq!(ErrorCode::InvalidJson.exit_code(), 3);
        assert_eq!(ErrorCode::RootNotDeck.exit_code(), 3);
        assert_eq!(ErrorCode::SchemaInvalid.exit_code(), 3);
        assert_eq!(ErrorCode::UnknownField.exit_code(), 3);
        assert_eq!(ErrorCode::UnknownDeck.exit_code(), 3);
        assert_eq!(ErrorCode::NotFound.exit_code(), 4);
        assert_eq!(ErrorCode::Ambiguous.exit_code(), 5);
        assert_eq!(ErrorCode::ExternalFailure.exit_code(), 9);
        assert_eq!(ErrorCode::Internal.exit_code(), 70);
    }

    #[test]
    fn edit_codes_match_process_contract() {
        assert_eq!(ErrorCode::SourceNotCanonical.exit_code(), 3);
        assert_eq!(ErrorCode::InvalidRequest.exit_code(), 3);
        assert_eq!(ErrorCode::DuplicateEditTarget.exit_code(), 3);
        assert_eq!(ErrorCode::NoteNotFound.exit_code(), 4);
        assert_eq!(ErrorCode::ExportInvalid.exit_code(), 6);
        assert_eq!(ErrorCode::ExportNotMutable.exit_code(), 6);
        assert_eq!(ErrorCode::ExpectedMismatch.exit_code(), 7);
        assert_eq!(ErrorCode::SourceChanged.exit_code(), 7);
        assert_eq!(ErrorCode::WriteFailed.exit_code(), 8);
        assert_eq!(
            ErrorCode::SourceNotCanonical.as_str(),
            "source_not_canonical"
        );
        assert_eq!(ErrorCode::InvalidRequest.as_str(), "invalid_request");
        assert_eq!(
            ErrorCode::DuplicateEditTarget.as_str(),
            "duplicate_edit_target"
        );
        assert_eq!(ErrorCode::NoteNotFound.as_str(), "note_not_found");
        assert_eq!(ErrorCode::ExportInvalid.as_str(), "export_invalid");
        assert_eq!(ErrorCode::ExportNotMutable.as_str(), "export_not_mutable");
        assert_eq!(ErrorCode::ExpectedMismatch.as_str(), "expected_mismatch");
        assert_eq!(ErrorCode::SourceChanged.as_str(), "source_changed");
        assert_eq!(ErrorCode::WriteFailed.as_str(), "write_failed");
        assert_eq!(ErrorCode::ExternalFailure.as_str(), "external_failure");
    }

    /// Семантика кодов завершения, которую обязан документировать `docs/ru/anki-repo/README.md`.
    ///
    /// Один exit code описывает несколько кодов ошибок, поэтому документация
    /// должна перечислять их вместе: `4` — `find`/`not_found` и
    /// `edit`/`note_not_found`, `7` — `edit`/`expected_mismatch` и
    /// `edit`/`source_changed`.
    #[test]
    fn shared_exit_codes_cover_all_documented_reasons() {
        // Exit 4: нет совпадений у `find` и нет заметки у `edit`.
        assert_eq!(ErrorCode::NotFound.exit_code(), 4);
        assert_eq!(ErrorCode::NoteNotFound.exit_code(), 4);

        // Exit 7: конфликт предусловия по значению поля и устаревший исходник.
        assert_eq!(ErrorCode::ExpectedMismatch.exit_code(), 7);
        assert_eq!(ErrorCode::SourceChanged.exit_code(), 7);

        // Коды различаются, несмотря на общий exit code: вызывающей программе нужна причина,
        // а не только код процесса.
        assert_ne!(ErrorCode::NotFound, ErrorCode::NoteNotFound);
        assert_ne!(ErrorCode::ExpectedMismatch, ErrorCode::SourceChanged);
        assert_ne!(
            ErrorCode::NotFound.as_str(),
            ErrorCode::NoteNotFound.as_str()
        );
        assert_ne!(
            ErrorCode::ExpectedMismatch.as_str(),
            ErrorCode::SourceChanged.as_str()
        );
    }

    #[test]
    fn codes_are_stable_snake_case() {
        let codes = [
            ErrorCode::Usage,
            ErrorCode::InputUnreadable,
            ErrorCode::DeckJsonMissing,
            ErrorCode::InvalidJson,
            ErrorCode::RootNotDeck,
            ErrorCode::SchemaInvalid,
            ErrorCode::UnknownField,
            ErrorCode::UnknownDeck,
            ErrorCode::AmbiguousDeck,
            ErrorCode::UnknownModel,
            ErrorCode::AmbiguousModel,
            ErrorCode::ModelSchemaUnusable,
            ErrorCode::MissingFieldValue,
            ErrorCode::MediaForbidden,
            ErrorCode::GuidCollision,
            ErrorCode::GuidConflict,
            ErrorCode::UnresolvedGuid,
            ErrorCode::UnresolvedDeckIdentity,
            ErrorCode::DeckIdentityMismatch,
            ErrorCode::UnknownQaCode,
            ErrorCode::InvalidComparison,
            ErrorCode::NotFound,
            ErrorCode::Ambiguous,
            ErrorCode::SourceNotCanonical,
            ErrorCode::InvalidRequest,
            ErrorCode::DuplicateEditTarget,
            ErrorCode::NoteNotFound,
            ErrorCode::ExportInvalid,
            ErrorCode::ExportNotMutable,
            ErrorCode::ExpectedMismatch,
            ErrorCode::SourceChanged,
            ErrorCode::WriteFailed,
            ErrorCode::ExternalFailure,
            ErrorCode::Internal,
        ];
        for code in codes {
            let text = code.as_str();
            assert!(
                text.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "код {text} не является snake_case"
            );
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn domain_error_reports_its_exit_code() {
        let error = DomainError::new(ErrorCode::NotFound, "нет совпадений");
        assert_eq!(error.exit_code(), 4);
        assert!(error.details.is_object());
    }

    #[test]
    fn details_macro_builds_object_with_values() {
        let details = crate::details! {
            "count" => 3,
            "name" => "значение",
        };
        assert_eq!(details["count"], serde_json::json!(3));
        assert_eq!(details["name"], serde_json::json!("значение"));
    }
}
