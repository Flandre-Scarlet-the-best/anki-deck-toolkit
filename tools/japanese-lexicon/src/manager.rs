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
    /// Новизну нельзя уверенно установить: источник публикации не публикует сумму.
    Unknown,
    /// Проверка не выполнена: сеть, источник публикации или его ответ не позволили.
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

/// Во сколько раз допустимо уменьшение содержимого словаря при обновлении:
/// кандидат обязан сохранить не менее `1 / RETAINED_CONTENT_FACTOR` прежнего.
///
/// Критерий причинный, а не подобранный под тест. Содержимое управляемых
/// словарей — курируемые корпуса: JMdict, JMnedict и KANJIDIC собирает EDRDG,
/// частотные метаданные — jpdb.io. Их выпуски правят статьи точечно и в целом
/// растут, поэтому потеря половины и более записей означает не редактуру, а
/// усечённый артефакт: оборванную выгрузку, чужой или частично собранный
/// набор данных. Границы применимости: сравнение относительное и включается
/// только при исправной прежней версии той же схемы корпуса с ненулевым
/// содержанием; при первом `install` сравнивать не с чем. Абсолютного порога
/// «не меньше N записей» нет намеренно: у словарей разный масштаб данных, и
/// единое число запретило бы законный маленький словарь, ничего не сказав о
/// потере относительно прежней версии.
const RETAINED_CONTENT_FACTOR: u64 = 2;

/// Аномальное сокращение содержимого кандидата против прежней исправной
/// версии: сообщение для диагностики либо `None`, если потеря допустима.
///
/// Показатели берутся у [`index::content_amount`] — по роли источника, а не
/// одной константой на все словари. Полная потеря чтений у источника,
/// который их извлекает, отвергается раньше как [`index::ContentDefect`].
/// Нулевая прежняя величина отказа не даёт: относительное сравнение с пустой
/// базой ничего не измеряет, а `update` берёт эталоном только исправную
/// версию, у которой обязательный банк заведомо непуст.
fn anomalous_shrinkage(
    source: &Source,
    baseline: &Manifest,
    candidate: &BuildStats,
) -> Option<String> {
    let previous = index::content_amount(source, &baseline.corpus.stats);
    let current = index::content_amount(source, candidate);
    let mut amounts = vec![(
        "записей обязательных банков",
        current.records,
        previous.records,
    )];
    if let (Some(now), Some(before)) = (current.readings, previous.readings) {
        amounts.push(("пригодных чтений", now, before));
    }
    let lost: Vec<String> = amounts
        .into_iter()
        .filter(|(_, now, before)| now.saturating_mul(RETAINED_CONTENT_FACTOR) < *before)
        .map(|(label, now, before)| format!("{label} {now} против {before}"))
        .collect();
    if lost.is_empty() {
        return None;
    }
    Some(format!(
        "содержимое архива аномально меньше активной исправной версии (revision {}): {} — потеряно больше половины; кандидат не активируется",
        baseline.index_json.revision,
        lost.join(", ")
    ))
}

