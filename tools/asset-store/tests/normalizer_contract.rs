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

    // Прерывание до начала работы ничего не помечает завершённым.
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
    // Один вход уже обработан пакетом partial: кэш общий.
    assert_eq!(interrupted.processed_now, 0);
    let stop = AtomicBool::new(false);
    let finished = nbatch::resume(&workspace, "interrupted", None, None, &stop).unwrap();
    assert_eq!(finished.outcome, "completed");
    assert_eq!(finished.processed_now + finished.reused, 5);

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

#[test]
fn missing_ffmpeg_and_unwritable_workspace_fail_before_any_state() {
    let ws = TempWorkspace::create("normalizer-env").unwrap();
    let input = corpus(ws.path());
    let workspace = ws.path().join("work");
    let tools = ws.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pronunciation-assets"))
        .arg("--repository-root")
        .arg(ws.path())
        .args([
            "normalizer",
            "batch-start",
            "--batch-id",
            "nofm",
            "--input-dir",
        ])
        .arg(&input)
        .arg("--workspace")
        .arg(&workspace)
        .env("PATH", &tools)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        json(&output)["error"]["details"]["blocker"],
        "audio_tool_missing"
    );
    assert!(!workspace.join(".runtime/batches/nofm").exists());

    use std::os::unix::fs::PermissionsExt;
    let locked = ws.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
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
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!denied.status.success());
    assert!(matches!(
        json(&denied)["error"]["code"].as_str(),
        Some("io_failure" | "boundary_violation")
    ));
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
    assert!(publish(&original.sha256, false).is_err());
    assert!(publish(&"0".repeat(64), true).is_err());
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
    // Повторная обработка производного варианта не допускается.
    let again = process(&output, &profile, Mode::Normalize, &toolchain()).unwrap();
    let again_output = again.output_bytes.clone();
    if let Some(bytes) = again_output {
        let refused = derivation::publish(derivation::PublishRequest {
            store: &store,
            request: &request,
            result: &again.result,
            profile: &profile,
            output_bytes: &bytes,
            source_bytes: &output,
            replace_expected_sha256: &derived.sha256,
            allow_replace_verified: true,
            override_gate: true,
        });
        assert!(refused.is_err());
    }
    // KEEP_ORIGINAL и неизвестная пара не публикуются.
    let kept = process(good(), &profile, Mode::Normalize, &toolchain()).unwrap();
    assert!(
        derivation::publish(derivation::PublishRequest {
            store: &store,
            request: &Pronunciation::new("良い", "よい").unwrap(),
            result: &kept.result,
            profile: &profile,
            output_bytes: b"",
            source_bytes: good(),
            replace_expected_sha256: &"0".repeat(64),
            allow_replace_verified: false,
            override_gate: false,
        })
        .is_err()
    );
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
