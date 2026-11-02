use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::batch_runtime::{
    REFERENCED_BLOB_READS, RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime, create_log_file,
};
use crate::error::{AssetError, ErrorCode};
use crate::temp_workspace::TempWorkspace;

struct TemporaryDirectory {
    _workspace: TempWorkspace,
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Self {
        let workspace = TempWorkspace::create("asset-store-generic-batch-tests").unwrap();
        let path = workspace.path().join("fixture");
        fs::create_dir(&path).unwrap();
        Self {
            _workspace: workspace,
            path,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlainTextBatch {
    schema_version: u32,
    batch_id: String,
    revision: u64,
    description: String,
    blob: Option<RuntimeBlobRef>,
}

impl RuntimeBatchState for PlainTextBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != 1 || self.description.trim().is_empty() {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "состояние тестовой текстовой партии недопустимо",
            ));
        }
        Ok(())
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.blob.iter().cloned().collect()
    }
}

#[test]
fn generic_runtime_resumes_non_image_state_and_exact_blob_bytes() {
    let temporary = TemporaryDirectory::new();
    let bytes = "обычный текст, не изображение".as_bytes();
    let state = {
        let mut runtime = SafeBatchRuntime::open(temporary.path(), "plain-text").unwrap();
        assert!(runtime.load::<PlainTextBatch>().unwrap().is_none());
        let blob = runtime.persist_blob(bytes, "bin").unwrap();
        let state = PlainTextBatch {
            schema_version: 1,
            batch_id: "plain-text".into(),
            revision: 1,
            description: "состояние предметной партии без семантики изображений".into(),
            blob: Some(blob),
        };
        runtime.save(&state).unwrap();
        state
    };

    let mut runtime = SafeBatchRuntime::open(temporary.path(), "plain-text").unwrap();
    let resumed = runtime.load::<PlainTextBatch>().unwrap().unwrap();
    assert_eq!(resumed, state);
    assert_eq!(
        runtime.read_blob(resumed.blob.as_ref().unwrap()).unwrap(),
        bytes
    );
}

