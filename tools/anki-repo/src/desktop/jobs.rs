use super::{Options, Result, error, io_error, read_json};
use super::{environment::Target, patch, transport};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

fn safe_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':' | '\0') || c.is_control())
    {
        return Err(error(
            "unsafe_media_name",
            "Имя media должно быть простым именем файла",
        ));
    }
    Ok(())
}
fn copy_import(source: &Path, dest: &Path) -> Result<()> {
    crate::layout::guard_local_path(source, "Источник CrowdAnki")?;
    patch::check_ancestors(source)?;
    // Используем существующий typed loader, а не альтернативный формат JSON.
    crate::loader::load_export(source)?;
    patch::ensure_private(dest)?;
    for name in ["deck.json", "import_config.yaml"] {
        let p = source.join(name);
        patch::check_ancestors(&p)?;
        if p.exists() {
            patch::write_atomic(&dest.join(name), &fs::read(p).map_err(io_error)?)?;
        }
    }
    let media = source.join("media");
    if media.exists() {
        patch::check_ancestors(&media)?;
        patch::ensure_private(&dest.join("media"))?;
        for e in fs::read_dir(media).map_err(io_error)? {
            let e = e.map_err(io_error)?;
            let name = e.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| error("unsafe_media_name", "Имя media не UTF-8"))?;
            safe_name(name)?;
            if !e.file_type().map_err(io_error)?.is_file() {
                return Err(error(
                    "unsafe_media_entry",
                    "В media допустимы только обычные файлы",
                ));
            }
            patch::copy_file_atomic(&e.path(), &dest.join("media").join(name))?;
        }
    }
    Ok(())
}

fn submission_is_unknown(error: &super::DomainError) -> bool {
    error.details["submission_state"] == "unknown"
}

fn discard_unsubmitted(path: &Path, mut error: super::DomainError) -> super::DomainError {
    if let Err(cleanup) = fs::remove_dir_all(path) {
        error.details["spool_cleanup_error"] = json!(cleanup.to_string());
    }
    error
}

fn prepare_unsubmitted(
    path: &Path,
    action: &str,
    deck: Option<&String>,
    profile: &str,
    source: Option<&Path>,
) -> Result<()> {
    let prepared = (|| {
        if let Some(source) = source {
            copy_import(source, &path.join("source"))?;
        }
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io_error)?
            .as_secs()
            + 600;
        let ticket = json!({"action":action,"profile":profile,"deck":deck,"approved":true,"expires":expires});
        patch::write_atomic(
            &path.join("request.json"),
            &serde_json::to_vec(&ticket).map_err(io_error)?,
        )?;
        Ok(())
    })();
    match prepared {
        Ok(()) => Ok(()),
        Err(error) => Err(discard_unsubmitted(path, error)),
    }
}

pub(crate) fn submit(
    t: &Target,
    o: &Options,
    action: &str,
    deck: Option<&String>,
    source: Option<&Path>,
    apply: bool,
) -> Result<Value> {
    let profile = o
        .profile
        .as_ref()
        .ok_or_else(|| error("profile_required", "Для import/export требуется --profile"))?;
    if !apply {
        return Ok(
            json!({"state":"operation_plan","ready":false,"action":action,"deck":deck,"source":source,
        "profile":profile,"consequences":"CrowdAnki изменяет метаданные коллекции при экспорте; импорт показывает штатный диалог и создаёт backup. Ошибка может оставить частичные изменения."}),
        );
    }
    let h = transport::handshake(t, o)?;
    if h["addons_ready"] != true || h["busy"] == true {
        return Err(error(
            "addons_not_ready",
            "Оба аддона должны быть загружены и свободны",
        ));
    }
    let id = patch::random_id()?;
    let spool = t.state().join("spool");
    patch::ensure_private(&spool)?;
    let path = spool.join(&id);
    patch::ensure_private(&path)?;
    prepare_unsubmitted(&path, action, deck, profile, source)?;
    let result = transport::patched(
        t,
        o,
        if action == "export" {
            "toolkitExport"
        } else {
            "toolkitImport"
        },
        json!({"id":id}),
    );
    match result {
        Ok(value) => Ok(
            json!({"state":"submitted","job":value,"spool":path,"ready":false,
            "next":"anki job ID: HTTP admission завершён; при импорте ответьте на видимый диалог Anki"}),
        ),
        // Таймаут мог произойти ПОСЛЕ admission: ticket не удаляется и запрос не повторяется.
        Err(e) if submission_is_unknown(&e) => Ok(
            json!({"state":"submission_unknown","id":id,"spool":path,"ready":false,"error":e.message,"next":"Проверьте anki job; не создавайте повторный импорт"}),
        ),
        Err(e) => Err(discard_unsubmitted(&path, e)),
    }
}

