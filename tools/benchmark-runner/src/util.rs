//! Время UTC, хэши и запись файлов без частичного результата.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_rfc3339(unix: f64) -> String {
    let ((y, mo, d), (h, mi, s)) = utc_parts(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Компактная UTC-метка для имён каталогов прогона.
pub fn utc_compact(unix: f64) -> String {
    let ((y, mo, d), (h, mi, s)) = utc_parts(unix);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn utc_parts(unix: f64) -> ((i64, u32, u32), (u32, u32, u32)) {
    let secs = unix.floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    // civil_from_days (Howard Hinnant) для пролептического григорианского календаря.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    ((year, month, day), (rem / 3600, rem % 3600 / 60, rem % 60))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Короткий случайный суффикс для имён каталогов прогона.
pub fn random_suffix() -> String {
    let mut bytes = [0u8; 3];
    if fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        bytes = [
            (nanos >> 16) as u8,
            (nanos >> 8) as u8,
            std::process::id() as u8,
        ];
    }
    hex(&bytes)
}

/// Записать новый файл целиком или не записать вовсе. Существующий файл не
/// перезаписывается: жёсткая ссылка на готовый временный файл атомарно
/// создаёт имя и отказывает, если оно занято.
pub fn write_new_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", random_suffix()));
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
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Экранирование одного аргумента для разбора в стиле POSIX shell, которым
/// Hyperfine 2.0 делит строку команды при `--shell=none`.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@+,%".contains(&b));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(utc_rfc3339(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(1_793_102_400.5), "2026-10-27T12:00:00Z");
        assert_eq!(utc_compact(1_793_102_400.0), "20261027T120000Z");
    }

    #[test]
    fn quotes_like_posix_shell() {
        assert_eq!(shell_quote("--json"), "--json");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn new_file_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("bench-runner-util-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("result.json");
        write_new_atomic(&path, b"first").unwrap();
        let error = write_new_atomic(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
