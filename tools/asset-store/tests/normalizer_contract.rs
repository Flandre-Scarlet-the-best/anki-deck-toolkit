//! Balanced Normalizer и QualityGate на настоящих синтетических MP3: CLI,
//! пакет с контрольными точками, отчёты и явная публикация через CAS.
use asset_store::hashing::sha256_hex;
use asset_store::pronunciation::normalizer::{
    GateOutcome, Mode, NormalizerProfile, Toolchain, Variant, process,
};
use asset_store::pronunciation::normalizer_batch as nbatch;
use asset_store::pronunciation::{
    Pronunciation, PronunciationDomainPolicy, derivation, import_bytes,
};
use asset_store::temp_workspace::TempWorkspace;
use asset_store::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

/// MP3 из lavfi: тон с заданными краевой тишиной и уровнем.
fn encode(name: &str, filter: &str) -> Vec<u8> {
    let workspace = TempWorkspace::create("normalizer-fixture").unwrap();
    let path = workspace.path().join(format!("{name}.mp3"));
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i", filter])
        .args(["-ac", "1", "-c:a", "libmp3lame", "-q:a", "4"])
        .arg(&path)
        .output()
        .expect("для тестов нужен FFmpeg с libmp3lame");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(path).unwrap()
}

/// Тон 440 Гц около −23 LUFS (источник `sine` имеет амплитуду 1/8) с паузами 0.5/0.6 с по краям: нужна только обрезка.
fn padded() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| {
        encode(
            "padded",
            "sine=frequency=440:duration=0.5:sample_rate=44100,volume=0.8,adelay=500:all=1,apad=pad_dur=0.6",
        )
    })
}

/// Тон без пауз на целевой громкости: изменение не нужно.
fn good() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| {
        encode(
            "good",
            "sine=frequency=997:duration=1.2:sample_rate=44100,volume=0.8",
        )
    })
}

/// Тихий тон с короткими краями: нужно усиление.
fn quiet() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| {
        encode(
            "quiet",
            "sine=frequency=440:duration=1.0:sample_rate=44100,volume=0.24,adelay=60:all=1,apad=pad_dur=0.06",
        )
    })
}

fn toolchain() -> Toolchain {
    Toolchain::detect().expect("для тестов нужен FFmpeg")
}

