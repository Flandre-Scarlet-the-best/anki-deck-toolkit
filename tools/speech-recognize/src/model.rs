//! Закреплённые акустические CTC-модели, их проверка и установка.
//!
//! Веса не скачиваются и не встраиваются: их кладёт в локальный каталог
//! явная операция [`install`] из уже загруженного пользователем снимка
//! репозитория модели, а каждый запуск заново сверяет файлы с манифестом —
//! точными длинами и SHA-256 на закреплённой ревизии. Файл открывается без
//! перехода по ссылкам, его длина проверяется до чтения, чтение ограничено
//! закреплённой длиной. Pickle (`pytorch_model.bin`, `training_args.bin`)
//! не читается вовсе: веса берутся только из safetensors.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::encoder::{Architecture, Model, Spec};
use crate::safetensors;

pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Missing,
    NotRegularFile,
    WrongVersion,
    Corrupt,
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

/// Что выдаёт словарь модели: фонемы pyopenjtalk или хирагану.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Phoneme,
    Hiragana,
}

#[derive(Debug, Clone, Copy)]
pub struct PinnedFile {
    pub name: &'static str,
    pub byte_length: u64,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct PinnedModel {
    pub id: &'static str,
    pub name: &'static str,
    pub repository: &'static str,
    pub revision: &'static str,
    pub license: &'static str,
    pub output: OutputKind,
    pub architecture: Architecture,
    pub prefix: &'static str,
    pub conv_bias: bool,
    pub hidden: usize,
    pub layers: usize,
    pub heads: usize,
    pub intermediate: usize,
    pub vocab: usize,
    pub blank_token: &'static str,
    /// Нормализация входа к нулевому среднему и единичной дисперсии
    /// (`do_normalize` препроцессора).
    pub normalize_input: bool,
    /// Тишина, добавляемая до и после входа, в отсчётах 16 kHz. Короткие
    /// слова без неё теряют начальные и конечные звуки (рекомендация
    /// карточки модели и проверка на коротких записях).
    pub lead_pad_samples: usize,
    pub trail_pad_samples: usize,
    pub files: [PinnedFile; 4],
}

const CONV_KERNEL: [usize; 7] = [10, 3, 3, 3, 3, 2, 2];
const CONV_STRIDE: [usize; 7] = [5, 2, 2, 2, 2, 2, 2];
/// Наибольший закреплённый файл весов (защита от подставленного огромного файла).
const MAX_MODEL_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Основная модель: фонемный CTC на базе rinna/japanese-hubert-base,
/// дообученный на ReazonSpeech с метками pyopenjtalk-plus.
pub const HUBERT_PHONEME_V4: PinnedModel = PinnedModel {
    id: "hubert-base-phoneme-ctc-v4",
    name: "prj-beatrice/japanese-hubert-base-phoneme-ctc-v4",
    repository: "https://huggingface.co/prj-beatrice/japanese-hubert-base-phoneme-ctc-v4",
    revision: "f5fe07043bcb0b77a86faf72ac6d8fc1ae558f99",
    license: "Apache-2.0",
    output: OutputKind::Phoneme,
    architecture: Architecture::PostLayerNorm,
    prefix: "hubert",
    conv_bias: false,
    hidden: 768,
    layers: 12,
    heads: 12,
    intermediate: 3072,
    vocab: 48,
    blank_token: "PAD",
    normalize_input: false,
    lead_pad_samples: 16_000,
    trail_pad_samples: 8_000,
    files: [
        PinnedFile {
            name: "config.json",
            byte_length: 1440,
            sha256: "3c3e7d96cf3cb38dc9c83193157569a150bf1592b9a1364d5f2b2e0f6cb49688",
        },
        PinnedFile {
            name: "model.safetensors",
            byte_length: 377_659_928,
            sha256: "6707669b3b73e5471b7e879d33105f8c1821ccb8ba27048f539bf4d0c2a95d93",
        },
        PinnedFile {
            name: "preprocessor_config.json",
            byte_length: 258,
            sha256: "3698647300e38b0a83ccf0ee9b24834e2d262157410305d54a3a1f655ead3002",
        },
        PinnedFile {
            name: "vocab.json",
            byte_length: 550,
            sha256: "9c5c09049b351bf19479c84c646c69dd3f6fa12c2030b4318215ba4458211b05",
        },
    ],
};

/// Независимая вторая гипотеза: XLSR-53 large, дообученный на хирагану
/// (Common Voice + JSUT). Другой предобученный энкодер, другие данные и
/// другой вид разметки — его ошибки не повторяют ошибки фонемной модели.
pub const XLSR_HIRAGANA: PinnedModel = PinnedModel {
    id: "xlsr-japanese-hiragana-ctc",
    name: "vumichien/wav2vec2-large-xlsr-japanese-hiragana",
    repository: "https://huggingface.co/vumichien/wav2vec2-large-xlsr-japanese-hiragana",
    revision: "017225bb128a6b1c6de9d58391f891908d69bc7b",
    license: "Apache-2.0",
    output: OutputKind::Hiragana,
    architecture: Architecture::StableLayerNorm,
    prefix: "wav2vec2",
    conv_bias: true,
    hidden: 1024,
    layers: 24,
    heads: 16,
    intermediate: 4096,
    vocab: 86,
    blank_token: "[PAD]",
    normalize_input: true,
    lead_pad_samples: 8_000,
    trail_pad_samples: 8_000,
    files: [
        PinnedFile {
            name: "config.json",
            byte_length: 1565,
            sha256: "3cf1aab56647e6e09b95e1e18d4cc3bdbbea40377d7c893b9d8b0639b5846393",
        },
        PinnedFile {
            name: "model.safetensors",
            byte_length: 1_262_160_028,
            sha256: "a109c55dd35b37b337cf216539be1192b438a276a4c13bbaeabb5ea63dfd779b",
        },
        PinnedFile {
            name: "preprocessor_config.json",
            byte_length: 158,
            sha256: "c403ce09975b90dff0dd8302c42d422e9de1f166cd7772df23490069893cb0cf",
        },
        PinnedFile {
            name: "vocab.json",
            byte_length: 938,
            sha256: "00c2e09d6bd1c615b56c8784f3a59596ba96b3f9e1f1e532cf739a2cba1a7491",
        },
    ],
};

pub const MODELS: [PinnedModel; 2] = [HUBERT_PHONEME_V4, XLSR_HIRAGANA];

pub fn find(id: &str) -> Option<&'static PinnedModel> {
    MODELS.iter().find(|m| m.id == id)
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
pub struct ModelIdentity {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub revision: String,
    pub license: String,
    pub output: OutputKind,
    pub manifest_sha256: String,
    pub files: Vec<FileIdentity>,
    pub sample_rate: u32,
    pub frame_stride: usize,
    pub receptive_field: usize,
    pub lead_pad_samples: usize,
    pub trail_pad_samples: usize,
    pub normalize_input: bool,
    pub blank_id: usize,
    /// Токены словаря по индексу.
    pub vocab: Vec<String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl PinnedModel {
    pub fn spec(&self) -> Spec {
        Spec {
            architecture: self.architecture,
            prefix: self.prefix.into(),
            conv_dim: 512,
            conv_kernel: CONV_KERNEL.to_vec(),
            conv_stride: CONV_STRIDE.to_vec(),
            conv_bias: self.conv_bias,
            hidden: self.hidden,
            layers: self.layers,
            heads: self.heads,
            intermediate: self.intermediate,
            pos_kernel: 128,
            pos_groups: 16,
            vocab: self.vocab,
        }
    }

    pub fn file_identities(&self) -> Vec<FileIdentity> {
        self.files
            .iter()
            .map(|f| FileIdentity {
                name: f.name.into(),
                byte_length: f.byte_length,
                sha256: f.sha256.into(),
            })
            .collect()
    }

    /// Отпечаток манифеста: идентификатор, ревизия и закреплённые файлы.
    pub fn manifest_sha256(&self) -> String {
        let manifest = serde_json::json!({
            "id": self.id,
            "revision": self.revision,
            "files": self.file_identities(),
        });
        sha256_hex(&serde_json::to_vec(&manifest).expect("манифест сериализуем"))
    }
}

/// Проверенные файлы модели.
pub struct LoadedModel {
    pub identity: ModelIdentity,
    pub model: Model,
}

fn read_regular(dir: &Path, pinned: &PinnedFile) -> Result<Vec<u8>, ModelError> {
    let name = pinned.name;
    let fd = open(
        dir.join(name),
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
    if meta.len() != pinned.byte_length || pinned.byte_length > MAX_MODEL_FILE_BYTES {
        return Err(error(
            ModelErrorKind::Corrupt,
            format!("{name}: длина не совпала с закреплённым манифестом"),
        ));
    }
    let mut bytes = Vec::with_capacity(pinned.byte_length as usize);
    file.take(pinned.byte_length + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error(ModelErrorKind::Io, format!("{name}: {e}")))?;
    if bytes.len() as u64 != pinned.byte_length || sha256_hex(&bytes) != pinned.sha256 {
        return Err(error(
            ModelErrorKind::Corrupt,
            format!("{name}: длина или SHA-256 не совпали с закреплённым манифестом"),
        ));
    }
    Ok(bytes)
}

fn file_bytes<'a>(files: &'a BTreeMap<&str, Vec<u8>>, name: &str) -> &'a [u8] {
    files.get(name).map(Vec::as_slice).unwrap_or_default()
}

/// Сверяет смысловые поля конфигурации и словаря с манифестом. При
/// совпавших SHA-256 это повторная проверка самого манифеста, но она же
/// документирует, на какие свойства опирается код.
fn vocab_tokens(
    pinned: &PinnedModel,
    files: &BTreeMap<&str, Vec<u8>>,
) -> Result<Vec<String>, ModelError> {
    let wrong = |m: String| error(ModelErrorKind::WrongVersion, m);
    let config: serde_json::Value = serde_json::from_slice(file_bytes(files, "config.json"))
        .map_err(|e| wrong(format!("config.json: {e}")))?;
    let stable = pinned.architecture == Architecture::StableLayerNorm;
    if config["hidden_size"] != pinned.hidden
        || config["num_hidden_layers"] != pinned.layers
        || config["num_attention_heads"] != pinned.heads
        || config["do_stable_layer_norm"] != stable
        || config["feat_extract_norm"] != if stable { "layer" } else { "group" }
        || config["hidden_act"] != "gelu"
        || config["conv_stride"] != serde_json::json!(CONV_STRIDE)
        || config["conv_kernel"] != serde_json::json!(CONV_KERNEL)
    {
        return Err(wrong("config.json описывает другую архитектуру".into()));
    }
    let pre: serde_json::Value =
        serde_json::from_slice(file_bytes(files, "preprocessor_config.json"))
            .map_err(|e| wrong(format!("preprocessor_config.json: {e}")))?;
    if pre["sampling_rate"] != SAMPLE_RATE || pre["do_normalize"] != pinned.normalize_input {
        return Err(wrong(
            "препроцессор задаёт другую частоту или нормализацию".into(),
        ));
    }
    let vocab: BTreeMap<String, usize> = serde_json::from_slice(file_bytes(files, "vocab.json"))
        .map_err(|e| wrong(format!("vocab.json: {e}")))?;
    let mut tokens = vec![String::new(); pinned.vocab];
    for (token, id) in vocab {
        let slot = tokens
            .get_mut(id)
            .ok_or_else(|| wrong(format!("vocab.json: индекс {id} вне словаря")))?;
        if !slot.is_empty() {
            return Err(wrong(format!("vocab.json: индекс {id} повторяется")));
        }
        *slot = token;
    }
    if tokens.iter().any(String::is_empty) || !tokens.iter().any(|t| t == pinned.blank_token) {
        return Err(wrong("vocab.json не покрывает словарь модели".into()));
    }
    Ok(tokens)
}

pub fn identity(pinned: &PinnedModel, tokens: Vec<String>) -> ModelIdentity {
    let spec = pinned.spec();
    ModelIdentity {
        id: pinned.id.into(),
        name: pinned.name.into(),
        repository: pinned.repository.into(),
        revision: pinned.revision.into(),
        license: pinned.license.into(),
        output: pinned.output,
        manifest_sha256: pinned.manifest_sha256(),
        files: pinned.file_identities(),
        sample_rate: SAMPLE_RATE,
        frame_stride: spec.frame_stride(),
        receptive_field: spec.receptive_field(),
        lead_pad_samples: pinned.lead_pad_samples,
        trail_pad_samples: pinned.trail_pad_samples,
        normalize_input: pinned.normalize_input,
        blank_id: tokens
            .iter()
            .position(|t| t == pinned.blank_token)
            .expect("проверено"),
        vocab: tokens,
    }
}

fn read_all(
    pinned: &PinnedModel,
    dir: &Path,
) -> Result<BTreeMap<&'static str, Vec<u8>>, ModelError> {
    if !dir.is_dir() {
        return Err(error(
            ModelErrorKind::Missing,
            format!("каталог модели {} не найден", dir.display()),
        ));
    }
    pinned
        .files
        .iter()
        .map(|f| Ok((f.name, read_regular(dir, f)?)))
        .collect()
}

