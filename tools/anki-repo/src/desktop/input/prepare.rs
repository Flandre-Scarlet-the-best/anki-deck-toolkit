//! Подготовка независимой рабочей копии `decks/output/` из подтверждённого C.
//!
//! Exchange решает, что разрешено (`prepare_first`, `refresh_unchanged`, `keep`,
//! `blocked`); этот владелец только физически переносит полный экспорт.
//! Протокол одной подготовки:
//!
//! 1. под блокировкой владельца input — актуальный план и доверенный C;
//! 2. полный экспорт копируется в приватный `stage` с проверкой каждого SHA-256
//!    и семантического снимка;
//! 3. надёжно записывается `prepare.json` (intent): C, инвентарь, прежний B;
//! 4. под `write::ExportLock` B перепроверяется и уходит в `previous`, затем
//!    `stage` становится рабочей копией (rename без замены назначения);
//! 5. после отпускания `ExportLock` — `ExchangeStore::record_prepared` того же плана;
//! 6. `previous` и intent убираются.
//!
//! План не сериализуется, поэтому после остановки между шагами 3 и 5 intent
//! откатывает только доказанно собственные перемещения и подготовка выполняется
//! заново; если база уже записана (шаг 5 завершён), остаётся уборка шага 6.
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::publish::Store;
use super::work::{
    self, CONFIG, Inventory, PREPARE, PREVIOUS, PUBLISH, STAGE, blocked, corrupt, read_inventory,
};
use crate::exchange::{
    DeckIdentity, ExchangeError, PreparationAction, Result, StateView, StreamState,
    filesystem::Directory,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrepareIntent {
    pub identity: DeckIdentity,
    pub output_directory: String,
    pub action: PreparationAction,
    pub source_sequence: u64,
    pub revision: String,
    pub inventory: Inventory,
    pub previous_revision: Option<String>,
    pub previous_inventory: Option<Inventory>,
}

/// Результат восстановления собственного незавершённого intent.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    None,
    Finalized,
    RolledBack,
}

pub(super) fn run(repository: &Path, outputs: &[String], all: bool, apply: bool) -> Value {
    let outcome = (|| -> std::result::Result<Value, crate::error::DomainError> {
        crate::layout::guard_local_path(repository, "Корень подготовки output")?;
        let store = Store::open_existing(repository).map_err(super::publication_error)?;
        let state = store.exchange().read().map_err(super::publication_error)?;
        let mut decks = Vec::new();
        for item in select(&state, outputs, all) {
            decks.push(match item {
                Ok(stream) => {
                    let result = if apply {
                        apply_one(&store, &stream)
                    } else {
                        plan_one(&store, &stream)
                    };
                    result.unwrap_or_else(|e| failure(&stream, e))
                }
                Err(name) => json!({"output_directory":name,"state":"blocked",
                    "reason":"output_not_registered","applied":false,
                    "next":"Привязка output появляется только после sync-input; присвоение существующих каталогов не выполняется"}),
            });
        }
        let ok = decks
            .iter()
            .all(|d| matches!(d["state"].as_str(), Some("prepared" | "kept" | "planned")));
        Ok(
            json!({"state": if !apply {"prepare_plan"} else if ok {"succeeded"} else {"partial"},
            "apply":apply,"decks":decks,
            "message":"Подготовка выполняет только решения exchange: prepare_first, refresh_unchanged или keep; merge и overwrite не выполняются"}),
        )
    })();
    outcome.unwrap_or_else(
        |e| json!({"state":"failed","reason":e.details["reason"],"message":e.message}),
    )
}

