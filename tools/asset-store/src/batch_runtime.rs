//! Домен-независимая безопасная файловая граница сохранённых пакетов.
//!
//! Модуль управляет только каталогом пакета, сериализованным состоянием и
//! неизменяемыми blob-файлами. Предметные правила и формат состояния задаёт
//! вызывающий домен через [`RuntimeBatchState`].

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, RenameFlags, flock, mkdirat, open, openat, renameat,
    renameat_with, unlinkat,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;

/// Максимальный размер сохранённого состояния одного пакета.
pub const MAX_RUNTIME_STATE_BYTES: u64 = 64 * 1024 * 1024;
/// Общий предел одного blob; конкретный домен может применять более строгий предел.
pub const MAX_RUNTIME_BLOB_BYTES: u64 = 64 * 1024 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Открытый диагностический файл одного запуска. Его дескриптор сохраняет
/// работоспособность при освобождении блокировки состояния пакета.
#[derive(Debug)]
pub struct RuntimeRunLog {
    pub run_id: String,
    pub path: PathBuf,
    pub file: File,
}

/// Минимальный контракт сериализуемого состояния предметного пакета.
///
/// Runtime сам проверяет безопасные границы файла и монотонность revision,
/// а предметную структуру проверяет `validate`. Ссылки на неизменяемые blob
/// возвращаются в нейтральном виде, чтобы runtime мог повторно сверить их байты.
pub trait RuntimeBatchState: Serialize + DeserializeOwned {
    fn batch_id(&self) -> &str;
    fn revision(&self) -> u64;
    fn validate(&self) -> Result<(), AssetError>;
    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef>;

    /// Отклоняет сохранённое состояние несовместимой версии до типизированного
    /// разбора, чтобы старая схема получила явный отказ, а не сообщение о повреждении.
    fn check_stored_compatibility(_raw: &serde_json::Value) -> Result<(), AssetError> {
        Ok(())
    }

    /// Доменные состояния могут сохранить более строгий лимит, чем общий runtime.
    fn maximum_blob_bytes(&self) -> u64 {
        MAX_RUNTIME_BLOB_BYTES
    }

    /// Непрозрачный ключ контекста проверки содержимого для конкретной ссылки.
    /// Он входит в кэш runtime и должен меняться, если меняется ожидаемое
    /// предметное свойство байтов. `None` означает, что дополнительной проверки нет.
    fn blob_validation_context(&self, _blob: &RuntimeBlobRef) -> Option<&str> {
        None
    }

    /// Проверяет предметные свойства байтов после проверки пути, размера и SHA-256.
    fn validate_blob_bytes(&self, _blob: &RuntimeBlobRef, _bytes: &[u8]) -> Result<(), AssetError> {
        Ok(())
    }
}

/// Ссылка на blob, путь к которому вычисляется по SHA-256, внутри каталога пакета.
///
/// `storage_path` сохраняет существующую форму `candidates/<sha256>.<ext>`;
/// расширение ограничено безопасным токеном имени файла и не влияет на проверку SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBlobRef {
    pub sha256: String,
    pub storage_path: String,
}

/// Безопасный runtime-пакет: доступ относительно dirfd, `NOFOLLOW`, явный жизненный цикл
/// блокировки, ограниченное состояние и атомарная запись. После освобождения
/// и повторного захвата блокировки состояние требуется загрузить заново.
#[derive(Debug)]
pub struct SafeBatchRuntime {
    directory: File,
    blobs: File,
    batch_id: String,
    loaded_revision: Option<u64>,
    directory_path: PathBuf,
    locked: bool,
    needs_reload: bool,
    verified_blob_keys: BTreeSet<(String, String, u64, Option<String>)>,
    /// Идентичность `state.json`, который этот объект при непрерывно
    /// удерживаемой блокировке сам надёжно опубликовал или чью надёжность
    /// подтвердил `fsync` файла и каталога. Сбрасывается при освобождении
    /// блокировки и при любом отказе записи.
    durable_state: Option<FileIdentity>,
}

/// Отпечаток inode по `fstat`. Замена файла меняет `ino`, запись на месте —
/// `mtime` и `ctime`, но лишь с точностью меток времени ФС: внешняя
/// перезапись теми же по размеру байтами в пределах одного тика может оставить
/// отпечаток прежним. Писатели под блокировкой пакета сбрасывают отпечаток
/// сами, поэтому для них совпадение означает тот же неизменённый файл,
/// который был синхронизирован при публикации или подтверждении.
///
/// Отпечаток снимается с открытого дескриптора, а не с имени: процесс, который
/// заменяет `state.json`, не удерживая блокировку пакета, во время сравнения
/// может направить имя на другой inode. Такие писатели вне контракта —
/// `flock` рекомендательная, и прежняя атомарная запись от них тоже не
/// защищала; последовательная внешняя замена между вызовами `save`
/// обнаруживается по смене отпечатка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// Как `save` выполнил обязательство надёжного сохранения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveOutcome {
    /// Новые байты атомарно опубликованы с синхронизацией файла и каталога.
    Written,
    /// Назначение побайтно совпадает и этим объектом уже надёжно опубликовано.
    UnchangedAlreadyDurable,
    /// Назначение побайтно совпадает; его надёжность подтверждена синхронизацией
    /// существующего файла и каталога без новой публикации.
    UnchangedDurabilityConfirmed,
}

