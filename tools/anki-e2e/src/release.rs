//! Выбор официального релиза Linux Anki и проверенный кэш дистрибутива.
//!
//! Первичный источник — GitHub Releases API репозитория `ankitects/anki`.
//! По умолчанию каждый запуск заново определяет последнюю стабильную версию;
//! кэш используется только для уже выбранного релиза и только после повторной
//! сверки SHA-256. Отказ сети или проверки не превращается в запуск устаревшей
//! версии. Максимальная стабильная версия выбирается только по полностью
//! прочитанному индексу: GitHub упорядочивает релизы по дате, а не по номеру.

use crate::report::Stop;
use crate::util::{self, sha256_file};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const API: &str = "https://api.github.com/repos/ankitects/anki";
const DOWNLOAD_PREFIX: &str = "https://github.com/ankitects/anki/releases/download/";
const PAGE_SIZE: usize = 100;
/// Сетевой бюджет индекса: до 500 релизов. Больше — максимум не подтверждается.
const MAX_PAGES: u32 = 5;
const STAGE: &str = "resolve_anki_release";
/// Загрузка без единого полученного байта дольше этого срока считается зависшей.
const DOWNLOAD_STALL: Duration = Duration::from_secs(60);
/// Как часто ожидание данных проверяет прерывание пользователем.
const INTERRUPT_CHECK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Архитектура Docker daemon (`docker info`), а не хоста CLI.
    pub fn from_docker(value: &str) -> Option<Arch> {
        match value {
            "x86_64" | "amd64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    pub fn docker_platform(self) -> &'static str {
        match self {
            Arch::X86_64 => "linux/amd64",
            Arch::Aarch64 => "linux/arm64",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Request {
    /// Штатный режим: последняя опубликованная стабильная версия.
    Latest,
    /// Регрессионное воспроизведение конкретного официального релиза.
    Explicit {
        version: String,
        allow_prerelease: bool,
    },
    /// Без сети: только заранее проверенный артефакт с известным SHA-256.
    Offline {
        version: String,
        sha256: String,
        archive: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseJson {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub assets: Vec<AssetJson>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetJson {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub mode: &'static str,
    pub latest_stable: Option<String>,
    pub latest_stable_error: Option<String>,
    pub version: String,
    pub prerelease: bool,
    pub published_at: Option<String>,
    pub release_url: Option<String>,
    pub arch: Arch,
    pub asset_name: String,
    pub asset_url: Option<String>,
    pub asset_size: Option<u64>,
    pub upstream_sha256: Option<String>,
    pub checksum_source: &'static str,
    pub decision: String,
}

/// Источник индекса релизов; в тестах заменяется синтетическим списком.
pub trait ReleaseSource {
    fn page(&self, page: u32) -> Result<Vec<ReleaseJson>, String>;
    fn by_tag(&self, tag: &str) -> Result<Option<ReleaseJson>, String>;
}

pub fn asset_name(version: &str, arch: Arch) -> String {
    format!("anki-{version}-linux-{}.tar.zst", arch.as_str())
}

/// Только числовые компоненты: `26.09b3`, `rc` и подобные метки не стабильны.
pub fn stable_version(tag: &str) -> Option<Vec<u64>> {
    let parts: Vec<&str> = tag.split('.').collect();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    parts
        .iter()
        .map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        })
        .collect()
}

fn is_stable(release: &ReleaseJson) -> bool {
    !release.draft && !release.prerelease && stable_version(&release.tag_name).is_some()
}

pub fn parse_digest(digest: Option<&str>) -> Option<String> {
    let hex = digest?.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

fn latest_stable(releases: &[ReleaseJson]) -> Option<&ReleaseJson> {
    releases
        .iter()
        .filter(|r| is_stable(r))
        .max_by(|a, b| stable_version(&a.tag_name).cmp(&stable_version(&b.tag_name)))
}

fn linux_asset(release: &ReleaseJson, arch: Arch) -> Result<&AssetJson, Stop> {
    let expected = asset_name(&release.tag_name, arch);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected)
        .ok_or_else(|| {
            let names: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
            Stop::blocked(
                STAGE,
                "release_without_linux_artifact",
                format!(
                    "В релизе {} нет официального артефакта {expected}; доступны: {}",
                    release.tag_name,
                    names.join(", ")
                ),
            )
        })?;
    let url = format!("{DOWNLOAD_PREFIX}{}/{}", release.tag_name, asset.name);
    if asset.browser_download_url != url {
        return Err(Stop::error(
            STAGE,
            "unexpected_artifact_url",
            format!(
                "URL артефакта {} не совпадает с официальным {url}",
                asset.browser_download_url
            ),
        ));
    }
    Ok(asset)
}

fn resolved_from(
    release: &ReleaseJson,
    arch: Arch,
    mode: &'static str,
    latest: Option<String>,
    latest_error: Option<String>,
    decision: String,
) -> Result<Resolved, Stop> {
    let asset = linux_asset(release, arch)?;
    let upstream = parse_digest(asset.digest.as_deref());
    Ok(Resolved {
        mode,
        latest_stable: latest,
        latest_stable_error: latest_error,
        version: release.tag_name.clone(),
        prerelease: release.prerelease,
        published_at: release.published_at.clone(),
        release_url: release.html_url.clone(),
        arch,
        asset_name: asset.name.clone(),
        asset_url: Some(asset.browser_download_url.clone()),
        asset_size: Some(asset.size),
        checksum_source: if upstream.is_some() {
            "github_release_asset_digest"
        } else {
            "unavailable"
        },
        upstream_sha256: upstream,
        decision,
    })
}

/// Полный индекс релизов: до неполной страницы. Порядок страниц — по дате
/// публикации, поэтому числовой максимум доказывает только полный список;
/// упор в [`MAX_PAGES`] — ошибка, а не молчаливый выбор из прочитанной части.
fn all_releases(source: &dyn ReleaseSource) -> Result<(Vec<ReleaseJson>, u32), String> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let items = source.page(page)?;
        let complete = items.len() < PAGE_SIZE;
        all.extend(items);
        if complete {
            return Ok((all, page));
        }
    }
    Err(format!(
        "индекс релизов длиннее {MAX_PAGES} страниц по {PAGE_SIZE}; максимальная стабильная версия не подтверждена"
    ))
}

pub fn resolve(
    source: &dyn ReleaseSource,
    request: &Request,
    arch: Arch,
) -> Result<Resolved, Stop> {
    match request {
        Request::Latest => {
            let (releases, pages) = all_releases(source).map_err(|e| {
                Stop::error(
                    STAGE,
                    "release_index_unavailable",
                    format!("Не удалось получить список релизов Anki; устаревшая версия не выбирается: {e}"),
                )
            })?;
            let latest = latest_stable(&releases).ok_or_else(|| {
                Stop::blocked(
                    STAGE,
                    "no_stable_release",
                    "В индексе нет опубликованного стабильного релиза",
                )
            })?;
            let skipped: Vec<&str> = releases
                .iter()
                .filter(|r| !is_stable(r))
                .map(|r| r.tag_name.as_str())
                .take(8)
                .collect();
            resolved_from(
                latest,
                arch,
                "latest_stable",
                Some(latest.tag_name.clone()),
                None,
                format!(
                    "Максимальная стабильная версия по числовому сравнению среди всех {} релизов индекса ({pages} стр.); draft/prerelease/beta исключены ({})",
                    releases.len(),
                    skipped.join(", ")
                ),
            )
        }
        Request::Explicit {
            version,
            allow_prerelease,
        } => {
            let (latest, latest_error) = match all_releases(source) {
                Ok((list, _)) => (latest_stable(&list).map(|r| r.tag_name.clone()), None),
                Err(e) => (None, Some(e)),
            };
            let release = source
                .by_tag(version)
                .map_err(|e| Stop::error(STAGE, "release_lookup_failed", e))?
                .ok_or_else(|| {
                    Stop::error(
                        STAGE,
                        "requested_release_not_found",
                        format!("Официальный релиз Anki {version} не найден"),
                    )
                })?;
            if release.draft {
                return Err(Stop::error(
                    STAGE,
                    "requested_release_is_draft",
                    "Черновик релиза не используется",
                ));
            }
            if release.prerelease && !allow_prerelease {
                return Err(Stop::blocked(
                    STAGE,
                    "prerelease_requires_flag",
                    format!("{version} опубликован как prerelease; добавьте --allow-prerelease"),
                ));
            }
            resolved_from(
                &release,
                arch,
                "explicit",
                latest,
                latest_error,
                format!(
                    "Версия {version} явно задана пользователем для воспроизведения; выбор по умолчанию не меняется"
                ),
            )
        }
        Request::Offline {
            version, sha256, ..
        } => {
            if version.is_empty()
                || !version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.')
            {
                return Err(Stop::error(
                    STAGE,
                    "invalid_version",
                    format!("Некорректная версия {version}"),
                ));
            }
            Ok(Resolved {
                mode: "offline_pinned",
                latest_stable: None,
                latest_stable_error: Some("Офлайн-режим: свежесть stable не проверялась".into()),
                version: version.clone(),
                prerelease: false,
                published_at: None,
                release_url: None,
                arch,
                asset_name: asset_name(version, arch),
                asset_url: None,
                asset_size: None,
                upstream_sha256: Some(sha256.to_ascii_lowercase()),
                checksum_source: "user_supplied_offline_sha256",
                decision: "Явный офлайн-режим с заранее проверенным SHA-256; сеть и свежесть не проверялись".into(),
            })
        }
    }
}

pub struct GitHub {
    client: reqwest::blocking::Client,
}

impl GitHub {
    pub fn new() -> Result<GitHub, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("anki-deck-toolkit-e2e")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(GitHub { client })
    }

    fn get(&self, url: &str) -> Result<Option<serde_json::Value>, String> {
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(|e| format!("{url}: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        response.json().map(Some).map_err(|e| format!("{url}: {e}"))
    }
}

impl ReleaseSource for GitHub {
    fn page(&self, page: u32) -> Result<Vec<ReleaseJson>, String> {
        let url = format!("{API}/releases?per_page={PAGE_SIZE}&page={page}");
        let value = self.get(&url)?.ok_or_else(|| format!("{url}: 404"))?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    fn by_tag(&self, tag: &str) -> Result<Option<ReleaseJson>, String> {
        if !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
            return Err(format!("Недопустимая версия {tag}"));
        }
        match self.get(&format!("{API}/releases/tags/{tag}"))? {
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }
}

/// Загрузка в поток; в тестах подменяется байтами в памяти. `interrupted`
/// проверяется и во время ожидания данных, а не только при записи.
pub trait Downloader {
    fn download(
        &self,
        url: &str,
        out: &mut dyn Write,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<u64, String>;
}

/// Открытие ответа и чтение в отдельном потоке: ожидание заголовков и
/// заблокированный `read()` не мешают заметить прерывание или отсутствие
/// прогресса. При отказе поток отсоединяется; запись в `out` идёт только из
/// вызывающего потока, поэтому частичный файл удаляет вызывающий.
pub fn pump<R: Read + Send + 'static>(
    open: impl FnOnce() -> Result<R, String> + Send + 'static,
    out: &mut dyn Write,
    interrupted: &dyn Fn() -> bool,
    stall: Duration,
) -> Result<u64, String> {
    let (sender, receiver) = mpsc::sync_channel::<io::Result<Vec<u8>>>(4);
    std::thread::spawn(move || {
        let mut source = match open() {
            Ok(source) => source,
            Err(e) => {
                let _ = sender.send(Err(io::Error::other(e)));
                return;
            }
        };
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let chunk = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => Ok(buffer[..n].to_vec()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => Err(e),
            };
            let failed = chunk.is_err();
            if sender.send(chunk).is_err() || failed {
                break;
            }
        }
    });
    let mut written = 0u64;
    let mut progress = Instant::now();
    loop {
        if interrupted() {
            return Err("загрузка архива прервана".into());
        }
        match receiver.recv_timeout(INTERRUPT_CHECK) {
            Ok(Ok(chunk)) => {
                out.write_all(&chunk).map_err(|e| e.to_string())?;
                written += chunk.len() as u64;
                progress = Instant::now();
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(written),
            Err(mpsc::RecvTimeoutError::Timeout) if progress.elapsed() >= stall => {
                return Err(format!(
                    "нет данных {stall:?}; загрузка остановлена после {written} байт"
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

pub struct HttpsDownloader;

impl Downloader for HttpsDownloader {
    fn download(
        &self,
        url: &str,
        out: &mut dyn Write,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<u64, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("anki-deck-toolkit-e2e")
            .connect_timeout(Duration::from_secs(30))
            // Общий срок не ограничивает медленный, но непрерывный поток.
            .timeout(None)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let host = attempt.url().host_str().unwrap_or("");
                let allowed = host == "github.com" || host.ends_with(".githubusercontent.com");
                if attempt.previous().len() > 5 || attempt.url().scheme() != "https" || !allowed {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| e.to_string())?;
        let request_url = url.to_owned();
        pump(
            move || {
                let response = client.get(&request_url).send().map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("HTTP {}", response.status()));
                }
                Ok(response)
            },
            out,
            interrupted,
            DOWNLOAD_STALL,
        )
        .map_err(|e| format!("{url}: {e}"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub reused_from_cache: bool,
    pub verification: String,
    pub quarantined: Vec<PathBuf>,
}

struct HashingWriter<'a> {
    inner: &'a mut fs::File,
    interrupted: &'a dyn Fn() -> bool,
    hasher: Sha256,
    written: u64,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if (self.interrupted)() {
            return Err(io::Error::other("загрузка архива прервана"));
        }
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Кэш `<root>/<версия>/linux-<arch>/<sha256>/<архив>`: ключ — фактический digest.
pub fn acquire(
    cache_root: &Path,
    resolved: &Resolved,
    offline_archive: Option<&Path>,
    downloader: Option<&dyn Downloader>,
    allow_unverified: bool,
) -> Result<Artifact, Stop> {
    acquire_with_interrupt(
        cache_root,
        resolved,
        offline_archive,
        downloader,
        allow_unverified,
        &|| false,
    )
}

pub fn acquire_interruptible(
    cache_root: &Path,
    resolved: &Resolved,
    offline_archive: Option<&Path>,
    downloader: Option<&dyn Downloader>,
    allow_unverified: bool,
    interrupted: &AtomicBool,
) -> Result<Artifact, Stop> {
    acquire_with_interrupt(
        cache_root,
        resolved,
        offline_archive,
        downloader,
        allow_unverified,
        &|| interrupted.load(Ordering::SeqCst),
    )
}

fn acquire_with_interrupt(
    cache_root: &Path,
    resolved: &Resolved,
    offline_archive: Option<&Path>,
    downloader: Option<&dyn Downloader>,
    allow_unverified: bool,
    interrupted: &dyn Fn() -> bool,
) -> Result<Artifact, Stop> {
    const STAGE: &str = "acquire_anki_artifact";
    let io_stop = |e: String| Stop::error(STAGE, "artifact_io", e);
    if interrupted() {
        return Err(Stop {
            status: crate::report::Status::Interrupted,
            stage: STAGE,
            reason: "interrupted".into(),
            message: "Загрузка архива Anki прервана пользователем".into(),
        });
    }
    let expected = match &resolved.upstream_sha256 {
        Some(sha) => sha.clone(),
        None if allow_unverified => String::new(),
        None => {
            return Err(Stop::blocked(
                STAGE,
                "upstream_checksum_unavailable",
                "Источник релиза не опубликовал SHA-256 артефакта; самостоятельно вычисленная сумма не является аутентификацией. Запуск возможен только с --allow-unverified-archive",
            ));
        }
    };
    let mut quarantined = Vec::new();
    if !expected.is_empty() {
        let dir = cache_root
            .join(&resolved.version)
            .join(format!("linux-{}", resolved.arch.as_str()))
            .join(&expected);
        let file = dir.join(&resolved.asset_name);
        if file.is_file() {
            let actual = sha256_file(&file).map_err(|e| io_stop(e.to_string()))?;
            if actual == expected {
                let size = fs::metadata(&file)
                    .map_err(|e| io_stop(e.to_string()))?
                    .len();
                return Ok(Artifact {
                    path: file,
                    sha256: actual,
                    size,
                    reused_from_cache: true,
                    verification: format!("cache_rehash_matches_{}", resolved.checksum_source),
                    quarantined,
                });
            }
            let moved = dir.join(format!(
                "{}.corrupt-{}",
                resolved.asset_name,
                util::utc_compact(util::unix_now())
            ));
            fs::rename(&file, &moved).map_err(|e| io_stop(e.to_string()))?;
            quarantined.push(moved);
        }
        if let Some(source) = offline_archive {
            let actual =
                sha256_file(source).map_err(|e| io_stop(format!("{}: {e}", source.display())))?;
            if actual != expected {
                return Err(Stop::error(
                    STAGE,
                    "archive_digest_mismatch",
                    format!("SHA-256 офлайн-архива {actual} не совпадает с заданным {expected}"),
                ));
            }
            fs::create_dir_all(&dir).map_err(|e| io_stop(e.to_string()))?;
            fs::copy(source, &file).map_err(|e| io_stop(e.to_string()))?;
            let size = fs::metadata(&file)
                .map_err(|e| io_stop(e.to_string()))?
                .len();
            return Ok(Artifact {
                path: file,
                sha256: actual,
                size,
                reused_from_cache: false,
                verification: "offline_archive_matches_user_supplied_sha256".into(),
                quarantined,
            });
        }
    }
    let (Some(url), Some(downloader)) = (&resolved.asset_url, downloader) else {
        return Err(Stop::blocked(
            STAGE,
            "offline_artifact_missing",
            "Офлайн-режим: проверенного артефакта нет ни в кэше, ни в --anki-archive",
        ));
    };
    let staging = cache_root.join(".partial");
    fs::create_dir_all(&staging).map_err(|e| io_stop(e.to_string()))?;
    let partial = staging.join(format!(
        "{}-{}",
        resolved.asset_name,
        util::random_hex(6).map_err(io_stop)?
    ));
    let mut file = fs::File::create(&partial).map_err(|e| io_stop(e.to_string()))?;
    let mut writer = HashingWriter {
        inner: &mut file,
        interrupted,
        hasher: Sha256::new(),
        written: 0,
    };
    let result = downloader.download(url, &mut writer, interrupted);
    let actual = util::hex(&writer.hasher.clone().finalize());
    let written = writer.written;
    drop(file);
    let cleanup = |p: &Path| {
        let _ = fs::remove_file(p);
    };
    if interrupted() {
        cleanup(&partial);
        return Err(Stop {
            status: crate::report::Status::Interrupted,
            stage: STAGE,
            reason: "interrupted".into(),
            message: "Загрузка архива Anki прервана пользователем".into(),
        });
    }
    if let Err(e) = result {
        cleanup(&partial);
        return Err(Stop::error(STAGE, "artifact_download_failed", e));
    }
    if resolved.asset_size.is_some_and(|size| size != written) {
        cleanup(&partial);
        return Err(Stop::error(
            STAGE,
            "archive_size_mismatch",
            format!("Получено {written} байт вместо {:?}", resolved.asset_size),
        ));
    }
    if !expected.is_empty() && actual != expected {
        cleanup(&partial);
        return Err(Stop::error(
            STAGE,
            "archive_digest_mismatch",
            format!("SHA-256 загрузки {actual} не совпадает с опубликованным {expected}"),
        ));
    }
    let dir = cache_root
        .join(&resolved.version)
        .join(format!("linux-{}", resolved.arch.as_str()))
        .join(&actual);
    fs::create_dir_all(&dir).map_err(|e| io_stop(e.to_string()))?;
    let file = dir.join(&resolved.asset_name);
    fs::rename(&partial, &file).map_err(|e| io_stop(e.to_string()))?;
    util::write_json(
        &dir.join("provenance.json"),
        &serde_json::json!({"url":url,"sha256":actual,"size":written,"checksum_source":resolved.checksum_source,
            "downloaded_at":util::utc_rfc3339(util::unix_now())}),
    )
    .map_err(io_stop)?;
    Ok(Artifact {
        path: file,
        sha256: actual,
        size: written,
        reused_from_cache: false,
        verification: if expected.is_empty() {
            "unverified_self_computed_sha256".into()
        } else {
            format!("download_matches_{}", resolved.checksum_source)
        },
        quarantined,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn release(
        tag: &str,
        draft: bool,
        prerelease: bool,
        linux: bool,
        digest: Option<&str>,
    ) -> ReleaseJson {
        let mut assets = vec![AssetJson {
            name: format!("anki-{tag}-win-x64.msi"),
            size: 1,
            browser_download_url: format!("{DOWNLOAD_PREFIX}{tag}/anki-{tag}-win-x64.msi"),
            digest: None,
        }];
        if linux {
            for arch in ["x86_64", "aarch64"] {
                let name = format!("anki-{tag}-linux-{arch}.tar.zst");
                assets.push(AssetJson {
                    browser_download_url: format!("{DOWNLOAD_PREFIX}{tag}/{name}"),
                    name,
                    size: 5,
                    digest: digest.map(str::to_owned),
                });
            }
        }
        ReleaseJson {
            tag_name: tag.into(),
            draft,
            prerelease,
            published_at: None,
            html_url: None,
            assets,
        }
    }

    struct Fake {
        releases: Vec<ReleaseJson>,
        fail: bool,
        pages: Cell<u32>,
    }
    impl ReleaseSource for Fake {
        fn page(&self, page: u32) -> Result<Vec<ReleaseJson>, String> {
            self.pages.set(self.pages.get() + 1);
            if self.fail {
                return Err("network unreachable".into());
            }
            let start = (page as usize - 1) * PAGE_SIZE;
            Ok(self
                .releases
                .iter()
                .skip(start)
                .take(PAGE_SIZE)
                .cloned()
                .collect())
        }
        fn by_tag(&self, tag: &str) -> Result<Option<ReleaseJson>, String> {
            if self.fail {
                return Err("network unreachable".into());
            }
            Ok(self.releases.iter().find(|r| r.tag_name == tag).cloned())
        }
    }
    fn fake(releases: Vec<ReleaseJson>) -> Fake {
        Fake {
            releases,
            fail: false,
            pages: Cell::new(0),
        }
    }

    fn betas(count: usize) -> Vec<ReleaseJson> {
        (0..count)
            .map(|i| release(&format!("27.{i:02}b1"), false, true, true, Some(D)))
            .collect()
    }

    const D: &str = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn latest_picks_highest_stable_and_skips_draft_prerelease_beta() {
        let source = fake(vec![
            release("26.10b1", false, true, true, Some(D)),
            release("26.11", true, false, true, Some(D)),
            release("26.09.10", false, false, true, Some(D)),
            release("26.09.3", false, false, true, Some(D)),
            release("26.09.1", false, true, true, Some(D)),
            release("26.09rc1", false, false, true, Some(D)),
        ]);
        let r = resolve(&source, &Request::Latest, Arch::X86_64).unwrap();
        assert_eq!(r.mode, "latest_stable");
        assert_eq!(r.version, "26.09.10");
        assert_eq!(r.latest_stable.as_deref(), Some("26.09.10"));
        assert_eq!(r.asset_name, "anki-26.09.10-linux-x86_64.tar.zst");
        assert_eq!(r.upstream_sha256.as_deref(), Some(&D[7..]));
        assert_eq!(r.checksum_source, "github_release_asset_digest");
    }

    #[test]
    fn numeric_maximum_on_later_page_wins_over_newer_published_backport() {
        // Страница 1 (новые по дате): 99 бета и позднее опубликованный бэкпорт 25.02.8.
        let mut releases = betas(99);
        releases.push(release("25.02.8", false, false, true, Some(D)));
        releases.push(release("26.09.3", false, false, true, Some(D)));
        releases.push(release("26.09.2", false, false, true, Some(D)));
        let source = fake(releases);
        let r = resolve(&source, &Request::Latest, Arch::X86_64).unwrap();
        assert_eq!(r.version, "26.09.3");
        assert_eq!(source.pages.get(), 2);
        assert!(r.decision.contains("102 релизов"), "{}", r.decision);
    }

    #[test]
    fn release_index_page_boundaries() {
        // Ровно полная страница требует чтения следующей, пустой.
        let mut exact = betas(99);
        exact.push(release("26.09.3", false, false, true, Some(D)));
        let source = fake(exact);
        assert_eq!(
            resolve(&source, &Request::Latest, Arch::X86_64)
                .unwrap()
                .version,
            "26.09.3"
        );
        assert_eq!(source.pages.get(), 2);

        // Индекс длиннее бюджета: максимум не подтверждён, старая версия не выбирается.
        let mut huge = betas(PAGE_SIZE * MAX_PAGES as usize - 1);
        huge.push(release("26.09.3", false, false, true, Some(D)));
        huge.push(release("26.10", false, false, true, Some(D)));
        let source = fake(huge);
        let stop = resolve(&source, &Request::Latest, Arch::X86_64).unwrap_err();
        assert_eq!(stop.reason, "release_index_unavailable");
        assert!(stop.message.contains("не подтверждена"), "{}", stop.message);
        assert_eq!(source.pages.get(), MAX_PAGES);

        // Явная версия при неполном индексе выбирается, а свежесть помечена ошибкой.
        let explicit = Request::Explicit {
            version: "26.09.3".into(),
            allow_prerelease: false,
        };
        let r = resolve(&source, &explicit, Arch::X86_64).unwrap();
        assert_eq!(
            (r.version.as_str(), r.latest_stable.as_deref()),
            ("26.09.3", None)
        );
        assert!(r.latest_stable_error.is_some());
    }

    #[test]
    fn newest_stable_without_linux_artifact_blocks_instead_of_falling_back() {
        let source = fake(vec![
            release("26.10", false, false, false, None),
            release("26.09.3", false, false, true, Some(D)),
        ]);
        let stop = resolve(&source, &Request::Latest, Arch::X86_64).unwrap_err();
        assert_eq!(stop.reason, "release_without_linux_artifact");
        assert!(stop.message.contains("26.10"));
    }

    #[test]
    fn network_failure_is_an_error_not_a_cached_or_old_version() {
        let source = Fake {
            releases: vec![],
            fail: true,
            pages: Cell::new(0),
        };
        let stop = resolve(&source, &Request::Latest, Arch::X86_64).unwrap_err();
        assert_eq!(stop.reason, "release_index_unavailable");
        assert_eq!(stop.status, crate::report::Status::Error);
    }

    #[test]
    fn no_stable_release_is_blocked() {
        let source = fake(vec![release("27.01b1", false, true, true, Some(D))]);
        assert_eq!(
            resolve(&source, &Request::Latest, Arch::X86_64)
                .unwrap_err()
                .reason,
            "no_stable_release"
        );
    }

    #[test]
    fn explicit_version_is_honoured_and_reports_detected_latest() {
        let source = fake(vec![
            release("26.09.3", false, false, true, Some(D)),
            release("26.09.2", false, false, true, Some(D)),
            release("26.09.1", false, true, true, Some(D)),
        ]);
        let request = Request::Explicit {
            version: "26.09.2".into(),
            allow_prerelease: false,
        };
        let r = resolve(&source, &request, Arch::Aarch64).unwrap();
        assert_eq!((r.mode, r.version.as_str()), ("explicit", "26.09.2"));
        assert_eq!(r.latest_stable.as_deref(), Some("26.09.3"));
        assert_eq!(r.asset_name, "anki-26.09.2-linux-aarch64.tar.zst");

        let pre = Request::Explicit {
            version: "26.09.1".into(),
            allow_prerelease: false,
        };
        assert_eq!(
            resolve(&source, &pre, Arch::X86_64).unwrap_err().reason,
            "prerelease_requires_flag"
        );
        let missing = Request::Explicit {
            version: "1.0".into(),
            allow_prerelease: false,
        };
        assert_eq!(
            resolve(&source, &missing, Arch::X86_64).unwrap_err().reason,
            "requested_release_not_found"
        );
    }

    #[test]
    fn unofficial_download_url_is_rejected() {
        let mut r = release("26.09.3", false, false, true, Some(D));
        r.assets[1].browser_download_url = "https://mirror.example/anki.tar.zst".into();
        let stop = resolve(&fake(vec![r]), &Request::Latest, Arch::X86_64).unwrap_err();
        assert_eq!(stop.reason, "unexpected_artifact_url");
    }

    #[test]
    fn version_and_digest_parsing() {
        assert_eq!(stable_version("26.09.3"), Some(vec![26, 9, 3]));
        assert!(stable_version("26.09b3").is_none());
        assert!(stable_version("26").is_none());
        assert!(stable_version("26..1").is_none());
        assert!(stable_version("26.09.3") > stable_version("26.09"));
        assert!(parse_digest(Some("md5:00")).is_none());
        assert!(parse_digest(Some("sha256:zz")).is_none());
        assert!(parse_digest(None).is_none());
    }

    struct Bytes(Vec<u8>, Cell<u32>);
    impl Downloader for Bytes {
        fn download(
            &self,
            _url: &str,
            out: &mut dyn Write,
            interrupted: &dyn Fn() -> bool,
        ) -> Result<u64, String> {
            self.1.set(self.1.get() + 1);
            let bytes = self.0.clone();
            pump(
                move || Ok(io::Cursor::new(bytes)),
                out,
                interrupted,
                Duration::from_secs(5),
            )
        }
    }

    /// Сеть отдала начало архива и перестала отвечать внутри `read()`.
    struct Stalled;
    impl Read for Stalled {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            static SENT: AtomicBool = AtomicBool::new(false);
            if !SENT.swap(true, Ordering::SeqCst) {
                buf[..4].copy_from_slice(b"part");
                return Ok(4);
            }
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    struct StalledDownload<'a>(&'a AtomicBool, Duration);
    impl Downloader for StalledDownload<'_> {
        fn download(
            &self,
            _url: &str,
            out: &mut dyn Write,
            interrupted: &dyn Fn() -> bool,
        ) -> Result<u64, String> {
            let flag = self.0;
            let started = Instant::now();
            // Ctrl+C приходит, пока поток чтения заблокирован.
            pump(
                || Ok(Stalled),
                out,
                &|| {
                    if started.elapsed() > Duration::from_millis(300) {
                        flag.store(true, Ordering::SeqCst);
                    }
                    interrupted()
                },
                self.1,
            )
        }
    }

    fn resolved_for(bytes: &[u8]) -> Resolved {
        let digest = format!("sha256:{}", util::sha256_hex(bytes));
        let mut r = release("26.09.3", false, false, true, Some(&digest));
        r.assets[1].size = bytes.len() as u64;
        resolve(&fake(vec![r]), &Request::Latest, Arch::X86_64).unwrap()
    }

    struct InterruptAfterChunk<'a>(&'a AtomicBool);
    impl Downloader for InterruptAfterChunk<'_> {
        fn download(
            &self,
            _url: &str,
            out: &mut dyn Write,
            _interrupted: &dyn Fn() -> bool,
        ) -> Result<u64, String> {
            out.write_all(b"partial").map_err(|e| e.to_string())?;
            self.0.store(true, Ordering::SeqCst);
            out.write_all(b"must not be written")
                .map_err(|e| e.to_string())?;
            Ok(26)
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-test-{name}-{}",
            util::random_hex(6).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_reuses_verified_archive_and_quarantines_corruption() {
        let root = temp("cache");
        let bytes = b"official archive bytes".to_vec();
        let r = resolved_for(&bytes);
        let dl = Bytes(bytes.clone(), Cell::new(0));
        let first = acquire(&root, &r, None, Some(&dl), false).unwrap();
        assert!(!first.reused_from_cache);
        assert_eq!(
            first.verification,
            "download_matches_github_release_asset_digest"
        );
        let second = acquire(&root, &r, None, Some(&dl), false).unwrap();
        assert!(second.reused_from_cache);
        assert_eq!(dl.1.get(), 1, "повторная загрузка той же версии не нужна");

        fs::write(&first.path, b"tampered archive bytes").unwrap();
        let third = acquire(&root, &r, None, Some(&dl), false).unwrap();
        assert!(!third.reused_from_cache);
        assert_eq!(third.quarantined.len(), 1);
        assert_eq!(fs::read(&third.path).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn substituted_download_is_rejected_and_not_cached() {
        let root = temp("mismatch");
        let r = resolved_for(b"official archive bytes");
        let dl = Bytes(b"substituted  archive!!".to_vec(), Cell::new(0));
        let stop = acquire(&root, &r, None, Some(&dl), false).unwrap_err();
        assert_eq!(stop.reason, "archive_digest_mismatch");
        let short = Bytes(b"short".to_vec(), Cell::new(0));
        assert_eq!(
            acquire(&root, &r, None, Some(&short), false)
                .unwrap_err()
                .reason,
            "archive_size_mismatch"
        );
        assert!(
            fs::read_dir(root.join(".partial"))
                .unwrap()
                .next()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_download_removes_partial_archive_and_reports_interrupted() {
        let root = temp("interrupted");
        let r = resolved_for(b"official archive bytes");
        let interrupted = AtomicBool::new(false);
        let downloader = InterruptAfterChunk(&interrupted);

        let stop = acquire_interruptible(&root, &r, None, Some(&downloader), false, &interrupted)
            .unwrap_err();

        assert_eq!(stop.status, crate::report::Status::Interrupted);
        assert_eq!(stop.reason, "interrupted");
        assert!(
            fs::read_dir(root.join(".partial"))
                .unwrap()
                .next()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupt_during_blocked_network_read_is_prompt_and_removes_partial() {
        let root = temp("stalled-interrupt");
        let r = resolved_for(b"official archive bytes");
        let interrupted = AtomicBool::new(false);
        let started = Instant::now();
        let stop = acquire_interruptible(
            &root,
            &r,
            None,
            Some(&StalledDownload(&interrupted, Duration::from_secs(3600))),
            false,
            &interrupted,
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(stop.status, crate::report::Status::Interrupted);
        assert!(
            fs::read_dir(root.join(".partial"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            !root.join("26.09.3").exists(),
            "частичный архив не попал в кэш"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stalled_download_without_progress_fails_instead_of_hanging() {
        let mut out = Vec::new();
        let started = Instant::now();
        let error = pump(
            || Ok(io::repeat(0).take(0).chain(BlockedRead)),
            &mut out,
            &|| false,
            Duration::from_millis(400),
        )
        .unwrap_err();
        assert!(error.contains("нет данных"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stalled_response_headers_are_subject_to_progress_timeout_and_interruption() {
        for interrupt in [false, true] {
            let (sender, receiver) = mpsc::channel();
            let started = Instant::now();
            let result = pump(
                move || {
                    receiver.recv().map_err(|e| e.to_string())?;
                    Ok(io::Cursor::new(Vec::<u8>::new()))
                },
                &mut Vec::new(),
                &|| interrupt && started.elapsed() > Duration::from_millis(50),
                Duration::from_millis(400),
            );
            // Освобождаем поток открытия после возврата pump.
            sender.send(()).unwrap();
            let error = result.unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(5));
            if interrupt {
                assert!(error.contains("прервана"), "{error}");
            } else {
                assert!(error.contains("нет данных"), "{error}");
            }
        }
    }

    #[test]
    fn continuously_progressing_download_can_outlast_the_stall_interval() {
        struct Slow(u8);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(100));
                self.0 -= 1;
                buf[0] = self.0;
                Ok(1)
            }
        }
        let started = Instant::now();
        let stall = Duration::from_millis(500);
        let mut out = Vec::new();
        assert_eq!(pump(|| Ok(Slow(8)), &mut out, &|| false, stall).unwrap(), 8);
        assert_eq!(out, [7, 6, 5, 4, 3, 2, 1, 0]);
        assert!(started.elapsed() > stall);
    }

    struct BlockedRead;
    impl Read for BlockedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_secs(3600));
            Ok(0)
        }
    }

    #[test]
    fn missing_upstream_checksum_requires_explicit_consent() {
        let root = temp("nodigest");
        let r = resolve(
            &fake(vec![release("26.09.3", false, false, true, None)]),
            &Request::Latest,
            Arch::X86_64,
        )
        .unwrap();
        assert_eq!(r.checksum_source, "unavailable");
        let dl = Bytes(vec![0; 5], Cell::new(0));
        assert_eq!(
            acquire(&root, &r, None, Some(&dl), false)
                .unwrap_err()
                .reason,
            "upstream_checksum_unavailable"
        );
        let a = acquire(&root, &r, None, Some(&dl), true).unwrap();
        assert_eq!(a.verification, "unverified_self_computed_sha256");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_mode_uses_only_preverified_artifact() {
        let root = temp("offline");
        let bytes = b"offline archive".to_vec();
        let sha = util::sha256_hex(&bytes);
        let request = Request::Offline {
            version: "26.09.2".into(),
            sha256: sha.clone(),
            archive: None,
        };
        let source = Fake {
            releases: vec![],
            fail: true,
            pages: Cell::new(0),
        };
        let r = resolve(&source, &request, Arch::X86_64).unwrap();
        assert_eq!(source.pages.get(), 0, "офлайн-режим не обращается к сети");
        assert_eq!(
            acquire(&root, &r, None, None, false).unwrap_err().reason,
            "offline_artifact_missing"
        );
        let file = root.join("given.tar.zst");
        fs::write(&file, &bytes).unwrap();
        let a = acquire(&root, &r, Some(&file), None, false).unwrap();
        assert_eq!(a.sha256, sha);
        assert!(
            acquire(&root, &r, None, None, false)
                .unwrap()
                .reused_from_cache
        );
        fs::write(&file, b"other").unwrap();
        let mut r2 = r.clone();
        r2.version = "26.09.1".into();
        assert_eq!(
            acquire(&root, &r2, Some(&file), None, false)
                .unwrap_err()
                .reason,
            "archive_digest_mismatch"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
