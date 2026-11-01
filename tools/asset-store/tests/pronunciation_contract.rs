//! Реальный синтетический MP3, общий API доверия и изолированный CLI.
use asset_store::hashing::sha256_hex;
use asset_store::pronunciation::{batch::*, *};
use asset_store::temp_workspace::TempWorkspace;
use asset_store::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

fn fixture(frequency: u32, tagged: bool, rate: u32, stereo: bool) -> Vec<u8> {
    let workspace = TempWorkspace::create("audio-fixture").unwrap();
    let path = workspace.path().join("synthetic.mp3");
    let mut command = Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        &format!("sine=frequency={frequency}:duration=0.6:sample_rate={rate}"),
        "-ac",
        if stereo { "2" } else { "1" },
        "-c:a",
        "libmp3lame",
        "-q:a",
        "4",
    ]);
    if !tagged {
        command.args(["-id3v2_version", "0", "-write_xing", "0"]);
    }
    let output = command
        .arg(&path)
        .output()
        .expect("для тестов нужен FFmpeg с libmp3lame");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(path).unwrap()
}
fn audio() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| fixture(440, true, 44100, false))
}
fn other_audio() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| fixture(880, true, 44100, false))
}
fn request() -> Pronunciation {
    Pronunciation::new("こいつ", "こいつ").unwrap()
}
fn store(path: &Path) -> AssetStore {
    AssetStore::open_with_policy(StoreOptions::new(path), PronunciationDomainPolicy).unwrap()
}
fn attest(
    store: &AssetStore,
    request: &Pronunciation,
    sha: &str,
    decision: HumanDecision,
) -> Result<IngestOutcome, AssetError> {
    store.attest(HumanAttestationRequest {
        identity: request.identity(),
        expected_sha256: sha.into(),
        decision,
        reason: "Прослушано: ровно ожидаемое чтение".into(),
    })
}
fn import(store: &AssetStore, request: &Pronunciation) -> AssetRecord {
    import_bytes(store, request, audio(), "pronunciation_ja_こいつ.mp3", None).unwrap()
}

fn web_acquisition(bytes: &[u8], request: &Pronunciation) -> asset_store::forvo::Acquisition {
    use asset_store::forvo::*;
    Acquisition {
        bytes: bytes.to_vec(),
        evidence: ForvoEvidence {
            schema_version: 1,
            page_url: word_url(&request.surface),
            downloaded_sha256: sha256_hex(bytes),
            selected_record_id: "101".into(),
            ranking_policy: RANKING.into(),
            candidates: vec![Candidate {
                record_id: "101".into(),
                username: Some("fixture-speaker".into()),
                votes: Some(-1),
                gender: Some(Gender::Female),
                country: Some("Japan".into()),
                download_available: true,
            }],
            attempts: vec![Attempt {
                record_id: "101".into(),
                code: "downloaded".into(),
            }],
        },
    }
}

#[test]
fn forvo_provenance_uses_the_existing_uncertain_and_trusted_boundary() {
    use asset_store::forvo::*;
    let workspace = TempWorkspace::create("forvo-store-contract").unwrap();
    let store = store(&workspace.path().join("audio"));
    let request = request();
    let acquisition = web_acquisition(audio(), &request);
    let record = import_acquisition(&store, &request, &acquisition, None).unwrap();
    assert_eq!(record.provenance.source_kind, "forvo_web");
    assert_eq!(record.consumer_filename, "こいつ.mp3");
    assert_eq!(record.lifecycle, LifecycleState::Quarantined);
    assert_eq!(
        record.validation.as_ref().unwrap().status,
        SemanticStatus::Uncertain
    );
    assert_eq!(
        record.domain_metadata.as_ref().unwrap()["forvo"]["downloaded_sha256"],
        sha256_hex(audio())
    );
    assert!(read_trusted(store.root(), std::slice::from_ref(&request)).is_err());
    attest(&store, &request, &record.sha256, HumanDecision::Approve).unwrap();
    assert!(read_trusted(store.root(), std::slice::from_ref(&request)).is_ok());
    let other = web_acquisition(other_audio(), &request);
    assert_eq!(
        import_acquisition(&store, &request, &other, None)
            .unwrap_err()
            .code,
        ErrorCode::IdentityConflict
    );
    assert_eq!(
        import_acquisition(&store, &request, &other, Some("0".repeat(64)))
            .unwrap_err()
            .code,
        ErrorCode::IdentityConflict
    );
    assert!(read_trusted(store.root(), std::slice::from_ref(&request)).is_ok());
    let changed = import_acquisition(&store, &request, &other, Some(record.sha256)).unwrap();
    assert_eq!(changed.lifecycle, LifecycleState::Quarantined);
    assert!(read_trusted(store.root(), &[request]).is_err());
    store.verify_integrity().unwrap();
}

#[test]
fn same_sha_local_source_requires_metadata_cas_to_become_forvo_and_revokes_trust() {
    use asset_store::forvo::*;
    let workspace = TempWorkspace::create("forvo-metadata-cas").unwrap();
    let store = store(&workspace.path().join("audio"));
    let request = request();
    let local = import(&store, &request);
    attest(&store, &request, &local.sha256, HumanDecision::Approve).unwrap();
    let acquisition = web_acquisition(audio(), &request);
    assert_eq!(
        import_acquisition(&store, &request, &acquisition, None)
            .unwrap_err()
            .code,
        ErrorCode::IdentityConflict
    );
    assert_eq!(
        read_trusted(store.root(), std::slice::from_ref(&request)).unwrap()[0]
            .record
            .provenance
            .source_kind,
        "local_import"
    );
    let web = import_acquisition(&store, &request, &acquisition, Some(local.sha256)).unwrap();
    assert_eq!(web.provenance.source_kind, "forvo_web");
    assert!(web.human_attestation.is_none());
    assert_eq!(web.lifecycle, LifecycleState::Quarantined);
    let mut bad = web.clone();
    bad.domain_metadata.as_mut().unwrap()["forvo"]["page_url"] =
        serde_json::json!("https://forvo.com/?token=secret");
    assert!(
        PronunciationDomainPolicy
            .validate_record_evidence(&bad)
            .is_err()
    );
    bad = web;
    bad.domain_metadata.as_mut().unwrap()["unexpected"] = serde_json::json!(true);
    assert!(
        PronunciationDomainPolicy
            .validate_record_evidence(&bad)
            .is_err()
    );
    store.verify_integrity().unwrap();
}

