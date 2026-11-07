//! Настоящий инференс DeepFilterNet3 на синтетике с известным чистым PCM.
//!
//! Веса берутся из каталога `DEEPFILTERNET3_MODEL_DIR` или из копии crate
//! `deep-filter-rt` в локальном реестре Cargo (её распаковывает сама сборка)
//! и проверяются по закреплённому манифесту. Сеть не используется.

use std::f32::consts::{PI, TAU};
use std::path::{Path, PathBuf};
use std::process::Command;

use speech_enhance::enhance::{EnhanceErrorKind, Enhancer, InferenceParams};
use speech_enhance::model::{self, ModelErrorKind};

const SR: f32 = 48_000.0;

fn model_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DEEPFILTERNET3_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    model::cargo_registry_copy()
        .expect("нет проверяемой копии весов DeepFilterNet3: задайте DEEPFILTERNET3_MODEL_DIR")
}

fn enhancer() -> Enhancer {
    let (_, files) = model::load_dir(&model_dir()).expect("веса проходят проверку");
    Enhancer::new(&files, &InferenceParams::default()).expect("модель загружается")
}

/// Гласноподобный сигнал: импульсы 120 Hz через три форманты, слоги по 250 мс.
fn vowel(len: usize, amplitude: f32) -> Vec<f32> {
    let formants = [(700., 80.), (1220., 90.), (2600., 120.)];
    let mut states = [[0f32; 2]; 3];
    (0..len)
        .map(|i| {
            let pulse = if i % 400 == 0 { 1. } else { 0. };
            let mut y = 0.;
            for (&(freq, bandwidth), s) in formants.iter().zip(states.iter_mut()) {
                let r = (-PI * bandwidth / SR).exp();
                let out = pulse + 2. * r * (TAU * freq / SR).cos() * s[0] - r * r * s[1];
                s[1] = s[0];
                s[0] = out;
                y += out;
            }
            let envelope = (0.5 - 0.5 * (TAU * 4. * i as f32 / SR).cos()).powi(2);
            amplitude * y * envelope
        })
        .collect()
}

/// Воспроизводимый белый шум (xorshift).
fn noise(len: usize, seed: u64, amplitude: f32) -> Vec<f32> {
    let mut x = seed.max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * amplitude
        })
        .collect()
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|v| f64::from(*v).powi(2)).sum()
}

fn best_lag(reference: &[f32], test: &[f32], max_lag: i64) -> i64 {
    (-max_lag..=max_lag)
        .max_by(|&a, &b| {
            let corr = |lag: i64| -> f64 {
                (0..reference.len() as i64)
                    .filter_map(|i| {
                        let j = i + lag;
                        (j >= 0 && (j as usize) < test.len())
                            .then(|| f64::from(reference[i as usize]) * f64::from(test[j as usize]))
                    })
                    .sum()
            };
            corr(a).total_cmp(&corr(b))
        })
        .unwrap()
}

#[test]
fn output_length_equals_input_for_short_and_unaligned_clips() {
    let e = enhancer();
    for len in [1usize, 37, 479, 480, 481, 1439, 1440, 2400, 48_000 + 123] {
        let input = vowel(len, 0.02);
        let (out, diag) = e.enhance(1, &input).unwrap();
        assert_eq!(out.len(), len, "длина {len}");
        assert_eq!(diag.output_frames, len as u64);
        assert_eq!(diag.delay_samples, 1440);
        assert!((diag.flush_samples as usize) >= diag.delay_samples as usize);
        assert!(out.iter().all(|s| s.is_finite()));
    }
}

#[test]
fn delay_is_compensated_exactly() {
    let e = enhancer();
    let input = vowel(48_000, 0.02);
    let (out, _) = e.enhance(1, &input).unwrap();
    assert_eq!(best_lag(&input[4800..43_200], &out[4800..43_200], 960), 0);
    // Конец клипа не теряется при сбросе хвоста: последний слог сохранён.
    let tail = 48_000 - 4800..48_000 - 480;
    assert!(energy(&out[tail.clone()]) > 0.25 * energy(&input[tail]));
}