impl SaveOutcome {
    fn code(self) -> &'static str {
        match self {
            Self::Written => "state_save_succeeded",
            Self::UnchangedAlreadyDurable => "state_save_unchanged_already_durable",
            Self::UnchangedDurabilityConfirmed => "state_save_unchanged_durability_confirmed",
        }
    }
}

/// Результат сравнения назначения с новыми байтами.
enum ExistingState {
    Missing,
    Different,
    Identical { file: File, identity: FileIdentity },
}

/// Изолированный каталог производных артефактов. Все имена — одиночные
/// компоненты, чтение ограничено и не следует символическим ссылкам.
pub(crate) struct ArtifactDirectory {
    directory: File,
    pub path: PathBuf,
}

impl ArtifactDirectory {
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<(), AssetError> {
        atomic_write(&self.directory, name, bytes, MAX_RUNTIME_STATE_BYTES)
    }

    /// Атомарно создаёт артефакт, только если имя ещё свободно.
    pub fn write_if_absent(&self, name: &str, bytes: &[u8]) -> Result<bool, AssetError> {
        atomic_write_if_absent(&self.directory, name, bytes, MAX_RUNTIME_STATE_BYTES)
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>, AssetError> {
        validate_leaf_name(name)?;
        read_file(&self.directory, name, MAX_RUNTIME_STATE_BYTES)
    }
}

impl SafeBatchRuntime {
    pub(crate) fn artifact_directory(&self, name: &str) -> Result<ArtifactDirectory, AssetError> {
        self.require_lock()?;
        validate_leaf_name(name)?;
        Ok(ArtifactDirectory {
            directory: ensure_directory(&self.directory, name)?,
            path: self.directory_path.join(name),
        })
    }
    /// Корень должен принадлежать вызывающему владельцу хранилища.
    pub fn open(store_root: &Path, batch_id: &str) -> Result<Self, AssetError> {
        validate_batch_id(batch_id)?;
        let store_path = if store_root.is_absolute() {
            store_root.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| AssetError::io("текущий каталог runtime", error))?
                .join(store_root)
        };
        let root = File::from(
            open(
                store_root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(boundary_io)?,
        );
        let runtime = ensure_directory(&root, ".runtime")?;
        let batches = ensure_directory(&runtime, "batches")?;
        let directory = ensure_directory(&batches, batch_id)?;
        flock(&directory, FlockOperation::LockExclusive).map_err(boundary_io)?;
        let blobs = ensure_directory(&directory, "candidates")?;
        Ok(Self {
            directory,
            blobs,
            batch_id: batch_id.into(),
            loaded_revision: None,
            directory_path: store_path.join(".runtime").join("batches").join(batch_id),
            locked: true,
            needs_reload: false,
            verified_blob_keys: BTreeSet::new(),
            durable_state: None,
        })
    }

    pub fn batch_id(&self) -> &str {
        &self.batch_id
    }

    /// Безопасно создаёт отдельный JSONL-файл запуска в пространстве имён пакета.
    /// Имя и путь принадлежат runtime. Коллизия имён не перезаписывает прежний
    /// файл: создаётся другое имя. Возвращённый объект записи живёт независимо от блокировки.
    pub fn create_run_log(&self) -> Result<RuntimeRunLog, AssetError> {
        self.require_lock()?;
        tracing::debug!(
            operation = "batch_runtime_log_create",
            stage = "diagnostic_log",
            code = "run_log_create_started",
            batch_id = self.batch_id,
            "Создание отдельного диагностического файла"
        );
        let logs = ensure_directory(&self.directory, "logs")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| invalid(format!("не удалось определить время запуска: {error}")))?
            .as_nanos();
        loop {
            let run_id = format!(
                "{timestamp}-{}-{}",
                std::process::id(),
                RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let name = format!("{run_id}.jsonl");
            match create_log_file(&logs, &name)? {
                Some(file) => {
                    logs.sync_all()
                        .map_err(|error| AssetError::io("синхронизация каталога логов", error))?;
                    tracing::info!(
                        operation = "batch_runtime_log_create",
                        stage = "diagnostic_log",
                        code = "run_log_created",
                        batch_id = self.batch_id,
                        run_id,
                        path = %self.directory_path.join("logs").join(&name).display(),
                        "Диагностический файл создан безопасным runtime"
                    );
                    return Ok(RuntimeRunLog {
                        run_id,
                        path: self.directory_path.join("logs").join(name),
                        file,
                    });
                }
                None => continue,
            }
        }
    }

