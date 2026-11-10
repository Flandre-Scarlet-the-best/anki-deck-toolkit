//! Обнаружение доступного артефакта у живого upstream и его скачивание.
//!
//! Версия, имя файла, ссылка и опубликованная контрольная сумма берутся из
//! ответа upstream при каждой проверке; в коде нет «последней версии».
//! Сетевой доступ идёт через [`Transport`]: настоящий [`HttpTransport`]
//! допускает только HTTPS и редиректы в пределах известных хостов GitHub,
//! тесты подставляют собственную реализацию.

use crate::catalog::{Discovery, Source};
use crate::error::{Code, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1_checked::Sha1;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Хосты, на которые допускаются запросы и редиректы.
pub const ALLOWED_HOSTS: [&str; 5] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "raw.githubusercontent.com",
];

/// Предел ответа API (JSON релиза или листинга каталога).
const MAX_API_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    Sha256,
    /// SHA-1 git-объекта `blob <size>\0<bytes>`, как его публикует GitHub.
    GitBlobSha1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub algorithm: ChecksumAlgorithm,
    pub value: String,
}

/// Точная идентичность артефакта у upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtifact {
    /// `github_release`, `github_repository_file` или `local_file`.
    pub provider: String,
    /// `owner/repo` или путь локального файла.
    pub locator: String,
    /// Тег релиза или дата в имени файла; `None`, если не известна.
    pub release: Option<String>,
    pub asset_name: String,
    pub download_url: Option<String>,
    pub size: Option<u64>,
    pub published_at: Option<String>,
    /// Опубликованная upstream контрольная сумма, если есть.
    pub checksum: Option<Checksum>,
}

/// Хеши фактически полученного файла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDigest {
    pub bytes: u64,
    pub sha256: String,
    pub git_blob_sha1: String,
    /// SHA-1 обнаружила в данных признаки атаки коллизией (SHAttered и
    /// подобные): такой git blob SHA-1 не доказывает целостность.
    #[serde(skip)]
    pub sha1_collision: bool,
}

impl FileDigest {
    pub fn matches(&self, checksum: &Checksum) -> bool {
        match checksum.algorithm {
            ChecksumAlgorithm::Sha256 => self.sha256 == checksum.value,
            ChecksumAlgorithm::GitBlobSha1 => {
                !self.sha1_collision && self.git_blob_sha1 == checksum.value
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 и git blob SHA-1 файла за один проход чтения.
///
/// SHA-1 нужна только для сверки с суммой, которую публикует GitHub, и
/// считается с обнаружением коллизий, как в самом Git.
pub fn digest_file(path: &Path) -> Result<FileDigest> {
    let size = std::fs::metadata(path)
        .map_err(|e| Error::io(path.display(), e))?
        .len();
    let mut sha256 = Sha256::new();
    let mut sha1 = Sha1::builder().safe_hash(false).build();
    sha1_checked::Digest::update(&mut sha1, format!("blob {size}\0").as_bytes());
    let mut file = File::open(path).map_err(|e| Error::io(path.display(), e))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| Error::io(path.display(), e))?;
        if n == 0 {
            break;
        }
        sha256.update(&buffer[..n]);
        sha1_checked::Digest::update(&mut sha1, &buffer[..n]);
        total += n as u64;
    }
    if total != size {
        return Err(Error::new(
            Code::Io,
            format!("{} изменился во время чтения", path.display()),
        ));
    }
    let sha1 = sha1.try_finalize();
    Ok(FileDigest {
        bytes: total,
        sha256: hex(&sha256.finalize()),
        git_blob_sha1: hex(sha1.hash().as_slice()),
        sha1_collision: sha1.has_collision(),
    })
}

/// Сетевой доступ менеджера.
pub trait Transport {
    /// Небольшой ответ (JSON API) целиком, не больше `limit` байт.
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>>;
    /// Потоковое скачивание в `sink`, не больше `limit` байт; возвращает число байт.
    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64>;
}

fn host_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(|h| ALLOWED_HOSTS.contains(&h))
}

/// Настоящий HTTPS-клиент.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!(
                "anki-deck-toolkit-japanese-lexicon/",
                env!("CARGO_PKG_VERSION")
            ))
            .https_only(true)
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(900))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 || !host_allowed(attempt.url()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| Error::new(Code::UpstreamUnavailable, format!("HTTP-клиент: {e}")))?;
        Ok(Self { client })
    }

    fn send(&self, url: &str) -> Result<reqwest::blocking::Response> {
        let parsed = reqwest::Url::parse(url).map_err(|e| {
            Error::new(Code::UpstreamUnexpected, format!("неверный URL {url}: {e}"))
        })?;
        if !host_allowed(&parsed) {
            return Err(Error::new(
                Code::UpstreamUnexpected,
                format!("URL {url} вне разрешённых HTTPS-хостов upstream"),
            ));
        }
        let response = self
            .client
            .get(parsed)
            .header(
                "Accept",
                "application/vnd.github+json, application/zip, */*",
            )
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(|e| {
                Error::new(
                    Code::UpstreamUnavailable,
                    format!("{url}: сеть или upstream недоступны: {e}"),
                )
            })?;
        if !host_allowed(response.url()) {
            return Err(Error::new(
                Code::UpstreamUnexpected,
                format!("{url}: редирект на неразрешённый адрес {}", response.url()),
            ));
        }
        let status = response.status();
        if status.is_redirection() {
            return Err(Error::new(
                Code::UpstreamUnexpected,
                format!("{url}: редирект отклонён политикой (HTTPS и хосты GitHub)"),
            ));
        }
        if status.is_success() {
            return Ok(response);
        }
        let rate_limited = status.as_u16() == 429
            || (status.as_u16() == 403
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"));
        let code = if rate_limited {
            Code::UpstreamRateLimited
        } else if status.is_server_error() {
            Code::UpstreamUnavailable
        } else {
            Code::UpstreamUnexpected
        };
        Err(Error::new(code, format!("{url}: HTTP {status}")))
    }
}

