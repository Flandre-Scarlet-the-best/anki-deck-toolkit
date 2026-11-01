//! Поддерживаемые Chromium-браузеры: обнаружение установок, проверка
//! разновидности по самому исполняемому файлу и вычисляемый ID распакованного
//! расширения.
//!
//! Браузеры различаются только обнаружением, развёртыванием и подсказками
//! пользователю. Транспорт, выбор записи, загрузка MP3 и пакеты общие.
use super::{Platform, bridge_error, linux_to_windows, wsl_to_linux};
use crate::AssetError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Предел чтения исполняемого файла Windows при разборе ресурса версии.
const MAX_PE_BYTES: u64 = 128 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum BrowserKind {
    Brave,
    Chrome,
    Edge,
    Vivaldi,
    /// Открытые и тестовые сборки (Chromium, Chrome for Testing). Используются
    /// изолированным стендом; пользовательская поддержка не заявляется.
    Chromium,
}

impl BrowserKind {
    /// Браузеры, для которых документирована пользовательская настройка.
    pub const USER: [BrowserKind; 4] = [Self::Brave, Self::Chrome, Self::Edge, Self::Vivaldi];
    pub const ALL: [BrowserKind; 5] = [
        Self::Brave,
        Self::Chrome,
        Self::Edge,
        Self::Vivaldi,
        Self::Chromium,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Vivaldi => "vivaldi",
            Self::Chromium => "chromium",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == text)
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Brave => "Brave",
            Self::Chrome => "Google Chrome",
            Self::Edge => "Microsoft Edge",
            Self::Vivaldi => "Vivaldi",
            Self::Chromium => "Chromium",
        }
    }

    /// Страница расширений, где выполняется «Загрузить распакованное».
    pub fn extensions_uri(self) -> &'static str {
        match self {
            Self::Brave => "brave://extensions",
            Self::Edge => "edge://extensions",
            Self::Vivaldi => "vivaldi://extensions",
            Self::Chrome | Self::Chromium => "chrome://extensions",
        }
    }

    /// Страница настройки «Спрашивать, куда сохранять каждый файл».
    pub fn downloads_settings_uri(self) -> &'static str {
        match self {
            Self::Brave => "brave://settings/downloads",
            Self::Edge => "edge://settings/downloads",
            Self::Vivaldi => "vivaldi://settings/downloads",
            Self::Chrome | Self::Chromium => "chrome://settings/downloads",
        }
    }

    /// Короткое точное действие первичной установки распакованного расширения.
    pub fn load_unpacked_action(self, path: &str) -> String {
        let developer = match self {
            Self::Edge => "включите «Режим разработчика» (Developer mode) слева внизу",
            _ => "включите «Режим разработчика» (Developer mode)",
        };
        format!(
            "Откройте {} в {}, {developer}, нажмите «Загрузить распакованное» (Load unpacked) и выберите {path}",
            self.extensions_uri(),
            self.display()
        )
    }

    /// Исполняемые файлы относительно `Program Files`, `Program Files (x86)` и `%LOCALAPPDATA%`.
    fn windows_relative(self) -> &'static [&'static str] {
        match self {
            Self::Brave => &["BraveSoftware/Brave-Browser/Application/brave.exe"],
            Self::Chrome => &["Google/Chrome/Application/chrome.exe"],
            Self::Edge => &["Microsoft/Edge/Application/msedge.exe"],
            Self::Vivaldi => &["Vivaldi/Application/vivaldi.exe"],
            Self::Chromium => &["Chromium/Application/chrome.exe"],
        }
    }

    fn linux_names(self) -> &'static [&'static str] {
        match self {
            Self::Brave => &["brave-browser", "brave-browser-stable", "brave"],
            Self::Chrome => &["google-chrome", "google-chrome-stable"],
            Self::Edge => &["microsoft-edge", "microsoft-edge-stable"],
            Self::Vivaldi => &["vivaldi", "vivaldi-stable"],
            Self::Chromium => &["chromium", "chromium-browser"],
        }
    }

    fn linux_fixed(self) -> &'static [&'static str] {
        match self {
            Self::Brave => &["/opt/brave.com/brave/brave"],
            Self::Chrome => &["/opt/google/chrome/chrome"],
            Self::Edge => &["/opt/microsoft/msedge/msedge"],
            Self::Vivaldi => &["/opt/vivaldi/vivaldi"],
            Self::Chromium => &[],
        }
    }

    /// Совпадает ли старшая часть версии браузера с версией Chromium.
    fn version_is_chromium(self) -> bool {
        !matches!(self, Self::Vivaldi)
    }

    /// Известное ограничение браузера, подтверждённое проверкой; `None` — нет.
    pub fn support_note(self) -> Option<&'static str> {
        match self {
            Self::Vivaldi => Some(
                "Поддержка не подтверждена: в изолированной проверке Vivaldi собственная фоновая вкладка получала нулевой viewport, и кнопка загрузки не нажималась (download_button_unavailable). Сопряжение, вход, кандидаты и диагностика работают; получение MP3 — нет",
            ),
            Self::Chromium => Some(
                "Тестовые и открытые сборки (Chromium, Chrome for Testing): используются изолированным стендом",
            ),
            _ => None,
        }
    }

    /// Бренд `navigator.userAgentData`, однозначно указывающий на этот браузер.
    /// Vivaldi представляется брендом «Google Chrome», поэтому собственного нет.
    pub fn brand(self) -> Option<&'static str> {
        match self {
            Self::Brave => Some("Brave"),
            Self::Chrome => Some("Google Chrome"),
            Self::Edge => Some("Microsoft Edge"),
            Self::Vivaldi | Self::Chromium => None,
        }
    }
}

