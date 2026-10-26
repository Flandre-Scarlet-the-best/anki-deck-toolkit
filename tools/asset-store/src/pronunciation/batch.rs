//! Локальный пакет: неизменяемые снимки источников и контрольные точки для каждого исхода.
use super::{MAX_AUDIO_BYTES, Pronunciation, import_bytes};
use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::{AssetError, AssetStore, ErrorCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const MAX_BATCH_ITEMS: usize = 128;
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlan {
    pub schema_version: u32,
    pub items: Vec<ImportItem>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportItem {
    pub surface: String,
    pub reading: String,
    pub file: PathBuf,
    pub expected_source_sha256: String,
    pub replace_expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemOutcome {
    pub code: String,
    pub message: String,
    pub sha256: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    pub request: Pronunciation,
    pub source_name: String,
    pub replace_expected_sha256: Option<String>,
    pub blob: Option<RuntimeBlobRef>,
    pub outcome: Option<ItemOutcome>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub cancelled: bool,
    pub items: Vec<BatchItem>,
}
fn invalid(message: &str) -> AssetError {
    AssetError::new(ErrorCode::ManifestCorrupt, message)
}
fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RuntimeBatchState for AudioBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn maximum_blob_bytes(&self) -> u64 {
        MAX_AUDIO_BYTES
    }
    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.items.iter().filter_map(|i| i.blob.clone()).collect()
    }
    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != 1
            || self.revision > i64::MAX as u64
            || self.items.is_empty()
            || self.items.len() > MAX_BATCH_ITEMS
        {
            return Err(invalid("неподдерживаемая схема или размер аудиопакета"));
        }
        let mut identities = BTreeSet::new();
        for item in &self.items {
            Pronunciation::from_identity(&item.request.identity())?;
            if !identities.insert(item.request.identity())
                || item.source_name.is_empty()
                || item.source_name.contains(['/', '\\'])
                || item.source_name.chars().any(char::is_control)
                || item
                    .replace_expected_sha256
                    .as_ref()
                    .is_some_and(|s| !valid_sha(s))
                || (item.blob.is_none() && item.outcome.is_none())
            {
                return Err(invalid("некорректный элемент аудиопакета"));
            }
            if let Some(outcome) = &item.outcome
                && (outcome.code.is_empty()
                    || outcome.message.is_empty()
                    || outcome.sha256.as_ref().is_some_and(|s| !valid_sha(s))
                    || (outcome.code == "imported"
                        && (item.blob.is_none()
                            || outcome.sha256.as_deref()
                                != item.blob.as_ref().map(|b| b.sha256.as_str()))))
            {
                return Err(invalid("некорректный исход аудиопакета"));
            }
        }
        Ok(())
    }
}

