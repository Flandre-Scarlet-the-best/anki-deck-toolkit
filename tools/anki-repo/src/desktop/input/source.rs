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
    // Старый журнал без этого поля НЕ доказывает отсутствие принятия задания по HTTP.
    #[serde(default = "uncertain_admission")]
    pub admission_started: bool,
}

fn uncertain_admission() -> bool {
    true
}

impl PendingJob {
    pub fn key(&self) -> String {
        asset_store::hashing::sha256_hex(
            serde_json::to_vec(&json!([self.profile_id, self.deck_id]))
                .expect("сериализуемые идентификаторы job"),
        )
    }

    pub fn prepared_action(&self, observer: &str) -> Result<PreparedAction> {
        if self.process == observer {
            return Ok(PreparedAction::SubmitSameId);
        }
        if !self.admission_started {
            return Ok(PreparedAction::RenewSource);
        }
        Err(super::super::external_error(
            "export_completion_unknown_after_restart",
            "Anki перезапущена, а надёжно сохранённый результат отсутствует после начала отправки. ID сохранён; prepared не доказывает отсутствие прежнего экспорта. Проверьте anki job и восстановите свидетельство; новый export автоматически не отправляется",
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PreparedAction {
    SubmitSameId,
    RenewSource,
}

/// Поля закрыты: рабочий путь создаёт его только по живому toolkitJob.
pub(super) struct VerifiedExport {
    snapshot: Snapshot,
    files: BTreeMap<String, Vec<u8>>,
    inventory: BTreeMap<String, String>,
    evidence: String,
    /// Отпечаток состояния колоды в коллекции, снятый адаптером в том же
    /// потоке GUI, что и сам экспорт. Это детектор изменений Anki → Anki,
    /// а не второй источник истины о содержимом.
    collection_state: Option<String>,
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
    /// Ожидаемое состояние для охраны импорта: отпечаток коллекции и хеши
    /// media проверенного C. Без отпечатка охрану построить нельзя, и импорт
    /// не отправляется.
    pub fn import_guard(&self) -> Option<Value> {
        let state = self.collection_state.as_ref()?;
        let media: BTreeMap<&str, &str> = self
            .inventory
            .iter()
            .filter_map(|(name, hash)| name.strip_prefix("media/").map(|n| (n, hash.as_str())))
            .collect();
        Some(serde_json::json!({"collection_state":state,
            "deck_uuid":self.snapshot.identity().crowdanki_uuid.as_str(),
            "revision":self.snapshot.revision(),"media":media}))
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
        admission_started: false,
    };
    let path = t.state().join("spool").join(&job.id);
    crate::layout::guard_local_path(&path, "Задание Desktop")?;
    let data = Directory::open(&t.data).map_err(io_error)?;
    let state = data.ensure_child("toolkit-desktop").map_err(io_error)?.0;
    state.require_private().map_err(|_| error("export_spool_not_private",
        "Каталог toolkit-desktop не обеспечивает приватность spool. В WSL DrvFS без metadata не поддерживается: настройте проверяемые права 0700; задание не отправлено"))?;
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
            final_rejection(
                "export_evidence_missing",
                "Нет полного свидетельства байтов живого экспорта",
            )
        })?;
    let directory = Directory::open(&path).map_err(io_error)?;
    let (snapshot, files, inventory) = read_export(
        &directory,
        ProfileId::new(&job.profile_id).map_err(io_error)?,
    )
    .map_err(|e| match e {
        crate::exchange::ExchangeError::Io(e) => io_error(e),
        _ => final_rejection(
            "export_validation_failed",
            "Экспорт не прошёл структурную проверку или проверку физических файлов",
        ),
    })?;
    if inventory != expected
        || snapshot.identity().crowdanki_uuid != value["deck_uuid"]
        || snapshot.name() != job.deck
    {
        return Err(final_rejection(
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
        collection_state: value["collection_state"].as_str().map(str::to_owned),
    })
}

fn final_rejection(reason: &str, message: &str) -> super::super::DomainError {
    let mut failure = error(reason, message);
    failure.details["export_rejection_final"] = json!(true);
    failure
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
    read_export_with_limit(directory, profile, 1024 * 1024 * 1024)
}

fn read_export_with_limit(
    directory: &Directory,
    profile: ProfileId,
    limit: usize,
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
    let mut total = 0;
    count_bytes(&mut total, json.len(), limit)?;
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
            count_bytes(&mut total, bytes.len(), limit)?;
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
        let bytes = directory.read("import_config.yaml")?;
        count_bytes(&mut total, bytes.len(), limit)?;
        files.insert("import_config.yaml".into(), bytes);
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

fn count_bytes(total: &mut usize, size: usize, limit: usize) -> crate::exchange::Result<()> {
    *total = total
        .checked_add(size)
        .filter(|n| *n <= limit)
        .ok_or_else(|| {
            crate::exchange::ExchangeError::UnsafePath("экспорт превышает 1 GiB".into())
        })?;
    Ok(())
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
        collection_state: Some(format!("synthetic-state:{evidence}")),
    }
}

/// Синтетический проверенный экспорт выбранного профиля, с необязательным YAML.
#[cfg(test)]
pub(super) fn synthetic_for(
    profile: &str,
    bytes: &[u8],
    media: &BTreeMap<String, Vec<u8>>,
    config: Option<&[u8]>,
    evidence: &str,
) -> VerifiedExport {
    let snapshot = Snapshot::from_bytes(ProfileId::new(profile).unwrap(), bytes, media).unwrap();
    let mut files = BTreeMap::from([("deck.json".into(), bytes.to_vec())]);
    files.extend(media.iter().map(|(n, b)| (format!("media/{n}"), b.clone())));
    if let Some(config) = config {
        files.insert("import_config.yaml".into(), config.to_vec());
    }
    let inventory = files
        .iter()
        .map(|(n, b)| (n.clone(), asset_store::hashing::sha256_hex(b)))
        .collect();
    VerifiedExport {
        snapshot,
        files,
        inventory,
        evidence: evidence.into(),
        collection_state: Some(format!("synthetic-state:{evidence}")),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, TempDir};

    pub(in crate::desktop::input) fn live_fixture() -> (TempDir, Target, PendingJob, Value) {
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
            id: patch::random_id().unwrap(),
            profile_id: "b".repeat(64),
            profile: "Synthetic".into(),
            deck_id: "42".into(),
            deck: "Test::Deck".into(),
            deck_uuid: Some("deck-uuid-1".into()),
            process: "process-1".into(),
            admission_started: true,
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
    fn windows_spool_without_private_modes_is_refused_before_creating_a_ticket() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, target, job, _) = live_fixture();
        std::fs::set_permissions(target.state(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let failure = prepare(
            &target,
            &options(&target, 1),
            &json!({"profile_id":job.profile_id,"process":job.process}),
            &json!({"id":job.deck_id,"name":job.deck}),
        )
        .unwrap_err();
        assert_eq!(failure.details["reason"], "export_spool_not_private");
        assert!(
            !target
                .state()
                .join("spool")
                .join(&job.id)
                .join("request.json")
                .exists()
        );
        assert_eq!(
            std::fs::read_dir(target.state().join("spool"))
                .unwrap()
                .count(),
            1
        );
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

    fn options(target: &Target, port: u16) -> Options {
        Options {
            action: crate::desktop::Action::Doctor,
            executable: Some(target.executable.clone()),
            data_dir: Some(target.data.clone()),
            profile: Some("Synthetic".into()),
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            api_key_env: "ANKI_TEST_UNUSED_API_KEY".into(),
        }
    }

    #[test]
    fn poll_rejects_persisted_fallback_and_live_http_without_extra_flag_can_publish() {
        use std::io::{BufRead, Read, Write};
        let (tmp, mut target, job, result) = live_fixture();
        target.executable = std::env::current_exe().unwrap();
        target.discovery = json!({"platform":"linux","processes":[{
            "pid":std::process::id().to_string(),"command_line":[target.executable]}]});
        std::fs::create_dir_all(target.bridge()).unwrap();
        std::fs::write(
            target.bridge().join("settings.json"),
            b"{\"token\":\"synthetic\"}",
        )
        .unwrap();
        let path = target
            .state()
            .join("spool")
            .join(&job.id)
            .join("result.json");
        std::fs::write(&path, serde_json::to_vec(&result["job"]).unwrap()).unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let o = options(&target, port);
        let fallback = jobs::status(&target, &o, &job.id).unwrap();
        assert_eq!(fallback["live_verified"], false);
        assert_eq!(fallback["state"], "succeeded");
        assert_eq!(
            poll(&target, &o, &job).unwrap_err().details["reason"],
            "export_not_live_verified"
        );
        let repository = tmp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let store = super::super::publish::Store::open(&repository).unwrap();
        assert!(
            store.audit().unwrap()["streams"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let o = options(&target, listener.local_addr().unwrap().port());
        let response = result["job"].clone();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&connection);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["action"], "toolkitJob");
            let body = serde_json::to_string(&json!({"result":response,"error":null})).unwrap();
            write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let live = poll(&target, &o, &job).unwrap();
        assert!(live.get("live_verified").is_none());
        let verified = verify(&target, &job, &live).unwrap();
        assert_eq!(store.publish(&verified).unwrap()["source_sequence"], 1);
        server.join().unwrap();
        store.audit().unwrap();
    }

    #[test]
    fn pending_selection_checks_name_known_uuid_and_accepts_first_assigned_uuid() {
        let (tmp, target, mut job, result) = live_fixture();
        let repository = tmp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let mut store = super::super::publish::Store::open(&repository).unwrap();
        store.save_job(&job.key(), job.clone()).unwrap();
        let o = options(&target, 1);
        let catalog = json!({"profile_id":job.profile_id});
        let deck = json!({"id":job.deck_id,"name":job.deck,"crowdanki_uuid":job.deck_uuid});
        super::super::validate_pending_source(&job, &catalog, &deck).unwrap();
        for (field, value) in [
            ("name", "Renamed"),
            ("crowdanki_uuid", "other-uuid"),
            ("id", "other-id"),
        ] {
            let mut changed = deck.clone();
            changed[field] = json!(value);
            let key = asset_store::hashing::sha256_hex(
                serde_json::to_vec(&json!([catalog["profile_id"], changed["id"]])).unwrap(),
            );
            assert_eq!(
                super::super::export_one(&target, &o, &catalog, &changed, &key, &mut store, 1)
                    .unwrap_err()
                    .details["reason"],
                "pending_export_identity_mismatch"
            );
            assert_eq!(store.pending(&job.key()).unwrap().id, job.id);
            assert!(
                store.audit().unwrap()["streams"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        job.deck_uuid = None;
        super::super::validate_pending_source(&job, &catalog, &deck).unwrap();
        verify(&target, &job, &result).unwrap();
    }

    #[test]
    fn missing_selection_keeps_uncertain_attempt_after_rename_or_removal() {
        let (tmp, mut target, job, _) = live_fixture();
        target.discovery = json!({"platform":"linux"});
        std::fs::create_dir_all(target.bridge()).unwrap();
        std::fs::write(
            target.bridge().join("settings.json"),
            b"{\"token\":\"synthetic\"}",
        )
        .unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let o = options(&target, socket.local_addr().unwrap().port());
        drop(socket);
        let repository = tmp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let mut store = super::super::publish::Store::open(&repository).unwrap();
        store.save_job(&job.key(), job.clone()).unwrap();
        for decks in [json!([]), json!([{"id":job.deck_id,"name":"Renamed"}])] {
            let selected = super::super::select(
                &json!({"decks":decks}),
                std::slice::from_ref(&job.deck),
                false,
            )
            .unwrap();
            let pending = store.pending_named(&job.profile_id, &job.deck);
            let outcome = super::super::missing_outcome(&target, &o, &selected[0], &pending);
            assert_eq!(outcome["reason"], "pending_export_source_missing");
            assert_eq!(outcome["anki_export_may_have_completed"], true);
            assert_eq!(outcome["pending_jobs"][0]["job_id"], job.id);
            assert_eq!(outcome["pending_jobs"][0]["reason"], "http_unreachable");
        }
        drop(store);
        let store = super::super::publish::Store::open(&repository).unwrap();
        assert_eq!(store.pending(&job.key()).unwrap().id, job.id);
        assert_eq!(store.audit().unwrap()["pending_jobs"], 1);
    }

    #[test]
    fn total_budget_counts_configuration_and_rejects_overflow() {
        let limit = 1024 * 1024 * 1024;
        let mut total = 0;
        count_bytes(&mut total, limit - 4, limit).unwrap();
        count_bytes(&mut total, 4, limit).unwrap();
        assert!(count_bytes(&mut total, 1, limit).is_err());
        let mut total = usize::MAX;
        assert!(count_bytes(&mut total, 1, limit).is_err());
        let (_tmp, target, job, mut result) = live_fixture();
        let export = target.state().join("spool").join(&job.id).join("export");
        std::fs::write(export.join("import_config.yaml"), b"config").unwrap();
        let directory = Directory::open(&export).unwrap();
        let profile = ProfileId::new(&job.profile_id).unwrap();
        let (_, files, _) = read_export(&directory, profile.clone()).unwrap();
        let actual: usize = files.values().map(Vec::len).sum();
        // Та же проверка чтения, но малый бюджет: без учёта YAML этот отказ исчезнет.
        assert!(read_export_with_limit(&directory, profile.clone(), actual - 1).is_err());
        assert!(read_export_with_limit(&directory, profile, actual).is_ok());
        assert_eq!(
            verify(&target, &job, &result).err().unwrap().details["reason"],
            "export_bytes_mismatch"
        );
        result["job"]["inventory"] = json!(
            read_export(
                &Directory::open(&export).unwrap(),
                ProfileId::new(&job.profile_id).unwrap()
            )
            .unwrap()
            .2
        );
        assert_eq!(
            verify(&target, &job, &result).unwrap().files()["import_config.yaml"],
            b"config"
        );
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
