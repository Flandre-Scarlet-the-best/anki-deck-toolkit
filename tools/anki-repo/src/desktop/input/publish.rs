//! Доверенный владелец input. Перед переключением надёжно сохраняет намерение публикации;
//! после сбоя дочитывает его, проверяет поколение и завершает тот же record_input.
use super::source::{PendingJob, VerifiedExport, read_export};
use crate::exchange::{
    Confirmation, DeckIdentity, ExchangeError, ExchangeStore, InputRevision, Result, Snapshot,
    filesystem::Directory,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, path::Path};

const OWNER: &str = "anki-repo-trusted-input-v1";
/// Рабочая область владельца подготовки/импорта output рядом с input.
pub(super) const WORK: &str = ".roundtrip";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    owner: String,
    jobs: BTreeMap<String, PendingJob>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    rejected: BTreeMap<String, RejectedJob>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectedJob {
    job: PendingJob,
    reason: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    snapshot: Snapshot,
    sequence: u64,
    evidence: String,
    inventory: BTreeMap<String, String>,
    previous: Option<InputRevision>,
    unchanged: bool,
    // None сохраняет смысл и checksum ранее записанных намерений.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    files_changed: Option<bool>,
    /// Только для readback после импорта output: фиксация идёт через
    /// `record_published` того же разрешения, а не через `record_input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication: Option<PublicationRecord>,
}

/// Что именно было отправлено в Anki и на каком C это основано. Разрешение
/// `Publication` не сериализуется: после сбоя exchange выдаёт его заново, и оно
/// обязано совпасть с этими полями, иначе публикация не подтверждается.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicationRecord {
    pub working_revision: String,
    pub checked_input_sequence: u64,
    pub base_sequence: u64,
    pub import_job: String,
}

/// Узкая доверенная передача подтверждённого C подготовителю output.
/// Каталог `current` открыт под блокировкой владельца input; содержимое ещё
/// не прочитано: копирующий обязан сверить каждый байт с `inventory`.
pub(super) struct ConfirmedSource {
    pub current: Directory,
    pub snapshot: Snapshot,
    pub source_sequence: u64,
    pub inventory: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionEnvelope {
    checksum: String,
    transaction: Transaction,
}

pub(super) struct Store {
    root: Directory,
    decks: Directory,
    input: Directory,
    exchange: ExchangeStore,
    control: Control,
    _lock: File,
    #[cfg(test)]
    pub(super) fault: std::cell::Cell<Option<&'static str>>,
}

impl Store {
    pub fn open(repository: &Path) -> Result<Self> {
        Self::open_mode(repository, true)
    }

    pub fn open_existing(repository: &Path) -> Result<Self> {
        Self::open_mode(repository, false)
    }

    fn open_mode(repository: &Path, initialize: bool) -> Result<Self> {
        crate::layout::guard_local_path(repository, "корень доверенного экспортёра")?;
        let root = Directory::open(repository)?;
        let decks = if initialize {
            root.ensure_child("decks")?.0
        } else {
            root.child("decks")?.ok_or_else(|| corrupt("нет decks"))?
        };
        let parent_lock = decks.try_lock()?;
        let input = if initialize {
            decks.ensure_child("input")?.0
        } else {
            decks.child("input")?.ok_or_else(|| corrupt("нет input"))?
        };
        input.require_private()?;
        drop(parent_lock);
        let lock = input.try_lock()?;
        if initialize {
            clean_temporaries(&input, &[".owner.json", ".new-stream.json"])?;
        }
        let names = input.names()?;
        let control = if names.is_empty() && initialize {
            let control = Control {
                owner: OWNER.into(),
                jobs: BTreeMap::new(),
                rejected: BTreeMap::new(),
            };
            input.publish(
                ".owner.json",
                &serde_json::to_vec(&control).map_err(corrupt)?,
            )?;
            control
        } else {
            let control: Control =
                serde_json::from_slice(&input.read(".owner.json")?).map_err(corrupt)?;
            if control.owner != OWNER {
                return Err(ExchangeError::CorruptState(
                    "владелец input не совпадает".into(),
                ));
            }
            control
        };
        let mut job_ids = std::collections::BTreeSet::new();
        for (key, job) in &control.jobs {
            if *key != job.key()
                || !valid_key(&job.id)
                || !valid_key(&job.profile_id)
                || !job_ids.insert(&job.id)
            {
                return Err(corrupt(
                    "индекс pending job повреждён; повторный export запрещён",
                ));
            }
        }
        for (id, rejected) in &control.rejected {
            if id != &rejected.job.id
                || !valid_key(id)
                || !valid_key(&rejected.job.profile_id)
                || !job_ids.insert(id)
                || rejected.reason.is_empty()
            {
                return Err(corrupt("журнал отвергнутых заданий повреждён"));
            }
        }
        let exchange = if initialize {
            ExchangeStore::initialize(repository)?
        } else {
            ExchangeStore::open(repository)?
        };
        let store = Self {
            root,
            decks,
            input,
            exchange,
            control,
            _lock: lock,
            #[cfg(test)]
            fault: std::cell::Cell::new(None),
        };
        store.verify_location()?;
        Ok(store)
    }

