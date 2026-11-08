//! Граница локальных акустических моделей распознавания: изолированный
//! процесс `speech-recognize-ctc` (Rust, CPU) того же рабочего пространства Cargo.
//!
//! `asset-store` не связывается с кодом инференса: воркер запускается через
//! `prlimit` с пределами памяти, CPU, дескрипторов и времени; обмен — файлы
//! PCM `f32` и эмиссий во временном каталоге и один JSON в stdout. Сеть не
//! используется, веса читаются только из установленного каталога и
//! сверяются воркером с закреплённым манифестом при каждом запуске, а
//! `asset-store` дополнительно сверяет отчёт с собственными ожиданиями
//! (идентификатор, ревизия, манифест, вид выхода, словарь).
//!
//! Идентичность backend — SHA-256 исполняемого файла воркера и его отчёт
//! `identify`; обе входят в ключ кэша эмиссий и в отпечаток конфигурации.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ctc::{Classes, Emissions};
use super::enhancer_backend::{backend_failure, run_limited};
use crate::hashing::sha256_hex;
use crate::temp_workspace::TempWorkspace;
use crate::{AssetError, ErrorCode};

pub const WORKER_NAME: &str = "speech-recognize-ctc";
pub const PROTOCOL_VERSION: u32 = 1;
pub const SAMPLE_RATE: u32 = 16_000;
/// Наибольшее число входов одного вызова воркера (как у воркера).
pub const MAX_CALL_ITEMS: usize = 256;
const GIB: u64 = 1024 * 1024 * 1024;

/// Закреплённая в `asset-store` модель: ожидания к отчёту воркера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedModel {
    pub id: &'static str,
    pub output: &'static str,
    pub revision: &'static str,
    pub manifest_sha256: &'static str,
    pub vocab: usize,
    /// Предел адресного пространства процесса (веса + активации).
    pub address_space: u64,
}

/// Основная фонемная модель (HuBERT base, pyopenjtalk-фонемы).
pub const PRIMARY_PHONEME: ExpectedModel = ExpectedModel {
    id: "hubert-base-phoneme-ctc-v4",
    output: "phoneme",
    revision: "f5fe07043bcb0b77a86faf72ac6d8fc1ae558f99",
    manifest_sha256: "d5ef589b9317003f322d43c13d9b561a3f8cbfde119fae58231ae27c1ebeb9fc",
    vocab: 48,
    address_space: 6 * GIB,
};

/// Независимая модель второй гипотезы (XLSR-53 large, хирагана).
pub const VERIFIER_HIRAGANA: ExpectedModel = ExpectedModel {
    id: "xlsr-japanese-hiragana-ctc",
    output: "hiragana",
    revision: "017225bb128a6b1c6de9d58391f891908d69bc7b",
    manifest_sha256: "a7c9b13a4724b8e4933d97ca67991f0d588e26c87ea4839764cea052c8872924",
    vocab: 86,
    address_space: 12 * GIB,
};

pub const MODELS: [ExpectedModel; 2] = [PRIMARY_PHONEME, VERIFIER_HIRAGANA];

pub fn expected(id: &str) -> Option<ExpectedModel> {
    MODELS.iter().copied().find(|m| m.id == id)
}

/// Коды сбоев среды распознавания: вход остаётся незавершённым и
/// повторяется, в исход входа и кэш сбой не превращается.
pub const INFRASTRUCTURE_BLOCKERS: [&str; 7] = [
    "recognizer_backend_unavailable",
    "recognizer_model_unavailable",
    "recognizer_backend_timeout",
    "recognizer_backend_crashed",
    "recognizer_backend_failed",
    "recognizer_backend_protocol",
    "recognizer_backend_changed",
];

pub fn is_backend_failure(error: &AssetError) -> bool {
    error.details["blocker"]
        .as_str()
        .is_some_and(|code| INFRASTRUCTURE_BLOCKERS.contains(&code))
}

fn protocol(message: impl Into<String>) -> AssetError {
    backend_failure(
        "recognizer_backend_protocol",
        message,
        serde_json::json!({}),
    )
}

fn identity_mismatch(message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        ErrorCode::IntegrityMismatch,
        message,
        serde_json::json!({"blocker": "recognizer_identity_mismatch"}),
    )
}

