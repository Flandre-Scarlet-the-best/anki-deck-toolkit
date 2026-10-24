//! Возобновляемый массовый конвейер локального рендера PNG кандзи.
//!
//! Этапы для каждой кодовой точки независимы и сохраняются атомарно в
//! `items/U+XXXX.json` рабочего каталога: рендер → семантическая проверка
//! `KanjiImageValidator` → слепой OCR → публикация через
//! [`AssetStore::ingest_verified`]. Повтор продолжает с первого
//! незавершённого этапа и не создаёт дубликатов. OCR — независимая
//! дополнительная проверка, а не источник доверия: публикуются только PNG,
//! прошедшие и штатный валидатор, и совпадение OCR с ожидаемым символом.
//!
//! Конвейер никогда не заменяет существующую PNG-запись символа: исторический
//! PNG остаётся, а новый рендер служит контрольной генерацией для сравнения.
//! Запись GIF того же символа — независимый слот и не затрагивается.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::domain::KanjiDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::inference::{InferenceProvenance, InferenceSession};
use crate::inference_config::TaskId;
use crate::kanji_ocr::{OcrOptions, TRANSFORM_VERSION, contract, interpret};
use crate::kanji_validator::{KanjiImageValidator, prepare_ocr_image};
use crate::local_render::{
    GeometryProfile, LOCAL_RENDER_ALGORITHM, LOCAL_RENDER_PROVIDER_VERSION,
    LOCAL_RENDER_SOURCE_KIND, LocalFont, RenderEvidence, RenderSettings, compare_images,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanDecision, LifecycleState, Provenance,
    SemanticStatus, ValidationEvidence, ValidatorIdentity,
};
use crate::store::{AssetStore, HumanAttestationRequest, IngestRequest, VerifiedIngestRequest};
use crate::validation::SemanticValidator;

const ITEM_SCHEMA: u32 = 1;
const RUN_SCHEMA: u32 = 1;
/// Имя файла источника в `Provenance`; путь рабочего каталога не сохраняется.
const SOURCE_NAME: &str = "local-render.png";

