//! Контролируемая японская нормализация чтений для фонетического сопоставления.
//!
//! Исходный текст никогда не заменяется: нормализованные формы — отдельные
//! производные значения для сравнения. Катакана приводится к хирагане,
//! чтение разбирается на моры, а каждая мора — на фонемы словаря
//! pyopenjtalk, которым размечена основная акустическая модель. `っ` (`cl`),
//! `ん` (`N`), долгота и малые кана сохраняются как различающие единицы.
//!
//! Варианты реализации одного и того же чтения перечислены явно и узко:
//! `えい` → `e e`|`e i`, `おう` → `o o`|`o u` (так произносят и так
//! размечает pyopenjtalk), `ティ`/`ディ` → `t i`|`ty i`. Оглушение гласных
//! (`I`, `U` модели) считается той же фонемой. Ничего сверх этого — в
//! частности, кандзи в кану здесь не превращаются вовсе: преобразование
//! распознанного кандзи в «самое вероятное» чтение не доказывает чтение.

/// Ошибка разбора: чтение содержит символы вне каны.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotKana(pub char);

pub fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c)
}

pub fn is_katakana(c: char) -> bool {
    ('\u{30A1}'..='\u{30F6}').contains(&c)
}

/// Катакана → хирагана; `ー` и прочие символы сохраняются.
pub fn to_hiragana(text: &str) -> String {
    text.chars()
        .map(|c| {
            if is_katakana(c) {
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Чтение пригодно для фонетического сопоставления: только кана и `ー`.
pub fn is_phonetic_reading(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| is_hiragana(c) || is_katakana(c) || c == 'ー' || c == 'ゔ' || c == 'ヴ')
}

const SMALL: [char; 9] = ['ゃ', 'ゅ', 'ょ', 'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'ゎ'];

/// Разбивает хирагану на моры: базовая кана с присоединённой малой
/// (`きゃ`, `てぃ`, `ふぁ`), отдельные `っ`, `ん`, `ー`.
pub fn morae(hiragana: &str) -> Result<Vec<String>, NotKana> {
    let mut out: Vec<String> = Vec::new();
    for c in hiragana.chars() {
        if !(is_hiragana(c) || c == 'ー' || c == 'ゔ') {
            return Err(NotKana(c));
        }
        if SMALL.contains(&c)
            && let Some(last) = out.last_mut()
            && last.chars().count() == 1
            && !matches!(last.as_str(), "っ" | "ん" | "ー")
        {
            last.push(c);
            continue;
        }
        out.push(c.to_string());
    }
    Ok(out)
}

/// Фонемы одной моры (без учёта долготы и контекста).
fn mora_phonemes(mora: &str) -> Option<Vec<Vec<&'static str>>> {
    let one = |p: &[&'static str]| Some(vec![p.to_vec()]);
    let two = |a: &[&'static str], b: &[&'static str]| Some(vec![a.to_vec(), b.to_vec()]);
    match mora {
        "あ" | "ぁ" => one(&["a"]),
        "い" | "ぃ" => one(&["i"]),
        "う" | "ぅ" => one(&["u"]),
        "え" | "ぇ" => one(&["e"]),
        "お" | "ぉ" | "を" => one(&["o"]),
        "か" => one(&["k", "a"]),
        "き" => one(&["k", "i"]),
        "く" => one(&["k", "u"]),
        "け" => one(&["k", "e"]),
        "こ" => one(&["k", "o"]),
        "が" => one(&["g", "a"]),
        "ぎ" => one(&["g", "i"]),
        "ぐ" => one(&["g", "u"]),
        "げ" => one(&["g", "e"]),
        "ご" => one(&["g", "o"]),
        "さ" => one(&["s", "a"]),
        "し" => one(&["sh", "i"]),
        "す" => one(&["s", "u"]),
        "せ" => one(&["s", "e"]),
        "そ" => one(&["s", "o"]),
        "ざ" => one(&["z", "a"]),
        "じ" | "ぢ" => one(&["j", "i"]),
        "ず" | "づ" => one(&["z", "u"]),
        "ぜ" => one(&["z", "e"]),
        "ぞ" => one(&["z", "o"]),
        "た" => one(&["t", "a"]),
        "ち" => one(&["ch", "i"]),
        "つ" => one(&["ts", "u"]),
        "て" => one(&["t", "e"]),
        "と" => one(&["t", "o"]),
        "だ" => one(&["d", "a"]),
        "で" => one(&["d", "e"]),
        "ど" => one(&["d", "o"]),
        "な" => one(&["n", "a"]),
        "に" => one(&["n", "i"]),
        "ぬ" => one(&["n", "u"]),
        "ね" => one(&["n", "e"]),
        "の" => one(&["n", "o"]),
        "は" => one(&["h", "a"]),
        "ひ" => one(&["h", "i"]),
        "ふ" => one(&["f", "u"]),
        "へ" => one(&["h", "e"]),
        "ほ" => one(&["h", "o"]),
        "ば" => one(&["b", "a"]),
        "び" => one(&["b", "i"]),
        "ぶ" => one(&["b", "u"]),
        "べ" => one(&["b", "e"]),
        "ぼ" => one(&["b", "o"]),
        "ぱ" => one(&["p", "a"]),
        "ぴ" => one(&["p", "i"]),
        "ぷ" => one(&["p", "u"]),
        "ぺ" => one(&["p", "e"]),
        "ぽ" => one(&["p", "o"]),
        "ま" => one(&["m", "a"]),
        "み" => one(&["m", "i"]),
        "む" => one(&["m", "u"]),
        "め" => one(&["m", "e"]),
        "も" => one(&["m", "o"]),
        "や" => one(&["y", "a"]),
        "ゆ" => one(&["y", "u"]),
        "よ" => one(&["y", "o"]),
        "ら" => one(&["r", "a"]),
        "り" => one(&["r", "i"]),
        "る" => one(&["r", "u"]),
        "れ" => one(&["r", "e"]),
        "ろ" => one(&["r", "o"]),
        "わ" | "ゎ" => one(&["w", "a"]),
        "ん" => one(&["N"]),
        "っ" => one(&["cl"]),
        "ゔ" => one(&["v", "u"]),
        "きゃ" => one(&["ky", "a"]),
        "きゅ" => one(&["ky", "u"]),
        "きょ" => one(&["ky", "o"]),
        "きぇ" => one(&["ky", "e"]),
        "ぎゃ" => one(&["gy", "a"]),
        "ぎゅ" => one(&["gy", "u"]),
        "ぎょ" => one(&["gy", "o"]),
        "しゃ" => one(&["sh", "a"]),
        "しゅ" => one(&["sh", "u"]),
        "しぇ" => one(&["sh", "e"]),
        "しょ" => one(&["sh", "o"]),
        "じゃ" | "ぢゃ" => one(&["j", "a"]),
        "じゅ" | "ぢゅ" => one(&["j", "u"]),
        "じぇ" => one(&["j", "e"]),
        "じょ" | "ぢょ" => one(&["j", "o"]),
        "ちゃ" => one(&["ch", "a"]),
        "ちゅ" => one(&["ch", "u"]),
        "ちぇ" => one(&["ch", "e"]),
        "ちょ" => one(&["ch", "o"]),
        "にゃ" => one(&["ny", "a"]),
        "にゅ" => one(&["ny", "u"]),
        "にょ" => one(&["ny", "o"]),
        "にぇ" => one(&["ny", "e"]),
        "ひゃ" => one(&["hy", "a"]),
        "ひゅ" => one(&["hy", "u"]),
        "ひょ" => one(&["hy", "o"]),
        "ひぇ" => one(&["hy", "e"]),
        "びゃ" => one(&["by", "a"]),
        "びゅ" => one(&["by", "u"]),
        "びょ" => one(&["by", "o"]),
        "ぴゃ" => one(&["py", "a"]),
        "ぴゅ" => one(&["py", "u"]),
        "ぴょ" => one(&["py", "o"]),
        "みゃ" => one(&["my", "a"]),
        "みゅ" => one(&["my", "u"]),
        "みょ" => one(&["my", "o"]),
        "りゃ" => one(&["ry", "a"]),
        "りゅ" => one(&["ry", "u"]),
        "りょ" => one(&["ry", "o"]),
        "てぃ" => two(&["t", "i"], &["ty", "i"]),
        "てゅ" => one(&["ty", "u"]),
        "でぃ" => two(&["d", "i"], &["dy", "i"]),
        "でゅ" => one(&["dy", "u"]),
        "すぃ" => one(&["s", "i"]),
        "ずぃ" => one(&["z", "i"]),
        "とぅ" => one(&["t", "u"]),
        "どぅ" => one(&["d", "u"]),
        "ふぁ" => one(&["f", "a"]),
        "ふぃ" => one(&["f", "i"]),
        "ふぇ" => one(&["f", "e"]),
        "ふぉ" => one(&["f", "o"]),
        "ふゅ" => one(&["fy", "u"]),
        "うぃ" => one(&["w", "i"]),
        "うぇ" => one(&["w", "e"]),
        "うぉ" => one(&["w", "o"]),
        "いぇ" => one(&["y", "e"]),
        "つぁ" => one(&["ts", "a"]),
        "つぃ" => one(&["ts", "i"]),
        "つぇ" => one(&["ts", "e"]),
        "つぉ" => one(&["ts", "o"]),
        "くぁ" => one(&["kw", "a"]),
        "ぐぁ" => one(&["gw", "a"]),
        "ゔぁ" => one(&["v", "a"]),
        "ゔぃ" => one(&["v", "i"]),
        "ゔぇ" => one(&["v", "e"]),
        "ゔぉ" => one(&["v", "o"]),
        _ => None,
    }
}

const VOWELS: [&str; 5] = ["a", "i", "u", "e", "o"];

pub fn is_vowel(p: &str) -> bool {
    VOWELS.contains(&p)
}

/// Неразобранная мора: например, малая кана без основы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMora(pub String);

/// Все фонемные реализации чтения (без повторов, в детерминированном
/// порядке; первая — каноническая). Чтение — хирагана или катакана.
pub fn phoneme_variants(reading: &str) -> Result<Vec<Vec<String>>, UnknownMora> {
    let hira = to_hiragana(reading);
    let morae = morae(&hira).map_err(|NotKana(c)| UnknownMora(c.to_string()))?;
    let mut variants: Vec<Vec<String>> = vec![Vec::new()];
    for (i, mora) in morae.iter().enumerate() {
        let prev_vowel = |seq: &Vec<String>| seq.iter().rev().find(|p| is_vowel(p)).cloned();
        let options: Vec<Vec<String>> = if mora == "ー" {
            // Долгота повторяет последний гласный каждой реализации отдельно.
            let mut next = Vec::new();
            for seq in &variants {
                let mut seq = seq.clone();
                match prev_vowel(&seq) {
                    Some(v) => seq.push(v),
                    None => return Err(UnknownMora(mora.clone())),
                }
                next.push(seq);
            }
            variants = next;
            continue;
        } else {
            let base = mora_phonemes(mora).ok_or_else(|| UnknownMora(mora.clone()))?;
            let mut options: Vec<Vec<String>> = base
                .into_iter()
                .map(|p| p.into_iter().map(String::from).collect())
                .collect();
            // えい / おう: вторая гласная может звучать как продление.
            let previous = i.checked_sub(1).map(|j| morae[j].as_str());
            let prev_ends = |vowel: &str| {
                previous
                    .and_then(mora_phonemes)
                    .is_some_and(|p| p[0].last() == Some(&vowel))
            };
            if mora == "い" && prev_ends("e") {
                options.push(vec!["e".into()]);
            }
            if mora == "う" && prev_ends("o") {
                options.push(vec!["o".into()]);
            }
            options
        };
        let mut next = Vec::new();
        for seq in &variants {
            for option in &options {
                let mut seq = seq.clone();
                seq.extend(option.iter().cloned());
                if !next.contains(&seq) {
                    next.push(seq);
                }
            }
        }
        variants = next;
        // Предел числа реализаций: длинное чтение с десятком «えい» не должно
        // порождать экспоненту; каноническая реализация всегда первая.
        variants.truncate(64);
    }
    Ok(variants)
}

/// Фонема гипотезы модели в форме для сравнения: оглушённые `I`/`U` — те
/// же `i`/`u`. Паузы и служебные токены сравнению не подлежат (`None`).
pub fn comparable_phoneme(token: &str) -> Option<&str> {
    match token {
        "I" => Some("i"),
        "U" => Some("u"),
        "pau" | "sil" | "PAD" | "UNK" | "SOS" | "EOS" => None,
        other => Some(other),
    }
}

/// Хирагана гипотезы модели-хираганы → фонемы (все реализации). Разделители
/// слов `|` и служебные токены пропускаются; неразбираемый фрагмент — `None`.
pub fn hiragana_hypothesis_phonemes(text: &str) -> Option<Vec<Vec<String>>> {
    let cleaned: String = text
        .chars()
        .filter(|c| is_hiragana(*c) || *c == 'ー' || *c == 'ゔ')
        .collect();
    if cleaned.is_empty() {
        return Some(vec![Vec::new()]);
    }
    phoneme_variants(&cleaned).ok()
}

/// Расстояние Левенштейна по фонемам.
pub fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(x != y))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Наименьшее расстояние между любыми реализациями двух множеств.
pub fn min_distance(a: &[Vec<String>], b: &[Vec<String>]) -> usize {
    a.iter()
        .flat_map(|x| b.iter().map(move |y| edit_distance(x, y)))
        .min()
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(reading: &str) -> Vec<String> {
        phoneme_variants(reading)
            .unwrap()
            .into_iter()
            .map(|v| v.join(" "))
            .collect()
    }

    #[test]
    fn hiragana_conversion_keeps_long_mark_and_other_text() {
        assert_eq!(to_hiragana("テーブル"), "てーぶる");
        assert_eq!(to_hiragana("生"), "生");
        assert!(is_phonetic_reading("なま"));
        assert!(is_phonetic_reading("テーブル"));
        assert!(!is_phonetic_reading("生"));
        assert!(!is_phonetic_reading(""));
    }

    #[test]
    fn morae_attach_small_kana_but_keep_sokuon_and_n() {
        assert_eq!(morae("きって").unwrap(), ["き", "っ", "て"]);
        assert_eq!(morae("びょういん").unwrap(), ["びょ", "う", "い", "ん"]);
        assert_eq!(morae("てぃーしゃつ").unwrap(), ["てぃ", "ー", "しゃ", "つ"]);
        assert!(morae("生").is_err());
    }

    #[test]
    fn canonical_phonemes_and_explicit_variants() {
        assert_eq!(joined("なま"), ["n a m a"]);
        assert_eq!(joined("せい"), ["s e i", "s e e"]);
        assert_eq!(joined("しょう"), ["sh o u", "sh o o"]);
        assert_eq!(joined("きって"), ["k i cl t e"]);
        assert_eq!(joined("きて"), ["k i t e"]);
        assert_eq!(joined("テーブル"), ["t e e b u r u"]);
        assert_eq!(joined("ティー"), ["t i i", "ty i i"]);
        assert_eq!(joined("ほん"), ["h o N"]);
        assert_eq!(joined("びよういん"), ["b i y o u i N", "b i y o o i N"]);
        assert_eq!(joined("びょういん"), ["by o u i N", "by o o i N"]);
        assert!(phoneme_variants("ゃ").is_err());
        assert!(phoneme_variants("ーあ").is_err());
    }

    #[test]
    fn distinctions_that_change_words_survive() {
        let d = |a: &str, b: &str| {
            min_distance(&phoneme_variants(a).unwrap(), &phoneme_variants(b).unwrap())
        };
        assert_eq!(d("きって", "きて"), 1);
        assert_eq!(d("おばさん", "おばあさん"), 1);
        assert_eq!(d("びょういん", "びよういん"), 3);
        assert_eq!(d("ほん", "ほ"), 1);
        assert_eq!(d("せい", "せ"), 1);
        assert_eq!(d("せい", "せえ"), 0, "えい/ええ — одна реализация");
    }

    #[test]
    fn hypothesis_tokens_are_compared_with_devoicing_merged() {
        assert_eq!(comparable_phoneme("U"), Some("u"));
        assert_eq!(comparable_phoneme("pau"), None);
        let hira = hiragana_hypothesis_phonemes("いち|や||ひと|よ").unwrap();
        assert_eq!(hira[0].join(" "), "i ch i y a h i t o y o");
    }
}
