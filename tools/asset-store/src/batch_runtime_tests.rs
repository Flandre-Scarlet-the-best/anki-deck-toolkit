use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::batch_runtime::{
    FS_OPERATIONS, FaultPoint, FsOperationCounts, INJECTED_FAULTS, REFERENCED_BLOB_READS,
    RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime, create_log_file,
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
    /// Не сериализуется: делает состояние недопустимым при тех же байтах.
    #[serde(skip)]
    poisoned: bool,
    /// Не сериализуется: меняет контекст проверки blob при тех же байтах.
    #[serde(skip)]
    blob_context: Option<String>,
}

impl PlainTextBatch {
    /// Базовое значение для `..` в литералах: несериализуемые флаги выключены.
    fn default_flags() -> Self {
        Self {
            schema_version: 1,
            batch_id: String::new(),
            revision: 0,
            description: String::new(),
            blob: None,
            poisoned: false,
            blob_context: None,
        }
    }

    fn new(batch_id: &str, revision: u64, description: &str) -> Self {
        Self {
            batch_id: batch_id.into(),
            revision,
            description: description.into(),
            ..Self::default_flags()
        }
    }
}

impl RuntimeBatchState for PlainTextBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != 1 || self.description.trim().is_empty() || self.poisoned {
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

    fn blob_validation_context(&self, _blob: &RuntimeBlobRef) -> Option<&str> {
        self.blob_context.as_deref()
    }

    fn validate_blob_bytes(&self, _blob: &RuntimeBlobRef, _bytes: &[u8]) -> Result<(), AssetError> {
        if self.blob_context.as_deref() == Some("reject") {
            return Err(AssetError::new(
                ErrorCode::ValidatorFailure,
                "тестовый контекст отвергает содержимое blob",
            ));
        }
        Ok(())
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
            ..PlainTextBatch::default_flags()
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
        ..PlainTextBatch::default_flags()
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
    // Даже большая ревизия устаревшего снимка не даёт права перезаписать чужое состояние.
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
        ..PlainTextBatch::default_flags()
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

const WRITE: FsOperationCounts = FsOperationCounts {
    temporary_files: 1,
    renames: 1,
    file_syncs: 1,
    directory_syncs: 1,
};
const CONFIRM: FsOperationCounts = FsOperationCounts {
    temporary_files: 0,
    renames: 0,
    file_syncs: 1,
    directory_syncs: 1,
};
const NOTHING: FsOperationCounts = FsOperationCounts::ZERO;

/// Выполняет действие и возвращает операции записи и синхронизации, которые оно сделало.
fn counted<T>(action: impl FnOnce() -> T) -> (T, FsOperationCounts) {
    FS_OPERATIONS.with(|cell| cell.set(FsOperationCounts::ZERO));
    let result = action();
    (result, FS_OPERATIONS.with(|cell| cell.get()))
}

fn state_path(root: &Path, batch_id: &str) -> PathBuf {
    root.join(".runtime/batches")
        .join(batch_id)
        .join("state.json")
}

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().ino()
}

fn temporary_leftovers(root: &Path, batch_id: &str) -> usize {
    fs::read_dir(root.join(".runtime/batches").join(batch_id))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
        })
        .count()
}

fn saved(root: &Path, batch_id: &str) -> PlainTextBatch {
    serde_json::from_slice(&fs::read(state_path(root, batch_id)).unwrap()).unwrap()
}

#[test]
fn identical_resave_skips_publication_and_syncs_after_own_durable_write() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "idempotent").unwrap();
    assert!(runtime.load::<PlainTextBatch>().unwrap().is_none());
    let state = PlainTextBatch::new("idempotent", 1, "первое состояние");

    let (result, first) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(
        first, WRITE,
        "создание первого состояния — настоящая запись"
    );
    let path = state_path(temporary.path(), "idempotent");
    let written = fs::read(&path).unwrap();
    let written_inode = inode(&path);

    for _ in 0..3 {
        let (result, repeated) = counted(|| runtime.save(&state));
        result.unwrap();
        assert_eq!(repeated, NOTHING);
    }
    assert_eq!(fs::read(&path).unwrap(), written);
    assert_eq!(inode(&path), written_inode, "назначение не заменялось");
    assert_eq!(temporary_leftovers(temporary.path(), "idempotent"), 0);

    drop(runtime);
    let mut reopened = SafeBatchRuntime::open(temporary.path(), "idempotent").unwrap();
    assert_eq!(reopened.load::<PlainTextBatch>().unwrap().unwrap(), state);
}

