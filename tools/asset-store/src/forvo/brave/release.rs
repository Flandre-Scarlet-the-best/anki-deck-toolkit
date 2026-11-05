//! Релиз адаптера: подписанные метаданные, закреплённый корень доверия и
//! полная проверка архива до установки.
//!
//! Подлинность даёт подпись Ed25519 над точными байтами
//! `forvo-extension-release.json`. Ключ проверки закреплён в этом CLI
//! (`release_keys.json` в исходниках) и не берётся из того же источника, что
//! архив: SHA-256 архива доверен только потому, что он находится внутри
//! подписанных метаданных. Метаданные называют канал, тег, репозиторий,
//! коммит, протокол bridge и точный состав ZIP; любое расхождение — отказ до
//! изменения каталога установки.
use super::package::{self, FileEntry, ManifestFacts, PackageFile, Version};
use super::{PROTOCOL, WS_PATH, bridge_error};
use crate::AssetError;
use crate::hashing::{encode_lower_hex, sha256_hex};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Репозиторий, чьи GitHub Releases являются каналом обновлений адаптера.
pub const REPOSITORY: &str = "Flandre-Scarlet-the-best/anki-deck-toolkit";
/// Независимый канал имён: тег `forvo-extension-v<версия>`.
pub const CHANNEL: &str = "forvo-extension";
pub const TAG_PREFIX: &str = "forvo-extension-v";
pub const METADATA_ASSET: &str = "forvo-extension-release.json";
pub const SIGNATURE_ASSET: &str = "forvo-extension-release.json.sig";
pub const METADATA_SCHEMA: u32 = 1;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
const TRUST_FILE: &str = include_str!("release_keys.json");

pub fn tag(version: &str) -> String {
    format!("{TAG_PREFIX}{version}")
}

pub fn archive_name(version: &str) -> String {
    format!("forvo-extension-{version}.zip")
}

fn release_error(code: &str, action: &str) -> AssetError {
    bridge_error(code, action)
}

/// Совместимость пакета с CLI: протокол bridge и запрошенные разрешения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub protocol: String,
    pub ws_path: String,
    pub minimum_chrome_version: Option<String>,
    pub permissions: Vec<String>,
    pub host_permissions: Vec<String>,
}

/// Подписываемое описание одного выпуска адаптера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMetadata {
    pub schema_version: u32,
    pub channel: String,
    pub version: String,
    pub tag: String,
    pub repository: String,
    /// Коммит `main`, из которого собран пакет (40 hex).
    pub commit: String,
    pub compatibility: Compatibility,
    pub archive: FileEntry,
    /// Точный состав ZIP по именам; `pairing.json` не входит никогда.
    pub files: Vec<FileEntry>,
}

impl ReleaseMetadata {
    /// Метаданные пакета из проверенных файлов.
    pub fn new(
        files: &[PackageFile],
        zip: &[u8],
        repository: &str,
        commit: &str,
    ) -> Result<Self, AssetError> {
        let facts = package::validate_files(files)?;
        if !valid_commit(commit) || !valid_repository(repository) {
            return Err(release_error(
                "release_source_invalid",
                "Укажите репозиторий owner/name и 40-символьный SHA коммита",
            ));
        }
        Ok(Self {
            schema_version: METADATA_SCHEMA,
            channel: CHANNEL.into(),
            tag: tag(&facts.version),
            repository: repository.into(),
            commit: commit.into(),
            compatibility: compatibility(&facts),
            archive: FileEntry {
                path: archive_name(&facts.version),
                sha256: sha256_hex(zip),
                size: zip.len() as u64,
            },
            files: package::describe(files),
            version: facts.version,
        })
    }

    /// Канонические байты: стабильный JSON с переводом строки в конце.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("метаданные сериализуются");
        bytes.push(b'\n');
        bytes
    }
}

