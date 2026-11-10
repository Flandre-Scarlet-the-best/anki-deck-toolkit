//! Поддерживаемый набор управляемых словарей.
//!
//! Каталог — единственное место, где названы источники: устойчивый ID,
//! назначение, upstream, способ обнаружения версии, ожидаемая форма архива,
//! лицензия и предпочтение при выдаче. Конкретные версии, имена релизов и
//! ссылки на скачивание здесь не хранятся: их возвращает живой upstream.

use serde::{Deserialize, Serialize};

/// Назначение словаря. Определяет, какие свидетельства он может дать.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Лексические статьи: написание ↔ чтение слова.
    Lexicon,
    /// Имена собственные: люди, места, организации.
    ProperNames,
    /// Сведения об отдельных кандзи: возможные он/кун-чтения символа.
    Kanji,
    /// Частотность: аннотация, а не источник чтений.
    Frequency,
}

/// Типы банков Yomitan-формата.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BankKind {
    Term,
    TermMeta,
    Kanji,
    KanjiMeta,
    Tag,
}

impl BankKind {
    /// Префикс имени файла банка: `term_bank_1.json` и т. д.
    pub fn prefix(self) -> &'static str {
        match self {
            BankKind::Term => "term_bank_",
            BankKind::TermMeta => "term_meta_bank_",
            BankKind::Kanji => "kanji_bank_",
            BankKind::KanjiMeta => "kanji_meta_bank_",
            BankKind::Tag => "tag_bank_",
        }
    }

    pub const ALL: [BankKind; 5] = [
        BankKind::TermMeta,
        BankKind::KanjiMeta,
        BankKind::Term,
        BankKind::Kanji,
        BankKind::Tag,
    ];
}

/// Как в банке терминов записано чтение. Семантику задаёт профиль
/// источника, а не имя архива: одна и та же JSON-форма у разных
/// конвертеров означает разное.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TermProfile {
    /// Jitendex: пустое чтение допустимо только у записи каной; статьи с
    /// `redirect-glossary` — указатели на другую форму, не чтения.
    Jitendex,
    /// JMnedict (yomitan-import): при пустом чтении глоссарий — список
    /// чтений этого написания каной; при непустом — переводы.
    JmnedictNames,
    /// Банк терминов не ожидается.
    None,
}

/// Механизм обнаружения доступного артефакта.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Discovery {
    /// Последний опубликованный (не draft и не prerelease) релиз GitHub и
    /// ассет с точным именем. GitHub публикует SHA-256 ассета (`digest`).
    GithubLatestRelease {
        owner: &'static str,
        repo: &'static str,
        asset: &'static str,
    },
    /// Файл в каталоге ветки репозитория вида `<prefix>YYYY-MM-DD<suffix>`;
    /// выбирается самая поздняя дата. GitHub публикует git blob SHA-1 файла.
    GithubDatedFile {
        owner: &'static str,
        repo: &'static str,
        branch: &'static str,
        directory: &'static str,
        prefix: &'static str,
        suffix: &'static str,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// Устойчивый внутренний идентификатор.
    pub id: &'static str,
    pub role: Role,
    pub title: &'static str,
    /// Страница проекта для человека.
    pub homepage: &'static str,
    pub discovery: Discovery,
    /// Ожидаемое начало `title` в `index.json`: защита от чужого архива.
    pub index_title_prefix: &'static str,
    pub required_banks: &'static [BankKind],
    pub allowed_banks: &'static [BankKind],
    pub term_profile: TermProfile,
    /// Совпадение `revision` из `index.json` с тегом релиза обязательно.
    pub revision_equals_release: bool,
    /// Предпочтение внутри роли: меньше — раньше в выдаче.
    pub priority: u32,
    /// Предел размера скачиваемого архива.
    pub max_archive_bytes: u64,
    pub license: &'static str,
    pub attribution: &'static str,
    /// Ограничения использования и распространения, о которых нужно знать.
    pub redistribution_note: &'static str,
}

const MIB: u64 = 1024 * 1024;

