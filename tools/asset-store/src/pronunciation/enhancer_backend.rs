//! Граница локального backend улучшения речи: изолированный процесс
//! `speech-enhance-dfn` (DeepFilterNet3 на CPU, Rust + tract) или
//! экспериментальный опциональный `speech-enhance-resemble` (только
//! denoiser Resemble Enhance, Python + PyTorch, CUDA при наличии).
//!
//! `asset-store` не связывается с библиотеками инференса: воркер — отдельный
//! исполняемый файл того же workspace, который запускается через `prlimit`
//! (адресное пространство, CPU, дескрипторы) с пределом времени. Обмен —
//! файлы PCM `f32` во временном каталоге и один JSON в stdout. Сеть не
//! используется, веса читаются только из явно указанного каталога.
//!
//! Идентичность backend — SHA-256 исполняемого файла воркера, его отчёт
//! `identify` (версии воркера и библиотеки, вариант и манифест весов,
//! частота, задержка) и параметры инференса. Всё это входит в отпечаток
//! конфигурации результата. Отчёт сверяется с закреплёнными ожиданиями:
//! другая модель (например, DeepFilterNet2) или другие веса не подменяют
//! DeepFilterNet3 незаметно.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::hashing::sha256_hex;
use crate::temp_workspace::TempWorkspace;
use crate::{AssetError, ErrorCode};

pub const WORKER_NAME: &str = "speech-enhance-dfn";
pub const BACKEND_DEEPFILTERNET3: &str = "deepfilternet3";
pub const MODEL_NAME: &str = "DeepFilterNet3";
/// Отпечаток закреплённого манифеста весов DeepFilterNet3 (четыре файла
/// upstream-архива на закреплённом коммите), см. `tools/speech-enhance`.
pub const DEEPFILTERNET3_MANIFEST_SHA256: &str =
    "8b378da4a759b6a244bb6bc9786e7ffd74c67f3f878dbd40e53c2f6565d05e5d";
pub const PROTOCOL_VERSION: u32 = 1;
pub const BACKEND_SAMPLE_RATE: u32 = 48_000;
pub const RESEMBLE_WORKER_NAME: &str = "speech-enhance-resemble";
pub const BACKEND_RESEMBLE_DENOISE: &str = "resemble-enhance-denoise";
/// Отпечаток закреплённого манифеста denoiser Resemble Enhance (чекпойнт и
/// `hparams.yaml` на закреплённой ревизии), см. `tools/speech-enhance/resemble`.
pub const RESEMBLE_DENOISE_MANIFEST_SHA256: &str =
    "58f26fd76b782e165581c7114b3e9046a9d8b3986d47f36f6e0d8818c4e897d3";
const GIB: u64 = 1024 * 1024 * 1024;

/// Вид backend. Выбирается профилем этапа; по умолчанию — DeepFilterNet3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BackendKind {
    #[default]
    #[serde(rename = "deepfilternet3")]
    DeepFilterNet3,
    /// Только denoiser Resemble Enhance: генеративная часть (LCFM +
    /// вокодер) синтезирует речь заново и в этап не допускается.
    #[serde(rename = "resemble-enhance-denoise")]
    ResembleEnhanceDenoise,
}

impl BackendKind {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => BACKEND_DEEPFILTERNET3,
            Self::ResembleEnhanceDenoise => BACKEND_RESEMBLE_DENOISE,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::DeepFilterNet3, Self::ResembleEnhanceDenoise]
            .into_iter()
            .find(|kind| kind.id() == id)
    }

    pub fn model_name(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => MODEL_NAME,
            Self::ResembleEnhanceDenoise => "Resemble Enhance",
        }
    }

    pub fn model_variant(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => BACKEND_DEEPFILTERNET3,
            Self::ResembleEnhanceDenoise => "denoiser",
        }
    }

    pub fn manifest_sha256(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => DEEPFILTERNET3_MANIFEST_SHA256,
            Self::ResembleEnhanceDenoise => RESEMBLE_DENOISE_MANIFEST_SHA256,
        }
    }

    pub fn worker_name(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => WORKER_NAME,
            Self::ResembleEnhanceDenoise => RESEMBLE_WORKER_NAME,
        }
    }

    /// Код причины выбранного улучшенного варианта.
    pub fn enhanced_reason(self) -> &'static str {
        match self {
            Self::DeepFilterNet3 => "enhanced_by_deepfilternet3",
            Self::ResembleEnhanceDenoise => "enhanced_by_resemble_enhance_denoise",
        }
    }

    /// Параметры `InferenceParams` относятся только к DeepFilterNet3.
    pub fn takes_inference_params(self) -> bool {
        self == Self::DeepFilterNet3
    }

    /// Предел адресного пространства процесса воркера: CUDA резервирует
    /// виртуальную память с запасом, поэтому у PyTorch-воркера он шире.
    fn address_space_limit(self) -> u64 {
        match self {
            Self::DeepFilterNet3 => 4 * GIB,
            Self::ResembleEnhanceDenoise => 16 * GIB,
        }
    }
}
const STDOUT_LIMIT: u64 = 256 * 1024;
const STDERR_LIMIT: u64 = 16 * 1024;
/// Коды выхода протокола воркера.
const EXIT_MODEL: i32 = 3;
const EXIT_INPUT: i32 = 4;

