//! Измеряемая нагрузка `asset-store` для сценариев каталога `benchmarks/`.
//!
//! Модуль исполняет настоящие операции [`AssetStore`] и [`SafeBatchRuntime`]
//! над детерминированным синтетическим корпусом и ничего не измеряет сам:
//! время, процессор и память собирает `benchmark-runner`. Таймеров, циклов
//! повторов и статистики здесь нет.
//!
//! Жизненный цикл одной серии повторяет шаги сценария:
//!
//! - [`setup`] строит эталонное хранилище штатной публикацией `ingest_verified`
//!   и сохраняет пакетное состояние штатным `SafeBatchRuntime::save`. Эталон
//!   прежней серии переиспользуется, только если его построил тот же
//!   исполняемый файл (SHA-256) по той же спецификации и дерево эталона
//!   совпало с записанным при построении; иначе он строится заново;
//! - [`prepare`] перед каждым изменяющим прогоном восстанавливает рабочую копию
//!   из эталона и надёжно сбрасывает её на диск, чтобы старт был одинаковым;
//! - [`run`] — измеряемая операция;
//! - [`verify`] проверяет постусловия вне измеряемого интервала;
//! - [`clean`] удаляет только каталог, помеченный этим модулем; с
//!   `keep_template` — только рабочую копию, оставляя эталон следующей серии.
//!
//! Семантический валидатор нагрузки — [`BenchValidator`]: полное чтение и
//! декодирование изображения без проверки начертания. Стоимость предметного
//! `KanjiImageValidator` в эти сценарии не входит. Всё остальное — политика
//! домена, staging, манифест, журнал, `fsync`, блокировки, CAS и проверка
//! SHA-256 при чтении — исполняется штатным кодом хранилища без обходов.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use serde::{Deserialize, Serialize};

use crate::batch_runtime::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, Provenance, SemanticDecision, SemanticStatus,
    ValidationEvidence, ValidatorIdentity,
};
use crate::store::{AssetStore, StoreOptions, VerifiedIngestRequest};
use crate::validation::{SemanticValidator, ValidatorFailure};
use crate::{AssetDomainPolicy, GenericDomainPolicy, KanjiDomainPolicy};

/// Версия генератора корпуса. Любое изменение байтов, которые порождает
/// генератор при той же спецификации, обязано увеличить её: иначе
/// идентичность нагрузки (SHA-256 файла спецификации) перестала бы описывать
/// содержимое корпуса. Спецификация с другой версией отвергается.
pub const GENERATOR_VERSION: u32 = 1;

/// Маркер рабочего каталога: [`clean`] и [`setup`] удаляют только помеченное.
pub const MARKER_FILE: &str = ".asset-store-bench.json";
const MARKER_TOOL: &str = "asset-store-bench";
const TEMPLATE_DIR: &str = "template";
const RUN_DIR: &str = "run";
const STORE_DIR: &str = "store";
const INCOMING_DIR: &str = "incoming";
const EXPECTED_FILE: &str = "expected.json";
/// Идентификатор пакетного состояния нагрузки.
pub const BATCH_ID: &str = "bench-batch";
const BATCH_SCHEMA_VERSION: u32 = 1;
const BLOB_EXTENSION: &str = "bin";
/// Первый символ идентичностей домена kanji: CJK Unified Ideographs.
const KANJI_FIRST: u32 = 0x4E00;
const KANJI_LAST: u32 = 0x9FFF;
const MAX_ASSETS: u32 = 20_000;
const MAX_WRITERS: u32 = 64;
const PROVENANCE_KIND: &str = "asset-store-bench";

/// Домен корпуса нагрузки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecDomain {
    /// Политика `kanji`: идентичность — один символ, раскладка по форматам,
    /// форматные варианты GIF и PNG одного символа.
    Kanji,
    /// Политика `generic`: адресация по содержимому, без вариантов формата.
    Generic,
}

/// Спецификация синтетической нагрузки. Файл спецификации — вход сценария:
/// его SHA-256 — идентичность нагрузки, объём сообщает [`describe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadSpec {
    /// Версия генератора; обязана совпасть с [`GENERATOR_VERSION`].
    pub generator: u32,
    pub domain: SpecDomain,
    /// Зерно псевдослучайного содержимого изображений и blob.
    pub seed: u64,
    /// Число идентичностей в корпусе.
    pub assets: u32,
    /// Каждая `png_every`-я идентичность kanji имеет и PNG-вариант; 0 — нет.
    pub png_every: u32,
    /// Сторона квадратного изображения в пикселях.
    pub image_side: u32,
    /// Число запросов чтения: первые записи корпуса в каноническом порядке.
    pub reads: u32,
    /// Число новых идентичностей, публикуемых за один прогон `ingest`.
    pub ingest: u32,
    /// Число записей пакетного состояния.
    pub batch_entries: u32,
    /// Число записей пакета со ссылкой на неизменяемый blob.
    pub batch_blobs: u32,
    /// Размер одного blob в байтах.
    pub blob_bytes: u32,
}

