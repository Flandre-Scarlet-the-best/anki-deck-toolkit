//! Контракт CLI команды `review`: отбор, страницы, компактность batch.
//!
//! Каждая фикстура строится здесь же, в собственном временном каталоге: тест не
//! знает ни layout репозитория, ни содержимого `decks/`, ни уровней JLPT, ни
//! конкретных note models и media. Поэтому состав колод может меняться или
//! временно отсутствовать, не ломая default test suite и CI.
//!
//! Проверяются три контракта `review`: область выбора (`--all`, `--guid`,
//! `--field`/`--value`, `--qa-code`, `--deck`), ограниченность страницы
//! (`--offset`/`--limit`) и адресуемость batch'а (`excluded_unaddressable`).
//! Групповые правила добавляют четвёртый: в batch попадают все участники группы,
//! а не только заметка, к которой приписано замечание.

use crate::common;

use common::{
    TempDir, base_export, collect_notes, export_with, mixed_export, parse_json,
    raw_note_model_fields, run_cli,
};
use serde_json::{Value, json};

fn review_json(export: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["--json", "review", export];
    args.extend_from_slice(extra);
    let (code, stdout, _) = run_cli(&args);
    (code, parse_json(&stdout))
}

/// Экспорт с пятью заметками, у которых поле `Толкование` равно «значение».
///
/// Поле называется явно через `--field`/`--value`: «слово» — понятие конкретной
/// колоды, а не формата CrowdAnki, поэтому критерий не может его предполагать.
fn selection_export() -> Value {
    export_with(|value| {
        value["notes"][0]["fields"] = json!(["слово-1", "значение", ""]);
        value["notes"][1]["fields"] = json!(["слово-2", "значение", ""]);
        for index in 0..3 {
            value["notes"].as_array_mut().expect("notes").push(json!({
                "__type__": "Note",
                "guid": format!("bulk-{index}"),
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": [format!("слово-{}", index + 3), "значение", ""],
            }));
        }
    })
}

/// Экспорт с вложенными колодами: Root, Root::Child, Root::Child::Leaf.
///
/// Имена узлов включают полный путь, как в реальных CrowdAnki-экспортах, и
/// `--deck` принимает только точный путь узла: короткое имя `Child` не
/// разрешается.
fn nested_export() -> Value {
    export_with(|value| {
        value["name"] = json!("Root");
        value["notes"] = json!([
            {"guid": "guid-root", "note_model_uuid": "model-1", "tags": [],
             "fields": ["根", "корень", ""]}
        ]);
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Root::Child",
                "crowdanki_uuid": "deck-child",
                "deck_config_uuid": "cfg-1",
                "children": [
                    {
                        "__type__": "Deck",
                        "name": "Root::Child::Leaf",
                        "crowdanki_uuid": "deck-leaf",
                        "deck_config_uuid": "cfg-1",
                        "children": [],
                        "notes": [
                            {"guid": "guid-leaf", "note_model_uuid": "model-1", "tags": [],
                             "fields": ["深い", "глубокий", ""]}
                        ],
                        "media_files": [],
                        "note_models": [],
                        "deck_configurations": []
                    }
                ],
                "notes": [
                    {"guid": "guid-child", "note_model_uuid": "model-1", "tags": [],
                     "fields": ["子", "ребёнок", ""]}
                ],
                "media_files": [],
                "note_models": [],
                "deck_configurations": []
            }
        ]);
    })
}

#[test]
fn review_defaults_to_one_bounded_page() {
    let temp = TempDir::new("review-page");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &[
            "--field",
            "Толкование",
            "--value",
            "значение",
            "--limit",
            "2",
        ],
    );

    assert_eq!(code, 0);
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["command"], "review");
    let result = &parsed["result"];
    assert_eq!(result["notes_total"], 5);
    assert_eq!(result["total_selected"], 5);
    assert_eq!(result["returned"], 2);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["next_offset"], 2);
    assert_eq!(result["offset"], 0);
    assert_eq!(result["limit"], 2);
    assert_eq!(result["items"].as_array().expect("items").len(), 2);
}

