//! Пакет распакованного MV3-адаптера: единственный источник его runtime-файлов.
//!
//! Один и тот же набор файлов разворачивает локальный `setup` (вместе с
//! индивидуальным `pairing.json`) и упаковывает релизный ZIP. Страничный модуль
//! `forvo_page.js` генерируется из общих `dom.js` и `auth.js`, поэтому второй
//! реализации правил Forvo нет ни в каталоге установки, ни в архиве.
//!
//! Здесь же — проверки, общие для сборки и установки: формат версии Chromium,
//! допустимый manifest (имя, разрешения, отсутствие `key`/`update_url`),
//! допустимые пути и детерминированный ZIP без сжатия.
use super::{MANIFEST, bridge_error};
use crate::AssetError;
use crate::hashing::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

pub(crate) const BACKGROUND: &str = include_str!("extension/background.js");
/// Файлы, без которых пакет не является адаптером.
pub const REQUIRED_FILES: [&str; 3] = ["manifest.json", "background.js", "forvo_page.js"];
/// Индивидуальный файл сопряжения: только в каталоге установки, никогда в пакете.
pub const PAIRING_FILE: &str = "pairing.json";
/// Разрешения, которые этот CLI готов принять у устанавливаемого расширения.
/// Новое разрешение требует нового CLI: обновление расширения его не добавит.
pub const ALLOWED_PERMISSIONS: [&str; 6] = [
    "alarms",
    "downloads",
    "scripting",
    "storage",
    "tabGroups",
    "webRequest",
];
pub const ALLOWED_HOSTS: [&str; 2] = ["https://forvo.com/*", "http://127.0.0.1/*"];
/// Ключи manifest, допустимые в пакете. `key` изменил бы ID распакованного
/// расширения, `update_url` — источник обновлений; оба запрещены.
const ALLOWED_MANIFEST_KEYS: [&str; 10] = [
    "manifest_version",
    "name",
    "version",
    "description",
    "minimum_chrome_version",
    "incognito",
    "background",
    "permissions",
    "host_permissions",
    "icons",
];
pub const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_FILES: usize = 32;

fn package_error(code: &str, action: &str) -> AssetError {
    bridge_error(code, action)
}

/// Модуль страницы собирается из тех же `dom.js` и `auth.js`, что исполняет
/// CDP-провайдер: второй реализации разметки Forvo нет.
pub fn page_module() -> String {
    format!(
        "// Сгенерировано pronunciation-assets из forvo/dom.js и forvo/auth.js; не редактировать.\n\
         export const forvoDom = {};\nexport const forvoAuth = () => {};\n",
        include_str!("../dom.js").trim_end(),
        include_str!("../auth.js").trim_end()
    )
}

/// Один файл пакета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Runtime-файлы встроенной в этот CLI версии адаптера в порядке записи:
/// `manifest.json` последним — каталог с новым manifest уже содержит новый код.
pub fn bundled_files() -> Vec<PackageFile> {
    vec![
        PackageFile {
            path: "forvo_page.js".into(),
            bytes: page_module().into_bytes(),
        },
        PackageFile {
            path: "background.js".into(),
            bytes: BACKGROUND.as_bytes().to_vec(),
        },
        PackageFile {
            path: "manifest.json".into(),
            bytes: MANIFEST.as_bytes().to_vec(),
        },
    ]
}

/// Порядок записи: все файлы по имени, `manifest.json` последним.
pub fn write_order(files: &mut [PackageFile]) {
    files.sort_by(|a, b| {
        (a.path == "manifest.json", &a.path).cmp(&(b.path == "manifest.json", &b.path))
    });
}

// --- Версия ------------------------------------------------------------------

