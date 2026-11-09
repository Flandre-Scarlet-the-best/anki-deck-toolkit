//! Проверенный импорт рабочей копии output в связанный профиль Anki.
//!
//! Последовательность одной публикации:
//!
//! 1. `plan_publication` (A == C, B совместим); B != C, иначе публиковать нечего;
//! 2. свежий живой экспорт той же колоды через существующий путь `sync-input`:
//!    подтверждённый C обязан остаться семантически прежним, иначе Anki
//!    изменилась после подготовки и импорт останавливается;
//! 3. повторный `plan_publication`; B копируется в приватный spool под
//!    `ExportLock` и сверяется с ревизией разрешения;
//! 4. до HTTP надёжно сохраняется журнал `publish.json` с признаком отправки;
//! 5. оригинальный CrowdAnki importer через `toolkitImport`, опрос того же job;
//! 6. readback — новый живой экспорт той же колоды того же профиля;
//! 7. readback == B: тем же протоколом публикации input фиксируется
//!    `record_published` (intent переживает сбой); иначе readback сохраняется как
//!    наблюдённый C без ложного published, B не трогается.
//!
//! Неопределённый исход никогда не отправляет импорт повторно: следующий вызов
//! опрашивает прежний job, а фактическое состояние Anki определяет readback.
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::publish::{PublicationRecord, Store};
use super::source::VerifiedExport;
use super::work::{self, Inventory, PREPARE, PUBLISH, read_inventory};
use super::{Options, Result, environment::Target, error, external_error, patch, transport};
use crate::exchange::{DeckIdentity, ExchangeError, Snapshot, StreamState, filesystem::Directory};

/// Журнал одной попытки импорта. Разрешение exchange в нём не хранится:
/// при фиксации оно выдаётся заново и обязано совпасть с этими полями.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportJournal {
    pub job_id: String,
    pub identity: DeckIdentity,
    pub profile: String,
    pub process: String,
    pub output_directory: String,
    pub working_revision: String,
    pub working_inventory: Inventory,
    pub checked_input_sequence: u64,
    pub checked_input_revision: String,
    pub base_sequence: u64,
    pub admission_started: bool,
    pub import_state: Option<String>,
    pub import_error: Option<String>,
    pub partial_changes_possible: Option<bool>,
}

/// Живые операции Anki; тесты подменяют их, не трогая журнал и решения.
pub(super) trait Bridge {
    fn status(&self, id: &str) -> Result<Value>;
    fn submit(&self, id: &str) -> Result<Value>;
    fn renew(&self, journal: &ImportJournal) -> Result<()>;
}

/// Все остальные живые обращения оркестрации: проверка актуальности, readback
/// и приватный spool Desktop. Решения и журналы от реализации не зависят.
pub(super) trait Port: Bridge {
    fn profile(&self) -> &str;
    /// Приватный `toolkit-desktop/spool` выбранной базы Anki.
    fn spool(&self) -> Result<Directory>;
    /// Свежий живой экспорт, подтверждённый тем же путём, что `sync-input`.
    fn fresh(&self, store: &mut Store, catalog: &Value, deck: &Value, key: &str) -> Result<Value>;
    /// Живой экспорт без публикации; pending job завершает вызывающий.
    fn readback(
        &self,
        store: &mut Store,
        catalog: &Value,
        deck: &Value,
        key: &str,
    ) -> Result<VerifiedExport>;
}

struct Live<'a> {
    target: &'a Target,
    options: &'a Options,
    wait_seconds: u64,
}