#[test]
fn forvo_batch_snapshots_resume_without_network_and_keeps_completed_items() {
    use asset_store::batch_runtime::SafeBatchRuntime;
    use asset_store::forvo::{self, batch as web_batch};
    let workspace = TempWorkspace::create("forvo-batch-resume").unwrap();
    let store = store(&workspace.path().join("audio"));
    let request = request();
    let plan = web_batch::FetchPlan {
        schema_version: 1,
        items: vec![web_batch::FetchItem {
            surface: request.surface.clone(),
            reading: request.reading.clone(),
            replace_expected_sha256: None,
        }],
    };
    web_batch::start(
        &store,
        "web-resume",
        plan,
        web_batch::Transport::CdpChromium,
    )
    .unwrap();
    let acquisition = web_acquisition(audio(), &request);
    {
        let mut runtime = SafeBatchRuntime::open(store.root(), "web-resume").unwrap();
        let mut batch: web_batch::ForvoBatch = runtime.load().unwrap().unwrap();
        let blob = runtime.persist_blob(audio(), "mp3").unwrap();
        batch.items[0].acquisition = Some(web_batch::StoredAcquisition {
            blob,
            evidence: acquisition.evidence.clone(),
        });
        batch.revision += 1;
        runtime.save(&batch).unwrap();
    }
    // Смерть после импорта до outcome: возобновление повторяет ровно те же байты.
    forvo::import_acquisition(&store, &request, &acquisition, None).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let batch = runtime
        .block_on(web_batch::resume(
            &store,
            "web-resume",
            web_batch::Backend::Cdp {
                profile: Path::new("/does-not-exist/no-browser-profile"),
            },
            1,
        ))
        .unwrap();
    assert_eq!(batch.items[0].outcome.as_ref().unwrap().code, "imported");
    // Восстановление прежнего состояния: завершённый пакет не должен оставаться paused.
    {
        let mut runtime = SafeBatchRuntime::open(store.root(), "web-resume").unwrap();
        let mut batch: web_batch::ForvoBatch = runtime.load().unwrap().unwrap();
        batch.paused = Some(web_batch::Failure {
            code: "dom_changed".into(),
            message: "прежний блокер".into(),
            report: None,
        });
        batch.revision += 1;
        runtime.save(&batch).unwrap();
    }
    let batch = runtime
        .block_on(web_batch::resume(
            &store,
            "web-resume",
            web_batch::Backend::Cdp {
                profile: Path::new("/does-not-exist/no-browser-profile"),
            },
            1,
        ))
        .unwrap();
    assert!(batch.paused.is_none());
    let revision = batch.revision;
    assert_eq!(
        runtime
            .block_on(web_batch::resume(
                &store,
                "web-resume",
                web_batch::Backend::Cdp {
                    profile: Path::new("/does-not-exist/no-browser-profile"),
                },
                1
            ))
            .unwrap()
            .revision,
        revision
    );
    assert!(web_batch::cancel(&store, "web-resume").unwrap().cancelled);
    assert!(
        runtime
            .block_on(web_batch::resume(
                &store,
                "web-resume",
                web_batch::Backend::Cdp {
                    profile: Path::new("/does-not-exist/no-browser-profile"),
                },
                1
            ))
            .is_err()
    );
    store.verify_integrity().unwrap();
}

