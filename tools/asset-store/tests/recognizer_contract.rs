//! Контракт CLI `pronunciation-assets recognizer` на настоящих MP3 и
//! поддельном воркере распознавания с настоящим протоколом: манифест целей,
//! пары SHA × цель, прерывание и возобновление, сбои среды не кэшируются и
//! повторяются, кэш результатов переиспользуется без вызова моделей, отчёт и
//! повторная проверка входа. Настоящий инференс проверяют тесты
//! `tools/speech-recognize`, решения распознавания — модульные тесты.
use asset_store::hashing::sha256_hex;
use asset_store::temp_workspace::TempWorkspace;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pronunciation-assets");
const PRIMARY: (&str, &str) = (
    "hubert-base-phoneme-ctc-v4",
    "d5ef589b9317003f322d43c13d9b561a3f8cbfde119fae58231ae27c1ebeb9fc",
);
const VERIFIER: (&str, &str) = (
    "xlsr-japanese-hiragana-ctc",
    "a7c9b13a4724b8e4933d97ca67991f0d588e26c87ea4839764cea052c8872924",
);

/// Поддельный `speech-recognize-ctc`: идентичность закреплённых моделей,
/// эмиссии — нули правильной геометрии с честными SHA-256. Каждый вызов
/// записывается в журнал; файл `fail` в каталоге состояния роняет `process`.
const WORKER: &str = r#"#!/bin/bash
STATE="__STATE__"
echo "$1" >> "$STATE/calls"
if [ -e "$STATE/fail" ] && [ "$1" = process ]; then
  echo '{"schema_version":1,"kind":"speech_recognize_error","code":"runtime","message":"поддельный сбой"}'
  exit 5
fi
cmd=$1; shift
model=""; inputs=(); outputs=()
while [ $# -gt 0 ]; do
  case "$1" in
    --model) model=$2; shift 2 ;;
    --model-dir) shift 2 ;;
    --input) inputs+=("$2"); shift 2 ;;
    --output) outputs+=("$2"); shift 2 ;;
    *) shift ;;
  esac
done
if [ "$model" = hubert-base-phoneme-ctc-v4 ]; then
  out=phoneme; rev=f5fe07043bcb0b77a86faf72ac6d8fc1ae558f99; man=d5ef589b9317003f322d43c13d9b561a3f8cbfde119fae58231ae27c1ebeb9fc
  vocab='"PAD","UNK","SOS","EOS","a","i","u","e","o","I","U","k","g","s","z","t","d","n","h","b","p","m","y","r","w","f","j","v","N","cl","sh","ch","ts","ky","gy","hy","by","py","my","ny","ry","fy","ty","dy","kw","gw","pau","sil"'; n=48
else
  out=hiragana; rev=017225bb128a6b1c6de9d58391f891908d69bc7b; man=a7c9b13a4724b8e4933d97ca67991f0d588e26c87ea4839764cea052c8872924
  vocab='"[PAD]"'; for c in ぁ あ ぃ い ぅ う ぇ え ぉ お か が き ぎ く ぐ け げ こ ご さ ざ し じ す ず せ ぜ そ ぞ た だ ち ぢ っ つ づ て で と ど な に ぬ ね の は ば ぱ ひ び ぴ ふ ぶ ぷ へ べ ぺ ほ ぼ ぽ ま み む め も ゃ や ゅ ゆ ょ よ ら り る れ ろ ゎ わ ゐ ゑ を ん ゔ ー; do vocab="$vocab,\"$c\""; done; n=86
fi
worker='"worker":{"name":"speech-recognize-ctc","version":"0.0.0-fake","protocol":1}'
runtime='"runtime":{"inference":"fake","device":"cpu","threads":1}'
if [ "$cmd" = identify ]; then
  echo "{\"schema_version\":1,\"kind\":\"speech_recognize_identity\",$worker,$runtime,\"model\":{\"id\":\"$model\",\"output\":\"$out\",\"repository\":\"fake\",\"revision\":\"$rev\",\"license\":\"test\",\"manifest_sha256\":\"$man\",\"sample_rate\":16000,\"frame_stride\":320,\"receptive_field\":400,\"lead_pad_samples\":0,\"trail_pad_samples\":0,\"blank_id\":0,\"vocab\":[$vocab]}}"
  exit 0
fi
items=""
for i in "${!inputs[@]}"; do
  size=$(stat -c %s "${inputs[$i]}"); len=$((size / 4))
  for ks in "10 5" "3 2" "3 2" "3 2" "3 2" "2 2" "2 2"; do
    set -- $ks; if [ $len -lt $1 ]; then len=0; else len=$(( (len - $1) / $2 + 1 )); fi
  done
  head -c $((len * n * 4)) /dev/zero > "${outputs[$i]}"
  isha=$(sha256sum "${inputs[$i]}" | cut -d' ' -f1); osha=$(sha256sum "${outputs[$i]}" | cut -d' ' -f1)
  [ -n "$items" ] && items="$items,"
  items="$items{\"input_samples\":$((size / 4)),\"input_sha256\":\"$isha\",\"frames\":$len,\"vocab\":$n,\"output_sha256\":\"$osha\"}"
