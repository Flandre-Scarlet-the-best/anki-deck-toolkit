//! Проверка Yomitan-архива как недоверенного формата импорта.
//!
//! Архив никогда не распаковывается на диск: банки читаются в память по
//! одному с ограничением фактически прочитанных байт. Поэтому traversal и
//! symlink не могут записать файл вне каталога, но такие имена всё равно
//! отвергаются: архив с ними не является корректным словарём.
//!
//! Семантика банка определяется его типом (`term_bank_*`, `kanji_bank_*`, …)
//! и профилем источника из каталога, а не именем архива.

use crate::catalog::{BankKind, Source};
use crate::error::{Code, Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Пределы распаковки. Значения по умолчанию с запасом покрывают реальные
/// словари стандартного профиля и отсекают zip bomb.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    /// Предельное отношение распакованного размера к сжатому для одной записи.
    pub max_ratio: u64,
    pub max_index_bytes: u64,
    pub max_name_bytes: usize,
    /// Предел строк одного банка.
    pub max_bank_rows: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_entry_bytes: 256 * 1024 * 1024,
            max_total_bytes: 3 * 1024 * 1024 * 1024,
            max_ratio: 1_000,
            max_index_bytes: 256 * 1024,
            max_name_bytes: 255,
            max_bank_rows: 2_000_000,
        }
    }
}

/// Метаданные `index.json`, нужные менеджеру; исходный JSON сохраняется целиком.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub title: String,
    pub revision: String,
    pub format: u64,
    pub author: Option<String>,
    pub url: Option<String>,
    pub attribution: Option<String>,
    pub source_language: Option<String>,
    pub target_language: Option<String>,
    pub frequency_mode: Option<String>,
    pub is_updatable: Option<bool>,
    pub index_url: Option<String>,
    pub download_url: Option<String>,
    pub raw: Value,
}

/// Банк: тип, номер и имя записи архива.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Bank {
    pub kind: BankKind,
    pub number: u32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub index: IndexMeta,
    /// Банки в детерминированном порядке: тип, затем номер.
    pub banks: Vec<Bank>,
    pub media_files: usize,
    pub entries: usize,
    pub declared_uncompressed_bytes: u64,
}

/// Медиа и стили, на которые ссылается structured-content Yomitan.
const MEDIA_EXTENSIONS: [&str; 11] = [
    "css", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico", "tif",
];

/// Сопроводительные тексты (лицензии встроенных шрифтов и т. п.). Их
/// содержимое не разбирается, но архив с ними — корректный словарь: так,
/// Jitendex кладёт `LICENSE` рядом со своим шрифтом.
const NOTICE_NAMES: [&str; 5] = ["license", "licence", "copying", "readme", "notice"];
const NOTICE_EXTENSIONS: [&str; 2] = ["txt", "md"];

fn is_auxiliary(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, extension)) => (stem.to_owned(), Some(extension.to_owned())),
        None => (base.clone(), None),
    };
    match extension.as_deref() {
        Some(e) if MEDIA_EXTENSIONS.contains(&e) || NOTICE_EXTENSIONS.contains(&e) => true,
        Some(_) => false,
        None => NOTICE_NAMES.contains(&stem.as_str()),
    }
}

/// Открытый проверенный архив: банки читаются только через него.
pub struct Archive {
    zip: zip::ZipArchive<File>,
    limits: Limits,
    read_total: u64,
}

fn bank_of(name: &str) -> Option<(BankKind, u32)> {
    let stem = name.strip_suffix(".json")?;
    for kind in BankKind::ALL {
        if let Some(number) = stem.strip_prefix(kind.prefix()) {
            let valid = !number.is_empty()
                && number.len() <= 6
                && number.bytes().all(|b| b.is_ascii_digit())
                && !number.starts_with('0');
            return valid
                .then(|| number.parse().ok())
                .flatten()
                .map(|n| (kind, n));
        }
    }
    None
}

/// Имя записи: относительный путь из обычных компонентов без опасных знаков.
fn check_name(name: &str, limits: &Limits) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::unsafe_archive(format!(
            "запись архива «{}»: {why}",
            name.escape_debug()
        )))
    };
    if name.is_empty() || name.len() > limits.max_name_bytes {
        return bad("пустое или слишком длинное имя");
    }
    if name.starts_with('/') || name.contains('\\') || name.contains(':') {
        return bad("абсолютный путь, обратная косая черта или двоеточие");
    }
    if name.chars().any(|c| c.is_control()) {
        return bad("управляющий символ в имени");
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return bad("пустой компонент, «.» или «..»");
    }
    Ok(())
}

fn is_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|m| m & 0o170_000 == 0o120_000)
}