impl WorkloadSpec {
    pub fn load(path: &Path) -> Result<Self, BenchError> {
        let text = fs::read_to_string(path)
            .map_err(|error| BenchError::usage(format!("спецификация не читается: {error}")))?;
        let spec: Self = toml::from_str(&text)
            .map_err(|error| BenchError::usage(format!("спецификация некорректна: {error}")))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), BenchError> {
        let fail = |message: &str| Err(BenchError::usage(message.to_owned()));
        if self.generator != GENERATOR_VERSION {
            return Err(BenchError::usage(format!(
                "спецификация для генератора версии {}, а исполнитель знает только {GENERATOR_VERSION}",
                self.generator
            )));
        }
        if self.assets == 0 || self.assets > MAX_ASSETS {
            return fail("assets — от 1 до 20000");
        }
        if self.domain == SpecDomain::Kanji
            && u64::from(KANJI_FIRST) + u64::from(self.assets) + u64::from(self.ingest)
                > u64::from(KANJI_LAST) + 1
        {
            return fail("assets + ingest не помещаются в диапазон CJK Unified Ideographs");
        }
        if self.domain == SpecDomain::Generic && self.png_every != 0 {
            return fail("домен generic не имеет форматных вариантов: png_every = 0");
        }
        if !(8..=256).contains(&self.image_side) {
            return fail("image_side — от 8 до 256 пикселей");
        }
        if self.reads == 0 || u64::from(self.reads) > self.records() {
            return fail("reads — от 1 до числа записей корпуса");
        }
        if self.ingest == 0 || self.ingest > MAX_ASSETS {
            return fail("ingest — от 1 до 20000");
        }
        if self.batch_entries == 0 || self.batch_entries > 200_000 {
            return fail("batch_entries — от 1 до 200000");
        }
        if self.batch_blobs > self.batch_entries {
            return fail("batch_blobs не больше batch_entries");
        }
        if self.blob_bytes == 0 || self.blob_bytes > 1 << 20 {
            return fail("blob_bytes — от 1 байта до 1 MiB");
        }
        Ok(())
    }

    /// Число записей корпуса: идентичности и их PNG-варианты.
    pub fn records(&self) -> u64 {
        let variants = if self.png_every == 0 {
            0
        } else {
            u64::from(self.assets).div_ceil(u64::from(self.png_every))
        };
        u64::from(self.assets) + variants
    }

    fn policy(&self) -> Arc<dyn AssetDomainPolicy> {
        match self.domain {
            SpecDomain::Kanji => Arc::new(KanjiDomainPolicy),
            SpecDomain::Generic => Arc::new(GenericDomainPolicy),
        }
    }

    fn identity(&self, index: u32) -> AssetIdentity {
        match self.domain {
            SpecDomain::Kanji => {
                let character =
                    char::from_u32(KANJI_FIRST + index).expect("диапазон проверен в validate");
                AssetIdentity::new("kanji", character.to_string())
            }
            SpecDomain::Generic => AssetIdentity::new("bench", format!("asset-{index:05}")),
        }
        .expect("синтетическая идентичность корректна")
    }

    fn domain_metadata(&self, identity: &AssetIdentity) -> Option<serde_json::Value> {
        match self.domain {
            SpecDomain::Kanji => Some(serde_json::json!({ "character": identity.key })),
            SpecDomain::Generic => None,
        }
    }

    /// Записи корпуса в каноническом порядке: идентичность, затем формат.
    fn corpus(&self) -> Vec<(u32, DetectedFormat)> {
        let mut items = Vec::with_capacity(self.records() as usize);
        for index in 0..self.assets {
            items.push((index, DetectedFormat::Gif));
            if self.png_every != 0 && index % self.png_every == 0 {
                items.push((index, DetectedFormat::Png));
            }
        }
        items
    }

    fn image(&self, index: u32, format: DetectedFormat) -> Vec<u8> {
        render_image(self.seed, index, self.image_side, format)
    }

    fn blob(&self, index: u32) -> Vec<u8> {
        let mut state = mix(self.seed ^ 0xB10B, u64::from(index));
        (0..self.blob_bytes)
            .map(|_| {
                state = next(state);
                (state >> 56) as u8
            })
            .collect()
    }
}

/// Ошибка исполнителя: использование (код 2) или отказ операции (код 1).
#[derive(Debug)]
pub struct BenchError {
    pub usage: bool,
    pub message: String,
}

impl BenchError {
    fn usage(message: String) -> Self {
        Self {
            usage: true,
            message,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            usage: false,
            message: message.into(),
        }
    }

    /// Код завершения процесса.
    pub fn exit_code(&self) -> i32 {
        if self.usage { 2 } else { 1 }
    }
}

impl From<AssetError> for BenchError {
    fn from(error: AssetError) -> Self {
        Self::failed(format!("{}: {}", error.code.as_str(), error.message))
    }
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn io_failed(context: &str, error: std::io::Error) -> BenchError {
    BenchError::failed(format!("{context}: {error}"))
}

/// Измеряемая операция.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Открытие существующего хранилища и `verify_integrity` всех записей.
    VerifyIntegrity,
    /// Штатный пакетный API: одна блокировка и одна проверка манифеста.
    ReadBatch,
    /// Те же запросы по одному вызову: блокировка и проверка манифеста на каждый.
    ReadSingle,
    /// Публикация новых идентичностей через `ingest_verified`.
    Ingest,
    /// Та же публикация, распределённая между независимыми писателями.
    IngestContention { writers: u32 },
    /// Открытие пакета и загрузка состояния с полной сверкой всех blob.
    BatchReopen,
    /// Загрузка и сохранение состояния, с изменением или без.
    BatchCheckpoint { modified: bool },
}

impl Operation {
    /// Разбор имени операции и её параметров командной строки.
    pub fn parse(
        name: &str,
        writers: Option<u32>,
        change: Option<&str>,
    ) -> Result<Self, BenchError> {
        let extra = |what: &str| {
            Err(BenchError::usage(format!(
                "параметр {what} не относится к операции {name}"
            )))
        };
        let operation = match name {
            "verify-integrity" => Self::VerifyIntegrity,
            "read-batch" => Self::ReadBatch,
            "read-single" => Self::ReadSingle,
            "ingest" => Self::Ingest,
            "ingest-contention" => {
                let writers = writers.ok_or_else(|| {
                    BenchError::usage("ingest-contention требует --writers".into())
                })?;
                if writers == 0 || writers > MAX_WRITERS {
                    return Err(BenchError::usage("--writers — от 1 до 64".into()));
                }
                return match change {
                    Some(_) => extra("--change"),
                    None => Ok(Self::IngestContention { writers }),
                };
            }
            "batch-reopen" => Self::BatchReopen,
            "batch-checkpoint" => {
                let modified = match change {
                    Some("unchanged") => false,
                    Some("modified") => true,
                    Some(other) => {
                        return Err(BenchError::usage(format!(
                            "--change — unchanged или modified, а не {other}"
                        )));
                    }
                    None => {
                        return Err(BenchError::usage(
                            "batch-checkpoint требует --change".into(),
                        ));
                    }
                };
                return match writers {
                    Some(_) => extra("--writers"),
                    None => Ok(Self::BatchCheckpoint { modified }),
                };
            }
            other => {
                return Err(BenchError::usage(format!("неизвестная операция {other}")));
            }
        };
        if writers.is_some() {
            return extra("--writers");
        }
        if change.is_some() {
            return extra("--change");
        }
        Ok(operation)
    }

