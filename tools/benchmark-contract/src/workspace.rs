//! Пакеты и исполняемые файлы рабочей области Cargo, на которые ссылаются
//! декларации. Читаются только манифесты: Cargo не запускается.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// Пакет → имена его исполняемых файлов.
    pub packages: BTreeMap<String, BTreeSet<String>>,
    /// Профили сборки: встроенные и объявленные в корневом манифесте.
    pub profiles: BTreeSet<String>,
}

impl Workspace {
    /// Рабочая область из `Cargo.toml` корня репозитория. Отсутствие манифеста —
    /// не ошибка: такой каталог просто не может ссылаться на `cargo-bin`.
    pub fn read(root: &Path) -> Result<Self, String> {
        let mut workspace = Workspace {
            profiles: ["dev", "release", "test", "bench"]
                .map(str::to_owned)
                .into(),
            ..Default::default()
        };
        let manifest = root.join("Cargo.toml");
        if !manifest.is_file() {
            return Ok(workspace);
        }
        let value = read_toml(&manifest)?;
        if let Some(profiles) = value.get("profile").and_then(toml::Value::as_table) {
            workspace.profiles.extend(profiles.keys().cloned());
        }
        let members = value
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(toml::Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if value.get("package").is_some() {
            workspace.add_package(root, &value);
        }
        for member in members {
            if member.contains('*') {
                return Err(format!(
                    "шаблон участника рабочей области «{member}» не поддерживается: перечислите пакеты явно"
                ));
            }
            let dir = root.join(member);
            let member_manifest = read_toml(&dir.join("Cargo.toml"))?;
            workspace.add_package(&dir, &member_manifest);
        }
        Ok(workspace)
    }

    fn add_package(&mut self, dir: &Path, manifest: &toml::Value) {
        let Some(name) = manifest
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            return;
        };
        let mut bins = BTreeSet::new();
        let explicit = manifest.get("bin").and_then(toml::Value::as_array);
        for bin in explicit.into_iter().flatten() {
            if let Some(bin_name) = bin.get("name").and_then(toml::Value::as_str) {
                bins.insert(bin_name.to_owned());
            }
        }
        let autobins = manifest
            .get("package")
            .and_then(|p| p.get("autobins"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        // Явно объявленный путь Cargo не обнаруживает повторно под другим именем.
        let declared: BTreeSet<&str> = explicit
            .into_iter()
            .flatten()
            .filter_map(|bin| bin.get("path").and_then(toml::Value::as_str))
            .collect();
        if autobins {
            if dir.join("src/main.rs").is_file() && !declared.contains("src/main.rs") {
                bins.insert(name.to_owned());
            }
            if let Ok(entries) = fs::read_dir(dir.join("src/bin")) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "rs")
                        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                        && !declared.contains(format!("src/bin/{stem}.rs").as_str())
                    {
                        bins.insert(stem.to_owned());
                    } else if path.join("main.rs").is_file()
                        && let Some(dir_name) = path.file_name().and_then(|s| s.to_str())
                        && !declared.contains(format!("src/bin/{dir_name}/main.rs").as_str())
                    {
                        bins.insert(dir_name.to_owned());
                    }
                }
            }
        }
        self.packages.insert(name.to_owned(), bins);
    }
}

fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("не удалось прочитать {}: {error}", path.display()))?;
    toml::from_str(&text)
        .map_err(|error| format!("не удалось разобрать {}: {error}", path.display()))
}