impl Port for Live<'_> {
    fn profile(&self) -> &str {
        self.options.profile.as_deref().unwrap_or_default()
    }
    fn spool(&self) -> Result<Directory> {
        let data = Directory::open(&self.target.data).map_err(super::io_error)?;
        let state = data
            .ensure_child("toolkit-desktop")
            .map_err(super::io_error)?
            .0;
        state.require_private().map_err(|_| {
            error(
                "export_spool_not_private",
                "Каталог toolkit-desktop не обеспечивает приватность spool; импорт не отправлен",
            )
        })?;
        Ok(state.ensure_child("spool").map_err(super::io_error)?.0)
    }
    fn fresh(&self, store: &mut Store, catalog: &Value, deck: &Value, key: &str) -> Result<Value> {
        super::export_one(
            self.target,
            self.options,
            catalog,
            deck,
            key,
            store,
            self.wait_seconds,
        )
    }
    fn readback(
        &self,
        store: &mut Store,
        catalog: &Value,
        deck: &Value,
        key: &str,
    ) -> Result<VerifiedExport> {
        super::obtain_export(
            self.target,
            self.options,
            catalog,
            deck,
            key,
            store,
            self.wait_seconds,
        )
        .map(|(verified, _)| verified)
    }
}

impl Bridge for Live<'_> {
    fn status(&self, id: &str) -> Result<Value> {
        // resolve заново подтверждает владельца порта и процесс Anki.
        let current = super::environment::resolve(self.options)?;
        crate::desktop::jobs::status(&current, self.options, id)
    }
    fn submit(&self, id: &str) -> Result<Value> {
        let current = super::environment::resolve(self.options)?;
        transport::patched(&current, self.options, "toolkitImport", json!({"id":id}))
    }
    fn renew(&self, journal: &ImportJournal) -> Result<()> {
        let job = spool_job(self, &journal.job_id)?;
        let mut ticket: Value =
            serde_json::from_slice(&job.read("request.json").map_err(super::io_error)?)
                .map_err(super::io_error)?;
        if ticket["action"] != "import" || ticket["profile_id"] != journal.identity.profile.as_str()
        {
            return Err(error(
                "import_ticket_changed",
                "Сохранённый ticket импорта изменился",
            ));
        }
        ticket["expires"] = json!(expiry()?);
        job.publish(
            "request.json",
            &serde_json::to_vec(&ticket).map_err(super::io_error)?,
        )
        .map_err(super::io_error)
    }
}