done
echo "{\"schema_version\":1,\"kind\":\"speech_recognize_result\",$worker,$runtime,\"model_id\":\"$model\",\"model_manifest_sha256\":\"$man\",\"items\":[$items],\"load_ms\":0,\"wall_ms\":0}"
"#;

struct Fixture {
    _tmp: TempWorkspace,
    root: PathBuf,
    worker: PathBuf,
    state: PathBuf,
    models: PathBuf,
    input: PathBuf,
    workspace: PathBuf,
}

fn ffmpeg(args: &[&str], out: &Path) {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .arg(out)
        .output()
        .expect("для тестов нужен FFmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Fixture {
    let tmp = TempWorkspace::create("recognizer-contract").unwrap();
    let root = tmp.path().to_path_buf();
    let state = root.join("state");
    let input = root.join("input");
    let models = root.join("models");
    for dir in [&state, &input] {
        fs::create_dir_all(dir).unwrap();
    }
    for (id, sha) in [PRIMARY, VERIFIER] {
        fs::create_dir_all(models.join(id).join(sha)).unwrap();
    }
    let worker = root.join("speech-recognize-ctc");
    fs::write(
        &worker,
        WORKER.replace("__STATE__", state.to_str().unwrap()),
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "aevalsrc='0.25*sin(PI*t/0.8)*sin(2*PI*(180+400*t)*t)':duration=0.8:sample_rate=44100,adelay=300:all=1,apad=pad_dur=0.3",
            "-ac",
            "1",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ],
        &input.join("生.mp3"),
    );
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono:d=1.2",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ],
        &input.join("silence.mp3"),
    );
    // Тот же звук под другим именем: один SHA, одна обработка.
    fs::copy(input.join("生.mp3"), input.join("copy.mp3")).unwrap();
    Fixture {
        workspace: root.join("workspace"),
        _tmp: tmp,
        root,
        worker,
        state,
        models,
        input,
    }
}

fn cli(f: &Fixture, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("recognizer")
        .args(args)
        .current_dir(&f.root)
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn calls(f: &Fixture) -> Vec<String> {
    fs::read_to_string(f.state.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn models_args(f: &Fixture) -> Vec<String> {
    vec![
        "--recognizer-bin".into(),
        f.worker.to_str().unwrap().into(),
        "--models-root".into(),
        f.models.to_str().unwrap().into(),
    ]
}

fn run(f: &Fixture, action: &str, extra: &[&str]) -> Output {
    let mut args: Vec<String> = vec![
        action.into(),
        "--batch-id".into(),
        "b1".into(),
        "--workspace".into(),
        f.workspace.to_str().unwrap().into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cli(f, &refs)
}

fn manifest(f: &Fixture) -> PathBuf {
    let path = f.root.join("targets.json");
    let value = serde_json::json!({
        "schema_version": 1,
        "kind": "pronunciation_target_manifest",
        "provenance": "test-plan",
        "entries": [
            {"id": "nama", "path": "生.mp3", "targets": [
                {"surface": "生", "reading": "なま", "competing_readings": ["せい"]},
                {"surface": "生", "reading": "せい"}
            ]},
            {"id": "missing", "path": "нет-такого.mp3", "targets": [{"surface": "本", "reading": "ほん"}]}
        ]
    });
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

#[test]
fn batch_resumes_retries_infrastructure_failures_and_reuses_cache() {
    let f = fixture();
    let targets = manifest(&f);
    let mut start: Vec<String> = vec![
        "--input".into(),
        f.input.to_str().unwrap().into(),
        "--targets".into(),
        targets.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
        "--max-items".into(),
        "1".into(),
    ];
    start.extend(models_args(&f));
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    let started = run(&f, "batch-start", &refs);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );
    let progress = &json(&started)["result"]["run"]["batch"]["progress"];
    // Два уникальных SHA; у SHA с целями — две пары, у тишины — свободный анализ.
    assert_eq!(progress["unique_inputs"], 2);
    assert_eq!(progress["pairs"], 3);
    assert_eq!(progress["pairs_with_target"], 2);
    assert_eq!(
        progress["unresolved_manifest_entries"],
        serde_json::json!(["missing"])
    );
    assert_eq!(json(&started)["result"]["run"]["outcome"], "paused");
    // Готова одна из двух пар входа с целями: вход ещё не завершён.
    assert_eq!(progress["completed_pairs"], 1);
    assert_eq!(progress["completed_inputs"], 0);

    // Сбой воркера: пары остаются незавершёнными, результат не кэшируется.
    fs::write(f.state.join("fail"), b"").unwrap();
    let resume_args = models_args(&f);
    let resume_refs: Vec<&str> = resume_args.iter().map(String::as_str).collect();
    let failed = run(&f, "batch-resume", &resume_refs);
    let failed_json = json(&failed);
    let completed_before =
        json(&run(&f, "batch-status", &[]))["result"]["progress"]["completed_pairs"].clone();
    assert_eq!(completed_before, 1, "{failed_json}");
    assert!(
        !failed.status.success()
            || failed_json["result"]["run"]["infrastructure_failed_now"].as_u64() > Some(0),
        "{failed_json}"
    );

    // Среда восстановлена: всё доделывается.
    fs::remove_file(f.state.join("fail")).unwrap();
    let done = run(&f, "batch-resume", &resume_refs);
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stdout)
    );
    let done = json(&done);
    assert_eq!(done["result"]["run"]["outcome"], "completed", "{done}");
    assert_eq!(
        done["result"]["run"]["batch"]["progress"]["completed_pairs"],
        3
    );
    assert_eq!(
        done["result"]["run"]["batch"]["progress"]["completed_inputs"],
        2
    );

    // Повтор: всё из кэша, модели не вызываются.
    let before = calls(&f).iter().filter(|c| *c == "process").count();
    let again = json(&run(&f, "batch-resume", &resume_refs));
    assert_eq!(again["result"]["run"]["processed_now"], 0, "{again}");
    assert_eq!(again["result"]["run"]["reused"], 3, "{again}");
    assert_eq!(calls(&f).iter().filter(|c| *c == "process").count(), before);

    // Отчёт: каждая пара — строка, исходники не изменены.
    let report_dir = f.root.join("report");
    let report = run(&f, "batch-report", &["--out", report_dir.to_str().unwrap()]);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stdout)
    );
    let items = fs::read_to_string(report_dir.join("items.jsonl")).unwrap();
    assert_eq!(items.lines().count(), 3);
    for line in items.lines() {
        let item: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(item["outcome"].is_string(), "{item}");
        assert!(item["next_action"].is_string(), "{item}");
    }
    assert!(report_dir.join("summary.json").is_file());
    assert!(report_dir.join("items.tsv").is_file());
    assert!(report_dir.join("occurrences.tsv").is_file());
    // Повторный отчёт в существующий каталог не перезаписывает его.
    assert!(
        !run(&f, "batch-report", &["--out", report_dir.to_str().unwrap()])
            .status
            .success()
    );
    let verified = run(&f, "batch-verify-sources", &[]);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
}

