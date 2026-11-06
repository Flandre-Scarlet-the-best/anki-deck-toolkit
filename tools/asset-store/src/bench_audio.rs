//! Нагрузка сценария `asset-store.audio-normalizer-batch`: детерминированный
//! синтетический корпус MP3, маркированный рабочий каталог нормализатора и
//! постусловия прогона. Измеряемая операция — штатный `pronunciation-assets
//! normalizer batch-start`; здесь только подготовка и проверка вне интервала.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use rustix::fs::{AtFlags, Dir, FlockOperation, Mode, OFlags, flock, openat, unlinkat};
use rustix::io::Errno;

use serde::{Deserialize, Serialize};

use crate::hashing::sha256_hex;
use crate::pronunciation::normalizer_batch as nbatch;

pub const CORPUS_MARKER: &str = ".asset-store-bench-audio.json";
pub const WORK_MARKER: &str = ".asset-store-bench-audio-work.json";
/// `2`: уровни приведены к заявленному пиковому диапазону −44…−8 dBFS
/// (у первой версии фактически было около −36…0 dBFS).
const GENERATOR_VERSION: u32 = 2;
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

/// Создаёт новый каталог с `count` MP3: тоны с пиковым уровнем от −44 до
/// −8 dBFS и разной длительности, краевая тишина 0–1.2 с, иногда внутренняя
/// пауза, моно/стерео, 22.05–48 кГц.
pub fn generate_corpus(
    out: &Path,
    count: usize,
    seed: u64,
) -> Result<serde_json::Value, AudioBenchError> {
    if count == 0 || count > 20_000 {
        return Err(fail("--count должен быть 1–20000"));
    }
    // Родитель создаётся заранее: новым обязан быть только сам каталог корпуса.
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| fail(format!("родитель каталога корпуса: {e}")))?;
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
        // Пиковый уровень тона до кодирования — от −44 до −8 dBFS с шагом 1 dB.
        // Фильтр FFmpeg `sine` выдаёт амплитуду 1/8 полной шкалы, поэтому
        // `volume` = требуемая амплитуда / (1/8).
        let peak_dbfs = -44.0 + rng.range(0, 36) as f64;
        let volume = 8.0 * 10f64.powf(peak_dbfs / 20.0);
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

fn io_fail(context: &str, error: impl std::fmt::Display) -> AudioBenchError {
    fail(format!("{context}: {error}"))
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Подкаталог без перехода по ссылкам: `None`, если его нет; ссылка или
/// не-каталог — отказ.
fn child_dir(parent: &File, name: &str) -> Result<Option<File>, AudioBenchError> {
    match openat(parent, name, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(fail(format!(
            "{name} в рабочем каталоге — не каталог или символическая ссылка; удалять через неё нельзя"
        ))),
        Err(error) => Err(io_fail(name, error)),
    }
}

/// Рекурсивное удаление относительно дескриптора родителя. Ссылки не
/// разыменовываются (удаляется сама ссылка), каждый каталог открывается с
/// `NOFOLLOW`: содержимое вне рабочего каталога недостижимо.
fn remove_tree_at(parent: &File, name: &OsStr) -> Result<(), AudioBenchError> {
    match openat(parent, name, dir_flags(), Mode::empty()) {
        Ok(fd) => {
            let directory = File::from(fd);
            let mut names = Vec::new();
            for entry in Dir::read_from(&directory).map_err(|e| io_fail("обход", e))? {
                let entry = entry.map_err(|e| io_fail("обход", e))?;
                let child = entry.file_name().to_bytes();
                if child != b"." && child != b".." {
                    names.push(OsStr::from_bytes(child).to_owned());
                }
            }
            for child in names {
                remove_tree_at(&directory, &child)?;
            }
            unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| io_fail("удаление", e))
        }
        Err(Errno::NOTDIR | Errno::LOOP) => {
            unlinkat(parent, name, AtFlags::empty()).map_err(|e| io_fail("удаление", e))
        }
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(io_fail("удаление", error)),
    }
}

