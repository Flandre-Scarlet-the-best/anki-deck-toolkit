//! Синтетические проверки цикла input → output → Anki без Docker и без
//! пользовательских данных: подготовка, сбои на каждой границе, журнал импорта,
//! решения по readback и согласованная фиксация публикации.
use super::super::publication::{self, Bridge, ImportJournal, Progress};
use super::super::source::{self, VerifiedExport};
use super::*;
use crate::exchange::ProfileId;
use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, TempDir};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;

const PROFILE: &str = "synthetic-profile";

fn media() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("a.mp3".into(), b"audio".to_vec()),
        ("b.png".into(), b"image".to_vec()),
    ])
}

fn deck(meaning: &str) -> Vec<u8> {
    let mut value: Value = serde_json::from_str(MINIMAL_EXPORT).unwrap();
    value["notes"][0]["fields"][1] = json!(meaning);
    serde_json::to_vec_pretty(&value).unwrap()
}

fn export(meaning: &str, evidence: &str) -> VerifiedExport {
    source::synthetic_for(PROFILE, &deck(meaning), &media(), None, evidence)
}

fn open(root: &Path) -> Store {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match Store::open(root) {
            Err(ExchangeError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now()
            }
            result => return result.unwrap(),
        }
    }
}

fn stream(store: &Store, export: &VerifiedExport) -> StreamState {
    store.exchange().read().unwrap().streams[&export.snapshot().identity().key()].clone()
}

fn output(root: &Path, stream: &StreamState) -> PathBuf {
    root.join("decks/output").join(&stream.output_directory)
}

fn tree(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if entry.file_type().unwrap().is_dir() {
            for (child, bytes) in tree(&entry.path()) {
                files.insert(format!("{name}/{child}"), bytes);
            }
        } else {
            files.insert(name, fs::read(entry.path()).unwrap());
        }
    }
    files
}

fn apply(store: &Store, export: &VerifiedExport) -> Result<Value> {
    apply_one(store, &stream(store, export))
}