#[test]
fn identical_save_after_load_confirms_durability_without_new_publication() {
    let temporary = TemporaryDirectory::new();
    let state = PlainTextBatch::new("confirmed", 4, "сохранено прежним процессом");
    {
        let mut runtime = SafeBatchRuntime::open(temporary.path(), "confirmed").unwrap();
        runtime.load::<PlainTextBatch>().unwrap();
        runtime.save(&state).unwrap();
    }
    let path = state_path(temporary.path(), "confirmed");
    let written_inode = inode(&path);

    let mut runtime = SafeBatchRuntime::open(temporary.path(), "confirmed").unwrap();
    let loaded = runtime.load::<PlainTextBatch>().unwrap().unwrap();
    // Чужая публикация могла не дойти до синхронизации каталога: надёжность
    // подтверждается синхронизацией существующего файла, но без новой записи.
    let (result, confirmed) = counted(|| runtime.save(&loaded));
    result.unwrap();
    assert_eq!(confirmed, CONFIRM);
    let (result, repeated) = counted(|| runtime.save(&loaded));
    result.unwrap();
    assert_eq!(repeated, NOTHING);
    assert_eq!(inode(&path), written_inode);
    assert_eq!(temporary_leftovers(temporary.path(), "confirmed"), 0);
}

#[test]
fn changed_bytes_are_written_even_with_the_same_revision() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "changes").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let mut state = PlainTextBatch::new("changes", 1, "исходное");
    runtime.save(&state).unwrap();

    state.description = "изменено без ревизии".into();
    let (result, same_revision) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(same_revision, WRITE);
    assert_eq!(saved(temporary.path(), "changes"), state);

    state.revision = 2;
    let (result, increased) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(increased, WRITE);
    assert_eq!(saved(temporary.path(), "changes"), state);

    let before = fs::read(state_path(temporary.path(), "changes")).unwrap();
    let mut decreased = state.clone();
    decreased.revision = 1;
    let (result, rejected) = counted(|| runtime.save(&decreased));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(rejected, NOTHING);
    assert_eq!(
        fs::read(state_path(temporary.path(), "changes")).unwrap(),
        before
    );

    // Отказ не испортил право сохранять: те же байты — без записи, новые — запись.
    let (result, unchanged) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(unchanged, NOTHING);
}

#[test]
fn identical_bytes_do_not_bypass_state_validation_or_batch_identity() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "validated").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let state = PlainTextBatch::new("validated", 1, "допустимое");
    runtime.save(&state).unwrap();
    let before = fs::read(state_path(temporary.path(), "validated")).unwrap();

    let mut poisoned = state.clone();
    poisoned.poisoned = true;
    assert_eq!(serde_json::to_vec_pretty(&poisoned).unwrap(), before);
    let (result, invalid) = counted(|| runtime.save(&poisoned));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(invalid, NOTHING);

    let mut foreign = state.clone();
    foreign.batch_id = "other".into();
    let (result, wrong_batch) = counted(|| runtime.save(&foreign));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(wrong_batch, NOTHING);
    assert_eq!(
        fs::read(state_path(temporary.path(), "validated")).unwrap(),
        before
    );
}

#[test]
fn identical_bytes_do_not_bypass_referenced_blob_verification() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "blob-check").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let blob = runtime.persist_blob(b"payload", "bin").unwrap();
    let state = PlainTextBatch {
        blob: Some(blob.clone()),
        ..PlainTextBatch::new("blob-check", 1, "со ссылкой на blob")
    };
    runtime.save(&state).unwrap();
    let before = fs::read(state_path(temporary.path(), "blob-check")).unwrap();

    // Новый контекст проверки заставляет перечитать blob; отказ домена — отказ save.
    let mut rejected = state.clone();
    rejected.blob_context = Some("reject".into());
    assert_eq!(serde_json::to_vec_pretty(&rejected).unwrap(), before);
    let (result, counts) = counted(|| runtime.save(&rejected));
    assert_eq!(result.unwrap_err().code, ErrorCode::ValidatorFailure);
    assert_eq!(counts, NOTHING);

    let blob_path = temporary
        .path()
        .join(".runtime/batches/blob-check")
        .join(&blob.storage_path);
    fs::write(&blob_path, b"tampered").unwrap();
    let mut corrupted = state.clone();
    corrupted.blob_context = Some("corrupted".into());
    let (result, counts) = counted(|| runtime.save(&corrupted));
    assert_eq!(result.unwrap_err().code, ErrorCode::IntegrityMismatch);
    assert_eq!(counts, NOTHING);

    fs::remove_file(&blob_path).unwrap();
    let mut missing = state.clone();
    missing.blob_context = Some("missing".into());
    let (result, counts) = counted(|| runtime.save(&missing));
    assert_eq!(result.unwrap_err().code, ErrorCode::MissingAssetFile);
    assert_eq!(counts, NOTHING);
    assert_eq!(
        fs::read(state_path(temporary.path(), "blob-check")).unwrap(),
        before
    );

    // Новый объект проверяет blob при загрузке и не получает права сохранять.
    drop(runtime);
    let mut reopened = SafeBatchRuntime::open(temporary.path(), "blob-check").unwrap();
    assert_eq!(
        reopened.load::<PlainTextBatch>().unwrap_err().code,
        ErrorCode::MissingAssetFile
    );
    let (result, counts) = counted(|| reopened.save(&state));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(counts, NOTHING);
}