fn owned(workdir: &File) -> Result<bool, AudioBenchError> {
    let file = match openat(
        workdir,
        WORK_MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(false),
        Err(error) => return Err(io_fail("маркер", error)),
    };
    if !file.metadata().map_err(|e| io_fail("маркер", e))?.is_file() {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| io_fail("маркер", e))?;
    let marker: Marker =
        serde_json::from_slice(&bytes).map_err(|e| fail(format!("маркер повреждён: {e}")))?;
    Ok(marker.tool == MARKER_TOOL && marker.kind == "audio-work")
}

/// Исключительные блокировки, которые держит работающий нормализатор:
/// кэш результатов (на всё время работы исполнителя), состояние и lease
/// исполнителя каждого пакета. Захват без ожидания: занятая блокировка —
/// отказ, а не ожидание, поэтому взаимной блокировки нет. Возвращённые
/// дескрипторы удерживают блокировки до конца сброса.
fn lock_idle_runtime(workdir: &File) -> Result<HeldLocks, AudioBenchError> {
    let busy =
        || fail("рабочий каталог используется активным пакетом нормализатора; сброс отклонён");
    let mut held = HeldLocks(Vec::new());
    let Some(runtime) = child_dir(workdir, ".runtime")? else {
        return Ok(held);
    };
    let Some(batches) = child_dir(&runtime, "batches")? else {
        return Ok(held);
    };
    let mut names = Vec::new();
    for entry in Dir::read_from(&batches).map_err(|e| io_fail("обход", e))? {
        let entry = entry.map_err(|e| io_fail("обход", e))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    names.sort();
    // Кэш первым: исполнитель держит его дольше всего.
    names.sort_by_key(|name| name != nbatch::CACHE_ID);
    for name in names {
        let Some(directory) = child_dir(&batches, &name)? else {
            continue;
        };
        flock(&directory, FlockOperation::NonBlockingLockExclusive).map_err(|_| busy())?;
        held.0.push(directory);
        let directory = held.0.last().expect("только что добавлен");
        if let Some(execution) = child_dir(directory, "execution")? {
            flock(&execution, FlockOperation::NonBlockingLockExclusive).map_err(|_| busy())?;
            held.0.push(execution);
        }
    }
    held.0.push(batches);
    held.0.push(runtime);
    Ok(held)
}

/// Блокировки сброса снимаются явно, в том числе при частичном захвате:
/// копия дескриптора в процессе, запущенном через `fork` до `exec`, иначе
/// удерживала бы их и после закрытия наших дескрипторов. Снятие блокировки,
/// которую этот дескриптор не держит (`batches`, `runtime`), безвредно.
struct HeldLocks(Vec<File>);

impl Drop for HeldLocks {
    fn drop(&mut self) {
        for file in self.0.iter().rev() {
            let _ = flock(file, FlockOperation::Unlock);
        }
    }
}

/// Приводит рабочий каталог к нужному состоянию. Чужой непустой каталог не трогается.
/// `keep_cache` сохраняет кэш результатов (тёплый повторный запуск), `remove` удаляет всё;
/// флаги несовместимы. Удаление идёт по дескрипторам без перехода по ссылкам и
/// отклоняется, пока каталог использует работающий пакет.
pub fn reset_workspace(
    workdir: &Path,
    keep_cache: bool,
    remove: bool,
) -> Result<serde_json::Value, AudioBenchError> {
    if keep_cache && remove {
        return Err(fail(
            "--keep-cache и --remove несовместимы: удаление каталога уничтожило бы сохраняемый кэш",
        ));
    }
    let absolute = std::path::absolute(workdir).map_err(|e| io_fail("рабочий путь", e))?;
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err(fail("рабочий путь должен иметь родительский каталог"));
    };
    if !remove {
        fs::create_dir_all(parent).map_err(|e| io_fail("родитель рабочего каталога", e))?;
    }
    let parent_fd = match rustix::fs::open(parent, dir_flags(), Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) if remove => return Ok(serde_json::json!({"removed": true})),
        Err(error) => return Err(io_fail("родитель рабочего каталога", error)),
    };
    let existing = match openat(&parent_fd, name, dir_flags(), Mode::empty()) {
        Ok(fd) => Some(File::from(fd)),
        Err(Errno::NOENT) => None,
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(fail("рабочий путь не является каталогом"));
        }
        Err(error) => return Err(io_fail("рабочий каталог", error)),
    };
    let mut _locks = HeldLocks(Vec::new());
    if let Some(directory) = &existing {
        // Ошибка чтения не считается пустотой: иначе чужой каталог прошёл бы
        // без проверки маркера и был бы очищен.
        let mut empty = true;
        for entry in Dir::read_from(directory).map_err(|e| io_fail("рабочий каталог", e))?
        {
            let entry = entry.map_err(|e| io_fail("рабочий каталог", e))?;
            if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
                empty = false;
                break;
            }
        }
        if !empty && !owned(directory)? {
            return Err(fail(
                "рабочий каталог не пуст и не помечен asset-store-bench; удалять чужие данные нельзя",
            ));
        }
        _locks = lock_idle_runtime(directory)?;
    }
    if remove {
        if existing.is_some() {
            remove_tree_at(&parent_fd, name)?;
        }
        return Ok(serde_json::json!({"removed": true}));
    }
    if let Some(directory) = &existing {
        if keep_cache {
            if let Some(runtime) = child_dir(directory, ".runtime")?
                && let Some(batches) = child_dir(&runtime, "batches")?
            {
                let mut names = Vec::new();
                for entry in Dir::read_from(&batches).map_err(|e| io_fail("обход", e))? {
                    let entry = entry.map_err(|e| io_fail("обход", e))?;
                    let child = entry.file_name().to_bytes();
                    if child != b"." && child != b".." && child != nbatch::CACHE_ID.as_bytes() {
                        names.push(OsStr::from_bytes(child).to_owned());
                    }
                }
                for child in names {
                    remove_tree_at(&batches, &child)?;
                }
            }
        } else {
            let mut names = Vec::new();
            for entry in Dir::read_from(directory).map_err(|e| io_fail("обход", e))? {
                let entry = entry.map_err(|e| io_fail("обход", e))?;
                let child = entry.file_name().to_bytes();
                if child != b"." && child != b".." {
                    names.push(OsStr::from_bytes(child).to_owned());
                }
            }
            for child in names {
                remove_tree_at(directory, &child)?;
            }
        }
    }
    drop(_locks);
    let directory = match existing {
        Some(directory) => directory,
        None => {
            rustix::fs::mkdirat(&parent_fd, name, Mode::from_raw_mode(0o755))
                .map_err(|e| io_fail("рабочий каталог", e))?;
            File::from(
                openat(&parent_fd, name, dir_flags(), Mode::empty())
                    .map_err(|e| io_fail("рабочий каталог", e))?,
            )
        }
    };
    let marker = Marker {
        tool: MARKER_TOOL.into(),
        kind: "audio-work".into(),
        generator: GENERATOR_VERSION,
        seed: None,
        count: None,
    };
    let mut file = File::from(
        openat(
            &directory,
            WORK_MARKER,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(|e| io_fail("маркер", e))?,
    );
    file.write_all(&serde_json::to_vec_pretty(&marker).expect("маркер"))
        .map_err(|e| io_fail("маркер", e))?;
    Ok(serde_json::json!({"reset": true, "kept_cache": keep_cache}))
}

/// Постусловие прогона: пакет завершён полностью, вход не изменён.
pub fn verify(workdir: &Path, batch_id: &str) -> Result<serde_json::Value, AudioBenchError> {
    let (batch, inventory) =
        nbatch::load(workdir, batch_id).map_err(|e| fail(format!("пакет не читается: {e}")))?;
    // Пакет без входов ничего не обработал: такое измерение не годно.
    if batch.progress.unique_inputs == 0
        || batch.progress.completed != batch.progress.unique_inputs
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
    if !check.verified {
        return Err(fail(format!(
            "неизменность входа не доказана: {}",
            serde_json::json!(check)
        )));
    }
    Ok(serde_json::json!({
        "completed": batch.progress.completed,
        "by_outcome": batch.progress.by_outcome,
        "inventory_sha256": batch.inventory_sha256,
        "sources_unchanged": check.unchanged,
        "config_sha256": sha256_hex(batch.config_sha256.as_bytes())[..16].to_owned(),
    }))
}
