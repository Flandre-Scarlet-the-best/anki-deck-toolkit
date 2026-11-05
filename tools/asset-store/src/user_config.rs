//! Общая пользовательская конфигурация инструментов `anki-deck-toolkit`.
//!
//! Это личный файл пользователя вне рабочей копии, а не versioned-конфиг
//! репозитория: `$XDG_CONFIG_HOME/anki-deck-toolkit/config.toml` (или
//! `~/.config/…`). Путь заменяют `ANKI_DECK_TOOLKIT_CONFIG` и, выше него,
//! явный `--config`. Отсутствие файла по умолчанию — обычное состояние:
//! действуют значения по умолчанию. Явно названный, но отсутствующий файл —
//! ошибка.
//!
//! Документ разбирается строго: неизвестные разделы и ключи, неизвестные
//! значения перечислений — ошибка с названием ключа. Секреты и пути профилей
//! сюда не входят: сопряжение браузера хранит каталог состояния адаптера.
//! Конфигурация локального inference живёт отдельно (`inference_config`).
//!
//! Приоритет каждого значения: явный флаг CLI, затем файл, затем умолчание;
//! [`ForvoSettings::sources`] сообщает, откуда взято каждое значение.
use crate::{AssetError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const ENV_PATH: &str = "ANKI_DECK_TOOLKIT_CONFIG";
const MAX_BYTES: u64 = 64 * 1024;

/// Стратегия транспорта общего пользовательского пути Forvo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum ForvoMode {
    /// Сначала собственный Chromium/CDP; при доказанной невозможности его
    /// использовать и до опасных побочных эффектов — настроенный MV3-адаптер.
    ChromiumFirst,
    /// Только установленное и сопряжённое расширение выбранной цели;
    /// собственный Chromium не запускается даже для проверки.
    ExtensionOnly,
}

impl ForvoMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::ChromiumFirst => "chromium_first",
            Self::ExtensionOnly => "extension_only",
        }
    }
}

/// Редкая автоматическая проверка новой версии адаптера при обычной работе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateCheck {
    Weekly,
    Off,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForvoSection {
    mode: Option<ForvoMode>,
    browser_target: Option<String>,
    update_check: Option<UpdateCheck>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: Option<u32>,
    forvo: Option<ForvoSection>,
}

/// Откуда взят файл конфигурации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "path")]
pub enum FileSource {
    Cli(PathBuf),
    Environment(PathBuf),
    Default(PathBuf),
    /// Каталог конфигурации пользователя неизвестен (нет HOME и XDG_CONFIG_HOME).
    Unavailable,
}

impl FileSource {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Cli(p) | Self::Environment(p) | Self::Default(p) => Some(p),
            Self::Unavailable => None,
        }
    }
}

/// Итоговые настройки Forvo после применения слоёв.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForvoSettings {
    pub mode: ForvoMode,
    pub browser_target: Option<String>,
    pub update_check: UpdateCheck,
    pub file: FileSource,
    pub file_loaded: bool,
    /// Слой каждого значения: `cli`, `file` или `default`.
    pub sources: [(&'static str, &'static str); 3],
}

impl ForvoSettings {
    pub fn describe(&self) -> Value {
        json!({
            "mode": self.mode.id(),
            "browser_target": self.browser_target,
            "update_check": self.update_check,
            "config_file": self.file,
            "config_file_loaded": self.file_loaded,
            "sources": self.sources.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        })
    }
}

/// Явные значения команды; `None` — не заданы.
#[derive(Debug, Clone, Default)]
pub struct ForvoOverrides {
    pub config: Option<PathBuf>,
    pub mode: Option<ForvoMode>,
    pub browser_target: Option<String>,
}

fn config_error(code: &str, message: impl Into<String>, details: Value) -> AssetError {
    let mut details = details;
    details["code"] = json!(code);
    AssetError::with_details(ErrorCode::InvalidIdentity, message, details)
}

fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("anki-deck-toolkit/config.toml"))
}