    /// Изменяет ли операция хранилище: тогда она работает на рабочей копии.
    pub fn mutates(self) -> bool {
        matches!(
            self,
            Self::Ingest | Self::IngestContention { .. } | Self::BatchCheckpoint { .. }
        )
    }
}

/// Семантический валидатор нагрузки: полное чтение и декодирование.
#[derive(Debug, Clone, Copy)]
pub struct BenchValidator {
    pub image_side: u32,
}

impl BenchValidator {
    pub fn validator_identity() -> ValidatorIdentity {
        ValidatorIdentity::new("asset-store-bench", GENERATOR_VERSION.to_string())
            .expect("идентичность валидатора корректна")
    }
}

impl SemanticValidator for BenchValidator {
    fn identity(&self) -> ValidatorIdentity {
        Self::validator_identity()
    }

    fn validate(
        &self,
        _asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;
        let image = image::load_from_memory(&contents)
            .map_err(|error| ValidatorFailure::new("decode_failed", error.to_string()))?;
        let status = if image.width() == self.image_side && image.height() == self.image_side {
            SemanticStatus::Verified
        } else {
            SemanticStatus::Rejected
        };
        Ok(SemanticDecision::new(
            status,
            vec![ValidationEvidence {
                kind: "bench-decode".to_owned(),
                summary: format!(
                    "декодировано {}x{} из {} байт",
                    image.width(),
                    image.height(),
                    contents.len()
                ),
                details: None,
            }],
        ))
    }
}

/// Запись пакетного состояния нагрузки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchBatchEntry {
    pub key: String,
    pub status: String,
    pub note: String,
    pub blob: Option<RuntimeBlobRef>,
}

/// Пакетное состояние нагрузки: нейтральная предметная структура поверх
/// штатного контракта [`RuntimeBatchState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchBatchState {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub entries: Vec<BenchBatchEntry>,
}

impl RuntimeBatchState for BenchBatchState {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != BATCH_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "неизвестная версия состояния нагрузки",
            ));
        }
        let mut keys = std::collections::BTreeSet::new();
        for entry in &self.entries {
            if !keys.insert(entry.key.as_str()) {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "повторный ключ записи пакета",
                ));
            }
        }
        Ok(())
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.entries
            .iter()
            .filter_map(|entry| entry.blob.clone())
            .collect()
    }
}

/// Объём нагрузки: вывод пробы входа сценария.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkloadDescription {
    pub generator: u32,
    pub domain: SpecDomain,
    pub assets: u32,
    pub records: u64,
    pub corpus_bytes: u64,
    pub reads: u32,
    pub read_bytes: u64,
    pub ingest: u32,
    pub ingest_bytes: u64,
    pub batch_entries: u32,
    pub batch_blobs: u32,
    pub batch_blob_bytes: u64,
    /// SHA-256 упорядоченных хешей всех порождаемых байтов: контроль того,
    /// что генератор той же версии дал тот же корпус.
    pub content_sha256: String,
}

/// Объём и контрольная сумма нагрузки без записи на диск.
pub fn describe(spec: &WorkloadSpec) -> WorkloadDescription {
    let mut digest = String::new();
    let mut corpus_bytes = 0u64;
    let mut read_bytes = 0u64;
    for (position, (index, format)) in spec.corpus().into_iter().enumerate() {
        let bytes = spec.image(index, format);
        corpus_bytes += bytes.len() as u64;
        if position < spec.reads as usize {
            read_bytes += bytes.len() as u64;
        }
        digest.push_str(&sha256_hex(&bytes));
    }
    let mut ingest_bytes = 0u64;
    for index in spec.assets..spec.assets + spec.ingest {
        let bytes = spec.image(index, DetectedFormat::Gif);
        ingest_bytes += bytes.len() as u64;
        digest.push_str(&sha256_hex(&bytes));
    }
    for index in 0..spec.batch_blobs {
        digest.push_str(&sha256_hex(spec.blob(index)));
    }
    WorkloadDescription {
        generator: spec.generator,
        domain: spec.domain,
        assets: spec.assets,
        records: spec.records(),
        corpus_bytes,
        reads: spec.reads,
        read_bytes,
        ingest: spec.ingest,
        ingest_bytes,
        batch_entries: spec.batch_entries,
        batch_blobs: spec.batch_blobs,
        batch_blob_bytes: u64::from(spec.batch_blobs) * u64::from(spec.blob_bytes),
        content_sha256: sha256_hex(digest.as_bytes()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    tool: String,
    generator: u32,
    spec_sha256: String,
    /// SHA-256 исполняемого файла, построившего эталон: эталон другой сборки
    /// кода не переиспользуется.
    executable_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedRead {
    identity: AssetIdentity,
    format: DetectedFormat,
    sha256: String,
    byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedIngest {
    identity: AssetIdentity,
    file: String,
    sha256: String,
}

/// Ожидаемое состояние эталона: записывается при [`setup`] и читается
/// операциями и проверками; путь хранилища в нём не хранится.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    records: u64,
    reads: Vec<ExpectedRead>,
    ingest: Vec<ExpectedIngest>,
    store_digest: String,
    batch_revision: u64,
}

/// Итог операции или проверки — одна строка JSON в stdout.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub operation: String,
    pub items: u64,
    pub bytes: u64,
    /// Для setup: эталон прежней серии переиспользован после сверки.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
}

impl Summary {
    fn new(operation: &str, items: u64, bytes: u64) -> Self {
        Self {
            operation: operation.into(),
            items,
            bytes,
            reused: false,
        }
    }
}

fn executable_sha256() -> Result<String, BenchError> {
    let bytes = fs::read("/proc/self/exe")
        .map_err(|error| io_failed("исполняемый файл не читается", error))?;
    Ok(sha256_hex(bytes))
}

fn spec_sha256(path: &Path) -> Result<String, BenchError> {
    let bytes = fs::read(path)
        .map_err(|error| BenchError::usage(format!("спецификация не читается: {error}")))?;
    Ok(sha256_hex(bytes))
}

fn read_marker(workdir: &Path) -> Result<Option<Marker>, BenchError> {
    let path = workdir.join(MARKER_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_failed("маркер рабочего каталога не читается", error)),
    };
    let marker: Marker = serde_json::from_str(&text).map_err(|error| {
        BenchError::failed(format!("маркер рабочего каталога повреждён: {error}"))
    })?;
    if marker.tool != MARKER_TOOL {
        return Err(BenchError::failed("маркер принадлежит другой программе"));
    }
    Ok(Some(marker))
}

/// Удаляет рабочий каталог, только если он помечен этим модулем; отсутствующий
/// каталог — успех.
fn remove_owned(workdir: &Path) -> Result<(), BenchError> {
    let meta = match fs::symlink_metadata(workdir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_failed("рабочий каталог недоступен", error)),
    };
    if !meta.is_dir() {
        return Err(BenchError::usage(
            "рабочий путь существует и не является каталогом".into(),
        ));
    }
    let empty = fs::read_dir(workdir)
        .map_err(|error| io_failed("рабочий каталог не читается", error))?
        .next()
        .is_none();
    if !empty && read_marker(workdir)?.is_none() {
        return Err(BenchError::usage(
            "рабочий каталог не пуст и не помечен asset-store-bench; удалять чужие данные нельзя"
                .into(),
        ));
    }
    fs::remove_dir_all(workdir).map_err(|error| io_failed("рабочий каталог не удаляется", error))
}