static STANDARD: [Source; 4] = [
    Source {
        id: "jitendex",
        role: Role::Lexicon,
        title: "Jitendex (Yomitan, Japanese → English)",
        homepage: "https://jitendex.org/pages/downloads.html",
        discovery: Discovery::GithubLatestRelease {
            owner: "stephenmk",
            repo: "stephenmk.github.io",
            asset: "jitendex-yomitan.zip",
        },
        index_title_prefix: "Jitendex",
        required_banks: &[BankKind::Term],
        allowed_banks: &[BankKind::Term, BankKind::Tag],
        term_profile: TermProfile::Jitendex,
        revision_equals_release: true,
        priority: 10,
        max_archive_bytes: 256 * MIB,
        license: "CC BY-SA 4.0 (Jitendex © Stephen Kraus); включает JMdict © EDRDG (CC BY-SA 4.0) и примеры Tatoeba (CC BY 2.0 FR)",
        attribution: "Jitendex.org, Stephen Kraus; JMdict — Electronic Dictionary Research and Development Group (https://www.edrdg.org/)",
        redistribution_note: "Данные скачиваются локально у пользователя и не входят в репозиторий; при распространении производных данных действуют атрибуция и ShareAlike.",
    },
    Source {
        id: "jmnedict",
        role: Role::ProperNames,
        title: "JMnedict (Yomitan)",
        homepage: "https://github.com/yomidevs/jmdict-yomitan",
        discovery: Discovery::GithubLatestRelease {
            owner: "yomidevs",
            repo: "jmdict-yomitan",
            asset: "JMnedict.zip",
        },
        index_title_prefix: "JMnedict",
        required_banks: &[BankKind::Term],
        allowed_banks: &[BankKind::Term, BankKind::Tag],
        term_profile: TermProfile::JmnedictNames,
        revision_equals_release: false,
        priority: 20,
        max_archive_bytes: 128 * MIB,
        license: "EDRDG General Dictionary Licence (CC BY-SA 4.0); конвертация yomitan-import",
        attribution: "JMnedict — Electronic Dictionary Research and Development Group (https://www.edrdg.org/edrdg/licence.html)",
        redistribution_note: "Данные скачиваются локально у пользователя и не входят в репозиторий; EDRDG требует атрибуцию и ShareAlike.",
    },
    Source {
        id: "kanjidic-english",
        role: Role::Kanji,
        title: "KANJIDIC English (Yomitan)",
        homepage: "https://github.com/yomidevs/jmdict-yomitan",
        discovery: Discovery::GithubLatestRelease {
            owner: "yomidevs",
            repo: "jmdict-yomitan",
            asset: "KANJIDIC_english.zip",
        },
        index_title_prefix: "KANJIDIC",
        required_banks: &[BankKind::Kanji],
        allowed_banks: &[BankKind::Kanji, BankKind::KanjiMeta, BankKind::Tag],
        term_profile: TermProfile::None,
        revision_equals_release: false,
        priority: 30,
        max_archive_bytes: 64 * MIB,
        license: "EDRDG General Dictionary Licence (CC BY-SA 4.0) для KANJIDIC2; для отдельных полей (например, SKIP) действуют особые условия EDRDG",
        attribution: "KANJIDIC2 — Electronic Dictionary Research and Development Group (https://www.edrdg.org/edrdg/licence.html)",
        redistribution_note: "Данные скачиваются локально у пользователя и не входят в репозиторий; особые условия KANJIDIC — на странице лицензии EDRDG.",
    },
    Source {
        id: "jpdb-v2.2-freq-kana",
        role: Role::Frequency,
        title: "JPDB v2.2 Frequency Kana (Yomitan)",
        homepage: "https://github.com/Kuuuube/yomitan-dictionaries",
        discovery: Discovery::GithubDatedFile {
            owner: "Kuuuube",
            repo: "yomitan-dictionaries",
            branch: "main",
            directory: "dictionaries",
            prefix: "JPDB_v2.2_Frequency_Kana_",
            suffix: ".zip",
        },
        index_title_prefix: "JPDBv2",
        required_banks: &[BankKind::TermMeta],
        allowed_banks: &[BankKind::TermMeta, BankKind::Tag],
        term_profile: TermProfile::None,
        revision_equals_release: false,
        priority: 40,
        max_archive_bytes: 64 * MIB,
        license: "Код репозитория Kuuuube/yomitan-dictionaries — MIT; отдельная лицензия частотных данных jpdb.io не указана",
        attribution: "JPDB v2.2 Frequency Kana — jpdb.io, Kuuube, Gecko",
        redistribution_note: "Права на повторное распространение частотных данных jpdb.io не установлены: данные только скачиваются локально и не распространяются.",
    },
];

/// Стандартный профиль: все поддерживаемые словари в порядке предпочтения.
pub fn standard() -> &'static [Source] {
    &STANDARD
}

pub fn find(id: &str) -> Option<&'static Source> {
    STANDARD.iter().find(|s| s.id == id)
}

pub fn ids() -> Vec<&'static str> {
    STANDARD.iter().map(|s| s.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_roles_cover_the_profile() {
        let mut ids = ids();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), STANDARD.len());
        for role in [
            Role::Lexicon,
            Role::ProperNames,
            Role::Kanji,
            Role::Frequency,
        ] {
            assert_eq!(STANDARD.iter().filter(|s| s.role == role).count(), 1);
        }
        for s in standard() {
            for bank in s.required_banks {
                assert!(s.allowed_banks.contains(bank), "{}", s.id);
            }
        }
    }
}
