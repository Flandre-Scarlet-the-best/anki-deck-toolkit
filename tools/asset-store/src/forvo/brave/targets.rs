//! Цели адаптера: независимые сопряжения с конкретными установками браузеров.
//!
//! Все цели хранятся в одном приватном `state.json` каталога состояния. У
//! каждой цели свой каталог расширения (а значит, свой вычисляемый ID), свой
//! секрет, свой порт bridge и свой закреплённый экземпляр расширения. Состояние
//! версии 1 (единственный Brave) читается как цель `brave` без изменения
//! секрета, ID, каталога и порта и переписывается в версию 2 только при
//! первой записи; исходный файл сохраняется рядом как `state.v1.json`.
use super::browsers::{self, BrowserKind, DiscoveryRoots, Installation};
use super::{
    BACKGROUND, DEFAULT_PORT, MANIFEST, Platform, WS_PATH, bridge_error, decode_hex,
    detect_platform, encode_lower_hex, linux_to_windows, page_module, private_dir, random_bytes,
    valid_extension_id, write_atomic,
};
use crate::AssetError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Цель, в которую читается состояние версии 1 и с которой работает `forvo brave`.
pub const LEGACY_TARGET: &str = "brave";
const STATE_FILE: &str = "state.json";
const LEGACY_BACKUP: &str = "state.v1.json";
const LOCK_FILE: &str = "state.lock";
/// Сколько портов после [`DEFAULT_PORT`] перебирается для новой цели.
const PORT_SPAN: u16 = 64;

/// Сопряжение одной цели. Секрет хранится только здесь и в каталоге
/// установленного расширения; в вывод не попадает.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterState {
    /// Имя цели — ключ в `targets`; в записи не дублируется.
    #[serde(skip)]
    pub target: String,
    pub browser: BrowserKind,
    pub port: u16,
    pub secret: String,
    pub install_dir: PathBuf,
    pub load_unpacked_path: String,
    pub platform: Platform,
    /// Закреплённый ID расширения (Origin). Для распакованного расширения он
    /// вычисляется из пути каталога и закрепляется при первом подключении.
    pub extension_id: Option<String>,
    /// Закреплённый экземпляр установленного расширения: случайный ID,
    /// который расширение хранит в собственном `chrome.storage.local` профиля.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Подтверждённый исполняемый файл браузера (путь, видимый пользователю).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

impl AdapterState {
    pub(crate) fn secret_bytes(&self) -> Result<Vec<u8>, AssetError> {
        decode_hex(&self.secret)
            .filter(|s| s.len() == 32)
            .ok_or_else(|| bridge_error("pairing_invalid", "Повторите setup этой цели"))
    }

    /// ID, которые может иметь расширение, загруженное из каталога цели.
    pub fn expected_extension_ids(&self) -> Vec<String> {
        browsers::expected_extension_ids(self.platform, &self.load_unpacked_path)
    }

    /// Точное действие первичной установки для браузера этой цели.
    pub fn user_action(&self) -> String {
        self.browser.load_unpacked_action(&self.load_unpacked_path)
    }

    /// Команда этой цели: прежний `forvo brave` для цели `brave`.
    pub fn command_prefix(&self) -> String {
        command_prefix(&self.target)
    }
}

