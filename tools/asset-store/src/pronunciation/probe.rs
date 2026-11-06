//! Ограниченная проверка полного MP3-потока и фактически декодированной длительности.
use super::{MAX_AUDIO_BYTES, MAX_DURATION_SECONDS};
use crate::{AssetError, ErrorCode, hashing::sha256_hex, temp_workspace::TempWorkspace};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioProbe {
    pub schema_version: u32,
    pub codec: String,
    pub sha256: String,
    pub byte_length: u64,
    pub sample_rate: u32,
    pub channels: u32,
    pub frames: u32,
    pub declared_duration_seconds: f64,
    pub decoded_duration_seconds: f64,
    pub decoded_samples: u64,
}
fn failure(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::ValidatorFailure, message)
}

fn infrastructure_failure(code: &str, message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        message,
        serde_json::json!({"blocker":code}),
    )
}

/// Полный обход границ MPEG Layer III отвергает усечённые кадры, мусор и
/// изменяемые параметры потока ещё до запуска внешнего декодера.
fn frames(bytes: &[u8]) -> Result<(u32, u32, u32, u64), AssetError> {
    let mut offset = 0;
    if bytes.starts_with(b"ID3") {
        if bytes.len() < 10
            || !(2..=4).contains(&bytes[3])
            || bytes[6..10].iter().any(|b| b & 0x80 != 0)
        {
            return Err(failure("повреждён ID3v2"));
        }
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |n, b| (n << 7) | *b as usize);
        offset = 10
            + size
            + if bytes[3] == 4 && bytes[5] & 0x10 != 0 {
                10
            } else {
                0
            };
    }
    let end = if bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG" {
        bytes.len() - 128
    } else {
        bytes.len()
    };
    let mut count = 0;
    let mut expected = None;
    let mut samples = 0;
    while offset < end {
        let header = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| failure("усечён заголовок MP3"))?;
        if header[0] != 0xff || header[1] & 0xe6 != 0xe2 {
            return Err(failure("повреждена граница кадра MPEG Layer III"));
        }
        let version = (header[1] >> 3) & 3;
        let bitrate = (header[2] >> 4) as usize;
        let rate_index = ((header[2] >> 2) & 3) as usize;
        if version == 1 || bitrate == 0 || bitrate == 15 || rate_index == 3 {
            return Err(failure(
                "неподдерживаемый заголовок MP3 (включая free bitrate)",
            ));
        }
        let rate = [44100u32, 48000, 32000][rate_index]
            / match version {
                3 => 1,
                2 => 2,
                _ => 4,
            };
        let channels = if header[3] >> 6 == 3 { 1 } else { 2 };
        let parameters = (rate, channels, version);
        if expected.is_some_and(|p| p != parameters) {
            return Err(failure("параметры MP3 меняются внутри потока"));
        }
        expected = Some(parameters);
        let kbps = if version == 3 {
            [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
            ][bitrate]
        } else {
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160][bitrate]
        };
        let length = (if version == 3 { 144000 } else { 72000 }) * kbps / rate as usize
            + ((header[2] >> 1) & 1) as usize;
        offset += length;
        if offset > end {
            return Err(failure("усечён MP3-кадр"));
        }
        count += 1;
        samples += if version == 3 { 1152 } else { 576 };
        if samples as f64 / rate as f64 > MAX_DURATION_SECONDS + 0.25 {
            return Err(failure("MP3 превышает предел длительности"));
        }
    }
    let (rate, channels, _) = expected.ok_or_else(|| failure("MP3 не содержит кадров"))?;
    if count < 2 || offset != end {
        return Err(failure("недостаточно полных MP3-кадров"));
    }
    Ok((rate, channels, count, samples))
}

/// Первый исполняемый файл `tool` в `PATH` — тот, который запустит `run`.
fn resolve_tool(tool: &str) -> Result<std::path::PathBuf, AssetError> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(tool))
                .find(|candidate| {
                    std::fs::metadata(candidate)
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
        })
        .ok_or_else(|| {
            AssetError::with_details(
                ErrorCode::ValidatorFailure,
                format!("в PATH отсутствует обязательный инструмент {tool}"),
                serde_json::json!({"blocker":"audio_tool_missing","tool":tool}),
            )
        })
}

