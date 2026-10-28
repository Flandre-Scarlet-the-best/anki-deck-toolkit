//! Неизменяемый архив исходных JSON-результатов — локальный источник истины.
//!
//! Идентификатор записи — SHA-256 канонической формы документа (ключи
//! отсортированы, без пробелов), поэтому один и тот же результат с другим
//! форматированием не создаёт второй записи. Файл хранится побайтно так, как
//! был принят, по пути `results/<первые два символа>/<id>.json`. Имя занимается
//! жёсткой ссылкой на полностью записанный временный файл: запись атомарна, а
//! существующий файл никогда не перезаписывается.

use benchmark_contract::catalog::Catalog;
use benchmark_contract::diagnostic::{Diagnostic, Severity};
use benchmark_contract::result::{self, BenchmarkResult};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const RESULTS_DIR: &str = "results";
/// Повреждённые копии, заменённые проверенными при `restore`.
pub const CORRUPT_DIR: &str = "corrupt";
pub const ID_LEN: usize = 64;

/// SHA-256 канонической формы JSON-документа.
pub fn content_id(text: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("документ не является JSON: {e}"))?;
    let mut canonical = Vec::new();
    write_canonical(&value, &mut canonical).map_err(|e| e.to_string())?;
    Ok(hex(&Sha256::digest(&canonical)))
}

/// Компактный JSON с ключами объектов по возрастанию. Порядок задаётся явно,
/// а не типом `serde_json::Map`: возможность `preserve_order`, включённая
/// любой зависимостью сборки, иначе сменила бы ID уже сохранённых записей.
fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) -> serde_json::Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (n, key) in keys.into_iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        other => serde_json::to_writer(&mut *out, other)?,
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_id(text: &str) -> bool {
    text.len() == ID_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Путь записи относительно корня хранилища.
pub fn relative_path(id: &str) -> PathBuf {
    Path::new(RESULTS_DIR)
        .join(&id[..2])
        .join(format!("{id}.json"))
}

/// Проверенный документ и его идентификатор.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub id: String,
    pub result: BenchmarkResult,
    pub warnings: Vec<Diagnostic>,
}

/// Проверить документ контрактом. С каталогом — также соответствие
/// декларации (историческая ревизия допустима с предупреждением).
pub fn accept(
    text: &str,
    name: &str,
    catalog: Option<&Catalog>,
) -> Result<Accepted, Vec<Diagnostic>> {
    let checked = result::check(text, name, catalog);
    let Some(result) = checked.result else {
        return Err(checked.diagnostics);
    };
    let id = content_id(text).map_err(|message| {
        vec![Diagnostic {
            severity: Severity::Error,
            code: "json_syntax",
            file: name.to_owned(),
            scenario: None,
            field: None,
            message,
        }]
    })?;
    Ok(Accepted {
        id,
        result,
        warnings: checked
            .diagnostics
            .into_iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect(),
    })
}

/// Целостность архивной копии: имя совпадает с содержимым, документ проходит
/// контракт. Сверка с каталогом выполнена при приёме; здесь она не
/// повторяется, чтобы правка каталога не делала архив «повреждённым».
pub fn verify(id: &str, text: &str) -> Result<BenchmarkResult, String> {
    let actual = content_id(text)?;
    if actual != id {
        return Err(format!(
            "содержимое не совпадает с идентификатором: SHA-256 канонической формы {actual}"
        ));
    }
    let checked = result::check(text, &format!("{id}.json"), None);
    checked.result.ok_or_else(|| {
        let errors: Vec<String> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| format!("[{}] {}", d.code, d.message))
            .collect();
        format!("документ не проходит контракт: {}", errors.join("; "))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Created,
    Existing,
}

/// Записать копию, если её ещё нет. Существующая копия не перезаписывается:
/// совпадающая — повторный приём, повреждённая — ошибка без записи.
pub fn put(root: &Path, id: &str, bytes: &[u8]) -> Result<Put, String> {
    let path = root.join(relative_path(id));
    if path.exists() {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("архивная копия {id} не читается: {e}"))?;
        return match content_id(&text) {
            Ok(actual) if actual == id => Ok(Put::Existing),
            _ => Err(format!(
                "архивная копия {id} повреждена и не перезаписывается; проверьте хранилище командой verify и восстановите её из резервной копии командой restore"
            )),
        };
    }
    let dir = path.parent().expect("путь записи содержит каталог");
    fs::create_dir_all(dir).map_err(|e| format!("каталог архива не создан: {e}"))?;
    match write_new(&root.join(crate::store::TMP_DIR), &path, bytes) {
        Ok(()) => Ok(Put::Created),
        // Параллельный приём того же документа успел первым.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Put::Existing),
        Err(e) => Err(format!("архивная копия {id} не записана: {e}")),
    }
}