/// Итог продвижения журнала без readback.
#[derive(Debug)]
pub(super) enum Progress {
    /// Job завершён либо его судьба после рестарта неизвестна: решает readback.
    Terminal,
    /// Нужно повторить ту же команду; журнал сохранён.
    Pending(&'static str, Option<String>),
    /// Доказано, что импорт не отправлялся; журнал можно закрыть.
    NotSubmitted(String),
}

pub(super) fn drive(
    bridge: &dyn Bridge,
    dir: &Directory,
    journal: &mut ImportJournal,
    wait: Duration,
) -> std::result::Result<Progress, ExchangeError> {
    if journal.import_state.is_some() {
        return Ok(Progress::Terminal);
    }
    let end = Instant::now() + wait;
    let mut sent = false;
    loop {
        let status = match bridge.status(&journal.job_id) {
            Ok(status) => status,
            Err(e) => return Ok(Progress::Pending("import_status_unknown", Some(e.message))),
        };
        if status["live_verified"] == false {
            return Ok(Progress::Pending(
                "import_status_unknown",
                Some(
                    "Anki недоступна: сохранённый result.json без живого API не доказывает исход"
                        .into(),
                ),
            ));
        }
        let job = &status["job"];
        match status["state"].as_str() {
            Some("prepared") => {
                let same_process = job["observer_process"].as_str() == Some(&journal.process);
                if !same_process {
                    if !journal.admission_started {
                        return Ok(Progress::NotSubmitted(
                            "Anki перезапущена до отправки; импорт не выполнялся".into(),
                        ));
                    }
                    // После рестарта prepared не доказывает отсутствие импорта.
                    journal.import_state = Some("unknown_after_restart".into());
                    journal.partial_changes_possible = Some(true);
                    work::save(dir, PUBLISH, journal)?;
                    return Ok(Progress::Terminal);
                }
                if !sent {
                    let prior = journal.admission_started;
                    if let Err(e) = bridge.renew(journal) {
                        return Ok(Progress::Pending(
                            "import_ticket_unavailable",
                            Some(e.message),
                        ));
                    }
                    if !prior {
                        // Намерение отправки сохраняется ДО HTTP.
                        journal.admission_started = true;
                        work::save(dir, PUBLISH, journal)?;
                    }
                    sent = true;
                    if let Err(e) = bridge.submit(&journal.job_id) {
                        let preflight = e.details["submission_state"].is_null()
                            && e.details["reason"] != "api_error";
                        let refused = e.details["submission_state"] == "rejected"
                            && matches!(e.message.as_str(), "anki_busy" | "addons_not_ready");
                        if !prior && (preflight || refused) {
                            return Ok(Progress::NotSubmitted(e.message));
                        }
                        return Ok(Progress::Pending(
                            "import_submission_unknown",
                            Some(e.message),
                        ));
                    }
                }
            }
            Some(state @ ("succeeded" | "failed" | "cancelled" | "interrupted")) => {
                if job["action"] != "import" || job["id"] != journal.job_id.as_str() {
                    return Ok(Progress::Pending(
                        "import_evidence_mismatch",
                        Some("Живое задание не соответствует журналу импорта".into()),
                    ));
                }
                journal.import_state = Some(state.into());
                journal.import_error = job["error"].as_str().map(str::to_owned);
                journal.partial_changes_possible = Some(
                    job["partial_changes_possible"]
                        .as_bool()
                        .unwrap_or(matches!(state, "succeeded" | "interrupted")),
                );
                work::save(dir, PUBLISH, journal)?;
                return Ok(Progress::Terminal);
            }
            Some("queued" | "running") => {}
            _ => return Ok(Progress::Pending("import_status_unknown", None)),
        }
        if Instant::now() >= end {
            return Ok(Progress::Pending(
                "import_awaiting_user",
                Some("Импорт ждёт ответа на штатный диалог CrowdAnki в Anki".into()),
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Уже ли exchange подтвердил именно эту попытку (повтор после сбоя записи журнала).
pub(super) fn already_published(
    store: &Store,
    journal: &ImportJournal,
) -> std::result::Result<bool, ExchangeError> {
    let state = store.exchange().read()?;
    Ok(state
        .streams
        .get(&journal.identity.key())
        .and_then(|s| s.published.as_ref())
        .is_some_and(|p| {
            p.working_revision == journal.working_revision
                && p.checked_input_sequence == journal.checked_input_sequence
        }))
}

/// Решение по фактическому readback. Журнал закрывается только после
/// надёжной публикации результата в input/exchange.
pub(super) fn conclude(
    store: &Store,
    dir: &Directory,
    journal: &ImportJournal,
    readback: &VerifiedExport,
    sent: &Snapshot,
) -> std::result::Result<Value, ExchangeError> {
    if readback.snapshot().identity() != &journal.identity {
        return Err(ExchangeError::IdentityMismatch);
    }
    let matches = readback.snapshot().revision() == journal.working_revision;
    let mut outcome = json!({"job_id":journal.job_id,"import_state":journal.import_state,
        "import_error":journal.import_error,"partial_changes_possible":journal.partial_changes_possible,
        "working_revision":journal.working_revision,"readback_revision":readback.snapshot().revision(),
        "readback_matches":matches});
    if matches {
        let result = store.publish_readback(
            readback,
            PublicationRecord {
                working_revision: journal.working_revision.clone(),
                checked_input_sequence: journal.checked_input_sequence,
                base_sequence: journal.base_sequence,
                import_job: journal.job_id.clone(),
            },
        )?;
        let confirmed = result["publication_confirmed"] == true;
        outcome["state"] = json!(if confirmed {
            "published"
        } else {
            "import_applied_unconfirmed"
        });
        outcome["published"] = json!(confirmed);
        outcome["source_sequence"] = result["source_sequence"].clone();
        if !confirmed {
            outcome["reason"] = json!("working_changed_after_import");
            outcome["next"] = json!(
                "Anki совпала с отправленным B, но рабочая копия изменилась до фиксации: readback сохранён как C, published не установлен"
            );
        }
    } else {
        let result = store.publish(readback)?;
        let comparison = crate::exchange::compare(sent, readback.snapshot(), sent)?;
        outcome["published"] = json!(false);
        outcome["source_sequence"] = result["source_sequence"].clone();
        outcome["differences"] = work::difference_summary(&comparison);
        if readback.snapshot().revision() == journal.checked_input_revision {
            outcome["state"] = json!("import_not_applied");
            outcome["reason"] = json!(match journal.import_state.as_deref() {
                Some("cancelled") => "import_cancelled",
                Some("failed") => "import_failed",
                _ => "import_not_applied",
            });
            outcome["next"] = json!(
                "Anki не изменилась: B сохранён, следующий явный publish-output --apply создаст новую попытку"
            );
        } else {
            outcome["state"] = json!("readback_mismatch");
            outcome["reason"] = json!("readback_differs_from_sent");
            outcome["next"] = json!(
                "Фактическое состояние Anki отличается от B (например, CrowdAnki не удаляет отсутствующие заметки). Readback сохранён как C, B не изменён; требуется явное согласование"
            );
        }
    }
    dir.remove_file(PUBLISH)?;
    Ok(outcome)
}

fn spool_job(port: &dyn Port, id: &str) -> Result<Directory> {
    port.spool()?
        .child(id)
        .map_err(super::io_error)?
        .ok_or_else(|| error("import_spool_missing", "Нет каталога задания импорта"))
}

fn expiry() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(super::io_error)?
        .as_secs()
        + 600)
}

fn ex(e: ExchangeError) -> crate::error::DomainError {
    super::publication_error(e)
}

/// План без Anki и без записи: разрешение exchange и различия A→B.
pub(super) fn plan(repository: &Path, outputs: &[String]) -> Value {
    let outcome = (|| -> Result<Value> {
        crate::layout::guard_local_path(repository, "Корень публикации output")?;
        let store = Store::open_existing(repository).map_err(ex)?;
        let state = store.exchange().read().map_err(ex)?;
        let mut decks = Vec::new();
        for item in super::prepare::select(&state, outputs, false) {
            decks.push(match item {
                Ok(stream) => plan_one(&store, repository, &stream).unwrap_or_else(|e| {
                    let mut v = super::prepare::describe(&stream);
                    v["state"] = json!("blocked");
                    v["reason"] = json!(e.details["reason"]);
                    v["message"] = json!(e.message);
                    v
                }),
                Err(name) => json!({"output_directory":name,"state":"blocked","reason":"output_not_registered"}),
            });
        }
        let ok = decks.iter().all(|d| d["state"] == "planned");
        Ok(
            json!({"state":if ok {"publish_plan"} else {"publish_plan_blocked"},"apply":false,"decks":decks,
            "consequences":"Импорт выполняет оригинальный CrowdAnki с видимым диалогом и backup коллекции. Отсутствующие в JSON заметки не удаляются из Anki; ошибка может оставить частичные изменения. Перед импортом выполняется свежий экспорт для проверки актуальности, после — readback"}),
        )
    })();
    outcome.unwrap_or_else(
        |e| json!({"state":"failed","reason":e.details["reason"],"message":e.message}),
    )
}

fn plan_one(store: &Store, repository: &Path, stream: &StreamState) -> Result<Value> {
    let identity = stream.current.snapshot.identity();
    let mut value = super::prepare::describe(stream);
    value["pending_journal"] = json!(publication_dir(store, &identity.key()).map_err(ex)?);
    let permit = match store.exchange().plan_publication(identity) {
        Ok(permit) => permit,
        Err(ExchangeError::Blocked(reason)) => {
            value["state"] = json!("blocked");
            value["reason"] = json!("publication_blocked");
            value["detail"] = json!(reason);
            value["next"] = json!(
                "Сначала prepare-output: публикация разрешена только при актуальной базе A == C и совместимом B"
            );
            return Ok(value);
        }
        Err(e) => return Err(ex(e)),
    };
    value["working_revision"] = json!(permit.working_revision());
    if permit.working_revision() == stream.current.snapshot.revision() {
        value["state"] = json!("nothing_to_publish");
        return Ok(value);
    }
    let working = Snapshot::read_working(
        identity.profile.clone(),
        &repository
            .join("decks/output")
            .join(&stream.output_directory),
    )
    .map_err(ex)?;
    let base = stream.base.as_ref().expect("разрешение требует базу");
    let comparison =
        crate::exchange::compare(&base.snapshot, &stream.current.snapshot, &working).map_err(ex)?;
    value["comparison"] = work::difference_summary(&comparison);
    value["state"] = json!("planned");
    value["expected_user_action"] =
        json!("Подтвердить штатный диалог импорта CrowdAnki в Anki; отмена не изменит базу");
    Ok(value)
}

fn publication_dir(store: &Store, key: &str) -> std::result::Result<bool, ExchangeError> {
    store.publication_pending(key)
}

/// Применение для каждой выбранной колоды независимо.
pub(super) fn apply(
    t: &Target,
    o: &Options,
    repository: &Path,
    outputs: &[String],
    wait_seconds: u64,
) -> Result<Value> {
    if o.profile.is_none() {
        return Err(error(
            "profile_required",
            "Укажите точный --profile; автоматическое переключение запрещено",
        ));
    }
    crate::layout::guard_local_path(repository, "Корень публикации output")?;
    let mut store = Store::open_existing(repository).map_err(ex)?;
    // Незавершённые intent input, включая фиксацию readback, завершаются первыми.
    let recovered = store.recover().map_err(ex)?;
    let state = store.exchange().read().map_err(ex)?;
    let selected = super::prepare::select(&state, outputs, false);
    let target = super::ready_target(t, o, true)?;
    let catalog = transport::patched(&target, o, "toolkitCatalog", json!({}))?;
    let port = Live {
        target: &target,
        options: o,
        wait_seconds,
    };
    let mut decks = Vec::new();
    for item in selected {
        decks.push(match item {
            Ok(stream) => apply_one(&port, &mut store, &catalog, &stream, wait_seconds)
                .unwrap_or_else(|e| {
                    let mut v = super::prepare::describe(&stream);
                    v["state"] = json!("failed");
                    v["reason"] = json!(e.details["reason"]);
                    v["message"] = json!(e.message);
                    v["published"] = json!(false);
                    v
                }),
            Err(name) => json!({"output_directory":name,"state":"blocked","reason":"output_not_registered","published":false}),
        });
    }
    let ok = decks.iter().all(|d| {
        matches!(
            d["state"].as_str(),
            Some("published" | "nothing_to_publish")
        )
    });
    Ok(
        json!({"state":if ok {"succeeded"} else {"partial"},"profile":o.profile,
        "profile_id":catalog["profile_id"],"recovered":recovered,"decks":decks,
        "message":"published означает совпадение живого readback с отправленным B и подтверждение exchange"}),
    )
}

pub(super) fn apply_one(
    port: &dyn Port,
    store: &mut Store,
    catalog: &Value,
    stream: &StreamState,
    wait_seconds: u64,
) -> Result<Value> {
    let identity = stream.current.snapshot.identity().clone();
    let key = identity.key();
    let mut value = super::prepare::describe(stream);
    value["published"] = json!(false);
    let blocked = |mut value: Value, reason: &str, next: &str| {
        value["state"] = json!("blocked");
        value["reason"] = json!(reason);
        value["next"] = json!(next);
        Ok(value)
    };
    if catalog["profile_id"] != identity.profile.as_str() {
        return blocked(
            value,
            "profile_mismatch",
            "Активный профиль Anki не связан с этой колодой: откройте нужный профиль и повторите",
        );
    }
    let area = store.work_area(true).map_err(ex)?.expect("создана");
    let dir = work::stream_dir(&area, &key).map_err(ex)?;
    if dir.entry_is_dir(PREPARE).map_err(ex)?.is_some() {
        return blocked(
            value,
            "prepare_recovery_pending",
            "Сначала завершите prepare-output --apply этой колоды",
        );
    }
    let mut journal = work::load::<ImportJournal>(&dir, PUBLISH).map_err(ex)?;
    let deck = catalog["decks"]
        .as_array()
        .and_then(|decks| {
            decks
                .iter()
                .find(|d| d["crowdanki_uuid"] == identity.crowdanki_uuid.as_str())
        })
        .cloned();
    let Some(deck) = deck else {
        return blocked(
            value,
            "deck_not_found_in_anki",
            "Колода с этим CrowdAnki UUID отсутствует в профиле; журнал импорта сохранён",
        );
    };
    let job_key = asset_store::hashing::sha256_hex(
        serde_json::to_vec(&json!([catalog["profile_id"], deck["id"]])).map_err(super::io_error)?,
    );
    if let Some(existing) = &journal {
        if existing.identity != identity {
            return Err(ex(work::corrupt("идентичность журнала импорта")));
        }
        if already_published(store, existing).map_err(ex)? {
            if store.pending(&job_key).is_some() {
                store.finish_job(&job_key).map_err(ex)?;
            }
            dir.remove_file(PUBLISH).map_err(ex)?;
            value["state"] = json!("published");
            value["published"] = json!(true);
            value["job_id"] = json!(existing.job_id);
            value["recovered"] = json!(true);
            return Ok(value);
        }
    }
    if journal.is_none() {
        let first = match store.exchange().plan_publication(&identity) {
            Ok(permit) => permit,
            Err(ExchangeError::Blocked(_)) => {
                return blocked(
                    value,
                    "publication_blocked",
                    "Сначала prepare-output: нужна актуальная база A == C",
                );
            }
            Err(e) => return Err(ex(e)),
        };
        if first.working_revision() == stream.current.snapshot.revision() {
            value["state"] = json!("nothing_to_publish");
            return Ok(value);
        }
        // Свежий живой экспорт тем же доверенным путём sync-input.
        let fresh = match port.fresh(store, catalog, &deck, &job_key) {
            Ok(fresh) => fresh,
            Err(e) => {
                value["fresh_export_error"] = json!(e.message);
                return blocked(
                    value,
                    "freshness_unconfirmed",
                    "Не удалось подтвердить актуальность Anki: повторите ту же команду",
                );
            }
        };
        value["fresh_input"] = json!({"source_sequence":fresh["source_sequence"],"revision":fresh["revision"],"job_id":fresh["job_id"]});
        let permit = match store.exchange().plan_publication(&identity) {
            Ok(permit) => permit,
            Err(ExchangeError::Blocked(_)) => {
                return blocked(
                    value,
                    "anki_changed_since_input",
                    "Живая Anki изменилась после подготовки: изменения сохранены как новый C. Выполните prepare-output; импорт не выполнялся",
                );
            }
            Err(e) => return Err(ex(e)),
        };
        if permit.working_revision() != first.working_revision() {
            return blocked(
                value,
                "working_changed_since_plan",
                "Рабочая копия изменилась во время проверки: повторите команду",
            );
        }
        let state = store.exchange().read().map_err(ex)?;
        let current = &state.streams[&key].current;
        let staged = stage_import(
            port,
            store,
            catalog,
            &permit,
            stream,
            current.snapshot.revision(),
        )?;
        journal = Some(staged);
    }
    let mut journal = journal.expect("журнал создан или восстановлен");
    value["job_id"] = json!(journal.job_id);
    let progress =
        drive(port, &dir, &mut journal, Duration::from_secs(wait_seconds)).map_err(ex)?;
    match progress {
        Progress::Pending(reason, detail) => {
            value["state"] = json!("pending");
            value["reason"] = json!(reason);
            value["detail"] = json!(detail);
            value["import_state"] = json!(journal.import_state);
            value["expected_user_action"] = json!(if reason == "import_awaiting_user" {
                "Ответьте на диалог импорта CrowdAnki в Anki, затем повторите ту же команду"
            } else {
                "Повторите ту же команду: будет опрошен тот же job, повторной отправки нет"
            });
            return Ok(value);
        }
        Progress::NotSubmitted(detail) => {
            if let Ok(job) = spool_job(port, &journal.job_id) {
                let _ = job.remove_file("request.json");
            }
            dir.remove_file(PUBLISH).map_err(ex)?;
            value["state"] = json!("import_not_submitted");
            value["reason"] = json!("import_not_submitted");
            value["detail"] = json!(detail);
            value["next"] = json!("Импорт точно не отправлялся: повторите publish-output --apply");
            return Ok(value);
        }
        Progress::Terminal => {}
    }
    let readback = match port.readback(store, catalog, &deck, &job_key) {
        Ok(result) => result,
        Err(e) => {
            value["state"] = json!("pending");
            value["reason"] = json!("readback_unconfirmed");
            value["detail"] = json!(e.message);
            value["import_state"] = json!(journal.import_state);
            value["expected_user_action"] = json!(
                "Повторите ту же команду: readback будет получен по сохранённому заданию; импорт не повторяется"
            );
            return Ok(value);
        }
    };
    let job = spool_job(port, &journal.job_id)?;
    let source = job
        .child("source")
        .map_err(super::io_error)?
        .ok_or_else(|| error("import_spool_missing", "Нет отправленной копии B в spool"))?;
    let (sent, _) = read_inventory(&source, identity.profile.clone()).map_err(ex)?;
    let outcome = conclude(store, &dir, &journal, &readback, &sent).map_err(ex)?;
    if store.finish_job(&job_key).is_err() {
        value["pending_cleanup"] = json!("unknown");
    }
    // A/C/published после фиксации, а не снимок до импорта.
    if let Some(current) = store.exchange().read().map_err(ex)?.streams.get(&key) {
        for (k, v) in super::prepare::describe(current)
            .as_object()
            .expect("объект")
        {
            value[k] = v.clone();
        }
    }
    for (k, v) in outcome.as_object().expect("объект") {
        value[k] = v.clone();
    }
    Ok(value)
}

fn stage_import(
    port: &dyn Port,
    store: &Store,
    catalog: &Value,
    permit: &crate::exchange::Publication,
    stream: &StreamState,
    checked_revision: &str,
) -> Result<ImportJournal> {
    let identity = permit.identity().clone();
    let output = store
        .decks()
        .child("output")
        .map_err(ex)?
        .ok_or_else(|| error("working_missing", "Нет decks/output"))?;
    let id = patch::random_id()?;
    let spool = port.spool()?;
    let (job, created) = spool.ensure_child(&id).map_err(super::io_error)?;
    if !created {
        return Err(error(
            "import_job_id_collision",
            "Каталог нового job уже существует",
        ));
    }
    let (source, _) = job.ensure_child("source").map_err(super::io_error)?;
    let (_working, lock) = work::lock_export(&output, &stream.output_directory)
        .map_err(ex)?
        .ok_or_else(|| error("working_missing", "Рабочая копия отсутствует"))?;
    let inventory = work::copy_export(&_working, &source, None).map_err(ex)?;
    drop(lock);
    let (sent, checked) = read_inventory(&source, identity.profile.clone()).map_err(ex)?;
    if sent.revision() != permit.working_revision() || checked != inventory {
        return Err(error(
            "working_changed_since_plan",
            "Рабочая копия изменилась во время копирования; импорт не отправлен",
        ));
    }
    let ticket = json!({"approved":true,"action":"import","profile":port.profile(),
        "profile_id":identity.profile.as_str(),"process":catalog["process"],"expires":expiry()?});
    job.publish(
        "request.json",
        &serde_json::to_vec(&ticket).map_err(super::io_error)?,
    )
    .map_err(super::io_error)?;
    let journal = ImportJournal {
        job_id: id,
        identity: identity.clone(),
        profile: port.profile().to_owned(),
        process: catalog["process"].as_str().unwrap_or_default().to_owned(),
        output_directory: stream.output_directory.clone(),
        working_revision: permit.working_revision().into(),
        working_inventory: inventory,
        checked_input_sequence: permit.source_sequence(),
        checked_input_revision: checked_revision.into(),
        base_sequence: permit.base_sequence(),
        admission_started: false,
        import_state: None,
        import_error: None,
        partial_changes_possible: None,
    };
    if journal.process.is_empty() {
        return Err(external_error(
            "invalid_export_catalog",
            "Нет процесса Anki в каталоге",
        ));
    }
    let area = store.work_area(true).map_err(ex)?.expect("создана");
    let dir = work::stream_dir(&area, &identity.key()).map_err(ex)?;
    work::save(&dir, PUBLISH, &journal).map_err(ex)?;
    Ok(journal)
}