/// Разновидность по `ProductName` ресурса версии Windows.
fn kind_from_product(product: &str) -> Option<BrowserKind> {
    match product.trim() {
        "Brave Browser" => Some(BrowserKind::Brave),
        "Google Chrome" => Some(BrowserKind::Chrome),
        "Microsoft Edge" => Some(BrowserKind::Edge),
        "Vivaldi" => Some(BrowserKind::Vivaldi),
        "Chromium" | "Google Chrome for Testing" => Some(BrowserKind::Chromium),
        _ => None,
    }
}

/// Разновидность и версия по выводу `--version` Linux-браузера.
pub(crate) fn kind_from_version_output(text: &str) -> Option<(BrowserKind, Option<String>)> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    // Порядок важен: «Google Chrome for Testing» — тестовая сборка, не Chrome.
    let prefixes = [
        ("Google Chrome for Testing", BrowserKind::Chromium),
        ("Google Chrome", BrowserKind::Chrome),
        ("Microsoft Edge", BrowserKind::Edge),
        ("Brave Browser", BrowserKind::Brave),
        ("Vivaldi", BrowserKind::Vivaldi),
        ("Chromium", BrowserKind::Chromium),
    ];
    let (prefix, kind) = prefixes.iter().find(|(p, _)| {
        line.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(' '))
    })?;
    let version = line[prefix.len()..]
        .split_whitespace()
        .next()
        .filter(|v| valid_version(v))
        .map(str::to_owned);
    Some((*kind, version))
}

