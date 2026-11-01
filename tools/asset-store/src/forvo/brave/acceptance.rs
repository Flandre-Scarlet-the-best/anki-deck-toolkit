//! Живые проверки адаптера Brave: `doctor` (транспорт и вкладка) и `smoke`
//! (одна японская запись до проверенных байтов MP3). Хранилище ресурсов не меняется.
use super::{
    AdapterState, Bridge, GROUP_TITLE, Peer, Platform, bridge_error, browser_path_to_local,
    load_state, save_state,
};
use crate::forvo::candidates::{DomResult, validated};
use crate::forvo::download::matches_download;
use crate::forvo::{
    AuthObservation, AuthState, Candidate, RANKING, auth_error, provider_code, word_url,
};
use crate::pronunciation::{MAX_AUDIO_BYTES, Pronunciation, probe_mp3};
use crate::{AssetError, ErrorCode, hashing::sha256_hex};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Pass,
    Fail,
    Blocked,
    NotTested,
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
        let status = if matches!(
            code.as_str(),
            "challenge_required"
                | "login_required"
                | "session_expired"
                | "access_denied"
                | "rate_limited"
                | "extension_not_connected"
        ) {
            Status::Blocked
        } else {
            Status::Fail
        };
        self.push(name, status, json!({"code":code,"message":error.message}));
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

#[derive(Debug, Clone, Serialize)]
pub struct Observed {
    #[serde(skip)]
    pub token: String,
    pub download_id: i64,
    pub url_path: String,
    pub final_url: Value,
    pub mime: Value,
    pub cancelled_before_prompt: bool,
    pub new_tabs_from_own_page: u64,
}

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

/// Rust повторно сверяет строку и контракт адреса: URL выводится не из
/// предположения, а из загрузки, которую начала обычная кнопка сайта.
pub async fn observe_download(
    bridge: &mut Bridge,
    request: &Pronunciation,
    candidate: &Candidate,
) -> Result<Observed, AssetError> {
    let before = bridge
        .call(
            "dom",
            json!({"surface":request.surface,"action":"inspect"}),
            Duration::from_secs(15),
        )
        .await?;
    let before: DomResult = serde_json::from_value(before)
        .map_err(|_| bridge_error("dom_changed", "Неизвестная структура кандидатов Forvo"))?;
    if before.code != "ok" || !before.candidates.contains(candidate) {
        return Err(bridge_error(
            "candidate_changed",
            "Строка кандидата изменилась; повторите выбор",
        ));
    }
    let value = bridge
        .call(
            "download",
            json!({"surface":request.surface,"record_id":candidate.record_id,"timeout_ms":60000}),
            Duration::from_secs(75),
        )
        .await?;
    let code = value["code"].as_str().unwrap_or("protocol_error");
    if code != "observed" {
        if code == "download_not_started" {
            // Блокировка доступа не должна выглядеть как отсутствие загрузки.
            let (state, _) = wait_auth(bridge, Duration::from_secs(2)).await?;
            if state != AuthState::Authenticated {
                return Err(auth_error(state));
            }
        }
        return Err(bridge_error(
            code,
            "Обычная кнопка выбранной записи не начала загрузку",
        ));
    }
    let url = observed_url(&value["url"]).ok_or_else(|| {
        bridge_error(
            "download_mismatch",
            "Адрес загрузки содержит query/fragment/учётные данные",
        )
    })?;
    if !matches_download(&url, &request.surface, &candidate.record_id) {
        return Err(bridge_error(
            "download_mismatch",
            "Загрузка не связана однозначно с выбранной записью",
        ));
    }
    // Политика referrer сайта может сократить его до origin; другой страницы в окне операции нет.
    if value["referrer_is_own_page"] != true && value["referrer_origin_is_forvo"] != true {
        return Err(bridge_error(
            "download_mismatch",
            "Загрузка начата не собственной страницей",
        ));
    }
    Ok(Observed {
        token: value["token"]
            .as_str()
            .filter(|t| t.len() == 64)
            .ok_or_else(|| bridge_error("protocol_error", "Нет токена наблюдения"))?
            .to_owned(),
        download_id: value["download_id"].as_i64().unwrap_or(-1),
        url_path: url::Url::parse(&url)
            .map(|u| u.path().to_owned())
            .unwrap_or_default(),
        final_url: value["final_url"].clone(),
        mime: value["mime"].clone(),
        cancelled_before_prompt: value["cancelled_before_prompt"] == true,
        new_tabs_from_own_page: value["new_tabs_from_own_page"].as_u64().unwrap_or(0),
    })
}