/// Откуда символ попал в очередь.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetOrigin {
    /// В хранилище есть доверенный GIF символа.
    CorpusGif,
    /// В хранилище уже есть PNG символа: генерация контрольная.
    ExistingPng,
    /// Символ передан явно.
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderState {
    pub sha256: String,
    pub raw_sha256: String,
    pub byte_length: u64,
    pub file: String,
    pub raw_file: String,
    pub evidence_file: String,
    pub glyph_area: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageError {
    pub code: String,
    pub message: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationState {
    pub sha256: String,
    pub status: SemanticStatus,
    pub validator: ValidatorIdentity,
    pub evidence: Vec<ValidationEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrState {
    pub sha256: String,
    pub observed: Option<String>,
    pub matches_expected: bool,
    pub failure: Option<String>,
    pub prepared_sha256: Option<String>,
    pub model: String,
    pub provenance_sha256: String,
    pub elapsed_ms: Option<u64>,
    /// Подряд идущие технические отказы OCR этих байтов; успешный ответ обнуляет счёт.
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationState {
    pub sha256: String,
    pub outcome: String,
    pub message: Option<String>,
    /// Подряд идущие `publish_error` этих байтов.
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlState {
    /// SHA исторического PNG хранилища, с которым сравнивался рендер.
    pub original_sha256: String,
    pub original_provenance: String,
    pub reference_file: String,
    pub metrics: crate::local_render::ImageComparison,
}

/// Контроль этих байтов обоснованно не выполнен: у оригинала нет доверенного
/// решения или он сам локальный рендер. Повтор — по `--retry-failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlSkip {
    pub sha256: String,
    pub reason: String,
}

/// Решение проверки по точным байтам рендера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewState {
    pub review_id: String,
    pub sha256: String,
    pub decision: ReviewDecision,
    pub reason: String,
}

/// Состояние одной кодовой точки; переписывается атомарно после каждого этапа.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemState {
    pub schema_version: u32,
    pub character: String,
    pub unicode: String,
    pub origins: BTreeSet<TargetOrigin>,
    pub render: Option<RenderState>,
    pub render_error: Option<StageError>,
    pub validation: Option<ValidationState>,
    pub ocr: Option<OcrState>,
    pub publication: Option<PublicationState>,
    pub control: Option<ControlState>,
    #[serde(default)]
    pub control_skip: Option<ControlSkip>,
    #[serde(default)]
    pub review: Option<ReviewState>,
}

impl ItemState {
    fn new(character: &str) -> Self {
        Self {
            schema_version: ITEM_SCHEMA,
            character: character.into(),
            unicode: unicode_of(character),
            origins: BTreeSet::new(),
            render: None,
            render_error: None,
            validation: None,
            ocr: None,
            publication: None,
            control: None,
            control_skip: None,
            review: None,
        }
    }

    fn identity(&self) -> AssetIdentity {
        AssetIdentity {
            namespace: "kanji".into(),
            key: self.character.clone(),
        }
    }

    fn rendered_sha(&self) -> Option<&str> {
        self.render.as_ref().map(|render| render.sha256.as_str())
    }

    fn validation_current(&self) -> Option<&ValidationState> {
        self.validation
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    fn ocr_current(&self) -> Option<&OcrState> {
        self.ocr
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    fn review_current(&self) -> Option<&ReviewState> {
        self.review
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    /// Группа проверки для байтов, не прошедших автоматические ворота; `None` —
    /// проверка не нужна или ещё не выполнены валидатор и OCR этих байтов.
    fn review_group(&self) -> Option<&'static str> {
        let validation = self.validation_current()?;
        let ocr = self.ocr_current()?;
        if self.passes_gates() || validation.status == SemanticStatus::Corrupt {
            return None;
        }
        let verified = validation.status == SemanticStatus::Verified;
        Some(if ocr.failure.is_some() {
            "ocr_failed"
        } else if ocr.matches_expected {
            "validator_doubt_ocr_match"
        } else if ocr.observed.as_deref() == Some("?") {
            "ocr_unrecognized"
        } else if verified {
            "ocr_mismatch"
        } else {
            "validator_doubt_ocr_mismatch"
        })
    }

    /// Для этих байтов действует решение человека `reject`: автоматика его не отменяет.
    fn human_rejected(&self) -> bool {
        self.review_current()
            .is_some_and(|review| review.decision == ReviewDecision::Reject)
    }

    /// Контроль с историческим PNG нужен этим байтам и ещё не выполнен.
    fn control_pending(&self, retry_failed: bool) -> bool {
        self.origins.contains(&TargetOrigin::ExistingPng)
            && self.render.is_some()
            && self.control.is_none()
            && (retry_failed
                || !self
                    .control_skip
                    .as_ref()
                    .is_some_and(|skip| Some(skip.sha256.as_str()) == self.rendered_sha()))
    }

    fn passes_gates(&self) -> bool {
        self.validation_current()
            .is_some_and(|state| state.status == SemanticStatus::Verified)
            && self
                .ocr_current()
                .is_some_and(|state| state.matches_expected)
    }
}

/// Неизменяемая конфигурация рабочего каталога: повтор с другой обязан
/// начинаться в новом каталоге.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunConfig {
    schema_version: u32,
    algorithm: String,
    settings: RenderSettings,
    font_sha256: String,
    store_id: String,
}

pub(crate) struct RunOptions {
    pub font: LocalFont,
    pub profile: GeometryProfile,
    pub work_dir: PathBuf,
    pub browsers: usize,
    pub from_corpus: bool,
    pub explicit: Vec<String>,
    pub limit: Option<usize>,
    pub ocr: Option<OcrOptions>,
    pub rescan_ocr: bool,
    pub publish: bool,
    pub retry_failed: bool,
}

/// Число автоматических попыток временно отказавшего этапа; дальше этап ждёт
/// явного `--retry-failed`, чтобы `--limit` не выбирал одни и те же символы вечно.
const MAX_AUTO_ATTEMPTS: u32 = 3;

/// Отказы рендера, которые при тех же шрифте и параметрах повторятся.
const FINAL_RENDER_ERRORS: &[&str] = &["glyph_missing", "font_fallback"];

/// Какие этапы запрошены в прогоне.
#[derive(Debug, Clone, Copy)]
struct Selection {
    ocr: bool,
    rescan_ocr: bool,
    publish: bool,
    retry_failed: bool,
}

/// Можно ли снова рендерить символ после сохранённого отказа. `glyph_missing`
/// определяется шрифтом из `run.json` и не повторяется никогда.
fn render_retry_allowed(error: Option<&StageError>, retry_failed: bool) -> bool {
    match error {
        None => true,
        Some(error) if error.code == "glyph_missing" => false,
        Some(_) if retry_failed => true,
        Some(error) => {
            !FINAL_RENDER_ERRORS.contains(&error.code.as_str())
                && error.attempts < MAX_AUTO_ATTEMPTS
        }
    }
}

/// Отказ OCR, который определяется самим кадром или ответом модели: повтор
/// того же запроса даст тот же исход, поэтому он выполняется только по `--rescan`.
fn ocr_failure_is_final(failure: &str) -> bool {
    matches!(failure, "invalid_observed_json" | "observed_not_single_han")
        || failure.starts_with("image_prepare_failed")
}

/// Нужен ли символу запрос OCR при параметрах прогона.
fn ocr_needed(state: &ItemState, selection: Selection) -> bool {
    match state.ocr_current() {
        None => true,
        Some(_) if selection.rescan_ocr => true,
        Some(ocr) => ocr.failure.as_deref().is_some_and(|failure| {
            !ocr_failure_is_final(failure)
                && (selection.retry_failed || ocr.attempts < MAX_AUTO_ATTEMPTS)
        }),
    }
}

/// Разрешена ли новая попытка публикации после сохранённого `publish_error`.
fn publication_retry_allowed(state: &ItemState, sha256: &str, retry_failed: bool) -> bool {
    match state
        .publication
        .as_ref()
        .filter(|publication| publication.sha256 == sha256)
    {
        Some(publication) if publication.outcome == "publish_error" => {
            retry_failed || publication.attempts < MAX_AUTO_ATTEMPTS
        }
        _ => true,
    }
}

/// Состояние PNG-слота символа относительно байтов рендера.
#[derive(Debug, Clone, PartialEq)]
enum SlotStatus {
    /// Слот пуст: публикация возможна.
    Empty,
    /// Слот занят; итог и пояснение для `PublicationState`.
    Settled(&'static str, Option<String>),
}

/// Оценивает текущий PNG-слот по фактическому хранилищу, а не по сохранённому
/// итогу: слот мог освободиться, а доверие — быть отозвано после прошлого прогона.
/// Совпадение SHA-256 не делает чужую запись локальным рендером.
fn png_slot_status(render: &RenderState, slot: Option<&AssetRecord>) -> SlotStatus {
    let Some(existing) = slot else {
        return SlotStatus::Empty;
    };
    let same_bytes = existing.sha256 == render.sha256;
    let local = existing.provenance.source_kind == LOCAL_RENDER_SOURCE_KIND;
    let rejected = existing
        .human_attestation
        .as_ref()
        .is_some_and(|attestation| attestation.decision == HumanDecision::Reject);
    if same_bytes && local && existing.lifecycle == LifecycleState::Verified {
        return SlotStatus::Settled("already_published", None);
    }
    if same_bytes && rejected {
        return SlotStatus::Settled(
            "trust_revoked",
            Some(
                "решение человека в хранилище отклонило эти байты; повтор их не восстанавливает"
                    .into(),
            ),
        );
    }
    if existing.lifecycle != LifecycleState::Verified {
        return SlotStatus::Settled(
            "png_slot_occupied",
            Some(format!(
                "PNG-слот занят непроверенной записью {}",
                existing.sha256
            )),
        );
    }
    if same_bytes {
        return SlotStatus::Settled(
            "png_slot_occupied",
            Some(format!(
                "те же байты уже в хранилище с происхождением {}; локальным рендером они не считаются",
                existing.provenance.source_kind
            )),
        );
    }
    if !local {
        return SlotStatus::Settled(
            "existing_original_kept",
            Some(format!(
                "исторический PNG {} сохранён; рендер — контрольная генерация",
                existing.sha256
            )),
        );
    }
    SlotStatus::Settled(
        "png_slot_occupied",
        Some(format!(
            "в хранилище другой локальный рендер {}; автоматическая замена не выполняется",
            existing.sha256
        )),
    )
}

/// PNG-слоты всех записей хранилища, включая `.runtime`.
fn png_slots(records: Vec<AssetRecord>) -> BTreeMap<String, AssetRecord> {
    records
        .into_iter()
        .filter(|record| record.format == DetectedFormat::Png)
        .map(|record| (record.identity.key.clone(), record))
        .collect()
}

/// Согласованность свидетельств рендера с байтами, состоянием и `run.json`.
fn check_evidence(
    state: &ItemState,
    render: &RenderState,
    evidence: &RenderEvidence,
    config: &RunConfig,
) -> Result<(), String> {
    let mismatch = if evidence.character != state.character {
        "символ"
    } else if evidence.corrected_sha256 != render.sha256 {
        "SHA-256 изображения"
    } else if evidence.raw_sha256 != render.raw_sha256 {
        "SHA-256 сырого снимка"
    } else if evidence.settings != config.settings
        || evidence.settings.algorithm != config.algorithm
    {
        "параметры рендера"
    } else if evidence.font.sha256 != config.font_sha256 {
        "SHA-256 шрифта"
    } else {
        return Ok(());
    };
    Err(format!(
        "{}: свидетельства рендера расходятся с прогоном ({mismatch})",
        state.unicode
    ))
}

pub(crate) fn unicode_of(character: &str) -> String {
    format!(
        "U+{:04X}",
        u32::from(character.chars().next().unwrap_or('\0'))
    )
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Атомарная запись файла рабочего каталога: временный файл, `fsync`, `rename`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("у пути результата нет каталога"))?;
    std::fs::create_dir_all(parent).map_err(|error| AssetError::io("создание каталога", error))?;
    let temp = parent.join(format!(
        ".{}.partial",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let mut file =
        std::fs::File::create(&temp).map_err(|error| AssetError::io("запись результата", error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("запись результата", error))?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|error| AssetError::io("публикация результата", error))
}

/// Рабочий каталог конвейера.
pub(crate) struct WorkDir {
    root: PathBuf,
}

/// Подкаталоги рабочего каталога; ни один из них не может быть symlink, иначе
/// запись артефактов ушла бы за пределы проверенной границы.
const WORK_AREAS: &[&str] = &[
    "items",
    "png",
    "raw",
    "evidence",
    "references",
    "review",
    "report",
];

/// Каталог не является symlink (отсутствующий каталог допустим).
fn ensure_not_linked(path: &Path) -> Result<(), AssetError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            format!(
                "{} в рабочем каталоге не может быть symlink",
                path.display()
            ),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(invalid(format!(
            "{} в рабочем каталоге не является каталогом",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AssetError::io("проверка рабочего каталога", error)),
    }
}

impl WorkDir {
    /// Создаёт или продолжает рабочий каталог `run` и привязывает его к
    /// конфигурации прогона. Граница с хранилищем проверяется до любой записи.
    fn create(root: &Path, store: &AssetStore, config: &RunConfig) -> Result<Self, AssetError> {
        super::local_png_cli::ensure_outside_store(root, store.root())?;
        let work = Self {
            root: root.to_path_buf(),
        };
        if work.root.exists() {
            work.ensure_areas_not_linked()?;
        }
        std::fs::create_dir_all(&work.root)
            .map_err(|error| AssetError::io("создание рабочего каталога", error))?;
        work.ensure_areas_not_linked()?;
        match work.load_config()? {
            Some(saved) if saved != *config => {
                return Err(invalid(
                    "рабочий каталог создан с другими параметрами рендера, шрифтом или хранилищем; \
                     начните новый каталог",
                ));
            }
            Some(_) => {}
            None => {
                let has_items = std::fs::read_dir(work.root.join("items"))
                    .is_ok_and(|mut entries| entries.next().is_some());
                if has_items {
                    return Err(invalid(
                        "рабочий каталог содержит состояния без run.json; начните новый каталог",
                    ));
                }
                write_atomic(
                    &work.root.join("run.json"),
                    &serde_json::to_vec_pretty(config).expect("конфигурация сериализуется"),
                )?;
            }
        }
        std::fs::create_dir_all(work.root.join("items"))
            .map_err(|error| AssetError::io("создание рабочего каталога", error))?;
        Ok(work)
    }

    /// Открывает каталог прежнего прогона этого же хранилища, ничего не создавая.
    /// Состояние другого хранилища, без `run.json` или с иной схемой отклоняется
    /// до чтения решений и изменения хранилища.
    pub(crate) fn open_existing(
        root: &Path,
        store: &AssetStore,
    ) -> Result<(Self, RunConfig), AssetError> {
        super::local_png_cli::ensure_outside_store(root, store.root())?;
        let work = Self {
            root: root.to_path_buf(),
        };
        if !work.root.join("items").is_dir() {
            return Err(invalid(format!(
                "{} не является рабочим каталогом local-png run",
                root.display()
            )));
        }
        work.ensure_areas_not_linked()?;
        let config = work
            .load_config()?
            .ok_or_else(|| invalid("в рабочем каталоге нет run.json"))?;
        if config.schema_version != RUN_SCHEMA {
            return Err(invalid(format!(
                "run.json: неподдерживаемая версия схемы {}",
                config.schema_version
            )));
        }
        if config.store_id != store.store_id() {
            return Err(invalid(format!(
                "рабочий каталог относится к хранилищу {}, а открыто {}; \
                 состояние прогона нельзя применять к другому хранилищу",
                config.store_id,
                store.store_id()
            )));
        }
        Ok((work, config))
    }

    fn ensure_areas_not_linked(&self) -> Result<(), AssetError> {
        for area in WORK_AREAS {
            ensure_not_linked(&self.root.join(area))?;
        }
        Ok(())
    }

    fn load_config(&self) -> Result<Option<RunConfig>, AssetError> {
        match std::fs::read(self.root.join("run.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| invalid(format!("повреждён run.json: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AssetError::io("чтение run.json", error)),
        }
    }

    /// Рендер цел: байты PNG и сырого снимка совпадают с состоянием, а
    /// свидетельства — с ними и с `run.json`. Иначе символ рендерится заново.
    fn render_intact(&self, state: &ItemState, config: &RunConfig) -> bool {
        self.verified_render(state, config).is_ok()
    }

    /// Байты рендера и его свидетельства после полной сверки; основание публикации.
    fn verified_render(
        &self,
        state: &ItemState,
        config: &RunConfig,
    ) -> Result<(Vec<u8>, RenderEvidence), AssetError> {
        let render = state
            .render
            .as_ref()
            .ok_or_else(|| invalid(format!("{}: нет рендера", state.unicode)))?;
        let mismatch = |message: String| AssetError::new(ErrorCode::IntegrityMismatch, message);
        let bytes = self.read(&render.file)?;
        if sha256_hex(&bytes) != render.sha256 || bytes.len() as u64 != render.byte_length {
            return Err(mismatch(format!("файл рендера {} изменился", render.file)));
        }
        if sha256_hex(self.read(&render.raw_file)?) != render.raw_sha256 {
            return Err(mismatch(format!(
                "сырой снимок {} изменился",
                render.raw_file
            )));
        }
        let evidence: RenderEvidence =
            serde_json::from_slice(&self.read(&render.evidence_file)?)
                .map_err(|error| mismatch(format!("свидетельства рендера: {error}")))?;
        check_evidence(state, render, &evidence, config).map_err(mismatch)?;
        Ok((bytes, evidence))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn item_path(&self, unicode: &str) -> PathBuf {
        self.root.join("items").join(format!("{unicode}.json"))
    }

    pub(crate) fn load_item(&self, character: &str) -> Result<Option<ItemState>, AssetError> {
        let path = self.item_path(&unicode_of(character));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let state: ItemState = serde_json::from_slice(&bytes).map_err(|error| {
                    invalid(format!("повреждено состояние {}: {error}", path.display()))
                })?;
                if state.character != character || state.schema_version != ITEM_SCHEMA {
                    return Err(invalid(format!(
                        "состояние {} не относится к символу {character}",
                        path.display()
                    )));
                }
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AssetError::io("чтение состояния символа", error)),
        }
    }

    pub(crate) fn save_item(&self, state: &ItemState) -> Result<(), AssetError> {
        write_atomic(
            &self.item_path(&state.unicode),
            &serde_json::to_vec_pretty(state).expect("состояние сериализуется"),
        )
    }

    /// Все сохранённые состояния в порядке кодовых точек.
    pub(crate) fn all_items(&self) -> Result<Vec<ItemState>, AssetError> {
        let mut items = Vec::new();
        let entries = std::fs::read_dir(self.root.join("items"))
            .map_err(|error| AssetError::io("чтение состояний", error))?;
        for entry in entries {
            let entry = entry.map_err(|error| AssetError::io("чтение состояний", error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("U+") || !name.ends_with(".json") {
                continue;
            }
            let bytes = std::fs::read(entry.path())
                .map_err(|error| AssetError::io("чтение состояния", error))?;
            let state: ItemState = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("повреждено состояние {name}: {error}")))?;
            items.push(state);
        }
        items.sort_by(|left, right| left.character.cmp(&right.character));
        Ok(items)
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>, AssetError> {
        std::fs::read(self.root.join(relative))
            .map_err(|error| AssetError::io("чтение артефакта рабочего каталога", error))
    }

    fn log(&self, line: &str) {
        eprintln!("{line}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("progress.log"))
        {
            let _ = writeln!(file, "{} {line}", now_unix());
        }
    }
}

/// Инвентарь хранилища, построенный только по проверенному `verify_integrity`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inventory {
    pub captured_at_unix: u64,
    pub store_id: String,
    pub records_total: usize,
    pub verified_gif_records: usize,
    pub trusted_gif_characters: usize,
    pub untrusted_gif_records: usize,
    pub png_records: usize,
    pub png_local_render_records: usize,
    pub png_other_records: usize,
    pub characters_with_gif_and_png: usize,
    pub runtime_records: usize,
    /// SHA-256 каждого GIF: основание проверки неизменности после публикации.
    pub gif_sha256: BTreeMap<String, String>,
    /// SHA-256 и источник каждого PNG.
    pub png: BTreeMap<String, (String, String)>,
}

pub(crate) fn inventory(store: &AssetStore) -> Result<(Inventory, Vec<AssetRecord>), AssetError> {
    let records = store.verify_integrity()?;
    let validator = KanjiImageValidator::validator_identity();
    let semantics = crate::domain::AssetDomainPolicy::trust_semantics(&KanjiDomainPolicy);
    let mut gif_sha256 = BTreeMap::new();
    let mut png = BTreeMap::new();
    let mut trusted_gif = BTreeSet::new();
    let mut untrusted_gif = 0;
    let mut verified_gif = 0;
    let mut runtime = 0;
    for record in &records {
        if record.lifecycle != LifecycleState::Verified {
            runtime += 1;
            continue;
        }
        match record.format {
            DetectedFormat::Gif => {
                verified_gif += 1;
                gif_sha256.insert(record.identity.key.clone(), record.sha256.clone());
                if semantics.confers_trust(record, &validator) {
                    trusted_gif.insert(record.identity.key.clone());
                } else {
                    untrusted_gif += 1;
                }
            }
            DetectedFormat::Png => {
                png.insert(
                    record.identity.key.clone(),
                    (record.sha256.clone(), record.provenance.source_kind.clone()),
                );
            }
            _ => {}
        }
    }
    let both = png
        .keys()
        .filter(|key| gif_sha256.contains_key(*key))
        .count();
    let local = png
        .values()
        .filter(|(_, kind)| kind == LOCAL_RENDER_SOURCE_KIND)
        .count();
    Ok((
        Inventory {
            captured_at_unix: now_unix(),
            store_id: store.store_id().to_owned(),
            records_total: records.len(),
            verified_gif_records: verified_gif,
            trusted_gif_characters: trusted_gif.len(),
            untrusted_gif_records: untrusted_gif,
            png_records: png.len(),
            png_local_render_records: local,
            png_other_records: png.len() - local,
            characters_with_gif_and_png: both,
            runtime_records: runtime,
            gif_sha256,
            png,
        },
        records,
    ))
}

fn trusted_gif_characters(records: &[AssetRecord]) -> BTreeSet<String> {
    let validator = KanjiImageValidator::validator_identity();
    let semantics = crate::domain::AssetDomainPolicy::trust_semantics(&KanjiDomainPolicy);
    records
        .iter()
        .filter(|record| {
            record.format == DetectedFormat::Gif
                && record.lifecycle == LifecycleState::Verified
                && semantics.confers_trust(record, &validator)
        })
        .map(|record| record.identity.key.clone())
        .collect()
}

/// Исторические PNG (не локальный рендер): для них генерация только контрольная.
fn existing_png_characters(records: &[AssetRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| {
            record.format == DetectedFormat::Png
                && record.lifecycle == LifecycleState::Verified
                && record.provenance.source_kind != LOCAL_RENDER_SOURCE_KIND
        })
        .map(|record| record.identity.key.clone())
        .collect()
}

fn glyph_area(png: &[u8]) -> u64 {
    image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map(|image| {
            let gray = image.to_luma8();
            let (width, height) = gray.dimensions();
            let mut area = 0;
            for y in 8..height.saturating_sub(8) {
                for x in 8..width.saturating_sub(8) {
                    area += u64::from(gray.get_pixel(x, y)[0] >= 180);
                }
            }
            area
        })
        .unwrap_or(0)
}

fn local_metadata(state: &ItemState, evidence: &RenderEvidence) -> Value {
    json!({
        "domain": "kanji",
        "character": state.character,
        "unicode_codepoints": [state.unicode],
        "local_render": {
            "provider": LOCAL_RENDER_SOURCE_KIND,
            "provider_version": LOCAL_RENDER_PROVIDER_VERSION,
            "algorithm": evidence.settings.algorithm,
            "geometry_profile": evidence.settings.geometry_profile,
            "origin_css": evidence.settings.origin_css,
            "device_scale_factor": evidence.settings.device_scale_factor,
            "font_size_css": evidence.settings.font_size_css,
            "glyph_translate_css": evidence.settings.glyph_translate_css,
            "coverage_gamma": evidence.settings.coverage_correction.gamma,
            "font_sha256": evidence.font.sha256,
            "font_version": evidence.font.version,
            "font_calibrated": evidence.font.calibrated,
            "browser_product": evidence.browser_product,
            "browser_revision": evidence.browser_revision,
            "platform_font": evidence.platform_fonts.first().map(|font| font.post_script_name.clone()),
            "raw_sha256": evidence.raw_sha256,
            "not_yarxi_capture": true,
        },
    })
}

fn validate_bytes(character: &str, bytes: &[u8]) -> Result<ValidationState, String> {
    let validator = KanjiImageValidator::new();
    let sha256 = sha256_hex(bytes);
    let record = AssetRecord {
        identity: AssetIdentity {
            namespace: "kanji".into(),
            key: character.into(),
        },
        storage_path: format!("assets/png/{character}.png"),
        consumer_filename: format!("{character}.png"),
        sha256: sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::from_signature(bytes),
        provenance: Provenance {
            source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
            source_name: SOURCE_NAME.into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: None,
    };
    let decision = validator
        .validate(&record, &mut std::io::Cursor::new(bytes))
        .map_err(|failure| format!("{}: {}", failure.code, failure.message))?;
    Ok(ValidationState {
        sha256,
        status: decision.status,
        validator: validator.identity(),
        evidence: decision.evidence,
    })
}

/// Выполняет все этапы конвейера и возвращает машиночитаемую сводку прогона.
pub(crate) fn run(store: &AssetStore, options: RunOptions) -> Result<Value, AssetError> {
    let settings = RenderSettings::for_profile(options.profile);
    let config = RunConfig {
        schema_version: RUN_SCHEMA,
        algorithm: LOCAL_RENDER_ALGORITHM.into(),
        settings: settings.clone(),
        font_sha256: options.font.sha256.clone(),
        store_id: store.store_id().to_owned(),
    };
    let work = WorkDir::create(&options.work_dir, store, &config)?;
    if options.publish && !options.font.calibrated {
        return Err(invalid(
            "публикация разрешена только для откалиброванного шрифта",
        ));
    }

    // 1. Инвентарь до любых изменений: фиксируется один раз и не переписывается.
    let (inventory_now, records) = inventory(store)?;
    let before_path = work.root().join("inventory-before.json");
    if !before_path.exists() {
        write_atomic(
            &before_path,
            &serde_json::to_vec_pretty(&inventory_now).expect("инвентарь сериализуется"),
        )?;
    }
    let existing_png = existing_png_characters(&records);
    let mut targets: BTreeMap<String, BTreeSet<TargetOrigin>> = BTreeMap::new();
    if options.from_corpus {
        for character in trusted_gif_characters(&records) {
            targets
                .entry(character)
                .or_default()
                .insert(TargetOrigin::CorpusGif);
        }
        for character in existing_png.iter().cloned() {
            targets
                .entry(character)
                .or_default()
                .insert(TargetOrigin::ExistingPng);
        }
    }
    for character in &options.explicit {
        targets
            .entry(character.clone())
            .or_default()
            .insert(TargetOrigin::Explicit);
    }
    let mut states = Vec::with_capacity(targets.len());
    for (character, origins) in targets {
        let mut state = work
            .load_item(&character)?
            .unwrap_or_else(|| ItemState::new(&character));
        let before = state.clone();
        state.origins.extend(origins);
        if existing_png.contains(&character) {
            state.origins.insert(TargetOrigin::ExistingPng);
        }
        if state != before {
            work.save_item(&state)?;
        }
        states.push(state);
    }
    let selection = Selection {
        ocr: options.ocr.is_some(),
        rescan_ocr: options.rescan_ocr,
        publish: options.publish,
        retry_failed: options.retry_failed,
    };
    if let Some(limit) = options.limit {
        states = limit_targets(
            &work,
            &config,
            states,
            limit,
            selection,
            &png_slots(records),
        );
    }
    // Сведения о составе хранилища остаются в инвентаре, а не в журнале хода.
    work.log(&format!(
        "local-png run: целей {}, браузеров {}",
        states.len(),
        options.browsers
    ));

    render_stage(&work, &config, &options, &mut states)?;
    validation_stage(&work, &mut states)?;
    control_stage(&work, store, options.retry_failed, &mut states)?;
    let ocr_report = match &options.ocr {
        Some(ocr) => Some(ocr_stage(&work, ocr, selection, &mut states)?),
        None => None,
    };
    if options.publish {
        publish_stage(&work, store, &config, options.retry_failed, &mut states)?;
    }
    let (inventory_after, _) = inventory(store)?;
    write_atomic(
        &work.root().join("inventory-latest.json"),
        &serde_json::to_vec_pretty(&inventory_after).expect("инвентарь сериализуется"),
    )?;
    let counts = outcome_counts(&work.all_items()?, &inventory_after);
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png run",
        "outcome": "completed",
        "work_dir": work.root().display().to_string(),
        "processed": states.len(),
        "counts": counts,
        "ocr": ocr_report,
    }))
}

/// Первые `limit` символов, у которых осталась работа. Предел ограничивает работу
/// прогона, но не забывает остальные символы.
fn limit_targets(
    work: &WorkDir,
    config: &RunConfig,
    mut states: Vec<ItemState>,
    limit: usize,
    selection: Selection,
    slots: &BTreeMap<String, AssetRecord>,
) -> Vec<ItemState> {
    let mut taken = 0;
    states.retain(|state| {
        if taken >= limit
            || !has_pending_work(work, config, state, selection, slots.get(&state.character))
        {
            return false;
        }
        taken += 1;
        true
    });
    states
}

/// Осталась ли у символа работа при параметрах прогона. По этому признаку
/// `--limit` отбирает символы: готовые этапы и устойчивые отказы, исчерпавшие
/// автоматические попытки, не должны занимать предел при каждом повторе, иначе
/// прогон не дойдёт до остальных символов. Критерии совпадают с отбором этапов.
fn has_pending_work(
    work: &WorkDir,
    config: &RunConfig,
    state: &ItemState,
    selection: Selection,
    slot: Option<&AssetRecord>,
) -> bool {
    if !work.render_intact(state, config) {
        return render_retry_allowed(state.render_error.as_ref(), selection.retry_failed);
    }
    let validator = KanjiImageValidator::validator_identity();
    if !state
        .validation_current()
        .is_some_and(|validation| validation.validator == validator)
    {
        return true;
    }
    if state.control_pending(selection.retry_failed) {
        return true;
    }
    if selection.ocr && ocr_needed(state, selection) {
        return true;
    }
    let render = state.render.as_ref().expect("рендер цел");
    selection.publish
        && state.passes_gates()
        && !state.human_rejected()
        && png_slot_status(render, slot) == SlotStatus::Empty
        && publication_retry_allowed(state, &render.sha256, selection.retry_failed)
}

fn render_stage(
    work: &WorkDir,
    config: &RunConfig,
    options: &RunOptions,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let mut pending = Vec::new();
    for state in states.iter() {
        if !work.render_intact(state, config)
            && render_retry_allowed(state.render_error.as_ref(), options.retry_failed)
        {
            pending.push(state.character.clone());
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    // Отсутствие глифа устанавливается до запуска браузера и окончательно.
    let (missing, pending): (Vec<_>, Vec<_>) = pending.into_iter().partition(|character| {
        !options
            .font
            .has_glyph(character.chars().next().expect("символ непуст"))
    });
    let index: BTreeMap<String, usize> = states
        .iter()
        .enumerate()
        .map(|(position, state)| (state.character.clone(), position))
        .collect();
    for character in missing {
        let state = &mut states[index[&character]];
        state.render = None;
        state.render_error = Some(StageError {
            code: "glyph_missing".into(),
            message: format!(
                "в шрифте {} нет глифа для {}",
                options.font.sha256, state.unicode
            ),
            attempts: state
                .render_error
                .as_ref()
                .map_or(0, |error| error.attempts)
                + 1,
        });
        work.save_item(state)?;
        work.log(&format!("render {}: glyph_missing", state.unicode));
    }
    let total = pending.len();
    let mut done = 0usize;
    work.log(&format!("render: к генерации {total}"));
    super::local_png_cli::render_pool(
        &options.font,
        options.profile,
        pending,
        options.browsers,
        |(character, result)| {
            done += 1;
            let state = &mut states[index[&character]];
            match result {
                Ok(rendered) => {
                    let file = format!("png/{}.png", state.unicode);
                    let raw_file = format!("raw/{}.png", state.unicode);
                    let evidence_file = format!("evidence/{}.json", state.unicode);
                    write_atomic(&work.root().join(&file), &rendered.png)?;
                    write_atomic(&work.root().join(&raw_file), &rendered.raw_png)?;
                    write_atomic(
                        &work.root().join(&evidence_file),
                        &serde_json::to_vec_pretty(&rendered.evidence)
                            .expect("свидетельства сериализуются"),
                    )?;
                    let render = RenderState {
                        sha256: rendered.evidence.corrected_sha256.clone(),
                        raw_sha256: rendered.evidence.raw_sha256.clone(),
                        byte_length: rendered.png.len() as u64,
                        file,
                        raw_file,
                        evidence_file,
                        glyph_area: glyph_area(&rendered.png),
                    };
                    if state.rendered_sha() != Some(render.sha256.as_str()) {
                        // Новые байты не наследуют решений прежних байтов.
                        state.validation = None;
                        state.ocr = None;
                        state.publication = None;
                        state.control = None;
                        state.control_skip = None;
                    }
                    state.render = Some(render);
                    state.render_error = None;
                }
                Err(message) => record_render_failure(state, message),
            }
            work.save_item(state)?;
            if done.is_multiple_of(100) || done == total {
                work.log(&format!("render: {done}/{total}"));
            }
            Ok(())
        },
    )?;
    Ok(())
}

/// Неудачный повторный рендер: символ попадает в рендер, только если прежние байты
/// не целы, поэтому они и зависящие от них решения сбрасываются, а ошибка
/// остаётся видимой в итоге символа.
fn record_render_failure(state: &mut ItemState, message: String) {
    let code = message
        .split(':')
        .next()
        .unwrap_or("render_failed")
        .trim()
        .to_owned();
    state.render = None;
    state.validation = None;
    state.ocr = None;
    state.publication = None;
    state.control = None;
    state.control_skip = None;
    state.render_error = Some(StageError {
        code,
        message,
        attempts: state
            .render_error
            .as_ref()
            .map_or(0, |error| error.attempts)
            + 1,
    });
}

fn validation_stage(work: &WorkDir, states: &mut [ItemState]) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::validator_identity();
    let total = states.iter().filter(|state| state.render.is_some()).count();
    let mut done = 0usize;
    for state in states.iter_mut() {
        let Some(render) = state.render.clone() else {
            continue;
        };
        if state
            .validation_current()
            .is_some_and(|validation| validation.validator == validator)
        {
            continue;
        }
        let bytes = work.read(&render.file)?;
        if sha256_hex(&bytes) != render.sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                format!("файл рендера {} изменился", render.file),
            ));
        }
        state.validation = Some(validate_bytes(&state.character, &bytes).map_err(|message| {
            AssetError::new(
                ErrorCode::ValidatorFailure,
                format!("{}: {message}", state.unicode),
            )
        })?);
        work.save_item(state)?;
        done += 1;
        if done.is_multiple_of(200) {
            work.log(&format!("validate: {done}/{total}"));
        }
    }
    work.log(&format!("validate: актуальных решений {total}"));
    Ok(())
}

/// Для символов с историческим PNG: копия оригинала и диагностические метрики.
fn control_stage(
    work: &WorkDir,
    store: &AssetStore,
    retry_failed: bool,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::validator_identity();
    let pending: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.control_pending(retry_failed))
        .map(|(index, _)| index)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let requests: Vec<_> = pending
        .iter()
        .map(|&index| (states[index].identity(), DetectedFormat::Png))
        .collect();
    AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &requests,
        &validator,
        &KanjiDomainPolicy,
        |request, original| {
            let state = &mut states[pending[request]];
            let render = state.render.clone().expect("отобран рендер");
            // Обоснованный пропуск сохраняется для этих байтов, чтобы символ не
            // возвращался в работу при каждом повторе.
            let skip = |state: &mut ItemState, reason: String| {
                work.log(&format!("control {}: пропущен ({reason})", state.unicode));
                state.control_skip = Some(ControlSkip {
                    sha256: render.sha256.clone(),
                    reason,
                });
                work.save_item(state)
            };
            let original = match original {
                Ok(original) => original,
                // Контроль — диагностика: исторический PNG без доверенного решения
                // нынешней версии валидатора не останавливает прогон.
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::InvalidValidationEvidence | ErrorCode::MissingAssetFile
                    ) =>
                {
                    return skip(state, error.code.as_str().to_owned());
                }
                Err(error) => return Err(error),
            };
            if original.record.provenance.source_kind == LOCAL_RENDER_SOURCE_KIND {
                return skip(state, "original_is_local_render".into());
            }
            let reference_file = format!("references/{}.png", state.unicode);
            write_atomic(&work.root().join(&reference_file), &original.bytes)?;
            let decode = |bytes: &[u8]| {
                image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
                    .map(|image| image.to_rgb8())
                    .map_err(|error| invalid(format!("декодирование PNG: {error}")))
            };
            let metrics = compare_images(
                &decode(&original.bytes)?,
                &decode(&work.read(&render.file)?)?,
            )
            .map_err(invalid)?;
            state.control = Some(ControlState {
                original_sha256: original.record.sha256.clone(),
                original_provenance: format!(
                    "{}:{}",
                    original.record.provenance.source_kind, original.record.provenance.source_name
                ),
                reference_file,
                metrics,
            });
            state.control_skip = None;
            work.save_item(state)
        },
    )
}

