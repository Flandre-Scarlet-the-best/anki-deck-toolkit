//! Обычный авторизованный Brave через собственное MV3-расширение.
//!
//! Расширение — тонкий транспорт: оно подключается к локальному bridge на
//! `127.0.0.1`, выполняет фиксированный набор команд над собственной фоновой
//! вкладкой и сообщает наблюдения. Выбор кандидата, проверка адреса загрузки,
//! SHA и MP3 остаются в Rust и используют те же функции, что и CDP-провайдер.
use crate::{AssetError, ErrorCode, hashing::encode_lower_hex};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    protocol::WebSocketConfig,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

pub mod acceptance;
pub mod download;
pub mod fetch;

pub const PROTOCOL: &str = "forvo-adapter/v1";
pub const WS_PATH: &str = "/forvo-adapter/v1";
pub const GROUP_TITLE: &str = "Anki Deck Toolkit";
pub const DEFAULT_PORT: u16 = 47613;
/// Байты MP3 через bridge не передаются: ответы — только состояния и безопасные метаданные.
const MAX_INBOUND_MESSAGE: usize = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST: &str = include_str!("brave/extension/manifest.json");
const BACKGROUND: &str = include_str!("brave/extension/background.js");

pub fn bridge_error(code: &str, action: &str) -> AssetError {
    AssetError::with_details(
        ErrorCode::InvalidTransition,
        action,
        serde_json::json!({"provider":"forvo_brave","code":code,"next_action":action}),
    )
}

/// Модуль страницы собирается из тех же `dom.js` и `auth.js`, что исполняет
/// CDP-провайдер: второй реализации разметки Forvo нет.
pub fn page_module() -> String {
    format!(
        "// Сгенерировано pronunciation-assets из forvo/dom.js и forvo/auth.js; не редактировать.\n\
         export const forvoDom = {};\nexport const forvoAuth = () => {};\n",
        include_str!("dom.js").trim_end(),
        include_str!("auth.js").trim_end()
    )
}

pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0_u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    let mut out = [0_u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

pub(crate) fn mac_text(role: &str, first: &str, second: &str, extension_id: &str) -> String {
    format!("{PROTOCOL}|{role}|{first}|{second}|{extension_id}")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2)
        || !text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn random_bytes() -> Result<[u8; 32], AssetError> {
    let mut bytes = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| AssetError::io("системный источник случайности", e))?;
    Ok(bytes)
}

/// ID расширения Chromium: 32 символа `a`–`p`.
pub(crate) fn valid_extension_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

pub(crate) fn origin_extension_id(origin: &str) -> Option<&str> {
    origin
        .strip_prefix("chrome-extension://")
        .filter(|id| valid_extension_id(id))
}

// --- Локальное состояние сопряжения -------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterState {
    pub schema_version: u32,
    pub port: u16,
    /// Секрет сопряжения хранится только в приватном файле состояния и
    /// в каталоге установленного расширения; в вывод не попадает.
    pub secret: String,
    pub install_dir: PathBuf,
    pub load_unpacked_path: String,
    pub platform: Platform,
    pub extension_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Wsl,
    Linux,
}

impl AdapterState {
    fn secret_bytes(&self) -> Result<Vec<u8>, AssetError> {
        decode_hex(&self.secret)
            .filter(|s| s.len() == 32)
            .ok_or_else(|| bridge_error("pairing_invalid", "Повторите forvo brave setup"))
    }
}

pub fn default_state_dir() -> Result<PathBuf, AssetError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| bridge_error("state_dir_unknown", "Задайте --state-dir"))?;
    Ok(base.join("anki-deck-toolkit/forvo-brave"))
}

pub(crate) fn private_dir(path: &Path) -> Result<(), AssetError> {
    std::fs::create_dir_all(path).map_err(|e| AssetError::io("каталог состояния Brave", e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| AssetError::io("права каталога состояния Brave", e))
}

/// Атомарная запись: временный файл рядом и rename. `mode` применяется до записи данных.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), AssetError> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| AssetError::io(format!("запись {}", path.display()), e))
}