#[test]
fn invalid_manifest_target_is_reported_with_its_entry() {
    let f = fixture();
    let path = f.root.join("bad-targets.json");
    let value = serde_json::json!({
        "schema_version": 1,
        "kind": "pronunciation_target_manifest",
        "provenance": "test-plan",
        "entries": [
            {"id": "ok", "path": "生.mp3", "targets": [{"surface": "生", "reading": "なま"}]},
            {"id": "bad", "path": "生.mp3", "targets": [
                {"surface": "生", "reading": "せい"},
                {"surface": "生", "reading": "しょう", "competing_readings": ["ｾｲ"]}
            ]}
        ]
    });
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let mut start: Vec<String> = vec![
        "--input".into(),
        f.input.to_str().unwrap().into(),
        "--targets".into(),
        path.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
    ];
    start.extend(models_args(&f));
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    let refused = run(&f, "batch-start", &refs);
    assert_eq!(refused.status.code(), Some(3));
    let error = &json(&refused)["error"];
    assert_eq!(error["code"], "invalid_identity", "{error}");
    let details = &error["details"];
    assert_eq!(details["entry_id"], "bad", "{error}");
    assert_eq!(details["path"], "生.mp3");
    assert_eq!(details["target_index"], 1);
    assert_eq!(details["reading"], "しょう");
    assert_eq!(details["competing_readings"], serde_json::json!(["ｾｲ"]));
    let leftovers = fs::read_dir(&f.workspace)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "отказ до создания пакета");
}

#[test]
fn changed_worker_is_refused_on_resume() {
    let f = fixture();
    let mut start: Vec<String> = vec![
        "--input".into(),
        f.input.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
        "--max-items".into(),
        "1".into(),
    ];
    start.extend(models_args(&f));
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    assert!(run(&f, "batch-start", &refs).status.success());
    // Другой воркер — другой отпечаток моделей: продолжать с чужим кэшем нельзя.
    let mut script = fs::read_to_string(&f.worker).unwrap();
    script.push_str("# изменён\n");
    fs::write(&f.worker, script).unwrap();
    let args = models_args(&f);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let refused = run(&f, "batch-resume", &refs);
    assert!(!refused.status.success());
    assert_eq!(
        json(&refused)["error"]["details"]["blocker"],
        "recognizer_backend_changed"
    );
}