/// Коды сбоев среды backend: вход остаётся незавершённым и повторяется
/// следующим запуском, в исход входа и кэш сбой не превращается.
pub const INFRASTRUCTURE_BLOCKERS: [&str; 6] = [
    "enhancer_backend_unavailable",
    "enhancer_model_unavailable",
    "enhancer_backend_timeout",
    "enhancer_backend_crashed",
    "enhancer_backend_failed",
    "enhancer_backend_protocol",
];

pub fn is_backend_failure(error: &AssetError) -> bool {
    error.details["blocker"]
        .as_str()
        .is_some_and(|code| INFRASTRUCTURE_BLOCKERS.contains(&code))
}

pub(crate) fn backend_failure(
    code: &str,
    message: impl Into<String>,
    extra: serde_json::Value,
) -> AssetError {
    let mut details = serde_json::json!({"blocker": code});
    if let (Some(map), serde_json::Value::Object(extra)) = (details.as_object_mut(), extra) {
        map.extend(extra);
    }
    AssetError::with_details(ErrorCode::ValidatorFailure, message, details)
}

fn identity_mismatch(message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        ErrorCode::IntegrityMismatch,
        message,
        serde_json::json!({"blocker": "enhancer_identity_mismatch"}),
    )
}

/// Параметры инференса DeepFilterNet3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceParams {
    /// Предел ослабления модели, dB (100 — без предела).
    pub atten_lim_db: f32,
    /// Коэффициент постфильтра; 0 — выключен.
    pub post_filter_beta: f32,
    /// Зеркальный контекст прогрева модели на каждом краю клипа, мс.
    pub context_ms: u32,
}

