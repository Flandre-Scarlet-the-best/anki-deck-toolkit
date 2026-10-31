//! Bounded-представление текстовых значений для диагностики.
//!
//! Human output, evidence QA-замечаний и отчёты `edit`/`review-check` обязаны
//! печатать значения полей ограниченной длины: значения содержат HTML, а сумма
//! по тысячам заметок несопоставима с пользой для читателя. Единственная
//! реализация этой нормализации живёт здесь, чтобы представления одного и того
//! же значения не разъезжались между командами.

/// Предел длины выборки значения в отчёте.
pub const VALUE_SAMPLE_CHARS: usize = 120;

/// Ограничивает выборку значения и убирает переводы строк.
///
/// Переводы строк и управляющие символы экранируются: выборка должна занимать
/// одну строку вывода независимо от содержимого поля.
#[must_use]
pub fn bounded_sample(text: &str) -> String {
    let mut result = String::new();
    for (position, character) in text.chars().enumerate() {
        if position == VALUE_SAMPLE_CHARS {
            result.push('…');
            break;
        }
        match character {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if (character as u32) < 0x20 => result.push('·'),
            character => result.push(character),
        }
    }
    result
}

/// Экранирует whitespace-последовательность в `\u{...}`-форме.
///
/// Нужна там, где пробелы — предмет диагностики: «поле начинается с пробела»
/// должно быть видно в выводе, а не выглядеть как обычное значение.
#[must_use]
pub fn escape_whitespace(text: &str, limit: usize) -> String {
    let mut result = String::new();
    for (position, character) in text.chars().enumerate() {
        if position == limit {
            result.push('…');
            break;
        }
        if character.is_whitespace() {
            result.push_str(&character.escape_unicode().to_string());
        } else {
            result.push(character);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_values_are_kept_verbatim() {
        assert_eq!(bounded_sample("короткое"), "короткое");
        assert_eq!(bounded_sample("偶然"), "偶然");
        assert_eq!(bounded_sample(""), "");
    }

    #[test]
    fn control_characters_stay_on_one_line() {
        assert_eq!(bounded_sample("а\nб"), "а\\nб");
        assert_eq!(bounded_sample("а\rб"), "а\\rб");
        assert_eq!(bounded_sample("а\tб"), "а\\tб");
        assert_eq!(bounded_sample("\u{1}"), "·");
    }

    #[test]
    fn long_values_are_truncated_with_ellipsis() {
        let long = "я".repeat(VALUE_SAMPLE_CHARS + 10);
        let truncated = bounded_sample(&long);
        assert_eq!(truncated.chars().count(), VALUE_SAMPLE_CHARS + 1);
        assert!(truncated.ends_with('…'));

        let exact = "я".repeat(VALUE_SAMPLE_CHARS);
        assert_eq!(bounded_sample(&exact), exact);
        assert!(!bounded_sample(&exact).ends_with('…'));
    }

    #[test]
    fn whitespace_escaping_makes_spaces_visible() {
        assert_eq!(escape_whitespace(" \t", 8), "\\u{20}\\u{9}");
        assert_eq!(escape_whitespace("\u{a0}", 8), "\\u{a0}");
        assert_eq!(escape_whitespace("аб", 8), "аб");
        assert_eq!(escape_whitespace("     ", 2), "\\u{20}\\u{20}…");
    }
}
