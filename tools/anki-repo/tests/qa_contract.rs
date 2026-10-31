//! Контракт CLI команды `qa`: коды правил, границы вывода, exit codes.
//!
//! Ни один тест здесь не знает ни layout репозитория, ни содержимого `decks/`:
//! каждая фикстура — синтетический экспорт в собственном временном каталоге, а
//! пути в аргументах CLI абсолютные, потому что `run_cli` не задаёт рабочий
//! каталог. Default test suite и CI обязаны оставаться корректными, когда
//! состав колод, уровни JLPT, note models и media изменятся или временно
//! отсутствуют.
//!
//! Ни одно ожидаемое число не зашито в тест: counts по каждому коду
//! пересчитываются из сырого `deck.json` независимым oracle'ом
//! ([`common::raw_qa_counts`]), число показанных замечаний и признак усечения
//! выводятся из этих counts, а адрес каждого показанного замечания проверяется
//! по сырым заметкам, а не по выводу tool'а.

use crate::common;

use common::{
    QA_CODES, TempDir, collect_notes, export_with, mixed_export, parse_json, raw_field_position,
    raw_models, raw_note_model_fields, raw_qa_counts, run_cli,
};
use serde_json::{Value, json};

/// Предел печати, который тесты передают явно.
///
/// Это аргумент теста, а не факт об экспорте: сколько замечаний вернётся и была
/// ли выборка обрезана, всегда считается из oracle'а.
const PRINT_LIMIT: usize = 200;

fn qa_json(export: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["--json", "qa", export];
    args.extend_from_slice(extra);
    let (code, stdout, _) = run_cli(&args);
    (code, parse_json(&stdout))
}

/// Набор значений, на которых срабатывают разные правила реестра.
///
/// Отдельная функция, а не замыкание: один и тот же набор нужно подставить и в
/// исходные имена полей, и в переименованную модель. Значения намеренно не
/// зависят ни от имён полей, ни от имён моделей — это свойство и проверяется.
fn rich_values(value: &mut Value) {
    value["notes"][0]["fields"] =
        json!([" <span style=\"color: #fff\">偶然</span> ", " значение "]);
    value["notes"][1]["fields"] = json!(["", "значение", ""]);
}

/// Экспорт, в котором срабатывает каждое правило реестра.
///
/// Набор значений подобран так, чтобы насыщенно покрыть все пять кодов: пустое
/// значение, оба краевых пробела, устаревшая белая `<span>`-обёртка и группа
/// заметок с полностью одинаковым содержимым. Ни одно число здесь не
/// утверждается — counts теста берутся из [`raw_qa_counts`].
fn rich_export() -> Value {
    export_with(|value| {
        value["notes"][0]["fields"] = json!([
            " <span style=\"color: #fff\">偶然</span> ",
            " значение ",
            "хвост "
        ]);
        value["notes"][1]["fields"] = json!(["", "значение", ""]);
        let notes = value["notes"].as_array_mut().expect("notes");
        for index in 0..2 {
            let mut copy = notes[1].clone();
            copy["guid"] = json!(format!("guid-dup-{index}"));
            notes.push(copy);
        }
    })
}

/// Насыщенный замечаниями экспорт другой формы.
///
/// Второй независимый пример: другое дерево колод, другие имена моделей и
/// полей, `flds` объявлены не в порядке `ord`, у одной модели одно поле. Набор
/// значений снова покрывает все коды, но в других позициях и с другими counts —
/// поэтому oracle проверяется не на одной форме экспорта.
fn mixed_rich_export() -> Value {
    let mut value = mixed_export();
    {
        let notes = value["children"][0]["notes"].as_array_mut().expect("notes");
        notes[0]["fields"] = json!(["<span style=\"color: #fff\">альфа</span>", " бета ", ""]);
        notes[1]["fields"] = json!(["одно поле "]);
    }
    value["children"][1]["notes"][0]["fields"] =
        json!(["<span style=\"color: #fff\">альфа</span>", " бета ", ""]);
    value
}