impl Archive {
    /// Открывает и проверяет архив целиком (без распаковки банков) и
    /// разбирает `index.json` под профилем источника.
    pub fn open(path: &Path, source: &Source, limits: Limits) -> Result<(Self, Inspection)> {
        let file = File::open(path).map_err(|e| Error::io(path.display(), e))?;
        let zip = zip::ZipArchive::new(file)
            .map_err(|e| Error::format(format!("архив не читается как ZIP: {e}")))?;
        if zip.len() > limits.max_entries {
            return Err(Error::limit(format!(
                "в архиве {} записей, предел {}",
                zip.len(),
                limits.max_entries
            )));
        }
        let mut archive = Archive {
            zip,
            limits,
            read_total: 0,
        };
        let inspection = archive.inspect(source)?;
        Ok((archive, inspection))
    }

    fn inspect(&mut self, source: &Source) -> Result<Inspection> {
        let limits = self.limits;
        let mut names = BTreeSet::new();
        let mut folded = BTreeSet::new();
        let mut banks = Vec::new();
        let mut media = 0usize;
        let mut has_index = false;
        let mut declared_total = 0u64;
        for i in 0..self.zip.len() {
            let entry = self
                .zip
                .by_index_raw(i)
                .map_err(|e| Error::format(format!("запись {i} архива не читается: {e}")))?;
            let name = entry
                .name()
                .map_err(|e| Error::unsafe_archive(format!("запись {i}: имя не читается: {e}")))?
                .into_owned();
            check_name(&name, &limits)?;
            if !names.insert(name.clone()) || !folded.insert(name.to_lowercase()) {
                return Err(Error::unsafe_archive(format!(
                    "повторяющееся имя записи «{name}»"
                )));
            }
            if is_symlink(entry.unix_mode()) {
                return Err(Error::unsafe_archive(format!(
                    "запись «{name}» — символическая ссылка"
                )));
            }
            if entry.encrypted() {
                return Err(Error::unsafe_archive(format!(
                    "запись «{name}» зашифрована"
                )));
            }
            if entry.is_dir() {
                continue;
            }
            match entry.compression() {
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated => {}
                other => {
                    return Err(Error::new(
                        Code::UnsupportedSchema,
                        format!("запись «{name}»: неподдерживаемое сжатие {other:?}"),
                    ));
                }
            }
            let size = entry.size();
            let compressed = entry.compressed_size().max(1);
            if size > limits.max_entry_bytes {
                return Err(Error::limit(format!(
                    "запись «{name}» распаковывается в {size} байт, предел {}",
                    limits.max_entry_bytes
                )));
            }
            if size / compressed > limits.max_ratio {
                return Err(Error::unsafe_archive(format!(
                    "запись «{name}»: коэффициент сжатия {} выше предела {} (zip bomb)",
                    size / compressed,
                    limits.max_ratio
                )));
            }
            declared_total = declared_total.saturating_add(size);
            if declared_total > limits.max_total_bytes {
                return Err(Error::limit(format!(
                    "суммарный распакованный размер больше {} байт",
                    limits.max_total_bytes
                )));
            }
            let at_root = !name.contains('/');
            if at_root && name == "index.json" {
                has_index = true;
            } else if at_root && let Some((kind, number)) = bank_of(&name) {
                if !source.allowed_banks.contains(&kind) {
                    return Err(Error::new(
                        Code::UnsupportedSchema,
                        format!(
                            "банк «{name}» ({kind:?}) не ожидается в словаре {}",
                            source.id
                        ),
                    ));
                }
                banks.push(Bank { kind, number, name });
            } else if at_root && name.contains("_bank_") {
                return Err(Error::new(
                    Code::UnsupportedSchema,
                    format!("неизвестный тип банка «{name}»"),
                ));
            } else {
                if !is_auxiliary(&name) {
                    return Err(Error::format(format!(
                        "неожиданный файл «{name}»: ни index.json, ни банк, ни медиа или сопроводительный текст Yomitan"
                    )));
                }
                media += 1;
            }
        }
        if !has_index {
            return Err(Error::format("в корне архива нет index.json"));
        }
        for required in source.required_banks {
            if !banks.iter().any(|b| b.kind == *required) {
                return Err(Error::format(format!(
                    "нет обязательного банка {required:?} для словаря {}",
                    source.id
                )));
            }
        }
        banks.sort();
        let index = self.read_index(source)?;
        Ok(Inspection {
            index,
            banks,
            media_files: media,
            entries: self.zip.len(),
            declared_uncompressed_bytes: declared_total,
        })
    }

