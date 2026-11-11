//! Контракт CLI без сети: коды возврата, JSON и подтверждение удаления.

mod common;

use common::*;
use japanese_lexicon::catalog;
use japanese_lexicon::manager::{self, Options};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(root: &Path, args: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_japanese-lexicon"))
        .arg("--data-dir")
        .arg(root)
        .arg("--json")
        .args(args)
        .env_remove("JAPANESE_LEXICON_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.code().unwrap(), json)
}

#[test]
fn remove_all_requires_explicit_confirmation_without_a_terminal() {
    let tmp = TempDir::new("cli-remove");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let sources: Vec<_> = catalog::standard().iter().collect();
    assert!(
        manager::install(&root, &sources, &up, None, &Options::default())
            .unwrap()
            .ok
    );

    let (code, json) = run(&root, &["remove", "--all"]);
    assert_eq!(code, 2);
    assert_eq!(json["code"], "confirmation_required");
    let (_, status) = run(&root, &["status"]);
    assert!(
        status["dictionaries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["state"] == "ok")
    );

    let (code, json) = run(&root, &["uninstall", "--all", "--yes"]);
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["items"].as_array().unwrap().len(), 4);
    assert!(
        json["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["outcome"] == "removed")
    );
}

#[test]
fn lookup_verify_and_errors_have_stable_json() {
    let tmp = TempDir::new("cli-lookup");
    let root = tmp.path().join("data");

    let (code, json) = run(&root, &["lookup", "--surface", "学校"]);
    assert_eq!(code, 0);
    assert_eq!(json["schema"], "japanese-lexicon.lookup/1");
    assert_eq!(json["status"], "source_missing");
    let (code, json) = run(&root, &["lookup", "--surface", "コーヒー"]);
    assert_eq!(code, 0);
    assert_eq!(json["status"], "single_reading");
    assert_eq!(json["readings"][0]["normalized"], "こーひー");

    let (code, json) = run(&root, &["install", "nonexistent"]);
    assert_eq!(code, 2);
    assert_eq!(json["code"], "unknown_dictionary");
    let (code, json) = run(&root, &["lookup", "--surface", "学 校"]);
    assert_eq!(code, 2);
    assert_eq!(json["code"], "invalid_input");

    let up = FakeUpstream::standard();
    let sources: Vec<_> = catalog::standard().iter().collect();
    manager::install(&root, &sources, &up, None, &Options::default()).unwrap();
    let (code, json) = run(&root, &["doctor"]);
    assert_eq!(code, 0);
    assert_eq!(json["ok"], true);
    std::fs::write(
        root.join("dictionaries/jmnedict")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("manifest.json"),
        b"{}",
    )
    .unwrap();
    let (code, json) = run(&root, &["verify", "jmnedict"]);
    assert_eq!(code, 1);
    assert_eq!(json["dictionaries"][0]["state"], "corrupt");
    // Поиск продолжает работать по остальным словарям и называет недоступный.
    let (code, json) = run(&root, &["lookup", "--surface", "学校"]);
    assert_eq!(code, 0);
    assert_eq!(json["status"], "single_reading");
    let jmnedict = json["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "jmnedict")
        .unwrap();
    assert_eq!(jmnedict["state"], "unavailable");
    assert_eq!(jmnedict["code"], "index_corrupt");

    // Текстовый вывод отличает повреждённый источник от неустановленного.
    let output = Command::new(env!("CARGO_BIN_EXE_japanese-lexicon"))
        .arg("--data-dir")
        .arg(&root)
        .args(["lookup", "--surface", "学校"])
        .env_remove("JAPANESE_LEXICON_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("нет источников: jmnedict"), "{text}");
    assert!(
        text.contains("jmnedict недоступен (index_corrupt)") && text.contains("verify jmnedict"),
        "{text}"
    );
}

fn run_text(root: &Path, args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_japanese-lexicon"))
        .arg("--data-dir")
        .arg(root)
        .args(args)
        .env_remove("JAPANESE_LEXICON_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// Вызов без добавления `--json` и чтение обоих потоков: нужен для ошибок
/// разбора аргументов, которые clap формирует до обработки приложения.
fn run_raw(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_japanese-lexicon"))
        .args(args)
        .env_remove("JAPANESE_LEXICON_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// Ни одной английской фразы clap: негативные сценарии CLI русифицированы.
fn assert_no_english_clap_text(text: &str) {
    for english in [
        "error:",
        "Usage:",
        "For more information",
        "unexpected argument",
        "unrecognized subcommand",
        "required arguments",
        "invalid value",
        "possible values",
        "Print help",
        "Print version",
    ] {
        assert!(!text.contains(english), "{english}: {text}");
    }
}

#[test]
fn argument_errors_are_russian_with_stable_exit_codes() {
    let tmp = TempDir::new("cli-args");
    let dir = tmp.path().join("data");
    let dir = dir.to_str().unwrap();

    // Неизвестный аргумент: код 2, пустой stdout, русский текст в stderr.
    let (code, out, err) = run_raw(&["--data-dir", dir, "--bogus"]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("ошибка аргументов"), "{err}");
    assert!(
        err.contains("неизвестный аргумент") && err.contains("--bogus"),
        "{err}"
    );
    assert!(
        err.contains("Использование:") && err.contains("--help"),
        "{err}"
    );
    assert_no_english_clap_text(&err);

    // Неизвестная команда.
    let (code, out, err) = run_raw(&["--data-dir", dir, "frobnicate"]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("неизвестная команда") && err.contains("frobnicate"),
        "{err}"
    );
    assert!(err.contains("Использование:"), "{err}");
    assert_no_english_clap_text(&err);

    // Пропущенная команда: перечисляются доступные, машинные имена без перевода.
    let (code, out, err) = run_raw(&["--data-dir", dir]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("не указана команда") && err.contains("lookup"),
        "{err}"
    );
    assert_no_english_clap_text(&err);

    // Пропущенный обязательный аргумент.
    let (code, out, err) = run_raw(&["--data-dir", dir, "lookup"]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("не указан обязательный аргумент") && err.contains("--surface"),
        "{err}"
    );
    assert!(err.contains("Использование:"), "{err}");
    assert_no_english_clap_text(&err);

    // Флаг без значения: имя флага остаётся машинным.
    let (code, _out, err) = run_raw(&["--data-dir", dir, "lookup", "--surface", "学校", "--limit"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("нужно значение") && err.contains("--limit"),
        "{err}"
    );
    assert_no_english_clap_text(&err);

    // Нечисловое значение числового аргумента.
    let (code, out, err) = run_raw(&[
        "--data-dir",
        dir,
        "lookup",
        "--surface",
        "学校",
        "--limit",
        "x",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("значение «x»") && err.contains("--limit") && err.contains("целое число"),
        "{err}"
    );
    assert_no_english_clap_text(&err);

    // Контракт при `--json`: ошибка разбора аргументов не получает нового
    // машинного кода, остаётся русский текст в stderr и код 2 без JSON.
    let (code, out, err) = run_raw(&[
        "--data-dir",
        dir,
        "--json",
        "lookup",
        "--surface",
        "学校",
        "--limit",
        "x",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("ошибка аргументов"), "{err}");
    assert_no_english_clap_text(&err);

    // Доменные ошибки по-прежнему дают машинный JSON и тот же код 2.
    let (code, json) = run(Path::new(dir), &["lookup", "--surface", "学 校"]);
    assert_eq!(code, 2, "{json}");
    assert_eq!(json["schema"], "japanese-lexicon.error/1");
    assert_eq!(json["code"], "invalid_input");

    // Справка и версия — не ошибки: stdout, код 0.
    for args in [vec!["--help"], vec!["-h"]] {
        let (code, out, err) = run_raw(&args);
        assert_eq!(code, 0, "{args:?}: {err}");
        assert!(
            out.contains("Использование:") && out.contains("Показать справку"),
            "{out}"
        );
        assert!(err.is_empty(), "{err}");
        assert_no_english_clap_text(&out);
    }
    let (code, out, err) = run_raw(&["lookup", "--help"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("Использование: japanese-lexicon lookup"),
        "{out}"
    );
    assert!(err.is_empty(), "{err}");
    assert_no_english_clap_text(&out);
    for args in [vec!["--version"], vec!["-V"]] {
        let (code, out, err) = run_raw(&args);
        assert_eq!(code, 0, "{args:?}: {err}");
        assert!(out.starts_with("japanese-lexicon "), "{out}");
        assert!(err.is_empty(), "{err}");
    }
}

#[test]
fn zero_limits_are_invalid_input() {
    let tmp = TempDir::new("cli-zero");
    let root = tmp.path().join("data");
    for flag in ["--limit", "--max-evidence"] {
        let (code, json) = run(&root, &["lookup", "--surface", "学校", flag, "0"]);
        assert_eq!(code, 2, "{flag}: {json}");
        assert_eq!(json["code"], "invalid_input", "{flag}");
        assert_eq!(json["schema"], "japanese-lexicon.error/1");
    }
    let (code, json) = run(
        &root,
        &[
            "lookup",
            "--surface",
            "学校",
            "--limit",
            "1",
            "--max-evidence",
            "1",
        ],
    );
    assert_eq!(code, 0, "{json}");
    assert_eq!(json["truncation"]["limit"], 1);
}

#[test]
fn text_output_is_russian_while_json_keeps_machine_names() {
    let tmp = TempDir::new("cli-text");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let sources: Vec<_> = catalog::standard().iter().collect();
    manager::install(&root, &sources, &up, None, &Options::default()).unwrap();

    let (code, text) = run_text(&root, &["status"]);
    assert_eq!(code, 0);
    assert!(text.contains("(задан --data-dir)"), "{text}");
    assert!(text.contains("[лексический словарь]"), "{text}");
    assert!(text.contains(": в порядке"), "{text}");
    assert!(text.contains("записи: статей "), "{text}");
    for machine in [
        "Lexicon",
        "Option",
        "terms ",
        "readings ",
        "term_meta ",
        ": ok",
    ] {
        assert!(!text.contains(machine), "{machine}: {text}");
    }
    let (_, json) = run(&root, &["status"]);
    assert_eq!(json["data_dir_origin"], "option");
    assert_eq!(json["dictionaries"][0]["state"], "ok");
    assert_eq!(json["dictionaries"][0]["role"], "lexicon");

    let (code, text) = run_text(&root, &["lookup", "--surface", "学校"]);
    assert_eq!(code, 0);
    assert!(text.starts_with("学校 — одно чтение"), "{text}");
    assert!(text.contains("статья"), "{text}");
    assert!(!text.contains("single_reading") && !text.contains("lexical_entry"));
    let (_, text) = run_text(&root, &["lookup", "--surface", "ｶﾒﾗ"]);
    assert!(text.contains("полуширинная кана"), "{text}");
    assert!(!text.contains("HalfwidthKana"), "{text}");

    let (code, text) = run_text(&root, &["remove", "jmnedict"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.starts_with("удаление — каталог данных"), "{text}");
    assert!(
        text.contains("jmnedict") && text.contains(" удалён: "),
        "{text}"
    );
    let (_, json) = run(&root, &["remove", "jmnedict"]);
    assert_eq!(json["operation"], "remove");
    assert_eq!(json["items"][0]["outcome"], "not_installed");

    let (code, text) = run_text(&root, &["--help"]);
    assert_eq!(code, 0);
    assert!(text.contains("Использование:") && text.contains("Показать справку"));
    assert!(!text.contains("Print help"), "{text}");
}

/// Текст словаря приходит из недоверенного ZIP: управляющая последовательность в
/// заголовке и ревизии не должна управлять терминалом в текстовом выводе.
#[test]
fn dictionary_text_is_escaped_in_text_output() {
    let tmp = TempDir::new("cli-escape");
    let root = tmp.path().join("data");
    let zip = tmp.path().join("Jitendex.zip");
    std::fs::write(&zip, jitendex("\u{1b}[31mPWN", &[])).unwrap();

    let (code, text) = run_text(
        &root,
        &["install", "jitendex", "--from-file", zip.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "управляющая последовательность осталась в выводе: {text:?}"
    );
    assert!(text.contains("\\u{001B}[31mPWN"), "{text}");

    // Машинный JSON хранит исходное значение: экранирование — свойство текста.
    let (_, json) = run(&root, &["status"]);
    assert!(
        json["dictionaries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["installed"]["revision"] == "\u{1b}[31mPWN"),
        "{json}"
    );
}

/// Документация обещает лицензию и атрибуцию каждого источника: текстовый
/// `status` показывает и то, и другое, включая ограничения распространения.
#[test]
fn text_status_shows_license_attribution_and_redistribution_note() {
    let tmp = TempDir::new("cli-attribution");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let sources: Vec<_> = catalog::standard().iter().collect();
    assert!(
        manager::install(&root, &sources, &up, None, &Options::default())
            .unwrap()
            .ok
    );

    let (code, text) = run_text(&root, &["status"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(text.matches("  лицензия: ").count(), 4, "{text}");
    assert_eq!(text.matches("  атрибуция: ").count(), 4, "{text}");
    assert_eq!(
        text.matches("  ограничения распространения: ").count(),
        4,
        "{text}"
    );
}

/// Подсказка о продолжении нужна только тогда, когда за выборкой остались ещё
/// группы: скрытые свидетельства открываются `--max-evidence`.
#[test]
fn truncation_hint_matches_what_is_hidden() {
    let tmp = TempDir::new("cli-truncation");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let sources: Vec<_> = catalog::standard().iter().collect();
    assert!(
        manager::install(&root, &sources, &up, None, &Options::default())
            .unwrap()
            .ok
    );

    // Скрыты только свидетельства: у 生 две статьи одного чтения.
    let (code, text) = run_text(&root, &["lookup", "--surface", "生", "--max-evidence", "1"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("показаны все"), "{text}");
    assert!(!text.contains("--offset/--limit"), "{text}");
    assert!(text.contains("--max-evidence"), "{text}");

    // Скрыты группы: пагинация уместна.
    let (code, text) = run_text(&root, &["lookup", "--surface", "生", "--limit", "1"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("--offset/--limit"), "{text}");
}