pub fn load_state(state_dir: &Path) -> Result<AdapterState, AssetError> {
    let path = state_dir.join("state.json");
    let bytes = std::fs::read(&path).map_err(|_| {
        bridge_error(
            "adapter_not_configured",
            "Выполните pronunciation-assets forvo brave setup",
        )
    })?;
    let meta = std::fs::metadata(&path).map_err(|e| AssetError::io("state.json", e))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(bridge_error(
            "state_permissions",
            "state.json доступен другим пользователям; повторите setup",
        ));
    }
    let state: AdapterState = serde_json::from_slice(&bytes)
        .map_err(|_| bridge_error("state_corrupt", "Повторите forvo brave setup"))?;
    if state.schema_version != 1 {
        return Err(bridge_error("state_corrupt", "Повторите forvo brave setup"));
    }
    state.secret_bytes()?;
    Ok(state)
}

pub fn save_state(state_dir: &Path, state: &AdapterState) -> Result<(), AssetError> {
    private_dir(state_dir)?;
    write_atomic(
        &state_dir.join("state.json"),
        &serde_json::to_vec_pretty(state).expect("состояние сериализуется"),
        0o600,
    )
}

// --- Окружение -----------------------------------------------------------

pub fn detect_platform() -> Platform {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if release.to_lowercase().contains("microsoft")
        && (std::env::var_os("WSL_INTEROP").is_some()
            || std::env::var_os("WSL_DISTRO_NAME").is_some())
    {
        Platform::Wsl
    } else {
        Platform::Linux
    }
}

