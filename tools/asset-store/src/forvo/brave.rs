//! Обычный авторизованный Brave через собственное MV3-расширение.
//!
//! Расширение — тонкий транспорт: оно подключается к локальному bridge на
//! `127.0.0.1`, выполняет фиксированный набор команд над собственной фоновой
//! вкладкой и сообщает наблюдения. Выбор кандидата, проверка адреса загрузки,
//! SHA и MP3 остаются в Rust и используют те же функции, что и CDP-провайдер.
pub(crate) use crate::hashing::encode_lower_hex;
use crate::{AssetError, ErrorCode};
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
pub mod browsers;
pub mod download;
pub mod fetch;
pub mod targets;

pub const PROTOCOL: &str = "forvo-adapter/v1";
pub const WS_PATH: &str = "/forvo-adapter/v1";
pub const GROUP_TITLE: &str = "Anki Deck Toolkit";
pub const DEFAULT_PORT: u16 = 47613;
/// Байты MP3 через bridge не передаются: ответы — только состояния и безопасные метаданные.
const MAX_INBOUND_MESSAGE: usize = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MANIFEST: &str = include_str!("brave/extension/manifest.json");
pub(crate) const BACKGROUND: &str = include_str!("brave/extension/background.js");

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

pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
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

pub(crate) fn random_bytes() -> Result<[u8; 32], AssetError> {
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

pub use browsers::BrowserKind;
pub use targets::{
    AdapterState, LEGACY_TARGET, SetupOptions, load_state, load_target, save_state, setup,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Wsl,
    Linux,
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
    std::fs::create_dir_all(path).map_err(|e| AssetError::io("каталог состояния адаптера", e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| AssetError::io("права каталога состояния адаптера", e))
}

/// Атомарная запись: временный файл рядом и rename. `mode` применяется до записи данных.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), AssetError> {
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

pub(crate) fn windows_command(program: &str, args: &[&str]) -> Result<String, AssetError> {
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

/// Абсолютный путь Windows с буквой диска (`C:\\…` или `C:/…`).
pub(crate) fn windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

pub(crate) fn linux_to_windows(path: &Path) -> Result<String, AssetError> {
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
    match platform {
        Platform::Wsl if windows_drive_path(path) => wsl_to_linux(path),
        _ if path.starts_with('/') && !path.contains('\0') => Ok(PathBuf::from(path)),
        _ => Err(bridge_error(
            "path_mapping_failed",
            "Неожиданный путь загрузки браузера",
        )),
    }
}

// --- Bridge --------------------------------------------------------------

/// Возможности браузера, от которых зависит расширение. Расширение сообщает
/// их о себе в `ready`/`ping`; отсутствие любой — честный отказ, а не PASS.
pub const REQUIRED_CAPABILITIES: [&str; 11] = [
    "service_worker",
    "downloads",
    "downloads_search",
    "web_request_observe",
    "tabs",
    "tab_groups",
    "scripting",
    "storage_local",
    "storage_session",
    "alarms",
    "websocket_hmac",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub extension_id: String,
    pub version: String,
    /// Экземпляр расширения в профиле браузера; у версий до 0.3 его нет.
    pub instance: Option<String>,
    /// Безопасные сведения о браузере от самого расширения: бренды
    /// `userAgentData`, версия Chromium и возможности API. `null` у старых версий.
    pub browser: serde_json::Value,
}

/// Кому bridge доверяет: закреплённый ID (или допустимые ID каталога цели до
/// закрепления) и закреплённый экземпляр расширения.
#[derive(Debug, Clone, Default)]
pub struct Trust {
    pub pinned: Option<String>,
    /// ID, вычисленные из пути каталога цели; пусто — любой (только до закрепления).
    pub expected: Vec<String>,
    pub instance: Option<String>,
}

type Socket = WebSocketStream<TokioAdapter<TcpStream>>;

pub struct Bridge {
    listener: TcpListener,
    port: u16,
    secret: Vec<u8>,
    trust: Trust,
    socket: Option<Socket>,
    next_id: u64,
    pub peer: Option<Peer>,
    /// Отклонённые подключения по причинам: доказательство для диагностики.
    rejections: std::collections::BTreeMap<&'static str, u32>,
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

fn short_text(value: &serde_json::Value, max: usize) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

/// Оставляет из сведений расширения только известные поля ограниченного размера.
pub(crate) fn sanitize_browser_facts(value: &serde_json::Value) -> serde_json::Value {
    if !value.is_object() {
        return serde_json::Value::Null;
    }
    let brands: Vec<serde_json::Value> = value["brands"]
        .as_array()
        .map(|a| {
            a.iter()
                .take(8)
                .filter_map(|b| {
                    Some(serde_json::json!({"brand":short_text(&b["brand"],64)?,"version":short_text(&b["version"],32)?}))
                })
                .collect()
        })
        .unwrap_or_default();
    let capabilities: serde_json::Map<String, serde_json::Value> = REQUIRED_CAPABILITIES
        .iter()
        .map(|k| {
            (
                (*k).to_owned(),
                serde_json::Value::Bool(value["capabilities"][k] == true),
            )
        })
        .collect();
    serde_json::json!({
        "brands": brands,
        "chromium_version": short_text(&value["chromium_version"], 32),
        "platform": short_text(&value["platform"], 32),
        "manifest_version": value["manifest_version"].as_u64(),
        "capabilities": capabilities,
    })
}

impl Bridge {
    /// Слушает только loopback IPv4; `0.0.0.0` не используется ни в каком режиме.
    pub async fn bind(
        port: u16,
        secret: Vec<u8>,
        pinned: Option<String>,
    ) -> Result<Self, AssetError> {
        Self::bind_with(
            port,
            secret,
            Trust {
                pinned,
                ..Trust::default()
            },
        )
        .await
    }

    pub async fn bind_with(port: u16, secret: Vec<u8>, trust: Trust) -> Result<Self, AssetError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    bridge_error("bridge_port_busy", "Порт bridge занят: идёт другая команда этой цели или порт занят другой программой; дождитесь завершения или задайте другой порт в setup")
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
            trust,
            socket: None,
            next_id: 1,
            peer: None,
            rejections: std::collections::BTreeMap::new(),
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn connected(&self) -> bool {
        self.socket.is_some()
    }
    /// Закрепления действуют и для последующих переподключений этого bridge.
    pub fn pin(&mut self, extension_id: &str, instance: Option<&str>) {
        self.trust.pinned = Some(extension_id.to_owned());
        if let Some(instance) = instance {
            self.trust.instance = Some(instance.to_owned());
        }
    }
    pub fn rejections(&self) -> serde_json::Value {
        serde_json::json!(self.rejections)
    }
    fn reject(&mut self, reason: &'static str) {
        *self.rejections.entry(reason).or_default() += 1;
        tracing::warn!(
            event = "brave_bridge_rejected",
            stage = "transport",
            reason,
            "Отклонено подключение к bridge"
        );
    }
    /// Принимает подключения до первого успешно сопряжённого расширения
    /// закреплённого экземпляра.
    pub async fn accept(&mut self, deadline: Instant) -> Result<Peer, AssetError> {
        self.disconnect().await;
        loop {
            let accepted = tokio::time::timeout_at(deadline, self.listener.accept()).await;
            let (stream, address) = match accepted {
                Ok(result) => result.map_err(|e| AssetError::io("accept bridge", e))?,
                Err(_) => return Err(self.not_connected()),
            };
            if !address.ip().is_loopback() {
                continue;
            }
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.handshake(stream)).await {
                Ok(Ok((socket, peer))) => {
                    let instance_ok = match (&self.trust.instance, &peer.instance) {
                        (Some(pinned), Some(seen)) => pinned == seen,
                        (Some(_), None) => false,
                        (None, _) => true,
                    };
                    if !instance_ok {
                        let mut socket = socket;
                        let _ =
                            tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
                        self.reject(if peer.instance.is_some() {
                            "instance_mismatch"
                        } else {
                            "instance_missing"
                        });
                        continue;
                    }
                    tracing::info!(event = "brave_bridge_connected", stage = "transport", extension_version = %peer.version, "Расширение подключено");
                    self.socket = Some(socket);
                    self.peer = Some(peer.clone());
                    return Ok(peer);
                }
                Ok(Err(reason)) => self.reject(reason),
                Err(_) => self.reject("handshake_timeout"),
            }
        }
    }
    /// Нет подключения: причина отказов (если были) превращается в точное действие.
    fn not_connected(&self) -> AssetError {
        let dominant = self
            .rejections
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(reason, _)| *reason);
        let (code, action) = match dominant {
            None | Some("handshake_timeout" | "upgrade_rejected") => (
                "extension_not_connected",
                "Расширение не подключилось; проверьте, что оно установлено и включено",
            ),
            Some("pairing_mismatch") => (
                "pairing_mismatch",
                "Расширение подключилось с другим секретом: перезагрузите его на странице расширений браузера после setup",
            ),
            Some("extension_id_unexpected" | "extension_id_not_pinned") => (
                "extension_id_mismatch",
                "Подключилось расширение не из каталога этой цели (другой каталог или профиль); загрузите распакованное расширение из каталога цели",
            ),
            Some("instance_mismatch" | "instance_missing") => (
                "instance_mismatch",
                "Подключился другой экземпляр расширения (другой профиль или переустановка); оставьте расширение в одном профиле или выполните setup --repin",
            ),
            Some(_) => (
                "extension_not_connected",
                "Расширение не прошло сопряжение; повторите setup и перезагрузите расширение",
            ),
        };
        let mut error = bridge_error(code, action);
        error.details["rejections"] = self.rejections();
        error
    }
    async fn handshake(&self, stream: TcpStream) -> Result<(Socket, Peer), &'static str> {
        let origin = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let refused = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
        let seen = origin.clone();
        let why = refused.clone();
        let port = self.port;
        let trust = self.trust.clone();
        // Сигнатура задана трейтом Callback tungstenite.
        #[allow(clippy::result_large_err)]
        let callback =
            move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
                let host_ok = header("host").is_some_and(|h| {
                    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
                });
                let id = header("origin").and_then(origin_extension_id);
                let identity = match (&trust.pinned, id) {
                    (Some(p), Some(id)) if p == id => Ok(()),
                    (Some(_), Some(_)) => Err("extension_id_not_pinned"),
                    (None, Some(id))
                        if trust.expected.is_empty() || trust.expected.iter().any(|e| e == id) =>
                    {
                        Ok(())
                    }
                    (None, Some(_)) => Err("extension_id_unexpected"),
                    (_, None) => Err("origin_rejected"),
                };
                let reason = if request.uri().path() != WS_PATH || !host_ok {
                    Some("upgrade_rejected")
                } else {
                    identity.err()
                };
                if let Some(reason) = reason {
                    *why.lock().expect("mutex") = Some(reason);
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
        let mut socket = match async_tungstenite::tokio::accept_hdr_async_with_config(
            stream,
            callback,
            Some(config),
        )
        .await
        {
            Ok(socket) => socket,
            Err(_) => return Err(refused.lock().expect("mutex").unwrap_or("upgrade_rejected")),
        };
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
        // `ready` приходит уже по взаимно проверенному каналу; старые версии
        // присылают только версию, новые — экземпляр и сведения о браузере.
        let ready = loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&text).map_err(|_| "ready_malformed")?;
                    match value["type"].as_str() {
                        Some("ready") => break value,
                        Some("keepalive") => continue,
                        _ => return Err("ready_malformed"),
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Err("ready_missing"),
            }
        };
        let instance = match &ready["instance"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(id) if decode_hex(id).is_some_and(|b| b.len() == 32) => {
                Some(id.clone())
            }
            _ => return Err("ready_malformed"),
        };
        Ok((
            socket,
            Peer {
                extension_id,
                version: auth.version,
                instance,
                browser: sanitize_browser_facts(&ready["browser"]),
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