/// Заменить повреждённую архивную копию проверенной: прежний файл
/// откладывается в `corrupt/` вне `results/` и не удаляется; при неудаче записи
/// возвращается на место. Исправную копию не заменяет.
pub fn replace_damaged(root: &Path, id: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = root.join(relative_path(id));
    if let Ok(text) = fs::read_to_string(&path)
        && content_id(&text).is_ok_and(|actual| actual == id)
    {
        return Err(format!("архивная копия {id} исправна и не заменяется"));
    }
    let aside_dir = root.join(CORRUPT_DIR);
    fs::create_dir_all(&aside_dir).map_err(|e| format!("каталог {CORRUPT_DIR} не создан: {e}"))?;
    let aside = aside_dir.join(format!("{id}.json.{}", nonce()));
    fs::rename(&path, &aside).map_err(|e| format!("повреждённая копия {id} не отложена: {e}"))?;
    match write_new(&root.join(crate::store::TMP_DIR), &path, bytes) {
        Ok(()) => Ok(aside),
        Err(e) => {
            let _ = fs::rename(&aside, &path);
            Err(format!("архивная копия {id} не записана: {e}"))
        }
    }
}

/// Полностью записанный и сброшенный на диск временный файл получает имя
/// жёсткой ссылкой; занятое имя — отказ, а не перезапись.
pub fn write_new(tmp_dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(tmp_dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = tmp_dir.join(format!("{name}.{}.{}.tmp", std::process::id(), nonce()));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::hard_link(&tmp, path)
    })();
    let _ = fs::remove_file(&tmp);
    written?;
    if let Some(dir) = path.parent()
        && let Ok(dir) = fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// Запись архива, найденная обходом каталога.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
}

/// Обход архива: записи с правильными именами и посторонние файлы.
pub fn scan(root: &Path) -> io::Result<(Vec<Entry>, Vec<PathBuf>)> {
    let mut entries = Vec::new();
    let mut strays = Vec::new();
    let results = root.join(RESULTS_DIR);
    if !results.exists() {
        return Ok((entries, strays));
    }
    for shard in fs::read_dir(&results)? {
        let shard = shard?.path();
        if !shard.is_dir() {
            strays.push(shard);
            continue;
        }
        let prefix = shard
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for file in fs::read_dir(&shard)? {
            let path = file?.path();
            let id = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|id| is_id(id) && id.starts_with(&prefix))
                .map(str::to_owned);
            match id {
                Some(id) if path.is_file() => entries.push(Entry { id, path }),
                _ => strays.push(path),
            }
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    strays.sort();
    Ok((entries, strays))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_id_ignores_key_order_and_formatting() {
        let a = content_id(r#"{"b": 1, "a": {"y": [2, {"q": 1, "p": 0}], "x": "s"}}"#).unwrap();
        let b = content_id(r#"{"a":{"x":"s","y":[2,{"p":0,"q":1}]},"b":1}"#).unwrap();
        assert_eq!(a, b);
        // Порядок элементов массива — часть содержимого.
        assert_ne!(content_id("[1, 2]").unwrap(), content_id("[2, 1]").unwrap());
        // Каноническая форма — компактный JSON с ключами по возрастанию.
        assert_eq!(
            content_id(r#"{"b": 1, "a": [true, null, "é"]}"#).unwrap(),
            hex(&Sha256::digest(r#"{"a":[true,null,"é"],"b":1}"#.as_bytes()))
        );
    }
}
