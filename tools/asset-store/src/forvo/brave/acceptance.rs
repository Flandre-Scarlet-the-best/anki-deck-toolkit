//! Живые проверки адаптера браузера: `doctor` (транспорт и вкладка) и `smoke`
//! (одна японская запись до проверенных байтов MP3). Хранилище ресурсов не меняется.
use super::download::{self, Limits};
use super::targets::{self, bundled_version};
use super::{
    AdapterState, Bridge, GROUP_TITLE, Peer, Platform, REQUIRED_CAPABILITIES, Trust, bridge_error,
    browser_path_to_local, load_target,
};
use crate::forvo::candidates::{DomResult, validated};
use crate::forvo::{
    AuthObservation, AuthState, Candidate, RANKING, auth_error, provider_code, word_url,
};
use crate::pronunciation::{MAX_AUDIO_BYTES, Pronunciation, probe_mp3};
use crate::{AssetError, ErrorCode, hashing::sha256_hex};
use serde::Serialize;
use serde_json::{Value, json};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Pass,
    Fail,
    Blocked,
    NotTested,
    Interrupted,
}

#[derive(Debug, Serialize)]
pub struct Step {
    pub name: &'static str,
    pub status: Status,
    pub evidence: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub steps: Vec<Step>,
}
impl Report {
    fn push(&mut self, name: &'static str, status: Status, evidence: Value) {
        tracing::info!(event = "brave_acceptance_step", stage = "acceptance", step = name, status = ?status, "Шаг проверки адаптера браузера");
        self.steps.push(Step {
            name,
            status,
            evidence,
        });
    }
    fn fail(&mut self, name: &'static str, error: &AssetError) {
        let code = provider_code(error).to_owned();
        let status = if code == "interrupted" || error.code == ErrorCode::Interrupted {
            Status::Interrupted
        } else if matches!(
            code.as_str(),
            "challenge_required"
                | "login_required"
                | "session_expired"
                | "access_denied"
                | "rate_limited"
                | "extension_not_connected"
                | "save_as_required"
                | "save_as_suspected"
                | "download_conflict"
        ) {
            Status::Blocked
        } else {
            Status::Fail
        };
        // Причина, устранимая настройкой браузера цели (например, подтверждение загрузок
        // Vivaldi), — BLOCKED с действием, а не неисправность адаптера.
        let status = if status == Status::Fail && error.details["setting_required"] == true {
            Status::Blocked
        } else {
            status
        };
        let mut evidence = json!({"code":code,"message":error.message});
        if let Some(action) = error.details.get("next_action") {
            evidence["next_action"] = action.clone();
        }
        if let Some(status) = error.details.get("status") {
            evidence["status"] = status.clone();
        }
        self.push(name, status, evidence);
    }
    pub fn passed(&self) -> bool {
        self.steps
            .iter()
            .all(|s| matches!(s.status, Status::Pass | Status::NotTested))
    }
}

fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u64::MAX as u128) as u64
}

/// Bridge, подключённое расширение выбранной цели и закрепление его ID и
/// экземпляра после первого сопряжения.
pub struct Connection {
    pub bridge: Bridge,
    pub state: AdapterState,
    pub peer: Peer,
    /// Что сделано для приведения расширения к версии этого CLI.
    pub update: Value,
}

/// Режим сети WSL2 (`mirrored`/`nat`/…); `None` — вне WSL или неизвестно.
pub fn wsl_networking_mode() -> Option<String> {
    let output = std::process::Command::new("wslinfo")
        .arg("--networking-mode")
        .output()
        .ok()?;
    let mode = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !mode.is_empty() && mode.len() <= 32).then_some(mode)
}

fn connect_action(state: &AdapterState) -> String {
    let mut action = if state.extension_id.is_none() {
        state.user_action()
    } else {
        format!(
            "Запустите {} и проверьте, что расширение «Anki Deck Toolkit: Forvo adapter» включено на {}",
            state.browser.display(),
            state.browser.extensions_uri()
        )
    };
    if state.platform == Platform::Wsl && wsl_networking_mode().as_deref() == Some("nat") {
        action.push_str(
            ". WSL2 работает в режиме NAT: браузер Windows достигает bridge только через localhost forwarding WSL (localhostForwarding=true); если он выключен, включите networkingMode=mirrored или localhostForwarding в .wslconfig сами — программа сетевые настройки не меняет",
        );
    }
    action
}