pub(crate) fn status(t: &Target, o: &Options, id: &str) -> Result<Value> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error(
            "invalid_job_id",
            "Ожидается идентификатор из 64 hex символов",
        ));
    }
    match transport::patched(t, o, "toolkitJob", json!({"id":id})) {
        Ok(value) => Ok(status_value(t, id, value)),
        Err(e) => {
            if e.details["reason"] != "http_unreachable" {
                return Err(e);
            }
            let path = t.state().join("spool").join(id).join("result.json");
            patch::check_ancestors(&path)?;
            if path.exists() {
                let mut value = read_json(&path)?;
                if value["state"] == "running" || value["state"] == "queued" {
                    value["state"] = json!("unknown_process_unreachable");
                }
                Ok(
                    json!({"state":value["state"],"job":value,"ready":false,"live_verified":false,"error":e.message}),
                )
            } else {
                Err(e)
            }
        }
    }
}

fn status_value(t: &Target, id: &str, value: Value) -> Value {
    let export = value["action"] == "export" && value["state"] == "succeeded";
    let mut result =
        json!({"state":value["state"],"ready":value["state"]=="succeeded","job":value});
    if export {
        result["local_export_dir"] = json!(t.state().join("spool").join(id).join("export"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_successful_export_status_advertises_result_directory() {
        let tmp = crate::test_support::TempDir::new("desktop-job-status");
        let t = Target {
            executable: tmp.path().join("anki"),
            data: tmp.path().to_path_buf(),
            native_data: tmp.path().to_string_lossy().into_owned(),
            version: "26.9.2".into(),
            config: json!({}),
            discovery: json!({"platform":"linux","processes":[]}),
            running: false,
            process_probe: || Ok(json!({"processes":[]})),
        };
        for action in ["import", "export"] {
            for state in [
                "prepared",
                "queued",
                "running",
                "succeeded",
                "failed",
                "cancelled",
                "interrupted",
            ] {
                let value = status_value(&t, "id", json!({"action":action,"state":state}));
                assert_eq!(value["job"]["action"], action);
                assert_eq!(value["ready"], state == "succeeded");
                assert_eq!(
                    value.get("local_export_dir").is_some(),
                    action == "export" && state == "succeeded"
                );
                if action == "export" && state == "succeeded" {
                    assert_eq!(
                        value["local_export_dir"],
                        json!(t.state().join("spool/id/export"))
                    );
                }
            }
        }
    }

    #[test]
    fn import_media_copy_preserves_multiple_chunks() {
        let tmp = crate::test_support::TempDir::new("desktop-import-large-media");
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join("media")).unwrap();
        fs::write(
            source.join("deck.json"),
            crate::test_support::MINIMAL_EXPORT,
        )
        .unwrap();
        let bytes: Vec<u8> = (0..200_003).map(|i| (i % 251) as u8).collect();
        fs::write(source.join("media/multi.bin"), &bytes).unwrap();
        let destination = tmp.path().join("destination");
        copy_import(&source, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("media/multi.bin")).unwrap(),
            bytes
        );
    }

    #[test]
    fn media_names_reject_traversal_and_windows_paths() {
        for name in ["../x", "C:x", "a\\b", "/x", ".", "..", "a\0b"] {
            assert!(safe_name(name).is_err(), "{name:?}");
        }
        assert!(safe_name("音声.png").is_ok());
    }

    #[test]
    fn only_requests_that_may_have_been_sent_are_submission_unknown() {
        let mut failure = error("api_error", "anki_busy");
        failure.details["submission_state"] = json!("rejected");
        assert!(!submission_is_unknown(&failure));

        failure.details["submission_state"] = json!("unknown");
        assert!(submission_is_unknown(&failure));
    }

    #[test]
    fn known_unsubmitted_ticket_is_removed_from_the_spool() {
        let tmp = crate::test_support::TempDir::new("desktop-job-rejected");
        let ticket = tmp.path().join("request.json");
        fs::write(&ticket, b"prepared").unwrap();
        let failure = discard_unsubmitted(tmp.path(), error("api_error", "anki_busy"));
        assert_eq!(failure.details["reason"], "api_error");
        assert!(!tmp.path().exists());
    }

    #[test]
    fn partial_import_copy_is_removed_before_submission() {
        let tmp = crate::test_support::TempDir::new("desktop-job-partial-copy");
        let source = tmp.path().join("export");
        let media = source.join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(
            source.join("deck.json"),
            crate::test_support::MINIMAL_EXPORT.as_bytes(),
        )
        .unwrap();
        fs::write(media.join("valid.mp3"), b"audio").unwrap();
        fs::write(media.join("C:invalid.mp3"), b"invalid name").unwrap();

        let spool = tmp.path().join("spool");
        fs::create_dir_all(&spool).unwrap();
        let error = prepare_unsubmitted(&spool, "import", None, "Test", Some(&source)).unwrap_err();

        assert_eq!(error.details["reason"], "unsafe_media_name");
        assert!(!spool.exists());
    }
}
