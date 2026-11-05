//! Релизный конвейер MV3-адаптера Forvo: тот же пакет, что разворачивает
//! `setup`, в виде детерминированного ZIP, подписанные метаданные и решения
//! workflow публикации. Сеть не используется: список выпусков и опубликованные
//! файлы передаёт workflow.
//!
//! Команды:
//! - `build` — ZIP и неподписанные метаданные встроенной версии;
//! - `sign` — подпись метаданных ключом PKCS#8 из переменной среды;
//! - `verify` — полная проверка каталога выпуска, как у установщика;
//! - `plan` — что делать с версией: опубликовать, сверить существующий выпуск
//!   или отказать (версия не повышена);
//! - `compare` — совпадает ли собранный пакет с уже опубликованным той же версии;
//! - `check-bump` — повышена ли версия относительно базовой;
//! - `keygen` — новый ключ подписи для сопровождающего (не для CI).
use asset_store::forvo::brave::package::{self, Version};
use asset_store::forvo::brave::release::{self, ReleaseMetadata, TrustRoot, TrustedKey};
use base64::Engine as _;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forvo-extension-release",
    about = "Сборка, подпись и проверка выпусков адаптера Forvo"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        commit: String,
        /// Файл описания выпуска (Markdown) для `gh release create --notes-file`.
        #[arg(long)]
        notes_out: Option<PathBuf>,
    },
    Sign {
        #[arg(long)]
        dir: PathBuf,
        /// Переменная среды с PKCS#8 Ed25519 в base64.
        #[arg(long, default_value = "FORVO_EXTENSION_SIGNING_KEY")]
        key_env: String,
        /// Разрешить ключ вне release_keys.json (только локальные проверки).
        #[arg(long)]
        allow_untrusted_key: bool,
    },
    Verify {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        trusted_public_key: Option<PathBuf>,
    },
    Plan {
        /// JSON списка выпусков (`gh api --paginate --slurp repos/<r>/releases`).
        #[arg(long)]
        releases: PathBuf,
    },
    Compare {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        published: PathBuf,
    },
    CheckBump {
        #[arg(long)]
        base_version: String,
    },
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
}

type Outcome = Result<Value, (String, Value)>;

fn fail(code: &str, details: Value) -> (String, Value) {
    (code.to_owned(), details)
}

fn from_asset(e: asset_store::AssetError) -> (String, Value) {
    (
        asset_store::forvo::provider_code(&e).to_owned(),
        json!({"message":e.message,"details":e.details}),
    )
}

fn read(path: &Path) -> Result<Vec<u8>, (String, Value)> {
    std::fs::read(path).map_err(|e| fail("read_failed", json!({"path":path,"error":e.to_string()})))
}