/// Форма замечания, не зависящая от имён: код, позиция заметки и `ord` поля.
fn finding_shape(parsed: &Value) -> Vec<(String, u64, Option<i64>)> {
    parsed["result"]["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| {
            (
                finding["code"].as_str().expect("code").to_string(),
                finding["note_index"].as_u64().expect("note_index"),
                finding["field_ord"].as_i64(),
            )
        })
        .collect()
}

#[test]
fn rules_registry_is_listed_with_stable_codes() {
    let temp = TempDir::new("qa-rules");
    temp.write_export(&rich_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(code, 0);
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["command"], "qa");

    // Реестр виден в JSON-выводе `qa` и совпадает с кодами, по которым тест
    // пересчитывает ожидания: это публичный контракт команды.
    let rules = parsed["result"]["rules"].as_array().expect("rules");
    let codes: Vec<&str> = rules
        .iter()
        .map(|rule| rule["code"].as_str().expect("code"))
        .collect();
    assert_eq!(codes, QA_CODES.to_vec());
    assert!(
        !codes.contains(&"duplicate_primary_field"),
        "удалённое правило не должно возвращаться в реестр"
    );
    for rule in rules {
        assert!(
            rule["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(rule["applicable"].as_bool().is_some());
        assert!(rule["findings"].as_u64().is_some());
    }

    let severities: Vec<&str> = rules
        .iter()
        .map(|rule| rule["severity"].as_str().expect("severity"))
        .collect();
    assert_eq!(
        severities,
        vec!["warning", "warning", "warning", "error", "warning"]
    );

    let by_code: Vec<&str> = parsed["result"]["by_code"]
        .as_array()
        .expect("by_code")
        .iter()
        .map(|entry| entry["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        by_code,
        QA_CODES.to_vec(),
        "распределение по кодам перечисляет тот же реестр"
    );
}

#[test]
fn qa_reports_error_severity_findings_without_failing() {
    let raw = rich_export();
    let temp = TempDir::new("qa-error-severity");
    temp.write_export(&raw);
    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "forbidden_white_span"],
    );

    assert_eq!(code, 0, "QA ERROR не делает команду неуспешной");
    assert_eq!(
        parsed["result"]["findings_total"],
        raw_qa_counts(&raw)["forbidden_white_span"],
        "число замечаний правила берётся из независимого пересчёта"
    );
    let finding = &parsed["result"]["findings"][0];
    assert_eq!(finding["severity"], "error");
    assert_eq!(finding["code"], "forbidden_white_span");
    assert_eq!(finding["note_index"], 0);
    assert_eq!(finding["guid"], "guid-1");
    assert_eq!(finding["field"], "Заголовок");
    assert_eq!(finding["field_ord"], 0);
    assert_eq!(finding["evidence"]["occurrences"], 1);
    assert!(
        finding["evidence"]["first_tag"]
            .as_str()
            .is_some_and(|tag| tag.contains("color: #fff"))
    );
}

#[test]
fn evidence_stays_bounded_and_never_contains_full_field_value() {
    let long = "я".repeat(2000);
    let temp = TempDir::new("qa-bounded-evidence");
    temp.write_export(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!(format!("{long} "));
    }));
    let (_, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "trailing_whitespace"],
    );

    let finding = &parsed["result"]["findings"][0];
    let sample = finding["evidence"]["sample"].as_str().expect("sample");
    assert!(
        sample.chars().count() <= 121,
        "выборка должна быть ограничена, длина {}",
        sample.chars().count()
    );
    assert!(sample.ends_with('…'));
    assert_eq!(finding["evidence"]["value_chars"], long.chars().count() + 1);
    assert_eq!(finding["evidence"]["boundary"]["chars"], 1);
}

