//! Оркестратор единственного привилегированного пути Anki → input и
//! управляемого цикла input → output → Anki с подтверждённым readback.
mod prepare;
mod publication;
mod publish;
mod source;
mod work;

pub(super) fn prepare_output(
    repository: &Path,
    outputs: &[String],
    all: bool,
    apply: bool,
) -> Value {
    prepare::run(repository, outputs, all, apply)
}

pub(super) fn exchange_status(repository: &Path) -> Value {
    prepare::status(repository)
}

pub(super) fn publish_plan(repository: &Path, outputs: &[String]) -> Value {
    publication::plan(repository, outputs)
}

pub(super) fn publish_apply(
    t: &Target,
    o: &Options,
    repository: &Path,
    outputs: &[String],
    wait_seconds: u64,
) -> Result<Value> {
    publication::apply(t, o, repository, outputs, wait_seconds)
}

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
    let store = if apply {
        Some(publish::Store::open(repository).map_err(publication_error)?)
    } else {
        None
    };
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
    // Повторяем выбор после привязки: текущие колоды могли измениться между HTTP-вызовами.
    let selected = select(&catalog, requested, all)?;
    let mut store = store.expect("применение уже получило блокировку до запуска Anki");
    let recovered = store.recover().map_err(publication_error)?;
    let mut outcomes = Vec::new();
    for deck in selected {
        if deck["state"] == "missing" {
            let pending = store.pending_named(
                catalog["profile_id"].as_str().unwrap_or(""),
                deck["name"].as_str().unwrap_or(""),
            );
            outcomes.push(missing_outcome(&target, o, &deck, &pending));
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
                "job_id":e.details.get("job_id").cloned().unwrap_or_else(|| json!(store.pending(&key).map(|p| p.id.clone()))),
                "attempt_closed":e.details["attempt_closed"],
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
    for deck in decks {
        let name = deck["name"].as_str().expect("проверено имя");
        if selected.iter().any(|root| {
            let root = root["name"].as_str().expect("проверено имя");
            name == root
                || name
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            crate::exchange::export_name(name).map_err(|_| {
                error(
                    "invalid_export_deck_name",
                    "Имя выбранной колоды или её потомка непригодно для экспорта",
                )
            })?;
        }
    }
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
    if let Some(uuid) = deck["crowdanki_uuid"].as_str()
        && let Ok(profile) =
            crate::exchange::ProfileId::new(catalog["profile_id"].as_str().unwrap_or_default())
        && let Ok(identity) = crate::exchange::DeckIdentity::new(profile, uuid)
        && store
            .publication_pending(&identity.key())
            .map_err(publication_error)?
    {
        return Err(error(
            "publication_pending",
            "Для колоды есть незавершённая публикация output; повторите publish-output --apply, чтобы проверить прежний импорт",
        ));
    }
    let (verified, job) = obtain_export(t, o, catalog, deck, key, store, wait_seconds)?;
    let mut published = store.publish(&verified).map_err(publication_error)?;
    published["job_id"] = json!(job.id);
    published["anki_export_state"] = json!("succeeded");
    published["input_confirmed"] = json!(true);
    if store.finish_job(key).is_err() {
        // Надёжно сохранённый C уже доказан. Ошибка уборки pending не отменяет
        // подтверждение; повтор сверит тот же job без новой ревизии.
        published["pending_cleanup"] = json!("unknown");
        published["next"] = json!("Повторите sync-input для проверки завершения служебной записи");
    }
    Ok(published)
}

/// Живой экспорт до проверенных байтов. Pending job остаётся сохранённым:
/// вызывающий завершает его только после надёжной публикации результата.
fn obtain_export(
    t: &Target,
    o: &Options,
    catalog: &Value,
    deck: &Value,
    key: &str,
    store: &mut publish::Store,
    wait_seconds: u64,
) -> Result<(source::VerifiedExport, source::PendingJob)> {
    if let Some(old) = store
        .pending_named(
            catalog["profile_id"].as_str().unwrap_or(""),
            deck["name"].as_str().unwrap_or(""),
        )
        .into_iter()
        .find(|job| job.key() != key)
    {
        let mut failure = error(
            "pending_export_identity_mismatch",
            "Имя принадлежало другому незавершённому источнику; сначала разрешите сохранённое задание",
        );
        failure.details["job_id"] = json!(old.id);
        return Err(failure);
    }
    let mut job = if let Some(job) = store.pending(key) {
        job.clone()
    } else {
        let job = source::prepare(t, o, catalog, deck)?;
        store
            .save_job(key, job.clone())
            .map_err(publication_error)?;
        job
    };
    validate_pending_source(&job, catalog, deck)?;
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
                        "Anki перезапущена до принятия задания; задание не выполнялось, повторите команду",
                    ));
                }
                let prior_admission_started = job.admission_started;
                if !prior_admission_started {
                    job.admission_started = true;
                    // Намерение отправки надёжно сохраняется ДО HTTP, включая неизвестный
                    // результат принятия задания и падение CLI между этими действиями.
                    store
                        .save_job(key, job.clone())
                        .map_err(publication_error)?;
                }
                sent = true;
                // При неизвестном результате принятия задания следующий шаг — только toolkitJob с тем же ID.
                if let Err(e) = source::submit(&current, o, &job) {
                    submission_error(store, key, &mut job, prior_admission_started, e)?;
                }
            }
            Some("succeeded") => {
                return match source::verify(&current, &job, &result) {
                    Ok(verified) => Ok((verified, job)),
                    Err(e) => Err(close_rejected_attempt(store, key, &job, e)?),
                };
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

fn submission_error(
    store: &mut publish::Store,
    key: &str,
    job: &mut source::PendingJob,
    prior_admission_started: bool,
    failure: crate::error::DomainError,
) -> Result<()> {
    if failure.details["submission_state"] == "unknown" {
        return Ok(());
    }
    // Transport помечает все неопределённые отказы после отправки как unknown.
    // API-ошибка сама по себе не доказывает отказ до принятия: consumed_ticket
    // и внутренние сбои могут относиться к уже принятому заданию.
    let preflight =
        failure.details["submission_state"].is_null() && failure.details["reason"] != "api_error";
    let refused = failure.details["submission_state"] == "rejected"
        && matches!(failure.message.as_str(), "anki_busy" | "addons_not_ready");
    if !prior_admission_started && (preflight || refused) {
        job.admission_started = false;
        store
            .save_job(key, job.clone())
            .map_err(publication_error)?;
    }
    Err(failure)
}

fn validate_pending_source(job: &source::PendingJob, catalog: &Value, deck: &Value) -> Result<()> {
    if job.profile_id != catalog["profile_id"]
        || job.deck_id != deck["id"]
        || job.deck != deck["name"]
        || job
            .deck_uuid
            .as_ref()
            .is_some_and(|id| deck["crowdanki_uuid"] != *id)
    {
        return Err(error(
            "pending_export_identity_mismatch",
            "Сохранённое задание принадлежит изменившемуся источнику; ID сохранён, публикация и новая отправка запрещены",
        ));
    }
    // None → UUID допустим: первый экспорт CrowdAnki сам назначает UUID.
    Ok(())
}

fn missing_outcome(
    t: &Target,
    o: &Options,
    deck: &Value,
    pending: &[&source::PendingJob],
) -> Value {
    let jobs: Vec<_> = pending
        .iter()
        .map(|job| {
            let result = source::poll(t, o, job);
            json!({"job_id":job.id,"state":result.as_ref().ok().map(|v| &v["state"]),
            "reason":result.as_ref().err().map(|e| &e.details["reason"])})
        })
        .collect();
    json!({"name":deck["name"],"state":"failed",
        "reason":if jobs.is_empty() {"deck_not_found"} else {"pending_export_source_missing"},
        "input_confirmed":false,"anki_export_may_have_completed":!jobs.is_empty(),
        "pending_jobs":jobs,"message":"Колода отсутствует; прежний input и сохранённые задания сохранены"})
}

fn close_rejected_attempt(
    store: &mut publish::Store,
    key: &str,
    job: &source::PendingJob,
    mut failure: crate::error::DomainError,
) -> Result<crate::error::DomainError> {
    if failure.details["export_rejection_final"] == true {
        store
            .reject_job(
                key,
                failure.details["reason"]
                    .as_str()
                    .unwrap_or("export_validation_failed"),
            )
            .map_err(publication_error)?;
        failure.details["job_id"] = json!(job.id);
        failure.details["attempt_closed"] = json!(true);
        failure.message.push_str(
            "; попытка сохранена как отвергнутая, следующий явный вызов создаст новое задание",
        );
    }
    Ok(failure)
}

fn publication_error(e: crate::exchange::ExchangeError) -> crate::error::DomainError {
    // Сообщения parse/validate могут содержать приватные значения: наружу только класс.
    let reason = match e {
        crate::exchange::ExchangeError::Blocked(_) => "input_recovery_required",
        crate::exchange::ExchangeError::StaleState => "input_cas_conflict",
        crate::exchange::ExchangeError::CorruptState(_) => "input_state_corrupt",
        crate::exchange::ExchangeError::UnsafePath(_) => "input_unsafe_path",
        crate::exchange::ExchangeError::Io(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            "input_busy"
        }
        crate::exchange::ExchangeError::Io(_) => "input_durability_failure",
        _ => "input_verification_failed",
    };
    if reason == "input_busy" {
        return external_error(
            reason,
            "Владелец input занят; ожидание блокировки отключено. Дождитесь завершения другой команды и повторите вызов",
        );
    }
    let message = match &e {
        crate::exchange::ExchangeError::CorruptState(detail)
            if matches!(
                detail.as_str(),
                "каталог input не зарегистрирован в exchange"
                    | "неучтённый объект внутри потока input"
                    | "нет каталога подтверждённого потока"
                    | "неизвестный объект input"
            ) =>
        {
            "Перечень объектов input не согласован с revisions.json: обнаружены неучтённые данные или отсутствует подтверждённый поток. Данные сохранены; восстановите согласованное хранилище"
        }
        _ => {
            "Публикация input не подтверждена; прежний снимок/intent сохранены. Повторите sync-input для восстановления; не сбрасывайте exchange"
        }
    };
    external_error(reason, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proven_first_submission_refusal_survives_restart_without_erasing_old_uncertainty() {
        for prior in [false, true] {
            for (state, reason, message, proven) in [
                (None, "desktop_io", "ошибка до HTTP", true),
                (Some("rejected"), "api_error", "anki_busy", true),
                (Some("rejected"), "api_error", "addons_not_ready", true),
                (
                    Some("unknown"),
                    "http_unreachable",
                    "неизвестная отправка",
                    false,
                ),
                (
                    Some("rejected"),
                    "api_error",
                    "consumed_ticket: query toolkitJob",
                    false,
                ),
                (
                    Some("rejected"),
                    "api_error",
                    "ошибка сохранения результата",
                    false,
                ),
                (None, "api_error", "неполный API-отказ", false),
            ] {
                let repository = crate::test_support::TempDir::new("submission-refusal-proof");
                let (_spool, _target, mut job, _) = source::tests::live_fixture();
                let key = job.key();
                job.admission_started = true;
                let mut store = publish::Store::open(repository.path()).unwrap();
                store.save_job(&key, job.clone()).unwrap();
                let mut failure = external_error(reason, message);
                if let Some(state) = state {
                    failure.details["submission_state"] = json!(state);
                }
                let result = submission_error(&mut store, &key, &mut job, prior, failure);
                assert_eq!(result.is_ok(), state == Some("unknown"));
                drop(store);
                // Положительная проверка повторного открытия допускает занятость
                // advisory-блокировки, но не ждёт бесконечно и не меняет политику CLI.
                let deadline = Instant::now() + Duration::from_secs(5);
                let store = loop {
                    match publish::Store::open(repository.path()) {
                        Ok(store) => break store,
                        Err(crate::exchange::ExchangeError::Io(e))
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                let persisted = store.pending(&key).unwrap();
                assert_eq!(persisted.id, job.id);
                assert_eq!(
                    persisted.admission_started,
                    prior || !proven,
                    "{prior} {state:?} {message}"
                );
                if prior || !proven {
                    assert_eq!(
                        persisted
                            .prepared_action("after-restart")
                            .unwrap_err()
                            .details["reason"],
                        "export_completion_unknown_after_restart"
                    );
                } else {
                    assert_eq!(
                        persisted.prepared_action("after-restart").unwrap(),
                        source::PreparedAction::RenewSource
                    );
                }
            }
        }
    }

    #[test]
    fn busy_input_refuses_apply_before_anki_setup_and_dry_run_never_opens_store() {
        let (_spool, mut target, _job, _) = source::tests::live_fixture();
        target.running = false;
        let options = Options {
            action: crate::desktop::Action::Doctor,
            executable: Some(target.executable.clone()),
            data_dir: Some(target.data.clone()),
            profile: Some("Synthetic".into()),
            endpoint: None,
            api_key_env: "ANKI_TEST_UNUSED_API_KEY".into(),
        };
        let repository = crate::test_support::TempDir::new("busy-before-anki");
        let _owner = publish::Store::open(repository.path()).unwrap();
        let failure = run(
            &target,
            &options,
            repository.path(),
            &["Selected".into()],
            false,
            1,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.details["reason"], "input_busy");
        // Без ранней блокировки этот путь дошёл бы до export_setup_required.
        let dry = crate::test_support::TempDir::new("input-plan-without-store");
        let plan = run(
            &target,
            &options,
            dry.path(),
            &["Selected".into()],
            false,
            1,
            false,
        )
        .unwrap();
        assert_eq!(plan["state"], "input_plan");
        assert!(!dry.path().join("decks").exists());
        // Ветка работающей Anki тоже не открывает Store: даже ошибка
        // разрешения отсутствующего тестового исполняемого файла не создаёт decks.
        target.running = true;
        assert!(
            run(
                &target,
                &options,
                dry.path(),
                &["Selected".into()],
                false,
                1,
                false
            )
            .is_err()
        );
        assert!(!dry.path().join("decks").exists());
    }

    #[test]
    fn unrelated_names_do_not_block_selected_exports_but_descendants_are_checked() {
        let catalog = json!({"decks":[{"id":"1","name":"Good"},{"id":"2","name":"Что это?"},
            {"id":"3","name":"Root"},{"id":"4","name":"Root::Bad?"}]});
        assert_eq!(
            select(&catalog, &["Good".into()], false).unwrap()[0]["name"],
            "Good"
        );
        for (requested, all) in [
            (vec!["Что это?".into()], false),
            (vec!["Root".into()], false),
            (vec![], true),
        ] {
            assert_eq!(
                select(&catalog, &requested, all).unwrap_err().details["reason"],
                "invalid_export_deck_name"
            );
        }
    }

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
