//! Идентичность и установка весов DeepFilterNet3.
//!
//! Веса не встраиваются в обработку и не скачиваются ею: их кладёт в
//! локальный каталог отдельная явная операция [`install`], а каждый запуск
//! обработки заново сверяет файлы с закреплённым манифестом. Манифест — точные
//! длины и SHA-256 четырёх файлов архива `models/DeepFilterNet3_onnx.tar.gz`
//! upstream-репозитория на закреплённом коммите. Файлы другой версии
//! (например, DeepFilterNet2) или повреждённые дают диагностируемый отказ,
//! а не подмену модели.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MODEL_NAME: &str = "DeepFilterNet3";
/// Значение `[train] model` в `config.ini` модели третьей версии.
pub const MODEL_VARIANT: &str = "deepfilternet3";
pub const SAMPLE_RATE: u32 = 48_000;
pub const UPSTREAM_REPOSITORY: &str = "https://github.com/Rikorose/DeepFilterNet";
pub const UPSTREAM_COMMIT: &str = "d375b2d8309e0935d165700c91da9de862a99c31";
pub const UPSTREAM_ARCHIVE: &str = "models/DeepFilterNet3_onnx.tar.gz";
pub const UPSTREAM_ARCHIVE_SHA256: &str =
    "c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616";
/// Лицензия кода и весов upstream (README репозитория DeepFilterNet).
pub const LICENSE: &str = "MIT OR Apache-2.0";
/// Предел чтения одного файла модели: защищает от подставленного огромного файла.
const MAX_MODEL_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct PinnedFile {
    pub name: &'static str,
    pub byte_length: u64,
    pub sha256: &'static str,
}

/// Закреплённые файлы DeepFilterNet3 (совпадают с содержимым upstream-архива
/// и с каталогом `models/DeepFilterNet3` crate `deep-filter-rt` 0.1.1).
pub const FILES: [PinnedFile; 4] = [
    PinnedFile {
        name: "config.ini",
        byte_length: 2067,
        sha256: "415eb925d44990d938fb739f514aa3662c1ec0ea836cff044fa1291b82cb4290",
    },
    PinnedFile {
        name: "enc.onnx",
        byte_length: 1_954_042,
        sha256: "7c5399d3da8a50ebef1c1a0ae421b33376aa5e45d0e92df16da7e83c9c131916",
    },
    PinnedFile {
        name: "erb_dec.onnx",
        byte_length: 3_292_397,
        sha256: "ab669a1d10afe20911728b33053a452071042317a90581092b325da7b2f9d895",
    },
    PinnedFile {
        name: "df_dec.onnx",
        byte_length: 3_340_803,
        sha256: "23114ce3b0f6464b763ee62f7bb8aab6b2a129a21eabd5bcfe59413db05f278a",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    /// Каталога или файла модели нет.
    Missing,
    /// Файл модели — ссылка, каталог или иной не обычный файл.
    NotRegularFile,
    /// `config.ini` описывает другую модель (например, DeepFilterNet2) или
    /// другую частоту.
    WrongVersion,
    /// Длина или SHA-256 не совпали с закреплённым манифестом.
    Corrupt,
    /// Ошибка ввода-вывода или формата источника установки.
    Io,
}

impl ModelErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Missing => "model_missing",
            Self::NotRegularFile => "model_not_regular_file",
            Self::WrongVersion => "model_wrong_version",
            Self::Corrupt => "model_corrupt",
            Self::Io => "model_io_failure",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for ModelError {}

fn error(kind: ModelErrorKind, message: impl Into<String>) -> ModelError {
    ModelError {
        kind,
        message: message.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub name: String,
    pub byte_length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSource {
    pub repository: String,
    pub commit: String,
    pub archive: String,
    pub archive_sha256: String,
}

/// Проверенная идентичность весов: имя и вариант модели, источник,
/// отпечаток манифеста и каждый файл.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub name: String,
    pub variant: String,
    pub sample_rate: u32,
    pub manifest_sha256: String,
    pub files: Vec<FileIdentity>,
    pub source: ModelSource,
    pub license: String,
}

/// Байты проверенных файлов модели.
#[derive(Clone)]
pub struct ModelFiles {
    pub config: Vec<u8>,
    pub enc: Vec<u8>,
    pub erb_dec: Vec<u8>,
    pub df_dec: Vec<u8>,
}

impl std::fmt::Debug for ModelFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelFiles")
            .field("config", &self.config.len())
            .field("enc", &self.enc.len())
            .field("erb_dec", &self.erb_dec.len())
            .field("df_dec", &self.df_dec.len())
            .finish()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Отпечаток закреплённого манифеста: SHA-256 канонического JSON списка файлов.
pub fn manifest_sha256() -> String {
    let files: Vec<FileIdentity> = FILES
        .iter()
        .map(|f| FileIdentity {
            name: f.name.into(),
            byte_length: f.byte_length,
            sha256: f.sha256.into(),
        })
        .collect();
    sha256_hex(&serde_json::to_vec(&files).expect("манифест сериализуем"))
}

pub fn pinned_identity() -> ModelIdentity {
    ModelIdentity {
        name: MODEL_NAME.into(),
        variant: MODEL_VARIANT.into(),
        sample_rate: SAMPLE_RATE,
        manifest_sha256: manifest_sha256(),
        files: FILES
            .iter()
            .map(|f| FileIdentity {
                name: f.name.into(),
                byte_length: f.byte_length,
                sha256: f.sha256.into(),
            })
            .collect(),
        source: ModelSource {
            repository: UPSTREAM_REPOSITORY.into(),
            commit: UPSTREAM_COMMIT.into(),
            archive: UPSTREAM_ARCHIVE.into(),
            archive_sha256: UPSTREAM_ARCHIVE_SHA256.into(),
        },
        license: LICENSE.into(),
    }
}

/// Значение ключа секции минимального INI `config.ini`.
fn ini_value<'a>(text: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut current = "";
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name.trim();
        } else if current == section
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim());
        }
    }
    None
}