/// Состояние обмена без записи: целостность input (тот же аудит, что у
/// input-status), решения подготовки и незавершённые журналы каждой колоды.
pub(super) fn status(repository: &Path) -> Value {
    let outcome = (|| -> std::result::Result<Value, crate::error::DomainError> {
        crate::layout::guard_local_path(repository, "Корень обмена")?;
        let store = Store::open_existing(repository).map_err(super::publication_error)?;
        let audit = store.audit().map_err(super::publication_error)?;
        let state = store.exchange().read().map_err(super::publication_error)?;
        let mut decks = Vec::new();
        for stream in state.streams.values() {
            let identity = stream.current.snapshot.identity();
            let mut value = describe(stream);
            match store.exchange().plan_preparation(identity) {
                Ok(plan) => {
                    value["action"] = json!(plan.action());
                    value["reason"] = json!(plan.reason);
                    value["working_revision"] = json!(plan.observed_working_revision);
                }
                Err(e) => value["reason"] = json!(failure(stream, e)["reason"]),
            }
            value["pending"] =
                json!(pending(&store, &identity.key()).map_err(super::publication_error)?);
            value["state"] = json!("observed");
            decks.push(value);
        }
        Ok(json!({"state":"succeeded","generation":state.generation,
            "pending_jobs":audit["pending_jobs"],"rejected_jobs":audit["rejected_jobs"],"decks":decks}))
    })();
    outcome.unwrap_or_else(
        |e| json!({"state":"failed","reason":e.details["reason"],"message":e.message}),
    )
}

pub(super) fn select(
    state: &StateView,
    outputs: &[String],
    all: bool,
) -> Vec<std::result::Result<StreamState, String>> {
    if all {
        return state.streams.values().cloned().map(Ok).collect();
    }
    let mut seen = std::collections::BTreeSet::new();
    // Обе формы одного имени (с префиксом и без) выбирают поток один раз.
    outputs
        .iter()
        .map(|name| name.strip_prefix("decks/output/").unwrap_or(name))
        .filter(|name| seen.insert(*name))
        .map(|name| {
            state
                .streams
                .values()
                .find(|s| s.output_directory == name)
                .cloned()
                .ok_or_else(|| name.to_owned())
        })
        .collect()
}

pub(super) fn describe(stream: &StreamState) -> Value {
    let identity = stream.current.snapshot.identity();
    json!({"name":stream.current.snapshot.name(),"identity":identity,
        "output_directory":format!("decks/output/{}", stream.output_directory),
        "input":{"source_sequence":stream.current.source_sequence,"revision":stream.current.snapshot.revision()},
        "base":stream.base.as_ref().map(|b| json!({"source_sequence":b.source_sequence,"revision":b.snapshot.revision()})),
        "published":stream.published})
}

fn failure(stream: &StreamState, e: ExchangeError) -> Value {
    let mut value = describe(stream);
    let (state, reason) = match &e {
        ExchangeError::Blocked(reason) => ("blocked", reason.clone()),
        ExchangeError::StaleState => ("blocked", "stale_state".into()),
        ExchangeError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock => {
            ("failed", "busy".into())
        }
        ExchangeError::CorruptState(_) => ("failed", "state_corrupt".into()),
        ExchangeError::UnsafePath(_) => ("failed", "unsafe_path".into()),
        ExchangeError::Io(_) => ("failed", "durability_failure".into()),
        _ => ("failed", "verification_failed".into()),
    };
    value["state"] = json!(state);
    value["reason"] = json!(reason);
    value["applied"] = json!(false);
    value["next"] = json!(next_step(&reason));
    value
}

fn next_step(reason: &str) -> &'static str {
    match reason {
        "untracked_output" => {
            "Каталог уже существует без базы: присвоение не выполняется. Перенесите его вручную вне decks/output и повторите"
        }
        "working_missing" => {
            "Рабочая копия пропала при наличии базы: восстановите её или отдельно согласуйте новую базу"
        }
        "stale_or_unproven_base" => {
            "Изменились обе стороны или сравнение недоказуемо: требуется отдельное явное согласование, автоматического merge нет"
        }
        "input_unproven" => "Подтверждённый C содержит недоказуемые ссылки: подготовка запрещена",
        "publication_pending" => "Завершите publish-output --apply для этой колоды",
        "input_recovery_required" => "Выполните sync-input --apply для восстановления input",
        "working_changed_since_plan" | "stale_state" => {
            "Рабочая копия или состояние изменились во время подготовки: повторите prepare-output"
        }
        "working_import_config_differs" => {
            "import_config.yaml рабочей копии отличается от C: согласуйте его вручную, иначе он был бы потерян"
        }
        "unexpected_export_entries" => {
            "В рабочей копии есть посторонние файлы: обновление их удалило бы. Перенесите их и повторите"
        }
        "working_changed_after_swap" => {
            "После переключения рабочую копию изменили до фиксации базы: previous и intent сохранены в decks/.roundtrip, разберите вручную"
        }
        "unknown_work_entries" => {
            "В decks/.roundtrip обнаружены неизвестные объекты: они не удаляются, разберите вручную"
        }
        "busy" => "Владелец input занят другой командой: дождитесь её завершения",
        _ => "Повторите команду после устранения причины; прежние данные сохранены",
    }
}