#[test]
fn max_per_code_bounds_output_but_not_counts() {
    let raw = export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..30 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    });
    let temp = TempDir::new("qa-max-per-code");
    temp.write_export(&raw);
    let export = temp.path().to_string_lossy().to_string();

    let expected = raw_qa_counts(&raw)["empty_field_value"];
    const SMALL: usize = 5;
    assert!(
        expected > SMALL,
        "фикстура обязана превышать предел печати, иначе тест ничего не проверяет"
    );

    let small = SMALL.to_string();
    let (code, parsed) = qa_json(
        &export,
        &["--code", "empty_field_value", "--max-per-code", &small],
    );
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["findings_total"], expected);
    assert_eq!(parsed["result"]["findings_returned"], SMALL);
    assert_eq!(parsed["result"]["truncated"], expected > SMALL);
    assert_eq!(parsed["result"]["max_per_code"], SMALL);
    assert_eq!(parsed["result"]["by_code"][0]["count"], expected);
    let shown = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(shown.len(), SMALL);
    for finding in shown {
        assert_finding_matches_raw_labelled(&raw, finding, "предел печати");
    }

    let complete_limit = PRINT_LIMIT.to_string();
    let (_, complete) = qa_json(
        &export,
        &[
            "--code",
            "empty_field_value",
            "--max-per-code",
            &complete_limit,
        ],
    );
    assert!(
        expected <= PRINT_LIMIT,
        "фикстура помещается в предел печати"
    );
    assert_eq!(complete["result"]["findings_returned"], expected);
    assert_eq!(complete["result"]["truncated"], false);
}

#[test]
fn output_is_deterministic_across_runs() {
    let temp = TempDir::new("qa-determinism");
    temp.write_export(&rich_export());
    let (_, first) = qa_json(&temp.path().to_string_lossy(), &[]);
    let (_, second) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(first["result"], second["result"]);
}

#[test]
fn unknown_code_is_a_usage_class_error_with_available_codes() {
    let temp = TempDir::new("qa-unknown-code");
    temp.write_export(&rich_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &["--code", "нет_такого"]);

    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
    let available = parsed["error"]["details"]["available_codes"]
        .as_array()
        .expect("available_codes");
    let available: Vec<&str> = available
        .iter()
        .map(|code| code.as_str().expect("code"))
        .collect();
    assert_eq!(
        available,
        QA_CODES.to_vec(),
        "в ошибке перечислен ровно действующий реестр"
    );
    assert_eq!(parsed["error"]["details"]["unknown_codes"][0], "нет_такого");
}

/// Удалённое правило больше не существует для CLI: `qa --code
/// duplicate_primary_field` — usage-класс ошибка с ненулевым exit code, а не
/// молчаливый пустой вывод.
#[test]
fn removed_duplicate_primary_field_code_is_rejected() {
    let temp = TempDir::new("qa-removed-code");
    temp.write_export(&rich_export());
    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "duplicate_primary_field"],
    );

    assert_eq!(code, 3, "неизвестный код — ошибка, а не пустой результат");
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
    assert_eq!(
        parsed["error"]["details"]["unknown_codes"][0],
        "duplicate_primary_field"
    );
    let available = parsed["error"]["details"]["available_codes"]
        .as_array()
        .expect("available_codes");
    assert_eq!(
        available.len(),
        QA_CODES.len(),
        "реестр остался из пяти кодов"
    );
    assert!(
        !available
            .iter()
            .any(|code| code == "duplicate_primary_field"),
        "удалённый код не может предлагаться как доступный"
    );
}