/// Снимок исполняемых файлов `prlimit`, `ffmpeg` и `ffprobe`, которые найдёт
/// `run`: фактический путь (после ссылок), устройство, inode, размер и
/// времена изменения. Замена или переустановка файла меняет снимок.
/// Разделяемые библиотеки в снимок не входят.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolFiles(Vec<(std::path::PathBuf, [i64; 7])>);

pub(super) fn tool_files() -> Result<ToolFiles, AssetError> {
    use std::os::unix::fs::MetadataExt;
    let mut files = Vec::new();
    for tool in ["prlimit", "ffmpeg", "ffprobe"] {
        let path = resolve_tool(tool)?;
        let real = std::fs::canonicalize(&path)
            .map_err(|e| infrastructure_failure("audio_tool_runtime_failed", e.to_string()))?;
        let m = std::fs::metadata(&real)
            .map_err(|e| infrastructure_failure("audio_tool_runtime_failed", e.to_string()))?;
        files.push((
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
        ));
    }
    Ok(ToolFiles(files))
}

/// Ошибка: инструменты заменены после определения окружения запуска.
pub(super) fn toolchain_changed() -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        "FFmpeg/ffprobe/prlimit заменены во время пакета: результат не сохранён; batch-resume заново определит окружение",
        serde_json::json!({"blocker":"audio_toolchain_changed"}),
    )
}

/// Первые `cap` байтов канала; остаток читается и отбрасывается.
fn read_capped(mut reader: impl Read, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    (&mut reader).take(cap).read_to_end(&mut data)?;
    std::io::copy(&mut reader, &mut std::io::sink())?;
    Ok(data)
}

/// prlimit задаёт CPU/адресное пространство до exec без unsafe/pre_exec.
/// stdout/stderr читаются одновременно с жёсткими пределами. При превышении времени
/// дочерний процесс завершается принудительно с ожиданием его остановки,
/// в том числе при заполненных каналах.
pub(super) fn run(tool: &str, args: &[&str], limit: u64) -> Result<Vec<u8>, AssetError> {
    use std::os::unix::process::ExitStatusExt;
    for required in ["prlimit", tool] {
        resolve_tool(required)?;
    }
    let mut child = Command::new("prlimit")
        .args(["--as=1073741824", "--cpu=12", "--nofile=64", "--", tool])
        .args(args)
        .env("OPENBLAS_NUM_THREADS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            infrastructure_failure(
                "audio_tool_start_failed",
                format!("не удалось запустить {tool}: требуются FFmpeg/ffprobe и prlimit в PATH"),
            )
        })?;
    let out = child
        .stdout
        .take()
        .expect("stdout дочернего процесса открыт как канал");
    let err = child
        .stderr
        .take()
        .expect("stderr дочернего процесса открыт как канал");
    // Сверх предела канал дочитывается вхолостую до конца: закрытый раньше
    // времени канал завершил бы процесс по SIGPIPE, и избыточный вывод
    // повреждённого файла выглядел бы сбоем среды, а не отказом декодирования.
    // Время чтения ограничено тем же 15-секундным пределом процесса.
    let stdout = thread::spawn(move || read_capped(out, limit + 1));
    let stderr = thread::spawn(move || read_capped(err, 16 * 1024 + 1));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() < Duration::from_secs(15) => {
                thread::sleep(Duration::from_millis(10))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(infrastructure_failure(
                    "audio_tool_runtime_failed",
                    if result.is_err() {
                        "ошибка ожидания декодера"
                    } else {
                        "декодер превысил 15 секунд"
                    },
                ));
            }
        }
    };
    let output = stdout
        .join()
        .map_err(|_| infrastructure_failure("audio_tool_runtime_failed", "сбой чтения stdout"))?
        .map_err(|e| infrastructure_failure("audio_tool_runtime_failed", e.to_string()))?;
    let errors = stderr
        .join()
        .map_err(|_| infrastructure_failure("audio_tool_runtime_failed", "сбой чтения stderr"))?
        .map_err(|e| infrastructure_failure("audio_tool_runtime_failed", e.to_string()))?;
    let status = status?;
    // prlimit использует 126/127, когда не удалось выполнить целевую программу;
    // ошибки декодирования самого ffmpeg остаются предметными отказами.
    if matches!(status.code(), Some(126 | 127)) {
        return Err(infrastructure_failure(
            "audio_tool_start_failed",
            format!("не удалось выполнить {tool} через prlimit"),
        ));
    }
    // Завершение сигналом (Ctrl+C группы процессов, SIGKILL по пределу CPU)
    // не говорит о содержимом файла: это сбой среды, а не отказ декодирования.
    if let Some(signal) = status.signal() {
        return Err(infrastructure_failure(
            "audio_tool_runtime_failed",
            format!("{tool} завершён сигналом {signal}"),
        ));
    }
    if !status.success() || !errors.is_empty() || output.len() as u64 > limit {
        // Не сохраняем stderr внешнего процесса: он может содержать пути/теги.
        return Err(failure(format!(
            "{tool}: ошибка декодирования, отсутствует инструмент или превышен предел вывода"
        )));
    }
    Ok(output)
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static FULL_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn probe_mp3(bytes: &[u8]) -> Result<AudioProbe, AssetError> {
    #[cfg(test)]
    FULL_PROBES.with(|count| count.set(count.get() + 1));
    probe_inner(bytes, false).map_err(|mut error| {
        error.details["schema_version"] = serde_json::json!(1);
        error.details["sha256"] = serde_json::json!(sha256_hex(bytes));
        error.details["byte_length"] = serde_json::json!(bytes.len());
        error.details["technical_only"] = serde_json::json!(true);
        error
    })
}

