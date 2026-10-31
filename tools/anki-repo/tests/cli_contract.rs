//! Контракт CLI: аргументы, exit codes, разделение stdout/stderr, JSON envelope.
//!
//! Ни один тест этого файла не знает ни структуру репозитория, ни содержимого
//! `decks/`: каждый тест строит фикстуру сам — синтетический экспорт в
//! собственном временном каталоге. Поэтому сьют остаётся корректным, когда
//! состав колод, уровни JLPT, note models и media изменятся или временно
//! отсутствуют.
//!
//! Ожидаемые количества и значения либо заданы локальной фикстурой этого файла
//! явно, либо пересчитаны независимо от CLI через `raw_*`/`raw_qa_counts`
//! вспомогательные функции `common`. Пути в аргументах всегда абсолютны: `run_cli` запускает
//! бинарь без задания рабочего каталога.

use crate::common;

#[cfg(target_os = "linux")]
use std::fs::File;
use std::process::{Command, Stdio};

use common::{
    QA_CODES, TempDir, base_export, canonical_base_export, canonical_export, cli_binary,
    export_with, mixed_export, parse_json, raw_qa_counts, run_cli, run_cli_in,
};
use serde_json::{Value, json};

/// Абсолютный путь каталога фикстуры как `&str`.
fn path_str(dir: &TempDir) -> String {
    dir.path()
        .to_str()
        .expect("путь временного каталога обязан быть валидным utf-8")
        .to_string()
}

/// Сколько различных значений поля `Толкование` строит [`distribution_export`].
const DISTINCT_VALUES: usize = 5;

/// Предел вывода распределения, при котором распределение обязано усечься.
const DISTRIBUTION_TOP: usize = 3;

/// Синтетический экспорт с предсказуемым распределением значений поля.
///
/// Форма экспорта та же, что у [`base_export`] (одна колода, одна модель,
/// три поля), меняется только набор заметок: их ровно [`DISTINCT_VALUES`], и у
/// каждой своё значение поля `Толкование`. Так границы `--limit` и `--top`
/// проверяются на количествах, которые тест задаёт сам, а не на фактическом
/// содержимом пользовательской колоды.
fn distribution_export() -> Value {
    export_with(|value| {
        let notes: Vec<Value> = (0..DISTINCT_VALUES)
            .map(|index| {
                json!({
                    "__type__": "Note",
                    "guid": format!("guid-{index}"),
                    "note_model_uuid": "model-1",
                    "tags": [],
                    "fields": [format!("слово-{index}"), format!("значение-{index}"), ""],
                })
            })
            .collect();
        value["notes"] = Value::Array(notes);
    })
}

#[test]
fn help_and_version_succeed() {
    let (code, stdout, _) = run_cli(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("inspect"));
    assert!(stdout.contains("validate"));
    assert!(stdout.contains("migrate-media"));
    assert!(stdout.contains("dry-run"));
    assert!(stdout.contains("--apply"));

    let (code, stdout, _) = run_cli(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("anki-repo"));
}

#[test]
fn migration_help_exposes_identity_and_explicit_apply_contract() {
    let (code, stdout, stderr) = run_cli(&["migrate-media", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    for literal in [
        "migrate-media",
        "--namespace",
        "--key",
        "--from",
        "--apply",
        "dry-run",
    ] {
        assert!(
            stdout.contains(literal),
            "в справке отсутствует {literal}: {stdout}"
        );
    }
}

#[test]
fn missing_subcommand_is_a_usage_error() {
    let (code, stdout, stderr) = run_cli(&[]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let (code, _, stderr) = run_cli(&["нет-такой-команды"]);
    assert_eq!(code, 2);
    assert!(!stderr.is_empty());
}

/// Неполные и несовместимые критерии поиска — всегда usage-ошибка с пустым
/// stdout, независимо от того, отверг их clap или сам инструмент.
///
/// Каталог экспорта валиден: часть этих проверок выполняется уже после загрузки
/// экспорта, и фикстура не должна подменять ожидаемую usage-ошибку ошибкой
/// ввода.
#[test]
fn conflicting_or_incomplete_criteria_are_usage_errors() {
    let dir = TempDir::new("cli-usage");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let cases: [&[&str]; 7] = [
        // Критерий не задан вовсе.
        &["find", path.as_str()],
        // `--guid` и `--field` взаимно исключают друг друга.
        &[
            "find",
            path.as_str(),
            "--guid",
            "x",
            "--field",
            "Заголовок",
            "--value",
            "y",
        ],
        // `--field` без значения.
        &["find", path.as_str(), "--field", "Заголовок"],
        // `--value` без поля.
        &["find", path.as_str(), "--value", "x"],
        // `--match` осмысленен только вместе с `--field` (проверка инструмента).
        &["find", path.as_str(), "--guid", "x", "--match", "exact"],
        // `--limit` вне объявленных границ.
        &[
            "find",
            path.as_str(),
            "--field",
            "Заголовок",
            "--value",
            "x",
            "--limit",
            "900",
        ],
        // `--top` без `--group-by`.
        &["stats", path.as_str(), "--top", "5"],
    ];
    for args in cases {
        let (code, stdout, _) = run_cli(args);
        assert_eq!(code, 2, "clap должен отклонить некорректные аргументы");
        assert!(stdout.is_empty(), "clap не должен писать в stdout");
    }
}

/// Ошибки, которые ловит сам clap, не становятся JSON-конвертом даже при
/// `--json`: команда ещё не определена, поэтому текст идёт в stderr. Ошибку
/// несовместимых аргументов находит сам инструмент, и она отдаётся конвертом.
#[test]
fn clap_usage_error_is_text_in_stderr_even_in_json_mode() {
    let dir = TempDir::new("cli-usage-json");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "x",
        "--limit",
        "900",
    ]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(!stderr.is_empty());

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--guid",
        "x",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], json!("usage"));
}

/// Один и тот же синтетический экспорт читается и по абсолютному пути из
/// произвольного рабочего каталога, и по относительному пути, разрешённому
/// относительно рабочего каталога процесса.
#[test]
fn inspect_works_with_relative_and_absolute_paths() {
    let dir = canonical_base_export("cli-paths");
    let absolute = path_str(&dir);

    let elsewhere = TempDir::new("cli-paths-cwd");
    let (code, stdout, stderr) = run_cli_in(Some(elsewhere.path()), &["inspect", &absolute]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Test::Deck"));
    assert!(stdout.contains("Заголовок"));

    let (code, stdout, stderr) = run_cli_in(Some(dir.path()), &["inspect", "."]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Test::Deck"));
}

#[test]
fn json_flag_works_before_and_after_the_subcommand() {
    let dir = TempDir::new("cli-json-flag");
    dir.write_export(&base_export());
    let path = path_str(&dir);

    for args in [
        vec!["--json", "inspect", path.as_str()],
        vec!["inspect", path.as_str(), "--json"],
    ] {
        let (code, stdout, stderr) = run_cli(&args);
        assert_eq!(code, 0, "аргументы {args:?}: stderr {stderr}");
        let value = parse_json(&stdout);
        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("inspect"));
        assert_eq!(value["result"]["root_deck_name"], json!("Test::Deck"));
    }
}

#[test]
fn human_output_is_not_json() {
    let dir = TempDir::new("cli-human");
    dir.write_export(&base_export());
    let path = path_str(&dir);

    let (code, stdout, _) = run_cli(&["inspect", &path]);
    assert_eq!(code, 0);
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());
    assert!(
        stdout.ends_with('\n'),
        "вывод должен заканчиваться переводом строки"
    );
}