/// Версия расширения Chromium: 1–4 десятичных числа 0–65535 без ведущих нулей.
#[derive(Debug, Clone)]
pub struct Version(Vec<u32>);

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split('.').collect();
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        parts
            .iter()
            .map(|part| {
                let valid = !part.is_empty()
                    && part.len() <= 5
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0'));
                valid
                    .then(|| part.parse::<u32>().ok())
                    .flatten()
                    .filter(|n| *n <= 65535)
            })
            .collect::<Option<Vec<_>>>()
            .map(Self)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    /// Недостающие части равны нулю: `1.2` == `1.2.0`, как у Chromium.
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.0.len().max(other.0.len());
        (0..len)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// Версия расширения, которую разворачивает этот CLI.
pub fn bundled_version() -> String {
    serde_json::from_str::<Value>(MANIFEST).expect("manifest")["version"]
        .as_str()
        .expect("version")
        .to_owned()
}

// --- Manifest ----------------------------------------------------------------

/// Проверенные сведения manifest пакета.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestFacts {
    pub version: String,
    pub minimum_chrome_version: Option<String>,
    pub permissions: Vec<String>,
    pub host_permissions: Vec<String>,
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => Some(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}

/// Manifest пакета допустим для этого CLI: то же имя и тип фонового кода,
/// версия Chromium, разрешения только из разрешённого списка, без `key`,
/// `update_url`, content scripts и внешних подключений.
pub fn validate_manifest(bytes: &[u8]) -> Result<ManifestFacts, AssetError> {
    let invalid = |reason: &str| {
        let mut e = package_error(
            "package_manifest_invalid",
            "manifest.json пакета не подходит этому CLI; обновите CLI или проверьте источник пакета",
        );
        e.details["reason"] = json!(reason);
        e
    };
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid("json"))?;
    let object = value.as_object().ok_or_else(|| invalid("object"))?;
    if let Some(key) = object
        .keys()
        .find(|k| !ALLOWED_MANIFEST_KEYS.contains(&k.as_str()))
    {
        let mut e = invalid("key_not_allowed");
        e.details["key"] = json!(key);
        return Err(e);
    }
    let ours: Value = serde_json::from_str(MANIFEST).expect("manifest");
    if value["manifest_version"] != 3 {
        return Err(invalid("manifest_version"));
    }
    if value["name"] != ours["name"] {
        return Err(invalid("name"));
    }
    if value["incognito"] != "not_allowed" {
        return Err(invalid("incognito"));
    }
    if value["background"] != json!({"service_worker":"background.js","type":"module"}) {
        return Err(invalid("background"));
    }
    let version = value["version"]
        .as_str()
        .filter(|v| Version::parse(v).is_some())
        .ok_or_else(|| invalid("version"))?
        .to_owned();
    let permissions = string_list(&value["permissions"]).ok_or_else(|| invalid("permissions"))?;
    if let Some(p) = permissions
        .iter()
        .find(|p| !ALLOWED_PERMISSIONS.contains(&p.as_str()))
    {
        let mut e = package_error(
            "package_permission_not_allowed",
            "Пакет запрашивает разрешение, которого этот CLI не допускает; обновите CLI",
        );
        e.details["permission"] = json!(p);
        return Err(e);
    }
    let host_permissions =
        string_list(&value["host_permissions"]).ok_or_else(|| invalid("host_permissions"))?;
    if let Some(h) = host_permissions
        .iter()
        .find(|h| !ALLOWED_HOSTS.contains(&h.as_str()))
    {
        let mut e = package_error(
            "package_permission_not_allowed",
            "Пакет запрашивает доступ к сайту, которого этот CLI не допускает; обновите CLI",
        );
        e.details["host_permission"] = json!(h);
        return Err(e);
    }
    let minimum_chrome_version = match &value["minimum_chrome_version"] {
        Value::Null => None,
        Value::String(v) if Version::parse(v).is_some() => Some(v.clone()),
        _ => return Err(invalid("minimum_chrome_version")),
    };
    Ok(ManifestFacts {
        version,
        minimum_chrome_version,
        permissions,
        host_permissions,
    })
}