/// Сверяет маркер эталона. Хеш исполняемого файла читает весь бинарник, поэтому в
/// измеряемой `run` он не проверяется: идентичность сборки доказывают `setup`,
/// `prepare` и `verify` вне измеряемого интервала.
fn checked_marker(
    workdir: &Path,
    spec_path: &Path,
    check_executable: bool,
) -> Result<(), BenchError> {
    let marker = read_marker(workdir)?
        .ok_or_else(|| BenchError::failed("эталон не подготовлен: сначала setup"))?;
    if marker.spec_sha256 != spec_sha256(spec_path)?
        || marker.generator != GENERATOR_VERSION
        || (check_executable && marker.executable_sha256 != executable_sha256()?)
    {
        return Err(BenchError::failed(
            "эталон построен для другой спецификации, версии генератора или сборки",
        ));
    }
    Ok(())
}

/// Пригоден ли эталон прежней серии: тот же маркер и неизменное дерево.
fn reusable_template(workdir: &Path, spec_path: &Path) -> bool {
    if checked_marker(workdir, spec_path, true).is_err() {
        return false;
    }
    let template = workdir.join(TEMPLATE_DIR);
    let Ok(expected) = read_expected(workdir) else {
        return false;
    };
    let incoming_ok = expected.ingest.iter().all(|item| {
        fs::read(template.join(INCOMING_DIR).join(&item.file))
            .is_ok_and(|bytes| sha256_hex(bytes) == item.sha256)
    });
    incoming_ok
        && tree_digest(&template.join(STORE_DIR))
            .is_ok_and(|digest| digest == expected.store_digest)
}

fn read_expected(workdir: &Path) -> Result<Expected, BenchError> {
    let path = workdir.join(TEMPLATE_DIR).join(EXPECTED_FILE);
    let bytes =
        fs::read(path).map_err(|error| io_failed("ожидаемое состояние не читается", error))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| BenchError::failed(format!("ожидаемое состояние повреждено: {error}")))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), BenchError> {
    fs::write(path, bytes).map_err(|error| io_failed("файл не записывается", error))?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_failed("файл не синхронизируется", error))
}

fn sync_dir(path: &Path) -> Result<(), BenchError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| io_failed("каталог не синхронизируется", error))
}

/// SHA-256 дерева: относительные пути, вид записи и содержимое файлов.
/// Сравнивается только с самим собой, поэтому формат внутренний.
fn tree_digest(root: &Path) -> Result<String, BenchError> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let dir = root.join(&relative);
        for entry in fs::read_dir(&dir).map_err(|error| io_failed("дерево не читается", error))?
        {
            let entry = entry.map_err(|error| io_failed("дерево не читается", error))?;
            let path = relative.join(entry.file_name());
            let kind = entry
                .file_type()
                .map_err(|error| io_failed("дерево не читается", error))?;
            let key = path.to_string_lossy().into_owned();
            if kind.is_dir() {
                entries.insert(key, "d".to_owned());
                stack.push(path);
            } else if kind.is_file() {
                let bytes = fs::read(root.join(&path))
                    .map_err(|error| io_failed("файл дерева не читается", error))?;
                entries.insert(key, format!("f{}", sha256_hex(bytes)));
            } else {
                return Err(BenchError::failed(format!(
                    "в дереве хранилища неожиданная запись {key}"
                )));
            }
        }
    }
    let mut text = String::new();
    for (path, value) in entries {
        text.push_str(&path);
        text.push('\0');
        text.push_str(&value);
        text.push('\n');
    }
    Ok(sha256_hex(text.as_bytes()))
}