    fn verify_location(&self) -> Result<()> {
        if !self.decks.is_same_child(&self.root, "decks")?
            || !self.input.is_same_child(&self.decks, "input")?
        {
            return Err(ExchangeError::UnsafePath("каталог input подменён".into()));
        }
        self.input.require_private()
    }

    pub fn pending(&self, key: &str) -> Option<&PendingJob> {
        self.control.jobs.get(key)
    }

    pub fn pending_named(&self, profile: &str, name: &str) -> Vec<&PendingJob> {
        self.control
            .jobs
            .values()
            .filter(|job| job.profile_id == profile && job.deck == name)
            .collect()
    }

    pub fn reject_job(&mut self, key: &str, reason: &str) -> Result<()> {
        self.verify_location()?;
        let job = self
            .control
            .jobs
            .get(key)
            .ok_or_else(|| corrupt("нет отвергаемого задания"))?
            .clone();
        let mut next = self.control.clone();
        next.jobs.remove(key);
        next.rejected.insert(
            job.id.clone(),
            RejectedJob {
                job,
                reason: reason.into(),
            },
        );
        #[cfg(test)]
        self.checkpoint("control")?;
        self.input
            .publish(".owner.json", &serde_json::to_vec(&next).map_err(corrupt)?)?;
        self.control = next;
        Ok(())
    }

    pub fn save_job(&mut self, key: &str, job: PendingJob) -> Result<()> {
        self.verify_location()?;
        if key != job.key() {
            return Err(corrupt("ключ pending не соответствует источнику"));
        }
        self.control.jobs.insert(key.into(), job);
        self.save_control()
    }

    pub fn finish_job(&mut self, key: &str) -> Result<()> {
        self.verify_location()?;
        self.control.jobs.remove(key);
        self.save_control()
    }

    fn save_control(&self) -> Result<()> {
        self.input.publish(
            ".owner.json",
            &serde_json::to_vec(&self.control).map_err(corrupt)?,
        )
    }

    /// Узкая диагностика владельца: только проверенные хеши/идентичности.
    /// Ни JSON, ни поля карточек, ни media через этот API не возвращаются.
    pub fn audit(&self) -> Result<Value> {
        self.verify_location()?;
        let state = self.exchange.read()?;
        let mut streams = Vec::new();
        if self.input.names()?.iter().any(|name| {
            name == ".new-stream.json" || is_temporary(name, &[".owner.json", ".new-stream.json"])
        }) {
            return Err(ExchangeError::Blocked("recovery_required".into()));
        }
        for name in self.input.names()? {
            if name == ".owner.json" {
                continue;
            }
            if !valid_key(&name) {
                return Err(corrupt("неизвестный объект input"));
            }
            let dir = self
                .input
                .child(&name)?
                .ok_or_else(|| corrupt("исчез поток"))?;
            dir.require_private()?;
            let names = dir.names()?;
            if names.iter().any(|n| n == "transaction.json") {
                return Err(ExchangeError::Blocked("recovery_required".into()));
            }
            if !state.streams.contains_key(&name) {
                return Err(corrupt("каталог input не зарегистрирован в exchange"));
            }
            if names != ["current", "receipt.json"] {
                return Err(corrupt("неучтённый объект внутри потока input"));
            }
        }
        for (key, stream) in &state.streams {
            let dir = self
                .input
                .child(key)?
                .ok_or_else(|| corrupt("нет каталога подтверждённого потока"))?;
            let names = dir.names()?;
            if names.iter().any(|n| n == "transaction.json") {
                return Err(ExchangeError::Blocked(
                    "recovery_required: повторите sync-input --apply".into(),
                ));
            }
            let tx = read_transaction(&dir, "receipt.json")?;
            if tx.snapshot.identity().key() != *key
                || tx.snapshot != stream.current.snapshot
                || tx.sequence != stream.current.source_sequence
                || tx.evidence != stream.current.evidence
            {
                return Err(corrupt("receipt и exchange не согласованы"));
            }
            let current = dir
                .child("current")?
                .ok_or_else(|| corrupt("нет current"))?;
            verify_generation(&current, &tx)?;
            streams.push(
                json!({"identity":tx.snapshot.identity(),"revision":tx.snapshot.revision(),
                "source_sequence":tx.sequence,"evidence":tx.evidence,"inventory":tx.inventory,
                "base_revision":stream.base.as_ref().map(|r| r.snapshot.revision()),
                "output_directory":stream.output_directory}),
            );
        }
        Ok(
            json!({"state":"succeeded","generation":state.generation,"streams":streams,
            "pending_jobs":self.control.jobs.len(),"rejected_jobs":self.control.rejected.iter().map(|(id, rejected)| json!({"job_id":id,"reason":rejected.reason})).collect::<Vec<_>>()}),
        )
    }

