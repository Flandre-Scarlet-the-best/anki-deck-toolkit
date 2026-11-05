//! Производный MP3 нормализатора в домене `pronunciation_audio`.
//!
//! Производный вариант — отдельные байты с собственным SHA и происхождением
//! `audio_derivative`. Свидетельство связывает его с SHA исходника, исходным
//! происхождением (включая Forvo) и точными параметрами обработки. Публикация
//! заменяет текущую запись только через CAS и никогда не переносит прежнее
//! решение человека: новый SHA снова требует прослушивания и `attest`.

use serde::{Deserialize, Serialize};

use super::normalizer::{
    self, GateDecision, GateOutcome, Mode, NormalizationResult, NormalizerProfile, Plan, Toolchain,
    Variant,
};
use super::{Pronunciation, technical_validator_identity};
use crate::hashing::sha256_hex;
use crate::{AssetError, AssetRecord, AssetStore, ErrorCode, LifecycleState, Provenance};

pub const DERIVED_SOURCE_KIND: &str = "audio_derivative";
pub const DERIVATION_KIND: &str = "balanced_normalizer";

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioDerivation {
    pub schema_version: u32,
    pub kind: String,
    pub source_sha256: String,
    pub source_provenance: Provenance,
    /// Метаданные исходной записи без изменений (для Forvo — полное свидетельство).
    pub source_metadata: serde_json::Value,
    pub output_sha256: String,
    pub mode: Mode,
    pub profile: NormalizerProfile,
    pub toolchain: Toolchain,
    pub config_sha256: String,
    pub plan: Plan,
    pub gate: GateDecision,
    /// Публикация вопреки исходу QualityGate (только по явному флагу).
    pub gate_overridden: bool,
}

pub fn source_name(source_sha256: &str) -> String {
    format!("{DERIVATION_KIND}-{}.mp3", &source_sha256[..16])
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl AudioDerivation {
    /// Проверяет внутреннюю согласованность и связь с исходником.
    pub fn validate(
        &self,
        request: &Pronunciation,
        record_sha: &str,
        provenance: &Provenance,
    ) -> Result<(), AssetError> {
        if self.schema_version != 1
            || self.kind != DERIVATION_KIND
            || self.mode != Mode::Normalize
            || !valid_sha(&self.source_sha256)
            || self.output_sha256 != record_sha
            || self.source_sha256 == record_sha
            || provenance.source_kind != DERIVED_SOURCE_KIND
            || provenance.source_name != source_name(&self.source_sha256)
            || normalizer::config_sha256(&self.profile, self.mode, &self.toolchain)
                != self.config_sha256
            || self.profile.validate().is_err()
            || !matches!(
                self.gate.outcome,
                GateOutcome::Accept | GateOutcome::ReviewRequired
            )
            || !self.gate.post_qc_verified
            || (self.gate.outcome != GateOutcome::Accept && !self.gate_overridden)
        {
            return Err(invalid(
                "неполное или противоречивое свидетельство производного MP3",
            ));
        }
        match self.source_provenance.source_kind.as_str() {
            "local_import" => {
                if self.source_metadata != serde_json::json!(request) {
                    return Err(invalid("метаданные исходника не совпадают с парой"));
                }
            }
            "forvo_web" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct WebMetadata {
                    surface: String,
                    reading: String,
                    forvo: crate::forvo::ForvoEvidence,
                }
                let parsed: WebMetadata = serde_json::from_value(self.source_metadata.clone())
                    .map_err(|_| invalid("неполное происхождение Forvo исходника"))?;
                if parsed.surface != request.surface
                    || parsed.reading != request.reading
                    || self.source_provenance.source_name
                        != format!("forvo-ja-{}.mp3", parsed.forvo.selected_record_id)
                {
                    return Err(invalid("происхождение Forvo исходника противоречиво"));
                }
                parsed.forvo.validate(request, &self.source_sha256)?;
            }
            _ => return Err(invalid("неподдерживаемое происхождение исходника")),
        }
        Ok(())
    }
}

pub struct PublishRequest<'a> {
    pub store: &'a AssetStore,
    pub request: &'a Pronunciation,
    pub result: &'a NormalizationResult,
    pub profile: &'a NormalizerProfile,
    pub output_bytes: &'a [u8],
    pub source_bytes: &'a [u8],
    pub replace_expected_sha256: &'a str,
    pub allow_replace_verified: bool,
    pub override_gate: bool,
}