fn windows_command(program: &str, args: &[&str]) -> Result<String, AssetError> {
    // Рабочий каталог на DrvFs избавляет cmd/powershell от UNC-предупреждений.
    let output = Command::new(program)
        .args(args)
        .current_dir("/mnt/c")
        .output()
        .map_err(|_| {
            bridge_error(
                "windows_interop_unavailable",
                "Проверьте Windows interop WSL",
            )
        })?;
    if !output.status.success() {
        return Err(bridge_error(
            "windows_interop_failed",
            "Windows interop вернул ошибку",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned())
}

pub fn wsl_to_linux(windows: &str) -> Result<PathBuf, AssetError> {
    let output = Command::new("wslpath")
        .args(["-u", windows])
        .output()
        .map_err(|_| bridge_error("path_mapping_failed", "wslpath недоступен"))?;
    if !output.status.success() {
        return Err(bridge_error("path_mapping_failed", "wslpath отказал"));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn linux_to_windows(path: &Path) -> Result<String, AssetError> {
    let output = Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
        .map_err(|_| bridge_error("path_mapping_failed", "wslpath недоступен"))?;
    if !output.status.success() {
        return Err(bridge_error("path_mapping_failed", "wslpath отказал"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Путь, который браузер сообщает о файле, в путь текущей Linux-среды.
/// Windows-путь допустим только в WSL и проходит через `wslpath`; абсолютный
/// Linux-путь сообщает Linux-браузер и используется как есть.
pub fn browser_path_to_local(platform: Platform, path: &str) -> Result<PathBuf, AssetError> {
    let bytes = path.as_bytes();
    let windows_drive = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    match platform {
        Platform::Wsl if windows_drive => wsl_to_linux(path),
        _ if path.starts_with('/') && !path.contains('\0') => Ok(PathBuf::from(path)),
        _ => Err(bridge_error(
            "path_mapping_failed",
            "Неожиданный путь загрузки браузера",
        )),
    }
}

fn default_install_dir(platform: Platform) -> Result<PathBuf, AssetError> {
    match platform {
        Platform::Wsl => {
            let local = windows_command(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Environment]::GetFolderPath('LocalApplicationData')",
                ],
            )?;
            if local.as_bytes().get(1) != Some(&b':') {
                return Err(bridge_error(
                    "windows_profile_unknown",
                    "Не найден LOCALAPPDATA",
                ));
            }
            Ok(wsl_to_linux(&local)?.join("AnkiDeckToolkit/forvo-brave-adapter"))
        }
        Platform::Linux => {
            let base = std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .ok_or_else(|| bridge_error("install_dir_unknown", "Задайте --install-dir"))?;
            Ok(base.join("anki-deck-toolkit/forvo-brave-adapter"))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BraveInstall {
    pub path: String,
    pub version: Option<String>,
}

pub fn discover_brave(platform: Platform) -> Vec<BraveInstall> {
    let mut found = Vec::new();
    match platform {
        Platform::Wsl => {
            let mut roots = vec![
                PathBuf::from("/mnt/c/Program Files/BraveSoftware/Brave-Browser/Application"),
                PathBuf::from("/mnt/c/Program Files (x86)/BraveSoftware/Brave-Browser/Application"),
            ];
            if let Ok(local) = default_install_dir(platform)
                && let Some(base) = local.parent().and_then(Path::parent)
            {
                roots.push(base.join("BraveSoftware/Brave-Browser/Application"));
            }
            for root in roots {
                if root.join("brave.exe").is_file() {
                    let version = std::fs::read_dir(&root)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .filter(|n| {
                            n.split('.').count() == 4
                                && n.split('.').all(|p| p.parse::<u32>().is_ok())
                        })
                        .max_by_key(|n| {
                            n.split('.')
                                .map(|p| p.parse::<u32>().unwrap_or(0))
                                .collect::<Vec<_>>()
                        });
                    found.push(BraveInstall {
                        path: linux_to_windows(&root.join("brave.exe"))
                            .unwrap_or_else(|_| root.join("brave.exe").display().to_string()),
                        version,
                    });
                }
            }
        }
        Platform::Linux => {
            let path = std::env::var_os("PATH").unwrap_or_default();
            for dir in std::env::split_paths(&path).chain([PathBuf::from("/opt/brave.com/brave")]) {
                for name in ["brave-browser", "brave", "brave-browser-stable"] {
                    let candidate = dir.join(name);
                    if candidate.is_file()
                        && !found
                            .iter()
                            .any(|f: &BraveInstall| f.path == candidate.display().to_string())
                    {
                        found.push(BraveInstall {
                            path: candidate.display().to_string(),
                            version: None,
                        });
                    }
                }
            }
        }
    }
    found
}

// --- Установка -----------------------------------------------------------

pub struct SetupOptions {
    pub state_dir: PathBuf,
    pub install_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub rotate_secret: bool,
    /// Явная платформа браузера (изолированный E2E с Linux-Chromium внутри WSL).
    pub platform: Option<Platform>,
}

/// Материализует расширение и сопряжение. Повторный вызов обновляет код,
/// сохраняя секрет и закреплённый ID, если не запрошена ротация.
pub fn setup(options: &SetupOptions) -> Result<serde_json::Value, AssetError> {
    let platform = options.platform.unwrap_or_else(detect_platform);
    let previous = load_state(&options.state_dir).ok();
    let install_dir = match (&options.install_dir, &previous) {
        (Some(dir), _) => {
            std::path::absolute(dir).map_err(|e| AssetError::io("каталог расширения", e))?
        }
        (None, Some(state)) => state.install_dir.clone(),
        (None, None) => default_install_dir(platform)?,
    };
    let port = options
        .port
        .or(previous.as_ref().map(|s| s.port))
        .unwrap_or(DEFAULT_PORT);
    if port < 1024 {
        return Err(bridge_error("invalid_port", "Порт должен быть 1024–65535"));
    }
    let keep = previous
        .as_ref()
        .filter(|s| !options.rotate_secret && s.install_dir == install_dir);
    let secret = match keep {
        Some(state) => state.secret.clone(),
        None => encode_lower_hex(random_bytes()?),
    };
    std::fs::create_dir_all(&install_dir).map_err(|e| AssetError::io("каталог расширения", e))?;
    let pairing = serde_json::json!({"schema_version":1,"port":port,"secret":secret});
    for (name, content) in [
        ("manifest.json", MANIFEST.to_owned()),
        ("background.js", BACKGROUND.to_owned()),
        ("forvo_page.js", page_module()),
        (
            "pairing.json",
            serde_json::to_string(&pairing).expect("JSON"),
        ),
    ] {
        write_atomic(&install_dir.join(name), content.as_bytes(), 0o600)?;
    }
    let load_unpacked_path = match platform {
        Platform::Wsl => linux_to_windows(&install_dir)?,
        Platform::Linux => install_dir.display().to_string(),
    };
    let state = AdapterState {
        schema_version: 1,
        port,
        secret,
        install_dir: install_dir.clone(),
        load_unpacked_path: load_unpacked_path.clone(),
        platform,
        extension_id: keep.and_then(|s| s.extension_id.clone()),
    };
    save_state(&options.state_dir, &state)?;
    tracing::info!(
        event = "brave_adapter_setup",
        stage = "setup",
        port,
        pairing_created = keep.is_none(),
        "Расширение Forvo для Brave подготовлено"
    );
    Ok(serde_json::json!({
        "platform": platform,
        "brave": discover_brave(platform),
        "install_dir": install_dir,
        "load_unpacked_path": load_unpacked_path,
        "bridge": format!("ws://127.0.0.1:{port}{WS_PATH}"),
        "pairing": if keep.is_some() {"kept"} else {"created"},
        "pinned_extension_id": state.extension_id,
        "state_file": options.state_dir.join("state.json"),
        "user_action": if state.extension_id.is_some() {
            serde_json::Value::Null
        } else {
            serde_json::json!(format!("brave://extensions → Режим разработчика → Загрузить распакованное → {load_unpacked_path}"))
        },
    }))
}

// --- Bridge --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub extension_id: String,
    pub version: String,
}

type Socket = WebSocketStream<TokioAdapter<TcpStream>>;

pub struct Bridge {
    listener: TcpListener,
    port: u16,
    secret: Vec<u8>,
    pinned: Option<String>,
    socket: Option<Socket>,
    next_id: u64,
    pub peer: Option<Peer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthMessage {
    r#type: String,
    v: u32,
    nonce: String,
    extension_id: String,
    version: String,
    mac: String,
}

impl Bridge {
    /// Слушает только loopback IPv4; `0.0.0.0` не используется ни в каком режиме.
    pub async fn bind(
        port: u16,
        secret: Vec<u8>,
        pinned: Option<String>,
    ) -> Result<Self, AssetError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    bridge_error("bridge_port_busy", "Порт bridge занят другим процессом; завершите его или задайте другой порт в setup")
                } else {
                    bridge_error("bridge_bind_failed", "Не удалось открыть loopback-порт bridge")
                }
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| AssetError::io("адрес bridge", e))?
            .port();
        Ok(Self {
            listener,
            port,
            secret,
            pinned,
            socket: None,
            next_id: 1,
            peer: None,
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn connected(&self) -> bool {
        self.socket.is_some()
    }
    /// Принимает подключения до первого успешно сопряжённого расширения.
    pub async fn accept(&mut self, deadline: Instant) -> Result<Peer, AssetError> {
        self.disconnect().await;
        loop {
            let (stream, address) = tokio::time::timeout_at(deadline, self.listener.accept())
                .await
                .map_err(|_| {
                    bridge_error(
                        "extension_not_connected",
                        "Расширение не подключилось; проверьте, что оно установлено и включено",
                    )
                })?
                .map_err(|e| AssetError::io("accept bridge", e))?;
            if !address.ip().is_loopback() {
                continue;
            }
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.handshake(stream)).await {
                Ok(Ok((socket, peer))) => {
                    tracing::info!(event = "brave_bridge_connected", stage = "transport", extension_version = %peer.version, "Расширение Brave подключено");
                    self.socket = Some(socket);
                    self.peer = Some(peer.clone());
                    return Ok(peer);
                }
                Ok(Err(reason)) => tracing::warn!(
                    event = "brave_bridge_rejected",
                    stage = "transport",
                    reason,
                    "Отклонено подключение к bridge"
                ),
                Err(_) => tracing::warn!(
                    event = "brave_bridge_rejected",
                    stage = "transport",
                    reason = "handshake_timeout",
                    "Отклонено подключение к bridge"
                ),
            }
        }
    }
    async fn handshake(&self, stream: TcpStream) -> Result<(Socket, Peer), &'static str> {
        let origin = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let seen = origin.clone();
        let port = self.port;
        let pinned = self.pinned.clone();
        // Сигнатура задана трейтом Callback tungstenite.
        #[allow(clippy::result_large_err)]
        let callback =
            move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
                let host_ok = header("host").is_some_and(|h| {
                    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
                });
                let id = header("origin").and_then(origin_extension_id);
                let pinned_ok = match (&pinned, id) {
                    (Some(p), Some(id)) => p == id,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if request.uri().path() != WS_PATH || !host_ok || !pinned_ok {
                    let mut reject = ErrorResponse::new(None);
                    *reject.status_mut() = StatusCode::FORBIDDEN;
                    return Err(reject);
                }
                *seen.lock().expect("mutex") = id.map(str::to_owned);
                Ok(response)
            };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_INBOUND_MESSAGE))
            .max_frame_size(Some(MAX_INBOUND_MESSAGE));
        let mut socket =
            async_tungstenite::tokio::accept_hdr_async_with_config(stream, callback, Some(config))
                .await
                .map_err(|_| "upgrade_rejected")?;
        let extension_id = origin
            .lock()
            .expect("mutex")
            .clone()
            .ok_or("origin_missing")?;
        let bridge_nonce = encode_lower_hex(random_bytes().map_err(|_| "random_failed")?);
        socket
            .send(Message::text(
                serde_json::json!({"type":"hello","v":1,"nonce":bridge_nonce}).to_string(),
            ))
            .await
            .map_err(|_| "send_failed")?;
        let auth: AuthMessage = match socket.next().await {
            Some(Ok(Message::Text(text))) if text.len() <= 4096 => {
                serde_json::from_str(&text).map_err(|_| "auth_malformed")?
            }
            _ => return Err("auth_missing"),
        };
        let mac = decode_hex(&auth.mac).ok_or("auth_malformed")?;
        if auth.r#type != "auth"
            || auth.v != 1
            || auth.extension_id != extension_id
            || decode_hex(&auth.nonce).is_none_or(|n| n.len() != 32)
            || auth.version.is_empty()
            || auth.version.len() > 32
        {
            return Err("auth_malformed");
        }
        let expected = hmac_sha256(
            &self.secret,
            mac_text("extension", &bridge_nonce, &auth.nonce, &extension_id).as_bytes(),
        );
        if !constant_time_eq(&expected, &mac) {
            return Err("pairing_mismatch");
        }
        let proof = hmac_sha256(
            &self.secret,
            mac_text("bridge", &auth.nonce, &bridge_nonce, &extension_id).as_bytes(),
        );
        socket
            .send(Message::text(
                serde_json::json!({"type":"auth_ok","mac":encode_lower_hex(proof)}).to_string(),
            ))
            .await
            .map_err(|_| "send_failed")?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(|_| "ready_malformed")?;
                    match value["type"].as_str() {
                        Some("ready") => break,
                        Some("keepalive") => continue,
                        _ => return Err("ready_malformed"),
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Err("ready_missing"),
            }
        }
        Ok((
            socket,
            Peer {
                extension_id,
                version: auth.version,
            },
        ))
    }
    pub async fn disconnect(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
        }
    }
    /// Одна команда — один ответ с тем же `id`. Опоздавшие ответы прошлых
    /// команд и keepalive отбрасываются.
    pub async fn call(
        &mut self,
        op: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, AssetError> {
        let id = self.next_id;
        self.next_id += 1;
        let socket = self
            .socket
            .as_mut()
            .ok_or_else(|| bridge_error("extension_disconnected", "Расширение не подключено"))?;
        let started = Instant::now();
        let deadline = started + timeout;
        socket
            .send(Message::text(
                serde_json::json!({"type":"cmd","id":id,"op":op,"args":args}).to_string(),
            ))
            .await
            .map_err(|_| {
                bridge_error("extension_disconnected", "Соединение с расширением закрыто")
            })?;
        loop {
            let message = match tokio::time::timeout_at(deadline, socket.next()).await {
                Err(_) => {
                    tracing::warn!(
                        event = "brave_command_timeout",
                        stage = "transport",
                        op,
                        "Команда расширения не завершилась вовремя"
                    );
                    return Err(bridge_error(
                        "extension_timeout",
                        "Расширение не ответило вовремя",
                    ));
                }
                Ok(Some(Ok(message))) => message,
                Ok(_) => {
                    self.socket = None;
                    return Err(bridge_error(
                        "extension_disconnected",
                        "Соединение с расширением закрыто",
                    ));
                }
            };
            let Message::Text(text) = message else {
                if matches!(message, Message::Close(_)) {
                    self.socket = None;
                    return Err(bridge_error(
                        "extension_disconnected",
                        "Расширение закрыло соединение",
                    ));
                }
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|_| bridge_error("protocol_error", "Расширение прислало неверный JSON"))?;
            match value["type"].as_str() {
                Some("keepalive") => continue,
                Some("res") if value["id"].as_u64() == Some(id) => {
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    if value["ok"] == true {
                        tracing::debug!(
                            event = "brave_command",
                            stage = "transport",
                            op,
                            elapsed_ms,
                            "Команда расширения выполнена"
                        );
                        return Ok(value["result"].clone());
                    }
                    let code = value["code"]
                        .as_str()
                        .filter(|c| {
                            c.len() <= 64 && c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        })
                        .unwrap_or("protocol_error");
                    tracing::info!(
                        event = "brave_command_failed",
                        stage = "transport",
                        op,
                        code,
                        elapsed_ms,
                        "Расширение отказало"
                    );
                    return Err(bridge_error(
                        code,
                        "Расширение отказало в выполнении команды",
                    ));
                }
                Some("res") => continue,
                _ => {
                    return Err(bridge_error(
                        "protocol_error",
                        "Неожиданное сообщение расширения",
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
