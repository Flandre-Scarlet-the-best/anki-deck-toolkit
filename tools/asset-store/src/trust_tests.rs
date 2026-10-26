//! Регрессии доверия человека к точному хешу и совместимости с прежним корпусом.
use super::*;
use crate::kanji_validator::MAX_MEDIA_BYTES;
use crate::model::ValidationEvidence;
use crate::selection::select_assets;
use crate::temp_workspace::TempWorkspace;

struct Fixture {
    store: AssetStore,
    identity: AssetIdentity,
    workspace: TempWorkspace,
}

impl Fixture {
    fn new() -> Self {
        let workspace = TempWorkspace::create("asset-store-trust-tests").unwrap();
        let store =
            AssetStore::open_kanji(StoreOptions::new(workspace.path().join("store"))).unwrap();
        Self {
            store,
            identity: AssetIdentity::new("kanji", "日").unwrap(),
            workspace,
        }
    }

    fn directory(&self) -> &std::path::Path {
        self.workspace.path()
    }

    fn ingest(&self, bytes: &[u8], replace: Option<String>) -> AssetRecord {
        let source = self.directory().join("candidate.png");
        fs::write(&source, bytes).unwrap();
        self.store
            .ingest(IngestRequest {
                identity: self.identity.clone(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: replace,
            })
            .unwrap()
            .asset
    }

    fn classify(&self, status: SemanticStatus) -> AssetRecord {
        self.store
            .validate(SelectionMode::Full, &Classifier(status))
            .unwrap();
        self.store.verify_integrity().unwrap().remove(0)
    }

    fn attest(&self, hash: &str, decision: HumanDecision) -> Result<IngestOutcome, AssetError> {
        self.store.attest(HumanAttestationRequest {
            identity: self.identity.clone(),
            expected_sha256: hash.into(),
            decision,
            reason: "явное пользовательское решение по изображению".into(),
        })
    }

    fn read(&self) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        AssetStore::read_verified_with_policy(
            self.directory().join("store"),
            std::slice::from_ref(&self.identity),
            &Classifier(SemanticStatus::Verified).identity(),
            &crate::domain::KanjiDomainPolicy,
        )
    }
}

struct Classifier(SemanticStatus);
impl SemanticValidator for Classifier {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new("trust-fixture", "1").unwrap()
    }
    fn validate(
        &self,
        _: &AssetRecord,
        _: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            self.0,
            vec![ValidationEvidence {
                kind: "fixture-semantic".into(),
                summary: "исходное automated evidence".into(),
                details: Some(serde_json::json!({"distance": 0.42})),
            }],
        ))
    }
}

