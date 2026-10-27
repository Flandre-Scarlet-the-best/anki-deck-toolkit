//! Указатели на значения машинного отчёта модуля (JSON).
//!
//! Синтаксис — JSON Pointer (RFC 6901: `/a/b/0`, `~0` — «~», `~1` — «/») с
//! одним расширением: сегмент `*` выбирает все элементы массива или все
//! значения объекта. Указатель с `*` выбирает несколько значений; что с ними
//! делать (например, суммировать счётчик), решает декларация, а не отчёт.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Any,
}

/// Разобрать указатель или вернуть причину, по которой он некорректен.
pub fn parse(text: &str) -> Result<Vec<Segment>, String> {
    let Some(rest) = text.strip_prefix('/') else {
        return Err(format!(
            "указатель «{text}» начинается с «/» (JSON Pointer, RFC 6901)"
        ));
    };
    rest.split('/')
        .map(|raw| {
            if raw == "*" {
                return Ok(Segment::Any);
            }
            let mut key = String::with_capacity(raw.len());
            let mut chars = raw.chars();
            while let Some(ch) = chars.next() {
                if ch != '~' {
                    key.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('0') => key.push('~'),
                    Some('1') => key.push('/'),
                    _ => {
                        return Err(format!(
                            "в указателе «{text}» после «~» допустимы только 0 и 1"
                        ));
                    }
                }
            }
            Ok(Segment::Key(key))
        })
        .collect()
}

/// Указатель выбирает несколько значений.
pub fn is_multiple(segments: &[Segment]) -> bool {
    segments.contains(&Segment::Any)
}

/// Значения по указателю. `None` — путь отсутствует хотя бы в одной ветви:
/// неполный отчёт не превращается молча в меньшую сумму.
pub fn select<'a>(value: &'a Value, segments: &[Segment]) -> Option<Vec<&'a Value>> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(vec![value]);
    };
    match first {
        Segment::Key(key) => {
            let child = match value {
                Value::Object(map) => map.get(key),
                Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => None,
            }?;
            select(child, rest)
        }
        Segment::Any => {
            let children: Vec<&Value> = match value {
                Value::Array(items) => items.iter().collect(),
                Value::Object(map) => map.values().collect(),
                _ => return None,
            };
            let mut found = Vec::new();
            for child in children {
                found.extend(select(child, rest)?);
            }
            Some(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_escapes_and_wildcards() {
        assert_eq!(
            parse("/a~1b/~0c/*").unwrap(),
            vec![
                Segment::Key("a/b".into()),
                Segment::Key("~c".into()),
                Segment::Any
            ]
        );
        assert!(parse("a/b").is_err());
        assert!(parse("/a~2").is_err());
    }

    #[test]
    fn selects_every_branch_or_nothing() {
        let report = json!({"scenarios": [{"n": 2}, {"n": 3}], "artifact": {"size": 7}});
        let all = select(&report, &parse("/scenarios/*/n").unwrap()).unwrap();
        assert_eq!(all, vec![&json!(2), &json!(3)]);
        assert_eq!(
            select(&report, &parse("/artifact/size").unwrap()).unwrap(),
            vec![&json!(7)]
        );
        assert_eq!(
            select(&report, &parse("/scenarios/1/n").unwrap()).unwrap(),
            vec![&json!(3)]
        );
        let partial = json!({"scenarios": [{"n": 2}, {}]});
        assert!(select(&partial, &parse("/scenarios/*/n").unwrap()).is_none());
    }
}
