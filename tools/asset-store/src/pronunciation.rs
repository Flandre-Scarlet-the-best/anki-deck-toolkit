//! Японское произношение: идентичность, локальный импорт и доверенное чтение.

pub mod batch;
pub mod ctc;
pub mod derivation;
pub mod dsp;
pub mod enhancer;
pub mod enhancer_backend;
pub mod enhancer_batch;
pub mod enhancer_report;
pub mod kana;
pub mod normalizer;
pub mod normalizer_batch;
pub mod normalizer_report;
pub(crate) mod probe;
pub mod recognizer;
pub mod recognizer_backend;
pub mod recognizer_batch;
pub mod recognizer_eval;
pub mod recognizer_pipeline;
pub mod recognizer_report;
pub mod selection;

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::domain::{CanonicalAssetLocation, validate_safe_consumer_filename};
use crate::hashing::sha256_hex;
use crate::temp_workspace::TempWorkspace;
use crate::*;

#[cfg(test)]
pub(crate) use probe::FULL_PROBES;
pub use probe::{AudioProbe, GaplessEvidence, PROBE_SCHEMA_VERSION, probe_mp3};
pub const MAX_AUDIO_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_DURATION_SECONDS: f64 = 30.0;

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

/// NFC сохраняет японское написание; совместимые символы NFKC не сворачиваются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pronunciation {
    pub surface: String,
    pub reading: String,
}

impl Pronunciation {
    pub fn new(surface: &str, reading: &str) -> Result<Self, AssetError> {
        let value = Self {
            surface: surface.nfc().collect(),
            reading: reading.nfc().collect(),
        };
        for text in [&value.surface, &value.reading] {
            if text.is_empty()
                || text.len() > 180
                || text.trim() != text
                || text.chars().any(char::is_control)
            {
                return Err(invalid(
                    "surface/reading должны содержать 1–180 байт без краевых пробелов и управляющих символов",
                ));
            }
        }
        if matches!(value.surface.as_str(), "." | "..")
            || value
                .surface
                .contains(['<', '>', '"', '|', '?', '*', '[', ']'])
        {
            return Err(invalid("surface не представима безопасным именем Anki"));
        }
        // Чтение входит в имя потребителя `<surface>_<reading>.mp3`. Без `_`
        // в чтении граница однозначна (последний `_`), поэтому разные пары
        // не дают одного имени.
        if value
            .reading
            .contains(['<', '>', '"', '|', '?', '*', '[', ']', '_', '/', '\\', ':'])
            || value.reading.ends_with(['.', ' '])
        {
            return Err(invalid(
                "reading не представимо однозначной частью имени файла (запрещены _ и символы пути)",
            ));
        }
        // Прежнее имя `<surface>.mp3` тоже обязано быть безопасным: так
        // написание остаётся под теми же правилами, что и до добавления чтения
        // в имя (например, `CON` по-прежнему недопустимо).
        validate_safe_consumer_filename(&value.legacy_consumer_filename(), DetectedFormat::Mp3)?;
        validate_safe_consumer_filename(&value.consumer_filename(), DetectedFormat::Mp3)?;
        value.identity().validate().map_err(invalid)?;
        Ok(value)
    }

    pub fn identity(&self) -> AssetIdentity {
        AssetIdentity {
            namespace: "pronunciation_audio".into(),
            key: serde_json::to_string(&[&self.surface, &self.reading]).expect("строки JSON"),
        }
    }

    /// Имя готового MP3 этой пары у потребителя: `<surface>_<reading>.mp3`
    /// (`生_せい.mp3`, `生_なま.mp3`). Два чтения одного написания не
    /// конфликтуют. Это имя результата, а не запрос Forvo.
    pub fn consumer_filename(&self) -> String {
        format!("{}_{}.mp3", self.surface, self.reading)
    }

    /// Прежнее имя `<surface>.mp3`. Сохраняется у исторических записей
    /// домена как есть; новые записи его не получают, а ссылки колод
    /// переводятся явным `migrate-media`.
    pub fn legacy_consumer_filename(&self) -> String {
        format!("{}.mp3", self.surface)
    }