/// Копия дерева с `fsync` каждого файла и каталога: рабочая копия стартует
/// надёжно записанной, и грязные страницы подготовки не сбрасываются во
/// время измеряемой операции.
fn copy_tree_synced(from: &Path, to: &Path) -> Result<(), BenchError> {
    fs::create_dir(to).map_err(|error| io_failed("каталог копии не создаётся", error))?;
    for entry in fs::read_dir(from).map_err(|error| io_failed("эталон не читается", error))?
    {
        let entry = entry.map_err(|error| io_failed("эталон не читается", error))?;
        let kind = entry
            .file_type()
            .map_err(|error| io_failed("эталон не читается", error))?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree_synced(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)
                .map_err(|error| io_failed("файл эталона не копируется", error))?;
            File::open(&target)
                .and_then(|file| file.sync_all())
                .map_err(|error| io_failed("копия не синхронизируется", error))?;
        } else {
            return Err(BenchError::failed("в эталоне неожиданная запись"));
        }
    }
    sync_dir(to)
}

fn ingest_request(
    spec: &WorkloadSpec,
    identity: AssetIdentity,
    bytes: Vec<u8>,
    source_name: String,
) -> VerifiedIngestRequest {
    let domain_metadata = spec.domain_metadata(&identity);
    VerifiedIngestRequest {
        identity,
        bytes,
        provenance: Provenance {
            source_kind: PROVENANCE_KIND.into(),
            source_name,
        },
        domain_metadata,
        replace_expected_sha256: None,
    }
}

fn open_store(spec: &WorkloadSpec, root: &Path, create: bool) -> Result<AssetStore, BenchError> {
    let options = StoreOptions::new(root);
    let store = match (spec.domain, create) {
        (SpecDomain::Kanji, true) => AssetStore::open_with_policy(options, KanjiDomainPolicy),
        (SpecDomain::Kanji, false) => {
            AssetStore::open_existing_with_policy(options, KanjiDomainPolicy)
        }
        (SpecDomain::Generic, true) => AssetStore::open_with_policy(options, GenericDomainPolicy),
        (SpecDomain::Generic, false) => {
            AssetStore::open_existing_with_policy(options, GenericDomainPolicy)
        }
    }?;
    Ok(store)
}

fn publish(
    store: &AssetStore,
    spec: &WorkloadSpec,
    request: VerifiedIngestRequest,
) -> Result<AssetRecord, BenchError> {
    let validator = BenchValidator {
        image_side: spec.image_side,
    };
    let outcome = store.ingest_verified(request, &validator)?;
    match (outcome.status, outcome.asset) {
        (SemanticStatus::Verified, Some(asset)) if outcome.changed => Ok(asset),
        (status, _) => Err(BenchError::failed(format!(
            "публикация не дала новой проверенной записи: статус {}, изменено {}",
            status.as_str(),
            outcome.changed
        ))),
    }
}

/// Строит эталон серии: корпус, входящие файлы, пакетное состояние и
/// ожидаемое состояние. Прежний помеченный каталог удаляется целиком.
pub fn setup(spec_path: &Path, workdir: &Path) -> Result<Summary, BenchError> {
    let spec = WorkloadSpec::load(spec_path)?;
    if reusable_template(workdir, spec_path) {
        remove_run_copy(workdir)?;
        let mut summary = Summary::new("setup", spec.records(), describe(&spec).corpus_bytes);
        summary.reused = true;
        return Ok(summary);
    }
    remove_owned(workdir)?;
    fs::create_dir_all(workdir)
        .map_err(|error| io_failed("рабочий каталог не создаётся", error))?;
    let marker = Marker {
        tool: MARKER_TOOL.into(),
        generator: GENERATOR_VERSION,
        spec_sha256: spec_sha256(spec_path)?,
        executable_sha256: executable_sha256()?,
    };
    // Маркер пишется первым: даже прерванный setup оставляет каталог, который
    // clean признаёт своим.
    write_synced(
        &workdir.join(MARKER_FILE),
        &serde_json::to_vec_pretty(&marker).expect("маркер сериализуется"),
    )?;
    let template = workdir.join(TEMPLATE_DIR);
    let store_root = template.join(STORE_DIR);
    let incoming = template.join(INCOMING_DIR);
    fs::create_dir_all(&incoming).map_err(|error| io_failed("эталон не создаётся", error))?;

    let store = open_store(&spec, &store_root, true)?;
    let mut reads = Vec::new();
    for (position, (index, format)) in spec.corpus().into_iter().enumerate() {
        let identity = spec.identity(index);
        let bytes = spec.image(index, format);
        let name = format!("seed-{index:05}.{}", extension(format));
        let asset = publish(
            &store,
            &spec,
            ingest_request(&spec, identity.clone(), bytes, name),
        )?;
        if position < spec.reads as usize {
            reads.push(ExpectedRead {
                identity,
                format,
                sha256: asset.sha256,
                byte_length: asset.byte_length,
            });
        }
    }
    let mut ingest = Vec::new();
    for index in spec.assets..spec.assets + spec.ingest {
        let bytes = spec.image(index, DetectedFormat::Gif);
        let file = format!("incoming-{index:05}.gif");
        write_synced(&incoming.join(&file), &bytes)?;
        ingest.push(ExpectedIngest {
            identity: spec.identity(index),
            file,
            sha256: sha256_hex(&bytes),
        });
    }
    sync_dir(&incoming)?;
    drop(store);

    let mut runtime = SafeBatchRuntime::open(&store_root, BATCH_ID)?;
    let mut entries = Vec::with_capacity(spec.batch_entries as usize);
    for index in 0..spec.batch_entries {
        let blob = if index < spec.batch_blobs {
            Some(runtime.persist_blob(&spec.blob(index), BLOB_EXTENSION)?)
        } else {
            None
        };
        entries.push(BenchBatchEntry {
            key: format!("entry-{index:06}"),
            status: "pending".into(),
            note: note_text(spec.seed, index),
            blob,
        });
    }
    let state = BenchBatchState {
        schema_version: BATCH_SCHEMA_VERSION,
        batch_id: BATCH_ID.into(),
        revision: 1,
        entries,
    };
    runtime.save(&state)?;
    drop(runtime);

    let expected = Expected {
        records: spec.records(),
        reads,
        ingest,
        store_digest: tree_digest(&store_root)?,
        batch_revision: state.revision,
    };
    write_synced(
        &template.join(EXPECTED_FILE),
        &serde_json::to_vec(&expected).expect("ожидаемое состояние сериализуется"),
    )?;
    sync_dir(&template)?;
    sync_dir(workdir)?;
    Ok(Summary::new(
        "setup",
        spec.records(),
        describe(&spec).corpus_bytes,
    ))
}