pub fn command_prefix(target: &str) -> String {
    if target == LEGACY_TARGET {
        "pronunciation-assets forvo brave".to_owned()
    } else {
        format!("pronunciation-assets forvo browser --target {target}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterFile {
    pub schema_version: u32,
    pub default_target: Option<String>,
    pub targets: BTreeMap<String, AdapterState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyV1 {
    schema_version: u32,
    port: u16,
    secret: String,
    install_dir: PathBuf,
    load_unpacked_path: String,
    platform: Platform,
    extension_id: Option<String>,
}

pub fn valid_target_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_instance(id: &str) -> bool {
    decode_hex(id).is_some_and(|b| b.len() == 32)
}

fn corrupt() -> AssetError {
    bridge_error(
        "state_corrupt",
        "state.json адаптера повреждён; сопряжение не пересоздаётся автоматически. Восстановите файл (рядом может лежать state.v1.json) или перенесите его и выполните setup заново — расширение придётся установить повторно",
    )
}

impl AdapterFile {
    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != 2
            || self
                .default_target
                .as_ref()
                .is_some_and(|d| !self.targets.contains_key(d))
        {
            return Err(corrupt());
        }
        let mut ports = std::collections::BTreeSet::new();
        let mut dirs = std::collections::BTreeSet::new();
        for (name, target) in &self.targets {
            if !valid_target_name(name)
                || target.secret_bytes().is_err()
                || target.port < 1024
                || !ports.insert(target.port)
                || !dirs.insert(target.install_dir.clone())
                || target
                    .extension_id
                    .as_deref()
                    .is_some_and(|id| !valid_extension_id(id))
                || target
                    .instance_id
                    .as_deref()
                    .is_some_and(|id| !valid_instance(id))
            {
                return Err(corrupt());
            }
        }
        Ok(())
    }

    /// Цель по явному имени, иначе по умолчанию, иначе единственная.
    /// «Первая ответившая» цель не выбирается никогда.
    pub fn resolve(&self, requested: Option<&str>) -> Result<AdapterState, AssetError> {
        let names = || self.targets.keys().cloned().collect::<Vec<_>>();
        let name = match requested {
            Some(name) => name.to_owned(),
            None => match (&self.default_target, self.targets.len()) {
                (Some(name), _) => name.clone(),
                (None, 1) => self.targets.keys().next().cloned().expect("одна цель"),
                _ => {
                    let mut e = bridge_error(
                        "target_ambiguous",
                        "Настроено несколько браузеров; укажите --target или выберите цель по умолчанию: forvo browser --target <имя> select",
                    );
                    e.details["targets"] = json!(names());
                    return Err(e);
                }
            },
        };
        let mut state = self.targets.get(&name).cloned().ok_or_else(|| {
            let mut e = bridge_error(
                "target_unknown",
                "Такая цель не настроена; выполните forvo browser setup",
            );
            e.details["target"] = json!(name);
            e.details["targets"] = json!(names());
            e
        })?;
        state.target = name;
        Ok(state)
    }
}

/// Прочитанное состояние и исходные байты версии 1 (для резервной копии при первой записи).
type Loaded = (AdapterFile, Option<Vec<u8>>);

fn read(state_dir: &Path) -> Result<Option<Loaded>, AssetError> {
    let path = state_dir.join(STATE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AssetError::io("state.json", e)),
    };
    let meta = std::fs::metadata(&path).map_err(|e| AssetError::io("state.json", e))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(bridge_error(
            "state_permissions",
            "state.json доступен другим пользователям; выполните chmod 600 state.json",
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
    let (file, legacy) = match value["schema_version"].as_u64() {
        Some(1) => {
            let old: LegacyV1 = serde_json::from_value(value).map_err(|_| corrupt())?;
            if old.schema_version != 1 {
                return Err(corrupt());
            }
            let state = AdapterState {
                target: LEGACY_TARGET.into(),
                browser: BrowserKind::Brave,
                port: old.port,
                secret: old.secret,
                install_dir: old.install_dir,
                load_unpacked_path: old.load_unpacked_path,
                platform: old.platform,
                extension_id: old.extension_id,
                instance_id: None,
                executable: None,
            };
            (
                AdapterFile {
                    schema_version: 2,
                    default_target: Some(LEGACY_TARGET.into()),
                    targets: BTreeMap::from([(LEGACY_TARGET.to_owned(), state)]),
                },
                Some(bytes),
            )
        }
        Some(2) => (
            serde_json::from_value::<AdapterFile>(value).map_err(|_| corrupt())?,
            None,
        ),
        _ => return Err(corrupt()),
    };
    file.validate()?;
    let mut file = file;
    for (name, target) in &mut file.targets {
        target.target = name.clone();
    }
    Ok(Some((file, legacy)))
}

fn write(state_dir: &Path, file: &AdapterFile, legacy: Option<&[u8]>) -> Result<(), AssetError> {
    file.validate()?;
    private_dir(state_dir)?;
    if let Some(bytes) = legacy {
        let backup = state_dir.join(LEGACY_BACKUP);
        if !backup.exists() {
            write_atomic(&backup, bytes, 0o600)?;
        }
    }
    write_atomic(
        &state_dir.join(STATE_FILE),
        &serde_json::to_vec_pretty(file).expect("состояние сериализуется"),
        0o600,
    )
}

/// Эксклюзивная блокировка read-modify-write состояния между процессами.
struct StateLock(#[allow(dead_code)] std::fs::File);

fn lock(state_dir: &Path) -> Result<StateLock, AssetError> {
    private_dir(state_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(state_dir.join(LOCK_FILE))
        .map_err(|e| AssetError::io("блокировка состояния адаптера", e))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| AssetError::io("блокировка состояния адаптера", e.into()))?;
    Ok(StateLock(file))
}

fn not_configured() -> AssetError {
    bridge_error(
        "adapter_not_configured",
        "Выполните pronunciation-assets forvo browser setup (или forvo brave setup)",
    )
}

pub fn load_file(state_dir: &Path) -> Result<Option<AdapterFile>, AssetError> {
    Ok(read(state_dir)?.map(|(file, _)| file))
}

pub fn load_target(state_dir: &Path, target: Option<&str>) -> Result<AdapterState, AssetError> {
    read(state_dir)?
        .ok_or_else(not_configured)?
        .0
        .resolve(target)
}

/// Цель по умолчанию (или единственная) — прежний однобраузерный контракт.
pub fn load_state(state_dir: &Path) -> Result<AdapterState, AssetError> {
    load_target(state_dir, None)
}

/// Записывает цель целиком (для тестов и программной настройки).
pub fn save_state(state_dir: &Path, state: &AdapterState) -> Result<(), AssetError> {
    let _lock = lock(state_dir)?;
    let (mut file, legacy) = read(state_dir)?.unwrap_or_else(|| {
        (
            AdapterFile {
                schema_version: 2,
                default_target: None,
                targets: BTreeMap::new(),
            },
            None,
        )
    });
    let name = if state.target.is_empty() {
        LEGACY_TARGET.to_owned()
    } else {
        state.target.clone()
    };
    if !valid_target_name(&name) {
        return Err(bridge_error("invalid_target", "Имя цели: a–z, 0–9, '-'"));
    }
    file.targets.insert(name.clone(), state.clone());
    file.default_target.get_or_insert(name);
    write(state_dir, &file, legacy.as_deref())
}

/// Закрепляет ID и экземпляр расширения после первого подключения. Запись идёт
/// под блокировкой и только если сопряжение цели не изменилось с момента
/// чтения (тот же секрет и каталог), а закрепление не появилось параллельно.
pub fn pin_peer(
    state_dir: &Path,
    expected: &AdapterState,
    extension_id: &str,
    instance_id: Option<&str>,
) -> Result<AdapterState, AssetError> {
    let _lock = lock(state_dir)?;
    let (mut file, legacy) = read(state_dir)?.ok_or_else(not_configured)?;
    let current = file
        .targets
        .get_mut(&expected.target)
        .ok_or_else(not_configured)?;
    if current.secret != expected.secret || current.install_dir != expected.install_dir {
        return Err(bridge_error(
            "pairing_changed",
            "Сопряжение цели изменилось во время подключения; повторите команду",
        ));
    }
    if current
        .extension_id
        .as_deref()
        .is_some_and(|id| id != extension_id)
        || current
            .instance_id
            .as_deref()
            .zip(instance_id)
            .is_some_and(|(a, b)| a != b)
    {
        return Err(bridge_error(
            "pairing_changed",
            "Расширение цели закреплено параллельно другим экземпляром; повторите команду",
        ));
    }
    let mut changed = false;
    if current.extension_id.is_none() {
        current.extension_id = Some(extension_id.to_owned());
        changed = true;
    }
    if current.instance_id.is_none()
        && let Some(instance) = instance_id
    {
        current.instance_id = Some(instance.to_owned());
        changed = true;
    }
    let mut state = current.clone();
    state.target = expected.target.clone();
    if changed {
        write(state_dir, &file, legacy.as_deref())?;
    }
    Ok(state)
}

/// Список целей без секретов.
pub fn describe(state_dir: &Path) -> Result<Value, AssetError> {
    let Some(file) = load_file(state_dir)? else {
        return Ok(json!({"configured":false,"default_target":null,"targets":[]}));
    };
    Ok(json!({
        "configured": true,
        "schema_version": file.schema_version,
        "default_target": file.default_target,
        "targets": file.targets.iter().map(|(name, t)| json!({
            "target": name, "browser": t.browser, "platform": t.platform, "executable": t.executable,
            "port": t.port, "load_unpacked_path": t.load_unpacked_path,
            "pinned_extension_id": t.extension_id, "instance_pinned": t.instance_id.is_some(),
            "command": command_prefix(name),
        })).collect::<Vec<_>>(),
    }))
}

/// Делает цель целью по умолчанию для `forvo browser` без `--target`.
pub fn select(state_dir: &Path, target: &str) -> Result<Value, AssetError> {
    let _lock = lock(state_dir)?;
    let (mut file, legacy) = read(state_dir)?.ok_or_else(not_configured)?;
    file.resolve(Some(target))?;
    let previous = file.default_target.replace(target.to_owned());
    if previous.as_deref() != Some(target) || legacy.is_some() {
        write(state_dir, &file, legacy.as_deref())?;
    }
    Ok(json!({"default_target":target,"previous_default":previous}))
}

// --- Файлы расширения ------------------------------------------------------

/// Версия расширения, которую разворачивает этот CLI.
pub fn bundled_version() -> String {
    serde_json::from_str::<Value>(MANIFEST).expect("manifest")["version"]
        .as_str()
        .expect("version")
        .to_owned()
}

fn desired_files(port: u16, secret: &str) -> [(&'static str, String); 4] {
    let pairing = json!({"schema_version":1,"port":port,"secret":secret});
    // manifest.json пишется последним: каталог с новым manifest уже содержит новый код.
    [
        ("forvo_page.js", page_module()),
        ("background.js", BACKGROUND.to_owned()),
        (
            "pairing.json",
            serde_json::to_string(&pairing).expect("JSON"),
        ),
        ("manifest.json", MANIFEST.to_owned()),
    ]
}

/// Наш ли это каталог расширения: manifest есть и называется как наш.
fn manifest_on_disk(dir: &Path) -> Result<Option<Value>, AssetError> {
    match std::fs::read(dir.join("manifest.json")) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let ours: Value = serde_json::from_str(MANIFEST).expect("manifest");
            if value["name"] != ours["name"] {
                return Err(bridge_error(
                    "install_dir_foreign",
                    "В каталоге расширения лежит чужое расширение; задайте другой --install-dir",
                ));
            }
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AssetError::io("manifest.json расширения", e)),
    }
}

/// Приводит каталог к файлам этого CLI. Меняются только отличающиеся файлы,
/// каждый — атомарной заменой; повторный вызов ничего не пишет.
pub fn write_extension_files(
    dir: &Path,
    port: u16,
    secret: &str,
) -> Result<Vec<&'static str>, AssetError> {
    manifest_on_disk(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| AssetError::io("каталог расширения", e))?;
    let mut changed = Vec::new();
    for (name, content) in desired_files(port, secret) {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|b| b == content.as_bytes()) {
            continue;
        }
        write_atomic(&path, content.as_bytes(), 0o600)?;
        changed.push(name);
    }
    Ok(changed)
}

/// Состояние файлов цели без записи: версия на диске и отличия от этого CLI.
pub fn files_status(state: &AdapterState) -> Value {
    let manifest = manifest_on_disk(&state.install_dir);
    let differs: Vec<&str> = desired_files(state.port, &state.secret)
        .iter()
        .filter(|(name, content)| {
            std::fs::read(state.install_dir.join(name)).map_or(true, |b| b != content.as_bytes())
        })
        .map(|(name, _)| *name)
        .collect();
    match manifest {
        Ok(Some(m)) => {
            json!({"present":true,"version_on_disk":m["version"],"bundled_version":bundled_version(),"differs":differs})
        }
        Ok(None) => json!({"present":false,"bundled_version":bundled_version()}),
        Err(e) => json!({"present":true,"error":crate::forvo::provider_code(&e)}),
    }
}

/// Обновляет файлы установленного расширения цели, если каталог уже содержит
/// наше расширение. Вызывается только под занятым портом bridge цели, то есть
/// когда другой процесс этой цели не выполняет загрузку.
pub fn sync_installed_files(state: &AdapterState) -> Result<Option<Vec<&'static str>>, AssetError> {
    if manifest_on_disk(&state.install_dir)?.is_none() {
        return Ok(None);
    }
    write_extension_files(&state.install_dir, state.port, &state.secret).map(Some)
}