/// Диагностическая проверка для нормализатора: та же, что `probe_mp3`, но
/// расхождение кадровой и декодированной длительности допускается, если его
/// **точно до отсчёта** объясняют задержка и дополнение из тега LAME. Это не
/// техническое свидетельство хранилища и не даёт права на импорт.
pub(super) fn probe_mp3_lame_gapless(bytes: &[u8]) -> Result<AudioProbe, AssetError> {
    probe_inner(bytes, true)
}

/// Задержка и дополнение кодировщика из тега LAME в кадре Xing/Info.
fn lame_gapless(bytes: &[u8]) -> Option<(u64, u64)> {
    let start = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        10 + bytes[6..10]
            .iter()
            .fold(0usize, |n, b| (n << 7) | (*b & 0x7f) as usize)
    } else {
        0
    };
    let window = bytes.get(start..start.checked_add(2048)?.min(bytes.len()))?;
    let xing = window
        .windows(4)
        .position(|w| w == b"Xing" || w == b"Info")?;
    let tag = window[xing..].windows(4).position(|w| w == b"LAME")? + xing;
    let raw = window.get(tag + 21..tag + 24)?;
    let delay = (u64::from(raw[0]) << 4) | (u64::from(raw[1]) >> 4);
    let padding = ((u64::from(raw[1]) & 0x0f) << 8) | u64::from(raw[2]);
    Some((delay, padding))
}

pub(super) fn inspect_frames(bytes: &[u8]) -> Result<(u32, u32, u32, u64), AssetError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err(failure("MP3 должен содержать 1–8388608 байт"));
    }
    frames(bytes)
}