    pub fn from_identity(identity: &AssetIdentity) -> Result<Self, AssetError> {
        identity.validate().map_err(invalid)?;
        if identity.namespace != "pronunciation_audio" {
            return Err(invalid("ожидается pronunciation_audio"));
        }
        let [surface, reading]: [String; 2] = serde_json::from_str(&identity.key)
            .map_err(|_| invalid("ключ должен быть канонической парой surface/reading"))?;
        let value = Self::new(&surface, &reading)?;
        if value.identity() != *identity {
            return Err(invalid("идентичность должна быть канонической NFC-парой"));
        }
        Ok(value)
    }
}

/// Текущая версия технической проверки MP3. Версия 2 принимает gapless-MP3,
/// чьё расхождение кадровой и декодированной длины точно объясняет
/// структурно проверенный тег Xing/Info/LAME. Свидетельство версии 1 остаётся
/// допустимыми данными, но доверия не даёт: запись снова проверяется
/// штатным `validate` той же пары/SHA без изменения решения человека.
pub fn technical_validator_identity() -> ValidatorIdentity {
    ValidatorIdentity::new("pronunciation-mp3-technical", "2").expect("постоянная версия")
}

/// Вид свидетельства текущей версии технической проверки.
pub const TECHNICAL_EVIDENCE_KIND: &str = "mp3_technical_v2";

#[derive(Debug, Clone, Copy)]
pub struct PronunciationDomainPolicy;