#[test]
fn non_phonetic_reading_is_not_matched_and_missing_worker_fails_early() {
    let f = fixture();
    let file = f.input.join("生.mp3");
    let mut args: Vec<String> = vec![
        "analyze".into(),
        "--file".into(),
        file.to_str().unwrap().into(),
        "--surface".into(),
        "生".into(),
        "--reading".into(),
        "生".into(),
        "--enhancer".into(),
        "none".into(),
    ];
    args.extend(models_args(&f));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Чтение не каной: доказуемой цели нет — только свободный анализ, без
    // сопоставления и без кандидата, но и без ожидания пользователя.
    let out = cli(&f, &refs);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let result = &json(&out)["result"]["result"];
    assert_eq!(result["outcome"], "TARGET_IDENTITY_UNAVAILABLE", "{result}");
    assert!(result["matching"].is_null());
    assert!(result["output"].is_null());
    let codes: Vec<&str> = result["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["code"].as_str())
        .collect();
    assert!(codes.contains(&"target_reading_not_phonetic"), "{codes:?}");

    let missing = cli(
        &f,
        &[
            "model-identify",
            "--recognizer-bin",
            f.root.join("нет").to_str().unwrap(),
            "--enhancer",
            "none",
        ],
    );
    assert_eq!(missing.status.code(), Some(5));
    assert_eq!(
        json(&missing)["error"]["details"]["blocker"],
        "recognizer_backend_unavailable"
    );
}

#[test]
fn analyze_without_target_records_free_analysis_only() {
    let f = fixture();
    let file = f.input.join("生.mp3");
    let mut args: Vec<String> = vec![
        "analyze".into(),
        "--file".into(),
        file.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
    ];
    args.extend(models_args(&f));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = cli(&f, &refs);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let result = &json(&out)["result"]["result"];
    assert_eq!(result["outcome"], "TARGET_IDENTITY_UNAVAILABLE", "{result}");
    assert!(result["matching"].is_null());
    assert!(result["pre_analysis"].is_object());

    // Конкурирующее чтение без цели не отбрасывается молча, а отвергается.
    args.extend(["--competing-reading".into(), "せい".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let refused = cli(&f, &refs);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--surface"));
}

/// Разметка рядом с копиями входов (каталог отчёта — вне её каталога).
fn labels(f: &Fixture) -> PathBuf {
    let dir = f.root.join("labels");
    fs::create_dir_all(&dir).unwrap();
    for name in ["生.mp3", "silence.mp3"] {
        fs::copy(f.input.join(name), dir.join(name)).unwrap();
    }
    let value = serde_json::json!({
        "schema_version": 1,
        "kind": "pronunciation_recognition_labels",
        "license": "test",
        "provenance": "synthetic chirp",
        "cases": [
            {"id": "nama", "file": "生.mp3", "category": "single_target",
             "target": {"surface": "生", "reading": "なま", "competing_readings": ["せい"]},
             "words": [{"reading": "なま", "start_ms": 300.0, "end_ms": 1100.0}]},
            {"id": "absent", "file": "生.mp3", "category": "target_absent",
             "target": {"surface": "本", "reading": "ほん"},
             "words": [{"reading": "なま", "start_ms": 300.0, "end_ms": 1100.0}]},
            {"id": "quiet", "file": "silence.mp3", "category": "no_speech",
             "target": null, "words": []}
        ]
    });
    let path = dir.join("labels.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

fn eval_args(f: &Fixture, labels: &Path, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "eval".into(),
        "--labels".into(),
        labels.to_str().unwrap().into(),
        "--out".into(),
        out.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
        "--jobs".into(),
        "2".into(),
    ];
    args.extend(models_args(f));
    args
}

#[test]
fn eval_accounts_for_every_labelled_case_and_writes_the_report() {
    let f = fixture();
    let labels = labels(&f);
    let out = f.root.join("eval");
    let args = eval_args(&f, &labels, &out);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = cli(&f, &refs);
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stdout)
    );
    let summary = &json(&done)["result"];
    // Каждая запись разметки оценена или учтена как сбой среды.
    assert_eq!(summary["labelled_cases"], 3, "{summary}");
    assert_eq!(summary["cases"], 3);
    assert_eq!(summary["infrastructure_failures"], 0);
    assert_eq!(summary["with_target"], 2);
    assert_eq!(summary["target_present"], 1);
    // Поддельные эмиссии ничего не слышат: принятий и ложных принятий нет,
    // цель с произнесённым словом — пропуск.
    assert_eq!(summary["accepted"], 0);
    assert_eq!(summary["false_positives"], 0);
    assert_eq!(summary["missed"], 1);
    assert!(summary["precision"].is_null());
    assert_eq!(summary["recall"], 0.0);
    assert_eq!(summary["delivered_boundaries_unresolved"], 0);
    assert_eq!(summary["extraction_cuts_target"], 0);
    assert_eq!(summary["confidence"], "uncalibrated");
    let by_outcome = summary["by_outcome"].as_object().unwrap();
    assert_eq!(
        by_outcome
            .values()
            .map(|v| v.as_u64().unwrap())
            .sum::<u64>(),
        3
    );
    assert_eq!(
        summary["legacy_balanced"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_u64().unwrap())
            .sum::<u64>(),
        3
    );
    // Файлы отчёта согласованы со сводкой.
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(out.join("summary.json")).unwrap()).unwrap();
    assert_eq!(&written, summary);
    let cases: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(out.join("cases.json")).unwrap()).unwrap();
    assert_eq!(cases.len(), 3);
    let quiet = cases.iter().find(|c| c["id"] == "quiet").unwrap();
    assert_eq!(quiet["outcome"], "TARGET_IDENTITY_UNAVAILABLE");
    assert!(quiet["delivered_boundaries_resolved"].is_null());
    assert!(cases.iter().all(|c| c["accepted"] == false));
    let results = fs::read_to_string(out.join("results.jsonl")).unwrap();
    assert_eq!(results.lines().count(), 3);
    // Каталог отчёта не перезаписывается, а внутри каталога разметки не создаётся.
    let again = cli(&f, &refs);
    assert!(!again.status.success());
    let inside = labels.parent().unwrap().join("eval");
    let args = eval_args(&f, &labels, &inside);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    assert!(!cli(&f, &refs).status.success());
    assert!(!inside.exists());
}

