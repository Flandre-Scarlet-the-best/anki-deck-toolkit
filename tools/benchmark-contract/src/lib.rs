//! Контракт измерений Anki-decks: каталог сценариев бенчмаркинга, модель
//! результатов и их проверка.
//!
//! Библиотека ничего не измеряет и не запускает: она загружает декларации,
//! проверяет их и результаты, импортирует экспорт Hyperfine 2.0 и отвечает,
//! сопоставимы ли два результата. Оркестратор измерений использует эти типы,
//! а не собственные копии.

pub mod catalog;
pub mod comparability;
pub mod diagnostic;
pub mod hyperfine;
pub mod placeholder;
pub mod preflight;
pub mod result;
pub mod scenario;
pub mod sensitive;
pub mod vocabulary;
pub mod workspace;
