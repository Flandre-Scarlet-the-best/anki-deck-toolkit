//! Исполнитель бенчмарков Anki-decks.
//!
//! Запускает сценарии каталога `benchmarks/` по ID и профилю: собирает
//! `cargo-bin`, выполняет жизненный цикл серии, измеряет каждый прогон через
//! Hyperfine 2.0, добавляет системные метрики Linux (cgroup v2, `/proc`,
//! отчёт модуля, файловая система) и записывает результат типами
//! `benchmark_contract`, проверив его тем же контрактом. Собственных моделей
//! каталога, сценария и результата здесь нет.

pub mod cgroup;
pub mod environment;
pub mod hyperfine_exec;
pub mod inputs;
pub mod offline;
pub mod plan;
pub mod procs;
pub mod programs;
pub mod readiness;
pub mod sampler;
pub mod series;
pub mod util;