    /// Завершает все сохранённые intent до отправки новых запросов в Anki.
    pub fn recover(&self) -> Result<Vec<Value>> {
        self.verify_location()?;
        let mut outcomes = Vec::new();
        if let Some(outcome) = self.recover_new_stream()? {
            outcomes.push(outcome);
        }
        let state = self.exchange.read()?;
        for key in self.input.names()? {
            if key == ".owner.json" {
                continue;
            }
            if !valid_key(&key) {
                return Err(ExchangeError::CorruptState(
                    "неизвестный объект в input".into(),
                ));
            }
            let dir = self
                .input
                .child(&key)?
                .ok_or_else(|| corrupt("исчез поток"))?;
            dir.require_private()?;
            if !state.streams.contains_key(&key)
                && !dir.names()?.iter().any(|n| n == "transaction.json")
            {
                return Err(corrupt("каталог input не зарегистрирован в exchange"));
            }
            clean_temporaries(&dir, &["receipt.json", "transaction.json"])?;
            let names = dir.names()?;
            if names.iter().any(|n| n == "transaction.json") {
                let tx = read_transaction(&dir, "transaction.json")?;
                if tx.snapshot.identity().key() != key {
                    return Err(corrupt("идентичность intent"));
                }
                outcomes.push(self.complete(&dir, &tx)?);
            } else if names.iter().any(|n| n == "previous") {
                return Err(corrupt(
                    "previous без intent; автоматический сброс запрещён",
                ));
            }
            // stage без intent никогда не был подтверждён и не затрагивал current.
            dir.remove_tree("stage")?;
        }
        Ok(outcomes)
    }

    pub fn publish(&self, export: &VerifiedExport) -> Result<Value> {
        self.publish_with(export, None)
    }

    /// Readback после импорта: те же staging, intent и recovery, но завершение
    /// через `record_published`. Новый поток так не создаётся.
    pub fn publish_readback(
        &self,
        export: &VerifiedExport,
        publication: PublicationRecord,
    ) -> Result<Value> {
        let key = export.snapshot().identity().key();
        if !self.exchange.read()?.streams.contains_key(&key) {
            return Err(ExchangeError::Blocked("нет потока публикации".into()));
        }
        self.publish_with(export, Some(publication))
    }