/// Проверяет файлы без загрузки весов в модель (быстрый `identify`).
pub fn verify_dir(pinned: &PinnedModel, dir: &Path) -> Result<ModelIdentity, ModelError> {
    let files = read_all(pinned, dir)?;
    let tokens = vocab_tokens(pinned, &files)?;
    Ok(identity(pinned, tokens))
}

pub fn load_dir(pinned: &PinnedModel, dir: &Path) -> Result<LoadedModel, ModelError> {
    let mut files = read_all(pinned, dir)?;
    let tokens = vocab_tokens(pinned, &files)?;
    let weights = files.remove("model.safetensors").unwrap_or_default();
    let tensors =
        safetensors::parse(&weights).map_err(|e| error(ModelErrorKind::Corrupt, e.to_string()))?;
    drop(weights);
    let model = Model::build(pinned.spec(), tensors)
        .map_err(|e| error(ModelErrorKind::WrongVersion, e.to_string()))?;
    Ok(LoadedModel {
        identity: identity(pinned, tokens),
        model,
    })
}

/// Каталог установки: `<models_root>/<id>/<manifest_sha256>`. Другая
/// ревизия той же модели ставится рядом и не подменяет прежнюю.
pub fn installed_dir(models_root: &Path, pinned: &PinnedModel) -> PathBuf {
    models_root.join(pinned.id).join(pinned.manifest_sha256())
}