fn cli(ws: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(ws)
        .arg("--store")
        .arg(ws.join("store"))
        .arg("normalizer")
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            let key = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if kind.is_symlink() {
                out.insert(key, "symlink".into());
            } else if kind.is_dir() {
                walk(&path, base, out);
            } else {
                out.insert(key, sha256_hex(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Корпус: дубль с Unicode-именем, подкаталог, повреждённые и посторонние файлы, ссылка.
fn corpus(root: &Path) -> std::path::PathBuf {
    let input = root.join("corpus");
    fs::create_dir_all(input.join("подкаталог")).unwrap();
    fs::write(input.join("padded.mp3"), padded()).unwrap();
    fs::write(input.join("good.mp3"), good()).unwrap();
    fs::write(input.join("こいつ.MP3"), good()).unwrap();
    fs::write(input.join("подкаталог/quiet.mp3"), quiet()).unwrap();
    fs::write(
        input.join("truncated.mp3"),
        &padded()[..padded().len() / 2 + 7],
    )
    .unwrap();
    fs::write(
        input.join("<img src=x onerror=alert(1)>.mp3"),
        b"<html>not audio</html>",
    )
    .unwrap();
    fs::write(input.join("notes.txt"), b"text").unwrap();
    std::os::unix::fs::symlink(input.join("good.mp3"), input.join("link.mp3")).unwrap();
    input
}

#[test]
fn single_file_process_trims_gains_keeps_and_rejects_explicitly() {
    let profile = NormalizerProfile::balanced();
    let tools = toolchain();
    let trimmed = process(padded(), &profile, Mode::Normalize, &tools).unwrap();
    let result = &trimmed.result;
    assert_eq!(
        result.gate.outcome,
        GateOutcome::Accept,
        "{:#?}",
        result.gate
    );
    assert_eq!(result.gate.proposed_variant, Some(Variant::Processed));
    let plan = result.plan.as_ref().unwrap();
    assert!(plan.leading.trim_ms > 350.0 && plan.trailing.trim_ms > 400.0);
    let qc = result.post_qc.as_ref().unwrap();
    assert!(qc.passed, "{:#?}", qc.checks);
    let output = trimmed.output_bytes.as_ref().unwrap();
    assert_eq!(result.output.as_ref().unwrap().sha256, sha256_hex(output));
    // Фактически декодированный результат: та же частота, моно, длительность по плану.
    let probe = asset_store::pronunciation::probe_mp3(output).unwrap();
    assert_eq!((probe.sample_rate, probe.channels), (44100, 1));
    assert!((probe.decoded_duration_seconds * 1000.0 - plan.expected.duration_ms).abs() < 10.0);
    // Повтор детерминирован: та же сборка FFmpeg даёт те же байты.
    let again = process(padded(), &profile, Mode::Normalize, &tools).unwrap();
    assert_eq!(again.output_bytes.as_ref(), Some(output));

    let kept = process(good(), &profile, Mode::Normalize, &tools).unwrap();
    assert_eq!(
        kept.result.gate.outcome,
        GateOutcome::KeepOriginal,
        "{:#?}",
        kept.result.plan
    );
    assert!(kept.output_bytes.is_none() && kept.result.output.is_none());

    let gained = process(quiet(), &profile, Mode::Normalize, &tools).unwrap();
    let gain = &gained.result.plan.as_ref().unwrap().gain;
    assert!(gain.applied_db > 6.0, "{gain:?}");
    let after = gained
        .result
        .post_qc
        .as_ref()
        .unwrap()
        .analysis
        .as_ref()
        .unwrap();
    assert!(after.true_peak_dbtp <= -2.0 + 0.5);

    let broken = process(
        &padded()[..padded().len() / 2 + 7],
        &profile,
        Mode::Normalize,
        &tools,
    )
    .unwrap();
    assert_eq!(broken.result.gate.outcome, GateOutcome::Reject);
    assert_eq!(broken.result.gate.reasons[0].code, "source_invalid");
    let html = process(b"<html>", &profile, Mode::Analyze, &tools).unwrap();
    assert_eq!(html.result.gate.outcome, GateOutcome::Reject);
    // Неопределённые величины сериализуются как null, а не NaN/0.
    let text = serde_json::to_string(&kept.result).unwrap();
    assert!(!text.contains("NaN") && !text.contains("Infinity"));
}

#[test]
fn analyze_mode_plans_without_encoding_and_marks_unverified() {
    let processed = process(
        padded(),
        &NormalizerProfile::balanced(),
        Mode::Analyze,
        &toolchain(),
    )
    .unwrap();
    assert!(processed.output_bytes.is_none());
    assert!(!processed.result.gate.post_qc_verified);
    assert!(processed.result.post_qc.is_none());
    assert!(processed.result.plan.unwrap().needs_processing);
}

#[test]
fn cli_analyze_and_trial_never_write_next_to_the_source() {
    let ws = TempWorkspace::create("normalizer-cli-single").unwrap();
    let input = ws.path().join("input");
    let out = ws.path().join("out");
    fs::create_dir_all(&input).unwrap();
    fs::create_dir_all(&out).unwrap();
    let source = input.join("padded.mp3");
    fs::write(&source, padded()).unwrap();
    let before = snapshot(&input);
    let analyzed = cli(ws.path(), &["analyze", "--file", source.to_str().unwrap()]);
    assert!(analyzed.status.success());
    assert_eq!(json(&analyzed)["result"]["gate"]["outcome"], "ACCEPT");
    assert_eq!(json(&analyzed)["result"]["gate"]["post_qc_verified"], false);
    let beside = input.join("padded.normalized.mp3");
    let refused = cli(
        ws.path(),
        &[
            "trial",
            "--file",
            source.to_str().unwrap(),
            "--out",
            beside.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&refused)["error"]["code"], "boundary_violation");
    let target = out.join("padded.mp3");
    let trial = cli(
        ws.path(),
        &[
            "trial",
            "--file",
            source.to_str().unwrap(),
            "--out",
            target.to_str().unwrap(),
        ],
    );
    assert!(
        trial.status.success(),
        "{}",
        String::from_utf8_lossy(&trial.stdout)
    );
    assert_eq!(json(&trial)["result"]["written"], true);
    assert_eq!(
        sha256_hex(fs::read(&target).unwrap()),
        json(&trial)["result"]["result"]["output"]["sha256"]
    );
    // Существующий файл вывода не перезаписывается.
    let again = cli(
        ws.path(),
        &[
            "trial",
            "--file",
            source.to_str().unwrap(),
            "--out",
            target.to_str().unwrap(),
        ],
    );
    assert!(!again.status.success());
    assert_eq!(snapshot(&input), before);
    let profile = cli(ws.path(), &["profile"]);
    assert_eq!(json(&profile)["result"]["id"], "balanced");
}

#[test]
fn batch_processes_directory_once_with_duplicates_unicode_and_failures() {
    let ws = TempWorkspace::create("normalizer-batch").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let before = snapshot(&input);
    let started = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "b1",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--jobs",
            "3",
        ],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );
    let value = json(&started);
    assert_eq!(value["result"]["run"]["outcome"], "completed");
    // 6 MP3, из них 5 уникальных SHA (дубль good/こいつ).
    assert_eq!(value["result"]["run"]["processed_now"], 5);
    let outcomes = &value["result"]["batch"]["progress"]["by_outcome"];
    assert_eq!(outcomes["REJECT"], 2);
    assert_eq!(outcomes["KEEP_ORIGINAL"], 1);
    assert_eq!(outcomes["ACCEPT"], 2);
    // Корпус не изменён и в нём ничего не появилось.
    assert_eq!(snapshot(&input), before);

    // Повтор и новый пакет той же конфигурации не повторяют работу.
    let resumed = cli(
        ws.path(),
        &[
            "batch-resume",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&resumed)["result"]["run"]["processed_now"], 0);
    assert_eq!(json(&resumed)["result"]["run"]["reused"], 5);
    let second = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "b2",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&second)["result"]["run"]["processed_now"], 0);
    assert_eq!(json(&second)["result"]["run"]["reused"], 5);
    let duplicate = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "b1",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&duplicate)["error"]["code"], "identity_conflict");

    // Отчёт: сводка, таблицы и офлайн HTML без сырых имён и сетевых ссылок.
    let report_dir = ws.path().join("report");
    let report = cli(
        ws.path(),
        &[
            "batch-report",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
            "--out",
            report_dir.to_str().unwrap(),
            "--html",
            "--include",
            "all",
        ],
    );
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stdout)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(report_dir.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["inventory"]["mp3_files"], 6);
    assert_eq!(summary["inventory"]["unique_sha256"], 5);
    assert_eq!(summary["inventory"]["duplicate_groups"], 1);
    assert_eq!(summary["inventory"]["skipped"]["symlink"], 1);
    assert_eq!(summary["inventory"]["skipped"]["unsupported_extension"], 1);
    assert_eq!(summary["missing_results"], 0);
    assert!(summary["metrics"]["duration_ms"]["quantiles"]["p50"].is_number());
    let html = fs::read_to_string(report_dir.join("report.html")).unwrap();
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("\\u003cimg src=x"));
    assert!(html.contains("Content-Security-Policy"));
    assert!(!html.contains("http://") && !html.contains("https://"));
    for entry in fs::read_dir(report_dir.join("media")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert_eq!(name.len(), 64 + 4, "{name}");
    }
    let tsv = fs::read_to_string(report_dir.join("items.tsv")).unwrap();
    assert_eq!(tsv.lines().count(), 6);
    assert!(tsv.contains("こいつ.MP3"));
    let exists = cli(
        ws.path(),
        &[
            "batch-report",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
            "--out",
            report_dir.to_str().unwrap(),
        ],
    );
    assert!(!exists.status.success());

    // Воспроизводимая выборка и сводка ручных решений.
    let sample_a = ws.path().join("sample-a.json");
    let sample_b = ws.path().join("sample-b.json");
    for path in [&sample_a, &sample_b] {
        let sampled = cli(
            ws.path(),
            &[
                "batch-sample",
                "--batch-id",
                "b1",
                "--workspace",
                workspace.to_str().unwrap(),
                "--seed",
                "7",
                "--per-stratum",
                "2",
                "--out",
                path.to_str().unwrap(),
            ],
        );
        assert!(sampled.status.success());
    }
    assert_eq!(fs::read(&sample_a).unwrap(), fs::read(&sample_b).unwrap());
    let sample: serde_json::Value = serde_json::from_slice(&fs::read(&sample_a).unwrap()).unwrap();
    let accepted = sample["strata"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "random_accept")
        .unwrap()["selected"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let decisions = ws.path().join("decisions.json");
    fs::write(
        &decisions,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "kind": "normalizer_review_decisions",
            "batch_id": "b1",
            "config_sha256": value["result"]["batch"]["config_sha256"],
            "decisions": [{"input_sha256": accepted, "verdict": "bad_trim", "note": "проверка"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let summary = cli(
        ws.path(),
        &[
            "batch-review-summary",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
            "--decisions",
            decisions.to_str().unwrap(),
            "--sample",
            sample_a.to_str().unwrap(),
        ],
    );
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stdout)
    );
    assert_eq!(json(&summary)["result"]["false_accepts"][0], accepted);
    assert_eq!(
        json(&summary)["result"]["by_stratum_and_verdict"]["random_accept"]["bad_trim"],
        1
    );

    // Проверка неизменности входа и обнаружение подмены.
    let verified = cli(
        ws.path(),
        &[
            "batch-verify-sources",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&verified)["result"]["unchanged"], 6);
    fs::write(input.join("good.mp3"), quiet()).unwrap();
    let changed = cli(
        ws.path(),
        &[
            "batch-verify-sources",
            "--batch-id",
            "b1",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&changed)["error"]["code"], "integrity_mismatch");
    assert_eq!(json(&changed)["error"]["details"]["changed"][0], "good.mp3");
}

/// ID кэша зарезервирован: пакет с ним иначе ждал бы блокировку, которую
/// держит сам кэш, и зависал на первой контрольной точке.
#[test]
fn cache_batch_id_is_reserved() {
    let ws = TempWorkspace::create("normalizer-reserved-id").unwrap();
    let input = ws.path().join("corpus");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("good.mp3"), good()).unwrap();
    let workspace = ws.path().join("work");
    fs::create_dir(&workspace).unwrap();
    let refused = nbatch::start(nbatch::StartRequest {
        batch_id: nbatch::CACHE_ID,
        workspace: &workspace,
        input_root: &fs::canonicalize(&input).unwrap(),
        mode: Mode::Analyze,
        profile: NormalizerProfile::balanced(),
        limits: nbatch::Limits {
            jobs: 1,
            max_files: 10,
            max_total_bytes: 1 << 20,
        },
    })
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidTransition);
    assert!(!workspace.join(".runtime").exists());
    let stop = AtomicBool::new(false);
    for result in [
        nbatch::resume(&workspace, nbatch::CACHE_ID, None, Some(0), &stop).map(|_| ()),
        nbatch::status(&workspace, nbatch::CACHE_ID).map(|_| ()),
        nbatch::cancel(&workspace, nbatch::CACHE_ID).map(|_| ()),
        nbatch::load(&workspace, nbatch::CACHE_ID).map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().code, ErrorCode::InvalidTransition);
    }
}

