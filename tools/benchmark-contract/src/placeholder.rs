//! Подстановки в аргументах и переменных окружения: `{param.<имя>}` и
//! `{input.<id>}`. Литеральная фигурная скобка записывается удвоенной.
//!
//! Исполнитель подставляет значения до передачи команды измерительному
//! движку, поэтому синтаксис не пересекается с параметрами Hyperfine.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placeholder {
    Param(String),
    Input(String),
}

/// Все подстановки строки или причина, по которой строка некорректна.
pub fn parse(text: &str) -> Result<Vec<Placeholder>, String> {
    let mut found = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        match ch {
            '{' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                chars.next();
            }
            '{' => {
                let rest = &text[start + 1..];
                let Some(end) = rest.find('}') else {
                    return Err(format!("незакрытая подстановка в «{text}»"));
                };
                let body = &rest[..end];
                let placeholder = match body.split_once('.') {
                    Some(("param", name)) if crate::vocabulary::is_identifier(name) => {
                        Placeholder::Param(name.to_owned())
                    }
                    Some(("input", id)) if crate::vocabulary::is_identifier(id) => {
                        Placeholder::Input(id.to_owned())
                    }
                    _ => {
                        return Err(format!(
                            "неизвестная подстановка {{{body}}}; допустимы {{param.<имя>}} и {{input.<id>}}, литеральная скобка — {{{{"
                        ));
                    }
                };
                found.push(placeholder);
                for _ in 0..=body.chars().count() {
                    chars.next();
                }
            }
            '}' if chars.peek().map(|(_, c)| *c) == Some('}') => {
                chars.next();
            }
            '}' => return Err(format!("непарная закрывающая скобка в «{text}»")),
            _ => {}
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_params_inputs_and_escapes() {
        assert_eq!(
            parse("--x={param.anki_version}:{input.export}").unwrap(),
            vec![
                Placeholder::Param("anki_version".into()),
                Placeholder::Input("export".into())
            ]
        );
        assert!(parse("{{literal}}").unwrap().is_empty());
        assert!(parse("{env.HOME}").is_err());
        assert!(parse("{param.x").is_err());
        assert!(parse("a}b").is_err());
        assert!(parse("{param.Bad}").is_err());
    }
}
