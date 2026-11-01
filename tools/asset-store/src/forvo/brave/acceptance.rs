//! Живые проверки адаптера Brave: `doctor` (транспорт и вкладка) и `smoke`
//! (одна японская запись до проверенных байтов MP3). Хранилище ресурсов не меняется.
use super::download::{self, Limits};
use super::{
    AdapterState, Bridge, GROUP_TITLE, Peer, Platform, bridge_error, browser_path_to_local,
    load_state, save_state,
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
        tracing::info!(event = "brave_acceptance_step", stage = "acceptance", step = name, status = ?status, "Шаг проверки адаптера Brave");
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
                | "download_conflict"
        ) {
            Status::Blocked
        } else {
            Status::Fail
        };
        let mut evidence = json!({"code":code,"message":error.message});
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

/// Bridge, подключённое расширение и закрепление его ID после первого сопряжения.
pub struct Connection {
    pub bridge: Bridge,
    pub state: AdapterState,
    pub peer: Peer,
}

pub async fn connect(state_dir: &Path, wait: Duration) -> Result<Connection, AssetError> {
    let mut state = load_state(state_dir)?;
    let mut bridge = Bridge::bind(
        state.port,
        state.secret_bytes()?,
        state.extension_id.clone(),
    )
    .await?;
    let peer = bridge
        .accept(Instant::now() + wait)
        .await
        .map_err(|mut e| {
            e.details["user_action"] = json!(format!(
                "brave://extensions → Режим разработчика → Загрузить распакованное → {}",
                state.load_unpacked_path
            ));
            e
        })?;
    if state.extension_id.is_none() {
        // Единственный акт доверия — установка расширения с секретом; ID закрепляется.
        state.extension_id = Some(peer.extension_id.clone());
        save_state(state_dir, &state)?;
    }
    Ok(Connection {
        bridge,
        state,
        peer,
    })
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

pub async fn doctor(options: &DoctorOptions) -> Result<Report, AssetError> {
    let mut report = Report::default();
    let state = load_state(&options.state_dir)?;
    report.push("configuration", Status::Pass, json!({"platform":state.platform,"brave":super::discover_brave(state.platform),
        "load_unpacked_path":state.load_unpacked_path,"pinned_extension_id":state.extension_id,"port":state.port}));
    report.push("proxy_environment", Status::Pass, json!({"proxy_variables":proxy_names(),
        "note":"bridge только принимает loopback-подключения и сам не делает исходящих HTTP-запросов; Chromium не проксирует 127.0.0.1"}));
    let mut connection = match connect(&options.state_dir, options.wait).await {
        Ok(connection) => connection,
        Err(e) => {
            report.fail("extension_connected", &e);
            return Ok(report);
        }
    };
    let (status, evidence) = exposure_check(connection.bridge.port());
    report.push("loopback_only", status, evidence);
    report.push(
        "extension_connected",
        Status::Pass,
        json!({"extension_id":connection.peer.extension_id,"version":connection.peer.version}),
    );
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

fn structure(snapshot: &Value) -> Value {
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
    let mut connection = match connect(&options.state_dir, options.wait).await {
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
            report.push("transport", Status::Pass, json!({"extension_id":connection.peer.extension_id,"version":connection.peer.version,"latency":stats}));
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
    let platform = connection.state.platform;
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
        bridge, platform, request, &selected, &op_id, options, report,
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
    platform: Platform,
    request: &Pronunciation,
    selected: &Candidate,
    op_id: &str,
    options: &mut SmokeOptions,
    report: &mut Report,
) -> Result<(), AssetError> {
    let before = bridge
        .call(
            "dom",
            json!({"surface":request.surface,"action":"inspect"}),
            Duration::from_secs(15),
        )
        .await
        .inspect_err(|e| report.fail("site_download", e))?;
    let before: DomResult = serde_json::from_value(before)
        .map_err(|_| bridge_error("dom_changed", "Неизвестная структура кандидатов Forvo"))?;
    if before.code != "ok" || !before.candidates.contains(selected) {
        let e = bridge_error(
            "candidate_changed",
            "Строка кандидата изменилась; повторите выбор",
        );
        report.fail("site_download", &e);
        return Err(e);
    }
    let acquired = download::acquire(
        bridge,
        platform,
        request,
        selected,
        op_id,
        &options.limits,
        &mut options.interrupt,
    )
    .await
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
        downloads_api_probe(platform, bridge, op_id, selected, &acquired.sha256, report).await;
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

pub async fn close(state_dir: &Path, wait: Duration) -> Result<Value, AssetError> {
    let mut connection = connect(state_dir, wait).await?;
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
                "Проверка адаптера Brave прервана; собственная операция освобождена",
                json!({"provider":"forvo_brave","code":"acceptance_interrupted","report":value}),
            ));
        }
        let blocked = report.steps.iter().any(|s| s.status == Status::Blocked);
        Err(AssetError::with_details(
            ErrorCode::InvalidTransition,
            if blocked {
                "Проверка адаптера Brave заблокирована внешним условием"
            } else {
                "Проверка адаптера Brave не пройдена"
            },
            json!({"provider":"forvo_brave","code":if blocked {"acceptance_blocked"} else {"acceptance_failed"},"report":value}),
        ))
    }
}