/// Заведомое превышение `--max-total-bytes` по `fstat` отклоняется до чтения:
/// разреженный файл в 1 TiB не читается и не хешируется целиком.
#[test]
fn inventory_rejects_declared_total_overflow_before_reading() {
    let ws = TempWorkspace::create("normalizer-total-before-read").unwrap();
    let input = ws.path().join("corpus");
    fs::create_dir_all(&input).unwrap();
    fs::File::create(input.join("huge.mp3"))
        .unwrap()
        .set_len(1 << 40)
        .unwrap();
    let started = std::time::Instant::now();
    let refused = nbatch::inventory(
        &input,
        &nbatch::Limits {
            jobs: 1,
            max_files: 10,
            max_total_bytes: 1 << 20,
        },
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidTransition);
    assert!(refused.message.contains("--max-total-bytes"));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn batch_limits_cancellation_interruption_and_boundaries() {
    let ws = TempWorkspace::create("normalizer-limits").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let args = |id: &'static str, extra: &[&'static str]| {
        let mut all = vec![
            "batch-start".to_owned(),
            "--batch-id".into(),
            id.into(),
            "--input-dir".into(),
            input.to_str().unwrap().into(),
            "--workspace".into(),
            workspace.to_str().unwrap().into(),
        ];
        all.extend(extra.iter().map(|s| s.to_string()));
        all
    };
    let run = |all: Vec<String>| {
        cli(
            ws.path(),
            &all.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    };
    let too_many = run(args("many", &["--max-files", "2"]));
    assert_eq!(json(&too_many)["error"]["code"], "invalid_transition");
    let too_big = run(args("big", &["--max-total-bytes", "100"]));
    assert_eq!(json(&too_big)["error"]["code"], "invalid_transition");
    let partial = run(args("partial", &["--max-items", "1", "--jobs", "1"]));
    assert!(partial.status.success());
    assert_eq!(json(&partial)["result"]["run"]["outcome"], "paused");
    assert_eq!(json(&partial)["result"]["run"]["remaining"], 4);
    let status = cli(
        ws.path(),
        &[
            "batch-status",
            "--batch-id",
            "partial",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&status)["result"]["progress"]["completed"], 1);
    let cancel = cli(
        ws.path(),
        &[
            "batch-cancel",
            "--batch-id",
            "partial",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&cancel)["result"]["cancelled"], true);
    let after_cancel = cli(
        ws.path(),
        &[
            "batch-resume",
            "--batch-id",
            "partial",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&after_cancel)["error"]["code"], "invalid_transition");

    // Прерывание до начала работы учитывает только готовый результат общего
    // кэша (вход, обработанный пакетом partial).
    let stop = AtomicBool::new(true);
    nbatch::start(nbatch::StartRequest {
        batch_id: "interrupted",
        workspace: &workspace,
        input_root: &fs::canonicalize(&input).unwrap(),
        mode: Mode::Normalize,
        profile: NormalizerProfile::balanced(),
        limits: nbatch::Limits {
            jobs: 2,
            max_files: 100,
            max_total_bytes: 1 << 30,
        },
    })
    .unwrap();
    let interrupted = nbatch::resume(&workspace, "interrupted", None, None, &stop).unwrap();
    assert_eq!(interrupted.outcome, "interrupted");
    assert_eq!(interrupted.processed_now, 0);
    assert_eq!(interrupted.reused, 1);
    let state = nbatch::status(&workspace, "interrupted").unwrap();
    assert_eq!(state.progress.completed, interrupted.reused);
    let stop = AtomicBool::new(false);
    let finished = nbatch::resume(&workspace, "interrupted", None, None, &stop).unwrap();
    assert_eq!(finished.outcome, "completed");
    assert_eq!(finished.processed_now + finished.reused, 5);
    assert!(finished.reused >= 1);

    // Вход и рабочий каталог не могут быть вложены друг в друга или в decks/.
    let nested = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "nested",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            input.join("work").to_str().unwrap(),
        ],
    );
    assert_eq!(json(&nested)["error"]["code"], "boundary_violation");
    fs::create_dir_all(ws.path().join("decks/output")).unwrap();
    fs::write(ws.path().join("AGENTS.md"), b"").unwrap();
    let decks = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "decks",
            "--input-dir",
            ws.path().join("decks/output").to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&decks)["error"]["code"], "boundary_violation");
    assert!(!input.join("work").exists());
}