#[test]
fn pagination_covers_every_selected_note_exactly_once() {
    let temp = TempDir::new("review-pagination");
    temp.write_export(&selection_export());
    let export = temp.path().to_string_lossy().to_string();

    let mut guids = Vec::new();
    let mut offset = 0;
    loop {
        let (code, parsed) = review_json(
            &export,
            &[
                "--field",
                "Толкование",
                "--value",
                "значение",
                "--limit",
                "2",
                "--offset",
                &offset.to_string(),
            ],
        );
        assert_eq!(code, 0);
        let result = &parsed["result"];
        for item in result["items"].as_array().expect("items") {
            guids.push(item["guid"].as_str().expect("guid").to_string());
        }
        match result["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }

    guids.sort();
    assert_eq!(
        guids,
        vec!["bulk-0", "bulk-1", "bulk-2", "guid-1", "guid-2"]
    );
}

#[test]
fn offset_past_the_end_is_a_valid_empty_page() {
    let temp = TempDir::new("review-offset-past-end");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--all", "--limit", "2", "--offset", "1000"],
    );

    assert_eq!(code, 0, "страница за концом — не ошибка, а конец пагинации");
    let result = &parsed["result"];
    assert_eq!(result["returned"], 0);
    assert_eq!(result["truncated"], false);
    assert_eq!(result["next_offset"], Value::Null);
    assert_eq!(result["total_selected"], 5);
}

#[test]
fn review_batch_is_bounded_and_carries_qa_context() {
    let temp = TempDir::new("review-compact");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "empty_field_value", "--limit", "2"],
    );

    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["selection"]["kind"], "qa_code");
    assert_eq!(result["selection"]["qa_code"], "empty_field_value");
    assert_eq!(result["total_selected"], 5);
    assert_eq!(result["returned"], 2, "страница меньше выбранного");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["next_offset"], 2);

    let item = &result["items"][0];
    assert_eq!(item["note_index"], 0);
    assert_eq!(item["qa_findings"].as_array().expect("findings").len(), 1);
    assert_eq!(item["qa_findings"][0]["code"], "empty_field_value");
    // Поле берётся из общей фикстуры `common::base_export`: пустое значение
    // там стоит на третьем по `ord` поле.
    assert_eq!(item["qa_findings"][0]["field"], "Пример");
    assert_eq!(item["qa_findings_truncated"], false);
}

#[test]
fn item_findings_follow_registry_order() {
    let temp = TempDir::new("review-findings-order");
    temp.write_export(&export_with(|value| {
        // Значение из одних пробелов даёт и leading, и trailing замечание, а
        // одинаковая заметка рядом создаёт ещё и группу дубликатов содержимого.
        value["notes"][0]["fields"] = json!(["слово", "  ", ""]);
        value["notes"][1]["fields"] = json!(["слово", "  ", ""]);
    }));
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--field", "Заголовок", "--value", "слово", "--limit", "10"],
    );
    assert_eq!(code, 0);

    let items = parsed["result"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "обе заметки отобраны по значению поля");

    // Порядок замечаний — порядок реестра правил, а не порядок обнаружения.
    let codes: Vec<&str> = items[0]["qa_findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            "empty_field_value",
            "leading_whitespace",
            "trailing_whitespace",
            "duplicate_note_content"
        ]
    );
    assert_eq!(items[0]["qa_findings_truncated"], false);

    // Групповое замечание называет остальных участников, а сами участники знают,
    // что попали в batch как члены группы, а не по своему критерию.
    let group = items[0]["qa_findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|finding| finding["code"] == "duplicate_note_content")
        .expect("замечание группы");
    assert_eq!(group["group_size"], 2);
    assert_eq!(group["related"][0]["guid"], "guid-2");
    assert_eq!(items[1]["group_membership"][0]["owner_guid"], "guid-1");
}