// --- Настройка -------------------------------------------------------------

pub(crate) fn windows_local_app_data() -> Result<PathBuf, AssetError> {
    let local = super::windows_command(
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
    super::wsl_to_linux(&local)
}

fn default_install_dir(platform: Platform, target: &str) -> Result<PathBuf, AssetError> {
    // Каталог цели `brave` совпадает с прежним: ID установленного расширения сохраняется.
    let name = format!("forvo-{target}-adapter");
    match platform {
        Platform::Wsl => Ok(windows_local_app_data()?.join("AnkiDeckToolkit").join(name)),
        Platform::Linux => {
            let base = std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .ok_or_else(|| bridge_error("install_dir_unknown", "Задайте --install-dir"))?;
            Ok(base.join("anki-deck-toolkit").join(name))
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub state_dir: PathBuf,
    /// Имя цели; по умолчанию — имя браузера.
    pub target: Option<String>,
    pub browser: Option<BrowserKind>,
    /// Явный исполняемый файл (нестандартная установка).
    pub executable: Option<String>,
    pub install_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub rotate_secret: bool,
    /// Снять закрепление экземпляра и ID (расширение переустановлено).
    pub repin: bool,
    /// Явная платформа браузера (изолированный E2E с Linux-Chromium внутри WSL).
    pub platform: Option<Platform>,
    pub make_default: bool,
    /// Где искать установки; `None` — корни текущей системы.
    pub discovery: Option<DiscoveryRoots>,
}

impl SetupOptions {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            target: None,
            browser: None,
            executable: None,
            install_dir: None,
            port: None,
            rotate_secret: false,
            repin: false,
            platform: None,
            make_default: false,
            discovery: None,
        }
    }
}

fn ambiguous(code: &str, message: &str, found: &[Installation]) -> AssetError {
    let mut e = bridge_error(code, message);
    e.details["installations"] = json!(found);
    e.details["next_action"] = json!(
        "Повторите с --browser brave|chrome|edge|vivaldi и при нескольких установках одного браузера с --executable <путь>"
    );
    e
}

fn port_free(port: u16) -> Option<std::net::TcpListener> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).ok()
}