#[test]
fn human_mode_lists_every_rule_and_keeps_stdout_only() {
    let temp = TempDir::new("qa-human");
    temp.write_export(&rich_export());
    let (code, stdout, stderr) = run_cli(&["qa", &temp.path().to_string_lossy()]);

    assert_eq!(code, 0);
    assert!(stderr.is_empty());
    let mut needles = vec![
        "По кодам:".to_string(),
        "Findings:".to_string(),
        format!("Правила ({}):", QA_CODES.len()),
        "усечено".to_string(),
    ];
    needles.extend(QA_CODES.iter().map(|code| (*code).to_string()));
    for needle in &needles {
        assert!(
            stdout.contains(needle.as_str()),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("duplicate_primary_field"),
        "удалённое правило не должно печататься\n{stdout}"
    );
}

/// Counts и адреса замечаний на synthetic-экспортах пересчитываются независимо.
///
/// Тест не утверждает ни одного зашитого числа: ожидаемые counts берутся из
/// [`raw_qa_counts`], признак усечения и число показанных замечаний выводятся из
/// них, а каждое показанное замечание проверяется по сырым заметкам. Насыщенность
/// (замечания есть у каждого кода реестра) тоже проверяется через oracle.
#[test]
fn synthetic_exports_report_independently_recomputed_counts() {
    let exports = [("rich", rich_export()), ("mixed", mixed_rich_export())];

    for (label, raw) in exports {
        let temp = TempDir::new(&format!("qa-oracle-{label}"));
        temp.write_export(&raw);
        let export = temp.path().to_string_lossy().to_string();
        let expected = raw_qa_counts(&raw);

        let mut notes = Vec::new();
        collect_notes(&raw, &mut notes);
        for code in QA_CODES {
            assert!(
                expected[code] > 0,
                "{label}: фикстура обязана давать замечания кода {code}"
            );
        }

        let limit = PRINT_LIMIT.to_string();
        let (exit, parsed) = qa_json(&export, &["--max-per-code", &limit]);
        assert_eq!(exit, 0, "{label}");
        assert_eq!(
            parsed["result"]["notes_total"],
            notes.len(),
            "{label}: число заметок"
        );

        let by_code = parsed["result"]["by_code"].as_array().expect("by_code");
        assert_eq!(by_code.len(), QA_CODES.len(), "{label}: все коды в выводе");
        for code in QA_CODES {
            let count = by_code
                .iter()
                .find(|entry| entry["code"] == code)
                .map_or(0, |entry| entry["count"].as_u64().expect("count"));
            assert_eq!(
                count as usize, expected[code],
                "{label}: {code} должен совпадать с независимым пересчётом"
            );
        }

        let total: u64 = by_code
            .iter()
            .map(|entry| entry["count"].as_u64().expect("count"))
            .sum();
        assert_eq!(
            total as usize, parsed["result"]["findings_total"],
            "{label}: сумма по кодам"
        );

        // Реестр правил не зависит от содержимого экспорта.
        assert_eq!(
            parsed["result"]["rules"].as_array().expect("rules").len(),
            QA_CODES.len(),
            "{label}"
        );

        // Печать ограничена кодом, у которого замечаний больше предела; адрес
        // каждого показанного замечания проверяется прямо по сырым заметкам.
        let widest = QA_CODES
            .iter()
            .map(|code| expected[code])
            .max()
            .expect("реестр не пуст");
        assert_eq!(
            parsed["result"]["truncated"],
            widest > PRINT_LIMIT,
            "{label}: признак усечения следует из oracle'а"
        );
        // Предел применяется к каждому коду отдельно, поэтому ожидание — сумма
        // урезанных по коду счётчиков, а не урезанная сумма всех замечаний.
        assert_eq!(
            parsed["result"]["findings_returned"],
            QA_CODES
                .iter()
                .map(|code| expected[code].min(PRINT_LIMIT))
                .sum::<usize>(),
            "{label}: показано по пределу на каждый код"
        );
        for finding in parsed["result"]["findings"].as_array().expect("findings") {
            assert_finding_matches_raw_labelled(&raw, finding, label);
        }
    }
}

/// Группа больше предела участников обрезается явно, и ветка `related_truncated`
/// проверяется тем же oracle'ом, а не отдельным набором утверждений.
#[test]
fn oversized_group_is_reported_as_truncated() {
    let limit = anki_repo::qa::duplicate_rules::MAX_RELATED_NOTES;
    let raw = export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..=limit {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-bulk-{index}"));
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    });
    let temp = TempDir::new("qa-group-truncated");
    temp.write_export(&raw);

    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "duplicate_note_content"],
    );
    assert_eq!(code, 0);

    // Ожидаемая группа собирается из сырых заметок: участники — заметки той же
    // модели с теми же значениями `fields`, в порядке экспорта.
    let mut notes = Vec::new();
    collect_notes(&raw, &mut notes);
    let group_fingerprint = serde_json::to_string(&notes[0]["fields"]).expect("fields");
    let group_model = notes[0]["note_model_uuid"].clone();
    let members: Vec<usize> = notes
        .iter()
        .enumerate()
        .filter(|(_, note)| {
            note["note_model_uuid"] == group_model
                && serde_json::to_string(&note["fields"]).expect("fields") == group_fingerprint
        })
        .map(|(index, _)| index)
        .collect();
    assert!(
        members.len() > limit,
        "фикстура обязана превышать предел числа участников"
    );

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "группа описывается одним замечанием");
    let finding = &findings[0];
    assert_eq!(finding["related_truncated"], true);
    assert_eq!(
        finding["related_note_indices"],
        json!(members[..limit].to_vec()),
        "усечённый список участников — первые участники группы"
    );
    assert_eq!(
        finding["related_guids"]
            .as_array()
            .expect("related_guids")
            .len(),
        limit,
        "guid на каждого показанного участника"
    );
    assert_eq!(finding["group_size"], members.len());
    assert_finding_matches_raw_labelled(&raw, finding, "группа сверх предела");
}

