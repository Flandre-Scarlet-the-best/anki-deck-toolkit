//! Этап улучшения речи на настоящих синтетических MP3 и поддельных воркерах
//! с известным поведением: маршрутизация, точные байты входа, одно
//! кодирование, ключ кэша, сбои backend и границы. Настоящий инференс
//! DeepFilterNet3 проверяют тесты `tools/speech-enhance`.
use asset_store::hashing::sha256_hex;
use asset_store::pronunciation::enhancer::{EnhancementOutcome, EnhancementProfile, Selection};
use asset_store::pronunciation::enhancer_backend::{
    BackendKind, DEEPFILTERNET3_MANIFEST_SHA256, RESEMBLE_DENOISE_MANIFEST_SHA256,
};
use asset_store::pronunciation::enhancer_batch as ebatch;
use asset_store::pronunciation::enhancer_report as ereport;
use asset_store::pronunciation::normalizer::{Mode, NormalizerProfile, Variant};
use asset_store::pronunciation::normalizer_batch as nbatch;
use asset_store::temp_workspace::TempWorkspace;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

const RATE: &str = "48000";

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

/// Речеподобный чирп с огибающей (сдвиг по корреляции однозначен, в отличие
/// от чистого синуса) и паузами 0.3 с по краям, по желанию с белым шумом на
/// всей длине.
fn tone_mp3(path: &Path, noise: Option<f64>) {
    let tone = format!(
        "aevalsrc='0.25*sin(PI*t/0.8)*(sin(2*PI*(180+400*t)*t)+0.5*sin(2*PI*(900+1500*t)*t))':duration=0.8:sample_rate={RATE},adelay=300:all=1,apad=pad_dur=0.3"
    );
    let graph = match noise {
        None => format!("{tone}[out0]"),
        Some(amplitude) => format!(
            "{tone}[s];anoisesrc=duration=1.4:color=white:amplitude={amplitude}:sample_rate={RATE}:seed=7[n];[s][n]amix=inputs=2:normalize=0:duration=first[out0]"
        ),
    };
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &graph,
            "-ac",
            "1",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
        ],
        path,
    );
}

fn decode_f32(mp3: &Path, out: &Path) {
    ffmpeg(
        &[
            "-i",
            mp3.to_str().unwrap(),
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
        ],
        out,
    );
}

fn silent_mp3(path: &Path) {
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("anullsrc=sample_rate={RATE}:channel_layout=mono"),
            "-t",
            "1",
            "-c:a",
            "libmp3lame",
        ],
        path,
    );
}

struct Fixture {
    _ws: TempWorkspace,
    root: PathBuf,
    input: PathBuf,
    workspace: PathBuf,
    model_dir: PathBuf,
    /// Декодированный PCM чистого эталона шумной записи.
    clean_f32: PathBuf,
}

/// Корпус: чистая запись, шумная запись и её дубль, тишина.
fn fixture(name: &str) -> Fixture {
    let ws = TempWorkspace::create(name).unwrap();
    let root = ws.path().to_path_buf();
    let input = root.join("corpus");
    fs::create_dir_all(&input).unwrap();
    tone_mp3(&input.join("clean.mp3"), None);
    tone_mp3(&input.join("noisy.mp3"), Some(0.006));
    fs::copy(input.join("noisy.mp3"), input.join("noisy-copy.mp3")).unwrap();
    silent_mp3(&input.join("silent.mp3"));
    let reference = root.join("reference.mp3");
    tone_mp3(&reference, None);
    let clean_f32 = root.join("clean.f32");
    decode_f32(&reference, &clean_f32);
    let model_dir = root.join("models");
    fs::create_dir_all(&model_dir).unwrap();
    let workspace = root.join("work");
    fs::create_dir_all(&workspace).unwrap();
    Fixture {
        _ws: ws,
        root,
        input,
        workspace,
        model_dir,
        clean_f32,
    }
}

