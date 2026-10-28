//! Где лежит локальная история пользователя.
//!
//! По умолчанию — пользовательский каталог данных по XDG Base Directory:
//! `$XDG_DATA_HOME/anki-deck-toolkit/benchmark-history`, а без него —
//! `~/.local/share/anki-deck-toolkit/benchmark-history`. Переменная
//! [`STORE_ENV`] переопределяет путь для тестов и управляемого переноса.
//! Относительные пути отвергаются: по XDG они недействительны, а результат
//! зависел бы от текущего каталога.

use std::path::{Path, PathBuf};

/// Явный корень хранилища; абсолютный путь.
pub const STORE_ENV: &str = "BENCHMARK_HISTORY_DIR";
/// Подкаталог внутри каталога пользовательских данных.
pub const DATA_SUBDIR: &str = "anki-deck-toolkit/benchmark-history";

/// Откуда взят корень хранилища: это видно пользователю в `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    Option,
    Environment,
    XdgDataHome,
    Home,
}

#[derive(Debug, Clone)]
pub struct Location {
    pub root: PathBuf,
    pub origin: Origin,
}

/// Корень хранилища: явный `--store`, затем [`STORE_ENV`], затем XDG.
pub fn resolve(explicit: Option<&Path>) -> Result<Location, String> {
    resolve_with(explicit, &|name| std::env::var_os(name))
}

pub fn resolve_with(
    explicit: Option<&Path>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Location, String> {
    let found = choose(explicit, env)?;
    // Путь сверяется с рабочей копией по компонентам: `..` после ещё не
    // созданного каталога увёл бы хранилище внутрь репозитория незаметно.
    if found
        .root
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "путь хранилища {} содержит «..»: укажите путь без переходов вверх",
            found.root.display()
        ));
    }
    Ok(found)
}

fn choose(
    explicit: Option<&Path>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Location, String> {
    if let Some(path) = explicit {
        return absolute(path, "--store").map(|root| Location {
            root,
            origin: Origin::Option,
        });
    }
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(path) = set(STORE_ENV) {
        return absolute(&path, STORE_ENV).map(|root| Location {
            root,
            origin: Origin::Environment,
        });
    }
    // Относительный XDG_DATA_HOME по спецификации недействителен и пропускается.
    if let Some(data) = set("XDG_DATA_HOME").filter(|p| p.is_absolute()) {
        return Ok(Location {
            root: data.join(DATA_SUBDIR),
            origin: Origin::XdgDataHome,
        });
    }
    match set("HOME").filter(|p| p.is_absolute()) {
        Some(home) => Ok(Location {
            root: home.join(".local/share").join(DATA_SUBDIR),
            origin: Origin::Home,
        }),
        None => Err(format!(
            "каталог пользовательских данных не определён: нет абсолютных XDG_DATA_HOME и HOME; укажите {STORE_ENV} или --store"
        )),
    }
}

fn absolute(path: &Path, what: &str) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{what}: путь хранилища должен быть абсолютным, получен {}",
            path.display()
        ))
    }
}

/// Путь с разрешёнными символическими ссылками. Каталога может ещё не быть:
/// разрешается ближайший существующий предок, а остаток присоединяется как
/// есть. `..` в остатке отвергается: после ещё не созданного каталога он увёл
/// бы путь в другое место незаметно для проверки.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut probe = path.clone();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent.to_path_buf(),
            None => break,
        }
    }
    let rest = path.strip_prefix(&probe).unwrap_or(Path::new(""));
    if rest
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "путь {} содержит «..» после несуществующего каталога: укажите путь без переходов вверх",
            path.display()
        ));
    }
    Ok(probe
        .canonicalize()
        .unwrap_or_else(|_| probe.clone())
        .join(rest))
}

/// Локальные JSON и SQLite не должны попадать в Git: путь внутри любой рабочей
/// копии отвергается до создания файлов. Рабочая копия — ближайший предок с
/// `.git` (каталогом обычного клона или файлом worktree и подмодуля).
/// Проверяется сам путь, поэтому результат не зависит от текущего каталога,
/// каталога сценариев и того, откуда взят путь (`--store`, [`STORE_ENV`], XDG).
pub fn refuse_inside_git(path: &Path, what: &str) -> Result<(), String> {
    let resolved = resolved(path)?;
    match resolved
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
    {
        Some(work_tree) => Err(format!(
            "{what} {} лежит внутри рабочей копии Git {}: локальные результаты не должны попадать в Git; выберите каталог вне репозитория",
            path.display(),
            work_tree.display()
        )),
        None => Ok(()),
    }
}

/// Путь с разрешёнными символическими ссылками для сравнения двух каталогов.
/// В отличие от [`resolved`], не допускает висячей ссылки: она есть на диске,
/// но не разрешается, и запись через неё ушла бы туда, куда она указывает.
fn strictly_resolved(path: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut probe = path.clone();
    while probe.symlink_metadata().is_err() {
        match probe.parent() {
            Some(parent) => probe = parent.to_path_buf(),
            None => break,
        }
    }
    let rest = path.strip_prefix(&probe).unwrap_or(Path::new(""));
    if rest
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "путь {} содержит «..» после несуществующего каталога: укажите путь без переходов вверх",
            path.display()
        ));
    }
    let base = probe.canonicalize().map_err(|e| {
        format!(
            "путь {} не разрешается ({e}): возможно, он ведёт через висячую символическую ссылку",
            path.display()
        )
    })?;
    Ok(base.join(rest))
}

