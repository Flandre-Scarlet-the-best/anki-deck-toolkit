//! Проверки воркера Resemble Enhance без PyTorch и весов (stdlib-тесты
//! `resemble/test_speech_enhance_resemble.py`) и согласие закреплённого
//! манифеста весов между воркером и `asset-store`.
use std::path::Path;
use std::process::Command;

use asset_store::pronunciation::enhancer_backend::RESEMBLE_DENOISE_MANIFEST_SHA256;

#[test]
fn resemble_worker_trust_boundary_and_protocol() {
    let tests =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("resemble/test_speech_enhance_resemble.py");
    let output = Command::new("python3")
        .arg("-I")
        .arg(&tests)
        .env(
            "RESEMBLE_DENOISE_MANIFEST_SHA256",
            RESEMBLE_DENOISE_MANIFEST_SHA256,
        )
        .output()
        .expect("для теста воркера Resemble нужен python3");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Opt-in: настоящий воркер на закреплённых весах
// ---------------------------------------------------------------------------

/// Приёмка настоящего воркера Resemble Enhance на закреплённых весах через
/// путь `asset-store`: `identify`, `process`, Enhancement QC и решение.
/// Нужны окружение CPython 3.15 из `resemble/requirements.txt` и
/// установленные веса, поэтому тест не входит в обычный `cargo test`:
///
/// ```text
/// RESEMBLE_RUNTIME_PYTHON=<venv>/bin/python RESEMBLE_MODEL_DIR=<каталог весов> \
/// RESEMBLE_RUNTIME_DEVICES=cpu,cuda \
///   cargo test -p speech-enhance --test resemble -- --ignored --nocapture
/// ```
///
/// Каждый процесс воркера запускается в `unshare -rn` (без сети). Устройство
/// из `RESEMBLE_RUNTIME_DEVICES` (по умолчанию `cpu`), недоступное в среде,
/// — отказ теста, а не пропуск.
#[test]
#[ignore = "opt-in: нужны окружение Python 3.15 и веса Resemble Enhance"]
fn resemble_runtime_on_pinned_weights() {
    use asset_store::hashing::sha256_hex;
    use asset_store::pronunciation::enhancer::{
        self, EnhancementOutcome, EnhancementProfile, ItemContext, QC_CHECKS, Selection,
    };
    use asset_store::pronunciation::enhancer_backend::{self as backend, BackendKind};
    use asset_store::pronunciation::normalizer::{
        self, Mode, NormalizerProfile, Toolchain, Variant,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let python = std::env::var("RESEMBLE_RUNTIME_PYTHON").expect("задайте RESEMBLE_RUNTIME_PYTHON");
    let model = std::path::PathBuf::from(
        std::env::var("RESEMBLE_MODEL_DIR").expect("задайте RESEMBLE_MODEL_DIR"),
    );
    let devices = std::env::var("RESEMBLE_RUNTIME_DEVICES").unwrap_or_else(|_| "cpu".into());
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("resemble");
    let dir = std::env::temp_dir().join(format!("resemble-runtime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Стандартные тесты воркера в целевом интерпретаторе, включая проверки
    // настоящего torch.load (без torch они были бы пропущены — здесь отказ).
    let unit = Command::new(&python)
        .arg("-I")
        .arg(source.join("test_speech_enhance_resemble.py"))
        .env(
            "RESEMBLE_DENOISE_MANIFEST_SHA256",
            RESEMBLE_DENOISE_MANIFEST_SHA256,
        )
        .env("RESEMBLE_REQUIRE_TORCH", "1")
        .output()
        .unwrap();
    assert!(
        unit.status.success(),
        "{}",
        String::from_utf8_lossy(&unit.stderr)
    );

    let profile = EnhancementProfile::resemble_denoise();
    let base_profile = NormalizerProfile::balanced();
    let toolchain = Toolchain::detect().unwrap();
    let noisy = dir.join("noisy.mp3");
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg(
            "aevalsrc='0.25*sin(PI*t/0.8)*(sin(2*PI*(180+400*t)*t)+0.5*sin(2*PI*(900+1500*t)*t))':duration=0.8:sample_rate=44100,adelay=300:all=1,apad=pad_dur=0.3[s];anoisesrc=duration=1.4:color=pink:amplitude=0.02:sample_rate=44100:seed=3[n];[s][n]amix=inputs=2:normalize=0:duration=first[out0]",
        )
        .args(["-ac", "1", "-c:a", "libmp3lame", "-q:a", "2"])
        .arg(&noisy)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let noisy = std::fs::read(noisy).unwrap();
    let base = normalizer::process(&noisy, &base_profile, Mode::Normalize, &toolchain).unwrap();
    let selected = match base.result.gate.proposed_variant {
        Some(Variant::Processed) => base.output_bytes.clone(),
        Some(Variant::Original) => Some(noisy.clone()),
        None => None,
    };

    for device in devices.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        let launcher = dir.join(format!("speech-enhance-resemble-{device}"));
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nSPEECH_ENHANCE_RESEMBLE_DEVICE={device} exec unshare -rn '{python}' -I '{}' \"$@\"\n",
                source.join("speech_enhance_resemble.py").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let identity = backend::identify(
            &launcher,
            &model,
            BackendKind::ResembleEnhanceDenoise,
            &profile.backend.inference,
        )
        .unwrap_or_else(|e| panic!("{device}: identify: {e:?}"));
        let runtime = &identity.report["runtime"];
        assert_eq!(runtime["device"], device, "{runtime}");
        assert!(
            runtime["python"].as_str().unwrap().starts_with("3.15."),
            "{runtime}"
        );
        assert!(
            runtime["torch"].as_str().unwrap().starts_with("2.15.0"),
            "{runtime}"
        );
        assert_eq!(identity.execution_mode(), format!("{device}-pytorch"));
        eprintln!("{device}: identify {runtime}");

        // Прямой `process`: стерео с разными каналами, длина и конечность,
        // одинаковые байты на одинаковом входе того же устройства.
        let frames = 48_000;
        let pcm: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let t = i as f32 / 48_000.0;
                let env = (std::f32::consts::PI * t).sin();
                let noise = ((i as u32).wrapping_mul(2_654_435_761) >> 8) as f32 / 16_777_216.0;
                [
                    env * 0.3 * (std::f32::consts::TAU * 220.0 * t).sin() + 0.01 * (noise - 0.5),
                    env * 0.05 * (std::f32::consts::TAU * 330.0 * t).sin() + 0.01 * (0.5 - noise),
                ]
            })
            .collect();
        let run = |samples: &[f32]| {
            backend::enhance(
                &launcher,
                &model,
                &identity,
                2,
                samples,
                Duration::from_secs(300),
            )
            .unwrap_or_else(|e| panic!("{device}: process: {e:?}"))
        };
        let (first, info) = run(&pcm);
        let (second, _) = run(&pcm);
        assert_eq!(first.len(), pcm.len());
        assert!(first.iter().all(|s| s.is_finite()));
        let bytes = |v: &[f32]| v.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        assert_eq!(
            sha256_hex(bytes(&first)),
            sha256_hex(bytes(&second)),
            "{device}: недетерминированный вывод"
        );
        assert_eq!(info.diagnostics["device"], device);
        eprintln!(
            "{device}: process {} кадров, SHA вывода {}, {:.0} мс",
            frames,
            sha256_hex(bytes(&first)),
            info.wall_ms
        );

        // Полный путь этапа: решение QualityGate без сбоя backend.
        let config = enhancer::config_sha256(&profile, &identity, &base.result.config_sha256);
        let ctx = ItemContext {
            profile: &profile,
            backend: &identity,
            worker: &launcher,
            model_dir: &model,
            base_profile: &base_profile,
            base_toolchain: &toolchain,
            config_sha256: &config,
        };
        let item = enhancer::process_item(&ctx, &noisy, &base.result, selected.as_deref())
            .unwrap_or_else(|e| panic!("{device}: этап: {e:?}"));
        item.result.validate().unwrap();
        let qc = item
            .result
            .qc
            .as_ref()
            .expect("шумная запись дошла до модели");
        assert_eq!(qc.checks.len(), QC_CHECKS.len());
        match item.result.gate.outcome {
            EnhancementOutcome::Enhanced => assert!(item.output_bytes.is_some()),
            EnhancementOutcome::EnhancementRejected => {
                assert_eq!(item.result.gate.selected, Some(Selection::Input));
                assert!(item.output_bytes.is_none());
            }
            other => panic!("{device}: неожиданный исход {other:?}"),
        }
        eprintln!(
            "{device}: исход {:?}, снижение шума {:.1} dB, отказы QC {:?}",
            item.result.gate.outcome,
            qc.noise_floor_before_dbfs - qc.noise_floor_after_dbfs,
            qc.checks
                .iter()
                .filter(|c| !c.passed || !c.resolved)
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