    /// Освобождает исключительную блокировку на время внешнего ожидания.
    /// Дескрипторы и кэш уже проверенных неизменяемых blob остаются привязаны
    /// к этому объекту. Право сохранять загруженный снимок аннулируется;
    /// после повторного захвата необходимо успешное `load`/`reload_cached`.
    pub fn release_lock(&mut self) -> Result<(), AssetError> {
        if !self.locked {
            tracing::warn!(
                operation = "batch_runtime_lock",
                stage = "runtime_lock",
                code = "lock_release_without_lock",
                batch_id = self.batch_id,
                "Запрошено освобождение уже снятой блокировки"
            );
            return Err(invalid("блокировка runtime уже освобождена"));
        }
        tracing::debug!(
            operation = "batch_runtime_lock",
            stage = "runtime_lock",
            code = "lock_release_started",
            batch_id = self.batch_id,
            loaded_revision = ?self.loaded_revision,
            "Освобождение блокировки состояния пакета"
        );
        if let Err(error) = flock(&self.directory, FlockOperation::Unlock) {
            let error = boundary_io(error);
            tracing::error!(
                operation = "batch_runtime_lock",
                stage = "runtime_lock",
                code = "lock_release_failed",
                batch_id = self.batch_id,
                message = %crate::diagnostics::safe_message(&error.message),
                "Не удалось освободить блокировку состояния пакета"
            );
            return Err(error);
        }
        self.locked = false;
        self.loaded_revision = None;
        self.needs_reload = true;
        // Без блокировки файл может заменить другой владелец; прежнее
        // подтверждение надёжности к новому содержимому не относится.
        self.durable_state = None;
        tracing::debug!(
            operation = "batch_runtime_lock",
            stage = "runtime_lock",
            code = "lock_released_snapshot_invalidated",
            batch_id = self.batch_id,
            "Блокировка снята; сохранение старого снимка запрещено"
        );
        Ok(())
    }

    /// Повторно захватывает блокировку после внешнего ожидания.
    pub fn reacquire_lock(&mut self) -> Result<(), AssetError> {
        if self.locked {
            tracing::warn!(
                operation = "batch_runtime_lock",
                stage = "runtime_lock",
                code = "lock_reacquire_while_locked",
                batch_id = self.batch_id,
                "Запрошено повторное получение удерживаемой блокировки"
            );
            return Err(invalid("блокировка runtime уже удерживается"));
        }
        tracing::debug!(
            operation = "batch_runtime_lock",
            stage = "runtime_lock",
            code = "lock_reacquire_started",
            batch_id = self.batch_id,
            "Повторное получение блокировки состояния пакета"
        );
        if let Err(error) = flock(&self.directory, FlockOperation::LockExclusive) {
            let error = boundary_io(error);
            tracing::error!(
                operation = "batch_runtime_lock",
                stage = "runtime_lock",
                code = "lock_reacquire_failed",
                batch_id = self.batch_id,
                message = %crate::diagnostics::safe_message(&error.message),
                "Не удалось повторно получить блокировку состояния пакета"
            );
            return Err(error);
        }
        self.locked = true;
        tracing::debug!(
            operation = "batch_runtime_lock",
            stage = "runtime_lock",
            code = "lock_reacquired_reload_required",
            batch_id = self.batch_id,
            reload_required = self.needs_reload,
            "Блокировка повторно получена; перед записью нужно перечитать состояние"
        );
        Ok(())
    }

    /// Загружает состояние и сверяет все сохранённые ссылки на blob.
    pub fn load<S: RuntimeBatchState>(&mut self) -> Result<Option<S>, AssetError> {
        self.load_inner(true)
    }

    /// Перечитывает состояние после повторного захвата блокировки. Ключи
    /// ссылок на blob, которые были проверены этим объектом до освобождения
    /// блокировки, переиспользуются; новые ссылки проходят полную проверку.
    pub fn reload_cached<S: RuntimeBatchState>(&mut self) -> Result<Option<S>, AssetError> {
        self.load_inner(false)
    }

