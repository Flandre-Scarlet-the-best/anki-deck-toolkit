//! Японское произношение: идентичность, локальный импорт и доверенное чтение.

pub mod batch;
mod probe;

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::domain::{CanonicalAssetLocation, validate_safe_consumer_filename};
use crate::hashing::sha256_hex;
use crate::temp_workspace::TempWorkspace;
use crate::*;

pub use probe::{AudioProbe, probe_mp3};
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

    pub fn consumer_filename(&self) -> String {
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

pub fn technical_validator_identity() -> ValidatorIdentity {
    ValidatorIdentity::new("pronunciation-mp3-technical", "1").expect("постоянная версия")
}

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
        if metadata != &serde_json::json!(request) {
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
            parsed.forvo.validate(&request, &record.sha256)?;
        } else if record.provenance.source_kind == "forvo_web" {
            return Err(invalid(
                "Forvo требует полного происхождения для конкретного SHA",
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
                    probe.schema_version == 1
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
                || validation.evidence[0].kind != "mp3_technical_v1"
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
                    kind: "mp3_technical_v1".into(),
                    summary:
                        "MP3 декодирован; фонетическое соответствие требует прослушивания человеком"
                            .into(),
                    details: Some(serde_json::json!({"request": request, "probe": probe})),
                }],
            ))
        };
        run().map_err(|e| ValidatorFailure::new(e.code.as_str(), e.message))
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
    import_source(store, request, bytes, source_name, replace_expected, None)
}

pub(crate) fn import_forvo_bytes(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    evidence: &crate::forvo::ForvoEvidence,
    replace_expected: Option<String>,
) -> Result<AssetRecord, AssetError> {
    evidence.validate(request, &sha256_hex(bytes))?;
    import_source(
        store,
        request,
        bytes,
        &format!("forvo-ja-{}.mp3", evidence.selected_record_id),
        replace_expected,
        Some(evidence),
    )
}

fn import_source(
    store: &AssetStore,
    request: &Pronunciation,
    bytes: &[u8],
    source_name: &str,
    replace_expected: Option<String>,
    forvo: Option<&crate::forvo::ForvoEvidence>,
) -> Result<AssetRecord, AssetError> {
    PronunciationDomainPolicy.validate_identity(&request.identity())?;
    if source_name.is_empty()
        || source_name.contains(['/', '\\'])
        || source_name.chars().any(char::is_control)
    {
        return Err(invalid("нужно имя источника без пути"));
    }
    probe_mp3(bytes)?;
    let workspace = TempWorkspace::create("pronunciation-import")
        .map_err(|e| AssetError::io("временный MP3", e))?;
    let snapshot = workspace.path().join("candidate.mp3");
    std::fs::write(&snapshot, bytes).map_err(|e| AssetError::io("снимок MP3", e))?;
    let ingest = if forvo.is_some() && replace_expected.is_some() {
        AssetStore::ingest_with_metadata_cas
    } else {
        AssetStore::ingest_with_provenance
    };
    let outcome = ingest(
        store,
        IngestRequest {
            identity: request.identity(),
            source_path: snapshot,
            expected_source_sha256: Some(sha256_hex(bytes)),
            domain_metadata: Some(match forvo {
                Some(evidence) => {
                    serde_json::json!({"surface":request.surface,"reading":request.reading,"forvo":evidence})
                }
                None => serde_json::json!(request),
            }),
            replace_expected_sha256: replace_expected,
        },
        Provenance {
            source_kind: if forvo.is_some() {
                "forvo_web"
            } else {
                "local_import"
            }
            .into(),
            source_name: source_name.into(),
        },
    )?;
    if forvo.is_some() && outcome.asset.provenance.source_kind != "forvo_web" {
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