fn copy_limited(mut from: impl Read, sink: &mut dyn Write, limit: u64, url: &str) -> Result<u64> {
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = from.read(&mut buffer).map_err(|e| {
            Error::new(
                Code::UpstreamUnavailable,
                format!("{url}: обрыв загрузки: {e}"),
            )
        })?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
        if total > limit {
            return Err(Error::limit(format!(
                "{url}: ответ больше предела {limit} байт"
            )));
        }
        sink.write_all(&buffer[..n])
            .map_err(|e| Error::io("запись загрузки", e))?;
    }
}

impl Transport for HttpTransport {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let response = self.send(url)?;
        let mut out = Vec::new();
        copy_limited(response, &mut out, limit, url)?;
        Ok(out)
    }

    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64> {
        let response = self.send(url)?;
        if let Some(length) = response.content_length()
            && length > limit
        {
            return Err(Error::limit(format!(
                "{url}: заявлено {length} байт, предел {limit}"
            )));
        }
        copy_limited(response, sink, limit, url)
    }
}

fn json(transport: &dyn Transport, url: &str) -> Result<Value> {
    let bytes = transport.get(url, MAX_API_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|e| {
        Error::new(
            Code::UpstreamUnexpected,
            format!("{url}: ответ не JSON: {e}"),
        )
    })
}

fn unexpected(message: impl Into<String>) -> Error {
    Error::new(Code::UpstreamUnexpected, message)
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn valid_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Обнаруживает доступный артефакт источника.
pub fn discover(transport: &dyn Transport, source: &Source) -> Result<RemoteArtifact> {
    match source.discovery {
        Discovery::GithubLatestRelease { owner, repo, asset } => {
            let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let release = json(transport, &url)?;
            if release.get("draft").and_then(Value::as_bool) == Some(true)
                || release.get("prerelease").and_then(Value::as_bool) == Some(true)
            {
                return Err(unexpected(format!(
                    "{url}: последний релиз — draft или prerelease"
                )));
            }
            let tag = release
                .get("tag_name")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| unexpected(format!("{url}: нет tag_name")))?;
            let found = release
                .get("assets")
                .and_then(Value::as_array)
                .and_then(|assets| {
                    assets
                        .iter()
                        .find(|a| a.get("name").and_then(Value::as_str) == Some(asset))
                })
                .ok_or_else(|| {
                    unexpected(format!(
                        "релиз {tag} репозитория {owner}/{repo} не содержит ассет {asset}"
                    ))
                })?;
            let download_url = found
                .get("browser_download_url")
                .and_then(Value::as_str)
                .ok_or_else(|| unexpected(format!("ассет {asset}: нет browser_download_url")))?;
            let expected_prefix = format!("https://github.com/{owner}/{repo}/releases/download/");
            if !download_url.starts_with(&expected_prefix)
                || !download_url.ends_with(&format!("/{asset}"))
            {
                return Err(unexpected(format!(
                    "ассет {asset}: неожиданная ссылка скачивания {download_url}"
                )));
            }
            let checksum = match found.get("digest").and_then(Value::as_str) {
                Some(digest) => {
                    let value = digest.strip_prefix("sha256:").ok_or_else(|| {
                        unexpected(format!(
                            "ассет {asset}: неизвестный алгоритм digest {digest}"
                        ))
                    })?;
                    if !is_hex(value, 64) {
                        return Err(unexpected(format!(
                            "ассет {asset}: неверный digest {digest}"
                        )));
                    }
                    Some(Checksum {
                        algorithm: ChecksumAlgorithm::Sha256,
                        value: value.to_owned(),
                    })
                }
                None => None,
            };
            Ok(RemoteArtifact {
                provider: "github_release".into(),
                locator: format!("{owner}/{repo}"),
                release: Some(tag.to_owned()),
                asset_name: asset.to_owned(),
                download_url: Some(download_url.to_owned()),
                size: found.get("size").and_then(Value::as_u64),
                published_at: release
                    .get("published_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                checksum,
            })
        }
        Discovery::GithubDatedFile {
            owner,
            repo,
            branch,
            directory,
            prefix,
            suffix,
        } => {
            let url = format!(
                "https://api.github.com/repos/{owner}/{repo}/contents/{directory}?ref={branch}"
            );
            let listing = json(transport, &url)?;
            let items = listing
                .as_array()
                .ok_or_else(|| unexpected(format!("{url}: ожидается список файлов")))?;
            let best = items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("file"))
                .filter_map(|i| {
                    let name = i.get("name").and_then(Value::as_str)?;
                    let date = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
                    valid_date(date).then(|| (date.to_owned(), i))
                })
                .max_by(|a, b| a.0.cmp(&b.0))
                .ok_or_else(|| {
                    unexpected(format!(
                        "в {owner}/{repo}/{directory} нет файла {prefix}YYYY-MM-DD{suffix}"
                    ))
                })?;
            let (date, item) = best;
            let name = format!("{prefix}{date}{suffix}");
            let expected = format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{directory}/{name}"
            );
            let download_url = item.get("download_url").and_then(Value::as_str);
            if download_url != Some(expected.as_str()) {
                return Err(unexpected(format!(
                    "файл {name}: неожиданная ссылка скачивания {download_url:?}"
                )));
            }
            let sha = item
                .get("sha")
                .and_then(Value::as_str)
                .filter(|s| is_hex(s, 40))
                .ok_or_else(|| unexpected(format!("файл {name}: нет git blob sha")))?;
            Ok(RemoteArtifact {
                provider: "github_repository_file".into(),
                locator: format!("{owner}/{repo}@{branch}:{directory}"),
                release: Some(date),
                asset_name: name,
                download_url: Some(expected),
                size: item.get("size").and_then(Value::as_u64),
                published_at: None,
                checksum: Some(Checksum {
                    algorithm: ChecksumAlgorithm::GitBlobSha1,
                    value: sha.to_owned(),
                }),
            })
        }
    }
}