    fn publish_with(
        &self,
        export: &VerifiedExport,
        publication: Option<PublicationRecord>,
    ) -> Result<Value> {
        self.verify_location()?;
        let snapshot = export.snapshot();
        let key = snapshot.identity().key();
        if self.input.names()?.iter().any(|n| n == ".new-stream.json") {
            return Err(ExchangeError::Blocked(
                "сначала восстановите первую публикацию".into(),
            ));
        }
        let state = self.exchange.read()?;
        let previous = state.streams.get(&key).map(|s| s.current.clone());
        let new_stream = previous.is_none();
        if new_stream {
            if self.input.child(&key)?.is_some() {
                return Err(corrupt("каталог input не зарегистрирован в exchange"));
            }
            // Намерение создания сохраняется ДО mkdir: только оно доказывает
            // владение ещё не зарегистрированным каталогом после перезапуска.
            let reservation = Transaction {
                snapshot: snapshot.clone(),
                sequence: 1,
                evidence: export.evidence().into(),
                inventory: export.inventory().clone(),
                previous: None,
                unchanged: false,
                files_changed: Some(true),
                publication: None,
            };
            self.input
                .publish(".new-stream.json", &encode_transaction(&reservation)?)?;
            #[cfg(test)]
            self.checkpoint("reserved")?;
        }
        let (dir, _) = self.input.ensure_child(&key)?;
        dir.require_private()?;
        if dir.names()?.iter().any(|n| n == "transaction.json") {
            return Err(corrupt("сначала выполните recovery"));
        }
        #[cfg(test)]
        if new_stream {
            self.checkpoint("created")?;
        }
        // Повтор того же задания (в том числе readback после неизвестного итога)
        // не выдаёт новую ревизию тем же байтам.
        if previous
            .as_ref()
            .is_some_and(|p| p.snapshot == *snapshot && p.evidence == export.evidence())
        {
            let receipt = read_transaction(&dir, "receipt.json")?;
            if receipt.snapshot != *snapshot
                || receipt.evidence != export.evidence()
                || &receipt.inventory != export.inventory()
                || previous
                    .as_ref()
                    .is_none_or(|p| p.source_sequence != receipt.sequence)
            {
                return Err(corrupt(
                    "receipt повторного задания не совпадает с exchange",
                ));
            }
            let current = dir
                .child("current")?
                .ok_or_else(|| corrupt("нет current"))?;
            verify_generation(&current, &receipt)?;
            let mut outcome = json!({"state":"published","name":snapshot.name(),"identity":snapshot.identity(),
                "revision":snapshot.revision(),"source_sequence":receipt.sequence,"evidence":receipt.evidence,
                "content_changed":false,"files_changed":false,"recovered":true,"inventory":receipt.inventory,
                "input_directory":format!("decks/input/{key}/current")});
            if let Some(publication) = &publication {
                // Истину о подтверждении даёт сохранённый published, как в `record`.
                let confirmed = state.streams[&key].published.as_ref().is_some_and(|p| {
                    p.confirmed_sequence == receipt.sequence
                        && p.evidence == receipt.evidence
                        && p.working_revision == publication.working_revision
                });
                outcome["publication_confirmed"] = json!(confirmed);
            }
            return Ok(outcome);
        }
        let sequence = previous
            .as_ref()
            .map_or(Some(1), |p| p.source_sequence.checked_add(1))
            .ok_or_else(|| corrupt("переполнение source_sequence"))?;
        let unchanged = previous
            .as_ref()
            .is_some_and(|p| p.snapshot.revision() == snapshot.revision());
        let files_changed = if let Some(prior) = &previous {
            let receipt = read_transaction(&dir, "receipt.json")?;
            let current = dir
                .child("current")?
                .ok_or_else(|| corrupt("нет предыдущего полного экспорта"))?;
            if receipt.snapshot != prior.snapshot
                || receipt.sequence != prior.source_sequence
                || receipt.evidence != prior.evidence
            {
                return Err(corrupt("receipt отличается от exchange"));
            }
            verify_generation(&current, &receipt)?;
            &receipt.inventory != export.inventory()
        } else {
            true
        };
        let inventory = if !files_changed {
            export.inventory().clone()
        } else {
            dir.remove_tree("stage")?;
            let (stage, _) = dir.ensure_child("stage")?;
            for (name, bytes) in export.files() {
                if let Some(media) = name.strip_prefix("media/") {
                    let (media_dir, _) = stage.ensure_child("media")?;
                    media_dir.publish(media, bytes)?;
                } else {
                    stage.publish(name, bytes)?;
                }
                #[cfg(test)]
                if new_stream {
                    self.checkpoint("writing")?;
                }
            }
            stage.sync()?;
            dir.sync()?;
            #[cfg(test)]
            self.checkpoint("staged")?;
            let (checked, _, inventory) = read_export(&stage, snapshot.identity().profile.clone())?;
            if checked != *snapshot || &inventory != export.inventory() {
                return Err(corrupt("staging не совпал с проверенными байтами"));
            }
            inventory
        };
        let tx = Transaction {
            snapshot: snapshot.clone(),
            sequence,
            evidence: export.evidence().into(),
            inventory,
            previous,
            unchanged,
            files_changed: Some(files_changed),
            publication,
        };
        // С этого момента любой сбой восстанавливается только из этого intent.
        dir.publish("transaction.json", &encode_transaction(&tx)?)?;
        #[cfg(test)]
        self.checkpoint("intent")?;
        let outcome = self.complete(&dir, &tx)?;
        #[cfg(test)]
        if new_stream {
            self.checkpoint("new_committed")?;
        }
        if new_stream {
            self.input.remove_file(".new-stream.json")?;
        }
        Ok(outcome)
    }