/// Проверяет одно замечание по сырому `deck.json`.
///
/// Проверка идёт от адреса (`note_index` → заметка в сыром порядке обхода) и от
/// свойства правила, а не от сообщения tool'а: тест не повторяет формулировки,
/// а пересчитывает факт. `prefix` называет экспорт в диагностике, поэтому
/// oracle'ом пользуются и синтетические фикстуры без номера колоды.
fn assert_finding_matches_raw_labelled(raw: &Value, finding: &Value, prefix: &str) {
    let code = finding["code"].as_str().expect("code");
    let label = format!("{prefix}: {code} #{}", finding["note_index"]);

    let mut notes = Vec::new();
    collect_notes(raw, &mut notes);
    let position = finding["note_index"].as_u64().expect("note_index") as usize;
    let note = notes
        .get(position)
        .unwrap_or_else(|| panic!("{label}: нет заметки"));
    let models = raw_models(raw);

    // Групповые утверждения относятся только к `duplicate_note_content`:
    // контракт `related`/`group_size`/`related_truncated` принадлежит правилу
    // дубликатов содержимого, а не какому-либо «главному полю» заметки.
    if code == "duplicate_note_content" {
        assert!(
            finding["addressable"].as_bool().expect("addressable"),
            "{label}: владелец группы обязан быть адресуемым"
        );
        let related = finding["related_note_indices"]
            .as_array()
            .expect("related_note_indices");
        let group_size = finding["group_size"].as_u64().expect("group_size");
        let truncated = finding["related_truncated"]
            .as_bool()
            .expect("related_truncated");
        assert!(
            related.len() >= 2,
            "{label}: группа не может состоять из одной заметки"
        );
        // Список участников ограничен доменом, поэтому «размер группы равен
        // длине списка» верно только без усечения; усечённый список обязан быть
        // полным до предела, а группа — больше него.
        if truncated {
            assert_eq!(
                related.len(),
                anki_repo::qa::duplicate_rules::MAX_RELATED_NOTES,
                "{label}: усечённый список участников полон до предела"
            );
            assert!(
                group_size > related.len() as u64,
                "{label}: усечённая группа больше показанного списка"
            );
        } else {
            assert_eq!(
                group_size,
                related.len() as u64,
                "{label}: без усечения размер группы совпадает со списком"
            );
        }
        let guids = finding["related_guids"].as_array().expect("related_guids");
        assert_eq!(
            guids.len(),
            related.len(),
            "{label}: guid на каждого участника"
        );

        let members: Vec<&Value> = related
            .iter()
            .map(|position| {
                *notes
                    .get(position.as_u64().expect("note_index") as usize)
                    .unwrap_or_else(|| panic!("{label}: нет участника группы"))
            })
            .collect();
        let first = serde_json::to_string(&members[0]["fields"]).expect("fields");
        for member in &members {
            assert_eq!(
                serde_json::to_string(&member["fields"]).expect("fields"),
                first,
                "{label}: участники обязаны иметь одинаковые fields"
            );
        }
        return;
    }

    let ord = raw_field_position(&models, note, finding["field"].as_str().expect("field"));
    let value = note["fields"][ord]
        .as_str()
        .unwrap_or_else(|| panic!("{label}: значение поля должно быть строкой"));

    match code {
        "empty_field_value" => {
            assert!(value.is_empty(), "{label}: значение должно быть пустым");
            assert_eq!(finding["evidence"]["value_chars"], 0, "{label}");
        }
        "leading_whitespace" => {
            assert!(
                value.starts_with(char::is_whitespace),
                "{label}: значение должно начинаться с whitespace: {value:?}"
            );
        }
        "trailing_whitespace" => {
            assert!(
                value.ends_with(char::is_whitespace),
                "{label}: значение должно заканчиваться whitespace: {value:?}"
            );
        }
        "forbidden_white_span" => {
            assert!(
                finding["evidence"]["occurrences"]
                    .as_u64()
                    .expect("occurrences")
                    >= 1
                    && value.contains("<span"),
                "{label}: значение должно содержать <span>-обёртку"
            );
        }
        other => panic!("{label}: неизвестный код {other}"),
    }
}