#[test]
fn missing_directory_is_an_input_error() {
    let dir = TempDir::new("cli-missing");
    let missing = dir
        .path()
        .join("НетТакой")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, stdout, stderr) = run_cli(&["inspect", &missing]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn deck_json_path_instead_of_directory_is_an_input_error() {
    let dir = TempDir::new("cli-deck-json-path");
    dir.write_export(&base_export());
    let deck_json = dir
        .path()
        .join("deck.json")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, _, stderr) = run_cli(&["inspect", &deck_json]);
    assert_eq!(code, 3);
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn invalid_json_is_an_input_error_for_inspect() {
    let dir = TempDir::new("cli-invalid-json");
    dir.write_raw_deck_json("{ это не JSON");

    let (code, _, stderr) = run_cli(&["inspect", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 3);
    assert!(stderr.contains("invalid_json"));
}

#[test]
fn root_that_is_not_a_deck_is_an_input_error() {
    let dir = TempDir::new("cli-root");
    dir.write_export(&json!({"__type__": "Note"}));

    let (code, _, stderr) = run_cli(&["stats", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 3);
    assert!(stderr.contains("root_not_deck"));
}

#[test]
fn find_without_matches_exits_with_four() {
    let dir = TempDir::new("cli-not-found");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "ЭТОГО_НЕТ",
    ]);
    assert_eq!(code, 4);
    assert!(stdout.is_empty());
    assert!(stderr.contains("not_found"));

    let (code, stdout, _) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "ЭТОГО_НЕТ",
    ]);
    assert_eq!(code, 4);
    let value = parse_json(&stdout);
    assert_eq!(value["error"]["code"], json!("not_found"));
    assert_eq!(value["schema_version"], json!(1));
}

#[test]
fn ambiguous_guid_exits_with_five() {
    let dir = TempDir::new("cli-ambiguous");
    dir.write_export(&export_with(|value| {
        value["notes"][1]["guid"] = json!("guid-1");
    }));
    dir.write_media(&["a.mp3"]);

    let (code, _, stderr) = run_cli(&[
        "find",
        dir.path().to_str().expect("utf-8"),
        "--guid",
        "guid-1",
    ]);
    assert_eq!(code, 5);
    assert!(stderr.contains("ambiguous"));
}

