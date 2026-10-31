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

/// Подставить значения в строку. `resolve` возвращает значение подстановки
/// или `None`, если значения нет: тогда подстановка — ошибка, а не пустая
/// строка. Удвоенные скобки становятся литеральными.
pub fn substitute(
    text: &str,
    resolve: impl Fn(&Placeholder) -> Option<String>,
) -> Result<String, String> {
    parse(text)?;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(['{', '}']) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        // parse уже проверил, что подстановка закрыта и известна.
        let end = tail.find('}').unwrap_or(tail.len() - 1);
        let body = &tail[1..end];
        let placeholder = match body.split_once('.') {
            Some(("param", name)) => Placeholder::Param(name.to_owned()),
            Some((_, id)) => Placeholder::Input(id.to_owned()),
            None => return Err(format!("неизвестная подстановка {{{body}}}")),
        };
        let value = resolve(&placeholder).ok_or_else(|| match &placeholder {
            Placeholder::Param(name) => format!("у параметра «{name}» нет значения"),
            Placeholder::Input(id) => format!("у входа «{id}» нет пути"),
        })?;
        out.push_str(&value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_values_and_literal_braces() {
        let resolve = |p: &Placeholder| match p {
            Placeholder::Param(name) if name == "v" => Some("26.09".to_owned()),
            Placeholder::Input(id) if id == "export" => Some("/data/export".to_owned()),
            _ => None,
        };
        assert_eq!(
            substitute("--v={param.v} {input.export} {{x}}", resolve).unwrap(),
            "--v=26.09 /data/export {x}"
        );
        assert!(substitute("{param.missing}", resolve).is_err());
        assert!(substitute("{env.HOME}", resolve).is_err());
    }

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
