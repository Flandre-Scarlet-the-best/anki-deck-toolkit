//! Нормализованный индекс одного словаря в SQLite и чтение из него.
//!
//! Индекс строится один раз в каталоге staging и после активации не
//! меняется; поиск открывает его только для чтения как неизменяемый файл.
//!
//! Схема корпуса ([`CORPUS_SCHEMA`]):
//!
//! - `terms` — каждая строка банка терминов с исходными полями Yomitan
//!   (`term`, `reading`, `definition_tags`, `rules`, `score`, `sequence`,
//!   `term_tags`), классом записи (`kind`) и ссылкой на исходную строку
//!   (`bank`, `row`). `score` схема банка объявляет как `number`, поэтому
//!   он хранится числом с плавающей точкой без усечения; `sequence` —
//!   целое. Глоссарий (английские значения, части речи, формы,
//!   ссылки) остаётся в сохранённом рядом проверенном `archive.zip` и
//!   адресуется парой `bank`/`row`: импорт ничего не выбрасывает, а индекс
//!   не дублирует сотни мегабайт structured-content.
//! - `readings` — допустимые пары написание/чтение, выведенные из `terms`
//!   по профилю источника, с происхождением (`origin`).
//! - `redirects` — статьи-указатели на другую форму: не чтения написания.
//! - `kanji`, `term_meta`, `tags` — соответствующие банки.
//!
//! Точный поиск идёт по отдельным столбцам-ключам (`surface_key`,
//! `character_key`, `term_key`) — NFC исходной строки ([`kana::exact_key`]),
//! той же функцией, что нормализует запрос. Исходные строки словаря
//! хранятся без изменений рядом с ключом и вместе с `bank`/`row` остаются
//! происхождением свидетельства.

use crate::catalog::{BankKind, Source, TermProfile};
use crate::error::{Error, Result};
use crate::kana;
use crate::yomitan::{Archive, Inspection};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Версия схемы корпуса. Изменение несовместимо с уже построенными индексами:
/// `verify` сообщит о несовпадении, `update` перестроит индекс.
///
/// `2`: ключи точного поиска в NFC отдельно от исходных строк.
/// `3`: `score` статьи хранится числом без усечения до целого.
pub const CORPUS_SCHEMA: u32 = 3;

const SCHEMA_SQL: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE tags(name TEXT PRIMARY KEY, category TEXT NOT NULL, sort_order INTEGER NOT NULL,
  notes TEXT NOT NULL, score INTEGER NOT NULL);
CREATE TABLE terms(id INTEGER PRIMARY KEY, bank TEXT NOT NULL, row INTEGER NOT NULL,
  term TEXT NOT NULL, reading TEXT NOT NULL, definition_tags TEXT NOT NULL, rules TEXT NOT NULL,
  score REAL NOT NULL, sequence INTEGER, term_tags TEXT NOT NULL, kind TEXT NOT NULL);
CREATE TABLE readings(term_id INTEGER NOT NULL REFERENCES terms(id), surface TEXT NOT NULL,
  surface_key TEXT NOT NULL, reading TEXT NOT NULL, match_key TEXT NOT NULL, origin TEXT NOT NULL);
CREATE TABLE redirects(term_id INTEGER NOT NULL REFERENCES terms(id), surface TEXT NOT NULL,
  surface_key TEXT NOT NULL, target TEXT NOT NULL, target_reading TEXT);
CREATE TABLE kanji(id INTEGER PRIMARY KEY, bank TEXT NOT NULL, row INTEGER NOT NULL,
  character TEXT NOT NULL, character_key TEXT NOT NULL, onyomi TEXT NOT NULL,
  kunyomi TEXT NOT NULL, tags TEXT NOT NULL, meanings TEXT NOT NULL, stats TEXT NOT NULL);
CREATE TABLE term_meta(id INTEGER PRIMARY KEY, bank TEXT NOT NULL, row INTEGER NOT NULL,
  term TEXT NOT NULL, term_key TEXT NOT NULL, mode TEXT NOT NULL, reading TEXT, value INTEGER,
  display TEXT, data TEXT NOT NULL);
";