#[test]
fn validate_reports_invalid_export_with_exit_six() {
    let dir = TempDir::new("cli-validate-invalid");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["note_model_uuid"] = json!("нет-такой");
    }));
    dir.write_media(&["a.mp3"]);

    let (code, stdout, stderr) = run_cli(&["validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6, "stderr: {stderr}");
    assert!(stdout.contains("Валиден: нет"));
    assert!(stdout.contains("note_model_unresolved"));

    let (code, stdout, _) = run_cli(&["--json", "validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["valid"], json!(false));
    assert!(value["result"]["summary"]["errors"].as_u64().unwrap_or(0) >= 1);
}

/// Каждый валидный синтетический экспорт обязан проходить `validate` с exit 0,
/// в том числе экспорт другой формы ([`mixed_export`]) и экспорт с другим
/// набором заметок ([`distribution_export`]).
#[test]
fn validate_of_canonical_synthetic_exports_exits_with_zero() {
    let cases: [(&str, Value); 3] = [
        ("base", base_export()),
        ("mixed", mixed_export()),
        ("distribution", distribution_export()),
    ];

    for (label, export) in cases {
        let dir = canonical_export(label, &export);
        let path = path_str(&dir);
        let (code, stdout, stderr) = run_cli(&["validate", &path]);
        assert_eq!(code, 0, "{label}: stderr {stderr}");
        assert!(stdout.contains("Валиден: да"), "{label}: stdout {stdout}");
    }
}

#[test]
fn invalid_json_inside_validate_is_reported_as_issue() {
    let dir = TempDir::new("cli-validate-json");
    dir.write_raw_deck_json("{");

    let (code, stdout, _) = run_cli(&["validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    assert!(stdout.contains("invalid_json"));

    let (code, stdout, _) = run_cli(&["--json", "validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["summary"]["errors"], json!(1));
}

#[test]
fn json_error_goes_to_stdout_only() {
    let dir = TempDir::new("cli-json-error");
    dir.write_export(&base_export());
    let path = path_str(&dir);
    let missing = dir
        .path()
        .join("НетТакого")
        .to_str()
        .expect("utf-8")
        .to_string();

    let cases: [Vec<&str>; 4] = [
        vec!["--json", "inspect", &missing],
        vec![
            "--json",
            "find",
            &path,
            "--field",
            "Заголовок",
            "--value",
            "ЭТОГО_НЕТ",
        ],
        vec!["--json", "stats", &missing],
        vec!["--json", "validate", &missing],
    ];
    for args in &cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_ne!(code, 0, "аргументы {args:?}");
        assert!(stderr.is_empty(), "аргументы {args:?}: stderr {stderr}");
        let value = parse_json(&stdout);
        assert_eq!(value["schema_version"], json!(1));
        assert!(value["error"]["code"].is_string(), "аргументы {args:?}");
        assert!(value["error"]["message"].is_string());
        assert!(value["error"]["details"].is_object());
    }
}

#[test]
fn human_error_goes_to_stderr_only() {
    let dir = TempDir::new("cli-human-error");
    let missing = dir
        .path()
        .join("НетТакого")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, stdout, stderr) = run_cli(&["stats", &missing]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn find_json_contains_criteria_limit_and_notes() {
    let dir = TempDir::new("cli-find-json");
    dir.write_export(&distribution_export());
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Толкование",
        "--value",
        "значение-",
        "--limit",
        "1",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    assert_eq!(result["criteria"]["kind"], json!("field"));
    assert_eq!(result["criteria"]["field"], json!("Толкование"));
    assert_eq!(result["criteria"]["match_mode"], json!("contains"));
    assert_eq!(result["criteria"]["limit"], json!(1));
    // Значение критерия встречается у всех заметок фикстуры, но `--limit`
    // возвращает только первую, сохраняя полное число совпадений.
    assert_eq!(result["matched_total"], json!(DISTINCT_VALUES));
    assert_eq!(result["returned"], json!(1));
    assert_eq!(result["truncated"], json!(true));
    assert!(result["notes"][0]["fields"]["Толкование"].is_string());
}

#[test]
fn stats_json_contains_group_by_distribution() {
    let dir = TempDir::new("cli-stats-json");
    dir.write_export(&distribution_export());
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "stats",
        &path,
        "--group-by",
        "Толкование",
        "--top",
        &DISTRIBUTION_TOP.to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let group = &value["result"]["group_by"];
    assert_eq!(group["field"], json!("Толкование"));
    assert_eq!(group["distinct_values"], json!(DISTINCT_VALUES));
    assert_eq!(group["truncated"], json!(true));
    assert_eq!(
        group["values"].as_array().map(Vec::len),
        Some(DISTRIBUTION_TOP)
    );
}

#[test]
fn inspect_verbose_json_adds_diagnostics() {
    let dir = TempDir::new("cli-inspect-verbose");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["--json", "inspect", &path, "--verbose"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let verbose = &value["result"]["verbose"];
    assert!(verbose.is_object(), "ожидался блок verbose");
    assert_eq!(
        verbose["deck_json"],
        json!(dir.path().join("deck.json").to_string_lossy())
    );
    // Фикстура `base_export` — одна колода без потомков.
    assert_eq!(verbose["nodes"].as_array().map(Vec::len), Some(1));
    assert_eq!(verbose["guid_duplicates"], json!(0));
    assert_eq!(
        verbose["model_templates"][0]["templates"][0]["name"],
        json!("Карточка 1")
    );

    let (_, stdout, _) = run_cli(&["--json", "inspect", &path]);
    let value = parse_json(&stdout);
    assert!(
        value["result"]["verbose"].is_null(),
        "без --verbose блока быть не должно"
    );
}

#[test]
fn commands_do_not_modify_the_export() {
    let dir = canonical_base_export("cli-read-only");
    dir.write_media(&["a.mp3"]);
    let deck_before = std::fs::read(dir.path().join("deck.json")).expect("deck.json");
    let media_before = std::fs::read_dir(dir.path().join("media"))
        .expect("media")
        .count();
    let path = path_str(&dir);

    for args in [
        vec!["inspect".to_string(), path.clone()],
        vec!["inspect".to_string(), path.clone(), "--verbose".to_string()],
        vec![
            "find".to_string(),
            path.clone(),
            "--field".to_string(),
            "Заголовок".to_string(),
            "--value".to_string(),
            "偶然".to_string(),
        ],
        vec!["stats".to_string(), path.clone()],
        vec![
            "stats".to_string(),
            path.clone(),
            "--group-by".to_string(),
            "Толкование".to_string(),
        ],
        vec!["validate".to_string(), path.clone()],
    ] {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, stderr) = run_cli(&borrowed);
        assert_eq!(code, 0, "аргументы {args:?}: stderr {stderr}");
    }

    assert_eq!(
        deck_before,
        std::fs::read(dir.path().join("deck.json")).expect("deck.json")
    );
    assert_eq!(
        media_before,
        std::fs::read_dir(dir.path().join("media"))
            .expect("media")
            .count()
    );
    assert!(!dir.path().join("anki-repo.tmp").exists());
}

#[test]
fn repeated_runs_produce_byte_identical_output() {
    let base = TempDir::new("cli-deterministic-base");
    base.write_export(&base_export());
    base.write_media(&["a.mp3"]);
    let base_path = path_str(&base);

    let distribution = TempDir::new("cli-deterministic-distribution");
    distribution.write_export(&distribution_export());
    let distribution_path = path_str(&distribution);

    for args in [
        vec!["inspect", base_path.as_str()],
        vec!["--json", "inspect", base_path.as_str(), "--verbose"],
        vec!["stats", base_path.as_str(), "--group-by", "Толкование"],
        vec!["--json", "validate", base_path.as_str()],
        vec![
            "--json",
            "find",
            distribution_path.as_str(),
            "--field",
            "Толкование",
            "--value",
            "значение-",
        ],
    ] {
        let (code_one, first, _) = run_cli(&args);
        let (code_two, second, _) = run_cli(&args);
        assert_eq!(code_one, code_two, "аргументы {args:?}");
        assert_eq!(
            first, second,
            "аргументы {args:?}: вывод должен быть детерминированным"
        );
    }
}

#[test]
fn find_exact_match_requires_the_whole_value() {
    let dir = TempDir::new("cli-exact");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = dir.path().to_str().expect("utf-8");

    let (code, _, _) = run_cli(&[
        "find",
        path,
        "--field",
        "Заголовок",
        "--value",
        "必然",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0);

    let (code, _, _) = run_cli(&[
        "find",
        path,
        "--field",
        "Заголовок",
        "--value",
        "必",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 4);

    let (code, _, _) = run_cli(&["find", path, "--field", "Заголовок", "--value", "必"]);
    assert_eq!(code, 0, "режим по умолчанию — contains");
}

#[test]
fn unknown_field_and_deck_are_reported() {
    let dir = TempDir::new("cli-unknown");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, _, stderr) = run_cli(&["find", &path, "--field", "НетТакого", "--value", "x"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_field"));

    let (code, _, stderr) = run_cli(&[
        "find",
        &path,
        "--field",
        "Заголовок",
        "--value",
        "偶然",
        "--deck",
        "Нет::Такой",
    ]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_deck"));
}

/// Закрытый читателем pipe не должен приводить ни к panic, ни к внутренней
/// ошибке: команда уже выполнилась, сохраняется её собственный exit code.
#[test]
fn closed_stdout_pipe_keeps_the_command_exit_code() {
    let dir = TempDir::new("closed-stdout");
    let long_value = "偶然".repeat(60);
    let notes: Vec<Value> = (0..500)
        .map(|index| {
            json!({
                "__type__": "Note",
                "guid": format!("guid-{index}"),
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": [long_value, "случайность", "偶然の一致"],
            })
        })
        .collect();
    let mut value = base_export();
    value["notes"] = Value::Array(notes);
    dir.write_export(&value);
    dir.write_media(&["a.mp3"]);

    // Вывод заведомо больше буфера pipe, поэтому запись не сможет завершиться
    // успешно после закрытия читающего конца.
    let export_dir = dir.path().to_string_lossy().into_owned();
    let mut child = Command::new(cli_binary())
        .args([
            "find",
            &export_dir,
            "--field",
            "Заголовок",
            "--value",
            "偶然",
            "--json",
            "--limit",
            "500",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("anki-repo должен запускаться");
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("ожидание процесса");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        !stderr.contains("panicked"),
        "процесс не должен паниковать: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "читающий конец закрыт, данных в stdout быть не может"
    );
}

/// Отказ записи вывода по другой причине — это внутренняя ошибка: `ENOSPC`
/// получается записью в `/dev/full`.
#[test]
#[cfg(target_os = "linux")]
fn unwritable_stdout_is_an_internal_error() {
    let dir = TempDir::new("unwritable-stdout");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);

    let export_dir = dir.path().to_string_lossy().into_owned();
    let full = File::options()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full должен открываться на запись");

    let output = Command::new(cli_binary())
        .args(["inspect", &export_dir, "--json"])
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .expect("anki-repo должен запускаться");
    assert_eq!(
        output.status.code(),
        Some(70),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Снятый флаг `--word` не должен вернуться незаметно: там, где критерий задаётся
/// парой `--field`/`--value`, clap обязан отклонить неизвестный аргумент
/// (exit 2, только stderr), а не молча его проигнорировать.
#[test]
fn removed_word_flag_is_rejected() {
    let dir = canonical_base_export("cli-no-word");
    let path = path_str(&dir);

    let cases: [&[&str]; 2] = [
        &["find", path.as_str(), "--word", "偶然"],
        // `--all` дополнительно закрывает обязательную группу критериев
        // `review`, чтобы ошибкой был именно неизвестный аргумент.
        &["review", path.as_str(), "--all", "--word", "偶然"],
    ];
    for args in cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_eq!(code, 2, "удалённый флаг должен отклоняться");
        assert!(stdout.is_empty(), "ошибка clap не должна писать в stdout");
        assert!(
            stderr.contains("--word"),
            "ошибка должна указывать на неизвестный флаг"
        );
    }
}

/// Удалённое правило `duplicate_primary_field` не должно вернуться в реестр QA:
/// `qa --code` принимает только коды реестра, а неизвестный код — доменная
/// ошибка `unknown_qa_code` (exit 3), а не молчаливо пустой результат.
#[test]
fn removed_qa_rule_code_is_unknown() {
    let dir = canonical_base_export("cli-no-primary-dup");
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["qa", &path, "--code", "duplicate_primary_field"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(stderr.contains("unknown_qa_code"), "stderr: {stderr}");

    let (code, stdout, stderr) =
        run_cli(&["--json", "qa", &path, "--code", "duplicate_primary_field"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["error"]["code"], json!("unknown_qa_code"));
    assert_eq!(
        value["error"]["details"]["unknown_codes"],
        json!(["duplicate_primary_field"])
    );
    // Реестр наружу — ровно `QA_CODES`, в порядке реестра.
    let available: Vec<Value> = QA_CODES.iter().map(|code| json!(code)).collect();
    assert_eq!(
        value["error"]["details"]["available_codes"],
        json!(available)
    );
}

/// Реестр QA состоит ровно из [`QA_CODES`]: каждый код принимается, фильтр
/// выбирает только своё правило, а число замечаний совпадает с независимым
/// пересчётом `raw_qa_counts` из того же сырого JSON.
#[test]
fn qa_accepts_exactly_the_registry_codes() {
    let raw = base_export();
    let dir = canonical_export("cli-qa-codes", &raw);
    let path = path_str(&dir);
    let expected = raw_qa_counts(&raw);

    for code in QA_CODES {
        let (exit, stdout, stderr) = run_cli(&["--json", "qa", &path, "--code", code]);
        assert_eq!(exit, 0, "код {code}: stderr {stderr}");
        let value = parse_json(&stdout);
        let result = &value["result"];
        assert_eq!(result["codes"], json!([code]), "код {code}");
        assert_eq!(
            result["by_code"].as_array().map(Vec::len),
            Some(1),
            "код {code}: фильтр обязан оставить одно правило"
        );
        assert_eq!(result["by_code"][0]["code"], json!(code), "код {code}");
        assert_eq!(
            result["findings_total"],
            json!(expected[code]),
            "код {code}: CLI и независимый пересчёт обязаны совпасть"
        );
    }
}

/// Читающие команды обязаны работать и на экспорте другой формы: другое число
/// узлов колоды, другое число моделей и полей. Ожидания вычисляются из самой
/// фикстуры `mixed_export()`, а не из состава `decks/`.
#[test]
fn read_commands_support_mixed_export() {
    let dir = TempDir::new("cli-mixed");
    let raw = mixed_export();
    dir.write_export(&raw);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["--json", "inspect", &path]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    assert_eq!(result["root_deck_name"], json!("Группа"));
    assert_eq!(result["deck_nodes"], json!(3));
    assert_eq!(result["notes_total"], json!(3));
    assert_eq!(result["note_models"].as_array().map(Vec::len), Some(2));

    // Поля модели перечисляются в порядке `ord`, а не в порядке объявления `flds`.
    let models = common::raw_models(&raw);
    let mut expected: Vec<(i64, String)> = models["model-out-of-order"]
        .iter()
        .map(|(name, ord)| (*ord, name.clone()))
        .collect();
    expected.sort();
    let expected: Vec<String> = expected.into_iter().map(|(_, name)| name).collect();
    let reported: Vec<&str> = result["note_models"][0]["fields"]
        .as_array()
        .expect("поля модели")
        .iter()
        .map(|field| field["name"].as_str().expect("имя поля"))
        .collect();
    assert_eq!(reported, expected);

    // Распределение разрешается по `ord`: `Гамма` объявлена первой в `flds`,
    // но её `ord` — 2, то есть позиция 2 в `fields`.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "stats",
        &path,
        "--group-by",
        "Гамма",
        "--top",
        "5",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let group = &value["result"]["group_by"];
    assert_eq!(group["notes_with_field"], json!(2));
    assert_eq!(group["notes_without_field"], json!(1));
    assert_eq!(group["distinct_values"], json!(2));
    assert_eq!(group["truncated"], json!(false));

    let (code, stdout, stderr) = run_cli(&["--json", "validate", &path]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["valid"], json!(true));

    let (code, stdout, stderr) =
        run_cli(&["--json", "qa", &path, "--code", "duplicate_note_content"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["notes_total"], json!(3));
    assert_eq!(value["result"]["findings_total"], json!(0));
}

/// Значение поля разрешается по `ord` модели, а не по позиции значения в
/// `fields`: у `model-out-of-order` значения намеренно «перепутаны»
/// относительно имён полей, поэтому ожидания берутся из сырого JSON.
#[test]
fn mixed_export_resolves_field_values_by_ord() {
    let dir = TempDir::new("cli-mixed-ord");
    let raw = mixed_export();
    dir.write_export(&raw);
    let path = path_str(&dir);

    let models = common::raw_models(&raw);
    let mut notes = Vec::new();
    common::collect_notes(&raw, &mut notes);
    let first = notes[0];

    let position = common::raw_field_position(&models, first, "Альфа");
    let expected = first["fields"][position]
        .as_str()
        .expect("значение поля — строка")
        .to_string();

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Альфа",
        "--value",
        &expected,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    // Значение `Альфа` одинаково у обеих заметок с этой моделью.
    assert_eq!(result["matched_total"], json!(2));
    assert_eq!(result["notes"][0]["guid"], json!("первая-1"));
    assert_eq!(result["notes"][0]["fields"]["Альфа"], json!(expected));

    // Того же значения под именем `Гамма` в экспорте нет: имя поля не
    // «угадывается» по позиции значения.
    let (code, _, stderr) = run_cli(&[
        "find",
        &path,
        "--field",
        "Гамма",
        "--value",
        &expected,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 4, "stderr: {stderr}");

    // Позиция 2 у `Гамма` — её значение есть только у первой заметки.
    let position = common::raw_field_position(&models, first, "Гамма");
    let gamma = first["fields"][position]
        .as_str()
        .expect("значение поля — строка")
        .to_string();
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Гамма",
        "--value",
        &gamma,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("первая-1"));

    // Ограничение по колоде не мешает разрешению поля по `ord`.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Альфа",
        "--value",
        &expected,
        "--deck",
        "Группа::Вторая",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("вторая-1"));

    // Модель с единственным полем ищется в том же экспорте.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Единственное поле",
        "--value",
        "одно поле",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("первая-2"));
}

#[test]
fn migration_cli_rejects_unproven_identity_before_reading_missing_store() {
    let dir = canonical_export("migration-identity", &base_export());
    let path = path_str(&dir);
    for apply in [false, true] {
        let mut args = vec![
            "migrate-media",
            &path,
            "--namespace",
            "pitch_accent",
            "--key",
            "word",
            "--from",
            "other.png",
            "--json",
        ];
        if apply {
            args.push("--apply");
        }
        let (code, stdout, stderr) = run_cli(&args);
        assert_eq!(code, 3, "{stderr}");
        let json = parse_json(&stdout);
        assert_eq!(json["command"], "migrate-media");
        assert_eq!(json["error"]["code"], "invalid_request");
        assert_eq!(
            json["error"]["details"]["reason"],
            "legacy_media_identity_unproven"
        );
    }
}

/// Изолированный Cargo workspace с подменённым `cargo` для проверок `maintenance`:
/// они не должны обходить реальный `target` рабочей копии, запускать настоящий
/// Cargo или зависеть от каталогов конкретной машины.
#[cfg(target_os = "linux")]
fn isolated_maintenance_workspace(fixture: &TempDir, bin: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\n\
         case \"$1\" in\n\
         --version) printf 'cargo 1.99.0\\n' ;;\n\
         metadata) printf '{\"workspace_root\":\"%s\",\"target_directory\":\"%s/target\"}\\n' \"$ANKI_MAINTENANCE_ROOT\" \"$ANKI_MAINTENANCE_ROOT\" ;;\n\
         *) printf 'Непредусмотренный вызов Cargo: %s\\n' \"$*\" >&2; exit 98 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    root.canonicalize().unwrap()
}

/// Загруженные активные units: либо без локальных файлов, либо с точными
/// управляемыми шаблонами и бинарником, но без конфигурации привязки.
#[cfg(target_os = "linux")]
fn unbound_timer_fixture(fixture: &TempDir, local_units: bool) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let base = fixture.path();
    for dir in ["home", "config/systemd/user", "state", "cache", "mock-bin"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    if local_units {
        const MANAGED: &str = "# Managed by repository-maintenance; do not edit by hand.";
        for (name, template) in [
            (
                "anki-repository-maintenance.service",
                include_str!(
                    "../../repository-maintenance/systemd/anki-repository-maintenance.service"
                )
                .replace("@CARGO_PATH_ENV@", "Environment=\"PATH=/bin\""),
            ),
            (
                "anki-repository-maintenance.timer",
                include_str!(
                    "../../repository-maintenance/systemd/anki-repository-maintenance.timer"
                )
                .to_owned(),
            ),
        ] {
            std::fs::write(
                base.join("config/systemd/user").join(name),
                format!("{MANAGED}\n{template}"),
            )
            .unwrap();
        }
        let binary = base.join("home/.local/bin/anki-repository-maintenance");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let systemctl = base.join("mock-bin/systemctl");
    std::fs::write(&systemctl, r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/systemctl-calls.log"
case "$2" in
show-environment) exit 0;;
is-enabled) echo enabled; exit 0;;
is-active) echo active; exit 0;;
show)
 echo LoadState=loaded
 if [ -f "$XDG_CONFIG_HOME/systemd/user/$3" ]; then
  echo "FragmentPath=$XDG_CONFIG_HOME/systemd/user/$3"
 else echo "FragmentPath=/foreign/$3"; fi
 if [ "$3" = anki-repository-maintenance.timer ]; then echo ActiveState=active; else echo ActiveState=inactive; fi
 echo DropInPaths=
 echo NeedDaemonReload=no
 echo Result=success
 binary="$HOME/.local/bin/anki-repository-maintenance"
 echo "ExecStart={ path=$binary ; argv[]=$binary clean --apply --installed-root ; }"
 exit 0;;
*) echo "Непредусмотренный вызов systemctl: $*" >&2; exit 97;;
esac
"#).unwrap();
    std::fs::set_permissions(systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    isolated_maintenance_workspace(fixture, &base.join("mock-bin"))
}

#[cfg(target_os = "linux")]
fn assert_unbound_timer_is_preserved(local_units: bool, reason: &str) {
    let fixture = TempDir::new("maintenance-disable-unbound");
    let root = unbound_timer_fixture(&fixture, local_units);
    let base = fixture.path();
    let mut command = Command::new(cli_binary());
    command
        .args(["--json", "maintenance", "--workspace-root"])
        .arg(&root)
        .arg("disable")
        .env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_STATE_HOME", base.join("state"))
        .env("XDG_CACHE_HOME", base.join("cache"))
        .env("ANKI_MAINTENANCE_ROOT", &root)
        .env("PATH", base.join("mock-bin"));
    let result = command.output().unwrap();
    assert_eq!(result.status.code(), Some(8));
    let parsed = parse_json(&String::from_utf8(result.stdout).unwrap());
    assert_eq!(parsed["error"]["code"], "write_failed");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains(reason),
        "{parsed}"
    );
    assert!(!base.join("config/anki-decks/gc-preference.json").exists());
    let calls = std::fs::read_to_string(base.join("home/systemctl-calls.log")).unwrap();
    assert!(calls.contains("--user is-enabled anki-repository-maintenance.timer"));
    assert!(calls.contains("--user is-active anki-repository-maintenance.timer"));
    assert!(!calls.contains("--user disable") && !calls.contains("--user stop"));
    for name in [
        "anki-repository-maintenance.service",
        "anki-repository-maintenance.timer",
    ] {
        assert_eq!(
            base.join("config/systemd/user").join(name).exists(),
            local_units
        );
    }
}

/// Активная загруженная установка без файлов на диске не подтверждает владение.
#[cfg(target_os = "linux")]
#[test]
fn maintenance_disable_reports_failure_when_binding_cannot_be_proven() {
    assert_unbound_timer_is_preserved(
        false,
        "FragmentPath=/foreign/anki-repository-maintenance.timer",
    );
}

/// Правильные управляемые units без конфигурации checkout не дают права снять их.
#[cfg(target_os = "linux")]
#[test]
fn maintenance_disable_rejects_owner_marked_units_without_binding() {
    assert_unbound_timer_is_preserved(
        true,
        "принадлежность таймера рабочей области не подтверждена",
    );
}

/// Владение экспортом определяется его расположением: корень рабочей области не
/// зависит от текущего каталога процесса.
#[test]
fn export_repository_root_follows_the_export_not_the_process_directory() {
    let fixture = TempDir::new("export-repository-root");
    let repository = fixture.path().join("checkout");
    let export = repository.join("decks/demo");
    std::fs::create_dir_all(&export).unwrap();
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    std::fs::write(repository.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    assert_eq!(
        anki_repo::run::export_repository_root(&export),
        Some(repository.canonicalize().unwrap())
    );

    let detached = fixture.path().join("detached");
    std::fs::create_dir_all(&detached).unwrap();
    assert_eq!(
        anki_repo::run::export_repository_root(&detached),
        None,
        "экспорт вне репозитория не даёт корня"
    );
}

/// Предложение обслуживания измеряет репозиторий экспорта, а не тот каталог, из
/// которого запущен процесс: иначе согласие установило бы таймер, ежедневно
/// очищающий `target` постороннего проекта.
#[cfg(target_os = "linux")]
#[test]
fn gc_onboarding_offer_uses_the_export_repository_not_the_process_directory() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new("gc-onboarding-export-root");
    let bin = fixture.path().join("mock-bin");
    let home = fixture.path().join("home");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let cache = fixture.path().join("cache");
    for dir in [&bin, &home, &config, &state, &cache] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         '--user show-environment') exit 0 ;;\n\
         '--user is-enabled anki-repository-maintenance.timer') echo disabled; exit 1 ;;\n\
         '--user is-active anki-repository-maintenance.timer') echo inactive; exit 3 ;;\n\
         '--user show '* ) printf '%s\\n' LoadState=not-found ActiveState=inactive FragmentPath= DropInPaths= NeedDaemonReload=no Result=success; exit 0 ;;\n\
         'is-active systemd-tmpfiles-clean.timer') exit 1 ;;\n\
         *) echo \"Непредусмотренный вызов systemctl: $*\" >&2; exit 97 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let repository = isolated_maintenance_workspace(&fixture, &bin);
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    let export = repository.join("decks/demo");
    std::fs::create_dir_all(&export).unwrap();
    std::fs::write(
        export.join("deck.json"),
        serde_json::to_string_pretty(&base_export()).unwrap(),
    )
    .unwrap();

    let sandbox_path = std::env::join_paths(std::iter::once(bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut child = Command::new("script")
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ANKI_GC_PTY_CLI\" models \"$ANKI_GC_PTY_EXPORT\"",
            "/dev/null",
        ])
        // Каталог процесса лежит вне любой рабочей области Cargo: корень можно
        // взять только из расположения экспорта.
        .current_dir(fixture.path())
        .env("ANKI_GC_PTY_CLI", cli_binary())
        .env("ANKI_GC_PTY_EXPORT", &export)
        .env_remove("ANKI_REPOSITORY_ROOT")
        .env("ANKI_MAINTENANCE_ROOT", &repository)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CACHE_HOME", &cache)
        .env("PATH", &sandbox_path)
        .env_remove("CI")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("для живого Linux PTY-теста нужен util-linux script");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"\x04").unwrap();
    drop(stdin);
    let result = child.wait_with_output().unwrap();
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.status.success(), "{output}");
    assert!(
        output.contains("Включить автоматическую очистку? [y/N]"),
        "предложение обязано найти рабочую область экспорта: {output}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn maintenance_status_and_decline_keep_json_pure_with_piped_streams() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new("maintenance-contract");
    let home = fixture.path().join("home");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let bin = fixture.path().join("mock-bin");
    for dir in [&home, &config, &state, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // Таймер не установлен: проверяется чистый отказ, а не состояние таймера
    // машины, на которой выполняется тест.
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         '--user show-environment') exit 0 ;;\n\
         '--user is-enabled anki-repository-maintenance.timer') echo disabled; exit 1 ;;\n\
         '--user is-active anki-repository-maintenance.timer') echo inactive; exit 3 ;;\n\
         '--user show '* ) printf '%s\\n' LoadState=not-found ActiveState=inactive FragmentPath= DropInPaths= NeedDaemonReload=no Result=success; exit 0 ;;\n\
         'is-active systemd-tmpfiles-clean.timer') exit 1 ;;\n\
         *) echo \"Непредусмотренный вызов systemctl: $*\" >&2; exit 97 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sandbox_path = std::env::join_paths(std::iter::once(bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let repository = isolated_maintenance_workspace(&fixture, &bin);
    let cache = fixture.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let root = repository.to_str().unwrap();

    let invoke = |action: &str| {
        Command::new(cli_binary())
            .args(["--json", "maintenance", "--workspace-root", root, action])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CACHE_HOME", &cache)
            .env("ANKI_MAINTENANCE_ROOT", &repository)
            .env("PATH", &sandbox_path)
            .output()
            .expect("CLI должен запускаться с перенаправленными потоками")
    };

    let before = invoke("status");
    assert!(
        before.status.success(),
        "{:?}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert!(!String::from_utf8_lossy(&before.stderr).contains("[y/N]"));
    let initial = parse_json(&String::from_utf8(before.stdout).unwrap());
    assert_eq!(initial["schema_version"], json!(1));
    assert_eq!(initial["command"], "maintenance");
    assert_eq!(initial["result"]["schema_version"], json!(1));
    assert_eq!(initial["result"]["preference"], "unset");
    assert!(initial["result"]["diagnostics"].is_array());
    assert_eq!(
        initial["result"]["workspace_root"],
        redacted_maintenance_path(&repository),
        "измеряться обязана изолированная рабочая область, а не рабочая копия репозитория"
    );

    let declined = invoke("disable");
    assert!(
        declined.status.success(),
        "{}",
        String::from_utf8_lossy(&declined.stderr)
    );
    let saved = parse_json(&String::from_utf8(declined.stdout).unwrap());
    assert_eq!(saved["command"], "maintenance");
    assert_eq!(saved["result"]["preference"], "no");
    assert!(!String::from_utf8_lossy(&declined.stderr).contains("[y/N]"));

    let repeated = invoke("status");
    assert!(repeated.status.success());
    let status = parse_json(&String::from_utf8(repeated.stdout).unwrap());
    assert_eq!(status["result"]["preference"], "no");
    assert!(config.join("anki-decks/gc-preference.json").is_file());
}

