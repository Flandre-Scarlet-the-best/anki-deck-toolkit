use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

pub(crate) struct Signed {
    pub metadata: Vec<u8>,
    pub signature: Vec<u8>,
    pub zip: Vec<u8>,
    pub trust: TrustRoot,
    pub pkcs8: Vec<u8>,
}

/// Подписанный тестовым ключом выпуск из файлов (по умолчанию — встроенных).
pub(crate) fn signed(files: Option<Vec<PackageFile>>) -> Signed {
    let files = files.unwrap_or_else(package::bundled_files);
    let zip = package::build_zip(&files).unwrap();
    let metadata = ReleaseMetadata::new(&files, &zip, REPOSITORY, COMMIT)
        .unwrap()
        .to_bytes();
    let pkcs8 = generate_key().unwrap();
    let signature = sign(&metadata, &pkcs8).unwrap().to_bytes();
    let trust = TrustRoot::Explicit(
        TrustedKey::from_public_key_hex(&public_key_of(&pkcs8).unwrap(), None).unwrap(),
    );
    Signed {
        metadata,
        signature,
        zip,
        trust,
        pkcs8,
    }
}

fn code(error: &AssetError) -> String {
    crate::forvo::provider_code(error).to_owned()
}

fn resign(
    mut value: serde_json::Value,
    s: &Signed,
    edit: impl FnOnce(&mut serde_json::Value),
) -> (Vec<u8>, Vec<u8>) {
    edit(&mut value);
    let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
    bytes.push(b'\n');
    let signature = sign(&bytes, &s.pkcs8).unwrap().to_bytes();
    (bytes, signature)
}

#[test]
fn embedded_trust_root_is_well_formed() {
    // Пустой корень — честный BLOCKED, а не принятие любого ключа.
    let embedded = TrustRoot::embedded();
    if embedded.keys().is_empty() {
        let s = signed(None);
        let error = verify(&s.metadata, &s.signature, &s.zip, &embedded, None).unwrap_err();
        assert_eq!(code(&error), "release_trust_root_missing");
        assert_eq!(error.details["status"], "BLOCKED");
    }
}

#[test]
fn correctly_signed_release_is_accepted() {
    let s = signed(None);
    let verified = verify(
        &s.metadata,
        &s.signature,
        &s.zip,
        &s.trust,
        Some(&tag(&package::bundled_version())),
    )
    .unwrap();
    assert_eq!(verified.metadata.version, package::bundled_version());
    assert_eq!(verified.metadata.repository, REPOSITORY);
    assert!(verified.files.iter().any(|f| f.path == "forvo_page.js"));
    assert!(
        verified
            .files
            .iter()
            .all(|f| f.path != package::PAIRING_FILE)
    );
}

#[test]
fn tampering_and_wrong_provenance_are_rejected_before_install() {
    let s = signed(None);
    // Изменённый байт архива.
    let mut zip = s.zip.clone();
    let middle = zip.len() / 2;
    zip[middle] ^= 1;
    assert_eq!(
        code(&verify(&s.metadata, &s.signature, &zip, &s.trust, None).unwrap_err()),
        "release_archive_mismatch"
    );
    // Подменённый SHA в метаданных без новой подписи.
    let text = String::from_utf8(s.metadata.clone()).unwrap();
    let sha = sha256_hex(&s.zip);
    let forged = text.replace(&sha, &"0".repeat(64)).into_bytes();
    assert_eq!(
        code(&verify(&forged, &s.signature, &s.zip, &s.trust, None).unwrap_err()),
        "release_signature_invalid"
    );
    // Подпись чужим ключом.
    let other = generate_key().unwrap();
    let foreign = sign(&s.metadata, &other).unwrap().to_bytes();
    assert_eq!(
        code(&verify(&s.metadata, &foreign, &s.zip, &s.trust, None).unwrap_err()),
        "release_key_untrusted"
    );
    // Без подписи и с мусором вместо подписи.
    assert_eq!(
        code(&verify(&s.metadata, b"", &s.zip, &s.trust, None).unwrap_err()),
        "release_signature_invalid"
    );
    let mut sig: serde_json::Value = serde_json::from_slice(&s.signature).unwrap();
    sig["signature"] = json!("00".repeat(64));
    let zeroed = serde_json::to_vec(&sig).unwrap();
    assert_eq!(
        code(&verify(&s.metadata, &zeroed, &s.zip, &s.trust, None).unwrap_err()),
        "release_signature_invalid"
    );
    // Подписанный выпуск другой версии вместо запрошенной.
    assert_eq!(
        code(
            &verify(
                &s.metadata,
                &s.signature,
                &s.zip,
                &s.trust,
                Some("forvo-extension-v9.9.9")
            )
            .unwrap_err()
        ),
        "release_metadata_mismatch"
    );
}