fn probe_inner(bytes: &[u8], allow_lame_gapless: bool) -> Result<AudioProbe, AssetError> {
    let (sample_rate, channels, frame_count, encoded_samples) = inspect_frames(bytes)?;
    let workspace = TempWorkspace::create("pronunciation-probe")
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let path = workspace.path().join("input.mp3");
    std::fs::write(&path, bytes)
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let name = path
        .to_str()
        .ok_or_else(|| failure("временный путь должен быть UTF-8"))?;
    let metadata = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-threads",
            "1",
            "-protocol_whitelist",
            "file",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_name,sample_rate,channels:format=duration,format_name",
            "-of",
            "json",
            name,
        ],
        64 * 1024,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&metadata)
        .map_err(|_| failure("ffprobe вернул некорректный JSON"))?;
    let streams = value["streams"]
        .as_array()
        .ok_or_else(|| failure("нет массива streams"))?;
    if streams.len() != 1
        || streams[0]["codec_name"] != "mp3"
        || value["format"]["format_name"] != "mp3"
        || streams[0]["sample_rate"]
            .as_str()
            .and_then(|v| v.parse::<u32>().ok())
            != Some(sample_rate)
        || streams[0]["channels"].as_u64() != Some(channels as u64)
    {
        return Err(failure("ожидается один звуковой поток MP3"));
    }
    let declared = value["format"]["duration"]
        .as_str()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| failure("длительность не определена"))?;
    let pcm = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
            "-xerror",
            "-err_detect",
            "explode",
            "-threads",
            "1",
            "-protocol_whitelist",
            "file",
            "-i",
            name,
            "-map",
            "0:a:0",
            "-threads",
            "1",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "pipe:1",
        ],
        6 * 1024 * 1024,
    )?;
    let stride = channels as usize * 2;
    if pcm.is_empty() || pcm.len() % stride != 0 {
        return Err(failure("декодер не выдал полный поток PCM"));
    }
    let samples = (pcm.len() / stride) as u64;
    let decoded = samples as f64 / sample_rate as f64;
    // Кадр Xing/Info и задержка/дополнение кодировщика могут убрать до трёх кадров.
    // Допуск абсолютный, а не процентный: длинная усечённая запись не проходит.
    let tolerance = 3.0 * 1152.0 / sample_rate as f64 + 0.002;
    let gapless_explained = allow_lame_gapless
        && lame_gapless(bytes).is_some_and(|(delay, padding)| {
            let per_frame = encoded_samples / u64::from(frame_count);
            encoded_samples.checked_sub(per_frame + delay + padding) == Some(samples)
        });
    if decoded > MAX_DURATION_SECONDS
        || (declared - decoded).abs() > tolerance
        || ((encoded_samples as f64 / sample_rate as f64 - decoded).abs() > tolerance
            && !gapless_explained)
    {
        return Err(AssetError::with_details(
            ErrorCode::ValidatorFailure,
            "заявленная/кадровая и декодированная длительности расходятся",
            serde_json::json!({"check":"duration_consistency"}),
        ));
    }
    Ok(AudioProbe {
        schema_version: 1,
        codec: "mp3".into(),
        sha256: sha256_hex(bytes),
        byte_length: bytes.len() as u64,
        sample_rate,
        channels,
        frames: frame_count,
        declared_duration_seconds: declared,
        decoded_duration_seconds: decoded,
        decoded_samples: samples,
    })
}

/// Повторно декодирует уже проверенный `probe_mp3` поток в отсчёты `f32` с
/// чередованием каналов той же частоты и раскладки. `f32` сохраняет выбросы декодера выше полной
/// шкалы, которые `s16` скрыл бы насыщением.
pub(super) fn decode_f32(bytes: &[u8], probe: &AudioProbe) -> Result<Vec<f32>, AssetError> {
    let workspace = TempWorkspace::create("pronunciation-decode")
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let path = workspace.path().join("input.mp3");
    std::fs::write(&path, bytes)
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let name = path
        .to_str()
        .ok_or_else(|| failure("временный путь должен быть UTF-8"))?;
    let pcm = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-filter_threads",
            "1",
            "-xerror",
            "-err_detect",
            "explode",
            "-threads",
            "1",
            "-protocol_whitelist",
            "file",
            "-i",
            name,
            "-map",
            "0:a:0",
            "-threads",
            "1",
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "pipe:1",
        ],
        MAX_F32_PCM_BYTES,
    )?;
    let stride = probe.channels as usize * 4;
    if pcm.is_empty() || pcm.len() % stride != 0 {
        return Err(failure("декодер не выдал полный поток PCM f32"));
    }
    let samples: Vec<f32> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(failure("декодер выдал неконечные отсчёты"));
    }
    if (samples.len() / probe.channels as usize) as u64 != probe.decoded_samples {
        return Err(failure(
            "повторное декодирование расходится с технической проверкой",
        ));
    }
    Ok(samples)
}