#[test]
fn eval_stops_on_backend_failure_instead_of_scoring_a_partial_set() {
    let f = fixture();
    let labels = labels(&f);
    fs::write(f.state.join("fail"), b"").unwrap();
    let out = f.root.join("eval");
    let args = eval_args(&f, &labels, &out);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let failed = cli(&f, &refs);
    assert_eq!(
        failed.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&failed.stdout)
    );
    assert!(
        json(&failed)["error"]["details"]["blocker"]
            .as_str()
            .is_some_and(|b| b.starts_with("recognizer_backend"))
    );
    assert!(!out.join("summary.json").exists());
}

#[test]
fn eval_refuses_invalid_labels_before_creating_the_report() {
    let f = fixture();
    let labels = labels(&f);
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&labels).unwrap()).unwrap();
    type Mutation = fn(&mut serde_json::Value);
    let broken: [(&str, Mutation, &str); 7] = [
        (
            "duplicate",
            |v| v["cases"][1]["id"] = "nama".into(),
            "invalid_transition",
        ),
        (
            "empty",
            |v| v["cases"][0]["id"] = "".into(),
            "invalid_transition",
        ),
        (
            "reversed",
            |v| v["cases"][0]["words"][0]["end_ms"] = 300.0.into(),
            "invalid_transition",
        ),
        (
            "negative",
            |v| v["cases"][0]["words"][0]["start_ms"] = (-1.0).into(),
            "invalid_transition",
        ),
        // Конкурирующее чтение совпадает с целью: цель отвергается при
        // чтении разметки, а не после прогрева моделей.
        (
            "competing",
            |v| v["cases"][0]["target"]["competing_readings"] = serde_json::json!(["なま"]),
            "invalid_identity",
        ),
        (
            "non-nfc",
            |v| v["cases"][1]["target"]["surface"] = "\u{30DB}\u{309A}".into(),
            "invalid_identity",
        ),
        // Файл записи не читается: ошибка входа не оставляет каталога отчёта.
        (
            "missing-file",
            |v| v["cases"][2]["file"] = "нет-такого.mp3".into(),
            "",
        ),
    ];
    for (name, mutate, code) in broken {
        let mut value = original.clone();
        mutate(&mut value);
        fs::write(&labels, serde_json::to_vec(&value).unwrap()).unwrap();
        let out = f.root.join(format!("eval-{name}"));
        let args = eval_args(&f, &labels, &out);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let refused = cli(&f, &refs);
        assert!(!refused.status.success(), "{name}");
        if !code.is_empty() {
            assert_eq!(refused.status.code(), Some(3), "{name}");
            assert_eq!(json(&refused)["error"]["code"], code, "{name}");
        }
        assert!(!out.exists(), "{name}");
    }
}

fn selection(f: &Fixture, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("selection")
        .args(args)
        .current_dir(&f.root)
        .output()
        .unwrap()
}