/// Подключает расширение выбранной цели. Пока порт bridge занят этим
/// процессом, другая команда этой цели не работает, поэтому здесь же
/// обновляются файлы установленного расширения и, если версия отличается,
/// расширение штатно перезагружается командой `reload` — но только когда у
/// него нет незавершённой операции загрузки.
pub async fn connect(
    state_dir: &Path,
    target: Option<&str>,
    wait: Duration,
) -> Result<Connection, AssetError> {
    let state = load_target(state_dir, target)?;
    let mut bridge = Bridge::bind_with(
        state.port,
        state.secret_bytes()?,
        Trust {
            pinned: state.extension_id.clone(),
            expected: state.expected_extension_ids(),
            instance: state.instance_id.clone(),
        },
    )
    .await?;
    let files = targets::sync_installed_files(&state)?;
    let with_action = |mut e: AssetError, state: &AdapterState| {
        if e.details["user_action"].is_null() {
            e.details["user_action"] = json!(connect_action(state));
        }
        e.details["target"] = json!(state.target);
        e.details["browser"] = json!(state.browser);
        e
    };
    let peer = bridge
        .accept(Instant::now() + wait)
        .await
        .map_err(|e| with_action(e, &state))?;
    let mut state = pin(state_dir, &state, &mut bridge, &peer)?;
    let bundled = bundled_version();
    let mut update = json!({"files_changed":files,"extension_version":peer.version,"bundled_version":bundled,"reload":"not_needed"});
    let mut peer = peer;
    if peer.version != bundled {
        if files.is_none() {
            update["reload"] = json!("files_not_installed");
        } else {
            let ping = bridge
                .call("ping", json!({}), Duration::from_secs(5))
                .await?;
            if !ping["operation"].is_null() {
                // Незавершённую операцию сначала освобождает обычный путь
                // восстановления; обновление откладывается до следующего запуска.
                update["reload"] = json!("deferred_operation_pending");
            } else {
                let started = Instant::now();
                tracing::info!(event = "extension_update_reload", stage = "transport", from = %peer.version, to = %bundled,
                    "Расширение перезагружается для обновления до версии CLI");
                bridge
                    .call("reload", json!({}), Duration::from_secs(5))
                    .await?;
                peer = bridge
                    .accept(Instant::now() + Duration::from_secs(60))
                    .await
                    .map_err(|e| with_action(e, &state))?;
                state = pin(state_dir, &state, &mut bridge, &peer)?;
                if peer.version != bundled {
                    let mut e = bridge_error(
                        "extension_reload_ineffective",
                        "После перезагрузки расширение осталось прежней версии: браузер загрузил его не из каталога цели или не перечитал файлы",
                    );
                    e.details["user_action"] = json!(format!(
                        "Нажмите «Обновить» у расширения на {} или загрузите распакованное из {}",
                        state.browser.extensions_uri(),
                        state.load_unpacked_path
                    ));
                    e.details["extension_version"] = json!(peer.version);
                    return Err(e);
                }
                update["reload"] = json!("reloaded");
                update["reload_ms"] = json!(duration_ms(started.elapsed()));
                update["extension_version"] = json!(peer.version);
            }
        }
    }
    Ok(Connection {
        bridge,
        state,
        peer,
        update,
    })
}

/// Закрепляет ID и экземпляр расширения (первое подключение — единственный акт
/// доверия после установки из каталога цели).
fn pin(
    state_dir: &Path,
    state: &AdapterState,
    bridge: &mut Bridge,
    peer: &Peer,
) -> Result<AdapterState, AssetError> {
    let unchanged = state.extension_id.as_deref() == Some(peer.extension_id.as_str())
        && (peer.instance.is_none() || state.instance_id == peer.instance);
    let state = if unchanged {
        state.clone()
    } else {
        targets::pin_peer(
            state_dir,
            state,
            &peer.extension_id,
            peer.instance.as_deref(),
        )?
    };
    bridge.pin(&peer.extension_id, state.instance_id.as_deref());
    Ok(state)
}