/// Linux live-acceptance: настоящая псевдотерминальная сессия, но полностью
/// отдельные пользовательские настройки и подменённый systemctl. Вызовы
/// настоящего пользовательского таймера при этом не выполняются.
#[cfg(target_os = "linux")]
#[test]
fn gc_onboarding_pty_decline_is_saved_and_prompt_does_not_repeat() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new("gc-onboarding-pty");
    fixture.write_export(&base_export());
    let isolated_home = fixture.path().join("home");
    let isolated_config = fixture.path().join("config");
    let isolated_state = fixture.path().join("state");
    let isolated_cache = fixture.path().join("cache");
    let mock_bin = fixture.path().join("mock-bin");
    for path in [
        &isolated_home,
        &isolated_config,
        &isolated_state,
        &isolated_cache,
        &mock_bin,
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let systemctl = mock_bin.join("systemctl");
    let mock_log = fixture.path().join("systemctl-calls.log");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$ANKI_GC_PTY_MOCK_LOG\"\n\
         case \"$*\" in\n\
         '--user show-environment') exit 0 ;;\n\
         '--user is-enabled anki-repository-maintenance.timer') echo disabled; exit 1 ;;\n\
         '--user is-active anki-repository-maintenance.timer') echo inactive; exit 3 ;;\n\
         '--user show '* ) printf '%s\\n' LoadState=not-found ActiveState=inactive FragmentPath= DropInPaths= NeedDaemonReload=no Result=success; exit 0 ;;\n\
         'is-active systemd-tmpfiles-clean.timer') exit 1 ;;\n\
         *) echo \"Непредусмотренный вызов systemctl: $*\" >&2; exit 97 ;;\n\
         esac\n",
    ).unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let sandbox_path = std::env::join_paths(
        std::iter::once(mock_bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();

    let repository = isolated_maintenance_workspace(&fixture, &mock_bin);

    let launch = |input: &[u8]| -> String {
        // util-linux script предоставляет все три TTY самому anki-repo.
        let mut child = Command::new("script")
            .args([
                "-q",
                "-e",
                "-c",
                "exec \"$ANKI_GC_PTY_CLI\" models \"$ANKI_GC_PTY_EXPORT\"",
                "/dev/null",
            ])
            .env("ANKI_GC_PTY_CLI", cli_binary())
            .env("ANKI_GC_PTY_EXPORT", fixture.path())
            .env("ANKI_GC_PTY_MOCK_LOG", &mock_log)
            .env("ANKI_REPOSITORY_ROOT", &repository)
            .env("ANKI_MAINTENANCE_ROOT", &repository)
            .env("HOME", &isolated_home)
            .env("XDG_CONFIG_HOME", &isolated_config)
            .env("XDG_STATE_HOME", &isolated_state)
            .env("XDG_CACHE_HOME", &isolated_cache)
            .env("PATH", &sandbox_path)
            // Тест проверяет интерактивный сценарий: признак CI снимается
            // явно, чтобы результат не зависел от окружения запуска.
            .env_remove("CI")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("для живого Linux PTY-теста нужен util-linux script");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).unwrap();
        drop(stdin);
        let result = child.wait_with_output().unwrap();
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.status.success(),
            "CLI в PTY завершился с ошибкой: {output}"
        );
        output
    };

    let preference = isolated_config.join("anki-decks/gc-preference.json");
    // Ctrl-D в начале строки — это EOF, то есть отсутствие ответа, а не отказ:
    // выбор сохраняться не должен, поэтому вопрос обязан появиться снова.
    let eof = launch(b"\x04");
    assert!(
        eof.contains("Включить автоматическую очистку? [y/N]"),
        "{eof}"
    );
    assert!(
        !preference.exists(),
        "пустой ввод на EOF не должен сохранять отказ: {eof}"
    );

    // Нераспознанный ответ — не отказ: выбор не сохраняется, вопрос задаётся снова.
    let unrecognized = launch(b"maybe\n");
    assert!(
        unrecognized.contains("Включить автоматическую очистку? [y/N]"),
        "{unrecognized}"
    );
    assert!(
        unrecognized.contains("Ответ не распознан"),
        "{unrecognized}"
    );
    assert!(
        !preference.exists(),
        "нераспознанный ответ не должен сохраняться как отказ: {unrecognized}"
    );

    let first = launch(b"n\n");
    assert!(
        first.contains("Включить автоматическую очистку? [y/N]"),
        "{first}"
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&preference).unwrap()).unwrap();
    assert_eq!(saved["choice"], "no");

    let second = launch(b"\n");
    assert!(
        !second.contains("Включить автоматическую очистку? [y/N]"),
        "после сохранённого отказа нельзя задавать вопрос повторно: {second}"
    );
    let saved_again: Value = serde_json::from_slice(&std::fs::read(&preference).unwrap()).unwrap();
    assert_eq!(saved_again["choice"], "no");

    let calls = std::fs::read_to_string(&mock_log).unwrap();
    assert!(calls.contains("--user show-environment"), "{calls}");
    assert!(
        calls.contains("--user is-enabled anki-repository-maintenance.timer"),
        "{calls}"
    );
    assert!(
        calls.contains("--user is-active anki-repository-maintenance.timer"),
        "{calls}"
    );
    assert!(
        calls.lines().all(|line| matches!(
            line,
            "--user show-environment"
                | "--user is-enabled anki-repository-maintenance.timer"
                | "--user is-active anki-repository-maintenance.timer"
                | "--user show anki-repository-maintenance.service --property=LoadState,FragmentPath,ExecStart,ActiveState,Result,DropInPaths,NeedDaemonReload"
                | "--user show anki-repository-maintenance.timer --property=LoadState,FragmentPath,ExecStart,ActiveState,Result,DropInPaths,NeedDaemonReload"
                | "is-active systemd-tmpfiles-clean.timer"
        )),
        "неожиданный systemctl-вызов: {calls}"
    );
    println!(
        "PTY-приёмка пройдена: первый [y/N], сохранённый N и отсутствие повтора; \
         настоящий systemctl не вызывался"
    );
}