#[test]
fn japanese_identity_nfc_only_and_safe_filename() {
    let decomposed = Pronunciation::new("か\u{3099}", "か\u{3099}").unwrap();
    assert_eq!(decomposed, Pronunciation::new("が", "が").unwrap());
    assert_ne!(
        Pronunciation::new("ｶﾞ", "が").unwrap().identity(),
        decomposed.identity()
    );
    assert_ne!(
        Pronunciation::new("生", "せい").unwrap().identity(),
        Pronunciation::new("生", "なま").unwrap().identity()
    );
    for surface in [
        "",
        "..",
        "../こいつ",
        "/こいつ",
        "こ\\いつ",
        "こ:いつ",
        "こいつ ",
        "こ\nいつ",
        "a?",
        "a]",
        "CON",
        "nul",
        "LPT1",
        "a|",
        "a*",
        "a\"",
        "a<",
    ] {
        assert!(
            Pronunciation::new(surface, "こいつ").is_err(),
            "{surface:?}"
        );
    }
    assert!(Pronunciation::new("a", &"\"".repeat(180)).is_ok());
    assert_eq!(request().consumer_filename(), "こいつ.mp3");
    let generic = GenericDomainPolicy;
    assert!(generic.validate_identity(&request().identity()).is_err());
}
#[test]
fn actual_tagged_untagged_vbr_stereo_and_mpeg2_decode() {
    for bytes in [
        audio().to_vec(),
        fixture(440, false, 44100, false),
        fixture(440, true, 22050, true),
        fixture(440, false, 11025, false),
    ] {
        let probe = probe_mp3(&bytes).unwrap();
        assert_eq!(probe.codec, "mp3");
        assert_eq!(probe.sha256, sha256_hex(&bytes));
        assert!(probe.decoded_duration_seconds >= 0.59 && probe.decoded_duration_seconds < 0.9);
        assert_eq!(probe.byte_length, bytes.len() as u64);
        assert_eq!(DetectedFormat::from_signature(&bytes), DetectedFormat::Mp3);
    }
    // ID3 APIC — допустимый тег; доверяется и декодируется только звуковой поток.
    let ws = TempWorkspace::create("audio-cover").unwrap();
    let original = ws.path().join("original.mp3");
    let cover = ws.path().join("cover.png");
    let tagged = ws.path().join("covered.mp3");
    fs::write(&original, audio()).unwrap();
    image::RgbImage::from_pixel(4, 4, image::Rgb([20, 40, 60]))
        .save(&cover)
        .unwrap();
    let attached = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&original)
        .arg("-i")
        .arg(&cover)
        .args([
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c",
            "copy",
            "-id3v2_version",
            "3",
        ])
        .arg(&tagged)
        .output()
        .unwrap();
    assert!(
        attached.status.success(),
        "{}",
        String::from_utf8_lossy(&attached.stderr)
    );
    assert!(probe_mp3(&fs::read(tagged).unwrap()).is_ok());
    let mut id3v1 = audio().to_vec();
    id3v1.extend_from_slice(b"TAG");
    id3v1.extend_from_slice(&[0; 125]);
    assert!(probe_mp3(&id3v1).is_ok());
}
#[test]
fn rejects_disguised_corrupt_truncated_and_lying_timeline() {
    let mut broken = audio().to_vec();
    let middle = broken.len() / 2;
    broken.drain(middle..middle + 8);
    let mut wrong_duration = audio().to_vec();
    let marker = wrong_duration
        .windows(4)
        .position(|w| w == b"Xing" || w == b"Info")
        .unwrap();
    // За флагами Xing идёт число кадров: заявляем в десять раз больше аудио.
    let frames = u32::from_be_bytes(wrong_duration[marker + 8..marker + 12].try_into().unwrap());
    wrong_duration[marker + 8..marker + 12].copy_from_slice(&(frames * 10).to_be_bytes());
    for bytes in [
        vec![],
        b"<html>audio</html>".to_vec(),
        b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec(),
        b"ID3\x04\0\0\0\0\0\0".to_vec(),
        audio()[..audio().len() - 1].to_vec(),
        broken,
        wrong_duration,
    ] {
        let error = probe_mp3(&bytes).unwrap_err();
        assert_eq!(error.details["sha256"], sha256_hex(&bytes));
        assert_eq!(error.details["byte_length"], bytes.len());
    }
    let mut oversize = vec![0; MAX_AUDIO_BYTES as usize + 1];
    oversize[..3].copy_from_slice(b"ID3");
    assert!(probe_mp3(&oversize).is_err());
}
#[test]
fn candidate_requires_human_and_returns_original_bytes_and_surface_name() {
    let ws = TempWorkspace::create("audio-trust").unwrap();
    let store = store(&ws.path().join("store"));
    let record = import(&store, &request());
    assert_eq!(record.format, DetectedFormat::Mp3);
    assert_eq!(record.lifecycle, LifecycleState::Quarantined);
    assert_eq!(
        record.validation.as_ref().unwrap().status,
        SemanticStatus::Uncertain
    );
    assert_eq!(record.provenance.source_name, "pronunciation_ja_こいつ.mp3");
    assert!(read_trusted(store.root(), &[request()]).is_err());
    let repeated = import(&store, &request());
    assert_eq!(repeated, record);
    assert!(attest(&store, &request(), &"0".repeat(64), HumanDecision::Approve).is_err());
    assert!(
        attest(
            &store,
            &Pronunciation::new("こいつ", "あいつ").unwrap(),
            &record.sha256,
            HumanDecision::Approve
        )
        .is_err()
    );
    attest(&store, &request(), &record.sha256, HumanDecision::Approve).unwrap();
    let trusted = read_trusted(store.root(), &[request()])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(trusted.bytes, audio());
    assert_eq!(trusted.record.consumer_filename, "こいつ.mp3");
    assert_eq!(
        trusted.record.validation.as_ref().unwrap().status,
        SemanticStatus::Uncertain
    );
    assert_eq!(
        trusted.record.human_attestation.unwrap().identity,
        request().identity()
    );
    assert!(store.journal_check().unwrap().is_clean());
}
#[test]
fn readings_and_equivalent_names_store_independently_but_conflict_on_consumer_selection() {
    let ws = TempWorkspace::create("audio-collision").unwrap();
    let store = store(&ws.path().join("store"));
    let a = Pronunciation::new("生", "なま").unwrap();
    let b = Pronunciation::new("生", "せい").unwrap();
    let first = import(&store, &a);
    let second = import(&store, &b);
    assert_ne!(first.storage_path, second.storage_path);
    assert_eq!(first.consumer_filename, second.consumer_filename);
    attest(&store, &a, &first.sha256, HumanDecision::Approve).unwrap();
    attest(&store, &b, &second.sha256, HumanDecision::Approve).unwrap();
    assert_eq!(store.verify_integrity().unwrap().len(), 2);
    assert!(read_trusted(store.root(), std::slice::from_ref(&a)).is_ok());
    assert!(read_trusted(store.root(), std::slice::from_ref(&b)).is_ok());
    assert_eq!(
        read_trusted(store.root(), &[a, b]).unwrap_err().code,
        ErrorCode::IdentityConflict
    );
    let a = Pronunciation::new("Voice", "a").unwrap();
    let b = Pronunciation::new("voice", "b").unwrap();
    for r in [&a, &b] {
        let record = import(&store, r);
        attest(&store, r, &record.sha256, HumanDecision::Approve).unwrap();
    }
    assert_eq!(
        read_trusted(store.root(), &[a, b]).unwrap_err().code,
        ErrorCode::IdentityConflict
    );
}
#[test]
fn rejection_reimport_replacement_and_stale_cas_are_safe() {
    let ws = TempWorkspace::create("audio-cas").unwrap();
    let store = store(&ws.path().join("store"));
    let original = import(&store, &request());
    attest(&store, &request(), &original.sha256, HumanDecision::Reject).unwrap();
    assert_eq!(
        import(&store, &request()).current_human_decision(),
        Some(HumanDecision::Reject)
    );
    assert!(read_trusted(store.root(), &[request()]).is_err());
    attest(&store, &request(), &original.sha256, HumanDecision::Approve).unwrap();
    for expected in [None, Some("0".repeat(64))] {
        assert_eq!(
            import_bytes(&store, &request(), other_audio(), "other.mp3", expected)
                .unwrap_err()
                .code,
            ErrorCode::IdentityConflict
        );
    }
    let changed = import_bytes(
        &store,
        &request(),
        other_audio(),
        "other.mp3",
        Some(original.sha256.clone()),
    )
    .unwrap();
    assert_ne!(changed.sha256, original.sha256);
    assert!(changed.human_attestation.is_none());
    assert!(attest(&store, &request(), &original.sha256, HumanDecision::Approve).is_err());
    assert!(read_trusted(store.root(), &[request()]).is_err());
    attest(&store, &request(), &changed.sha256, HumanDecision::Approve).unwrap();
    assert_eq!(
        read_trusted(store.root(), &[request()]).unwrap()[0].bytes,
        other_audio()
    );
}
#[test]
fn unvalidated_and_corrupt_candidates_cannot_be_human_approved() {
    let ws = TempWorkspace::create("audio-pending").unwrap();
    let store = store(&ws.path().join("store"));
    let path = ws.path().join("candidate.mp3");
    fs::write(&path, audio()).unwrap();
    let raw = store
        .ingest(IngestRequest {
            identity: request().identity(),
            source_path: path,
            expected_source_sha256: None,
            domain_metadata: Some(serde_json::json!(request())),
            replace_expected_sha256: None,
        })
        .unwrap()
        .asset;
    assert!(attest(&store, &request(), &raw.sha256, HumanDecision::Approve).is_err());
    store
        .validate_exact(&request().identity(), &raw.sha256, &AudioTechnicalValidator)
        .unwrap();
    // Подмена физического кандидата после проверки.
    fs::set_permissions(
        store.root().join(".runtime").join(&raw.storage_path),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::write(
        store.root().join(".runtime").join(&raw.storage_path),
        other_audio(),
    )
    .unwrap();
    assert!(attest(&store, &request(), &raw.sha256, HumanDecision::Approve).is_err());
}
#[test]
fn trusted_read_rechecks_bytes_and_refuses_symlinks() {
    let ws = TempWorkspace::create("audio-read-integrity").unwrap();
    let store = store(&ws.path().join("store"));
    let record = import(&store, &request());
    attest(&store, &request(), &record.sha256, HumanDecision::Approve).unwrap();
    let path = store.root().join(&record.storage_path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, other_audio()).unwrap();
    assert!(read_trusted(store.root(), &[request()]).is_err());
    fs::remove_file(&path).unwrap();
    let target = ws.path().join("target.mp3");
    fs::write(&target, audio()).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(read_trusted(store.root(), &[request()]).is_err());
}

#[test]
fn import_returns_the_record_committed_by_its_validation_pass() {
    let ws = TempWorkspace::create("audio-import-returned-record").unwrap();
    let store = store(&ws.path().join("store"));

    let imported = import(&store, &request());
    let stored = store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == request().identity())
        .unwrap();

    assert_eq!(imported, stored);
}