/// Абсолютный путь инструмента из текущего PATH.
fn which(tool: &str) -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(tool))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("для теста нужен {tool}"))
}

/// Каталог PATH: настоящие `prlimit` и `ffprobe` и управляемая обёртка над
/// `ffmpeg`. Файл `fail` в каталоге заставляет обработку (но не определение
/// версий и отпечатка кодировщика) завершаться сигналом — сбой среды;
/// `alter` дописывает байт к отпечатку кодировщика — «другая сборка LAME».
fn tool_dir(root: &Path, with_ffmpeg: bool) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("tools");
    fs::create_dir_all(&dir).unwrap();
    for tool in ["prlimit", "ffprobe"] {
        std::os::unix::fs::symlink(which(tool), dir.join(tool)).unwrap();
    }
    if with_ffmpeg {
        let script = format!(
            r#"#!/bin/sh
case "$*" in
  *-version*) exec "{real}" "$@" ;;
  *toolchain-input*)
    for last; do :; done
    "{real}" "$@" || exit $?
    if [ -e "{dir}/alter" ]; then printf x >> "$last"; fi
    exit 0 ;;
esac
if [ -e "{dir}/fail" ]; then kill -KILL $$; fi
exec "{real}" "$@"
"#,
            real = which("ffmpeg").display(),
            dir = dir.display()
        );
        let path = dir.join("ffmpeg");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn cli_with_path(ws: &Path, path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(ws)
        .arg("--store")
        .arg(ws.join("store"))
        .arg("normalizer")
        .args(args)
        .env("PATH", path)
        .output()
        .unwrap()
}

fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Отсутствует именно FFmpeg: `prlimit` и `ffprobe` доступны, ошибка
/// называет недостающий инструмент, состояние пакета не создаётся.
#[test]
fn missing_ffmpeg_fails_before_any_state_and_names_the_tool() {
    let ws = TempWorkspace::create("normalizer-env").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let tools = tool_dir(ws.path(), false);
    assert!(tools.join("prlimit").exists() && !tools.join("ffmpeg").exists());
    let output = cli_with_path(
        ws.path(),
        &tools,
        &[
            "batch-start",
            "--batch-id",
            "nofm",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let error = &json(&output)["error"];
    assert_eq!(error["details"]["blocker"], "audio_tool_missing");
    assert_eq!(error["details"]["tool"], "ffmpeg");
    assert!(!workspace.join(".runtime/batches/nofm").exists());
}

/// Недоступный для записи рабочий каталог — отдельное условие.
#[test]
fn unwritable_workspace_fails_without_state() {
    if running_as_root() {
        return;
    }
    let ws = TempWorkspace::create("normalizer-ro").unwrap();
    let input = corpus(ws.path());
    let locked = ws.path().join("locked");
    fs::create_dir(&locked).unwrap();
    let restore = ClosedDirectory::close(&locked);
    let denied = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "ro",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            locked.to_str().unwrap(),
        ],
    );
    drop(restore);
    assert!(!denied.status.success());
    assert!(matches!(
        json(&denied)["error"]["code"].as_str(),
        Some("io_failure" | "boundary_violation")
    ));
    assert!(fs::read_dir(&locked).unwrap().next().is_none());
}