/// Проверенная идентичность модели и воркера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub model_id: String,
    pub output: String,
    pub repository: String,
    pub revision: String,
    pub license: String,
    pub manifest_sha256: String,
    pub worker_sha256: String,
    pub worker_byte_length: u64,
    pub worker_version: String,
    pub inference: String,
    pub device: String,
    pub threads: u32,
    pub sample_rate: u32,
    pub frame_stride: usize,
    pub receptive_field: usize,
    pub lead_pad_samples: usize,
    pub trail_pad_samples: usize,
    pub blank_id: usize,
    pub vocab: Vec<String>,
}

impl ModelIdentity {
    /// Отпечаток всего, что может изменить эмиссии: модель, воркер, среда.
    pub fn fingerprint(&self) -> String {
        sha256_hex(serde_json::to_vec(self).expect("идентичность сериализуема"))
    }
}

/// Модель, готовая к вызовам: идентичность, каталог весов и классы сравнения.
#[derive(Debug, Clone)]
pub struct ModelHandle {
    pub identity: ModelIdentity,
    pub model_dir: PathBuf,
    pub worker: PathBuf,
    pub address_space: u64,
    pub classes: Classes,
}

/// Классы сравнения словаря: у фонемной модели оглушённые `I`/`U` сливаются
/// с `i`/`u`, паузы и служебные токены классов не дают; у модели хираганы
/// класс — сам символ.
pub fn classes_for(identity: &ModelIdentity) -> Classes {
    let mut names: Vec<String> = Vec::new();
    let mut tokens: Vec<Vec<usize>> = Vec::new();
    for (k, token) in identity.vocab.iter().enumerate() {
        if k == identity.blank_id {
            continue;
        }
        let name = if identity.output == "phoneme" {
            match super::kana::comparable_phoneme(token) {
                Some(name) => name.to_string(),
                None => continue,
            }
        } else if token.chars().count() == 1
            && token
                .chars()
                .all(|c| super::kana::is_hiragana(c) || c == 'ー' || c == 'ゔ')
        {
            token.clone()
        } else {
            continue;
        };
        match names.iter().position(|n| *n == name) {
            Some(i) => tokens[i].push(k),
            None => {
                names.push(name);
                tokens.push(vec![k]);
            }
        }
    }
    Classes { names, tokens }
}

/// Воркер: явный путь, иначе файл рядом с текущим исполняемым, иначе `PATH`.
pub fn locate_worker(explicit: Option<&Path>) -> Result<PathBuf, AssetError> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let unavailable = |message: String| {
        backend_failure(
            "recognizer_backend_unavailable",
            message,
            serde_json::json!({}),
        )
    };
    if let Some(path) = explicit {
        return if executable(path) {
            std::path::absolute(path).map_err(|e| AssetError::io("путь воркера", e))
        } else {
            Err(unavailable(format!(
                "воркер {} не найден или не исполняемый",
                path.display()
            )))
        };
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(WORKER_NAME)))
        .filter(|p| executable(p));
    let in_path = || {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(WORKER_NAME))
                .find(|p| executable(p))
        })
    };
    sibling.or_else(in_path).ok_or_else(|| {
        unavailable(format!(
            "воркер {WORKER_NAME} не найден: соберите `cargo build --release -p speech-recognize` или укажите --recognizer-bin"
        ))
    })
}

/// Каталог установки модели: `<models_root>/<id>/<manifest_sha256>`.
pub fn installed_model_dir(models_root: &Path, model: ExpectedModel) -> PathBuf {
    models_root.join(model.id).join(model.manifest_sha256)
}

fn worker_error(code: Option<i32>, stdout: &[u8]) -> AssetError {
    let report: serde_json::Value = serde_json::from_slice(stdout).unwrap_or_default();
    let error = report["code"].as_str().unwrap_or("").to_owned();
    let message = report["message"].as_str().unwrap_or("").to_owned();
    match code {
        Some(3) => backend_failure(
            "recognizer_model_unavailable",
            format!("веса модели недоступны или не проходят проверку: {error}: {message}"),
            serde_json::json!({"model_error": error}),
        ),
        Some(4) => AssetError::with_details(
            ErrorCode::ValidatorFailure,
            format!("воркер отверг подготовленный этапом вход: {error}: {message}"),
            serde_json::json!({"blocker": "recognizer_input_contract"}),
        ),
        other => backend_failure(
            "recognizer_backend_failed",
            format!("воркер завершился с кодом {other:?}: {error}: {message}"),
            serde_json::json!({"exit_code": other}),
        ),
    }
}