impl Default for InferenceParams {
    fn default() -> Self {
        Self {
            atten_lim_db: 100.0,
            post_filter_beta: 0.0,
            context_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerFile {
    pub path: String,
    pub sha256: String,
    pub byte_length: u64,
}

/// Проверенная идентичность backend для отпечатка конфигурации и provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendIdentity {
    pub backend: String,
    pub worker_sha256: String,
    pub worker_byte_length: u64,
    /// Отчёт `identify` воркера как есть.
    pub report: serde_json::Value,
    pub model_name: String,
    pub model_variant: String,
    pub model_manifest_sha256: String,
    pub delay_samples: u64,
    pub params: InferenceParams,
}

impl BackendIdentity {
    /// Режим исполнения инференса по подтверждённому отчёту `identify`:
    /// DeepFilterNet3 — `tract` на CPU в один поток, Resemble — PyTorch на
    /// фактическом устройстве (`cpu` или `cuda`). Влияет на байты результата,
    /// поэтому входит в отпечаток конфигурации и в отчёт пакета.
    pub fn execution_mode(&self) -> String {
        let runtime = &self.report["runtime"];
        let text = |key: &str| runtime[key].as_str().unwrap_or("unknown");
        match BackendKind::from_id(&self.backend) {
            Some(BackendKind::DeepFilterNet3) => {
                let threads = match runtime["threads"].as_u64() {
                    Some(1) => "single-thread".to_owned(),
                    Some(n) => format!("{n}-threads"),
                    None => "unknown-threads".to_owned(),
                };
                format!("{}-{}-{threads}", text("device"), text("inference"))
            }
            Some(BackendKind::ResembleEnhanceDenoise) => format!("{}-pytorch", text("device")),
            None => format!("unknown-backend-{}", self.backend),
        }
    }
}

impl BackendIdentity {
    /// Исходник, который исполняет запускатель Python-воркера (из
    /// подтверждённого `identify`). У воркера-исполняемого файла его нет:
    /// код покрывает снимок самого исполняемого файла.
    pub fn worker_source(&self) -> Option<PathBuf> {
        if self.backend != BACKEND_RESEMBLE_DENOISE {
            return None;
        }
        self.report["worker"]["source_path"]
            .as_str()
            .map(PathBuf::from)
    }
}

/// Поля среды исполнения, которые обязан подтвердить `identify`: без них
/// режим исполнения в отпечатке и отчёте был бы догадкой.
fn check_runtime(kind: BackendKind, report: &serde_json::Value) -> Result<(), AssetError> {
    let runtime = &report["runtime"];
    let valid = match kind {
        BackendKind::DeepFilterNet3 => {
            runtime["device"] == "cpu" && runtime["inference"] == "tract" && runtime["threads"] == 1
        }
        BackendKind::ResembleEnhanceDenoise => {
            matches!(runtime["device"].as_str(), Some("cpu" | "cuda"))
                && runtime["torch"].is_string()
                && report["worker"]["source_sha256"]
                    .as_str()
                    .is_some_and(|sha| sha.len() == 64)
                && report["worker"]["source_path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).is_absolute())
        }
    };
    if valid {
        Ok(())
    } else {
        Err(identity_mismatch(format!(
            "отчёт identify не подтверждает среду исполнения backend {}: {runtime}",
            kind.id()
        )))
    }
}

/// Снимок файла воркера (исполняемого файла или исходника Python-воркера):
/// замена во время пакета останавливает его (как у FFmpeg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSnapshot(PathBuf, [i64; 7]);

pub fn worker_snapshot(path: &Path) -> Result<WorkerSnapshot, AssetError> {
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(path).map_err(|e| {
        backend_failure(
            "enhancer_backend_unavailable",
            format!("исполняемый файл воркера недоступен: {e}"),
            serde_json::json!({}),
        )
    })?;
    let m = std::fs::metadata(&real).map_err(|e| {
        backend_failure(
            "enhancer_backend_unavailable",
            e.to_string(),
            serde_json::json!({}),
        )
    })?;
    Ok(WorkerSnapshot(
        real,
        [
            m.dev() as i64,
            m.ino() as i64,
            m.size() as i64,
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        ],
    ))
}

pub fn worker_changed() -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        "исполняемый файл или исходник воркера улучшения речи заменён во время пакета: результат не сохранён",
        serde_json::json!({"blocker": "enhancer_backend_changed"}),
    )
}

/// Воркер: явный путь, иначе файл рядом с текущим исполняемым (сборка
/// workspace кладёт их в один каталог), иначе первый в `PATH`.
pub fn locate_worker(explicit: Option<&Path>, kind: BackendKind) -> Result<PathBuf, AssetError> {
    let name = kind.worker_name();
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(path) = explicit {
        return if executable(path) {
            std::path::absolute(path).map_err(|e| AssetError::io("путь воркера", e))
        } else {
            Err(backend_failure(
                "enhancer_backend_unavailable",
                format!("воркер {} не найден или не исполняемый", path.display()),
                serde_json::json!({}),
            ))
        };
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| executable(p));
    let in_path = || {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|p| executable(p))
        })
    };
    sibling.or_else(in_path).ok_or_else(|| {
        let hint = match kind {
            BackendKind::DeepFilterNet3 => "соберите `cargo build --release -p speech-enhance`",
            BackendKind::ResembleEnhanceDenoise => {
                "установите запускатель по docs/ru/asset-store/enhancer.md"
            }
        };
        backend_failure(
            "enhancer_backend_unavailable",
            format!("воркер {name} не найден: {hint} или укажите --backend-bin"),
            serde_json::json!({}),
        )
    })
}

pub(crate) fn read_capped(mut reader: impl Read, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    (&mut reader).take(cap).read_to_end(&mut data)?;
    std::io::copy(&mut reader, &mut std::io::sink())?;
    Ok(data)
}

/// Результат завершившегося процесса воркера.
pub(crate) struct Finished {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
}

