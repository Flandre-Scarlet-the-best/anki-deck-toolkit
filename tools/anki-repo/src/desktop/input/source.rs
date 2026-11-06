//! Связь байтов spool с подтверждённым заданием настоящего Desktop bridge.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{Options, Result, environment::Target, error, io_error, jobs, patch, transport};
use crate::exchange::{ProfileId, Snapshot, filesystem::Directory};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingJob {
    pub id: String,
    pub profile_id: String,
    pub profile: String,
    pub deck_id: String,
    pub deck: String,
    pub deck_uuid: Option<String>,
    pub process: String,
}

impl PendingJob {
    pub fn key(&self) -> String {
        asset_store::hashing::sha256_hex(
            serde_json::to_vec(&json!([self.profile_id, self.deck_id]))
                .expect("сериализуемые идентификаторы job"),
        )
    }
}

/// Поля закрыты: production-путь создаёт его только по живому toolkitJob.
pub(super) struct VerifiedExport {
    snapshot: Snapshot,
    files: BTreeMap<String, Vec<u8>>,
    inventory: BTreeMap<String, String>,
    evidence: String,
}

impl VerifiedExport {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }
    pub fn inventory(&self) -> &BTreeMap<String, String> {
        &self.inventory
    }
    pub fn evidence(&self) -> &str {
        &self.evidence
    }
}

pub(super) fn prepare(
    t: &Target,
    o: &Options,
    catalog: &Value,
    deck: &Value,
) -> Result<PendingJob> {
    let job = PendingJob {
        id: patch::random_id()?,
        profile_id: required(catalog, "profile_id")?,
        profile: o
            .profile
            .clone()
            .ok_or_else(|| error("profile_required", "Укажите --profile"))?,
        deck_id: required(deck, "id")?,
        deck: required(deck, "name")?,
        deck_uuid: deck["crowdanki_uuid"].as_str().map(str::to_owned),
        process: required(catalog, "process")?,
    };
    let path = t.state().join("spool").join(&job.id);
    crate::layout::guard_local_path(&path, "Задание Desktop")?;
    let data = Directory::open(&t.data).map_err(io_error)?;
    let state = data.ensure_child("toolkit-desktop").map_err(io_error)?.0;
    state.require_private().map_err(io_error)?;
    let spool = state.ensure_child("spool").map_err(io_error)?.0;
    let (directory, created) = spool.ensure_child(&job.id).map_err(io_error)?;
    if !created {
        return Err(error(
            "export_job_id_collision",
            "Каталог нового job уже существует; ticket не записан",
        ));
    }
    let ticket = json!({"approved":true,"action":"export","profile":job.profile,
        "profile_id":job.profile_id,"deck":job.deck,"deck_id":job.deck_id,
        "deck_uuid":job.deck_uuid,"process":job.process,"expires":expiry()?});
    directory
        .publish(
            "request.json",
            &serde_json::to_vec(&ticket).map_err(io_error)?,
        )
        .map_err(io_error)?;
    Ok(job)
}

pub(super) fn poll(t: &Target, o: &Options, job: &PendingJob) -> Result<Value> {
    let result = jobs::status(t, o, &job.id)?;
    if result["live_verified"] == false {
        return Err(error(
            "export_not_live_verified",
            "Сохранённый result.json без живого API не подтверждает публикацию",
        ));
    }
    Ok(result)
}

pub(super) fn submit(t: &Target, o: &Options, job: &PendingJob) -> Result<Value> {
    // Вызывается только после живого prepared. Новое явное --apply может
    // продлить ещё не использованный ticket, сохраняя тот же ID и источник.
    let directory = Directory::open(&t.state().join("spool").join(&job.id)).map_err(io_error)?;
    let mut ticket: Value =
        serde_json::from_slice(&directory.read("request.json").map_err(io_error)?)
            .map_err(io_error)?;
    if ticket["profile_id"] != job.profile_id
        || ticket["deck_id"] != job.deck_id
        || ticket["profile"] != job.profile
        || ticket["deck"] != job.deck
        || ticket["process"] != job.process
        || ticket["deck_uuid"] != json!(job.deck_uuid)
        || ticket["action"] != "export"
        || ticket["approved"] != true
    {
        return Err(error(
            "export_ticket_changed",
            "Источник сохранённого ticket изменился",
        ));
    }
    ticket["expires"] = json!(expiry()?);
    directory
        .publish(
            "request.json",
            &serde_json::to_vec(&ticket).map_err(io_error)?,
        )
        .map_err(io_error)?;
    transport::patched(t, o, "toolkitExport", json!({"id":job.id}))
}

fn expiry() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io_error)?
        .as_secs()
        + 600)
}