    fn load_inner<S: RuntimeBatchState>(
        &mut self,
        force_recheck: bool,
    ) -> Result<Option<S>, AssetError> {
        self.require_lock()?;
        tracing::debug!(
            operation = "batch_runtime_state_load",
            stage = "runtime_reload",
            code = "state_load_started",
            batch_id = self.batch_id,
            force_blob_recheck = force_recheck,
            "Загрузка текущего состояния пакета"
        );
        // Неудачная загрузка тоже лишает старый снимок права на запись.
        self.loaded_revision = None;
        self.needs_reload = true;
        let bytes = match read_file(&self.directory, "state.json", MAX_RUNTIME_STATE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                self.loaded_revision = None;
                self.needs_reload = false;
                tracing::debug!(
                    operation = "batch_runtime_state_load",
                    stage = "runtime_reload",
                    code = "state_missing",
                    batch_id = self.batch_id,
                    "Состояние пакета ещё не создано"
                );
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("JSON runtime-пакета повреждён: {error}"),
            )
        })?;
        S::check_stored_compatibility(&raw)?;
        // Типизированный разбор идёт по исходным байтам: `Value` молча схлопывает
        // повторные ключи, а разбор структуры их отвергает.
        let state: S = serde_json::from_slice(&bytes).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("JSON runtime-пакета повреждён: {error}"),
            )
        })?;
        state.validate()?;
        if state.batch_id() != self.batch_id {
            return Err(invalid(
                "идентификатор пакета не совпадает с каталогом runtime-данных",
            ));
        }
        self.verify_referenced_blobs(&state, force_recheck)?;
        self.loaded_revision = Some(state.revision());
        self.needs_reload = false;
        tracing::debug!(
            operation = "batch_runtime_state_load",
            stage = "runtime_reload",
            code = "state_loaded",
            batch_id = self.batch_id,
            revision = state.revision(),
            force_blob_recheck = force_recheck,
            cached_blob_verifications = self.verified_blob_keys.len(),
            "Текущее состояние пакета загружено"
        );
        Ok(Some(state))
    }

    /// Сохраняет состояние после его загрузки; новый пакет допускает только
    /// если `state.json` ещё отсутствует. Запись атомарна и синхронизирует файл
    /// и каталог до возврата.
    ///
    /// Если загруженный пакет уже содержит побайтно те же данные, новая
    /// публикация не создаётся: после всех проверок состояния и blob runtime
    /// сравнивает байты назначения и либо опирается на собственную надёжную
    /// публикацию того же inode, либо подтверждает надёжность синхронизацией
    /// существующего файла и каталога. Ошибка сравнения или подтверждения
    /// приводит к обычной атомарной записи, а не к успеху без записи.
    pub fn save<S: RuntimeBatchState>(&mut self, state: &S) -> Result<(), AssetError> {
        self.require_lock()?;
        if self.needs_reload {
            tracing::warn!(
                operation = "batch_runtime_state_save",
                stage = "compare_and_swap",
                code = "stale_snapshot_save_rejected",
                batch_id = self.batch_id,
                requested_revision = state.revision(),
                loaded_revision = ?self.loaded_revision,
                "Сохранение запрещено до повторной загрузки состояния"
            );
            return Err(invalid(
                "состояние требуется успешно загрузить перед сохранением",
            ));
        }
        tracing::debug!(
            operation = "batch_runtime_state_save",
            stage = "checkpoint",
            code = "state_save_started",
            batch_id = self.batch_id,
            requested_revision = state.revision(),
            loaded_revision = ?self.loaded_revision,
            "Начата запись состояния пакета"
        );
        state.validate()?;
        if state.batch_id() != self.batch_id {
            return Err(invalid(
                "идентификатор пакета не совпадает с каталогом runtime-данных",
            ));
        }
        if let Some(revision) = self.loaded_revision {
            if state.revision() < revision {
                return Err(invalid("номер изменения состояния пакета нельзя уменьшить"));
            }
        } else {
            match read_file(&self.directory, "state.json", MAX_RUNTIME_STATE_BYTES) {
                Ok(_) => {
                    return Err(invalid(
                        "существующий пакет нужно загрузить перед сохранением",
                    ));
                }
                Err(error) if error.code == ErrorCode::MissingAssetFile => {}
                Err(error) => return Err(error),
            }
        }
        self.verify_referenced_blobs(state, false)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() as u64 > MAX_RUNTIME_STATE_BYTES {
            return Err(invalid(
                "состояние пакета превышает ограничение runtime-данных",
            ));
        }
        let outcome = self.publish_state(&bytes, self.loaded_revision.is_some())?;
        self.loaded_revision = Some(state.revision());
        match outcome {
            SaveOutcome::Written => tracing::info!(
                operation = "batch_runtime_state_save",
                stage = "checkpoint",
                code = outcome.code(),
                batch_id = self.batch_id,
                revision = state.revision(),
                state_bytes = bytes.len(),
                "Состояние пакета надёжно записано"
            ),
            SaveOutcome::UnchangedAlreadyDurable | SaveOutcome::UnchangedDurabilityConfirmed => {
                tracing::info!(
                    operation = "batch_runtime_state_save",
                    stage = "checkpoint",
                    code = outcome.code(),
                    batch_id = self.batch_id,
                    revision = state.revision(),
                    state_bytes = bytes.len(),
                    durability_sync = outcome == SaveOutcome::UnchangedDurabilityConfirmed,
                    "Состояние пакета уже сохранено в тех же байтах; новая запись не выполнялась"
                )
            }
        }
        Ok(())
    }

    /// Обеспечивает надёжное наличие `bytes` в `state.json`. Сравнение
    /// выполняется только для уже загруженного пакета: создание первого
    /// состояния всегда остаётся настоящей атомарной записью.
    fn publish_state(&mut self, bytes: &[u8], compare: bool) -> Result<SaveOutcome, AssetError> {
        if compare {
            match compare_existing(
                &self.directory,
                "state.json",
                bytes,
                MAX_RUNTIME_STATE_BYTES,
            ) {
                Ok(ExistingState::Identical { file, identity }) => {
                    if self.durable_state == Some(identity) {
                        return Ok(SaveOutcome::UnchangedAlreadyDurable);
                    }
                    match confirm_durability(&file, &self.directory) {
                        Ok(()) => {
                            self.durable_state = Some(identity);
                            return Ok(SaveOutcome::UnchangedDurabilityConfirmed);
                        }
                        Err(error) => tracing::warn!(
                            operation = "batch_runtime_state_save",
                            stage = "checkpoint",
                            code = "state_durability_confirmation_failed",
                            batch_id = self.batch_id,
                            message = %crate::diagnostics::safe_message(&error.message),
                            "Надёжность совпадающего состояния не подтверждена; выполняется полная запись"
                        ),
                    }
                }
                Ok(ExistingState::Missing | ExistingState::Different) => {}
                Err(error) if error.code == ErrorCode::IoFailure => tracing::warn!(
                    operation = "batch_runtime_state_save",
                    stage = "checkpoint",
                    code = "state_compare_failed",
                    batch_id = self.batch_id,
                    message = %crate::diagnostics::safe_message(&error.message),
                    "Не удалось сравнить сохранённое состояние; выполняется полная запись"
                ),
                // Тип, размер и граница назначения проверяются так же, как
                // перед атомарной заменой: такой отказ не обходится записью.
                Err(error) => {
                    self.durable_state = None;
                    return Err(error);
                }
            }
        }
        self.durable_state = None;
        let identity = atomic_write_with_identity(
            &self.directory,
            "state.json",
            bytes,
            MAX_RUNTIME_STATE_BYTES,
        )?;
        self.durable_state = identity;
        Ok(SaveOutcome::Written)
    }

    /// Сохраняет blob по пути, вычисленному из SHA-256. Повторная запись того же SHA допускается
    /// только при совпадении байтов. Runtime сохраняет прежнюю структуру каталога candidates/.
    pub fn persist_blob(
        &self,
        bytes: &[u8],
        extension: &str,
    ) -> Result<RuntimeBlobRef, AssetError> {
        self.require_lock()?;
        self.persist_blob_with_limit(bytes, extension, MAX_RUNTIME_BLOB_BYTES)
    }

    pub(crate) fn persist_blob_with_limit(
        &self,
        bytes: &[u8],
        extension: &str,
        maximum: u64,
    ) -> Result<RuntimeBlobRef, AssetError> {
        self.require_lock()?;
        if maximum > MAX_RUNTIME_BLOB_BYTES || bytes.len() as u64 > maximum {
            return Err(invalid("blob превышает общий предел размера runtime"));
        }
        validate_extension(extension)?;
        let sha256 = sha256_hex(bytes);
        let storage_path = format!("candidates/{sha256}.{extension}");
        let name = storage_path
            .strip_prefix("candidates/")
            .expect("путь runtime создаётся с фиксированным префиксом");
        match read_file(&self.blobs, name, maximum) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "по существующему пути blob сохранены другие байты",
                ));
            }
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                atomic_write(&self.blobs, name, bytes, maximum)?
            }
            Err(error) => return Err(error),
        }
        Ok(RuntimeBlobRef {
            sha256,
            storage_path,
        })
    }

    /// Возвращает байты только после проверки пути, обычного файла, размера и SHA.
    pub fn read_blob(&self, blob: &RuntimeBlobRef) -> Result<Vec<u8>, AssetError> {
        self.require_lock()?;
        self.read_blob_with_limit(blob, MAX_RUNTIME_BLOB_BYTES)
    }

    pub(crate) fn read_blob_with_limit(
        &self,
        blob: &RuntimeBlobRef,
        maximum: u64,
    ) -> Result<Vec<u8>, AssetError> {
        if maximum > MAX_RUNTIME_BLOB_BYTES {
            return Err(invalid("blob превышает общий предел размера runtime"));
        }
        let name = validate_blob_ref(blob)?;
        #[cfg(test)]
        REFERENCED_BLOB_READS.with(|reads| reads.set(reads.get() + 1));
        let bytes = read_file(&self.blobs, name, maximum)?;
        if sha256_hex(&bytes) != blob.sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты blob изменились относительно SHA-256",
            ));
        }
        Ok(bytes)
    }

    /// Записывает ограниченный доменный артефакт в корень runtime пакета.
    /// Используется адаптером для review HTML; произвольные пути запрещены.
    pub(crate) fn write_artifact(
        &self,
        name: &str,
        bytes: &[u8],
        maximum: u64,
    ) -> Result<PathBuf, AssetError> {
        self.require_lock()?;
        validate_leaf_name(name)?;
        if bytes.len() as u64 > maximum {
            return Err(invalid("runtime-артефакт превышает ограничение размера"));
        }
        atomic_write(&self.directory, name, bytes, maximum)?;
        Ok(self.directory_path.join(name))
    }

    fn require_lock(&self) -> Result<(), AssetError> {
        if self.locked {
            Ok(())
        } else {
            Err(invalid("операция runtime требует удерживаемую блокировку"))
        }
    }

    fn verify_referenced_blobs<S: RuntimeBatchState>(
        &mut self,
        state: &S,
        force_recheck: bool,
    ) -> Result<(), AssetError> {
        let mut checked_in_this_pass = BTreeSet::new();
        for blob in state.referenced_blobs() {
            let key = validate_blob_ref(&blob)?.to_owned();
            let maximum = state.maximum_blob_bytes();
            let cache_key = (
                key,
                blob.sha256.clone(),
                maximum,
                state.blob_validation_context(&blob).map(str::to_owned),
            );
            if !checked_in_this_pass.insert(cache_key.clone())
                || (!force_recheck && self.verified_blob_keys.contains(&cache_key))
            {
                continue;
            }
            let bytes = self.read_blob_with_limit(&blob, maximum)?;
            state.validate_blob_bytes(&blob, &bytes)?;
            self.verified_blob_keys.insert(cache_key);
        }
        Ok(())
    }
}

