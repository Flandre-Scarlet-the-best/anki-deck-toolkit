//! Синтетические Yomitan-архивы с собственными тестовыми статьями и
//! поддельный источник публикации. Реальные словари в тестах не используются.

#![allow(dead_code)]

use japanese_lexicon::error::{Code, Error, Result};
use japanese_lexicon::upstream::Transport;
use serde_json::{Value, json};
use sha1_checked::Sha1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use zip::write::SimpleFileOptions;

/// Временный каталог вне репозитория; удаляется при завершении теста.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "japanese-lexicon-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Запись архива: имя, содержимое и признак символической ссылки.
pub enum Entry {
    File(String, Vec<u8>),
    Symlink(String, String),
    Dir(String),
}

pub fn zip_bytes(entries: Vec<Entry>) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for entry in entries {
        match entry {
            Entry::File(name, bytes) => {
                writer.start_file(name, options).unwrap();
                writer.write_all(&bytes).unwrap();
            }
            Entry::Symlink(name, target) => writer.add_symlink(name, target, options).unwrap(),
            Entry::Dir(name) => writer.add_directory(name, options).unwrap(),
        }
    }
    writer.finish().unwrap().into_inner()
}

pub fn file(name: &str, value: &Value) -> Entry {
    Entry::File(name.into(), serde_json::to_vec(value).unwrap())
}

fn gloss(text: &str) -> Value {
    json!([{"type": "structured-content", "content": {"tag": "ul", "data": {"content": "glossary"}, "content": [{"tag": "li", "content": text}]}}])
}

fn redirect(target: &str, target_href: &str) -> Value {
    json!([
        {"type": "structured-content", "content": {"tag": "div", "data": {"content": "redirect-glossary"},
          "content": ["⟶", {"tag": "a", "href": target_href, "content": target}]}},
        [target, ["redirected"]]
    ])
}

/// Jitendex-подобный словарь. `extra` добавляет статьи (для новых версий).
pub fn jitendex(revision: &str, extra: &[Value]) -> Vec<u8> {
    jitendex_with_tags(
        revision,
        extra,
        json!([
            ["★", "popular", 2, "high priority entry", 2],
            [
                "priority\u{a0}form",
                "frequent",
                1,
                "high priority spelling",
                1
            ],
            [
                "old\u{a0}kanji\u{a0}form",
                "archaism",
                0,
                "old character form",
                1
            ]
        ]),
    )
}

/// Как [`jitendex`], но с собственным банком тегов: нужен там, где проверяется
/// разбор целочисленных полей тегов.
pub fn jitendex_with_tags(revision: &str, extra: &[Value], tags: Value) -> Vec<u8> {
    let mut terms = vec![
        json!(["学校", "がっこう", "★", "", 200, gloss("school"), 1, ""]),
        json!([
            "今日",
            "きょう",
            "★ priority\u{a0}form",
            "",
            200,
            gloss("today"),
            2,
            ""
        ]),
        json!([
            "今日",
            "こんにち",
            "★",
            "",
            199,
            gloss("today; these days"),
            2,
            ""
        ]),
        json!(["生", "なま", "★", "", 200, gloss("raw"), 3, ""]),
        // Вторая статья того же чтения: группа с несколькими свидетельствами.
        json!(["生", "なま", "", "", 50, gloss("draft beer"), 30, ""]),
        json!(["生", "せい", "", "", 0, gloss("life"), 4, ""]),
        json!(["カメラ", "", "★", "", 100, gloss("camera"), 5, ""]),
        // Указатель старой формы на 労働: у самого 勞働 чтения нет.
        json!([
            "勞働",
            "",
            "old\u{a0}kanji\u{a0}form",
            "",
            -101,
            redirect(
                "労働",
                "?query=%E5%8A%B4%E5%83%8D&wildcards=off&primary_reading=%E3%82%8D%E3%81%86%E3%81%A9%E3%81%86"
            ),
            -6,
            ""
        ]),
        // Статья с кандзи без чтения не даёт свидетельств.
        json!(["謎字", "", "", "", 0, gloss("mystery"), 7, ""]),
    ];
    terms.extend(extra.iter().cloned());
    zip_bytes(vec![
        file(
            "index.json",
            &json!({"title": format!("Jitendex.org [{revision}]"), "format": 3, "revision": revision,
                    "sequenced": true, "attribution": "test"}),
        ),
        file("tag_bank_1.json", &tags),
        file("term_bank_1.json", &Value::Array(terms)),
        Entry::Dir("jitendex/".into()),
        Entry::File("jitendex/LICENSE".into(), b"font licence".to_vec()),
        Entry::File("styles.css".into(), b".x{}".to_vec()),
    ])
}