#[test]
fn empty_field_findings_point_at_a_real_empty_value() {
    let raw = rich_export();
    let temp = TempDir::new("qa-empty-field");
    temp.write_export(&raw);
    let export = temp.path().to_string_lossy().to_string();

    let mut notes = Vec::new();
    collect_notes(&raw, &mut notes);
    let expected = raw_qa_counts(&raw)["empty_field_value"];

    let limit = PRINT_LIMIT.to_string();
    let (code, parsed) = qa_json(
        &export,
        &["--code", "empty_field_value", "--max-per-code", &limit],
    );
    assert_eq!(code, 0);

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(
        findings.len(),
        expected.min(PRINT_LIMIT),
        "показаны ровно те замечания правила, что помещаются в предел печати"
    );
    assert_eq!(
        parsed["result"]["truncated"],
        expected > PRINT_LIMIT,
        "признак усечения следует из oracle'а"
    );

    // Адрес каждого замечания разрешается по сырому deck.json: заметка берётся из
    // preorder-обхода (а не из корневого массива), а имя поля — из модели этой
    // заметки, а не из исторического содержимого колоды.
    for finding in findings {
        let index =
            usize::try_from(finding["note_index"].as_u64().expect("note_index")).expect("usize");
        let ord =
            usize::try_from(finding["field_ord"].as_u64().expect("field_ord")).expect("usize");
        let note = &notes[index];
        let (name, declared_ord) = raw_note_model_fields(&raw, note)
            .into_iter()
            .find(|(_, model_ord)| usize::try_from(*model_ord).ok() == Some(ord))
            .expect("поле модели с таким ord");

        assert_eq!(finding["field"], name, "замечание называет поле модели");
        assert_eq!(declared_ord, i64::try_from(ord).expect("i64"));
        assert_eq!(finding["severity"], "warning");
        assert_eq!(
            note["fields"][ord], "",
            "замечание должно указывать на пустое значение"
        );
        assert_eq!(finding["evidence"]["value_chars"], 0);
    }
}