fn threads_env(threads: u32) -> String {
    threads.clamp(1, 4).to_string()
}

/// Проверяет установку и возвращает готовую к вызовам модель.
pub fn identify(
    worker: &Path,
    model_dir: &Path,
    model: ExpectedModel,
    threads: u32,
) -> Result<ModelHandle, AssetError> {
    let bytes = std::fs::read(worker).map_err(|e| {
        backend_failure(
            "recognizer_backend_unavailable",
            format!("воркер не читается: {e}"),
            serde_json::json!({}),
        )
    })?;
    let threads_text = threads_env(threads);
    let finished = run_limited(
        worker,
        model.address_space,
        &[("MATMUL_NUM_THREADS", &threads_text)],
        &[
            "identify".as_ref(),
            "--model".as_ref(),
            model.id.as_ref(),
            "--model-dir".as_ref(),
            model_dir.as_os_str(),
        ],
        Duration::from_secs(120),
        "recognizer",
    )?;
    if finished.code != Some(0) {
        return Err(worker_error(finished.code, &finished.stdout));
    }
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout)
        .map_err(|_| protocol("отчёт identify воркера не разбирается"))?;
    let field = |pointer: &str| report.pointer(pointer).cloned().unwrap_or_default();
    let expect = |pointer: &str, value: serde_json::Value, what: &str| {
        if field(pointer) == value {
            Ok(())
        } else {
            Err(identity_mismatch(format!(
                "идентичность модели распознавания не совпала ({what}): ожидалось {value}, получено {}",
                field(pointer)
            )))
        }
    };
    expect("/kind", "speech_recognize_identity".into(), "вид отчёта")?;
    expect(
        "/worker/protocol",
        PROTOCOL_VERSION.into(),
        "версия протокола",
    )?;
    expect("/model/id", model.id.into(), "модель")?;
    expect("/model/output", model.output.into(), "вид выхода")?;
    expect("/model/revision", model.revision.into(), "ревизия")?;
    expect(
        "/model/manifest_sha256",
        model.manifest_sha256.into(),
        "манифест весов",
    )?;
    expect("/model/sample_rate", SAMPLE_RATE.into(), "частота")?;
    expect("/runtime/device", "cpu".into(), "устройство")?;
    let vocab: Vec<String> = serde_json::from_value(field("/model/vocab"))
        .map_err(|_| protocol("в отчёте нет словаря"))?;
    let number = |pointer: &str| {
        field(pointer)
            .as_u64()
            .ok_or_else(|| protocol(format!("в отчёте нет {pointer}")))
    };
    let text = |pointer: &str| field(pointer).as_str().unwrap_or("").to_owned();
    let identity = ModelIdentity {
        model_id: model.id.into(),
        output: model.output.into(),
        repository: text("/model/repository"),
        revision: model.revision.into(),
        license: text("/model/license"),
        manifest_sha256: model.manifest_sha256.into(),
        worker_sha256: sha256_hex(&bytes),
        worker_byte_length: bytes.len() as u64,
        worker_version: text("/worker/version"),
        inference: text("/runtime/inference"),
        device: text("/runtime/device"),
        threads: number("/runtime/threads")? as u32,
        sample_rate: SAMPLE_RATE,
        frame_stride: number("/model/frame_stride")? as usize,
        receptive_field: number("/model/receptive_field")? as usize,
        lead_pad_samples: number("/model/lead_pad_samples")? as usize,
        trail_pad_samples: number("/model/trail_pad_samples")? as usize,
        blank_id: number("/model/blank_id")? as usize,
        vocab,
    };
    if identity.vocab.len() != model.vocab
        || identity.blank_id >= identity.vocab.len()
        || identity.frame_stride == 0
        || identity.receptive_field < identity.frame_stride
    {
        return Err(identity_mismatch(
            "словарь или геометрия кадров не совпали с ожидаемыми",
        ));
    }
    let classes = classes_for(&identity);
    Ok(ModelHandle {
        identity,
        model_dir: model_dir.to_path_buf(),
        worker: worker.to_path_buf(),
        address_space: model.address_space,
        classes,
    })
}