/// Запуск воркера через prlimit с пределом времени. Таймаут и завершение
/// сигналом — сбои среды с собственными кодами. Воркер живёт в собственной
/// группе процессов: при таймауте завершается вся группа, иначе потомок,
/// унаследовавший каналы, держал бы чтение stdout/stderr после таймаута.
fn run_worker(
    worker: &Path,
    kind: BackendKind,
    args: &[&std::ffi::OsStr],
    timeout: Duration,
) -> Result<Finished, AssetError> {
    run_limited(
        worker,
        kind.address_space_limit(),
        1,
        &[],
        args,
        timeout,
        "enhancer",
    )
}

/// Предел процессорного времени для `prlimit --cpu`: `RLIMIT_CPU` суммирует
/// время всех потоков процесса, поэтому воркер в `cpu_threads` потоках
/// получает `timeout × cpu_threads`; реальное время ограничено отдельно.
pub(crate) fn cpu_limit_seconds(timeout: Duration, cpu_threads: u32) -> u64 {
    timeout
        .as_secs()
        .max(1)
        .saturating_mul(u64::from(cpu_threads.max(1)))
}

/// Общий ограниченный запуск воркера инференса (улучшение речи и
/// распознавание): `prlimit` с пределами адресного пространства, CPU и
/// дескрипторов, собственная группа процессов, предел времени. Коды сбоев
/// среды — `<prefix>_backend_{unavailable,failed,timeout,crashed,protocol}`.
pub(crate) fn run_limited(
    worker: &Path,
    address_space: u64,
    cpu_threads: u32,
    env: &[(&str, &str)],
    args: &[&std::ffi::OsStr],
    timeout: Duration,
    prefix: &str,
) -> Result<Finished, AssetError> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let code = |suffix: &str| format!("{prefix}_backend_{suffix}");
    let cpu = cpu_limit_seconds(timeout, cpu_threads).to_string();
    let mut child = Command::new("prlimit")
        .envs(env.iter().copied())
        .arg(format!("--as={address_space}"))
        .arg(format!("--cpu={cpu}"))
        .args(["--nofile=64", "--"])
        .arg(worker)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            backend_failure(
                &code("unavailable"),
                format!("не удалось запустить воркер через prlimit: {e}"),
                serde_json::json!({}),
            )
        })?;
    let out = child.stdout.take().expect("stdout открыт как канал");
    let err = child.stderr.take().expect("stderr открыт как канал");
    let stdout = thread::spawn(move || read_capped(out, STDOUT_LIMIT + 1));
    let stderr = thread::spawn(move || read_capped(err, STDERR_LIMIT));
    let group = rustix::process::Pid::from_raw(child.id() as i32);
    let kill_group = || {
        if let Some(group) = group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    };
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(5)),
            result => {
                // Группа завершается до сбора лидера: пока он не собран, его
                // PID не может достаться чужой группе.
                kill_group();
                let _ = child.kill();
                let _ = child.wait();
                break Err(if result.is_err() {
                    backend_failure(
                        &code("failed"),
                        "ошибка ожидания воркера",
                        serde_json::json!({}),
                    )
                } else {
                    backend_failure(
                        &code("timeout"),
                        format!("воркер превысил {} с", timeout.as_secs_f64()),
                        serde_json::json!({"timeout_seconds": timeout.as_secs_f64()}),
                    )
                });
            }
        }
    };
    // Лидер мог выйти, оставив потомка с унаследованными каналами: чтение
    // ждёт не дольше общего предела, затем группа завершается. Пока каналы
    // кем-то удерживаются, группа не пуста и её PGID не переиспользуется.
    let readers_done = || stdout.is_finished() && stderr.is_finished();
    let wait_readers = |until: Instant| {
        while !readers_done() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
    };
    // Короткая отсрочка даёт дочитать конец канала воркеру, вышедшему у самой
    // границы предела: без потомков EOF приходит сразу после выхода.
    wait_readers((start + timeout).max(Instant::now() + Duration::from_millis(200)));
    if !readers_done() {
        kill_group();
        wait_readers(Instant::now() + Duration::from_secs(1));
        // Если чтение так и не завершилось, его потоки оставлены: возврат не
        // ждёт чужих каналов.
        status?;
        return Err(backend_failure(
            &code("timeout"),
            "потомки воркера удерживали каналы вывода дольше предела времени",
            serde_json::json!({"timeout_seconds": timeout.as_secs_f64()}),
        ));
    }
    let stdout = stdout.join().ok().and_then(Result::ok).unwrap_or_default();
    let _ = stderr.join();
    let status = status?;
    if matches!(status.code(), Some(126 | 127)) {
        return Err(backend_failure(
            &code("unavailable"),
            "prlimit не смог выполнить воркер",
            serde_json::json!({}),
        ));
    }
    if let Some(signal) = status.signal() {
        // SIGXCPU/SIGKILL по пределу CPU — тот же класс, что таймаут.
        let failure = if matches!(signal, 9 | 24) {
            code("timeout")
        } else {
            code("crashed")
        };
        return Err(backend_failure(
            &failure,
            format!("воркер завершён сигналом {signal} (авария, нехватка памяти или предел CPU)"),
            serde_json::json!({"signal": signal}),
        ));
    }
    if stdout.len() as u64 > STDOUT_LIMIT {
        return Err(backend_failure(
            &code("protocol"),
            "воркер превысил предел stdout",
            serde_json::json!({}),
        ));
    }
    Ok(Finished {
        code: status.code(),
        stdout,
    })
}

