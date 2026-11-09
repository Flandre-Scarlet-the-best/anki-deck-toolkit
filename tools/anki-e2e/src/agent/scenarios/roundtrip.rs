//! Полный управляемый цикл через настоящий CLI внутри контейнера:
//! Anki V1 → input → prepare_first → правка B штатным `edit` → проверенный
//! импорт CrowdAnki с видимым диалогом → readback → правка в Anki V3 →
//! sync-input → refresh_unchanged. Затем отмена диалога, расхождение readback
//! при удалении заметки в JSON и изменение Anki после подготовки.
//!
//! Стенд не читает `decks/input`: подтверждения дают input-status и
//! exchange-status, а содержимое Anki — upstream AnkiConnect и файлы output.
use super::*;

const AUDIO: &str = "e2e-toolkit-tone.mp3";
const SECOND: &str = "E2E Roundtrip Second";

/// Синтетический MP3-кадр; байты известны до запуска Anki.
fn tone() -> Vec<u8> {
    let mut bytes = b"ID3\x03\x00\x00\x00\x00\x00\x00".to_vec();
    bytes.extend((0..4096u32).map(|i| (i * 7 % 256) as u8));
    bytes
}

fn repo_arg(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

fn cli(c: &mut Ctx, args: &[&str], policy: &Policy) -> Result<super::super::ctx::Cmd, Fail> {
    c.anki_repo(args, policy)
}

fn item(result: &Value) -> Value {
    result["decks"]
        .as_array()
        .and_then(|d| d.first())
        .cloned()
        .unwrap_or(Value::Null)
}

fn sync(c: &mut Ctx, repo: &Path, deck: &str) -> Result<Value, Fail> {
    let r = cli(
        c,
        &[
            "sync-input",
            "--repository",
            &repo_arg(repo),
            "--deck",
            deck,
            "--apply",
        ],
        &Policy::none(),
    )?;
    c.check(
        "sync-input --apply подтвердил живой экспорт",
        r.exit == 0 && r.state() == "succeeded",
        r.json.clone(),
    )?;
    Ok(item(r.result()))
}

fn status(c: &mut Ctx, repo: &Path, identity: &Value) -> Result<Value, Fail> {
    let r = cli(
        c,
        &["exchange-status", "--repository", &repo_arg(repo)],
        &Policy::none(),
    )?;
    c.check(
        "exchange-status согласован с input без записи",
        r.exit == 0 && r.state() == "succeeded",
        r.json.clone(),
    )?;
    r.result()["decks"]
        .as_array()
        .and_then(|d| d.iter().find(|s| s["identity"] == *identity))
        .cloned()
        .ok_or_else(|| Fail::failed("roundtrip_missing_stream", "Нет потока в exchange-status"))
}

fn audit(c: &mut Ctx, repo: &Path) -> Result<Value, Fail> {
    let r = cli(
        c,
        &["input-status", "--repository", &repo_arg(repo)],
        &Policy::none(),
    )?;
    c.check(
        "input-status перепроверил receipt, байты и exchange",
        r.exit == 0 && r.state() == "succeeded",
        r.json.clone(),
    )?;
    Ok(r.result().clone())
}

fn prepare(
    c: &mut Ctx,
    repo: &Path,
    output: &str,
    apply: bool,
) -> Result<super::super::ctx::Cmd, Fail> {
    let repo = repo_arg(repo);
    let mut args = vec!["prepare-output", "--repository", &repo, "--output", output];
    if apply {
        args.push("--apply");
    }
    cli(c, &args, &Policy::none())
}

fn publish(
    c: &mut Ctx,
    repo: &Path,
    output: &str,
    apply: bool,
    policy: &Policy,
) -> Result<super::super::ctx::Cmd, Fail> {
    let repo = repo_arg(repo);
    let mut args = vec![
        "publish-output",
        "--repository",
        &repo,
        "--output",
        output,
        "--wait-seconds",
        "150",
    ];
    if apply {
        args.push("--apply");
    }
    cli(c, &args, policy)
}

fn read_json(path: &Path) -> Result<Value, Fail> {
    serde_json::from_slice(&fs::read(path).map_err(|e| Fail::error("io", e.to_string()))?)
        .map_err(|e| Fail::error("json", e.to_string()))
}

fn guid_of(deck: &Value, front: &str) -> Result<String, Fail> {
    export_notes(deck)
        .iter()
        .find(|n| n["fields"][0] == front)
        .and_then(|n| n["guid"].as_str().map(str::to_owned))
        .ok_or_else(|| Fail::failed("roundtrip_note_missing", format!("Нет заметки {front}")))
}

fn edit(c: &mut Ctx, dir: &Path, guid: &str, expect: &str, value: &str) -> Outcome {
    let r = c.anki_repo_raw(
        &[
            "--json",
            "edit",
            &dir.to_string_lossy(),
            "--guid",
            guid,
            "--field",
            "Back",
            "--set",
            value,
            "--expect",
            expect,
            "--apply",
        ],
        &Policy::none(),
    )?;
    c.check(
        "Штатный anki-repo edit изменил поле B с сохранением GUID и модели",
        r.exit == 0,
        r.json.clone(),
    )
}

fn anki_back(c: &mut Ctx, deck: &str, front: &str) -> Result<String, Fail> {
    let ids = c.connect("findNotes", json!({"query":format!("\"deck:{deck}\"")}))?;
    let info = c.connect("notesInfo", json!({"notes":ids}))?;
    info.as_array()
        .and_then(|n| n.iter().find(|n| n["fields"]["Front"]["value"] == front))
        .and_then(|n| n["fields"]["Back"]["value"].as_str().map(str::to_owned))
        .ok_or_else(|| {
            Fail::failed(
                "roundtrip_anki_note_missing",
                format!("Нет заметки {front} в Anki"),
            )
        })
}

fn note_id(c: &mut Ctx, deck: &str, front: &str) -> Result<Value, Fail> {
    let ids = c.connect(
        "findNotes",
        json!({"query":format!("\"deck:{deck}\" \"Front:{front}\"")}),
    )?;
    ids.as_array()
        .and_then(|v| v.first())
        .cloned()
        .ok_or_else(|| {
            Fail::failed(
                "roundtrip_anki_note_missing",
                format!("Нет заметки {front}"),
            )
        })
}

pub(super) fn run(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    fx.base_patched(c)?;
    let repo = c.dir.join("roundtrip-repository");
    fs::create_dir_all(repo.join(".git")).map_err(|e| Fail::error("io", e.to_string()))?;
    c.step("roundtrip: синтетический профиль с PNG и MP3");
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    create_fixture_data(c)?;
    let data = base64::engine::general_purpose::STANDARD.encode(tone());
    let stored = c.connect("storeMediaFile", json!({"filename":AUDIO,"data":data}))?;
    c.check("MP3 сохранён штатным API", stored == AUDIO, stored.clone())?;
    c.connect(
        "addNote",
        json!({"note":{"deckName":DECK,"modelName":MODEL,
        "fields":{"Front":"audio note","Back":format!("[sound:{AUDIO}]")},"tags":["e2e","audio"]}}),
    )?;
    c.connect("createDeck", json!({"deck":SECOND}))?;
    for (front, back) in [("second keep", "kept-back"), ("second drop", "drop-back")] {
        c.connect(
            "addNote",
            json!({"note":{"deckName":SECOND,"modelName":MODEL,"fields":{"Front":front,"Back":back},"tags":["e2e"]}}),
        )?;
    }

    c.step("roundtrip: V1 в input и prepare_first");
    let v1 = sync(c, &repo, DECK)?;
    let identity = v1["identity"].clone();
    let stream = status(c, &repo, &identity)?;
    let output_name = stream["output_directory"]
        .as_str()
        .and_then(|p| p.strip_prefix("decks/output/"))
        .unwrap_or_default()
        .to_owned();
    c.check(
        "До подготовки решение exchange — prepare_first, output отсутствует",
        stream["action"] == "prepare_first" && !repo.join("decks/output").exists(),
        stream.clone(),
    )?;
    let plan = prepare(c, &repo, &output_name, false)?;
    c.check(
        "prepare-output без --apply — план без записи",
        plan.exit == 0
            && plan.state() == "prepare_plan"
            && item(plan.result())["action"] == "prepare_first"
            && !repo.join("decks/output").exists(),
        plan.json.clone(),
    )?;
    let applied = prepare(c, &repo, &output_name, true)?;
    c.check(
        "prepare-output --apply создал рабочую копию и базу A == C",
        applied.exit == 0
            && item(applied.result())["state"] == "prepared"
            && item(applied.result())["base"]["revision"] == v1["revision"],
        applied.json.clone(),
    )?;
    let working = repo.join("decks/output").join(&output_name);
    let png =
        crate::util::sha256_hex(&fs::read(working.join("media").join(MEDIA)).unwrap_or_default());
    let mp3 =
        crate::util::sha256_hex(&fs::read(working.join("media").join(AUDIO)).unwrap_or_default());
    c.check(
        "Рабочая копия содержит побайтно исходные PNG и MP3",
        png == fixtures::pixel_sha256() && mp3 == crate::util::sha256_hex(&tone()),
        json!({"png":png,"mp3":mp3}),
    )?;
    let again = prepare(c, &repo, &output_name, true)?;
    c.check(
        "Повторная подготовка идемпотентна: keep",
        again.exit == 0 && item(again.result())["state"] == "kept",
        again.json.clone(),
    )?;

    c.step("roundtrip: агентская правка B и план публикации");
    let deck = read_json(&working.join("deck.json"))?;
    let guid = guid_of(&deck, NOTES[0].front)?;
    edit(c, &working, &guid, NOTES[0].back, "agent V2")?;
    let plan = publish(c, &repo, &output_name, false, &Policy::none())?;
    c.check(
        "publish-output без --apply показывает разрешение и ожидаемое действие",
        plan.exit == 0
            && plan.state() == "publish_plan"
            && item(plan.result())["state"] == "planned"
            && item(plan.result())["comparison"]["disposition"] == "working_only",
        plan.json.clone(),
    )?;

    c.step("roundtrip: проверенный импорт V2 с подтверждением диалога");
    let accept = Policy::none().click("crowdanki_import_dialog", "OK", 1);
    let published = publish(c, &repo, &output_name, true, &accept)?;
    let outcome = item(published.result());
    c.check(
        "Импорт подтверждён настоящим readback: published",
        published.exit == 0
            && outcome["state"] == "published"
            && outcome["readback_matches"] == true,
        published.json.clone(),
    )?;
    let step = c.ui.step.clone();
    c.check(
        "Штатный диалог импорта CrowdAnki показан и подтверждён ровно один раз",
        c.ui.seen_in_step("crowdanki_import_dialog", &step) == 1,
        json!(null),
    )?;
    let job_id = outcome["job_id"].as_str().unwrap_or_default().to_owned();
    let job = cli(c, &["job", &job_id], &Policy::none())?;
    c.check(
        "Job импорта завершён succeeded в живом API",
        job.state() == "succeeded" && job.result()["job"]["action"] == "import",
        job.json.clone(),
    )?;
    let back = anki_back(c, DECK, NOTES[0].front)?;
    c.check("AnkiConnect видит поле V2", back == "agent V2", json!(back))?;
    let after = status(c, &repo, &identity)?;
    c.check(
        "exchange: published B, новый C и база A == readback",
        after["published"]["working_revision"] == outcome["working_revision"]
            && after["input"]["revision"] == outcome["working_revision"]
            && after["base"]["revision"] == outcome["working_revision"]
            && after["action"] == "keep"
            && after["pending"].as_array().is_some_and(Vec::is_empty),
        after.clone(),
    )?;
    let input = audit(c, &repo)?;
    let durable = input["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["identity"] == identity))
        .cloned()
        .unwrap_or(Value::Null);
    c.check(
        "Подтверждённый input содержит те же физические PNG и MP3",
        durable["inventory"][format!("media/{MEDIA}")] == fixtures::pixel_sha256()
            && durable["inventory"][format!("media/{AUDIO}")] == crate::util::sha256_hex(&tone())
            && durable["revision"] == outcome["working_revision"],
        durable.clone(),
    )?;

    c.step("roundtrip: правка в Anki V3, sync-input и refresh_unchanged");
    let second_note = note_id(c, DECK, NOTES[1].front)?;
    c.connect(
        "updateNoteFields",
        json!({"note":{"id":second_note,"fields":{"Back":"changed-in-Anki-V3"}}}),
    )?;
    let v3 = sync(c, &repo, DECK)?;
    let planned = prepare(c, &repo, &output_name, false)?;
    c.check(
        "После V3 решение — refresh_unchanged",
        item(planned.result())["action"] == "refresh_unchanged",
        planned.json.clone(),
    )?;
    let refreshed = prepare(c, &repo, &output_name, true)?;
    let deck = read_json(&working.join("deck.json"))?;
    c.check(
        "refresh_unchanged обновил B до V3 с сохранением V2 и media",
        item(refreshed.result())["state"] == "prepared"
            && item(refreshed.result())["base"]["revision"] == v3["revision"]
            && export_notes(&deck)
                .iter()
                .any(|n| n["fields"][1] == "changed-in-Anki-V3")
            && export_notes(&deck)
                .iter()
                .any(|n| n["fields"][1] == "agent V2")
            && crate::util::sha256_hex(
                &fs::read(working.join("media").join(AUDIO)).unwrap_or_default(),
            ) == crate::util::sha256_hex(&tone()),
        refreshed.json.clone(),
    )?;

    c.step("roundtrip: отмена диалога не продвигает published");
    edit(c, &working, &guid, "agent V2", "agent V4 cancelled")?;
    let cancel = Policy::none().click("crowdanki_import_dialog", "Cancel", 1);
    let cancelled = publish(c, &repo, &output_name, true, &cancel)?;
    let outcome = item(cancelled.result());
    c.check(
        "Отмена — import_not_applied/import_cancelled, B сохранён",
        cancelled.exit == 6
            && outcome["state"] == "import_not_applied"
            && outcome["reason"] == "import_cancelled"
            && outcome["published"] == false,
        cancelled.json.clone(),
    )?;
    let back = anki_back(c, DECK, NOTES[0].front)?;
    let deck = read_json(&working.join("deck.json"))?;
    c.check(
        "Anki не изменилась, рабочая копия сохранила правку",
        back == "agent V2"
            && export_notes(&deck)
                .iter()
                .any(|n| n["fields"][1] == "agent V4 cancelled"),
        json!({"anki":back}),
    )?;

    c.step("roundtrip: изменение Anki после подготовки останавливает импорт");
    c.connect(
        "updateNoteFields",
        json!({"note":{"id":second_note,"fields":{"Back":"changed-in-Anki-after-prepare"}}}),
    )?;
    let stale = publish(c, &repo, &output_name, true, &Policy::none())?;
    let outcome = item(stale.result());
    let step = c.ui.step.clone();
    c.check(
        "Свежий экспорт выявил изменение Anki: импорт не отправлен",
        stale.exit == 6
            && outcome["state"] == "blocked"
            && outcome["reason"] == "anki_changed_since_input"
            && c.ui.seen_in_step("crowdanki_import_dialog", &step) == 0,
        stale.json.clone(),
    )?;
    let back = anki_back(c, DECK, NOTES[1].front)?;
    let blocked = prepare(c, &repo, &output_name, true)?;
    c.check(
        "Изменения пользователя в Anki сохранены; обе стороны изменены → blocked",
        back == "changed-in-Anki-after-prepare"
            && item(blocked.result())["state"] == "blocked"
            && blocked.exit == 6,
        blocked.json.clone(),
    )?;

    c.step("roundtrip: удаление заметки в JSON не удаляет её из Anki");
    let second = sync(c, &repo, SECOND)?;
    let second_stream = status(c, &repo, &second["identity"])?;
    let second_name = second_stream["output_directory"]
        .as_str()
        .and_then(|p| p.strip_prefix("decks/output/"))
        .unwrap_or_default()
        .to_owned();
    let prepared = prepare(c, &repo, &second_name, true)?;
    c.check(
        "Вторая колода подготовлена независимо",
        item(prepared.result())["state"] == "prepared",
        prepared.json.clone(),
    )?;
    let second_dir = repo.join("decks/output").join(&second_name);
    let keep = guid_of(&read_json(&second_dir.join("deck.json"))?, "second keep")?;
    // Сначала штатная правка канонического экспорта, затем удаление заметки в JSON.
    edit(c, &second_dir, &keep, "kept-back", "second V2")?;
    let mut deck = read_json(&second_dir.join("deck.json"))?;
    let notes = deck["notes"]
        .as_array_mut()
        .ok_or_else(|| Fail::failed("roundtrip_notes_missing", "Нет списка notes"))?;
    let before = notes.len();
    notes.retain(|n| n["fields"][0] != "second drop");
    if before - notes.len() != 1 {
        return Err(Fail::failed(
            "roundtrip_note_missing",
            "Заметка second drop не удалена ровно один раз",
        ));
    }
    fs::write(
        second_dir.join("deck.json"),
        serde_json::to_vec_pretty(&deck).expect("json"),
    )
    .map_err(|e| Fail::error("io", e.to_string()))?;
    let mismatch = publish(c, &repo, &second_name, true, &accept)?;
    let outcome = item(mismatch.result());
    c.check(
        "Импорт применил правку, но readback содержит невыдаленную заметку: не published",
        mismatch.exit == 6
            && outcome["state"] == "readback_mismatch"
            && outcome["published"] == false
            && outcome["import_state"] == "succeeded",
        mismatch.json.clone(),
    )?;
    let kept = anki_back(c, SECOND, "second drop")?;
    let after = status(c, &repo, &second["identity"])?;
    c.check(
        "Заметка осталась в Anki; published не установлен, readback сохранён как C",
        kept == "drop-back" && after["published"].is_null() && after["action"] == "blocked",
        after.clone(),
    )?;
    audit(c, &repo)?;
    stop_owned(c)
}