    fn recover_new_stream(&self) -> Result<Option<Value>> {
        if !self.input.names()?.iter().any(|n| n == ".new-stream.json") {
            return Ok(None);
        }
        let tx = read_transaction(&self.input, ".new-stream.json")?;
        if tx.previous.is_some()
            || tx.sequence != 1
            || tx.unchanged
            || tx.files_changed != Some(true)
        {
            return Err(corrupt("неверное намерение создания потока"));
        }
        let state = self.check_exchange(&tx)?;
        let key = tx.snapshot.identity().key();
        let Some(dir) = self.input.child(&key)? else {
            if state.streams.contains_key(&key) {
                return Err(corrupt("нет каталога подтверждённого потока"));
            }
            self.input.remove_file(".new-stream.json")?;
            return Ok(None);
        };
        dir.require_private()?;
        let names = dir.names()?;
        let mut outcome = None;
        if names
            .iter()
            .any(|n| matches!(n.as_str(), "transaction.json" | "receipt.json"))
        {
            if names.iter().any(|n| {
                !matches!(
                    n.as_str(),
                    "stage" | "current" | "previous" | "transaction.json" | "receipt.json"
                )
            }) {
                return Err(corrupt("неучтённые данные первой публикации"));
            }
            for name in ["transaction.json", "receipt.json"] {
                if names.iter().any(|n| n == name)
                    && transaction_hash(&read_transaction(&dir, name)?)? != transaction_hash(&tx)?
                {
                    return Err(corrupt("несогласованные намерения первой публикации"));
                }
            }
            if names.iter().any(|n| n == "transaction.json") {
                outcome = Some(self.complete(&dir, &tx)?);
            } else {
                let current = dir
                    .child("current")?
                    .ok_or_else(|| corrupt("нет current"))?;
                verify_generation(&current, &tx)?;
                let state = self.exchange.read()?;
                if state.streams.get(&key).is_none_or(|stream| {
                    stream.current.snapshot != tx.snapshot
                        || stream.current.source_sequence != tx.sequence
                        || stream.current.evidence != tx.evidence
                }) {
                    return Err(ExchangeError::StaleState);
                }
                if names != ["current", "receipt.json"] {
                    return Err(corrupt("лишние данные завершённой первой публикации"));
                }
                outcome = Some(published(&tx));
            }
        } else {
            if names.iter().any(|n| n != "stage") {
                return Err(corrupt("неучтённые данные первой публикации"));
            }
            if let Some(stage) = dir.child("stage")?
                && verify_partial_generation(&stage, &tx)?
            {
                verify_generation(&stage, &tx)?;
                dir.publish("transaction.json", &encode_transaction(&tx)?)?;
                outcome = Some(self.complete(&dir, &tx)?);
            }
            if outcome.is_none() {
                // Удаляется только зарезервированная нами незавершённая запись,
                // каждый имеющийся файл которой доказан полным хешем inventory.
                if !dir.is_same_child(&self.input, &key)? {
                    return Err(corrupt("подмена потока"));
                }
                self.input.remove_tree(&key)?;
            }
        }
        self.input.remove_file(".new-stream.json")?;
        Ok(outcome)
    }