/// Ограниченное чтение обычного файла без перехода по символической ссылке.
/// NOFOLLOW запрещает открыть ссылку, NONBLOCK не даёт FIFO зависнуть до
/// проверки типа файла.
fn read_regular_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, (String, Value)> {
    let failed = |reason: &str| {
        fail(
            "release_artifact_invalid",
            json!({"path":path,"reason":reason}),
        )
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32
                | rustix::fs::OFlags::NONBLOCK.bits() as i32
                | rustix::fs::OFlags::CLOEXEC.bits() as i32,
        )
        .open(path)
        .map_err(|e| {
            fail(
                "release_artifact_invalid",
                json!({"path":path,"reason":"open_failed","error":e.to_string()}),
            )
        })?;
    let metadata = file.metadata().map_err(|e| {
        fail(
            "release_artifact_invalid",
            json!({"path":path,"reason":"metadata_failed","error":e.to_string()}),
        )
    })?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(failed("not_regular_or_too_large"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| {
            fail(
                "release_artifact_invalid",
                json!({"path":path,"reason":"read_failed","error":e.to_string()}),
            )
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(failed("too_large"));
    }
    Ok(bytes)
}

struct ArtifactDir {
    path: PathBuf,
    root: File,
}

impl ArtifactDir {
    fn open(path: &Path) -> Result<Self, (String, Value)> {
        let root = rustix::fs::openat(
            rustix::fs::CWD,
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":path,"reason":"directory_unavailable","error":error.to_string()}),
            )
        })?;
        Ok(Self {
            path: path.to_owned(),
            root,
        })
    }

    fn check_path_binding(&self) -> Result<(), (String, Value)> {
        let opened = self.root.metadata().map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path,"reason":"directory_metadata_failed","error":error.to_string()}),
            )
        })?;
        let named = std::fs::symlink_metadata(&self.path).map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path,"reason":"directory_unavailable","error":error.to_string()}),
            )
        })?;
        if !named.is_dir() || named.dev() != opened.dev() || named.ino() != opened.ino() {
            return Err(fail(
                "release_artifact_invalid",
                json!({"path":self.path,"reason":"directory_changed_or_symlink"}),
            ));
        }
        Ok(())
    }

    fn read_regular_bounded(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>, (String, Value)> {
        let failed = |reason: &str| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path.join(name),"reason":reason}),
            )
        };
        self.check_path_binding()?;
        let file = rustix::fs::openat(
            &self.root,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path.join(name),"reason":"open_failed","error":error.to_string()}),
            )
        })?;
        let metadata = file.metadata().map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path.join(name),"reason":"metadata_failed","error":error.to_string()}),
            )
        })?;
        if !metadata.is_file() || metadata.len() > max_bytes {
            return Err(failed("not_regular_or_too_large"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| {
                fail(
                    "release_artifact_invalid",
                    json!({"path":self.path.join(name),"reason":"read_failed","error":error.to_string()}),
                )
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(failed("too_large"));
        }
        self.check_path_binding()?;
        Ok(bytes)
    }

    fn require_exact_assets(&self, expected: &[String]) -> Result<(), (String, Value)> {
        self.check_path_binding()?;
        let mut actual = Vec::new();
        for entry in std::fs::read_dir(&self.path).map_err(|error| {
            fail(
                "release_artifact_invalid",
                json!({"path":self.path,"reason":"directory_read_failed","error":error.to_string()}),
            )
        })? {
            let entry = entry.map_err(|error| {
                fail(
                    "release_artifact_invalid",
                    json!({"path":self.path,"reason":"directory_entry_failed","error":error.to_string()}),
                )
            })?;
            if !entry
                .file_type()
                .map_err(|error| {
                    fail(
                        "release_artifact_invalid",
                        json!({"path":entry.path(),"reason":"entry_type_failed","error":error.to_string()}),
                    )
                })?
                .is_file()
            {
                return Err(fail(
                    "release_artifact_invalid",
                    json!({"path":entry.path(),"reason":"non_regular_entry"}),
                ));
            }
            actual.push(entry.file_name().to_string_lossy().into_owned());
        }
        actual.sort();
        let mut expected = expected.to_vec();
        expected.sort();
        if actual != expected {
            return Err(fail(
                "release_artifact_invalid",
                json!({"path":self.path,"reason":"unexpected_assets","expected":expected,"actual":actual}),
            ));
        }
        self.check_path_binding()
    }
}

fn read_metadata(dir: &ArtifactDir) -> Result<(Vec<u8>, ReleaseMetadata), (String, Value)> {
    let metadata =
        dir.read_regular_bounded(release::METADATA_ASSET, release::MAX_METADATA_BYTES as u64)?;
    let parsed = release::validate_metadata_bytes(&metadata, None).map_err(from_asset)?;
    Ok((metadata, parsed))
}

fn read_archive(dir: &ArtifactDir, metadata: &ReleaseMetadata) -> Result<Vec<u8>, (String, Value)> {
    // Имя берётся только из уже проверенной числовой версии.
    dir.read_regular_bounded(
        &release::archive_name(&metadata.version),
        package::MAX_ARCHIVE_BYTES,
    )
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), (String, Value)> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .map_err(|e| fail("write_failed", json!({"path":path,"error":e.to_string()})))
}

