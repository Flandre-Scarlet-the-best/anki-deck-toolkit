use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::filesystem::Directory;
use super::{
    Comparison, DeckIdentity, Disposition, ExchangeError, Result, Snapshot, compare, digest,
    valid_hash,
};

const STATE_FILE: &str = "revisions.json";
const MAGIC: &str = "anki-repo-exchange";
const VERSION: u32 = 1;

/// CAS-токен всего состояния, включая идентичность хранилища и поколение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevisionToken(String);

/// Только будущий доверенный адаптер вправе утверждать, что это подтверждённый
/// снимок Anki. Конструктор — явная граница доверия, не проверка живой Anki.
#[derive(Debug, Clone)]
pub struct Confirmation {
    snapshot: Snapshot,
    source_sequence: u64,
    evidence: String,
}

impl Confirmation {
    /// `source_sequence` монотонен для пары профиль/колода, переживает перезапуск
    /// адаптера. `evidence` — ссылка/ID результата реально завершённой операции.
    pub fn from_trusted_owner(
        snapshot: Snapshot,
        source_sequence: u64,
        evidence: impl Into<String>,
    ) -> Result<Self> {
        let evidence = evidence.into();
        check_evidence(source_sequence, &evidence)?;
        snapshot.verify()?;
        Ok(Self {
            snapshot,
            source_sequence,
            evidence,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRevision {
    pub source_sequence: u64,
    pub evidence: String,
    pub snapshot: Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRevision {
    pub identity: DeckIdentity,
    /// Ревизия B, действительно отправленная и затем подтверждённая readback.
    pub working_revision: String,
    pub based_on_sequence: u64,
    pub checked_input_sequence: u64,
    pub confirmed_sequence: u64,
    pub evidence: String,
}

/// Не история: сохраняются только C, база A и последнее подтверждение публикации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamState {
    /// Привязка сохраняется при переименовании колоды без копирования файлов.
    pub output_directory: String,
    pub current: InputRevision,
    pub base: Option<InputRevision>,
    pub published: Option<PublishedRevision>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateView {
    pub token: RevisionToken,
    pub generation: u64,
    pub streams: BTreeMap<String, StreamState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationAction {
    PrepareFirst,
    RefreshUnchanged,
    Keep,
    Blocked,
}

/// Проверяемое решение; оно ничего не копирует и не даёт права перезаписи без
/// повторной проверки исходника под общей блокировкой каталога экспорта.
#[derive(Debug, Clone, Serialize)]
pub struct Preparation {
    token: RevisionToken,
    identity: DeckIdentity,
    input_revision: String,
    action: PreparationAction,
    pub output_directory: String,
    pub observed_working_revision: Option<String>,
    pub comparison: Option<Comparison>,
    pub reason: Option<String>,
}

/// Проверка перед будущим импортом. Открытых методов изменения и десериализации нет.
#[derive(Debug, Clone, Serialize)]
pub struct Publication {
    token: RevisionToken,
    identity: DeckIdentity,
    source_sequence: u64,
    base_sequence: u64,
    working_revision: String,
}

impl Preparation {
    pub fn action(&self) -> PreparationAction {
        self.action
    }
}

impl Publication {
    pub fn working_revision(&self) -> &str {
        &self.working_revision
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    magic: String,
    version: u32,
    store_id: String,
    generation: u64,
    streams: BTreeMap<String, StreamState>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    checksum: String,
    state: State,
}

/// Владение только метаданными `decks/.exchange/`. `input` не открывается;
/// `output` только читается. Все операции заново читают состояние под flock.
pub struct ExchangeStore {
    root: Directory,
    decks: Directory,
    metadata: Directory,
}

impl ExchangeStore {
    /// Явная инициализация. Уже существующий пустой/повреждённый каталог не
    /// присваивается и не сбрасывается: это неполное состояние, требующее разбора.
    pub fn initialize(repository_root: &Path) -> Result<Self> {
        crate::layout::guard_local_path(repository_root, "корень обмена")?;
        let root = Directory::open(repository_root)?;
        let (decks, _) = root.ensure_child("decks")?;
        // Блокировка родителя закрывает гонку двух initialize до появления состояния.
        let lock = decks.lock()?;
        let (metadata, created) = decks.ensure_child(".exchange")?;
        if created {
            let mut random = [0_u8; 32];
            getrandom::fill(&mut random)
                .map_err(|e| ExchangeError::Blocked(format!("нет случайного ID: {e}")))?;
            let state = State {
                magic: MAGIC.into(),
                version: VERSION,
                store_id: asset_store::hashing::sha256_hex(random),
                generation: 0,
                streams: BTreeMap::new(),
            };
            save(&metadata, &state)?;
        }
        drop(lock);
        let store = Self {
            root,
            decks,
            metadata,
        };
        store.read()?;
        Ok(store)
    }

    pub fn open(repository_root: &Path) -> Result<Self> {
        crate::layout::guard_local_path(repository_root, "корень обмена")?;
        let root = Directory::open(repository_root)?;
        let decks = required_child(&root, "decks")?;
        let metadata = required_child(&decks, ".exchange")?;
        let store = Self {
            root,
            decks,
            metadata,
        };
        store.read()?;
        Ok(store)
    }

    pub fn read(&self) -> Result<StateView> {
        let _lock = self.metadata.lock()?;
        self.verify_location()?;
        let state = load(&self.metadata)?;
        Ok(view(state))
    }

    /// Фиксирует подтверждённый C. Повтор той же `source_sequence` идемпотентен,
    /// устаревшая последовательность либо другое содержимое с тем же номером — отказ.
    pub fn record_input(
        &self,
        expected: &RevisionToken,
        confirmation: Confirmation,
    ) -> Result<StateView> {
        self.mutate(expected, |state| {
            let snapshot = confirmation.snapshot;
            let identity = snapshot.identity();
            let key = identity.key();
            if let Some(stream) = state.streams.get_mut(&key) {
                if stream.current.snapshot.identity() != identity {
                    return Err(ExchangeError::IdentityMismatch);
                }
                let current = &stream.current;
                if confirmation.source_sequence < current.source_sequence {
                    return Err(ExchangeError::StaleState);
                }
                if confirmation.source_sequence == current.source_sequence {
                    if current.snapshot == snapshot && current.evidence == confirmation.evidence {
                        return Ok(false);
                    }
                    return Err(ExchangeError::StaleState);
                }
                stream.current = InputRevision {
                    source_sequence: confirmation.source_sequence,
                    evidence: confirmation.evidence,
                    snapshot,
                };
            } else {
                let folder = identity.output_directory(snapshot.name())?;
                state.streams.insert(
                    key,
                    StreamState {
                        output_directory: folder,
                        current: InputRevision {
                            source_sequence: confirmation.source_sequence,
                            evidence: confirmation.evidence,
                            snapshot,
                        },
                        base: None,
                        published: None,
                    },
                );
            }
            Ok(true)
        })
    }

    pub fn plan_preparation(&self, identity: &DeckIdentity) -> Result<Preparation> {
        let _lock = self.metadata.lock()?;
        self.verify_location()?;
        let state = load(&self.metadata)?;
        let stream = stream(&state, identity)?;
        let working = self.working(stream)?;
        let mut plan = Preparation {
            token: RevisionToken(digest(&state)),
            identity: identity.clone(),
            input_revision: stream.current.snapshot.revision().into(),
            action: PreparationAction::Blocked,
            output_directory: stream.output_directory.clone(),
            observed_working_revision: working.as_ref().map(|w| w.revision().into()),
            comparison: None,
            reason: None,
        };
        let input = &stream.current.snapshot;
        match (&stream.base, &working) {
            (None, None) if input.unproven().is_empty() => {
                plan.action = PreparationAction::PrepareFirst
            }
            (None, None) => plan.reason = Some("input_unproven".into()),
            (None, Some(_)) => plan.reason = Some("untracked_output".into()),
            (Some(_), None) => plan.reason = Some("working_missing".into()),
            (Some(base), Some(working)) => {
                let comparison = compare(&base.snapshot, input, working)?;
                if comparison.proven_compatible() {
                    plan.action = match comparison.disposition {
                        Disposition::Unchanged | Disposition::WorkingOnly => {
                            PreparationAction::Keep
                        }
                        Disposition::InputOnly => PreparationAction::RefreshUnchanged,
                        // Даже одинаковый итог двух сторон требует явного согласования
                        // происхождения; не продвигаем базу автоматически.
                        _ => PreparationAction::Blocked,
                    };
                }
                if plan.action == PreparationAction::Blocked {
                    plan.reason = Some("stale_or_unproven_base".into());
                }
                plan.comparison = Some(comparison);
            }
        }
        Ok(plan)
    }

    /// Подтверждает уже выполненную будущим владельцем подготовку. Только
    /// допустимый план, тот же токен, тот же C и фактический output == C продвигают базу.
    /// Сам API не копирует/не переписывает данные.
    pub fn record_prepared(&self, preparation: &Preparation) -> Result<StateView> {
        if !matches!(
            preparation.action,
            PreparationAction::PrepareFirst | PreparationAction::RefreshUnchanged
        ) {
            return Err(ExchangeError::Blocked(
                "план не разрешает подготовку".into(),
            ));
        }
        self.mutate(&preparation.token, |state| {
            let stream = stream_mut(state, &preparation.identity)?;
            if stream.current.snapshot.revision() != preparation.input_revision {
                return Err(ExchangeError::StaleState);
            }
            let working = self
                .working(stream)?
                .ok_or_else(|| ExchangeError::Blocked("output отсутствует".into()))?;
            if working != stream.current.snapshot {
                return Err(ExchangeError::Blocked(
                    "output не совпадает с подтверждённым C".into(),
                ));
            }
            if stream.base.as_ref() == Some(&stream.current) {
                return Ok(false);
            }
            stream.base = Some(stream.current.clone());
            Ok(true)
        })
    }

    /// Проверяет актуальную базу, совместимость и фактический B перед импортом.
    /// Выданное разрешение не означает успешного импорта.
    pub fn plan_publication(&self, identity: &DeckIdentity) -> Result<Publication> {
        let _lock = self.metadata.lock()?;
        self.verify_location()?;
        let state = load(&self.metadata)?;
        let stream = stream(&state, identity)?;
        let base = stream
            .base
            .as_ref()
            .ok_or_else(|| ExchangeError::Blocked("нет базы output".into()))?;
        let working = self
            .working(stream)?
            .ok_or_else(|| ExchangeError::Blocked("нет output".into()))?;
        let comparison = compare(&base.snapshot, &stream.current.snapshot, &working)?;
        if base.snapshot != stream.current.snapshot || !comparison.proven_compatible() {
            return Err(ExchangeError::Blocked(
                "устаревшая или недоказанная база публикации".into(),
            ));
        }
        Ok(Publication {
            token: RevisionToken(digest(&state)),
            identity: identity.clone(),
            source_sequence: stream.current.source_sequence,
            base_sequence: base.source_sequence,
            working_revision: working.revision().into(),
        })
    }

    /// Вызывать только после успеха живого импорта И подтверждённого readback.
    /// Требуется тот же профиль/UUID, новый номер и семантически тот же B.
    /// Если Anki изменил результат, отказ: новый C регистрируется отдельно.
    pub fn record_published(
        &self,
        publication: &Publication,
        readback: Confirmation,
    ) -> Result<StateView> {
        self.mutate(&publication.token, |state| {
            let stream = stream_mut(state, &publication.identity)?;
            if readback.snapshot.identity() != &publication.identity {
                return Err(ExchangeError::IdentityMismatch);
            }
            if readback.source_sequence <= stream.current.source_sequence
                || stream.current.source_sequence != publication.source_sequence
            {
                return Err(ExchangeError::StaleState);
            }
            let working = self
                .working(stream)?
                .ok_or_else(|| ExchangeError::Blocked("output исчез после импорта".into()))?;
            if working.revision() != publication.working_revision || readback.snapshot != working {
                return Err(ExchangeError::Blocked(
                    "readback или output отличается от отправленного B".into(),
                ));
            }
            stream.published = Some(PublishedRevision {
                identity: publication.identity.clone(),
                working_revision: publication.working_revision.clone(),
                based_on_sequence: publication.base_sequence,
                checked_input_sequence: publication.source_sequence,
                confirmed_sequence: readback.source_sequence,
                evidence: readback.evidence.clone(),
            });
            stream.current = InputRevision {
                source_sequence: readback.source_sequence,
                evidence: readback.evidence,
                snapshot: readback.snapshot,
            };
            stream.base = Some(stream.current.clone());
            Ok(true)
        })
    }

    fn working(&self, stream: &StreamState) -> Result<Option<Snapshot>> {
        let Some(output) = self.decks.child("output")? else {
            return Ok(None);
        };
        let Some(directory) = output.child(&stream.output_directory)? else {
            return Ok(None);
        };
        let working = Snapshot::read_directory(
            stream.current.snapshot.identity().profile.clone(),
            &directory,
        )?;
        if working.identity() != stream.current.snapshot.identity() {
            return Err(ExchangeError::IdentityMismatch);
        }
        Ok(Some(working))
    }

    fn mutate(
        &self,
        expected: &RevisionToken,
        change: impl FnOnce(&mut State) -> Result<bool>,
    ) -> Result<StateView> {
        let _lock = self.metadata.lock()?;
        self.verify_location()?;
        let mut state = load(&self.metadata)?;
        if &RevisionToken(digest(&state)) != expected {
            return Err(ExchangeError::StaleState);
        }
        if change(&mut state)? {
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or_else(|| ExchangeError::CorruptState("переполнение поколения".into()))?;
            verify(&state)?;
            save(&self.metadata, &state)?;
        }
        Ok(view(state))
    }

    fn verify_location(&self) -> Result<()> {
        self.metadata.require_private()?;
        if !self.decks.is_same_child(&self.root, "decks")?
            || !self.metadata.is_same_child(&self.decks, ".exchange")?
        {
            return Err(ExchangeError::CorruptState(
                "каталог состояния заменён или перемещён".into(),
            ));
        }
        Ok(())
    }
}

fn required_child(parent: &Directory, name: &str) -> Result<Directory> {
    parent.child(name)?.ok_or_else(|| {
        ExchangeError::CorruptState(format!(
            "нет каталога {name}; требуется явная инициализация"
        ))
    })
}

fn stream<'a>(state: &'a State, identity: &DeckIdentity) -> Result<&'a StreamState> {
    identity.validate()?;
    let stream = state
        .streams
        .get(&identity.key())
        .ok_or_else(|| ExchangeError::Blocked("нет подтверждённого входа".into()))?;
    if stream.current.snapshot.identity() != identity {
        return Err(ExchangeError::IdentityMismatch);
    }
    Ok(stream)
}

fn stream_mut<'a>(state: &'a mut State, identity: &DeckIdentity) -> Result<&'a mut StreamState> {
    stream(state, identity)?;
    Ok(state
        .streams
        .get_mut(&identity.key())
        .expect("проверенный поток"))
}

fn check_evidence(sequence: u64, evidence: &str) -> Result<()> {
    if sequence == 0
        || evidence.trim().is_empty()
        || evidence.len() > 4096
        || evidence.chars().any(char::is_control)
    {
        return Err(ExchangeError::CorruptState(
            "нет подтверждения/монотонного номера источника".into(),
        ));
    }
    Ok(())
}

fn verify(state: &State) -> Result<()> {
    if state.magic != MAGIC || state.version != VERSION || !valid_hash(&state.store_id) {
        return Err(ExchangeError::CorruptState("magic/version/store_id".into()));
    }
    let mut paths = BTreeSet::new();
    for (key, stream) in &state.streams {
        super::filesystem::plain_filename(&stream.output_directory)?;
        if !paths.insert(super::identity::path_key(&stream.output_directory))
            || key != &stream.current.snapshot.identity().key()
        {
            return Err(ExchangeError::CorruptState(
                "коллизия пути или идентичности".into(),
            ));
        }
        for revision in std::iter::once(&stream.current).chain(stream.base.as_ref()) {
            revision.snapshot.verify()?;
            check_evidence(revision.source_sequence, &revision.evidence)?;
            if revision.snapshot.identity() != stream.current.snapshot.identity()
                || revision.source_sequence > stream.current.source_sequence
                || (revision.source_sequence == stream.current.source_sequence
                    && revision != &stream.current)
            {
                return Err(ExchangeError::CorruptState("несогласованная база".into()));
            }
        }
        if let Some(published) = &stream.published {
            check_evidence(published.confirmed_sequence, &published.evidence)?;
            if published.identity != *stream.current.snapshot.identity()
                || !valid_hash(&published.working_revision)
                || published.based_on_sequence == 0
                || published.based_on_sequence > published.checked_input_sequence
                || published.checked_input_sequence >= published.confirmed_sequence
                || published.confirmed_sequence > stream.current.source_sequence
            {
                return Err(ExchangeError::CorruptState(
                    "несогласованная публикация".into(),
                ));
            }
        }
    }
    Ok(())
}

fn load(directory: &Directory) -> Result<State> {
    let bytes = directory
        .read(STATE_FILE)
        .map_err(|e| ExchangeError::CorruptState(format!("нельзя прочитать {STATE_FILE}: {e}")))?;
    let envelope: Envelope =
        serde_json::from_slice(&bytes).map_err(|e| ExchangeError::CorruptState(e.to_string()))?;
    if envelope.checksum != digest(&envelope.state) {
        return Err(ExchangeError::CorruptState("checksum".into()));
    }
    verify(&envelope.state)?;
    Ok(envelope.state)
}

fn save(directory: &Directory, state: &State) -> Result<()> {
    let envelope = Envelope {
        checksum: digest(state),
        state: state.clone(),
    };
    let bytes = serde_json::to_vec(&envelope).expect("сериализуемый State");
    directory.publish(STATE_FILE, &bytes)
}

fn view(state: State) -> StateView {
    StateView {
        token: RevisionToken(digest(&state)),
        generation: state.generation,
        streams: state.streams,
    }
}