    fn complete(&self, dir: &Directory, tx: &Transaction) -> Result<Value> {
        self.verify_location()?;
        let key = tx.snapshot.identity().key();
        if !dir.is_same_child(&self.input, &key)? {
            return Err(corrupt("подмена потока"));
        }
        // Сначала проверяем CAS-происхождение, ещё до переключения каталогов.
        self.check_exchange(tx)?;
        if tx.files_changed.unwrap_or(!tx.unchanged) && dir.child("stage")?.is_some() {
            let stage = dir.child("stage")?.expect("checked stage");
            verify_generation(&stage, tx)?;
            if dir.child("current")?.is_some() {
                if dir.child("previous")?.is_some() {
                    return Err(corrupt("лишний previous"));
                }
                dir.rename_child("current", "previous")?;
                #[cfg(test)]
                self.checkpoint("displaced")?;
            }
            dir.rename_child("stage", "current")?;
            #[cfg(test)]
            self.checkpoint("visible")?;
        }
        let current = dir
            .child("current")?
            .ok_or_else(|| corrupt("нет проверяемого поколения current"))?;
        verify_generation(&current, tx)?;
        self.verify_location()?;
        if !dir.is_same_child(&self.input, &key)? || !current.is_same_child(dir, "current")? {
            return Err(corrupt("поколение подменено перед record_input"));
        }
        // record_input может успешно выполнить rename, но вернуть ошибку fsync.
        // Следующая попытка читает state и распознаёт тот же sequence/evidence.
        let mut recorded = None;
        for _ in 0..8 {
            let state = self.check_exchange(tx)?;
            match self.record(tx, &state) {
                Ok(confirmed) => {
                    recorded = Some(confirmed);
                    break;
                }
                Err(ExchangeError::StaleState) => continue,
                Err(e) => return Err(e),
            }
        }
        let Some(publication_confirmed) = recorded else {
            return Err(ExchangeError::StaleState);
        };
        self.verify_location()?;
        if !dir.is_same_child(&self.input, &key)? || !current.is_same_child(dir, "current")? {
            // Intent и previous сохраняются, даже если метаданные уже записаны.
            return Err(corrupt("поколение перемещено после record_input"));
        }
        #[cfg(test)]
        self.checkpoint("recorded")?;
        // Receipt описывает фактически сохранённые байты и C. Неподтверждённая
        // публикация не оставляет в нём записи об отправленном B.
        let mut receipt = tx.clone();
        if publication_confirmed == Some(false) {
            receipt.publication = None;
        }
        dir.publish("receipt.json", &encode_transaction(&receipt)?)?;
        #[cfg(test)]
        self.checkpoint("receipt")?;
        dir.remove_tree("previous")?;
        dir.remove_file("transaction.json")?;
        self.verify_location()?;
        let mut outcome = published(tx);
        if let Some(confirmed) = publication_confirmed {
            outcome["publication_confirmed"] = json!(confirmed);
        }
        Ok(outcome)
    }

    /// `Some(true)` — exchange подтвердил публикацию B; `Some(false)` — тот же
    /// readback сохранён лишь как наблюдённый C; `None` — обычный вход.
    fn record(&self, tx: &Transaction, state: &crate::exchange::StateView) -> Result<Option<bool>> {
        let key = tx.snapshot.identity().key();
        let confirmation =
            Confirmation::from_trusted_owner(tx.snapshot.clone(), tx.sequence, &tx.evidence)?;
        let Some(publication) = &tx.publication else {
            self.exchange.record_input(&state.token, confirmation)?;
            return Ok(None);
        };
        let stream = state.streams.get(&key);
        let already = stream.is_some_and(|s| {
            s.current.source_sequence == tx.sequence
                && s.current.snapshot == tx.snapshot
                && s.current.evidence == tx.evidence
        });
        if already {
            // Повтор после неизвестного итога: истину даёт сохранённый published.
            let confirmed = stream.and_then(|s| s.published.as_ref()).is_some_and(|p| {
                p.confirmed_sequence == tx.sequence
                    && p.evidence == tx.evidence
                    && p.working_revision == publication.working_revision
            });
            return Ok(Some(confirmed));
        }
        let permit = match self.exchange.plan_publication(tx.snapshot.identity()) {
            Ok(permit)
                if permit.working_revision() == publication.working_revision
                    && permit.source_sequence() == publication.checked_input_sequence
                    && permit.base_sequence() == publication.base_sequence =>
            {
                Some(permit)
            }
            Ok(_) | Err(ExchangeError::Blocked(_)) => None,
            Err(e) => return Err(e),
        };
        if let Some(permit) = permit {
            #[cfg(test)]
            self.checkpoint("permit")?;
            match self
                .exchange
                .record_published(&permit, confirmation.clone())
            {
                Ok(_) => return Ok(Some(true)),
                // B изменился после readback: подтверждение публикации невозможно.
                Err(ExchangeError::Blocked(_)) => {}
                Err(e) => return Err(e),
            }
        }
        // Readback честно фиксируется как наблюдённое состояние Anki без
        // ложного published; B и база не меняются.
        let state = self.check_exchange(tx)?;
        self.exchange.record_input(&state.token, confirmation)?;
        Ok(Some(false))
    }