type Mutation = Box<dyn FnOnce(&mut serde_json::Value)>;

#[test]
fn signed_but_inconsistent_metadata_is_rejected() {
    let s = signed(None);
    let base: serde_json::Value = serde_json::from_slice(&s.metadata).unwrap();
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "repository",
            Box::new(|v| v["repository"] = json!("someone/fork")),
            "release_metadata_mismatch",
        ),
        (
            "channel",
            Box::new(|v| v["channel"] = json!("other")),
            "release_metadata_mismatch",
        ),
        (
            "tag",
            Box::new(|v| v["tag"] = json!("v1.0")),
            "release_metadata_mismatch",
        ),
        (
            "protocol",
            Box::new(|v| v["compatibility"]["protocol"] = json!("forvo-adapter/v2")),
            "release_incompatible",
        ),
        (
            "version",
            Box::new(|v| {
                v["version"] = json!("99.0");
                v["tag"] = json!("forvo-extension-v99.0");
                v["archive"]["path"] = json!("forvo-extension-99.0.zip");
            }),
            "release_metadata_mismatch",
        ),
        (
            "files",
            Box::new(|v| v["files"].as_array_mut().unwrap().pop().map(drop).unwrap()),
            "release_metadata_mismatch",
        ),
        (
            "commit",
            Box::new(|v| v["commit"] = json!("main")),
            "release_metadata_mismatch",
        ),
        (
            "unknown_field",
            Box::new(|v| v["extra"] = json!(1)),
            "release_metadata_mismatch",
        ),
        (
            "permissions",
            Box::new(|v| v["compatibility"]["permissions"] = json!([])),
            "release_metadata_mismatch",
        ),
    ];
    for (name, edit, expected) in cases {
        let (metadata, signature) = resign(base.clone(), &s, edit);
        let error = verify(&metadata, &signature, &s.zip, &s.trust, None).unwrap_err();
        assert_eq!(code(&error), expected, "{name}");
    }
}

#[test]
fn manifest_version_must_match_signed_version() {
    // Архив с manifest другой версии, чем у подписанных метаданных.
    let mut files = package::bundled_files();
    let manifest = files
        .iter_mut()
        .find(|f| f.path == "manifest.json")
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&manifest.bytes).unwrap();
    value["version"] = json!("99.0");
    manifest.bytes = serde_json::to_vec_pretty(&value).unwrap();
    let swapped = package::build_zip(&files).unwrap();
    let s = signed(None);
    let base: serde_json::Value = serde_json::from_slice(&s.metadata).unwrap();
    let (metadata, signature) = resign(base, &s, |v| {
        v["archive"]["sha256"] = json!(sha256_hex(&swapped));
        v["archive"]["size"] = json!(swapped.len());
        v["files"] = json!(package::describe(&files));
    });
    let error = verify(&metadata, &signature, &swapped, &s.trust, None).unwrap_err();
    assert_eq!(code(&error), "release_metadata_mismatch");
    assert_eq!(error.details["field"], "manifest_version");
}

#[test]
fn trusted_key_ids_are_derived_from_keys() {
    let pkcs8 = generate_key().unwrap();
    let public = public_key_of(&pkcs8).unwrap();
    let key = TrustedKey::from_public_key_hex(&public, None).unwrap();
    assert_eq!(key.key_id.len(), 16);
    assert!(key.valid());
    assert_eq!(
        code(&TrustedKey::from_public_key_hex("abc", None).unwrap_err()),
        "trusted_key_invalid"
    );
    assert_eq!(
        code(&sign(b"x", b"not a key").unwrap_err()),
        "signing_key_invalid"
    );
}