/// Читает файл эмиссий ровно ожидаемой длины без перехода по ссылкам.
fn read_output(path: &Path, expected: u64) -> Result<Vec<u8>, AssetError> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| protocol("воркер не создал файл эмиссий или это не обычный файл"))?;
    let file = std::fs::File::from(fd);
    let meta = file
        .metadata()
        .map_err(|_| protocol("файл эмиссий не читается"))?;
    if !meta.is_file() || meta.len() != expected {
        return Err(protocol(format!(
            "длина эмиссий {} байт вместо {expected}",
            meta.len()
        )));
    }
    let mut bytes = Vec::with_capacity(expected as usize);
    file.take(expected + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| protocol("файл эмиссий не читается"))?;
    if bytes.len() as u64 != expected {
        return Err(protocol("файл эмиссий изменился при чтении"));
    }
    Ok(bytes)
}

/// Время вызова воркера (диагностика, не детерминирована).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CallTiming {
    pub items: usize,
    pub load_ms: f64,
    pub wall_ms: f64,
}

/// Эмиссии для группы моно-входов 16 kHz за один вызов воркера. Любое
/// отклонение протокола (другая модель, число кадров, SHA вывода) — сбой
/// среды, а не свойство записи.
pub fn emissions(
    handle: &ModelHandle,
    inputs: &[&[f32]],
    timeout: Duration,
) -> Result<(Vec<Emissions>, CallTiming), AssetError> {
    if inputs.is_empty() {
        return Ok((
            Vec::new(),
            CallTiming {
                items: 0,
                load_ms: 0.0,
                wall_ms: 0.0,
            },
        ));
    }
    if inputs.len() > MAX_CALL_ITEMS {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            format!("не больше {MAX_CALL_ITEMS} входов за вызов"),
        ));
    }
    let workspace = TempWorkspace::create("speech-recognize").map_err(|e| {
        backend_failure(
            "recognizer_backend_failed",
            format!("временный каталог: {e}"),
            serde_json::json!({}),
        )
    })?;
    let mut paths = Vec::with_capacity(inputs.len());
    for (i, samples) in inputs.iter().enumerate() {
        let input = workspace.path().join(format!("in-{i}.f32"));
        let output = workspace.path().join(format!("out-{i}.f32"));
        let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&input, &raw).map_err(|e| {
            backend_failure(
                "recognizer_backend_failed",
                format!("временный файл: {e}"),
                serde_json::json!({}),
            )
        })?;
        paths.push((input, output, sha256_hex(&raw)));
    }
    let threads = threads_env(handle.identity.threads);
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "process".as_ref(),
        "--model".as_ref(),
        handle.identity.model_id.as_ref(),
        "--model-dir".as_ref(),
        handle.model_dir.as_os_str(),
    ];
    for (input, output, _) in &paths {
        args.extend::<[&std::ffi::OsStr; 4]>([
            "--input".as_ref(),
            input.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
        ]);
    }
    let start = Instant::now();
    let finished = run_limited(
        &handle.worker,
        handle.address_space,
        &[("MATMUL_NUM_THREADS", &threads)],
        &args,
        timeout,
        "recognizer",
    )?;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    if finished.code != Some(0) {
        return Err(worker_error(finished.code, &finished.stdout));
    }
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout)
        .map_err(|_| protocol("отчёт process воркера не разбирается"))?;
    if report["kind"] != "speech_recognize_result"
        || report["model_id"] != handle.identity.model_id.as_str()
        || report["model_manifest_sha256"] != handle.identity.manifest_sha256.as_str()
        || report["worker"]["version"] != handle.identity.worker_version.as_str()
    {
        return Err(protocol(
            "воркер отчитался о другой модели или в неизвестном формате",
        ));
    }
    let items = report["items"]
        .as_array()
        .filter(|items| items.len() == inputs.len())
        .ok_or_else(|| protocol("число результатов не совпало с числом входов"))?;
    let vocab = handle.identity.vocab.len();
    let geometry = |samples: usize| {
        let padded = samples + handle.identity.lead_pad_samples + handle.identity.trail_pad_samples;
        speech_frames(padded)
    };
    let mut out = Vec::with_capacity(inputs.len());
    for ((item, (_, output, input_sha)), samples) in items.iter().zip(&paths).zip(inputs) {
        let frames = item["frames"]
            .as_u64()
            .ok_or_else(|| protocol("в результате нет числа кадров"))?
            as usize;
        if item["input_sha256"] != input_sha.as_str()
            || item["vocab"].as_u64() != Some(vocab as u64)
            || frames != geometry(samples.len())
        {
            return Err(protocol(
                "результат воркера не согласован с входом (SHA, словарь или число кадров)",
            ));
        }
        let length = Emissions::byte_length(frames, vocab)
            .ok_or_else(|| protocol("размер эмиссий воркера не представим"))?;
        let bytes = read_output(output, length as u64)?;
        if item["output_sha256"] != sha256_hex(&bytes).as_str() {
            return Err(protocol("SHA-256 эмиссий не совпал с отчётом воркера"));
        }
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        out.push(
            Emissions::new(frames, vocab, values)
                .ok_or_else(|| protocol("эмиссии содержат неконечные значения"))?,
        );
    }
    let timing = CallTiming {
        items: inputs.len(),
        load_ms: report["load_ms"].as_f64().unwrap_or(0.0),
        wall_ms,
    };
    Ok((out, timing))
}