pub fn jmnedict(revision: &str) -> Vec<u8> {
    zip_bytes(vec![
        file(
            "index.json",
            &json!({"title": format!("JMnedict [{revision}]"), "format": 3, "revision": revision}),
        ),
        file(
            "tag_bank_1.json",
            &json!([
                ["surname", "name", 4, "family or surname", 0],
                ["fem", "name", 4, "female given name", 0]
            ]),
        ),
        file(
            "term_bank_1.json",
            &json!([
                [
                    "田中",
                    "",
                    "surname",
                    "",
                    0,
                    ["たなか", "でんちゅう"],
                    10,
                    ""
                ],
                ["今日", "", "fem", "", 0, ["きょう"], 11, ""],
                ["東京", "トンキン", "place", "", 0, ["Tonkin"], 12, ""],
                // Одиночный кандзи, у которого есть и чтения символа в KANJIDIC,
                // и вариант имени собственного.
                ["間", "", "surname", "", 0, ["あいだ"], 13, ""]
            ]),
        ),
    ])
}

pub fn kanjidic(revision: &str) -> Vec<u8> {
    zip_bytes(vec![
        file(
            "index.json",
            &json!({"title": format!("KANJIDIC [{revision}]"), "format": 3, "revision": revision}),
        ),
        file(
            "tag_bank_1.json",
            &json!([[
                "jouyou",
                "frequent",
                -5,
                "included in list of regular-use characters",
                0
            ]]),
        ),
        file(
            "kanji_bank_1.json",
            &json!([
                ["生", "セイ ショウ", "い.きる なま- -う", "jouyou", ["life", "birth"], {"grade": "1"}],
                ["学", "ガク", "まな.ぶ", "jouyou", ["study"], {"grade": "1"}],
                // Помета с дефисами с обеих сторон: кун-чтение с `affix: infix`.
                ["間", "", "-ま-", "jouyou", ["interval"], {"grade": "2"}]
            ]),
        ),
    ])
}

pub fn jpdb(revision: &str) -> Vec<u8> {
    jpdb_rows(
        revision,
        &[
            json!(["学校", "freq", {"reading": "がっこう", "frequency": {"value": 616, "displayValue": "616"}}]),
            json!(["今日", "freq", {"reading": "きょう", "frequency": {"value": 122, "displayValue": "122"}}]),
            json!(["がっこう", "freq", {"value": 5, "displayValue": "5㋕"}]),
            // Частотность для чтения, которого нет ни в одном словаре:
            // не должна превратиться в чтение.
            json!(["学校", "freq", {"reading": "まなびや", "frequency": {"value": 9, "displayValue": "9"}}]),
        ],
    )
}

/// JPDB-подобный архив ровно с заданными строками банка метаданных.
pub fn jpdb_rows(revision: &str, rows: &[Value]) -> Vec<u8> {
    zip_bytes(vec![
        file(
            "index.json",
            &json!({"title": "JPDBv2㋕", "format": 3, "revision": revision, "frequencyMode": "rank-based"}),
        ),
        file("term_meta_bank_1.json", &Value::Array(rows.to_vec())),
    ])
}