/// До публикации всех кандидатов план фиксируется вместе со снимками. Смена
/// исходных файлов после этого не влияет на продолжение; абсолютные пути не нужны.
pub fn start(
    store: &AssetStore,
    batch_id: &str,
    plan: ImportPlan,
    base: &Path,
    read: impl Fn(&Path) -> Result<Vec<u8>, AssetError>,
) -> Result<AudioBatch, AssetError> {
    if plan.schema_version != 1 || plan.items.is_empty() || plan.items.len() > MAX_BATCH_ITEMS {
        return Err(invalid(
            "ожидается план schema_version=1 с 1–128 элементами",
        ));
    }
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    if runtime.load::<AudioBatch>()?.is_some() {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "пакет уже существует; используйте resume",
        ));
    }
    let mut identities = BTreeSet::new();
    // Структурные ошибки плана проверяются до чтения файлов.
    for item in &plan.items {
        let request = Pronunciation::new(&item.surface, &item.reading)?;
        if !identities.insert(request.identity())
            || !valid_sha(&item.expected_source_sha256)
            || item
                .replace_expected_sha256
                .as_ref()
                .is_some_and(|s| !valid_sha(s))
        {
            return Err(invalid("повтор идентичности или некорректный SHA в плане"));
        }
    }
    let mut batch = AudioBatch {
        schema_version: 1,
        batch_id: batch_id.into(),
        revision: 0,
        cancelled: false,
        items: Vec::new(),
    };
    let mut total = 0usize;
    let mut prepared = Vec::with_capacity(plan.items.len());
    for item in plan.items {
        let request = Pronunciation::new(&item.surface, &item.reading)?;
        let source_name = item
            .file
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.chars().any(char::is_control) && !s.contains('\\'))
            .ok_or_else(|| invalid("нет безопасного имени исходника"))?
            .to_owned();
        let source = read(&base.join(&item.file));
        if let Ok(bytes) = &source {
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| invalid("суммарный размер пакета превышает 64 MiB"))?;
            if total > MAX_BATCH_BYTES {
                return Err(invalid("суммарный размер пакета превышает 64 MiB"));
            }
        }
        prepared.push((item, request, source_name, source));
    }

    // Не сохраняем снимки в служебной области до проверки суммарного размера пакета.
    for (item, request, source_name, source) in prepared {
        let result = source.and_then(|bytes| {
            if crate::hashing::sha256_hex(&bytes) != item.expected_source_sha256 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "источник не совпадает с SHA плана",
                ));
            }
            runtime.persist_blob_with_limit(&bytes, "mp3", MAX_AUDIO_BYTES)
        });
        let (blob, outcome) = match result {
            Ok(blob) => (Some(blob), None),
            Err(error) => (
                None,
                Some(ItemOutcome {
                    code: error.code.as_str().into(),
                    message: error.message,
                    sha256: None,
                }),
            ),
        };
        batch.items.push(BatchItem {
            request,
            source_name,
            replace_expected_sha256: item.replace_expected_sha256,
            blob,
            outcome,
        });
    }
    runtime.save(&batch)?;
    Ok(batch)
}

pub fn status(store: &AssetStore, batch_id: &str) -> Result<AudioBatch, AssetError> {
    SafeBatchRuntime::open(store.root(), batch_id)?
        .load()?
        .ok_or_else(|| invalid("аудиопакет отсутствует"))
}
pub fn cancel(store: &AssetStore, batch_id: &str) -> Result<AudioBatch, AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    let mut batch: AudioBatch = runtime
        .load()?
        .ok_or_else(|| invalid("аудиопакет отсутствует"))?;
    if !batch.cancelled {
        batch.cancelled = true;
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}

/// Блокировка служебной области сериализует исполнителей. Смерть после импорта до контрольной точки
/// приводит только к идемпотентному повтору той же пары/SHA при следующем продолжении.
pub fn resume(
    store: &AssetStore,
    batch_id: &str,
    maximum_items: usize,
) -> Result<AudioBatch, AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    let mut batch: AudioBatch = runtime
        .load()?
        .ok_or_else(|| invalid("аудиопакет отсутствует"))?;
    if batch.cancelled {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "аудиопакет отменён",
        ));
    }
    for index in (0..batch.items.len())
        .filter(|&i| batch.items[i].outcome.is_none())
        .take(maximum_items)
        .collect::<Vec<_>>()
    {
        let item = &batch.items[index];
        let blob = item.blob.as_ref().expect("состояние проверено");
        let result = runtime.read_blob(blob).and_then(|bytes| {
            import_bytes(
                store,
                &item.request,
                &bytes,
                &item.source_name,
                item.replace_expected_sha256.clone(),
            )
        });
        batch.items[index].outcome = Some(match result {
            Ok(record) => ItemOutcome {
                code: "imported".into(),
                message: "кандидат сохранён; доверие определяется текущим манифестом".into(),
                sha256: Some(record.sha256),
            },
            Err(error) => ItemOutcome {
                code: error.code.as_str().into(),
                message: error.message,
                sha256: Some(blob.sha256.clone()),
            },
        });
        batch.revision += 1;
        runtime.save(&batch)?;
    }
    Ok(batch)
}