/// Проверка архива, построение индекса и манифест в каталоге staging.
///
/// `baseline` — активная исправная версия того же словаря, если она есть:
/// она задаёт планку полноты для кандидата и позволяет не перестраивать
/// индекс, когда скачанный архив совпал с ней по SHA-256.
fn prepare(
    staging: &Path,
    source: &Source,
    artifact: &RemoteArtifact,
    digest: &FileDigest,
    checked_at: &str,
    options: &Options,
    baseline: Option<&Manifest>,
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
    // Содержательная полнота: обязательный банк, в котором нет ни одной
    // записи, и лексический корпус без единого чтения — это не словарь.
    // Проверка до манифеста и до активации, чтобы синтаксически верный
    // неполный архив не заменил рабочий.
    if let Some(defect) = index::content_defect(source, &stats) {
        return Err(Error::format(format!(
            "архив отклонён для словаря {}: {}; неполный словарь не активируется",
            source.id,
            defect.describe()
        )));
    }
    // Аномальное сокращение относительно прежней исправной версии: тот же
    // отказ до манифеста и активации, поэтому прежний корпус и реестр целы.
    if let Some(baseline) = baseline
        && let Some(message) = anomalous_shrinkage(source, baseline, &stats)
    {
        return Err(Error::new(Code::UpstreamUnexpected, message));
    }
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
    // Каталог словаря мог смениться ссылкой после начала операции:
    // перепроверка до создания и до переноса версии.
    store.ensure_layout()?;
    fs::create_dir_all(&parent).map_err(|e| Error::io(parent.display(), e))?;
    store.ensure_layout()?;
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
        // Ошибка могла случиться уже после замены реестра (fsync каталога):
        // активную версию не удалять; при сомнении каталог остаётся остатком.
        let activated = store
            .registry()
            .map(|r| r.dictionaries.get(source.id).map(|v| v.version.as_str()) == Some(&version));
        if let Ok(false) = activated {
            let _ = store::remove_owned_dir(&dest);
        }
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
///
/// `baseline` — прежняя исправная версия при обновлении: она отсеивает
/// аномально сокращённый кандидат и избавляет от переиндексации, когда
/// скачанный архив совпал с ней по SHA-256. При установке с нуля её нет.
fn obtain_and_activate(
    store: &Store,
    source: &Source,
    fetch: Fetch<'_>,
    options: &Options,
    baseline: Option<&Manifest>,
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
        if baseline.is_some_and(|b| b.archive.sha256 == digest.sha256) {
            return Ok(None);
        }
        let manifest = prepare(
            &staging,
            source,
            &artifact,
            &digest,
            &checked_at,
            options,
            baseline,
        )?;
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

/// `install`: недостающие словари с живого источника публикации или из локального ZIP.
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
                        "содержимое совпадает с опубликованным по контрольной сумме; маркер релиза у источника публикации — {}, у установленного — {}",
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
            "источник публикации не публикует контрольную сумму: новизну можно установить только скачиванием (update)".into(),
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
        // Реестр и манифест читаются под `active.lock`, чтобы параллельный
        // update не удалил прочитанную версию; блокировка не держится на сеть.
        let current = match &store {
            Some(store) => store.read_consistent(|| active(store, source.id))?,
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
        // не считается актуальной, и update её восстанавливает. Она же —
        // эталон полноты: испорченный или устаревший корпус не может
        // служить образцом, а сравнение с ним ещё и маскировало бы причину
        // — перестроить индекс такой версии важнее, чем измерить потерю.
        let baseline = manifest.as_ref().ok().filter(|m| {
            m.corpus.schema == CORPUS_SCHEMA
                && verify_version(&store, source, &version, m, true).is_empty()
        });
        if let Some(m) = baseline
            && matches!(compare(m, &remote).0, Outcome::UpToDate)
        {
            let mut item = ItemReport::new(source.id, Outcome::UpToDate, compare(m, &remote).1);
            item.installed = previous;
            item.remote = Some(remote);
            report.push(item);
            continue;
        }
        let remote_copy = remote.clone();
        let mut item = match obtain_and_activate(
            &store,
            source,
            Fetch::Remote(transport, remote),
            options,
            baseline,
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
///
/// Собираются все виды проблем, какие удалось установить: неполнота корпуса
/// не отменяет проверку целостности файлов, иначе вызывающая сторона увидела
/// бы только дефект содержимого и не узнала о повреждённом архиве или индексе.
/// Пропускаются только проверки, для которых физическое состояние файлов
/// делает их заведомо бессмысленными: отсутствующий или неточного размера
/// файл нечего хешировать и открывать.
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
        // Схема корпуса не наша: счётчики строк в ней значат другое, поэтому
        // проверка SQLite пропускается — но целостность файлов проверяется:
        // испорченный файл остаётся испорченным при любой схеме.
        problems.push(problem(
            Code::IndexCorrupt,
            format!(
                "схема корпуса {} устарела (текущая {CORPUS_SCHEMA}); update перестроит индекс",
                manifest.corpus.schema
            ),
        ));
    }
    // Содержательная неполнота: версия, установленная до появления проверки,
    // остаётся активной, `verify` её не исправляет, а только показывает.
    // `update` такую версию не считает актуальной и либо переустановит рабочий
    // артефакт, либо откажет на неполном, сохранив прежнюю.
    if let Some(defect) = index::content_defect(source, &manifest.corpus.stats) {
        problems.push(problem(
            Code::InvalidFormat,
            format!(
                "в установленном корпусе словаря {} {}: словарь неполон; исправный артефакт вернёт update, иначе удалите словарь и установите заново",
                source.id,
                defect.describe()
            ),
        ));
    }
    let archive = dir.join(store::ARCHIVE_FILE);
    let index_path = dir.join(store::INDEX_FILE);
    let mut file_is_useless = false;
    for (path, bytes) in [
        (&archive, manifest.archive.bytes),
        (&index_path, manifest.corpus.bytes),
    ] {
        match fs::metadata(path) {
            Ok(meta) if meta.len() == bytes => {}
            Ok(meta) => {
                file_is_useless = true;
                problems.push(problem(
                    Code::IndexCorrupt,
                    format!(
                        "{}: {} байт, в манифесте {bytes}",
                        path.display(),
                        meta.len()
                    ),
                ));
            }
            Err(e) => {
                file_is_useless = true;
                problems.push(problem(
                    Code::IndexCorrupt,
                    format!("{}: {e}", path.display()),
                ));
            }
        }
    }
    if file_is_useless || !deep {
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
    // Дальше индекс проверяется, даже если расхождение уже найдено: две
    // независимые проверки одного файла говорят о разном, и вызывающая
    // сторона должна видеть обе — и потерю файла, и его порчу.
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
    /// ID в реестре, которых нет в каталоге словарей: делают `ok` ложным.
    pub unknown_registry_entries: Vec<String>,
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
        unknown_registry_entries: Vec::new(),
    };
    let Some(store) = store else {
        report.dictionaries = sources.iter().map(|s| not_installed_status(s)).collect();
        return Ok(report);
    };
    // Реестр и версии читаются согласованно: иначе параллельная активация
    // может удалить версию, на которую ссылается уже прочитанный реестр.
    store.read_consistent(|| {
        let mut report = report.clone();
        survey_store(&store, &mut report, sources, deep)?;
        Ok(report)
    })
}

fn not_installed_status(source: &Source) -> DictionaryStatus {
    DictionaryStatus {
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
    }
}

fn survey_store(
    store: &Store,
    report: &mut StatusReport,
    sources: &[&Source],
    deep: bool,
) -> Result<()> {
    let registry = store.registry()?;
    for source in sources {
        let mut status = not_installed_status(source);
        if let Some(entry) = registry.dictionaries.get(source.id) {
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
    report.operation_in_progress = store.operation_in_progress()?;
    report.leftovers = store.leftovers(&registry)?;
    report.untouched = store.foreign_entries()?;
    report.unknown_registry_entries = registry
        .dictionaries
        .keys()
        .filter(|id| catalog::find(id).is_none())
        .cloned()
        .collect();
    if !report.unknown_registry_entries.is_empty() {
        report.ok = false;
    }
    Ok(())
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