fn remove_run_copy(workdir: &Path) -> Result<(), BenchError> {
    match fs::remove_dir_all(workdir.join(RUN_DIR)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_failed("прежняя рабочая копия не удаляется", error)),
    }
}

/// Восстанавливает рабочую копию хранилища из эталона.
pub fn prepare(spec_path: &Path, workdir: &Path) -> Result<Summary, BenchError> {
    checked_marker(workdir, spec_path, true)?;
    let run = workdir.join(RUN_DIR);
    remove_run_copy(workdir)?;
    fs::create_dir(&run).map_err(|error| io_failed("рабочая копия не создаётся", error))?;
    copy_tree_synced(
        &workdir.join(TEMPLATE_DIR).join(STORE_DIR),
        &run.join(STORE_DIR),
    )?;
    sync_dir(&run)?;
    sync_dir(workdir)?;
    Ok(Summary::new("prepare", 0, 0))
}

/// Хранилище, над которым работает операция: эталон для только читающих
/// операций, рабочая копия для изменяющих.
fn operation_store(workdir: &Path, operation: Operation) -> PathBuf {
    if operation.mutates() {
        workdir.join(RUN_DIR).join(STORE_DIR)
    } else {
        workdir.join(TEMPLATE_DIR).join(STORE_DIR)
    }
}

fn read_requests(expected: &Expected) -> Vec<(AssetIdentity, DetectedFormat)> {
    expected
        .reads
        .iter()
        .map(|read| (read.identity.clone(), read.format))
        .collect()
}

fn check_read(
    expected: &ExpectedRead,
    read: &crate::VerifiedAssetBytes,
) -> Result<u64, BenchError> {
    if read.record.identity != expected.identity
        || read.record.format != expected.format
        || read.record.sha256 != expected.sha256
        || read.bytes.len() as u64 != expected.byte_length
    {
        return Err(BenchError::failed(format!(
            "прочитана не та запись: {}",
            expected.identity
        )));
    }
    Ok(read.bytes.len() as u64)
}

fn ingest_share(
    spec: &WorkloadSpec,
    store: &AssetStore,
    incoming: &Path,
    items: &[ExpectedIngest],
) -> Result<u64, BenchError> {
    let mut bytes_total = 0;
    for item in items {
        let bytes = fs::read(incoming.join(&item.file))
            .map_err(|error| io_failed("входящий файл не читается", error))?;
        bytes_total += bytes.len() as u64;
        let asset = publish(
            store,
            spec,
            ingest_request(spec, item.identity.clone(), bytes, item.file.clone()),
        )?;
        if asset.sha256 != item.sha256 {
            return Err(BenchError::failed("опубликованы не те байты"));
        }
    }
    Ok(bytes_total)
}

fn load_batch(runtime: &mut SafeBatchRuntime) -> Result<BenchBatchState, BenchError> {
    runtime
        .load::<BenchBatchState>()?
        .ok_or_else(|| BenchError::failed("пакетное состояние отсутствует"))
}