/// Контекст прогрева не меняет длину и выравнивание и детерминирован. Его
/// польза для начала слова измерена на реальных произношениях при
/// калибровке профиля, а не на синтетике: синтетические импульсные гласные
/// прогретая модель гасит сильнее, потому что речью их не считает.
#[test]
fn warmup_context_keeps_length_alignment_and_determinism() {
    let (_, files) = model::load_dir(&model_dir()).unwrap();
    let warmed = Enhancer::new(
        &files,
        &InferenceParams {
            context_ms: 200,
            ..InferenceParams::default()
        },
    )
    .unwrap();
    let input = vowel(24_000, 0.02)[6_000..].to_vec();
    let (warm, diag) = warmed.enhance(1, &input).unwrap();
    assert_eq!(warm.len(), input.len());
    assert_eq!(diag.context_samples, 9_600);
    assert_eq!(best_lag(&input[2400..15_000], &warm[2400..15_000], 960), 0);
    // Клип короче контекста: зеркальное продолжение повторяется.
    let tiny = vowel(300, 0.02);
    let (a, _) = warmed.enhance(1, &tiny).unwrap();
    let (b, _) = warmed.enhance(1, &tiny).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.len(), 300);
}

#[test]
fn state_does_not_leak_between_clips_and_is_deterministic() {
    let e = enhancer();
    let a: Vec<f32> = vowel(24_000, 0.02)
        .iter()
        .zip(noise(24_000, 7, 0.05))
        .map(|(v, n)| v + n)
        .collect();
    let b: Vec<f32> = vowel(30_000, 0.01)
        .iter()
        .zip(noise(30_000, 11, 0.01))
        .map(|(v, n)| v + n)
        .collect();
    let (b_alone, _) = e.enhance(1, &b).unwrap();
    let (_, _) = e.enhance(1, &a).unwrap();
    let (b_after_a, _) = e.enhance(1, &b).unwrap();
    assert_eq!(b_alone, b_after_a, "результат зависит от предыдущего клипа");
    let (b_new_instance, _) = enhancer().enhance(1, &b).unwrap();
    assert_eq!(b_alone, b_new_instance, "инференс недетерминирован");
}

/// Воспроизводимый «коричневый» шум: интегрированный белый, энергия в
/// низких частотах (гул, фон помещения).
fn brown_noise(len: usize, seed: u64, amplitude: f32) -> Vec<f32> {
    let white = noise(len, seed, 1.0);
    let mut acc = 0f32;
    let raw: Vec<f32> = white
        .iter()
        .map(|w| {
            acc = 0.995 * acc + 0.05 * w;
            acc
        })
        .collect();
    let peak = raw.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-9);
    raw.iter().map(|v| v / peak * amplitude).collect()
}

#[test]
fn noisy_synthetic_speech_gets_measurable_spectral_benefit() {
    let e = enhancer();
    let clean = vowel(96_000, 0.02);
    for noisy_part in [
        noise(clean.len(), 3, 0.05),
        brown_noise(clean.len(), 5, 0.15),
    ] {
        let noisy: Vec<f32> = clean.iter().zip(&noisy_part).map(|(c, n)| c + n).collect();
        let (out, _) = e.enhance(1, &noisy).unwrap();
        let before = spectral_snr_db(&clean, &noisy);
        let after = spectral_snr_db(&clean, &out);
        assert!(
            after > before + 6.0,
            "польза не измерима: спектральный SNR до {before:.1} dB, после {after:.1} dB"
        );
    }
}

