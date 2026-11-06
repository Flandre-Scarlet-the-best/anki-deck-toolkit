use super::super::source;
use super::*;
use crate::{
    exchange::ProfileId,
    test_support::{MINIMAL_EXPORT, TempDir},
};
use std::fs;

fn export(version: &str, evidence: &str) -> VerifiedExport {
    let mut value: Value = serde_json::from_str(MINIMAL_EXPORT).unwrap();
    value["notes"][0]["fields"][0] = json!(version);
    source::synthetic(
        &serde_json::to_vec(&value).unwrap(),
        &BTreeMap::from([
            ("a.mp3".into(), b"audio".to_vec()),
            ("b.png".into(), b"image".to_vec()),
        ]),
        evidence,
    )
}

fn key_dir(store: &Store, export: &VerifiedExport) -> Directory {
    store
        .input
        .child(&export.snapshot().identity().key())
        .unwrap()
        .unwrap()
}

fn receipt(dir: &Directory) -> Transaction {
    read_transaction(dir, "receipt.json").unwrap()
}

#[test]
fn complete_generations_sequences_and_same_job_replay() {
    let tmp = TempDir::new("trusted-input-generations");
    let v1 = export("v1", "live-job-1");
    let store = Store::open(tmp.path()).unwrap();
    let one = store.publish(&v1).unwrap();
    assert_eq!(one["source_sequence"], 1);
    assert_eq!(store.publish(&v1).unwrap()["source_sequence"], 1);
    let repeated = export("v1", "live-job-2");
    let dir = key_dir(&store, &v1);
    use std::os::unix::fs::MetadataExt;
    let path = tmp
        .path()
        .join("decks/input")
        .join(v1.snapshot().identity().key())
        .join("current/deck.json");
    let inode = fs::metadata(&path).unwrap().ino();
    let media_path = path.parent().unwrap().join("media/a.mp3");
    let media_inode = fs::metadata(&media_path).unwrap().ino();
    let two = store.publish(&repeated).unwrap();
    assert_eq!(two["source_sequence"], 2);
    assert_eq!(two["content_changed"], false);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::metadata(&media_path).unwrap().ino(), media_inode);
    assert_eq!(one["revision"], two["revision"]);
    let v2 = export("v2", "live-job-3");
    let three = store.publish(&v2).unwrap();
    assert_eq!(three["source_sequence"], 3);
    assert_ne!(three["revision"], one["revision"]);
    assert_eq!(dir.names().unwrap(), ["current", "receipt.json"]);
    let state = store.exchange.read().unwrap();
    let stream = &state.streams[&v1.snapshot().identity().key()];
    assert!(stream.base.is_none());
    assert!(stream.published.is_none());
    assert!(!tmp.path().join("decks/output").exists());
}

fn prepare_intent(store: &Store, export: &VerifiedExport) -> (Directory, Transaction) {
    let key = export.snapshot().identity().key();
    let (dir, _) = store.input.ensure_child(&key).unwrap();
    let previous = store
        .exchange
        .read()
        .unwrap()
        .streams
        .get(&key)
        .map(|s| s.current.clone());
    let sequence = previous.as_ref().map_or(1, |p| p.source_sequence + 1);
    let (stage, _) = dir.ensure_child("stage").unwrap();
    for (name, bytes) in export.files() {
        if let Some(name) = name.strip_prefix("media/") {
            let (media, _) = stage.ensure_child("media").unwrap();
            media.publish(name, bytes).unwrap();
        } else {
            stage.publish(name, bytes).unwrap();
        }
    }
    let tx = Transaction {
        snapshot: export.snapshot().clone(),
        sequence,
        evidence: export.evidence().into(),
        inventory: export.inventory().clone(),
        previous,
        unchanged: false,
    };
    dir.publish("transaction.json", &encode_transaction(&tx).unwrap())
        .unwrap();
    (dir, tx)
}