fn png(value: u8) -> Vec<u8> {
    let image = image::GrayImage::from_pixel(2, 2, image::Luma([value]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

#[test]
fn approval_requires_current_automated_evidence_and_new_does_not_hide_its_absence() {
    let fixture = Fixture::new();
    let record = fixture.ingest(&png(35), None);
    let mut malformed = record.clone();
    malformed.human_attestation = Some(HumanAttestation {
        identity: malformed.identity.clone(),
        content_sha256: malformed.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: "проверено человеком".into(),
    });

    assert_eq!(malformed.effective_status(), None);
    assert!(!malformed.is_trusted_for(&Classifier(SemanticStatus::Verified).identity()));
    assert_eq!(
        select_assets(
            &[malformed],
            SelectionMode::New,
            &Classifier(SemanticStatus::Verified).identity()
        )
        .len(),
        1
    );
    let mut stale = record.clone();
    stale.validation = Some(ValidationRecord {
        status: SemanticStatus::Uncertain,
        validator: Classifier(SemanticStatus::Uncertain).identity(),
        content_sha256: "f".repeat(64),
        evidence: vec![ValidationEvidence {
            kind: "fixture-semantic".into(),
            summary: "устаревшее решение".into(),
            details: None,
        }],
    });
    stale.human_attestation = Some(HumanAttestation {
        identity: stale.identity.clone(),
        content_sha256: stale.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: "проверено человеком".into(),
    });
    assert_eq!(stale.effective_status(), None);
    assert!(!stale.is_trusted_for(&Classifier(SemanticStatus::Uncertain).identity()));

    assert_eq!(
        fixture
            .attest(&record.sha256, HumanDecision::Approve)
            .unwrap_err()
            .code,
        ErrorCode::InvalidValidationEvidence
    );
    assert!(
        fixture.store.verify_integrity().unwrap()[0]
            .human_attestation
            .is_none()
    );
}

#[test]
fn current_validation_policy_rechecks_human_approved_stale_records() {
    let fixture = Fixture::new();
    fixture.ingest(&png(37), None);
    let mut record = fixture.classify(SemanticStatus::Verified);
    let expected = Classifier(SemanticStatus::Verified).identity();
    let stale_validator = ValidatorIdentity::new("trust-fixture", "2").unwrap();
    record.validation.as_mut().unwrap().validator = stale_validator;
    record.human_attestation = Some(HumanAttestation {
        identity: record.identity.clone(),
        content_sha256: record.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: "проверено человеком".into(),
    });

    let current_validation = crate::domain::TrustSemantics::HUMAN_WITH_CURRENT_VALIDATION;
    assert!(!current_validation.confers_trust(&record, &expected));
    assert!(current_validation.requires_new_validation(&record, &expected));
    assert!(
        !crate::domain::TrustSemantics::HUMAN_ATTESTED.requires_new_validation(&record, &expected)
    );

    record.validation.as_mut().unwrap().validator = expected.clone();
    assert!(current_validation.confers_trust(&record, &expected));
    assert!(!current_validation.requires_new_validation(&record, &expected));
}

#[test]
fn malformed_approval_without_validation_is_rejected_by_manifest_and_read() {
    let fixture = Fixture::new();
    fixture.ingest(&png(36), None);
    let record = fixture.classify(SemanticStatus::Verified);
    let path = fixture.directory().join("store/manifest.json");
    let mut manifest: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let stored = &mut manifest.assets[0];
    assert_eq!(stored.identity, record.identity);
    stored.validation = None;
    stored.human_attestation = Some(HumanAttestation {
        identity: stored.identity.clone(),
        content_sha256: stored.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: "поддельное approval без evidence".into(),
    });
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    assert_eq!(
        fixture.store.verify_integrity().unwrap_err().code,
        ErrorCode::ManifestCorrupt
    );
    assert_eq!(fixture.read().unwrap_err().code, ErrorCode::ManifestCorrupt);
}

#[test]
fn oversized_asset_cannot_be_human_approved() {
    let fixture = Fixture::new();
    let bytes = vec![0x5a; MAX_MEDIA_BYTES + 1];
    let record = fixture.ingest(&bytes, None);
    fixture.classify(SemanticStatus::Uncertain);

    assert_eq!(
        fixture
            .attest(&record.sha256, HumanDecision::Approve)
            .unwrap_err()
            .code,
        ErrorCode::InvalidTransition
    );
    assert!(
        fixture.store.verify_integrity().unwrap()[0]
            .human_attestation
            .is_none()
    );
}

#[test]
fn human_approval_overrides_uncertain_rejected_and_preserves_automated_evidence() {
    for status in [SemanticStatus::Uncertain, SemanticStatus::Rejected] {
        let fixture = Fixture::new();
        let bytes = png(40);
        fixture.ingest(&bytes, None);
        let classified = fixture.classify(status);
        let approved = fixture
            .attest(&classified.sha256, HumanDecision::Approve)
            .unwrap();
        assert_eq!(approved.asset.validation, classified.validation);
        assert_eq!(
            approved.asset.effective_status(),
            Some(SemanticStatus::Verified)
        );
        assert_eq!(fixture.read().unwrap()[0].bytes, bytes);
        assert!(
            !fixture
                .attest(&classified.sha256, HumanDecision::Approve)
                .unwrap()
                .changed
        );
        // Повторная классификация не отменяет сохранённое переопределение человеком.
        assert_eq!(fixture.classify(status).lifecycle, LifecycleState::Verified);
        let other_validator = ValidatorIdentity::new("new-validator", "2").unwrap();
        assert!(
            AssetStore::read_verified_with_policy(
                fixture.directory().join("store"),
                std::slice::from_ref(&fixture.identity),
                &other_validator,
                &crate::domain::KanjiDomainPolicy,
            )
            .is_ok()
        );
        drop(
            AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
                .unwrap(),
        );
    }
}

#[test]
fn human_reject_removes_automated_trust_and_automated_revalidation_cannot_restore_it() {
    let fixture = Fixture::new();
    let bytes = png(45);
    fixture.ingest(&bytes, None);
    let automated = fixture.classify(SemanticStatus::Verified);
    assert!(fixture.read().is_ok());
    let rejected = fixture
        .attest(&automated.sha256, HumanDecision::Reject)
        .unwrap();
    assert_eq!(rejected.asset.validation, automated.validation);
    assert_eq!(rejected.asset.lifecycle, LifecycleState::Quarantined);
    assert_eq!(
        fixture.read().unwrap_err().code,
        ErrorCode::MissingAssetFile
    );
    assert_eq!(
        fixture.classify(SemanticStatus::Verified).lifecycle,
        LifecycleState::Quarantined
    );
    let error = fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: fixture.identity.clone(),
                bytes,
                provenance: automated.provenance,
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &Classifier(SemanticStatus::Verified),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    assert!(
        fixture
            .attest(&automated.sha256, HumanDecision::Approve)
            .unwrap()
            .changed
    );
    assert!(fixture.read().is_ok());
}

#[test]
fn replacement_drops_attestation_and_stale_hash_cannot_be_approved() {
    let fixture = Fixture::new();
    let old = fixture.ingest(&png(30), None);
    fixture.classify(SemanticStatus::Uncertain);
    fixture.attest(&old.sha256, HumanDecision::Approve).unwrap();
    let new = fixture.ingest(&png(31), Some(old.sha256.clone()));
    assert_ne!(old.sha256, new.sha256);
    assert!(new.human_attestation.is_none());
    assert!(new.effective_status().is_none());
    assert_eq!(
        fixture
            .attest(&old.sha256, HumanDecision::Approve)
            .unwrap_err()
            .code,
        ErrorCode::IdentityConflict
    );
    assert_eq!(
        fixture.read().unwrap_err().code,
        ErrorCode::MissingAssetFile
    );
}

#[test]
fn corrupt_classification_and_broken_decode_cannot_be_overridden() {
    for (bytes, status) in [
        (png(50), SemanticStatus::Corrupt),
        (b"GIF89a broken frame".to_vec(), SemanticStatus::Uncertain),
        (
            b"\x89PNG\r\n\x1a\n truncated".to_vec(),
            SemanticStatus::Rejected,
        ),
    ] {
        let fixture = Fixture::new();
        let record = fixture.ingest(&bytes, None);
        fixture.classify(status);
        assert_eq!(
            fixture
                .attest(&record.sha256, HumanDecision::Approve)
                .unwrap_err()
                .code,
            ErrorCode::InvalidTransition
        );
        assert!(
            fixture.store.verify_integrity().unwrap()[0]
                .human_attestation
                .is_none()
        );
    }
}

#[test]
fn hash_and_path_integrity_cannot_be_overridden() {
    for symlink in [false, true] {
        let fixture = Fixture::new();
        let record = fixture.ingest(&png(60), None);
        fixture.classify(SemanticStatus::Uncertain);
        let path = fixture
            .directory()
            .join("store/.runtime")
            .join(&record.storage_path);
        fs::remove_file(&path).unwrap();
        if symlink {
            let outside = fixture.directory().join("outside.png");
            fs::write(&outside, png(60)).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
        } else {
            fs::write(&path, png(61)).unwrap();
        }
        let error = fixture
            .attest(&record.sha256, HumanDecision::Approve)
            .unwrap_err();
        std::assert_matches!(
            error.code,
            ErrorCode::IntegrityMismatch | ErrorCode::BoundaryViolation
        );
    }
}

#[test]
fn schema_v3_read_retains_trust_and_first_decision_migrates_without_evidence_loss() {
    let fixture = Fixture::new();
    fixture.ingest(&png(70), None);
    let record = fixture.classify(SemanticStatus::Verified);
    let path = fixture.directory().join("store/manifest.json");
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    legacy["schema_version"] = 3.into();
    legacy.as_object_mut().unwrap().remove("domain_id");
    let record_json = legacy["assets"][0].as_object_mut().unwrap();
    record_json.remove("consumer_filename");
    record_json.insert(
        "storage_path".into(),
        serde_json::Value::String("assets/日.png".into()),
    );
    let old_path = fixture.directory().join("store/assets/png/日.png");
    let legacy_path = fixture.directory().join("store/assets/日.png");
    fs::rename(&old_path, &legacy_path).unwrap();
    fs::remove_dir(fixture.directory().join("store/assets/png")).unwrap();
    let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&path, &legacy_bytes).unwrap();
    assert!(fixture.read().is_ok());
    assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
    let migrated =
        AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
            .unwrap();
    assert!(migrated.layout_migrated_on_open());
    assert!(!legacy_path.exists());
    let approved = migrated
        .attest(HumanAttestationRequest {
            identity: fixture.identity.clone(),
            expected_sha256: record.sha256.clone(),
            decision: HumanDecision::Approve,
            reason: "явное пользовательское решение по изображению".into(),
        })
        .unwrap();
    assert_eq!(approved.asset.validation, record.validation);
    let current: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(current.schema_version, MANIFEST_SCHEMA_VERSION);
    assert!(fixture.read().is_ok());
}

#[test]
fn approval_recovery_keeps_exact_bytes_and_both_decisions() {
    let fixture = Fixture::new();
    let bytes = png(80);
    let record = fixture.ingest(&bytes, None);
    let classified = fixture.classify(SemanticStatus::Rejected);
    FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.set(true));
    assert_eq!(
        fixture
            .attest(&record.sha256, HumanDecision::Approve)
            .unwrap_err()
            .code,
        ErrorCode::IoFailure
    );
    let reopened =
        AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
            .unwrap();
    let result = reopened.verify_integrity().unwrap();
    assert_eq!(result[0].validation, classified.validation);
    assert_eq!(
        result[0].current_human_decision(),
        Some(HumanDecision::Approve)
    );
    assert_eq!(fixture.read().unwrap()[0].bytes, bytes);
}

#[test]
fn runtime_batch_extension_survives_open_and_asset_lifecycle_recovery() {
    let fixture = Fixture::new();
    let batches = fixture.directory().join("store/.runtime/batches");
    fs::create_dir(&batches).unwrap();
    fs::write(batches.join("owned-by-domain.json"), b"domain payload").unwrap();
    let reopened =
        AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
            .unwrap();
    assert!(reopened.verify_integrity().unwrap().is_empty());
    // Импорт создаёт транзакцию публикации внутри каталога `.runtime`; затем подтверждение
    // выполняет восстановление после удаления из `.runtime`. Оба этапа используют
    // загрузчики, работающие с любой областью.
    let record = fixture.ingest(&png(90), None);
    fixture.classify(SemanticStatus::Uncertain);
    fixture
        .attest(&record.sha256, HumanDecision::Approve)
        .unwrap();
    assert_eq!(fixture.read().unwrap()[0].bytes, png(90));
    assert_eq!(
        fs::read(batches.join("owned-by-domain.json")).unwrap(),
        b"domain payload"
    );
}

#[test]
fn runtime_batch_extension_rejects_symlink_and_non_directory() {
    for symlink in [false, true] {
        let fixture = Fixture::new();
        let batches = fixture.directory().join("store/.runtime/batches");
        if symlink {
            std::os::unix::fs::symlink(fixture.directory().join("store/assets"), &batches).unwrap();
        } else {
            fs::write(&batches, b"not a directory").unwrap();
        }
        assert_eq!(
            AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
                .unwrap_err()
                .code,
            ErrorCode::BoundaryViolation,
        );
        assert_eq!(
            fixture.store.verify_integrity().unwrap_err().code,
            ErrorCode::BoundaryViolation
        );
    }
}

#[test]
fn batch_extension_is_not_allowed_in_canonical_store() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.directory().join("store/batches")).unwrap();
    assert_eq!(
        fixture.store.verify_integrity().unwrap_err().code,
        ErrorCode::UnexpectedPath
    );
    assert_eq!(
        AssetStore::open_kanji_existing(StoreOptions::new(fixture.directory().join("store")))
            .unwrap_err()
            .code,
        ErrorCode::StoreNotOwned,
    );
}