/// Измеряемая операция.
pub fn run(spec_path: &Path, workdir: &Path, operation: Operation) -> Result<Summary, BenchError> {
    let spec = WorkloadSpec::load(spec_path)?;
    checked_marker(workdir, spec_path, false)?;
    let expected = read_expected(workdir)?;
    let root = operation_store(workdir, operation);
    if operation.mutates() && !root.is_dir() {
        return Err(BenchError::failed(
            "рабочая копия не подготовлена: сначала prepare",
        ));
    }
    let policy = spec.policy();
    let validator = BenchValidator::validator_identity();
    let (name, items, bytes) = match operation {
        Operation::VerifyIntegrity => {
            let store = open_store(&spec, &root, false)?;
            let records = store.verify_integrity()?;
            if records.len() as u64 != expected.records {
                return Err(BenchError::failed("число записей отличается от эталона"));
            }
            let bytes = records.iter().map(|record| record.byte_length).sum();
            ("verify-integrity", records.len() as u64, bytes)
        }
        Operation::ReadBatch => {
            let requests = read_requests(&expected);
            let mut bytes = 0;
            AssetStore::visit_verified_variants_with_policy(
                &root,
                &requests,
                &validator,
                policy.as_ref(),
                |index, read| {
                    let read = read?;
                    bytes += check_read(&expected.reads[index], &read).map_err(|error| {
                        AssetError::new(ErrorCode::IntegrityMismatch, error.message)
                    })?;
                    Ok(())
                },
            )?;
            ("read-batch", requests.len() as u64, bytes)
        }
        Operation::ReadSingle => {
            let mut bytes = 0;
            for read in &expected.reads {
                let found = AssetStore::read_verified_variants_with_policy(
                    &root,
                    &[(read.identity.clone(), read.format)],
                    &validator,
                    policy.as_ref(),
                )?;
                let [found] = found.as_slice() else {
                    return Err(BenchError::failed(
                        "одиночное чтение вернуло не одну запись",
                    ));
                };
                bytes += check_read(read, found)?;
            }
            ("read-single", expected.reads.len() as u64, bytes)
        }
        Operation::Ingest => {
            let store = open_store(&spec, &root, false)?;
            let incoming = workdir.join(TEMPLATE_DIR).join(INCOMING_DIR);
            let bytes = ingest_share(&spec, &store, &incoming, &expected.ingest)?;
            ("ingest", expected.ingest.len() as u64, bytes)
        }
        Operation::IngestContention { writers } => {
            let incoming = workdir.join(TEMPLATE_DIR).join(INCOMING_DIR);
            let barrier = Arc::new(Barrier::new(writers as usize));
            let results: Vec<Result<u64, BenchError>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..writers)
                    .map(|writer| {
                        let share: Vec<ExpectedIngest> = expected
                            .ingest
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| *index as u32 % writers == writer)
                            .map(|(_, item)| item.clone())
                            .collect();
                        let barrier = Arc::clone(&barrier);
                        let (spec, root, incoming) = (&spec, &root, &incoming);
                        scope.spawn(move || {
                            // Каждый писатель — независимый дескриптор хранилища:
                            // исключение обеспечивает штатная блокировка flock,
                            // как между процессами.
                            let store = open_store(spec, root, false);
                            barrier.wait();
                            ingest_share(spec, &store?, incoming, &share)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            Err(BenchError::failed("писатель аварийно завершился"))
                        })
                    })
                    .collect()
            });
            let mut bytes = 0;
            for result in results {
                bytes += result?;
            }
            ("ingest-contention", expected.ingest.len() as u64, bytes)
        }
        Operation::BatchReopen => {
            let mut runtime = SafeBatchRuntime::open(&root, BATCH_ID)?;
            let state = load_batch(&mut runtime)?;
            if state.entries.len() != spec.batch_entries as usize {
                return Err(BenchError::failed(
                    "число записей пакета отличается от эталона",
                ));
            }
            (
                "batch-reopen",
                state.entries.len() as u64,
                u64::from(spec.batch_blobs) * u64::from(spec.blob_bytes),
            )
        }
        Operation::BatchCheckpoint { modified } => {
            let mut runtime = SafeBatchRuntime::open(&root, BATCH_ID)?;
            let mut state = load_batch(&mut runtime)?;
            if modified {
                let blob = runtime.persist_blob(&checkpoint_blob(&spec), BLOB_EXTENSION)?;
                let entry = state
                    .entries
                    .first_mut()
                    .ok_or_else(|| BenchError::failed("пакет пуст"))?;
                entry.status = "checkpointed".into();
                entry.blob = Some(blob);
                state.revision += 1;
            }
            runtime.save(&state)?;
            (
                if modified {
                    "batch-checkpoint-modified"
                } else {
                    "batch-checkpoint-unchanged"
                },
                state.entries.len() as u64,
                0,
            )
        }
    };
    Ok(Summary::new(name, items, bytes))
}

/// Постусловия операции; вызывается вне измеряемого интервала.
pub fn verify(
    spec_path: &Path,
    workdir: &Path,
    operation: Operation,
) -> Result<Summary, BenchError> {
    let spec = WorkloadSpec::load(spec_path)?;
    checked_marker(workdir, spec_path, true)?;
    let expected = read_expected(workdir)?;
    let root = operation_store(workdir, operation);
    if !operation.mutates() {
        // Только читающая операция не изменила ни байта эталона.
        if tree_digest(&root)? != expected.store_digest {
            return Err(BenchError::failed(
                "только читающая операция изменила эталонное хранилище",
            ));
        }
        return Ok(Summary::new("verify-read-only", expected.records, 0));
    }
    match operation {
        Operation::Ingest | Operation::IngestContention { .. } => {
            let store = open_store(&spec, &root, false)?;
            let records = store.verify_integrity()?;
            let total = expected.records + expected.ingest.len() as u64;
            if records.len() as u64 != total {
                return Err(BenchError::failed(format!(
                    "после публикации записей {}, ожидалось {total}",
                    records.len()
                )));
            }
            let journal = store.journal_check()?;
            if !journal.is_clean() || !journal.open_intents.is_empty() {
                return Err(BenchError::failed("журнал после публикации не чист"));
            }
            drop(store);
            let policy = spec.policy();
            let validator = BenchValidator::validator_identity();
            // Прежние записи не изменились, новые читаются доверенными байтами.
            let mut requests = read_requests(&expected);
            requests.extend(
                expected
                    .ingest
                    .iter()
                    .map(|item| (item.identity.clone(), DetectedFormat::Gif)),
            );
            let reads = AssetStore::read_verified_variants_with_policy(
                &root,
                &requests,
                &validator,
                policy.as_ref(),
            )?;
            for (read, want) in reads.iter().zip(&expected.reads) {
                check_read(want, read)?;
            }
            for (read, want) in reads[expected.reads.len()..].iter().zip(&expected.ingest) {
                if read.record.sha256 != want.sha256 || sha256_hex(&read.bytes) != want.sha256 {
                    return Err(BenchError::failed(
                        "опубликованный ресурс читается не теми байтами",
                    ));
                }
            }
            if fs::read_dir(root.join(".tmp"))
                .map_err(|error| io_failed("каталог staging не читается", error))?
                .next()
                .is_some()
            {
                return Err(BenchError::failed(
                    "после публикации в staging остались файлы",
                ));
            }
            Ok(Summary::new("verify-ingest", records.len() as u64, 0))
        }
        Operation::BatchCheckpoint { modified } => {
            let mut runtime = SafeBatchRuntime::open(&root, BATCH_ID)?;
            let state = load_batch(&mut runtime)?;
            let revision = expected.batch_revision + u64::from(modified);
            if state.revision != revision || state.entries.len() != spec.batch_entries as usize {
                return Err(BenchError::failed("ревизия или размер пакета не те"));
            }
            let first = state
                .entries
                .first()
                .ok_or_else(|| BenchError::failed("пакет пуст"))?;
            if modified {
                let blob = first
                    .blob
                    .as_ref()
                    .ok_or_else(|| BenchError::failed("новый blob не записан"))?;
                if runtime.read_blob(blob)? != checkpoint_blob(&spec) {
                    return Err(BenchError::failed("новый blob читается не теми байтами"));
                }
            } else if first.status != "pending" {
                return Err(BenchError::failed(
                    "сохранение без изменений изменило запись",
                ));
            }
            Ok(Summary::new(
                "verify-batch-checkpoint",
                state.entries.len() as u64,
                0,
            ))
        }
        _ => unreachable!("только читающие операции обработаны выше"),
    }
}

