//! Production-получение через Brave против программного расширения, которое
//! говорит с настоящим bridge по протоколу `background.js` и кладёт настоящие
//! файлы в каталог «загрузок». Проверяются общий порядок попыток, единый
//! `ForvoEvidence`, освобождение собственных операций и пакет поверх
//! `SafeBatchRuntime` с закреплённым транспортом.
use super::*;
use crate::forvo::batch::{self, Backend, FetchItem, FetchPlan, Transport};
use crate::forvo::tests::{candidate, synthetic_mp3};
use crate::forvo::{Gender, pauses_batch};
use crate::hashing::{encode_lower_hex, sha256_hex};
use crate::temp_workspace::TempWorkspace;
use async_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const ID: &str = "abcdefghijklmnopabcdefghijklmnop";
const SECRET: [u8; 32] = [5; 32];

/// Что отдаёт загрузка записи.
#[derive(Clone)]
enum Body {
    Bytes(Vec<u8>),
    /// Окно «Сохранить как»: имени файла нет.
    SaveAs,
    /// Загрузка идёт и не завершается.
    Hang,
}

#[derive(Clone)]
struct Record {
    candidate: Candidate,
    body: Body,
}

#[derive(Default)]
struct World {
    words: BTreeMap<String, Vec<Record>>,
    /// Строки, которые увидит повторная проверка перед загрузкой (`inspect`).
    inspect_override: BTreeMap<String, Vec<Candidate>>,
    challenge: bool,
    begins: Vec<String>,
    releases: Vec<Value>,
    op: Option<(String, String, String)>,
    page: Option<String>,
    /// Экземпляр расширения в `ready` (у старых версий его нет).
    instance: Option<String>,
}

type Shared = Arc<Mutex<World>>;

fn lock(world: &Shared) -> std::sync::MutexGuard<'_, World> {
    world
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn download_url(surface: &str, record: &str) -> Value {
    json!({"origin":"https://forvo.com","path":format!("/download/mp3/{surface}/ja/{record}"),
        "has_query":false,"has_fragment":false,"has_credentials":false})
}

