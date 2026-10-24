use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use super::{ExchangeError, Result, digest};

/// Устойчивый идентификатор профиля, назначенный адаптером; не отображаемое имя.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_id(&id)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Имя/путь экспорта не участвуют в равенстве идентичностей.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckIdentity {
    pub profile: ProfileId,
    pub crowdanki_uuid: String,
}

impl DeckIdentity {
    pub fn new(profile: ProfileId, crowdanki_uuid: impl Into<String>) -> Result<Self> {
        let identity = Self {
            profile,
            crowdanki_uuid: crowdanki_uuid.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_id(self.profile.as_str())?;
        validate_id(&self.crowdanki_uuid)
    }

    /// Полный SHA пары с однозначным JSON-кодированием, без усечения UUID.
    pub fn key(&self) -> String {
        digest(self)
    }

    /// Плоский адрес: читаемый префикс и суффикс идентичности.
    pub fn output_directory(&self, name: &str) -> Result<String> {
        self.validate()?;
        let name = export_name(name)?;
        let mut prefix = String::new();
        for c in name.chars() {
            if prefix.len() + c.len_utf8() > 100 {
                break;
            }
            prefix.push(c);
        }
        Ok(format!("{prefix}--{}", self.key()))
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 512 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(ExchangeError::UnsafePath(
            "пустая или неоднозначная идентичность".into(),
        ));
    }
    Ok(())
}

/// Соглашение об имени, а не идентичность. Небезопасные компоненты не исправляются.
pub fn export_name(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 4096 {
        return Err(ExchangeError::UnsafePath(
            "пустое или слишком длинное имя колоды".into(),
        ));
    }
    for part in name.split("::") {
        validate_component(part)?;
    }
    Ok(name.nfc().collect::<String>().replace("::", "__"))
}

pub(super) fn validate_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.trim() != name
        || name.ends_with('.')
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
    {
        return Err(ExchangeError::UnsafePath(format!(
            "недопустимый компонент {name:?}"
        )));
    }
    Ok(())
}

pub(super) fn path_key(name: &str) -> String {
    name.nfc().flat_map(char::to_lowercase).collect()
}
