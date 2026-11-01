use super::*;
use crate::temp_workspace::TempWorkspace;

fn request() -> Pronunciation {
    Pronunciation::new("元気", "げんき").unwrap()
}

fn item(state: &str) -> ItemFacts {
    ItemFacts {
        id: 7,
        state: state.into(),
        error: None,
        danger: "safe".into(),
        exists: true,
        paused: false,
        has_filename: true,
        filename: Some("/home/u/Downloads/pronunciation_ja_元気.mp3".into()),
        file_name: Some("pronunciation_ja_元気.mp3".into()),
        bytes_received: 3,
        total_bytes: 3,
        file_size: 3,
        mime: Some("audio/mpeg".into()),
        url: json!({"origin":"https://forvo.com","path":"/download/mp3/%E5%85%83%E6%B0%97/ja/3661006",
            "has_query":false,"has_fragment":false,"has_credentials":false}),
        final_url: Value::Null,
        referrer_is_own_page: true,
        referrer_origin_is_forvo: true,
        by_extension: false,
        start_time: None,
        end_time: None,
    }
}

fn facts(item: Option<ItemFacts>, elapsed_ms: u64) -> StatusFacts {
    StatusFacts {
        code: "status".into(),
        observer_alive: true,
        own_requests: 1,
        foreign_requests: 0,
        matching_downloads: u32::from(item.is_some()),
        conflict: false,
        observer_lost: false,
        bound_id: item.as_ref().map(|i| i.id),
        bound_missing: false,
        item,
        tab_present: true,
        page_unchanged: true,
        elapsed_ms,
    }
}

fn classify_default(facts: &StatusFacts) -> Progress {
    classify(facts, &request(), "3661006", &Limits::default())
}

#[test]
fn ownership_ambiguity_stops_before_any_download_state() {
    // Конфликт важнее даже завершённой загрузки: несвязанная операция ничего не принимает.
    let mut value = facts(Some(item("complete")), 100);
    value.conflict = true;
    assert_eq!(
        classify_default(&value),
        Progress::Failed("download_conflict")
    );
    let mut lost = facts(None, 100);
    lost.observer_lost = true;
    assert_eq!(
        classify_default(&lost),
        Progress::Failed("ownership_unproven")
    );
    let unknown = StatusFacts {
        code: "operation_unknown".into(),
        ..StatusFacts::default()
    };
    assert_eq!(
        classify_default(&unknown),
        Progress::Failed("operation_lost")
    );
    let mut vanished = facts(None, 100);
    vanished.bound_missing = true;
    assert_eq!(
        classify_default(&vanished),
        Progress::Failed("download_vanished")
    );
}

#[test]
fn unbound_operation_waits_then_fails_closed() {
    let mut waiting = facts(None, 1000);
    waiting.own_requests = 0;
    assert_eq!(
        classify_default(&waiting),
        Progress::Waiting("waiting_request")
    );
    waiting.own_requests = 1;
    assert_eq!(
        classify_default(&waiting),
        Progress::Waiting("waiting_download")
    );
    let mut late = facts(None, 25_000);
    late.matching_downloads = 0;
    assert_eq!(
        classify_default(&late),
        Progress::Failed("download_not_started")
    );
    // Совпавшая загрузка без доказанного запроса своей вкладки не усыновляется.
    late.own_requests = 0;
    late.matching_downloads = 1;
    assert_eq!(
        classify_default(&late),
        Progress::Failed("ownership_unproven")
    );
    let mut closed = facts(None, 100);
    closed.tab_present = false;
    assert_eq!(
        classify_default(&closed),
        Progress::Failed("own_tab_closed")
    );
}

#[test]
fn bound_item_must_match_selected_record_and_own_page() {
    let mut other = item("complete");
    other.url["path"] = json!("/download/mp3/%E5%85%83%E6%B0%97/ja/5036880");
    assert_eq!(
        classify_default(&facts(Some(other), 100)),
        Progress::Failed("download_mismatch")
    );
    let mut query = item("complete");
    query.url["has_query"] = json!(true);
    assert_eq!(
        classify_default(&facts(Some(query), 100)),
        Progress::Failed("download_mismatch")
    );
    let mut extension = item("complete");
    extension.by_extension = true;
    assert_eq!(
        classify_default(&facts(Some(extension), 100)),
        Progress::Failed("download_mismatch")
    );
    let mut foreign_page = item("complete");
    foreign_page.referrer_is_own_page = false;
    foreign_page.referrer_origin_is_forvo = false;
    assert_eq!(
        classify_default(&facts(Some(foreign_page), 100)),
        Progress::Failed("download_mismatch")
    );
}