fn plan_one(store: &Store, stream: &StreamState) -> Result<Value> {
    let identity = stream.current.snapshot.identity();
    let plan = store.exchange().plan_preparation(identity)?;
    let mut value = describe(stream);
    value["action"] = json!(plan.action());
    value["reason"] = json!(plan.reason);
    value["working_revision"] = json!(plan.observed_working_revision);
    value["comparison"] = plan
        .comparison
        .as_ref()
        .map_or(Value::Null, work::difference_summary);
    value["applied"] = json!(false);
    value["recovery_pending"] = json!(pending(store, &identity.key())?);
    value["state"] = json!(match plan.action() {
        PreparationAction::Blocked => "blocked",
        _ => "planned",
    });
    value["expected_changes"] = json!(match plan.action() {
        PreparationAction::PrepareFirst => "Создать рабочую копию из подтверждённого C",
        PreparationAction::RefreshUnchanged => "Заменить неизменённую рабочую копию свежим C",
        PreparationAction::Keep => "Изменений нет: рабочая копия и база сохраняются",
        PreparationAction::Blocked => "Изменений нет: подготовка запрещена",
    });
    if plan.action() == PreparationAction::Keep {
        value["physical_differences"] = json!(config_difference(store, stream)?);
    }
    if let Some(reason) = &plan.reason {
        value["next"] = json!(next_step(reason));
    }
    Ok(value)
}

fn pending(store: &Store, key: &str) -> Result<Vec<&'static str>> {
    let Some(area) = store.work_area(false)? else {
        return Ok(Vec::new());
    };
    let Some(dir) = area.child(key)? else {
        return Ok(Vec::new());
    };
    let names = dir.names()?;
    Ok([PREPARE, PUBLISH]
        .into_iter()
        .filter(|n| names.iter().any(|x| x == n))
        .collect())
}

/// `import_config.yaml` не входит в семантическую ревизию; его расхождение
/// между C и B показывается отдельно и при keep ничего не переписывает.
fn config_difference(store: &Store, stream: &StreamState) -> Result<Vec<&'static str>> {
    let source = store.confirmed_source(stream.current.snapshot.identity())?;
    let working = store
        .decks()
        .child("output")?
        .and_then(|o| o.child(&stream.output_directory).transpose())
        .transpose()?;
    let working_config = match working {
        Some(dir) if dir.entry_is_dir(CONFIG)? == Some(false) => Some(dir.stream_hash(CONFIG)?.0),
        _ => None,
    };
    Ok(if source.inventory.get(CONFIG) != working_config.as_ref() {
        vec![CONFIG]
    } else {
        Vec::new()
    })
}