fn compatibility(facts: &ManifestFacts) -> Compatibility {
    Compatibility {
        protocol: PROTOCOL.into(),
        ws_path: WS_PATH.into(),
        minimum_chrome_version: facts.minimum_chrome_version.clone(),
        permissions: facts.permissions.clone(),
        host_permissions: facts.host_permissions.clone(),
    }
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_repository(repository: &str) -> bool {
    let parts: Vec<&str> = repository.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            (1..=100).contains(&p.len())
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

// --- Подпись и корень доверия ---------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureFile {
    pub schema_version: u32,
    pub algorithm: String,
    pub key_id: String,
    pub signature: String,
}

impl SignatureFile {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("подпись сериализуется");
        bytes.push(b'\n');
        bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedKey {
    pub key_id: String,
    /// Открытый ключ Ed25519, 32 байта в hex.
    pub public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    schema_version: u32,
    keys: Vec<TrustedKey>,
}

/// Идентификатор ключа: первые 16 hex SHA-256 открытого ключа.
pub fn key_id(public_key: &[u8]) -> String {
    sha256_hex(public_key)[..16].to_owned()
}

fn public_key_bytes(hex: &str) -> Option<Vec<u8>> {
    super::decode_hex(hex).filter(|b| b.len() == 32)
}

impl TrustedKey {
    pub fn from_public_key_hex(hex: &str, comment: Option<String>) -> Result<Self, AssetError> {
        let hex = hex.trim();
        let bytes = public_key_bytes(hex).ok_or_else(|| {
            release_error(
                "trusted_key_invalid",
                "Открытый ключ: 64 строчных hex-символа Ed25519",
            )
        })?;
        Ok(Self {
            key_id: key_id(&bytes),
            public_key: hex.to_owned(),
            comment,
        })
    }
    fn valid(&self) -> bool {
        public_key_bytes(&self.public_key).is_some_and(|b| key_id(&b) == self.key_id)
    }
}

/// Кому доверяет проверка релиза.
#[derive(Debug, Clone)]
pub enum TrustRoot {
    /// Ключи, закреплённые в исходниках этого CLI.
    Embedded(Vec<TrustedKey>),
    /// Явный ключ пользователя (`--trusted-public-key`): сборка собственного
    /// форка или изолированная проверка. Заменяет встроенные ключи.
    Explicit(TrustedKey),
}

impl TrustRoot {
    pub fn embedded() -> Self {
        let file: TrustFile = serde_json::from_str(TRUST_FILE).expect("release_keys.json");
        assert_eq!(file.schema_version, 1, "release_keys.json");
        assert!(
            file.keys.iter().all(TrustedKey::valid),
            "release_keys.json: key_id не совпадает с ключом"
        );
        Self::Embedded(file.keys)
    }
    pub fn keys(&self) -> &[TrustedKey] {
        match self {
            Self::Embedded(keys) => keys,
            Self::Explicit(key) => std::slice::from_ref(key),
        }
    }
    pub fn source(&self) -> &'static str {
        match self {
            Self::Embedded(_) => "embedded",
            Self::Explicit(_) => "explicit_key",
        }
    }
    pub fn describe(&self) -> serde_json::Value {
        json!({"source":self.source(),"key_ids":self.keys().iter().map(|k| &k.key_id).collect::<Vec<_>>()})
    }
}

/// Отказ из-за отсутствия корня доверия — внешний блокирующий шаг владельца,
/// а не неисправность пакета.
pub fn no_trusted_keys() -> AssetError {
    let mut e = release_error(
        "release_trust_root_missing",
        "В этом CLI ещё нет закреплённого ключа подписи релизов адаптера: сопровождающий должен создать ключ, добавить открытый ключ в release_keys.json и секрет подписи в GitHub (docs/ru/asset-store/forvo-extension-release.md)",
    );
    e.details["status"] = json!("BLOCKED");
    e
}

/// Подписывает точные байты метаданных ключом PKCS#8.
pub fn sign(metadata: &[u8], pkcs8: &[u8]) -> Result<SignatureFile, AssetError> {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| {
        release_error(
            "signing_key_invalid",
            "Ключ подписи не является PKCS#8 Ed25519",
        )
    })?;
    Ok(SignatureFile {
        schema_version: 1,
        algorithm: "ed25519".into(),
        key_id: key_id(pair.public_key().as_ref()),
        signature: encode_lower_hex(pair.sign(metadata).as_ref()),
    })
}