const INDEX_SQL: &str = "
CREATE INDEX readings_surface_key ON readings(surface_key);
CREATE INDEX redirects_surface_key ON redirects(surface_key);
CREATE INDEX kanji_character_key ON kanji(character_key);
CREATE INDEX term_meta_term_key ON term_meta(term_key);
";

/// Класс строки банка терминов.
pub mod kind {
    /// Обычная статья с чтением.
    pub const ENTRY: &str = "entry";
    /// Указатель на другую форму (`redirect-glossary`): не чтение написания.
    pub const REDIRECT: &str = "redirect";
    /// JMnedict: глоссарий — список чтений написания каной.
    pub const READING_LIST: &str = "reading_list";
    /// Статья без пригодного чтения: сохраняется, но не даёт свидетельств.
    pub const NO_READING: &str = "no_reading";
}

/// Откуда взято чтение строки `readings`.
pub mod origin {
    pub const READING_FIELD: &str = "reading_field";
    pub const KANA_TERM: &str = "kana_term";
    pub const GLOSSARY_READING_LIST: &str = "glossary_reading_list";
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BuildStats {
    pub banks: usize,
    pub terms: u64,
    pub readings: u64,
    pub redirects: u64,
    pub terms_without_reading: u64,
    pub kanji: u64,
    pub term_meta: u64,
    pub tags: u64,
}

fn cell_str<'a>(row: &'a [Value], i: usize, bank: &str, n: usize) -> Result<&'a str> {
    match row.get(i) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(Error::format(format!("{bank}[{n}][{i}]: ожидается строка"))),
    }
}

/// Строка или `null` → строка (Yomitan допускает `null` в definitionTags).
fn cell_str_or_null<'a>(row: &'a [Value], i: usize, bank: &str, n: usize) -> Result<&'a str> {
    match row.get(i) {
        Some(Value::Null) => Ok(""),
        _ => cell_str(row, i, bank, n),
    }
}

fn cell_i64(row: &[Value], i: usize, bank: &str, n: usize) -> Result<i64> {
    row.get(i)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .ok_or_else(|| Error::format(format!("{bank}[{n}][{i}]: ожидается число")))
}

/// Целочисленное поле, которое допускает `null` (`sequence` статьи):
/// отсутствие значения — это `null`, а строка, логическое значение и дробное
/// число — ошибка формата, а не молчаливая потеря поля.
fn cell_i64_or_null(row: &[Value], i: usize, bank: &str, n: usize) -> Result<Option<i64>> {
    match row.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => integer_value(number)
            .map(Some)
            .ok_or_else(|| Error::format(format!("{bank}[{n}][{i}]: ожидается целое число"))),
        Some(_) => Err(Error::format(format!(
            "{bank}[{n}][{i}]: ожидается целое число"
        ))),
    }
}

/// Целое значение JSON-числа: целая запись как есть, дробная запись с нулевой
/// дробной частью (`1.0`) — тем же целым, если `f64` представляет его точно.
fn integer_value(number: &serde_json::Number) -> Option<i64> {
    if let Some(value) = number.as_i64() {
        return Some(value);
    }
    let value = number.as_f64()?;
    (value.fract() == 0.0 && value.abs() <= EXACT_INTEGER).then_some(value as i64)
}

/// Граница, до которой `f64` точно представляет целое.
const EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// Поле, которое схема Yomitan объявляет как `number` (`score` статьи):
/// сохраняется без приведения к целому, иначе допустимые дробные оценки
/// (`0.9` и `0.1`) стали бы одинаковыми и порядок чтений изменился бы.
fn cell_number(row: &[Value], i: usize, bank: &str, n: usize) -> Result<f64> {
    let value = row
        .get(i)
        .ok_or_else(|| Error::format(format!("{bank}[{n}][{i}]: ожидается число")))?;
    let Some(number) = value.as_f64() else {
        return Err(Error::format(format!("{bank}[{n}][{i}]: ожидается число")));
    };
    // JSON не допускает NaN и бесконечности, но неявное значение такого
    // числа сломало бы и сравнение, и сериализацию ответа.
    if !number.is_finite() {
        return Err(Error::format(format!(
            "{bank}[{n}][{i}]: ожидается конечное число"
        )));
    }
    Ok(number)
}

