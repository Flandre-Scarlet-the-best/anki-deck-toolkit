//! Отказы процесса `speech-recognize-ctc` без настоящих весов: каждый —
//! один JSON `speech_recognize_error` в stdout и код выхода протокола
//! (3 — модель, 4 — вход). Входы проверяются до загрузки модели.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use speech_recognize::protocol::{EXIT_INPUT, EXIT_MODEL};

const BIN: &str = env!("CARGO_BIN_EXE_speech-recognize-ctc");
const MODEL: &str = "hubert-base-phoneme-ctc-v4";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "speech-recognize-cli-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn worker(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

/// Код выхода и код ошибки из единственного JSON в stdout.
fn refused(output: &Output) -> (i32, String) {
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["kind"], "speech_recognize_error", "{report}");
    assert!(!report["message"].as_str().unwrap().is_empty());
    (
        output.status.code().unwrap(),
        report["code"].as_str().unwrap().to_string(),
    )
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn f32_file(path: &Path, samples: &[f32]) {
    let bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn unknown_model_and_missing_weights_are_model_failures() {
    let dir = scratch("model");
    let out = worker(&["identify", "--model", "nope", "--model-dir", p(&dir)]);
    assert_eq!(refused(&out), (EXIT_MODEL, "model_unknown".into()));
    let out = worker(&["identify", "--model", MODEL, "--model-dir", p(&dir)]);
    assert_eq!(refused(&out), (EXIT_MODEL, "model_missing".into()));
    let out = worker(&[
        "identify",
        "--model",
        MODEL,
        "--model-dir",
        p(&dir),
        "--load",
    ]);
    assert_eq!(refused(&out), (EXIT_MODEL, "model_missing".into()));
    // Установка из неполного снимка ничего не публикует.
    let root = dir.join("models");
    let out = worker(&[
        "install-model",
        "--model",
        MODEL,
        "--source",
        p(&dir.join("нет-снимка")),
        "--models-root",
        p(&root),
    ]);
    assert_eq!(refused(&out), (EXIT_MODEL, "model_missing".into()));
    assert!(!root.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_inputs_are_refused_before_the_model_is_loaded() {
    let dir = scratch("input");
    let good = dir.join("good.f32");
    f32_file(&good, &[0.0; 1600]);
    let out_path = dir.join("out.f32");
    let process = |input: &Path| {
        worker(&[
            "process",
            "--model",
            MODEL,
            "--model-dir",
            p(&dir),
            "--input",
            p(input),
            "--output",
            p(&out_path),
        ])
    };
    // Не кратно f32.
    let odd = dir.join("odd.f32");
    std::fs::write(&odd, [0u8; 6]).unwrap();
    assert_eq!(
        refused(&process(&odd)),
        (EXIT_INPUT, "input_invalid".into())
    );
    // Неконечный отсчёт.
    let nan = dir.join("nan.f32");
    f32_file(&nan, &[0.0, f32::NAN]);
    assert_eq!(
        refused(&process(&nan)),
        (EXIT_INPUT, "input_invalid".into())
    );
    // Ссылка вместо файла и отсутствующий файл.
    let link = dir.join("link.f32");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    assert_eq!(
        refused(&process(&link)),
        (EXIT_INPUT, "input_unreadable".into())
    );
    assert_eq!(
        refused(&process(&dir.join("нет.f32"))),
        (EXIT_INPUT, "input_unreadable".into())
    );
    // Каталог вместо файла.
    assert_eq!(
        refused(&process(&dir)),
        (EXIT_INPUT, "input_invalid".into())
    );
    // Входов и выходов не поровну.
    let out = worker(&[
        "process",
        "--model",
        MODEL,
        "--model-dir",
        p(&dir),
        "--input",
        p(&good),
        "--input",
        p(&good),
        "--output",
        p(&out_path),
    ]);
    assert_eq!(refused(&out), (EXIT_INPUT, "input_invalid".into()));
    // Корректный вход доходит до проверки весов, а их нет.
    assert_eq!(
        refused(&process(&good)),
        (EXIT_MODEL, "model_missing".into())
    );
    assert!(!out_path.exists(), "частичного вывода нет");
    let out = worker(&[
        "process",
        "--model",
        "nope",
        "--model-dir",
        p(&dir),
        "--input",
        p(&good),
        "--output",
        p(&out_path),
    ]);
    assert_eq!(refused(&out), (EXIT_MODEL, "model_unknown".into()));
    std::fs::remove_dir_all(&dir).unwrap();
}
