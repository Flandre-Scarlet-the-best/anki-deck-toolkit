//! Оркестратор единственного привилегированного пути Anki → input.
mod publish;
mod source;

use super::{
    Options, Result,
    environment::{self, Target},
    error, external_error, io_error, patch, transport,
};

pub(super) fn status(repository: &Path) -> Result<Value> {
    publish::Store::open_existing(repository)
        .and_then(|s| s.audit())
        .map_err(publication_error)
}
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

pub(super) fn run(
    t: &Target,
    o: &Options,
    repository: &Path,
    requested: &[String],
    all: bool,
    wait_seconds: u64,
    apply: bool,
) -> Result<Value> {
    if o.profile.is_none() {
        return Err(error(
            "profile_required",
            "Укажите точный --profile; автоматическое переключение запрещено",
        ));
    }
    crate::layout::guard_local_path(repository, "Корень публикации")?;
    if !t.running && !apply {
        return Ok(json!({"state":"input_plan","apply":false,"data_dir":t.data,
            "profile":o.profile,"requested":requested,"all":all,"needs_launch":true,
            "message":"Anki остановлена: список колод и идентичность будут проверены после запуска; записи не выполнялись",
            "consequences":"CrowdAnki может менять UUID и метаданные Anki; output не синхронизируется"}));
    }
    let target = ready_target(t, o, apply)?;
    let catalog = transport::patched(&target, o, "toolkitCatalog", json!({}))?;
    let selected = select(&catalog, requested, all)?;
    if !apply {
        return Ok(
            json!({"state":"input_plan","apply":false,"data_dir":target.data,
            "profile":o.profile,"profile_id":catalog["profile_id"],"decks":selected,
            "needs_profile_binding":catalog["profile_id"].is_null(),
            "consequences":"CrowdAnki может менять UUID и метаданные Anki; output не синхронизируется"}),
        );
    }
    let catalog = transport::patched(&target, o, "toolkitBindProfile", json!({"approved":true}))?;
    // Повторяем выбор после binding: текущие колоды могли измениться между HTTP-вызовами.
    let selected = select(&catalog, requested, all)?;
    let mut store = publish::Store::open(repository).map_err(publication_error)?;
    let recovered = store.recover().map_err(publication_error)?;
    let mut outcomes = Vec::new();
    for deck in selected {
        if deck["state"] == "missing" {
            outcomes.push(
                json!({"name":deck["name"],"state":"failed","reason":"deck_not_found",
                "input_confirmed":false,"anki_export_may_have_completed":false,
                "message":"Колода отсутствует; прежний input сохранён"}),
            );
            continue;
        }
        let key = asset_store::hashing::sha256_hex(
            serde_json::to_vec(&json!([catalog["profile_id"], deck["id"]])).map_err(io_error)?,
        );
        let result = export_one(&target, o, &catalog, &deck, &key, &mut store, wait_seconds);
        match result {
            Ok(value) => outcomes.push(value),
            Err(e) => outcomes.push(json!({"name":deck["name"],"state":"failed",
                "reason":e.details["reason"],"message":e.message,
                "job_id":store.pending(&key).map(|p| p.id.clone()),
                "anki_export_may_have_completed":true,"input_confirmed":false})),
        }
    }
    let succeeded = outcomes.iter().all(|v| v["state"] == "published");
    Ok(
        json!({"state":if succeeded {"succeeded"} else {"partial"},"profile":o.profile,
        "profile_id":catalog["profile_id"],"data_dir":target.data,"decks":outcomes,"recovered":recovered,
        "message":"Каждый published подтверждён в input и exchange; output не синхронизируется",
        "consequences":"Оригинальный CrowdAnki мог изменить UUID/метаданные коллекции Anki; откат экспорта не обещается"}),
    )
}

fn ready_target(t: &Target, o: &Options, apply: bool) -> Result<Target> {
    if !t.running && apply {
        if patch::status(t)?["state"] != "installed" {
            return Err(error(
                "export_setup_required",
                "Выполните anki setup и patch install, затем повторите sync-input",
            ));
        }
        environment::launch(t, o, true)?;
    }
    let end = Instant::now() + Duration::from_secs(if t.running { 1 } else { 90 });
    loop {
        let current = environment::resolve(o)?;
        match transport::handshake(&current, o) {
            Ok(h) if h["addons_ready"] == true && h["busy"] != true => return Ok(current),
            Ok(_) => {
                return Err(error(
                    "anki_busy_or_addons_not_ready",
                    "Завершите занятие/диалог; проверьте doctor и загруженные аддоны",
                ));
            }
            Err(e)
                if (e.details["reason"] == "http_unreachable"
                    || (!t.running && e.message.contains("profile_not_ready")))
                    && Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(500))
            }
            Err(e) => return Err(e),
        }
    }
}

fn select(catalog: &Value, requested: &[String], all: bool) -> Result<Vec<Value>> {
    let decks = catalog["decks"]
        .as_array()
        .ok_or_else(|| error("invalid_export_catalog", "Нет списка колод живой Anki"))?;
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for d in decks {
        let name = d["name"]
            .as_str()
            .ok_or_else(|| error("invalid_export_catalog", "Нет имени колоды"))?;
        let id = d["id"]
            .as_str()
            .ok_or_else(|| error("invalid_export_catalog", "Нет ID колоды"))?;
        crate::exchange::export_name(name).map_err(io_error)?;
        if !names.insert(name) || !ids.insert(id) {
            return Err(error(
                "ambiguous_export_catalog",
                "Неоднозначные имена/ID колод",
            ));
        }
    }
    let wanted: BTreeSet<&str> = if all {
        names.clone()
    } else {
        requested.iter().map(String::as_str).collect()
    };
    let mut selected: Vec<_> = decks
        .iter()
        .filter(|d| {
            let name = d["name"].as_str().expect("checked name");
            wanted.contains(name)
                && !wanted
                    .iter()
                    .filter(|parent| names.contains(**parent))
                    .any(|parent| {
                        name.strip_prefix(parent)
                            .is_some_and(|s| s.starts_with("::"))
                    })
        })
        .cloned()
        .collect();
    selected.extend(
        wanted
            .iter()
            .filter(|name| !names.contains(**name))
            .map(|name| json!({"name":name,"state":"missing"})),
    );
    selected.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    if selected.is_empty() {
        return Err(error("no_exportable_decks", "Нет подходящих обычных колод"));
    }
    Ok(selected)
}