fn locate(cli: Option<&Path>) -> FileSource {
    if let Some(path) = cli {
        return FileSource::Cli(path.to_owned());
    }
    if let Some(path) = std::env::var_os(ENV_PATH).filter(|v| !v.is_empty()) {
        return FileSource::Environment(PathBuf::from(path));
    }
    default_path().map_or(FileSource::Unavailable, FileSource::Default)
}

fn read(source: &FileSource) -> Result<Option<String>, AssetError> {
    let Some(path) = source.path() else {
        return Ok(None);
    };
    let explicit = !matches!(source, FileSource::Default(_));
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => return Ok(None),
        Err(_) => {
            return Err(config_error(
                "config_unreadable",
                "Файл конфигурации не найден или недоступен",
                json!({"config_file":path}),
            ));
        }
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(config_error(
            "config_unreadable",
            "Файл конфигурации должен быть обычным файлом не больше 64 КиБ",
            json!({"config_file":path}),
        ));
    }
    std::fs::read_to_string(path).map(Some).map_err(|_| {
        config_error(
            "config_unreadable",
            "Файл конфигурации не читается как UTF-8",
            json!({"config_file":path}),
        )
    })
}

/// Строгий разбор текста конфигурации (без слоёв и путей).
fn parse(text: &str, path: Option<&Path>) -> Result<Document, AssetError> {
    let document: Document = toml::from_str(text).map_err(|e| {
        config_error(
            "config_invalid",
            "Файл конфигурации не прошёл строгую проверку: неизвестный раздел, ключ или значение",
            json!({"config_file":path,"reason":e.message(),
                "allowed":{"schema_version":[1],"forvo.mode":["chromium_first","extension_only"],
                    "forvo.browser_target":"имя настроенной цели","forvo.update_check":["weekly","off"]}}),
        )
    })?;
    if document.schema_version.is_some_and(|v| v != 1) {
        return Err(config_error(
            "config_invalid",
            "Неподдерживаемая schema_version конфигурации",
            json!({"config_file":path,"schema_version":document.schema_version}),
        ));
    }
    if let Some(target) = document
        .forvo
        .as_ref()
        .and_then(|f| f.browser_target.as_deref())
        && !crate::forvo::brave::targets::valid_target_name(target)
    {
        return Err(config_error(
            "config_invalid",
            "forvo.browser_target: имя цели 1–32 символа a–z, 0–9, '-'",
            json!({"config_file":path,"browser_target":target}),
        ));
    }
    Ok(document)
}

/// Применяет слои: явный флаг CLI, файл, значение по умолчанию.
pub fn forvo(overrides: &ForvoOverrides) -> Result<ForvoSettings, AssetError> {
    let file = locate(overrides.config.as_deref());
    let text = read(&file)?;
    let section = match &text {
        Some(text) => parse(text, file.path())?.forvo.unwrap_or_default(),
        None => ForvoSection::default(),
    };
    if let Some(target) = &overrides.browser_target
        && !crate::forvo::brave::targets::valid_target_name(target)
    {
        return Err(config_error(
            "invalid_target",
            "Имя цели: 1–32 символа a–z, 0–9, '-'",
            json!({"browser_target":target}),
        ));
    }
    let layer = |cli: bool, file: bool| {
        if cli {
            "cli"
        } else if file {
            "file"
        } else {
            "default"
        }
    };
    Ok(ForvoSettings {
        sources: [
            (
                "mode",
                layer(overrides.mode.is_some(), section.mode.is_some()),
            ),
            (
                "browser_target",
                layer(
                    overrides.browser_target.is_some(),
                    section.browser_target.is_some(),
                ),
            ),
            ("update_check", layer(false, section.update_check.is_some())),
        ],
        mode: overrides
            .mode
            .or(section.mode)
            .unwrap_or(ForvoMode::ChromiumFirst),
        browser_target: overrides.browser_target.clone().or(section.browser_target),
        update_check: section.update_check.unwrap_or(UpdateCheck::Weekly),
        file_loaded: text.is_some(),
        file,
    })
}

#[cfg(test)]
#[path = "user_config_tests.rs"]
mod tests;