/// Проверка байтов: сначала вариант модели и частота (понятная диагностика
/// для DeepFilterNet2 и других моделей), затем длины и SHA-256.
pub fn verify_bytes(files: &ModelFiles) -> Result<ModelIdentity, ModelError> {
    let config = std::str::from_utf8(&files.config)
        .map_err(|_| error(ModelErrorKind::WrongVersion, "config.ini не в UTF-8"))?;
    let variant = ini_value(config, "train", "model").unwrap_or("");
    if variant != MODEL_VARIANT {
        return Err(error(
            ModelErrorKind::WrongVersion,
            format!(
                "config.ini описывает модель «{variant}», а требуется {MODEL_VARIANT} ({MODEL_NAME})"
            ),
        ));
    }
    let rate = ini_value(config, "df", "sr").unwrap_or("");
    if rate != SAMPLE_RATE.to_string() {
        return Err(error(
            ModelErrorKind::WrongVersion,
            format!("config.ini задаёт частоту «{rate}», а требуется {SAMPLE_RATE}"),
        ));
    }
    for (pinned, bytes) in
        FILES
            .iter()
            .zip([&files.config, &files.enc, &files.erb_dec, &files.df_dec])
    {
        if bytes.len() as u64 != pinned.byte_length || sha256_hex(bytes) != pinned.sha256 {
            return Err(error(
                ModelErrorKind::Corrupt,
                format!(
                    "{} не совпадает с закреплённым манифестом {MODEL_NAME} (длина или SHA-256)",
                    pinned.name
                ),
            ));
        }
    }
    Ok(pinned_identity())
}

fn read_regular(dir: &Path, name: &str) -> Result<Vec<u8>, ModelError> {
    let path = dir.join(name);
    // NOFOLLOW: ссылка на посторонний файл вместо веса не принимается.
    let fd = open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| match e {
        rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR => {
            error(ModelErrorKind::Missing, format!("нет файла модели {name}"))
        }
        rustix::io::Errno::LOOP => error(
            ModelErrorKind::NotRegularFile,
            format!("{name} — символическая ссылка"),
        ),
        other => error(ModelErrorKind::Io, format!("{name}: {other}")),
    })?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|e| error(ModelErrorKind::Io, format!("{name}: {e}")))?;
    if !meta.is_file() {
        return Err(error(
            ModelErrorKind::NotRegularFile,
            format!("{name} — не обычный файл"),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MODEL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error(ModelErrorKind::Io, format!("{name}: {e}")))?;
    if bytes.len() as u64 > MAX_MODEL_FILE_BYTES {
        return Err(error(
            ModelErrorKind::Corrupt,
            format!("{name} больше {MAX_MODEL_FILE_BYTES} байт"),
        ));
    }
    Ok(bytes)
}

/// Читает и проверяет каталог модели с четырьмя файлами.
pub fn load_dir(dir: &Path) -> Result<(ModelIdentity, ModelFiles), ModelError> {
    if !dir.is_dir() {
        return Err(error(
            ModelErrorKind::Missing,
            format!("каталог модели {} не найден", dir.display()),
        ));
    }
    let files = ModelFiles {
        config: read_regular(dir, "config.ini")?,
        enc: read_regular(dir, "enc.onnx")?,
        erb_dec: read_regular(dir, "erb_dec.onnx")?,
        df_dec: read_regular(dir, "df_dec.onnx")?,
    };
    let identity = verify_bytes(&files)?;
    Ok((identity, files))
}

/// Читает файлы модели из upstream-архива `.tar.gz`: берутся записи с
/// базовыми именами из манифеста, повторы и не обычные файлы отвергаются.
fn read_archive(path: &Path) -> Result<ModelFiles, ModelError> {
    let file =
        File::open(path).map_err(|e| error(ModelErrorKind::Io, format!("архив модели: {e}")))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut found: [Option<Vec<u8>>; 4] = Default::default();
    let entries = archive
        .entries()
        .map_err(|e| error(ModelErrorKind::Io, format!("архив модели: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| error(ModelErrorKind::Io, format!("архив модели: {e}")))?;
        let name = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(String::from));
        let Some(index) = name
            .as_deref()
            .and_then(|n| FILES.iter().position(|f| f.name == n))
        else {
            continue;
        };
        if !entry.header().entry_type().is_file() {
            return Err(error(
                ModelErrorKind::NotRegularFile,
                format!("{} в архиве — не обычный файл", FILES[index].name),
            ));
        }
        if found[index].is_some() {
            return Err(error(
                ModelErrorKind::Corrupt,
                format!("{} встречается в архиве дважды", FILES[index].name),
            ));
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_MODEL_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| error(ModelErrorKind::Io, format!("архив модели: {e}")))?;
        found[index] = Some(bytes);
    }
    let [Some(config), Some(enc), Some(erb_dec), Some(df_dec)] = found else {
        return Err(error(
            ModelErrorKind::Missing,
            "в архиве нет всех файлов модели (config.ini, enc.onnx, erb_dec.onnx, df_dec.onnx)",
        ));
    };
    Ok(ModelFiles {
        config,
        enc,
        erb_dec,
        df_dec,
    })
}

/// Копия тех же весов в исходниках crate `deep-filter-rt` 0.1.1, которые Cargo
/// уже распаковал в локальный реестр при сборке (без сети). Годится как
/// источник [`install`]; сама обработка из реестра не читает.
pub fn cargo_registry_copy() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let src = home.join("registry").join("src");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&src)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| {
            e.path()
                .join("deep-filter-rt-0.1.1")
                .join("models")
                .join(MODEL_NAME)
        })
        .filter(|p| p.is_dir())
        .collect();
    candidates.sort();
    candidates.into_iter().find(|p| load_dir(p).is_ok())
}