/// Слепой OCR каждого сгенерированного PNG. Модели передаётся только кадр и
/// штатный промпт задачи; ожидаемый символ сравнивается после ответа.
fn ocr_stage(
    work: &WorkDir,
    options: &OcrOptions,
    selection: Selection,
    states: &mut [ItemState],
) -> Result<Value, AssetError> {
    // Технические отказы повторяются не больше `MAX_AUTO_ATTEMPTS` раз;
    // недопустимый ответ модели и непригодный кадр — только по `--rescan`.
    let targets: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.render.is_some() && ocr_needed(state, selection))
        .map(|(position, _)| position)
        .collect();
    if targets.is_empty() {
        work.log("ocr: нечего распознавать");
        return Ok(json!({"requests": 0}));
    }
    let task = options.inference.resolve(TaskId::KanjiOcr)?;
    task.require_ready()?;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AssetError::io("runtime OCR", error))?;
    work.log(&format!("ocr: подготовка модели, целей {}", targets.len()));
    let prepared = executor.block_on(InferenceSession::prepare(
        task,
        contract(),
        TRANSFORM_VERSION,
    ));
    let mut session = match prepared {
        Ok(session) => session,
        Err(error) => {
            let (failure, report) = *error;
            let stop_unproven = failure.stop_unproven();
            return Err(crate::inference::with_session_evidence(
                failure.into_asset_error(),
                stop_unproven,
                Some(&report),
            ));
        }
    };
    let provenance: InferenceProvenance = session.provenance().clone();
    let provenance_sha256 =
        sha256_hex(serde_json::to_vec(&provenance).expect("провенанс сериализуется"));
    let model = provenance.summary();
    write_atomic(
        &work.root().join("ocr-model.json"),
        &serde_json::to_vec_pretty(&json!({
            "provenance": provenance,
            "provenance_sha256": provenance_sha256,
            "summary": model,
        }))
        .expect("провенанс сериализуется"),
    )?;
    work.log(&format!("ocr: модель {model}"));
    let total = targets.len();
    let mut requests = 0usize;
    let mut interrupted = false;
    let mut failure: Option<AssetError> = None;
    for (done, position) in targets.into_iter().enumerate() {
        if session.interrupted() {
            interrupted = true;
            break;
        }
        let state = &mut states[position];
        let render = state.render.clone().expect("цель OCR отрендерена");
        let bytes = match work.read(&render.file) {
            Ok(bytes) => bytes,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        let failed_before = state
            .ocr_current()
            .filter(|ocr| ocr.failure.is_some())
            .map_or(0, |ocr| ocr.attempts);
        let mut ocr = OcrState {
            sha256: render.sha256.clone(),
            observed: None,
            matches_expected: false,
            failure: None,
            prepared_sha256: None,
            model: model.clone(),
            provenance_sha256: provenance_sha256.clone(),
            elapsed_ms: None,
            attempts: 0,
        };
        match prepare_ocr_image(&bytes) {
            Ok(image) => {
                ocr.prepared_sha256 = Some(sha256_hex(&image.png));
                requests += 1;
                match executor.block_on(session.run(Some(&image.png))) {
                    Err(error) if error.is_interrupted() => {
                        interrupted = true;
                        break;
                    }
                    Err(error) => ocr.failure = Some(error.code),
                    Ok(response) => {
                        ocr.elapsed_ms = Some(response.timings.elapsed_ms);
                        match interpret(&response.content) {
                            Ok(observed) => {
                                // Сравнение с ожидаемым — только после ответа модели.
                                ocr.matches_expected = observed == state.character;
                                ocr.observed = Some(observed);
                            }
                            Err(reason) => ocr.failure = Some(reason),
                        }
                    }
                }
            }
            Err(reason) => ocr.failure = Some(format!("image_prepare_failed: {reason}")),
        }
        if ocr.failure.is_some() {
            ocr.attempts = failed_before + 1;
        }
        state.ocr = Some(ocr);
        if let Err(error) = work.save_item(state) {
            failure = Some(error);
            break;
        }
        if (done + 1).is_multiple_of(50) || done + 1 == total {
            work.log(&format!("ocr: {}/{total}", done + 1));
        }
    }
    let finished = executor.block_on(session.finish());
    let stop_unproven = finished.stop.as_ref().is_some_and(|stop| !stop.proven());
    if let Some(error) = failure {
        return Err(crate::inference::with_session_evidence(
            error,
            stop_unproven,
            Some(&finished),
        ));
    }
    if interrupted {
        return Err(crate::inference::interrupted_error(
            "OCR прерван сигналом; уже сохранённые результаты остаются в рабочем каталоге",
            requests,
            stop_unproven,
            Some(&finished),
        ));
    }
    Ok(json!({
        "requests": requests,
        "model": model,
        "provenance_sha256": provenance_sha256,
        "stop_unproven": stop_unproven,
    }))
}

/// Публикует прошедшие ворота байты в пустой PNG-слот. Сохранённый итог прежнего
/// прогона не считается окончательным: слот каждый раз оценивается заново, а
/// состояние перезаписывается только при фактическом изменении итога.
fn publish_stage(
    work: &WorkDir,
    store: &AssetStore,
    config: &RunConfig,
    retry_failed: bool,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::new();
    let (_, records) = inventory(store)?;
    let slots = png_slots(records);
    let mut published = 0usize;
    for state in states.iter_mut() {
        let Some(render) = state.render.clone() else {
            continue;
        };
        if state.human_rejected() || !state.passes_gates() {
            continue;
        }
        let previous = state
            .publication
            .clone()
            .filter(|publication| publication.sha256 == render.sha256);
        let settled = |outcome: &str, message: Option<String>| PublicationState {
            sha256: render.sha256.clone(),
            outcome: outcome.into(),
            message,
            attempts: 0,
        };
        let outcome = match png_slot_status(&render, slots.get(&state.character)) {
            // Собственная публикация прошлого прогона остаётся `published`.
            SlotStatus::Settled("already_published", _)
                if previous
                    .as_ref()
                    .is_some_and(|publication| publication.outcome == "published") =>
            {
                continue;
            }
            // Отказ штатной проверки оставляет в слоте собственного кандидата: итог прежний.
            SlotStatus::Settled("png_slot_occupied", _)
                if previous.as_ref().is_some_and(|publication| {
                    publication.outcome.starts_with("store_validator_")
                }) && slots.get(&state.character).is_some_and(|record| {
                    record.sha256 == render.sha256
                        && record.provenance.source_kind == LOCAL_RENDER_SOURCE_KIND
                }) =>
            {
                continue;
            }
            SlotStatus::Settled(outcome, message) => settled(outcome, message),
            SlotStatus::Empty
                if !publication_retry_allowed(state, &render.sha256, retry_failed) =>
            {
                continue;
            }
            SlotStatus::Empty => match work.verified_render(state, config) {
                // Несогласованные артефакты не доходят до хранилища; следующий
                // прогон рендерит символ заново.
                Err(error) => settled("evidence_mismatch", Some(describe_error(&error))),
                Ok((bytes, evidence)) => {
                    let request = VerifiedIngestRequest {
                        identity: state.identity(),
                        bytes,
                        provenance: Provenance {
                            source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
                            source_name: SOURCE_NAME.into(),
                        },
                        domain_metadata: Some(local_metadata(state, &evidence)),
                        // PNG-слот пуст: CAS не нужен, а GIF этого символа не затрагивается.
                        replace_expected_sha256: None,
                    };
                    match store.ingest_verified(request, &validator) {
                        Ok(outcome) if outcome.status == SemanticStatus::Verified => {
                            published += 1;
                            settled(
                                if outcome.changed {
                                    "published"
                                } else {
                                    "already_published"
                                },
                                None,
                            )
                        }
                        Ok(outcome) => settled(
                            &format!("store_validator_{}", outcome.status.as_str()),
                            Some("штатная проверка при публикации не дала VERIFIED".into()),
                        ),
                        Err(error) => PublicationState {
                            attempts: previous
                                .as_ref()
                                .filter(|publication| publication.outcome == "publish_error")
                                .map_or(0, |publication| publication.attempts)
                                + 1,
                            ..settled("publish_error", Some(describe_error(&error)))
                        },
                    }
                }
            },
        };
        if state.publication.as_ref() != Some(&outcome) {
            state.publication = Some(outcome);
            work.save_item(state)?;
        }
        if published > 0 && published.is_multiple_of(100) {
            work.log(&format!("publish: опубликовано {published}"));
        }
    }
    work.log(&format!("publish: опубликовано в этом прогоне {published}"));
    Ok(())
}