/// Открытый ключ ключа подписи (hex).
pub fn public_key_of(pkcs8: &[u8]) -> Result<String, AssetError> {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    Ed25519KeyPair::from_pkcs8(pkcs8)
        .map(|pair| encode_lower_hex(pair.public_key().as_ref()))
        .map_err(|_| {
            release_error(
                "signing_key_invalid",
                "Ключ подписи не является PKCS#8 Ed25519",
            )
        })
}

/// Новый ключ подписи PKCS#8 (для сопровождающего; в CI не создаётся).
pub fn generate_key() -> Result<Vec<u8>, AssetError> {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::Ed25519KeyPair;
    Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map(|doc| doc.as_ref().to_vec())
        .map_err(|_| release_error("keygen_failed", "Не удалось создать ключ"))
}

fn verify_signature(
    metadata: &[u8],
    signature: &[u8],
    trust: &TrustRoot,
) -> Result<String, AssetError> {
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    if trust.keys().is_empty() {
        return Err(no_trusted_keys());
    }
    let invalid = || {
        release_error(
            "release_signature_invalid",
            "Подпись метаданных релиза не прошла проверку закреплённым ключом; пакет не установлен",
        )
    };
    if signature.len() > MAX_SIGNATURE_BYTES {
        return Err(invalid());
    }
    let file: SignatureFile = serde_json::from_slice(signature).map_err(|_| invalid())?;
    if file.schema_version != 1 || file.algorithm != "ed25519" {
        return Err(invalid());
    }
    let key = trust
        .keys()
        .iter()
        .find(|k| k.key_id == file.key_id)
        .ok_or_else(|| {
            let mut e = release_error(
                "release_key_untrusted",
                "Релиз подписан ключом, которому этот CLI не доверяет; пакет не установлен",
            );
            e.details["key_id"] = json!(file.key_id);
            e.details["trusted_key_ids"] =
                json!(trust.keys().iter().map(|k| &k.key_id).collect::<Vec<_>>());
            e
        })?;
    let public = public_key_bytes(&key.public_key).ok_or_else(invalid)?;
    let raw = super::decode_hex(&file.signature)
        .filter(|s| s.len() == 64)
        .ok_or_else(invalid)?;
    UnparsedPublicKey::new(&ED25519, public)
        .verify(metadata, &raw)
        .map_err(|_| invalid())?;
    Ok(key.key_id.clone())
}

/// Проверенный выпуск: метаданные, файлы и ключ, которым он подписан.
#[derive(Debug, Clone)]
pub struct VerifiedRelease {
    pub metadata: ReleaseMetadata,
    pub files: Vec<PackageFile>,
    pub key_id: String,
}

impl VerifiedRelease {
    pub fn version(&self) -> Version {
        Version::parse(&self.metadata.version).expect("версия проверена")
    }
    pub fn summary(&self) -> serde_json::Value {
        json!({"version":self.metadata.version,"tag":self.metadata.tag,"repository":self.metadata.repository,
            "commit":self.metadata.commit,"key_id":self.key_id,"archive":self.metadata.archive,
            "files":self.metadata.files,"compatibility":self.metadata.compatibility})
    }
}

