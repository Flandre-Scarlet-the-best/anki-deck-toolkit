//! Машинные коды отказов и единый тип ошибки подсистемы.
//!
//! Коды — публичный контракт JSON-вывода: они различают отсутствие словаря,
//! неправильный формат, повреждённый индекс, недоступный источник публикации и
//! непроверяемую версию. Текст сообщения — для человека и может меняться.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// Идентификатор не входит в поддерживаемый набор.
    UnknownDictionary,
    /// Словарь не установлен.
    NotInstalled,
    /// Архив не является ожидаемым Yomitan-словарём: структура, JSON, типы записей.
    InvalidFormat,
    /// Формат распознан, но его версия или тип банка не поддерживаются.
    UnsupportedSchema,
    /// Небезопасный архив: traversal, symlink, zip bomb, запрещённые имена.
    UnsafeArchive,
    /// Превышены пределы размера, распаковки или числа записей.
    LimitExceeded,
    /// Полученные байты не совпали с опубликованной контрольной суммой.
    ChecksumMismatch,
    /// Локальный индекс или манифест повреждён либо не совпал с записанным.
    IndexCorrupt,
    /// Сеть или источник публикации недоступны (DNS, TLS, тайм-аут, 5xx).
    UpstreamUnavailable,
    /// Upstream ответил, но не так, как ожидается: 404, нет ассета, чужой хост.
    UpstreamUnexpected,
    /// Upstream ограничил частоту запросов.
    UpstreamRateLimited,
    /// Другая изменяющая операция уже выполняется над тем же каталогом данных.
    Busy,
    /// Каталог данных не принадлежит подсистеме или лежит в рабочей копии Git.
    NotOwned,
    /// Разрушительное действие без подтверждения.
    ConfirmationRequired,
    /// Неверный запрос пользователя.
    InvalidInput,
    /// Ошибка ввода-вывода локальной файловой системы (в том числе нехватка места).
    Io,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub code: Code,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn io(context: impl fmt::Display, error: std::io::Error) -> Self {
        Self::new(Code::Io, format!("{context}: {error}"))
    }

    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::new(Code::IndexCorrupt, message)
    }

    pub fn format(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidFormat, message)
    }

    pub fn unsafe_archive(message: impl Into<String>) -> Self {
        Self::new(Code::UnsafeArchive, message)
    }

    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(Code::LimitExceeded, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        // Нехватка места и сбой ввода-вывода — не повреждение данных:
        // пользователь должен видеть, что чинить нужно диск, а не индекс.
        let io = matches!(
            &error,
            rusqlite::Error::SqliteFailure(e, _)
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DiskFull
                        | rusqlite::ErrorCode::SystemIoFailure
                        | rusqlite::ErrorCode::CannotOpen
                        | rusqlite::ErrorCode::PermissionDenied
                        | rusqlite::ErrorCode::ReadOnly
                )
        );
        if io {
            Self::new(Code::Io, format!("SQLite: {error}"))
        } else {
            Self::corrupt(format!("SQLite: {error}"))
        }
    }
}

/// Машинное имя кода, как в JSON.
pub fn code_name(code: Code) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