/// Сериализация оценки статьи: в JSON целое значение остаётся целым
/// (`200`, а не `200.0`), дробное печатается дробным. Так машинный ответ не
/// меняется там, где словарь задал целую оценку, и сохраняет точность там,
/// где она дробная.
pub fn serialize_score<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    if let Some(number) = value.filter(|v| v.fract() == 0.0 && v.abs() <= EXACT_INTEGER) {
        return serde_json::Number::from(number as i64).serialize(serializer);
    }
    let number = value
        .and_then(serde_json::Number::from_f64)
        .ok_or_else(|| serde::ser::Error::custom("оценка статьи — не конечное число"))?;
    number.serialize(serializer)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = text.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Цель указателя Jitendex: `href` вида `?query=…&primary_reading=…`,
/// иначе — форма `[цель, [пояснения]]` из глоссария.
fn redirect_target(glossary: &[Value]) -> Option<(String, Option<String>)> {
    fn find_href(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(href)) = map.get("href")
                    && href.starts_with("?query=")
                {
                    return Some(href.clone());
                }
                map.values().find_map(find_href)
            }
            Value::Array(items) => items.iter().find_map(find_href),
            _ => None,
        }
    }
    if let Some(href) = glossary.iter().find_map(find_href) {
        let mut target = None;
        let mut reading = None;
        for pair in href.trim_start_matches('?').split('&') {
            match pair.split_once('=') {
                Some(("query", v)) => target = percent_decode(v),
                Some(("primary_reading", v)) => reading = percent_decode(v),
                _ => {}
            }
        }
        if let Some(target) = target.filter(|t| !t.is_empty()) {
            return Some((target, reading.filter(|r| !r.is_empty())));
        }
    }
    glossary.iter().find_map(|g| match g {
        Value::Array(items) => items
            .first()
            .and_then(Value::as_str)
            .map(|t| (t.to_owned(), None)),
        _ => None,
    })
}

fn is_redirect(glossary: &[Value]) -> bool {
    fn marks(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                map.get("data")
                    .and_then(|d| d.get("content"))
                    .and_then(Value::as_str)
                    == Some("redirect-glossary")
                    || map.values().any(marks)
            }
            Value::Array(items) => items.iter().any(marks),
            _ => false,
        }
    }
    glossary.iter().any(|g| g.is_array() || marks(g))
}

struct Writer<'c> {
    term: rusqlite::Statement<'c>,
    reading: rusqlite::Statement<'c>,
    redirect: rusqlite::Statement<'c>,
    kanji: rusqlite::Statement<'c>,
    meta: rusqlite::Statement<'c>,
    tag: rusqlite::Statement<'c>,
}