/// `None` означает коллизию имени, включая существующую символическую ссылку:
/// вызывающий код выбирает другое имя, не открывая и не меняя чужой файл.
pub(crate) fn create_log_file(parent: &File, name: &str) -> Result<Option<File>, AssetError> {
    validate_leaf_name(name)?;
    match openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(error) if error == rustix::io::Errno::EXIST => Ok(None),
        Err(error) => Err(boundary_io(error)),
    }
}

fn validate_blob_ref(blob: &RuntimeBlobRef) -> Result<&str, AssetError> {
    validate_hash(&blob.sha256)?;
    let name = blob
        .storage_path
        .strip_prefix("candidates/")
        .ok_or_else(path_error)?;
    let (hash, extension) = name.split_once('.').ok_or_else(path_error)?;
    if hash != blob.sha256 || extension.contains('.') {
        return Err(path_error());
    }
    validate_extension(extension)?;
    Ok(name)
}

fn validate_extension(extension: &str) -> Result<(), AssetError> {
    if extension.is_empty()
        || extension.len() > 16
        || !extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(invalid(
            "расширение blob должно быть безопасным токеном имени файла",
        ));
    }
    Ok(())
}

fn validate_leaf_name(name: &str) -> Result<(), AssetError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with(".tmp-")
        || name
            .chars()
            .any(|character| !character.is_alphanumeric() && !"._-".contains(character))
    {
        return Err(path_error());
    }
    Ok(())
}

