//! Контракт CLI `pronunciation-assets recognizer` на настоящих MP3 и
//! поддельном воркере распознавания с настоящим протоколом: манифест целей,
//! пары SHA × цель, прерывание и возобновление, сбои среды не кэшируются и
//! повторяются, кэш результатов переиспользуется без вызова моделей, отчёт и
//! повторная проверка входа. Настоящий инференс проверяют тесты
//! `tools/speech-recognize`, решения распознавания — модульные тесты.
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
}