// --- Пути --------------------------------------------------------------------

fn valid_segment(segment: &str) -> bool {
    (1..=64).contains(&segment.len())
        && segment.as_bytes()[0].is_ascii_alphanumeric()
        && segment.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
        && !segment.contains("..")
}

/// Путь файла пакета: не более двух сегментов `[a-z0-9][a-z0-9_.-]*`,
/// расширение `.js`, `.json` или `.png`, не `pairing.json` и не служебные
/// файлы установщика.
pub fn valid_package_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    (1..=2).contains(&segments.len())
        && segments.iter().all(|s| valid_segment(s))
        && [".js", ".json", ".png"]
            .iter()
            .any(|ext| path.ends_with(ext))
        && path != PAIRING_FILE
        && !path.contains(".update-")
}

// --- Описание и ZIP -----------------------------------------------------------

/// Описание файла в релизных метаданных.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

pub fn describe(files: &[PackageFile]) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = files
        .iter()
        .map(|f| FileEntry {
            path: f.path.clone(),
            sha256: sha256_hex(&f.bytes),
            size: f.bytes.len() as u64,
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Проверяет состав пакета без учёта происхождения: обязательные файлы,
/// допустимые пути, пределы размеров и допустимый manifest.
pub fn validate_files(files: &[PackageFile]) -> Result<ManifestFacts, AssetError> {
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(package_error(
            "package_limits_exceeded",
            "Пакет содержит недопустимое число файлов",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0_u64;
    for file in files {
        if !valid_package_path(&file.path) {
            let mut e = package_error(
                "package_path_rejected",
                "Пакет содержит недопустимый путь файла",
            );
            e.details["path"] = json!(file.path);
            return Err(e);
        }
        if !seen.insert(file.path.as_str()) {
            let mut e = package_error("package_duplicate_entry", "Файл пакета повторяется");
            e.details["path"] = json!(file.path);
            return Err(e);
        }
        total += file.bytes.len() as u64;
        if file.bytes.len() as u64 > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES {
            return Err(package_error(
                "package_limits_exceeded",
                "Файлы пакета превышают предел размера",
            ));
        }
    }
    if let Some(missing) = REQUIRED_FILES.iter().find(|r| !seen.contains(**r)) {
        let mut e = package_error(
            "package_incomplete",
            "В пакете нет обязательного файла адаптера",
        );
        e.details["missing"] = json!(missing);
        return Err(e);
    }
    let manifest = files
        .iter()
        .find(|f| f.path == "manifest.json")
        .expect("обязательный файл");
    validate_manifest(&manifest.bytes)
}

/// Детерминированный ZIP: файлы по имени, без сжатия, фиксированное время
/// 1980-01-01 и права 0644. Одинаковые файлы дают одинаковые байты архива.
pub fn build_zip(files: &[PackageFile]) -> Result<Vec<u8>, AssetError> {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, DateTime, ZipWriter};
    validate_files(files)?;
    let mut sorted: Vec<&PackageFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(DateTime::DEFAULT)
        .unix_permissions(0o644);
    let failed = |e: &dyn std::fmt::Display| {
        let mut error = package_error("package_build_failed", "Не удалось собрать ZIP пакета");
        error.details["reason"] = json!(e.to_string());
        error
    };
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for file in sorted {
        writer
            .start_file(file.path.as_str(), options)
            .map_err(|e| failed(&e))?;
        writer.write_all(&file.bytes).map_err(|e| failed(&e))?;
    }
    Ok(writer.finish().map_err(|e| failed(&e))?.into_inner())
}

/// Число записей центрального каталога по записи EOCD — независимо от
/// библиотеки ZIP, которая повторяющиеся имена схлопывает. ZIP64, комментарий
/// и несколько дисков не используются сборкой и отвергаются.
fn central_entries(bytes: &[u8]) -> Option<u16> {
    const EOCD: usize = 22;
    let start = bytes.len().checked_sub(EOCD)?;
    let record = &bytes[start..];
    let u16_at = |i: usize| u16::from_le_bytes([record[i], record[i + 1]]);
    (record[..4] == *b"PK\x05\x06"
        && u16_at(4) == 0
        && u16_at(6) == 0
        && u16_at(8) == u16_at(10)
        && u16_at(10) != 0xffff
        && u16_at(20) == 0)
        .then(|| u16_at(10))
}

/// Читает ZIP пакета с отказом до записи на диск: только обычные файлы без
/// сжатия и шифрования, без каталогов, ссылок, абсолютных путей и `..`, без
/// повторов, в пределах размера и числа файлов. Байты каждого файла читаются
/// не дальше заявленного размера, поэтому «ZIP-бомба» не раскрывается.
pub fn extract_zip(bytes: &[u8]) -> Result<Vec<PackageFile>, AssetError> {
    use zip::{CompressionMethod, ZipArchive};
    let rejected = |code: &str, path: Option<&str>| {
        let mut e = package_error(code, "ZIP пакета отклонён до установки");
        if let Some(path) = path {
            e.details["path"] = json!(path);
        }
        e
    };
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(rejected("package_limits_exceeded", None));
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| rejected("package_archive_invalid", None))?;
    match central_entries(bytes) {
        None => return Err(rejected("package_archive_invalid", None)),
        Some(count) if count as usize != archive.len() => {
            return Err(rejected("package_duplicate_entry", None));
        }
        Some(_) => {}
    }
    if archive.is_empty() || archive.len() > MAX_FILES {
        return Err(rejected("package_limits_exceeded", None));
    }
    let mut files = Vec::with_capacity(archive.len());
    let mut names = std::collections::BTreeSet::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index_raw(index)
            .map_err(|_| rejected("package_archive_invalid", None))?;
        let name = entry
            .name()
            .map_err(|_| rejected("package_path_rejected", None))?
            .into_owned();
        if entry.is_symlink() {
            return Err(rejected("package_link_rejected", Some(&name)));
        }
        if entry.is_dir() || !entry.is_file() {
            return Err(rejected("package_path_rejected", Some(&name)));
        }
        if entry.enclosed_name().is_none()
            || name.starts_with('/')
            || name.contains('\\')
            || name.split('/').any(|s| s == ".." || s == ".")
            || !valid_package_path(&name)
        {
            return Err(rejected("package_path_rejected", Some(&name)));
        }
        if !names.insert(name.clone()) {
            return Err(rejected("package_duplicate_entry", Some(&name)));
        }
        // Права файла не исполняемые; тип, отличный от обычного файла, отвергнут выше.
        if entry.unix_mode().is_some_and(|mode| mode & 0o7111 != 0) {
            return Err(rejected("package_mode_rejected", Some(&name)));
        }
        if entry.encrypted() || entry.compression() != CompressionMethod::Stored {
            return Err(rejected("package_compression_rejected", Some(&name)));
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if size > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES || entry.compressed_size() != size {
            return Err(rejected("package_limits_exceeded", Some(&name)));
        }
        let mut content = Vec::with_capacity(size as usize);
        (&mut entry)
            .take(size + 1)
            .read_to_end(&mut content)
            .map_err(|_| rejected("package_archive_invalid", Some(&name)))?;
        if content.len() as u64 != size {
            return Err(rejected("package_archive_invalid", Some(&name)));
        }
        files.push(PackageFile {
            path: name,
            bytes: content,
        });
    }
    validate_files(&files)?;
    Ok(files)
}

/// Набор файлов по путям для сравнения с диском.
pub fn by_path(files: &[PackageFile]) -> BTreeMap<&str, &[u8]> {
    files
        .iter()
        .map(|f| (f.path.as_str(), f.bytes.as_slice()))
        .collect()
}

#[cfg(test)]
#[path = "package_tests.rs"]
mod tests;