fn build(out: &Path, repository: &str, commit: &str, notes: Option<&Path>) -> Outcome {
    let files = package::bundled_files();
    let zip = package::build_zip(&files).map_err(from_asset)?;
    let metadata = ReleaseMetadata::new(&files, &zip, repository, commit).map_err(from_asset)?;
    std::fs::create_dir(out).map_err(|e| {
        fail(
            "out_dir_unavailable",
            json!({"path":out,"error":e.to_string()}),
        )
    })?;
    write_new(&out.join(&metadata.archive.path), &zip, 0o644)?;
    write_new(
        &out.join(release::METADATA_ASSET),
        &metadata.to_bytes(),
        0o644,
    )?;
    if let Some(notes) = notes {
        let text = format!(
            "Адаптер Forvo (MV3) версии {version}.\n\n\
             - канал: `{channel}`, тег `{tag}`;\n\
             - протокол bridge: `{protocol}`;\n\
             - коммит: `{commit}`;\n\
             - ZIP `{archive}`: SHA-256 `{sha}`, {size} байт;\n\
             - подлинность: подпись Ed25519 метаданных `{meta}` (`{sig}`), ключ закреплён в CLI.\n\n\
             Установка обновления: `pronunciation-assets forvo browser update`. Первичная \
             установка распакованного расширения — вручную, см. docs/ru/asset-store/forvo-brave.md.\n",
            version = metadata.version,
            channel = metadata.channel,
            tag = metadata.tag,
            protocol = metadata.compatibility.protocol,
            commit = metadata.commit,
            archive = metadata.archive.path,
            sha = metadata.archive.sha256,
            size = metadata.archive.size,
            meta = release::METADATA_ASSET,
            sig = release::SIGNATURE_ASSET,
        );
        write_new(notes, text.as_bytes(), 0o644)?;
    }
    Ok(
        json!({"version":metadata.version,"tag":metadata.tag,"archive":metadata.archive,"files":metadata.files,
        "pairing_included":files.iter().any(|f| f.path == package::PAIRING_FILE)}),
    )
}

fn sign(dir: &Path, key_env: &str, allow_untrusted: bool) -> Outcome {
    let encoded = std::env::var(key_env).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| {
        fail("signing_key_missing", json!({"status":"BLOCKED","env":key_env,
            "next_action":"Сопровождающий: создайте ключ (keygen), добавьте открытый ключ в release_keys.json и секрет FORVO_EXTENSION_SIGNING_KEY в окружение forvo-extension-release репозитория"}))
    })?;
    let pkcs8 = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| fail("signing_key_invalid", json!({"env":key_env})))?;
    let public = release::public_key_of(&pkcs8).map_err(from_asset)?;
    let key = TrustedKey::from_public_key_hex(&public, None).map_err(from_asset)?;
    let trusted = TrustRoot::embedded()
        .keys()
        .iter()
        .any(|k| k.key_id == key.key_id && k.public_key == key.public_key);
    if !trusted && !allow_untrusted {
        return Err(fail(
            "signing_key_untrusted",
            json!({"status":"BLOCKED","key_id":key.key_id,
                "next_action":"Открытый ключ секрета не закреплён в release_keys.json этого коммита: такой выпуск CLI отвергнет"}),
        ));
    }
    let metadata = read_regular_bounded(
        &dir.join(release::METADATA_ASSET),
        release::MAX_METADATA_BYTES as u64,
    )?;
    release::validate_metadata_bytes(&metadata, None).map_err(from_asset)?;
    let signature = release::sign(&metadata, &pkcs8).map_err(from_asset)?;
    write_new(
        &dir.join(release::SIGNATURE_ASSET),
        &signature.to_bytes(),
        0o644,
    )?;
    Ok(json!({"key_id":signature.key_id,"trusted":trusted}))
}

fn verify(dir: &Path, key: Option<&Path>) -> Outcome {
    let trust = match key {
        Some(path) => TrustRoot::Explicit(
            TrustedKey::from_public_key_hex(
                &String::from_utf8_lossy(&read_regular_bounded(path, 4 * 1024)?),
                None,
            )
            .map_err(from_asset)?,
        ),
        None => TrustRoot::embedded(),
    };
    let artifacts = ArtifactDir::open(dir)?;
    let (metadata, claimed) = read_metadata(&artifacts)?;
    artifacts.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::SIGNATURE_ASSET.to_owned(),
        release::archive_name(&claimed.version),
    ])?;
    let signature = artifacts.read_regular_bounded(
        release::SIGNATURE_ASSET,
        release::MAX_SIGNATURE_BYTES as u64,
    )?;
    let zip = read_archive(&artifacts, &claimed)?;
    let verified = release::verify(&metadata, &signature, &zip, &trust, Some(&claimed.tag))
        .map_err(from_asset)?;
    artifacts.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::SIGNATURE_ASSET.to_owned(),
        release::archive_name(&claimed.version),
    ])?;
    Ok(json!({"verified":verified.summary(),"trust_root":trust.describe()}))
}

