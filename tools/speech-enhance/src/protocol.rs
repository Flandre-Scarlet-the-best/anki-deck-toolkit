//! Машинный протокол процесса `speech-enhance-dfn`.
//!
//! Обмен — файлами PCM `f32` little-endian с чередованием каналов на 48 kHz и
//! одним JSON-объектом в stdout. Ошибка — тоже JSON в stdout с кодом и
//! ненулевым кодом выхода: 3 — модель (нет, повреждена, не та версия),
//! 4 — вход, 5 — ввод-вывод или инференс.

use serde::{Deserialize, Serialize};

use crate::enhance::{Diagnostics, InferenceParams};
use crate::model::ModelIdentity;

pub const PROTOCOL_VERSION: u32 = 1;
pub const WORKER_NAME: &str = "speech-enhance-dfn";
pub const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Точная версия crate инференса — единственный источник в коде. Её
/// совпадение с пином Cargo.toml и с версией в Cargo.lock проверяет тест
/// `model::tests::declared_deep_filter_rt_version_is_the_resolved_dependency`.
pub const DEEP_FILTER_RT_VERSION: &str = "0.1.1";
/// Предел файла входа: 30 с, 48 kHz, стерео, f32.
pub const MAX_INPUT_BYTES: u64 = 30 * 48_000 * 2 * 4;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInfo {
    pub library: String,
    pub library_version: String,
    pub inference: String,
    pub device: String,
    pub threads: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub hop_samples: u64,
    pub delay_samples: u64,
    pub max_channels: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub schema_version: u32,
    pub kind: String,
    pub backend: String,
    pub worker: WorkerInfo,
    pub runtime: RuntimeInfo,
    pub model: ModelIdentity,
    pub stream: StreamInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessResult {
    pub schema_version: u32,
    pub kind: String,
    pub model_manifest_sha256: String,
    pub params: InferenceParams,
    pub diagnostics: Diagnostics,
    /// SHA-256 байтов файла вывода.
    pub output_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorReport {
    pub schema_version: u32,
    pub kind: String,
    pub code: String,
    pub message: String,
}

pub fn runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        library: "deep-filter-rt".into(),
        library_version: DEEP_FILTER_RT_VERSION.into(),
        inference: "tract".into(),
        device: "cpu".into(),
        threads: 1,
    }
}

pub fn worker_info() -> WorkerInfo {
    WorkerInfo {
        name: WORKER_NAME.into(),
        version: WORKER_VERSION.into(),
        protocol: PROTOCOL_VERSION,
    }
}