/// Linux PTY-приёмка согласия: настоящая интерактивная команда, но фиктивный
/// systemd и отдельный HOME. Установка не затрагивает настоящий пользовательский timer.
#[cfg(target_os = "linux")]
#[test]
fn gc_onboarding_pty_consent_installs_and_disables_isolated_timer() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let fixture = TempDir::new("gc-onboarding-consent");
    fixture.write_export(&base_export());
    let root = fixture.path().join("workspace");
    let home = fixture.path().join("home");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let cache = fixture.path().join("cache");
    let bin = fixture.path().join("mock-bin");
    for dir in [&root, &home, &config, &state, &cache, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    let root = root.canonicalize().unwrap();

    let systemctl_log = fixture.path().join("systemctl-consent.log");
    let active_marker = fixture.path().join("fake-timer-active");
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$ANKI_GC_CONSENT_SYSTEMCTL_LOG"
case "$*" in
  '--user show-environment') exit 0 ;;
  '--user is-enabled anki-repository-maintenance.timer'|'--user is-active anki-repository-maintenance.timer')
    if test -f "$ANKI_GC_CONSENT_ACTIVE_MARKER"; then case "$2" in is-enabled) echo enabled;; *) echo active;; esac; else case "$2" in is-enabled) echo disabled; exit 1;; *) echo inactive; exit 3;; esac; fi ;;
  '--user show '*)