fn apply_one(store: &Store, stream: &StreamState) -> Result<Value> {
    let identity = stream.current.snapshot.identity().clone();
    let area = store.work_area(true)?.expect("создана");
    let dir = work::stream_dir(&area, &identity.key())?;
    if dir.entry_is_dir(PUBLISH)?.is_some() {
        return Err(blocked("publication_pending"));
    }
    let recovery = recover(store, &dir, &identity)?;
    let stream = store
        .exchange()
        .read()?
        .streams
        .get(&identity.key())
        .cloned()
        .ok_or_else(|| blocked("нет подтверждённого входа"))?;
    let plan = store.exchange().plan_preparation(&identity)?;
    let mut value = describe(&stream);
    value["action"] = json!(plan.action());
    value["recovery"] = json!(format!("{recovery:?}").to_lowercase());
    value["working_revision"] = json!(plan.observed_working_revision);
    value["applied"] = json!(false);
    match plan.action() {
        PreparationAction::Keep => {
            value["state"] = json!("kept");
            value["physical_differences"] = json!(config_difference(store, &stream)?);
            return Ok(value);
        }
        PreparationAction::Blocked => {
            let reason = plan.reason.clone().unwrap_or_else(|| "blocked".into());
            value["state"] = json!("blocked");
            value["next"] = json!(next_step(&reason));
            value["reason"] = json!(reason);
            value["comparison"] = plan
                .comparison
                .as_ref()
                .map_or(Value::Null, work::difference_summary);
            return Ok(value);
        }
        _ => {}
    }
    let source = store.confirmed_source(&identity)?;
    if source.snapshot.revision() != plan.input_revision() {
        return Err(ExchangeError::StaleState);
    }
    // Stage без intent никогда не был видим как рабочая копия.
    dir.remove_tree(STAGE)?;
    let (stage, _) = dir.ensure_child(STAGE)?;
    work::copy_export(&source.current, &stage, Some(&source.inventory))?;
    let (staged, _) = read_inventory(&stage, identity.profile.clone())?;
    if staged != source.snapshot {
        return Err(corrupt("копия не совпала с подтверждённым снимком"));
    }
    #[cfg(test)]
    store.checkpoint("prepare_staged")?;
    let (output, _) = store.decks().ensure_child("output")?;
    let name = plan.output_directory.clone();
    let mut intent = PrepareIntent {
        identity: identity.clone(),
        output_directory: name.clone(),
        action: plan.action(),
        source_sequence: source.source_sequence,
        revision: source.snapshot.revision().into(),
        inventory: source.inventory.clone(),
        previous_revision: None,
        previous_inventory: None,
    };
    if plan.action() == PreparationAction::RefreshUnchanged {
        let (working, lock) =
            work::lock_export(&output, &name)?.ok_or_else(|| blocked("working_missing"))?;
        let (snapshot, inventory) = read_inventory(&working, identity.profile.clone())?;
        if Some(snapshot.revision()) != plan.observed_working_revision.as_deref() {
            return Err(blocked("working_changed_since_plan"));
        }
        if inventory.get(CONFIG) != source.inventory.get(CONFIG) {
            return Err(blocked("working_import_config_differs"));
        }
        intent.previous_revision = Some(snapshot.revision().into());
        intent.previous_inventory = Some(inventory);
        work::save(&dir, PREPARE, &intent)?;
        #[cfg(test)]
        store.checkpoint("prepare_intent")?;
        output.rename_into(&name, &dir, PREVIOUS)?;
        #[cfg(test)]
        store.checkpoint("prepare_displaced")?;
        dir.rename_into(STAGE, &output, &name)?;
        drop(lock);
    } else {
        if output.entry_is_dir(&name)?.is_some() {
            return Err(blocked("untracked_output"));
        }
        work::save(&dir, PREPARE, &intent)?;
        #[cfg(test)]
        store.checkpoint("prepare_intent")?;
        dir.rename_into(STAGE, &output, &name)?;
    }
    #[cfg(test)]
    store.checkpoint("prepare_visible")?;
    // ExportLock отпущен: порядок блокировок exchange → экспорт сохраняется.
    match store.exchange().record_prepared(&plan) {
        Ok(_) => {}
        Err(ExchangeError::Blocked(_) | ExchangeError::StaleState) => {
            let rolled = rollback(store, &dir, &intent);
            value["state"] = json!("blocked");
            value["reason"] = json!(if rolled.is_ok() {
                "working_changed_since_plan"
            } else {
                "working_changed_after_swap"
            });
            value["next"] = json!(next_step(value["reason"].as_str().unwrap_or_default()));
            return Ok(value);
        }
        Err(e) => return Err(e),
    }
    #[cfg(test)]
    store.checkpoint("prepare_recorded")?;
    finalize(&dir, &intent)?;
    let state = store.exchange().read()?;
    let mut value = describe(&state.streams[&identity.key()]);
    value["action"] = json!(intent.action);
    value["state"] = json!("prepared");
    value["applied"] = json!(true);
    value["recovery"] = json!(format!("{recovery:?}").to_lowercase());
    value["working_revision"] = json!(intent.revision);
    value["files"] = json!(intent.inventory.len());
    Ok(value)
}