#[test]
fn findings_on_unaddressable_notes_are_marked_and_counted() {
    let raw = export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("notes");
        let mut no_guid = notes[0].clone();
        no_guid["guid"] = json!(null);
        no_guid["fields"] = json!(["", "значение", ""]);
        notes.push(no_guid);
        let mut duplicate_guid = notes[0].clone();
        duplicate_guid["fields"] = json!(["", "значение", ""]);
        notes.push(duplicate_guid);
    });
    let temp = TempDir::new("qa-unaddressable");
    temp.write_export(&raw);
    let export = temp.path().to_string_lossy().to_string();

    // Ожидание считается по самому fixture: заметка неадресуема, если её guid
    // отсутствует или встречается больше одного раза.
    let mut raw_notes = Vec::new();
    collect_notes(&raw, &mut raw_notes);
    let guids: Vec<Option<&str>> = raw_notes.iter().map(|note| note["guid"].as_str()).collect();
    let duplicated: Vec<&str> = guids
        .iter()
        .flatten()
        .filter(|guid| guids.iter().filter(|other| **other == Some(**guid)).count() > 1)
        .copied()
        .collect();

    let limit = PRINT_LIMIT.to_string();
    let (code, parsed) = qa_json(
        &export,
        &["--code", "empty_field_value", "--max-per-code", &limit],
    );
    assert_eq!(code, 0, "неадресуемость — не отказ команды");

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(
        findings.len(),
        raw_qa_counts(&raw)["empty_field_value"],
        "counts замечаний пересчитываются из сырого JSON"
    );

    let mut unaddressable = 0;
    let mut addressable = 0;
    for finding in findings {
        let index = finding["note_index"].as_u64().expect("note_index") as usize;
        let guid = guids[index];
        let expected = guid.is_none_or(|guid| duplicated.contains(&guid));
        assert_eq!(
            finding["addressable"], !expected,
            "адресуемость обязана совпасть с сырым guid: {finding}"
        );
        assert_eq!(
            finding["guid"], raw_notes[index]["guid"],
            "guid заметки не подменяется"
        );
        unaddressable += usize::from(expected);
        addressable += usize::from(!expected);
    }
    assert_eq!(
        parsed["result"]["unaddressable_findings"], unaddressable,
        "счётчик неадресуемых замечаний выводится из сырых guid, а не из вывода"
    );
    assert!(
        unaddressable > 0 && addressable > 0,
        "фикстура обязана покрывать обе стороны, и правило не помечает \
         неадресуемым всё подряд: {unaddressable} неадресуемых, {addressable} адресуемых"
    );
}

#[test]
fn qa_summarizes_the_full_group_for_grouped_findings() {
    let raw = export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 3..6 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-{index}"));
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    });
    let temp = TempDir::new("qa-group-context");
    temp.write_export(&raw);

    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "duplicate_note_content"],
    );
    assert_eq!(code, 0);

    // Группа и её участники выводятся из сырых заметок, а не из вывода tool'а.
    let mut notes = Vec::new();
    collect_notes(&raw, &mut notes);
    let group_fingerprint = serde_json::to_string(&notes[0]["fields"]).expect("fields");
    let group_model = notes[0]["note_model_uuid"].clone();
    let members: Vec<usize> = notes
        .iter()
        .enumerate()
        .filter(|(_, note)| {
            note["note_model_uuid"] == group_model
                && serde_json::to_string(&note["fields"]).expect("fields") == group_fingerprint
        })
        .map(|(index, _)| index)
        .collect();
    let member_guids: Vec<&str> = members
        .iter()
        .map(|index| notes[*index]["guid"].as_str().expect("guid"))
        .collect();

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(
        findings.len(),
        raw_qa_counts(&raw)["duplicate_note_content"],
        "группа описывается одним замечанием"
    );
    let finding = &findings[0];
    assert_eq!(finding["group_size"], members.len());
    assert_eq!(finding["related_note_indices"], json!(members));
    assert_eq!(
        finding["related_guids"],
        json!(member_guids),
        "участники названы так, чтобы предложение можно было исполнить"
    );
    assert_eq!(finding["related_truncated"], false);
    assert_eq!(
        finding["evidence"]["fields_total"],
        notes[0]["fields"].as_array().expect("fields").len(),
        "evidence хранит только то, что относится к правилу"
    );
}

