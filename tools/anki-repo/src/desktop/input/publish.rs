//! Доверенный владелец input. Перед переключением надёжно сохраняет намерение публикации;
//! после сбоя дочитывает его, проверяет поколение и завершает тот же record_input.
use super::source::{PendingJob, VerifiedExport, read_export};
use crate::exchange::{
    Confirmation, ExchangeError, ExchangeStore, InputRevision, Result, Snapshot,
    filesystem::Directory,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, path::Path};

const OWNER: &str = "anki-repo-trusted-input-v1";

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
    fault: std::cell::Cell<Option<&'static str>>,
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
            clean_temporaries(&input, &[".owner.json"])?;
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
        self.verify_location()?;
        let snapshot = export.snapshot();
        let key = snapshot.identity().key();
        let (dir, _) = self.input.ensure_child(&key)?;
        dir.require_private()?;
        if dir.names()?.iter().any(|n| n == "transaction.json") {
            return Err(corrupt("сначала выполните recovery"));
        }
        let state = self.exchange.read()?;
        let previous = state.streams.get(&key).map(|s| s.current.clone());
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
            return Ok(
                json!({"state":"published","name":snapshot.name(),"identity":snapshot.identity(),
                "revision":snapshot.revision(),"source_sequence":receipt.sequence,"evidence":receipt.evidence,
                "content_changed":false,"files_changed":false,"recovered":true,"inventory":receipt.inventory,
                "input_directory":format!("decks/input/{key}/current")}),
            );
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
        };
        // С этого момента любой сбой восстанавливается только из этого intent.
        dir.publish("transaction.json", &encode_transaction(&tx)?)?;
        #[cfg(test)]
        self.checkpoint("intent")?;
        self.complete(&dir, &tx)
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
        let mut recorded = false;
        for _ in 0..8 {
            let state = self.check_exchange(tx)?;
            let confirmation =
                Confirmation::from_trusted_owner(tx.snapshot.clone(), tx.sequence, &tx.evidence)?;
            match self.exchange.record_input(&state.token, confirmation) {
                Ok(_) => {
                    recorded = true;
                    break;
                }
                Err(ExchangeError::StaleState) => continue,
                Err(e) => return Err(e),
            }
        }
        if !recorded {
            return Err(ExchangeError::StaleState);
        }
        self.verify_location()?;
        if !dir.is_same_child(&self.input, &key)? || !current.is_same_child(dir, "current")? {
            // Intent и previous сохраняются, даже если метаданные уже записаны.
            return Err(corrupt("поколение перемещено после record_input"));
        }
        #[cfg(test)]
        self.checkpoint("recorded")?;
        dir.publish("receipt.json", &encode_transaction(tx)?)?;
        #[cfg(test)]
        self.checkpoint("receipt")?;
        dir.remove_tree("previous")?;
        dir.remove_file("transaction.json")?;
        self.verify_location()?;
        Ok(
            json!({"state":"published", "name":tx.snapshot.name(),"identity":tx.snapshot.identity(),
            "revision":tx.snapshot.revision(),"source_sequence":tx.sequence,"evidence":tx.evidence,
            "content_changed":!tx.unchanged,"files_changed":tx.files_changed.unwrap_or(!tx.unchanged),"input_directory":format!("decks/input/{key}/current"),
            "inventory":tx.inventory}),
        )
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
    fn checkpoint(&self, step: &'static str) -> Result<()> {
        if self.fault.get() == Some(step) {
            self.fault.set(None);
            return Err(
                std::io::Error::other(format!("injected durability failure after {step}")).into(),
            );
        }
        Ok(())
    }
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

fn clean_temporaries(directory: &Directory, targets: &[&str]) -> Result<()> {
    for name in directory.names()? {
        if targets.iter().any(|target| {
            name.strip_prefix(&format!(".{target}.tmp-"))
                .is_some_and(|suffix| {
                    let parts: Vec<_> = suffix.split('-').collect();
                    parts.len() == 3
                        && parts
                            .iter()
                            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                })
        }) {
            directory.remove_file(&name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