/// Распознаёт собственный незавершённый intent этого потока. Stage без intent
/// удаляется: он не был видим как рабочая копия и не подтверждался.
pub(super) fn recover(store: &Store, dir: &Directory, identity: &DeckIdentity) -> Result<Recovery> {
    let Some(intent) = work::load::<PrepareIntent>(dir, PREPARE)? else {
        if dir.entry_is_dir(PREVIOUS)?.is_some() {
            return Err(corrupt(
                "previous без intent; автоматический сброс запрещён",
            ));
        }
        dir.remove_tree(STAGE)?;
        return Ok(Recovery::None);
    };
    if &intent.identity != identity {
        return Err(corrupt("идентичность intent подготовки"));
    }
    let state = store.exchange().read()?;
    let committed = state.streams.get(&identity.key()).is_some_and(|s| {
        s.output_directory == intent.output_directory
            && s.base.as_ref().is_some_and(|b| {
                b.source_sequence == intent.source_sequence
                    && b.snapshot.revision() == intent.revision
            })
    });
    if committed {
        finalize(dir, &intent)?;
        return Ok(Recovery::Finalized);
    }
    rollback(store, dir, &intent)?;
    Ok(Recovery::RolledBack)
}

/// Возвращает рабочую копию в состояние до переключения. Перемещается только
/// каталог, байты которого доказанно равны собственному инвентарю intent.
fn rollback(store: &Store, dir: &Directory, intent: &PrepareIntent) -> Result<()> {
    let profile = intent.identity.profile.clone();
    let output = match store.decks().child("output")? {
        Some(output) => output,
        None => store.decks().ensure_child("output")?.0,
    };
    let name = &intent.output_directory;
    let has_previous = dir.entry_is_dir(PREVIOUS)?.is_some();
    if let Some((working, lock)) = work::lock_export(&output, name)? {
        let ours = read_inventory(&working, profile.clone())
            .is_ok_and(|(_, inventory)| inventory == intent.inventory);
        if ours && dir.entry_is_dir(STAGE)?.is_none() {
            output.rename_into(name, dir, STAGE)?;
        } else if has_previous || ours {
            return Err(blocked("working_changed_after_swap"));
        }
        drop(lock);
    }
    if has_previous {
        let previous = dir
            .child(PREVIOUS)?
            .ok_or_else(|| corrupt("исчез previous"))?;
        let (_, inventory) = read_inventory(&previous, profile)?;
        if Some(&inventory) != intent.previous_inventory.as_ref() {
            return Err(corrupt("previous не совпадает с intent"));
        }
        if output.entry_is_dir(name)?.is_some() {
            return Err(blocked("working_changed_after_swap"));
        }
        dir.rename_into(PREVIOUS, &output, name)?;
    }
    dir.remove_file(PREPARE)
}

#[cfg(test)]
#[path = "roundtrip_tests.rs"]
mod tests;

/// После записанной базы: убрать прежнюю копию B (только доказанно ту, что
/// была перемещена) и intent. Новая рабочая копия могла уже правиться.
fn finalize(dir: &Directory, intent: &PrepareIntent) -> Result<()> {
    if let Some(previous) = dir.child(PREVIOUS)? {
        let (_, inventory) = read_inventory(&previous, intent.identity.profile.clone())?;
        if Some(&inventory) != intent.previous_inventory.as_ref() {
            return Err(corrupt(
                "previous не совпадает с intent; сохранён для разбора",
            ));
        }
        dir.remove_tree(PREVIOUS)?;
    }
    dir.remove_tree(STAGE)?;
    dir.remove_file(PREPARE)
}