fn work_names(root: &Path, export: &VerifiedExport) -> Vec<String> {
    let dir = root
        .join("decks/.roundtrip")
        .join(export.snapshot().identity().key());
    let mut names: Vec<_> = fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn base_sequence(store: &Store, export: &VerifiedExport) -> Option<u64> {
    stream(store, export).base.map(|b| b.source_sequence)
}

#[test]
fn first_preparation_copies_exact_bytes_and_repeats_idempotently() {
    let tmp = TempDir::new("prepare-first");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    let planned = plan_one(&store, &stream(&store, &v1)).unwrap();
    assert_eq!(planned["action"], "prepare_first");
    assert!(
        !tmp.path().join("decks/output").exists(),
        "план ничего не пишет"
    );

    let prepared = apply(&store, &v1).unwrap();
    assert_eq!(prepared["state"], "prepared");
    let path = output(tmp.path(), &stream(&store, &v1));
    assert_eq!(tree(&path), v1.files().clone());
    assert_eq!(base_sequence(&store, &v1), Some(1));
    assert!(work_names(tmp.path(), &v1).is_empty());

    use std::os::unix::fs::MetadataExt;
    let inode = fs::metadata(path.join("deck.json")).unwrap().ino();
    let again = apply(&store, &v1).unwrap();
    assert_eq!(again["state"], "kept");
    // Новый живой номер при том же снимке не продвигает базу и не переписывает файлы.
    store.publish(&export("v1", "job-2")).unwrap();
    assert_eq!(apply(&store, &v1).unwrap()["state"], "kept");
    assert_eq!(base_sequence(&store, &v1), Some(1));
    assert_eq!(fs::metadata(path.join("deck.json")).unwrap().ino(), inode);
}

#[test]
fn refresh_replaces_only_unchanged_working_copy_and_keeps_edits() {
    let tmp = TempDir::new("prepare-refresh");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let v2 = export("v2", "job-2");
    store.publish(&v2).unwrap();
    assert_eq!(
        plan_one(&store, &stream(&store, &v2)).unwrap()["action"],
        "refresh_unchanged"
    );
    let refreshed = apply(&store, &v2).unwrap();
    assert_eq!(refreshed["state"], "prepared");
    let path = output(tmp.path(), &stream(&store, &v2));
    assert_eq!(tree(&path), v2.files().clone());
    assert_eq!(base_sequence(&store, &v2), Some(2));
    assert!(work_names(tmp.path(), &v2).is_empty());

    // Агентская правка B и неизменный C → keep без перезаписи.
    fs::write(path.join("deck.json"), deck("agent edit")).unwrap();
    let kept = apply(&store, &v2).unwrap();
    assert_eq!(kept["state"], "kept");
    assert_eq!(
        fs::read(path.join("deck.json")).unwrap(),
        deck("agent edit")
    );

    // Обе стороны изменились → blocked; B, A и C сохранены.
    store.publish(&export("v3", "job-3")).unwrap();
    let blocked = apply(&store, &v2).unwrap();
    assert_eq!(blocked["state"], "blocked");
    assert_eq!(blocked["reason"], "stale_or_unproven_base");
    assert_eq!(
        fs::read(path.join("deck.json")).unwrap(),
        deck("agent edit")
    );
    assert_eq!(base_sequence(&store, &v2), Some(2));
}

#[test]
fn untracked_and_missing_working_copies_are_never_assigned() {
    let tmp = TempDir::new("prepare-untracked");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    fs::create_dir_all(&path).unwrap();
    // Даже побайтно совпадающий каталог не присваивается.
    for (name, bytes) in v1.files() {
        fs::create_dir_all(path.join(name).parent().unwrap()).unwrap();
        fs::write(path.join(name), bytes).unwrap();
    }
    let blocked = apply(&store, &v1).unwrap();
    assert_eq!(blocked["reason"], "untracked_output");
    assert_eq!(base_sequence(&store, &v1), None);
    assert_eq!(tree(&path), v1.files().clone());
    fs::remove_dir_all(&path).unwrap();
    apply(&store, &v1).unwrap();
    fs::remove_dir_all(&path).unwrap();
    let missing = apply(&store, &v1).unwrap();
    assert_eq!(missing["reason"], "working_missing");
    assert!(!path.exists());
}

#[test]
fn import_config_is_copied_reported_and_never_silently_dropped() {
    let tmp = TempDir::new("prepare-config");
    let store = open(tmp.path());
    let v1 = source::synthetic_for(PROFILE, &deck("v1"), &media(), Some(b"a: 1\n"), "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    assert_eq!(
        fs::read(path.join("import_config.yaml")).unwrap(),
        b"a: 1\n"
    );
    // Физическое изменение YAML в C без смены ревизии: keep показывает различие.
    let yaml = source::synthetic_for(PROFILE, &deck("v1"), &media(), Some(b"a: 2\n"), "job-2");
    store.publish(&yaml).unwrap();
    let kept = apply(&store, &v1).unwrap();
    assert_eq!(kept["state"], "kept");
    assert_eq!(kept["physical_differences"], json!(["import_config.yaml"]));
    assert_eq!(
        fs::read(path.join("import_config.yaml")).unwrap(),
        b"a: 1\n"
    );
    // Обновление C при собственном YAML рабочей копии не теряет его байты.
    let v2 = source::synthetic_for(PROFILE, &deck("v2"), &media(), Some(b"a: 2\n"), "job-3");
    store.publish(&v2).unwrap();
    let blocked = apply(&store, &v2).unwrap_err();
    assert_eq!(
        failure(&stream(&store, &v2), blocked)["reason"],
        "working_import_config_differs"
    );
    assert_eq!(
        fs::read(path.join("import_config.yaml")).unwrap(),
        b"a: 1\n"
    );
}

#[test]
fn unknown_files_links_and_work_entries_block_without_removal() {
    let tmp = TempDir::new("prepare-unsafe");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    let v2 = export("v2", "job-2");
    store.publish(&v2).unwrap();

    fs::write(path.join("notes.txt"), b"user").unwrap();
    assert_eq!(
        apply(&store, &v2).unwrap_err().to_string(),
        "операция заблокирована: unexpected_export_entries"
    );
    assert_eq!(fs::read(path.join("notes.txt")).unwrap(), b"user");
    fs::remove_file(path.join("notes.txt")).unwrap();

    fs::hard_link(path.join("media/a.mp3"), tmp.path().join("hardlink")).unwrap();
    assert!(apply(&store, &v2).is_err());
    fs::remove_file(tmp.path().join("hardlink")).unwrap();

    let work = tmp
        .path()
        .join("decks/.roundtrip")
        .join(v1.snapshot().identity().key());
    fs::write(work.join("foreign"), b"x").unwrap();
    assert_eq!(
        apply(&store, &v2).unwrap_err().to_string(),
        "операция заблокирована: unknown_work_entries"
    );
    assert!(work.join("foreign").exists());
    fs::remove_file(work.join("foreign")).unwrap();

    // Symlink вместо рабочей копии не открывается и не заменяется.
    let real = tmp.path().join("real-copy");
    fs::rename(&path, &real).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    assert!(apply(&store, &v2).is_err());
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&path).unwrap();
    fs::rename(&real, &path).unwrap();
    assert_eq!(apply(&store, &v2).unwrap()["state"], "prepared");
}