fn respond(world: &Shared, downloads: &std::path::Path, op: &str, args: &Value) -> Value {
    let mut w = lock(world);
    match op {
        "ping" => {
            json!({"ok":true,"result":{"operation":null,"listeners":{"determining_filename":false,"web_request":false}}})
        }
        "snapshot" => json!({"ok":true,"result":{"tabs":[{"id":1}],"groups":[]}}),
        "open" => {
            json!({"ok":true,"result":{"created":true,"active":false,"group_title":super::super::GROUP_TITLE}})
        }
        "navigate" => {
            let url = args["url"].as_str().unwrap_or_default();
            let surface = url::Url::parse(url)
                .ok()
                .and_then(|u| {
                    u.path_segments()
                        .and_then(|mut s| s.nth(1).map(str::to_owned))
                })
                .map(|s| percent_decode(&s));
            w.page = surface;
            json!({"ok":true,"result":{"load":"complete","path":"/word/"}})
        }
        "auth" => {
            let known = w.page.as_ref().is_some_and(|p| w.words.contains_key(p));
            json!({"ok":true,"result":{"same_origin":true,"status":if known {200} else {404},
                "challenge":w.challenge,"logout":true,"login":false,"ready":true}})
        }
        "dom" => {
            let surface = args["surface"].as_str().unwrap_or_default().to_owned();
            let rows: Vec<Candidate> = if args["action"] == "inspect" {
                w.inspect_override.get(&surface).cloned()
            } else {
                None
            }
            .unwrap_or_else(|| {
                w.words
                    .get(&surface)
                    .map(|r| r.iter().map(|r| r.candidate.clone()).collect())
                    .unwrap_or_default()
            });
            json!({"ok":true,"result":{"code":"ok","candidates":rows}})
        }
        "download_begin" => {
            let op_id = args["op_id"].as_str().unwrap().to_owned();
            let surface = args["surface"].as_str().unwrap().to_owned();
            let record = args["record_id"].as_str().unwrap().to_owned();
            w.begins.push(record.clone());
            if let Some(Body::Bytes(bytes)) = w.words[&surface]
                .iter()
                .find(|r| r.candidate.record_id == record)
                .map(|r| r.body.clone())
            {
                std::fs::write(
                    downloads.join(format!("pronunciation_ja_{record}.mp3")),
                    bytes,
                )
                .unwrap();
            }
            w.op = Some((op_id.clone(), surface, record));
            json!({"ok":true,"result":{"code":"activated","op_id":op_id}})
        }
        "download_status" => {
            let Some((op_id, surface, record)) = w.op.clone() else {
                return json!({"ok":true,"result":{"code":"operation_unknown"}});
            };
            assert_eq!(args["op_id"], op_id);
            let body = w.words[&surface]
                .iter()
                .find(|r| r.candidate.record_id == record)
                .unwrap()
                .body
                .clone();
            let path = downloads.join(format!("pronunciation_ja_{record}.mp3"));
            let (state, has_filename, size, elapsed) = match &body {
                Body::Bytes(b) => ("complete", true, b.len() as i64, 10),
                Body::SaveAs => ("in_progress", false, 0, 60_000),
                Body::Hang => ("in_progress", true, 0, 10),
            };
            json!({"ok":true,"result":{"code":"status","observer_alive":true,"own_requests":1,"foreign_requests":0,
                "matching_downloads":1,"conflict":false,"observer_lost":false,"bound_id":7,"bound_missing":false,
                "tab_present":true,"page_unchanged":true,"elapsed_ms":elapsed,
                "item":{"id":7,"state":state,"error":null,"danger":"safe","exists":true,"paused":false,
                    "has_filename":has_filename,"filename":has_filename.then(|| path.display().to_string()),
                    "file_name":has_filename.then(|| path.file_name().unwrap().to_string_lossy().into_owned()),
                    "bytes_received":size,"total_bytes":size,"file_size":size,"mime":"audio/mpeg",
                    "url":download_url(&surface,&record),"final_url":download_url(&surface,&record),
                    "referrer_is_own_page":true,"referrer_origin_is_forvo":true,"by_extension":false,
                    "start_time":null,"end_time":null}}})
        }
        "download_release" => {
            let current = w.op.take();
            let mut result = json!({"released":current.is_some(),"bound":current.is_some(),"canceled":false,"removed_file":false,"erased":false});
            if let Some((_, _, record)) = current {
                let path = downloads.join(format!("pronunciation_ja_{record}.mp3"));
                result["removed_file"] = json!(std::fs::remove_file(path).is_ok());
                result["canceled"] = json!(!result["removed_file"].as_bool().unwrap());
                result["erased"] = json!(true);
            }
            w.releases.push(result.clone());
            json!({"ok":true,"result":result})
        }
        "close" => json!({"ok":true,"result":{"closed":true}}),
        other => panic!("неожиданная команда {other}"),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

type Client = async_tungstenite::WebSocketStream<TokioAdapter<TcpStream>>;
use async_tungstenite::tokio::TokioAdapter;
use tokio::net::TcpStream;

async fn next_json(socket: &mut Client) -> Option<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).ok(),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

async fn pair(port: u16, instance: Option<String>) -> Option<Client> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let mut request = format!("ws://127.0.0.1:{port}{}", super::super::WS_PATH)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "origin",
        format!("chrome-extension://{ID}").parse().unwrap(),
    );
    let (mut socket, _) = async_tungstenite::tokio::client_async(request, stream)
        .await
        .ok()?;
    let hello = next_json(&mut socket).await?;
    let nonce = hello["nonce"].as_str()?.to_owned();
    let own = "22".repeat(32);
    let mac = encode_lower_hex(super::super::hmac_sha256(
        &SECRET,
        super::super::mac_text("extension", &nonce, &own, ID).as_bytes(),
    ));
    socket
        .send(Message::text(
            json!({"type":"auth","v":1,"nonce":own,"extension_id":ID,"version":"0.1.0","mac":mac})
                .to_string(),
        ))
        .await
        .ok()?;
    next_json(&mut socket)
        .await
        .filter(|v| v["type"] == "auth_ok")?;
    socket
        .send(Message::text(
            json!({"type":"ready","instance":instance}).to_string(),
        ))
        .await
        .ok()?;
    Some(socket)
}

/// Программное расширение: переподключается, как настоящее, пока тест не завершён.
fn spawn_extension(port: u16, world: Shared, downloads: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let instance = lock(&world).instance.clone();
            let Some(mut socket) = pair(port, instance).await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            while let Some(message) = next_json(&mut socket).await {
                if message["type"] != "cmd" {
                    continue;
                }
                let op = message["op"].as_str().unwrap_or_default().to_owned();
                let mut reply = respond(&world, &downloads, &op, &message["args"]);
                reply["type"] = json!("res");
                reply["id"] = message["id"].clone();
                if socket.send(Message::text(reply.to_string())).await.is_err() {
                    break;
                }
            }
        }
    })
}

