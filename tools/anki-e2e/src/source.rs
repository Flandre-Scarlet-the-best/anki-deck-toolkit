//! Снимок исходников toolkit для сборки внутри Docker и его происхождение.
//!
//! В контекст сборки попадают только файлы, которые Git считает частью рабочей
//! копии (отслеживаемые и неигнорируемые новые). Пользовательские области
//! `decks/` и `.asset-store/` игнорируются Git и дополнительно отвергаются здесь.
//! Рабочий каталог стенда (`--work-dir`) с кэшами, снимками и отчётами в снимок
//! не входит, даже если Git его не игнорирует.

use crate::util::{Result, sha256_hex};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub commit: String,
    pub dirty: bool,
    pub dirty_paths: Vec<String>,
    pub tree_sha256: String,
    pub files: usize,
    pub msrv: String,
    /// Путь рабочего каталога стенда внутри репозитория, исключённый из снимка.
    pub excluded_work_dir: Option<String>,
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

pub fn repository_root(start: &Path) -> Result<PathBuf> {
    let out = git(start, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
}

fn private_area(path: &str) -> bool {
    path.starts_with("decks/")
        || path.starts_with(".asset-store/")
        || path == "decks"
        || path == ".asset-store"
}

/// MSRV — единственный владелец версии toolchain для builder-образа.
pub fn workspace_msrv(repo: &Path) -> Result<String> {
    let text = fs::read_to_string(repo.join("Cargo.toml")).map_err(|e| e.to_string())?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[workspace.package]";
            continue;
        }
        if in_package && let Some(value) = line.strip_prefix("rust-version") {
            let value = value
                .trim_start()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"');
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return Ok(value.to_owned());
            }
        }
    }
    Err("В корневом Cargo.toml нет [workspace.package] rust-version".into())
}

fn dirty_paths(status: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        paths.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        if (matches!(entry[0], b'R' | b'C') || matches!(entry[1], b'R' | b'C'))
            && let Some(original) = entries.next()
        {
            paths.push(String::from_utf8_lossy(original).into_owned());
        }
    }
    paths
}