#[test]
fn lock_gap_requires_reload_and_drops_own_durability_record() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "relock").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let state = PlainTextBatch::new("relock", 1, "до паузы");
    runtime.save(&state).unwrap();

    runtime.release_lock().unwrap();
    runtime.reacquire_lock().unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(counts, NOTHING);

    let reloaded = runtime.reload_cached::<PlainTextBatch>().unwrap().unwrap();
    assert_eq!(reloaded, state);
    let (result, counts) = counted(|| runtime.save(&reloaded));
    result.unwrap();
    assert_eq!(
        counts, CONFIRM,
        "после паузы прежняя публикация не считается своей"
    );

    // Пока блокировка снята, другой владелец меняет состояние.
    runtime.release_lock().unwrap();
    let updated = {
        let mut other = SafeBatchRuntime::open(temporary.path(), "relock").unwrap();
        let mut updated = other.load::<PlainTextBatch>().unwrap().unwrap();
        updated.revision = 2;
        updated.description = "другой владелец".into();
        other.save(&updated).unwrap();
        updated
    };
    runtime.reacquire_lock().unwrap();
    assert_eq!(
        runtime.save(&state).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(saved(temporary.path(), "relock"), updated);

    // Неуспешная повторная загрузка тоже не возвращает права сохранять.
    let path = state_path(temporary.path(), "relock");
    let valid = fs::read(&path).unwrap();
    fs::write(&path, b"{ broken").unwrap();
    assert_eq!(
        runtime.reload_cached::<PlainTextBatch>().unwrap_err().code,
        ErrorCode::ManifestCorrupt
    );
    let (result, counts) = counted(|| runtime.save(&updated));
    assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    assert_eq!(counts, NOTHING);
    assert_eq!(fs::read(&path).unwrap(), b"{ broken");

    fs::write(&path, &valid).unwrap();
    let reloaded = runtime.reload_cached::<PlainTextBatch>().unwrap().unwrap();
    assert_eq!(reloaded, updated);
    let (result, counts) = counted(|| runtime.save(&reloaded));
    result.unwrap();
    assert_eq!(counts, CONFIRM);
}

#[test]
fn missing_corrupted_or_replaced_state_is_never_a_silent_no_op() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "tampered").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let state = PlainTextBatch::new("tampered", 1, "эталонное состояние");
    runtime.save(&state).unwrap();
    let path = state_path(temporary.path(), "tampered");
    let expected = fs::read(&path).unwrap();

    fs::remove_file(&path).unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(
        counts, WRITE,
        "удалённое назначение восстанавливается записью"
    );
    assert_eq!(fs::read(&path).unwrap(), expected);

    // Повреждение той же длины, отличие только в последнем байте.
    let mut corrupted = expected.clone();
    *corrupted.last_mut().unwrap() ^= 0x01;
    fs::write(&path, &corrupted).unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, WRITE);
    assert_eq!(fs::read(&path).unwrap(), expected);

    // Внешняя замена теми же байтами: содержимое совпадает, но публикация чужая.
    let external = path.with_file_name("external.json");
    fs::write(&external, &expected).unwrap();
    fs::rename(&external, &path).unwrap();
    let replaced_inode = inode(&path);
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, CONFIRM);
    assert_eq!(inode(&path), replaced_inode);

    // Перезапись на месте теми же байтами: снова подтверждение. Метки времени
    // ФС грубые, поэтому mtime задаётся явно, иначе запись в пределах одного
    // тика могла бы оставить отпечаток прежним.
    fs::write(&path, &expected).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, CONFIRM);
    assert_eq!(temporary_leftovers(temporary.path(), "tampered"), 0);
}

