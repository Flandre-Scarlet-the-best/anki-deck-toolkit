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

/// Итог явной публикации.
#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub record: AssetRecord,
    /// Текущая запись пары уже была этим производным вариантом (повтор после
    /// сбоя вспомогательной записи или остановки): второй замены не было.
    pub already_published: bool,
}

/// Явная публикация выбранного обработанного варианта вместо текущего исходника.
pub fn publish(request: PublishRequest<'_>) -> Result<AssetRecord, AssetError> {
    publish_with_outcome(request).map(|outcome| outcome.record)
}

/// Повтор публикации, уже зафиксированной в хранилище: свидетельство текущей
/// записи обязано описывать ровно этот вход, вариант и конфигурацию. Повтор
/// не заменяет запись, не меняет происхождение и не трогает решение человека;
/// при необходимости он лишь завершает прерванную техническую проверку.
fn resume_publication(
    request: &PublishRequest<'_>,
    current: AssetRecord,
) -> Result<PublishOutcome, AssetError> {
    let result = request.result;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DerivedMetadata {
        #[allow(dead_code)]
        surface: String,
        #[allow(dead_code)]
        reading: String,
        derivation: AudioDerivation,
    }
    let stored: DerivedMetadata = current
        .domain_metadata
        .clone()
        .ok_or_else(|| invalid("у текущей записи нет свидетельства обработки"))
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|_| invalid("текущая запись не является производным вариантом"))
        })?;
    let derivation = stored.derivation;
    if current.provenance.source_kind != DERIVED_SOURCE_KIND
        || derivation.source_sha256 != result.input.sha256
        || derivation.source_sha256 != request.replace_expected_sha256
        || derivation.output_sha256 != current.sha256
        || derivation.config_sha256 != result.config_sha256
        || Some(&derivation.plan) != result.plan.as_ref()
        || derivation.gate != result.gate
    {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            "текущая запись пары — другой производный вариант: повтор публикации невозможен",
        ));
    }
    let record = super::import_derived_bytes(
        request.store,
        request.request,
        request.output_bytes,
        &derivation,
        current.sha256.clone(),
        request.allow_replace_verified,
    )?;
    Ok(PublishOutcome {
        record,
        already_published: true,
    })
}

/// Как [`publish`], но сообщает, была ли публикация уже зафиксирована.
pub fn publish_with_outcome(request: PublishRequest<'_>) -> Result<PublishOutcome, AssetError> {
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
    if current.sha256 == output.sha256 {
        return resume_publication(&request, current);
    }
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
    // Ранний понятный отказ; решающая проверка повторяется хранилищем под
    // блокировкой вместе с CAS (подтверждение могло появиться после чтения).
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
    #[cfg(test)]
    BEFORE_COMMIT.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let record = super::import_derived_bytes(
        request.store,
        request.request,
        request.output_bytes,
        &derivation,
        current.sha256.clone(),
        request.allow_replace_verified,
    )?;
    Ok(PublishOutcome {
        record,
        already_published: false,
    })
}