fn ensure_directory(parent: &File, name: &str) -> Result<File, AssetError> {
    match mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::EXIST => {}
        Err(error) => return Err(boundary_io(error)),
    }
    Ok(File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(boundary_io)?,
    ))
}

fn open_regular_runtime_file(
    parent: &File,
    name: &str,
    maximum: u64,
) -> Result<Option<File>, AssetError> {
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            AssetError::new(
                ErrorCode::MissingAssetFile,
                "файл runtime-данных отсутствует",
            )
        } else {
            boundary_io(error)
        }
    });
    let descriptor = match descriptor {
        Ok(descriptor) => descriptor,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(None),
        Err(error) => return Err(error),
    };
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| AssetError::io("метаданные runtime-файла", error))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "запись runtime должна быть ограниченным обычным файлом",
        ));
    }
    Ok(Some(file))
}

fn read_file(parent: &File, name: &str, maximum: u64) -> Result<Vec<u8>, AssetError> {
    let file = open_regular_runtime_file(parent, name, maximum)?.ok_or_else(|| {
        AssetError::new(
            ErrorCode::MissingAssetFile,
            "файл runtime-данных отсутствует",
        )
    })?;
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("чтение runtime-файла", error))?;
    if bytes.len() as u64 > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "размер записи runtime превысил ограничение",
        ));
    }
    Ok(bytes)
}