impl AssetDomainPolicy for PronunciationDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "pronunciation_audio"
    }
    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        Pronunciation::from_identity(identity).map(|_| ())
    }
    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        let request = Pronunciation::from_identity(identity)?;
        if format != DetectedFormat::Mp3
            || sha.len() != 64
            || !sha
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid("домен принимает только MP3 и SHA-256"));
        }
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{}/{sha}.mp3", sha256_hex(identity.key.as_bytes())),
            consumer_filename: request.consumer_filename(),
        })
    }
    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Mp3
    }
    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Mp3
    }
    fn max_import_bytes(&self) -> Option<u64> {
        Some(MAX_AUDIO_BYTES)
    }
    fn max_asset_bytes(&self) -> Option<u64> {
        Some(MAX_AUDIO_BYTES)
    }
    fn content_addressed_storage(&self) -> bool {
        true
    }
    fn allows_consumer_name_overlap(&self) -> bool {
        true
    }
    fn consumer_name_key(&self, name: &str) -> String {
        name.nfc().collect::<String>().to_lowercase()
    }
    fn proves_legacy_consumer_filename(&self, identity: &AssetIdentity, filename: &str) -> bool {
        Pronunciation::from_identity(identity)
            .is_ok_and(|request| request.legacy_consumer_filename() == filename)
    }
    fn accepts_historical_consumer_filename(
        &self,
        identity: &AssetIdentity,
        filename: &str,
    ) -> bool {
        self.proves_legacy_consumer_filename(identity, filename)
    }
    fn trust_semantics(&self) -> TrustSemantics {
        TrustSemantics::HUMAN_WITH_CURRENT_VALIDATION
    }
    fn validate_trust_grant(&self, record: &AssetRecord) -> Result<(), AssetError> {
        if !self
            .trust_semantics()
            .confers_trust(record, &technical_validator_identity())
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "одобрение требует текущего технического валидатора для той же пары/SHA; повторите импорт или validate, затем подтвердите запись",
            ));
        }
        Ok(())
    }
    fn validate_media_bytes(&self, bytes: &[u8]) -> Result<(), AssetError> {
        probe_mp3(bytes).map(|_| ())
    }
    fn validate_record_evidence(&self, record: &AssetRecord) -> Result<(), AssetError> {
        let request = Pronunciation::from_identity(&record.identity)?;
        let metadata = record
            .domain_metadata
            .as_ref()
            .ok_or_else(|| invalid("нет метаданных произношения"))?;
        if metadata != &serde_json::json!(request) && metadata.get("selection_derivative").is_some()
        {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct SelectionMetadata {
                surface: String,
                reading: String,
                selection_derivative: derivation::SelectionDerivative,
            }
            let parsed: SelectionMetadata = serde_json::from_value(metadata.clone())
                .map_err(|_| invalid("неподдерживаемые метаданные производного MP3 выбора"))?;
            if parsed.surface != request.surface || parsed.reading != request.reading {
                return Err(invalid("метаданные не совпадают с идентичностью"));
            }
            parsed
                .selection_derivative
                .validate(&request, &record.sha256, &record.provenance)?;
        } else if metadata != &serde_json::json!(request) && metadata.get("derivation").is_some() {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct DerivedMetadata {
                surface: String,
                reading: String,
                derivation: derivation::AudioDerivation,
            }
            let parsed: DerivedMetadata = serde_json::from_value(metadata.clone())
                .map_err(|_| invalid("неподдерживаемые метаданные производного MP3"))?;
            if parsed.surface != request.surface || parsed.reading != request.reading {
                return Err(invalid("метаданные не совпадают с идентичностью"));
            }
            parsed
                .derivation
                .validate(&request, &record.sha256, &record.provenance)?;
        } else if metadata != &serde_json::json!(request) {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct WebMetadata {
                surface: String,
                reading: String,
                forvo: crate::forvo::ForvoEvidence,
            }
            let parsed: WebMetadata = serde_json::from_value(metadata.clone())
                .map_err(|_| invalid("неподдерживаемые метаданные произношения"))?;
            if parsed.surface != request.surface
                || parsed.reading != request.reading
                || record.provenance.source_kind != "forvo_web"
                || record.provenance.source_name
                    != format!("forvo-ja-{}.mp3", parsed.forvo.selected_record_id)
            {
                return Err(invalid(
                    "метаданные не совпадают с идентичностью/происхождением",
                ));
            }
            parsed.forvo.validate(&request.surface, &record.sha256)?;
        } else if record.provenance.source_kind == "forvo_web" {
            return Err(invalid(
                "Forvo требует полного происхождения для конкретного SHA",
            ));
        } else if record.provenance.source_kind == derivation::DERIVED_SOURCE_KIND
            || record.provenance.source_kind == derivation::SELECTION_SOURCE_KIND
        {
            return Err(invalid(
                "производный MP3 требует полного свидетельства обработки",
            ));
        }
        // Структура свидетельства принадлежит конкретной версии валидатора.
        // Старые версии остаются допустимыми данными, но не дают доверия.
        let current_validation = record
            .validation
            .as_ref()
            .filter(|validation| validation.validator == technical_validator_identity());
        if let Some(validation) = current_validation {
            let probe = validation
                .evidence
                .first()
                .and_then(|e| e.details.as_ref())
                .and_then(|details| {
                    serde_json::from_value::<AudioProbe>(details["probe"].clone()).ok()
                })
                .filter(|probe| {
                    probe.schema_version == PROBE_SCHEMA_VERSION
                        && probe.codec == "mp3"
                        && probe.sha256 == record.sha256
                        && probe.byte_length == record.byte_length
                        && matches!(probe.channels, 1 | 2)
                        && probe.frames >= 2
                        && [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000]
                            .contains(&probe.sample_rate)
                        && probe.decoded_samples > 0
                        && probe.decoded_duration_seconds.is_finite()
                        && probe.decoded_duration_seconds > 0.0
                        && probe.decoded_duration_seconds <= MAX_DURATION_SECONDS
                        && probe.declared_duration_seconds.is_finite()
                        && probe.declared_duration_seconds > 0.0
                        && (probe.decoded_duration_seconds
                            - probe.decoded_samples as f64 / probe.sample_rate as f64)
                            .abs()
                            < 0.000_001
                        && probe.gapless.as_ref().is_none_or(|tag| {
                            tag.xing_frames.checked_add(1) == Some(probe.frames)
                                && tag.expected_decoded_samples(probe::samples_per_frame(
                                    probe.sample_rate,
                                )) == Some(probe.decoded_samples)
                        })
                });
            if probe.is_none() {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "неполные или противоречивые диагностические сведения MP3",
                ));
            }
        }
        if let Some(validation) = current_validation
            && (!matches!(
                validation.status,
                SemanticStatus::Uncertain | SemanticStatus::Corrupt
            ) || !validation.is_valid_for_sha(&record.sha256)
                || validation.evidence.len() != 1
                || validation.evidence[0].kind != TECHNICAL_EVIDENCE_KIND
                || validation.evidence[0]
                    .details
                    .as_ref()
                    .is_none_or(|details| {
                        details["request"] != serde_json::json!(request)
                            || details["probe"]["sha256"] != record.sha256
                    }))
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "неактуальное техническое свидетельство MP3 или другая пара surface/reading",
            ));
        }
        if record.lifecycle == LifecycleState::Verified
            && record.current_human_decision() != Some(HumanDecision::Approve)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "MP3 требует явного одобрения человеком",
            ));
        }
        Ok(())
    }
}

