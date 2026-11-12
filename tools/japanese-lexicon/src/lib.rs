//! Japanese Lexicon — самостоятельная японская лексическая подсистема.
//!
//! Управляет локальными словарями Yomitan-формата (установка с живого
//! источника публикации, проверка и получение обновлений, проверка целостности,
//! удаление) и отвечает офлайн на вопрос «какие чтения допустимы у этого
//! написания и на основании какого словаря».
//!
//! Подсистема не принадлежит Forvo, ASR, `anki-repo` или конкретной колоде
//! и не читает их данные. Публичный контракт — [`lookup::Lexicon`],
//! [`lookup::LookupResult`] и функции [`manager`]; полный пользовательский
//! контракт — `docs/ru/japanese-lexicon/README.md`.

pub mod catalog;
pub mod error;
pub mod index;
pub mod kana;
pub mod lookup;
pub mod manager;
pub mod store;
pub mod time;
pub mod upstream;
pub mod yomitan;