/// Плоский список выпусков из `gh api --paginate --slurp` (массив массивов) или массива.
fn flatten(value: Value) -> Result<Vec<Value>, (String, Value)> {
    match value {
        Value::Array(items) if items.iter().all(Value::is_array) => {
            let flattened = items
                .into_iter()
                .flat_map(|page| page.as_array().cloned().unwrap_or_default())
                .collect::<Vec<_>>();
            if flattened.iter().all(Value::is_object) {
                Ok(flattened)
            } else {
                Err(fail(
                    "releases_invalid",
                    json!({"reason":"page_item_not_object"}),
                ))
            }
        }
        Value::Array(items) if items.iter().all(Value::is_object) => Ok(items),
        Value::Array(items) if items.is_empty() => Ok(items),
        _ => Err(fail(
            "releases_invalid",
            json!({"reason":"expected_release_array"}),
        )),
    }
}

/// Решение публикации для встроенной версии по списку выпусков репозитория.
fn plan(releases: Value) -> Outcome {
    let version = package::bundled_version();
    let tag = release::tag(&version);
    let current = Version::parse(&version).expect("встроенная версия");
    let releases = flatten(releases)?;
    for release in &releases {
        if release["tag_name"].as_str().is_none()
            || release["draft"].as_bool().is_none()
            || release["prerelease"].as_bool().is_none()
        {
            return Err(fail(
                "releases_invalid",
                json!({"reason":"release_missing_tag_or_channel_flags"}),
            ));
        }
    }
    let same: Vec<&Value> = releases
        .iter()
        .filter(|r| r["tag_name"] == tag.as_str())
        .collect();
    if same.len() > 1 {
        return Err(fail("release_duplicated", json!({"tag":tag})));
    }
    if let Some(existing) = same.first() {
        let draft = existing["draft"].as_bool().ok_or_else(|| {
            fail(
                "releases_invalid",
                json!({"tag":tag,"reason":"draft_not_boolean"}),
            )
        })?;
        let prerelease = existing["prerelease"].as_bool().ok_or_else(|| {
            fail(
                "releases_invalid",
                json!({"tag":tag,"reason":"prerelease_not_boolean"}),
            )
        })?;
        if draft || prerelease {
            return Err(fail(
                "release_tag_occupied",
                json!({"status":"BLOCKED","tag":tag,"draft":draft,"prerelease":prerelease,
                    "next_action":"Проверьте существующий draft/prerelease и освободите тег вручную; автоматическое удаление или перезапись выпуска запрещены"}),
            ));
        }
        return Ok(
            json!({"action":"verify_existing","version":version,"tag":tag,
            "note":"стабильный выпуск этой версии уже опубликован: подпись и полный пакет должны пройти проверку доверия и совпасть со сборкой"}),
        );
    }
    let highest = releases
        .iter()
        .filter(|r| r["draft"] == false && r["prerelease"] == false)
        .filter_map(|r| r["tag_name"].as_str())
        .filter_map(|t| t.strip_prefix(release::TAG_PREFIX))
        .filter_map(|v| {
            Version::parse(v).filter(|p| release::tag(&p.to_string()) == release::tag(v))
        })
        .max();
    if let Some(highest) = &highest
        && current <= *highest
    {
        return Err(fail(
            "version_not_increased",
            json!({"version":version,"highest_published":highest.to_string(),
                "next_action":"Повысьте version в manifest.json адаптера выше опубликованной"}),
        ));
    }
    Ok(
        json!({"action":"publish","version":version,"tag":tag,"bootstrap":highest.is_none(),
        "highest_published":highest.map(|h| h.to_string())}),
    )
}

fn compare(dir: &Path, published: &Path) -> Outcome {
    compare_with_trust(dir, published, &TrustRoot::embedded())
}