/// Полная проверка выпуска до любой записи: подпись, канал, источник, тег,
/// совместимость, SHA и размер архива, безопасное извлечение, точный состав и
/// SHA каждого файла, manifest и его версия. `expected_tag` — тег, который
/// запрашивал установщик (подмена версии другим подписанным выпуском — отказ).
pub fn verify(
    metadata_bytes: &[u8],
    signature: &[u8],
    zip: &[u8],
    trust: &TrustRoot,
    expected_tag: Option<&str>,
) -> Result<VerifiedRelease, AssetError> {
    let (metadata, key_id) = verify_metadata(metadata_bytes, signature, trust, expected_tag)?;
    verify_archive(metadata, key_id, zip)
}

/// Подпись и содержание метаданных без архива (проверка доступной версии).
pub fn verify_metadata(
    metadata_bytes: &[u8],
    signature: &[u8],
    trust: &TrustRoot,
    expected_tag: Option<&str>,
) -> Result<(ReleaseMetadata, String), AssetError> {
    if metadata_bytes.len() > MAX_METADATA_BYTES {
        return Err(release_error(
            "release_metadata_invalid",
            "Метаданные релиза слишком велики",
        ));
    }
    let key_id = verify_signature(metadata_bytes, signature, trust)?;
    let mismatch = |field: &str| {
        let mut e = release_error(
            "release_metadata_mismatch",
            "Подписанные метаданные не соответствуют каналу адаптера, источнику или архиву; пакет не установлен",
        );
        e.details["field"] = json!(field);
        e
    };
    let metadata: ReleaseMetadata =
        serde_json::from_slice(metadata_bytes).map_err(|_| mismatch("schema"))?;
    if metadata.schema_version != METADATA_SCHEMA {
        return Err(mismatch("schema_version"));
    }
    if metadata.channel != CHANNEL {
        return Err(mismatch("channel"));
    }
    if Version::parse(&metadata.version).is_none() || metadata.tag != tag(&metadata.version) {
        return Err(mismatch("tag"));
    }
    if expected_tag.is_some_and(|t| t != metadata.tag) {
        return Err(mismatch("requested_tag"));
    }
    // Источник фиксирован и для явного ключа: тот же канал, другая подпись.
    if metadata.repository != REPOSITORY {
        return Err(mismatch("repository"));
    }
    if !valid_commit(&metadata.commit) {
        return Err(mismatch("commit"));
    }
    if metadata.compatibility.protocol != PROTOCOL || metadata.compatibility.ws_path != WS_PATH {
        let mut e = release_error(
            "release_incompatible",
            "Пакет использует другой протокол bridge; обновите CLI",
        );
        e.details["protocol"] = json!(metadata.compatibility.protocol);
        e.details["supported_protocol"] = json!(PROTOCOL);
        return Err(e);
    }
    if metadata.archive.path != archive_name(&metadata.version) {
        return Err(mismatch("archive_name"));
    }
    Ok((metadata, key_id))
}

fn verify_archive(
    metadata: ReleaseMetadata,
    key_id: String,
    zip: &[u8],
) -> Result<VerifiedRelease, AssetError> {
    let mismatch = |field: &str| {
        let mut e = release_error(
            "release_metadata_mismatch",
            "Подписанные метаданные не соответствуют каналу адаптера, источнику или архиву; пакет не установлен",
        );
        e.details["field"] = json!(field);
        e
    };
    if zip.len() as u64 != metadata.archive.size || sha256_hex(zip) != metadata.archive.sha256 {
        return Err(release_error(
            "release_archive_mismatch",
            "Архив не совпадает с подписанным SHA-256; пакет не установлен",
        ));
    }
    let files = package::extract_zip(zip)?;
    if package::describe(&files) != metadata.files {
        return Err(mismatch("files"));
    }
    let manifest = files
        .iter()
        .find(|f| f.path == "manifest.json")
        .expect("обязательный файл");
    let facts = package::validate_manifest(&manifest.bytes)?;
    if facts.version != metadata.version {
        return Err(mismatch("manifest_version"));
    }
    if compatibility(&facts) != metadata.compatibility {
        return Err(mismatch("compatibility"));
    }
    Ok(VerifiedRelease {
        metadata,
        files,
        key_id,
    })
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