#[test]
fn every_preparation_crash_window_recovers_to_confirmed_bytes() {
    for refresh in [false, true] {
        for step in [
            "prepare_staged",
            "prepare_intent",
            "prepare_displaced",
            "prepare_visible",
            "prepare_recorded",
        ] {
            if step == "prepare_displaced" && !refresh {
                continue;
            }
            let tmp = TempDir::new("prepare-crash");
            let store = open(tmp.path());
            let v1 = export("v1", "job-1");
            store.publish(&v1).unwrap();
            let target = if refresh {
                apply(&store, &v1).unwrap();
                let v2 = export("v2", "job-2");
                store.publish(&v2).unwrap();
                v2
            } else {
                v1
            };
            store.fault.set(Some(step));
            assert!(apply(&store, &target).is_err(), "{step}");
            drop(store);
            let store = open(tmp.path());
            let recovered = apply(&store, &target).unwrap();
            assert!(
                matches!(recovered["state"].as_str(), Some("prepared" | "kept")),
                "{refresh} {step}: {recovered}"
            );
            let path = output(tmp.path(), &stream(&store, &target));
            assert_eq!(tree(&path), target.files().clone(), "{refresh} {step}");
            assert_eq!(
                base_sequence(&store, &target),
                Some(if refresh { 2 } else { 1 })
            );
            assert!(
                work_names(tmp.path(), &target).is_empty(),
                "{refresh} {step}"
            );
        }
    }
}

#[test]
fn edit_between_swap_and_base_commit_is_preserved_for_manual_review() {
    let tmp = TempDir::new("prepare-race");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let v2 = export("v2", "job-2");
    store.publish(&v2).unwrap();
    store.fault.set(Some("prepare_visible"));
    assert!(apply(&store, &v2).is_err());
    let path = output(tmp.path(), &stream(&store, &v2));
    fs::write(path.join("deck.json"), deck("concurrent edit")).unwrap();
    let blocked = apply(&store, &v2).unwrap_err();
    assert_eq!(
        blocked.to_string(),
        "операция заблокирована: working_changed_after_swap"
    );
    assert_eq!(
        fs::read(path.join("deck.json")).unwrap(),
        deck("concurrent edit")
    );
    assert_eq!(base_sequence(&store, &v2), Some(1));
    assert_eq!(work_names(tmp.path(), &v2), ["prepare.json", "previous"]);
}