#[test]
fn human_truncation_message_names_the_real_way_to_see_the_rest() {
    let temp = TempDir::new("qa-human-truncated");
    temp.write_export(&export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..30 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let (code, stdout, _) = run_cli(&[
        "qa",
        &temp.path().to_string_lossy(),
        "--code",
        "empty_field_value",
        "--max-per-code",
        "5",
    ]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("--max-per-code"),
        "сообщение обязано называть реальный способ увидеть остаток: {stdout}"
    );
    assert!(
        !stdout.contains("полный список доступен"),
        "обещание полного списка неверно: JSON сериализует тот же vector\n{stdout}"
    );
    assert!(stdout.contains("остальные не выводятся"), "{stdout}");
}

/// QA судит о значениях, а не об именах полей, моделей и колод.
///
/// Один и тот же набор значений под полностью другими именами обязан дать тот
/// же набор замечаний — те же коды в тех же позициях. `ord` при этом не меняется,
/// а объявление `flds` намеренно идёт не по порядку: адрес поля задаётся `ord`
/// модели, поэтому ни имя, ни позиция объявления на результат не влияют.
#[test]
fn rules_are_independent_of_field_and_model_names() {
    let baseline = export_with(rich_values);
    let renamed = export_with(|value| {
        rich_values(value);
        value["name"] = json!("Совсем другая колода");
        let model = &mut value["note_models"][0];
        model["crowdanki_uuid"] = json!("model-alpha");
        model["name"] = json!("Модель \"Альфа\"");
        model["flds"] = json!([
            {"name": "Поле третье", "ord": 2},
            {"name": "Поле первое", "ord": 0},
            {"name": "Поле второе", "ord": 1}
        ]);
        for note in value["notes"].as_array_mut().expect("notes") {
            note["note_model_uuid"] = json!("model-alpha");
        }
    });

    let baseline_dir = TempDir::new("qa-renamed-baseline");
    baseline_dir.write_export(&baseline);
    let renamed_dir = TempDir::new("qa-renamed-model");
    renamed_dir.write_export(&renamed);

    let limit = PRINT_LIMIT.to_string();
    let (code, base_parsed) = qa_json(
        &baseline_dir.path().to_string_lossy(),
        &["--max-per-code", &limit],
    );
    assert_eq!(code, 0);
    let (code, renamed_parsed) = qa_json(
        &renamed_dir.path().to_string_lossy(),
        &["--max-per-code", &limit],
    );
    assert_eq!(code, 0);

    let shape = finding_shape(&base_parsed);
    assert!(
        !shape.is_empty(),
        "фикстура обязана давать замечания, иначе равенство пусто"
    );
    assert_eq!(
        shape,
        finding_shape(&renamed_parsed),
        "переименование полей, моделей и колоды не меняет набор замечаний"
    );
    assert_eq!(
        base_parsed["result"]["by_code"], renamed_parsed["result"]["by_code"],
        "counts по кодам тоже не зависят от имён"
    );

    // Проверка, что переименование действительно применилось: иначе равенство
    // выше выполнялось бы по тривиальной причине.
    let renamed_findings = renamed_parsed["result"]["findings"]
        .as_array()
        .expect("findings");
    for finding in renamed_findings {
        assert_eq!(finding["note_model_uuid"], "model-alpha");
        if let Some(name) = finding["field"].as_str() {
            assert!(
                name.starts_with("Поле "),
                "имя поля обязано быть переименованным: {name}"
            );
        }
    }
}

/// Шкала QA не связана со шкалой `validate`: `error` у правила — это серьёзный
/// дефект содержимого, а не структурная ошибка экспорта.
#[test]
fn qa_error_severity_neither_invalidates_the_export_nor_blocks_edit() {
    let temp = TempDir::new("qa-error-independence");
    temp.write_canonical_deck_json(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!("<span style=\"color: #fff\">偶然</span>");
    }));
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = qa_json(&export, &["--code", "forbidden_white_span"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["findings"][0]["severity"], "error");

    // Экспорт с QA `error` остаётся структурно валидным: это другая ось.
    let (code, stdout, _) = run_cli(&["--json", "validate", &export]);
    assert_eq!(code, 0, "QA error не делает export невалидным");
    let validated = parse_json(&stdout);
    assert_eq!(validated["result"]["valid"], true);
    assert_eq!(validated["result"]["summary"]["errors"], 0);

    // И не мешает правке значения поля.
    let request = common::edit_request(&[(
        "guid-1",
        "Заголовок",
        "<span style=\"color: #fff\">偶然</span>",
        "偶然",
    )]);
    let path = common::write_request(&temp, "edit.json", &request);
    let (code, stdout, _) = run_cli(&[
        "--json",
        "edit",
        &export,
        "--request",
        &path.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "QA замечания не блокируют edit");
    let planned = parse_json(&stdout);
    assert_eq!(planned["result"]["effective_edits"], 1);
    assert_eq!(planned["result"]["applied"], false);
}