/// Сравнивает обычный ограниченный файл назначения с ожидаемыми байтами
/// потоково, без второй копии состояния в памяти. Проверки типа, размера и
/// `NOFOLLOW` совпадают с проверками перед атомарной заменой.
fn compare_existing(
    parent: &File,
    name: &str,
    bytes: &[u8],
    maximum: u64,
) -> Result<ExistingState, AssetError> {
    validate_leaf_name(name)?;
    let Some(mut file) = open_regular_runtime_file(parent, name, maximum)? else {
        return Ok(ExistingState::Missing);
    };
    let metadata = file
        .metadata()
        .map_err(|error| AssetError::io("метаданные runtime-файла", error))?;
    if metadata.len() != bytes.len() as u64 {
        return Ok(ExistingState::Different);
    }
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_usize;
    loop {
        let read = match inject_fault(FaultPoint::CompareRead).and_then(|()| file.read(&mut buffer))
        {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(AssetError::io("сравнение runtime-файла", error)),
        };
        if read == 0 {
            break;
        }
        let end = offset + read;
        if end > bytes.len() || buffer[..read] != bytes[offset..end] {
            return Ok(ExistingState::Different);
        }
        offset = end;
    }
    if offset != bytes.len() {
        return Ok(ExistingState::Different);
    }
    let identity = FileIdentity::of(
        &file
            .metadata()
            .map_err(|error| AssetError::io("метаданные runtime-файла", error))?,
    );
    // Файл менялся во время чтения: прочитанные байты не описывают один inode.
    if identity != FileIdentity::of(&metadata) {
        return Ok(ExistingState::Different);
    }
    Ok(ExistingState::Identical { file, identity })
}

/// Делает надёжными данные уже существующего файла и запись каталога о нём:
/// это то же постусловие, что даёт атомарная публикация, без новых данных.
fn confirm_durability(file: &File, parent: &File) -> Result<(), AssetError> {
    record_fs_operation(|counts| counts.file_syncs += 1);
    inject_fault(FaultPoint::ConfirmFileSync)
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("синхронизация runtime-файла", error))?;
    record_fs_operation(|counts| counts.directory_syncs += 1);
    inject_fault(FaultPoint::ConfirmDirectorySync)
        .and_then(|()| parent.sync_all())
        .map_err(|error| AssetError::io("синхронизация каталога runtime", error))
}

fn atomic_write(parent: &File, name: &str, bytes: &[u8], maximum: u64) -> Result<(), AssetError> {
    atomic_write_with_identity(parent, name, bytes, maximum).map(|_| ())
}

/// Атомарная запись, которая дополнительно возвращает отпечаток
/// опубликованного inode после синхронизации файла и каталога. `None` — запись
/// надёжна, но отпечаток получить не удалось.
fn atomic_write_with_identity(
    parent: &File,
    name: &str,
    bytes: &[u8],
    maximum: u64,
) -> Result<Option<FileIdentity>, AssetError> {
    validate_leaf_name(name)?;
    if bytes.len() as u64 > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "запись runtime превышает ограничение размера",
        ));
    }
    // Проверяем тип назначения без чтения старых данных перед атомарной заменой.
    let _existing = open_regular_runtime_file(parent, name, maximum)?;
    let temporary = format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let descriptor = openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(boundary_io)?;
    record_fs_operation(|counts| counts.temporary_files += 1);
    let result = (|| {
        let mut file = File::from(descriptor);
        file.write_all(bytes)
            .map_err(|error| AssetError::io("запись runtime-файла", error))?;
        record_fs_operation(|counts| counts.file_syncs += 1);
        inject_fault(FaultPoint::WriteFileSync)
            .and_then(|()| file.sync_all())
            .map_err(|error| AssetError::io("синхронизация runtime-файла", error))?;
        record_fs_operation(|counts| counts.renames += 1);
        renameat(parent, temporary.as_str(), parent, name).map_err(boundary_io)?;
        record_fs_operation(|counts| counts.directory_syncs += 1);
        inject_fault(FaultPoint::WriteDirectorySync)
            .and_then(|()| parent.sync_all())
            .map_err(|error| AssetError::io("синхронизация каталога runtime", error))?;
        // `ctime` меняется при переименовании, поэтому отпечаток снимается после него.
        Ok(file
            .metadata()
            .ok()
            .map(|metadata| FileIdentity::of(&metadata)))
    })();
    if result.is_err() {
        let _ = unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn atomic_write_if_absent(
    parent: &File,
    name: &str,
    bytes: &[u8],
    maximum: u64,
) -> Result<bool, AssetError> {
    validate_leaf_name(name)?;
    if bytes.len() as u64 > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "запись runtime превышает ограничение размера",
        ));
    }
    let temporary = format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let descriptor = openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(boundary_io)?;
    let result = (|| {
        let mut file = File::from(descriptor);
        file.write_all(bytes)
            .map_err(|error| AssetError::io("запись runtime-файла", error))?;
        file.sync_all()
            .map_err(|error| AssetError::io("синхронизация runtime-файла", error))?;
        match renameat_with(
            parent,
            temporary.as_str(),
            parent,
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                parent
                    .sync_all()
                    .map_err(|error| AssetError::io("синхронизация каталога runtime", error))?;
                Ok(true)
            }
            Err(error) if error == rustix::io::Errno::EXIST => Ok(false),
            Err(error) => Err(boundary_io(error)),
        }
    })();
    if matches!(&result, Ok(false) | Err(_)) {
        let _ = unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

pub(crate) fn validate_batch_id(batch_id: &str) -> Result<(), AssetError> {
    if batch_id.is_empty()
        || batch_id.len() > 128
        || !batch_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(invalid(
            "batch_id должен содержать не более 128 символов из a-z, A-Z, 0-9, дефиса и подчёркивания",
        ));
    }
    Ok(())
}