name="$3"
fragment="$XDG_CONFIG_HOME/systemd/user/$name"
if [ -f "$fragment" ]; then echo LoadState=loaded; echo "FragmentPath=$fragment";
else echo LoadState=not-found; echo FragmentPath=; fi
echo ActiveState=inactive
echo Result=success
echo DropInPaths=
echo NeedDaemonReload=no
execpath="$HOME/.local/bin/anki-repository-maintenance"
echo "ExecStart={ path=$execpath ; argv[]=$execpath clean --apply --installed-root ; }"
    ;;
  '--user stop anki-repository-maintenance.service') exit 0 ;;
  'is-active systemd-tmpfiles-clean.timer') exit 1 ;;
  '--user daemon-reload') exit 0 ;;
  '--user enable --now anki-repository-maintenance.timer')
    if [ -f "$ANKI_GC_CONSENT_ACTIVE_MARKER.fail" ]; then exit 9; fi
    : > "$ANKI_GC_CONSENT_ACTIVE_MARKER" ;;
  '--user status anki-repository-maintenance.timer --no-pager')
    if test -f "$ANKI_GC_CONSENT_ACTIVE_MARKER"; then case "$2" in is-enabled) echo enabled;; *) echo active;; esac; else case "$2" in is-enabled) echo disabled; exit 1;; *) echo inactive; exit 3;; esac; fi ;;
  '--user disable --now anki-repository-maintenance.timer')
    rm -f "$ANKI_GC_CONSENT_ACTIVE_MARKER" ;;
  *) printf 'Непредусмотренный вызов systemctl: %s\n' "$*" >&2; exit 97 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