/// Строит индекс в новом файле `out` (его не должно существовать).
pub fn build(
    archive: &mut Archive,
    inspection: &Inspection,
    source: &Source,
    out: &Path,
    meta: &BTreeMap<String, String>,
) -> Result<BuildStats> {
    if out.exists() {
        return Err(Error::corrupt(format!("{} уже существует", out.display())));
    }
    let mut conn = Connection::open(out)?;
    // Индекс строится в staging и активируется только после проверки и
    // fsync: журнал SQLite здесь не нужен — при сбое staging выбрасывается.
    conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA_SQL)?;
    let mut stats = BuildStats {
        banks: inspection.banks.len(),
        ..BuildStats::default()
    };
    let tx = conn.transaction()?;
    {
        let mut w = Writer {
            term: tx.prepare("INSERT INTO terms(bank,row,term,reading,definition_tags,rules,score,sequence,term_tags,kind) VALUES(?,?,?,?,?,?,?,?,?,?)")?,
            reading: tx.prepare("INSERT INTO readings(term_id,surface,surface_key,reading,match_key,origin) VALUES(?,?,?,?,?,?)")?,
            redirect: tx.prepare("INSERT INTO redirects(term_id,surface,surface_key,target,target_reading) VALUES(?,?,?,?,?)")?,
            kanji: tx.prepare("INSERT INTO kanji(bank,row,character,character_key,onyomi,kunyomi,tags,meanings,stats) VALUES(?,?,?,?,?,?,?,?,?)")?,
            meta: tx.prepare("INSERT INTO term_meta(bank,row,term,term_key,mode,reading,value,display,data) VALUES(?,?,?,?,?,?,?,?,?)")?,
            tag: tx.prepare("INSERT OR REPLACE INTO tags(name,category,sort_order,notes,score) VALUES(?,?,?,?,?)")?,
        };
        for bank in &inspection.banks {
            let rows = archive.read_bank(bank)?;
            for (n, row) in rows.iter().enumerate() {
                match bank.kind {
                    BankKind::Term => {
                        insert_term(&tx, &mut w, source, &bank.name, n, row, &mut stats)?
                    }
                    BankKind::Kanji => insert_kanji(&mut w, &bank.name, n, row, &mut stats)?,
                    BankKind::TermMeta => insert_term_meta(&mut w, &bank.name, n, row, &mut stats)?,
                    BankKind::Tag => insert_tag(&mut w, &bank.name, n, row, &mut stats)?,
                    // Метаданные кандзи разрешены, но для чтений не нужны:
                    // они остаются в сохранённом архиве.
                    BankKind::KanjiMeta => {}
                }
            }
        }
        let mut put = tx.prepare("INSERT INTO meta(key,value) VALUES(?,?)")?;
        let mut all = meta.clone();
        all.insert("corpus_schema".into(), CORPUS_SCHEMA.to_string());
        all.insert("dictionary_id".into(), source.id.into());
        all.insert(
            "stats".into(),
            serde_json::to_string(&stats).unwrap_or_default(),
        );
        for (k, v) in &all {
            put.execute(params![k, v])?;
        }
    }
    tx.commit()?;
    conn.execute_batch(INDEX_SQL)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::corrupt(format!(
            "построенный индекс не прошёл quick_check: {check}"
        )));
    }
    conn.close().map_err(|(_, e)| Error::from(e))?;
    Ok(stats)
}