/// Удаляет помеченный рабочий каталог; чужой каталог не трогает. С
/// `keep_template` удаляется только рабочая копия: эталон остаётся для
/// следующей серии той же сборки и спецификации.
pub fn clean(workdir: &Path, keep_template: bool) -> Result<Summary, BenchError> {
    if keep_template {
        if fs::symlink_metadata(workdir).is_err() {
            return Ok(Summary::new("clean", 0, 0));
        }
        if read_marker(workdir)?.is_none() {
            return Err(BenchError::usage(
                "рабочий каталог не помечен asset-store-bench; удалять чужие данные нельзя".into(),
            ));
        }
        remove_run_copy(workdir)?;
    } else {
        remove_owned(workdir)?;
    }
    Ok(Summary::new("clean", 0, 0))
}

fn checkpoint_blob(spec: &WorkloadSpec) -> Vec<u8> {
    spec.blob(u32::MAX)
}

fn note_text(seed: u64, index: u32) -> String {
    let mut state = mix(seed ^ 0x0E07E, u64::from(index));
    (0..64)
        .map(|_| {
            state = next(state);
            char::from(b'a' + (state >> 59) as u8 % 26)
        })
        .collect()
}

fn extension(format: DetectedFormat) -> &'static str {
    match format {
        DetectedFormat::Png => "png",
        _ => "gif",
    }
}

fn mix(seed: u64, index: u64) -> u64 {
    let mut state = seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for _ in 0..4 {
        state = next(state);
    }
    state
}

/// xorshift64*: детерминированный генератор без внешних зависимостей.
fn next(mut state: u64) -> u64 {
    if state == 0 {
        state = 0x2545_F491_4F6C_DD1D;
    }
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Небольшое настоящее изображение: блоки 4×4 псевдослучайных оттенков
/// серого, чтобы сжатие давало реалистичный, а не вырожденный размер.
fn render_image(seed: u64, index: u32, side: u32, format: DetectedFormat) -> Vec<u8> {
    let mut state = mix(seed, u64::from(index));
    let blocks = side.div_ceil(4);
    let shades: Vec<u8> = (0..blocks * blocks)
        .map(|_| {
            state = next(state);
            (state >> 58) as u8 * 4
        })
        .collect();
    let image = image::GrayImage::from_fn(side, side, |x, y| {
        image::Luma([shades[((y / 4) * blocks + x / 4) as usize]])
    });
    let mut bytes = Cursor::new(Vec::new());
    let image = image::DynamicImage::ImageLuma8(image);
    // Кодировщик GIF принимает только RGB(A).
    match format {
        DetectedFormat::Png => image.write_to(&mut bytes, image::ImageFormat::Png),
        _ => image::DynamicImage::ImageRgba8(image.to_rgba8())
            .write_to(&mut bytes, image::ImageFormat::Gif),
    }
    .expect("кодирование изображения в память не отказывает");
    bytes.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> WorkloadSpec {
        WorkloadSpec {
            generator: GENERATOR_VERSION,
            domain: SpecDomain::Kanji,
            seed: 7,
            assets: 9,
            png_every: 4,
            image_side: 16,
            reads: 10,
            ingest: 3,
            batch_entries: 5,
            batch_blobs: 2,
            blob_bytes: 64,
        }
    }

    #[test]
    fn generator_is_deterministic_and_seed_sensitive() {
        let base = spec();
        assert_eq!(describe(&base), describe(&base));
        let other = WorkloadSpec {
            seed: 8,
            ..base.clone()
        };
        let (a, b) = (describe(&base), describe(&other));
        assert_ne!(a.content_sha256, b.content_sha256);
        assert_eq!(
            (a.records, a.reads, a.ingest),
            (b.records, b.reads, b.ingest)
        );
        assert_eq!(a.records, 9 + 3);
    }

    #[test]
    fn generated_images_are_real_and_detectable() {
        let base = spec();
        for format in [DetectedFormat::Gif, DetectedFormat::Png] {
            let bytes = base.image(1, format);
            assert_eq!(DetectedFormat::from_signature(&bytes), format);
            let image = image::load_from_memory(&bytes).expect("изображение декодируется");
            assert_eq!((image.width(), image.height()), (16, 16));
        }
    }

    #[test]
    fn spec_limits_are_enforced() {
        let invalid = [
            WorkloadSpec {
                generator: GENERATOR_VERSION + 1,
                ..spec()
            },
            WorkloadSpec {
                assets: 0,
                ..spec()
            },
            WorkloadSpec {
                reads: 13,
                ..spec()
            },
            WorkloadSpec {
                batch_blobs: 6,
                ..spec()
            },
            WorkloadSpec {
                domain: SpecDomain::Generic,
                ..spec()
            },
            WorkloadSpec {
                assets: 20_000,
                ingest: 1_000,
                ..spec()
            },
        ];
        for spec in invalid {
            assert!(spec.validate().unwrap_err().usage, "{spec:?}");
        }
        spec().validate().unwrap();
    }

    #[test]
    fn operation_arguments_are_strict() {
        assert_eq!(
            Operation::parse("ingest-contention", Some(4), None).unwrap(),
            Operation::IngestContention { writers: 4 }
        );
        assert!(Operation::parse("ingest-contention", None, None).is_err());
        assert!(Operation::parse("ingest", Some(2), None).is_err());
        assert!(Operation::parse("batch-checkpoint", None, Some("maybe")).is_err());
        assert!(Operation::parse("read-batch", None, Some("modified")).is_err());
        assert!(Operation::parse("unknown", None, None).is_err());
    }
}