#[test]
fn guid_lookup_mirrors_find_error_contract() {
    let temp = TempDir::new("review-guid");
    temp.write_export(&selection_export());
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = review_json(&export, &["--guid", "guid-2"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["returned"], 1);
    assert_eq!(parsed["result"]["items"][0]["guid"], "guid-2");
    assert_eq!(parsed["result"]["next_offset"], Value::Null);

    // Код ошибки совпадает с `find`: generic `not_found`, exit 4.
    // `edit` для той же ситуации использует `note_not_found` с тем же exit.
    let (code, parsed) = review_json(&export, &["--guid", "нет-такого"]);
    assert_eq!(code, 4);
    assert_eq!(parsed["error"]["code"], "not_found");
}

#[test]
fn deck_scope_limits_selection_and_unknown_deck_is_rejected() {
    let temp = TempDir::new("review-scope");
    temp.write_export(&nested_export());
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Root::Child"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 2, "Child и его Leaf");

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Root::нет"]);
    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_deck");

    // Короткое имя узла не является путём колоды.
    let (code, parsed) = review_json(&export, &["--all", "--deck", "Child"]);
    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_deck");
}

#[test]
fn empty_selection_is_not_an_error() {
    let temp = TempDir::new("review-empty");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--field", "Толкование", "--value", "нет-такого-значения"],
    );

    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 0);
    assert_eq!(parsed["result"]["returned"], 0);
    assert!(
        parsed["result"]["items"]
            .as_array()
            .expect("items")
            .is_empty()
    );
}

#[test]
fn unknown_qa_code_is_rejected() {
    let temp = TempDir::new("review-unknown-code");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(&temp.path().to_string_lossy(), &["--qa-code", "нет_такого"]);

    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
}

/// Флаг `--word` удалён: заметку называют полем модели, а не «словом».
///
/// «Слово» — понятие конкретной колоды, у которой есть поле с таким именем, а не
/// свойство формата CrowdAnki. Единственный переносимый критерий по значению —
/// явные `--field`/`--value`, поэтому старый флаг обязан быть отвергнут как
/// неизвестный аргумент, а не молча проигнорирован.
#[test]
fn word_flag_is_no_longer_accepted() {
    let temp = TempDir::new("review-word-flag");
    temp.write_export(&base_export());

    let (code, stdout, stderr) =
        run_cli(&["review", &temp.path().to_string_lossy(), "--word", "偶然"]);

    assert_eq!(code, 2, "неизвестный флаг — usage-ошибка: {stderr}");
    assert!(
        stderr.contains("--word"),
        "диагностика обязана назвать отвергнутый флаг\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "usage-ошибка не выдаёт result:\n{stdout}"
    );
}