/// Предел PCM f32: 30 с, 48 kHz, стерео, с запасом на дополнение кодировщика.
pub(super) const MAX_F32_PCM_BYTES: u64 = 12 * 1024 * 1024;

/// Один контролируемый проход libmp3lame без тегов и метаданных источника.
/// `-bitexact` убирает версию сборки из потока, поэтому одинаковые входы
/// той же сборки FFmpeg дают одинаковые байты.
pub(super) fn encode_mp3(
    samples: &[f32],
    input_rate: u32,
    channels: u32,
    output_rate: u32,
    encoder: &[String],
) -> Result<Vec<u8>, AssetError> {
    encode_mp3_in("", samples, input_rate, channels, output_rate, encoder)
}

/// `stem` различает назначение файлов (`""` — вариант, `"toolchain-"` —
/// отпечаток кодировщика), чтобы диагностика и тестовые обёртки отличали их.
fn encode_mp3_in(
    stem: &str,
    samples: &[f32],
    input_rate: u32,
    channels: u32,
    output_rate: u32,
    encoder: &[String],
) -> Result<Vec<u8>, AssetError> {
    let workspace = TempWorkspace::create("pronunciation-encode")
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let input = workspace.path().join(format!("{stem}input.f32"));
    let output = workspace.path().join(format!("{stem}output.mp3"));
    let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&input, raw)
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let (Some(input_name), Some(output_name)) = (input.to_str(), output.to_str()) else {
        return Err(failure("временный путь должен быть UTF-8"));
    };
    let (input_rate, channels, output_rate) = (
        input_rate.to_string(),
        channels.to_string(),
        output_rate.to_string(),
    );
    let mut args: Vec<&str> = vec![
        "-v",
        "error",
        "-nostdin",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-protocol_whitelist",
        "file",
        "-f",
        "f32le",
        "-ar",
        &input_rate,
        "-ac",
        &channels,
        "-i",
        input_name,
        "-map_metadata",
        "-1",
        "-ar",
        &output_rate,
        "-c:a",
        "libmp3lame",
    ];
    args.extend(encoder.iter().map(String::as_str));
    args.extend([
        "-id3v2_version",
        "0",
        "-write_id3v1",
        "0",
        "-write_xing",
        "1",
        "-fflags",
        "+bitexact",
        "-flags:a",
        "+bitexact",
        "-f",
        "mp3",
        output_name,
    ]);
    let stdout = run("ffmpeg", &args, 1024)?;
    if !stdout.is_empty() {
        return Err(failure("кодировщик неожиданно писал в stdout"));
    }
    let bytes = std::fs::read(&output)
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err(failure("кодировщик выдал пустой или слишком большой MP3"));
    }
    Ok(bytes)
}

/// Пересэмплирует подготовленный PCM `f32` тем же `libswresample`, которым
/// пользуется кодировщик, без иной обработки. Результат — точный эталон для
/// пост-QC при фиксированной частоте вывода: кодировщик получает уже эти
/// отсчёты, и сравнение «декодированный вывод ↔ эталон» идёт на одной частоте.
pub(super) fn resample_f32(
    samples: &[f32],
    input_rate: u32,
    channels: u32,
    output_rate: u32,
) -> Result<Vec<f32>, AssetError> {
    let workspace = TempWorkspace::create("pronunciation-resample")
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let input = workspace.path().join("input.f32");
    let raw: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&input, raw)
        .map_err(|e| infrastructure_failure("audio_temp_io_failure", e.to_string()))?;
    let name = input
        .to_str()
        .ok_or_else(|| failure("временный путь должен быть UTF-8"))?;
    let (input_rate, channels_text, output_rate) = (
        input_rate.to_string(),
        channels.to_string(),
        output_rate.to_string(),
    );
    let pcm = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-protocol_whitelist",
            "file",
            "-f",
            "f32le",
            "-ar",
            &input_rate,
            "-ac",
            &channels_text,
            "-i",
            name,
            "-ar",
            &output_rate,
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "pipe:1",
        ],
        MAX_F32_PCM_BYTES,
    )?;
    let stride = channels as usize * 4;
    if pcm.is_empty() || pcm.len() % stride != 0 {
        return Err(failure("пересэмплирование не выдало полный поток PCM f32"));
    }
    let samples: Vec<f32> = pcm
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(failure("пересэмплирование выдало неконечные отсчёты"));
    }
    Ok(samples)
}