/// Поддельный воркер по протоколу `speech-enhance-dfn`. `process` задаёт
/// поведение обработки; файл `fail-flag` рядом со скриптом включает аварию
/// (тот же исполняемый файл, поэтому идентичность не меняется).
fn fake_worker(root: &Path, name: &str, variant: &str, process: &str) -> PathBuf {
    let path = root.join(name);
    let flag = root.join("fail-flag");
    let script = format!(
        r#"#!/bin/bash
cmd=$1; shift
while [ $# -gt 0 ]; do
  case $1 in
    --model-dir) md=$2; shift 2;;
    --input) in=$2; shift 2;;
    --output) out=$2; shift 2;;
    --channels) ch=$2; shift 2;;
    *) shift;;
  esac
done
if [ ! -d "$md" ]; then echo '{{"schema_version":1,"kind":"speech_enhance_error","code":"model_missing","message":"нет"}}'; exit 3; fi
case $cmd in
identify)
  echo '{{"schema_version":1,"kind":"speech_enhance_identity","backend":"deepfilternet3","worker":{{"name":"fake","version":"0","protocol":1}},"runtime":{{"device":"cpu"}},"model":{{"name":"DeepFilterNet3","variant":"{variant}","manifest_sha256":"{DEEPFILTERNET3_MANIFEST_SHA256}","source":{{}},"license":"MIT OR Apache-2.0"}},"stream":{{"sample_rate":48000,"hop_samples":480,"delay_samples":1440,"max_channels":2}}}}'
  ;;
process)
  if [ -e "{flag}" ]; then kill -SEGV $$; fi
  {process}
  frames=$(( $(stat -c %s "$in") / 4 / ch ))
  sha=$(sha256sum "$out" | cut -d' ' -f1)
  echo "{{\"schema_version\":1,\"kind\":\"speech_enhance_result\",\"model_manifest_sha256\":\"{DEEPFILTERNET3_MANIFEST_SHA256}\",\"params\":{{}},\"diagnostics\":{{\"channels\":$ch,\"input_frames\":$frames,\"output_frames\":$frames,\"delay_samples\":1440,\"flush_samples\":1500,\"identical_channels\":true,\"per_channel\":[{{\"lsnr_mean_db\":10.0}}]}},\"output_sha256\":\"$sha\"}}"
  ;;
esac
"#,
        flag = flag.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Поддельный воркер Resemble Enhance (denoiser): своя идентичность, нет
/// параметров DeepFilterNet3 — их появление в вызове роняет процесс.
fn fake_resemble_worker(root: &Path, name: &str, process: &str) -> PathBuf {
    fake_resemble_worker_on(root, name, process, "cpu")
}

/// `device` — устройство, о котором воркер отчитывается в `process`;
/// `identify` всегда сообщает `cpu`.
fn fake_resemble_worker_on(root: &Path, name: &str, process: &str, device: &str) -> PathBuf {
    let path = root.join(name);
    let script = format!(
        r#"#!/bin/bash
cmd=$1; shift
while [ $# -gt 0 ]; do
  case $1 in
    --model-dir) md=$2; shift 2;;
    --input) in=$2; shift 2;;
    --output) out=$2; shift 2;;
    --channels) ch=$2; shift 2;;
    --atten-lim-db|--post-filter-beta|--context-ms) exit 9;;
    *) shift;;
  esac
done
case $cmd in
identify)
  echo '{{"schema_version":1,"kind":"speech_enhance_identity","backend":"resemble-enhance-denoise","worker":{{"name":"fake","version":"0","protocol":1}},"runtime":{{"device":"cpu"}},"model":{{"name":"Resemble Enhance","variant":"denoiser","manifest_sha256":"{RESEMBLE_DENOISE_MANIFEST_SHA256}","source":{{}},"license":"MIT"}},"stream":{{"sample_rate":48000,"hop_samples":1,"delay_samples":0,"max_channels":2}}}}'
  ;;
