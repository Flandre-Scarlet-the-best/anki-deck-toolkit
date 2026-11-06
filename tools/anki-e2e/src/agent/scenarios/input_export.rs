//! Настоящий CLI → CrowdAnki → доверенный владелец input внутри контейнера.
//! Тестовый стенд не читает input: байты проверяет input-status, затем SHA сверяются
//! с независимым живым spool и заранее известными медиа/заметками.
use super::*;

const SECOND: &str = "E2E Second Root";
const CHILD: &str = "E2E Toolkit Deck::Child";

fn sync(c: &mut Ctx, repo: &Path, decks: &[&str], apply: bool) -> Result<Value, Fail> {
    let repo = repo.to_string_lossy().into_owned();
    let mut args = vec!["sync-input", "--repository", &repo];
    for deck in decks {
        args.extend(["--deck", *deck]);
    }
    if apply {
        args.push("--apply");
    }
    let result = c.anki_repo(&args, &Policy::none())?;
    c.check(
        "sync-input: ожидаемый исход",
        result.exit == 0,
        result.json.clone(),
    )?;
    Ok(result.result().clone())
}

fn audit(c: &mut Ctx, repo: &Path) -> Result<Value, Fail> {
    let result = c.anki_repo(
        &["input-status", "--repository", &repo.to_string_lossy()],
        &Policy::none(),
    )?;
    c.check(
        "input-status перепроверил физические байты, receipt и exchange",
        result.exit == 0 && result.state() == "succeeded",
        result.json.clone(),
    )?;
    Ok(result.result().clone())
}

fn published(result: &Value, name: &str) -> Result<Value, Fail> {
    result["decks"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["name"] == name))
        .cloned()
        .ok_or_else(|| {
            Fail::failed(
                "input_export_missing_result",
                format!("Нет результата колоды {name}"),
            )
        })
}

fn stream(audit: &Value, item: &Value) -> Result<Value, Fail> {
    audit["streams"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["identity"] == item["identity"]))
        .cloned()
        .ok_or_else(|| Fail::failed("input_export_missing_stream", "Нет сохранённого потока"))
}

