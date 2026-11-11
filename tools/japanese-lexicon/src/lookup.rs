//! Офлайн-поиск допустимых чтений по точному написанию.
//!
//! Классы свидетельств:
//!
//! - `lexical_entry` — пара написание/чтение из лексической статьи (Jitendex);
//! - `surface_kana` — чтение, фонетически выводимое из записи каной;
//! - `proper_name` — вариант имени собственного (JMnedict), отдельно от лексики;
//! - `kanji_reference` — он/кун-чтение символа (KANJIDIC): возможное чтение
//!   знака, а не подтверждённое самостоятельное слово.
//!
//! Частотность (JPDB) только аннотирует найденные чтения и никогда их не
//! создаёт. Указатели Jitendex (`redirects`) показываются отдельно: это
//! ссылка на другую форму, а не чтение запрошенного написания.
//!
//! Никакой результат словаря не является акустической проверкой.

use crate::catalog::{self, Role, Source};
use crate::error::{Code, Error, Result};
use crate::index::{self, Index, KanjiRow, ReadingRow, RedirectRow, TermMetaRow};
use crate::kana::{self, KanaReading, Script};
use crate::store::{self, Store};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

pub const LOOKUP_SCHEMA: &str = "japanese-lexicon.lookup/1";
pub const NOTICE: &str = "Чтения — свидетельства словарей, а не акустическая проверка аудио; частотность не является источником чтений.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    LexicalEntry,
    SurfaceKana,
    ProperName,
    KanjiReference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Нет ни одного свидетельства при наличии нужных словарей.
    NotFound,
    /// Ровно одно лексическое (или выводимое из каны) чтение.
    SingleReading,
    /// Несколько лексических чтений: выбор требует контекста.
    MultipleReadings,
    /// Только чтения символа из KANJIDIC: ограниченный статус.
    KanjiReferenceOnly,
    /// Только варианты имени собственного.
    ProperNameOnly,
    /// Только указатель на другую форму.
    RedirectOnly,
    /// Запись каной с сомнительными знаками: чтение не выводится уверенно.
    Ambiguous,
    /// Нужный лексический словарь не установлен или недоступен.
    SourceMissing,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DictionaryRef {
    pub id: String,
    pub role: Role,
    pub release: Option<String>,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Ссылка на исходную строку словаря в сохранённом архиве.
#[derive(Debug, Clone, Serialize)]
pub struct EntryRef {
    pub bank: String,
    pub row: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    /// Оценка статьи Yomitan (`number`): дробные значения сохраняются.
    #[serde(serialize_with = "crate::index::serialize_score")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub class: EvidenceClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<DictionaryRef>,
    pub surface: String,
    /// Чтение в исходной записи словаря.
    pub reading: String,
    /// Как чтение получено: `reading_field`, `kana_term`,
    /// `glossary_reading_list`, `surface`, `onyomi`, `kunyomi`.
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TagInfo>,
    /// Для кун-чтений KANJIDIC: часть, читаемая самим знаком, и окончание.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_reading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub okurigana: Option<String>,
    /// `prefix`/`suffix`/`infix` для помет `-` в KANJIDIC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affix: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrequencyNote {
    pub dictionary: DictionaryRef,
    pub rank: Option<i64>,
    pub display: Option<String>,
    /// JPDB `㋕`: частотность записи этого слова каной.
    pub kana_writing: bool,
}

/// Одно чтение одного класса с полным происхождением.
#[derive(Debug, Clone, Serialize)]
pub struct ReadingGroup {
    pub class: EvidenceClass,
    /// Чтение в исходной форме первого свидетельства.
    pub reading: String,
    /// Нормализованная хирагана для сопоставления (с ASR и между словарями).
    pub normalized: String,
    pub evidence: Vec<Evidence>,
    pub evidence_total: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub frequency: Vec<FrequencyNote>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedirectHint {
    pub dictionary: DictionaryRef,
    pub surface: String,
    pub target: String,
    pub target_reading: Option<String>,
    pub tags: Vec<TagInfo>,
    pub entry: EntryRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Used,
    NotInstalled,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub id: &'static str,
    pub role: Role,
    pub state: SourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<DictionaryRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Truncation {
    pub groups_total: usize,
    pub offset: usize,
    pub limit: usize,
    pub groups_returned: usize,
    /// Свидетельства, не показанные из-за `max_evidence`.
    pub evidence_omitted: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Query {
    pub input: String,
    /// Написание после NFC: по нему идёт точный поиск.
    pub surface: String,
    pub nfc_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LookupResult {
    pub schema: &'static str,
    pub query: Query,
    pub script: Script,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kana: Option<KanaReading>,
    pub status: Status,
    /// Все относящиеся к запросу словари установлены и доступны.
    pub complete: bool,
    pub missing_sources: Vec<&'static str>,
    /// Все различные нормализованные чтения классов `lexical_entry` и
    /// `surface_kana` (без усечения): основа статуса.
    pub distinct_readings: Vec<String>,
    pub readings: Vec<ReadingGroup>,
    pub redirects: Vec<RedirectHint>,
    /// Частотность, не привязанная ни к одному найденному чтению.
    pub frequency_unmatched: Vec<FrequencyNote>,
    pub sources: Vec<SourceReport>,
    pub truncation: Truncation,
    pub notice: &'static str,
}

/// Кандидат цели `surface + reading` для будущего сопоставления с аудио.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CandidateTarget {
    pub surface: String,
    pub reading: String,
    pub classes: Vec<EvidenceClass>,
    pub dictionaries: Vec<String>,
}

impl LookupResult {
    /// Кандидаты из показанных групп `lexical_entry` и `surface_kana`.
    /// Это подтверждённые словарём пары, а не результат прослушивания.
    pub fn candidate_targets(&self) -> Vec<CandidateTarget> {
        let mut by_key: BTreeMap<String, CandidateTarget> = BTreeMap::new();
        for group in &self.readings {
            if !matches!(
                group.class,
                EvidenceClass::LexicalEntry | EvidenceClass::SurfaceKana
            ) {
                continue;
            }
            let target =
                by_key
                    .entry(group.normalized.clone())
                    .or_insert_with(|| CandidateTarget {
                        surface: self.query.surface.clone(),
                        reading: group.normalized.clone(),
                        classes: Vec::new(),
                        dictionaries: Vec::new(),
                    });
            if !target.classes.contains(&group.class) {
                target.classes.push(group.class);
            }
            for e in &group.evidence {
                if let Some(d) = &e.dictionary
                    && !target.dictionaries.contains(&d.id)
                {
                    target.dictionaries.push(d.id.clone());
                }
            }
        }
        by_key.into_values().collect()
    }
}

/// Пагинация ответа: пропустить `offset` групп, вернуть не больше `limit`
/// групп и не больше `max_evidence` свидетельств в группе (оба ≥ 1).
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub offset: usize,
    pub limit: usize,
    pub max_evidence: usize,
}

impl Options {
    fn validate(&self) -> Result<()> {
        for (name, flag, value) in [
            ("limit", "--limit", self.limit),
            ("max_evidence", "--max-evidence", self.max_evidence),
        ] {
            if value == 0 {
                return Err(Error::new(
                    Code::InvalidInput,
                    format!("{name} ({flag}) должен быть не меньше 1"),
                ));
            }
        }
        Ok(())
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            max_evidence: 10,
        }
    }
}

struct Opened {
    source: &'static Source,
    dictionary: DictionaryRef,
    index: Index,
}

/// Открытый набор словарей для поиска. Индексы открыты как неизменяемые
/// файлы: удаление или замена версии после открытия не мешает текущему
/// поиску, а новая версия видна следующему открытию.
pub struct Lexicon {
    opened: Vec<Opened>,
    states: Vec<SourceReport>,
}

impl Lexicon {
    pub fn open(root: &Path) -> Result<Self> {
        match Store::open_existing(root)? {
            // Реестр читается и индексы открываются согласованно со сменой версий.
            Some(store) => store.read_consistent(|| Self::open_store(Some(&store))),
            None => Self::open_store(None),
        }
    }

    fn open_store(store: Option<&Store>) -> Result<Self> {
        let mut opened = Vec::new();
        let mut states = Vec::new();
        let registry = match store {
            Some(store) => Some((store, store.registry()?)),
            None => None,
        };
        for source in catalog::standard() {
            let entry = registry
                .as_ref()
                .and_then(|(_, r)| r.dictionaries.get(source.id).map(|e| e.version.clone()));
            let (Some((store, _)), Some(version)) = (&registry, entry) else {
                states.push(SourceReport {
                    id: source.id,
                    role: source.role,
                    state: SourceState::NotInstalled,
                    dictionary: None,
                    code: Some(Code::NotInstalled),
                    message: None,
                });
                continue;
            };
            match open_one(store, source, &version) {
                Ok(o) => {
                    states.push(SourceReport {
                        id: source.id,
                        role: source.role,
                        state: SourceState::Used,
                        dictionary: Some(o.dictionary.clone()),
                        code: None,
                        message: None,
                    });
                    opened.push(o);
                }
                Err(e) => states.push(SourceReport {
                    id: source.id,
                    role: source.role,
                    state: SourceState::Unavailable,
                    dictionary: None,
                    code: Some(e.code),
                    message: Some(e.message),
                }),
            }
        }
        Ok(Self { opened, states })
    }

    fn by_role(&self, role: Role) -> impl Iterator<Item = &Opened> {
        self.opened.iter().filter(move |o| o.source.role == role)
    }

    fn available(&self, role: Role) -> bool {
        self.by_role(role).next().is_some()
    }

    /// Поиск чтений. `limit` и `max_evidence` должны быть не меньше 1:
    /// нулевой предел отвергается как `invalid_input`, а не заменяется
    /// молча (группа без свидетельств потеряла бы происхождение).
    pub fn lookup(&self, input: &str, options: &Options) -> Result<LookupResult> {
        options.validate()?;
        let surface =
            kana::normalize_query(input).map_err(|m| Error::new(Code::InvalidInput, m))?;
        let script = kana::classify(&surface);
        let mut groups: Vec<ReadingGroup> = Vec::new();
        let mut redirects = Vec::new();
        let kana_analysis = (script == Script::Kana).then(|| kana::analyze_kana(&surface));

        for o in self
            .by_role(Role::Lexicon)
            .chain(self.by_role(Role::ProperNames))
        {
            let class = if o.source.role == Role::Lexicon {
                EvidenceClass::LexicalEntry
            } else {
                EvidenceClass::ProperName
            };
            for row in o.index.readings(&surface)? {
                let evidence = reading_evidence(o, class, &row)?;
                add(&mut groups, class, &row.reading, &row.match_key, evidence);
            }
            if o.source.role == Role::Lexicon {
                for row in o.index.redirects(&surface)? {
                    redirects.push(redirect_hint(o, row)?);
                }
            }
        }
        if let Some(KanaReading {
            normalized: Some(normalized),
            ..
        }) = &kana_analysis
        {
            add(
                &mut groups,
                EvidenceClass::SurfaceKana,
                &surface,
                normalized,
                Evidence {
                    class: EvidenceClass::SurfaceKana,
                    dictionary: None,
                    surface: surface.clone(),
                    reading: surface.clone(),
                    origin: "surface".into(),
                    entry: None,
                    tags: Vec::new(),
                    character_reading: None,
                    okurigana: None,
                    affix: None,
                },
            );
        }
        if script == Script::SingleKanji {
            for o in self.by_role(Role::Kanji) {
                for row in o.index.kanji(&surface)? {
                    for (reading, evidence) in kanji_evidence(o, &row)? {
                        let key = kana::match_key(&reading);
                        add(
                            &mut groups,
                            EvidenceClass::KanjiReference,
                            &reading,
                            &key,
                            evidence,
                        );
                    }
                }
            }
        }

        let mut unmatched = Vec::new();
        for o in self.by_role(Role::Frequency) {
            for row in o.index.term_meta(&surface)? {
                if row.mode != "freq" {
                    continue;
                }
                attach_frequency(&mut groups, &mut unmatched, &surface, o, &row);
            }
        }

        sort_groups(&mut groups, &self.opened);
        let mut distinct: Vec<String> = groups
            .iter()
            .filter(|g| {
                matches!(
                    g.class,
                    EvidenceClass::LexicalEntry | EvidenceClass::SurfaceKana
                )
            })
            .map(|g| g.normalized.clone())
            .collect();
        distinct.sort();
        distinct.dedup();

        let mut relevant = vec![Role::Lexicon, Role::ProperNames, Role::Frequency];
        if script == Script::SingleKanji {
            relevant.push(Role::Kanji);
        }
        let missing_sources: Vec<&'static str> = self
            .states
            .iter()
            .filter(|s| relevant.contains(&s.role) && s.state != SourceState::Used)
            .map(|s| s.id)
            .collect();
        let has = |class| groups.iter().any(|g| g.class == class);
        let status = match distinct.len() {
            1 => Status::SingleReading,
            n if n > 1 => Status::MultipleReadings,
            // Кана без вывода чтения сомнительна сама по себе: словари этого не исправят.
            _ if kana_analysis.is_some() => Status::Ambiguous,
            _ if !self.available(Role::Lexicon) => Status::SourceMissing,
            _ if has(EvidenceClass::KanjiReference) => Status::KanjiReferenceOnly,
            _ if has(EvidenceClass::ProperName) => Status::ProperNameOnly,
            _ if !redirects.is_empty() => Status::RedirectOnly,
            _ => Status::NotFound,
        };

        let total = groups.len();
        let limit = options.limit;
        let mut evidence_omitted = 0;
        let shown: Vec<ReadingGroup> = groups
            .into_iter()
            .skip(options.offset)
            .take(limit)
            .map(|mut g| {
                let keep = options.max_evidence;
                if g.evidence.len() > keep {
                    evidence_omitted += g.evidence.len() - keep;
                    g.evidence.truncate(keep);
                }
                g
            })
            .collect();
        let truncation = Truncation {
            groups_total: total,
            offset: options.offset,
            limit,
            groups_returned: shown.len(),
            evidence_omitted,
            truncated: shown.len() < total || evidence_omitted > 0,
        };
        Ok(LookupResult {
            schema: LOOKUP_SCHEMA,
            query: Query {
                input: input.to_owned(),
                nfc_changed: input != surface,
                surface,
            },
            script,
            kana: kana_analysis,
            status,
            complete: missing_sources.is_empty(),
            missing_sources,
            distinct_readings: distinct,
            readings: shown,
            redirects,
            frequency_unmatched: unmatched,
            sources: self.states.clone(),
            truncation,
            notice: NOTICE,
        })
    }
}

fn open_one(store: &Store, source: &'static Source, version: &str) -> Result<Opened> {
    let manifest = store.read_manifest(source.id, version)?;
    let path = store
        .version_dir(source.id, version)?
        .join(store::INDEX_FILE);
    let index = Index::open(&path)?;
    if index.meta_value("archive_sha256")?.as_deref() != Some(manifest.archive.sha256.as_str()) {
        return Err(Error::corrupt(
            "индекс не соответствует манифесту установленной версии",
        ));
    }
    Ok(Opened {
        source,
        dictionary: DictionaryRef {
            id: source.id.into(),
            role: source.role,
            release: manifest.artifact.release,
            revision: manifest.index_json.revision,
        },
        index,
    })
}

fn tags(o: &Opened, raw: &str) -> Result<Vec<TagInfo>> {
    index::split_tags(raw)
        .into_iter()
        .map(|name| {
            let row = o.index.tag(&name)?;
            Ok(TagInfo {
                category: row
                    .as_ref()
                    .map(|t| t.category.clone())
                    .filter(|c| !c.is_empty()),
                notes: row.map(|t| t.notes).filter(|n| !n.is_empty()),
                name,
            })
        })
        .collect()
}

fn reading_evidence(o: &Opened, class: EvidenceClass, row: &ReadingRow) -> Result<Evidence> {
    let mut all_tags = tags(o, &row.definition_tags)?;
    all_tags.extend(tags(o, &row.term_tags)?);
    Ok(Evidence {
        class,
        dictionary: Some(o.dictionary.clone()),
        surface: row.surface.clone(),
        reading: row.reading.clone(),
        origin: row.origin.clone(),
        entry: Some(EntryRef {
            bank: row.bank.clone(),
            row: row.row,
            sequence: row.sequence,
            score: Some(row.score),
        }),
        tags: all_tags,
        character_reading: None,
        okurigana: None,
        affix: None,
    })
}

fn redirect_hint(o: &Opened, row: RedirectRow) -> Result<RedirectHint> {
    Ok(RedirectHint {
        dictionary: o.dictionary.clone(),
        tags: tags(o, &row.definition_tags)?,
        surface: row.surface,
        target: row.target,
        target_reading: row.target_reading,
        entry: EntryRef {
            bank: row.bank,
            row: row.row,
            sequence: row.sequence,
            score: None,
        },
    })
}

/// Он/кун-чтения символа. Кун-чтение `い.きる`: знак читается `い`,
/// `きる` — окуригана; `なま-`/`-う` — употребление как префикс/суффикс,
/// дефисы с обеих сторон (`-ま-`) — `infix`.
fn kanji_evidence(o: &Opened, row: &KanjiRow) -> Result<Vec<(String, Evidence)>> {
    let row_tags = tags(o, &row.tags)?;
    let mut out = Vec::new();
    for (origin, field) in [("onyomi", &row.onyomi), ("kunyomi", &row.kunyomi)] {
        for raw in index::split_tags(field) {
            let affix = match (raw.starts_with('-'), raw.ends_with('-')) {
                (true, false) => Some("suffix"),
                (false, true) => Some("prefix"),
                (true, true) => Some("infix"),
                _ => None,
            };
            let bare = raw.trim_matches('-');
            let (character_reading, okurigana) = match bare.split_once('.') {
                Some((stem, tail)) => (stem.to_owned(), Some(tail.to_owned())),
                None => (bare.to_owned(), None),
            };
            let reading: String = bare.chars().filter(|c| *c != '.').collect();
            if reading.is_empty() {
                continue;
            }
            out.push((
                reading,
                Evidence {
                    class: EvidenceClass::KanjiReference,
                    dictionary: Some(o.dictionary.clone()),
                    surface: row.character.clone(),
                    reading: raw.clone(),
                    origin: origin.into(),
                    entry: Some(EntryRef {
                        bank: row.bank.clone(),
                        row: row.row,
                        sequence: None,
                        score: None,
                    }),
                    tags: row_tags.clone(),
                    character_reading: Some(character_reading),
                    okurigana,
                    affix,
                },
            ));
        }
    }
    Ok(out)
}

fn add(
    groups: &mut Vec<ReadingGroup>,
    class: EvidenceClass,
    reading: &str,
    key: &str,
    evidence: Evidence,
) {
    if let Some(g) = groups
        .iter_mut()
        .find(|g| g.class == class && g.normalized == key)
    {
        g.evidence.push(evidence);
        g.evidence_total += 1;
    } else {
        groups.push(ReadingGroup {
            class,
            reading: reading.to_owned(),
            normalized: key.to_owned(),
            evidence: vec![evidence],
            evidence_total: 1,
            frequency: Vec::new(),
        });
    }
}

fn attach_frequency(
    groups: &mut [ReadingGroup],
    unmatched: &mut Vec<FrequencyNote>,
    surface: &str,
    o: &Opened,
    row: &TermMetaRow,
) {
    let note = FrequencyNote {
        dictionary: o.dictionary.clone(),
        rank: row.value,
        kana_writing: row
            .display
            .as_deref()
            .is_some_and(|d| d.ends_with('\u{32D5}')),
        display: row.display.clone(),
    };
    // Запись без чтения относится к самому написанию (обычно кане).
    let key = kana::match_key(row.reading.as_deref().unwrap_or(surface));
    let mut attached = false;
    for g in groups.iter_mut().filter(|g| g.normalized == key) {
        g.frequency.push(note.clone());
        attached = true;
    }
    if !attached {
        unmatched.push(note);
    }
}

/// Детерминированный порядок: класс; внутри лексики — предпочтение
/// словаря, затем наибольший score статьи (это только порядок показа, а не
/// выбор единственного чтения); затем нормализованное чтение.
fn sort_groups(groups: &mut [ReadingGroup], opened: &[Opened]) {
    let priority = |g: &ReadingGroup| {
        g.evidence
            .iter()
            .filter_map(|e| e.dictionary.as_ref())
            .filter_map(|d| opened.iter().find(|o| o.source.id == d.id))
            .map(|o| o.source.priority)
            .min()
            .unwrap_or(0)
    };
    let score = |g: &ReadingGroup| {
        g.evidence
            .iter()
            .filter_map(|e| e.entry.as_ref().and_then(|x| x.score))
            .max_by(f64::total_cmp)
            .unwrap_or(0.0)
    };
    // Сортировка устойчива: чтения кандзи сохраняют порядок словаря
    // (он, затем кун), который уже детерминирован порядком строк индекса.
    groups.sort_by(|a, b| {
        let head = a.class.cmp(&b.class).then(priority(a).cmp(&priority(b)));
        if a.class == EvidenceClass::KanjiReference {
            return head;
        }
        head.then_with(|| {
            if a.class == EvidenceClass::LexicalEntry {
                // Полный порядок `f64`: дробные и отрицательные оценки
                // сравниваются без усечения и без паники на NaN.
                score(b).total_cmp(&score(a))
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| a.normalized.cmp(&b.normalized))
        .then_with(|| a.reading.cmp(&b.reading))
    });
}
