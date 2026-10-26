//! Диагностика: где (файл, сценарий, поле) и почему нарушен контракт.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Стабильный машинный код причины.
    pub code: &'static str,
    /// Путь файла относительно корня каталога или имя проверяемого документа.
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    /// Путь поля в декларации или документе, например `profile[1].prepare[0].program`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            Severity::Error => "ошибка",
            Severity::Warning => "предупреждение",
        };
        write!(f, "{level} [{}] {}", self.code, self.file)?;
        if let Some(scenario) = &self.scenario {
            write!(f, " · сценарий {scenario}")?;
        }
        if let Some(field) = &self.field {
            write!(f, " · поле {field}")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Накопитель диагностики с общим контекстом файла и сценария.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn at<'a>(&'a mut self, file: &str, scenario: Option<&str>) -> Sink<'a> {
        Sink {
            diagnostics: self,
            file: file.to_owned(),
            scenario: scenario.map(str::to_owned),
        }
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn into_vec(mut self) -> Vec<Diagnostic> {
        self.items.sort_by(|a, b| {
            (a.severity, &a.file, &a.scenario, &a.field, a.code).cmp(&(
                b.severity,
                &b.file,
                &b.scenario,
                &b.field,
                b.code,
            ))
        });
        self.items.dedup();
        self.items
    }
}

pub struct Sink<'a> {
    diagnostics: &'a mut Diagnostics,
    file: String,
    scenario: Option<String>,
}

impl Sink<'_> {
    pub fn error(
        &mut self,
        code: &'static str,
        field: impl Into<Option<String>>,
        message: impl Into<String>,
    ) {
        self.emit(Severity::Error, code, field.into(), message.into());
    }

    pub fn warning(
        &mut self,
        code: &'static str,
        field: impl Into<Option<String>>,
        message: impl Into<String>,
    ) {
        self.emit(Severity::Warning, code, field.into(), message.into());
    }

    fn emit(
        &mut self,
        severity: Severity,
        code: &'static str,
        field: Option<String>,
        message: String,
    ) {
        self.diagnostics.push(Diagnostic {
            severity,
            code,
            file: self.file.clone(),
            scenario: self.scenario.clone(),
            field,
            message,
        });
    }
}

/// Короткая запись `Some(field.to_owned())` для вызовов диагностики.
pub fn field(path: impl Into<String>) -> Option<String> {
    Some(path.into())
}
