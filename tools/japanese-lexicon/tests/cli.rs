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