fn compare_with_trust(dir: &Path, published: &Path, trust: &TrustRoot) -> Outcome {
    let built = ArtifactDir::open(dir)?;
    let (our_bytes, ours) = read_metadata(&built)?;
    built.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::archive_name(&ours.version),
    ])?;
    let our_zip = read_archive(&built, &ours)?;
    let (ours, _) = release::validate_built_release(&our_bytes, &our_zip, Some(&ours.tag))
        .map_err(from_asset)?;
    built.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::archive_name(&ours.version),
    ])?;

    let published_assets = ArtifactDir::open(published)?;
    let (their_bytes, claimed) = read_metadata(&published_assets)?;
    published_assets.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::SIGNATURE_ASSET.to_owned(),
        release::archive_name(&claimed.version),
    ])?;
    let signature = published_assets.read_regular_bounded(
        release::SIGNATURE_ASSET,
        release::MAX_SIGNATURE_BYTES as u64,
    )?;
    let their_zip = read_archive(&published_assets, &claimed)?;
    let verified = release::verify(&their_bytes, &signature, &their_zip, trust, Some(&ours.tag))
        .map_err(from_asset)?;
    published_assets.require_exact_assets(&[
        release::METADATA_ASSET.to_owned(),
        release::SIGNATURE_ASSET.to_owned(),
        release::archive_name(&verified.metadata.version),
    ])?;

    let theirs = verified.metadata;
    if ours.schema_version != theirs.schema_version
        || ours.channel != theirs.channel
        || ours.version != theirs.version
        || ours.tag != theirs.tag
        || ours.repository != theirs.repository
        || ours.compatibility != theirs.compatibility
        || ours.archive != theirs.archive
        || ours.files != theirs.files
    {
        return Err(fail(
            "published_release_differs",
            json!({"version":ours.version,"built":ours.archive,"published":theirs.archive,
                "next_action":"Содержимое адаптера изменилось без повышения версии: повысьте version в manifest.json"}),
        ));
    }
    Ok(
        json!({"identical":true,"version":ours.version,"archive":ours.archive,
        "published_commit":theirs.commit,"key_id":verified.key_id,
        "trust_root":trust.describe()}),
    )
}

fn check_bump(base: &str) -> Outcome {
    let head = package::bundled_version();
    let parsed_base = Version::parse(base)
        .ok_or_else(|| fail("invalid_version", json!({"base_version":base})))?;
    if Version::parse(&head).expect("встроенная версия") > parsed_base {
        Ok(json!({"bumped":true,"base_version":base,"version":head,"tag":release::tag(&head)}))
    } else {
        Err(fail(
            "version_not_increased",
            json!({"base_version":base,"version":head,
                "next_action":"Исходники адаптера изменены: повысьте version в tools/asset-store/src/forvo/brave/extension/manifest.json"}),
        ))
    }
}