case "$1" in
  --version) printf 'cargo 1.99.0\n' ;;
  metadata)
    printf '{"workspace_root":"%s","target_directory":"%s/target"}\n' \
      "$ANKI_REPOSITORY_ROOT" "$ANKI_REPOSITORY_ROOT" ;;
  *) printf 'Непредусмотренный вызов Cargo: %s\n' "$*" >&2; exit 98 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Если отдельный GC-бинарник не собран рядом с anki-repo, поиск в PATH
    // получит именно этот безвредный файл. Установщик лишь копирует его.
    let dummy_gc = bin.join("anki-repository-maintenance");
    std::fs::write(&dummy_gc, b"#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(&dummy_gc, std::fs::Permissions::from_mode(0o755)).unwrap();

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let isolated_path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();

    let configure = |command: &mut Command| {
        command
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CACHE_HOME", &cache)
            .env("ANKI_REPOSITORY_ROOT", &root)
            .env("ANKI_GC_CONSENT_SYSTEMCTL_LOG", &systemctl_log)
            .env("ANKI_GC_CONSENT_ACTIVE_MARKER", &active_marker)
            .env("PATH", &isolated_path)
            // Тест проверяет интерактивный сценарий: признак CI снимается
            // явно, чтобы результат не зависел от окружения запуска.
            .env_remove("CI");
    };
    let launch_pty = |input: &[u8]| -> String {
        let mut command = Command::new("script");
        command
            .args([
                "-q",
                "-e",
                "-c",
                "exec \"$ANKI_GC_CONSENT_CLI\" models \"$ANKI_GC_CONSENT_EXPORT\"",
                "/dev/null",
            ])
            .env("ANKI_GC_CONSENT_CLI", cli_binary())
            .env("ANKI_GC_CONSENT_EXPORT", fixture.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().expect("нужен util-linux script");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).unwrap();
        drop(stdin);
        let result = child.wait_with_output().unwrap();
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "PTY CLI: {output}");
        output
    };
    let invoke = |action: &str| -> Value {
        let mut command = Command::new(cli_binary());
        command.args([
            "--json",
            "maintenance",
            "--workspace-root",
            root.to_str().unwrap(),
            action,
        ]);
        configure(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            action,
            String::from_utf8_lossy(&output.stderr)
        );
        parse_json(&String::from_utf8(output.stdout).unwrap())
    };

    std::fs::create_dir_all(cache.join("anki-decks")).unwrap();
    let lock = repository_maintenance::guard::MaintenanceLock::acquire_at(
        &cache.join("anki-decks/maintenance-lock"),
    )
    .unwrap();
    let blocked = launch_pty(b"y\n");
    assert!(blocked.contains("Согласие не сохранено"), "{blocked}");
    assert!(!blocked.contains("Согласие сохранено"), "{blocked}");
    assert!(!config.join("anki-decks/gc-preference.json").exists());
    drop(lock);

    let failed_marker = active_marker.with_file_name("fake-timer-active.fail");
    std::fs::write(&failed_marker, "").unwrap();
    let failed = launch_pty(b"y\n");
    assert!(failed.contains("Не удалось включить GC"), "{failed}");
    assert!(failed.contains("Согласие сохранено"), "{failed}");
    assert!(!active_marker.exists());
    assert_eq!(invoke("status")["result"]["preference"], "yes");
    std::fs::remove_file(failed_marker).unwrap();
    let recovered = invoke("enable");
    assert_eq!(recovered["result"]["automatic"], "enabled");
    let first = failed;
    assert!(
        first.contains("Включить автоматическую очистку? [y/N]"),
        "{first}"
    );
    assert!(recovered["result"]["timer_active"] == true, "{first}");
    let choice_file = config.join("anki-decks/gc-preference.json");
    let choice: Value = serde_json::from_slice(&std::fs::read(&choice_file).unwrap()).unwrap();
    assert_eq!(choice["choice"], "yes");
    let unit_dir = config.join("systemd/user");
    let service = unit_dir.join("anki-repository-maintenance.service");
    let timer = unit_dir.join("anki-repository-maintenance.timer");
    let install_config = config.join("anki-decks/repository-maintenance-install.toml");
    let installed_binary = home.join(".local/bin/anki-repository-maintenance");
    for file in [&service, &timer, &install_config, &installed_binary] {
        assert!(file.is_file(), "не установлен файл {}", file.display());
    }
    assert!(active_marker.is_file());
    assert!(std::fs::read_to_string(&service).unwrap().contains(
        "ExecStart=%h/.local/bin/anki-repository-maintenance clean --apply --installed-root"
    ));
    assert!(
        std::fs::read_to_string(&install_config)
            .unwrap()
            .contains(&format!("repository_root = \"{}\"", root.display()))
    );
    let enabled = invoke("status");
    assert_eq!(enabled["result"]["preference"], "yes");
    assert_eq!(enabled["result"]["automatic"], "enabled");
    assert_eq!(
        enabled["result"]["installed_workspace"],
        redacted_maintenance_path(&root)
    );
    assert_eq!(enabled["result"]["timer_active"], true);

    let second = launch_pty(b"\n");
    assert!(
        !second.contains("Включить автоматическую очистку? [y/N]"),
        "повторное предложение при согласии: {second}"
    );

    let disabled = invoke("disable");
    assert_eq!(disabled["result"]["preference"], "no");
    assert_eq!(disabled["result"]["automatic"], "disabled");
    assert!(!active_marker.exists());
    for file in [&service, &timer, &install_config, &installed_binary] {
        assert!(
            !file.exists(),
            "после отключения остался {}",
            file.display()
        );
    }
    let declined = launch_pty(b"\n");
    assert!(
        !declined.contains("Включить автоматическую очистку? [y/N]"),
        "после отключения нельзя повторять приглашение: {declined}"
    );

    // Отдельно сохраняем приёмку успешного Y без предыдущей ошибки установки.
    std::fs::remove_file(&choice_file).unwrap();
    let accepted = launch_pty(b"y\n");
    assert!(
        accepted.contains("Автоочистка подтверждена: таймер установлен и активен."),
        "{accepted}"
    );
    assert_eq!(invoke("status")["result"]["preference"], "yes");
    assert_eq!(invoke("disable")["result"]["preference"], "no");

    let calls = std::fs::read_to_string(&systemctl_log).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "--user enable --now anki-repository-maintenance.timer")
            .count(),
        3
    );
    assert_eq!(
        calls
            .lines()
            .filter(|call| *call == "--user disable --now anki-repository-maintenance.timer")
            .count(),
        2
    );
    assert!(
        calls.lines().all(|line| matches!(
            line,
            "--user show-environment"
                | "--user is-enabled anki-repository-maintenance.timer"
                | "--user is-active anki-repository-maintenance.timer"
                | "--user show anki-repository-maintenance.service --property=LoadState,FragmentPath,ExecStart,ActiveState,Result,DropInPaths,NeedDaemonReload"
                | "--user show anki-repository-maintenance.timer --property=LoadState,FragmentPath,ExecStart,ActiveState,Result,DropInPaths,NeedDaemonReload"
                | "is-active systemd-tmpfiles-clean.timer"
                | "--user daemon-reload"
                | "--user enable --now anki-repository-maintenance.timer"
                | "--user status anki-repository-maintenance.timer --no-pager"
                | "--user disable --now anki-repository-maintenance.timer"
                | "--user stop anki-repository-maintenance.service"
        )),
        "непредусмотренный вызов systemctl: {calls}"
    );
    println!(
        "PTY-приёмка согласия пройдена: согласие Y, файлы и работающий фиктивный timer, \
         отсутствие повтора, disable и удаление фиктивной установки; \
         настоящий systemctl не вызывался"
    );
}

#[cfg(target_os = "linux")]
fn redacted_maintenance_path(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").unwrap().uid();
    path.to_string_lossy()
        .replace(&format!("anki-decks-{uid}"), "anki-decks-<uid>")
}

#[test]
fn maintenance_missing_workspace_returns_input_unreadable() {
    let fixture = TempDir::new("maintenance-input-contract");
    let output = Command::new(cli_binary())
        .args(["--json", "maintenance", "--workspace-root"])
        .arg(fixture.path().join("missing-checkout"))
        .arg("status")
        .env("HOME", fixture.path().join("home"))
        .env("XDG_CONFIG_HOME", fixture.path().join("config"))
        .env("XDG_STATE_HOME", fixture.path().join("state"))
        .env("XDG_CACHE_HOME", fixture.path().join("cache"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "input_unreadable");
    assert!(!fixture.path().join("config").exists());
    assert!(!fixture.path().join("state").exists());
}