#[test]
fn reimport_revalidates_stale_technical_version_for_unchanged_bytes() {
    let ws = TempWorkspace::create("audio-reimport-stale-validator").unwrap();
    let store = store(&ws.path().join("store"));
    let original = import(&store, &request());
    attest(&store, &request(), &original.sha256, HumanDecision::Approve).unwrap();

    let manifest_path = store.root().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let identity = serde_json::to_value(request().identity()).unwrap();
    let asset = manifest["assets"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|asset| asset["identity"] == identity)
        .unwrap();
    asset["validation"]["validator"]["version"] = serde_json::json!("0");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert!(read_trusted(store.root(), &[request()]).is_err());

    let reimported = import(&store, &request());

    assert_eq!(reimported.sha256, original.sha256);
    assert_eq!(
        reimported.validation.as_ref().unwrap().validator,
        technical_validator_identity()
    );
    assert_eq!(
        reimported.current_human_decision(),
        Some(HumanDecision::Approve)
    );
    assert_eq!(
        read_trusted(store.root(), &[request()]).unwrap()[0].bytes,
        audio()
    );
}

#[test]
fn invalid_identity_does_not_abort_other_verified_variant_visits() {
    let ws = TempWorkspace::create("audio-variant-invalid-request").unwrap();
    let store = store(&ws.path().join("store"));
    let record = import(&store, &request());
    attest(&store, &request(), &record.sha256, HumanDecision::Approve).unwrap();
    let invalid = AssetIdentity::new("pronunciation_audio", "not-a-canonical-pair").unwrap();
    let requests = [
        (invalid, DetectedFormat::Mp3),
        (request().identity(), DetectedFormat::Mp3),
    ];
    let mut visited = Vec::new();

    AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &requests,
        &technical_validator_identity(),
        &PronunciationDomainPolicy,
        |index, result| {
            visited.push((
                index,
                result
                    .map(|bytes| bytes.record.sha256)
                    .map_err(|error| error.code),
            ));
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(
        visited,
        [(0, Err(ErrorCode::InvalidIdentity)), (1, Ok(record.sha256)),]
    );
}

fn plan(items: &[(&str, &str, &str, &[u8])]) -> ImportPlan {
    ImportPlan {
        schema_version: 1,
        items: items
            .iter()
            .map(|(surface, reading, file, bytes)| ImportItem {
                surface: (*surface).into(),
                reading: (*reading).into(),
                file: (*file).into(),
                expected_source_sha256: sha256_hex(bytes),
                replace_expected_sha256: None,
            })
            .collect(),
    }
}
#[test]
fn batch_size_limit_is_checked_before_persisting_source_blobs() {
    let ws = TempWorkspace::create("audio-batch-size-limit").unwrap();
    let store = store(&ws.path().join("store"));
    let large = vec![0; MAX_AUDIO_BYTES as usize];
    let large_sha = sha256_hex(&large);
    let final_source = [1];
    let items = [
        ("一", "いち"),
        ("二", "に"),
        ("三", "さん"),
        ("四", "し"),
        ("五", "ご"),
        ("六", "ろく"),
        ("七", "しち"),
        ("八", "はち"),
        ("九", "きゅう"),
    ];
    let plan = ImportPlan {
        schema_version: 1,
        items: items
            .iter()
            .enumerate()
            .map(|(index, (surface, reading))| ImportItem {
                surface: (*surface).into(),
                reading: (*reading).into(),
                file: format!("item-{index}.mp3").into(),
                expected_source_sha256: if index == items.len() - 1 {
                    sha256_hex(final_source)
                } else {
                    large_sha.clone()
                },
                replace_expected_sha256: None,
            })
            .collect(),
    };

    let error = start(&store, "oversized", plan, ws.path(), |path| {
        if path.file_name().unwrap() == "item-8.mp3" {
            Ok(final_source.to_vec())
        } else {
            Ok(large.clone())
        }
    })
    .unwrap_err();

    assert!(error.message.contains("суммарный размер пакета"));
    let batch_directory = store.root().join(".runtime/batches/oversized");
    assert_eq!(
        fs::read_dir(batch_directory.join("candidates"))
            .unwrap()
            .count(),
        0
    );
    assert!(!batch_directory.join("state.json").exists());
}
#[test]
fn batch_snapshots_resume_partial_outcomes_and_never_recounts_completed_items() {
    let ws = TempWorkspace::create("audio-batch").unwrap();
    let store = store(&ws.path().join("store"));
    let plan = plan(&[
        ("一", "いち", "one.mp3", audio()),
        ("二", "に", "two.mp3", other_audio()),
        ("三", "さん", "bad.mp3", b"HTML"),
    ]);
    let initial = start(&store, "local", plan, ws.path(), |path| {
        Ok(match path.file_name().unwrap().to_str().unwrap() {
            "one.mp3" => audio(),
            "two.mp3" => other_audio(),
            _ => b"HTML",
        }
        .to_vec())
    })
    .unwrap();
    assert_eq!(initial.revision, 0);
    let partial = resume(&store, "local", 1).unwrap();
    assert_eq!(partial.revision, 1);
    assert_eq!(
        partial.items.iter().filter(|i| i.outcome.is_some()).count(),
        1
    );
    drop(store);
    let reopened = AssetStore::open_existing_with_policy(
        StoreOptions::new(ws.path().join("store")),
        PronunciationDomainPolicy,
    )
    .unwrap();
    let completed = resume(&reopened, "local", 128).unwrap();
    assert_eq!(
        completed.items[0].outcome.as_ref().unwrap().code,
        "imported"
    );
    assert_eq!(
        completed.items[1].outcome.as_ref().unwrap().code,
        "imported"
    );
    assert_eq!(
        completed.items[2].outcome.as_ref().unwrap().code,
        "validator_failure"
    );
    assert_eq!(reopened.verify_integrity().unwrap().len(), 2);
    let before = fs::read(reopened.root().join(".runtime/manifest.json")).unwrap();
    assert_eq!(
        resume(&reopened, "local", 128).unwrap().revision,
        completed.revision
    );
    assert_eq!(
        fs::read(reopened.root().join(".runtime/manifest.json")).unwrap(),
        before
    );
    assert_eq!(
        status(&reopened, "local").unwrap().revision,
        completed.revision
    );
}
#[test]
fn batch_replay_after_ingest_before_checkpoint_is_idempotent_and_cancel_is_terminal() {
    let ws = TempWorkspace::create("audio-batch-replay").unwrap();
    let store = store(&ws.path().join("store"));
    start(
        &store,
        "replay",
        plan(&[("こいつ", "こいつ", "source.mp3", audio())]),
        ws.path(),
        |_| Ok(audio().to_vec()),
    )
    .unwrap();
    let record = import(&store, &request());
    attest(&store, &request(), &record.sha256, HumanDecision::Approve).unwrap();
    let before = fs::read(store.root().join("manifest.json")).unwrap();
    resume(&store, "replay", 128).unwrap();
    assert_eq!(
        fs::read(store.root().join("manifest.json")).unwrap(),
        before
    );
    start(
        &store,
        "cancelled",
        plan(&[("別", "べつ", "source.mp3", audio())]),
        ws.path(),
        |_| Ok(audio().to_vec()),
    )
    .unwrap();
    assert!(cancel(&store, "cancelled").unwrap().cancelled);
    assert!(resume(&store, "cancelled", 128).is_err());
    assert_eq!(store.verify_integrity().unwrap().len(), 1);
}
#[test]
fn strict_batch_plan_identity_source_sha_and_runtime_blob_are_checked() {
    assert!(
        serde_json::from_str::<ImportPlan>(r#"{"schema_version":1,"schema_version":1,"items":[]}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<ImportPlan>(r#"{"schema_version":1,"items":[],"unknown":true}"#)
            .is_err()
    );
    let ws = TempWorkspace::create("audio-batch-input").unwrap();
    let store = store(&ws.path().join("store"));
    let mut unsupported = plan(&[("一", "いち", "a.mp3", audio())]);
    unsupported.schema_version = 2;
    assert!(
        start(&store, "unsupported", unsupported, ws.path(), |_| panic!(
            "не читать"
        ))
        .is_err()
    );
    let duplicate = plan(&[
        ("が", "が", "a.mp3", audio()),
        ("か\u{3099}", "が", "b.mp3", audio()),
    ]);
    assert!(
        start(&store, "duplicate", duplicate, ws.path(), |_| panic!(
            "не читать"
        ))
        .is_err()
    );
    let stale = start(
        &store,
        "stale",
        plan(&[("一", "いち", "a.mp3", audio())]),
        ws.path(),
        |_| Ok(other_audio().to_vec()),
    )
    .unwrap();
    assert_eq!(
        stale.items[0].outcome.as_ref().unwrap().code,
        "integrity_mismatch"
    );
    let snap = start(
        &store,
        "tamper",
        plan(&[("一", "いち", "a.mp3", audio())]),
        ws.path(),
        |_| Ok(audio().to_vec()),
    )
    .unwrap();
    fs::write(
        store
            .root()
            .join(".runtime/batches/tamper")
            .join(&snap.items[0].blob.as_ref().unwrap().storage_path),
        other_audio(),
    )
    .unwrap();
    assert!(resume(&store, "tamper", 128).is_err());
    assert!(store.verify_integrity().unwrap().is_empty());
}
fn cli(ws: &TempWorkspace, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(ws.path())
        .arg("--store")
        .arg(ws.path().join("store"))
        .args(args)
        .output()
        .unwrap()
}
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)))
}
#[test]
fn forvo_login_without_gui_reports_a_blocker_before_creating_profile_or_store() {
    let workspace = TempWorkspace::create("forvo-no-gui").unwrap();
    let profile = workspace.path().join("profile");
    let output = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(workspace.path())
        .arg("forvo")
        .arg("--profile")
        .arg(&profile)
        .arg("login")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(json(&output)["error"]["details"]["code"], "gui_unavailable");
    assert!(!profile.exists());
    assert!(!workspace.path().join(".asset-store").exists());
}