fn export_one(
    t: &Target,
    o: &Options,
    catalog: &Value,
    deck: &Value,
    key: &str,
    store: &mut publish::Store,
    wait_seconds: u64,
) -> Result<Value> {
    let mut job = if let Some(job) = store.pending(key) {
        job.clone()
    } else {
        let job = source::prepare(t, o, catalog, deck)?;
        store
            .save_job(key, job.clone())
            .map_err(publication_error)?;
        job
    };
    if job.profile_id != catalog["profile_id"] || job.deck_id != deck["id"] {
        return Err(error(
            "pending_export_identity_mismatch",
            "Сохранённое задание принадлежит другому источнику",
        ));
    }
    let end = Instant::now() + Duration::from_secs(wait_seconds);
    let mut sent = false;
    loop {
        // resolve заново подтверждает владельца порта, включая управляемый запуск.
        let current = environment::resolve(o)?;
        let result = source::poll(&current, o, &job)?;
        match result["state"].as_str() {
            Some("prepared") if !sent => {
                let observer = result["job"]["observer_process"].as_str().ok_or_else(|| {
                    error(
                        "export_observer_unverified",
                        "Не подтверждён процесс, ответивший на toolkitJob",
                    )
                })?;
                if job.prepared_action(observer)? == source::PreparedAction::RenewSource {
                    store.finish_job(key).map_err(publication_error)?;
                    return Err(error(
                        "unsubmitted_source_restarted",
                        "Anki перезапущена до admission; задание не выполнялось, повторите команду",
                    ));
                }
                if !job.admission_started {
                    job.admission_started = true;
                    // Намерение отправки durable ДО HTTP, включая неизвестный
                    // результат admission и падение CLI между этими действиями.
                    store
                        .save_job(key, job.clone())
                        .map_err(publication_error)?;
                }
                sent = true;
                // При неизвестном admission следующий шаг — только toolkitJob с тем же ID.
                if let Err(e) = source::submit(&current, o, &job)
                    && e.details["submission_state"] != "unknown"
                {
                    return Err(e);
                }
            }
            Some("succeeded") => {
                let verified = source::verify(&current, &job, &result)?;
                let mut published = store.publish(&verified).map_err(publication_error)?;
                published["job_id"] = json!(job.id);
                published["anki_export_state"] = json!("succeeded");
                published["input_confirmed"] = json!(true);
                if store.finish_job(key).is_err() {
                    // Durable C уже доказан. Ошибка уборки pending не отменяет
                    // подтверждение; повтор сверит тот же job без новой ревизии.
                    published["pending_cleanup"] = json!("unknown");
                    published["next"] =
                        json!("Повторите sync-input для проверки завершения служебной записи");
                }
                return Ok(published);
            }
            Some("failed" | "cancelled" | "interrupted") => {
                store.finish_job(key).map_err(publication_error)?;
                return Err(external_error(
                    "anki_export_failed",
                    format!(
                        "Задание {} завершилось {}; следующий вызов создаст новое задание",
                        job.id, result["state"]
                    ),
                ));
            }
            Some("queued" | "running" | "prepared") => {}
            _ => {
                return Err(external_error(
                    "export_completion_unknown",
                    "Нет доказанного завершения; повторите ту же команду для опроса сохранённого ID",
                ));
            }
        }
        if Instant::now() >= end {
            return Err(external_error(
                "export_wait_timeout",
                "Экспорт ещё не подтверждён; повторите ту же команду, сохранённый ID не будет заменён",
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn publication_error(e: crate::exchange::ExchangeError) -> crate::error::DomainError {
    // Сообщения parse/validate могут содержать приватные значения: наружу только класс.
    let reason = match e {
        crate::exchange::ExchangeError::Blocked(_) => "input_recovery_required",
        crate::exchange::ExchangeError::StaleState => "input_cas_conflict",
        crate::exchange::ExchangeError::CorruptState(_) => "input_state_corrupt",
        crate::exchange::ExchangeError::UnsafePath(_) => "input_unsafe_path",
        crate::exchange::ExchangeError::Io(_) => "input_durability_failure",
        _ => "input_verification_failed",
    };
    external_error(
        reason,
        "Публикация input не подтверждена; прежний снимок/intent сохранены. Повторите sync-input для recovery; не сбрасывайте exchange",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_subtrees_are_exact_and_never_duplicate_children() {
        let catalog = json!({"decks":[{"id":"1","name":"A"},{"id":"2","name":"A::Child"},
            {"id":"3","name":"B"},{"id":"4","name":"AB"}]});
        assert_eq!(
            select(&catalog, &["A::Child".into()], false).unwrap()[0]["id"],
            "2"
        );
        assert_eq!(
            select(&catalog, &["A".into(), "A::Child".into()], false)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(select(&catalog, &[], true).unwrap().len(), 3);
        assert_eq!(
            select(&catalog, &["missing".into()], false).unwrap()[0]["state"],
            "missing"
        );
    }
}