fn listed(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Положение рабочего каталога стенда относительно корня репозитория после
/// каноникализации: `Some(относительный путь)`, если он внутри.
fn work_dir_in_repository(repo: &Path, work: &Path) -> Result<Option<PathBuf>> {
    let repo = fs::canonicalize(repo).map_err(|e| format!("{}: {e}", repo.display()))?;
    let work = fs::canonicalize(work).map_err(|e| format!("{}: {e}", work.display()))?;
    if repo.starts_with(&work) {
        return Err(format!(
            "Рабочий каталог стенда {} содержит репозиторий; выберите отдельный --work-dir",
            work.display()
        ));
    }
    Ok(work.strip_prefix(&repo).ok().map(Path::to_path_buf))
}

/// Копирует снимок в `dest` и возвращает происхождение; dest должен быть пуст.
/// `work` — существующий рабочий каталог стенда: его файлы в снимок не входят,
/// а отслеживаемые Git файлы внутри него — ошибка, а не молчаливое исключение.
pub fn snapshot(repo: &Path, dest: &Path, work: &Path) -> Result<Provenance> {
    let excluded = work_dir_in_repository(repo, work)?;
    let own = |path: &str| {
        excluded
            .as_deref()
            .is_some_and(|dir| Path::new(path).starts_with(dir))
    };
    let commit = String::from_utf8_lossy(&git(repo, &["rev-parse", "HEAD"])?)
        .trim()
        .to_owned();
    let status = git(
        repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut dirty_paths = dirty_paths(&status);
    dirty_paths.retain(|path| !own(path));
    let tracked = listed(&git(repo, &["ls-files", "-z", "--cached"])?);
    if let Some(path) = tracked.iter().find(|path| own(path)) {
        return Err(format!(
            "В рабочем каталоге стенда есть отслеживаемый Git файл {path}; выберите другой --work-dir"
        ));
    }
    let mut paths = tracked;
    paths.extend(
        listed(&git(
            repo,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        )?)
        .into_iter()
        .filter(|path| !own(path)),
    );
    paths.sort();
    paths.dedup();
    let mut manifest = String::new();
    let mut files = 0;
    for path in paths {
        if private_area(&path) {
            return Err(format!(
                "Пользовательская область попала в индекс Git: {path}"
            ));
        }
        let source = repo.join(&path);
        // Удалённый, но ещё индексированный файл не входит в снимок рабочей копии.
        let Ok(meta) = fs::symlink_metadata(&source) else {
            continue;
        };
        if !meta.is_file() && !meta.file_type().is_symlink() {
            continue;
        }
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if meta.file_type().is_symlink() {
            // Сохраняем саму ссылку, не читая её цель за границей снимка.
            let link = fs::read_link(&source).map_err(|e| format!("{path}: {e}"))?;
            symlink(&link, &target).map_err(|e| format!("{}: {e}", target.display()))?;
            manifest.push_str(&format!(
                "l {}  {path}\n",
                sha256_hex(link.as_os_str().as_encoded_bytes())
            ));
        } else {
            let bytes = fs::read(&source).map_err(|e| format!("{path}: {e}"))?;
            let mode = meta.permissions().mode() & 0o7777;
            fs::write(&target, &bytes).map_err(|e| format!("{}: {e}", target.display()))?;
            fs::set_permissions(&target, meta.permissions())
                .map_err(|e| format!("{}: {e}", target.display()))?;
            manifest.push_str(&format!("f {mode:04o} {}  {path}\n", sha256_hex(&bytes)));
        }
        files += 1;
    }
    fs::write(dest.join(".e2e-source-manifest"), &manifest).map_err(|e| e.to_string())?;
    Ok(Provenance {
        commit,
        dirty: !dirty_paths.is_empty(),
        dirty_paths,
        tree_sha256: sha256_hex(manifest.as_bytes()),
        files,
        msrv: workspace_msrv(repo)?,
        excluded_work_dir: excluded.map(|dir| dir.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_areas_are_recognised() {
        assert!(private_area("decks/output/x/deck.json"));
        assert!(private_area(".asset-store/manifest.json"));
        assert!(!private_area("tools/anki-e2e/src/main.rs"));
        assert!(!private_area("decksmith.md"));
    }

    #[test]
    fn msrv_is_read_from_workspace_package_only() {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-msrv-{}",
            crate::util::random_hex(4).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nrust-version = \"1.0\"\n[workspace.package]\n# comment\nrust-version = \"1.99.1\"\n",
        )
        .unwrap();
        assert_eq!(workspace_msrv(&dir).unwrap(), "1.99.1");
        fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(workspace_msrv(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    fn git_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-snapshot-{name}-{}",
            crate::util::random_hex(4).unwrap()
        ));
        fs::create_dir_all(dir.join("tools")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace.package]\nrust-version = \"1.90\"\n",
        )
        .unwrap();
        fs::write(dir.join("tools/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(dir.join(".gitignore"), b"/target/\n").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            git(&dir, args).unwrap();
        }
        dir
    }

    #[test]
    fn snapshot_preserves_symlinks_permissions_and_hashes_their_changes() {
        let repo = git_repo("file-types");
        let work = repo.join("target/anki-e2e");
        fs::create_dir_all(&work).unwrap();
        let source = repo.join("tools/main.rs");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        symlink("tools", repo.join("tool-alias")).unwrap();
        symlink("../absent-outside", repo.join("external-alias")).unwrap();
        git(&repo, &["add", "tool-alias", "external-alias"]).unwrap();
        let first_dir = repo.with_extension("first");
        let first = snapshot(&repo, &first_dir, &work).unwrap();
        assert_eq!(
            fs::read_link(first_dir.join("tool-alias")).unwrap(),
            Path::new("tools")
        );
        assert_eq!(
            fs::read_link(first_dir.join("external-alias")).unwrap(),
            Path::new("../absent-outside")
        );
        assert_eq!(first.files, 5);

        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let second_dir = repo.with_extension("second");
        let second = snapshot(&repo, &second_dir, &work).unwrap();
        assert_ne!(first.tree_sha256, second.tree_sha256);
        assert_eq!(
            fs::metadata(second_dir.join("tools/main.rs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        assert_eq!(
            fs::read(first_dir.join("tools/main.rs")).unwrap(),
            fs::read(second_dir.join("tools/main.rs")).unwrap()
        );

        fs::remove_file(repo.join("tool-alias")).unwrap();
        symlink("other-target", repo.join("tool-alias")).unwrap();
        let third_dir = repo.with_extension("third");
        let third = snapshot(&repo, &third_dir, &work).unwrap();
        assert_ne!(second.tree_sha256, third.tree_sha256);
        for path in [first_dir, second_dir, third_dir, repo] {
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn unignored_work_dir_inside_repository_never_enters_the_snapshot() {
        let repo = git_repo("work-inside");
        let work = repo.join("e2e-results");
        let dest = |n: u32| {
            repo.join(format!(
                "../{}-dest-{n}",
                repo.file_name().unwrap().to_string_lossy()
            ))
        };
        // Уже заполненный рабочий каталог: кэш архивов, прошлые снимки и отчёты.
        fs::create_dir_all(work.join("cache/anki/26.09.3")).unwrap();
        fs::write(work.join("cache/anki/26.09.3/anki.tar.zst"), b"archive").unwrap();
        fs::create_dir_all(work.join("build/src-0123/src/tools")).unwrap();
        fs::write(work.join("build/src-0123/src/tools/main.rs"), b"old").unwrap();
        fs::create_dir_all(work.join("runs/1")).unwrap();
        fs::write(work.join("runs/1/report.json"), b"{}").unwrap();
        fs::write(repo.join("tools/new.rs"), b"// untracked source\n").unwrap();

        let first = snapshot(&repo, &dest(1), &work).unwrap();
        assert_eq!(first.excluded_work_dir.as_deref(), Some("e2e-results"));
        assert_eq!(first.files, 4, "Cargo.toml, .gitignore, main.rs, new.rs");
        assert_eq!(first.dirty_paths, ["tools/new.rs"]);
        assert!(!dest(1).join("e2e-results").exists());
        assert!(dest(1).join("tools/new.rs").is_file());

        // Следующий прогон кладёт в рабочий каталог собственный снимок: он не растёт.
        fs::create_dir_all(work.join("build/src-next")).unwrap();
        fs::rename(dest(1), work.join("build/src-next/src")).unwrap();
        // Относительный путь к тому же каталогу даёт то же исключение.
        let relative = repo.join("tools/../e2e-results");
        let second = snapshot(&repo, &dest(2), &relative).unwrap();
        assert_eq!(second.tree_sha256, first.tree_sha256);
        assert_eq!(second.files, first.files);
        fs::remove_dir_all(dest(2)).unwrap();
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn work_dir_with_tracked_sources_or_containing_repository_is_rejected() {
        let repo = git_repo("work-tracked");
        let dest = repo.with_extension("dest");
        let error = snapshot(&repo, &dest, &repo.join("tools")).unwrap_err();
        assert!(error.contains("tools/main.rs"), "{error}");
        let error = snapshot(&repo, &dest, &repo).unwrap_err();
        assert!(error.contains("содержит репозиторий"), "{error}");
        assert!(!dest.exists());

        // Внешний и игнорируемый каталоги допустимы и ничего не исключают из исходников.
        let outside = repo.with_extension("work");
        fs::create_dir_all(&outside).unwrap();
        let external = snapshot(&repo, &dest, &outside).unwrap();
        assert_eq!(external.excluded_work_dir, None);
        assert_eq!(external.files, 3);
        fs::remove_dir_all(&dest).unwrap();
        fs::create_dir_all(repo.join("target/anki-e2e")).unwrap();
        let ignored = snapshot(&repo, &dest, &repo.join("target/anki-e2e")).unwrap();
        assert_eq!(ignored.files, 3);
        for dir in [dest, outside, repo] {
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn dirty_paths_keeps_both_rename_and_copy_paths() {
        let status = b"R  tools/new_destination.rs\0tools/old_source.rs\0 C copied_destination.rs\0x\0 M edited.rs\0?? n\0";

        assert_eq!(
            dirty_paths(status),
            [
                "tools/new_destination.rs",
                "tools/old_source.rs",
                "copied_destination.rs",
                "x",
                "edited.rs",
                "n"
            ]
        );
    }
}
