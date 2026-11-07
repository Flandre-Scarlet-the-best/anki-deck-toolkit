//! Сквозной путь с настоящим DeepFilterNet3: Balanced → триаж → воркер →
//! Enhancement QC → решение. Проверяются автоматические решения без
//! участия человека, точные байты входа и воспроизводимость; конкретный
//! исход шумной синтетики не закрепляется — его выбирает QualityGate.

use std::path::{Path, PathBuf};
use std::process::Command;

use asset_store::hashing::sha256_hex;
use asset_store::pronunciation::enhancer::{
    self, EnhancementOutcome, EnhancementProfile, ItemContext, Selection,
};
use asset_store::pronunciation::enhancer_backend as backend;
use asset_store::pronunciation::normalizer::{self, Mode, NormalizerProfile, Toolchain, Variant};

fn model_dir() -> PathBuf {
    std::env::var_os("DEEPFILTERNET3_MODEL_DIR")
        .map(PathBuf::from)
        .or_else(speech_enhance::model::cargo_registry_copy)
        .expect("нет проверяемой копии весов DeepFilterNet3")
}

fn mp3(dir: &Path, name: &str, graph: &str) -> Vec<u8> {
    let path = dir.join(name);
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", graph])
        .args(["-ac", "1", "-c:a", "libmp3lame", "-q:a", "2"])
        .arg(&path)
        .output()
        .expect("нужен FFmpeg");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(path).unwrap()
}

const SPEECHLIKE: &str = "aevalsrc='0.25*sin(PI*t/0.8)*(sin(2*PI*(180+400*t)*t)+0.5*sin(2*PI*(900+1500*t)*t))':duration=0.8:sample_rate=44100,adelay=300:all=1,apad=pad_dur=0.3";

struct Run {
    outcome: EnhancementOutcome,
    selected: Option<Selection>,
    selected_sha: Option<String>,
    output_sha: Option<String>,
    inferred: bool,
}

fn run(bytes: &[u8]) -> Run {
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_speech-enhance-dfn"));
    let model = model_dir();
    let profile = EnhancementProfile::conservative();
    let base_profile = NormalizerProfile::balanced();
    let toolchain = Toolchain::detect().unwrap();
    let identity = backend::identify(
        &worker,
        &model,
        profile.backend.kind,
        &profile.backend.inference,
    )
    .unwrap();
    assert_eq!(identity.model_variant, "deepfilternet3");
    let base = normalizer::process(bytes, &base_profile, Mode::Normalize, &toolchain).unwrap();
    let selected = match base.result.gate.proposed_variant {
        Some(Variant::Processed) => base.output_bytes.clone(),
        Some(Variant::Original) => Some(bytes.to_vec()),
        None => None,
    };
    let config = enhancer::config_sha256(&profile, &identity, &base.result.config_sha256);
    let ctx = ItemContext {
        profile: &profile,
        backend: &identity,
        worker: &worker,
        model_dir: &model,
        base_profile: &base_profile,
        base_toolchain: &toolchain,
        config_sha256: &config,
    };
    let item = enhancer::process_item(&ctx, bytes, &base.result, selected.as_deref()).unwrap();
    item.result.validate().unwrap();
    if let Some(out) = &item.output_bytes {
        assert_eq!(
            Some(sha256_hex(out)),
            item.result.output.as_ref().map(|o| o.sha256.clone())
        );
    }
    Run {
        outcome: item.result.gate.outcome,
        selected: item.result.gate.selected,
        selected_sha: item
            .result
            .selected_input
            .as_ref()
            .map(|s| s.sha256.clone()),
        output_sha: item.result.output.as_ref().map(|o| o.sha256.clone()),
        inferred: item.result.inference.is_some(),
    }
}

#[test]
fn real_model_decisions_are_automatic_reproducible_and_keep_exact_input_bytes() {
    let dir = std::env::temp_dir().join(format!("speech-enhance-pipeline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clean = mp3(&dir, "clean.mp3", &format!("{SPEECHLIKE}[out0]"));
    let noisy = mp3(
        &dir,
        "noisy.mp3",
        &format!(
            "{SPEECHLIKE}[s];anoisesrc=duration=1.4:color=pink:amplitude=0.02:sample_rate=44100:seed=3[n];[s][n]amix=inputs=2:normalize=0:duration=first[out0]"
        ),
    );

    let clean_run = run(&clean);
    assert_eq!(clean_run.outcome, EnhancementOutcome::NotNeeded);
    assert!(!clean_run.inferred);
    assert_eq!(clean_run.selected, Some(Selection::Input));

    let first = run(&noisy);
    assert!(
        first.inferred,
        "шумная запись должна стать кандидатом триажа"
    );
    assert!(matches!(
        first.outcome,
        EnhancementOutcome::Enhanced | EnhancementOutcome::EnhancementRejected
    ));
    if first.outcome == EnhancementOutcome::EnhancementRejected {
        // Отклонённый нейровариант: выбраны точные байты Balanced.
        assert_eq!(first.selected, Some(Selection::Input));
        assert!(first.selected_sha.is_some());
    }
    // Тот же вход, профиль, воркер и веса — те же решение и байты.
    let second = run(&noisy);
    assert_eq!(first.outcome, second.outcome);
    assert_eq!(first.output_sha, second.output_sha);
    assert_eq!(first.selected_sha, second.selected_sha);
    std::fs::remove_dir_all(dir).unwrap();
}
