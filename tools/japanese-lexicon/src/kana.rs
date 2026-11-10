//! Классы японских символов и фонетический вывод чтения из записи каной.
//!
//! Поддерживаемая кана: хирагана U+3041–U+3096, катакана U+30A1–U+30F6,
//! знак долготы `ー` (U+30FC) не в начале слова и разделитель `・` (U+30FB),
//! который в нормализованной форме опускается. Нормализованная форма —
//! хирагана: катакана сдвигается на постоянное смещение кодовой таблицы, а
//! `ー`, малые знаки и `っ` сохраняются как есть. Посимвольная «расшифровка»
//! долгих гласных не выполняется: она зависит от слова и для ASR не нужна.
//!
//! Полуширинная катакана, знаки повтора (`ゝゞヽヾ`), отдельные знаки
//! дакутэна, комбинирующие метки после NFC и редкие буквы (`ヷヸヹヺ`, `ゟ`,
//! `ヿ`, малая катакана айнского расширения) не угадываются: для них
//! возвращается диагностируемая неопределённость. NFKC не применяется — он
//! незаметно переписал бы исходную форму.

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

/// Длина запроса в символах: защита от случайной вставки целого текста.
pub const MAX_QUERY_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Script {
    /// Только кана и знаки японской записи (возможно, с неподдерживаемыми).
    Kana,
    /// Ровно один кандзи.
    SingleKanji,
    /// Есть кандзи (возможно, вместе с каной).
    Kanji,
    /// Иное: латиница, цифры, полноширинные знаки и т. п.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KanaIssue {
    HalfwidthKana,
    IterationMark,
    SeparateVoicingMark,
    UnsupportedKanaLetter,
    LeadingProlongedMark,
    NonKanaCharacter,
}

/// Результат фонетического разбора записи каной.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KanaReading {
    /// Нормализованная хирагана для сопоставления; `None`, если вывод сомнителен.
    pub normalized: Option<String>,
    pub issues: Vec<KanaIssue>,
}

pub fn is_kanji(c: char) -> bool {
    matches!(c,
        '\u{3005}' // 々
        | '\u{3007}' // 〇
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2FA1F}'
        | '\u{30000}'..='\u{323AF}')
}

fn is_core_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c)
}

fn is_core_katakana(c: char) -> bool {
    ('\u{30A1}'..='\u{30F6}').contains(&c)
}

const PROLONGED: char = '\u{30FC}';
const MIDDLE_DOT: char = '\u{30FB}';

/// Символ относится к японской кана-записи (поддерживаемой или нет).
fn is_kana_like(c: char) -> bool {
    matches!(c,
        '\u{3041}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{31F0}'..='\u{31FF}'
        | '\u{FF66}'..='\u{FF9F}')
}

fn kana_issue(c: char) -> Option<KanaIssue> {
    match c {
        c if is_core_hiragana(c) || is_core_katakana(c) => None,
        PROLONGED | MIDDLE_DOT => None,
        '\u{FF66}'..='\u{FF9F}' => Some(KanaIssue::HalfwidthKana),
        '\u{309D}' | '\u{309E}' | '\u{30FD}' | '\u{30FE}' => Some(KanaIssue::IterationMark),
        '\u{3099}' | '\u{309A}' | '\u{309B}' | '\u{309C}' => Some(KanaIssue::SeparateVoicingMark),
        c if is_kana_like(c) => Some(KanaIssue::UnsupportedKanaLetter),
        _ => Some(KanaIssue::NonKanaCharacter),
    }
}

