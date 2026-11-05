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
//! - `sources` — исходники, изменение которых требует повышения версии;
//! - `keygen` — новый ключ подписи для сопровождающего (не для CI).
use asset_store::forvo::brave::package::{self, Version};
use asset_store::forvo::brave::release::{self, ReleaseMetadata, TrustRoot, TrustedKey};
use base64::Engine as _;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
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
    Sources,
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
    let metadata = read(&dir.join(release::METADATA_ASSET))?;
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
            TrustedKey::from_public_key_hex(&String::from_utf8_lossy(&read(path)?), None)
                .map_err(from_asset)?,
        ),
        None => TrustRoot::embedded(),
    };
    let metadata = read(&dir.join(release::METADATA_ASSET))?;
    let signature = read(&dir.join(release::SIGNATURE_ASSET))?;
    let claimed: ReleaseMetadata = serde_json::from_slice(&metadata)
        .map_err(|_| fail("release_metadata_mismatch", json!({})))?;
    let zip = read(&dir.join(release::archive_name(&claimed.version)))?;
    let verified = release::verify(&metadata, &signature, &zip, &trust, Some(&claimed.tag))
        .map_err(from_asset)?;
    Ok(json!({"verified":verified.summary(),"trust_root":trust.describe()}))
}

/// Плоский список выпусков из `gh api --paginate --slurp` (массив массивов) или массива.
fn flatten(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) if items.iter().all(Value::is_array) => items
            .into_iter()
            .flat_map(|page| page.as_array().cloned().unwrap_or_default())
            .collect(),
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// Решение публикации для встроенной версии по списку выпусков репозитория.
fn plan(releases: Value) -> Outcome {
    let version = package::bundled_version();
    let tag = release::tag(&version);
    let current = Version::parse(&version).expect("встроенная версия");
    let releases = flatten(releases);
    let same: Vec<&Value> = releases
        .iter()
        .filter(|r| r["tag_name"] == tag.as_str())
        .collect();
    let published = same.iter().filter(|r| r["draft"] != true).count();
    if published > 1 {
        return Err(fail("release_duplicated", json!({"tag":tag})));
    }
    if published == 1 {
        return Ok(
            json!({"action":"verify_existing","version":version,"tag":tag,
            "note":"выпуск этой версии уже опубликован: собранный ZIP обязан совпасть с ним побайтово"}),
        );
    }
    let highest = releases
        .iter()
        .filter(|r| r["draft"] != true)
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
        "highest_published":highest.map(|h| h.to_string()),
        "drafts_to_replace":same.iter().filter_map(|r| r["id"].as_u64()).collect::<Vec<_>>()}),
    )
}

fn compare(dir: &Path, published: &Path) -> Outcome {
    let ours: ReleaseMetadata = serde_json::from_slice(&read(&dir.join(release::METADATA_ASSET))?)
        .map_err(|_| fail("release_metadata_mismatch", json!({"side":"built"})))?;
    let theirs: ReleaseMetadata =
        serde_json::from_slice(&read(&published.join(release::METADATA_ASSET))?)
            .map_err(|_| fail("release_metadata_mismatch", json!({"side":"published"})))?;
    if ours.version != theirs.version
        || ours.archive != theirs.archive
        || ours.files != theirs.files
    {
        return Err(fail(
            "published_release_differs",
            json!({"version":ours.version,"built":ours.archive,"published":theirs.archive,
                "next_action":"Содержимое адаптера изменилось без повышения версии: повысьте version в manifest.json"}),
        ));
    }
    let zip = read(&published.join(&theirs.archive.path))?;
    if asset_store::hashing::sha256_hex(&zip) != ours.archive.sha256 {
        return Err(fail(
            "published_archive_mismatch",
            json!({"version":ours.version}),
        ));
    }
    Ok(
        json!({"identical":true,"version":ours.version,"archive":ours.archive,"published_commit":theirs.commit}),
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
        Command::Sources => Ok(json!({"paths":[
            "tools/asset-store/src/forvo/brave/extension",
            "tools/asset-store/src/forvo/dom.js",
            "tools/asset-store/src/forvo/auth.js",
        ]})),
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

    fn rel(tag: &str, draft: bool) -> Value {
        json!({"id":7,"tag_name":tag,"draft":draft,"prerelease":false})
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
            rel("v99.0.0", false),
            rel("forvo-extension-v99.0", true)
        ]]))
        .unwrap();
        assert_eq!(other["action"], "publish");
        let existing = plan(json!([rel(&tag, false)])).unwrap();
        assert_eq!(existing["action"], "verify_existing");
        let higher = plan(json!([rel("forvo-extension-v99.0", false)])).unwrap_err();
        assert_eq!(higher.0, "version_not_increased");
        let draft = plan(json!([rel(&tag, true)])).unwrap();
        assert_eq!(draft["action"], "publish");
        assert_eq!(draft["drafts_to_replace"], json!([7]));
        assert_eq!(
            plan(json!([rel(&tag, false), rel(&tag, false)]))
                .unwrap_err()
                .0,
            "release_duplicated"
        );
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