#[test]
fn every_directory_and_exchange_crash_window_recovers_exactly_once() {
    for window in 0..5 {
        let tmp = TempDir::new("trusted-input-crash");
        let store = Store::open(tmp.path()).unwrap();
        store.publish(&export("v1", "first-live-job")).unwrap();
        let candidate = export("v2", "next-live-job");
        let (dir, tx) = prepare_intent(&store, &candidate);
        if window >= 1 {
            dir.rename_child("current", "previous").unwrap();
        }
        if window >= 2 {
            dir.rename_child("stage", "current").unwrap();
        }
        if window >= 3 {
            let state = store.exchange.read().unwrap();
            store
                .exchange
                .record_input(
                    &state.token,
                    Confirmation::from_trusted_owner(
                        tx.snapshot.clone(),
                        tx.sequence,
                        &tx.evidence,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        if window >= 4 {
            dir.remove_tree("previous").unwrap();
        }
        drop(store);
        let reopened = Store::open(tmp.path()).unwrap();
        assert_eq!(reopened.recover().unwrap().len(), 1, "window {window}");
        assert!(reopened.recover().unwrap().is_empty());
        let dir = key_dir(&reopened, &candidate);
        let tx = receipt(&dir);
        assert_eq!(tx.sequence, 2);
        assert_eq!(tx.snapshot, *candidate.snapshot());
        assert_eq!(dir.names().unwrap(), ["current", "receipt.json"]);
        assert_eq!(reopened.publish(&candidate).unwrap()["source_sequence"], 2);
    }
}

#[test]
fn corrupted_staging_and_cas_conflict_never_confirm_candidate() {
    for cas in [false, true] {
        let tmp = TempDir::new("trusted-input-refusal");
        let store = Store::open(tmp.path()).unwrap();
        let v1 = export("v1", "live-job-1");
        store.publish(&v1).unwrap();
        let v2 = export("v2", "live-job-2");
        let (dir, _) = prepare_intent(&store, &v2);
        if cas {
            let state = store.exchange.read().unwrap();
            store
                .exchange
                .record_input(
                    &state.token,
                    Confirmation::from_trusted_owner(
                        v1.snapshot().clone(),
                        3,
                        "independent-writer",
                    )
                    .unwrap(),
                )
                .unwrap();
        } else {
            dir.child("stage")
                .unwrap()
                .unwrap()
                .publish("deck.json", b"{}")
                .unwrap();
        }
        assert!(store.recover().is_err());
        let current = dir.child("current").unwrap().unwrap();
        assert_eq!(
            read_export(&current, ProfileId::new("synthetic-profile").unwrap())
                .unwrap()
                .0,
            *v1.snapshot()
        );
        assert_ne!(
            store.exchange.read().unwrap().streams[&v1.snapshot().identity().key()]
                .current
                .snapshot,
            *v2.snapshot()
        );
        assert!(dir.names().unwrap().contains(&"transaction.json".into()));
    }
}

#[test]
fn output_and_base_survive_independent_working_change() {
    let tmp = TempDir::new("trusted-input-output");
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    let key = v1.snapshot().identity().key();
    let state = store.exchange.read().unwrap();
    let output = tmp
        .path()
        .join("decks/output")
        .join(&state.streams[&key].output_directory);
    let plan = store
        .exchange
        .plan_preparation(v1.snapshot().identity())
        .unwrap();
    assert_eq!(
        plan.action(),
        crate::exchange::PreparationAction::PrepareFirst
    );
    fs::create_dir_all(&output).unwrap();
    for (name, bytes) in v1.files() {
        let target = output.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }
    store.exchange.record_prepared(&plan).unwrap();
    let base = store.exchange.read().unwrap().streams[&key].base.clone();
    let working = b"independent-user-working-bytes";
    fs::write(output.join("deck.json"), working).unwrap();
    store.publish(&export("v2", "job-2")).unwrap();
    store.publish(&export("v3", "job-3")).unwrap();
    assert_eq!(fs::read(output.join("deck.json")).unwrap(), working);
    assert_eq!(store.exchange.read().unwrap().streams[&key].base, base);
}

#[test]
fn renamed_deck_keeps_identity_directory_and_output_binding() {
    let tmp = TempDir::new("trusted-input-rename");
    let store = Store::open(tmp.path()).unwrap();
    let v1 = export("v1", "job-1");
    store.publish(&v1).unwrap();
    let key = v1.snapshot().identity().key();
    let original_binding = store.exchange.read().unwrap().streams[&key]
        .output_directory
        .clone();
    let mut value: Value = serde_json::from_slice(&v1.files()["deck.json"]).unwrap();
    value["name"] = json!("Renamed");
    let renamed = source::synthetic(
        &serde_json::to_vec(&value).unwrap(),
        &BTreeMap::from([
            ("a.mp3".into(), b"audio".to_vec()),
            ("b.png".into(), b"image".to_vec()),
        ]),
        "job-2",
    );
    store.publish(&renamed).unwrap();
    assert_eq!(store.exchange.read().unwrap().streams.len(), 1);
    assert_eq!(
        store.exchange.read().unwrap().streams[&key].output_directory,
        original_binding
    );
    assert_eq!(
        key_dir(&store, &v1).names().unwrap(),
        ["current", "receipt.json"]
    );
}

#[test]
fn source_rejects_links_special_entries_normalization_collision_and_missing_media() {
    use std::os::unix::fs::symlink;
    for kind in ["symlink", "hardlink", "directory", "collision", "missing"] {
        let tmp = TempDir::new("trusted-input-files");
        let export_dir = tmp.path().join("source");
        fs::create_dir_all(export_dir.join("media")).unwrap();
        let mut value: Value = serde_json::from_str(MINIMAL_EXPORT).unwrap();
        value["media_files"] = json!(["image.png"]);
        fs::write(
            export_dir.join("deck.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let media = export_dir.join("media/image.png");
        match kind {
            "symlink" => {
                fs::write(tmp.path().join("outside"), b"private").unwrap();
                symlink(tmp.path().join("outside"), &media).unwrap();
            }
            "hardlink" => {
                fs::write(tmp.path().join("outside"), b"private").unwrap();
                fs::hard_link(tmp.path().join("outside"), &media).unwrap();
            }
            "directory" => fs::create_dir(&media).unwrap(),
            "collision" => {
                fs::write(&media, b"a").unwrap();
                fs::write(export_dir.join("media/IMAGE.png"), b"b").unwrap();
            }
            _ => {}
        }
        let directory = Directory::open(&export_dir).unwrap();
        assert!(
            read_export(&directory, ProfileId::new("test").unwrap()).is_err(),
            "{kind}"
        );
    }
}

#[test]
fn input_symlink_and_unknown_owner_are_never_adopted() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new("trusted-input-owner");
    fs::create_dir_all(tmp.path().join("decks")).unwrap();
    let outside = TempDir::new("trusted-input-outside");
    let before: Vec<_> = fs::read_dir(outside.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    symlink(outside.path(), tmp.path().join("decks/input")).unwrap();
    assert!(Store::open(tmp.path()).is_err());
    let after: Vec<_> = fs::read_dir(outside.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn io_errors_at_real_publication_boundaries_preserve_recoverable_state() {
    for step in [
        "staged",
        "intent",
        "displaced",
        "visible",
        "recorded",
        "receipt",
    ] {
        let tmp = TempDir::new("trusted-input-io-boundaries");
        let store = Store::open(tmp.path()).unwrap();
        let first = export("v1", "first-job");
        let next = export("v2", "next-job");
        store.publish(&first).unwrap();
        store.fault.set(Some(step));
        assert!(
            matches!(store.publish(&next), Err(ExchangeError::Io(_))),
            "{step}"
        );
        drop(store);
        let store = Store::open(tmp.path()).unwrap();
        store.recover().unwrap();
        let state = store.exchange.read().unwrap();
        let current = &state.streams[&first.snapshot().identity().key()].current;
        if step == "staged" {
            assert_eq!(current.snapshot, *first.snapshot());
            assert_eq!(current.source_sequence, 1);
        } else {
            assert_eq!(current.snapshot, *next.snapshot());
            assert_eq!(current.source_sequence, 2);
        }
        store.audit().unwrap();
        assert_eq!(store.publish(&next).unwrap()["source_sequence"], 2);
    }
}

#[test]
fn concurrent_owners_serialize_sequences_across_restart() {
    let tmp = TempDir::new("trusted-input-concurrent");
    let root = tmp.path().to_path_buf();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let store = Store::open(&root).unwrap();
                store.recover().unwrap();
                store
                    .publish(&export("same-content", &format!("live-job-{i}")))
                    .unwrap()["source_sequence"]
                    .as_u64()
                    .unwrap()
            })
        })
        .collect();
    let mut sequences: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    sequences.sort();
    assert_eq!(sequences, [1, 2, 3, 4]);
    let store = Store::open(&root).unwrap();
    assert_eq!(store.audit().unwrap()["streams"][0]["source_sequence"], 4);
}

#[test]
fn accidental_journal_corruption_never_becomes_a_confirmation() {
    let tmp = TempDir::new("trusted-input-corrupt-intent");
    let store = Store::open(tmp.path()).unwrap();
    let first = export("v1", "first-job");
    store.publish(&first).unwrap();
    let candidate = export("v2", "next-job");
    let (dir, _) = prepare_intent(&store, &candidate);
    let mut envelope: Value =
        serde_json::from_slice(&dir.read("transaction.json").unwrap()).unwrap();
    envelope["transaction"]["evidence"] = json!("altered-evidence");
    dir.publish("transaction.json", &serde_json::to_vec(&envelope).unwrap())
        .unwrap();
    assert!(store.recover().is_err());
    assert_eq!(
        store.exchange.read().unwrap().streams[&first.snapshot().identity().key()]
            .current
            .source_sequence,
        1
    );
}

#[test]
fn corrupted_pending_index_cannot_trigger_a_second_export() {
    let tmp = TempDir::new("trusted-input-corrupt-pending");
    let mut store = Store::open(tmp.path()).unwrap();
    let job = PendingJob {
        id: "a".repeat(64),
        profile_id: "b".repeat(64),
        profile: "Synthetic".into(),
        deck_id: "1".into(),
        deck: "Deck".into(),
        deck_uuid: None,
        process: "process".into(),
    };
    let key = job.key();
    store.save_job(&key, job).unwrap();
    let job = store.control.jobs.remove(&key).unwrap();
    store.control.jobs.insert("c".repeat(64), job);
    store.save_control().unwrap();
    drop(store);
    assert!(matches!(
        Store::open(tmp.path()),
        Err(ExchangeError::CorruptState(_))
    ));
}