/// Сбой среды не становится исходом входа и не попадает в кэш; после
/// восстановления FFmpeg повтор реально обрабатывает незавершённые входы.
/// Изменение сборки кодировщика останавливает возобновление.
#[test]
fn infrastructure_failure_is_retried_after_recovery_and_toolchain_change_is_detected() {
    let ws = TempWorkspace::create("normalizer-infra").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let tools = tool_dir(ws.path(), true);
    let start = |id: &str| {
        cli_with_path(
            ws.path(),
            &tools,
            &[
                "batch-start",
                "--batch-id",
                id,
                "--input-dir",
                input.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
                "--jobs",
                "2",
            ],
        )
    };
    let resume = |id: &str| {
        cli_with_path(
            ws.path(),
            &tools,
            &[
                "batch-resume",
                "--batch-id",
                id,
                "--workspace",
                workspace.to_str().unwrap(),
            ],
        )
    };
    fs::write(tools.join("fail"), b"").unwrap();
    let failed = json(&start("infra"));
    let run = &failed["result"]["run"];
    assert_eq!(run["outcome"], "paused", "{failed}");
    assert!(
        run["infrastructure_failed_now"].as_u64().unwrap() >= 3,
        "{failed}"
    );
    assert!(run["remaining"].as_u64().unwrap() >= 3);
    let outcomes = &failed["result"]["batch"]["progress"]["by_outcome"];
    assert!(outcomes.get("REVIEW_REQUIRED").is_none(), "{outcomes}");
    let results = fs::read_dir(workspace.join(".runtime/batches/normalizer-cache/results"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    // В кэше только входы, отвергнутые до запуска FFmpeg (не MP3, усечённый).
    assert_eq!(outcomes["REJECT"], 2, "{outcomes}");
    assert_eq!(results, 2, "в кэш попал результат сбоя среды: {results}");
    // Отчётная выборка по неполному пакету отклоняется явно.
    let sample = cli_with_path(
        ws.path(),
        &tools,
        &[
            "batch-sample",
            "--batch-id",
            "infra",
            "--workspace",
            workspace.to_str().unwrap(),
            "--seed",
            "1",
            "--out",
            ws.path().join("sample.json").to_str().unwrap(),
        ],
    );
    assert_eq!(
        json(&sample)["error"]["details"]["blocker"],
        "batch_incomplete"
    );
    assert!(
        json(&sample)["error"]["details"]["missing_results"]
            .as_u64()
            .unwrap()
            >= 3
    );
    assert!(!ws.path().join("sample.json").exists());

    fs::remove_file(tools.join("fail")).unwrap();
    let recovered = json(&resume("infra"));
    assert_eq!(
        recovered["result"]["run"]["outcome"], "completed",
        "{recovered}"
    );
    let outcomes = &recovered["result"]["batch"]["progress"]["by_outcome"];
    assert_eq!(outcomes["REJECT"], 2);
    assert_eq!(outcomes["KEEP_ORIGINAL"], 1);
    assert_eq!(outcomes["ACCEPT"], 2);
    assert_eq!(
        recovered["result"]["batch"]["progress"]["infrastructure_failures"],
        serde_json::json!({})
    );

    fs::write(tools.join("alter"), b"").unwrap();
    let changed = json(&resume("infra"));
    assert_eq!(changed["error"]["code"], "invalid_transition");
    assert!(
        changed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("кодировщика")
    );
}

/// Повреждённый или потерянный вариант в кэше и изменённый после
/// инвентаризации вход не считаются готовыми; годный кэш не выбрасывается.
#[test]
fn stale_cache_entries_and_changed_inputs_are_not_reused() {
    let ws = TempWorkspace::create("normalizer-stale").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let args = |command: &str, id: &str| -> Vec<String> {
        let mut all = vec![command.into(), "--batch-id".into(), id.into()];
        if command == "batch-start" {
            all.extend(["--input-dir".into(), input.to_str().unwrap().into()]);
        }
        all.extend(["--workspace".into(), workspace.to_str().unwrap().into()]);
        all
    };
    let run = |all: Vec<String>| {
        json(&cli(
            ws.path(),
            &all.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
    };
    assert_eq!(
        run(args("batch-start", "s1"))["result"]["run"]["outcome"],
        "completed"
    );
    let candidates = workspace.join(".runtime/batches/normalizer-cache/candidates");
    let mut blobs: Vec<_> = fs::read_dir(&candidates)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    blobs.sort();
    assert_eq!(blobs.len(), 2, "{blobs:?}");
    fs::write(&blobs[0], b"corrupted").unwrap();
    fs::remove_file(&blobs[1]).unwrap();
    let resumed = run(args("batch-resume", "s1"));
    assert_eq!(
        resumed["result"]["run"]["outcome"], "completed",
        "{resumed}"
    );
    assert_eq!(resumed["result"]["run"]["processed_now"], 2);
    assert_eq!(resumed["result"]["run"]["reused"], 3);
    for blob in &blobs {
        let name = blob.file_name().unwrap().to_str().unwrap();
        assert_eq!(format!("{}.mp3", sha256_hex(fs::read(blob).unwrap())), name);
    }

    use std::io::Write;
    // Вариант, раздутый сверх предела 8 MiB, тоже повреждён и заменяется,
    // а не останавливает пакет нарушением границы.
    fs::OpenOptions::new()
        .append(true)
        .open(&blobs[0])
        .unwrap()
        .write_all(&vec![0u8; 9 * 1024 * 1024])
        .unwrap();
    let inflated = run(args("batch-resume", "s1"));
    assert_eq!(
        inflated["result"]["run"]["outcome"], "completed",
        "{inflated}"
    );
    assert_eq!(inflated["result"]["run"]["processed_now"], 1);
    let name = blobs[0].file_name().unwrap().to_str().unwrap();
    assert_eq!(
        format!("{}.mp3", sha256_hex(fs::read(&blobs[0]).unwrap())),
        name
    );

    // Вход изменился после инвентаризации: прежний результат не переиспользуется.
    fs::write(input.join("good.mp3"), quiet()).unwrap();
    let changed = run(args("batch-resume", "s1"));
    let run_summary = &changed["result"]["run"];
    assert_eq!(run_summary["outcome"], "paused", "{changed}");
    assert_eq!(run_summary["reused"], 4);
    assert_eq!(run_summary["remaining"], 1);
    assert!(
        changed["result"]["batch"]["progress"]["source_errors"]
            .get("good.mp3")
            .is_some()
    );
}

/// Инвентаризация учитывает все MP3 в общем пределе, а проверка неизменности
/// обнаруживает новые, удалённые, изменённые и непроверяемые записи.
#[test]
fn inventory_limits_and_source_verification_cover_the_whole_input() {
    let ws = TempWorkspace::create("normalizer-inventory").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    fs::write(input.join("big.mp3"), &big).unwrap();
    let small_total: u64 = [
        "padded.mp3",
        "good.mp3",
        "こいつ.MP3",
        "подкаталог/quiet.mp3",
        "truncated.mp3",
        "<img src=x onerror=alert(1)>.mp3",
    ]
    .iter()
    .map(|p| fs::metadata(input.join(p)).unwrap().len())
    .sum::<u64>()
        + 4096;
    // Файл больше 8 MiB не обходит общий предел.
    let limited = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "limit",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--max-total-bytes",
            &small_total.to_string(),
        ],
    );
    assert_eq!(json(&limited)["error"]["code"], "invalid_transition");
    let started = json(&cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "v1",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    ));
    assert_eq!(
        started["result"]["run"]["outcome"], "completed",
        "{started}"
    );
    let verify = || {
        json(&cli(
            ws.path(),
            &[
                "batch-verify-sources",
                "--batch-id",
                "v1",
                "--workspace",
                workspace.to_str().unwrap(),
            ],
        ))
    };
    let ok = verify();
    assert_eq!(ok["result"]["verified"], true, "{ok}");
    assert_eq!(ok["result"]["unchanged"], 7);
    // Изменённые байты слишком большого файла обнаруживаются по его хешу.
    let mut altered = big.clone();
    altered[100] ^= 1;
    fs::write(input.join("big.mp3"), &altered).unwrap();
    let details = verify()["error"]["details"].clone();
    assert_eq!(details["changed"], serde_json::json!(["big.mp3"]));
    fs::write(input.join("big.mp3"), &big).unwrap();
    // Новый MP3, удалённый MP3 и новая посторонняя запись.
    fs::write(input.join("new.mp3"), good()).unwrap();
    fs::remove_file(input.join("подкаталог/quiet.mp3")).unwrap();
    fs::write(input.join("extra.txt"), b"x").unwrap();
    let details = verify()["error"]["details"].clone();
    assert_eq!(details["verified"], false);
    assert_eq!(details["added"], serde_json::json!(["new.mp3"]));
    assert_eq!(
        details["removed"],
        serde_json::json!(["подкаталог/quiet.mp3"])
    );
    assert_eq!(details["skipped_changed"], serde_json::json!(["extra.txt"]));
    fs::remove_file(input.join("new.mp3")).unwrap();
    fs::remove_file(input.join("extra.txt")).unwrap();
    fs::write(input.join("подкаталог/quiet.mp3"), quiet()).unwrap();
    assert_eq!(verify()["result"]["verified"], true);

    // Нечитаемый при инвентаризации файл непроверяем, даже если стал читаемым.
    if !running_as_root() {
        use std::os::unix::fs::PermissionsExt;
        let hidden = input.join("hidden.mp3");
        fs::write(&hidden, good()).unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let second = json(&cli(
            ws.path(),
            &[
                "batch-start",
                "--batch-id",
                "v2",
                "--input-dir",
                input.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
            ],
        ));
        assert_eq!(second["result"]["run"]["outcome"], "completed", "{second}");
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o644)).unwrap();
        let unverifiable = json(&cli(
            ws.path(),
            &[
                "batch-verify-sources",
                "--batch-id",
                "v2",
                "--workspace",
                workspace.to_str().unwrap(),
            ],
        ));
        assert_eq!(
            unverifiable["error"]["details"]["unverifiable"],
            serde_json::json!(["hidden.mp3"])
        );
    }
}