#[test]
fn concurrent_export_lock_holder_delays_refresh_and_sees_replaced_directory() {
    let tmp = TempDir::new("prepare-lock");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    let v2 = export("v2", "job-2");
    store.publish(&v2).unwrap();
    let lock = crate::write::ExportLock::acquire(&path).unwrap();
    let original = fs::read(path.join("deck.json")).unwrap();
    let handle = std::thread::spawn({
        let root = tmp.path().to_path_buf();
        move || {
            let store = open(&root);
            let stream = store
                .exchange()
                .read()
                .unwrap()
                .streams
                .values()
                .next()
                .unwrap()
                .clone();
            apply_one(&store, &stream).map(|v| v["state"].clone())
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!handle.is_finished(), "подготовка ждёт ExportLock");
    drop(store);
    // Держатель блокировки (как edit) после замены видит смену каталога.
    drop(lock);
    let state = handle.join().unwrap().unwrap();
    assert_eq!(state, "prepared");
    assert!(
        crate::write::ExportLock::acquire(&path)
            .unwrap()
            .check_source(&path.join("deck.json"), &original)
            .is_err()
    );
}

#[test]
fn nested_models_renamed_root_and_profiles_have_independent_addresses() {
    let tmp = TempDir::new("prepare-identities");
    let store = open(tmp.path());
    let nested = |name: &str, profile: &str, evidence: &str| {
        let mut value: Value = serde_json::from_str(NESTED_EXPORT).unwrap();
        value["name"] = json!(name);
        value["children"][0]["name"] = json!(format!("{name}::Child"));
        value["children"][0]["children"][0]["name"] = json!(format!("{name}::Child::Leaf"));
        let mut model = value["note_models"][0].clone();
        model["crowdanki_uuid"] = json!("model-2");
        model["name"] = json!("Вторая модель");
        value["note_models"].as_array_mut().unwrap().push(model);
        value["children"][0]["notes"][0]["note_model_uuid"] = json!("model-2");
        source::synthetic_for(
            profile,
            &serde_json::to_vec(&value).unwrap(),
            &BTreeMap::new(),
            None,
            evidence,
        )
    };
    let one = nested("Root", PROFILE, "job-1");
    let other = nested("Root", "other-profile", "job-2");
    store.publish(&one).unwrap();
    store.publish(&other).unwrap();
    apply(&store, &one).unwrap();
    apply(&store, &other).unwrap();
    let first = stream(&store, &one).output_directory;
    let second = stream(&store, &other).output_directory;
    assert_ne!(first, second);
    assert_eq!(
        tree(&tmp.path().join("decks/output").join(&first)),
        one.files().clone()
    );
    // Переименование корня при том же UUID обновляет тот же закреплённый адрес.
    let renamed = nested("Renamed", PROFILE, "job-3");
    store.publish(&renamed).unwrap();
    assert_eq!(apply(&store, &renamed).unwrap()["state"], "prepared");
    assert_eq!(stream(&store, &renamed).output_directory, first);
    assert_eq!(
        tree(&tmp.path().join("decks/output").join(&first)),
        renamed.files().clone()
    );
    assert_eq!(
        tree(&tmp.path().join("decks/output").join(&second)),
        other.files().clone()
    );
}

#[test]
fn large_media_is_streamed_and_verified_without_growing_history() {
    let tmp = TempDir::new("prepare-large");
    let store = open(tmp.path());
    let big: Vec<u8> = (0..24 * 1024 * 1024)
        .map(|i: u32| (i % 251) as u8)
        .collect();
    let mut files = media();
    files.insert("big.bin".into(), big.clone());
    let v1 = source::synthetic_for(PROFILE, &deck("v1"), &files, None, "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    assert_eq!(fs::read(path.join("media/big.bin")).unwrap(), big);
    let v2 = source::synthetic_for(PROFILE, &deck("v2"), &files, None, "job-2");
    store.publish(&v2).unwrap();
    apply(&store, &v2).unwrap();
    // Ровно одна рабочая копия: previous/stage после фиксации не остаются.
    assert!(work_names(tmp.path(), &v2).is_empty());
    assert_eq!(
        fs::read_dir(tmp.path().join("decks/output"))
            .unwrap()
            .count(),
        1
    );
}

// ---- Публикация: фиксация readback, журнал импорта и решения ----

/// V1 подготовлен, B отредактирован агентом; возвращает (store, v1, B-экспорт).
fn edited(tmp: &TempDir) -> (Store, VerifiedExport, PathBuf) {
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    fs::write(path.join("deck.json"), deck("agent V2")).unwrap();
    (store, v1, path)
}

fn journal(store: &Store, v1: &VerifiedExport, state: Option<&str>) -> ImportJournal {
    let permit = store
        .exchange()
        .plan_publication(v1.snapshot().identity())
        .unwrap();
    let s = stream(store, v1);
    ImportJournal {
        job_id: "1".repeat(64),
        identity: v1.snapshot().identity().clone(),
        profile: "Synthetic".into(),
        process: "process-1".into(),
        output_directory: s.output_directory.clone(),
        working_revision: permit.working_revision().into(),
        working_inventory: BTreeMap::new(),
        checked_input_sequence: permit.source_sequence(),
        checked_input_revision: s.current.snapshot.revision().into(),
        base_sequence: permit.base_sequence(),
        admission_started: true,
        import_state: state.map(str::to_owned),
        import_error: None,
        partial_changes_possible: Some(false),
    }
}

fn work_dir(store: &Store, v1: &VerifiedExport) -> Directory {
    let area = store.work_area(true).unwrap().unwrap();
    work::stream_dir(&area, &v1.snapshot().identity().key()).unwrap()
}

fn audit_consistent(store: &Store, key: &str) {
    let audit = store.audit().unwrap();
    let state = store.exchange().read().unwrap();
    let stream = audit["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["identity"] == json!(state.streams[key].current.snapshot.identity()))
        .unwrap()
        .clone();
    assert_eq!(
        stream["source_sequence"],
        state.streams[key].current.source_sequence
    );
}

#[test]
fn matching_readback_is_published_atomically_with_input() {
    let tmp = TempDir::new("publish-readback");
    let (store, v1, _) = edited(&tmp);
    let key = v1.snapshot().identity().key();
    let journal = journal(&store, &v1, Some("succeeded"));
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal).unwrap();
    assert!(store.publication_pending(&key).unwrap());
    let readback = export("agent V2", "readback-job");
    let sent = readback.snapshot().clone();
    let outcome = publication::conclude(&store, &dir, &journal, &readback, &sent).unwrap();
    assert_eq!(outcome["state"], "published");
    let state = store.exchange().read().unwrap();
    let s = &state.streams[&key];
    let published = s.published.as_ref().unwrap();
    assert_eq!(published.working_revision, journal.working_revision);
    assert_eq!(published.checked_input_sequence, 1);
    assert_eq!(published.confirmed_sequence, 2);
    assert_eq!(s.current.snapshot, *readback.snapshot());
    assert_eq!(s.base.as_ref().unwrap().source_sequence, 2);
    assert!(!store.publication_pending(&key).unwrap());
    audit_consistent(&store, &key);
    // Следующая подготовка видит keep: опубликованное B стало базой.
    assert_eq!(apply(&store, &v1).unwrap()["state"], "kept");
}

#[test]
fn readback_finalization_survives_every_input_boundary_without_false_publication() {
    for step in [
        "staged",
        "intent",
        "displaced",
        "visible",
        "permit",
        "recorded",
        "receipt",
    ] {
        let tmp = TempDir::new("publish-readback-crash");
        let (store, v1, _) = edited(&tmp);
        let key = v1.snapshot().identity().key();
        let journal = journal(&store, &v1, Some("succeeded"));
        let dir = work_dir(&store, &v1);
        work::save(&dir, PUBLISH, &journal).unwrap();
        let readback = export("agent V2", "readback-job");
        let sent = readback.snapshot().clone();
        store.fault.set(Some(step));
        assert!(publication::conclude(&store, &dir, &journal, &readback, &sent).is_err());
        // Пока intent не завершён, published не объявлен без нового current.
        let state = store.exchange().read().unwrap();
        if state.streams[&key].published.is_some() {
            assert_eq!(
                state.streams[&key].current.snapshot,
                *readback.snapshot(),
                "{step}"
            );
        }
        drop(state);
        drop(dir);
        drop(store);
        let store = open(tmp.path());
        store.recover().unwrap();
        audit_consistent(&store, &key);
        let published = store.exchange().read().unwrap().streams[&key]
            .published
            .is_some();
        if step == "staged" {
            // До intent ничего не зафиксировано: повтор получает новый readback.
            assert!(!published);
            assert_eq!(
                store.exchange().read().unwrap().streams[&key]
                    .current
                    .source_sequence,
                1
            );
            let dir = work_dir(&store, &v1);
            let again = export("agent V2", "readback-job-2");
            let outcome = publication::conclude(&store, &dir, &journal, &again, &sent).unwrap();
            assert_eq!(outcome["state"], "published");
        } else {
            assert!(published, "{step}");
            // Журнал импорта остаётся до явного повтора; повтор распознаёт подтверждение.
            assert!(
                publication::already_published(&store, &journal).unwrap(),
                "{step}"
            );
        }
        let state = store.exchange().read().unwrap();
        assert_eq!(state.streams[&key].current.source_sequence, 2, "{step}");
        assert_eq!(
            state.streams[&key].current.snapshot,
            *readback.snapshot(),
            "{step}"
        );
    }
}

#[test]
fn working_change_after_readback_records_observed_c_without_publication() {
    let tmp = TempDir::new("publish-working-changed");
    let (store, v1, path) = edited(&tmp);
    let key = v1.snapshot().identity().key();
    let journal = journal(&store, &v1, Some("succeeded"));
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal).unwrap();
    fs::write(path.join("deck.json"), deck("agent V2b")).unwrap();
    let readback = export("agent V2", "readback-job");
    let sent = readback.snapshot().clone();
    let outcome = publication::conclude(&store, &dir, &journal, &readback, &sent).unwrap();
    assert_eq!(outcome["state"], "import_applied_unconfirmed");
    let state = store.exchange().read().unwrap();
    assert!(state.streams[&key].published.is_none());
    assert_eq!(state.streams[&key].current.snapshot, *readback.snapshot());
    assert_eq!(
        state.streams[&key].base.as_ref().unwrap().source_sequence,
        1
    );
    assert_eq!(fs::read(path.join("deck.json")).unwrap(), deck("agent V2b"));
    audit_consistent(&store, &key);
}

#[test]
fn unchanged_or_divergent_readback_never_claims_publication() {
    // Отмена: Anki осталась C.
    let tmp = TempDir::new("publish-cancelled");
    let (store, v1, path) = edited(&tmp);
    let key = v1.snapshot().identity().key();
    let journal_value = journal(&store, &v1, Some("cancelled"));
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal_value).unwrap();
    let sent = export("agent V2", "sent").snapshot().clone();
    let readback = export("v1", "readback-cancelled");
    let outcome = publication::conclude(&store, &dir, &journal_value, &readback, &sent).unwrap();
    assert_eq!(outcome["state"], "import_not_applied");
    assert_eq!(outcome["reason"], "import_cancelled");
    let state = store.exchange().read().unwrap();
    assert!(state.streams[&key].published.is_none());
    assert_eq!(state.streams[&key].current.source_sequence, 2);
    assert_eq!(fs::read(path.join("deck.json")).unwrap(), deck("agent V2"));
    drop(state);
    // Тот же B снова разрешён к публикации новой явной попыткой.
    assert!(
        store
            .exchange()
            .plan_publication(v1.snapshot().identity())
            .is_ok()
    );

    // Удаление заметки в B: CrowdAnki её не удаляет, readback содержит лишнюю заметку.
    let tmp = TempDir::new("publish-delete");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    apply(&store, &v1).unwrap();
    let path = output(tmp.path(), &stream(&store, &v1));
    let mut without: Value = serde_json::from_slice(&deck("v1")).unwrap();
    without["notes"].as_array_mut().unwrap().remove(1);
    without["notes"][0]["fields"][1] = json!("agent V2");
    fs::write(
        path.join("deck.json"),
        serde_json::to_vec(&without).unwrap(),
    )
    .unwrap();
    let journal_value = journal(&store, &v1, Some("succeeded"));
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal_value).unwrap();
    let sent = source::synthetic_for(
        PROFILE,
        &serde_json::to_vec(&without).unwrap(),
        &media(),
        None,
        "sent",
    )
    .snapshot()
    .clone();
    // Anki применил правку поля, но удалённая в JSON заметка осталась.
    let readback = export("agent V2", "readback-delete");
    let outcome = publication::conclude(&store, &dir, &journal_value, &readback, &sent).unwrap();
    assert_eq!(outcome["state"], "readback_mismatch");
    assert!(
        outcome["differences"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"].as_str().unwrap().contains("guid-2"))
    );
    let state = store.exchange().read().unwrap();
    let key = v1.snapshot().identity().key();
    assert!(state.streams[&key].published.is_none());
    assert_eq!(state.streams[&key].current.snapshot, *readback.snapshot());
    assert_eq!(
        fs::read(path.join("deck.json")).unwrap(),
        serde_json::to_vec(&without).unwrap()
    );
}

struct Fake {
    statuses: RefCell<VecDeque<crate::desktop::Result<Value>>>,
    submit: RefCell<Option<crate::desktop::Result<Value>>>,
    submits: Cell<u32>,
}

impl Fake {
    fn new(
        statuses: Vec<crate::desktop::Result<Value>>,
        submit: crate::desktop::Result<Value>,
    ) -> Self {
        Self {
            statuses: RefCell::new(statuses.into()),
            submit: RefCell::new(Some(submit)),
            submits: Cell::new(0),
        }
    }
}

impl Bridge for Fake {
    fn status(&self, _id: &str) -> crate::desktop::Result<Value> {
        let mut statuses = self.statuses.borrow_mut();
        if statuses.len() > 1 {
            statuses.pop_front().unwrap()
        } else {
            match statuses.front().unwrap() {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(crate::desktop::external_error(
                    e.details["reason"].as_str().unwrap_or_default(),
                    e.message.clone(),
                )),
            }
        }
    }
    fn submit(&self, _id: &str) -> crate::desktop::Result<Value> {
        self.submits.set(self.submits.get() + 1);
        self.submit
            .borrow_mut()
            .take()
            .expect("единственная отправка")
    }
    fn renew(&self, _journal: &ImportJournal) -> crate::desktop::Result<()> {
        Ok(())
    }
}

fn job(state: &str, process: &str) -> crate::desktop::Result<Value> {
    Ok(
        json!({"state":state,"job":{"id":"1".repeat(64),"action":"import","state":state,
        "observer_process":process,"process":process}}),
    )
}

fn refused(state: Option<&str>, reason: &str, message: &str) -> crate::desktop::Result<Value> {
    let mut e = crate::desktop::external_error(reason, message);
    if let Some(state) = state {
        e.details["submission_state"] = json!(state);
    }
    Err(e)
}

fn drive_case(
    statuses: Vec<crate::desktop::Result<Value>>,
    submit: crate::desktop::Result<Value>,
    admission_started: bool,
) -> (Progress, ImportJournal, u32, ImportJournal) {
    let tmp = TempDir::new("publish-drive");
    let (store, v1, _) = edited(&tmp);
    let mut journal = journal(&store, &v1, None);
    journal.admission_started = admission_started;
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal).unwrap();
    let fake = Fake::new(statuses, submit);
    let progress = publication::drive(
        &fake,
        &dir,
        &mut journal,
        std::time::Duration::from_millis(900),
    )
    .unwrap();
    let persisted = work::load::<ImportJournal>(&dir, PUBLISH).unwrap().unwrap();
    (progress, journal, fake.submits.get(), persisted)
}