/// Название инструмента и версии библиотек из `<tool> -version`.
fn tool_identity(tool: &str) -> Result<String, AssetError> {
    let output = run(tool, &["-hide_banner", "-version"], 64 * 1024)?;
    let text = String::from_utf8_lossy(&output);
    let mut parts = Vec::new();
    let banner = format!("{tool} version ");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&banner) {
            parts.push(format!(
                "{tool} {}",
                rest.split_whitespace().next().unwrap_or("unknown")
            ));
        } else if [
            "libavutil",
            "libavcodec",
            "libavformat",
            "libavfilter",
            "libswresample",
        ]
        .iter()
        .any(|library| line.starts_with(library))
        {
            parts.push(
                line.split('/')
                    .next()
                    .unwrap_or(line)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
    if !parts.first().is_some_and(|p| p.starts_with(tool))
        || parts.iter().any(|p| p.chars().any(char::is_control))
    {
        return Err(infrastructure_failure(
            "audio_tool_runtime_failed",
            format!("не удалось определить версию {tool}"),
        ));
    }
    Ok(parts.join("; "))
}

/// Версии FFmpeg и его библиотек из `ffmpeg -version`: входят в отпечаток
/// конфигурации, потому что другой сборки достаточно для иных байтов.
pub(super) fn ffmpeg_identity() -> Result<String, AssetError> {
    tool_identity("ffmpeg")
}

/// Версии `ffprobe` и его библиотек: техническая проверка `probe_mp3` входа
/// и результата идёт через него, поэтому он тоже определяет результат.
pub(super) fn ffprobe_identity() -> Result<String, AssetError> {
    tool_identity("ffprobe")
}

/// Функциональный отпечаток MP3-кодировщика. `ffmpeg -version` не сообщает
/// версию связанной libmp3lame, поэтому фиксированный детерминированный сигнал
/// кодируется тем же путём, что и варианты (`-bitexact`), и в отпечаток входит
/// SHA-256 полученных байтов. Иная сборка LAME, меняющая байты, меняет отпечаток.
pub(super) fn encoder_identity() -> Result<String, AssetError> {
    const RATE: u32 = 44100;
    let samples: Vec<f32> = (0..RATE as usize / 4)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            (0.25 * (2.0 * std::f64::consts::PI * (220.0 + 1800.0 * t) * t).sin()) as f32
        })
        .collect();
    let bytes = encode_mp3_in(
        "toolchain-",
        &samples,
        RATE,
        1,
        RATE,
        &["-q:a".into(), "2".into()],
    )
    .map_err(|error| {
        if error.details["blocker"].is_string() {
            error
        } else {
            infrastructure_failure(
                "audio_tool_runtime_failed",
                format!("кодировщик libmp3lame недоступен: {}", error.message),
            )
        }
    })?;
    Ok(format!("libmp3lame sha256:{}", &sha256_hex(&bytes)[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Вывод сверх предела дочитывается: процесс не получает SIGPIPE, и
    /// избыточный stderr остаётся предметным отказом, а не сбоем среды.
    #[test]
    fn excess_stderr_is_drained_not_turned_into_sigpipe() {
        if run("true", &[], 0).is_err() {
            return; // нет prlimit: проверка невозможна в этом окружении
        }
        let script = "i=0; while [ $i -lt 400 ]; do printf '%0100d\\n' 0 >&2; i=$((i+1)); done";
        let error = run("sh", &["-c", script], 1024).unwrap_err();
        assert!(error.details["blocker"].is_null(), "{error:?}");
        assert!(!super::super::normalizer::is_infrastructure_failure(&error));
    }
}