/// Ненулевой код выхода воркера → классифицированная ошибка.
fn worker_error(finished: &Finished) -> AssetError {
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout).unwrap_or_default();
    let code = report["code"].as_str().unwrap_or("").to_owned();
    let message = report["message"].as_str().unwrap_or("").to_owned();
    match finished.code {
        Some(EXIT_MODEL) => backend_failure(
            "enhancer_model_unavailable",
            format!("веса модели недоступны или не проходят проверку: {code}: {message}"),
            serde_json::json!({"model_error": code}),
        ),
        // Вход готовит сам этап: отказ воркера от него — нарушение протокола,
        // а не свойство записи.
        Some(EXIT_INPUT) => AssetError::with_details(
            ErrorCode::ValidatorFailure,
            format!("воркер отверг подготовленный этапом вход: {code}: {message}"),
            serde_json::json!({"blocker": "enhancer_input_contract"}),
        ),
        other => backend_failure(
            "enhancer_backend_failed",
            format!("воркер завершился с кодом {other:?}: {code}: {message}"),
            serde_json::json!({"exit_code": other}),
        ),
    }
}

fn protocol(message: impl Into<String>) -> AssetError {
    backend_failure("enhancer_backend_protocol", message, serde_json::json!({}))
}

/// Параметры DeepFilterNet3 конечны и неотрицательны; у backend без
/// параметров они обязаны остаться значениями по умолчанию, чтобы профиль не
/// описывал настройки, которых backend не применяет.
pub fn check_params(kind: BackendKind, params: &InferenceParams) -> Result<(), AssetError> {
    let valid = if kind.takes_inference_params() {
        params.atten_lim_db.is_finite()
            && params.atten_lim_db >= 0.0
            && params.post_filter_beta.is_finite()
            && params.post_filter_beta >= 0.0
            && params.context_ms <= 5_000
    } else {
        *params == InferenceParams::default()
    };
    if valid {
        Ok(())
    } else {
        Err(AssetError::new(
            ErrorCode::InvalidTransition,
            format!(
                "параметры инференса недопустимы для backend {}: у DeepFilterNet3 они конечны и неотрицательны, у остальных — значения по умолчанию",
                kind.id()
            ),
        ))
    }
}

/// Проверка установки: веса, загрузка модели и идентичность воркера.
pub fn identify(
    worker: &Path,
    model_dir: &Path,
    kind: BackendKind,
    params: &InferenceParams,
) -> Result<BackendIdentity, AssetError> {
    check_params(kind, params)?;
    let bytes = std::fs::read(worker).map_err(|e| {
        backend_failure(
            "enhancer_backend_unavailable",
            format!("воркер не читается: {e}"),
            serde_json::json!({}),
        )
    })?;
    let finished = run_worker(
        worker,
        kind,
        &[
            "identify".as_ref(),
            "--model-dir".as_ref(),
            model_dir.as_os_str(),
        ],
        Duration::from_secs(60),
    )?;
    if finished.code != Some(0) {
        return Err(worker_error(&finished));
    }
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout)
        .map_err(|_| protocol("отчёт identify воркера не разбирается"))?;
    let field = |pointer: &str| report.pointer(pointer).cloned().unwrap_or_default();
    let expect = |pointer: &str, value: serde_json::Value, what: &str| {
        if field(pointer) == value {
            Ok(())
        } else {
            Err(identity_mismatch(format!(
                "идентичность backend не совпала ({what}): ожидалось {value}, получено {}",
                field(pointer)
            )))
        }
    };
    expect("/kind", "speech_enhance_identity".into(), "вид отчёта")?;
    expect(
        "/worker/protocol",
        PROTOCOL_VERSION.into(),
        "версия протокола",
    )?;
    expect("/backend", kind.id().into(), "backend")?;
    expect("/model/name", kind.model_name().into(), "имя модели")?;
    expect(
        "/model/variant",
        kind.model_variant().into(),
        "вариант модели",
    )?;
    expect(
        "/model/manifest_sha256",
        kind.manifest_sha256().into(),
        "манифест весов",
    )?;
    expect("/stream/sample_rate", BACKEND_SAMPLE_RATE.into(), "частота")?;
    check_runtime(kind, &report)?;
    let delay = field("/stream/delay_samples")
        .as_u64()
        .ok_or_else(|| protocol("в отчёте нет задержки модели"))?;
    Ok(BackendIdentity {
        backend: kind.id().into(),
        worker_sha256: sha256_hex(&bytes),
        worker_byte_length: bytes.len() as u64,
        report,
        model_name: kind.model_name().into(),
        model_variant: kind.model_variant().into(),
        model_manifest_sha256: kind.manifest_sha256().into(),
        delay_samples: delay,
        params: params.clone(),
    })
}

