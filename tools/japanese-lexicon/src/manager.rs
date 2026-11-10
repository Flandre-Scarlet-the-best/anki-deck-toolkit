//! Жизненный цикл словарей: установка, проверка и получение обновлений,
//! удаление, проверка целостности и статус.
//!
//! Этапы разделены: обнаружение версии → скачивание с проверкой суммы →
//! проверка архива → построение индекса → манифест и fsync → перенос из
//! staging → атомарная замена реестра. Новая версия невидима до последнего
//! шага; любой отказ до него оставляет прежнюю активную версию нетронутой.
//! Каждый словарь обрабатывается независимо: отказ одного не затрагивает
//! остальные, а отчёт перечисляет фактический исход по каждому.

use crate::catalog::{self, Source};
use crate::error::{Code, Error, Result};
use crate::index::{self, BuildStats, CORPUS_SCHEMA, Index};
use crate::store::{self, Manifest, RegistryEntry, Store};
use crate::upstream::{self, FileDigest, RemoteArtifact, Transport};
use crate::yomitan::{Archive, Limits};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const OPERATION_SCHEMA: &str = "japanese-lexicon.operation/1";
pub const STATUS_SCHEMA: &str = "japanese-lexicon.status/1";
pub const VERIFY_SCHEMA: &str = "japanese-lexicon.verify/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Installed,
    AlreadyInstalled,
    Updated,
    UpToDate,
    UpdateAvailable,
    NotInstalled,
    /// Новизну нельзя уверенно установить: upstream не публикует сумму.
    Unknown,
    /// Проверка не выполнена: сеть, upstream или ответ не позволили.
    CheckFailed,
    Failed,
    Removed,
}

impl Outcome {
    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Failed | Outcome::CheckFailed)
    }
}

/// Краткие сведения об установленной версии.
#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub version: String,
    pub release: Option<String>,
    pub revision: String,
    pub title: String,
    pub provider: String,
    pub locator: String,
    pub asset_name: String,
    pub download_url: Option<String>,
    pub archive_sha256: String,
    pub published_checksum_verified: bool,
    pub installed_at: String,
    pub checked_at: String,
    pub stats: BuildStats,
    pub path: PathBuf,
}