/// Успешное декодирование не доказывает чтение. Оно всегда даёт UNCERTAIN.
#[derive(Debug)]
pub struct AudioTechnicalValidator;
impl SemanticValidator for AudioTechnicalValidator {
    fn identity(&self) -> ValidatorIdentity {
        technical_validator_identity()
    }
    fn validate(
        &self,
        record: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut run = || -> Result<SemanticDecision, AssetError> {
            let request = Pronunciation::from_identity(&record.identity)?;
            let mut data = Vec::new();
            bytes
                .take(MAX_AUDIO_BYTES + 1)
                .read_to_end(&mut data)
                .map_err(|e| AssetError::io("чтение MP3", e))?;
            if sha256_hex(&data) != record.sha256 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "изменены проверяемые байты",
                ));
            }
            let probe = probe_mp3(&data)?;
            Ok(SemanticDecision::new(
                SemanticStatus::Uncertain,
                vec![ValidationEvidence {
                    kind: TECHNICAL_EVIDENCE_KIND.into(),
                    summary:
                        "MP3 декодирован; фонетическое соответствие требует прослушивания человеком"
                            .into(),
                    details: Some(serde_json::json!({"request": request, "probe": probe})),
                }],
            ))
        };
        run().map_err(|e| {
            ValidatorFailure::new(
                e.details["blocker"].as_str().unwrap_or(e.code.as_str()),
                e.message,
            )
        })
    }
}

/// Импортирует снимок оригинальных байтов. Предварительная проверка MP3 не заменяет
/// ValidationRecord: проверка текущего кандидата сохраняется общим API доверия.
pub fn import_bytes(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    source_name: &str,
    replace_expected: Option<String>,
) -> Result<AssetRecord, AssetError> {
    import_source(
        store,
        request,
        bytes,
        source_name,
        replace_expected,
        Origin::Local,
    )
}

/// Производный вариант всегда заменяет текущую запись через CAS. Запрет
/// замены `Verified` без `allow_replace_verified` проверяется хранилищем
/// под той же блокировкой, что и CAS.
pub(crate) fn import_derived_bytes(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    derivation: &derivation::AudioDerivation,
    replace_expected: String,
    allow_replace_verified: bool,
) -> Result<AssetRecord, AssetError> {
    import_source(
        store,
        request,
        bytes,
        &derivation::source_name(&derivation.source_sha256),
        Some(replace_expected),
        Origin::Derived(derivation, allow_replace_verified),
    )
}

/// Производный MP3 пакета выбора: фрагмент, материализованный интервал или
/// обработанная запись конкретного чтения со свидетельством цепочки.
pub(crate) fn import_selection_derived_bytes(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    derivative: &derivation::SelectionDerivative,
    replace_expected: Option<String>,
) -> Result<AssetRecord, AssetError> {
    let source_name =
        derivation::selection_source_name(&derivative.transform, &derivative.source_sha256);
    derivative.validate(
        request,
        &sha256_hex(bytes),
        &Provenance {
            source_kind: derivation::SELECTION_SOURCE_KIND.into(),
            source_name: source_name.clone(),
        },
    )?;
    import_source(
        store,
        request,
        bytes,
        &source_name,
        replace_expected,
        Origin::Selection(derivative),
    )
}