/// Диагностика одного вызова воркера.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRun {
    /// Диагностика воркера как есть (кадры, задержка, сброс хвоста, LSNR).
    pub diagnostics: serde_json::Value,
    pub delay_samples: u64,
    pub flush_samples: u64,
    pub identical_channels: bool,
    pub lsnr_mean_db: Option<f64>,
    /// Время процесса воркера (стена), мс. Диагностика, не детерминирована.
    pub wall_ms: f64,
}

/// Читает вывод воркера ровно ожидаемой длины. Файл другой длины, не
/// обычный файл или ссылка отвергаются до чтения, а само чтение ограничено
/// ожидаемым объёмом + 1 байт: неисправный воркер не исчерпает память
/// родителя длинным (в том числе разреженным) выводом.
fn read_output(path: &Path, expected: u64) -> Result<Vec<u8>, AssetError> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| protocol("воркер не создал файл вывода или это не обычный файл"))?;
    let file = std::fs::File::from(fd);
    let meta = file
        .metadata()
        .map_err(|_| protocol("файл вывода воркера не читается"))?;
    if !meta.is_file() || meta.len() != expected {
        return Err(protocol(format!(
            "длина вывода воркера {} байт вместо {expected}: добавленная задержка, тишина или обрезка недопустимы",
            meta.len()
        )));
    }
    let mut bytes = Vec::with_capacity(expected as usize);
    file.take(expected + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| protocol("файл вывода воркера не читается"))?;
    if bytes.len() as u64 != expected {
        return Err(protocol(format!(
            "вывод воркера изменился при чтении: {} байт вместо {expected}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Улучшает PCM 48 kHz. Длина результата обязана совпасть со входом;
/// любое отклонение протокола — сбой среды, а не «улучшение».
pub fn enhance(
    worker: &Path,
    model_dir: &Path,
    identity: &BackendIdentity,
    channels: usize,
    samples: &[f32],
    timeout: Duration,
) -> Result<(Vec<f32>, InferenceRun), AssetError> {
    let workspace = TempWorkspace::create("speech-enhance").map_err(|e| {
        backend_failure(
            "enhancer_backend_failed",
            format!("временный каталог: {e}"),
            serde_json::json!({}),
        )
    })?;
    let input = workspace.path().join("input.f32");
    let output = workspace.path().join("output.f32");
    let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&input, raw).map_err(|e| {
        backend_failure(
            "enhancer_backend_failed",
            format!("временный файл: {e}"),
            serde_json::json!({}),
        )
    })?;
    let kind = BackendKind::from_id(&identity.backend)
        .ok_or_else(|| protocol(format!("неизвестный backend {}", identity.backend)))?;
    let channels_text = channels.to_string();
    let atten = identity.params.atten_lim_db.to_string();
    let beta = identity.params.post_filter_beta.to_string();
    let context = identity.params.context_ms.to_string();
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "process".as_ref(),
        "--model-dir".as_ref(),
        model_dir.as_os_str(),
        "--input".as_ref(),
        input.as_os_str(),
        "--output".as_ref(),
        output.as_os_str(),
        "--channels".as_ref(),
        channels_text.as_ref(),
    ];
    if kind.takes_inference_params() {
        args.extend::<[&std::ffi::OsStr; 6]>([
            "--atten-lim-db".as_ref(),
            atten.as_ref(),
            "--post-filter-beta".as_ref(),
            beta.as_ref(),
            "--context-ms".as_ref(),
            context.as_ref(),
        ]);
    }
    let start = Instant::now();
    let finished = run_worker(worker, kind, &args, timeout)?;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    if finished.code != Some(0) {
        return Err(worker_error(&finished));
    }
    let report: serde_json::Value = serde_json::from_slice(&finished.stdout)
        .map_err(|_| protocol("отчёт process воркера не разбирается"))?;
    if report["kind"] != "speech_enhance_result"
        || report["model_manifest_sha256"] != identity.model_manifest_sha256.as_str()
    {
        return Err(protocol(
            "воркер отчитался о другой модели или в неизвестном формате",
        ));
    }
    let bytes = read_output(&output, samples.len() as u64 * 4)?;
    if report["output_sha256"] != sha256_hex(&bytes).as_str() {
        return Err(protocol("SHA-256 вывода не совпал с отчётом воркера"));
    }
    let diagnostics = report["diagnostics"].clone();
    let delay = diagnostics["delay_samples"].as_u64().unwrap_or(u64::MAX);
    if delay != identity.delay_samples
        || diagnostics["output_frames"].as_u64() != Some((samples.len() / channels) as u64)
        || diagnostics["channels"].as_u64() != Some(channels as u64)
    {
        return Err(protocol(
            "диагностика воркера не согласована с запросом (задержка, кадры, каналы)",
        ));
    }
    // Python-воркер исполняет исходник, который запускатель не фиксирует:
    // каждый процесс сообщает SHA-256 своего исходника и версии среды, и
    // они обязаны совпасть с `identify`, по которому рассчитан ключ кэша.
    if kind == BackendKind::ResembleEnhanceDenoise
        && (report["worker"] != identity.report["worker"]
            || report["runtime"] != identity.report["runtime"])
    {
        return Err(protocol(
            "воркер исполнил другой исходник или другую среду, чем подтвердил identify",
        ));
    }
    // Устройство входит в ключ кэша через отчёт `identify`; воркер, сам
    // выбирающий его в каждом процессе, не должен молча сменить его посреди
    // пакета. Воркеры без поля `device` (DeepFilterNet3) не затрагиваются.
    if let Some(device) = diagnostics.get("device")
        && *device != identity.report["runtime"]["device"]
    {
        return Err(protocol(format!(
            "воркер исполнялся на устройстве {device}, а не на {} из identify",
            identity.report["runtime"]["device"]
        )));
    }
    let enhanced: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    if enhanced.iter().any(|s| !s.is_finite()) {
        return Err(protocol("воркер выдал неконечные отсчёты"));
    }
    // LSNR сообщает только DeepFilterNet3; у остальных метрики нет.
    let lsnr = diagnostics["per_channel"].as_array().and_then(|channels| {
        let values: Vec<f64> = channels
            .iter()
            .filter_map(|c| c["lsnr_mean_db"].as_f64())
            .collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    });
    Ok((
        enhanced,
        InferenceRun {
            delay_samples: delay,
            flush_samples: diagnostics["flush_samples"].as_u64().unwrap_or(0),
            identical_channels: diagnostics["identical_channels"].as_bool().unwrap_or(false),
            lsnr_mean_db: lsnr,
            diagnostics,
            wall_ms,
        },
    ))
}

/// Явная установка весов делегируется воркеру (он владеет форматом архива).
pub fn install_model(
    worker: &Path,
    kind: BackendKind,
    source: Option<&Path>,
    models_root: &Path,
) -> Result<serde_json::Value, AssetError> {
    let mut args: Vec<&std::ffi::OsStr> = vec!["install-model".as_ref()];
    match (source, kind) {
        (Some(source), _) => {
            args.push("--source".as_ref());
            args.push(source.as_os_str());
        }
        (None, BackendKind::DeepFilterNet3) => args.push("--from-cargo-registry".as_ref()),
        (None, BackendKind::ResembleEnhanceDenoise) => {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "веса Resemble Enhance устанавливаются только из явного локального каталога --source",
            ));
        }
    }
    args.push("--models-root".as_ref());
    args.push(models_root.as_os_str());
    // Копирование и хеширование чекпойнта Resemble (≈700 МБ) дольше DF3.
    let finished = run_worker(worker, kind, &args, Duration::from_secs(600))?;
    if finished.code != Some(0) {
        return Err(worker_error(&finished));
    }
    serde_json::from_slice(&finished.stdout).map_err(|_| protocol("отчёт установки не разбирается"))
}