pub(super) fn verify(t: &Target, job: &PendingJob, result: &Value) -> Result<VerifiedExport> {
    let value = &result["job"];
    if result["state"] != "succeeded"
        || value["state"] != "succeeded"
        || value["action"] != "export"
        || value["id"] != job.id
        || value["profile"] != job.profile
        || value["profile_id"] != job.profile_id
        || value["deck_id"] != job.deck_id
        || value["deck_name"] != job.deck
        || value["process"] != job.process
        || job
            .deck_uuid
            .as_ref()
            .is_some_and(|id| value["deck_uuid"] != *id)
    {
        return Err(error(
            "export_evidence_mismatch",
            "Живое задание не соответствует выбранному источнику",
        ));
    }
    let path = t.state().join("spool").join(&job.id).join("export");
    crate::layout::guard_local_path(&path, "Проверенный spool")?;
    // Native Windows export_dir не используется как Linux-путь. Только локальный
    // путь выбранной установки, уже отображённый environment::resolve.
    if result["local_export_dir"] != json!(path) {
        return Err(error(
            "export_path_mismatch",
            "Не совпадает локальный путь spool",
        ));
    }
    let expected: BTreeMap<String, String> = serde_json::from_value(value["inventory"].clone())
        .map_err(|_| {
            error(
                "export_evidence_missing",
                "Нет полного свидетельства байтов живого экспорта",
            )
        })?;
    let directory = Directory::open(&path).map_err(io_error)?;
    let (snapshot, files, inventory) = read_export(
        &directory,
        ProfileId::new(&job.profile_id).map_err(io_error)?,
    )
    .map_err(|_| {
        error(
            "export_validation_failed",
            "Экспорт не прошёл структурную проверку или проверку физических файлов",
        )
    })?;
    if inventory != expected
        || snapshot.identity().crowdanki_uuid != value["deck_uuid"]
        || snapshot.name() != job.deck
    {
        return Err(error(
            "export_bytes_mismatch",
            "Байты или идентичность spool не совпадают с живым результатом",
        ));
    }
    let parent = Directory::open(path.parent().expect("spool parent")).map_err(io_error)?;
    if !directory
        .is_same_child(&parent, "export")
        .map_err(io_error)?
    {
        return Err(error(
            "export_directory_replaced",
            "Каталог export подменён во время проверки",
        ));
    }
    let evidence = format!(
        "desktop-export:{}:{}:{}:{}",
        job.id,
        job.profile_id,
        snapshot.revision(),
        asset_store::hashing::sha256_hex(serde_json::to_vec(&expected).map_err(io_error)?)
    );
    Ok(VerifiedExport {
        snapshot,
        files,
        inventory,
        evidence,
    })
}

pub(super) type ExportBytes = (
    Snapshot,
    BTreeMap<String, Vec<u8>>,
    BTreeMap<String, String>,
);

pub(super) fn read_export(
    directory: &Directory,
    profile: ProfileId,
) -> crate::exchange::Result<ExportBytes> {
    use crate::exchange::ExchangeError;
    let names = directory.names()?;
    if names
        .iter()
        .any(|n| !matches!(n.as_str(), "deck.json" | "media" | "import_config.yaml"))
    {
        return Err(ExchangeError::UnsafePath(
            "неожиданный файл экспорта".into(),
        ));
    }
    let json = directory.read("deck.json")?;
    let mut files = BTreeMap::from([("deck.json".into(), json.clone())]);
    let mut media_bytes = BTreeMap::new();
    let mut total = json.len();
    if let Some(media) = directory.child("media")? {
        let media_names = media.names()?;
        let mut normalized = std::collections::BTreeSet::new();
        use unicode_normalization::UnicodeNormalization;
        for name in &media_names {
            let key: String = name.nfc().flat_map(char::to_lowercase).collect();
            if !normalized.insert(key) {
                return Err(ExchangeError::UnsafePath("коллизия имён media".into()));
            }
            let bytes = media.read(name)?;
            total += bytes.len();
            if total > 1024 * 1024 * 1024 {
                return Err(ExchangeError::UnsafePath("экспорт превышает 1 GiB".into()));
            }
            files.insert(format!("media/{name}"), bytes.clone());
            media_bytes.insert(name.clone(), bytes);
        }
        if media_names != media.names()? || !media.is_same_child(directory, "media")? {
            return Err(ExchangeError::UnsafePath(
                "media изменились при чтении".into(),
            ));
        }
    }
    if names.iter().any(|n| n == "import_config.yaml") {
        files.insert(
            "import_config.yaml".into(),
            directory.read("import_config.yaml")?,
        );
    }
    let snapshot = Snapshot::from_bytes(profile, &json, &media_bytes)?;
    if snapshot
        .unproven()
        .iter()
        .any(|v| v.starts_with("missing_media:"))
    {
        return Err(ExchangeError::InvalidSnapshot(
            "неполные физические media".into(),
        ));
    }
    if names != directory.names()? {
        return Err(ExchangeError::UnsafePath(
            "экспорт изменился при чтении".into(),
        ));
    }
    let inventory = files
        .iter()
        .map(|(n, b)| (n.clone(), asset_store::hashing::sha256_hex(b)))
        .collect();
    Ok((snapshot, files, inventory))
}