#[test]
fn import_job_outcomes_are_distinct_and_never_resubmitted_blindly() {
    // Штатный путь: отправка после надёжного признака, затем succeeded.
    let (progress, _, submits, persisted) = drive_case(
        vec![
            job("prepared", "process-1"),
            job("running", "process-1"),
            job("succeeded", "process-1"),
        ],
        Ok(json!({"state":"queued"})),
        false,
    );
    assert!(matches!(progress, Progress::Terminal));
    assert_eq!(submits, 1);
    assert!(persisted.admission_started);
    assert_eq!(persisted.import_state.as_deref(), Some("succeeded"));

    // Отмена GUI и ошибка импортера — завершённые, но не успешные исходы.
    for state in ["cancelled", "failed", "interrupted"] {
        let (progress, _, submits, persisted) =
            drive_case(vec![job(state, "process-1")], Ok(json!({})), true);
        assert!(matches!(progress, Progress::Terminal));
        assert_eq!(submits, 0);
        assert_eq!(persisted.import_state.as_deref(), Some(state));
    }

    // Обрыв после возможного admission: тот же ID сохраняется, повтора нет.
    let (progress, _, _, persisted) = drive_case(
        vec![job("prepared", "process-1")],
        refused(Some("unknown"), "http_unreachable", "timeout"),
        false,
    );
    assert!(matches!(
        progress,
        Progress::Pending("import_submission_unknown", _)
    ));
    assert!(persisted.admission_started);

    // Доказанный отказ до admission закрывает попытку без импорта.
    for (state, reason, message) in [
        (Some("rejected"), "api_error", "anki_busy"),
        (None, "desktop_io", "до HTTP"),
    ] {
        let (progress, _, _, _) = drive_case(
            vec![job("prepared", "process-1")],
            refused(state, reason, message),
            false,
        );
        assert!(matches!(progress, Progress::NotSubmitted(_)), "{message}");
    }
    // Тот же отказ после прежней неопределённой отправки её не отменяет.
    let (progress, _, _, _) = drive_case(
        vec![job("prepared", "process-1")],
        refused(Some("rejected"), "api_error", "anki_busy"),
        true,
    );
    assert!(matches!(
        progress,
        Progress::Pending("import_submission_unknown", _)
    ));

    // Рестарт Anki: без начатой отправки — доказанно не отправлено; с ней — readback решает.
    let (progress, _, submits, _) =
        drive_case(vec![job("prepared", "process-2")], Ok(json!({})), false);
    assert!(matches!(progress, Progress::NotSubmitted(_)));
    assert_eq!(submits, 0);
    let (progress, _, submits, persisted) =
        drive_case(vec![job("prepared", "process-2")], Ok(json!({})), true);
    assert!(matches!(progress, Progress::Terminal));
    assert_eq!(submits, 0);
    assert_eq!(
        persisted.import_state.as_deref(),
        Some("unknown_after_restart")
    );

    // Недоступный API и сохранённый result.json без живого ответа не дают исхода.
    let (progress, _, _, _) = drive_case(
        vec![refused(None, "http_unreachable", "нет связи")],
        Ok(json!({})),
        true,
    );
    assert!(matches!(
        progress,
        Progress::Pending("import_status_unknown", _)
    ));
    let (progress, _, _, _) = drive_case(
        vec![Ok(
            json!({"state":"succeeded","live_verified":false,"job":{}}),
        )],
        Ok(json!({})),
        true,
    );
    assert!(matches!(
        progress,
        Progress::Pending("import_status_unknown", _)
    ));

    // Диалог ждёт пользователя: журнал сохранён, повторной отправки нет.
    let (progress, _, submits, persisted) =
        drive_case(vec![job("running", "process-1")], Ok(json!({})), true);
    assert!(matches!(
        progress,
        Progress::Pending("import_awaiting_user", _)
    ));
    assert_eq!(submits, 0);
    assert!(persisted.import_state.is_none());

    // Чужое задание не принимается как исход импорта.
    let (progress, _, _, _) = drive_case(
        vec![Ok(
            json!({"state":"succeeded","job":{"id":"2".repeat(64),"action":"import"}}),
        )],
        Ok(json!({})),
        true,
    );
    assert!(matches!(
        progress,
        Progress::Pending("import_evidence_mismatch", _)
    ));
}

