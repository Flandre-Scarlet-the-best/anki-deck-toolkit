use super::*;
use crate::temp_workspace::TempWorkspace;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use serde_json::{Value, json};

const ID: &str = "abcdefghijklmnopabcdefghijklmnop";

fn hex(text: &str) -> Vec<u8> {
    decode_hex(text).expect("hex")
}

#[test]
fn hmac_matches_rfc4231() {
    assert_eq!(
        encode_lower_hex(hmac_sha256(&[0x0b; 20], b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        encode_lower_hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // Ключ длиннее блока хэшируется (тест 6).
    assert_eq!(
        encode_lower_hex(hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn extension_origin_is_strict() {
    assert_eq!(
        origin_extension_id(&format!("chrome-extension://{ID}")),
        Some(ID)
    );
    for bad in [
        "https://forvo.com".to_owned(),
        format!("chrome-extension://{}", ID.to_uppercase()),
        format!("chrome-extension://{ID}/"),
        "chrome-extension://short".to_owned(),
        format!("moz-extension://{ID}"),
    ] {
        assert_eq!(origin_extension_id(&bad), None, "{bad}");
    }
    assert!(decode_hex("ABCD").is_none());
    assert!(decode_hex("abc").is_none());
}

#[test]
fn page_module_reuses_single_dom_contract() {
    let module = page_module();
    assert!(module.contains("export const forvoDom = "));
    assert!(module.contains("export const forvoAuth = () => "));
    assert!(module.contains(include_str!("../dom.js").trim_end()));
    assert!(module.contains(include_str!("../auth.js").trim_end()));
    assert!(include_str!("../dom.js").contains("action === 'activate'"));
}

#[test]
fn extension_manifest_is_minimal() {
    let manifest: Value = serde_json::from_str(MANIFEST).expect("manifest");
    assert_eq!(manifest["manifest_version"], 3);
    assert_eq!(
        manifest["host_permissions"],
        json!(["https://forvo.com/*", "http://127.0.0.1/*"])
    );
    let permissions = manifest["permissions"].as_array().expect("permissions");
    // Нет tabs/debugger/cookies и блокирующих сетевых API: webRequest только
    // наблюдает tabId запроса выбранной записи и ничего не изменяет.
    for forbidden in [
        "tabs",
        "debugger",
        "cookies",
        "webRequestBlocking",
        "declarativeNetRequest",
        "declarativeNetRequestWithHostAccess",
        "<all_urls>",
        "nativeMessaging",
    ] {
        assert!(!permissions.iter().any(|p| p == forbidden), "{forbidden}");
    }
    assert!(permissions.iter().any(|p| p == "webRequest"));
    // Глобального перехватчика имён загрузок и отмены чужих загрузок нет.
    assert!(!BACKGROUND.contains("onDeterminingFilename.addListener"));
    assert!(!BACKGROUND.contains("'blocking'"));
    assert!(!BACKGROUND.contains("eval("));
    assert!(!BACKGROUND.contains("new Function"));
    assert!(BACKGROUND.contains(GROUP_TITLE));
    assert!(BACKGROUND.contains(WS_PATH));
}

#[test]
fn setup_writes_private_pairing_and_keeps_secret() {
    let temp = TempWorkspace::create("brave-setup").expect("temp");
    let options = SetupOptions {
        state_dir: temp.path().join("state"),
        install_dir: Some(temp.path().join("extension")),
        port: Some(48000),
        rotate_secret: false,
        platform: None,
    };
    let first = setup(&options).expect("setup");
    assert_eq!(first["pairing"], "created");
    let state = load_state(&options.state_dir).expect("state");
    assert_eq!(
        std::fs::metadata(options.state_dir.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let pairing: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("extension/pairing.json")).unwrap())
            .unwrap();
    assert_eq!(pairing["port"], 48000);
    assert_eq!(pairing["secret"], state.secret);
    for name in ["manifest.json", "background.js", "forvo_page.js"] {
        assert!(temp.path().join("extension").join(name).is_file());
    }
    // Секрет не попадает в машинный вывод setup.
    assert!(!first.to_string().contains(&state.secret));
    let again = setup(&options).expect("setup");
    assert_eq!(again["pairing"], "kept");
    assert_eq!(load_state(&options.state_dir).unwrap().secret, state.secret);
    let rotated = setup(&SetupOptions {
        rotate_secret: true,
        ..options
    })
    .expect("setup");
    assert_eq!(rotated["pairing"], "created");
    assert_ne!(
        load_state(&temp.path().join("state")).unwrap().secret,
        state.secret
    );
}

#[test]
fn state_with_open_permissions_is_rejected() {
    let temp = TempWorkspace::create("brave-state").expect("temp");
    let state = AdapterState {
        schema_version: 1,
        port: 48001,
        secret: "00".repeat(32),
        install_dir: temp.path().join("x"),
        load_unpacked_path: "x".into(),
        platform: Platform::Linux,
        extension_id: None,
    };
    save_state(temp.path(), &state).expect("save");
    assert_eq!(load_state(temp.path()).expect("load"), state);
    std::fs::set_permissions(
        temp.path().join("state.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        crate::forvo::provider_code(&load_state(temp.path()).unwrap_err()),
        "state_permissions"
    );
}

#[test]
fn browser_paths_map_per_platform() {
    assert_eq!(
        browser_path_to_local(Platform::Linux, "/home/u/Downloads/a.mp3").unwrap(),
        PathBuf::from("/home/u/Downloads/a.mp3")
    );
    assert!(browser_path_to_local(Platform::Linux, "C:\\a.mp3").is_err());
    assert!(browser_path_to_local(Platform::Wsl, "relative.mp3").is_err());
    assert!(browser_path_to_local(Platform::Wsl, "C:relative.mp3").is_err());
    // Linux-браузер внутри WSL сообщает обычный абсолютный путь.
    assert_eq!(
        browser_path_to_local(Platform::Wsl, "/tmp/a.mp3").unwrap(),
        PathBuf::from("/tmp/a.mp3")
    );
}

// --- Синтетическое расширение -------------------------------------------

type Client = async_tungstenite::WebSocketStream<TokioAdapter<TcpStream>>;

async fn client(port: u16, origin: &str) -> Result<Client, async_tungstenite::tungstenite::Error> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut request = format!("ws://127.0.0.1:{port}{WS_PATH}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", origin.parse().unwrap());
    async_tungstenite::tokio::client_async(request, stream)
        .await
        .map(|(s, _)| s)
}

async fn next_json(socket: &mut Client) -> Value {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return json!({"type":"closed"}),
            Some(Ok(_)) => continue,
        }
    }
}

async fn send(socket: &mut Client, value: Value) {
    socket
        .send(Message::text(value.to_string()))
        .await
        .expect("send");
}

/// Повторяет протокол background.js: hello → auth → auth_ok (проверка) → ready.
async fn paired_client(port: u16, secret: &[u8], id: &str) -> Result<Client, &'static str> {
    let mut socket = client(port, &format!("chrome-extension://{id}"))
        .await
        .map_err(|_| "upgrade")?;
    let hello = next_json(&mut socket).await;
    let bridge_nonce = hello["nonce"].as_str().ok_or("hello")?.to_owned();
    let own = "11".repeat(32);
    let mac = encode_lower_hex(hmac_sha256(
        secret,
        mac_text("extension", &bridge_nonce, &own, id).as_bytes(),
    ));
    send(
        &mut socket,
        json!({"type":"auth","v":1,"nonce":own,"extension_id":id,"version":"0.1.0","mac":mac}),
    )
    .await;
    let ok = next_json(&mut socket).await;
    if ok["type"] != "auth_ok" {
        return Err("rejected");
    }
    let expected = hmac_sha256(
        secret,
        mac_text("bridge", &own, &bridge_nonce, id).as_bytes(),
    );
    if hex(ok["mac"].as_str().unwrap()) != expected {
        return Err("bridge_not_paired");
    }
    send(&mut socket, json!({"type":"ready","version":"0.1.0"})).await;
    Ok(socket)
}

fn secret() -> Vec<u8> {
    vec![7; 32]
}

#[tokio::test]
async fn paired_extension_executes_typed_commands() {
    let mut bridge = Bridge::bind(0, secret(), None).await.expect("bind");
    let port = bridge.port();
    let extension = tokio::spawn(async move {
        let mut socket = paired_client(port, &secret(), ID).await.expect("pair");
        let first = next_json(&mut socket).await;
        assert_eq!(first["op"], "ping");
        // Опоздавший ответ и keepalive не подменяют ответ текущей команды.
        send(&mut socket, json!({"type":"res","id":first["id"].as_u64().unwrap() + 100,"ok":true,"result":"stale"})).await;
        send(&mut socket, json!({"type":"keepalive"})).await;
        send(
            &mut socket,
            json!({"type":"res","id":first["id"],"ok":true,"result":{"pong":true}}),
        )
        .await;
        let second = next_json(&mut socket).await;
        send(
            &mut socket,
            json!({"type":"res","id":second["id"],"ok":false,"code":"own_tab_missing"}),
        )
        .await;
        let third = next_json(&mut socket).await;
        send(
            &mut socket,
            json!({"type":"res","id":third["id"],"ok":false,"code":"Bad Code"}),
        )
        .await;
        // Четвёртая команда остаётся без ответа: bridge должен завершить её по таймауту.
        let _ = next_json(&mut socket).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let peer = bridge
        .accept(Instant::now() + Duration::from_secs(5))
        .await
        .expect("accept");
    assert_eq!(
        peer,
        Peer {
            extension_id: ID.into(),
            version: "0.1.0".into()
        }
    );
    assert_eq!(
        bridge
            .call("ping", json!({}), Duration::from_secs(5))
            .await
            .unwrap(),
        json!({"pong":true})
    );
    let error = bridge
        .call("auth", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(crate::forvo::provider_code(&error), "own_tab_missing");
    let error = bridge
        .call("auth", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(crate::forvo::provider_code(&error), "protocol_error");
    let error = bridge
        .call("ping", json!({}), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert_eq!(crate::forvo::provider_code(&error), "extension_timeout");
    extension.abort();
}

#[tokio::test]
async fn wrong_secret_is_never_accepted() {
    let mut bridge = Bridge::bind(0, secret(), None).await.expect("bind");
    let port = bridge.port();
    let extension = tokio::spawn(async move { paired_client(port, &[9; 32], ID).await.err() });
    let error = bridge
        .accept(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(
        crate::forvo::provider_code(&error),
        "extension_not_connected"
    );
    assert!(extension.await.unwrap().is_some());
    assert!(!bridge.connected());
}

#[tokio::test]
async fn foreign_origin_and_unpinned_extension_are_refused_before_upgrade() {
    let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let mut bridge = Bridge::bind(0, secret(), Some(ID.into()))
        .await
        .expect("bind");
    let port = bridge.port();
    let attempts = tokio::spawn(async move {
        let web = client(port, "https://forvo.com").await.is_err();
        let unpinned = client(port, &format!("chrome-extension://{other}"))
            .await
            .is_err();
        (web, unpinned)
    });
    assert!(
        bridge
            .accept(Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    assert_eq!(attempts.await.unwrap(), (true, true));
}

#[tokio::test]
async fn bridge_listens_only_on_loopback() {
    let bridge = Bridge::bind(0, secret(), None).await.expect("bind");
    let address = bridge.listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    assert_eq!(address.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
}

#[test]
fn observed_url_requires_clean_forvo_download_contract() {
    use super::acceptance::observed_url;
    let clean = json!({"origin":"https://forvo.com","path":"/download/mp3/%E5%85%83%E6%B0%97/ja/3661006","has_query":false,"has_fragment":false,"has_credentials":false});
    let url = observed_url(&clean).expect("url");
    assert!(crate::forvo::download::matches_download(
        &url, "元気", "3661006"
    ));
    assert!(!crate::forvo::download::matches_download(
        &url, "元気", "5036880"
    ));
    let mut query = clean.clone();
    query["has_query"] = json!(true);
    assert!(observed_url(&query).is_none());
}