process)
  {process}
  frames=$(( $(stat -c %s "$in") / 4 / ch ))
  sha=$(sha256sum "$out" | cut -d' ' -f1)
  echo "{{\"schema_version\":1,\"kind\":\"speech_enhance_result\",\"model_manifest_sha256\":\"{RESEMBLE_DENOISE_MANIFEST_SHA256}\",\"params\":{{}},\"diagnostics\":{{\"channels\":$ch,\"input_frames\":$frames,\"output_frames\":$frames,\"delay_samples\":0,\"flush_samples\":0,\"identical_channels\":false,\"device\":\"{device}\",\"per_channel\":[{{}}]}},\"output_sha256\":\"$sha\"}}"
  ;;
esac
"#
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn base_batch(f: &Fixture) -> nbatch::NormalizerBatch {
    let started = nbatch::start(nbatch::StartRequest {
        batch_id: "base",
        workspace: &f.workspace,
        input_root: &f.input,
        mode: Mode::Normalize,
        profile: NormalizerProfile::balanced(),
        limits: nbatch::Limits {
            jobs: 2,
            max_files: 100,
            max_total_bytes: 1 << 30,
        },
    })
    .unwrap();
    let run =
        nbatch::resume_started(&f.workspace, &started, None, &AtomicBool::new(false)).unwrap();
    assert_eq!(run.outcome, "completed");
    run.batch
}

fn start(
    f: &Fixture,
    id: &str,
    worker: &Path,
    profile: EnhancementProfile,
) -> Result<ebatch::RunSummary, asset_store::AssetError> {
    let started = ebatch::start(ebatch::StartRequest {
        batch_id: id,
        workspace: &f.workspace,
        base_batch_id: "base",
        profile,
        worker,
        model_dir: &f.model_dir,
        jobs: 2,
    })?;
    ebatch::resume(
        &f.workspace,
        &started.batch_id,
        None,
        None,
        &AtomicBool::new(false),
    )
}

fn results(
    f: &Fixture,
    id: &str,
) -> BTreeMap<String, asset_store::pronunciation::enhancer::EnhancementResult> {
    let (batch, _, inventory) = ebatch::load(&f.workspace, id).unwrap();
    let cache = ebatch::EnhancerCache::open(&f.workspace).unwrap();
    let all = ebatch::collect_results(&cache, &batch, &inventory).unwrap();
    let mut by_path = BTreeMap::new();
    for entry in &inventory.entries {
        if let Some(sha) = &entry.sha256
            && let Some(r) = all.iter().find(|r| &r.input.sha256 == sha)
        {
            by_path.insert(entry.path.clone(), r.clone());
        }
    }
    by_path
}

fn tree_hashes(dir: &Path) -> BTreeMap<String, String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                sha256_hex(fs::read(&p).unwrap()),
            )
        })
        .collect()
}