#[test]
fn human_mode_prints_selected_and_next_page() {
    let temp = TempDir::new("review-human");
    temp.write_export(&selection_export());
    let (code, stdout, stderr) = run_cli(&[
        "review",
        &temp.path().to_string_lossy(),
        "--all",
        "--limit",
        "1",
    ]);

    assert_eq!(code, 0);
    assert!(stderr.is_empty());
    for needle in [
        "Критерий:",
        "Следующая страница: --offset 1",
        "[#0]",
        "QA (1):",
    ] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

/// Human и `--json` описывают одну и ту же страницу одними и теми же числами.
///
/// Batch читает внешний агент, а решение по нему принимает человек: если
/// количества и адрес следующей страницы разъедутся между формами вывода, сверить
/// их будет нечем.
#[test]
fn human_and_json_modes_describe_the_same_page() {
    let temp = TempDir::new("review-human-json");
    temp.write_export(&selection_export());
    let export = temp.path().to_string_lossy().to_string();
    let criteria = [
        "--field",
        "Толкование",
        "--value",
        "значение",
        "--limit",
        "2",
    ];

    let (code, parsed) = review_json(&export, &criteria);
    assert_eq!(code, 0);
    let result = &parsed["result"];
    let total = result["total_selected"].as_u64().expect("total_selected");
    let returned = result["returned"].as_u64().expect("returned");
    let next = result["next_offset"].as_u64().expect("next_offset");

    let mut args = vec!["review", export.as_str()];
    args.extend_from_slice(&criteria);
    let (code, stdout, stderr) = run_cli(&args);
    assert_eq!(code, 0, "stderr: {stderr}");

    for needle in [
        format!("Выбрано заметок: {total} из "),
        format!("возвращено {returned}"),
        format!("Следующая страница: --offset {next}"),
    ] {
        assert!(
            stdout.contains(&needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

/// Экспорт, воспроизводящий форму реальной словарной колоды: у модели больше
/// полей, чем значимых значений, а пустое значение поля — легитимное состояние
/// заметки, поэтому таких заметок несколько.
///
/// Значения объявлены здесь же, поэтому ожидания теста пересчитываются из этого
/// fixture, а не берутся из содержимого `decks/`.
fn empty_field_export() -> Value {
    export_with(|value| {
        value["note_models"][0]["flds"] = json!([
            {"name": "Заголовок", "ord": 0},
            {"name": "Вид", "ord": 1},
            {"name": "Толкование", "ord": 2},
            {"name": "Дополнение", "ord": 3}
        ]);
        value["notes"][0]["fields"] = json!(["偶然", "существительное", "случайность", ""]);
        // Два пустых поля в одной заметке: замечания два, item один.
        value["notes"][1]["fields"] = json!(["必然", "", "", "пример"]);

        let notes = value["notes"].as_array_mut().expect("notes");
        notes.push(json!({
            "__type__": "Note",
            "guid": "guid-3",
            "note_model_uuid": "model-1",
            "tags": [],
            "fields": ["слово-1", "наречие", "", "пример"],
        }));
        notes.push(json!({
            "__type__": "Note",
            "guid": "guid-4",
            "note_model_uuid": "model-1",
            "tags": [],
            "fields": ["слово-2", "наречие", "значение", "пример"],
        }));
    })
}

#[test]
fn batch_is_bounded_per_note_and_carries_model_fields() {
    let fixture = empty_field_export();
    let temp = TempDir::new("review-bounded");
    temp.write_export(&fixture);

    // Ожидаемое число заметок с пустым полем пересчитывается из самого fixture.
    // `review` выбирает заметки, а не замечания: заметка с двумя пустыми полями
    // даёт два замечания и один item. Поэтому ожидание считается по заметкам.
    let mut notes = Vec::new();
    collect_notes(&fixture, &mut notes);
    let expected_selected = count_notes_with_empty_field(&fixture);

    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "empty_field_value", "--limit", "1"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(result["notes_total"], notes.len());
    assert_eq!(result["total_selected"], expected_selected);
    assert!(expected_selected < notes.len(), "не все заметки дефектны");
    assert_eq!(result["excluded_unaddressable"], 0);
    assert_eq!(result["returned"], 1);
    assert_eq!(result["truncated"], true, "страница ограничена одним item");

    let item = &result["items"][0];
    let fields = item["fields"].as_object().expect("fields");
    let note_index =
        usize::try_from(item["note_index"].as_u64().expect("note_index")).expect("usize");
    let model_fields = model_field_names(&fixture, notes[note_index]);
    assert_eq!(
        fields.len(),
        model_fields.len(),
        "batch несёт ровно именованные поля модели этой заметки"
    );
    assert!(
        item["qa_findings"]
            .as_array()
            .expect("qa_findings")
            .iter()
            .any(|finding| finding["code"] == "empty_field_value"),
        "item обязан нести замечание, по которому выбран"
    );
    // Значение поля берётся из fixture по адресу, разрешённому через `ord`
    // модели, а не сравнивается с историческим содержимым колоды.
    let raw_fields = notes[note_index]["fields"]
        .as_array()
        .expect("fields заметки");
    let empty: Vec<&String> = model_fields
        .iter()
        .filter(|(_, ord)| {
            raw_fields
                .get(*ord)
                .and_then(Value::as_str)
                .is_some_and(str::is_empty)
        })
        .map(|(name, _)| name)
        .collect();
    assert!(
        !empty.is_empty(),
        "выбранная заметка действительно содержит пустое поле"
    );
    for name in empty {
        assert_eq!(fields[name], "", "пустое поле перенесено в batch");
    }
}

/// Сколько заметок экспорта имеют хотя бы одно пустое значение поля.
fn count_notes_with_empty_field(raw: &Value) -> usize {
    let mut notes = Vec::new();
    collect_notes(raw, &mut notes);
    notes
        .iter()
        .filter(|note| {
            model_field_ords(raw, note).iter().any(|ord| {
                note["fields"]
                    .as_array()
                    .and_then(|fields| fields.get(*ord))
                    .and_then(Value::as_str)
                    .is_some_and(str::is_empty)
            })
        })
        .count()
}

/// Имена и позиции значений полей заметки в порядке модели.
fn model_field_names(raw: &Value, note: &Value) -> Vec<(String, usize)> {
    raw_note_model_fields(raw, note)
        .into_iter()
        .filter_map(|(name, ord)| Some((name, usize::try_from(ord).ok()?)))
        .collect()
}

/// `ord` полей модели заметки (в порядке `flds`).
fn model_field_ords(raw: &Value, note: &Value) -> Vec<usize> {
    model_field_names(raw, note)
        .into_iter()
        .map(|(_, ord)| ord)
        .collect()
}

/// Экспорт с одной группой из трёх одинаковых заметок и одной уникальной.
fn content_group_export() -> Value {
    export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 3..6 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-{index}"));
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    })
}

/// Экспорт с двумя группами по три заметки: группы различает содержимое.
///
/// Первая группа — три копии базовой заметки `guid-1`, вторая — три копии
/// базовой заметки `guid-2`. Модель у обеих групп одна, поэтому именно
/// содержимое, а не модель, обязано разделять группы.
fn duplicate_groups_export() -> Value {
    export_with(|value| {
        let existing = value["notes"].as_array().expect("notes");
        let first = existing[0].clone();
        let second = existing[1].clone();

        let notes = value["notes"].as_array_mut().expect("notes");
        for (position, template) in [(2, &first), (3, &first), (4, &second), (5, &second)] {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-{}", position + 1));
            notes.push(copy);
        }
    })
}

#[test]
fn grouped_qa_code_selection_brings_every_participant() {
    let temp = TempDir::new("review-group-content");
    temp.write_export(&content_group_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "duplicate_note_content", "--limit", "10"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(
        result["total_selected"], 4,
        "в batch попадают все участники группы, а не только владелец замечания"
    );

    let owner = &result["items"][0];
    let finding = owner["qa_findings"]
        .as_array()
        .expect("qa_findings")
        .iter()
        .find(|finding| finding["code"] == "duplicate_note_content")
        .expect("замечание группы у владельца");
    assert_eq!(finding["group_size"], 4);
    assert_eq!(finding["related_truncated"], false);

    // Владелец называет остальных участников с их guid.
    let related: Vec<&Value> = finding["related"]
        .as_array()
        .expect("related")
        .iter()
        .map(|related| &related["guid"])
        .collect();
    assert_eq!(
        related,
        vec![&json!("guid-3"), &json!("guid-4"), &json!("guid-5")],
        "все участники группы, кроме самого владельца"
    );

    // Остальные участники понимают, почему они в batch'е: каждая группа, в
    // которой заметка — участник, названа вместе со своим владельцем.
    for item in &result["items"].as_array().expect("items")[1..] {
        let membership = item["group_membership"]
            .as_array()
            .expect("group_membership");
        assert!(!membership.is_empty(), "участник обязан знать свою группу");
        assert!(
            membership
                .iter()
                .any(|entry| entry["code"] == "duplicate_note_content"),
            "участник выбран по этому коду"
        );
        for entry in membership {
            assert_eq!(entry["owner_note_index"], 0);
            assert_eq!(entry["owner_guid"], "guid-1");
            assert_eq!(entry["group_size"], 4);
        }
    }
}

/// Групповой отбор расширяет batch до всех участников каждой группы.
///
/// Групп в экспорте две, и они не смешиваются: владелец каждой называет ровно
/// своих участников, а каждый участник ссылается на своего владельца. На одной
/// группе такое поведение неотличимо от «в batch попала первая заметка группы».
#[test]
fn grouped_duplicate_selection_brings_every_group_participant() {
    let temp = TempDir::new("review-group-duplicates");
    temp.write_export(&duplicate_groups_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "duplicate_note_content", "--limit", "10"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(
        result["total_selected"], 6,
        "участники обеих групп, а не только по одному владельцу на группу"
    );

    let items = result["items"].as_array().expect("items");
    assert_eq!(
        items.len(),
        6,
        "каждый участник — отдельный item с полями своей модели"
    );
    let guids: Vec<&str> = items
        .iter()
        .map(|item| item["guid"].as_str().expect("guid"))
        .collect();
    assert_eq!(
        guids,
        vec!["guid-1", "guid-2", "guid-3", "guid-4", "guid-5", "guid-6"],
        "порядок items — порядок заметок в экспорте"
    );

    let owner_of = |item: &Value, code: &str| -> Value {
        item["qa_findings"]
            .as_array()
            .expect("qa_findings")
            .iter()
            .find(|finding| finding["code"] == code)
            .unwrap_or_else(|| panic!("у владельца должно быть замечание {code}"))
            .clone()
    };

    for (position, expected_related) in
        [(0, vec!["guid-3", "guid-4"]), (1, vec!["guid-5", "guid-6"])]
    {
        let item = &items[position];
        assert_eq!(
            item["fields"].as_object().expect("fields").len(),
            3,
            "item несёт именованные поля модели участника"
        );
        let finding = owner_of(item, "duplicate_note_content");
        assert_eq!(finding["group_size"], 3);
        assert_eq!(finding["related_truncated"], false);
        let related: Vec<&str> = finding["related"]
            .as_array()
            .expect("related")
            .iter()
            .map(|related| related["guid"].as_str().expect("guid"))
            .collect();
        assert_eq!(
            related, expected_related,
            "владелец называет участников только своей группы"
        );
    }

    // Каждый участник указывает на владельца своей группы.
    for (position, owner_guid) in [(2, "guid-1"), (3, "guid-1"), (4, "guid-2"), (5, "guid-2")] {
        let membership = items[position]["group_membership"]
            .as_array()
            .expect("group_membership");
        assert_eq!(membership.len(), 1, "заметка входит ровно в одну группу");
        assert_eq!(membership[0]["code"], "duplicate_note_content");
        assert_eq!(membership[0]["owner_guid"], owner_guid);
        assert_eq!(membership[0]["group_size"], 3);
    }
}

#[test]
fn unaddressable_notes_never_reach_the_batch() {
    let temp = TempDir::new("review-unaddressable");
    temp.write_export(&export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("notes");
        // Заметка без guid: её нельзя назвать предложением.
        let mut no_guid = notes[0].clone();
        no_guid["guid"] = json!(null);
        no_guid["fields"] = json!(["дубль", "значение", ""]);
        notes.push(no_guid);
        // Заметка с повторяющимся guid: адресация неоднозначна.
        let mut duplicate_guid = notes[0].clone();
        duplicate_guid["fields"] = json!(["ещё один", "значение", ""]);
        notes.push(duplicate_guid);
    }));

    let (code, parsed) = review_json(&temp.path().to_string_lossy(), &["--all", "--limit", "10"]);
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(
        result["notes_total"], 4,
        "неадресуемые заметки остаются частью экспорта"
    );
    assert_eq!(
        result["excluded_unaddressable"], 3,
        "исключены заметка без guid и оба вхождения повторённого guid: \
         адрес неоднозначен у каждого из них"
    );
    assert_eq!(result["total_selected"], 1);
    assert_eq!(
        result["items"][0]["guid"], "guid-2",
        "в batch остаётся единственная однозначно адресуемая заметка"
    );

    // То же для критерия по полю: исключение не зависит от вида критерия.
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &[
            "--field",
            "Толкование",
            "--value",
            "значение",
            "--limit",
            "10",
        ],
    );
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["excluded_unaddressable"], 2);
    assert_eq!(
        result["total_selected"], 0,
        "обе подходящие заметки неадресуемы, и это видно снаружи"
    );
}

#[test]
fn guid_lookup_of_an_unresolvable_note_reports_the_exclusion() {
    let temp = TempDir::new("review-guid-unresolvable");
    temp.write_export(&export_with(|value| {
        value["notes"][0]["note_model_uuid"] = json!("нет-такой-модели");
    }));

    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--guid", "guid-1", "--limit", "10"],
    );
    assert_eq!(
        code, 0,
        "identity lookup нашёл заметку, отказом это не является"
    );
    let result = &parsed["result"];
    assert_eq!(result["total_selected"], 0);
    assert_eq!(result["returned"], 0);
    assert_eq!(
        result["excluded_unaddressable"], 1,
        "пустая страница объяснена, а не молчалива"
    );
}

