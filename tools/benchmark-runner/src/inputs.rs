//! Идентичность и объём входов сценария.
//!
//! `content-tree-sha256` исполнителя — SHA-256 упорядоченного перечня
//! записей дерева: вид (файл или ссылка), признак исполняемости, путь
//! относительно корня входа, SHA-256 содержимого или цель ссылки. Ссылки не
//! разыменовываются, каталоги выражены путями файлов. Для `repo-tree` перечень
//! — файлы, которые Git отслеживает или не игнорирует.

use crate::util::{hex, sha256_hex};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Записи дерева в каноническом порядке.
fn walk(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Ok(vec![PathBuf::new()]);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    Ok(found)
}

fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Путь записи; пустой относительный путь означает сам вход-файл
/// (`root.join("")` дал бы `файл/`).
fn entry_path(root: &Path, relative: &Path) -> PathBuf {
    if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    }
}

fn tree_hash(root: &Path, entries: &[PathBuf]) -> io::Result<String> {
    let mut hasher = Sha256::new();
    for relative in entries {
        let path = entry_path(root, relative);
        let meta = fs::symlink_metadata(&path)?;
        let (kind, content) = if meta.file_type().is_symlink() {
            (b"L", fs::read_link(&path)?.as_os_str().as_bytes().to_vec())
        } else if meta.is_file() {
            (b"F", file_digest(&path)?.into_bytes())
        } else {
            continue;
        };
        let exec = if meta.permissions().mode() & 0o111 != 0 {
            b"x"
        } else {
            b"-"
        };
        for part in [
            &kind[..],
            &exec[..],
            relative.as_os_str().as_bytes(),
            &content,
        ] {
            hasher.update(part);
            hasher.update([0u8]);
        }
        hasher.update(b"\n");
    }
    Ok(hex(&hasher.finalize()))
}

/// Идентичность дерева или одного файла по пути.
pub fn content_tree_sha256(root: &Path) -> io::Result<String> {
    tree_hash(root, &walk(root)?)
}

pub fn file_sha256(path: &Path) -> io::Result<String> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("ожидается файл"));
    }
    file_digest(path)
}

/// Записи дерева рабочей копии: отслеживаемые и неигнорируемые файлы Git.
pub fn repo_tree_entries(repo: &Path) -> Result<Vec<PathBuf>, String> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git не запускается: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut entries: Vec<PathBuf> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        // Удалённый, но ещё отслеживаемый файл в дерево не входит.
        .filter(|p| fs::symlink_metadata(repo.join(p)).is_ok())
        .collect();
    entries.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    entries.dedup();
    Ok(entries)
}

pub fn repo_tree_sha256(repo: &Path) -> Result<String, String> {
    tree_hash(repo, &repo_tree_entries(repo)?)
        .map_err(|e| format!("дерево рабочей копии не читается: {e}"))
}

/// Состояние Git рабочей копии: коммит и наличие незафиксированных изменений.
pub fn git_state(repo: &Path) -> (Option<String>, Option<bool>) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let commit = run(&["rev-parse", "HEAD"])
        .map(|s| s.trim().to_owned())
        .filter(|s| s.len() == 40 || s.len() == 64);
    let dirty = commit
        .as_ref()
        .and_then(|_| run(&["status", "--porcelain", "--untracked-files=normal"]))
        .map(|s| !s.trim().is_empty());
    (commit, dirty)
}

/// Записи, по которым считается объём: тот же перечень, что и у идентичности
/// входа, чтобы объём `repo-tree` не зависел от игнорируемых Git данных.
pub fn size_entries(root: &Path, repo_tree: bool) -> Result<Vec<PathBuf>, String> {
    if repo_tree {
        repo_tree_entries(root)
    } else {
        walk(root).map_err(|e| e.to_string())
    }
}

pub fn tree_bytes(root: &Path, entries: &[PathBuf]) -> io::Result<u64> {
    let mut total = 0;
    for relative in entries {
        let meta = fs::symlink_metadata(entry_path(root, relative))?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}

pub fn tree_files(root: &Path, entries: &[PathBuf]) -> io::Result<u64> {
    let mut total = 0;
    for relative in entries {
        if fs::symlink_metadata(entry_path(root, relative))?.is_file() {
            total += 1;
        }
    }
    Ok(total)
}

/// Кажущийся размер дерева в байтах, как `du -sb` из методики: файлы и
/// ссылки без перехода по ним, каталоги не считаются; жёсткие ссылки
/// учитываются один раз.
pub fn apparent_bytes(root: &Path) -> io::Result<u64> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;
    let mut seen = BTreeSet::new();
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_dir() && seen.insert((meta.dev(), meta.ino())) {
            total += meta.len();
        }
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(total)
}

/// Безопасный путь внутри корня входа (без выхода за его пределы).
pub fn inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(root.join(candidate))
}

/// Короткий отпечаток для журнала исполнителя, не раскрывающий путь.
pub fn path_fingerprint(path: &Path) -> String {
    sha256_hex(path.as_os_str().as_bytes())[..12].to_owned()
}

/// Вывод пробы — JSON; размер ограничен, чтобы проба не исчерпала память.
pub const PROBE_OUTPUT_LIMIT: usize = 64 << 20;
pub const PROBE_GRACE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "benchmark-runner-inputs-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn size_of_single_file_input() {
        let dir = scratch("file");
        let file = dir.join("deck.json");
        fs::write(&file, b"12345").unwrap();
        let entries = size_entries(&file, false).unwrap();
        assert_eq!(tree_bytes(&file, &entries).unwrap(), 5);
        assert_eq!(tree_files(&file, &entries).unwrap(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apparent_bytes_matches_du_sb() {
        let dir = scratch("apparent");
        fs::write(dir.join("one"), b"x").unwrap();
        fs::hard_link(dir.join("one"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink("abcdefgh", dir.join("symlink")).unwrap();
        fs::create_dir(dir.join("empty")).unwrap();
        assert_eq!(apparent_bytes(&dir).unwrap(), 1 + 8);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repo_tree_size_skips_git_ignored_data() {
        let dir = scratch("repo");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .map(|o| o.status.success());
            status.unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            eprintln!("ПРОПУСК: git недоступен");
            return;
        }
        fs::write(dir.join(".gitignore"), b"ignored/\n").unwrap();
        fs::write(dir.join("kept.txt"), b"abc").unwrap();
        fs::create_dir_all(dir.join("ignored")).unwrap();
        fs::write(dir.join("ignored/big.bin"), vec![0u8; 4096]).unwrap();
        let entries = size_entries(&dir, true).unwrap();
        assert_eq!(tree_files(&dir, &entries).unwrap(), 2);
        assert_eq!(tree_bytes(&dir, &entries).unwrap(), 3 + 9);
        fs::remove_dir_all(&dir).unwrap();
    }
}