/// Катакана → хирагана для основного блока; прочие символы без изменений.
pub fn to_hiragana(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_core_katakana(c) {
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Ключ сопоставления чтений разных словарей: NFC + хирагана, без `・`.
/// Применяется только к строкам, уже признанным чтениями; знак долготы и
/// малые знаки сохраняются.
pub fn match_key(reading: &str) -> String {
    let nfc: String = reading.nfc().collect();
    to_hiragana(&nfc)
        .chars()
        .filter(|c| *c != MIDDLE_DOT)
        .collect()
}

/// Вся строка — поддерживаемая кана без сомнительных знаков.
pub fn is_plain_kana(text: &str) -> bool {
    !text.is_empty() && analyze_kana(text).issues.is_empty()
}

/// Фонетический разбор записи, состоящей из каны.
pub fn analyze_kana(text: &str) -> KanaReading {
    let mut issues: Vec<KanaIssue> = text.chars().filter_map(kana_issue).collect();
    if text.starts_with(PROLONGED) {
        issues.push(KanaIssue::LeadingProlongedMark);
    }
    if text.chars().all(|c| c == MIDDLE_DOT || c == PROLONGED) {
        issues.push(KanaIssue::NonKanaCharacter);
    }
    issues.sort();
    issues.dedup();
    let normalized = issues.is_empty().then(|| match_key(text));
    KanaReading { normalized, issues }
}

pub fn classify(surface: &str) -> Script {
    let kanji = surface.chars().filter(|c| is_kanji(*c)).count();
    if kanji > 0 {
        if surface.chars().count() == 1 {
            Script::SingleKanji
        } else {
            Script::Kanji
        }
    } else if surface.chars().all(is_kana_like) {
        Script::Kana
    } else {
        Script::Other
    }
}

/// Нормализованный запрос: NFC, без управляющих символов и пробелов.
pub fn normalize_query(input: &str) -> Result<String, String> {
    let surface: String = input.nfc().collect();
    if surface.is_empty() {
        return Err("пустое написание".into());
    }
    if surface.chars().count() > MAX_QUERY_CHARS {
        return Err(format!(
            "написание длиннее {MAX_QUERY_CHARS} символов: ожидается одно слово"
        ));
    }
    if surface
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\u{FEFF}')
    {
        return Err("написание содержит пробел или управляющий символ".into());
    }
    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn katakana_maps_to_hiragana_keeping_long_and_small_marks() {
        assert_eq!(to_hiragana("コーヒー"), "こーひー");
        assert_eq!(to_hiragana("ヴァイオリン"), "ゔぁいおりん");
        assert_eq!(to_hiragana("ヶ"), "ゖ");
        assert_eq!(match_key("ジョン・スミス"), "じょんすみす");
        assert_eq!(match_key("がっこう"), "がっこう");
    }

    #[test]
    fn plain_kana_is_derivable_and_dubious_kana_is_not() {
        let r = analyze_kana("きょう");
        assert_eq!(r.normalized.as_deref(), Some("きょう"));
        assert!(r.issues.is_empty());
        assert_eq!(analyze_kana("ｶﾒﾗ").issues, vec![KanaIssue::HalfwidthKana]);
        assert_eq!(
            analyze_kana("いすゞ").issues,
            vec![KanaIssue::IterationMark]
        );
        assert_eq!(
            analyze_kana("ーあ").issues,
            vec![KanaIssue::LeadingProlongedMark]
        );
        assert_eq!(
            analyze_kana("ヷ").issues,
            vec![KanaIssue::UnsupportedKanaLetter]
        );
        assert!(analyze_kana("ー").normalized.is_none());
    }

    #[test]
    fn scripts_are_classified() {
        assert_eq!(classify("生"), Script::SingleKanji);
        assert_eq!(classify("学校"), Script::Kanji);
        assert_eq!(classify("見る"), Script::Kanji);
        assert_eq!(classify("カメラ"), Script::Kana);
        assert_eq!(classify("ｶﾒﾗ"), Script::Kana);
        assert_eq!(classify("Ｔシャツ"), Script::Other);
        assert_eq!(classify("々"), Script::SingleKanji);
    }

    #[test]
    fn query_is_nfc_and_bounded() {
        // が в разложенной форме (か + U+3099) после NFC — один символ.
        assert_eq!(normalize_query("\u{304B}\u{3099}").unwrap(), "が");
        assert!(normalize_query("").is_err());
        assert!(normalize_query("学 校").is_err());
        assert!(normalize_query(&"あ".repeat(65)).is_err());
    }
}