const REVIEW_HEADER: &str = "unicode\tcharacter\tsha256\tgroup\tvalidator\tocr\tdecision\treason";

fn review_id_of(rows: &[(String, String)]) -> String {
    let mut text = String::new();
    for (unicode, sha256) in rows {
        text.push_str(&format!("{unicode}\t{sha256}\n"));
    }
    sha256_hex(text.as_bytes())[..32].to_owned()
}

/// Снимок третьего этапа: символы, у которых валидатор и слепой OCR не дали
/// совместного автоматического подтверждения. Хранилище не меняется.
pub(crate) fn review_export(store: &AssetStore, root: &Path) -> Result<Value, AssetError> {
    let (work, _) = WorkDir::open_existing(root, store)?;
    // Слот занимает запись любого жизненного цикла, как и при `review-apply`.
    let (_, records) = inventory(store)?;
    let occupied = png_slots(records);
    let mut rows = Vec::new();
    for state in work.all_items()? {
        let Some(group) = state.review_group() else {
            continue;
        };
        // Занятый PNG-слот этим путём не меняется, а решённое не переспрашивается.
        if state.review_current().is_some() || occupied.contains_key(&state.character) {
            continue;
        }
        rows.push((group, state));
    }
    rows.sort_by(|left, right| {
        left.0
            .cmp(right.0)
            .then(left.1.character.cmp(&right.1.character))
    });
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|(_, state)| {
            (
                state.unicode.clone(),
                state.rendered_sha().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let review_id = review_id_of(&keys);
    let mut tsv = format!("# review_id={review_id}\n{REVIEW_HEADER}\n");
    let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
    for (group, state) in &rows {
        *groups.entry(group).or_insert(0) += 1;
        let ocr = state.ocr_current().expect("группа требует OCR");
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t\t\n",
            state.unicode,
            state.character,
            state.rendered_sha().unwrap_or(""),
            group,
            state
                .validation_current()
                .expect("группа требует проверки")
                .status
                .as_str(),
            ocr.observed.as_deref().unwrap_or("-"),
        ));
    }
    let directory = format!("review/{review_id}");
    ensure_not_linked(&root.join(&directory))?;
    // Тот же снимок экспортируется повторно: заполненные, но не применённые решения не затираются.
    let decisions_path = root.join(&directory).join("decisions.tsv");
    if decisions_path.exists() {
        work.log(&format!(
            "review-export {review_id}: decisions.tsv сохранён без изменений"
        ));
    } else {
        write_atomic(&decisions_path, tsv.as_bytes())?;
    }
    write_atomic(
        &root.join(&directory).join("index.html"),
        review_html(&review_id, &rows).as_bytes(),
    )?;
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png review-export",
        "outcome": "completed",
        "review_id": review_id,
        "counts": {"rows": rows.len(), "groups": groups},
        "decisions": format!("{directory}/decisions.tsv"),
        "html": format!("{directory}/index.html"),
    }))
}

fn review_html(review_id: &str, rows: &[(&str, ItemState)]) -> String {
    let mut html = format!(
        "<!doctype html><html lang=ru><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Проверка local-png</title><style>\
         :root{{color-scheme:light dark;--bg:#fff;--fg:#111;--line:#ccc}}\
         @media (prefers-color-scheme:dark){{:root{{--bg:#151515;--fg:#eee;--line:#444}}}}\
         body{{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}\
         .grid{{display:flex;flex-wrap:wrap;gap:12px}}.card{{border:1px solid var(--line);padding:8px;width:360px}}\
         .card img{{width:348px;height:348px;image-rendering:pixelated;display:block}}\
         .expected{{font:64px 'Noto Serif JP',serif}}</style><h1>Снимок {review_id}</h1>"
    );
    let mut current = "";
    for (group, state) in rows {
        if *group != current {
            if !current.is_empty() {
                html.push_str("</div>");
            }
            html.push_str(&format!("<h2>{}</h2><div class=grid>", html_escape(group)));
            current = group;
        }
        let file = state
            .render
            .as_ref()
            .map(|render| render.file.as_str())
            .unwrap_or("");
        let ocr = state
            .ocr_current()
            .and_then(|ocr| ocr.observed.clone())
            .unwrap_or_else(|| "-".into());
        html.push_str(&format!(
            "<div class=card><img src='../../{}' alt='{}'><div><span class=expected>{}</span> {} · OCR {} · валидатор {}</div></div>",
            html_escape(file),
            html_escape(&state.character),
            html_escape(&state.character),
            html_escape(&state.unicode),
            html_escape(&ocr),
            state.validation_current().map(|v| v.status.as_str()).unwrap_or("-"),
        ));
    }
    if !current.is_empty() {
        html.push_str("</div>");
    }
    html.push_str("</html>\n");
    html
}

struct ReviewRow {
    unicode: String,
    sha256: String,
    decision: Option<ReviewDecision>,
    reason: String,
}

fn parse_review(text: &str, review_id: &str) -> Result<Vec<ReviewRow>, AssetError> {
    let mut lines = text.lines();
    if lines.next() != Some(format!("# review_id={review_id}").as_str()) {
        return Err(invalid("decisions.tsv не относится к указанному снимку"));
    }
    if lines.next() != Some(REVIEW_HEADER) {
        return Err(invalid("неизвестный заголовок decisions.tsv"));
    }
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        if columns.len() != 8 {
            return Err(invalid(format!(
                "строка {}: ожидается 8 столбцов",
                index + 3
            )));
        }
        let decision = match columns[6].trim() {
            "" | "-" => None,
            "approve" => Some(ReviewDecision::Approve),
            "reject" => Some(ReviewDecision::Reject),
            other => {
                return Err(invalid(format!(
                    "строка {}: неизвестное решение {other:?}; допустимы approve, reject, - или пусто",
                    index + 3
                )));
            }
        };
        let reason = columns[7].trim().to_owned();
        if decision.is_some() && reason.is_empty() {
            return Err(invalid(format!(
                "строка {}: решение требует основания",
                index + 3
            )));
        }
        rows.push(ReviewRow {
            unicode: columns[0].into(),
            sha256: columns[2].into(),
            decision,
            reason,
        });
    }
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|row| (row.unicode.clone(), row.sha256.clone()))
        .collect();
    if review_id_of(&keys) != review_id {
        return Err(invalid(
            "набор строк decisions.tsv изменён после review-export",
        ));
    }
    Ok(rows)
}

/// Применяет решения снимка. Весь файл проверяется до первого изменения;
/// повтор уже применённых решений ничего не меняет.
pub(crate) fn review_apply(
    store: &AssetStore,
    root: &Path,
    review_id: &str,
) -> Result<Value, AssetError> {
    if review_id.len() != 32 || !review_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("review_id — 32 шестнадцатеричных символа"));
    }
    let (work, config) = WorkDir::open_existing(root, store)?;
    ensure_not_linked(&root.join(format!("review/{review_id}")))?;
    let text = String::from_utf8(work.read(&format!("review/{review_id}/decisions.tsv"))?)
        .map_err(|_| invalid("decisions.tsv не в UTF-8"))?;
    let rows = parse_review(&text, review_id)?;
    let mut decided = Vec::new();
    for row in &rows {
        let Some(decision) = row.decision else {
            continue;
        };
        let character = char::from_u32(
            u32::from_str_radix(row.unicode.trim_start_matches("U+"), 16)
                .map_err(|_| invalid(format!("неверная кодовая точка {}", row.unicode)))?,
        )
        .ok_or_else(|| invalid(format!("неверная кодовая точка {}", row.unicode)))?
        .to_string();
        let state = work
            .load_item(&character)?
            .ok_or_else(|| invalid(format!("нет состояния {}", row.unicode)))?;
        if state.rendered_sha() != Some(row.sha256.as_str()) {
            return Err(invalid(format!(
                "{}: SHA-256 строки не совпадает с текущим рендером",
                row.unicode
            )));
        }
        if let Some(previous) = state.review_current()
            && (previous.decision != decision || previous.review_id != review_id)
        {
            return Err(invalid(format!(
                "{}: для этих байтов уже применено другое решение",
                row.unicode
            )));
        }
        if state.review_current().is_none() && state.review_group().is_none() {
            return Err(invalid(format!(
                "{}: символ больше не требует проверки",
                row.unicode
            )));
        }
        decided.push((state, decision, row.reason.clone()));
    }
    let (_, records) = inventory(store)?;
    let mut png_slots = png_slots(records);
    let (mut approved, mut rejected, mut failed) = (0usize, 0usize, 0usize);
    let mut items = Vec::new();
    for (mut state, decision, reason) in decided {
        state.review = Some(ReviewState {
            review_id: review_id.into(),
            sha256: state.rendered_sha().unwrap_or("").to_owned(),
            decision,
            reason: reason.clone(),
        });
        work.save_item(&state)?;
        let outcome = match decision {
            ReviewDecision::Reject => {
                rejected += 1;
                "review_rejected".to_owned()
            }
            ReviewDecision::Approve => {
                let slot = png_slots.get(&state.character).cloned();
                let previous_attempts = state
                    .publication
                    .as_ref()
                    .filter(|publication| publication.outcome == "publish_error")
                    .map_or(0, |publication| publication.attempts);
                // Решение уже сохранено: при отказе публикации повтор того же
                // `review-apply` продолжает её, не требуя нового решения.
                let (outcome, message, attempts) =
                    match publish_reviewed(store, &work, &config, &state, slot, review_id, &reason)
                    {
                        Ok((outcome, message, record)) => {
                            if let Some(record) = record {
                                png_slots.insert(state.character.clone(), record);
                            }
                            match outcome {
                                "published_after_review" | "already_published" => approved += 1,
                                // Исторический оригинал сохранён штатно: это не ошибка
                                // публикации, и повтор снимка её не исправит.
                                "existing_original_kept" => {}
                                _ => failed += 1,
                            }
                            (outcome, message, 0)
                        }
                        Err(error) => {
                            failed += 1;
                            (
                                "publish_error",
                                Some(describe_error(&error)),
                                previous_attempts + 1,
                            )
                        }
                    };
                state.publication = Some(PublicationState {
                    sha256: state.rendered_sha().unwrap_or("").to_owned(),
                    outcome: outcome.into(),
                    message,
                    attempts,
                });
                outcome.to_owned()
            }
        };
        work.save_item(&state)?;
        items.push(json!({
            "character": state.character,
            "unicode": state.unicode,
            "sha256": state.rendered_sha(),
            "outcome": outcome,
        }));
    }
    work.log(&format!(
        "review-apply {review_id}: одобрено {approved}, отклонено {rejected}, ошибок {failed}"
    ));
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png review-apply",
        "outcome": if failed == 0 { "completed" } else { "completed_with_errors" },
        "review_id": review_id,
        "counts": {"rows": rows.len(), "approved": approved, "rejected": rejected, "failed": failed},
        "items": items,
    }))
}

/// Тот же путь хранилища, что у решения человека в пакетах: кандидат `pending`
/// с происхождением локального рендера, точная автоматическая проверка, затем
/// `Approve` по точному SHA. GIF символа не затрагивается.
///
/// Решение относится только к записи этого рендера: запись в слоте с теми же
/// байтами, но другим происхождением или другими свидетельствами не получает
/// доверия, а отклонённая человеком в хранилище — не восстанавливается.
fn publish_reviewed(
    store: &AssetStore,
    work: &WorkDir,
    config: &RunConfig,
    state: &ItemState,
    slot: Option<AssetRecord>,
    review_id: &str,
    reason: &str,
) -> Result<(&'static str, Option<String>, Option<AssetRecord>), AssetError> {
    let render = state
        .render
        .as_ref()
        .ok_or_else(|| invalid("нет рендера для публикации"))?;
    // Свидетельства сверяются до любого изменения хранилища.
    let (_, evidence) = work.verified_render(state, config)?;
    let metadata = local_metadata(state, &evidence);
    let identity = state.identity();
    let validator_identity = KanjiImageValidator::validator_identity();
    let record = match slot {
        Some(record) => match png_slot_status(render, Some(&record)) {
            SlotStatus::Settled("already_published", _)
                if record.is_trusted_for(&validator_identity) =>
            {
                return Ok(("already_published", None, Some(record)));
            }
            SlotStatus::Settled(
                outcome @ ("trust_revoked" | "existing_original_kept"),
                message,
            ) => {
                return Ok((outcome, message, None));
            }
            _ if record.sha256 != render.sha256 => {
                return Ok((
                    "png_slot_occupied",
                    Some(format!("PNG-слот занят записью {}", record.sha256)),
                    None,
                ));
            }
            _ if record.provenance.source_kind != LOCAL_RENDER_SOURCE_KIND
                || record.domain_metadata.as_ref() != Some(&metadata) =>
            {
                return Ok((
                    "png_slot_occupied",
                    Some(format!(
                        "в слоте те же байты с происхождением {} или другими свидетельствами; \
                         решение к ним не относится",
                        record.provenance.source_kind
                    )),
                    None,
                ));
            }
            _ => record,
        },
        None => {
            store
                .ingest_with_provenance(
                    IngestRequest {
                        identity: identity.clone(),
                        source_path: work.root().join(&render.file),
                        expected_source_sha256: Some(render.sha256.clone()),
                        domain_metadata: Some(metadata),
                        replace_expected_sha256: None,
                    },
                    Provenance {
                        source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
                        source_name: SOURCE_NAME.into(),
                    },
                )?
                .asset
        }
    };
    if record.validation.is_none() {
        let report =
            store.validate_exact(&identity, &render.sha256, &KanjiImageValidator::new())?;
        if !report.blockers.is_empty() {
            return Err(AssetError::new(
                ErrorCode::ValidatorFailure,
                "точная автоматическая проверка завершилась техническим отказом",
            ));
        }
    }
    let outcome = store.attest(HumanAttestationRequest {
        identity: identity.clone(),
        expected_sha256: render.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: format!("local-png review {review_id}: {reason}"),
    })?;
    if outcome.asset.sha256 != render.sha256
        || outcome.asset.format != DetectedFormat::Png
        || outcome.asset.lifecycle != LifecycleState::Verified
        || outcome.asset.provenance.source_kind != LOCAL_RENDER_SOURCE_KIND
        || !outcome.asset.is_trusted_for(&validator_identity)
    {
        return Err(invalid(
            "после решения человека хранилище не подтверждает доверие к точным байтам",
        ));
    }
    Ok(("published_after_review", None, Some(outcome.asset)))
}