fn insert_term(
    tx: &rusqlite::Transaction<'_>,
    w: &mut Writer<'_>,
    source: &Source,
    bank: &str,
    n: usize,
    row: &[Value],
    stats: &mut BuildStats,
) -> Result<()> {
    if row.len() < 8 {
        return Err(Error::format(format!(
            "{bank}[{n}]: строка банка терминов v3 должна содержать 8 полей"
        )));
    }
    let term = cell_str(row, 0, bank, n)?;
    let reading = cell_str(row, 1, bank, n)?;
    let definition_tags = cell_str_or_null(row, 2, bank, n)?;
    let rules = cell_str(row, 3, bank, n)?;
    let score = cell_number(row, 4, bank, n)?;
    let Some(glossary) = row[5].as_array() else {
        return Err(Error::format(format!(
            "{bank}[{n}][5]: глоссарий — не массив"
        )));
    };
    let sequence = cell_i64_or_null(row, 6, bank, n)?;
    let term_tags = cell_str(row, 7, bank, n)?;
    if term.is_empty() {
        return Err(Error::format(format!("{bank}[{n}]: пустое написание")));
    }
    // Решения «запись каной или нет» и ключ поиска — по канонической форме:
    // разложенный дакутэн не должен превращать кану в «сомнительную».
    let surface_key = kana::exact_key(term);

    let mut pairs: Vec<(String, &'static str)> = Vec::new();
    let mut redirect = None;
    let record_kind = match source.term_profile {
        TermProfile::Jitendex if is_redirect(glossary) => {
            redirect = redirect_target(glossary);
            kind::REDIRECT
        }
        TermProfile::JmnedictNames if reading.is_empty() && !kana::is_plain_kana(&surface_key) => {
            let listed: Option<Vec<&str>> = glossary
                .iter()
                .map(|g| {
                    g.as_str()
                        .filter(|s| kana::is_plain_kana(&kana::exact_key(s)))
                })
                .collect();
            match listed {
                Some(list) if !list.is_empty() => {
                    pairs.extend(
                        list.into_iter()
                            .map(|r| (r.to_owned(), origin::GLOSSARY_READING_LIST)),
                    );
                    kind::READING_LIST
                }
                _ => kind::NO_READING,
            }
        }
        _ if !reading.is_empty() => {
            pairs.push((reading.to_owned(), origin::READING_FIELD));
            kind::ENTRY
        }
        // Пустое чтение по Yomitan означает «совпадает с написанием» —
        // осмысленно только для записи каной.
        _ if kana::is_plain_kana(&surface_key) => {
            pairs.push((term.to_owned(), origin::KANA_TERM));
            kind::ENTRY
        }
        _ => kind::NO_READING,
    };
    w.term.execute(params![
        bank,
        n as i64,
        term,
        reading,
        definition_tags,
        rules,
        score,
        sequence,
        term_tags,
        record_kind
    ])?;
    let id = tx.last_insert_rowid();
    stats.terms += 1;
    if record_kind == kind::NO_READING {
        stats.terms_without_reading += 1;
    }
    for (r, o) in pairs {
        w.reading
            .execute(params![id, term, surface_key, r, kana::match_key(&r), o])?;
        stats.readings += 1;
    }
    if record_kind == kind::REDIRECT {
        if let Some((target, target_reading)) = redirect {
            w.redirect
                .execute(params![id, term, surface_key, target, target_reading])?;
            stats.redirects += 1;
        } else {
            stats.terms_without_reading += 1;
        }
    }
    Ok(())
}

fn insert_kanji(
    w: &mut Writer<'_>,
    bank: &str,
    n: usize,
    row: &[Value],
    stats: &mut BuildStats,
) -> Result<()> {
    if row.len() < 6 {
        return Err(Error::format(format!(
            "{bank}[{n}]: строка банка кандзи v3 должна содержать 6 полей"
        )));
    }
    let character = cell_str(row, 0, bank, n)?;
    let character_key = kana::exact_key(character);
    if character_key.chars().count() != 1 {
        return Err(Error::format(format!("{bank}[{n}]: ожидается один символ")));
    }
    let meanings = row[4]
        .as_array()
        .ok_or_else(|| Error::format(format!("{bank}[{n}][4]: ожидается массив")))?;
    if !row[5].is_object() {
        return Err(Error::format(format!("{bank}[{n}][5]: ожидается объект")));
    }
    w.kanji.execute(params![
        bank,
        n as i64,
        character,
        character_key,
        cell_str(row, 1, bank, n)?,
        cell_str(row, 2, bank, n)?,
        cell_str(row, 3, bank, n)?,
        Value::Array(meanings.clone()).to_string(),
        row[5].to_string()
    ])?;
    stats.kanji += 1;
    Ok(())
}

/// Значение частотности Yomitan: число, строка или `{value, displayValue}`.
fn frequency_value(value: &Value) -> (Option<i64>, Option<String>) {
    match value {
        Value::Number(n) => (n.as_i64(), Some(n.to_string())),
        Value::String(s) => (s.trim().parse().ok(), Some(s.clone())),
        Value::Object(map) => {
            let v = map.get("value").and_then(Value::as_i64);
            let display = map
                .get("displayValue")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| v.map(|v| v.to_string()));
            (v, display)
        }
        _ => (None, None),
    }
}

fn insert_term_meta(
    w: &mut Writer<'_>,
    bank: &str,
    n: usize,
    row: &[Value],
    stats: &mut BuildStats,
) -> Result<()> {
    if row.len() < 3 {
        return Err(Error::format(format!(
            "{bank}[{n}]: строка банка метаданных v3 должна содержать 3 поля"
        )));
    }
    let term = cell_str(row, 0, bank, n)?;
    let mode = cell_str(row, 1, bank, n)?;
    let data = &row[2];
    let (reading, value, display) = if mode == "freq" {
        match data {
            Value::Object(map) if map.contains_key("reading") => {
                let reading = map
                    .get("reading")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let (v, d) = map
                    .get("frequency")
                    .map(frequency_value)
                    .unwrap_or((None, None));
                (reading, v, d)
            }
            other => {
                let (v, d) = frequency_value(other);
                (None, v, d)
            }
        }
    } else {
        (
            data.get("reading")
                .and_then(Value::as_str)
                .map(str::to_owned),
            None,
            None,
        )
    };
    w.meta.execute(params![
        bank,
        n as i64,
        term,
        kana::exact_key(term),
        mode,
        reading,
        value,
        display,
        data.to_string()
    ])?;
    stats.term_meta += 1;
    Ok(())
}