fn valid_version(text: &str) -> bool {
    let parts: Vec<_> = text.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn version_major(version: &str) -> Option<u32> {
    version.split('.').next()?.parse().ok()
}

/// Строки `StringFileInfo` ресурса `VS_VERSIONINFO` исполняемого файла Windows.
/// Ищется сам блок `VS_VERSION_INFO` с проверкой заголовков структур, а не
/// первое попавшееся вхождение ключа: в бинарнике бывают таблицы с теми же словами.
pub(crate) fn pe_version_strings(
    bytes: &[u8],
) -> Option<std::collections::BTreeMap<String, String>> {
    let utf16 = |s: &str| -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    };
    let u16_at = |b: &[u8], i: usize| -> Option<usize> {
        Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize)
    };
    let root_key = utf16("VS_VERSION_INFO");
    let mut from = 0;
    let block = loop {
        let at = find(&bytes[from..], &root_key)? + from;
        if at >= 6 && at % 2 == 0 {
            let length = u16_at(bytes, at - 6)?;
            let value_length = u16_at(bytes, at - 4)?;
            let kind = u16_at(bytes, at - 2)?;
            // VS_FIXEDFILEINFO — 52 байта; блок двоичный (wType = 0).
            if kind == 0 && value_length == 52 && length > 92 && at - 6 + length <= bytes.len() {
                break &bytes[at - 6..at - 6 + length];
            }
        }
        from = at + 2;
    };
    let mut out = std::collections::BTreeMap::new();
    for key in [
        "ProductName",
        "CompanyName",
        "FileVersion",
        "ProductVersion",
    ] {
        let needle = utf16(key);
        let mut from = 0;
        while let Some(found) = find(&block[from..], &needle) {
            let at = found + from;
            from = at + 2;
            if at < 6 || at % 2 != 0 {
                continue;
            }
            let start = at - 6;
            let (Some(length), Some(chars), Some(kind)) = (
                u16_at(block, start),
                u16_at(block, start + 2),
                u16_at(block, start + 4),
            ) else {
                continue;
            };
            let mut value = at + needle.len();
            value += (4 - value % 4) % 4;
            if kind != 1 || chars == 0 || chars > 256 || start + length > block.len() {
                continue;
            }
            let Some(raw) = block.get(value..value + chars * 2) else {
                continue;
            };
            let words: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .take_while(|w| *w != 0)
                .collect();
            if let Ok(text) = String::from_utf16(&words) {
                out.insert(key.to_owned(), text);
                break;
            }
        }
    }
    Some(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// ID распакованного расширения Chromium: первые 128 бит SHA-256 абсолютного
/// пути каталога, цифры `0`–`f` записаны буквами `a`–`p`. В Windows хэшируется
/// путь UTF-16LE, в Linux — байты пути после разрешения символических ссылок.
pub fn unpacked_extension_id(platform: Platform, load_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = match platform {
        Platform::Wsl => Sha256::digest(
            load_path
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        ),
        Platform::Linux => Sha256::digest(load_path.as_bytes()),
    };
    digest[..16]
        .iter()
        .flat_map(|b| [b >> 4, b & 0x0f])
        .map(|n| char::from(b'a' + n))
        .collect()
}

/// Допустимые ID для каталога: путь как есть и, в Linux, путь после разрешения ссылок.
pub fn expected_extension_ids(platform: Platform, load_path: &str) -> Vec<String> {
    let mut ids = vec![unpacked_extension_id(platform, load_path)];
    if platform == Platform::Linux
        && let Ok(real) = std::fs::canonicalize(load_path)
        && let Some(real) = real.to_str()
    {
        let id = unpacked_extension_id(platform, real);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// Установка браузера, подтверждённая по самому исполняемому файлу.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Installation {
    pub browser: BrowserKind,
    pub platform: Platform,
    /// Путь в виде, понятном пользователю: в WSL — путь Windows.
    pub executable: String,
    #[serde(skip)]
    pub local_path: PathBuf,
    /// Версия продукта; `null` — не установлена надёжно.
    pub version: Option<String>,
    /// Старшая версия Chromium; `null` — неизвестна (у Vivaldi своя нумерация).
    pub chromium_major: Option<u32>,
    /// Чем подтверждена разновидность.
    pub identified_by: &'static str,
}

/// Где искать установки. Подменяется в тестах.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryRoots {
    /// Каталоги Windows (`Program Files`, `Program Files (x86)`, `%LOCALAPPDATA%`) в путях WSL.
    pub windows: Vec<PathBuf>,
    /// Каталоги поиска Linux-браузеров (обычно `PATH`).
    pub linux_path: Vec<PathBuf>,
    /// Учитывать фиксированные пути `/opt/...`.
    pub linux_fixed: bool,
}

impl DiscoveryRoots {
    /// Корни текущего окружения: Windows-браузеры видны только из WSL.
    pub fn system(platform_is_wsl: bool) -> Self {
        let mut windows = Vec::new();
        if platform_is_wsl {
            windows.push(PathBuf::from("/mnt/c/Program Files"));
            windows.push(PathBuf::from("/mnt/c/Program Files (x86)"));
            if let Ok(local) = super::targets::windows_local_app_data() {
                windows.push(local);
            }
        }
        let linux_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Self {
            windows,
            linux_path,
            linux_fixed: true,
        }
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Запуск `--version` с ограничением по времени; браузер не открывает окон.
fn version_output(path: &Path) -> Option<String> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    Some(text)
}

/// Подтверждает разновидность исполняемого файла Windows по ресурсу версии.
fn identify_windows(local: &Path, shown: String) -> Result<Installation, AssetError> {
    let unrecognized = || {
        bridge_error(
            "browser_unrecognized",
            "Исполняемый файл не распознан как поддерживаемый браузер (Brave, Chrome, Edge, Vivaldi)",
        )
    };
    let metadata = std::fs::metadata(local)
        .map_err(|_| bridge_error("browser_missing", "Исполняемый файл браузера не найден"))?;
    if !metadata.is_file() || metadata.len() > MAX_PE_BYTES {
        return Err(unrecognized());
    }
    let bytes = std::fs::read(local).map_err(|_| {
        bridge_error(
            "browser_missing",
            "Исполняемый файл браузера недоступен для чтения",
        )
    })?;
    if !bytes.starts_with(b"MZ") {
        return Err(unrecognized());
    }
    let strings = pe_version_strings(&bytes).ok_or_else(unrecognized)?;
    let kind = strings
        .get("ProductName")
        .and_then(|p| kind_from_product(p))
        .ok_or_else(unrecognized)?;
    let version = strings
        .get("FileVersion")
        .or_else(|| strings.get("ProductVersion"))
        .map(|v| v.trim().to_owned())
        .filter(|v| valid_version(v));
    Ok(Installation {
        browser: kind,
        platform: Platform::Wsl,
        executable: shown,
        local_path: local.to_owned(),
        chromium_major: version
            .as_deref()
            .filter(|_| kind.version_is_chromium())
            .and_then(version_major),
        version,
        identified_by: "pe_version_info",
    })
}

fn identify_linux(path: &Path) -> Result<Installation, AssetError> {
    if !path.exists() {
        return Err(bridge_error(
            "browser_missing",
            "Исполняемый файл браузера не найден",
        ));
    }
    if !executable(path) {
        return Err(bridge_error(
            "browser_not_executable",
            "Файл браузера не является исполняемым",
        ));
    }
    let (kind, version) = version_output(path)
        .as_deref()
        .and_then(kind_from_version_output)
        .ok_or_else(|| {
            bridge_error(
                "browser_unrecognized",
                "Исполняемый файл не распознан как поддерживаемый браузер (Brave, Chrome, Edge, Vivaldi)",
            )
        })?;
    Ok(Installation {
        browser: kind,
        platform: Platform::Linux,
        executable: path.display().to_string(),
        local_path: path.to_owned(),
        chromium_major: version
            .as_deref()
            .filter(|_| kind.version_is_chromium())
            .and_then(version_major),
        version,
        identified_by: "version_output",
    })
}

/// Подтверждает установку по явному пути: Windows-путь (`C:\…`) или путь
/// `/mnt/…/*.exe` разбирается как исполняемый файл Windows (только в WSL),
/// остальное — как Linux-браузер.
pub fn identify(path: &str, wsl: bool) -> Result<Installation, AssetError> {
    let bytes = path.as_bytes();
    let windows_drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let is_exe = path.to_ascii_lowercase().ends_with(".exe");
    if windows_drive || is_exe {
        if !wsl {
            return Err(bridge_error(
                "browser_platform_unsupported",
                "Браузер Windows доступен только из WSL",
            ));
        }
        let local = if windows_drive {
            wsl_to_linux(path)?
        } else {
            std::path::absolute(path).map_err(|e| AssetError::io("путь браузера", e))?
        };
        let shown = if windows_drive {
            path.to_owned()
        } else {
            linux_to_windows(&local).unwrap_or_else(|_| local.display().to_string())
        };
        return identify_windows(&local, shown);
    }
    let absolute = std::path::absolute(path).map_err(|e| AssetError::io("путь браузера", e))?;
    identify_linux(&absolute)
}

/// Все подтверждённые установки поддерживаемых браузеров. Каталог без
/// исполняемого файла или файл с чужим `ProductName` установкой не считается.
pub fn discover(roots: &DiscoveryRoots) -> Vec<Installation> {
    let mut found: Vec<Installation> = Vec::new();
    let push = |found: &mut Vec<Installation>, item: Installation| {
        if !found
            .iter()
            .any(|f| f.local_path == item.local_path || f.executable == item.executable)
        {
            found.push(item);
        }
    };
    for kind in BrowserKind::ALL {
        for root in &roots.windows {
            for relative in kind.windows_relative() {
                let local = root.join(relative);
                if !local.is_file() {
                    continue;
                }
                let shown =
                    linux_to_windows(&local).unwrap_or_else(|_| local.display().to_string());
                if let Ok(item) = identify_windows(&local, shown)
                    && item.browser == kind
                {
                    push(&mut found, item);
                }
            }
        }
        let fixed = if roots.linux_fixed {
            kind.linux_fixed()
        } else {
            &[]
        };
        let candidates = roots
            .linux_path
            .iter()
            .flat_map(|dir| kind.linux_names().iter().map(move |n| dir.join(n)))
            .chain(fixed.iter().map(PathBuf::from));
        for candidate in candidates {
            if !executable(&candidate) {
                continue;
            }
            // Ссылка в PATH, скрипт-обёртка пакета и сам исполняемый файл в /opt — одна
            // установка: после разрешения ссылок они лежат в одном каталоге.
            let real = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if found.iter().any(|f| {
                f.browser == kind
                    && f.platform == Platform::Linux
                    && std::fs::canonicalize(&f.local_path)
                        .is_ok_and(|r| r == real || r.parent() == real.parent())
            }) {
                continue;
            }
            if let Ok(item) = identify_linux(&candidate)
                && item.browser == kind
            {
                push(&mut found, item);
            }
        }
    }
    found
}

#[cfg(test)]
#[path = "browsers_tests.rs"]
mod tests;