    /// Проверенный C для подготовки output. Требует согласованных receipt и
    /// exchange без незавершённых intent; байты сверяет копирующий.
    pub fn confirmed_source(&self, identity: &DeckIdentity) -> Result<ConfirmedSource> {
        self.verify_location()?;
        if self.input.names()?.iter().any(|n| n == ".new-stream.json") {
            return Err(ExchangeError::Blocked("input_recovery_required".into()));
        }
        let key = identity.key();
        let state = self.exchange.read()?;
        let stream = state
            .streams
            .get(&key)
            .ok_or_else(|| ExchangeError::Blocked("нет подтверждённого входа".into()))?;
        let dir = self
            .input
            .child(&key)?
            .ok_or_else(|| corrupt("нет каталога подтверждённого потока"))?;
        dir.require_private()?;
        if dir.names()? != ["current", "receipt.json"] {
            return Err(ExchangeError::Blocked("input_recovery_required".into()));
        }
        let receipt = read_transaction(&dir, "receipt.json")?;
        if receipt.snapshot != stream.current.snapshot
            || receipt.sequence != stream.current.source_sequence
            || receipt.evidence != stream.current.evidence
        {
            return Err(corrupt("receipt и exchange не согласованы"));
        }
        let current = dir
            .child("current")?
            .ok_or_else(|| corrupt("нет current"))?;
        if !current.is_same_child(&dir, "current")? || !dir.is_same_child(&self.input, &key)? {
            return Err(corrupt("поколение подменено"));
        }
        Ok(ConfirmedSource {
            current,
            snapshot: receipt.snapshot,
            source_sequence: receipt.sequence,
            inventory: receipt.inventory,
        })
    }

    pub fn exchange(&self) -> &ExchangeStore {
        &self.exchange
    }

    pub fn decks(&self) -> &Directory {
        &self.decks
    }

    /// Приватная рабочая область подготовки/публикации output; создаётся явно.
    pub fn work_area(&self, create: bool) -> Result<Option<Directory>> {
        self.verify_location()?;
        let area = if create {
            Some(self.decks.ensure_child(WORK)?.0)
        } else {
            self.decks.child(WORK)?
        };
        if let Some(area) = &area {
            area.require_private()?;
            if !area.is_same_child(&self.decks, WORK)? {
                return Err(ExchangeError::UnsafePath(
                    "рабочая область подменена".into(),
                ));
            }
        }
        Ok(area)
    }

    /// Незавершённая публикация output блокирует обычный sync-input этой колоды:
    /// иначе свежий C сделал бы разрешение устаревшим до проверки readback.
    pub fn publication_pending(&self, identity_key: &str) -> Result<bool> {
        let Some(area) = self.work_area(false)? else {
            return Ok(false);
        };
        let Some(dir) = area.child(identity_key)? else {
            return Ok(false);
        };
        Ok(dir.names()?.iter().any(|n| n == "publish.json"))
    }

    fn check_exchange(&self, tx: &Transaction) -> Result<crate::exchange::StateView> {
        let state = self.exchange.read()?;
        let current = state
            .streams
            .get(&tx.snapshot.identity().key())
            .map(|s| &s.current);
        let intended = current.is_some_and(|r| {
            r.source_sequence == tx.sequence
                && r.snapshot == tx.snapshot
                && r.evidence == tx.evidence
        });
        if !intended && current != tx.previous.as_ref() {
            return Err(ExchangeError::StaleState);
        }
        if tx.sequence
            != tx
                .previous
                .as_ref()
                .map_or(Some(1), |p| p.source_sequence.checked_add(1))
                .ok_or_else(|| corrupt("sequence intent"))?
        {
            return Err(corrupt("неверная последовательность intent"));
        }
        Ok(state)
    }