#[test]
fn forvo_checkpoint_survives_missing_prlimit_and_resumes_without_browser() {
    use asset_store::batch_runtime::SafeBatchRuntime;
    use asset_store::forvo::batch as web;
    let workspace = TempWorkspace::create("forvo-checkpoint-resume").unwrap();
    let store = store(&workspace.path().join("store"));
    let request = request();
    let acquisition = web_acquisition(audio(), &request);
    web::start(
        &store,
        "checkpoint",
        web::FetchPlan {
            schema_version: 1,
            items: vec![web::FetchItem {
                surface: request.surface.clone(),
                reading: request.reading.clone(),
                replace_expected_sha256: None,
            }],
        },
        web::Transport::CdpChromium,
    )
    .unwrap();
    let mut runtime = SafeBatchRuntime::open(store.root(), "checkpoint").unwrap();
    let mut batch: web::ForvoBatch = runtime.load().unwrap().unwrap();
    let blob = runtime.persist_blob(&acquisition.bytes, "mp3").unwrap();
    let sha = blob.sha256.clone();
    batch.items[0].acquisition = Some(web::StoredAcquisition {
        blob,
        evidence: acquisition.evidence,
    });
    batch.revision += 1;
    runtime.save(&batch).unwrap();
    drop(runtime);
    let tools = workspace.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let run = |path: &Path| {
        Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
            .arg("--repository-root")
            .arg(workspace.path())
            .arg("--store")
            .arg(store.root())
            .args(["forvo", "batch-resume", "--batch-id", "checkpoint"])
            .env("PATH", path)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    };
    let blocked = run(&tools);
    assert_eq!(
        blocked.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    let value = json(&blocked);
    assert_eq!(value["outcome"], "paused");
    assert_eq!(value["result"]["paused"]["code"], "audio_tool_missing");
    assert!(value["result"]["items"][0]["outcome"].is_null());
    assert_eq!(
        value["result"]["items"][0]["acquisition"]["blob"]["sha256"],
        sha
    );
    let first_log = value["result"]["diagnostic_log"].as_str().unwrap();
    let lines = fs::read_to_string(first_log).unwrap();
    assert!(
        lines
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(lines.contains("browser_acquisition_stage"));
    assert!(lines.contains("import_mp3"));
    assert!(lines.contains("audio_tool_missing"));
    assert!(!lines.contains("fixture-speaker"));
    let original_path = std::env::var_os("PATH").expect("для теста нужен PATH с FFmpeg и prlimit");
    let restored = run(Path::new(&original_path));
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stdout)
    );
    let value = json(&restored);
    assert_eq!(value["result"]["items"][0]["outcome"]["code"], "imported");
    assert_eq!(
        value["result"]["items"][0]["acquisition"]["blob"]["sha256"],
        sha
    );
    assert!(!workspace.path().join(".codex/local/forvo-profile").exists());
    assert_ne!(value["result"]["diagnostic_log"], first_log);
    let asset = store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|a| a.identity == request.identity())
        .unwrap();
    assert_eq!(asset.sha256, sha);
    assert_eq!(asset.validation.unwrap().status, SemanticStatus::Uncertain);
}
#[test]
fn forvo_cli_strict_plan_status_and_cancel_do_not_require_browser() {
    let workspace = TempWorkspace::create("forvo-plan-cli").unwrap();
    let plan = workspace.path().join("plan.json");
    fs::write(
        &plan,
        r#"{"schema_version":1,"items":[{"surface":"元気","reading":"げんき"}]}"#,
    )
    .unwrap();
    let output = cli(
        &workspace,
        &[
            "forvo",
            "batch-start",
            "--batch-id",
            "offline-web",
            "--plan",
            plan.to_str().unwrap(),
            "--max-items",
            "0",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        cli(
            &workspace,
            &["forvo", "batch-status", "--batch-id", "offline-web"]
        )
        .status
        .success()
    );
    assert_eq!(
        json(&cli(
            &workspace,
            &["forvo", "batch-cancel", "--batch-id", "offline-web"]
        ))["result"]["cancelled"],
        true
    );
    let stopped = cli(
        &workspace,
        &["forvo", "batch-resume", "--batch-id", "offline-web"],
    );
    assert!(!stopped.status.success());
    let stopped_json = json(&stopped);
    let log = stopped_json["error"]["details"]["diagnostic_log"]
        .as_str()
        .unwrap();
    let log = fs::read_to_string(log).unwrap();
    assert!(
        log.lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(log.contains("run_stopped"));
    fs::write(&plan,r#"{"schema_version":1,"schema_version":1,"items":[{"surface":"元気","reading":"げんき"}]}"#).unwrap();
    assert!(
        !cli(
            &workspace,
            &[
                "forvo",
                "batch-start",
                "--batch-id",
                "duplicate",
                "--plan",
                plan.to_str().unwrap(),
                "--max-items",
                "0"
            ]
        )
        .status
        .success()
    );
    fs::write(&plan,r#"{"schema_version":1,"items":[{"surface":"元気","reading":"げんき","password":"forbidden"}]}"#).unwrap();
    assert!(
        !cli(
            &workspace,
            &[
                "forvo",
                "batch-start",
                "--batch-id",
                "unknown-field",
                "--plan",
                plan.to_str().unwrap(),
                "--max-items",
                "0"
            ]
        )
        .status
        .success()
    );
}
#[test]
fn cli_real_local_import_attest_read_and_resume() {
    let ws = TempWorkspace::create("audio-cli").unwrap();
    let path = ws.path().join("pronunciation_ja_こいつ.mp3");
    fs::write(&path, audio()).unwrap();
    let args = [
        "import",
        "--surface",
        "こいつ",
        "--reading",
        "こいつ",
        "--file",
        path.to_str().unwrap(),
    ];
    let imported = cli(&ws, &args);
    assert!(imported.status.success(), "{:?}", json(&imported));
    let value = json(&imported);
    assert_eq!(value["result"]["asset"]["consumer_filename"], "こいつ.mp3");
    assert_eq!(value["result"]["asset"]["lifecycle"], "quarantined");
    let sha = value["result"]["asset"]["sha256"].as_str().unwrap();
    let accepted = cli(
        &ws,
        &[
            "attest",
            "--surface",
            "こいつ",
            "--reading",
            "こいつ",
            "--sha256",
            sha,
            "--decision",
            "approve",
            "--reason",
            "Прослушал ожидаемое чтение",
        ],
    );
    assert!(accepted.status.success(), "{:?}", json(&accepted));
    let requests = ws.path().join("requests.json");
    fs::write(
        &requests,
        serde_json::to_vec(&serde_json::json!({"schema_version":1,"items":[request()]})).unwrap(),
    )
    .unwrap();
    let read = cli(&ws, &["read", "--requests", requests.to_str().unwrap()]);
    assert!(read.status.success(), "{:?}", json(&read));
    assert_eq!(
        json(&read)["result"]["assets"][0]["consumer_filename"],
        "こいつ.mp3"
    );
    let plan_path = ws.path().join("plan.json");
    fs::write(
        &plan_path,
        serde_json::to_vec(&plan(&[
            ("一", "いち", "pronunciation_ja_こいつ.mp3", audio()),
            ("二", "に", "pronunciation_ja_こいつ.mp3", audio()),
        ]))
        .unwrap(),
    )
    .unwrap();
    let start = cli(
        &ws,
        &[
            "batch-start",
            "--batch-id",
            "cli",
            "--plan",
            plan_path.to_str().unwrap(),
            "--max-items",
            "1",
        ],
    );
    assert!(start.status.success(), "{:?}", json(&start));
    fs::remove_file(&path).unwrap();
    let finish = cli(&ws, &["batch-resume", "--batch-id", "cli"]);
    assert!(finish.status.success(), "{:?}", json(&finish));
    assert_eq!(json(&finish)["result"]["revision"], 2);
}
#[test]
fn cli_rejects_symlinks_paths_decks_and_source_races() {
    let ws = TempWorkspace::create("audio-cli-boundary").unwrap();
    let file = ws.path().join("source.mp3");
    fs::write(&file, audio()).unwrap();
    let link = ws.path().join("link.mp3");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let import = |path: &Path| {
        cli(
            &ws,
            &[
                "import",
                "--surface",
                "こいつ",
                "--reading",
                "こいつ",
                "--file",
                path.to_str().unwrap(),
            ],
        )
    };
    assert!(!import(&link).status.success());
    let folder = ws.path().join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("audio.mp3"), audio()).unwrap();
    let folder_link = ws.path().join("folder-link");
    std::os::unix::fs::symlink(&folder, &folder_link).unwrap();
    assert!(!import(&folder_link.join("audio.mp3")).status.success());
    // Синтетический защищённый путь: файл не создаётся и не открывается.
    let denied = import(&ws.path().join("decks/input/forbidden.mp3"));
    assert_eq!(json(&denied)["error"]["code"], "boundary_violation");
    let stale = cli(
        &ws,
        &[
            "import",
            "--surface",
            "こいつ",
            "--reading",
            "こいつ",
            "--file",
            file.to_str().unwrap(),
            "--expected-source-sha256",
            &"0".repeat(64),
        ],
    );
    assert_eq!(json(&stale)["error"]["code"], "integrity_mismatch");
    assert!(
        store(&ws.path().join("store"))
            .verify_integrity()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn concurrent_importers_cannot_silently_replace_each_other() {
    let ws = TempWorkspace::create("audio-concurrent").unwrap();
    let root = ws.path().join("store");
    drop(store(&root));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles = [audio().to_vec(), other_audio().to_vec()]
        .into_iter()
        .map(|bytes| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = store(&root);
                barrier.wait();
                import_bytes(&store, &request(), &bytes, "candidate.mp3", None)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
        ErrorCode::IdentityConflict
    );
    assert_eq!(store(&root).verify_integrity().unwrap().len(), 1);
}

#[test]
fn stale_or_mismatched_persisted_attestations_and_technical_versions_fail_closed() {
    let ws = TempWorkspace::create("audio-stale-evidence").unwrap();
    let store = store(&ws.path().join("store"));
    let record = import(&store, &request());
    attest(&store, &request(), &record.sha256, HumanDecision::Approve).unwrap();
    let path = store.root().join("manifest.json");
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut variants = Vec::new();
    let mut stale = original.clone();
    stale["assets"][0]["validation"]["validator"]["version"] = serde_json::json!("0");
    variants.push(stale);
    let mut identity = original.clone();
    identity["assets"][0]["human_attestation"]["identity"] =
        serde_json::json!(Pronunciation::new("こいつ", "べつ").unwrap().identity());
    variants.push(identity);
    let mut sha = original.clone();
    sha["assets"][0]["human_attestation"]["content_sha256"] = serde_json::json!("0".repeat(64));
    variants.push(sha);
    let mut request = original.clone();
    request["assets"][0]["validation"]["evidence"][0]["details"]["request"]["reading"] =
        serde_json::json!("べつ");
    variants.push(request);
    let mut diagnostics = original.clone();
    diagnostics["assets"][0]["validation"]["evidence"][0]["details"]["probe"]["schema_version"] =
        serde_json::json!(99);
    variants.push(diagnostics);
    for value in variants {
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(read_trusted(store.root(), &[self::request()]).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "API чтения не исправляет входной манифест"
        );
    }
    fs::write(&path, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
    assert_eq!(
        read_trusted(store.root(), &[self::request()]).unwrap()[0].bytes,
        audio()
    );
}

#[test]
fn stale_technical_version_does_not_block_other_trusted_records() {
    let ws = TempWorkspace::create("audio-stale-version-isolation").unwrap();
    let store = store(&ws.path().join("store"));
    let stale_request = request();
    let stale_record = import(&store, &stale_request);
    attest(
        &store,
        &stale_request,
        &stale_record.sha256,
        HumanDecision::Approve,
    )
    .unwrap();

    let current_request = Pronunciation::new("別", "べつ").unwrap();
    let current_record = import_bytes(
        &store,
        &current_request,
        other_audio(),
        "pronunciation_ja_別.mp3",
        None,
    )
    .unwrap();
    attest(
        &store,
        &current_request,
        &current_record.sha256,
        HumanDecision::Approve,
    )
    .unwrap();

    let path = store.root().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let stale_identity = serde_json::to_value(stale_request.identity()).unwrap();
    let stale_asset = manifest["assets"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|asset| asset["identity"] == stale_identity)
        .unwrap();
    stale_asset["validation"]["validator"]["version"] = serde_json::json!("0");
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    assert_eq!(store.verify_integrity().unwrap().len(), 2);
    assert!(read_trusted(store.root(), &[stale_request]).is_err());
    let trusted = read_trusted(store.root(), &[current_request]).unwrap();
    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].bytes, other_audio());
}

#[test]
fn technical_success_cannot_be_mislabeled_as_automatic_semantic_verified() {
    struct WrongValidator;
    impl SemanticValidator for WrongValidator {
        fn identity(&self) -> ValidatorIdentity {
            technical_validator_identity()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn std::io::Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "decode".into(),
                    summary: "MP3 декодирован".into(),
                    details: None,
                }],
            ))
        }
    }
    let ws = TempWorkspace::create("audio-no-auto-trust").unwrap();
    let store = store(&ws.path().join("store"));
    let result = store.ingest_verified(
        VerifiedIngestRequest {
            identity: request().identity(),
            bytes: audio().to_vec(),
            provenance: Provenance {
                source_kind: "local_import".into(),
                source_name: "candidate.mp3".into(),
            },
            domain_metadata: Some(serde_json::json!(request())),
            replace_expected_sha256: None,
        },
        &WrongValidator,
    );
    assert!(result.is_err());
    assert!(store.verify_integrity().unwrap().is_empty());
}
#[test]
fn cli_reports_missing_decoder_tools_explicitly() {
    let ws = TempWorkspace::create("audio-tools").unwrap();
    let path = ws.path().join("source.mp3");
    fs::write(&path, audio()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(ws.path())
        .args(["probe", "--file"])
        .arg(&path)
        .env("PATH", ws.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        json(&output)["error"]["details"]["blocker"],
        "audio_tool_missing"
    );
    assert_eq!(json(&output)["error"]["details"]["tool"], "prlimit");
}
#[test]
fn rejects_real_wav_and_audio_beyond_duration_limit() {
    let ws = TempWorkspace::create("audio-limits").unwrap();
    for (name, duration) in [("disguised.wav", "0.5"), ("long.mp3", "31")] {
        let path = ws.path().join(name);
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={duration}"),
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(probe_mp3(&fs::read(path).unwrap()).is_err());
    }
}

#[test]
fn stale_unapproved_candidate_requires_revalidation_before_trust_grant() {
    let ws = TempWorkspace::create("audio-stale-approval").unwrap();
    let store = store(&ws.path().join("store"));
    let candidate = import(&store, &request());
    assert!(candidate.human_attestation.is_none());
    let path = store.root().join(".runtime/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["assets"][0]["validation"]["validator"]["version"] = serde_json::json!("0");
    let stale = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(&path, &stale).unwrap();
    let canonical = fs::read(store.root().join("manifest.json")).unwrap();
    let journal = fs::read(store.root().join(".journal/events.jsonl")).unwrap();

    let error = attest(
        &store,
        &request(),
        &candidate.sha256,
        HumanDecision::Approve,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidValidationEvidence);
    assert!(error.message.contains("повторите импорт или validate"));
    assert_eq!(fs::read(&path).unwrap(), stale);
    assert_eq!(
        fs::read(store.root().join("manifest.json")).unwrap(),
        canonical
    );
    assert_eq!(
        fs::read(store.root().join(".journal/events.jsonl")).unwrap(),
        journal
    );
    let inspected = store.verify_integrity().unwrap();
    assert_eq!(inspected[0].lifecycle, LifecycleState::Quarantined);
    assert!(inspected[0].human_attestation.is_none());
    assert!(read_trusted(store.root(), &[request()]).is_err());

    let revalidated = import(&store, &request());
    assert_eq!(revalidated.sha256, candidate.sha256);
    assert_eq!(
        revalidated.validation.as_ref().unwrap().validator,
        technical_validator_identity()
    );
    assert!(revalidated.human_attestation.is_none());
    assert_eq!(revalidated.lifecycle, LifecycleState::Quarantined);
    let approved = attest(
        &store,
        &request(),
        &candidate.sha256,
        HumanDecision::Approve,
    )
    .unwrap();
    assert_eq!(approved.asset.lifecycle, LifecycleState::Verified);
    assert_eq!(
        read_trusted(store.root(), &[request()]).unwrap()[0].bytes,
        audio()
    );
}

#[test]
fn cli_allows_external_decks_but_never_reads_protected_sources() {
    let ws = TempWorkspace::create("audio-path-repository").unwrap();
    let external = TempWorkspace::create("audio-path-external").unwrap();
    let external_decks = external.path().join("decks");
    fs::create_dir(&external_decks).unwrap();
    let allowed = external_decks.join("audio.mp3");
    fs::write(&allowed, audio()).unwrap();
    let probe = |path: &Path| cli(&ws, &["probe", "--file", path.to_str().unwrap()]);
    let accepted = probe(&allowed);
    assert!(accepted.status.success(), "{:?}", json(&accepted));
    assert_eq!(json(&accepted)["result"]["sha256"], sha256_hex(audio()));

    let protected = ws.path().join("decks");
    fs::create_dir(&protected).unwrap();
    // Другие корректные байты позволяют обнаружить случайную выдачу защищённого файла.
    let secret = protected.join("audio.mp3");
    fs::write(&secret, other_audio()).unwrap();
    for path in [
        secret.clone(),
        ws.path().join("./decks//audio.mp3"),
        protected.join("missing.mp3"),
    ] {
        let denied = probe(&path);
        assert!(!denied.status.success());
        let value = json(&denied);
        assert_eq!(value["error"]["code"], "boundary_violation");
        assert!(value.get("result").is_none());
        assert!(!String::from_utf8_lossy(&denied.stdout).contains(&sha256_hex(other_audio())));
    }
    let traversal = probe(&external_decks.join("../decks/audio.mp3"));
    assert_eq!(json(&traversal)["error"]["code"], "path_traversal");
    let alias = external.path().join("alias");
    std::os::unix::fs::symlink(&protected, &alias).unwrap();
    let file_alias = external.path().join("alias.mp3");
    std::os::unix::fs::symlink(&secret, &file_alias).unwrap();
    for path in [alias.join("audio.mp3"), file_alias] {
        let denied = probe(&path);
        assert!(!denied.status.success());
        assert!(json(&denied).get("result").is_none());
    }
    assert!(!ws.path().join("store").exists());
    assert_eq!(fs::read(secret).unwrap(), other_audio());
}

#[test]
fn cli_read_streams_metadata_in_order_and_fails_as_a_whole() {
    let ws = TempWorkspace::create("audio-cli-read-selection").unwrap();
    let store = store(&ws.path().join("store"));
    let first = request();
    let second = Pronunciation::new("別", "べつ").unwrap();
    let mut records = Vec::new();
    for item in [&first, &second] {
        let record = import(&store, item);
        records.push(
            attest(&store, item, &record.sha256, HumanDecision::Approve)
                .unwrap()
                .asset,
        );
    }
    let requests = ws.path().join("requests.json");
    let read = |items: &[Pronunciation]| {
        fs::write(
            &requests,
            serde_json::to_vec(&serde_json::json!({"schema_version":1,"items":items})).unwrap(),
        )
        .unwrap();
        cli(&ws, &["read", "--requests", requests.to_str().unwrap()])
    };
    let selected = [second.clone(), first.clone()];
    let output = read(&selected);
    assert!(output.status.success(), "{:?}", json(&output));
    assert_eq!(
        json(&output),
        serde_json::json!({"schema_version":1,"outcome":"ok","result":{"assets":[records[1],records[0]]}})
    );
    let fail = |output: Output| {
        assert!(!output.status.success());
        let value = json(&output);
        assert_eq!(value["outcome"], "error");
        assert!(value.get("result").is_none());
        value
    };
    fail(read(&[
        first.clone(),
        Pronunciation::new("不存在", "ふそんざい").unwrap(),
    ]));
    let unapproved = Pronunciation::new("未確認", "みかくにん").unwrap();
    import(&store, &unapproved);
    fail(read(&[first.clone(), unapproved]));

    let manifest_path = store.root().join("manifest.json");
    let original = fs::read(&manifest_path).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let stale = manifest["assets"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["identity"] == serde_json::json!(second.identity()))
        .unwrap();
    stale["validation"]["validator"]["version"] = serde_json::json!("0");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        fail(read(&[first.clone(), second.clone()]))["error"]["code"],
        "invalid_validation_evidence"
    );
    fs::write(&manifest_path, original).unwrap();

    let conflict = Pronunciation::new("こいつ", "べつ").unwrap();
    let candidate = import(&store, &conflict);
    attest(&store, &conflict, &candidate.sha256, HumanDecision::Approve).unwrap();
    assert_eq!(
        fail(read(&[second.clone(), first.clone(), conflict.clone()]))["error"]["code"],
        "identity_conflict"
    );
    let mut visits = 0;
    let error = AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &[
            (second.identity(), DetectedFormat::Mp3),
            (first.identity(), DetectedFormat::Mp3),
            (conflict.identity(), DetectedFormat::Mp3),
        ],
        &technical_validator_identity(),
        &PronunciationDomainPolicy,
        |_, _| {
            visits += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::IdentityConflict);
    assert_eq!(visits, 0);

    let file = store.root().join(&records[1].storage_path);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, "повреждено".as_bytes()).unwrap();
    fail(read(&[first.clone(), second.clone()]));
    fs::remove_file(file).unwrap();
    fail(read(&[first, second]));
}

#[test]
fn cli_read_uses_the_streaming_store_boundary() {
    // Архитектурная граница: CLI накапливает только записи, а не пакет аудиобайтов.
    let cli = include_str!("../src/pronunciation_cli.rs");
    let read = cli
        .split("Action::Read { requests } =>")
        .nth(1)
        .unwrap()
        .split("Action::BatchStart")
        .next()
        .unwrap();
    assert!(read.contains("AssetStore::visit_verified_variants_with_policy("));
    assert!(read.contains("Vec<crate::AssetRecord>"));
    assert!(!read.contains("read_trusted("));
    assert!(!read.contains("VerifiedAssetBytes"));
}