fn keygen(out: &Path) -> Outcome {
    let pkcs8 = release::generate_key().map_err(from_asset)?;
    let public = release::public_key_of(&pkcs8).map_err(from_asset)?;
    let key = TrustedKey::from_public_key_hex(&public, None).map_err(from_asset)?;
    write_new(
        out,
        base64::engine::general_purpose::STANDARD
            .encode(&pkcs8)
            .as_bytes(),
        0o600,
    )?;
    Ok(
        json!({"private_key_file":out,"key_id":key.key_id,"public_key":key.public_key,
        "release_keys_entry":{"key_id":key.key_id,"public_key":key.public_key,"comment":"<кто и когда создал>"},
        "next_action":"Содержимое файла — секрет FORVO_EXTENSION_SIGNING_KEY; запись release_keys_entry — в release_keys.json через PR. Файл ключа храните офлайн и не коммитьте"}),
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Build {
            out,
            repository,
            commit,
            notes_out,
        } => build(out, repository, commit, notes_out.as_deref()),
        Command::Sign {
            dir,
            key_env,
            allow_untrusted_key,
        } => sign(dir, key_env, *allow_untrusted_key),
        Command::Verify {
            dir,
            trusted_public_key,
        } => verify(dir, trusted_public_key.as_deref()),
        Command::Plan { releases } => read(releases).and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|_| fail("releases_invalid", json!({"path":releases})))
                .and_then(plan)
        }),
        Command::Compare { dir, published } => compare(dir, published),
        Command::CheckBump { base_version } => check_bump(base_version),
        Command::Keygen { out } => keygen(out),
    };
    match result {
        Ok(value) => {
            println!("{}", json!({"outcome":"ok","result":value}));
            ExitCode::SUCCESS
        }
        Err((code, details)) => {
            println!(
                "{}",
                json!({"outcome":"error","code":code,"details":details})
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn rel(tag: &str, draft: bool, prerelease: bool) -> Value {
        json!({"id":7,"tag_name":tag,"draft":draft,"prerelease":prerelease})
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "forvo-extension-release-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("pub"));
        }
    }

    fn write_signed_fixture(dir: &Path) -> (Vec<u8>, Vec<u8>, PathBuf) {
        let files = package::bundled_files();
        let zip = package::build_zip(&files).unwrap();
        let metadata = ReleaseMetadata::new(
            &files,
            &zip,
            release::REPOSITORY,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap()
        .to_bytes();
        let pkcs8 = release::generate_key().unwrap();
        let signature = release::sign(&metadata, &pkcs8).unwrap().to_bytes();
        let trusted_public_key = release::public_key_of(&pkcs8).unwrap();
        let parsed: ReleaseMetadata = serde_json::from_slice(&metadata).unwrap();
        std::fs::write(dir.join(release::METADATA_ASSET), &metadata).unwrap();
        std::fs::write(dir.join(release::SIGNATURE_ASSET), &signature).unwrap();
        std::fs::write(dir.join(parsed.archive.path), &zip).unwrap();
        let key_path = dir.with_extension("pub");
        std::fs::write(&key_path, trusted_public_key).unwrap();
        (metadata, zip, key_path)
    }

    #[test]
    fn plan_bootstraps_first_release_and_refuses_unbumped_versions() {
        let version = package::bundled_version();
        let tag = release::tag(&version);
        let first = plan(json!([])).unwrap();
        assert_eq!(first["action"], "publish");
        assert_eq!(first["bootstrap"], true);
        // Другие компоненты репозитория и черновики не считаются выпусками адаптера.
        let other = plan(json!([[
            rel("v99.0.0", false, false),
            rel("forvo-extension-v99.0", true, false),
            rel("forvo-extension-v100.0", false, true)
        ]]))
        .unwrap();
        assert_eq!(other["action"], "publish");
        assert_eq!(other["bootstrap"], true);
        let existing = plan(json!([rel(&tag, false, false)])).unwrap();
        assert_eq!(existing["action"], "verify_existing");
        let higher = plan(json!([rel("forvo-extension-v99.0", false, false)])).unwrap_err();
        assert_eq!(higher.0, "version_not_increased");
        let draft = plan(json!([rel(&tag, true, false)])).unwrap_err();
        assert_eq!(draft.0, "release_tag_occupied");
        assert_eq!(draft.1["status"], "BLOCKED");
        let prerelease = plan(json!([rel(&tag, false, true)])).unwrap_err();
        assert_eq!(prerelease.0, "release_tag_occupied");
        assert_eq!(
            plan(json!([rel(&tag, false, false), rel(&tag, true, false)]))
                .unwrap_err()
                .0,
            "release_duplicated"
        );
        assert_eq!(
            plan(json!([{"tag_name":tag,"draft":false}])).unwrap_err().0,
            "releases_invalid"
        );
    }

    #[test]
    fn verify_accepts_explicit_test_key_and_rejects_hostile_metadata_before_archive_read() {
        let scratch = Scratch::new("verify");
        let (metadata, _, key_path) = write_signed_fixture(scratch.path());
        assert!(verify(scratch.path(), Some(&key_path)).is_ok());

        let mut hostile: Value = serde_json::from_slice(&metadata).unwrap();
        hostile["version"] = json!("../../outside");
        hostile["tag"] = json!("forvo-extension-v../../outside");
        hostile["archive"]["path"] = json!("../../outside.zip");
        std::fs::write(
            scratch.path().join(release::METADATA_ASSET),
            serde_json::to_vec(&hostile).unwrap(),
        )
        .unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_metadata_mismatch");

        let mut hostile_path: Value = serde_json::from_slice(&metadata).unwrap();
        hostile_path["archive"]["path"] = json!("../../outside.zip");
        std::fs::write(
            scratch.path().join(release::METADATA_ASSET),
            serde_json::to_vec(&hostile_path).unwrap(),
        )
        .unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_metadata_mismatch");
    }

    #[test]
    fn verify_rejects_oversized_assets_symlinks_and_unexpected_files() {
        let scratch = Scratch::new("asset-bounds");
        let (metadata, zip, key_path) = write_signed_fixture(scratch.path());

        let too_large = vec![0; release::MAX_METADATA_BYTES + 1];
        std::fs::write(scratch.path().join(release::METADATA_ASSET), too_large).unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");

        std::fs::write(scratch.path().join(release::METADATA_ASSET), &metadata).unwrap();
        let parsed: ReleaseMetadata = serde_json::from_slice(&metadata).unwrap();
        let archive = scratch.path().join(parsed.archive.path);
        std::fs::remove_file(&archive).unwrap();
        let external = scratch.path().join("outside.zip");
        std::fs::write(&external, b"outside").unwrap();
        std::os::unix::fs::symlink(&external, &archive).unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");

        std::fs::remove_file(&external).unwrap();
        std::fs::remove_file(&archive).unwrap();
        std::fs::write(&archive, vec![0; package::MAX_ARCHIVE_BYTES as usize + 1]).unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");

        std::fs::write(&archive, &zip).unwrap();
        std::fs::write(scratch.path().join("extra.txt"), b"extra").unwrap();
        let error = verify(scratch.path(), Some(&key_path)).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");
    }

    #[test]
    fn compare_rejects_untrusted_or_unsigned_published_artifacts() {
        let local = Scratch::new("compare-local");
        let published = Scratch::new("compare-published");
        let local_dist = local.path().join("dist");
        build(
            &local_dist,
            release::REPOSITORY,
            "0123456789abcdef0123456789abcdef01234567",
            None,
        )
        .unwrap();
        write_signed_fixture(published.path());
        std::fs::write(published.path().join("extra.txt"), b"extra").unwrap();
        let error = compare(&local_dist, published.path()).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");
        std::fs::remove_file(published.path().join("extra.txt")).unwrap();
        let error = compare(&local_dist, published.path()).unwrap_err();
        assert!(
            matches!(
                error.0.as_str(),
                "release_trust_root_missing" | "release_key_untrusted"
            ),
            "{}",
            error.0
        );

        let unsigned = Scratch::new("compare-unsigned");
        let unsigned_dist = unsigned.path().join("dist");
        build(
            &unsigned_dist,
            release::REPOSITORY,
            "0123456789abcdef0123456789abcdef01234567",
            None,
        )
        .unwrap();
        let error = compare(&local_dist, &unsigned_dist).unwrap_err();
        assert_eq!(error.0, "release_artifact_invalid");
    }

    #[test]
    fn compare_accepts_a_signed_identical_fixture_and_rejects_a_substituted_signature() {
        let local = Scratch::new("compare-trusted-local");
        let published = Scratch::new("compare-trusted-published");
        let local_dist = local.path().join("dist");
        build(
            &local_dist,
            release::REPOSITORY,
            "0123456789abcdef0123456789abcdef01234567",
            None,
        )
        .unwrap();
        let (_, _, public_path) = write_signed_fixture(published.path());
        let public_hex = std::fs::read_to_string(public_path).unwrap();
        let trust = TrustRoot::Explicit(
            TrustedKey::from_public_key_hex(&public_hex, Some("isolated test fixture".into()))
                .unwrap(),
        );

        let identical = compare_with_trust(&local_dist, published.path(), &trust).unwrap();
        assert_eq!(identical["identical"], true);
        assert_eq!(identical["trust_root"]["source"], "explicit_key");

        std::fs::write(
            published.path().join(release::SIGNATURE_ASSET),
            b"substituted signature",
        )
        .unwrap();
        let error = compare_with_trust(&local_dist, published.path(), &trust).unwrap_err();
        assert_eq!(error.0, "release_signature_invalid");
    }

    #[test]
    fn check_bump_requires_strictly_greater_version() {
        assert!(check_bump("0.0.1").is_ok());
        assert_eq!(
            check_bump(&package::bundled_version()).unwrap_err().0,
            "version_not_increased"
        );
        assert_eq!(check_bump("x").unwrap_err().0, "invalid_version");
    }
}