#[test]
fn symlinked_or_non_regular_state_is_rejected_even_with_identical_bytes() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let outside = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "boundary").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let state = PlainTextBatch::new("boundary", 1, "состояние у границы");
    runtime.save(&state).unwrap();
    let path = state_path(temporary.path(), "boundary");
    let expected = fs::read(&path).unwrap();

    let target = outside.path().join("state.json");
    fs::write(&target, &expected).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    assert_eq!(result.unwrap_err().code, ErrorCode::BoundaryViolation);
    assert_eq!(counts, NOTHING);
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), expected);

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let (result, counts) = counted(|| runtime.save(&state));
    assert_eq!(result.unwrap_err().code, ErrorCode::BoundaryViolation);
    assert_eq!(counts, NOTHING);
    assert!(path.is_dir());
    assert_eq!(temporary_leftovers(temporary.path(), "boundary"), 0);
}

/// Выполняет `save` со взведёнными одноразовыми отказами и проверяет, что
/// каждый из них действительно сработал на пути сохранения.
fn save_with_faults(
    runtime: &mut SafeBatchRuntime,
    state: &PlainTextBatch,
    faults: &[FaultPoint],
) -> (Result<(), AssetError>, FsOperationCounts) {
    INJECTED_FAULTS.with(|armed| *armed.borrow_mut() = faults.to_vec());
    let outcome = counted(|| runtime.save(state));
    let unused = INJECTED_FAULTS.with(|armed| std::mem::take(&mut *armed.borrow_mut()));
    assert!(unused.is_empty(), "отказы не достигнуты: {unused:?}");
    outcome
}

const WRITE_AFTER_FAILED_FILE_CONFIRMATION: FsOperationCounts = FsOperationCounts {
    file_syncs: 2,
    ..WRITE
};
const WRITE_AFTER_FAILED_DIRECTORY_CONFIRMATION: FsOperationCounts = FsOperationCounts {
    file_syncs: 2,
    directory_syncs: 2,
    ..WRITE
};

#[test]
fn compare_read_failure_is_never_success_without_publication() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "compare-fault").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let state = PlainTextBatch::new("compare-fault", 1, "сравниваемое состояние");
    runtime.save(&state).unwrap();
    let path = state_path(temporary.path(), "compare-fault");
    let expected = fs::read(&path).unwrap();
    let durable_inode = inode(&path);

    // Даже при собственной надёжной публикации ошибка чтения ведёт к полной записи.
    let (result, counts) = save_with_faults(&mut runtime, &state, &[FaultPoint::CompareRead]);
    result.unwrap();
    assert_eq!(counts, WRITE);
    assert_ne!(
        inode(&path),
        durable_inode,
        "назначение опубликовано заново"
    );
    assert_eq!(fs::read(&path).unwrap(), expected);
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(
        counts, NOTHING,
        "новая публикация надёжна и принадлежит объекту"
    );

    // Если и запись не удалась, save возвращает ошибку, прежнее содержимое цело.
    let published_inode = inode(&path);
    let (result, counts) = save_with_faults(
        &mut runtime,
        &state,
        &[FaultPoint::CompareRead, FaultPoint::WriteFileSync],
    );
    assert_eq!(result.unwrap_err().code, ErrorCode::IoFailure);
    assert_eq!(counts.renames, 0);
    assert_eq!(inode(&path), published_inode);
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(temporary_leftovers(temporary.path(), "compare-fault"), 0);
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(
        counts, CONFIRM,
        "после отказа прежнее подтверждение не используется"
    );
}