/// Итоговое состояние символа по его этапам и финальному хранилищу.
pub(crate) fn item_outcome(state: &ItemState, after: &Inventory) -> &'static str {
    if let Some(error) = &state.render_error
        && state.render.is_none()
    {
        return if error.code == "glyph_missing" {
            "glyph_missing"
        } else {
            "render_error"
        };
    }
    let Some(render) = &state.render else {
        return "not_rendered";
    };
    let in_store = after
        .png
        .get(&state.character)
        .is_some_and(|(sha256, kind)| sha256 == &render.sha256 && kind == LOCAL_RENDER_SOURCE_KIND);
    if in_store {
        return match state.review_current() {
            Some(review) if review.decision == ReviewDecision::Approve => "published_after_review",
            _ => "published",
        };
    }
    let publication = state
        .publication
        .as_ref()
        .filter(|publication| publication.sha256 == render.sha256)
        .map(|publication| publication.outcome.as_str());
    match state.review_current().map(|review| review.decision) {
        Some(ReviewDecision::Reject) => return "review_rejected",
        // Решение человека принято, но байты в хранилище не попали: это не
        // ожидание проверки, а незавершённая публикация после `approve`.
        Some(ReviewDecision::Approve) => {
            return match publication {
                Some("publish_error") => "approved_publish_error",
                Some("png_slot_occupied") => "approved_png_slot_occupied",
                // Исторический PNG занял слот после экспорта: повтор этого не изменит.
                Some("existing_original_kept") => "existing_original_kept",
                Some("trust_revoked") => "trust_revoked",
                _ => "approved_awaiting_publication",
            };
        }
        None => {}
    }
    if after
        .png
        .get(&state.character)
        .is_some_and(|(_, kind)| kind != LOCAL_RENDER_SOURCE_KIND)
        && state.origins.contains(&TargetOrigin::ExistingPng)
    {
        return "existing_original_kept";
    }
    match state
        .validation_current()
        .map(|validation| validation.status)
    {
        None => return "awaiting_validation",
        Some(SemanticStatus::Rejected) => return "validator_rejected",
        Some(SemanticStatus::Uncertain) => return "validator_uncertain",
        Some(SemanticStatus::Corrupt) => return "validator_corrupt",
        Some(SemanticStatus::Verified) => {}
    }
    match state.ocr_current() {
        None => "awaiting_ocr",
        Some(ocr) if ocr.failure.is_some() => "ocr_failed",
        Some(ocr) if ocr.observed.as_deref() == Some("?") => "ocr_unrecognized",
        Some(ocr) if !ocr.matches_expected => "ocr_mismatch",
        Some(_) => match publication {
            Some("png_slot_occupied") => "png_slot_occupied",
            Some("publish_error") => "publish_error",
            Some("trust_revoked") => "trust_revoked",
            Some("evidence_mismatch") => "evidence_mismatch",
            Some(outcome) if outcome.starts_with("store_validator_") => "store_validator_refused",
            _ => "awaiting_publication",
        },
    }
}

/// Итоги, после которых у символа осталась работа: повтор `run` или прежнего
/// `review-apply` может их продвинуть. Это признак полноты, а не целостности.
const UNFINISHED_OUTCOMES: &[&str] = &[
    "not_rendered",
    "render_error",
    "awaiting_validation",
    "awaiting_ocr",
    "awaiting_publication",
    "publish_error",
    "evidence_mismatch",
    "approved_awaiting_publication",
    "approved_publish_error",
];

pub(crate) fn outcome_counts(items: &[ItemState], after: &Inventory) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts
            .entry(item_outcome(item, after).to_owned())
            .or_insert(0) += 1;
    }
    counts
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Разница двух PNG, усиленная ×4: только для визуальной диагностики.
fn diff_png(reference: &[u8], generated: &[u8]) -> Result<Vec<u8>, AssetError> {
    let decode = |bytes: &[u8]| {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map(|image| image.to_rgb8())
            .map_err(|error| invalid(format!("декодирование PNG: {error}")))
    };
    let (left, right) = (decode(reference)?, decode(generated)?);
    if left.dimensions() != right.dimensions() {
        return Err(invalid("размеры PNG различаются"));
    }
    let diff = image::RgbImage::from_fn(left.width(), left.height(), |x, y| {
        let (a, b) = (left.get_pixel(x, y), right.get_pixel(x, y));
        image::Rgb(std::array::from_fn(|channel| {
            a[channel].abs_diff(b[channel]).saturating_mul(4)
        }))
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    diff.write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|error| invalid(format!("кодирование PNG: {error}")))?;
    Ok(bytes.into_inner())
}

/// Проверяет, что PNG — нормальное изображение 174×174.
fn png_is_valid(bytes: &[u8]) -> bool {
    DetectedFormat::from_signature(bytes) == DetectedFormat::Png
        && image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .is_ok_and(|image| image.width() == 174 && image.height() == 174)
}

/// Итог чтения опубликованных вариантов одного символа в отчёте.
#[derive(Clone, Default)]
struct PublishedReadCheck {
    png_ok: bool,
    gif_read: bool,
    gif_ok: bool,
    error: Option<String>,
}

fn describe_error(error: &AssetError) -> String {
    format!("{}: {}", error.code.as_str(), error.message)
}

/// Отчёт по рабочему каталогу и финальному состоянию хранилища.
///
/// Возвращает сводку и признак успешной проверки целостности.
pub(crate) fn report(store: &AssetStore, root: &Path) -> Result<(Value, bool), AssetError> {
    let (work, _) = WorkDir::open_existing(root, store)?;
    let items = work.all_items()?;
    let before: Inventory = serde_json::from_slice(&work.read("inventory-before.json")?)
        .map_err(|error| invalid(format!("inventory-before.json: {error}")))?;
    let (after, after_records) = inventory(store)?;
    // Совместное чтение GIF — только для доверенных: прочие GIF не меняются, это проверяет SHA-256.
    let trusted_gif = trusted_gif_characters(&after_records);
    let validator = KanjiImageValidator::validator_identity();
    let report_dir = work.root().join("report");
    ensure_not_linked(&report_dir.join("diff"))?;
    std::fs::create_dir_all(report_dir.join("diff"))
        .map_err(|error| AssetError::io("создание каталога отчёта", error))?;

    // Неизменность каждого исходного GIF по SHA из инвентаря до запуска.
    let mut gif_changed = Vec::new();
    for (key, sha256) in &before.gif_sha256 {
        if after.gif_sha256.get(key) != Some(sha256) {
            gif_changed.push(json!({
                "character": key,
                "before": sha256,
                "after": after.gif_sha256.get(key),
            }));
        }
    }
    // Исторические PNG не заменены.
    let mut original_png_changed = Vec::new();
    for (key, (sha256, kind)) in &before.png {
        if kind != LOCAL_RENDER_SOURCE_KIND
            && after.png.get(key).map(|(sha, _)| sha) != Some(sha256)
        {
            original_png_changed.push(json!({"character": key, "before": sha256}));
        }
    }
    let journal = store.journal_check()?;

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut issues = Vec::new();
    let mut published = Vec::new();
    let mut controls = Vec::new();
    let mut read_failures = Vec::new();
    let mut ocr_stats: BTreeMap<&str, usize> = BTreeMap::new();
    let mut validator_stats: BTreeMap<String, usize> = BTreeMap::new();
    let mut read_requests = Vec::new();
    let mut read_owners = Vec::new();
    for item in &items {
        let outcome = item_outcome(item, &after);
        *counts.entry(outcome.to_owned()).or_insert(0) += 1;
        if let Some(validation) = item.validation_current() {
            *validator_stats
                .entry(validation.status.as_str().to_owned())
                .or_insert(0) += 1;
        }
        if let Some(ocr) = item.ocr_current() {
            let key = if ocr.failure.is_some() {
                "failed"
            } else if ocr.observed.as_deref() == Some("?") {
                "unrecognized"
            } else if ocr.matches_expected {
                "matched"
            } else {
                "mismatch"
            };
            *ocr_stats.entry(key).or_insert(0) += 1;
        } else if item.render.is_some() {
            *ocr_stats.entry("not_run").or_insert(0) += 1;
        }
        if matches!(outcome, "published" | "published_after_review") {
            let index = published.len();
            read_requests.push((item.identity(), DetectedFormat::Png));
            read_owners.push(index);
            if trusted_gif.contains(&item.character) {
                read_requests.push((item.identity(), DetectedFormat::Gif));
                read_owners.push(index);
            }
            published.push(item);
        }
        if let Some(control) = &item.control {
            let render = item.render.as_ref().expect("контроль требует рендера");
            let diff_file = format!("diff/{}.png", item.unicode);
            let diff = diff_png(
                &work.read(&control.reference_file)?,
                &work.read(&render.file)?,
            )?;
            write_atomic(&report_dir.join(&diff_file), &diff)?;
            controls.push((item, control, diff_file));
        }
        let problem = !matches!(
            outcome,
            "published" | "published_after_review" | "existing_original_kept"
        );
        if problem {
            let review = item.review_current();
            // Для принятого, но не опубликованного решения повтор того же снимка
            // продолжает публикацию без нового решения.
            let recovery = outcome.starts_with("approved_").then(|| {
                format!(
                    "local-png review-apply --review-id {}",
                    review.map_or("", |review| review.review_id.as_str())
                )
            });
            issues.push(json!({
                "character": item.character,
                "unicode": item.unicode,
                "outcome": outcome,
                "expected": item.character,
                "observed": item.ocr_current().and_then(|ocr| ocr.observed.clone()),
                "ocr_failure": item.ocr_current().and_then(|ocr| ocr.failure.clone()),
                "sha256": item.rendered_sha(),
                "validator_status": item.validation_current().map(|v| v.status.as_str()),
                "validator_summary": item.validation_current()
                    .and_then(|v| v.evidence.first().map(|e| e.summary.clone())),
                "render_error": item.render_error,
                "publication": item.publication,
                "review": review,
                "recovery": recovery,
                "control_skip": item.control_skip,
                "artifact": item.render.as_ref().map(|render| render.file.clone()),
                "state_file": format!("items/{}.json", item.unicode),
            }));
        }
    }

    // Все опубликованные варианты читаются одним согласованным снимком: манифест и
    // корпус проверяются один раз, байты каждого запроса — по своей записи.
    let mut checks = vec![PublishedReadCheck::default(); published.len()];
    let snapshot = AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &read_requests,
        &validator,
        &KanjiDomainPolicy,
        |request, read| {
            let item = published[read_owners[request]];
            let check = &mut checks[read_owners[request]];
            let read = match read {
                Ok(read) => read,
                Err(error) => {
                    check.error.get_or_insert_with(|| describe_error(&error));
                    return Ok(());
                }
            };
            match read_requests[request].1 {
                DetectedFormat::Png => {
                    let render = item.render.as_ref().expect("опубликован рендер");
                    check.png_ok = read.record.sha256 == render.sha256
                        && read.record.consumer_filename == format!("{}.png", item.character)
                        && png_is_valid(&read.bytes);
                }
                _ => {
                    check.gif_read = true;
                    // Неизменность исходных GIF проверяет `gif_changed`; здесь прочитанные
                    // байты сверяются с текущим инвентарём, в том числе для нового GIF.
                    check.gif_ok = Some(&read.record.sha256)
                        == after.gif_sha256.get(&item.character)
                        && read.record.consumer_filename == format!("{}.gif", item.character);
                }
            }
            Ok(())
        },
    );
    if let Err(error) = snapshot {
        let error = describe_error(&error);
        for check in &mut checks {
            check.error = Some(error.clone());
        }
    }
    let mut dual_read_checked = 0usize;
    for (item, check) in published.iter().zip(&checks) {
        if let Some(error) = &check.error {
            read_failures.push(json!({"character": item.character, "error": error}));
            continue;
        }
        let gif_ok = !check.gif_read || check.gif_ok;
        if check.gif_read {
            dual_read_checked += 1;
        }
        if !check.png_ok || !gif_ok {
            read_failures.push(
                json!({"character": item.character, "png_ok": check.png_ok, "gif_ok": gif_ok}),
            );
        }
    }

    let mean = |selector: fn(&crate::local_render::ImageComparison) -> f64| {
        if controls.is_empty() {
            0.0
        } else {
            controls
                .iter()
                .map(|(_, control, _)| selector(&control.metrics))
                .sum::<f64>()
                / controls.len() as f64
        }
    };
    let control_mean = json!({
        "count": controls.len(),
        "mae": mean(|m| m.mae),
        "pixel_similarity": mean(|m| m.pixel_similarity),
        "glyph_iou": mean(|m| m.glyph_iou),
        "exact_pixel_fraction": mean(|m| m.exact_pixel_fraction),
    });
    let unfinished: usize = counts
        .iter()
        .filter(|(outcome, _)| UNFINISHED_OUTCOMES.contains(&outcome.as_str()))
        .map(|(_, count)| count)
        .sum();
    let healthy = gif_changed.is_empty()
        && original_png_changed.is_empty()
        && read_failures.is_empty()
        && journal.is_clean();
    let ocr_model: Option<Value> = work
        .read("ocr-model.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let run_config: Option<Value> = work
        .read("run.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let summary = json!({
        "schema_version": 1,
        "operation": "local-png report",
        "outcome": if healthy { "completed" } else { "integrity_problems" },
        "work_dir": work.root().display().to_string(),
        "items": items.len(),
        "counts": counts,
        "validator": validator_stats,
        "ocr": ocr_stats,
        "ocr_model": ocr_model.as_ref().and_then(|model| model.get("summary").cloned()),
        "controls": control_mean,
        // Полнота прогона отдельно от целостности хранилища: незавершённые
        // символы не делают хранилище повреждённым и не меняют код возврата.
        "progress": {
            "finished": unfinished == 0,
            "unfinished": unfinished,
        },
        "integrity": {
            "healthy": healthy,
            "store_verify_integrity": true,
            "journal_clean": journal.is_clean(),
            "gif_before": before.gif_sha256.len(),
            "gif_after": after.gif_sha256.len(),
            "gif_changed": gif_changed,
            "original_png_changed": original_png_changed,
            "published_read_failures": read_failures,
            "published_with_gif_read_together": dual_read_checked,
        },
        "store_before": {
            "records": before.records_total,
            "gif": before.verified_gif_records,
            "trusted_gif_characters": before.trusted_gif_characters,
            "png": before.png_records,
            "png_local_render": before.png_local_render_records,
            "gif_and_png": before.characters_with_gif_and_png,
        },
        "store_after": {
            "records": after.records_total,
            "gif": after.verified_gif_records,
            "trusted_gif_characters": after.trusted_gif_characters,
            "png": after.png_records,
            "png_local_render": after.png_local_render_records,
            "png_other": after.png_other_records,
            "gif_and_png": after.characters_with_gif_and_png,
            "runtime_records": after.runtime_records,
        },
        "run": run_config,
    });
    write_atomic(
        &report_dir.join("summary.json"),
        &serde_json::to_vec_pretty(&summary).expect("сводка сериализуется"),
    )?;
    write_atomic(
        &report_dir.join("issues.json"),
        &serde_json::to_vec_pretty(&issues).expect("реестр сериализуется"),
    )?;
    let html = report_html(&summary, &controls, &published, &issues);
    write_atomic(&report_dir.join("index.html"), html.as_bytes())?;
    Ok((
        json!({
            "schema_version": 1,
            "operation": "local-png report",
            "outcome": summary["outcome"],
            "report": report_dir.join("index.html").display().to_string(),
            "counts": summary["counts"],
            "progress": summary["progress"],
            "integrity": summary["integrity"]["healthy"],
        }),
        healthy,
    ))
}

