//! Программный контракт обмена. Универсальный CLI этот API не вызывает.
//!
//! Подтверждённый вход передаётся будущим доверенным адаптером в памяти;
//! `decks/input` здесь не открывается. Рабочие экспорты читаются отдельно,
//! через прежнюю границу запрета input. Метаданные не являются историей колод.

mod compare;
mod filesystem;
mod identity;
mod snapshot;
mod state;

pub use compare::{Comparison, Difference, Disposition, compare};
pub use identity::{DeckIdentity, ProfileId, export_name};
pub use snapshot::{FieldAddress, Snapshot};
pub use state::{
    Confirmation, ExchangeStore, InputRevision, Preparation, PreparationAction, Publication,
    PublishedRevision, RevisionToken, StateView, StreamState,
};

/// Ошибки обмена отделены от стабильных кодов существующего CLI.
#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("небезопасная адресация: {0}")]
    UnsafePath(String),
    #[error("непригодный снимок: {0}")]
    InvalidSnapshot(String),
    #[error("несовпадение идентичности профиля/колоды")]
    IdentityMismatch,
    #[error("служебное состояние повреждено или не поддерживается: {0}")]
    CorruptState(String),
    #[error("предусловие устарело; перечитайте состояние")]
    StaleState,
    #[error("операция заблокирована: {0}")]
    Blocked(String),
    #[error("ошибка файловой системы: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Export(#[from] crate::error::DomainError),
}

pub type Result<T> = std::result::Result<T, ExchangeError>;

pub(super) fn digest(value: &impl serde::Serialize) -> String {
    // Value упорядочивает ключи объектов и убирает зависимость хеша от порядка
    // полей Rust-структуры, как и от порядка ключей входного CrowdAnki JSON.
    let value = serde_json::to_value(value).expect("сериализуемый тип обмена");
    asset_store::hashing::sha256_hex(serde_json::to_vec(&value).expect("JSON Value"))
}

pub(super) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