pub(super) fn run(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    fx.base_patched(c)?;
    let repo = c.dir.join("synthetic-repository");
    fs::create_dir_all(repo.join(".git")).map_err(|e| Fail::error("io", e.to_string()))?;
    c.step("input: запущенная Anki и синтетические данные");
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    create_fixture_data(c)?;
    c.connect("createDeck", json!({"deck":CHILD}))?;
    c.connect("createDeck", json!({"deck":SECOND}))?;
    c.connect(
        "createModel",
        json!({"modelName":"E2E Child Model","inOrderFields":["Term","Meaning"],
        "cardTemplates":[{"Name":"Card","Front":"{{Term}}","Back":"{{Meaning}}"}]}),
    )?;
    c.connect(
        "addNote",
        json!({"note":{"deckName":CHILD,"modelName":"E2E Child Model",
        "fields":{"Term":"child-term","Meaning":"child-value"},"tags":["e2e::child"]}}),
    )?;
    c.connect(
        "addNote",
        json!({"note":{"deckName":SECOND,"modelName":MODEL,
        "fields":{"Front":"second-front","Back":"second-back"},"tags":["e2e::second"]}}),
    )?;
    c.step("input: dry-run не создаёт файлов");
    let plan = sync(c, &repo, &[DECK, CHILD, SECOND], false)?;
    c.check(
        "План схлопывает выбранные поддеревья без записи",
        plan["state"] == "input_plan"
            && plan["decks"].as_array().is_some_and(|v| v.len() == 2)
            && !repo.join("decks").exists(),
        plan,
    )?;

    c.step("input: первая полная публикация V1");
    let first = sync(c, &repo, &[DECK, CHILD, SECOND], true)?;
    let one = published(&first, DECK)?;
    let second = published(&first, SECOND)?;
    c.check(
        "Выбранные два корня опубликованы, третьи колоды не затронуты",
        first["decks"].as_array().is_some_and(|v| v.len() == 2)
            && one["source_sequence"] == 1
            && second["source_sequence"] == 1
            && !repo.join("decks/output").exists(),
        first.clone(),
    )?;
    let id = one["job_id"].as_str().unwrap_or("");
    let job = c.anki_repo(&["job", id], &Policy::none())?;
    c.check(
        "Публикация связана с действительно завершённым live export",
        job.state() == "succeeded"
            && job.result()["job"]["profile_id"] == one["identity"]["profile"]
            && job.result()["job"]["deck_uuid"] == one["identity"]["crowdanki_uuid"],
        job.json.clone(),
    )?;
    let spool = PathBuf::from(job.result()["local_export_dir"].as_str().unwrap_or(""));
    let deck: Value = serde_json::from_slice(
        &fs::read(spool.join("deck.json")).map_err(|e| Fail::error("io", e.to_string()))?,
    )
    .map_err(|e| Fail::error("json", e.to_string()))?;
    let notes = export_notes(&deck);
    c.check(
        "Живой exporter сохранил дерево, обе модели, теги и все GUID",
        deck["children"].as_array().is_some_and(|v| v.len() == 1)
            && notes.len() == NOTES.len() + 1
            && notes
                .iter()
                .all(|n| n["guid"].as_str().is_some_and(|s| !s.is_empty()))
            && NOTES.iter().all(|spec| {
                notes
                    .iter()
                    .any(|n| n["fields"][0] == spec.front && n["fields"][1] == spec.back)
            }),
        json!({"notes":notes.len(),"root_uuid":deck["crowdanki_uuid"]}),
    )?;
    let stored = audit(c, &repo)?;
    let durable = stream(&stored, &one)?;
    c.check(
        "Durable input содержит ровно байты job, включая полный физический PNG",
        durable["inventory"] == job.result()["job"]["inventory"]
            && durable["inventory"][format!("media/{MEDIA}")] == fixtures::pixel_sha256()
            && durable["revision"] == one["revision"]
            && durable["source_sequence"] == 1,
        durable.clone(),
    )?;

    c.step("input: закрытая область недоступна обычным CLI и новому пути через псевдоним");
    let input = repo.join(one["input_directory"].as_str().unwrap_or(""));
    let denied = c.anki_repo_raw(
        &["--json", "inspect", &input.to_string_lossy()],
        &Policy::none(),
    )?;
    c.check(
        "inspect не читает синтетический input",
        denied.error_reason() == "input_area_forbidden",
        denied.json,
    )?;
    let denied = c.anki_repo(
        &[
            "sync-input",
            "--repository",
            &input.to_string_lossy(),
            "--deck",
            DECK,
            "--apply",
        ],
        &Policy::none(),
    )?;
    c.check(
        "sync-input не принимает input как repository",
        denied.error_reason() == "input_area_forbidden",
        denied.json,
    )?;

    c.step("input: новый live export без содержательной правки");
    let again = published(&sync(c, &repo, &[DECK], true)?, DECK)?;
    c.check(
        "Новый job увеличивает sequence без ложной содержательной ревизии",
        again["revision"] == one["revision"]
            && again["source_sequence"] == 2
            && again["content_changed"] == false
            && again["job_id"] != one["job_id"],
        again.clone(),
    )?;
    let all_notes = c.connect(
        "findNotes",
        json!({"query":format!("\"deck:{DECK}\" -\"deck:{CHILD}\"")}),
    )?;
    let note = all_notes
        .as_array()
        .and_then(|v| v.first())
        .cloned()
        .ok_or_else(|| Fail::failed("fixture_missing", "Нет заметки для V2"))?;
    c.step("input: живое изменение V2");
    c.connect(
        "updateNoteFields",
        json!({"note":{"id":note,"fields":{"Back":"changed-in-Anki-V2"}}}),
    )?;
    let v2 = published(&sync(c, &repo, &[DECK], true)?, DECK)?;
    c.check(
        "V2 подтверждена отдельно от V1",
        v2["revision"] != one["revision"] && v2["source_sequence"] == 3,
        v2.clone(),
    )?;

    c.step("input: независимый output и V3");
    let working = repo.join("decks/output/independent");
    fs::create_dir_all(&working).map_err(|e| Fail::error("io", e.to_string()))?;
    let working_bytes = b"independent-working-copy-sentinel";
    fs::write(working.join("deck.json"), working_bytes)
        .map_err(|e| Fail::error("io", e.to_string()))?;
    c.connect(
        "updateNoteFields",
        json!({"note":{"id":note,"fields":{"Back":"changed-in-Anki-V3"}}}),
    )?;
    let v3 = published(&sync(c, &repo, &[DECK], true)?, DECK)?;
    let state = audit(c, &repo)?;
    let current = stream(&state, &v3)?;
    c.check(
        "V3 актуальна, A и рабочие байты не изменены",
        v3["revision"] != v2["revision"]
            && v3["source_sequence"] == 4
            && current["revision"] == v3["revision"]
            && current["base_revision"].is_null()
            && fs::read(working.join("deck.json")).unwrap_or_default() == working_bytes,
        current.clone(),
    )?;
    c.check(
        "Незапрошенная в повторных операциях вторая колода не экспортировалась",
        stream(&state, &second)?["source_sequence"] == 1,
        state.clone(),
    )?;

    c.step("input: рестарт Anki и CLI сохраняет профиль и ревизию");
    stop_owned(c)?;
    // sync-input сам запускает остановленную подготовленную Anki.
    let restarted = published(&sync(c, &repo, &[DECK], true)?, DECK)?;
    c.check(
        "После рестарта сохранены profile ID, UUID и содержательная ревизия",
        restarted["identity"] == one["identity"]
            && restarted["revision"] == v3["revision"]
            && restarted["source_sequence"] == 5,
        restarted,
    )?;
    let after_restart = audit(c, &repo)?;
    c.check(
        "После рестарта все сохранённые потоки снова проверены владельцем",
        after_restart["streams"]
            .as_array()
            .is_some_and(|v| v.len() == 2),
        after_restart,
    )?;
    stop_owned(c)?;

    c.step("input: одноимённая колода другого профиля в другой базе");
    let original_base = c.base.clone();
    c.base = c.dir.join("second-anki-base");
    fx.base_patched(c)?;
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    create_fixture_data(c)?;
    let other = published(&sync(c, &repo, &[DECK], true)?, DECK)?;
    c.check(
        "Одинаковые имена профиля/колоды разных баз не смешивают потоки",
        other["identity"]["profile"] != one["identity"]["profile"] && other["source_sequence"] == 1,
        other.clone(),
    )?;
    let final_state = audit(c, &repo)?;
    c.check(
        "Три независимых потока и неизменный output",
        final_state["streams"]
            .as_array()
            .is_some_and(|v| v.len() == 3)
            && fs::read(working.join("deck.json")).unwrap_or_default() == working_bytes,
        final_state,
    )?;
    c.step("input: частичный пакет не скрывает отсутствующую колоду");
    let partial = c.anki_repo(
        &[
            "sync-input",
            "--repository",
            &repo.to_string_lossy(),
            "--deck",
            DECK,
            "--deck",
            SECOND,
            "--apply",
        ],
        &Policy::none(),
    )?;
    c.check(
        "Доступная колода опубликована, отсутствующая имеет отдельный отказ",
        partial.exit == 6
            && partial.state() == "partial"
            && published(partial.result(), DECK)?["source_sequence"] == 2
            && published(partial.result(), SECOND)?["reason"] == "deck_not_found",
        partial.json,
    )?;
    let after_partial = audit(c, &repo)?;
    c.check(
        "Частичный пакет сохранил прежние снимки другого профиля",
        stream(&after_partial, &second)?["source_sequence"] == 1
            && after_partial["streams"]
                .as_array()
                .is_some_and(|v| v.len() == 3),
        after_partial,
    )?;
    c.step("input: все подходящие корни из живой Anki");
    let all_plan = c.anki_repo(
        &[
            "sync-input",
            "--repository",
            &repo.to_string_lossy(),
            "--all",
        ],
        &Policy::none(),
    )?;
    let root_names = |result: &Value| -> Option<Vec<String>> {
        let mut names = result["decks"]
            .as_array()?
            .iter()
            .map(|item| item["name"].as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        names.sort();
        Some(names)
    };
    let expected_roots = vec!["Default".to_owned(), DECK.to_owned()];
    c.check(
        "--all планирует ровно Default и корень синтетической колоды",
        all_plan.exit == 0
            && all_plan.state() == "input_plan"
            && root_names(all_plan.result()).as_ref() == Some(&expected_roots),
        all_plan.json,
    )?;
    let all = c.anki_repo(
        &[
            "sync-input",
            "--repository",
            &repo.to_string_lossy(),
            "--all",
            "--apply",
        ],
        &Policy::none(),
    )?;
    c.check(
        "--all экспортирует именно живой план корней выбранного профиля",
        all.exit == 0
            && root_names(all.result()).as_ref() == Some(&expected_roots)
            && published(all.result(), DECK)?["source_sequence"] == 3,
        all.json.clone(),
    )?;
    let default = published(all.result(), "Default")?;
    let after_all = audit(c, &repo)?;
    c.check(
        "Default подтверждён отдельным потоком выбранного профиля",
        default["state"] == "published"
            && default["identity"]["profile"] == other["identity"]["profile"]
            && stream(&after_all, &default)?["source_sequence"] == 1,
        default,
    )?;
    stop_owned(c)?;
    c.base = original_base;
    Ok(())
}