/// Каталог резервной копии не должен пересекаться с хранилищем: копия внутри
/// архива попала бы в его обход, а хранилище внутри копии — в её состав.
/// Пути сравниваются после разрешения символических ссылок, по компонентам.
pub fn refuse_overlap(store: &Path, other: &Path, what: &str) -> Result<(), String> {
    let store_resolved = strictly_resolved(store)?;
    let other_resolved = strictly_resolved(other)?;
    let relation = if other_resolved == store_resolved {
        "совпадает с хранилищем"
    } else if other_resolved.starts_with(&store_resolved) {
        "лежит внутри хранилища"
    } else if store_resolved.starts_with(&other_resolved) {
        "содержит хранилище"
    } else {
        return Ok(());
    };
    Err(format!(
        "{what} {} {relation} {} (после разрешения ссылок: {} и {}): выберите каталог вне хранилища, не содержащий его",
        other.display(),
        store.display(),
        other_resolved.display(),
        store_resolved.display()
    ))
}

/// История — локальные данные пользователя: внутри рабочей копии Git или
/// репозитория каталога сценариев она попала бы в `git status` и могла бы
/// быть случайно опубликована.
pub fn refuse_inside_repository(root: &Path, repository: &Path) -> Result<(), String> {
    refuse_inside_git(root, "хранилище истории")?;
    let repository = repository
        .canonicalize()
        .unwrap_or_else(|_| repository.to_path_buf());
    if resolved(root)?.starts_with(&repository) {
        return Err(format!(
            "хранилище истории {} лежит внутри рабочей копии репозитория: локальные результаты не должны попадать в Git; выберите каталог вне репозитория",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn order_of_sources() {
        let all = env(&[
            (STORE_ENV, "/explicit/env"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        let found = resolve_with(Some(Path::new("/opt/h")), &all).unwrap();
        assert_eq!(
            (found.root, found.origin),
            (PathBuf::from("/opt/h"), Origin::Option)
        );
        let found = resolve_with(None, &all).unwrap();
        assert_eq!(found.origin, Origin::Environment);
        let found = resolve_with(None, &env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/h")])).unwrap();
        assert_eq!(found.root, PathBuf::from("/xdg").join(DATA_SUBDIR));
        let found = resolve_with(None, &env(&[("HOME", "/h")])).unwrap();
        assert_eq!(
            found.root,
            PathBuf::from("/h/.local/share").join(DATA_SUBDIR)
        );
    }

    #[test]
    fn relative_paths_are_refused_or_skipped() {
        assert!(resolve_with(Some(Path::new("rel")), &env(&[])).is_err());
        assert!(resolve_with(None, &env(&[(STORE_ENV, "rel")])).is_err());
        let found = resolve_with(None, &env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/h")])).unwrap();
        assert_eq!(found.origin, Origin::Home);
        assert!(resolve_with(None, &env(&[("HOME", "")])).is_err());
    }

    #[test]
    fn parent_components_are_refused() {
        let escaped = Path::new("/tmp/x/missing/../repo/history");
        assert!(resolve_with(Some(escaped), &env(&[])).is_err());
        let via_env = "/tmp/x/missing/../repo/history";
        assert!(resolve_with(None, &env(&[(STORE_ENV, via_env)])).is_err());
        assert!(resolve_with(None, &env(&[("XDG_DATA_HOME", "/tmp/x/../y")])).is_err());
    }

    #[test]
    fn store_inside_repository_is_refused() {
        let repo = std::env::temp_dir().join(format!("bh-loc-{}", std::process::id()));
        std::fs::create_dir_all(&repo).unwrap();
        assert!(refuse_inside_repository(&repo.join("x/history"), &repo).is_err());
        assert!(refuse_inside_repository(&repo, &repo).is_err());
        assert!(
            refuse_inside_repository(&std::env::temp_dir().join("elsewhere-bh"), &repo).is_ok()
        );
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn git_work_tree_is_found_for_missing_nested_paths() {
        let base = std::env::temp_dir().join(format!("bh-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        // Вложенный репозиторий (worktree, подмодуль) отмечен файлом `.git`.
        std::fs::create_dir_all(repo.join("vendor/inner")).unwrap();
        std::fs::write(
            repo.join("vendor/inner/.git"),
            "gitdir: ../../.git/modules/inner\n",
        )
        .unwrap();
        let missing = repo.join("sub/./a/b/history");
        assert!(refuse_inside_git(&missing, "хранилище").is_err());
        assert!(!repo.join("sub/a").exists(), "проверка ничего не создаёт");
        assert!(refuse_inside_git(&repo, "хранилище").is_err());
        let inner = refuse_inside_git(&repo.join("vendor/inner/x"), "хранилище").unwrap_err();
        assert!(inner.contains("inner"), "{inner}");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(repo.join("sub"), base.join("link")).unwrap();
            assert!(refuse_inside_git(&base.join("link/new/history"), "хранилище").is_err());
        }
        assert!(refuse_inside_git(&base.join("outside/history"), "хранилище").is_ok());
        assert!(refuse_inside_git(&base.join("outside/missing/../../repo/x"), "копия").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
