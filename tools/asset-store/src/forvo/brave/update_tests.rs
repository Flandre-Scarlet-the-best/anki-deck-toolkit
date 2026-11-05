//! Обновление против локального HTTP-канала и программного расширения,
//! которое, как Chromium, после `reload` перечитывает manifest из каталога.
use super::*;
use crate::forvo::brave::release::{ReleaseMetadata, TrustedKey};
use crate::forvo::provider_code;
use crate::hashing::encode_lower_hex;
use crate::temp_workspace::TempWorkspace;
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ID: &str = "abcdefghijklmnopabcdefghijklmnop";
const OTHER_ID: &str = "ponmlkjihgfedcbaponmlkjihgfedcba";
const SECRET: [u8; 32] = [7; 32];
const COMMIT: &str = "89abcdef0123456789abcdef0123456789abcdef";

// --- Локальный HTTP ---------------------------------------------------------

type Routes = Arc<BTreeMap<String, (u16, Vec<u8>)>>;

async fn serve(routes: BTreeMap<String, (u16, Vec<u8>)>, stall: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let routes: Routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                if stall {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, body) = routes.get(&path).cloned().unwrap_or((404, b"{}".to_vec()));
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    format!("http://{address}")
}

fn channel(base: &str) -> Channel {
    Channel {
        api_base: base.into(),
        download_base: base.into(),
        timeout: Duration::from_secs(5),
    }
}

fn listing_path(page: u32) -> String {
    format!(
        "/repos/{}/releases?per_page=100&page={page}",
        release::REPOSITORY
    )
}

fn asset_path(tag: &str, name: &str) -> String {
    format!("/{}/releases/download/{tag}/{name}", release::REPOSITORY)
}

#[tokio::test]
async fn latest_version_ignores_other_components_drafts_and_prereleases() {
    let listing = json!([
        {"tag_name":"v9.0.0","draft":false,"prerelease":false},
        {"tag_name":"forvo-extension-v1.2","draft":false,"prerelease":true},
        {"tag_name":"forvo-extension-v1.1","draft":true,"prerelease":false},
        {"tag_name":"forvo-extension-v1.0","draft":"false","prerelease":false},
        {"tag_name":"forvo-extension-v0.99","prerelease":false},
        {"tag_name":"forvo-extension-v01.5","draft":false,"prerelease":false},
        {"tag_name":"forvo-extension-v0.10","draft":false,"prerelease":false},
        {"tag_name":"forvo-extension-v0.9","draft":false,"prerelease":false},
    ]);
    let base = serve(
        BTreeMap::from([(
            listing_path(1),
            (200, serde_json::to_vec(&listing).unwrap()),
        )]),
        false,
    )
    .await;
    let (version, tag) = latest_published(&channel(&base)).await.unwrap().unwrap();
    assert_eq!(version.to_string(), "0.10");
    assert_eq!(tag, "forvo-extension-v0.10");
    let empty = serve(
        BTreeMap::from([(listing_path(1), (200, b"[]".to_vec()))]),
        false,
    )
    .await;
    assert!(latest_published(&channel(&empty)).await.unwrap().is_none());
}

#[tokio::test]
async fn network_failures_are_typed_and_bounded() {
    let stalled = serve(BTreeMap::new(), true).await;
    let mut slow = channel(&stalled);
    slow.timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    let error = latest_published(&slow).await.unwrap_err();
    assert_eq!(provider_code(&error), "update_source_unavailable");
    assert!(started.elapsed() < Duration::from_secs(10));
    let huge = serve(
        BTreeMap::from([(listing_path(1), (200, vec![b' '; MAX_LISTING_BYTES + 1]))]),
        false,
    )
    .await;
    assert_eq!(
        latest_published(&channel(&huge)).await.unwrap_err().details["reason"],
        "too_large"
    );
    let failing = serve(
        BTreeMap::from([(listing_path(1), (500, b"{}".to_vec()))]),
        false,
    )
    .await;
    assert_eq!(
        latest_published(&channel(&failing))
            .await
            .unwrap_err()
            .details["status"],
        500
    );
}

// --- Выпуск --------------------------------------------------------------------

