//! Нагрузка сценария `asset-store.audio-normalizer-batch`: детерминированный
//! синтетический корпус MP3, маркированный рабочий каталог нормализатора и
//! постусловия прогона. Измеряемая операция — штатный `pronunciation-assets
//! normalizer batch-start`; здесь только подготовка и проверка вне интервала.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::hashing::sha256_hex;
use crate::pronunciation::normalizer_batch as nbatch;

pub const CORPUS_MARKER: &str = ".asset-store-bench-audio.json";
pub const WORK_MARKER: &str = ".asset-store-bench-audio-work.json";
const GENERATOR_VERSION: u32 = 1;
const MARKER_TOOL: &str = "asset-store-bench";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AudioBenchError(pub String);

fn fail(message: impl Into<String>) -> AudioBenchError {
    AudioBenchError(message.into())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    tool: String,
    kind: String,
    generator: u32,
    seed: Option<u64>,
    count: Option<usize>,
}

/// Детерминированный генератор параметров (splitmix64).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

/// Создаёт новый каталог с `count` MP3: тоны разной громкости и длительности,
/// краевая тишина 0–1.2 с, иногда внутренняя пауза, моно/стерео, 22.05–48 кГц.
pub fn generate_corpus(
    out: &Path,
    count: usize,
    seed: u64,
) -> Result<serde_json::Value, AudioBenchError> {
    if count == 0 || count > 20_000 {
        return Err(fail("--count должен быть 1–20000"));
    }
    fs::create_dir(out).map_err(|e| fail(format!("каталог корпуса должен быть новым: {e}")))?;
    let mut rng = Rng(seed);
    let mut bytes = 0u64;
    for index in 0..count {
        let rate = [22050, 44100, 44100, 48000][rng.range(0, 3) as usize];
        let channels = if rng.range(0, 2) == 0 { 2 } else { 1 };
        let lead = rng.range(0, 1200);
        let tail = rng.range(0, 1200);
        let speech = rng.range(150, 2500);
        let pause = if rng.range(0, 3) == 0 {
            rng.range(80, 500)
        } else {
            0
        };
        let frequency = rng.range(150, 900);
        // Амплитуда источника sine — 1/8; volume задаёт итог от −44 до −8 dBFS.
        let volume = 10f64.powf(rng.range(0, 36) as f64 / 20.0) / 8.0;
        let first = speech / 2;
        let filter = format!(
            "sine=frequency={frequency}:duration={a}:sample_rate={rate},volume={volume:.5},apad=pad_dur={p}[a];\
             sine=frequency={f2}:duration={b}:sample_rate={rate},volume={volume:.5}[b];\
             [a][b]concat=n=2:v=0:a=1,adelay={lead}:all=1,apad=pad_dur={t}",
            a = first as f64 / 1000.0,
            b = (speech - first) as f64 / 1000.0,
            p = pause as f64 / 1000.0,
            f2 = frequency + 40,
            t = tail as f64 / 1000.0,
        );
        let path = out.join(format!("clip-{index:05}.mp3"));
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin"])
            .args(["-filter_complex", &filter, "-ac", &channels.to_string()])
            .args(["-c:a", "libmp3lame", "-q:a", "4", "-fflags", "+bitexact"])
            .arg(&path)
            .output()
            .map_err(|e| fail(format!("FFmpeg не запускается: {e}")))?;
        if !output.status.success() {
            return Err(fail(format!(
                "FFmpeg не создал клип: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        bytes += fs::metadata(&path).map_err(|e| fail(e.to_string()))?.len();
    }
    let marker = Marker {
        tool: MARKER_TOOL.into(),
        kind: "audio-corpus".into(),
        generator: GENERATOR_VERSION,
        seed: Some(seed),
        count: Some(count),
    };
    fs::write(
        out.join(CORPUS_MARKER),
        serde_json::to_vec_pretty(&marker).expect("маркер"),
    )
    .map_err(|e| fail(e.to_string()))?;
    Ok(
        serde_json::json!({"files": count, "bytes": bytes, "seed": seed, "generator": GENERATOR_VERSION}),
    )
}

fn owned(workdir: &Path) -> Result<bool, AudioBenchError> {
    match fs::read(workdir.join(WORK_MARKER)) {
        Ok(bytes) => {
            let marker: Marker = serde_json::from_slice(&bytes)
                .map_err(|e| fail(format!("маркер повреждён: {e}")))?;
            Ok(marker.tool == MARKER_TOOL && marker.kind == "audio-work")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(fail(e.to_string())),
    }
}

/// Приводит рабочий каталог к нужному состоянию. Чужой непустой каталог не трогается.
/// `keep_cache` сохраняет кэш результатов (тёплый повторный запуск), `remove` удаляет всё.
pub fn reset_workspace(
    workdir: &Path,
    keep_cache: bool,
    remove: bool,
) -> Result<serde_json::Value, AudioBenchError> {
    match fs::symlink_metadata(workdir) {
        Ok(meta) if !meta.is_dir() => return Err(fail("рабочий путь не является каталогом")),
        Ok(_) => {
            let empty = fs::read_dir(workdir)
                .map_err(|e| fail(e.to_string()))?
                .next()
                .is_none();
            if !empty && !owned(workdir)? {
                return Err(fail(
                    "рабочий каталог не пуст и не помечен asset-store-bench; удалять чужие данные нельзя",
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fail(e.to_string())),
    }
    if remove {
        if workdir.exists() {
            fs::remove_dir_all(workdir).map_err(|e| fail(e.to_string()))?;
        }
        return Ok(serde_json::json!({"removed": true}));
    }
    if workdir.exists() {
        let batches = workdir.join(".runtime/batches");
        if keep_cache {
            if batches.exists() {
                for entry in fs::read_dir(&batches).map_err(|e| fail(e.to_string()))? {
                    let entry = entry.map_err(|e| fail(e.to_string()))?;
                    if entry.file_name() != nbatch::CACHE_ID {
                        fs::remove_dir_all(entry.path()).map_err(|e| fail(e.to_string()))?;
                    }
                }
            }
        } else {
            fs::remove_dir_all(workdir).map_err(|e| fail(e.to_string()))?;
        }
    }
    fs::create_dir_all(workdir).map_err(|e| fail(e.to_string()))?;
    let marker = Marker {
        tool: MARKER_TOOL.into(),
        kind: "audio-work".into(),
        generator: GENERATOR_VERSION,
        seed: None,
        count: None,
    };
    fs::write(
        workdir.join(WORK_MARKER),
        serde_json::to_vec_pretty(&marker).expect("маркер"),
    )
    .map_err(|e| fail(e.to_string()))?;
    Ok(serde_json::json!({"reset": true, "kept_cache": keep_cache}))
}

/// Постусловие прогона: пакет завершён полностью, вход не изменён.
pub fn verify(workdir: &Path, batch_id: &str) -> Result<serde_json::Value, AudioBenchError> {
    let (batch, inventory) =
        nbatch::load(workdir, batch_id).map_err(|e| fail(format!("пакет не читается: {e}")))?;
    if batch.progress.completed != batch.progress.unique_inputs
        || !batch.progress.source_errors.is_empty()
    {
        return Err(fail(format!(
            "пакет не завершён: {}/{} входов, ошибок источника {}",
            batch.progress.completed,
            batch.progress.unique_inputs,
            batch.progress.source_errors.len()
        )));
    }
    let check = nbatch::verify_sources(&batch, &inventory).map_err(|e| fail(e.to_string()))?;
    if !check.changed.is_empty() || !check.unreadable.is_empty() {
        return Err(fail("вход изменился во время прогона"));
    }
    Ok(serde_json::json!({
        "completed": batch.progress.completed,
        "by_outcome": batch.progress.by_outcome,
        "inventory_sha256": batch.inventory_sha256,
        "sources_unchanged": check.unchanged,
        "config_sha256": sha256_hex(batch.config_sha256.as_bytes())[..16].to_owned(),
    }))
}