fn required(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| error("invalid_export_catalog", format!("В каталоге нет {key}")))
}

#[cfg(test)]
pub(super) fn synthetic(
    bytes: &[u8],
    media: &BTreeMap<String, Vec<u8>>,
    evidence: &str,
) -> VerifiedExport {
    let profile = ProfileId::new("synthetic-profile").unwrap();
    let snapshot = Snapshot::from_bytes(profile, bytes, media).unwrap();
    let mut files = BTreeMap::from([("deck.json".into(), bytes.to_vec())]);
    files.extend(media.iter().map(|(n, b)| (format!("media/{n}"), b.clone())));
    let inventory = files
        .iter()
        .map(|(n, b)| (n.clone(), asset_store::hashing::sha256_hex(b)))
        .collect();
    VerifiedExport {
        snapshot,
        files,
        inventory,
        evidence: evidence.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, TempDir};

    fn live_fixture() -> (TempDir, Target, PendingJob, Value) {
        let temp = TempDir::new("trusted-live-evidence");
        let target = Target {
            executable: temp.path().join("anki"),
            data: temp.path().to_path_buf(),
            native_data: "C:\\Anki2".into(),
            version: "26.9.4".into(),
            config: json!({}),
            discovery: json!({"platform":"windows"}),
            running: true,
            process_probe: || Ok(json!({})),
        };
        let job = PendingJob {
            id: "a".repeat(64),
            profile_id: "b".repeat(64),
            profile: "Synthetic".into(),
            deck_id: "42".into(),
            deck: "Test::Deck".into(),
            deck_uuid: Some("deck-uuid-1".into()),
            process: "process-1".into(),
        };
        let export = target.state().join("spool").join(&job.id).join("export");
        std::fs::create_dir_all(export.join("media")).unwrap();
        std::fs::write(export.join("deck.json"), MINIMAL_EXPORT).unwrap();
        std::fs::write(export.join("media/a.mp3"), vec![13; 200_003]).unwrap();
        std::fs::write(export.join("media/b.png"), b"image").unwrap();
        let directory = Directory::open(&export).unwrap();
        let inventory = read_export(&directory, ProfileId::new(&job.profile_id).unwrap())
            .unwrap()
            .2;
        let result = json!({"state":"succeeded","ready":true,"local_export_dir":export,
            "job":{"state":"succeeded","action":"export","id":job.id,"profile_id":job.profile_id,
                "profile":job.profile,"deck_id":job.deck_id,"deck_name":job.deck,"deck_uuid":"deck-uuid-1",
                "process":job.process,"inventory":inventory,"export_dir":"C:\\Anki2\\untrusted-native-path"}});
        (temp, target, job, result)
    }

    #[test]
    fn only_exact_succeeded_live_job_can_construct_verified_export() {
        let (_temp, target, job, result) = live_fixture();
        let verified = verify(&target, &job, &result).unwrap();
        assert_eq!(verified.files()["media/a.mp3"].len(), 200_003);
        assert_eq!(verified.files()["deck.json"], MINIMAL_EXPORT.as_bytes());
        for (key, changed) in [
            ("state", json!("submitted")),
            ("action", json!("import")),
            ("id", json!("c".repeat(64))),
            ("process", json!("other-process")),
            ("profile_id", json!("other-profile")),
            ("profile", json!("other-name")),
            ("deck_id", json!("43")),
            ("deck_name", json!("Renamed")),
            ("deck_uuid", json!("other-uuid")),
            ("inventory", json!({})),
        ] {
            let mut changed_result = result.clone();
            changed_result["job"][key] = changed;
            assert!(verify(&target, &job, &changed_result).is_err(), "{key}");
        }
        for state in [
            "ready",
            "submitted",
            "submission_unknown",
            "queued",
            "running",
            "failed",
        ] {
            let mut changed = result.clone();
            changed["state"] = json!(state);
            assert!(verify(&target, &job, &changed).is_err(), "{state}");
        }
    }

    #[test]
    fn post_export_tampering_or_substituted_directory_is_not_confirmed() {
        for tamper in ["bytes", "missing", "directory"] {
            let (_temp, target, job, result) = live_fixture();
            let path = target.state().join("spool").join(&job.id).join("export");
            match tamper {
                "bytes" => {
                    std::fs::write(path.join("media/a.mp3"), b"changed-after-success").unwrap()
                }
                "missing" => std::fs::remove_file(path.join("media/a.mp3")).unwrap(),
                _ => {
                    let outside = path.with_file_name("displaced");
                    std::fs::rename(&path, &outside).unwrap();
                    std::os::unix::fs::symlink(outside, &path).unwrap();
                }
            }
            assert!(verify(&target, &job, &result).is_err(), "{tamper}");
        }
    }
}