impl Installed {
    fn from(manifest: &Manifest, version: &str, path: PathBuf) -> Self {
        Self {
            version: version.to_owned(),
            release: manifest.artifact.release.clone(),
            revision: manifest.index_json.revision.clone(),
            title: manifest.index_json.title.clone(),
            provider: manifest.artifact.provider.clone(),
            locator: manifest.artifact.locator.clone(),
            asset_name: manifest.artifact.asset_name.clone(),
            download_url: manifest.artifact.download_url.clone(),
            archive_sha256: manifest.archive.sha256.clone(),
            published_checksum_verified: manifest.published_checksum_verified,
            installed_at: manifest.installed_at.clone(),
            checked_at: manifest.checked_at.clone(),
            stats: manifest.corpus.stats.clone(),
            path,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemReport {
    pub id: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Installed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Installed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteArtifact>,
}

impl ItemReport {
    fn new(id: &str, outcome: Outcome, message: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            outcome,
            code: None,
            message: message.into(),
            installed: None,
            previous: None,
            remote: None,
        }
    }

    fn failed(id: &str, outcome: Outcome, error: &Error) -> Self {
        let mut item = Self::new(id, outcome, error.message.clone());
        item.code = Some(error.code);
        item
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationReport {
    pub schema: &'static str,
    pub operation: &'static str,
    pub data_dir: PathBuf,
    /// Все словари обработаны без отказа.
    pub ok: bool,
    pub items: Vec<ItemReport>,
    /// Остатки прерванных операций, убранные перед началом.
    pub cleaned: Vec<PathBuf>,
    /// Посторонние записи каталога данных: не трогались.
    pub untouched: Vec<PathBuf>,
}

impl OperationReport {
    fn new(operation: &'static str, root: &Path) -> Self {
        Self {
            schema: OPERATION_SCHEMA,
            operation,
            data_dir: root.to_path_buf(),
            ok: true,
            items: Vec::new(),
            cleaned: Vec::new(),
            untouched: Vec::new(),
        }
    }

    fn push(&mut self, item: ItemReport) {
        if item.outcome.is_failure() {
            self.ok = false;
        }
        self.items.push(item);
    }
}

/// Параметры менеджера.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub limits: Limits,
}

/// Разрешает список ID: пустой список с `all` — весь стандартный набор.
pub fn select(ids: &[String], all: bool) -> Result<Vec<&'static Source>> {
    if all && !ids.is_empty() {
        return Err(Error::new(
            Code::InvalidInput,
            "укажите либо --all, либо ID словарей",
        ));
    }
    if all {
        return Ok(catalog::standard().iter().collect());
    }
    if ids.is_empty() {
        return Err(Error::new(
            Code::InvalidInput,
            format!(
                "укажите --all или ID словаря: {}",
                catalog::ids().join(", ")
            ),
        ));
    }
    let mut out: Vec<&'static Source> = Vec::new();
    for id in ids {
        let source = catalog::find(id).ok_or_else(|| {
            Error::new(
                Code::UnknownDictionary,
                format!(
                    "неизвестный словарь «{id}»; поддерживаются: {}",
                    catalog::ids().join(", ")
                ),
            )
        })?;
        if !out.iter().any(|s| s.id == source.id) {
            out.push(source);
        }
    }
    Ok(out)
}

/// Убирает остатки прерванных операций. Вызывается под блокировкой операций.
fn clean_leftovers(store: &Store) -> Result<Vec<PathBuf>> {
    let registry = store.registry()?;
    let leftovers = store.leftovers(&registry)?;
    for path in &leftovers {
        store::remove_owned_dir(path)?;
    }
    Ok(leftovers)
}

/// Активная версия: имя каталога и манифест (если читается).
fn active(store: &Store, id: &str) -> Result<Option<(String, Result<Manifest>)>> {
    let registry = store.registry()?;
    Ok(registry
        .dictionaries
        .get(id)
        .map(|e| (e.version.clone(), store.read_manifest(id, &e.version))))
}

fn installed_summary(store: &Store, id: &str) -> Option<Installed> {
    let (version, manifest) = active(store, id).ok()??;
    let manifest = manifest.ok()?;
    let path = store.version_dir(id, &version).ok()?;
    Some(Installed::from(&manifest, &version, path))
}

/// Проверка архива, построение индекса и манифест в каталоге staging.
fn prepare(
    staging: &Path,
    source: &Source,
    artifact: &RemoteArtifact,
    digest: &FileDigest,
    checked_at: &str,
    options: &Options,
) -> Result<Manifest> {
    let archive_path = staging.join(store::ARCHIVE_FILE);
    let (mut archive, inspection) = Archive::open(&archive_path, source, options.limits)?;
    let revision_matches_release = if source.revision_equals_release {
        let matches = artifact.release.as_deref() == Some(inspection.index.revision.as_str());
        if !matches && artifact.provider != "local_file" {
            return Err(Error::new(
                Code::UpstreamUnexpected,
                format!(
                    "revision «{}» в index.json не совпадает с релизом {:?}: неожиданный артефакт",
                    inspection.index.revision, artifact.release
                ),
            ));
        }
        Some(matches)
    } else {
        None
    };
    let index_path = staging.join(store::INDEX_FILE);
    let mut meta = BTreeMap::new();
    meta.insert("archive_sha256".to_owned(), digest.sha256.clone());
    meta.insert("revision".to_owned(), inspection.index.revision.clone());
    meta.insert("title".to_owned(), inspection.index.title.clone());
    let stats = index::build(&mut archive, &inspection, source, &index_path, &meta)?;
    let index_digest = upstream::digest_file(&index_path)?;
    let manifest = Manifest {
        schema: store::MANIFEST_SCHEMA.into(),
        dictionary_id: source.id.into(),
        role: source.role,
        artifact: artifact.clone(),
        archive: digest.clone(),
        published_checksum_verified: artifact.checksum.is_some(),
        revision_matches_release,
        index_json: inspection.index,
        corpus: store::CorpusInfo {
            schema: CORPUS_SCHEMA,
            file: store::INDEX_FILE.into(),
            bytes: index_digest.bytes,
            sha256: index_digest.sha256,
            stats,
        },
        checked_at: checked_at.to_owned(),
        installed_at: crate::time::now(),
        license: source.license.into(),
        attribution: source.attribution.into(),
    };
    store::write_atomic(
        &staging.join(store::MANIFEST_FILE),
        &serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )?;
    for file in [store::ARCHIVE_FILE, store::INDEX_FILE] {
        let path = staging.join(file);
        fs::File::open(&path)
            .and_then(|f| f.sync_all())
            .map_err(|e| Error::io(path.display(), e))?;
    }
    store::sync_dir(staging)?;
    Ok(manifest)
}

/// Перенос собранной версии и атомарное переключение реестра.
fn activate(store: &Store, source: &Source, staging: &Path, manifest: &Manifest) -> Result<String> {
    let id8 = store::random_id()?;
    let version = format!("{}-{}", &manifest.archive.sha256[..16], &id8[..8]);
    let parent = store.dictionary_dir(source.id);
    fs::create_dir_all(&parent).map_err(|e| Error::io(parent.display(), e))?;
    let dest = parent.join(&version);
    fs::rename(staging, &dest).map_err(|e| Error::io(dest.display(), e))?;
    store::sync_dir(&parent)?;
    let _guard = store.lock_activate()?;
    let mut registry = store.registry()?;
    let previous = registry.dictionaries.insert(
        source.id.to_owned(),
        RegistryEntry {
            version: version.clone(),
            activated_at: crate::time::now(),
        },
    );
    if let Err(e) = store.write_registry(&registry) {
        let _ = store::remove_owned_dir(&dest);
        return Err(e);
    }
    if let Some(old) = previous
        && old.version != version
        && let Ok(old_dir) = store.version_dir(source.id, &old.version)
    {
        // Неудача удаления прежней версии не отменяет активацию: каталог
        // станет остатком и будет убран следующей операцией.
        let _ = store::remove_owned_dir(&old_dir);
    }
    Ok(version)
}

enum Fetch<'a> {
    Remote(&'a dyn Transport, RemoteArtifact),
    Local(&'a Path),
}

/// Получает архив в новый каталог staging, собирает и активирует версию.
fn obtain_and_activate(
    store: &Store,
    source: &Source,
    fetch: Fetch<'_>,
    options: &Options,
    unchanged_if: Option<&str>,
) -> Result<Option<(String, Manifest)>> {
    let staging = store.new_staging()?;
    let result = (|| {
        let archive = staging.join(store::ARCHIVE_FILE);
        let checked_at = crate::time::now();
        let (artifact, digest) = match fetch {
            Fetch::Remote(transport, artifact) => {
                let digest = upstream::fetch(transport, source, &artifact, &archive)?;
                (artifact, digest)
            }
            Fetch::Local(path) => {
                let meta = fs::metadata(path).map_err(|e| Error::io(path.display(), e))?;
                if !meta.is_file() {
                    return Err(Error::new(
                        Code::InvalidInput,
                        format!("{} — не файл", path.display()),
                    ));
                }
                if meta.len() > source.max_archive_bytes {
                    return Err(Error::limit(format!(
                        "{}: {} байт, предел {}",
                        path.display(),
                        meta.len(),
                        source.max_archive_bytes
                    )));
                }
                fs::copy(path, &archive).map_err(|e| Error::io(path.display(), e))?;
                let digest = upstream::digest_file(&archive)?;
                let locator = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                let artifact = RemoteArtifact {
                    provider: "local_file".into(),
                    locator: locator.display().to_string(),
                    release: None,
                    asset_name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    download_url: None,
                    size: Some(digest.bytes),
                    published_at: None,
                    checksum: None,
                };
                (artifact, digest)
            }
        };
        if unchanged_if == Some(digest.sha256.as_str()) {
            return Ok(None);
        }
        let manifest = prepare(&staging, source, &artifact, &digest, &checked_at, options)?;
        let version = activate(store, source, &staging, &manifest)?;
        Ok(Some((version, manifest)))
    })();
    if staging.exists() {
        let _ = store::remove_owned_dir(&staging);
    }
    result
}

fn begin(
    root: &Path,
    operation: &'static str,
) -> Result<(Store, store::LockGuard, OperationReport)> {
    let store = Store::open_or_init(root)?;
    let guard = store.lock_operations()?;
    let mut report = OperationReport::new(operation, root);
    report.cleaned = clean_leftovers(&store)?;
    report.untouched = store.foreign_entries()?;
    Ok((store, guard, report))
}

/// `install`: недостающие словари с живого upstream или из локального ZIP.
pub fn install(
    root: &Path,
    sources: &[&Source],
    transport: &dyn Transport,
    local: Option<&Path>,
    options: &Options,
) -> Result<OperationReport> {
    if local.is_some() && sources.len() != 1 {
        return Err(Error::new(
            Code::InvalidInput,
            "--from-file принимает ровно один ID словаря",
        ));
    }
    let (store, _guard, mut report) = begin(root, "install")?;
    for source in sources {
        if let Some(installed) = installed_summary(&store, source.id) {
            let mut item = ItemReport::new(
                source.id,
                Outcome::AlreadyInstalled,
                "уже установлен; новые версии — через check-updates и update",
            );
            item.installed = Some(installed);
            report.push(item);
            continue;
        }
        let item = match local {
            Some(path) => {
                match obtain_and_activate(&store, source, Fetch::Local(path), options, None) {
                    Ok(Some((version, manifest))) => installed_item(
                        &store,
                        source,
                        &version,
                        &manifest,
                        Outcome::Installed,
                        "установлен из локального файла",
                    ),
                    Ok(None) => unreachable!("без эталона установка не пропускается"),
                    Err(e) => ItemReport::failed(source.id, Outcome::Failed, &e),
                }
            }
            None => match upstream::discover(transport, source) {
                Err(e) => ItemReport::failed(source.id, Outcome::Failed, &e),
                Ok(artifact) => {
                    let remote = artifact.clone();
                    let mut item = match obtain_and_activate(
                        &store,
                        source,
                        Fetch::Remote(transport, artifact),
                        options,
                        None,
                    ) {
                        Ok(Some((version, manifest))) => installed_item(
                            &store,
                            source,
                            &version,
                            &manifest,
                            Outcome::Installed,
                            "установлен",
                        ),
                        Ok(None) => unreachable!("без эталона установка не пропускается"),
                        Err(e) => ItemReport::failed(source.id, Outcome::Failed, &e),
                    };
                    item.remote = Some(remote);
                    item
                }
            },
        };
        report.push(item);
    }
    Ok(report)
}

fn installed_item(
    store: &Store,
    source: &Source,
    version: &str,
    manifest: &Manifest,
    outcome: Outcome,
    message: &str,
) -> ItemReport {
    let mut item = ItemReport::new(source.id, outcome, message);
    if let Ok(path) = store.version_dir(source.id, version) {
        item.installed = Some(Installed::from(manifest, version, path));
    }
    item
}

fn release_label(release: &Option<String>) -> String {
    release
        .clone()
        .unwrap_or_else(|| "без маркера релиза".into())
}

fn installed_label(manifest: &Manifest) -> String {
    match (
        &manifest.artifact.release,
        manifest.artifact.provider.as_str(),
    ) {
        (Some(r), _) => r.clone(),
        (None, "local_file") => format!(
            "локального файла (revision {})",
            manifest.index_json.revision
        ),
        (None, _) => format!("revision {}", manifest.index_json.revision),
    }
}

/// Сравнение установленного с обнаруженным артефактом.
fn compare(installed: &Manifest, remote: &RemoteArtifact) -> (Outcome, String) {
    match &remote.checksum {
        Some(checksum) if installed.archive.matches(checksum) => {
            if installed.artifact.release != remote.release {
                (
                    Outcome::UpToDate,
                    format!(
                        "содержимое совпадает с опубликованным по контрольной сумме; маркер релиза upstream — {}, у установленного — {}",
                        release_label(&remote.release),
                        installed_label(installed)
                    ),
                )
            } else {
                (Outcome::UpToDate, "установлена опубликованная версия (контрольная сумма совпала)".into())
            }
        }
        Some(_) => (
            Outcome::UpdateAvailable,
            format!(
                "доступен другой артефакт: {} вместо установленного {}",
                release_label(&remote.release),
                installed_label(installed)
            ),
        ),
        None => (
            Outcome::Unknown,
            "upstream не публикует контрольную сумму: новизну можно установить только скачиванием (update)".into(),
        ),
    }
}

/// `check-updates`: ничего не пишет в каталог данных.
pub fn check_updates(
    root: &Path,
    sources: &[&Source],
    transport: &dyn Transport,
) -> Result<OperationReport> {
    let mut report = OperationReport::new("check-updates", root);
    let store = Store::open_existing(root)?;
    for source in sources {
        let current = match &store {
            Some(store) => active(store, source.id)?,
            None => None,
        };
        let remote = upstream::discover(transport, source);
        let item = match (current, remote) {
            (_, Err(e)) => ItemReport::failed(source.id, Outcome::CheckFailed, &e),
            (None, Ok(remote)) => {
                let mut item = ItemReport::new(
                    source.id,
                    Outcome::NotInstalled,
                    "не установлен; доступен для install",
                );
                item.remote = Some(remote);
                item
            }
            (Some((_, Err(e))), Ok(remote)) => {
                let mut item = ItemReport::failed(source.id, Outcome::CheckFailed, &e);
                item.message = format!(
                    "установленная версия повреждена ({}); update переустановит её",
                    e.message
                );
                item.remote = Some(remote);
                item
            }
            (Some((version, Ok(manifest))), Ok(remote)) => {
                let (outcome, message) = compare(&manifest, &remote);
                let mut item = ItemReport::new(source.id, outcome, message);
                if let Some(store) = &store
                    && let Ok(path) = store.version_dir(source.id, &version)
                {
                    item.installed = Some(Installed::from(&manifest, &version, path));
                }
                item.remote = Some(remote);
                item
            }
        };
        report.push(item);
    }
    Ok(report)
}

/// `update`: получает и активирует новые версии установленных словарей.
pub fn update(
    root: &Path,
    sources: &[&Source],
    transport: &dyn Transport,
    options: &Options,
) -> Result<OperationReport> {
    let (store, _guard, mut report) = begin(root, "update")?;
    for source in sources {
        let current = active(&store, source.id)?;
        let Some((version, manifest)) = current else {
            report.push(ItemReport::new(
                source.id,
                Outcome::NotInstalled,
                "не установлен; используйте install",
            ));
            continue;
        };
        let previous = manifest.as_ref().ok().and_then(|m| {
            store
                .version_dir(source.id, &version)
                .ok()
                .map(|p| Installed::from(m, &version, p))
        });
        let remote = match upstream::discover(transport, source) {
            Ok(r) => r,
            Err(e) => {
                let mut item = ItemReport::failed(source.id, Outcome::Failed, &e);
                item.installed = previous;
                item.message = format!("{}; активная версия сохранена", e.message);
                report.push(item);
                continue;
            }
        };
        // Глубокая проверка: повреждённая активная версия того же артефакта
        // не считается актуальной, и update её восстанавливает.
        let healthy = manifest.as_ref().ok().filter(|m| {
            m.corpus.schema == CORPUS_SCHEMA
                && verify_version(&store, source, &version, m, true).is_empty()
        });
        if let Some(m) = healthy
            && matches!(compare(m, &remote).0, Outcome::UpToDate)
        {
            let mut item = ItemReport::new(source.id, Outcome::UpToDate, compare(m, &remote).1);
            item.installed = previous;
            item.remote = Some(remote);
            report.push(item);
            continue;
        }
        let unchanged_if = healthy.map(|m| m.archive.sha256.clone());
        let remote_copy = remote.clone();
        let mut item = match obtain_and_activate(
            &store,
            source,
            Fetch::Remote(transport, remote),
            options,
            unchanged_if.as_deref(),
        ) {
            Ok(Some((version, manifest))) => {
                let mut item = installed_item(
                    &store,
                    source,
                    &version,
                    &manifest,
                    Outcome::Updated,
                    "обновлён",
                );
                item.previous = previous;
                item
            }
            Ok(None) => {
                let mut item = ItemReport::new(
                    source.id,
                    Outcome::UpToDate,
                    "скачанный архив совпал с установленным по SHA-256: индекс не перестраивался",
                );
                item.installed = previous;
                item
            }
            Err(e) => {
                let mut item = ItemReport::failed(source.id, Outcome::Failed, &e);
                item.message = format!("{}; активная версия сохранена", e.message);
                item.installed = previous;
                item
            }
        };
        item.remote = Some(remote_copy);
        report.push(item);
    }
    Ok(report)
}

/// `remove <id>` / `remove --all`. Удаляются только каталоги словарей из
/// каталога и staging; посторонние записи остаются и перечисляются.
pub fn remove(root: &Path, sources: &[&Source]) -> Result<OperationReport> {
    let Some(store) = Store::open_existing(root)? else {
        let mut report = OperationReport::new("remove", root);
        for source in sources {
            report.push(ItemReport::new(
                source.id,
                Outcome::NotInstalled,
                "каталог данных ещё не создан",
            ));
        }
        return Ok(report);
    };
    let (store, _guard, mut report) = begin(store.root(), "remove")?;
    let _activate = store.lock_activate()?;
    let mut registry = store.registry()?;
    let mut removed = Vec::new();
    for source in sources {
        if registry.dictionaries.remove(source.id).is_some() {
            removed.push(*source);
        } else {
            report.push(ItemReport::new(
                source.id,
                Outcome::NotInstalled,
                "не установлен",
            ));
        }
    }
    store.write_registry(&registry)?;
    for source in removed {
        let item = match store::remove_owned_dir(&store.dictionary_dir(source.id)) {
            Ok(()) => ItemReport::new(source.id, Outcome::Removed, "удалён"),
            Err(e) => {
                let mut item = ItemReport::failed(source.id, Outcome::Failed, &e);
                item.message = format!(
                    "словарь выведен из реестра, но его файлы удалить не удалось ({}); следующая операция уберёт остаток",
                    e.message
                );
                item
            }
        };
        report.push(item);
    }
    report
        .items
        .sort_by_key(|i| catalog::standard().iter().position(|s| s.id == i.id));
    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    pub code: Code,
    pub message: String,
}

fn problem(code: Code, message: impl Into<String>) -> Problem {
    Problem {
        code,
        message: message.into(),
    }
}

/// Проверка одной версии. `deep` — с хешированием и quick_check SQLite.
fn verify_version(
    store: &Store,
    source: &Source,
    version: &str,
    manifest: &Manifest,
    deep: bool,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    let Ok(dir) = store.version_dir(source.id, version) else {
        return vec![problem(
            Code::IndexCorrupt,
            "недопустимое имя версии в реестре",
        )];
    };
    if manifest.corpus.schema != CORPUS_SCHEMA {
        problems.push(problem(
            Code::IndexCorrupt,
            format!(
                "схема корпуса {} устарела (текущая {CORPUS_SCHEMA}); update перестроит индекс",
                manifest.corpus.schema
            ),
        ));
    }
    let archive = dir.join(store::ARCHIVE_FILE);
    let index_path = dir.join(store::INDEX_FILE);
    for (path, bytes) in [
        (&archive, manifest.archive.bytes),
        (&index_path, manifest.corpus.bytes),
    ] {
        match fs::metadata(path) {
            Ok(meta) if meta.len() == bytes => {}
            Ok(meta) => problems.push(problem(
                Code::IndexCorrupt,
                format!(
                    "{}: {} байт, в манифесте {bytes}",
                    path.display(),
                    meta.len()
                ),
            )),
            Err(e) => problems.push(problem(
                Code::IndexCorrupt,
                format!("{}: {e}", path.display()),
            )),
        }
    }
    if !problems.is_empty() || !deep {
        return problems;
    }
    match upstream::digest_file(&archive) {
        Ok(d) if d == manifest.archive => {}
        Ok(_) => problems.push(problem(
            Code::IndexCorrupt,
            "SHA-256 архива не совпадает с манифестом",
        )),
        Err(e) => problems.push(problem(e.code, e.message)),
    }
    match upstream::digest_file(&index_path) {
        Ok(d) if d.sha256 == manifest.corpus.sha256 => {}
        Ok(_) => problems.push(problem(
            Code::IndexCorrupt,
            "SHA-256 индекса не совпадает с манифестом",
        )),
        Err(e) => problems.push(problem(e.code, e.message)),
    }
    if !problems.is_empty() {
        return problems;
    }
    let checked = Index::open(&index_path).and_then(|index| {
        index.quick_check()?;
        if index.meta_value("archive_sha256")?.as_deref() != Some(manifest.archive.sha256.as_str())
        {
            return Err(Error::corrupt("индекс построен из другого архива"));
        }
        let stats = &manifest.corpus.stats;
        for (table, expected) in [
            ("terms", stats.terms),
            ("readings", stats.readings),
            ("redirects", stats.redirects),
            ("kanji", stats.kanji),
            ("term_meta", stats.term_meta),
        ] {
            let actual = index.count(table)?;
            if actual != expected {
                return Err(Error::corrupt(format!(
                    "{table}: {actual} строк, в манифесте {expected}"
                )));
            }
        }
        Ok(())
    });
    if let Err(e) = checked {
        problems.push(problem(e.code, e.message));
    }
    problems
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ok,
    Corrupt,
    NotInstalled,
}

#[derive(Debug, Clone, Serialize)]
pub struct DictionaryStatus {
    pub id: &'static str,
    pub role: catalog::Role,
    pub title: &'static str,
    pub homepage: &'static str,
    pub license: &'static str,
    pub attribution: &'static str,
    pub redistribution_note: &'static str,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Installed>,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub schema: &'static str,
    pub data_dir: PathBuf,
    pub data_dir_origin: store::Origin,
    pub exists: bool,
    /// Для `verify`: всё проверенное в порядке.
    pub ok: bool,
    pub dictionaries: Vec<DictionaryStatus>,
    /// Остатки прерванных операций (убираются следующей изменяющей операцией).
    pub leftovers: Vec<PathBuf>,
    /// Изменяющая операция выполняется прямо сейчас.
    pub operation_in_progress: bool,
    pub untouched: Vec<PathBuf>,
}

fn survey(
    location: &store::Location,
    sources: &[&Source],
    deep: bool,
    schema: &'static str,
) -> Result<StatusReport> {
    let store = Store::open_existing(&location.root)?;
    let mut report = StatusReport {
        schema,
        data_dir: location.root.clone(),
        data_dir_origin: location.origin,
        exists: store.is_some(),
        ok: true,
        dictionaries: Vec::new(),
        leftovers: Vec::new(),
        operation_in_progress: false,
        untouched: Vec::new(),
    };
    let registry = match &store {
        Some(store) => Some(store.registry()?),
        None => None,
    };
    let _read = match &store {
        Some(store) => Some(store.lock_read()?),
        None => None,
    };
    for source in sources {
        let mut status = DictionaryStatus {
            id: source.id,
            role: source.role,
            title: source.title,
            homepage: source.homepage,
            license: source.license,
            attribution: source.attribution,
            redistribution_note: source.redistribution_note,
            state: State::NotInstalled,
            installed: None,
            problems: Vec::new(),
        };
        if let (Some(store), Some(registry)) = (&store, &registry)
            && let Some(entry) = registry.dictionaries.get(source.id)
        {
            match store.read_manifest(source.id, &entry.version) {
                Ok(manifest) => {
                    status.problems =
                        verify_version(store, source, &entry.version, &manifest, deep);
                    if let Ok(path) = store.version_dir(source.id, &entry.version) {
                        status.installed = Some(Installed::from(&manifest, &entry.version, path));
                    }
                }
                Err(e) => status.problems.push(problem(e.code, e.message)),
            }
            status.state = if status.problems.is_empty() {
                State::Ok
            } else {
                State::Corrupt
            };
        }
        if status.state == State::Corrupt {
            report.ok = false;
        }
        report.dictionaries.push(status);
    }
    if let (Some(store), Some(registry)) = (&store, &registry) {
        report.operation_in_progress =
            matches!(store.lock_operations(), Err(e) if e.code == Code::Busy);
        report.leftovers = store.leftovers(registry)?;
        report.untouched = store.foreign_entries()?;
        for id in registry.dictionaries.keys() {
            if catalog::find(id).is_none() {
                report.ok = false;
            }
        }
    }
    Ok(report)
}

/// `status`: быстрый обзор без хеширования.
pub fn status(location: &store::Location) -> Result<StatusReport> {
    let all: Vec<&Source> = catalog::standard().iter().collect();
    survey(location, &all, false, STATUS_SCHEMA)
}

/// `verify`: полная локальная проверка без сети и без исправлений.
pub fn verify(location: &store::Location, sources: &[&Source]) -> Result<StatusReport> {
    survey(location, sources, true, VERIFY_SCHEMA)
}