/// Каталог проверенной установки весов backend внутри корня моделей.
pub fn installed_model_dir(models_root: &Path, kind: BackendKind) -> PathBuf {
    models_root.join(kind.id()).join(kind.manifest_sha256())
}

/// Корень моделей по умолчанию: `$XDG_DATA_HOME/anki-deck-toolkit/speech-models`
/// (иначе `~/.local/share/...`) — вне рабочей копии и вне Git.
pub fn default_models_root() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|base| base.join("anki-deck-toolkit").join("speech-models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(kind: BackendKind, runtime: serde_json::Value) -> BackendIdentity {
        BackendIdentity {
            backend: kind.id().into(),
            worker_sha256: "a".repeat(64),
            worker_byte_length: 1,
            report: serde_json::json!({"runtime": runtime}),
            model_name: kind.model_name().into(),
            model_variant: kind.model_variant().into(),
            model_manifest_sha256: kind.manifest_sha256().into(),
            delay_samples: 0,
            params: InferenceParams::default(),
        }
    }

    /// Режим исполнения берётся из подтверждённой идентичности: Resemble не
    /// выдаёт себя за `tract`, а CPU и CUDA различаются в отпечатке.
    #[test]
    fn cpu_limit_covers_every_worker_thread() {
        let timeout = Duration::from_secs(600);
        assert_eq!(cpu_limit_seconds(timeout, 1), 600);
        assert_eq!(cpu_limit_seconds(timeout, 4), 2400);
        // Нулевые значения не дают предела 0, при котором воркер сразу
        // получил бы SIGXCPU.
        assert_eq!(cpu_limit_seconds(Duration::ZERO, 0), 1);
    }

    #[test]
    fn execution_mode_follows_confirmed_runtime() {
        let df3 = identity(
            BackendKind::DeepFilterNet3,
            serde_json::json!({"device": "cpu", "inference": "tract", "threads": 1}),
        );
        assert_eq!(df3.execution_mode(), "cpu-tract-single-thread");
        let cpu = identity(
            BackendKind::ResembleEnhanceDenoise,
            serde_json::json!({"device": "cpu", "torch": "x"}),
        );
        let cuda = identity(
            BackendKind::ResembleEnhanceDenoise,
            serde_json::json!({"device": "cuda", "torch": "x"}),
        );
        assert_eq!(cpu.execution_mode(), "cpu-pytorch");
        assert_eq!(cuda.execution_mode(), "cuda-pytorch");
        let profile = crate::pronunciation::enhancer::EnhancementProfile::resemble_denoise();
        let key = |id: &BackendIdentity| {
            crate::pronunciation::enhancer::config_sha256(&profile, id, &"b".repeat(64))
        };
        assert_ne!(key(&cpu), key(&cuda));
        assert_ne!(key(&cpu), key(&df3));
    }

    #[test]
    fn identify_must_confirm_runtime_of_each_backend() {
        let resemble = serde_json::json!({
            "runtime": {"device": "cuda", "torch": "2"},
            "worker": {"source_sha256": "c".repeat(64), "source_path": "/opt/w.py"},
        });
        assert!(check_runtime(BackendKind::ResembleEnhanceDenoise, &resemble).is_ok());
        for broken in [
            serde_json::json!({"runtime": {"device": "tpu", "torch": "2"}, "worker": resemble["worker"]}),
            serde_json::json!({"runtime": resemble["runtime"], "worker": {"source_sha256": "c".repeat(64)}}),
            serde_json::json!({"runtime": resemble["runtime"], "worker": {"source_path": "w.py", "source_sha256": "c".repeat(64)}}),
        ] {
            let error = check_runtime(BackendKind::ResembleEnhanceDenoise, &broken).unwrap_err();
            assert_eq!(error.details["blocker"], "enhancer_identity_mismatch");
        }
        let df3 =
            serde_json::json!({"runtime": {"device": "cpu", "inference": "tract", "threads": 1}});
        assert!(check_runtime(BackendKind::DeepFilterNet3, &df3).is_ok());
        assert!(check_runtime(BackendKind::DeepFilterNet3, &resemble).is_err());
    }
}