#[test]
fn pending_publication_blocks_preparation_and_profile_id_bound_journal_round_trips() {
    let tmp = TempDir::new("publish-pending");
    let (store, v1, _) = edited(&tmp);
    let journal = journal(&store, &v1, None);
    let dir = work_dir(&store, &v1);
    work::save(&dir, PUBLISH, &journal).unwrap();
    assert_eq!(
        work::load::<ImportJournal>(&dir, PUBLISH).unwrap().unwrap(),
        journal
    );
    assert_eq!(
        apply(&store, &v1).unwrap_err().to_string(),
        "операция заблокирована: publication_pending"
    );
    // Повреждённый журнал не принимается и не удаляется.
    let path = tmp
        .path()
        .join("decks/.roundtrip")
        .join(v1.snapshot().identity().key())
        .join(PUBLISH);
    let mut bytes = fs::read(&path).unwrap();
    bytes[20] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(work::load::<ImportJournal>(&dir, PUBLISH).is_err());
    assert!(path.exists());
    let _ = ProfileId::new(PROFILE).unwrap();
}

/// Пользовательские точки входа без Anki: план/применение/состояние и план импорта.
#[test]
fn command_entry_points_report_per_deck_outcomes_without_anki() {
    let tmp = TempDir::new("roundtrip-commands");
    let store = open(tmp.path());
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    let name = stream(&store, &v1).output_directory;
    drop(store);
    let root = tmp.path();
    let names = [name.clone(), "missing".into()];

    let plan = run(root, &names, false, false);
    assert_eq!(plan["state"], "prepare_plan");
    assert_eq!(plan["decks"][0]["action"], "prepare_first");
    assert_eq!(plan["decks"][1]["reason"], "output_not_registered");
    assert!(!root.join("decks/output").exists());

    let applied = run(root, &names, false, true);
    assert_eq!(
        applied["state"], "partial",
        "отсутствующая привязка не скрыта"
    );
    assert_eq!(applied["decks"][0]["state"], "prepared");
    let all = run(root, &[], true, true);
    assert_eq!(all["state"], "succeeded");
    assert_eq!(all["decks"][0]["state"], "kept");

    let observed = status(root);
    assert_eq!(observed["state"], "succeeded");
    assert_eq!(observed["decks"][0]["action"], "keep");
    assert_eq!(observed["decks"][0]["pending"], json!([]));

    // План импорта: нечего публиковать, затем разрешение после правки B.
    let prefixed = [format!("decks/output/{name}")];
    let nothing = publication::plan(root, &prefixed);
    assert_eq!(nothing["decks"][0]["state"], "nothing_to_publish");
    fs::write(
        root.join("decks/output").join(&name).join("deck.json"),
        deck("agent"),
    )
    .unwrap();
    let planned = publication::plan(root, &prefixed);
    assert_eq!(planned["state"], "publish_plan");
    assert_eq!(
        planned["decks"][0]["comparison"]["disposition"],
        "working_only"
    );
    let text = crate::desktop::render_text(&planned);
    assert!(
        text.contains("Подтвердить штатный диалог импорта"),
        "{text}"
    );
    assert!(!text.contains("null"), "{text}");

    // Изменились обе стороны: публикация запрещена до согласования.
    let store = open(root);
    store.publish(&export("v2", "job-2")).unwrap();
    drop(store);
    let blocked = publication::plan(root, &prefixed);
    assert_eq!(blocked["state"], "publish_plan_blocked");
    assert_eq!(blocked["decks"][0]["reason"], "publication_blocked");
    let refused = run(root, &[name], false, true);
    assert_eq!(refused["decks"][0]["state"], "blocked");
    assert_eq!(refused["decks"][0]["reason"], "stale_or_unproven_base");

    // Без доверенного состояния — отказ без создания каталогов.
    let empty = TempDir::new("roundtrip-empty");
    fs::create_dir_all(empty.path().join(".git")).unwrap();
    assert_eq!(status(empty.path())["state"], "failed");
    assert_eq!(
        publication::plan(empty.path(), &prefixed)["state"],
        "failed"
    );
    assert!(!empty.path().join("decks").exists());
}