/// Текущая запись пары в общем корпусе (основной и runtime-манифесты).
pub fn current_record(
    store: &AssetStore,
    request: &Pronunciation,
) -> Result<Option<AssetRecord>, AssetError> {
    let identity = request.identity();
    let mut records: Vec<AssetRecord> = store
        .select(crate::SelectionMode::Full, &technical_validator_identity())?
        .into_iter()
        .filter(|r| r.identity == identity)
        .collect();
    if records.len() > 1 {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "у пары несколько неразрешённых записей; сначала разрешите их",
        ));
    }
    Ok(records.pop())
}

/// Явная публикация выбранного обработанного варианта вместо текущего исходника.
pub fn publish(request: PublishRequest<'_>) -> Result<AssetRecord, AssetError> {
    let result = request.result;
    let output = result
        .output
        .as_ref()
        .ok_or_else(|| invalid("у результата нет обработанного варианта"))?;
    if result.mode != Mode::Normalize
        || !result.gate.post_qc_verified
        || !result.post_qc.as_ref().is_some_and(|q| q.passed)
    {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "публикуется только вариант, прошедший пост-QC фактического результата",
        ));
    }
    let gate_ok = result.gate.outcome == GateOutcome::Accept
        && result.gate.proposed_variant == Some(Variant::Processed);
    if !gate_ok && !(request.override_gate && result.gate.outcome == GateOutcome::ReviewRequired) {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "QualityGate не предлагает этот вариант; для REVIEW_REQUIRED после прослушивания нужен --override-gate",
        ));
    }
    if sha256_hex(request.output_bytes) != output.sha256
        || sha256_hex(request.source_bytes) != result.input.sha256
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты варианта или исходника не совпадают с результатом",
        ));
    }
    let current = current_record(request.store, request.request)?.ok_or_else(|| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            "в хранилище нет записи этой пары: сначала импортируйте исходник с явными surface/reading",
        )
    })?;
    if current.sha256 != result.input.sha256 || current.sha256 != request.replace_expected_sha256 {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "текущая запись пары не совпадает с исходником результата или --replace-expected-sha256",
        ));
    }
    if current.provenance.source_kind == DERIVED_SOURCE_KIND {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "повторная обработка производного варианта не поддерживается",
        ));
    }
    if current.lifecycle == LifecycleState::Verified && !request.allow_replace_verified {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "текущая запись подтверждена человеком; замена требует --allow-replace-verified и нового подтверждения",
        ));
    }
    let derivation = AudioDerivation {
        schema_version: 1,
        kind: DERIVATION_KIND.into(),
        source_sha256: result.input.sha256.clone(),
        source_provenance: current.provenance.clone(),
        source_metadata: current
            .domain_metadata
            .clone()
            .ok_or_else(|| invalid("у исходной записи нет метаданных"))?,
        output_sha256: output.sha256.clone(),
        mode: result.mode,
        profile: result_profile(result, request.profile)?,
        toolchain: result.toolchain.clone(),
        config_sha256: result.config_sha256.clone(),
        plan: result
            .plan
            .clone()
            .ok_or_else(|| invalid("у результата нет плана"))?,
        gate: result.gate.clone(),
        gate_overridden: !gate_ok,
    };
    derivation.validate(
        request.request,
        &output.sha256,
        &Provenance {
            source_kind: DERIVED_SOURCE_KIND.into(),
            source_name: source_name(&result.input.sha256),
        },
    )?;
    super::import_derived_bytes(
        request.store,
        request.request,
        request.output_bytes,
        &derivation,
        current.sha256.clone(),
    )
}

/// Профиль берётся из состояния пакета: в результате хранится только его
/// идентичность и отпечаток конфигурации, с которым профиль обязан совпасть.
fn result_profile(
    result: &NormalizationResult,
    profile: &NormalizerProfile,
) -> Result<NormalizerProfile, AssetError> {
    if profile.id != result.profile.id
        || profile.version != result.profile.version
        || normalizer::config_sha256(profile, result.mode, &result.toolchain)
            != result.config_sha256
    {
        return Err(invalid(
            "профиль не соответствует отпечатку конфигурации результата",
        ));
    }
    Ok(profile.clone())
}