async fn ping_stats(bridge: &mut Bridge, count: usize) -> Result<Value, AssetError> {
    let mut samples = Vec::with_capacity(count);
    let mut last = Value::Null;
    for _ in 0..count {
        let started = Instant::now();
        last = bridge
            .call("ping", json!({}), Duration::from_secs(5))
            .await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    Ok(
        json!({"samples":count,"median_ms":(samples[count / 2] * 100.0).round() / 100.0,"max_ms":(samples[count - 1] * 100.0).round() / 100.0,"ping":last}),
    )
}

async fn observe_auth(bridge: &mut Bridge) -> Result<AuthObservation, AssetError> {
    let value = bridge
        .call("auth", json!({}), Duration::from_secs(10))
        .await?;
    serde_json::from_value(value)
        .map_err(|_| bridge_error("dom_changed", "Неизвестные признаки авторизации Forvo"))
}

/// Ограниченное ожидание: одна загрузка страницы, без повторов при Challenge.
pub(crate) async fn wait_auth(
    bridge: &mut Bridge,
    wait: Duration,
) -> Result<(AuthState, AuthObservation), AssetError> {
    let deadline = Instant::now() + wait;
    loop {
        let observation = match observe_auth(bridge).await {
            Ok(observation) => Some(observation),
            Err(e) if provider_code(&e) == "injection_failed" => None,
            Err(e) => return Err(e),
        };
        if let Some(observation) = observation {
            if observation.status == 429 && !observation.challenge {
                return Err(bridge_error(
                    "rate_limited",
                    "Forvo ограничил запросы; продолжите позже",
                ));
            }
            let state = observation.state();
            if state != AuthState::PageUnstable || Instant::now() >= deadline {
                return Ok((state, observation));
            }
        } else if Instant::now() >= deadline {
            return Err(bridge_error(
                "page_unstable",
                "Страница Forvo не стала доступной для чтения",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Собственная вкладка на точной странице слова с подтверждённой авторизацией.
/// Блокировка доступа возвращается типизированной ошибкой без повторов.
pub async fn open_word(bridge: &mut Bridge, surface: &str) -> Result<Value, AssetError> {
    let opened = bridge
        .call("open", json!({}), Duration::from_secs(15))
        .await?;
    let navigation = bridge
        .call(
            "navigate",
            json!({"url":word_url(surface),"timeout_ms":30000}),
            Duration::from_secs(40),
        )
        .await?;
    let (state, observation) = wait_auth(bridge, Duration::from_secs(20)).await?;
    if state != AuthState::Authenticated {
        return Err(auth_error(state));
    }
    if observation.status == 404 {
        return Err(bridge_error(
            "word_not_found",
            "Forvo вернул 404 для точной страницы слова",
        ));
    }
    Ok(
        json!({"tab":opened,"load":navigation["load"],"path":navigation["path"],"status":observation.status}),
    )
}

pub async fn candidates(
    bridge: &mut Bridge,
    request: &Pronunciation,
) -> Result<Vec<Candidate>, AssetError> {
    let value = bridge
        .call(
            "dom",
            json!({"surface":request.surface,"action":"expand"}),
            Duration::from_secs(15),
        )
        .await?;
    let result: DomResult = serde_json::from_value(value)
        .map_err(|_| bridge_error("dom_changed", "Неизвестная структура кандидатов Forvo"))?;
    validated(result)
}

/// Адрес загрузки без query/fragment/учётных данных из безопасного описания расширения.
pub(crate) fn observed_url(value: &Value) -> Option<String> {
    let clean = value["has_query"] == false
        && value["has_fragment"] == false
        && value["has_credentials"] == false;
    if !clean {
        return None;
    }
    Some(format!(
        "{}{}",
        value["origin"].as_str()?,
        value["path"].as_str()?
    ))
}

fn mp3_signature(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"ID3") {
        "id3"
    } else if bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0 {
        "mpeg_frame_sync"
    } else {
        "unknown"
    }
}

fn write_evidence(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, AssetError> {
    super::private_dir(dir)?;
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| AssetError::io("файл свидетельства", e))?;
    std::io::Write::write_all(&mut file, bytes)
        .map_err(|e| AssetError::io("файл свидетельства", e))?;
    file.sync_all()
        .map_err(|e| AssetError::io("файл свидетельства", e))?;
    Ok(path)
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// --- doctor --------------------------------------------------------------

pub struct DoctorOptions {
    pub state_dir: PathBuf,
    /// Цель; `None` — цель по умолчанию (или единственная).
    pub target: Option<String>,
    pub wait: Duration,
    pub forvo: bool,
    pub reconnect: bool,
    pub reload: bool,
}

fn proxy_names() -> Vec<&'static str> {
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .into_iter()
    .filter(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()))
    .collect()
}

/// Новую вкладку адаптер создаёт неактивной; повторно используемую мог
/// активировать сам пользователь (например, проходя проверку сайта вручную).
pub(crate) fn own_tab_grouped(opened: &Value) -> bool {
    opened["group_title"] == GROUP_TITLE
        && (opened["created"] == false || opened["active"] == false)
}

/// Локальные IPv4-адреса: `ip -j` или, где iproute2 нет (минимальный контейнер),
/// таблица `/proc/net/fib_trie`.
fn local_ipv4() -> Result<(Vec<std::net::Ipv4Addr>, &'static str), &'static str> {
    if let Ok(output) = std::process::Command::new("ip")
        .args(["-j", "-4", "addr", "show"])
        .output()
    {
        let interfaces = serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .map_err(|_| "ip_output_unknown")?;
        let addresses = interfaces
            .iter()
            .flat_map(|i| i["addr_info"].as_array().cloned().unwrap_or_default())
            .filter_map(|a| a["local"].as_str().and_then(|s| s.parse().ok()))
            .collect();
        return Ok((addresses, "ip"));
    }
    let table = std::fs::read_to_string("/proc/net/fib_trie").map_err(|_| "ip_unavailable")?;
    Ok((fib_trie_local(&table), "fib_trie"))
}

/// Адреса с пометкой `/32 host LOCAL` в `/proc/net/fib_trie`.
pub(crate) fn fib_trie_local(table: &str) -> Vec<std::net::Ipv4Addr> {
    let mut addresses = Vec::new();
    let mut previous: Option<&str> = None;
    for line in table.lines() {
        let line = line.trim();
        if line == "/32 host LOCAL"
            && let Some(address) = previous
                .and_then(|p| p.rsplit(' ').next())
                .and_then(|a| a.parse().ok())
            && !addresses.contains(&address)
        {
            addresses.push(address);
        }
        previous = Some(line);
    }
    addresses
}

/// Проверяет, что порт bridge недоступен по нелокальным адресам этого хоста.
fn exposure_check(port: u16) -> (Status, Value) {
    let (addresses, source) = match local_ipv4() {
        Ok(found) => found,
        Err(reason) => return (Status::NotTested, json!({"reason":reason})),
    };
    let addresses = addresses
        .into_iter()
        .filter(|a: &std::net::Ipv4Addr| !a.is_loopback())
        .collect::<Vec<_>>();
    let exposed = addresses
        .iter()
        .filter(|a| {
            std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from((**a, port)),
                Duration::from_millis(300),
            )
            .is_ok()
        })
        .count();
    if addresses.is_empty() {
        (Status::NotTested, json!({"reason":"no_non_loopback_ipv4"}))
    } else {
        (
            if exposed == 0 {
                Status::Pass
            } else {
                Status::Fail
            },
            json!({"listen":"127.0.0.1","address_source":source,"non_loopback_addresses_checked":addresses.len(),"reachable":exposed}),
        )
    }
}

/// Установка браузера цели: ранее подтверждённый файл или обнаружение.
fn installation_check(state: &AdapterState) -> (Status, Value) {
    use super::browsers;
    let wsl = super::detect_platform() == Platform::Wsl;
    match &state.executable {
        Some(path) => match browsers::identify(path, wsl) {
            Ok(found) if found.browser == state.browser => (Status::Pass, json!(found)),
            Ok(found) => (
                Status::Fail,
                json!({"code":"browser_kind_mismatch","expected":state.browser,"found":found,
                    "next_action":format!("Исполняемый файл цели теперь другой браузер; выполните setup с --executable {} нужного браузера", state.browser.id())}),
            ),
            Err(e) => (
                Status::Blocked,
                json!({"code":crate::forvo::provider_code(&e),"executable":path,
                    "next_action":"Браузер цели не найден по сохранённому пути; переустановите его или выполните setup с --executable"}),
            ),
        },
        None => {
            let found: Vec<_> = browsers::discover(&browsers::DiscoveryRoots::system(
                state.platform == Platform::Wsl,
            ))
            .into_iter()
            .filter(|i| i.browser == state.browser && i.platform == state.platform)
            .collect();
            if found.is_empty() {
                (
                    Status::NotTested,
                    json!({"reason":"установка не найдена в каталогах по умолчанию; при нестандартном пути выполните setup с --executable"}),
                )
            } else {
                (Status::Pass, json!({"discovered":found}))
            }
        }
    }
}

/// Сверяет браузер, о котором сообщает само расширение, с браузером цели.
pub(crate) fn identity_check(kind: super::BrowserKind, facts: &Value) -> (Status, Value) {
    use super::BrowserKind;
    if facts.is_null() {
        return (
            Status::NotTested,
            json!({"reason":"расширение до версии 0.3 не сообщает сведений о браузере"}),
        );
    }
    let brands: Vec<&str> = facts["brands"]
        .as_array()
        .map(|a| a.iter().filter_map(|b| b["brand"].as_str()).collect())
        .unwrap_or_default();
    let known = ["Brave", "Google Chrome", "Microsoft Edge"];
    let foreign: Vec<&str> = brands
        .iter()
        .copied()
        .filter(|b| known.contains(b) && Some(*b) != kind.brand())
        .collect();
    let evidence = json!({"browser":kind,"brands":brands});
    let mismatch = || {
        (
            Status::Fail,
            json!({"code":"browser_kind_mismatch","browser":kind,"brands":brands,
                "next_action":"Расширение этой цели загружено в другой браузер; загрузите его в браузер цели или настройте отдельную цель"}),
        )
    };
    match kind {
        BrowserKind::Vivaldi => {
            if brands.contains(&"Brave") || brands.contains(&"Microsoft Edge") {
                mismatch()
            } else {
                (
                    Status::NotTested,
                    json!({"browser":kind,"brands":brands,"reason":"Vivaldi представляется брендом Google Chrome и по userAgentData неотличим; принадлежность подтверждают каталог цели и закреплённый экземпляр"}),
                )
            }
        }
        BrowserKind::Chromium => {
            if foreign.is_empty() {
                (Status::Pass, evidence)
            } else {
                mismatch()
            }
        }
        _ => {
            let own = kind.brand().is_some_and(|b| brands.contains(&b));
            // Brave и Edge добавляют свой бренд, «Google Chrome» — только Chrome и Vivaldi.
            let conflicting = foreign
                .iter()
                .any(|b| !(kind == BrowserKind::Brave && *b == "Google Chrome"));
            if own && !conflicting {
                let mut evidence = evidence;
                if kind == BrowserKind::Chrome {
                    evidence["note"] = json!("бренд Google Chrome сообщает и Vivaldi");
                }
                (Status::Pass, evidence)
            } else if conflicting || !brands.is_empty() {
                mismatch()
            } else {
                (
                    Status::NotTested,
                    json!({"browser":kind,"reason":"браузер не сообщил бренды userAgentData"}),
                )
            }
        }
    }
}

/// Минимальная версия Chromium из manifest расширения.
pub(crate) fn minimum_chromium() -> u32 {
    serde_json::from_str::<Value>(super::MANIFEST).expect("manifest")["minimum_chrome_version"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .expect("minimum_chrome_version")
}

/// Возможности API, которые сообщил сам браузер. Недостаток — `BLOCKED` с
/// точной причиной, а не предположение «раз Chromium, значит работает».
pub(crate) fn capabilities_check(facts: &Value) -> (Status, Value) {
    if facts.is_null() {
        return (
            Status::NotTested,
            json!({"reason":"расширение до версии 0.3 не сообщает возможностей браузера"}),
        );
    }
    let missing: Vec<&str> = REQUIRED_CAPABILITIES
        .iter()
        .copied()
        .filter(|k| facts["capabilities"][k] != true)
        .collect();
    let chromium: Option<u32> = facts["chromium_version"]
        .as_str()
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse().ok());
    let minimum = minimum_chromium();
    let evidence = json!({"chromium_version":facts["chromium_version"],"minimum_chromium":minimum,
        "manifest_version":facts["manifest_version"],"capabilities":facts["capabilities"],"missing":missing});
    if !missing.is_empty() {
        let mut evidence = evidence;
        evidence["code"] = json!("browser_capability_missing");
        return (Status::Blocked, evidence);
    }
    match chromium {
        Some(major) if major >= minimum => (Status::Pass, evidence),
        Some(_) => {
            let mut evidence = evidence;
            evidence["code"] = json!("browser_version_unsupported");
            (Status::Blocked, evidence)
        }
        None => {
            let mut evidence = evidence;
            evidence["reason"] = json!("версия Chromium не сообщена");
            (Status::NotTested, evidence)
        }
    }
}

pub async fn doctor(options: &DoctorOptions) -> Result<Report, AssetError> {
    let mut report = Report::default();
    let state = load_target(&options.state_dir, options.target.as_deref())?;
    let expected = state.expected_extension_ids();
    report.push("configuration", Status::Pass, json!({"target":state.target,"browser":state.browser,"platform":state.platform,
        "executable":state.executable,"load_unpacked_path":state.load_unpacked_path,"extensions_page":state.browser.extensions_uri(),
        "expected_extension_id":expected.first(),"pinned_extension_id":state.extension_id,
        "instance_pinned":state.instance_id.is_some(),"port":state.port,"command":state.command_prefix(),
        "support_note":state.browser.support_note()}));
    let (status, evidence) = installation_check(&state);
    report.push("installation", status, evidence);
    let files = targets::files_status(&state);
    report.push(
        "extension_files",
        if files["present"] == true && files["error"].is_null() {
            Status::Pass
        } else {
            Status::Fail
        },
        json!({"before_connect":files,"install_dir":state.install_dir,
            "next_action":if files["present"] == true {Value::Null} else {json!(format!("Выполните {} setup", state.command_prefix()))}}),
    );
    let networking = (state.platform == Platform::Wsl).then(wsl_networking_mode);
    report.push("proxy_environment", Status::Pass, json!({"proxy_variables":proxy_names(),
        "note":"bridge только принимает loopback-подключения и сам не делает исходящих HTTP-запросов; Chromium не проксирует 127.0.0.1"}));
    let mut connection = match connect(&options.state_dir, options.target.as_deref(), options.wait)
        .await
    {
        Ok(connection) => connection,
        Err(e) => {
            if let Some(mode) = &networking {
                report.push("wsl_networking", if mode.as_deref() == Some("nat") {Status::Blocked} else {Status::NotTested},
                    json!({"mode":mode,"note":"браузер Windows не подключился к bridge в WSL; в NAT это зависит от localhost forwarding, программа сетевые настройки не меняет"}));
            }
            report.fail("extension_connected", &e);
            if let Some(last) = report.steps.last_mut() {
                last.evidence["rejections"] = e.details["rejections"].clone();
                last.evidence["user_action"] = e.details["user_action"].clone();
            }
            return Ok(report);
        }
    };
    if let Some(mode) = &networking {
        report.push("wsl_networking", Status::Pass, json!({"mode":mode,
            "evidence":"браузер Windows подключился к bridge на 127.0.0.1 в WSL и прошёл взаимную проверку HMAC"}));
    }
    let (status, evidence) = exposure_check(connection.bridge.port());
    report.push("loopback_only", status, evidence);
    let peer = &connection.peer;
    report.push(
        "extension_connected",
        Status::Pass,
        json!({"extension_id":peer.extension_id,"version":peer.version,"from_target_directory":expected.contains(&peer.extension_id),
            "instance_pinned":connection.state.instance_id.is_some(),"rejected_connections":connection.bridge.rejections()}),
    );
    report.push(
        "extension_version",
        if peer.version == bundled_version() {
            Status::Pass
        } else {
            Status::Fail
        },
        connection.update.clone(),
    );
    let (status, evidence) = identity_check(connection.state.browser, &peer.browser);
    report.push("browser_identity", status, evidence);
    let (status, evidence) = capabilities_check(&peer.browser);
    report.push("browser_capabilities", status, evidence);
    report.push("download_settings", Status::NotTested, json!({"setting":connection.state.browser.downloads_settings_uri(),
        "reason":"расширение не читает настройки браузера; «Спрашивать, куда сохранять каждый файл» должно быть выключено. Фактическое поведение проверяет smoke (save_as_required)"}));
    let stats = match ping_stats(&mut connection.bridge, 5).await {
        Ok(stats) => stats,
        Err(e) => {
            report.fail("command_latency", &e);
            return Ok(report);
        }
    };
    report.push("command_latency", Status::Pass, stats.clone());
    // Фактическое состояние событий браузера: у расширения нет перехватчика имён
    // загрузок, а наблюдатель запросов существует только во время собственной операции.
    let listeners = &stats["ping"]["listeners"];
    let operation = &stats["ping"]["operation"];
    let clean = listeners["determining_filename"] == false
        && (listeners["web_request"] == false || !operation.is_null());
    report.push(
        "download_listeners",
        if clean { Status::Pass } else { Status::Fail },
        json!({"listeners":listeners,"pending_operation":operation,
            "note":"onDeterminingFilename не регистрируется; webRequest только наблюдает адрес выбранной записи во время операции"}),
    );
    if options.forvo {
        let result = async {
            let opened = connection
                .bridge
                .call("open", json!({}), Duration::from_secs(15))
                .await?;
            connection
                .bridge
                .call(
                    "navigate",
                    json!({"url":"https://forvo.com/","timeout_ms":30000}),
                    Duration::from_secs(40),
                )
                .await?;
            let (state, observation) =
                wait_auth(&mut connection.bridge, Duration::from_secs(20)).await?;
            Ok::<_, AssetError>((opened, state, observation))
        }
        .await;
        match result {
            Ok((opened, AuthState::Authenticated, observation)) => {
                report.push(
                    "own_tab_group",
                    if own_tab_grouped(&opened) {
                        Status::Pass
                    } else {
                        Status::Fail
                    },
                    opened,
                );
                report.push(
                    "forvo_auth",
                    Status::Pass,
                    json!({"auth_state":AuthState::Authenticated,"status":observation.status}),
                );
            }
            Ok((opened, state, observation)) => {
                report.push("own_tab_group", Status::Pass, opened);
                report.push("forvo_auth", Status::Blocked, json!({"auth_state":state,"status":observation.status,"challenge":observation.challenge}));
            }
            Err(e) => report.fail("forvo_auth", &e),
        }
    }
    if options.reconnect {
        connection.bridge.disconnect().await;
        let started = Instant::now();
        match connection
            .bridge
            .accept(Instant::now() + Duration::from_secs(40))
            .await
        {
            Ok(_) => match connection
                .bridge
                .call("ping", json!({}), Duration::from_secs(5))
                .await
            {
                Ok(_) => report.push(
                    "bridge_reconnect",
                    Status::Pass,
                    json!({"reconnect_ms":duration_ms(started.elapsed())}),
                ),
                Err(e) => report.fail("bridge_reconnect", &e),
            },
            Err(e) => report.fail("bridge_reconnect", &e),
        }
    }
    if options.reload {
        let before = connection
            .bridge
            .call("tab_status", json!({}), Duration::from_secs(5))
            .await
            .unwrap_or(Value::Null);
        let started = Instant::now();
        let result = async {
            connection
                .bridge
                .call("reload", json!({}), Duration::from_secs(5))
                .await?;
            connection
                .bridge
                .accept(Instant::now() + Duration::from_secs(60))
                .await?;
            connection
                .bridge
                .call("tab_status", json!({}), Duration::from_secs(5))
                .await
        }
        .await;
        match result {
            Ok(after) => {
                let same = before["exists"] == false || before["tab_id"] == after["tab_id"];
                report.push("service_worker_restart", if same { Status::Pass } else { Status::Fail },
                    json!({"reconnect_ms":duration_ms(started.elapsed()),"own_tab_before":before["tab_id"],"own_tab_after":after["tab_id"]}));
            }
            Err(e) => report.fail("service_worker_restart", &e),
        }
    }
    connection.bridge.disconnect().await;
    Ok(report)
}

// --- smoke ---------------------------------------------------------------

pub struct SmokeOptions {
    pub state_dir: PathBuf,
    pub target: Option<String>,
    pub wait: Duration,
    pub surface: String,
    pub reading: String,
    pub probe_downloads_api: bool,
    pub close_tab: bool,
    /// Оставить собственный скачанный файл в загрузках браузера (по умолчанию удаляется после копии в свидетельства).
    pub keep_download: bool,
    pub limits: Limits,
    /// Сигнал прерывания: операция освобождается до выхода.
    pub interrupt: Option<watch::Receiver<bool>>,
}

/// Структура вкладок и групп вне собственной: без URL и заголовков.
pub(crate) fn structure(snapshot: &Value) -> Value {
    json!({"tabs":snapshot["tabs"],"groups":snapshot["groups"]})
}

/// Освобождает операцию, оставшуюся от прерванного запуска. Расширение
/// затрагивает только связанную собственную загрузку; несвязанную — нет.
pub async fn recover_previous(
    bridge: &mut Bridge,
    ping: &Value,
) -> Option<Result<Value, AssetError>> {
    let op_id = ping["operation"]["op_id"].as_str()?.to_owned();
    tracing::warn!(event = "brave_operation_recovery", stage = "recovery", operation_id = %op_id, "Найдена незавершённая операция прошлого запуска");
    Some(download::release(bridge, &op_id, false).await.map(
        |released| json!({"operation_id":op_id,"previous":ping["operation"],"release":released}),
    ))
}

pub async fn smoke(mut options: SmokeOptions) -> Result<Report, AssetError> {
    let request = Pronunciation::new(&options.surface, &options.reading)?;
    let mut report = Report::default();
    let mut connection =
        match connect(&options.state_dir, options.target.as_deref(), options.wait).await {
            Ok(connection) => connection,
            Err(e) => {
                report.fail("transport", &e);
                return Ok(report);
            }
        };
    match ping_stats(&mut connection.bridge, 5).await {
        Ok(stats) => {
            if let Some(recovered) = recover_previous(&mut connection.bridge, &stats["ping"]).await
            {
                match recovered {
                    Ok(value) => report.push("previous_operation", Status::Pass, value),
                    Err(e) => report.fail("previous_operation", &e),
                }
            }
            report.push("transport", Status::Pass, json!({"target":connection.state.target,"browser":connection.state.browser,"extension_id":connection.peer.extension_id,"version":connection.peer.version,"update":connection.update,"latency":stats}));
        }
        Err(e) => {
            report.fail("transport", &e);
            return Ok(report);
        }
    }
    let before = connection
        .bridge
        .call("snapshot", json!({}), Duration::from_secs(5))
        .await;
    let flow = run_flow(&mut connection, &request, &mut options, &mut report).await;
    if let Err(e) = &flow {
        tracing::warn!(
            event = "brave_smoke_stopped",
            stage = "acceptance",
            code = provider_code(e),
            "Живая проверка остановлена"
        );
    }
    if flow
        .as_ref()
        .err()
        .is_some_and(|e| e.code == ErrorCode::Interrupted)
    {
        connection.bridge.disconnect().await;
        return Ok(report);
    }
    let after = connection
        .bridge
        .call("snapshot", json!({}), Duration::from_secs(5))
        .await;
    match (before, after) {
        (Ok(before), Ok(after)) => {
            let same = structure(&before) == structure(&after);
            let count = |v: &Value, key: &str| v[key].as_array().map_or(0, Vec::len);
            report.push(
                "user_tabs_untouched",
                if same { Status::Pass } else { Status::Fail },
                json!({"identical":same,"tabs_before":count(&before,"tabs"),"tabs_after":count(&after,"tabs"),
                    "groups_before":count(&before,"groups"),"groups_after":count(&after,"groups"),
                    "note":"снимок не содержит URL и заголовков; изменение может быть действием пользователя во время проверки"}),
            );
        }
        (Err(e), _) | (_, Err(e)) => report.fail("user_tabs_untouched", &e),
    }
    match connection
        .bridge
        .call("ping", json!({}), Duration::from_secs(5))
        .await
    {
        Ok(ping) => {
            let clean = ping["listeners"]["determining_filename"] == false
                && ping["listeners"]["web_request"] == false
                && ping["operation"].is_null();
            report.push(
                "no_residual_listeners",
                if clean { Status::Pass } else { Status::Fail },
                json!({"listeners":ping["listeners"],"operation":ping["operation"]}),
            );
        }
        Err(e) => report.fail("no_residual_listeners", &e),
    }
    if options.close_tab {
        match connection
            .bridge
            .call("close", json!({}), Duration::from_secs(10))
            .await
        {
            Ok(value) => report.push("own_tab_closed", Status::Pass, value),
            Err(e) => report.fail("own_tab_closed", &e),
        }
    }
    connection.bridge.disconnect().await;
    Ok(report)
}

async fn run_flow(
    connection: &mut Connection,
    request: &Pronunciation,
    options: &mut SmokeOptions,
    report: &mut Report,
) -> Result<(), AssetError> {
    let bridge = &mut connection.bridge;
    let opened = bridge
        .call("open", json!({}), Duration::from_secs(15))
        .await
        .inspect_err(|e| report.fail("own_tab_group", e))?;
    report.push(
        "own_tab_group",
        if own_tab_grouped(&opened) {
            Status::Pass
        } else {
            Status::Fail
        },
        opened,
    );
    let page = word_url(&request.surface);
    let navigation = bridge
        .call(
            "navigate",
            json!({"url":page,"timeout_ms":30000}),
            Duration::from_secs(40),
        )
        .await
        .inspect_err(|e| report.fail("forvo_page_auth", e))?;
    let (state, observation) = wait_auth(bridge, Duration::from_secs(20))
        .await
        .inspect_err(|e| report.fail("forvo_page_auth", e))?;
    let page_evidence = json!({"auth_state":state,"status":observation.status,"challenge":observation.challenge,"load":navigation["load"],"path":navigation["path"]});
    if state != AuthState::Authenticated {
        // Достоверный сигнал блокировки фиксируется один раз, без повторных попыток.
        report.push("forvo_page_auth", Status::Blocked, page_evidence);
        return Err(auth_error(state));
    }
    if observation.status == 404 {
        report.push("forvo_page_auth", Status::Fail, page_evidence);
        return Err(bridge_error(
            "word_not_found",
            "Forvo вернул 404 для точной страницы слова",
        ));
    }
    report.push("forvo_page_auth", Status::Pass, page_evidence);
    let ranked = candidates(bridge, request)
        .await
        .inspect_err(|e| report.fail("candidates", e))?;
    report.push("candidates", if ranked.is_empty() { Status::Fail } else { Status::Pass }, json!({"section":"ja","count":ranked.len(),
        "ranked":ranked.iter().map(|c| json!({"record_id":c.record_id,"votes":c.votes,"gender":c.gender,"country":c.country,"download_available":c.download_available})).collect::<Vec<_>>()}));
    let Some(selected) = ranked.iter().find(|c| c.download_available).cloned() else {
        let e = bridge_error("no_usable_candidates", "Нет записи с доступной загрузкой");
        report.fail("selection", &e);
        return Err(e);
    };
    report.push("selection", Status::Pass, json!({"record_id":selected.record_id,"ranking_policy":RANKING,"surface":request.surface,"reading":request.reading}));
    let op_id = download::new_operation_id()?;
    let outcome = download_selected(
        bridge,
        &connection.state,
        request,
        &selected,
        &op_id,
        options,
        report,
    )
    .await;
    // Освобождение — при любом исходе, включая прерывание: только собственная связанная загрузка.
    let keep = outcome.is_ok() && options.keep_download;
    match tokio::time::timeout(
        Duration::from_secs(15),
        download::release(bridge, &op_id, keep),
    )
    .await
    {
        Ok(Ok(value)) => report.push(
            "download_cleanup",
            Status::Pass,
            json!({"operation_id":op_id,"keep_download":keep,"release":value}),
        ),
        Ok(Err(e)) => report.fail("download_cleanup", &e),
        Err(_) => report.fail(
            "download_cleanup",
            &bridge_error(
                "extension_timeout",
                "Расширение не освободило операцию вовремя",
            ),
        ),
    }
    outcome
}

/// Свежая проверка строки, одна штатная загрузка, байты файла и копия-свидетельство.
pub async fn download_selected(
    bridge: &mut Bridge,
    state: &AdapterState,
    request: &Pronunciation,
    selected: &Candidate,
    op_id: &str,
    options: &mut SmokeOptions,
    report: &mut Report,
) -> Result<(), AssetError> {
    // Та же узкая операция, что у production-получения; smoke лишь добавляет
    // отчёт и копию-свидетельство.
    let acquired = super::fetch::download_candidate(
        bridge,
        state.platform,
        request,
        selected,
        op_id,
        &options.limits,
        &mut options.interrupt,
    )
    .await
    .and_then(|downloaded| download::probed(op_id, downloaded))
    .map_err(|e| super::fetch::browser_auth_error(e, state))
    .inspect_err(|e| report.fail("site_download", e))?;
    let item = &acquired.item;
    report.push("site_download", Status::Pass, json!({"operation_id":op_id,"download_id":item.id,"file_name":item.file_name,
        "state":item.state,"danger":item.danger,"mime":item.mime,"url":item.url,"final_url":item.final_url,
        "referrer_is_own_page":item.referrer_is_own_page,"reconnects":acquired.reconnects,"elapsed_ms":acquired.elapsed_ms,
        "method":"одна штатная загрузка обычной кнопки выбранной записи; владение — запрос собственной вкладки и единственная совпавшая загрузка"}));
    let evidence_dir = options.state_dir.join("evidence");
    let path = write_evidence(
        &evidence_dir,
        &format!(
            "{}-forvo-{}-{}.mp3",
            unix_ms(),
            selected.record_id,
            &acquired.sha256[..12]
        ),
        &acquired.bytes,
    )?;
    report.push("mp3_bytes", Status::Pass, json!({"operation_id":op_id,"record_id":selected.record_id,"download_id":item.id,
        "byte_length":acquired.bytes.len(),"sha256":acquired.sha256,"signature":mp3_signature(&acquired.bytes),"probe":acquired.probe,
        "evidence_file":path,"source":"файл завершённой собственной загрузки браузера"}));
    if options.probe_downloads_api {
        downloads_api_probe(
            state.platform,
            bridge,
            op_id,
            selected,
            &acquired.sha256,
            report,
        )
        .await;
    } else {
        report.push(
            "downloads_api_file",
            Status::NotTested,
            json!({"reason":"запустите smoke с --probe-downloads-api"}),
        );
    }
    Ok(())
}

async fn downloads_api_probe(
    platform: Platform,
    bridge: &mut Bridge,
    op_id: &str,
    selected: &Candidate,
    sha: &str,
    report: &mut Report,
) {
    let stem = format!("forvo-{}-{}", selected.record_id, &op_id[..12]);
    let result = bridge
        .call(
            "downloads_api",
            json!({"op_id":op_id,"filename_stem":stem,"prompt_ms":8000,"timeout_ms":60000}),
            Duration::from_secs(75),
        )
        .await;
    let value = match result {
        Ok(value) => value,
        Err(e) => return report.fail("downloads_api_file", &e),
    };
    match value["code"].as_str() {
        Some("complete") => {
            let id = value["download_id"].clone();
            let checked = (|| {
                let path = browser_path_to_local(platform, value["filename"].as_str().unwrap_or_default())?;
                let bytes = download::read_regular(&path, MAX_AUDIO_BYTES)?;
                let probe = probe_mp3(&bytes)?;
                Ok::<_, AssetError>((path, bytes.len(), sha256_hex(&bytes), probe))
            })();
            let cleanup = bridge.call("download_cleanup", json!({"download_id":id}), Duration::from_secs(10)).await;
            match checked {
                Ok((path, length, file_sha, probe)) => {
                    let same = file_sha == sha;
                    report.push("downloads_api_file", if same { Status::Pass } else { Status::Fail }, json!({"download_id":id,
                        "file_name":path.file_name().map(|n| n.to_string_lossy().into_owned()),
                        "byte_length":length,"sha256":file_sha,"matches_site_download":same,"probe_codec":probe.codec,"cleanup":cleanup.as_ref().ok(),"file_removed":!path.exists()}));
                }
                Err(e) => report.fail("downloads_api_file", &e),
            }
        }
        Some("save_as_prompt_suspected") => report.push("downloads_api_file", Status::Blocked, json!({"code":"save_as_prompt_suspected","api":"chrome.downloads.download({saveAs:false})",
            "observed":value,"meaning":"настройка «Спрашивать, куда сохранять» открывает выбор пути; загрузка отменена и удалена из истории"})),
        _ => report.push("downloads_api_file", Status::Fail, value),
    }
}

pub async fn close(
    state_dir: &Path,
    target: Option<&str>,
    wait: Duration,
) -> Result<Value, AssetError> {
    let mut connection = connect(state_dir, target, wait).await?;
    let value = connection
        .bridge
        .call("close", json!({}), Duration::from_secs(10))
        .await?;
    connection.bridge.disconnect().await;
    Ok(value)
}

pub fn report_error(report: Report) -> Result<Value, AssetError> {
    let value = json!(report);
    if report.passed() {
        Ok(value)
    } else {
        if report.steps.iter().any(|s| s.status == Status::Interrupted) {
            return Err(AssetError::with_details(
                ErrorCode::Interrupted,
                "Проверка адаптера браузера прервана; собственная операция освобождена",
                json!({"provider":"forvo_brave","code":"acceptance_interrupted","report":value}),
            ));
        }
        let blocked = report.steps.iter().any(|s| s.status == Status::Blocked);
        Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            if blocked {
                "Проверка адаптера браузера заблокирована внешним условием"
            } else {
                "Проверка адаптера браузера не пройдена"
            },
            json!({"provider":"forvo_brave","code":if blocked {"acceptance_blocked"} else {"acceptance_failed"},"report":value}),
        ))
    }
}