struct Harness {
    _workspace: TempWorkspace,
    options: BraveOptions,
    world: Shared,
    downloads: PathBuf,
    root: PathBuf,
    extension: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.extension.abort();
    }
}

fn harness(words: Vec<(&str, Vec<Record>)>) -> Harness {
    let workspace = TempWorkspace::create("forvo-brave-fetch").unwrap();
    let root = workspace.path().to_owned();
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let state_dir = root.join("state");
    super::super::save_state(
        &state_dir,
        &super::super::AdapterState {
            target: super::super::LEGACY_TARGET.into(),
            browser: super::super::BrowserKind::Brave,
            port,
            secret: encode_lower_hex(SECRET),
            install_dir: root.join("extension"),
            load_unpacked_path: "unused".into(),
            platform: Platform::Linux,
            extension_id: Some(ID.into()),
            instance_id: None,
            executable: None,
        },
    )
    .unwrap();
    let world: Shared = Arc::new(Mutex::new(World {
        words: words.into_iter().map(|(w, r)| (w.to_owned(), r)).collect(),
        ..World::default()
    }));
    let extension = spawn_extension(port, world.clone(), downloads.clone());
    Harness {
        _workspace: workspace,
        options: BraveOptions {
            state_dir,
            target: None,
            binding: None,
            wait: Duration::from_secs(10),
            limits: Limits {
                start: Duration::from_secs(2),
                prompt: Duration::from_secs(1),
                total: Duration::from_secs(5),
            },
            close_tab: false,
        },
        world,
        downloads,
        root,
        extension,
    }
}

fn record(id: &str, votes: i64, body: Body) -> Record {
    Record {
        candidate: candidate(id, Some(votes), Some(Gender::Female), Some("Japan")),
        body,
    }
}

