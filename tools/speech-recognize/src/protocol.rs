//! Машинный протокол процесса `speech-recognize-ctc`.
//!
//! Вход — файл PCM `f32` little-endian, моно, 16 kHz, без дополнения.
//! Выход — файл `f32` little-endian `кадры × словарь` логарифмов
//! вероятностей по строкам и один JSON-объект в stdout. Ошибка — тоже JSON в
//! stdout с кодом и ненулевым кодом выхода: 3 — модель (нет, повреждена, не
//! та ревизия), 4 — вход, 5 — ввод-вывод или инференс.

use serde::{Deserialize, Serialize};

use crate::model::ModelIdentity;

pub const PROTOCOL_VERSION: u32 = 1;
pub const WORKER_NAME: &str = "speech-recognize-ctc";
pub const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Предел входа: 31 с моно 16 kHz f32 (запись до 30 с плюс запас).
pub const MAX_INPUT_BYTES: u64 = 31 * 16_000 * 4;

pub const EXIT_MODEL: i32 = 3;
pub const EXIT_INPUT: i32 = 4;
pub const EXIT_RUNTIME: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerInfo {
    pub name: String,
    pub version: String,
    pub protocol: u32,
}

/// Среда исполнения. Число потоков не меняет результат (порядок
/// суммирования фиксирован), но сообщается для воспроизводимости замеров.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInfo {
    pub inference: String,
    pub device: String,
    pub threads: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub schema_version: u32,
    pub kind: String,
    pub worker: WorkerInfo,
    pub runtime: RuntimeInfo,
    pub model: ModelIdentity,
}

/// Один вход пакетного `process`: веса проверяются и загружаются один раз
/// на вызов, каждый вход даёт собственный файл эмиссий.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessItem {
    pub input_samples: u64,
    pub input_sha256: String,
    pub frames: u64,
    pub vocab: u64,
    pub output_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessReport {
    pub schema_version: u32,
    pub kind: String,
    pub worker: WorkerInfo,
    pub runtime: RuntimeInfo,
    pub model_id: String,
    pub model_manifest_sha256: String,
    pub items: Vec<ProcessItem>,
    /// Проверка и загрузка весов, мс (диагностика, не детерминирована).
    pub load_ms: f64,
    pub wall_ms: f64,
}

/// Наибольшее число входов одного вызова `process`.
pub const MAX_BATCH_ITEMS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallReport {
    pub schema_version: u32,
    pub kind: String,
    pub model: ModelIdentity,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorReport {
    pub schema_version: u32,
    pub kind: String,
    pub code: String,
    pub message: String,
}

pub fn worker_info() -> WorkerInfo {
    WorkerInfo {
        name: WORKER_NAME.into(),
        version: WORKER_VERSION.into(),
        protocol: PROTOCOL_VERSION,
    }
}

/// Потоки матричного умножения так, как их выберет `matrixmultiply`:
/// значение `MATMUL_NUM_THREADS` в пределах 1–4, без переменной — число
/// физических ядер в тех же пределах (запускатель `asset-store` всегда
/// задаёт переменную явно).
pub fn runtime_info() -> RuntimeInfo {
    let threads = std::env::var("MATMUL_NUM_THREADS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1)
        })
        .clamp(1, 4);
    RuntimeInfo {
        inference: "ndarray-matrixmultiply-f32".into(),
        device: "cpu".into(),
        threads,
    }
}