    #[cfg(test)]
    pub(super) fn checkpoint(&self, step: &'static str) -> Result<()> {
        if self.fault.get() == Some(step) {
            self.fault.set(None);
            return Err(
                std::io::Error::other(format!("injected durability failure after {step}")).into(),
            );
        }
        Ok(())
    }
}

fn published(tx: &Transaction) -> Value {
    let key = tx.snapshot.identity().key();
    json!({"state":"published", "name":tx.snapshot.name(),"identity":tx.snapshot.identity(),
        "revision":tx.snapshot.revision(),"source_sequence":tx.sequence,"evidence":tx.evidence,
        "content_changed":!tx.unchanged,"files_changed":tx.files_changed.unwrap_or(!tx.unchanged),
        "input_directory":format!("decks/input/{key}/current"),"inventory":tx.inventory})
}

fn verify_partial_generation(directory: &Directory, tx: &Transaction) -> Result<bool> {
    let mut present = BTreeMap::new();
    for name in directory.names()? {
        if name == "media" {
            let media = directory
                .child("media")?
                .ok_or_else(|| corrupt("нет media"))?;
            for name in media.names()? {
                present.insert(
                    format!("media/{name}"),
                    asset_store::hashing::sha256_hex(media.read(&name)?),
                );
            }
            if !media.is_same_child(directory, "media")? {
                return Err(corrupt("подмена media"));
            }
        } else {
            present.insert(
                name.clone(),
                asset_store::hashing::sha256_hex(directory.read(&name)?),
            );
        }
    }
    if present
        .iter()
        .any(|(name, hash)| tx.inventory.get(name) != Some(hash))
    {
        return Err(corrupt("неучтённые байты неполного поколения"));
    }
    Ok(present == tx.inventory)
}

fn verify_generation(directory: &Directory, tx: &Transaction) -> Result<()> {
    let (snapshot, _, inventory) = read_export(directory, tx.snapshot.identity().profile.clone())?;
    if snapshot != tx.snapshot || inventory != tx.inventory {
        return Err(corrupt("байты поколения не совпадают с intent"));
    }
    Ok(())
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn corrupt(message: impl std::fmt::Display) -> ExchangeError {
    ExchangeError::CorruptState(message.to_string())
}

fn transaction_hash(tx: &Transaction) -> Result<String> {
    let value = serde_json::to_value(tx).map_err(corrupt)?;
    Ok(asset_store::hashing::sha256_hex(
        serde_json::to_vec(&value).map_err(corrupt)?,
    ))
}

fn encode_transaction(tx: &Transaction) -> Result<Vec<u8>> {
    serde_json::to_vec(&TransactionEnvelope {
        checksum: transaction_hash(tx)?,
        transaction: tx.clone(),
    })
    .map_err(corrupt)
}

fn read_transaction(dir: &Directory, name: &str) -> Result<Transaction> {
    let envelope: TransactionEnvelope =
        serde_json::from_slice(&dir.read(name)?).map_err(corrupt)?;
    if envelope.checksum != transaction_hash(&envelope.transaction)? {
        return Err(corrupt("контрольная сумма intent/receipt"));
    }
    let tx = &envelope.transaction;
    if tx.evidence.starts_with("desktop-export:") {
        let inventory_hash =
            asset_store::hashing::sha256_hex(serde_json::to_vec(&tx.inventory).map_err(corrupt)?);
        let parts: Vec<_> = tx.evidence.split(':').collect();
        if parts.len() != 5
            || !valid_key(parts[1])
            || parts[2] != tx.snapshot.identity().profile.as_str()
            || parts[3] != tx.snapshot.revision()
            || parts[4] != inventory_hash
        {
            return Err(corrupt(
                "свидетельство задания не соответствует полному inventory",
            ));
        }
    }
    Ok(envelope.transaction)
}

pub(super) fn is_temporary(name: &str, targets: &[&str]) -> bool {
    targets.iter().any(|target| {
        name.strip_prefix(&format!(".{target}.tmp-"))
            .is_some_and(|suffix| {
                let parts: Vec<_> = suffix.split('-').collect();
                parts.len() == 3
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            })
    })
}

fn clean_temporaries(directory: &Directory, targets: &[&str]) -> Result<()> {
    for name in directory.names()? {
        if is_temporary(&name, targets) {
            directory.remove_file(&name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