pub(crate) fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "SHA-256 должен содержать 64 строчных шестнадцатеричных символа",
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Счётчик чтения blob-файлов для проверок ограниченного числа обращений к диску.
    pub(crate) static REFERENCED_BLOB_READS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    /// Счётчики операций записи и синхронизации для контрактных тестов `save`.
    pub(crate) static FS_OPERATIONS: std::cell::Cell<FsOperationCounts> =
        const { std::cell::Cell::new(FsOperationCounts::ZERO) };
    /// Взведённые одноразовые отказы ввода-вывода для регрессионных тестов `save`.
    pub(crate) static INJECTED_FAULTS: std::cell::RefCell<Vec<FaultPoint>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Места сохранения состояния, где тест может вызвать отказ ввода-вывода,
/// не воспроизводимый настройкой прав доступа. В рабочей сборке отказы не
/// внедряются.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultPoint {
    /// Чтение назначения при побайтовом сравнении.
    CompareRead,
    /// `fsync` существующего файла при подтверждении надёжности.
    ConfirmFileSync,
    /// `fsync` каталога при подтверждении надёжности.
    ConfirmDirectorySync,
    /// `fsync` временного файла до `renameat`.
    WriteFileSync,
    /// `fsync` каталога после `renameat`.
    WriteDirectorySync,
}

/// Срабатывает один раз для каждой взведённой точки.
#[cfg(test)]
fn inject_fault(point: FaultPoint) -> std::io::Result<()> {
    INJECTED_FAULTS.with(|faults| {
        let mut faults = faults.borrow_mut();
        match faults.iter().position(|armed| *armed == point) {
            Some(index) => {
                faults.remove(index);
                Err(std::io::Error::other(format!("внедрённый отказ {point:?}")))
            }
            None => Ok(()),
        }
    })
}

#[cfg(not(test))]
#[inline(always)]
fn inject_fault(_point: FaultPoint) -> std::io::Result<()> {
    Ok(())
}

/// Действия с файловой системой, которыми тесты отличают запись от сохранения без записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FsOperationCounts {
    pub temporary_files: usize,
    pub renames: usize,
    pub file_syncs: usize,
    pub directory_syncs: usize,
}

#[cfg(test)]
impl FsOperationCounts {
    pub const ZERO: Self = Self {
        temporary_files: 0,
        renames: 0,
        file_syncs: 0,
        directory_syncs: 0,
    };
}

#[cfg(test)]
fn record_fs_operation(update: impl FnOnce(&mut FsOperationCounts)) {
    FS_OPERATIONS.with(|cell| {
        let mut counts = cell.get();
        update(&mut counts);
        cell.set(counts);
    });
}

#[cfg(not(test))]
fn record_fs_operation(_update: impl FnOnce(&mut FsOperationCounts)) {}

fn path_error() -> AssetError {
    AssetError::new(
        ErrorCode::PathTraversal,
        "ссылка на blob runtime должна совпадать с путём, вычисленным по SHA-256",
    )
}

fn boundary_io(error: rustix::io::Errno) -> AssetError {
    if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        AssetError::new(
            ErrorCode::BoundaryViolation,
            "путь runtime содержит символическую ссылку или ведёт не к каталогу либо обычному файлу",
        )
    } else {
        AssetError::io(
            "файловая система runtime пакета",
            std::io::Error::from(error),
        )
    }
}