/// Рабочий каталог, отчёт и выборка не попадают во вход и в decks/** ни
/// напрямую, ни через символическую ссылку на родителя; отказ не оставляет
/// следов. Отчёт публикуется атомарно и повторяем после сбоя.
#[test]
fn outputs_never_reach_protected_trees_and_report_is_atomic() {
    let ws = TempWorkspace::create("normalizer-bounds").unwrap();
    let input = corpus(ws.path());
    fs::create_dir_all(ws.path().join("decks/output")).unwrap();
    fs::write(ws.path().join("AGENTS.md"), b"").unwrap();
    std::os::unix::fs::symlink(ws.path().join("decks/output"), ws.path().join("to-decks")).unwrap();
    std::os::unix::fs::symlink(&input, ws.path().join("to-input")).unwrap();
    let decks_before = snapshot(&ws.path().join("decks"));
    let input_before = snapshot(&input);
    for target in ["to-decks/work", "to-input/work"] {
        let refused = cli(
            ws.path(),
            &[
                "batch-start",
                "--batch-id",
                "b",
                "--input-dir",
                input.to_str().unwrap(),
                "--workspace",
                ws.path().join(target).to_str().unwrap(),
            ],
        );
        assert_eq!(
            json(&refused)["error"]["code"],
            "boundary_violation",
            "{target}"
        );
    }
    let workspace = ws.path().join("work");
    let started = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "b",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(started.status.success());
    for (command, extra) in [
        ("batch-sample", vec!["--seed", "1"]),
        ("batch-report", vec![]),
    ] {
        for target in ["to-decks/out", "to-input/out", "work/.runtime/out"] {
            let mut all = vec![
                command,
                "--batch-id",
                "b",
                "--workspace",
                workspace.to_str().unwrap(),
            ];
            all.extend(extra.iter().copied());
            let out = ws.path().join(target);
            all.extend(["--out", out.to_str().unwrap()]);
            let refused = cli(ws.path(), &all);
            assert_eq!(
                json(&refused)["error"]["code"],
                "boundary_violation",
                "{command} {target}"
            );
        }
    }
    assert_eq!(snapshot(&ws.path().join("decks")), decks_before);
    assert_eq!(snapshot(&input), input_before);

    // Рабочий каталог, перенесённый в decks/**, не открывается ни одной подкомандой.
    let moved = ws.path().join("decks/output/work");
    fs::rename(&workspace, &moved).unwrap();
    for command in ["batch-status", "batch-resume", "batch-verify-sources"] {
        let refused = cli(
            ws.path(),
            &[
                command,
                "--batch-id",
                "b",
                "--workspace",
                moved.to_str().unwrap(),
            ],
        );
        assert_eq!(
            json(&refused)["error"]["code"],
            "boundary_violation",
            "{command}"
        );
    }
    fs::rename(&moved, &workspace).unwrap();

    // Сбой на середине копирования media не оставляет частичного отчёта.
    let report_dir = ws.path().join("report");
    let report = |out: &Path| {
        cli(
            ws.path(),
            &[
                "batch-report",
                "--batch-id",
                "b",
                "--workspace",
                workspace.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--html",
                "--include",
                "all",
            ],
        )
    };
    if !running_as_root() {
        use std::os::unix::fs::PermissionsExt;
        let source = input.join("подкаталог/quiet.mp3");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        let failed = report(&report_dir);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!failed.status.success());
        assert!(!report_dir.exists());
        let leftovers: Vec<_> = fs::read_dir(ws.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
    let done = report(&report_dir);
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stdout)
    );
    assert!(report_dir.join("report.html").exists());
    assert_eq!(snapshot(&input), input_before);
}