/// Модель не прозрачна: чистый синтетический сигнал она заметно меняет
/// (на этой синтетике — около −2 dB энергии и спектральный SNR около 10 dB).
/// Поэтому применять её к чистым записям нельзя, и решение о пользе
/// принимает QualityGate, а не сам факт работы модели.
#[test]
fn model_alters_clean_input_so_enhancement_must_be_gated() {
    let e = enhancer();
    let clean = vowel(96_000, 0.02);
    let (out, _) = e.enhance(1, &clean).unwrap();
    assert!(spectral_snr_db(&clean, &out) < 30.0);
    assert!(energy(&out) < energy(&clean));
}

#[test]
fn digital_silence_stays_silent() {
    let e = enhancer();
    let (out, _) = e.enhance(1, &vec![0.0; 24_000]).unwrap();
    assert!(energy(&out) < 1e-12);
}

#[test]
fn stereo_channels_are_independent_and_antiphase_is_not_cancelled() {
    let e = enhancer();
    let mono: Vec<f32> = vowel(24_000, 0.02)
        .iter()
        .zip(noise(24_000, 9, 0.01))
        .map(|(v, n)| v + n)
        .collect();
    let (mono_out, _) = e.enhance(1, &mono).unwrap();

    let identical: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();
    let (out, diag) = e.enhance(2, &identical).unwrap();
    assert!(diag.identical_channels);
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
    assert_eq!(left, mono_out);
    assert_eq!(right, mono_out);

    let antiphase: Vec<f32> = mono.iter().flat_map(|s| [*s, -*s]).collect();
    let (out, diag) = e.enhance(2, &antiphase).unwrap();
    assert!(!diag.identical_channels);
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
    // Левый канал обработан так же, как моно: каналы независимы.
    assert_eq!(left, mono_out);
    // Правый не погашен (сведения в моно нет). Модель нелинейна по фазе
    // спектральных признаков, поэтому -x не обязан дать ровно -f(x).
    let ratio = energy(&right) / energy(&mono_out);
    assert!(ratio > 0.25, "энергия правого канала {ratio}");
}

#[test]
fn invalid_input_is_rejected() {
    let e = enhancer();
    // Отказ от входа — категория `Input` (выход 4), а не сбой инференса.
    for (channels, samples) in [
        (1, &[][..]),
        (2, &[0.0; 3][..]),
        (3, &[0.0; 3][..]),
        (1, &[0.0, f32::NAN][..]),
    ] {
        let error = e.enhance(channels, samples).unwrap_err();
        assert_eq!(error.kind, EnhanceErrorKind::Input, "{error}");
    }
}