#[cfg(test)]
std::thread_local! {
    /// Детерминированная точка между чтением текущей записи и импортом.
    pub(crate) static BEFORE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pronunciation::dsp::Pcm;
    use crate::pronunciation::normalizer::{Toolchain, process};
    use crate::pronunciation::{PronunciationDomainPolicy, import_bytes};
    use crate::temp_workspace::TempWorkspace;
    use crate::{HumanAttestationRequest, HumanDecision, StoreOptions};

    /// MP3 с краевой тишиной и речеподобной серединой (тон с шумом): нужна обрезка.
    fn source_mp3() -> Vec<u8> {
        let rate = 44100u32;
        let mut state = 0x2545_f491u32;
        let mut samples = vec![0.0f32; rate as usize * 2 / 5];
        for i in 0..(rate as usize * 3 / 5) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((state >> 8) as f64 / f64::from(1u32 << 24) - 0.5) * 0.01;
            let t = i as f64 / f64::from(rate);
            samples.push((0.1 * (2.0 * std::f64::consts::PI * 210.0 * t).sin() + noise) as f32);
        }
        samples.extend(vec![0.0f32; rate as usize / 2]);
        let pcm = Pcm {
            sample_rate: rate,
            channels: 1,
            samples,
        };
        crate::pronunciation::probe::encode_mp3(
            &pcm.samples,
            rate,
            1,
            rate,
            &["-q:a".into(), "4".into()],
        )
        .expect("для теста нужен FFmpeg с libmp3lame")
    }

    fn attest(store: &AssetStore, request: &Pronunciation, sha: &str) {
        store
            .attest(HumanAttestationRequest {
                identity: request.identity(),
                expected_sha256: sha.into(),
                decision: HumanDecision::Approve,
                reason: "Прослушано".into(),
            })
            .unwrap();
    }

    /// Подтверждение человека, зафиксированное после чтения текущей записи, но
    /// до импорта, не теряется: запрет замены `Verified` проверяется под
    /// блокировкой хранилища вместе с CAS.
    #[test]
    fn approval_landing_between_read_and_commit_is_not_silently_replaced() {
        let ws = TempWorkspace::create("derivation-race").unwrap();
        let store = AssetStore::open_with_policy(
            StoreOptions::new(ws.path().join("store")),
            PronunciationDomainPolicy,
        )
        .unwrap();
        let request = Pronunciation::new("こいつ", "こいつ").unwrap();
        let bytes = source_mp3();
        let original = import_bytes(&store, &request, &bytes, "こいつ.mp3", None).unwrap();
        let profile = NormalizerProfile::balanced();
        let processed = process(
            &bytes,
            &profile,
            Mode::Normalize,
            &Toolchain::detect().unwrap(),
        )
        .unwrap();
        assert_eq!(processed.result.gate.outcome, GateOutcome::Accept);
        let output = processed.output_bytes.clone().unwrap();
        let publish = |allow: bool| {
            publish_with_outcome(PublishRequest {
                store: &store,
                request: &request,
                result: &processed.result,
                profile: &profile,
                output_bytes: &output,
                source_bytes: &bytes,
                replace_expected_sha256: &original.sha256,
                allow_replace_verified: allow,
                override_gate: false,
            })
        };
        // Другой процесс-владелец того же хранилища подтверждает текущий SHA
        // ровно между чтением записи публикацией и захватом блокировки импорта.
        let root = store.root().to_path_buf();
        let concurrent = request.clone();
        let sha = original.sha256.clone();
        BEFORE_COMMIT.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let other = AssetStore::open_existing_with_policy(
                    StoreOptions::new(root),
                    PronunciationDomainPolicy,
                )
                .unwrap();
                attest(&other, &concurrent, &sha);
            }));
        });
        let refused = publish(false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidTransition);
        assert_eq!(refused.details["blocker"], "verified_replacement_forbidden");
        let current = current_record(&store, &request).unwrap().unwrap();
        assert_eq!(current.sha256, original.sha256);
        assert_eq!(current.lifecycle, LifecycleState::Verified);

        // С явным разрешением замена проходит, и новое доверие не наследуется.
        let published = publish(true).unwrap();
        assert!(!published.already_published);
        assert_eq!(published.record.sha256, sha256_hex(&output));
        assert!(published.record.current_human_decision().is_none());
        assert_ne!(published.record.lifecycle, LifecycleState::Verified);

        // Повтор той же публикации идемпотентен: второй замены нет,
        // происхождение исходника сохранено.
        let again = publish(false).unwrap();
        assert!(again.already_published);
        assert_eq!(again.record.sha256, published.record.sha256);
        assert_eq!(again.record.lifecycle, published.record.lifecycle);
        assert_eq!(
            again.record.domain_metadata.as_ref().unwrap()["derivation"]["source_provenance"]["source_kind"],
            "local_import"
        );
        // Подтверждённый производный вариант повтором тоже не меняется.
        attest(&store, &request, &published.record.sha256);
        let verified = publish(false).unwrap();
        assert!(verified.already_published);
        assert_eq!(verified.record.lifecycle, LifecycleState::Verified);
    }
}