/// Материализует расширение и сопряжение цели. Повторный вызов обновляет код,
/// сохраняя секрет, порт, каталог и закрепления, если не запрошена ротация.
pub fn setup(options: &SetupOptions) -> Result<Value, AssetError> {
    if let Some(name) = &options.target
        && !valid_target_name(name)
    {
        return Err(bridge_error(
            "invalid_target",
            "Имя цели: 1–32 символа a–z, 0–9, '-', начинается с буквы",
        ));
    }
    let wsl = detect_platform() == Platform::Wsl;
    let _lock = lock(&options.state_dir)?;
    // Повреждённое состояние — отказ, а не тихая «чистая» настройка с новым секретом.
    let (mut file, legacy) = read(&options.state_dir)?.unwrap_or_else(|| {
        (
            AdapterFile {
                schema_version: 2,
                default_target: None,
                targets: BTreeMap::new(),
            },
            None,
        )
    });
    let explicit = match &options.executable {
        Some(path) => Some(browsers::identify(path, wsl)?),
        None => None,
    };
    let lazy_roots = || {
        options
            .discovery
            .clone()
            .unwrap_or_else(|| DiscoveryRoots::system(wsl))
    };
    let mut discovered: Option<Vec<Installation>> = None;
    let discover = |discovered: &mut Option<Vec<Installation>>| {
        discovered
            .get_or_insert_with(|| browsers::discover(&lazy_roots()))
            .clone()
    };
    // Имя цели: явное, по браузеру, по исполняемому файлу, повтор цели по умолчанию
    // или единственный найденный браузер.
    let name = match (&options.target, options.browser, &explicit) {
        (Some(name), _, _) => name.clone(),
        (None, Some(kind), _) => kind.id().to_owned(),
        (None, None, Some(found)) => found.browser.id().to_owned(),
        (None, None, None) => match &file.default_target {
            Some(name) => name.clone(),
            None => {
                let found: Vec<Installation> = discover(&mut discovered)
                    .into_iter()
                    .filter(|i| BrowserKind::USER.contains(&i.browser))
                    .collect();
                let mut kinds: Vec<BrowserKind> = found.iter().map(|i| i.browser).collect();
                kinds.dedup();
                match kinds.as_slice() {
                    [kind] => kind.id().to_owned(),
                    [] => {
                        return Err(ambiguous(
                            "browser_not_found",
                            "Поддерживаемый браузер не найден; укажите --browser и --executable",
                            &found,
                        ));
                    }
                    _ => {
                        return Err(ambiguous(
                            "browser_ambiguous",
                            "Найдено несколько браузеров; выберите один через --browser",
                            &found,
                        ));
                    }
                }
            }
        },
    };
    if !valid_target_name(&name) {
        return Err(bridge_error("invalid_target", "Недопустимое имя цели"));
    }
    let previous = file.targets.get(&name).cloned();
    let kind = match (&previous, options.browser, &explicit) {
        (Some(p), Some(k), _) if p.browser != k => {
            let mut e = bridge_error(
                "target_browser_mismatch",
                "Цель закреплена за другим браузером; для этого браузера создайте отдельную цель (--target <имя>)",
            );
            e.details["target_browser"] = json!(p.browser);
            return Err(e);
        }
        (Some(p), _, _) => p.browser,
        (None, Some(k), _) => k,
        (None, None, Some(found)) => found.browser,
        (None, None, None) => BrowserKind::parse(&name).ok_or_else(|| {
            bridge_error(
                "browser_required",
                "Для новой цели укажите --browser brave|chrome|edge|vivaldi",
            )
        })?,
    };
    if let Some(found) = &explicit
        && found.browser != kind
    {
        let mut e = bridge_error(
            "browser_kind_mismatch",
            "Исполняемый файл принадлежит другому браузеру",
        );
        e.details["found"] = json!(found);
        e.details["requested"] = json!(kind);
        return Err(e);
    }
    // Установка: явный файл, ранее подтверждённый файл или однозначное обнаружение.
    let installation: Option<Installation> = match &explicit {
        Some(found) => Some(found.clone()),
        None => {
            let candidates: Vec<Installation> = discover(&mut discovered)
                .into_iter()
                .filter(|i| i.browser == kind)
                .filter(|i| options.platform.is_none_or(|p| p == i.platform))
                .collect();
            let remembered = previous.as_ref().and_then(|p| p.executable.clone());
            match (remembered, candidates.as_slice()) {
                (Some(path), _) => candidates.iter().find(|i| i.executable == path).cloned(),
                (None, [one]) => Some(one.clone()),
                (None, []) => None,
                (None, many) => {
                    if previous.is_some() || options.platform.is_some() {
                        None
                    } else {
                        return Err(ambiguous(
                            "installation_ambiguous",
                            "Найдено несколько установок этого браузера; укажите --executable",
                            many,
                        ));
                    }
                }
            }
        }
    };
    let platform = options
        .platform
        .or(explicit.as_ref().map(|i| i.platform))
        .or(previous.as_ref().map(|p| p.platform))
        .or(installation.as_ref().map(|i| i.platform))
        .unwrap_or_else(detect_platform);
    if platform == Platform::Wsl && !wsl {
        return Err(bridge_error(
            "browser_platform_unsupported",
            "Браузер Windows доступен только из WSL",
        ));
    }
    let install_dir = match (&options.install_dir, &previous) {
        (Some(dir), _) => {
            std::path::absolute(dir).map_err(|e| AssetError::io("каталог расширения", e))?
        }
        (None, Some(state)) if state.platform == platform => state.install_dir.clone(),
        _ => default_install_dir(platform, &name)?,
    };
    let others = || file.targets.iter().filter(|(n, _)| **n != name);
    if let Some((other, _)) = others().find(|(_, t)| t.install_dir == install_dir) {
        let mut e = bridge_error(
            "install_dir_conflict",
            "Каталог расширения уже принадлежит другой цели: общий каталог дал бы общий ID и секрет",
        );
        e.details["other_target"] = json!(other);
        return Err(e);
    }
    let taken: Vec<u16> = others().map(|(_, t)| t.port).collect();
    let requested_port = options.port.or(previous.as_ref().map(|p| p.port));
    if let Some(port) = requested_port {
        if port < 1024 {
            return Err(bridge_error("invalid_port", "Порт должен быть 1024–65535"));
        }
        if let Some((other, _)) = others().find(|(_, t)| t.port == port) {
            let mut e = bridge_error(
                "port_conflict",
                "Порт bridge уже принадлежит другой цели; задайте другой --port",
            );
            e.details["other_target"] = json!(other);
            return Err(e);
        }
    }
    // Занятый порт служит блокировкой: пока он открыт, процесс этой цели не
    // выполняет загрузку, и файлы расширения можно менять.
    let (port, _port_lock) = match requested_port {
        Some(port) => match port_free(port) {
            Some(listener) => (port, listener),
            None if previous.is_some() && options.port.is_none() => {
                return Err(bridge_error(
                    "adapter_busy",
                    "Порт bridge цели занят: идёт fetch/пакет этой цели или порт занят другой программой. Дождитесь завершения и повторите setup; при чужой программе задайте --port",
                ));
            }
            None => {
                return Err(bridge_error(
                    "bridge_port_busy",
                    "Порт bridge занят другой программой; задайте другой --port",
                ));
            }
        },
        None => (DEFAULT_PORT..DEFAULT_PORT + PORT_SPAN)
            .filter(|p| !taken.contains(p))
            .find_map(|p| port_free(p).map(|l| (p, l)))
            .ok_or_else(|| {
                bridge_error(
                    "bridge_port_busy",
                    "Свободный порт bridge не найден; задайте --port",
                )
            })?,
    };
    let keep = previous.as_ref().filter(|p| {
        !options.rotate_secret && p.install_dir == install_dir && p.platform == platform
    });
    let secret = match keep {
        Some(state) => state.secret.clone(),
        None => encode_lower_hex(random_bytes()?),
    };
    let changed = write_extension_files(&install_dir, port, &secret)?;
    let load_unpacked_path = match platform {
        Platform::Wsl => linux_to_windows(&install_dir)?,
        Platform::Linux => install_dir.display().to_string(),
    };
    let (extension_id, instance_id) = match keep {
        Some(p) if !options.repin => (p.extension_id.clone(), p.instance_id.clone()),
        _ => (None, None),
    };
    let state = AdapterState {
        target: name.clone(),
        browser: kind,
        port,
        secret,
        install_dir: install_dir.clone(),
        load_unpacked_path: load_unpacked_path.clone(),
        platform,
        extension_id,
        instance_id,
        executable: installation
            .as_ref()
            .map(|i| i.executable.clone())
            .or_else(|| keep.and_then(|p| p.executable.clone())),
    };
    let unchanged_state = previous.as_ref() == Some(&state) && legacy.is_none();
    file.targets.insert(name.clone(), state.clone());
    if file.default_target.is_none() || options.make_default {
        file.default_target = Some(name.clone());
    }
    if !unchanged_state || options.make_default {
        write(&options.state_dir, &file, legacy.as_deref())?;
    }
    tracing::info!(
        event = "browser_adapter_setup",
        stage = "setup",
        target = %name,
        browser = kind.id(),
        port,
        pairing_created = keep.is_none(),
        files_changed = changed.len(),
        "Расширение Forvo подготовлено"
    );
    let expected = state.expected_extension_ids();
    let user_action = match (&state.extension_id, changed.is_empty()) {
        (None, _) => json!(state.user_action()),
        (Some(_), true) => Value::Null,
        (Some(_), false) => json!(format!(
            "Файлы расширения обновлены; оно перезагрузится автоматически при следующей команде {} (или сразу: doctor --reload)",
            state.command_prefix()
        )),
    };
    Ok(json!({
        "target": name,
        "browser": kind,
        "browser_name": kind.display(),
        "support_note": kind.support_note(),
        "platform": platform,
        "installation": match &installation {
            Some(found) => json!({"status":"confirmed","executable":found.executable,"version":found.version,
                "chromium_major":found.chromium_major,"identified_by":found.identified_by}),
            None => json!({"status":"not_confirmed","executable":state.executable,
                "note":"установка браузера не подтверждена по исполняемому файлу; укажите --executable при нестандартном пути"}),
        },
        "install_dir": install_dir,
        "load_unpacked_path": load_unpacked_path,
        "extensions_page": kind.extensions_uri(),
        "expected_extension_id": expected.first(),
        "bridge": format!("ws://127.0.0.1:{port}{WS_PATH}"),
        "pairing": if keep.is_some() {"kept"} else {"created"},
        "pinned_extension_id": state.extension_id,
        "instance_pinned": state.instance_id.is_some(),
        "extension_files": {"version": bundled_version(), "changed": changed},
        "default_target": file.default_target,
        "targets": file.targets.keys().collect::<Vec<_>>(),
        "command": state.command_prefix(),
        "state_file": options.state_dir.join(STATE_FILE),
        "migrated_from_v1": legacy.is_some(),
        "user_action": user_action,
        "downloads_setting": format!("{}: «Спрашивать, куда сохранять каждый файл» должно быть выключено, иначе загрузка остановится с save_as_required", kind.downloads_settings_uri()),
    }))
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