/// Обычные одиночные правила не расширяют выборку до чужих заметок.
#[test]
fn single_note_qa_code_does_not_pull_unrelated_notes() {
    let temp = TempDir::new("review-single-code");
    temp.write_export(&content_group_export());

    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "empty_field_value", "--limit", "10"],
    );
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(
        result["total_selected"], 1,
        "группа дубликатов содержимого не относится к правилу пустого поля"
    );
    let item = &result["items"][0];
    assert_eq!(item["guid"], "guid-2");
    assert!(
        item["group_membership"]
            .as_array()
            .expect("group_membership")
            .is_empty(),
        "у заметки вне группы нет group_membership"
    );
    // Item несёт *все* замечания заметки, а не только отобравший его код
    // (см. `item_findings_follow_registry_order`), поэтому здесь важно лишь то,
    // что причина попадания заметки в batch не потеряна.
    assert!(
        item["qa_findings"]
            .as_array()
            .expect("qa_findings")
            .iter()
            .any(|finding| finding["code"] == "empty_field_value"),
        "item обязан нести замечание, по которому выбран"
    );
}

/// Исключение неадресуемых заметок видно и в человеческом выводе.
#[test]
fn human_mode_explains_which_notes_were_excluded() {
    let temp = TempDir::new("review-human-excluded");
    temp.write_export(&export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("notes");
        let mut no_guid = notes[0].clone();
        no_guid["guid"] = json!(null);
        notes.push(no_guid);
    }));

    let (code, stdout, stderr) = run_cli(&[
        "review",
        &temp.path().to_string_lossy(),
        "--all",
        "--limit",
        "10",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for needle in ["Исключено неадресуемых заметок: 1", "guid"] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

/// `review` работает одинаково на экспорте другой формы.
///
/// Фикстура [`mixed_export`] отличается от [`base_export`] всем, что может
/// случайно попасть в логику: две колоды вместо одной, две модели с разным
/// числом полей, `flds` объявлены не в порядке `ord` и значения полей поэтому
/// лежат не на «своих» позициях массива `fields`. Отбор и пагинация обязаны
/// остаться теми же, а имена полей — разрешаться моделью заметки.
#[test]
fn mixed_export_is_selected_and_paged_the_same_way() {
    let temp = TempDir::new("review-mixed");
    temp.write_export(&mixed_export());
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = review_json(&export, &["--all", "--limit", "10"]);
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["notes_total"], 3);
    assert_eq!(result["total_selected"], 3);
    assert_eq!(result["excluded_unaddressable"], 0);

    // `model-out-of-order` объявляет `Гамма`/`Альфа`/`Бета` с `ord` 2/0/1,
    // поэтому значение поля берётся по `ord`, а не по позиции в `flds`.
    let first = &result["items"][0];
    assert_eq!(first["guid"], "первая-1");
    let fields = first["fields"].as_object().expect("fields");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields["Альфа"], "значение гамма");
    assert_eq!(fields["Бета"], "значение альфа");
    assert_eq!(fields["Гамма"], "значение бета");

    // Вторая модель имеет ровно одно поле, и оно тоже переносится в batch.
    let single = &result["items"][1];
    assert_eq!(single["guid"], "первая-2");
    let single_fields = single["fields"].as_object().expect("fields");
    assert_eq!(single_fields.len(), 1);
    assert_eq!(single_fields["Единственное поле"], "одно поле");

    // Область колоды: точный путь дочернего узла и путь корня.
    let (code, parsed) = review_json(&export, &["--all", "--deck", "Группа::Вторая"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 1);
    assert_eq!(parsed["result"]["items"][0]["guid"], "вторая-1");

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Группа", "--limit", "10"]);
    assert_eq!(code, 0);
    assert_eq!(
        parsed["result"]["total_selected"], 3,
        "корневая колода покрывает обе дочерние"
    );

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Вторая"]);
    assert_eq!(code, 3, "короткое имя узла — не путь колоды");
    assert_eq!(parsed["error"]["code"], "unknown_deck");

    // Критерий по полю разрешается моделью каждой заметки отдельно.
    let (code, parsed) = review_json(
        &export,
        &[
            "--field",
            "Альфа",
            "--value",
            "значение гамма",
            "--limit",
            "10",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(
        parsed["result"]["total_selected"], 2,
        "одно и то же значение `Альфа` у двух заметок разных колод"
    );

    let (code, parsed) = review_json(
        &export,
        &[
            "--field",
            "Единственное поле",
            "--value",
            "одно поле",
            "--limit",
            "10",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 1);
    assert_eq!(parsed["result"]["items"][0]["guid"], "первая-2");
}