fn insert_tag(
    w: &mut Writer<'_>,
    bank: &str,
    n: usize,
    row: &[Value],
    stats: &mut BuildStats,
) -> Result<()> {
    if row.len() < 5 {
        return Err(Error::format(format!(
            "{bank}[{n}]: строка банка тегов должна содержать 5 полей"
        )));
    }
    w.tag.execute(params![
        cell_str(row, 0, bank, n)?,
        cell_str(row, 1, bank, n)?,
        cell_i64(row, 2, bank, n)?,
        cell_str(row, 3, bank, n)?,
        cell_i64(row, 4, bank, n)?
    ])?;
    stats.tags += 1;
    Ok(())
}

/// Строка `readings` вместе с исходной статьёй.
#[derive(Debug, Clone, Serialize)]
pub struct ReadingRow {
    pub surface: String,
    pub reading: String,
    pub match_key: String,
    pub origin: String,
    pub term_id: i64,
    pub bank: String,
    pub row: i64,
    pub sequence: Option<i64>,
    /// Оценка статьи Yomitan (`number`), без усечения до целого.
    pub score: f64,
    pub definition_tags: String,
    pub term_tags: String,
    pub rules: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedirectRow {
    pub surface: String,
    pub target: String,
    pub target_reading: Option<String>,
    pub definition_tags: String,
    pub bank: String,
    pub row: i64,
    pub sequence: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KanjiRow {
    pub character: String,
    pub onyomi: String,
    pub kunyomi: String,
    pub tags: String,
    pub meanings: Value,
    pub bank: String,
    pub row: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TermMetaRow {
    pub term: String,
    pub mode: String,
    pub reading: Option<String>,
    pub value: Option<i64>,
    pub display: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagRow {
    pub name: String,
    pub category: String,
    pub notes: String,
}

/// Открытый только для чтения индекс. Методы поиска сами приводят
/// аргумент к ключу [`kana::exact_key`].
pub struct Index {
    conn: Connection,
}

fn uri(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::corrupt(format!("путь {} не UTF-8", path.display())))?;
    let mut out = String::from("file:");
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.push_str("?mode=ro&immutable=1");
    Ok(out)
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(Error::corrupt(format!(
                "нет файла индекса {}",
                path.display()
            )));
        }
        let conn = Connection::open_with_flags(
            uri(path)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let index = Index { conn };
        let schema = index.meta_value("corpus_schema")?;
        if schema.as_deref() != Some(&CORPUS_SCHEMA.to_string()) {
            return Err(Error::corrupt(format!(
                "схема корпуса {} не совпадает с поддерживаемой {CORPUS_SCHEMA}",
                schema.unwrap_or_else(|| "отсутствует".into())
            )));
        }
        Ok(index)
    }

    pub fn meta_value(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn quick_check(&self) -> Result<()> {
        let check: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check == "ok" {
            Ok(())
        } else {
            Err(Error::corrupt(format!("quick_check: {check}")))
        }
    }

    pub fn count(&self, table: &str) -> Result<u64> {
        let allowed = [
            "terms",
            "readings",
            "redirects",
            "kanji",
            "term_meta",
            "tags",
        ];
        if !allowed.contains(&table) {
            return Err(Error::corrupt(format!("неизвестная таблица {table}")));
        }
        let n: i64 = self
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn readings(&self, surface: &str) -> Result<Vec<ReadingRow>> {
        let key = kana::exact_key(surface);
        let mut st = self.conn.prepare(
            "SELECT r.surface, r.reading, r.match_key, r.origin, t.id, t.bank, t.row, t.sequence, t.score,
                    t.definition_tags, t.term_tags, t.rules
             FROM readings r JOIN terms t ON t.id = r.term_id
             WHERE r.surface_key = ? ORDER BY t.id, r.rowid",
        )?;
        let rows = st.query_map([&key], |r| {
            Ok(ReadingRow {
                surface: r.get(0)?,
                reading: r.get(1)?,
                match_key: r.get(2)?,
                origin: r.get(3)?,
                term_id: r.get(4)?,
                bank: r.get(5)?,
                row: r.get(6)?,
                sequence: r.get(7)?,
                score: r.get(8)?,
                definition_tags: r.get(9)?,
                term_tags: r.get(10)?,
                rules: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn redirects(&self, surface: &str) -> Result<Vec<RedirectRow>> {
        let key = kana::exact_key(surface);
        let mut st = self.conn.prepare(
            "SELECT d.surface, d.target, d.target_reading, t.definition_tags, t.bank, t.row, t.sequence
             FROM redirects d JOIN terms t ON t.id = d.term_id WHERE d.surface_key = ? ORDER BY t.id",
        )?;
        let rows = st.query_map([&key], |r| {
            Ok(RedirectRow {
                surface: r.get(0)?,
                target: r.get(1)?,
                target_reading: r.get(2)?,
                definition_tags: r.get(3)?,
                bank: r.get(4)?,
                row: r.get(5)?,
                sequence: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn kanji(&self, character: &str) -> Result<Vec<KanjiRow>> {
        let key = kana::exact_key(character);
        let mut st = self.conn.prepare(
            "SELECT character, onyomi, kunyomi, tags, meanings, bank, row FROM kanji WHERE character_key = ? ORDER BY id",
        )?;
        let rows = st.query_map([&key], |r| {
            let meanings: String = r.get(4)?;
            Ok(KanjiRow {
                character: r.get(0)?,
                onyomi: r.get(1)?,
                kunyomi: r.get(2)?,
                tags: r.get(3)?,
                meanings: serde_json::from_str(&meanings).unwrap_or(Value::Null),
                bank: r.get(5)?,
                row: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn term_meta(&self, term: &str) -> Result<Vec<TermMetaRow>> {
        let key = kana::exact_key(term);
        let mut st = self.conn.prepare(
            "SELECT term, mode, reading, value, display FROM term_meta WHERE term_key = ? ORDER BY id",
        )?;
        let rows = st.query_map([&key], |r| {
            Ok(TermMetaRow {
                term: r.get(0)?,
                mode: r.get(1)?,
                reading: r.get(2)?,
                value: r.get(3)?,
                display: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn tag(&self, name: &str) -> Result<Option<TagRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name, category, notes FROM tags WHERE name = ?",
                [name],
                |r| {
                    Ok(TagRow {
                        name: r.get(0)?,
                        category: r.get(1)?,
                        notes: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }
}

/// Список тегов Yomitan: разделитель — только ASCII-пробел; неразрывный
/// пробел внутри имени (`old kanji form` в Jitendex) — часть имени тега.
pub fn split_tags(text: &str) -> Vec<String> {
    text.split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redirects_are_recognised_with_their_target() {
        let glossary = json!([
            {"type": "structured-content", "content": {"tag": "div", "data": {"content": "redirect-glossary"},
              "content": ["⟶", {"tag": "a", "href": "?query=%E5%8A%B4%E5%83%8D&wildcards=off&primary_reading=%E3%82%8D%E3%81%86%E3%81%A9%E3%81%86", "content": "労働"}]}},
            ["労働", ["redirected from 勞働"]]
        ]);
        let g = glossary.as_array().unwrap();
        assert!(is_redirect(g));
        assert_eq!(
            redirect_target(g),
            Some(("労働".into(), Some("ろうどう".into())))
        );
        let plain = json!([{"type": "structured-content", "content": "school"}]);
        assert!(!is_redirect(plain.as_array().unwrap()));
    }

    #[test]
    fn tags_split_only_on_ascii_space() {
        assert_eq!(
            split_tags("★ priority\u{a0}form"),
            vec!["★".to_string(), "priority\u{a0}form".to_string()]
        );
    }

    #[test]
    fn frequency_values_of_all_yomitan_shapes() {
        assert_eq!(frequency_value(&json!(5)), (Some(5), Some("5".into())));
        assert_eq!(
            frequency_value(&json!({"value": 7, "displayValue": "7㋕"})),
            (Some(7), Some("7㋕".into()))
        );
        assert_eq!(frequency_value(&json!("12")), (Some(12), Some("12".into())));
    }

    #[test]
    fn uri_escapes_special_characters() {
        assert_eq!(
            uri(Path::new("/a b/%c.sqlite")).unwrap(),
            "file:/a%20b/%25c.sqlite?mode=ro&immutable=1"
        );
    }
}