/// Сквозной путь без карточек: пакет распознавания → приём исходников без
/// чтения → связь с результатами (без вызова моделей) → пакет свидетельств →
/// авто-выбор без вопросов → обзор → адресный ответ пользователя → чтение по
/// явной политике. `decks/**` не открывается.
#[test]
fn selection_links_recognizer_results_without_cards_and_routes_decisions() {
    let f = fixture();
    let targets = manifest(&f);
    let mut start: Vec<String> = vec![
        "--input".into(),
        f.input.to_str().unwrap().into(),
        "--targets".into(),
        targets.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
    ];
    start.extend(models_args(&f));
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    let started = run(&f, "batch-start", &refs);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );

    // Защищённые данные не читаются даже как каталог исходников: пригодный
    // MP3 внутри `decks/` отвергается по границе, а не по пустому набору.
    fs::create_dir_all(f.root.join("decks")).unwrap();
    fs::copy(f.input.join("生.mp3"), f.root.join("decks/生.mp3")).unwrap();
    fs::write(f.root.join("AGENTS.md"), b"").unwrap();
    let refused = selection(&f, &["intake", "--batch-id", "sel", "--dir", "decks"]);
    assert!(!refused.status.success());
    assert_eq!(
        json(&refused)["error"]["code"],
        "boundary_violation",
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    // Хранилище по умолчанию: `--repository-root` = текущий каталог, без `--store`.
    assert!(
        !f.root
            .join(".asset-store/pronunciation-audio/.runtime/batches/sel/state.json")
            .exists(),
        "отвергнутый приём не создаёт пакет"
    );

    let intake = json(&selection(
        &f,
        &[
            "intake",
            "--batch-id",
            "sel",
            "--dir",
            f.input.to_str().unwrap(),
        ],
    ));
    assert_eq!(intake["outcome"], "ok", "{intake}");
    assert!(
        f.root
            .join(".asset-store/pronunciation-audio/.runtime/batches/sel/state.json")
            .exists(),
        "проверяемый путь пакета принадлежит фактическому хранилищу"
    );
    assert_eq!(
        intake["result"]["added"].as_array().unwrap().len(),
        2,
        "{intake}"
    );
    assert_eq!(
        intake["result"]["already_present"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let before = calls(&f).len();
    let linked = json(&selection(
        &f,
        &[
            "link-recognizer",
            "--batch-id",
            "sel",
            "--workspace",
            f.workspace.to_str().unwrap(),
            "--recognizer-batch",
            "b1",
        ],
    ));
    assert_eq!(linked["outcome"], "ok", "{linked}");
    assert_eq!(linked["result"]["links_created"], 2, "{linked}");
    assert_eq!(linked["result"]["free_analyses"], 1, "{linked}");
    assert_eq!(calls(&f).len(), before, "связывание не вызывает модели");

    let status = json(&selection(
        &f,
        &["status", "--batch-id", "sel", "--filter", "all"],
    ));
    assert_eq!(status["result"]["groups"], 2, "{status}");
    assert!(
        status["result"]["perception_notice"]
            .as_str()
            .unwrap()
            .contains("не прослушивание")
    );
    let auto = json(&selection(&f, &["auto-select", "--batch-id", "sel"]));
    assert_eq!(auto["outcome"], "ok", "{auto}");
    assert_eq!(auto["result"]["selected_deterministic"], 0, "{auto}");

    let export = json(&selection(
        &f,
        &["review-export", "--batch-id", "sel", "--filter", "all"],
    ));
    assert_eq!(export["outcome"], "ok", "{export}");
    let index = PathBuf::from(export["result"]["index"].as_str().unwrap());
    assert!(fs::read_to_string(index).unwrap().contains("<audio"));

    let group = status["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["reading"] == "なま")
        .unwrap();
    let candidate = group["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "source_original")
        .unwrap();
    let decided = json(&selection(
        &f,
        &[
            "decide",
            "--batch-id",
            "sel",
            "--surface",
            "生",
            "--reading",
            "なま",
            "--candidate-id",
            candidate["assessment"]["candidate_id"].as_str().unwrap(),
            "--sha256",
            candidate["addressed_sha256"].as_str().unwrap(),
            "--authority",
            "user-via-agent",
            "--agent-id",
            "codex-agent",
            "--channel",
            "agent-dialog",
            "--statement",
            "для なま выберите исходный файл",
            "--reason",
            "явный ответ пользователя",
        ],
    ));
    assert_eq!(decided["outcome"], "ok", "{decided}");
    assert_eq!(
        decided["result"]["decision"]["authority"]["user_listened"],
        false
    );
    let requests = f.root.join("requests.json");
    fs::write(
        &requests,
        serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"items":[{"surface":"生","reading":"なま"}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let read = json(&selection(
        &f,
        &[
            "read",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
            "--policy",
            "human-decision",
        ],
    ));
    assert_eq!(read["outcome"], "ok", "{read}");
    assert_eq!(
        read["result"]["assets"][0]["consumer_filename"],
        "生_なま.mp3"
    );
    assert!(
        read["result"]["assets"][0].get("bytes").is_none(),
        "JSON просмотра не содержит байтов"
    );
    // Байты выдаются отдельной явной командой: каталог вывода — само
    // разрешение потребить запись, повтор идемпотентен, `decks/**` запрещён.
    let chosen = f.root.join("chosen");
    let exported = json(&selection(
        &f,
        &[
            "export",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
            "--policy",
            "human-decision",
            "--out-dir",
            chosen.to_str().unwrap(),
        ],
    ));
    assert_eq!(exported["outcome"], "ok", "{exported}");
    assert_eq!(
        exported["result"]["exported"][0]["consumer_filename"],
        "生_なま.mp3"
    );
    assert_eq!(exported["result"]["exported"][0]["written"], true);
    let written = fs::read(chosen.join("生_なま.mp3")).unwrap();
    assert_eq!(
        sha256_hex(&written),
        exported["result"]["exported"][0]["sha256"]
            .as_str()
            .unwrap()
    );
    assert_eq!(
        exported["result"]["exported"][0]["sha256"],
        read["result"]["assets"][0]["sha256"]
    );
    let repeated = json(&selection(
        &f,
        &[
            "export",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
            "--policy",
            "human-decision",
            "--out-dir",
            chosen.to_str().unwrap(),
        ],
    ));
    assert_eq!(repeated["result"]["exported"][0]["written"], false);
    let refused_export = selection(
        &f,
        &[
            "export",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
            "--policy",
            "human-decision",
            "--out-dir",
            "decks/chosen",
        ],
    );
    assert!(
        !refused_export.status.success(),
        "decks/** не каталог вывода"
    );
    assert!(!f.root.join("decks/chosen").exists());
    // Явная связь без распознавания и локальная экспертиза на фейковом воркере
    // (нулевые эмиссии): вердикт не «поддерживает», цель ждёт пользователя.
    let sha = candidate["source_sha256"].as_str().unwrap();
    let linked = json(&selection(
        &f,
        &[
            "link-target",
            "--batch-id",
            "sel",
            "--source-sha256",
            sha,
            "--surface",
            "生",
            "--reading",
            "しょう",
        ],
    ));
    assert_eq!(linked["outcome"], "ok", "{linked}");
    let manual = linked["result"]["link"]["candidates"][0]["candidate_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut audio: Vec<String> = [
        "audio-review",
        "--batch-id",
        "sel",
        "--candidate-id",
        &manual,
        "--enhancer",
        "none",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    audio.extend(models_args(&f));
    let refs: Vec<&str> = audio.iter().map(String::as_str).collect();
    let reviewed = json(&selection(&f, &refs));
    assert_eq!(reviewed["outcome"], "ok", "{reviewed}");
    assert_ne!(reviewed["result"]["audio_review"]["verdict"], "supports");
    assert_eq!(reviewed["result"]["audio_review"]["local"], true);
    let decide = |extra: &[&str]| {
        let mut args = vec![
            "decide",
            "--batch-id",
            "sel",
            "--surface",
            "生",
            "--reading",
            "しょう",
            "--agent-id",
            "codex-agent",
            "--reason",
            "проверка",
        ];
        args.extend_from_slice(extra);
        json(&selection(&f, &args))
    };
    let missing_review = decide(&[
        "--candidate-id",
        &manual,
        "--sha256",
        sha,
        "--authority",
        "agent-audio",
    ]);
    assert_eq!(missing_review["outcome"], "error", "{missing_review}");
    let no_statement = decide(&[
        "--candidate-id",
        &manual,
        "--sha256",
        sha,
        "--authority",
        "user-via-agent",
        "--channel",
        "agent-dialog",
    ]);
    assert_eq!(no_statement["outcome"], "error", "{no_statement}");
    let review_id = reviewed["result"]["audio_review"]["review_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let unsupported = decide(&[
        "--candidate-id",
        &manual,
        "--sha256",
        sha,
        "--authority",
        "agent-audio",
        "--audio-review-id",
        &review_id,
    ]);
    assert_eq!(
        unsupported["outcome"], "error",
        "неподдержанная экспертиза не даёт права выбора"
    );
    let rejected = decide(&[
        "--candidate-id",
        &manual,
        "--sha256",
        sha,
        "--reject",
        "--authority",
        "user-via-agent",
        "--channel",
        "agent-dialog",
        "--statement",
        "это не しょう",
    ]);
    assert_eq!(
        rejected["result"]["decision"]["action"]["kind"], "reject_candidate",
        "{rejected}"
    );
    let deferred = decide(&[
        "--defer",
        "--authority",
        "user-via-agent",
        "--channel",
        "agent-dialog",
        "--statement",
        "не знаю, отложите",
    ]);
    assert_eq!(
        deferred["result"]["decision"]["action"]["kind"], "defer",
        "{deferred}"
    );

    // Снимок до решений и аудиоэкспертизы устарел, даже если все строки пусты.
    let review_id = export["result"]["review_id"].as_str().unwrap();
    let applied = json(&selection(
        &f,
        &[
            "review-apply",
            "--batch-id",
            "sel",
            "--review-id",
            review_id,
        ],
    ));
    assert_eq!(applied["outcome"], "error", "{applied}");
    // Новый пустой снимок текущей ревизии применяется без изменений.
    let fresh = json(&selection(
        &f,
        &["review-export", "--batch-id", "sel", "--filter", "all"],
    ));
    assert_eq!(fresh["outcome"], "ok", "{fresh}");
    let fresh_id = fresh["result"]["review_id"].as_str().unwrap();
    let applied = json(&selection(
        &f,
        &["review-apply", "--batch-id", "sel", "--review-id", fresh_id],
    ));
    assert_eq!(applied["outcome"], "ok", "{applied}");
    assert_eq!(applied["result"]["changed"], false);
    // Файл решений, сохранённый страницей вне каталога снимка, проходит те же
    // проверки; не-UTF-8 файл отвергается до применения.
    let saved = f.root.join(format!("decisions-{fresh_id}.tsv"));
    fs::copy(
        f.root.join(format!(
            ".asset-store/pronunciation-audio/.runtime/batches/sel/review-{fresh_id}/decisions.tsv"
        )),
        &saved,
    )
    .unwrap();
    let from_page = json(&selection(
        &f,
        &[
            "review-apply",
            "--batch-id",
            "sel",
            "--review-id",
            fresh_id,
            "--decisions-file",
            saved.to_str().unwrap(),
        ],
    ));
    assert_eq!(from_page["outcome"], "ok", "{from_page}");
    assert_eq!(from_page["result"]["changed"], false);
    fs::write(&saved, [0xff, 0xfe]).unwrap();
    let broken = json(&selection(
        &f,
        &[
            "review-apply",
            "--batch-id",
            "sel",
            "--review-id",
            fresh_id,
            "--decisions-file",
            saved.to_str().unwrap(),
        ],
    ));
    assert_eq!(broken["outcome"], "error", "{broken}");
    // Материализация без выбранных интервалов ничего не создаёт, а адресная
    // для пары с выбранным исходником — отказ без изменений.
    let materialized = json(&selection(&f, &["materialize", "--batch-id", "sel"]));
    assert_eq!(materialized["outcome"], "ok", "{materialized}");
    assert!(
        materialized["result"]["materialized"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let addressed = json(&selection(
        &f,
        &[
            "materialize",
            "--batch-id",
            "sel",
            "--surface",
            "生",
            "--reading",
            "なま",
        ],
    ));
    assert_eq!(
        addressed["error"]["code"], "invalid_transition",
        "{addressed}"
    );
    // Явная публикация исходника без заявленного прослушивания: не Verified.
    let promoted = json(&selection(
        &f,
        &[
            "promote",
            "--batch-id",
            "sel",
            "--surface",
            "生",
            "--reading",
            "なま",
        ],
    ));
    assert_eq!(promoted["outcome"], "ok", "{promoted}");
    assert_eq!(promoted["result"]["human_attestation_created"], false);
    assert_ne!(promoted["result"]["asset"]["lifecycle"], "verified");
    let refused = json(&selection(
        &f,
        &[
            "promote",
            "--batch-id",
            "sel",
            "--surface",
            "生",
            "--reading",
            "なま",
            "--attest-heard-decision",
        ],
    ));
    assert_eq!(refused["outcome"], "error");

    // Политика потребителя обязательна: значения по умолчанию нет.
    let missing = selection(
        &f,
        &[
            "read",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
        ],
    );
    assert_eq!(missing.status.code(), Some(2));

    // Режим проверки фиксируется журналом; решение пользователя действует в
    // любом режиме, а агентское в ручном режиме не применяется.
    let delegated = json(&selection(
        &f,
        &[
            "review-mode",
            "--batch-id",
            "sel",
            "--mode",
            "delegated",
            "--agent-id",
            "codex-agent",
            "--agent-trust",
            "agent-selected",
            "--statement",
            "решай сам",
        ],
    ));
    assert_eq!(delegated["outcome"], "ok", "{delegated}");
    assert_eq!(delegated["result"]["changed"], true);
    let manual = json(&selection(
        &f,
        &[
            "review-mode",
            "--batch-id",
            "sel",
            "--mode",
            "manual",
            "--statement",
            "проверю сам",
        ],
    ));
    assert_eq!(manual["outcome"], "ok", "{manual}");
    assert_eq!(manual["result"]["grant"]["mode"], "manual_user");
    let still_read = json(&selection(
        &f,
        &[
            "read",
            "--batch-id",
            "sel",
            "--requests",
            requests.to_str().unwrap(),
            "--policy",
            "human-decision",
        ],
    ));
    assert_eq!(still_read["outcome"], "ok", "{still_read}");
}

/// Отказ отдельного файла при `link --intake-missing` остаётся исходом этого
/// файла: операция не падает, а остальные результаты связываются.
#[test]
fn link_intake_missing_keeps_single_file_refusal_inside_the_report() {
    let f = fixture();
    let targets = manifest(&f);
    let mut start: Vec<String> = vec![
        "--input".into(),
        f.input.to_str().unwrap().into(),
        "--targets".into(),
        targets.to_str().unwrap().into(),
        "--enhancer".into(),
        "none".into(),
    ];
    start.extend(models_args(&f));
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    let started = run(&f, "batch-start", &refs);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );
    // Пакет выбора уже существует, но исходников входа в нём нет: для `sel`
    // принят только посторонний MP3.
    let extra = f.root.join("extra");
    fs::create_dir_all(&extra).unwrap();
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=700:duration=0.4:sample_rate=44100",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ],
        &extra.join("other.mp3"),
    );
    let warmup = json(&selection(
        &f,
        &[
            "intake",
            "--batch-id",
            "sel",
            "--dir",
            extra.to_str().unwrap(),
        ],
    ));
    assert_eq!(warmup["outcome"], "ok", "{warmup}");

    // Файл входа перестаёт быть обычным файлом уже после инвентаризации.
    fs::remove_file(f.input.join("silence.mp3")).unwrap();
    fs::create_dir(f.input.join("silence.mp3")).unwrap();

    let linked = selection(
        &f,
        &[
            "link-recognizer",
            "--batch-id",
            "sel",
            "--workspace",
            f.workspace.to_str().unwrap(),
            "--recognizer-batch",
            "b1",
            "--intake-missing",
        ],
    );
    assert!(
        linked.status.success(),
        "отказ одного файла не отменяет операцию: {}",
        String::from_utf8_lossy(&linked.stdout)
    );
    let linked = json(&linked);
    assert_eq!(linked["outcome"], "ok", "{linked}");
    let rejected = linked["result"]["sources_rejected"]
        .as_array()
        .unwrap_or_else(|| panic!("нет поля отказов: {linked}"));
    assert_eq!(rejected.len(), 1, "{linked}");
    assert_eq!(rejected[0]["code"], "source_not_regular", "{linked}");
    assert!(
        linked["result"]["links_created"].as_u64().unwrap() >= 1,
        "остальные результаты связаны: {linked}"
    );
}