/// Выписка publications/ не сохранилась после фиксации в хранилище: ошибка
/// явно сообщает о совершённой публикации, а повтор идемпотентен.
#[test]
fn publication_record_failure_after_store_commit_is_repairable() {
    if running_as_root() {
        return;
    }
    let ws = TempWorkspace::create("normalizer-publish-repair").unwrap();
    let input = ws.path().join("corpus");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("padded.mp3"), padded()).unwrap();
    let workspace = ws.path().join("work");
    let started = cli(
        ws.path(),
        &[
            "batch-start",
            "--batch-id",
            "p",
            "--input-dir",
            input.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ],
    );
    assert!(started.status.success());
    let request = Pronunciation::new("こいつ", "こいつ").unwrap();
    let source_sha = sha256_hex(padded());
    {
        let store = store(&ws.path().join("store"));
        import_bytes(&store, &request, padded(), "こいつ.mp3", None).unwrap();
    }
    let publications = workspace.join(".runtime/batches/normalizer-cache/publications");
    fs::create_dir_all(&publications).unwrap();
    let restore = ClosedDirectory::close(&publications);
    let publish = || {
        cli(
            ws.path(),
            &[
                "publish",
                "--batch-id",
                "p",
                "--workspace",
                workspace.to_str().unwrap(),
                "--input-sha256",
                &source_sha,
                "--surface",
                "こいつ",
                "--reading",
                "こいつ",
                "--replace-expected-sha256",
                &source_sha,
            ],
        )
    };
    let failed_output = publish();
    drop(restore);
    let failed = json(&failed_output);
    let details = &failed["error"]["details"];
    assert_eq!(details["store_committed"], true, "{failed}");
    let published = details["published_sha256"].as_str().unwrap().to_owned();
    let store_view = store(&ws.path().join("store"));
    let current = derivation::current_record(&store_view, &request)
        .unwrap()
        .unwrap();
    assert_eq!(current.sha256, published);
    for _ in 0..2 {
        let repaired = json(&publish());
        assert_eq!(repaired["result"]["already_published"], true, "{repaired}");
        assert_eq!(repaired["result"]["asset"]["sha256"], published.as_str());
    }
    assert!(
        publications
            .join(format!("{source_sha}.{published}.json"))
            .exists()
    );
    let after = derivation::current_record(&store_view, &request)
        .unwrap()
        .unwrap();
    assert_eq!(after.sha256, current.sha256);
    assert_eq!(after.lifecycle, current.lifecycle);
}

/// Каталог без права записи на время проверки; права возвращаются и при
/// панике, иначе `TempWorkspace` не смог бы убрать содержимое.
struct ClosedDirectory<'a>(&'a Path);

impl<'a> ClosedDirectory<'a> {
    fn close(path: &'a Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
        Self(path)
    }
}

impl Drop for ClosedDirectory<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o700));
    }
}

fn store(path: &Path) -> AssetStore {
    AssetStore::open_with_policy(StoreOptions::new(path), PronunciationDomainPolicy).unwrap()
}

fn attest(store: &AssetStore, request: &Pronunciation, sha: &str) {
    store
        .attest(HumanAttestationRequest {
            identity: request.identity(),
            expected_sha256: sha.into(),
            decision: HumanDecision::Approve,
            reason: "Прослушано: ожидаемое чтение".into(),
        })
        .unwrap();
}

#[test]
fn publish_requires_cas_keeps_provenance_and_never_inherits_approval() {
    let ws = TempWorkspace::create("normalizer-publish").unwrap();
    let store = store(&ws.path().join("store"));
    let request = Pronunciation::new("こいつ", "こいつ").unwrap();
    let original = import_bytes(
        &store,
        &request,
        padded(),
        "pronunciation_ja_こいつ.mp3",
        None,
    )
    .unwrap();
    attest(&store, &request, &original.sha256);
    let profile = NormalizerProfile::balanced();
    let processed = process(padded(), &profile, Mode::Normalize, &toolchain()).unwrap();
    let output = processed.output_bytes.clone().unwrap();
    let publish = |replace: &str, allow: bool| {
        derivation::publish(derivation::PublishRequest {
            store: &store,
            request: &request,
            result: &processed.result,
            profile: &profile,
            output_bytes: &output,
            source_bytes: padded(),
            replace_expected_sha256: replace,
            allow_replace_verified: allow,
            override_gate: false,
        })
    };
    // Подтверждённый ресурс не заменяется без явного флага; превью его не трогает.
    assert_eq!(
        publish(&original.sha256, false).unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(
        publish(&"0".repeat(64), true).unwrap_err().code,
        ErrorCode::IdentityConflict
    );
    let current = derivation::current_record(&store, &request)
        .unwrap()
        .unwrap();
    assert_eq!(current.sha256, original.sha256);
    assert_eq!(current.lifecycle, LifecycleState::Verified);
    let trusted =
        asset_store::pronunciation::read_trusted(store.root(), std::slice::from_ref(&request))
            .unwrap();
    assert_eq!(trusted[0].record.sha256, original.sha256);

    let derived = publish(&original.sha256, true).unwrap();
    assert_eq!(derived.sha256, sha256_hex(&output));
    assert_eq!(derived.provenance.source_kind, "audio_derivative");
    assert_eq!(derived.lifecycle, LifecycleState::Quarantined);
    assert!(derived.current_human_decision().is_none());
    let derivation = &derived.domain_metadata.as_ref().unwrap()["derivation"];
    assert_eq!(derivation["source_sha256"], original.sha256);
    assert_eq!(
        derivation["source_provenance"]["source_kind"],
        "local_import"
    );
    assert_eq!(derivation["gate"]["outcome"], "ACCEPT");
    // Новый SHA не доверен до нового прослушивания.
    assert!(
        asset_store::pronunciation::read_trusted(store.root(), std::slice::from_ref(&request))
            .is_err()
    );
    attest(&store, &request, &derived.sha256);
    let trusted =
        asset_store::pronunciation::read_trusted(store.root(), std::slice::from_ref(&request))
            .unwrap();
    assert_eq!(trusted[0].record.sha256, derived.sha256);
    // Перепроверка корпуса принимает производную запись и её свидетельство.
    assert!(
        store
            .validate(
                SelectionMode::Full,
                &asset_store::pronunciation::AudioTechnicalValidator
            )
            .unwrap()
            .blockers
            .is_empty()
    );
    // Тот же профиль не предлагает нового варианта для уже выровненного.
    let again = process(&output, &profile, Mode::Normalize, &toolchain()).unwrap();
    assert_eq!(again.result.gate.outcome, GateOutcome::KeepOriginal);
    assert!(again.output_bytes.is_none());
    // Другая цель даёт новый вариант, но публикация поверх производного
    // варианта запрещена: CAS, байты и гейт при этом корректны.
    let mut quieter = profile.clone();
    quieter.loudness.target_lufs -= 6.0;
    let reprocessed = process(&output, &quieter, Mode::Normalize, &toolchain()).unwrap();
    assert_eq!(reprocessed.result.gate.outcome, GateOutcome::Accept);
    let refused = derivation::publish(derivation::PublishRequest {
        store: &store,
        request: &request,
        result: &reprocessed.result,
        profile: &quieter,
        output_bytes: reprocessed.output_bytes.as_deref().unwrap(),
        source_bytes: &output,
        replace_expected_sha256: &derived.sha256,
        allow_replace_verified: true,
        override_gate: false,
    })
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidTransition);
    assert!(refused.message.contains("производного варианта"));
    // KEEP_ORIGINAL не публикуется: остальные условия запроса допустимы.
    let kept = process(good(), &profile, Mode::Normalize, &toolchain()).unwrap();
    assert_eq!(kept.result.gate.outcome, GateOutcome::KeepOriginal);
    let kept_refused = derivation::publish(derivation::PublishRequest {
        store: &store,
        request: &request,
        result: &kept.result,
        profile: &profile,
        output_bytes: kept.output_bytes.as_deref().unwrap_or_default(),
        source_bytes: good(),
        replace_expected_sha256: &derived.sha256,
        allow_replace_verified: true,
        override_gate: false,
    })
    .unwrap_err();
    assert_eq!(kept_refused.code, ErrorCode::InvalidIdentity);
    assert!(kept_refused.message.contains("нет обработанного варианта"));
    // Пары нет в хранилище: результат, байты и CAS при этом корректны.
    let unknown_refused = derivation::publish(derivation::PublishRequest {
        store: &store,
        request: &Pronunciation::new("良い", "よい").unwrap(),
        result: &processed.result,
        profile: &profile,
        output_bytes: &output,
        source_bytes: padded(),
        replace_expected_sha256: &original.sha256,
        allow_replace_verified: true,
        override_gate: false,
    })
    .unwrap_err();
    assert_eq!(unknown_refused.code, ErrorCode::InvalidTransition);
    assert!(unknown_refused.message.contains("нет записи этой пары"));
}