/// Каталог установленной модели внутри корня моделей.
pub fn installed_dir(models_root: &Path) -> PathBuf {
    models_root.join(MODEL_VARIANT).join(manifest_sha256())
}

/// Явная установка весов из локального архива upstream или каталога с
/// четырьмя файлами. Сеть не используется. Установка идемпотентна:
/// существующий проверенный каталог не переписывается, повреждённый —
/// отказ с указанием пути (удалять чужие файлы операция не берётся).
pub fn install(source: &Path, models_root: &Path) -> Result<(PathBuf, ModelIdentity), ModelError> {
    let files = if source.is_dir() {
        load_dir(source)?.1
    } else if source.is_file() {
        read_archive(source)?
    } else {
        return Err(error(
            ModelErrorKind::Missing,
            format!("источник модели {} не найден", source.display()),
        ));
    };
    let identity = verify_bytes(&files)?;
    let target = installed_dir(models_root);
    if target.exists() {
        return load_dir(&target)
            .map(|(identity, _)| (target.clone(), identity))
            .map_err(|e| {
                error(
                    e.kind,
                    format!(
                        "существующая установка {} не проходит проверку ({}); удалите её и повторите установку",
                        target.display(),
                        e.message
                    ),
                )
            });
    }
    let parent = target.parent().expect("у каталога модели есть родитель");
    std::fs::create_dir_all(parent).map_err(|e| error(ModelErrorKind::Io, e.to_string()))?;
    let staging = parent.join(format!(
        ".install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let write_all = || -> std::io::Result<()> {
        std::fs::create_dir(&staging)?;
        for (pinned, bytes) in
            FILES
                .iter()
                .zip([&files.config, &files.enc, &files.erb_dec, &files.df_dec])
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(staging.join(pinned.name))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        let mut meta = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.join("model.json"))?;
        meta.write_all(&serde_json::to_vec_pretty(&identity).expect("идентичность сериализуема"))?;
        meta.sync_all()?;
        File::open(&staging)?.sync_all()?;
        std::fs::rename(&staging, &target)?;
        File::open(parent)?.sync_all()
    };
    if let Err(e) = write_all() {
        let _ = std::fs::remove_dir_all(&staging);
        // Параллельная установка того же манифеста успела раньше.
        if target.exists() {
            return load_dir(&target).map(|(identity, _)| (target, identity));
        }
        return Err(error(ModelErrorKind::Io, e.to_string()));
    }
    let (identity, _) = load_dir(&target)?;
    Ok((target, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_lookup_respects_sections() {
        let text = "[train]\nmodel = deepfilternet3\n[df]\nsr = 48000\n[other]\nmodel = x\n";
        assert_eq!(ini_value(text, "train", "model"), Some("deepfilternet3"));
        assert_eq!(ini_value(text, "other", "model"), Some("x"));
        assert_eq!(ini_value(text, "df", "model"), None);
    }

    #[test]
    fn deepfilternet2_config_is_wrong_version_not_corrupt() {
        let files = ModelFiles {
            config: b"[train]\nmodel = deepfilternet2\n[df]\nsr = 48000\n".to_vec(),
            enc: Vec::new(),
            erb_dec: Vec::new(),
            df_dec: Vec::new(),
        };
        let err = verify_bytes(&files).unwrap_err();
        assert_eq!(err.kind, ModelErrorKind::WrongVersion);
        assert!(err.message.contains("deepfilternet2"));
    }

    #[test]
    fn manifest_fingerprint_is_stable() {
        assert_eq!(manifest_sha256(), manifest_sha256());
        assert_eq!(pinned_identity().files.len(), 4);
    }
}