#[test]
fn copy_backend_routes_every_input_and_keeps_exact_selected_bytes() {
    let f = fixture("enhancer-copy");
    let before = tree_hashes(&f.input);
    base_batch(&f);
    let worker = fake_worker(
        &f.root,
        "copy-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    let run = start(&f, "enh", &worker, EnhancementProfile::conservative()).unwrap();
    assert_eq!(run.outcome, "completed", "{run:?}");
    // Четыре файла, три уникальных SHA (дубль шумной записи).
    assert_eq!(run.processed_now, 3);
    let r = results(&f, "enh");
    let clean = &r["clean.mp3"];
    assert_eq!(clean.gate.outcome, EnhancementOutcome::NotNeeded);
    assert_eq!(clean.gate.selected, Some(Selection::Input));
    assert!(clean.inference.is_none());
    // Копия входа не снижает шум: нейровариант отклонён, вход сохранён, шум
    // передан следующей автоматической стадии.
    let noisy = &r["noisy.mp3"];
    assert_eq!(noisy.gate.outcome, EnhancementOutcome::EnhancementRejected);
    assert_eq!(
        noisy.gate.next_action.as_str(),
        "deferred_automatic_processing"
    );
    assert!(
        noisy
            .gate
            .reasons
            .iter()
            .any(|x| x.code == "enhancement_benefit_insufficient")
    );
    assert!(noisy.output.is_none() && noisy.balanced.is_none());
    let silent = &r["silent.mp3"];
    assert_eq!(silent.gate.outcome, EnhancementOutcome::InputRejected);
    assert_eq!(silent.gate.selected, None);
    // Выбранные байты — точные байты, выбранные Balanced.
    for result in r.values() {
        if let Some(selected) = &result.selected_input {
            match selected.variant {
                Variant::Original => assert_eq!(selected.sha256, result.input.sha256),
                Variant::Processed => assert_ne!(selected.sha256, result.input.sha256),
            }
        }
    }
    // Отчёт различает уникальные SHA и физические файлы.
    let (batch, base, inventory) = ebatch::load(&f.workspace, "enh").unwrap();
    let cache = ebatch::EnhancerCache::open(&f.workspace).unwrap();
    let all = ebatch::collect_results(&cache, &batch, &inventory).unwrap();
    drop(cache);
    let summary = ereport::summarize(&batch, &inventory, &all);
    assert!(summary.completeness.complete);
    assert_eq!(summary.completeness.unique_sha256, 3);
    assert_eq!(summary.completeness.readable_files, 4);
    assert_eq!(summary.counts.outcomes["ENHANCEMENT_REJECTED"], 1);
    assert_eq!(summary.counts.outcomes_by_files["ENHANCEMENT_REJECTED"], 2);
    assert_eq!(summary.model["model_variant"], "deepfilternet3");
    let report = f.root.join("report");
    ereport::write_report(&batch, &inventory, &all, &report).unwrap();
    assert!(report.join("summary.json").is_file() && report.join("items.tsv").is_file());
    assert!(ereport::write_report(&batch, &inventory, &all, &report).is_err());
    // Вход не изменён.
    assert!(ebatch::verify_sources(&base, &inventory).unwrap().verified);
    assert_eq!(tree_hashes(&f.input), before);
    // Повтор не повторяет работу; другой профиль — другой ключ, без ложного попадания.
    let again = ebatch::resume(&f.workspace, "enh", None, None, &AtomicBool::new(false)).unwrap();
    assert_eq!((again.processed_now, again.reused), (0, 3));
    let mut profile = EnhancementProfile::conservative();
    profile.version = 2;
    let other = start(&f, "enh-v2", &worker, profile).unwrap();
    assert_eq!((other.processed_now, other.reused), (3, 0));
}

#[test]
fn ideal_backend_is_accepted_with_single_encoding_and_provenance() {
    let f = fixture("enhancer-ideal");
    base_batch(&f);
    let worker = fake_worker(
        &f.root,
        "ideal-worker",
        "deepfilternet3",
        &format!(r#"cp "{}" "$out""#, f.clean_f32.display()),
    );
    let run = start(&f, "enh", &worker, EnhancementProfile::conservative()).unwrap();
    assert_eq!(run.outcome, "completed", "{run:?}");
    let r = results(&f, "enh");
    let noisy = &r["noisy.mp3"];
    assert_eq!(
        noisy.gate.outcome,
        EnhancementOutcome::Enhanced,
        "{:#?}",
        noisy.qc
    );
    assert_eq!(noisy.gate.selected, Some(Selection::Enhanced));
    assert_eq!(noisy.gate.next_action.as_str(), "use_enhanced");
    let qc = noisy.qc.as_ref().unwrap();
    assert!(qc.passed);
    assert!(qc.noise_floor_before_dbfs - qc.noise_floor_after_dbfs > 20.0);
    // Цепочка происхождения: исходник → Balanced → backend → итоговый MP3.
    let balanced = noisy.balanced.as_ref().unwrap();
    assert_eq!(balanced.input.sha256, noisy.input.sha256);
    assert_eq!(
        noisy.backend.model_manifest_sha256,
        DEEPFILTERNET3_MANIFEST_SHA256
    );
    let output = noisy.output.as_ref().unwrap();
    assert_eq!(
        balanced.output.as_ref().map(|o| o.sha256.as_str()),
        Some(output.sha256.as_str())
    );
    let cache = ebatch::EnhancerCache::open(&f.workspace).unwrap();
    let bytes = cache.variant_bytes(output).unwrap();
    assert_eq!(sha256_hex(&bytes), output.sha256);
    // Один шаг кодирования из PCM: усиление Balanced над улучшенным сигналом
    // доводит громкость до цели, а не накладывается второй раз.
    let after = balanced
        .post_qc
        .as_ref()
        .unwrap()
        .analysis
        .as_ref()
        .unwrap();
    let loudness = after.integrated_lufs.or(after.short_window_lufs).unwrap();
    assert!((loudness - (-23.0)).abs() < 1.5, "{loudness}");
}

#[test]
fn tampered_cache_entry_is_not_trusted_and_is_recomputed() {
    let f = fixture("enhancer-tamper");
    base_batch(&f);
    let worker = fake_worker(
        &f.root,
        "copy-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    start(&f, "enh", &worker, EnhancementProfile::conservative()).unwrap();
    let results_dir = f.workspace.join(".runtime/batches/enhancer-cache/results");
    let mut forged = 0;
    for entry in fs::read_dir(&results_dir).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        if text.contains("ENHANCEMENT_REJECTED") {
            // Подделка: нейровариант «принят» без вывода и пост-QC.
            fs::write(
                &path,
                text.replace("\"ENHANCEMENT_REJECTED\"", "\"ENHANCED\"")
                    .replace("\"input\"", "\"enhanced\""),
            )
            .unwrap();
            forged += 1;
        }
    }
    assert_eq!(forged, 1);
    let run = ebatch::resume(&f.workspace, "enh", None, None, &AtomicBool::new(false)).unwrap();
    assert_eq!((run.processed_now, run.reused), (1, 2));
    let r = results(&f, "enh");
    assert_eq!(
        r["noisy.mp3"].gate.outcome,
        EnhancementOutcome::EnhancementRejected
    );
}

#[test]
fn backend_crash_is_retried_after_recovery_and_never_cached() {
    let f = fixture("enhancer-crash");
    base_batch(&f);
    let worker = fake_worker(
        &f.root,
        "flaky-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    fs::write(f.root.join("fail-flag"), b"").unwrap();
    let run = start(&f, "enh", &worker, EnhancementProfile::conservative()).unwrap();
    assert_eq!(run.outcome, "paused");
    assert_eq!(run.infrastructure_failed_now, 1);
    assert_eq!(run.remaining, 1);
    assert_eq!(run.batch.progress.infrastructure_failures.len(), 1);
    fs::remove_file(f.root.join("fail-flag")).unwrap();
    let run = ebatch::resume(&f.workspace, "enh", None, None, &AtomicBool::new(false)).unwrap();
    assert_eq!(run.outcome, "completed");
    assert_eq!((run.processed_now, run.reused), (1, 2));
    assert!(run.batch.progress.infrastructure_failures.is_empty());
}

#[test]
fn timeout_and_wrong_length_are_infrastructure_failures() {
    for (name, process, timeout) in [
        ("slow-worker", "sleep 5; cp \"$in\" \"$out\"", 1),
        // Лидер выходит успешно, но фоновый потомок держит stdout.
        ("orphan-worker", "(sleep 5) & cp \"$in\" \"$out\"", 1),
        ("short-worker", "head -c 400 \"$in\" > \"$out\"", 60),
        (
            "garbage-worker",
            "cp \"$in\" \"$out\"; echo garbage; exit 0",
            60,
        ),
    ] {
        let f = fixture(&format!("enhancer-{name}"));
        base_batch(&f);
        let worker = fake_worker(&f.root, name, "deepfilternet3", process);
        let mut profile = EnhancementProfile::conservative();
        profile.backend.timeout_seconds = timeout;
        let begun = std::time::Instant::now();
        let run = start(&f, "enh", &worker, profile).unwrap();
        if timeout == 1 {
            // Потомок воркера (`sleep`) завершается вместе с группой и не
            // держит каналы до конца собственного сна.
            assert!(
                begun.elapsed() < std::time::Duration::from_secs(4),
                "{name}: предел времени не завершил группу воркера: {:?}",
                begun.elapsed()
            );
        }
        assert_eq!(run.outcome, "paused", "{name}");
        assert_eq!(run.infrastructure_failed_now, 1, "{name}");
        let r = results(&f, "enh");
        assert!(!r.contains_key("noisy.mp3"), "{name}: сбой попал в кэш");
    }
}

#[test]
fn wrong_model_missing_model_and_missing_worker_fail_before_state() {
    let f = fixture("enhancer-identity");
    base_batch(&f);
    let df2 = fake_worker(
        &f.root,
        "df2-worker",
        "deepfilternet2",
        r#"cp "$in" "$out""#,
    );
    let error = start(&f, "enh", &df2, EnhancementProfile::conservative()).unwrap_err();
    assert_eq!(
        error.details["blocker"], "enhancer_identity_mismatch",
        "{error:?}"
    );
    let good = fake_worker(
        &f.root,
        "good-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    let missing_model = ebatch::start(ebatch::StartRequest {
        batch_id: "enh",
        workspace: &f.workspace,
        base_batch_id: "base",
        profile: EnhancementProfile::conservative(),
        worker: &good,
        model_dir: &f.root.join("no-such-models"),
        jobs: 1,
    })
    .unwrap_err();
    assert_eq!(
        missing_model.details["blocker"],
        "enhancer_model_unavailable"
    );
    let absent = asset_store::pronunciation::enhancer_backend::locate_worker(
        Some(&f.root.join("absent")),
        BackendKind::DeepFilterNet3,
    )
    .unwrap_err();
    assert_eq!(absent.details["blocker"], "enhancer_backend_unavailable");
    // Ни один отказ не создал состояние пакета.
    assert!(ebatch::load(&f.workspace, "enh").is_err());
}

#[test]
fn model_dir_inside_input_or_protected_tree_is_a_boundary_violation() {
    let f = fixture("enhancer-boundary");
    let protected: std::collections::BTreeSet<PathBuf> = [f.root.join("decks")].into();
    fs::create_dir_all(f.root.join("decks/models")).unwrap();
    // В сообщении — метка случая, а не путь: путь временного каталога
    // содержит идентификатор пользователя.
    for (case, dir) in [
        ("внутри входа", f.input.join("models")),
        ("внутри decks", f.root.join("decks/models")),
        ("внутри рабочего каталога", f.workspace.join("m")),
    ] {
        let error = ebatch::check_model_dir(&dir, &f.input, &f.workspace, &protected).unwrap_err();
        assert_eq!(
            error.code,
            asset_store::ErrorCode::BoundaryViolation,
            "{case}"
        );
    }
    assert!(ebatch::check_model_dir(&f.model_dir, &f.input, &f.workspace, &protected).is_ok());
}

#[test]
fn incomplete_base_batch_is_refused_with_missing_count() {
    let f = fixture("enhancer-incomplete");
    let started = nbatch::start(nbatch::StartRequest {
        batch_id: "base",
        workspace: &f.workspace,
        input_root: &f.input,
        mode: Mode::Normalize,
        profile: NormalizerProfile::balanced(),
        limits: nbatch::Limits {
            jobs: 1,
            max_files: 100,
            max_total_bytes: 1 << 30,
        },
    })
    .unwrap();
    nbatch::resume_started(&f.workspace, &started, Some(1), &AtomicBool::new(false)).unwrap();
    let worker = fake_worker(
        &f.root,
        "copy-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    let error = start(&f, "enh", &worker, EnhancementProfile::conservative()).unwrap_err();
    assert_eq!(error.details["blocker"], "base_batch_incomplete");
    assert_eq!(error.details["missing_results"], 2);
}

#[test]
fn resemble_profile_keeps_df3_fingerprint_and_rejects_foreign_params() {
    // Поле вида backend не появляется в профиле DF3: его отпечаток прежний.
    let df3 = serde_json::to_value(EnhancementProfile::conservative()).unwrap();
    assert!(df3["backend"].get("kind").is_none(), "{df3}");
    let resemble = EnhancementProfile::resemble_denoise();
    resemble.validate().unwrap();
    let json = serde_json::to_value(&resemble).unwrap();
    assert_eq!(json["backend"]["kind"], "resemble-enhance-denoise");
    let back: EnhancementProfile = serde_json::from_value(json).unwrap();
    assert_eq!(back, resemble);
    // Параметры DeepFilterNet3 в профиле Resemble — ошибка, а не тихое игнорирование.
    let mut foreign = EnhancementProfile::resemble_denoise();
    foreign.backend.inference.atten_lim_db = 18.0;
    assert!(foreign.validate().is_err());
    // Тот же gate и triage, другой backend.
    assert_eq!(resemble.gate, EnhancementProfile::conservative().gate);
    assert_eq!(resemble.triage, EnhancementProfile::conservative().triage);
}

#[test]
fn resemble_device_drift_is_infrastructure_failure_and_never_cached() {
    let f = fixture("enhancer-resemble-device");
    base_batch(&f);
    let drifted = fake_resemble_worker_on(
        &f.root,
        "resemble-drift",
        &format!(r#"cp "{}" "$out""#, f.clean_f32.display()),
        "cuda",
    );
    let run = start(&f, "res", &drifted, EnhancementProfile::resemble_denoise()).unwrap();
    assert_eq!(run.outcome, "paused", "{run:?}");
    assert_eq!(run.infrastructure_failed_now, 1);
    assert!(!results(&f, "res").contains_key("noisy.mp3"));
}

#[test]
fn resemble_backend_goes_through_the_same_gate_with_own_identity_and_cache() {
    let f = fixture("enhancer-resemble");
    base_batch(&f);
    let ideal = fake_resemble_worker(
        &f.root,
        "resemble-ideal",
        &format!(r#"cp "{}" "$out""#, f.clean_f32.display()),
    );
    let run = start(&f, "res", &ideal, EnhancementProfile::resemble_denoise()).unwrap();
    assert_eq!(run.outcome, "completed", "{run:?}");
    let r = results(&f, "res");
    let noisy = &r["noisy.mp3"];
    assert_eq!(
        noisy.gate.outcome,
        EnhancementOutcome::Enhanced,
        "{:#?}",
        noisy.qc
    );
    assert_eq!(noisy.backend.backend, "resemble-enhance-denoise");
    assert_eq!(
        noisy.backend.model_manifest_sha256,
        RESEMBLE_DENOISE_MANIFEST_SHA256
    );
    assert!(
        noisy
            .gate
            .reasons
            .iter()
            .any(|x| x.code == "enhanced_by_resemble_enhance_denoise")
    );
    assert!(
        !noisy
            .gate
            .reasons
            .iter()
            .any(|x| x.code == "enhanced_by_deepfilternet3")
    );
    // LSNR сообщает только DeepFilterNet3: у Resemble метрики нет, а не ноль.
    assert_eq!(noisy.inference.as_ref().unwrap().run.lsnr_mean_db, None);
    // Тот же вход под DF3 — другой ключ кэша: результаты Resemble не переиспользуются.
    let copy = fake_worker(
        &f.root,
        "copy-worker",
        "deepfilternet3",
        r#"cp "$in" "$out""#,
    );
    let df3 = start(&f, "df3", &copy, EnhancementProfile::conservative()).unwrap();
    assert_eq!((df3.processed_now, df3.reused), (3, 0));
    // Воркер одного вида не выдаёт себя за другой ни в одну сторону.
    for (id, worker, profile) in [
        ("x1", &ideal, EnhancementProfile::conservative()),
        ("x2", &copy, EnhancementProfile::resemble_denoise()),
    ] {
        let error = start(&f, id, worker, profile).unwrap_err();
        assert_eq!(
            error.details["blocker"], "enhancer_identity_mismatch",
            "{id}"
        );
    }
}