/// Число кадров свёрточного стека wav2vec2/HuBERT для `samples` отсчётов.
pub fn speech_frames(samples: usize) -> usize {
    const KERNELS: [(usize, usize); 7] = [(10, 5), (3, 2), (3, 2), (3, 2), (3, 2), (2, 2), (2, 2)];
    KERNELS.iter().fold(
        samples,
        |len, (k, s)| {
            if len < *k { 0 } else { (len - k) / s + 1 }
        },
    )
}

/// Явная установка весов делегируется воркеру (он владеет манифестом).
pub fn install_model(
    worker: &Path,
    model: ExpectedModel,
    source: &Path,
    models_root: &Path,
) -> Result<serde_json::Value, AssetError> {
    let finished = run_limited(
        worker,
        model.address_space,
        &[],
        &[
            "install-model".as_ref(),
            "--model".as_ref(),
            model.id.as_ref(),
            "--source".as_ref(),
            source.as_os_str(),
            "--models-root".as_ref(),
            models_root.as_os_str(),
        ],
        Duration::from_secs(600),
        "recognizer",
    )?;
    if finished.code != Some(0) {
        return Err(worker_error(finished.code, &finished.stdout));
    }
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout)
        .map_err(|_| protocol("отчёт install-model не разбирается"))?;
    if report["model"]["manifest_sha256"] != model.manifest_sha256 {
        return Err(identity_mismatch("установлена модель с другим манифестом"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_count_matches_the_convolution_stack() {
        assert_eq!(speech_frames(16_000), 49);
        assert_eq!(speech_frames(400), 1);
        assert_eq!(speech_frames(399), 0);
    }

    #[test]
    fn phoneme_classes_merge_devoiced_vowels_and_drop_service_tokens() {
        let vocab: Vec<String> = ["PAD", "UNK", "a", "i", "I", "pau", "sil", "N", "cl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let identity = ModelIdentity {
            model_id: "m".into(),
            output: "phoneme".into(),
            repository: String::new(),
            revision: String::new(),
            license: String::new(),
            manifest_sha256: String::new(),
            worker_sha256: String::new(),
            worker_byte_length: 0,
            worker_version: String::new(),
            inference: String::new(),
            device: "cpu".into(),
            threads: 1,
            sample_rate: SAMPLE_RATE,
            frame_stride: 320,
            receptive_field: 400,
            lead_pad_samples: 0,
            trail_pad_samples: 0,
            blank_id: 0,
            vocab,
        };
        let classes = classes_for(&identity);
        assert_eq!(classes.names, ["a", "i", "N", "cl"]);
        assert_eq!(classes.tokens[1], [3, 4]);
    }
}