#[test]
fn derived_record_keeps_forvo_provenance_of_the_source() {
    use asset_store::forvo::*;
    let ws = TempWorkspace::create("normalizer-forvo").unwrap();
    let store = store(&ws.path().join("store"));
    let request = Pronunciation::new("こいつ", "こいつ").unwrap();
    let acquisition = Acquisition {
        bytes: padded().to_vec(),
        evidence: ForvoEvidence {
            schema_version: 1,
            page_url: word_url(&request.surface),
            downloaded_sha256: sha256_hex(padded()),
            selected_record_id: "101".into(),
            ranking_policy: RANKING.into(),
            candidates: vec![Candidate {
                record_id: "101".into(),
                username: Some("fixture-speaker".into()),
                votes: Some(1),
                gender: Some(Gender::Female),
                country: Some("Japan".into()),
                download_available: true,
            }],
            attempts: vec![Attempt {
                record_id: "101".into(),
                code: "downloaded".into(),
            }],
        },
    };
    let source = import_acquisition(&store, &request, &acquisition, None).unwrap();
    let profile = NormalizerProfile::balanced();
    let processed = process(padded(), &profile, Mode::Normalize, &toolchain()).unwrap();
    let derived = derivation::publish(derivation::PublishRequest {
        store: &store,
        request: &request,
        result: &processed.result,
        profile: &profile,
        output_bytes: processed.output_bytes.as_ref().unwrap(),
        source_bytes: padded(),
        replace_expected_sha256: &source.sha256,
        allow_replace_verified: false,
        override_gate: false,
    })
    .unwrap();
    let derivation = &derived.domain_metadata.as_ref().unwrap()["derivation"];
    assert_eq!(derivation["source_provenance"]["source_kind"], "forvo_web");
    assert_eq!(
        derivation["source_metadata"]["forvo"]["downloaded_sha256"],
        source.sha256
    );
    // Производный MP3 не выдаётся за исходный forvo_web.
    assert_eq!(derived.provenance.source_kind, "audio_derivative");
    assert_ne!(
        derived.provenance.source_name,
        source.provenance.source_name
    );
    assert!(
        store
            .verify_integrity()
            .unwrap()
            .iter()
            .any(|r| r.sha256 == derived.sha256)
    );
}

/// `available` страты крайних значений считает только элементы с конечной
/// метрикой сортировки: запись без измеримой громкости в «самые тихие» не
/// попадает и доступной не считается.
#[test]
fn sample_strata_count_only_selectable_items() {
    use asset_store::pronunciation::normalizer_report as report;
    let ws = TempWorkspace::create("normalizer-sample").unwrap();
    let input = ws.path().join("corpus");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("good.mp3"), good()).unwrap();
    fs::write(
        input.join("tiny.mp3"),
        encode(
            "tiny",
            "sine=frequency=440:duration=0.06:sample_rate=44100,volume=0.8",
        ),
    )
    .unwrap();
    let workspace = ws.path().join("work");
    fs::create_dir(&workspace).unwrap();
    nbatch::start(nbatch::StartRequest {
        batch_id: "s",
        workspace: &workspace,
        input_root: &fs::canonicalize(&input).unwrap(),
        mode: Mode::Analyze,
        profile: NormalizerProfile::balanced(),
        limits: nbatch::Limits {
            jobs: 1,
            max_files: 10,
            max_total_bytes: 1 << 20,
        },
    })
    .unwrap();
    let stop = AtomicBool::new(false);
    assert_eq!(
        nbatch::resume(&workspace, "s", None, None, &stop)
            .unwrap()
            .outcome,
        "completed"
    );
    let (batch, inventory) = nbatch::load(&workspace, "s").unwrap();
    let cache = nbatch::ResultCache::open(&workspace).unwrap();
    let results = nbatch::collect_results(&cache, &batch, &inventory).unwrap();
    let tiny = results
        .iter()
        .find(|r| {
            r.plan
                .as_ref()
                .is_some_and(|p| p.gain.measured_lufs.is_none())
        })
        .expect("короткий клип без измеримой громкости");
    assert!(tiny.analysis.is_some());
    let sample = report::sample(&batch, &inventory, &results, 1, 5);
    let quietest = sample.strata.iter().find(|s| s.id == "quietest").unwrap();
    assert_eq!(quietest.available, 1);
    assert_eq!(quietest.selected.len(), 1);
    assert_ne!(quietest.selected[0], tiny.input.sha256);
    let shortest = sample.strata.iter().find(|s| s.id == "shortest").unwrap();
    assert_eq!(shortest.available, 2);
}