#[test]
fn unlocking_invalidates_stale_save_and_reload_preserves_verified_blob_cache() {
    let temporary = TemporaryDirectory::new();
    let mut runtime_a = SafeBatchRuntime::open(temporary.path(), "shared-state").unwrap();
    let blob = runtime_a.persist_blob(b"immutable", "bin").unwrap();
    let initial = PlainTextBatch {
        schema_version: 1,
        batch_id: "shared-state".into(),
        revision: 1,
        description: "исходное состояние".into(),
        blob: Some(blob),
    };
    runtime_a.save(&initial).unwrap();
    let mut stale = runtime_a.load::<PlainTextBatch>().unwrap().unwrap();
    runtime_a.release_lock().unwrap();

    let updated = {
        let mut runtime_b = SafeBatchRuntime::open(temporary.path(), "shared-state").unwrap();
        let mut updated = runtime_b.load::<PlainTextBatch>().unwrap().unwrap();
        updated.revision = 2;
        updated.description = "сохранено другим владельцем".into();
        runtime_b.save(&updated).unwrap();
        updated
    };

    runtime_a.reacquire_lock().unwrap();
    // Даже большая revision stale snapshot не даёт права перезаписать чужое состояние.
    stale.revision = 10;
    let error = runtime_a.save(&stale).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    let saved: PlainTextBatch = serde_json::from_slice(
        &fs::read(
            temporary
                .path()
                .join(".runtime/batches/shared-state/state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved, updated);

    REFERENCED_BLOB_READS.with(|reads| reads.set(0));
    let mut reloaded = runtime_a
        .reload_cached::<PlainTextBatch>()
        .unwrap()
        .unwrap();
    assert_eq!(reloaded, updated);
    REFERENCED_BLOB_READS.with(|reads| assert_eq!(reads.get(), 0));
    reloaded.revision += 1;
    reloaded.description = "изменено после перечитывания".into();
    runtime_a.save(&reloaded).unwrap();
    REFERENCED_BLOB_READS.with(|reads| assert_eq!(reads.get(), 0));
    assert_eq!(
        runtime_a.load::<PlainTextBatch>().unwrap().unwrap(),
        reloaded
    );
}

#[test]
fn cached_reload_validates_new_blob_after_another_owner_changes_revision() {
    let temporary = TemporaryDirectory::new();
    let mut first = SafeBatchRuntime::open(temporary.path(), "new-blob").unwrap();
    let old = first.persist_blob(b"old", "bin").unwrap();
    let state = PlainTextBatch {
        schema_version: 1,
        batch_id: "new-blob".into(),
        revision: 1,
        description: "old".into(),
        blob: Some(old),
    };
    first.save(&state).unwrap();
    first.load::<PlainTextBatch>().unwrap();
    first.release_lock().unwrap();
    let new_blob = {
        let mut second = SafeBatchRuntime::open(temporary.path(), "new-blob").unwrap();
        let mut next = second.load::<PlainTextBatch>().unwrap().unwrap();
        let blob = second.persist_blob(b"new", "bin").unwrap();
        next.blob = Some(blob.clone());
        next.revision += 1;
        second.save(&next).unwrap();
        blob
    };
    let file = temporary
        .path()
        .join(".runtime/batches/new-blob")
        .join(&new_blob.storage_path);
    fs::write(&file, b"changed").unwrap();
    first.reacquire_lock().unwrap();
    assert_eq!(
        first.reload_cached::<PlainTextBatch>().unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );
}

#[test]
fn relocked_empty_runtime_also_requires_a_new_load() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "empty-state").unwrap();
    assert!(runtime.load::<PlainTextBatch>().unwrap().is_none());
    let state = PlainTextBatch {
        schema_version: 1,
        batch_id: "empty-state".into(),
        revision: 1,
        description: "новое состояние".into(),
        blob: None,
    };
    runtime.release_lock().unwrap();
    runtime.reacquire_lock().unwrap();
    assert_eq!(
        runtime.save(&state).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert!(runtime.load::<PlainTextBatch>().unwrap().is_none());
    runtime.save(&state).unwrap();
}

#[test]
fn run_logs_are_unique_and_writer_survives_unlock_and_runtime_drop() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "diagnostic").unwrap();
    let mut first = runtime.create_run_log().unwrap();
    first.file.write_all(b"{\"event\":\"started\"}\n").unwrap();
    let second = runtime.create_run_log().unwrap();
    assert_ne!(first.run_id, second.run_id);
    assert_ne!(first.path, second.path);
    assert!(first.path.is_absolute());
    assert_eq!(
        first.path,
        temporary
            .path()
            .join(".runtime/batches/diagnostic/logs")
            .join(format!("{}.jsonl", first.run_id))
    );
    runtime.release_lock().unwrap();
    first.file.write_all(b"{\"event\":\"waiting\"}\n").unwrap();
    drop(runtime);
    first.file.write_all(b"{\"event\":\"finished\"}\n").unwrap();
    first.file.sync_all().unwrap();
    assert_eq!(
        fs::read_to_string(&first.path).unwrap(),
        "{\"event\":\"started\"}\n{\"event\":\"waiting\"}\n{\"event\":\"finished\"}\n"
    );
    assert!(fs::read(second.path).unwrap().is_empty());
}

#[test]
fn run_log_collision_never_overwrites_existing_files_or_follows_symlinks() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let directory = fs::File::open(temporary.path()).unwrap();
    let mut writer = create_log_file(&directory, "run.jsonl").unwrap().unwrap();
    writer.write_all(b"original\n").unwrap();
    drop(writer);
    assert!(create_log_file(&directory, "run.jsonl").unwrap().is_none());
    assert_eq!(
        fs::read(temporary.path().join("run.jsonl")).unwrap(),
        b"original\n"
    );

    fs::write(temporary.path().join("outside"), b"untouched").unwrap();
    symlink("outside", temporary.path().join("linked.jsonl")).unwrap();
    assert!(
        create_log_file(&directory, "linked.jsonl")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read(temporary.path().join("outside")).unwrap(),
        b"untouched"
    );
    assert_eq!(
        create_log_file(&directory, "../escape.jsonl")
            .unwrap_err()
            .code,
        ErrorCode::PathTraversal
    );
}

#[test]
fn run_log_creation_rejects_symlinked_logs_directory() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let outside = TemporaryDirectory::new();
    let runtime = SafeBatchRuntime::open(temporary.path(), "linked-logs").unwrap();
    symlink(
        outside.path(),
        temporary.path().join(".runtime/batches/linked-logs/logs"),
    )
    .unwrap();
    assert_eq!(
        runtime.create_run_log().unwrap_err().code,
        ErrorCode::BoundaryViolation
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