struct Release {
    dir: PathBuf,
    trust: TrustRoot,
    metadata: Vec<u8>,
    signature: Vec<u8>,
    zip: Vec<u8>,
    version: String,
}

fn files_with_version(version: &str) -> Vec<PackageFile> {
    let mut files = package::bundled_files();
    let manifest = files
        .iter_mut()
        .find(|f| f.path == "manifest.json")
        .unwrap();
    let mut value: Value = serde_json::from_slice(&manifest.bytes).unwrap();
    value["version"] = json!(version);
    manifest.bytes = serde_json::to_vec_pretty(&value).unwrap();
    files
}

fn make_release(root: &Path, version: &str, pkcs8: &[u8]) -> Release {
    let files = files_with_version(version);
    let zip = package::build_zip(&files).unwrap();
    let metadata = ReleaseMetadata::new(&files, &zip, release::REPOSITORY, COMMIT)
        .unwrap()
        .to_bytes();
    let signature = release::sign(&metadata, pkcs8).unwrap().to_bytes();
    let dir = root.join(format!("release-{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(release::METADATA_ASSET), &metadata).unwrap();
    std::fs::write(dir.join(release::SIGNATURE_ASSET), &signature).unwrap();
    std::fs::write(dir.join(release::archive_name(version)), &zip).unwrap();
    Release {
        dir,
        trust: TrustRoot::Explicit(
            TrustedKey::from_public_key_hex(&release::public_key_of(pkcs8).unwrap(), None).unwrap(),
        ),
        metadata,
        signature,
        zip,
        version: version.into(),
    }
}

async fn serve_changing_releases(first: &Release, second: &Release) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let listing_requests = Arc::new(AtomicUsize::new(0));
    let counter = listing_requests.clone();
    let first_tag = release::tag(&first.version);
    let second_tag = release::tag(&second.version);
    let mut routes = BTreeMap::new();
    for (tag, release) in [(&first_tag, first), (&second_tag, second)] {
        routes.insert(
            asset_path(tag, release::METADATA_ASSET),
            (200, release.metadata.clone()),
        );
        routes.insert(
            asset_path(tag, release::SIGNATURE_ASSET),
            (200, release.signature.clone()),
        );
    }
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            let counter = counter.clone();
            let first_tag = first_tag.clone();
            let second_tag = second_tag.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, body) = if path == listing_path(1) {
                    let request_index = counter.fetch_add(1, Ordering::SeqCst);
                    if request_index == 0 {
                        // Без сериализации старый ответ придёт позже и может
                        // перезаписать уже сохранённый результат следующего check.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    let tag = if request_index == 0 {
                        first_tag
                    } else {
                        second_tag
                    };
                    (
                        200,
                        serde_json::to_vec(
                            &json!([{"tag_name":tag,"draft":false,"prerelease":false}]),
                        )
                        .unwrap(),
                    )
                } else {
                    routes.get(&path).cloned().unwrap_or((404, b"{}".to_vec()))
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    (format!("http://{address}"), listing_requests)
}

async fn serve_counted_stall() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let _ = stream.read(&mut buffer).await;
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    (format!("http://{address}"), requests)
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_checks_cannot_overwrite_a_newer_result_with_an_older_response() {
    let workspace = TempWorkspace::create("forvo-update-check-race").unwrap();
    let pkcs8 = release::generate_key().unwrap();
    let first = make_release(workspace.path(), "99.6", &pkcs8);
    let second = make_release(workspace.path(), "99.7", &pkcs8);
    let (base, listings) = serve_changing_releases(&first, &second).await;
    let state_dir = workspace.path().join("state");
    let channel = channel(&base);

    let (first_result, second_result) = tokio::join!(
        check(&state_dir, None, &Source::GitHub, &first.trust, &channel),
        check(&state_dir, None, &Source::GitHub, &first.trust, &channel)
    );
    assert_eq!(first_result.unwrap()["status"], "verified");
    assert_eq!(second_result.unwrap()["status"], "verified");
    assert_eq!(listings.load(Ordering::SeqCst), 2);
    assert_eq!(
        cached_check(&state_dir).unwrap().latest_version.as_deref(),
        Some("99.7")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_check_caches_timeout_and_does_not_repeat_it_on_the_next_success() {
    let workspace = TempWorkspace::create("forvo-update-check-timeout").unwrap();
    let pkcs8 = release::generate_key().unwrap();
    let fixture = make_release(workspace.path(), "99.11", &pkcs8);
    let (base, requests) = serve_counted_stall().await;
    let state_dir = workspace.path().join("state");
    let timeout = Duration::from_millis(200);

    let started = std::time::Instant::now();
    let notice = auto_check_with_channel(
        &state_dir,
        None,
        Duration::from_secs(3600),
        timeout,
        channel(&base),
        fixture.trust.clone(),
    )
    .await;
    assert!(notice.is_none());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        cached_check(&state_dir).unwrap().error.as_deref(),
        Some("timeout")
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    let started = std::time::Instant::now();
    let notice = auto_check_with_channel(
        &state_dir,
        None,
        Duration::from_secs(3600),
        timeout,
        channel(&base),
        fixture.trust,
    )
    .await;
    assert!(notice.is_none());
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn check_verifies_metadata_signature_from_the_channel() {
    let workspace = TempWorkspace::create("forvo-update-check").unwrap();
    let pkcs8 = release::generate_key().unwrap();
    let r = make_release(workspace.path(), "99.1", &pkcs8);
    let tag = release::tag(&r.version);
    let base = serve(
        BTreeMap::from([
            (
                listing_path(1),
                (
                    200,
                    serde_json::to_vec(&json!([{"tag_name":tag,"draft":false,"prerelease":false}]))
                        .unwrap(),
                ),
            ),
            (
                asset_path(&tag, release::METADATA_ASSET),
                (200, r.metadata.clone()),
            ),
            (
                asset_path(&tag, release::SIGNATURE_ASSET),
                (200, r.signature.clone()),
            ),
        ]),
        false,
    )
    .await;
    let state_dir = workspace.path().join("state");
    let value = check(&state_dir, None, &Source::GitHub, &r.trust, &channel(&base))
        .await
        .unwrap();
    assert_eq!(value["status"], "verified");
    assert_eq!(value["version"], "99.1");
    assert_eq!(value["comparison"]["update_available"], true);
    assert_eq!(
        cached_check(&state_dir).unwrap().latest_version.as_deref(),
        Some("99.1")
    );
    // Свежий кэш: автоматическая проверка не ходит в сеть и сообщает о версии.
    let notice = auto_check(
        &state_dir,
        None,
        Duration::from_secs(3600),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(notice["available_version"], "99.1");

    let tag = release::tag(&r.version);
    let invalid_signature = serve(
        BTreeMap::from([
            (
                listing_path(1),
                (
                    200,
                    serde_json::to_vec(&json!([{"tag_name":tag,"draft":false,"prerelease":false}]))
                        .unwrap(),
                ),
            ),
            (
                asset_path(&tag, release::METADATA_ASSET),
                (200, r.metadata.clone()),
            ),
            (
                asset_path(&tag, release::SIGNATURE_ASSET),
                (200, b"not a signature".to_vec()),
            ),
        ]),
        false,
    )
    .await;
    let rejected = check(
        &state_dir,
        None,
        &Source::GitHub,
        &r.trust,
        &channel(&invalid_signature),
    )
    .await
    .unwrap_err();
    assert_eq!(provider_code(&rejected), "release_signature_invalid");
    assert_eq!(
        cached_check(&state_dir).unwrap().error.as_deref(),
        Some("release_signature_invalid")
    );

    // Без корня доверия явная проверка завершается блокирующей ошибкой.
    let blocked = check(
        &state_dir,
        None,
        &Source::GitHub,
        &TrustRoot::Embedded(vec![]),
        &channel(&base),
    )
    .await
    .unwrap_err();
    assert_eq!(provider_code(&blocked), "release_trust_root_missing");
    assert_eq!(blocked.details["status"], "BLOCKED");
    // Неподтверждённая версия в кэш как доступная не попадает, ошибка сохраняется.
    let cache = cached_check(&state_dir).unwrap();
    assert!(cache.latest_version.is_none());
    assert_eq!(cache.error.as_deref(), Some("release_trust_root_missing"));

    // Даже пустой список не становится успешной проверкой без корня доверия.
    let empty = serve(
        BTreeMap::from([(listing_path(1), (200, b"[]".to_vec()))]),
        false,
    )
    .await;
    let blocked_empty_channel = check(
        &state_dir,
        None,
        &Source::GitHub,
        &TrustRoot::Embedded(vec![]),
        &channel(&empty),
    )
    .await
    .unwrap_err();
    assert_eq!(
        provider_code(&blocked_empty_channel),
        "release_trust_root_missing"
    );
    assert_eq!(blocked_empty_channel.details["status"], "BLOCKED");
}

// --- Программное расширение ----------------------------------------------------

#[derive(Default)]
struct Ext {
    pending_operation: bool,
    /// Браузер не перечитывает файлы при reload (расширение загружено из другого каталога).
    stale_reload: bool,
    stale_version: Option<String>,
    extension_id: String,
    instance: Option<String>,
    reload_extension_id: Option<String>,
    reload_instance: Option<String>,
    reloads: u32,
    connections: u32,
}

type Shared = Arc<Mutex<Ext>>;

async fn next_json(
    socket: &mut async_tungstenite::WebSocketStream<
        async_tungstenite::tokio::TokioAdapter<tokio::net::TcpStream>,
    >,
) -> Option<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).ok(),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

fn disk_version(dir: &Path) -> String {
    serde_json::from_slice::<Value>(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap()["version"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn spawn_extension(port: u16, install_dir: PathBuf, ext: Shared) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (version, extension_id, instance) = {
                let e = ext.lock().unwrap();
                (
                    e.stale_version.clone(),
                    e.extension_id.clone(),
                    e.instance.clone(),
                )
            };
            let version = version.unwrap_or_else(|| disk_version(&install_dir));
            let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let mut request = format!("ws://127.0.0.1:{port}{}", super::super::WS_PATH)
                .into_client_request()
                .unwrap();
            request.headers_mut().insert(
                "origin",
                format!("chrome-extension://{extension_id}")
                    .parse()
                    .unwrap(),
            );
            let Ok((mut socket, _)) = async_tungstenite::tokio::client_async(request, stream).await
            else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let Some(hello) = next_json(&mut socket).await else {
                continue;
            };
            let nonce = hello["nonce"].as_str().unwrap().to_owned();
            let own = "33".repeat(32);
            let mac = encode_lower_hex(super::super::hmac_sha256(
                &SECRET,
                super::super::mac_text("extension", &nonce, &own, &extension_id).as_bytes(),
            ));
            let auth = json!({"type":"auth","v":1,"nonce":own,"extension_id":extension_id,"version":version,"mac":mac});
            if socket.send(Message::text(auth.to_string())).await.is_err() {
                continue;
            }
            if next_json(&mut socket)
                .await
                .is_none_or(|v| v["type"] != "auth_ok")
            {
                continue;
            }
            let ready = json!({"type":"ready","version":version,"instance":instance});
            if socket.send(Message::text(ready.to_string())).await.is_err() {
                continue;
            }
            ext.lock().unwrap().connections += 1;
            while let Some(message) = next_json(&mut socket).await {
                let op = message["op"].as_str().unwrap_or_default().to_owned();
                let result = match op.as_str() {
                    "ping" => {
                        let pending = ext.lock().unwrap().pending_operation;
                        json!({"operation":if pending {json!({"op_id":"x","bound":true})} else {Value::Null}})
                    }
                    "reload" => json!({"reloading":true}),
                    other => panic!("неожиданная команда {other}"),
                };
                let reply = json!({"type":"res","id":message["id"],"ok":true,"result":result});
                let _ = socket.send(Message::text(reply.to_string())).await;
                if op == "reload" {
                    let mut e = ext.lock().unwrap();
                    e.reloads += 1;
                    if !e.stale_reload {
                        e.stale_version = None;
                    }
                    if let Some(id) = e.reload_extension_id.take() {
                        e.extension_id = id;
                    }
                    if let Some(instance) = e.reload_instance.take() {
                        e.instance = Some(instance);
                    }
                    break;
                }
            }
        }
    })
}

struct Harness {
    workspace: TempWorkspace,
    state_dir: PathBuf,
    install_dir: PathBuf,
    ext: Shared,
    task: tokio::task::JoinHandle<()>,
    pkcs8: Vec<u8>,
    port: u16,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn harness(name: &str, installed: &str) -> Harness {
    let workspace = TempWorkspace::create(name).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let state_dir = workspace.path().join("state");
    let install_dir = workspace.path().join("extension");
    let state = AdapterState {
        target: "chrome".into(),
        browser: super::super::BrowserKind::Chrome,
        port,
        secret: encode_lower_hex(SECRET),
        install_dir: install_dir.clone(),
        load_unpacked_path: install_dir.display().to_string(),
        platform: super::super::Platform::Linux,
        extension_id: Some(ID.into()),
        instance_id: Some("aa".repeat(32)),
        executable: None,
    };
    super::super::save_state(&state_dir, &state).unwrap();
    let mut files = files_with_version(installed);
    files.push(PackageFile {
        path: package::PAIRING_FILE.into(),
        bytes: serde_json::to_vec(&json!({"schema_version":1,"port":port,"secret":state.secret}))
            .unwrap(),
    });
    install::install(&install_dir, &files, (None, None)).unwrap();
    let ext: Shared = Arc::new(Mutex::new(Ext {
        extension_id: ID.into(),
        instance: Some("aa".repeat(32)),
        ..Ext::default()
    }));
    let task = spawn_extension(port, install_dir.clone(), ext.clone());
    Harness {
        workspace,
        state_dir,
        install_dir,
        ext,
        task,
        pkcs8: release::generate_key().unwrap(),
        port,
    }
}

impl Harness {
    fn options(&self, release: &Release) -> UpdateOptions {
        UpdateOptions {
            state_dir: self.state_dir.clone(),
            target: None,
            source: Source::Directory(release.dir.clone()),
            version: None,
            trust: release.trust.clone(),
            wait: Duration::from_secs(10),
            reinstall: false,
            channel: Channel::default(),
        }
    }
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(&self.install_dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect()
    }
    fn state_bytes(&self) -> Vec<u8> {
        std::fs::read(self.state_dir.join("state.json")).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn update_installs_signed_release_in_place_and_keeps_pairing_and_identity() {
    let h = harness("forvo-update-flow", "0.1");
    let pairing = std::fs::read(h.install_dir.join("pairing.json")).unwrap();
    let state = h.state_bytes();
    let r = make_release(h.workspace.path(), "99.2", &h.pkcs8);
    let value = update(&h.options(&r)).await.unwrap();
    assert_eq!(value["status"], "updated", "{value}");
    assert_eq!(value["from_version"], "0.1");
    assert_eq!(value["to_version"], "99.2");
    assert_eq!(value["extension"]["version"], "99.2");
    assert_eq!(value["extension"]["same_extension_id"], true);
    assert_eq!(value["extension"]["same_instance"], true);
    assert_eq!(value["pairing"], "kept");
    assert_eq!(disk_version(&h.install_dir), "99.2");
    assert_eq!(
        std::fs::read(h.install_dir.join("pairing.json")).unwrap(),
        pairing
    );
    assert_eq!(
        h.state_bytes(),
        state,
        "state.json не меняется: секрет, порт, ID и экземпляр прежние"
    );
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
    // Повтор и та же версия — no-op без перезагрузки.
    let again = update(&h.options(&r)).await.unwrap();
    assert_eq!(again["status"], "up_to_date");
    assert_eq!(again["files_match_release"], true);
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
    // Обычный setup/sync более новую установленную версию не откатывает.
    let target = targets::load_target(&h.state_dir, None).unwrap();
    assert!(
        targets::write_extension_files(&target.install_dir, target.port, &target.secret)
            .unwrap()
            .is_empty()
    );
    assert_eq!(disk_version(&h.install_dir), "99.2");
    assert_eq!(
        targets::files_status(&target)["source"],
        "release_newer_than_bundled"
    );
    // Понижение версии — отказ без изменений.
    let before = h.files();
    let old = make_release(h.workspace.path(), "0.0.5", &h.pkcs8);
    assert_eq!(
        provider_code(&update(&h.options(&old)).await.unwrap_err()),
        "downgrade_refused"
    );
    assert_eq!(h.files(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_target_pending_operation_and_tampering_leave_installation_unchanged() {
    let h = harness("forvo-update-refusals", "0.1");
    let r = make_release(h.workspace.path(), "99.3", &h.pkcs8);
    let before = h.files();
    // Порт цели занят (идёт fetch/пакет или второй updater).
    {
        let _held = std::net::TcpListener::bind(("127.0.0.1", h.port)).unwrap();
        let error = update(&h.options(&r)).await.unwrap_err();
        assert_eq!(provider_code(&error), "adapter_busy");
    }
    assert_eq!(h.files(), before);
    // Незавершённая собственная операция расширения.
    h.ext.lock().unwrap().pending_operation = true;
    let error = update(&h.options(&r)).await.unwrap_err();
    assert_eq!(provider_code(&error), "update_deferred_operation_pending");
    assert_eq!(error.details["files_changed"], false);
    assert_eq!(h.files(), before);
    assert_eq!(h.ext.lock().unwrap().reloads, 0);
    h.ext.lock().unwrap().pending_operation = false;
    // Подменённый байт архива в каталоге выпуска.
    let mut zip = r.zip.clone();
    let last = zip.len() - 30;
    zip[last] ^= 0xff;
    std::fs::write(r.dir.join(release::archive_name(&r.version)), &zip).unwrap();
    let error = update(&h.options(&r)).await.unwrap_err();
    assert_eq!(provider_code(&error), "release_archive_mismatch");
    assert_eq!(h.files(), before);
    // Ключ, которому CLI не доверяет.
    std::fs::write(r.dir.join(release::archive_name(&r.version)), &r.zip).unwrap();
    let mut untrusted = h.options(&r);
    untrusted.trust = TrustRoot::Embedded(vec![]);
    assert_eq!(
        provider_code(&update(&untrusted).await.unwrap_err()),
        "release_trust_root_missing"
    );
    assert_eq!(h.files(), before);
    let _ = &r.metadata;
    let _ = &r.signature;
}

#[tokio::test(flavor = "multi_thread")]
async fn ineffective_reload_is_reported_without_claiming_full_update() {
    let h = harness("forvo-update-stale", "0.1");
    {
        let mut e = h.ext.lock().unwrap();
        e.stale_reload = true;
        e.stale_version = Some("0.1".into());
    }
    let r = make_release(h.workspace.path(), "99.4", &h.pkcs8);
    let error = update(&h.options(&r)).await.unwrap_err();
    assert_eq!(provider_code(&error), "extension_reload_ineffective");
    assert_eq!(error.details["files_installed"], true);
    assert_eq!(error.details["extension_version"], "0.1");
    assert!(error.details["user_action"].is_string());
    assert_eq!(disk_version(&h.install_dir), "99.4");
    assert!(h.ext.lock().unwrap().connections >= 2);

    // Повтор после обрыва между записью файлов и подтверждением reload
    // проверяет старый runtime и завершает только недостающую перезагрузку.
    {
        let mut e = h.ext.lock().unwrap();
        e.stale_reload = false;
        // До команды reload runtime остаётся старым; успешный reload в
        // эмуляторе сам снимает это закреплённое значение и перечитывает диск.
        e.stale_version = Some("0.1".into());
    }
    let mut retry = h.options(&r);
    retry.wait = Duration::from_secs(2);
    let value = update(&retry).await.unwrap();
    assert_eq!(value["status"], "updated", "{value}");
    assert_eq!(value["from_version"], "99.4");
    assert_eq!(value["to_version"], "99.4");
    assert_eq!(value["files_match_release"], true);
    assert_eq!(value["files_changed"], json!([]));
    assert_eq!(value["runtime_confirmed"], true);
    assert_eq!(h.ext.lock().unwrap().reloads, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn same_version_with_different_files_requires_explicit_reinstall() {
    let h = harness("forvo-update-different-files", "0.1");
    let release = make_release(h.workspace.path(), "0.1", &h.pkcs8);
    let path = h.install_dir.join("background.js");
    let mut tampered = std::fs::read(&path).unwrap();
    tampered.push(b'\n');
    std::fs::write(&path, tampered).unwrap();
    let before = h.files();

    let error = update(&h.options(&release)).await.unwrap_err();
    assert_eq!(provider_code(&error), "installed_files_differ");
    assert_eq!(error.details["status"], "REPAIR_REQUIRED");
    assert_eq!(error.details["files_match_release"], false);
    assert_eq!(h.files(), before);
    assert_eq!(h.ext.lock().unwrap().reloads, 0);

    let mut repair = h.options(&release);
    repair.reinstall = true;
    let value = update(&repair).await.unwrap();
    assert_eq!(value["status"], "updated", "{value}");
    assert_eq!(value["files_match_release"], true);
    assert_eq!(disk_version(&h.install_dir), "0.1");
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_state_uses_first_live_instance_as_reload_binding() {
    let h = harness("forvo-update-legacy-state", "0.1");
    let mut state = targets::load_target(&h.state_dir, None).unwrap();
    state.instance_id = None;
    targets::save_state(&h.state_dir, &state).unwrap();
    let release = make_release(h.workspace.path(), "99.5", &h.pkcs8);

    let value = update(&h.options(&release)).await.unwrap();
    assert_eq!(value["status"], "updated", "{value}");
    assert_eq!(value["extension"]["same_extension_id"], true);
    assert_eq!(value["extension"]["same_instance"], true);
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_live_peer_without_instance_refuses_update_before_writing() {
    let h = harness("forvo-update-legacy-peer", "0.1");
    let mut state = targets::load_target(&h.state_dir, None).unwrap();
    state.instance_id = None;
    targets::save_state(&h.state_dir, &state).unwrap();
    h.ext.lock().unwrap().instance = None;
    let release = make_release(h.workspace.path(), "99.8", &h.pkcs8);
    let before = h.files();

    let mut options = h.options(&release);
    options.wait = Duration::from_secs(2);
    let error = update(&options).await.unwrap_err();
    assert_eq!(provider_code(&error), "extension_identity_unverifiable");
    assert_eq!(error.details["files_changed"], false);
    assert_eq!(h.files(), before);
    assert_eq!(h.ext.lock().unwrap().reloads, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_noop_reports_unknown_instance_without_claiming_a_reload() {
    let h = harness("forvo-update-legacy-noop", "0.1");
    let mut state = targets::load_target(&h.state_dir, None).unwrap();
    state.instance_id = None;
    targets::save_state(&h.state_dir, &state).unwrap();
    h.ext.lock().unwrap().instance = None;
    let release = make_release(h.workspace.path(), "0.1", &h.pkcs8);

    let value = update(&h.options(&release)).await.unwrap();
    assert_eq!(value["status"], "up_to_date");
    assert_eq!(value["runtime_confirmed"], true);
    assert_eq!(value["extension"]["instance_available"], false);
    assert!(value["extension"]["same_instance"].is_null());
    assert_eq!(h.ext.lock().unwrap().reloads, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_rejects_changed_instance_after_reload() {
    let h = harness("forvo-update-instance-change", "0.1");
    h.ext.lock().unwrap().reload_instance = Some("bb".repeat(32));
    let release = make_release(h.workspace.path(), "99.9", &h.pkcs8);
    let mut options = h.options(&release);
    options.wait = Duration::from_millis(250);

    let error = update(&options).await.unwrap_err();
    assert_eq!(provider_code(&error), "extension_identity_changed");
    assert_eq!(error.details["files_installed"], true);
    assert_eq!(error.details["disk_version"], "99.9");
    assert_eq!(disk_version(&h.install_dir), "99.9");
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_rejects_changed_extension_id_after_reload() {
    let h = harness("forvo-update-extension-change", "0.1");
    h.ext.lock().unwrap().reload_extension_id = Some(OTHER_ID.into());
    let release = make_release(h.workspace.path(), "99.10", &h.pkcs8);
    let mut options = h.options(&release);
    options.wait = Duration::from_millis(250);

    let error = update(&options).await.unwrap_err();
    assert_eq!(provider_code(&error), "extension_identity_changed");
    assert_eq!(error.details["files_installed"], true);
    assert_eq!(error.details["disk_version"], "99.10");
    assert_eq!(disk_version(&h.install_dir), "99.10");
    assert_eq!(h.ext.lock().unwrap().reloads, 1);
}