#[test]
fn durability_confirmation_failure_is_never_success_without_publication() {
    for (fault, fallback) in [
        (
            FaultPoint::ConfirmFileSync,
            WRITE_AFTER_FAILED_FILE_CONFIRMATION,
        ),
        (
            FaultPoint::ConfirmDirectorySync,
            WRITE_AFTER_FAILED_DIRECTORY_CONFIRMATION,
        ),
    ] {
        let temporary = TemporaryDirectory::new();
        let state = PlainTextBatch::new("confirm-fault", 3, "сохранено прежним процессом");
        {
            let mut runtime = SafeBatchRuntime::open(temporary.path(), "confirm-fault").unwrap();
            runtime.load::<PlainTextBatch>().unwrap();
            runtime.save(&state).unwrap();
        }
        let path = state_path(temporary.path(), "confirm-fault");
        let expected = fs::read(&path).unwrap();
        let foreign_inode = inode(&path);

        // Неподтверждённая чужая публикация заменяется полной атомарной записью.
        let mut runtime = SafeBatchRuntime::open(temporary.path(), "confirm-fault").unwrap();
        let loaded = runtime.load::<PlainTextBatch>().unwrap().unwrap();
        let (result, counts) = save_with_faults(&mut runtime, &loaded, &[fault]);
        result.unwrap();
        assert_eq!(counts, fallback, "{fault:?}");
        assert_ne!(inode(&path), foreign_inode, "{fault:?}");
        assert_eq!(fs::read(&path).unwrap(), expected);
        let (result, counts) = counted(|| runtime.save(&loaded));
        result.unwrap();
        assert_eq!(counts, NOTHING, "{fault:?}");

        // Чужая замена теми же байтами, подтверждение и запись не удались:
        // ошибка, файл не тронут, собственное подтверждение сброшено.
        let external = path.with_file_name("external.json");
        fs::write(&external, &expected).unwrap();
        fs::rename(&external, &path).unwrap();
        let replaced_inode = inode(&path);
        let (result, _) =
            save_with_faults(&mut runtime, &loaded, &[fault, FaultPoint::WriteFileSync]);
        assert_eq!(result.unwrap_err().code, ErrorCode::IoFailure, "{fault:?}");
        assert_eq!(inode(&path), replaced_inode, "{fault:?}");
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(temporary_leftovers(temporary.path(), "confirm-fault"), 0);
        let (result, counts) = counted(|| runtime.save(&loaded));
        result.unwrap();
        assert_eq!(counts, CONFIRM, "{fault:?}");
    }
}

#[test]
fn failed_real_write_keeps_old_state_and_drops_durability_record() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "failing").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let first = PlainTextBatch::new("failing", 1, "сохранено");
    runtime.save(&first).unwrap();
    let path = state_path(temporary.path(), "failing");
    let expected = fs::read(&path).unwrap();

    // Отказ до атомарной замены: прежнее состояние и каталог не тронуты.
    let second = PlainTextBatch::new("failing", 2, "не будет записано");
    let (result, counts) = save_with_faults(&mut runtime, &second, &[FaultPoint::WriteFileSync]);
    assert_eq!(result.unwrap_err().code, ErrorCode::IoFailure);
    assert_eq!(counts.renames, 0);
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(temporary_leftovers(temporary.path(), "failing"), 0);

    // После отказа записи собственное подтверждение не используется.
    let (result, counts) = counted(|| runtime.save(&first));
    result.unwrap();
    assert_eq!(counts, CONFIRM);
    let (result, counts) = counted(|| runtime.save(&second));
    result.unwrap();
    assert_eq!(counts, WRITE);
    assert_eq!(saved(temporary.path(), "failing"), second);

    // Отказ синхронизации каталога после замены: байты уже видны, но
    // публикация не надёжна, и следующее сохранение её подтверждает.
    let third = PlainTextBatch::new("failing", 3, "замена без подтверждения каталога");
    let (result, counts) =
        save_with_faults(&mut runtime, &third, &[FaultPoint::WriteDirectorySync]);
    assert_eq!(result.unwrap_err().code, ErrorCode::IoFailure);
    assert_eq!(counts, WRITE);
    assert_eq!(saved(temporary.path(), "failing"), third);
    let (result, counts) = counted(|| runtime.save(&third));
    result.unwrap();
    assert_eq!(counts, CONFIRM);
}

#[test]
fn large_state_is_compared_completely_and_reopens_after_no_op() {
    let temporary = TemporaryDirectory::new();
    let mut runtime = SafeBatchRuntime::open(temporary.path(), "large").unwrap();
    runtime.load::<PlainTextBatch>().unwrap();
    let body = "あ".repeat(4 * 1024 * 1024);
    let mut state = PlainTextBatch::new("large", 1, &body);
    runtime.save(&state).unwrap();
    let size = fs::metadata(state_path(temporary.path(), "large"))
        .unwrap()
        .len();
    assert!(size > 12 * 1024 * 1024);

    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, NOTHING);

    // Та же длина, отличие в последнем символе описания: потоковое сравнение
    // обязано дойти до конца файла.
    state.description.pop();
    state.description.push('い');
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, WRITE);
    assert_eq!(
        fs::metadata(state_path(temporary.path(), "large"))
            .unwrap()
            .len(),
        size
    );
    let (result, counts) = counted(|| runtime.save(&state));
    result.unwrap();
    assert_eq!(counts, NOTHING);

    drop(runtime);
    let mut reopened = SafeBatchRuntime::open(temporary.path(), "large").unwrap();
    assert_eq!(reopened.load::<PlainTextBatch>().unwrap().unwrap(), state);
}