/// Дублирует запись центрального каталога ZIP: писатель `zip` повторяющихся имён
/// не создаёт, а читатель объединяет их во внутренней карте, поэтому архив
/// собирается правкой байтов — дубликат ссылается на тот же local header.
pub fn duplicate_central_record(zip: &[u8], name: &str) -> Vec<u8> {
    let eocd = zip.len() - 22;
    let entries = u16::from_le_bytes(zip[eocd + 10..eocd + 12].try_into().unwrap());
    let cd_size = u32::from_le_bytes(zip[eocd + 12..eocd + 16].try_into().unwrap());
    let cd_offset = u32::from_le_bytes(zip[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let name_len =
        u16::from_le_bytes(zip[cd_offset + 28..cd_offset + 30].try_into().unwrap()) as usize;
    let extra_len =
        u16::from_le_bytes(zip[cd_offset + 30..cd_offset + 32].try_into().unwrap()) as usize;
    let comment_len =
        u16::from_le_bytes(zip[cd_offset + 32..cd_offset + 34].try_into().unwrap()) as usize;
    assert_eq!(
        &zip[cd_offset + 46..cd_offset + 46 + name_len],
        name.as_bytes(),
        "первая запись каталога — не {name}"
    );
    let record_len = 46 + name_len + extra_len + comment_len;
    let mut out = zip[..eocd].to_vec();
    out.extend_from_slice(&zip[cd_offset..cd_offset + record_len]);
    let mut tail = zip[eocd..].to_vec();
    // Записей на этом диске и всего: оба поля, иначе архив выглядит многодисковым.
    tail[8..10].copy_from_slice(&(entries + 1).to_le_bytes());
    tail[10..12].copy_from_slice(&(entries + 1).to_le_bytes());
    tail[12..16].copy_from_slice(&(cd_size + record_len as u32).to_le_bytes());
    out.extend_from_slice(&tail);
    out
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    sha1_checked::Digest::update(&mut h, format!("blob {}\0", bytes.len()).as_bytes());
    sha1_checked::Digest::update(&mut h, bytes);
    let result = h.try_finalize();
    assert!(!result.has_collision());
    result.hash().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone)]
pub enum Reply {
    Bytes(Vec<u8>),
    Fail(Code),
}

/// Опубликованное состояние одного источника.
#[derive(Clone)]
pub struct Published {
    pub release: String,
    pub bytes: Vec<u8>,
    /// Подменить опубликованную сумму (проверка несовпадения).
    pub digest_override: Option<String>,
}

/// Поддельный upstream: те же URL и формы ответов, что у GitHub.
pub struct FakeUpstream {
    pub published: Mutex<BTreeMap<&'static str, Published>>,
    pub overrides: Mutex<BTreeMap<String, Reply>>,
    pub downloads: AtomicUsize,
    pub requests: AtomicUsize,
}

pub const JITENDEX_API: &str =
    "https://api.github.com/repos/stephenmk/stephenmk.github.io/releases/latest";
pub const JMDICT_API: &str = "https://api.github.com/repos/yomidevs/jmdict-yomitan/releases/latest";
pub const JPDB_API: &str =
    "https://api.github.com/repos/Kuuuube/yomitan-dictionaries/contents/dictionaries?ref=main";

impl FakeUpstream {
    pub fn standard() -> Self {
        let mut published = BTreeMap::new();
        published.insert(
            "jitendex",
            Published {
                release: "T1".into(),
                bytes: jitendex("T1", &[]),
                digest_override: None,
            },
        );
        published.insert(
            "jmnedict",
            Published {
                release: "2026-01-01".into(),
                bytes: jmnedict("JMnedict.2026-01-01"),
                digest_override: None,
            },
        );
        published.insert(
            "kanjidic-english",
            Published {
                release: "2026-01-01".into(),
                bytes: kanjidic("kanjidic2.test"),
                digest_override: None,
            },
        );
        published.insert(
            "jpdb-v2.2-freq-kana",
            Published {
                release: "2024-10-13".into(),
                bytes: jpdb("JPDB v2.2 Frequency Kana 2024-10-13"),
                digest_override: None,
            },
        );
        Self {
            published: Mutex::new(published),
            overrides: Mutex::new(BTreeMap::new()),
            downloads: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
        }
    }

    pub fn publish(&self, id: &'static str, release: &str, bytes: Vec<u8>) {
        self.published.lock().unwrap().insert(
            id,
            Published {
                release: release.into(),
                bytes,
                digest_override: None,
            },
        );
    }

    pub fn tamper_digest(&self, id: &'static str, digest: &str) {
        self.published
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .digest_override = Some(digest.into());
    }

    pub fn fail(&self, url: &str, code: Code) {
        self.overrides
            .lock()
            .unwrap()
            .insert(url.into(), Reply::Fail(code));
    }

    pub fn heal(&self) {
        self.overrides.lock().unwrap().clear();
    }

    fn asset_url(owner_repo: &str, release: &str, asset: &str) -> String {
        format!("https://github.com/{owner_repo}/releases/download/{release}/{asset}")
    }

    fn release_json(&self, owner_repo: &str, ids: &[(&'static str, &str)]) -> Value {
        let published = self.published.lock().unwrap();
        let release = published[ids[0].0].release.clone();
        let assets: Vec<Value> = ids
            .iter()
            .map(|(id, asset)| {
                let p = &published[id];
                json!({
                    "name": asset,
                    "size": p.bytes.len(),
                    "digest": format!("sha256:{}", p.digest_override.clone().unwrap_or_else(|| sha256(&p.bytes))),
                    "browser_download_url": Self::asset_url(owner_repo, &p.release, asset),
                })
            })
            .collect();
        json!({"tag_name": release, "draft": false, "prerelease": false,
               "published_at": "2026-01-01T00:00:00Z", "assets": assets})
    }

    fn respond(&self, url: &str) -> Result<Vec<u8>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if let Some(reply) = self.overrides.lock().unwrap().get(url).cloned() {
            return match reply {
                Reply::Bytes(b) => Ok(b),
                Reply::Fail(code) => Err(Error::new(code, format!("{url}: симулированный отказ"))),
            };
        }
        if url == JITENDEX_API {
            return Ok(self
                .release_json(
                    "stephenmk/stephenmk.github.io",
                    &[("jitendex", "jitendex-yomitan.zip")],
                )
                .to_string()
                .into_bytes());
        }
        if url == JMDICT_API {
            return Ok(self
                .release_json(
                    "yomidevs/jmdict-yomitan",
                    &[
                        ("jmnedict", "JMnedict.zip"),
                        ("kanjidic-english", "KANJIDIC_english.zip"),
                    ],
                )
                .to_string()
                .into_bytes());
        }
        if url == JPDB_API {
            let published = self.published.lock().unwrap();
            let p = &published["jpdb-v2.2-freq-kana"];
            let name = format!("JPDB_v2.2_Frequency_Kana_{}.zip", p.release);
            return Ok(json!([
                {"type": "file", "name": "JPDB_v2.2_Frequency_2024-10-13.zip", "size": 1, "sha": "0".repeat(40),
                 "download_url": "https://raw.githubusercontent.com/Kuuuube/yomitan-dictionaries/main/dictionaries/JPDB_v2.2_Frequency_2024-10-13.zip"},
                {"type": "file", "name": "JPDB_v2.2_Frequency_Kana_Display_Only_2099-01-01.zip", "size": 1, "sha": "0".repeat(40),
                 "download_url": "https://raw.githubusercontent.com/Kuuuube/yomitan-dictionaries/main/dictionaries/JPDB_v2.2_Frequency_Kana_Display_Only_2099-01-01.zip"},
                {"type": "file", "name": "JPDB_v2.2_Frequency_Kana_2020-01-01.zip", "size": 1, "sha": "0".repeat(40),
                 "download_url": "https://raw.githubusercontent.com/Kuuuube/yomitan-dictionaries/main/dictionaries/JPDB_v2.2_Frequency_Kana_2020-01-01.zip"},
                {"type": "file", "name": name, "size": p.bytes.len(),
                 "sha": p.digest_override.clone().unwrap_or_else(|| git_blob_sha1(&p.bytes)),
                 "download_url": format!("https://raw.githubusercontent.com/Kuuuube/yomitan-dictionaries/main/dictionaries/{name}")}
            ])
            .to_string()
            .into_bytes());
        }
        let published = self.published.lock().unwrap();
        for (id, owner_repo, asset) in [
            (
                "jitendex",
                "stephenmk/stephenmk.github.io",
                "jitendex-yomitan.zip",
            ),
            ("jmnedict", "yomidevs/jmdict-yomitan", "JMnedict.zip"),
            (
                "kanjidic-english",
                "yomidevs/jmdict-yomitan",
                "KANJIDIC_english.zip",
            ),
        ] {
            let p = &published[id];
            if url == Self::asset_url(owner_repo, &p.release, asset) {
                return Ok(p.bytes.clone());
            }
        }
        let p = &published["jpdb-v2.2-freq-kana"];
        if url
            == format!(
                "https://raw.githubusercontent.com/Kuuuube/yomitan-dictionaries/main/dictionaries/JPDB_v2.2_Frequency_Kana_{}.zip",
                p.release
            )
        {
            return Ok(p.bytes.clone());
        }
        Err(Error::new(
            Code::UpstreamUnexpected,
            format!("{url}: HTTP 404"),
        ))
    }
}

impl Transport for FakeUpstream {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let bytes = self.respond(url)?;
        if bytes.len() as u64 > limit {
            return Err(Error::new(Code::LimitExceeded, "limit"));
        }
        Ok(bytes)
    }

    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        let bytes = self.respond(url)?;
        if bytes.len() as u64 > limit {
            return Err(Error::new(Code::LimitExceeded, "limit"));
        }
        sink.write_all(&bytes).map_err(|e| Error::io("sink", e))?;
        Ok(bytes.len() as u64)
    }
}

/// Снимок дерева файлов: путь → (размер, SHA-256). Для проверки «ничего не изменилось».
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if !path.ends_with("operations.lock") && !path.ends_with("active.lock") {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (bytes.len() as u64, sha256(&bytes)),
                );
            }
        }
    }
    out
}