#[test]
fn download_states_are_distinguished() {
    let complete = item("complete");
    assert_eq!(
        classify_default(&facts(Some(complete.clone()), 100)),
        Progress::Complete(Box::new(complete))
    );
    let mut dialog = item("in_progress");
    dialog.has_filename = false;
    dialog.filename = None;
    assert_eq!(
        classify_default(&facts(Some(dialog.clone()), 1000)),
        Progress::Waiting("in_progress")
    );
    assert_eq!(
        classify_default(&facts(Some(dialog.clone()), 9000)),
        Progress::Failed("save_as_required")
    );
    let mut dismissed = dialog;
    dismissed.state = "interrupted".into();
    dismissed.error = Some("USER_CANCELED".into());
    assert_eq!(
        classify_default(&facts(Some(dismissed), 100)),
        Progress::Failed("save_as_required")
    );
    let mut canceled = item("interrupted");
    canceled.error = Some("USER_CANCELED".into());
    assert_eq!(
        classify_default(&facts(Some(canceled), 100)),
        Progress::Failed("download_canceled")
    );
    let mut network = item("interrupted");
    network.error = Some("NETWORK_FAILED".into());
    assert_eq!(
        classify_default(&facts(Some(network), 100)),
        Progress::Failed("download_interrupted")
    );
    assert_eq!(
        classify_default(&facts(Some(item("in_progress")), 100_000)),
        Progress::Failed("download_timeout")
    );
    let mut dangerous = item("complete");
    dangerous.danger = "file".into();
    assert_eq!(
        classify_default(&facts(Some(dangerous), 100)),
        Progress::Failed("download_dangerous")
    );
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> ItemFacts {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    let mut value = item("complete");
    value.filename = Some(path.display().to_string());
    value.bytes_received = bytes.len() as i64;
    value.total_bytes = bytes.len() as i64;
    value.file_size = bytes.len() as i64;
    value
}

fn code(result: Result<(PathBuf, Vec<u8>), AssetError>) -> String {
    provider_code(&result.unwrap_err()).to_owned()
}

#[test]
fn file_is_read_only_after_consistent_completion() {
    let temp = TempWorkspace::create("brave-download-file").unwrap();
    let ok = write(temp.path(), "a.mp3", b"\xff\xfb\x90");
    let (path, bytes) = verify_file(Platform::Linux, &ok).unwrap();
    assert_eq!(bytes, b"\xff\xfb\x90");
    assert_eq!(path, temp.path().join("a.mp3"));
    let mut partial = ok.clone();
    partial.filename = Some(format!("{}.crdownload", ok.filename.as_deref().unwrap()));
    assert_eq!(
        code(verify_file(Platform::Linux, &partial)),
        "download_incomplete"
    );
    let mut running = ok.clone();
    running.state = "in_progress".into();
    assert_eq!(
        code(verify_file(Platform::Linux, &running)),
        "download_incomplete"
    );
    let mut sizes = ok.clone();
    sizes.total_bytes = 4;
    assert_eq!(
        code(verify_file(Platform::Linux, &sizes)),
        "download_incomplete"
    );
    let mut grown = ok.clone();
    grown.bytes_received = 2;
    grown.total_bytes = 2;
    grown.file_size = 2;
    assert_eq!(
        code(verify_file(Platform::Linux, &grown)),
        "download_incomplete"
    );
    let mut missing = ok.clone();
    missing.filename = Some(temp.path().join("none.mp3").display().to_string());
    assert_eq!(code(verify_file(Platform::Linux, &missing)), "file_missing");
    let mut gone = ok.clone();
    gone.exists = false;
    assert_eq!(code(verify_file(Platform::Linux, &gone)), "file_missing");
    let html = write(temp.path(), "b.mp3", b"  <html>");
    assert_eq!(
        code(verify_file(Platform::Linux, &html)),
        "download_invalid"
    );
    std::os::unix::fs::symlink(temp.path().join("a.mp3"), temp.path().join("link.mp3")).unwrap();
    let mut link = ok.clone();
    link.filename = Some(temp.path().join("link.mp3").display().to_string());
    assert_eq!(
        code(verify_file(Platform::Linux, &link)),
        "file_access_denied"
    );
    let mut relative = ok;
    relative.filename = Some("a.mp3".into());
    assert_eq!(
        code(verify_file(Platform::Linux, &relative)),
        "path_mapping_failed"
    );
}

#[test]
fn operation_ids_are_unique_lower_hex() {
    let a = new_operation_id().unwrap();
    let b = new_operation_id().unwrap();
    assert_eq!(a.len(), 32);
    assert!(
        a.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    );
    assert_ne!(a, b);
}