fn zoom_box(src: &str, x: u32, y: u32) -> String {
    // Фрагмент 48×48 px исходника при увеличении ×4 без сглаживания.
    format!(
        "<div class=zoom style=\"background-image:url('{src}');background-position:-{}px -{}px\"></div>",
        x * 4,
        y * 4
    )
}

fn report_html(
    summary: &Value,
    controls: &[(&ItemState, &ControlState, String)],
    published: &[&ItemState],
    issues: &[Value],
) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=ru><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Локальные PNG кандзи</title><style>\
         :root{--bg:#f6f6f4;--fg:#1d1d1b;--muted:#6a6a66;--card:#fff;--line:#d9d9d4;--bad:#a8321e;--ok:#2d6a3e}\
         @media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ececea;--muted:#9a9a95;--card:#20201f;--line:#3a3a37;--bad:#e27a63;--ok:#7cc48f}}\
         body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}\
         main{max-width:1200px;margin:0 auto;padding:16px}h1{font-size:22px}h2{font-size:18px;margin-top:32px}\
         table{border-collapse:collapse;width:100%;font-size:13px}td,th{border-bottom:1px solid var(--line);padding:4px 6px;text-align:left;vertical-align:middle}\
         .grid{display:flex;flex-wrap:wrap;gap:12px}.card{background:var(--card);border:1px solid var(--line);border-radius:6px;padding:8px}\
         .card img{display:block;width:174px;height:174px;image-rendering:pixelated}\
         .zoom{width:192px;height:192px;background-size:696px 696px;image-rendering:pixelated;border:1px solid var(--line)}\
         .row{display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start}.muted{color:var(--muted)}.bad{color:var(--bad)}.ok{color:var(--ok)}\
         pre{white-space:pre-wrap;font-size:12px;background:var(--card);border:1px solid var(--line);padding:8px;overflow:auto}\
         .tbl{overflow-x:auto}</style><main>",
    );
    html.push_str("<h1>Локальная генерация PNG кандзи</h1>");
    let integrity = &summary["integrity"];
    let healthy = integrity["healthy"].as_bool().unwrap_or(false);
    html.push_str(&format!(
        "<p class={}>Целостность: {}. Журнал согласован: {}. GIF до/после: {}/{}; изменённых GIF: {}; изменённых исторических PNG: {}; ошибок чтения опубликованных: {}.</p>",
        if healthy { "ok" } else { "bad" },
        if healthy { "проверена" } else { "есть проблемы" },
        integrity["journal_clean"],
        integrity["gif_before"],
        integrity["gif_after"],
        integrity["gif_changed"].as_array().map_or(0, Vec::len),
        integrity["original_png_changed"].as_array().map_or(0, Vec::len),
        integrity["published_read_failures"].as_array().map_or(0, Vec::len),
    ));
    html.push_str("<h2>Итоги</h2><pre>");
    html.push_str(&html_escape(
        &serde_json::to_string_pretty(&json!({
            "counts": summary["counts"],
            "validator": summary["validator"],
            "ocr": summary["ocr"],
            "ocr_model": summary["ocr_model"],
            "store_before": summary["store_before"],
            "store_after": summary["store_after"],
            "controls_mean": summary["controls"],
        }))
        .unwrap_or_default(),
    ));
    html.push_str("</pre>");

    html.push_str("<h2>Контроль: исторические PNG и новый Rust-рендер</h2>\
        <p class=muted>Слева оригинал из хранилища, затем рендер, разница ×4 и увеличенный ×4 фрагмент 48×48 (верхний левый угол рамки и центр глифа). Геометрия — общий профиль из настроек прогона; таблицы «символ → фаза» нет.</p>");
    for (item, control, diff_file) in controls {
        let render = item.render.as_ref().expect("контроль требует рендера");
        let reference = format!("../{}", control.reference_file);
        let generated = format!("../{}", render.file);
        html.push_str(&format!(
            "<div class=card><b>{} {}</b> <span class=muted>IoU {:.4}%, similarity {:.4}%, MAE {:.4}, точные пиксели {:.4}%</span>\
             <div class=row><img src='{}' alt=оригинал><img src='{}' alt=рендер><img src='{}' alt=разница>{}{}{}{}</div></div>",
            html_escape(&item.character),
            item.unicode,
            control.metrics.glyph_iou * 100.0,
            control.metrics.pixel_similarity * 100.0,
            control.metrics.mae,
            control.metrics.exact_pixel_fraction * 100.0,
            html_escape(&reference),
            html_escape(&generated),
            html_escape(diff_file),
            zoom_box(&html_escape(&reference), 0, 0),
            zoom_box(&html_escape(&generated), 0, 0),
            zoom_box(&html_escape(&reference), 63, 63),
            zoom_box(&html_escape(&generated), 63, 63),
        ));
    }

    let mut by_area: Vec<&&ItemState> = published.iter().collect();
    by_area.sort_by_key(|item| item.render.as_ref().map_or(0, |render| render.glyph_area));
    let sample = |title: &str, items: &[&&ItemState], html: &mut String| {
        html.push_str(&format!("<h2>{title}</h2><div class=grid>"));
        for item in items {
            let render = item.render.as_ref().expect("опубликован рендер");
            html.push_str(&format!(
                "<div class=card><img src='../{}' alt='{}'><div>{} {} · OCR {}</div><div class=muted>{}</div></div>",
                html_escape(&render.file),
                html_escape(&item.character),
                html_escape(&item.character),
                item.unicode,
                html_escape(item.ocr_current().and_then(|ocr| ocr.observed.as_deref()).unwrap_or("—")),
                &render.sha256[..16],
            ));
        }
        html.push_str("</div>");
    };
    let count = by_area.len().min(12);
    sample(
        "Простые опубликованные (меньшая площадь глифа)",
        &by_area[..count],
        &mut html,
    );
    sample(
        "Сложные опубликованные (большая площадь глифа)",
        &by_area[by_area.len() - count..],
        &mut html,
    );

    html.push_str(&format!(
        "<h2>Реестр проблем ({})</h2><p class=muted>Полный машиночитаемый список — issues.json. Такие кандидаты не публиковались.</p><div class=tbl><table><tr><th>Символ</th><th>Unicode</th><th>Исход</th><th>Ожидался</th><th>OCR</th><th>Валидатор</th><th>SHA-256</th><th>Артефакт</th></tr>",
        issues.len()
    ));
    for issue in issues {
        let artifact = issue["artifact"].as_str().unwrap_or("");
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=bad>{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
            html_escape(issue["character"].as_str().unwrap_or("")),
            html_escape(issue["unicode"].as_str().unwrap_or("")),
            html_escape(issue["outcome"].as_str().unwrap_or("")),
            html_escape(issue["expected"].as_str().unwrap_or("")),
            html_escape(
                issue["observed"]
                    .as_str()
                    .or_else(|| issue["ocr_failure"].as_str())
                    .unwrap_or("—")
            ),
            html_escape(issue["validator_status"].as_str().unwrap_or("—")),
            html_escape(issue["sha256"].as_str().map_or("—", |sha| &sha[..16.min(sha.len())])),
            if artifact.is_empty() {
                "—".to_owned()
            } else {
                format!("<a href='../{0}'>{0}</a>", html_escape(artifact))
            },
        ));
    }
    html.push_str("</table></div></main></html>\n");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreOptions;
    use crate::temp_workspace::TempWorkspace;

    fn inventory_with(png: &[(&str, &str, &str)]) -> Inventory {
        Inventory {
            captured_at_unix: 0,
            store_id: "store".into(),
            records_total: png.len(),
            verified_gif_records: 0,
            trusted_gif_characters: 0,
            untrusted_gif_records: 0,
            png_records: png.len(),
            png_local_render_records: 0,
            png_other_records: 0,
            characters_with_gif_and_png: 0,
            runtime_records: 0,
            gif_sha256: BTreeMap::new(),
            png: png
                .iter()
                .map(|(character, sha256, kind)| {
                    ((*character).into(), ((*sha256).into(), (*kind).into()))
                })
                .collect(),
        }
    }

    fn rendered(character: &str, sha256: &str) -> ItemState {
        let mut state = ItemState::new(character);
        state.origins.insert(TargetOrigin::CorpusGif);
        state.render = Some(RenderState {
            sha256: sha256.into(),
            raw_sha256: "raw".into(),
            byte_length: 1,
            file: "png/U+4E00.png".into(),
            raw_file: "raw/U+4E00.png".into(),
            evidence_file: "evidence/U+4E00.json".into(),
            glyph_area: 1,
        });
        state
    }

    fn validated(mut state: ItemState, status: SemanticStatus) -> ItemState {
        state.validation = Some(ValidationState {
            sha256: state.render.as_ref().unwrap().sha256.clone(),
            status,
            validator: KanjiImageValidator::validator_identity(),
            evidence: Vec::new(),
        });
        state
    }

    fn recognized(mut state: ItemState, observed: &str) -> ItemState {
        state.ocr = Some(OcrState {
            sha256: state.render.as_ref().unwrap().sha256.clone(),
            observed: Some(observed.into()),
            matches_expected: observed == state.character,
            failure: None,
            prepared_sha256: None,
            model: "model".into(),
            provenance_sha256: "provenance".into(),
            elapsed_ms: None,
            attempts: 0,
        });
        state
    }

    fn ocr_failed(mut state: ItemState, failure: &str, attempts: u32) -> ItemState {
        state = recognized(state, "?");
        let ocr = state.ocr.as_mut().unwrap();
        ocr.observed = None;
        ocr.failure = Some(failure.into());
        ocr.attempts = attempts;
        state
    }

    fn publication(state: &ItemState, outcome: &str, attempts: u32) -> PublicationState {
        PublicationState {
            sha256: state.rendered_sha().unwrap().into(),
            outcome: outcome.into(),
            message: None,
            attempts,
        }
    }

    fn render_failed(character: &str, code: &str, attempts: u32) -> ItemState {
        let mut state = ItemState::new(character);
        state.render_error = Some(StageError {
            code: code.into(),
            message: code.into(),
            attempts,
        });
        state
    }

    const fn selection(
        ocr: bool,
        rescan_ocr: bool,
        publish: bool,
        retry_failed: bool,
    ) -> Selection {
        Selection {
            ocr,
            rescan_ocr,
            publish,
            retry_failed,
        }
    }

    /// Хранилище, рабочий каталог и `run.json`, созданные штатным путём `run`.
    struct Fixture {
        workspace: TempWorkspace,
        store: AssetStore,
        work: WorkDir,
        config: RunConfig,
    }

    fn test_config(store: &AssetStore) -> RunConfig {
        RunConfig {
            schema_version: RUN_SCHEMA,
            algorithm: LOCAL_RENDER_ALGORITHM.into(),
            settings: RenderSettings::for_profile(GeometryProfile::B),
            font_sha256: "f".repeat(64),
            store_id: store.store_id().into(),
        }
    }

    fn fixture(purpose: &str) -> Fixture {
        let workspace = TempWorkspace::create(purpose).unwrap();
        let store =
            AssetStore::open_kanji(StoreOptions::new(workspace.path().join("corpus"))).unwrap();
        let config = test_config(&store);
        let work = WorkDir::create(&workspace.path().join("work"), &store, &config).unwrap();
        Fixture {
            workspace,
            store,
            work,
            config,
        }
    }

    /// PNG 174×174: тёмная плитка с горизонтальной чертой, как у «一».
    fn tile_png() -> Vec<u8> {
        let mut image = image::RgbImage::from_pixel(174, 174, image::Rgb([34, 34, 34]));
        for y in 82..92 {
            for x in 30..144 {
                image.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let mut encoded = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    fn render_evidence(sha256: &str) -> RenderEvidence {
        RenderEvidence {
            character: "一".into(),
            settings: crate::local_render::RenderSettings::for_profile(
                crate::local_render::GeometryProfile::B,
            ),
            font: crate::local_render::FontEvidence {
                sha256: "f".repeat(64),
                file_name: "font.otf".into(),
                family_name: "Noto Serif JP".into(),
                version: "1.001".into(),
                calibrated: true,
            },
            browser_product: "Chrome".into(),
            browser_revision: "1".into(),
            platform_fonts: Vec::new(),
            tile_rect_css: [0.0; 4],
            glyph_rect_css: [0.0; 4],
            measured_device_pixel_ratio: 1.0,
            fonts_check: true,
            raw_sha256: sha256.into(),
            corrected_sha256: sha256.into(),
            blocked_network_requests: 0,
        }
    }

    fn write_evidence(work: &WorkDir, evidence: &RenderEvidence) {
        write_atomic(
            &work.root().join("evidence/U+4E00.json"),
            &serde_json::to_vec(evidence).unwrap(),
        )
        .unwrap();
    }

    /// Согласованные артефакты рендера «一»: PNG, сырой снимок и свидетельства.
    fn materialize(work: &WorkDir, png: &[u8]) -> ItemState {
        let sha = sha256_hex(png);
        write_atomic(&work.root().join("png/U+4E00.png"), png).unwrap();
        write_atomic(&work.root().join("raw/U+4E00.png"), png).unwrap();
        write_evidence(work, &render_evidence(&sha));
        let mut state = rendered("一", &sha);
        let render = state.render.as_mut().unwrap();
        render.raw_sha256 = sha.clone();
        render.byte_length = png.len() as u64;
        state
    }

    fn slot_record(sha256: &str, source_kind: &str, lifecycle: LifecycleState) -> AssetRecord {
        AssetRecord {
            identity: ItemState::new("一").identity(),
            storage_path: "assets/png/一.png".into(),
            consumer_filename: "一.png".into(),
            sha256: sha256.into(),
            byte_length: 1,
            format: DetectedFormat::Png,
            provenance: Provenance {
                source_kind: source_kind.into(),
                source_name: SOURCE_NAME.into(),
            },
            lifecycle,
            validation: None,
            human_attestation: None,
            domain_metadata: None,
        }
    }

    /// Заполняет единственную строку снимка решением `decision`.
    fn decide(work: &WorkDir, store: &AssetStore, decision: &str) -> String {
        let exported = review_export(store, work.root()).unwrap();
        let path = work.root().join(exported["decisions"].as_str().unwrap());
        let filled = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\t\t\n", &format!("\t{decision}\tглиф верный\n"));
        std::fs::write(&path, filled).unwrap();
        exported["review_id"].as_str().unwrap().to_owned()
    }

    fn top_level_names(path: &Path) -> BTreeSet<String> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn publication_requires_verified_validator_and_matching_blind_ocr() {
        let empty = inventory_with(&[]);
        let passing = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        assert!(passing.passes_gates());
        assert_eq!(item_outcome(&passing, &empty), "awaiting_publication");

        let mismatch = recognized(
            validated(rendered("丞", "a"), SemanticStatus::Verified),
            "承",
        );
        assert!(!mismatch.passes_gates());
        assert_eq!(item_outcome(&mismatch, &empty), "ocr_mismatch");

        let unknown = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "?",
        );
        assert!(!unknown.passes_gates());
        assert_eq!(item_outcome(&unknown, &empty), "ocr_unrecognized");

        for (status, outcome) in [
            (SemanticStatus::Uncertain, "validator_uncertain"),
            (SemanticStatus::Rejected, "validator_rejected"),
        ] {
            let state = recognized(validated(rendered("一", "a"), status), "一");
            assert!(!state.passes_gates());
            assert_eq!(item_outcome(&state, &empty), outcome);
        }
    }

    #[test]
    fn decisions_for_other_bytes_do_not_count_after_rerender() {
        let mut state = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        state.render.as_mut().unwrap().sha256 = "b".into();
        assert!(!state.passes_gates());
        assert_eq!(
            item_outcome(&state, &inventory_with(&[])),
            "awaiting_validation"
        );
    }

    #[test]
    fn only_local_render_with_the_same_sha_counts_as_published() {
        let state = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        let published = inventory_with(&[("一", "a", LOCAL_RENDER_SOURCE_KIND)]);
        assert_eq!(item_outcome(&state, &published), "published");
        let other_bytes = inventory_with(&[("一", "b", LOCAL_RENDER_SOURCE_KIND)]);
        assert_eq!(item_outcome(&state, &other_bytes), "awaiting_publication");

        let mut control = state.clone();
        control.origins.insert(TargetOrigin::ExistingPng);
        let historical = inventory_with(&[("一", "a", "local-directory")]);
        assert_eq!(
            item_outcome(&control, &historical),
            "existing_original_kept"
        );
    }

    #[test]
    fn review_groups_cover_only_bytes_that_missed_the_automatic_gates() {
        let passing = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        assert_eq!(passing.review_group(), None);
        let doubt = recognized(
            validated(rendered("一", "a"), SemanticStatus::Uncertain),
            "一",
        );
        assert_eq!(doubt.review_group(), Some("validator_doubt_ocr_match"));
        let mismatch = recognized(
            validated(rendered("丞", "a"), SemanticStatus::Verified),
            "承",
        );
        assert_eq!(mismatch.review_group(), Some("ocr_mismatch"));
        let corrupt = recognized(
            validated(rendered("一", "a"), SemanticStatus::Corrupt),
            "一",
        );
        assert_eq!(corrupt.review_group(), None);
        assert_eq!(
            validated(rendered("一", "a"), SemanticStatus::Uncertain).review_group(),
            None
        );
    }

    #[test]
    fn review_file_is_bound_to_its_exact_row_set() {
        let rows = vec![("U+4E00".to_owned(), "a".repeat(64))];
        let id = review_id_of(&rows);
        let line = format!("U+4E00\t一\t{}\tocr_mismatch\tverified\t二", "a".repeat(64));
        let file = |decision: &str, reason: &str| {
            format!("# review_id={id}\n{REVIEW_HEADER}\n{line}\t{decision}\t{reason}\n")
        };
        let parsed = parse_review(&file("approve", "глиф верный"), &id).unwrap();
        assert_eq!(parsed[0].decision, Some(ReviewDecision::Approve));
        assert!(
            parse_review(&file("", ""), &id).unwrap()[0]
                .decision
                .is_none()
        );
        assert!(parse_review(&file("approve", ""), &id).is_err());
        assert!(parse_review(&file("maybe", "x"), &id).is_err());
        let foreign = file("approve", "x").replace(&"a".repeat(64), &"b".repeat(64));
        assert!(parse_review(&foreign, &id).is_err());
    }

    #[test]
    fn missing_glyph_is_reported_without_an_image() {
        let state = render_failed("一", "glyph_missing", 1);
        assert_eq!(item_outcome(&state, &inventory_with(&[])), "glyph_missing");
        assert_eq!(state.unicode, "U+4E00");
    }

    #[test]
    fn state_files_without_retry_fields_remain_readable() {
        let mut state = ocr_failed(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "inference_timeout",
            2,
        );
        state.publication = Some(publication(&state, "publish_error", 1));
        let mut value = serde_json::to_value(&state).unwrap();
        value["ocr"].as_object_mut().unwrap().remove("attempts");
        value["publication"]
            .as_object_mut()
            .unwrap()
            .remove("attempts");
        value.as_object_mut().unwrap().remove("control_skip");
        let parsed: ItemState = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.ocr.unwrap().attempts, 0);
        assert_eq!(parsed.publication.unwrap().attempts, 0);
        assert!(parsed.control_skip.is_none());
    }

    #[test]
    fn limit_skips_characters_whose_stages_are_finished() {
        let fixture = fixture("local-png-pending-test");
        let (work, config) = (&fixture.work, &fixture.config);
        let state = materialize(work, &tile_png());
        let sha = state.rendered_sha().unwrap().to_owned();
        let finished = |status| recognized(validated(state.clone(), status), "一");
        let pending = |state: &ItemState, selection: Selection, slot: Option<&AssetRecord>| {
            has_pending_work(work, config, state, selection, slot)
        };
        let plain = selection(true, false, true, false);
        let local = slot_record(&sha, LOCAL_RENDER_SOURCE_KIND, LifecycleState::Verified);

        // Нет рендера — работа есть; нет глифа — окончательно нет даже с --retry-failed.
        assert!(pending(&ItemState::new("一"), plain, None));
        let missing = render_failed("一", "glyph_missing", 1);
        assert!(!pending(&missing, plain, None));
        assert!(!pending(&missing, selection(true, false, true, true), None));

        // Без --publish готовый символ не занимает предел; с --publish ждёт публикации.
        let passing = finished(SemanticStatus::Verified);
        assert!(!pending(
            &passing,
            selection(true, false, false, false),
            None
        ));
        assert!(pending(&passing, plain, None));
        let mut published = passing.clone();
        published.publication = Some(publication(&passing, "published", 0));
        assert!(!pending(&published, plain, Some(&local)));
        // Сохранённый итог не заменяет слот: исчезнувшая запись снова требует публикации.
        assert!(pending(&published, plain, None));

        // Ошибка публикации повторяется ограниченно, затем — только по --retry-failed.
        let mut failed = passing.clone();
        failed.publication = Some(publication(&passing, "publish_error", 1));
        assert!(pending(&failed, plain, None));
        failed.publication = Some(publication(&passing, "publish_error", MAX_AUTO_ATTEMPTS));
        assert!(!pending(&failed, plain, None));
        assert!(pending(&failed, selection(true, false, true, true), None));

        // Не прошедший ворота символ ждёт проверки человеком, а не новых этапов.
        assert!(!pending(&finished(SemanticStatus::Uncertain), plain, None));

        // OCR ещё нужен, а --rescan возвращает символ в работу.
        let unrecognized = validated(state.clone(), SemanticStatus::Verified);
        assert!(pending(
            &unrecognized,
            selection(true, false, false, false),
            None
        ));
        assert!(!pending(
            &unrecognized,
            selection(false, false, false, false),
            None
        ));
        assert!(pending(&passing, selection(true, true, false, false), None));

        // Недопустимый ответ модели повторяется только по --rescan.
        let answer = ocr_failed(unrecognized.clone(), "invalid_observed_json", 1);
        assert!(!pending(&answer, selection(true, false, false, true), None));
        assert!(pending(&answer, selection(true, true, false, false), None));
        // Технический отказ — ограниченное число раз.
        let technical = ocr_failed(unrecognized.clone(), "inference_timeout", 1);
        assert!(pending(
            &technical,
            selection(true, false, false, false),
            None
        ));
        let exhausted = ocr_failed(unrecognized, "inference_timeout", MAX_AUTO_ATTEMPTS);
        assert!(!pending(
            &exhausted,
            selection(true, false, false, false),
            None
        ));
        assert!(pending(
            &exhausted,
            selection(true, false, false, true),
            None
        ));

        // Отказы рендера: временный — ограниченно, устойчивый — только по --retry-failed.
        assert!(pending(
            &render_failed("一", "browser_timeout", 1),
            plain,
            None
        ));
        let browser = render_failed("一", "browser_timeout", MAX_AUTO_ATTEMPTS);
        assert!(!pending(&browser, plain, None));
        assert!(pending(&browser, selection(true, false, true, true), None));
        let fallback = render_failed("一", "font_fallback", 1);
        assert!(!pending(&fallback, plain, None));
        assert!(pending(&fallback, selection(true, false, true, true), None));

        // Свидетельства, расходящиеся с прогоном, делают рендер нецелым.
        let mut foreign = render_evidence(&sha);
        foreign.font.sha256 = "e".repeat(64);
        write_evidence(work, &foreign);
        assert!(!work.render_intact(&passing, config));
        assert!(pending(
            &passing,
            selection(false, false, false, false),
            None
        ));
    }

    #[test]
    fn limit_reaches_characters_after_persistent_failures() {
        let fixture = fixture("local-png-limit-test");
        let states = vec![
            render_failed("一", "font_fallback", 1),
            render_failed("丁", "browser_timeout", MAX_AUTO_ATTEMPTS),
            ItemState::new("七"),
        ];
        let take = |retry_failed| {
            limit_targets(
                &fixture.work,
                &fixture.config,
                states.clone(),
                1,
                selection(false, false, false, retry_failed),
                &BTreeMap::new(),
            )
            .into_iter()
            .map(|state| state.character)
            .collect::<Vec<_>>()
        };
        assert_eq!(take(false), ["七"]);
        assert_eq!(take(true), ["一"]);
    }

    #[test]
    fn interrupted_control_resumes_and_a_justified_skip_is_final() {
        let fixture = fixture("local-png-control-test");
        let mut state = validated(
            materialize(&fixture.work, &tile_png()),
            SemanticStatus::Verified,
        );
        state.origins.insert(TargetOrigin::ExistingPng);
        let without_stages = selection(false, false, false, false);
        let pending = |state: &ItemState, selection| {
            has_pending_work(&fixture.work, &fixture.config, state, selection, None)
        };
        // Рендер и проверка сохранены, контроль — нет: символ остаётся в работе.
        assert!(pending(&state, without_stages));

        // В хранилище нет исторического PNG: пропуск сохраняется и не занимает предел.
        let mut states = vec![state];
        control_stage(&fixture.work, &fixture.store, false, &mut states).unwrap();
        let skip = states[0].control_skip.clone().expect("пропуск сохранён");
        assert_eq!(Some(skip.sha256.as_str()), states[0].rendered_sha());
        assert!(states[0].control.is_none());
        assert!(!pending(&states[0], without_stages));
        assert_eq!(
            fixture.work.load_item("一").unwrap().as_ref(),
            Some(&states[0])
        );
        assert!(pending(&states[0], selection(false, false, false, true)));
    }

    #[test]
    fn work_dir_overlapping_the_store_is_refused_before_any_write() {
        let fixture = fixture("local-png-store-boundary-test");
        let corpus = fixture.store.root().to_path_buf();
        let before = top_level_names(&corpus);
        // Путь временного каталога содержит UID: в сообщения попадает только метка случая.
        let refuse = |case: &str, path: &Path| {
            let error = WorkDir::create(path, &fixture.store, &fixture.config)
                .err()
                .expect("пересечение отклонено");
            assert_eq!(error.code, ErrorCode::BoundaryViolation, "{case}");
            assert_eq!(
                WorkDir::open_existing(path, &fixture.store)
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::BoundaryViolation
            );
        };
        refuse("корень хранилища", &corpus);
        refuse("внутри хранилища", &corpus.join("run"));
        refuse("внутри области assets", &corpus.join("assets/run"));
        refuse("содержит хранилище", fixture.workspace.path());
        #[cfg(unix)]
        {
            let link = fixture.workspace.path().join("alias");
            std::os::unix::fs::symlink(&corpus, &link).unwrap();
            refuse("symlink на хранилище", &link);
            refuse("путь через symlink", &link.join("run"));

            // Подкаталог рабочего каталога, ведущий через symlink в хранилище.
            let linked = fixture.workspace.path().join("linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink(&corpus, linked.join("items")).unwrap();
            let error = WorkDir::create(&linked, &fixture.store, &fixture.config)
                .err()
                .unwrap();
            assert_eq!(error.code, ErrorCode::BoundaryViolation);
        }
        assert_eq!(top_level_names(&corpus), before);
        let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&corpus)).unwrap();
        reopened.verify_integrity().unwrap();
        assert!(reopened.journal_check().unwrap().is_clean());

        // Обычный каталог рядом с хранилищем допустим.
        WorkDir::create(
            &fixture.workspace.path().join("other-run"),
            &fixture.store,
            &fixture.config,
        )
        .unwrap();
    }

    #[test]
    fn work_state_of_one_store_is_not_applied_to_another() {
        let fixture = fixture("local-png-store-binding-test");
        let state = recognized(
            validated(
                materialize(&fixture.work, &tile_png()),
                SemanticStatus::Uncertain,
            ),
            "二",
        );
        fixture.work.save_item(&state).unwrap();
        let review_id = decide(&fixture.work, &fixture.store, "approve");
        let decisions = fixture
            .work
            .root()
            .join(format!("review/{review_id}/decisions.tsv"));
        let filled = std::fs::read(&decisions).unwrap();

        let other = AssetStore::open_kanji(StoreOptions::new(
            fixture.workspace.path().join("other-corpus"),
        ))
        .unwrap();
        let root = fixture.work.root();
        let refused = [
            review_export(&other, root).err(),
            review_apply(&other, root, &review_id).err(),
            report(&other, root).err(),
        ];
        for error in refused {
            assert_eq!(
                error.expect("чужое хранилище отклонено").code,
                ErrorCode::InvalidTransition
            );
        }
        assert!(inventory(&other).unwrap().0.png.is_empty());
        assert_eq!(other.verify_integrity().unwrap().len(), 0);
        assert_eq!(fixture.work.load_item("一").unwrap().unwrap(), state);
        assert_eq!(std::fs::read(&decisions).unwrap(), filled);
        // Повторный `run` с другим хранилищем тоже отклоняется.
        assert!(WorkDir::create(root, &other, &test_config(&other)).is_err());

        // Своё хранилище применяет решения штатно.
        let applied = review_apply(&fixture.store, root, &review_id).unwrap();
        assert_eq!(applied["items"][0]["outcome"], "published_after_review");

        // Отсутствующий или повреждённый run.json не допускается.
        let run_json = root.join("run.json");
        std::fs::write(&run_json, b"{").unwrap();
        assert!(review_export(&fixture.store, root).is_err());
        std::fs::remove_file(&run_json).unwrap();
        assert!(report(&fixture.store, root).is_err());
        assert!(WorkDir::create(root, &fixture.store, &fixture.config).is_err());
    }

    #[test]
    fn review_apply_publishes_approved_bytes_and_repeats_without_changes() {
        let fixture = fixture("local-png-review-apply-test");
        let (store, work) = (&fixture.store, &fixture.work);
        // Валидатор в сомнении, слепой OCR не совпал: символ ждёт решения человека.
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Uncertain),
            "二",
        );
        let sha = state.rendered_sha().unwrap().to_owned();
        assert_eq!(state.review_group(), Some("validator_doubt_ocr_mismatch"));
        work.save_item(&state).unwrap();
        let review_id = decide(work, store, "approve");

        let outcome = |summary: &Value| summary["items"][0]["outcome"].as_str().unwrap().to_owned();
        let first = review_apply(store, work.root(), &review_id).unwrap();
        assert_eq!(outcome(&first), "published_after_review");
        assert_eq!(first["counts"]["approved"], 1);
        let (published, _) = inventory(store).unwrap();
        assert_eq!(
            published.png.get("一"),
            Some(&(sha.clone(), LOCAL_RENDER_SOURCE_KIND.to_owned()))
        );
        let journal = store.journal_check().unwrap();

        // Повтор того же решения не меняет хранилище и сообщает прежний итог.
        let second = review_apply(store, work.root(), &review_id).unwrap();
        assert_eq!(outcome(&second), "already_published");
        assert_eq!(inventory(store).unwrap().0.png, published.png);
        assert_eq!(
            format!("{:?}", store.journal_check().unwrap()),
            format!("{journal:?}")
        );

        // Другое решение для тех же байтов отклоняется до любых изменений.
        let path = work
            .root()
            .join(format!("review/{review_id}/decisions.tsv"));
        let rejected = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\tapprove\t", "\treject\t");
        std::fs::write(&path, rejected).unwrap();
        assert!(review_apply(store, work.root(), &review_id).is_err());
        assert_eq!(inventory(store).unwrap().0.png, published.png);
    }

    #[test]
    fn approved_decision_survives_a_failed_publication_and_is_retried() {
        let fixture = fixture("local-png-approve-retry-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Uncertain),
            "二",
        );
        let sha = state.rendered_sha().unwrap().to_owned();
        work.save_item(&state).unwrap();
        write_atomic(
            &work.root().join("inventory-before.json"),
            &serde_json::to_vec(&inventory(store).unwrap().0).unwrap(),
        )
        .unwrap();
        let review_id = decide(work, store, "approve");

        // Сбой после сохранения решения: свидетельства расходятся с прогоном.
        let mut foreign = render_evidence(&sha);
        foreign.font.sha256 = "e".repeat(64);
        write_evidence(work, &foreign);
        let failed = review_apply(store, work.root(), &review_id).unwrap();
        assert_eq!(failed["items"][0]["outcome"], "publish_error");
        assert_eq!(failed["counts"]["failed"], 1);
        assert!(inventory(store).unwrap().0.png.is_empty());
        assert_eq!(store.verify_integrity().unwrap().len(), 0);
        let saved = work.load_item("一").unwrap().unwrap();
        assert_eq!(
            saved.review_current().map(|review| review.decision),
            Some(ReviewDecision::Approve)
        );
        let after = inventory(store).unwrap().0;
        assert_eq!(item_outcome(&saved, &after), "approved_publish_error");

        // Отчёт показывает принятое решение и путь восстановления, не трогая `integrity`.
        let (summary, healthy) = report(store, work.root()).unwrap();
        assert!(healthy);
        assert_eq!(summary["counts"]["approved_publish_error"], 1);
        assert_eq!(summary["progress"]["finished"], false);
        let issues: Value =
            serde_json::from_slice(&work.read("report/issues.json").unwrap()).unwrap();
        assert_eq!(
            issues[0]["recovery"],
            format!("local-png review-apply --review-id {review_id}")
        );
        // Решённое повторно не предлагается.
        assert_eq!(
            review_export(store, work.root()).unwrap()["counts"]["rows"],
            0
        );

        // После устранения причины прежний снимок завершает публикацию.
        write_evidence(work, &render_evidence(&sha));
        let retried = review_apply(store, work.root(), &review_id).unwrap();
        assert_eq!(retried["items"][0]["outcome"], "published_after_review");
        let after = inventory(store).unwrap().0;
        assert_eq!(
            after.png.get("一"),
            Some(&(sha, LOCAL_RENDER_SOURCE_KIND.to_owned()))
        );
        let saved = work.load_item("一").unwrap().unwrap();
        assert_eq!(item_outcome(&saved, &after), "published_after_review");
        assert_eq!(
            report(store, work.root()).unwrap().0["progress"]["finished"],
            true
        );
    }

    #[test]
    fn same_bytes_of_other_provenance_are_not_attributed_to_the_local_render() {
        let fixture = fixture("local-png-foreign-sha-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let png = tile_png();
        let state = recognized(
            validated(materialize(work, &png), SemanticStatus::Uncertain),
            "二",
        );
        work.save_item(&state).unwrap();
        let review_id = decide(work, store, "approve");

        // Между экспортом и применением другой путь кладёт те же байты кандидатом.
        let source = fixture.workspace.path().join("candidate.png");
        std::fs::write(&source, &png).unwrap();
        store
            .ingest(IngestRequest {
                identity: state.identity(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        let foreign_before = png_slots(store.verify_integrity().unwrap())["一"].clone();
        assert_ne!(
            foreign_before.provenance.source_kind,
            LOCAL_RENDER_SOURCE_KIND
        );

        let applied = review_apply(store, work.root(), &review_id).unwrap();
        assert_eq!(applied["items"][0]["outcome"], "png_slot_occupied");
        assert_eq!(applied["counts"]["failed"], 1);
        let slot = png_slots(store.verify_integrity().unwrap())["一"].clone();
        assert_eq!(slot, foreign_before);
        let saved = work.load_item("一").unwrap().unwrap();
        assert_eq!(
            item_outcome(&saved, &inventory(store).unwrap().0),
            "approved_png_slot_occupied"
        );

        // Автоматическая публикация тех же байтов тоже не считает чужую запись своей.
        let passing = recognized(validated(state, SemanticStatus::Verified), "一");
        let mut states = vec![passing];
        publish_stage(work, store, &fixture.config, false, &mut states).unwrap();
        assert_eq!(
            states[0].publication.as_ref().unwrap().outcome,
            "png_slot_occupied"
        );
        assert_eq!(
            png_slots(store.verify_integrity().unwrap())["一"],
            foreign_before
        );
    }

    #[test]
    fn inconsistent_evidence_is_not_published() {
        let fixture = fixture("local-png-evidence-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Verified),
            "一",
        );
        let sha = state.rendered_sha().unwrap().to_owned();
        for mutate in [
            (|evidence: &mut RenderEvidence| evidence.character = "二".into())
                as fn(&mut RenderEvidence),
            |evidence| evidence.corrected_sha256 = "0".repeat(64),
            |evidence| evidence.font.sha256 = "e".repeat(64),
            |evidence| evidence.settings.font_size_css += 1.0,
        ] {
            let mut evidence = render_evidence(&sha);
            mutate(&mut evidence);
            write_evidence(work, &evidence);
            let mut states = vec![state.clone()];
            publish_stage(work, store, &fixture.config, false, &mut states).unwrap();
            assert_eq!(
                states[0].publication.as_ref().unwrap().outcome,
                "evidence_mismatch"
            );
            assert_eq!(store.verify_integrity().unwrap().len(), 0);
        }
        write_evidence(work, &render_evidence(&sha));
        let mut states = vec![state];
        publish_stage(work, store, &fixture.config, false, &mut states).unwrap();
        assert_eq!(states[0].publication.as_ref().unwrap().outcome, "published");
    }

    #[test]
    fn publication_is_reevaluated_against_the_current_slot() {
        let fixture = fixture("local-png-slot-change-test");
        let (store, work, config) = (&fixture.store, &fixture.work, &fixture.config);
        let mut state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Verified),
            "一",
        );
        let sha = state.rendered_sha().unwrap().to_owned();
        // Прежний прогон видел занятый слот; теперь слот свободен.
        state.publication = Some(publication(&state, "png_slot_occupied", 0));
        work.save_item(&state).unwrap();
        let mut states = vec![state];
        publish_stage(work, store, config, false, &mut states).unwrap();
        assert_eq!(states[0].publication.as_ref().unwrap().outcome, "published");
        let records = store.verify_integrity().unwrap();
        let journal = format!("{:?}", store.journal_check().unwrap());
        let saved = std::fs::read(work.root().join("items/U+4E00.json")).unwrap();

        // Настоящий повтор — no-op: ни хранилище, ни журнал, ни состояние не меняются.
        publish_stage(work, store, config, false, &mut states).unwrap();
        assert_eq!(states[0].publication.as_ref().unwrap().outcome, "published");
        assert_eq!(store.verify_integrity().unwrap(), records);
        assert_eq!(format!("{:?}", store.journal_check().unwrap()), journal);
        assert_eq!(
            std::fs::read(work.root().join("items/U+4E00.json")).unwrap(),
            saved
        );

        // Намеренный отзыв доверия человеком не обходится повторной публикацией.
        store
            .attest(HumanAttestationRequest {
                identity: states[0].identity(),
                expected_sha256: sha,
                decision: HumanDecision::Reject,
                reason: "отозвано".into(),
            })
            .unwrap();
        let revoked = store.verify_integrity().unwrap();
        publish_stage(work, store, config, true, &mut states).unwrap();
        assert_eq!(
            states[0].publication.as_ref().unwrap().outcome,
            "trust_revoked"
        );
        assert_eq!(store.verify_integrity().unwrap(), revoked);
        assert!(inventory(store).unwrap().0.png.is_empty());
        assert_eq!(
            item_outcome(&states[0], &inventory(store).unwrap().0),
            "trust_revoked"
        );
        let slot = png_slots(revoked);
        assert!(!has_pending_work(
            work,
            config,
            &states[0],
            selection(true, false, true, true),
            slot.get("一")
        ));
    }

    #[test]
    fn failed_rerender_drops_the_stale_render_and_its_decisions() {
        let mut state = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        state.publication = Some(publication(&state, "published", 0));
        record_render_failure(&mut state, "browser_unavailable: нет Chromium".into());
        assert!(state.render.is_none());
        assert!(state.validation.is_none() && state.ocr.is_none() && state.publication.is_none());
        let error = state.render_error.as_ref().unwrap();
        assert_eq!(
            (error.code.as_str(), error.attempts),
            ("browser_unavailable", 1)
        );
        assert_eq!(item_outcome(&state, &inventory_with(&[])), "render_error");
        record_render_failure(&mut state, "browser_unavailable: нет Chromium".into());
        assert_eq!(state.render_error.as_ref().unwrap().attempts, 2);
    }

    #[test]
    fn repeated_review_export_keeps_filled_decisions() {
        let fixture = fixture("local-png-review-export-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Uncertain),
            "二",
        );
        work.save_item(&state).unwrap();

        let first = review_export(store, work.root()).unwrap();
        let decisions = work.root().join(first["decisions"].as_str().unwrap());
        let filled = std::fs::read_to_string(&decisions)
            .unwrap()
            .replace("\t\t\n", "\tapprove\tглиф верный\n");
        std::fs::write(&decisions, &filled).unwrap();

        let second = review_export(store, work.root()).unwrap();
        assert_eq!(second["review_id"], first["review_id"]);
        assert_eq!(std::fs::read_to_string(&decisions).unwrap(), filled);
    }

    #[test]
    fn review_export_reports_a_store_change_made_while_opening() {
        let fixture = fixture("local-png-open-change-test");
        let root = fixture.workspace.path().join("corpus");
        let work_dir = fixture.work.root().to_path_buf();
        drop(fixture.store);
        // Манифесты предыдущей схемы переводятся при открытии хранилища.
        for path in [
            root.join("manifest.json"),
            root.join(".runtime/manifest.json"),
        ] {
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest["schema_version"] = 5.into();
            std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        }
        let execute = |command| {
            super::super::local_png_cli::execute(
                &command,
                &root,
                &BTreeSet::new(),
                fixture.workspace.path(),
            )
            .unwrap()
            .0
        };
        let export = || super::super::local_png_cli::LocalPngCommand::ReviewExport {
            work_dir: work_dir.clone(),
        };
        assert_eq!(execute(export())["store_changed_on_open"], true);
        assert_eq!(execute(export())["store_changed_on_open"], false);
    }

    #[test]
    fn review_export_skips_a_slot_occupied_by_an_unverified_png() {
        let fixture = fixture("local-png-review-pending-slot-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let png = tile_png();
        let state = recognized(
            validated(materialize(work, &png), SemanticStatus::Uncertain),
            "二",
        );
        work.save_item(&state).unwrap();
        let source = fixture.workspace.path().join("candidate.png");
        std::fs::write(&source, &png).unwrap();
        store
            .ingest(IngestRequest {
                identity: state.identity(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        assert_ne!(
            png_slots(store.verify_integrity().unwrap())["一"].lifecycle,
            LifecycleState::Verified
        );

        let exported = review_export(store, work.root()).unwrap();
        assert_eq!(exported["counts"]["rows"], 0);
    }

    #[test]
    fn unverified_png_in_the_slot_is_not_reported_as_a_kept_original() {
        let fixture = fixture("local-png-pending-slot-test");
        let (store, work) = (&fixture.store, &fixture.work);
        // Кандидат другого происхождения лежит в слоте как `pending`.
        let mut other = image::RgbImage::from_pixel(174, 174, image::Rgb([34, 34, 34]));
        other.put_pixel(1, 1, image::Rgb([255, 255, 255]));
        let other_path = fixture.workspace.path().join("candidate.png");
        other.save(&other_path).unwrap();
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Verified),
            "一",
        );
        store
            .ingest(IngestRequest {
                identity: state.identity(),
                source_path: other_path,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        let mut states = vec![state];
        publish_stage(work, store, &fixture.config, false, &mut states).unwrap();
        let publication = states[0].publication.as_ref().unwrap();
        assert_eq!(publication.outcome, "png_slot_occupied");
    }

    #[test]
    fn review_apply_keeping_a_historical_png_is_not_a_failure() {
        let fixture = fixture("local-png-review-original-test");
        let (store, work) = (&fixture.store, &fixture.work);
        let state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Uncertain),
            "二",
        );
        work.save_item(&state).unwrap();
        let review_id = decide(work, store, "approve");

        // Между экспортом и применением в слот попадает проверенный исторический PNG.
        let mut other = image::RgbImage::from_pixel(174, 174, image::Rgb([34, 34, 34]));
        other.put_pixel(1, 1, image::Rgb([255, 255, 255]));
        let source = fixture.workspace.path().join("historical.png");
        other.save(&source).unwrap();
        let candidate = store
            .ingest(IngestRequest {
                identity: state.identity(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap()
            .asset;
        store
            .validate_exact(
                &state.identity(),
                &candidate.sha256,
                &KanjiImageValidator::new(),
            )
            .unwrap();
        store
            .attest(HumanAttestationRequest {
                identity: state.identity(),
                expected_sha256: candidate.sha256.clone(),
                decision: HumanDecision::Approve,
                reason: "исторический оригинал".into(),
            })
            .unwrap();
        assert_eq!(
            png_slots(store.verify_integrity().unwrap())["一"].lifecycle,
            LifecycleState::Verified
        );

        for _ in 0..2 {
            let applied = review_apply(store, work.root(), &review_id).unwrap();
            assert_eq!(applied["items"][0]["outcome"], "existing_original_kept");
            assert_eq!(applied["counts"]["failed"], 0);
            assert_eq!(applied["outcome"], "completed");
        }
        let saved = work.load_item("一").unwrap().unwrap();
        assert_eq!(
            item_outcome(&saved, &inventory(store).unwrap().0),
            "existing_original_kept"
        );
    }

    #[test]
    fn approved_item_behind_a_historical_png_is_finished() {
        let fixture = fixture("local-png-approved-original-test");
        let mut state = recognized(
            validated(
                materialize(&fixture.work, &tile_png()),
                SemanticStatus::Uncertain,
            ),
            "二",
        );
        state.review = Some(ReviewState {
            review_id: "r".repeat(32),
            sha256: state.rendered_sha().unwrap().into(),
            decision: ReviewDecision::Approve,
            reason: "глиф верный".into(),
        });
        state.publication = Some(publication(&state, "existing_original_kept", 0));
        let after = inventory_with(&[("一", "other", "yarxi")]);
        let outcome = item_outcome(&state, &after);
        assert_eq!(outcome, "existing_original_kept");
        assert!(!UNFINISHED_OUTCOMES.contains(&outcome));
    }

    #[test]
    fn human_reject_is_not_overridden_by_automatic_publication() {
        let fixture = fixture("local-png-reject-test");
        let (store, work) = (&fixture.store, &fixture.work);
        // Позднее OCR совпал, но для этих байтов уже действует решение `reject`.
        let mut state = recognized(
            validated(materialize(work, &tile_png()), SemanticStatus::Verified),
            "一",
        );
        state.review = Some(ReviewState {
            review_id: "r".repeat(32),
            sha256: state.rendered_sha().unwrap().into(),
            decision: ReviewDecision::Reject,
            reason: "глиф неверный".into(),
        });
        assert!(state.passes_gates());
        assert!(!has_pending_work(
            work,
            &fixture.config,
            &state,
            selection(true, false, true, true),
            None
        ));
        let mut states = vec![state];
        publish_stage(work, store, &fixture.config, true, &mut states).unwrap();
        assert!(states[0].publication.is_none());
        assert!(inventory(store).unwrap().0.png.is_empty());
        assert_eq!(
            item_outcome(&states[0], &inventory(store).unwrap().0),
            "review_rejected"
        );
    }
}
