//! Локальная история бенчмарков Anki-decks.
//!
//! Принимает только результаты, прошедшие `benchmark_contract::result::check`,
//! и хранит их исходные JSON без изменений в неизменяемом архиве,
//! адресуемом по содержимому. SQLite — производный индекс для поиска и
//! аналитики: он восстанавливается из архива в любой момент. Сопоставимость
//! решает `benchmark_contract::comparability`; здесь — выбор базы, разности
//! до/после, отчёты для PR и снимки русских страниц `docs/ru/<модуль>/performance.md`.
//! Ничего не измеряет, не ходит в сеть и ничего не публикует сам.

pub mod analysis;
pub mod archive;
pub mod format;
pub mod index;
pub mod location;
pub mod reason;
pub mod report;
pub mod snapshot;
pub mod store;