fn copy_model(target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for f in model::FILES {
        std::fs::copy(model_dir().join(f.name), target.join(f.name)).unwrap();
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("speech-enhance-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn missing_corrupt_wrong_version_and_symlinked_models_fail_honestly() {
    let root = scratch("model-errors");
    assert_eq!(
        model::load_dir(&root.join("absent")).unwrap_err().kind,
        ModelErrorKind::Missing
    );

    let corrupt = root.join("corrupt");
    copy_model(&corrupt);
    let path = corrupt.join("enc.onnx");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[1000] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(
        model::load_dir(&corrupt).unwrap_err().kind,
        ModelErrorKind::Corrupt
    );

    let df2 = root.join("df2");
    copy_model(&df2);
    let config = std::fs::read_to_string(df2.join("config.ini")).unwrap();
    std::fs::write(
        df2.join("config.ini"),
        config.replace("model = deepfilternet3", "model = deepfilternet2"),
    )
    .unwrap();
    assert_eq!(
        model::load_dir(&df2).unwrap_err().kind,
        ModelErrorKind::WrongVersion
    );

    let linked = root.join("linked");
    copy_model(&linked);
    std::fs::remove_file(linked.join("df_dec.onnx")).unwrap();
    std::os::unix::fs::symlink(model_dir().join("df_dec.onnx"), linked.join("df_dec.onnx"))
        .unwrap();
    assert_eq!(
        model::load_dir(&linked).unwrap_err().kind,
        ModelErrorKind::NotRegularFile
    );

    let missing_file = root.join("missing-file");
    copy_model(&missing_file);
    std::fs::remove_file(missing_file.join("erb_dec.onnx")).unwrap();
    assert_eq!(
        model::load_dir(&missing_file).unwrap_err().kind,
        ModelErrorKind::Missing
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn install_from_upstream_layout_archive_is_verified_and_idempotent() {
    let root = scratch("install");
    let archive = root.join("DeepFilterNet3_onnx.tar.gz");
    {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        for f in model::FILES {
            tar.append_path_with_name(model_dir().join(f.name), format!("tmp/export/{}", f.name))
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    let models = root.join("models");
    let (path, identity) = model::install(&archive, &models).unwrap();
    assert_eq!(path, model::installed_dir(&models));
    assert_eq!(identity.variant, "deepfilternet3");
    assert_eq!(identity, model::pinned_identity());
    let (again, _) = model::install(&archive, &models).unwrap();
    assert_eq!(again, path);

    // Повреждённая установка не «чинится» молча и не используется.
    let mut bytes = std::fs::read(path.join("config.ini")).unwrap();
    bytes.push(b'\n');
    std::fs::write(path.join("config.ini"), bytes).unwrap();
    assert_eq!(
        model::install(&archive, &models).unwrap_err().kind,
        ModelErrorKind::Corrupt
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn worker_process_protocol_reports_identity_and_errors() {
    let bin = env!("CARGO_BIN_EXE_speech-enhance-dfn");
    let out = Command::new(bin)
        .args(["identify", "--model-dir"])
        .arg(model_dir())
        .output()
        .unwrap();
    assert!(out.status.success());
    let identity: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(identity["model"]["name"], "DeepFilterNet3");
    assert_eq!(identity["model"]["variant"], "deepfilternet3");
    assert_eq!(identity["stream"]["delay_samples"], 1440);
    assert_eq!(identity["runtime"]["device"], "cpu");

    let root = scratch("protocol");
    let input = root.join("in.f32");
    let samples = vowel(9_000, 0.02);
    std::fs::write(
        &input,
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    let output = root.join("out.f32");
    let run = Command::new(bin)
        .args(["process", "--channels", "1", "--model-dir"])
        .arg(model_dir())
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();
    assert!(run.status.success());
    let result: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(result["diagnostics"]["output_frames"], 9_000);
    assert_eq!(std::fs::metadata(&output).unwrap().len(), 9_000 * 4);

    let missing = Command::new(bin)
        .args(["identify", "--model-dir"])
        .arg(root.join("none"))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(3));
    let report: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(report["code"], "model_missing");
    std::fs::remove_dir_all(root).unwrap();
}

/// SNR модулей спектра (dB) относительно эталона: не зависит от фазы, в
/// отличие от SNR формы волны, и взвешен энергией (тихие бины между
/// гармониками не доминируют).
fn spectral_snr_db(clean: &[f32], test: &[f32]) -> f64 {
    use realfft::RealFftPlanner;
    let n = 1024;
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
    let window: Vec<f64> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos())
        .collect();
    let magnitude = |x: &[f32], start: usize| -> Vec<f64> {
        let mut frame: Vec<f64> = (0..n)
            .map(|i| f64::from(x[start + i]) * window[i])
            .collect();
        let mut out = fft.make_output_vec();
        fft.process(&mut frame, &mut out).unwrap();
        out.iter().map(|c| c.norm()).collect()
    };
    let (mut signal, mut error) = (0.0, 0.0);
    let mut start = 0;
    while start + n <= clean.len() {
        let (c, t) = (magnitude(clean, start), magnitude(test, start));
        signal += c.iter().map(|v| v * v).sum::<f64>();
        error += c.iter().zip(&t).map(|(a, b)| (a - b).powi(2)).sum::<f64>();
        start += n / 2;
    }
    10.0 * (signal / error.max(1e-30)).log10()
}