/// Скачивает артефакт в `dest` и проверяет размер и опубликованную сумму.
/// Возвращает хеши фактически полученного файла.
pub fn fetch(
    transport: &dyn Transport,
    source: &Source,
    artifact: &RemoteArtifact,
    dest: &Path,
) -> Result<FileDigest> {
    let url = artifact
        .download_url
        .as_deref()
        .ok_or_else(|| unexpected("у артефакта нет ссылки скачивания"))?;
    if let Some(size) = artifact.size
        && size > source.max_archive_bytes
    {
        return Err(Error::limit(format!(
            "{}: upstream заявляет {size} байт, предел {}",
            artifact.asset_name, source.max_archive_bytes
        )));
    }
    {
        let mut file = File::create_new(dest).map_err(|e| Error::io(dest.display(), e))?;
        transport.download(url, &mut file, source.max_archive_bytes)?;
        file.sync_all().map_err(|e| Error::io(dest.display(), e))?;
    }
    let digest = digest_file(dest)?;
    if let Some(size) = artifact.size
        && size != digest.bytes
    {
        return Err(Error::new(
            Code::ChecksumMismatch,
            format!(
                "{}: получено {} байт, upstream заявляет {size}",
                artifact.asset_name, digest.bytes
            ),
        ));
    }
    if let Some(checksum) = &artifact.checksum
        && !digest.matches(checksum)
    {
        let reason =
            if digest.sha1_collision && checksum.algorithm == ChecksumAlgorithm::GitBlobSha1 {
                "в данных обнаружена атака коллизией SHA-1"
            } else {
                "не совпала с опубликованной upstream"
            };
        return Err(Error::new(
            Code::ChecksumMismatch,
            format!(
                "{}: контрольная сумма {:?} {reason}",
                artifact.asset_name, checksum.algorithm
            ),
        ));
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_blob_sha1_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        let dir = std::env::temp_dir().join(format!("jl-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        std::fs::write(&file, b"hello\n").unwrap();
        let d = digest_file(&file).unwrap();
        assert_eq!(d.git_blob_sha1, "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(
            d.sha256,
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sha1_collision_rejects_git_blob_checksum() {
        let digest = FileDigest {
            bytes: 6,
            sha256: "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03".into(),
            git_blob_sha1: "ce013625030ba8dba906f756967f9e9ca394464a".into(),
            sha1_collision: true,
        };
        let sha1 = Checksum {
            algorithm: ChecksumAlgorithm::GitBlobSha1,
            value: digest.git_blob_sha1.clone(),
        };
        let sha256 = Checksum {
            algorithm: ChecksumAlgorithm::Sha256,
            value: digest.sha256.clone(),
        };
        assert!(!digest.matches(&sha1));
        assert!(digest.matches(&sha256));
        let clean = FileDigest {
            sha1_collision: false,
            ..digest
        };
        assert!(clean.matches(&sha1));
    }

    #[test]
    fn dates_are_strict() {
        assert!(valid_date("2024-10-13"));
        assert!(!valid_date("2024-10-1"));
        assert!(!valid_date("2024_10_13"));
    }
}