    /// Читает запись целиком с проверкой фактического размера.
    fn read_entry(&mut self, name: &str, limit: u64) -> Result<Vec<u8>> {
        let entry = self
            .zip
            .by_name(name)
            .map_err(|e| Error::format(format!("запись «{name}» не читается: {e}")))?;
        let declared = entry.size();
        let mut bytes = Vec::with_capacity(declared.min(limit) as usize);
        entry
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::format(format!("запись «{name}» повреждена: {e}")))?;
        let actual = bytes.len() as u64;
        if actual > limit {
            return Err(Error::limit(format!("запись «{name}» больше {limit} байт")));
        }
        if actual != declared {
            return Err(Error::unsafe_archive(format!(
                "запись «{name}»: распаковано {actual} байт, заявлено {declared}"
            )));
        }
        self.read_total += actual;
        if self.read_total > self.limits.max_total_bytes {
            return Err(Error::limit("суммарно распаковано больше предела"));
        }
        Ok(bytes)
    }

    fn read_index(&mut self, source: &Source) -> Result<IndexMeta> {
        let bytes = self.read_entry("index.json", self.limits.max_index_bytes)?;
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::format(format!("index.json не является JSON: {e}")))?;
        let object = raw
            .as_object()
            .ok_or_else(|| Error::format("index.json — не объект"))?;
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        let format = object
            .get("format")
            .or_else(|| object.get("version"))
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::format("в index.json нет числового format/version"))?;
        if format != 3 {
            return Err(Error::new(
                Code::UnsupportedSchema,
                format!("формат Yomitan {format} не поддерживается: ожидается 3"),
            ));
        }
        let title = text("title").ok_or_else(|| Error::format("в index.json нет title"))?;
        let revision = text("revision")
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| Error::format("в index.json нет revision"))?;
        if !title.starts_with(source.index_title_prefix) {
            return Err(Error::format(format!(
                "title «{title}» не похож на словарь {} (ожидается начало «{}»)",
                source.id, source.index_title_prefix
            )));
        }
        Ok(IndexMeta {
            title,
            revision,
            format,
            author: text("author"),
            url: text("url"),
            attribution: text("attribution"),
            source_language: text("sourceLanguage"),
            target_language: text("targetLanguage"),
            frequency_mode: text("frequencyMode"),
            is_updatable: object.get("isUpdatable").and_then(Value::as_bool),
            index_url: text("indexUrl"),
            download_url: text("downloadUrl"),
            raw,
        })
    }

    /// Строки банка: JSON-массив, каждая строка — массив.
    pub fn read_bank(&mut self, bank: &Bank) -> Result<Vec<Vec<Value>>> {
        let bytes = self.read_entry(&bank.name, self.limits.max_entry_bytes)?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::format(format!("{} не является JSON: {e}", bank.name)))?;
        let Value::Array(rows) = value else {
            return Err(Error::format(format!("{} — не массив", bank.name)));
        };
        if rows.len() > self.limits.max_bank_rows {
            return Err(Error::limit(format!(
                "{}: {} строк, предел {}",
                bank.name,
                rows.len(),
                self.limits.max_bank_rows
            )));
        }
        rows.into_iter()
            .enumerate()
            .map(|(i, row)| match row {
                Value::Array(cells) => Ok(cells),
                _ => Err(Error::format(format!("{}[{i}] — не массив", bank.name))),
            })
            .collect()
    }
}

/// Распознанные банки по типам — для отчётов.
pub fn bank_counts(banks: &[Bank]) -> BTreeMap<BankKind, usize> {
    let mut counts = BTreeMap::new();
    for bank in banks {
        *counts.entry(bank.kind).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_names_are_strict() {
        assert_eq!(bank_of("term_bank_1.json"), Some((BankKind::Term, 1)));
        assert_eq!(
            bank_of("term_meta_bank_12.json"),
            Some((BankKind::TermMeta, 12))
        );
        assert_eq!(bank_of("kanji_bank_2.json"), Some((BankKind::Kanji, 2)));
        assert_eq!(bank_of("term_bank_01.json"), None);
        assert_eq!(bank_of("term_bank_.json"), None);
        assert_eq!(bank_of("term_bank_1.json.bak"), None);
    }

    #[test]
    fn auxiliary_files_are_media_or_notices() {
        for name in [
            "styles.css",
            "jitendex/HanaMinA/LICENSE",
            "a/b.avif",
            "README.md",
        ] {
            assert!(is_auxiliary(name), "{name}");
        }
        for name in ["run.sh", "payload.exe", "data.json", "Makefile"] {
            assert!(!is_auxiliary(name), "{name}");
        }
    }

    #[test]
    fn dangerous_names_are_refused() {
        let l = Limits::default();
        for name in [
            "../x", "/abs", "a/../b", "a\\b", "c:x", "a//b", "./a", "a\u{0}b",
        ] {
            assert!(check_name(name, &l).is_err(), "{name}");
        }
        for name in ["index.json", "jitendex/", "jitendex/graphics/a.svg"] {
            assert!(check_name(name, &l).is_ok(), "{name}");
        }
    }
}