pub struct Fetched {
    pub bytes: Vec<u8>,
    pub evidence: Value,
}

pub async fn fetch_observed(
    bridge: &mut Bridge,
    observed: &Observed,
) -> Result<Fetched, AssetError> {
    let value = bridge
        .call(
            "fetch_observed",
            json!({"token":observed.token}),
            Duration::from_secs(90),
        )
        .await?;
    if value["code"] != "ok" {
        return Err(bridge_error(
            value["code"].as_str().unwrap_or("fetch_failed"),
            "Не удалось прочитать наблюдённую загрузку в собственной вкладке",
        ));
    }
    if value["status"] != 200 {
        return Err(bridge_error("download_invalid", "Ответ загрузки не 200"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value["base64"].as_str().unwrap_or_default())
        .map_err(|_| bridge_error("protocol_error", "Неверный base64 байтов MP3"))?;
    if bytes.is_empty()
        || bytes.len() as u64 > MAX_AUDIO_BYTES
        || value["length"].as_u64() != Some(bytes.len() as u64)
    {
        return Err(bridge_error(
            "download_incomplete",
            "Размер байтов MP3 противоречив",
        ));
    }
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<') {
        return Err(bridge_error("download_invalid", "Вместо MP3 получен HTML"));
    }
    Ok(Fetched {
        evidence: json!({"status":value["status"],"redirected":value["redirected"],"final_origin":value["final_origin"],"final_path":value["final_path"],
            "final_has_query":value["final_has_query"],"content_type":value["content_type"]}),
        bytes,
    })
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

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, AssetError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| AssetError::io("файл загрузки браузера", e))?;
    if !file
        .metadata()
        .map_err(|e| AssetError::io("файл загрузки браузера", e))?
        .is_file()
    {
        return Err(bridge_error(
            "download_invalid",
            "Загрузка не является обычным файлом",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| AssetError::io("файл загрузки браузера", e))?;
    if bytes.len() as u64 > limit {
        return Err(bridge_error(
            "download_invalid",
            "Файл загрузки слишком велик",
        ));
    }
    Ok(bytes)
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

/// Проверяет, что порт bridge недоступен по нелокальным адресам этого хоста.
fn exposure_check(port: u16) -> (Status, Value) {
    let Ok(output) = std::process::Command::new("ip")
        .args(["-j", "-4", "addr", "show"])
        .output()
    else {
        return (Status::NotTested, json!({"reason":"ip_unavailable"}));
    };
    let Ok(interfaces) = serde_json::from_slice::<Vec<Value>>(&output.stdout) else {
        return (Status::NotTested, json!({"reason":"ip_output_unknown"}));
    };
    let addresses = interfaces
        .iter()
        .flat_map(|i| i["addr_info"].as_array().cloned().unwrap_or_default())
        .filter_map(|a| {
            a["local"]
                .as_str()
                .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
        })
        .filter(|a| !a.is_loopback())
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
            json!({"listen":"127.0.0.1","non_loopback_addresses_checked":addresses.len(),"reachable":exposed}),
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
    match ping_stats(&mut connection.bridge, 5).await {
        Ok(stats) => report.push("command_latency", Status::Pass, stats),
        Err(e) => {
            report.fail("command_latency", &e);
            return Ok(report);
        }
    }
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
                    if opened["group_title"] == GROUP_TITLE && opened["active"] == false {
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
}

fn structure(snapshot: &Value) -> Value {
    json!({"tabs":snapshot["tabs"],"groups":snapshot["groups"]})
}

pub async fn smoke(options: &SmokeOptions) -> Result<Report, AssetError> {
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
        Ok(stats) => report.push("transport", Status::Pass, json!({"extension_id":connection.peer.extension_id,"version":connection.peer.version,"latency":stats})),
        Err(e) => {
            report.fail("transport", &e);
            return Ok(report);
        }
    }
    let before = connection
        .bridge
        .call("snapshot", json!({}), Duration::from_secs(5))
        .await;
    let flow = run_flow(&mut connection, &request, options, &mut report).await;
    if let Err(e) = flow {
        tracing::warn!(
            event = "brave_smoke_stopped",
            stage = "acceptance",
            code = provider_code(&e),
            "Живая проверка остановлена"
        );
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
    options: &SmokeOptions,
    report: &mut Report,
) -> Result<(), AssetError> {
    let bridge = &mut connection.bridge;
    let opened = bridge
        .call("open", json!({}), Duration::from_secs(15))
        .await
        .inspect_err(|e| report.fail("own_tab_group", e))?;
    // Новую вкладку адаптер создаёт неактивной; повторно используемую мог открыть сам пользователь.
    let grouped = opened["group_title"] == GROUP_TITLE
        && (opened["created"] == false || opened["active"] == false);
    report.push(
        "own_tab_group",
        if grouped { Status::Pass } else { Status::Fail },
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
    let observed = observe_download(bridge, request, &selected)
        .await
        .inspect_err(|e| report.fail("site_download_observed", e))?;
    report.push("site_download_observed", Status::Pass, json!(observed));
    let fetched = fetch_observed(bridge, &observed)
        .await
        .inspect_err(|e| report.fail("mp3_bytes", e))?;
    let sha = sha256_hex(&fetched.bytes);
    let probe = probe_mp3(&fetched.bytes).inspect_err(|e| report.fail("mp3_bytes", e))?;
    let evidence_dir = options.state_dir.join("evidence");
    let path = write_evidence(
        &evidence_dir,
        &format!(
            "{}-forvo-{}-{}.mp3",
            unix_ms(),
            selected.record_id,
            &sha[..12]
        ),
        &fetched.bytes,
    )?;
    report.push("mp3_bytes", Status::Pass, json!({"record_id":selected.record_id,"download_id":observed.download_id,"byte_length":fetched.bytes.len(),"sha256":sha,
        "signature":mp3_signature(&fetched.bytes),"probe":probe,"response":fetched.evidence,"evidence_file":path,
        "method":"fetch наблюдённого адреса штатной загрузки в собственной вкладке forvo.com"}));
    if options.probe_downloads_api {
        downloads_api_probe(
            connection.state.platform,
            bridge,
            &observed,
            &selected,
            &sha,
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
    observed: &Observed,
    selected: &Candidate,
    sha: &str,
    report: &mut Report,
) {
    let stem = format!(
        "forvo-{}-{}",
        selected.record_id,
        &super::encode_lower_hex(super::random_bytes().unwrap_or([0; 32]))[..12]
    );
    let result = bridge
        .call("downloads_api", json!({"token":observed.token,"filename_stem":stem,"prompt_ms":8000,"timeout_ms":60000}), Duration::from_secs(75))
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
                let bytes = read_regular(&path, MAX_AUDIO_BYTES)?;
                let probe = probe_mp3(&bytes)?;
                Ok::<_, AssetError>((path, bytes.len(), sha256_hex(&bytes), probe))
            })();
            let cleanup = bridge.call("download_cleanup", json!({"download_id":id}), Duration::from_secs(10)).await;
            match checked {
                Ok((path, length, file_sha, probe)) => {
                    let same = file_sha == sha;
                    report.push("downloads_api_file", if same { Status::Pass } else { Status::Fail }, json!({"download_id":id,"browser_path":value["filename"],"local_path":path,
                        "byte_length":length,"sha256":file_sha,"matches_fetched":same,"probe_codec":probe.codec,"cleanup":cleanup.as_ref().ok(),"file_removed":!path.exists()}));
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