#[derive(Clone, Copy)]
enum Origin<'a> {
    Local,
    Forvo(&'a crate::forvo::ForvoEvidence),
    Derived(&'a derivation::AudioDerivation, bool),
    Selection(&'a derivation::SelectionDerivative),
}

pub(crate) fn import_forvo_bytes(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    evidence: &crate::forvo::ForvoEvidence,
    replace_expected: Option<String>,
) -> Result<AssetRecord, AssetError> {
    evidence.validate(&request.surface, &sha256_hex(bytes))?;
    import_source(
        store,
        request,
        bytes,
        &format!("forvo-ja-{}.mp3", evidence.selected_record_id),
        replace_expected,
        Origin::Forvo(evidence),
    )
}

fn import_source(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    source_name: &str,
    replace_expected: Option<String>,
    origin: Origin<'_>,
) -> Result<AssetRecord, AssetError> {
    let forvo = match origin {
        Origin::Forvo(evidence) => Some(evidence),
        _ => None,
    };
    PronunciationDomainPolicy.validate_identity(&request.identity())?;
    if source_name.is_empty()
        || source_name.contains(['/', '\\'])
        || source_name.chars().any(char::is_control)
    {
        return Err(invalid("нужно имя источника без пути"));
    }
    if forvo.is_some() {
        // Выбор диктора уже включает декодирование. Здесь проверяем структуру,
        // в том числе для восстановленного checkpoint, не доверяя provenance
        // как техническому свидетельству. Фактические байты нового кандидата
        // полностью проверит AudioTechnicalValidator через AssetStore.
        probe::inspect_frames(bytes)?;
    } else {
        probe_mp3(bytes)?;
    }
    let workspace = TempWorkspace::create("pronunciation-import")
        .map_err(|e| AssetError::io("временный MP3", e))?;
    let snapshot = workspace.path().join("candidate.mp3");
    std::fs::write(&snapshot, bytes).map_err(|e| AssetError::io("снимок MP3", e))?;
    let replacing = replace_expected.is_some();
    let ingest = |store: &AssetStore, request: IngestRequest, provenance: Provenance| match origin {
        Origin::Derived(_, allow) => store.ingest_with_provenance_guarded(
            request,
            provenance,
            if allow {
                crate::store::VerifiedReplacement::Allowed
            } else {
                crate::store::VerifiedReplacement::Forbidden
            },
        ),
        Origin::Forvo(_) | Origin::Selection(_) if replacing => {
            store.ingest_with_metadata_cas(request, provenance)
        }
        _ => store.ingest_with_provenance(request, provenance),
    };
    let outcome = ingest(
        store,
        IngestRequest {
            identity: request.identity(),
            source_path: snapshot,
            expected_source_sha256: Some(sha256_hex(bytes)),
            domain_metadata: Some(match origin {
                Origin::Forvo(evidence) => {
                    serde_json::json!({"surface":request.surface,"reading":request.reading,"forvo":evidence})
                }
                Origin::Derived(derivation, _) => {
                    serde_json::json!({"surface":request.surface,"reading":request.reading,"derivation":derivation})
                }
                Origin::Selection(derivative) => {
                    serde_json::json!({"surface":request.surface,"reading":request.reading,"selection_derivative":derivative})
                }
                Origin::Local => serde_json::json!(request),
            }),
            replace_expected_sha256: replace_expected,
        },
        Provenance {
            source_kind: match origin {
                Origin::Forvo(_) => "forvo_web",
                Origin::Derived(..) => derivation::DERIVED_SOURCE_KIND,
                Origin::Selection(_) => derivation::SELECTION_SOURCE_KIND,
                Origin::Local => "local_import",
            }
            .into(),
            source_name: source_name.into(),
        },
    )?;
    if !matches!(origin, Origin::Local)
        && outcome.asset.provenance.source_kind
            != match origin {
                Origin::Forvo(_) => "forvo_web",
                Origin::Selection(_) => derivation::SELECTION_SOURCE_KIND,
                _ => derivation::DERIVED_SOURCE_KIND,
            }
    {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "Те же байты уже хранятся с иным происхождением; ресурс сохранён без изменений",
        ));
    }
    if !outcome.changed
        && outcome
            .asset
            .has_current_automated_decision(&technical_validator_identity())
    {
        return Ok(outcome.asset);
    }
    let (report, validated_record) = store.validate_exact_with_record(
        &request.identity(),
        &outcome.asset.sha256,
        &AudioTechnicalValidator,
    )?;
    if !report.blockers.is_empty() {
        return Err(AssetError::with_details(
            ErrorCode::ValidatorFailure,
            "техническая проверка MP3 не завершена",
            serde_json::json!(report),
        ));
    }
    validated_record.ok_or_else(|| invalid("кандидат потерян"))
}

/// Выбор делается сразу для всего каталога media: одинаковые имена разных
/// идентичностей отклоняются общим API до возврата первого ресурса.
pub fn read_trusted(
    root: &Path,
    requests: &[Pronunciation],
) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
    AssetStore::read_verified_with_policy(
        root,
        &requests
            .iter()
            .map(Pronunciation::identity)
            .collect::<Vec<_>>(),
        &technical_validator_identity(),
        &PronunciationDomainPolicy,
    )
}