/// Устанавливает проверенные файлы из локального снимка репозитория модели в
/// `<models_root>/<id>/`. Источник проверяется полностью до записи; запись
/// идёт во временный каталог рядом и публикуется переименованием, поэтому
/// прерванная установка не оставляет частичной модели под рабочим именем.
pub fn install(
    pinned: &PinnedModel,
    source: &Path,
    models_root: &Path,
) -> Result<ModelIdentity, ModelError> {
    let files = read_all(pinned, source)?;
    let tokens = vocab_tokens(pinned, &files)?;
    let io = |e: std::io::Error| error(ModelErrorKind::Io, e.to_string());
    let target = installed_dir(models_root, pinned);
    let parent = target.parent().expect("каталог модели вложен в корень");
    std::fs::create_dir_all(parent).map_err(io)?;
    let staging = parent.join(format!(".installing-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io)?;
    }
    std::fs::create_dir(&staging).map_err(io)?;
    for (name, bytes) in &files {
        let mut file = File::create_new(staging.join(name)).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
    }
    if target.exists() {
        // Прежняя установка заменяется только полностью проверенной копией.
        std::fs::remove_dir_all(&target).map_err(io)?;
    }
    std::fs::rename(&staging, &target).map_err(io)?;
    verify_dir(pinned, &target)?;
    Ok(identity(pinned, tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_manifests_differ() {
        assert_ne!(MODELS[0].id, MODELS[1].id);
        assert_ne!(MODELS[0].manifest_sha256(), MODELS[1].manifest_sha256());
        assert_eq!(find("hubert-base-phoneme-ctc-v4").unwrap().vocab, 48);
        assert!(find("nope").is_none());
    }

    #[test]
    fn frame_geometry_is_20ms() {
        let spec = HUBERT_PHONEME_V4.spec();
        assert_eq!(spec.frame_stride(), 320);
        assert_eq!(spec.receptive_field(), 400);
        assert_eq!(spec.frames_for(16_000), 49);
        assert_eq!(spec.frames_for(399), 0);
        assert_eq!(spec.frames_for(400), 1);
    }

    #[test]
    fn missing_and_symlinked_files_are_diagnosed() {
        let dir =
            std::env::temp_dir().join(format!("speech-recognize-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = verify_dir(&HUBERT_PHONEME_V4, &dir).unwrap_err();
        assert_eq!(err.kind, ModelErrorKind::Missing);
        std::fs::write(dir.join("elsewhere"), b"{}").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("config.json")).unwrap();
        let err = verify_dir(&HUBERT_PHONEME_V4, &dir).unwrap_err();
        assert_eq!(err.kind, ModelErrorKind::NotRegularFile);
        std::fs::remove_file(dir.join("config.json")).unwrap();
        std::fs::write(dir.join("config.json"), b"{}").unwrap();
        let err = verify_dir(&HUBERT_PHONEME_V4, &dir).unwrap_err();
        assert_eq!(err.kind, ModelErrorKind::Corrupt);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
