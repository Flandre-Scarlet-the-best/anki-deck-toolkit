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

/// prlimit задаёт CPU/адресное пространство до exec без unsafe/pre_exec.
/// stdout/stderr читаются одновременно с жёсткими пределами. При превышении времени
/// дочерний процесс завершается принудительно с ожиданием его остановки,
/// в том числе при заполненных каналах.
fn run(tool: &str, args: &[&str], limit: u64) -> Result<Vec<u8>, AssetError> {
    use std::os::unix::fs::PermissionsExt;
    for required in ["prlimit", tool] {
        let available = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|path| {
                std::fs::metadata(path.join(required))
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
        });
        if !available {
            return Err(AssetError::with_details(
                ErrorCode::ValidatorFailure,
                format!("в PATH отсутствует обязательный инструмент {required}"),
                serde_json::json!({"blocker":"audio_tool_missing","tool":required}),
            ));
        }
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
    let out = child.stdout.take().expect("pipe");
    let err = child.stderr.take().expect("pipe");
    let stdout = thread::spawn(move || {
        let mut data = Vec::new();
        out.take(limit + 1).read_to_end(&mut data).map(|_| data)
    });
    let stderr = thread::spawn(move || {
        let mut data = Vec::new();
        err.take(16 * 1024 + 1).read_to_end(&mut data).map(|_| data)
    });
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
    probe_inner(bytes).map_err(|mut error| {
        error.details["schema_version"] = serde_json::json!(1);
        error.details["sha256"] = serde_json::json!(sha256_hex(bytes));
        error.details["byte_length"] = serde_json::json!(bytes.len());
        error.details["technical_only"] = serde_json::json!(true);
        error
    })
}

pub(super) fn inspect_frames(bytes: &[u8]) -> Result<(u32, u32, u32, u64), AssetError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err(failure("MP3 должен содержать 1–8388608 байт"));
    }
    frames(bytes)
}

fn probe_inner(bytes: &[u8]) -> Result<AudioProbe, AssetError> {
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
    if decoded > MAX_DURATION_SECONDS
        || (declared - decoded).abs() > tolerance
        || (encoded_samples as f64 / sample_rate as f64 - decoded).abs() > tolerance
    {
        return Err(failure(
            "заявленная/кадровая и декодированная длительности расходятся",
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