fn store(root: &std::path::Path) -> AssetStore {
    AssetStore::open_with_policy(
        crate::StoreOptions::new(root.join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap()
}

#[tokio::test]
async fn first_candidate_succeeds_with_single_download_and_release() {
    let mp3 = synthetic_mp3();
    let h = harness(vec![(
        "元気",
        vec![
            record("10", 5, Body::Bytes(mp3.clone())),
            record("20", 1, Body::Bytes(b"<html>".to_vec())),
        ],
    )]);
    let request = Pronunciation::new("元気", "げんき").unwrap();
    let mut session = BraveSession::connect(&h.options).await.unwrap();
    let acquisition = acquire(&mut session, &request).await.unwrap();
    assert_eq!(acquisition.bytes, mp3);
    acquisition
        .evidence
        .validate(&request, &sha256_hex(&mp3))
        .unwrap();
    assert_eq!(acquisition.evidence.attempts.len(), 1);
    assert_eq!(
        acquisition.evidence.page_url,
        crate::forvo::word_url("元気")
    );
    session.close().await.unwrap();
    let w = lock(&h.world);
    assert_eq!(w.begins, ["10"]);
    assert_eq!(w.releases.len(), 1);
    assert_eq!(w.releases[0]["removed_file"], true);
    assert_eq!(std::fs::read_dir(&h.downloads).unwrap().count(), 0);
}

#[tokio::test]
async fn rejected_candidate_falls_back_and_changed_row_is_skipped() {
    let mp3 = synthetic_mp3();
    let h = harness(vec![(
        "元気",
        vec![
            record("10", 9, Body::Bytes(b"not an mp3 at all".repeat(40))),
            record("20", 5, Body::Bytes(mp3.clone())),
            record("30", 4, Body::Bytes(mp3.clone())),
            record("40", 1, Body::Bytes(mp3.clone())),
        ],
    )]);
    let request = Pronunciation::new("元気", "げんき").unwrap();
    let mut session = BraveSession::connect(&h.options).await.unwrap();
    let first = acquire(&mut session, &request).await.unwrap();
    assert_eq!(
        first
            .evidence
            .attempts
            .iter()
            .map(|a| a.code.as_str())
            .collect::<Vec<_>>(),
        ["invalid_mp3", "downloaded"]
    );
    assert_eq!(first.evidence.selected_record_id, "20");
    // Повторная проверка строки видит другой голос у 20 и пропавшую 30:
    // эти кандидаты не скачиваются, выбирается следующий.
    {
        let mut w = lock(&h.world);
        let mut changed: Vec<Candidate> = w.words["元気"]
            .iter()
            .map(|r| r.candidate.clone())
            .filter(|c| c.record_id != "30")
            .collect();
        changed[1].votes = Some(6);
        w.inspect_override.insert("元気".into(), changed);
        w.begins.clear();
    }
    let second = acquire(&mut session, &request).await.unwrap();
    assert_eq!(
        second
            .evidence
            .attempts
            .iter()
            .map(|a| a.code.as_str())
            .collect::<Vec<_>>(),
        [
            "invalid_mp3",
            "candidate_changed",
            "candidate_changed",
            "downloaded"
        ]
    );
    assert_eq!(lock(&h.world).begins, ["10", "40"]);
    second
        .evidence
        .validate(&request, &sha256_hex(&mp3))
        .unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn exhausted_candidates_and_missing_word_are_item_outcomes() {
    let h = harness(vec![(
        "元気",
        vec![
            record("10", 2, Body::Bytes(b"junk".repeat(64))),
            record("20", 1, Body::Bytes(b"<html>login</html>".to_vec())),
        ],
    )]);
    let mut session = BraveSession::connect(&h.options).await.unwrap();
    let error = acquire(&mut session, &Pronunciation::new("元気", "げんき").unwrap())
        .await
        .err()
        .unwrap();
    assert_eq!(provider_code(&error), "candidates_exhausted");
    assert!(!pauses_batch(&error));
    let missing = acquire(&mut session, &Pronunciation::new("無語", "むご").unwrap())
        .await
        .err()
        .unwrap();
    assert_eq!(provider_code(&missing), "word_not_found");
    assert!(!pauses_batch(&missing));
    assert_eq!(lock(&h.world).releases.len(), 2);
    session.close().await.unwrap();
}

#[tokio::test]
async fn blockers_pause_without_trying_next_candidate() {
    let mp3 = synthetic_mp3();
    let h = harness(vec![(
        "元気",
        vec![
            record("10", 2, Body::SaveAs),
            record("20", 1, Body::Bytes(mp3)),
        ],
    )]);
    let request = Pronunciation::new("元気", "げんき").unwrap();
    let mut session = BraveSession::connect(&h.options).await.unwrap();
    let error = acquire(&mut session, &request).await.err().unwrap();
    assert_eq!(provider_code(&error), "save_as_required");
    assert!(pauses_batch(&error));
    assert_eq!(error.details["attempts"][0]["code"], "save_as_required");
    assert_eq!(lock(&h.world).begins, ["10"]);
    lock(&h.world).challenge = true;
    let challenge = acquire(&mut session, &request).await.err().unwrap();
    assert_eq!(provider_code(&challenge), "challenge_required");
    assert!(pauses_batch(&challenge));
    assert!(
        challenge.details["next_action"]
            .as_str()
            .unwrap()
            .contains("forvo brave batch-resume")
    );
    assert_eq!(lock(&h.world).begins, ["10"]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn interrupt_releases_in_progress_own_download() {
    let h = harness(vec![("元気", vec![record("10", 2, Body::Hang)])]);
    let (tx, rx) = watch::channel(false);
    let mut session = BraveSession::connect(&h.options).await.unwrap();
    session.set_interrupt(Some(rx));
    let request = Pronunciation::new("元気", "げんき").unwrap();
    let (result, ()) = tokio::join!(acquire(&mut session, &request), async {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let _ = tx.send(true);
    });
    let error = result.err().unwrap();
    assert_eq!(error.code, ErrorCode::Interrupted);
    let w = lock(&h.world);
    assert_eq!(w.releases.len(), 1);
    assert_eq!(w.releases[0]["canceled"], true);
    assert!(w.op.is_none());
}

fn plan(words: &[(&str, &str)]) -> FetchPlan {
    FetchPlan {
        schema_version: 1,
        items: words
            .iter()
            .map(|(s, r)| FetchItem {
                surface: (*s).into(),
                reading: (*r).into(),
                replace_expected_sha256: None,
            })
            .collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn brave_batch_checkpoints_imports_and_never_redownloads() {
    let mp3 = synthetic_mp3();
    let words = [("元気", "げんき"), ("水", "みず"), ("山", "やま")];
    let h = harness(
        words
            .iter()
            .enumerate()
            .map(|(i, (w, _))| {
                (
                    *w,
                    vec![record(&format!("{}1", i + 1), 3, Body::Bytes(mp3.clone()))],
                )
            })
            .collect(),
    );
    let store = store(&h.root);
    batch::start(&store, "brave", plan(&words), Transport::BraveExtension).unwrap();
    let cdp = batch::resume(
        &store,
        "brave",
        Backend::Cdp {
            profile: &h.root.join("never-created"),
        },
        10,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(provider_code(&cdp), "transport_mismatch");
    assert!(!h.root.join("never-created").exists());
    let first = batch::resume(&store, "brave", Backend::Brave(&h.options), 1)
        .await
        .unwrap();
    assert_eq!(first.items[0].outcome.as_ref().unwrap().code, "imported");
    assert!(first.items[1..].iter().all(|i| i.outcome.is_none()));
    let done = batch::resume(&store, "brave", Backend::Brave(&h.options), 10)
        .await
        .unwrap();
    assert!(done.paused.is_none());
    assert!(
        done.items
            .iter()
            .all(|i| i.outcome.as_ref().unwrap().code == "imported")
    );
    assert_eq!(done.transport(), Transport::BraveExtension);
    assert_eq!(lock(&h.world).begins, ["11", "21", "31"]);
    // Завершённый пакет: resume не подключается и ничего не скачивает.
    let again = batch::resume(&store, "brave", Backend::Brave(&h.options), 10)
        .await
        .unwrap();
    assert_eq!(again.revision, done.revision);
    assert_eq!(lock(&h.world).begins.len(), 3);
    for (surface, reading) in words {
        let request = Pronunciation::new(surface, reading).unwrap();
        let record = store
            .verify_integrity()
            .unwrap()
            .into_iter()
            .find(|r| r.identity == request.identity())
            .expect("ресурс импортирован");
        assert_eq!(record.sha256, sha256_hex(&mp3));
        assert_ne!(record.lifecycle, crate::model::LifecycleState::Verified);
    }
    assert_eq!(std::fs::read_dir(&h.downloads).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn blocker_pauses_batch_and_resume_continues_after_recovery() {
    let mp3 = synthetic_mp3();
    let h = harness(vec![
        ("元気", vec![record("11", 3, Body::Bytes(mp3.clone()))]),
        ("水", vec![record("21", 3, Body::Bytes(mp3.clone()))]),
    ]);
    let store = store(&h.root);
    batch::start(
        &store,
        "pause",
        plan(&[("元気", "げんき"), ("水", "みず")]),
        Transport::BraveExtension,
    )
    .unwrap();
    lock(&h.world).challenge = true;
    let paused = batch::resume(&store, "pause", Backend::Brave(&h.options), 10)
        .await
        .unwrap();
    assert_eq!(paused.paused.as_ref().unwrap().code, "challenge_required");
    assert!(paused.items.iter().all(|i| i.outcome.is_none()));
    lock(&h.world).challenge = false;
    let done = batch::resume(&store, "pause", Backend::Brave(&h.options), 10)
        .await
        .unwrap();
    assert!(done.paused.is_none());
    assert!(
        done.items
            .iter()
            .all(|i| i.outcome.as_ref().unwrap().code == "imported")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_is_bound_to_first_browser_session() {
    let mp3 = synthetic_mp3();
    let h = harness(vec![
        ("元気", vec![record("11", 3, Body::Bytes(mp3.clone()))]),
        ("水", vec![record("21", 3, Body::Bytes(mp3.clone()))]),
    ]);
    let mine = "aa".repeat(32);
    lock(&h.world).instance = Some(mine.clone());
    let store = store(&h.root);
    batch::start(
        &store,
        "bound",
        plan(&[("元気", "げんき"), ("水", "みず")]),
        Transport::BraveExtension,
    )
    .unwrap();
    let first = batch::resume(&store, "bound", Backend::Brave(&h.options), 1)
        .await
        .unwrap();
    assert_eq!(first.schema_version, 3);
    let binding = first.browser.clone().expect("пакет закреплён за сессией");
    assert_eq!(
        (
            binding.target.as_str(),
            binding.extension_id.as_str(),
            binding.instance_id.as_deref()
        ),
        ("brave", ID, Some(mine.as_str()))
    );
    // Другая цель — отказ до подключения к браузеру.
    let other_target = batch::resume(
        &store,
        "bound",
        Backend::Brave(&BraveOptions {
            target: Some("chrome".into()),
            ..h.options.clone()
        }),
        10,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(provider_code(&other_target), "browser_mismatch");
    // Тот же каталог в другом профиле (другой экземпляр) не продолжает пакет:
    // пакет приостанавливается, элемент не помечается неудачным.
    lock(&h.world).instance = Some("bb".repeat(32));
    let options = BraveOptions {
        wait: Duration::from_secs(2),
        ..h.options.clone()
    };
    let paused = batch::resume(&store, "bound", Backend::Brave(&options), 10)
        .await
        .unwrap();
    assert_eq!(paused.paused.as_ref().unwrap().code, "instance_mismatch");
    assert!(paused.items[1].outcome.is_none());
    assert_eq!(lock(&h.world).begins, ["11"]);
    lock(&h.world).instance = Some(mine);
    let done = batch::resume(&store, "bound", Backend::Brave(&h.options), 10)
        .await
        .unwrap();
    assert!(done.paused.is_none());
    assert_eq!(done.browser, Some(binding));
    assert_eq!(lock(&h.world).begins, ["11", "21"]);
}

#[test]
fn unbound_pre_selection_batch_continues_only_with_brave_target() {
    let workspace = TempWorkspace::create("forvo-v2-batch").unwrap();
    let store = store(workspace.path());
    let mut runtime = crate::batch_runtime::SafeBatchRuntime::open(store.root(), "v2").unwrap();
    let legacy = json!({"schema_version":2,"provider":"forvo_web","transport":"brave_extension","batch_id":"v2",
        "revision":0,"cancelled":false,"paused":null,
        "items":[{"request":Pronunciation::new("水","みず").unwrap(),"replace_expected_sha256":null,"acquisition":null,"outcome":null}]});
    runtime
        .save(&serde_json::from_value::<batch::ForvoBatch>(legacy).unwrap())
        .unwrap();
    drop(runtime);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(batch::resume(
            &store,
            "v2",
            Backend::Brave(&BraveOptions {
                state_dir: workspace.path().join("no-adapter"),
                target: Some("edge".into()),
                binding: None,
                wait: Duration::from_secs(1),
                limits: Limits::default(),
                close_tab: false,
            }),
            10,
        ))
        .err()
        .unwrap();
    assert_eq!(provider_code(&error), "browser_mismatch");
    assert_eq!(error.details["batch_target"], "brave");
    assert!(!workspace.path().join("no-adapter").exists());
}

#[test]
fn legacy_cdp_state_stays_readable_and_bound_to_cdp() {
    use crate::batch_runtime::SafeBatchRuntime;
    let workspace = TempWorkspace::create("forvo-legacy-state").unwrap();
    let store = store(workspace.path());
    // Состояние версии 1 так, как его сохранял прежний код: без поля transport.
    let legacy = json!({"schema_version":1,"provider":"forvo_web","batch_id":"old","revision":0,
        "cancelled":false,"paused":null,"items":[{"request":Pronunciation::new("元気","げんき").unwrap(),
        "replace_expected_sha256":null,"acquisition":null,"outcome":null}]});
    let batch: batch::ForvoBatch = serde_json::from_value(legacy.clone()).unwrap();
    let mut runtime = SafeBatchRuntime::open(store.root(), "old").unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    let loaded = batch::status(&store, "old").unwrap();
    assert_eq!(loaded.transport(), Transport::CdpChromium);
    assert_eq!(serde_json::to_value(&loaded).unwrap(), legacy);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let error = runtime
        .block_on(batch::resume(
            &store,
            "old",
            Backend::Brave(&BraveOptions {
                state_dir: workspace.path().join("no-adapter"),
                target: None,
                binding: None,
                wait: Duration::from_secs(1),
                limits: Limits::default(),
                close_tab: false,
            }),
            10,
        ))
        .err()
        .unwrap();
    assert_eq!(provider_code(&error), "transport_mismatch");
    assert_eq!(error.details["batch_transport"], "cdp_chromium");
    assert!(
        error.details["next_action"]
            .as_str()
            .unwrap()
            .starts_with("pronunciation-assets forvo batch-resume")
    );
    // Отмена работает без браузера и сохраняет версию 1.
    let cancelled = batch::cancel(&store, "old").unwrap();
    assert_eq!(cancelled.schema_version, 1);
    assert!(cancelled.transport.is_none());
    // Несогласованные сочетания версии и транспорта отвергаются.
    use crate::batch_runtime::RuntimeBatchState;
    let mut v1_with = loaded;
    v1_with.transport = Some(Transport::BraveExtension);
    assert!(v1_with.validate().is_err());
    v1_with.schema_version = 2;
    assert!(v1_with.validate().is_ok());
    v1_with.transport = None;
    assert!(v1_with.validate().is_err());
}
