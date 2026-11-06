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
