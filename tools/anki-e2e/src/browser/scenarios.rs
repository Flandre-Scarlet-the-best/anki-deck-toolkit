//! Сценарии браузерного набора. Каждый запускает собственный тестовый Chromium
//! и проверяет поведение: ответы настоящего расширения и bridge, фактические
//! файлы каталога загрузок и их SHA-256, а не наличие строк в коде.

use super::chrome::{LaunchOptions, TestBrowser, download_files};
use super::fixture::{self, Behavior, Gate};
use super::{Ctx, Outcome};
use crate::report::Status;
use crate::util::sha256_hex;
use asset_store::AssetError;
use asset_store::forvo::brave::acceptance::{self, Connection, SmokeOptions};
use asset_store::forvo::brave::download::{self, Limits, Progress, StatusFacts};
use asset_store::forvo::brave::{GROUP_TITLE, Platform};
use asset_store::forvo::{Candidate, provider_code};
use asset_store::pronunciation::Pronunciation;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCENARIO_LIMIT: Duration = Duration::from_secs(300);

pub struct Env<'a> {
    pub ctx: &'a Ctx,
    pub dir: PathBuf,
    pub browser: TestBrowser,
    pub conn: Option<Connection>,
    pub out: Outcome,
}

pub(super) type Result<T> = std::result::Result<T, String>;

pub(super) fn err(e: AssetError) -> String {
    format!("{}: {}", provider_code(&e), e.message)
}

pub(super) fn code<T>(result: &std::result::Result<T, AssetError>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(e) => provider_code(e).to_owned(),
    }
}

fn request(word: &str) -> Pronunciation {
    let reading = match word {
        fixture::WORD => fixture::READING,
        fixture::WORD_DOM_CHANGED => "べつご",
        fixture::WORD_OTHER_LANGUAGE => "えいご",
        fixture::WORD_PHRASES => "くご",
        fixture::WORD_LOGIN => "みにんしょう",
        fixture::WORD_MISMATCHED_BUTTON => "ごき",
        _ => "ごさき",
    };
    Pronunciation::new(word, reading).expect("корректный запрос fixture")
}

fn word_url(word: &str) -> String {
    format!("https://forvo.com/word/{}/", fixture::encode(word))
}

async fn wait_true(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

pub async fn run(ctx: &Ctx, name: &str) -> (Status, Option<String>, Outcome) {
    let dir = ctx.run_dir.join("scenarios").join(name);
    // Новый профиль сценария — новый экземпляр расширения: закрепление прошлого
    // сценария снимается явно (как `setup --repin` у пользователя).
    if let Err(e) = asset_store::forvo::brave::setup(&asset_store::forvo::brave::SetupOptions {
        repin: true,
        ..ctx.setup.clone()
    }) {
        return (Status::Error, Some(err(e)), Outcome::default());
    }
    let browser = match TestBrowser::launch(LaunchOptions {
        executable: &ctx.chrome,
        dir: &dir,
        extensions: std::slice::from_ref(&ctx.extension_dir),
        loader: ctx.loader,
        fixture_port: ctx.fixture.port,
        prompt_for_download: name == "save_as_prompt",
        restore_session: name == "browser_restart",
    })
    .await
    {
        Ok(browser) => browser,
        Err(e) => return (Status::Error, Some(e), Outcome::default()),
    };
    let flags = browser.flags.clone();
    let mut env = Env {
        ctx,
        dir,
        browser,
        conn: None,
        out: Outcome::default(),
    };
    if name.starts_with("cli_") && ctx.cli.is_none() {
        let _ = env.browser.shutdown().await;
        return (
            Status::Blocked,
            Some("нет pronunciation-assets: cargo build -p asset-store --bin pronunciation-assets или --cli".into()),
            Outcome::default(),
        );
    }
    let mut interrupt = ctx.interrupt.clone();
    let result = tokio::select! {
        result = tokio::time::timeout(SCENARIO_LIMIT, body(name, &mut env)) => match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err((Status::Fail, e)),
            Err(_) => Err((Status::Fail, format!("сценарий не завершился за {} с", SCENARIO_LIMIT.as_secs()))),
        },
        _ = wait_true(&mut interrupt) => Err((Status::Interrupted, "прервано пользователем".to_owned())),
    };
    let Env {
        browser,
        conn,
        mut out,
        dir,
        ..
    } = env;
    if let Some(mut conn) = conn {
        conn.bridge.disconnect().await;
    }
    out.evidence.insert("chromium_flags".into(), json!(flags));
    out.evidence
        .insert("browser_shutdown".into(), browser.shutdown().await);
    out.evidence
        .insert("downloads_after".into(), files_json(&dir.join("downloads")));
    match result {
        Ok(()) => {
            out.completed = true;
            let status = out.status();
            let reason = (status != Status::Pass).then(|| "проверка не выполнена".to_owned());
            (status, reason, out)
        }
        Err((status, reason)) => (status, Some(reason), out),
    }
}

async fn body(name: &str, env: &mut Env<'_>) -> Result<()> {
    match name {
        "extension_bridge_tab" => extension_bridge_tab(env).await,
        "auth_and_candidates" => auth_and_candidates(env).await,
        "full_download" => full_download(env).await,
        "concurrent_downloads" => concurrent_downloads(env).await,
        "download_failures" => download_failures(env).await,
        "service_worker_restart" => service_worker_restart(env).await,
        "bridge_reconnect" => bridge_reconnect(env).await,
        "own_tab_closed" => own_tab_closed(env).await,
        "interrupt" => interrupt(env).await,
        "save_as_prompt" => save_as_prompt(env).await,
        "browser_restart" => browser_restart(env).await,
        "cli_fetch" => super::cli_scenarios::cli_fetch(env).await,
        "cli_batch" => super::cli_scenarios::cli_batch(env).await,
        "cli_batch_recovery" => super::cli_scenarios::cli_batch_recovery(env).await,
        "cli_foreign_downloads" => super::cli_scenarios::cli_foreign_downloads(env).await,
        "adapter_doctor" => super::target_scenarios::adapter_doctor(env).await,
        "extension_update" => super::target_scenarios::extension_update(env).await,
        "target_isolation" => super::target_scenarios::target_isolation(env).await,
        _ => Err(format!("нет сценария {name}")),
    }
}

// --- Помощники -------------------------------------------------------------

pub(super) fn files_json(dir: &Path) -> Value {
    json!(
        download_files(dir)
            .into_iter()
            .map(|(name, size, sha)| json!({"name":name,"bytes":size,"sha256":sha}))
            .collect::<Vec<_>>()
    )
}

/// Ждёт, пока в каталоге загрузок не останется незавершённых файлов и появятся ожидаемые имена.
pub(super) async fn settle(
    dir: &Path,
    names: &[&str],
    timeout: Duration,
) -> Vec<(String, u64, String)> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let files = download_files(dir);
        let partial = files
            .iter()
            .any(|(n, _, _)| n.ends_with(".crdownload") || n.starts_with(".org.chromium"));
        let present = names
            .iter()
            .all(|want| files.iter().any(|(n, _, _)| n == want));
        if (!partial && present) || tokio::time::Instant::now() >= deadline {
            return files;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) fn sha_of(files: &[(String, u64, String)], name: &str) -> Option<String> {
    files
        .iter()
        .find(|(n, _, _)| n == name)
        .map(|(_, _, s)| s.clone())
}

impl Env<'_> {
    pub(super) async fn connect(&mut self) -> Result<()> {
        let conn = acceptance::connect(
            &self.ctx.state_dir,
            Some(&self.ctx.target),
            Duration::from_secs(30),
        )
        .await
        .map_err(err)?;
        self.conn = Some(conn);
        Ok(())
    }
    fn conn(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("сценарий подключил bridge")
    }
    pub(super) async fn call(
        &mut self,
        op: &str,
        args: Value,
    ) -> std::result::Result<Value, AssetError> {
        self.conn()
            .bridge
            .call(op, args, Duration::from_secs(20))
            .await
    }
    pub(super) fn downloads(&self) -> PathBuf {
        self.browser.downloads.clone()
    }
    fn sha_a(&self) -> String {
        sha256_hex(&self.ctx.fixture.mp3_a)
    }
    fn sha_b(&self) -> String {
        sha256_hex(&self.ctx.fixture.mp3_b)
    }
    pub(super) async fn select(&mut self, word: &str, record: &str) -> Result<Candidate> {
        let conn = self.conn.as_mut().expect("bridge");
        acceptance::open_word(&mut conn.bridge, word)
            .await
            .map_err(err)?;
        let ranked = acceptance::candidates(&mut conn.bridge, &request(word))
            .await
            .map_err(err)?;
        ranked
            .into_iter()
            .find(|c| c.record_id == record)
            .ok_or_else(|| format!("нет кандидата {record}"))
    }
    async fn acquire(
        &mut self,
        word: &str,
        candidate: &Candidate,
        limits: Limits,
    ) -> (String, std::result::Result<download::Acquired, AssetError>) {
        let op = download::new_operation_id().expect("op_id");
        let conn = self.conn.as_mut().expect("bridge");
        let result = download::acquire(
            &mut conn.bridge,
            Platform::Linux,
            &request(word),
            candidate,
            &op,
            &limits,
            &mut None,
        )
        .await;
        (op, result)
    }
    pub(super) async fn release(&mut self, op: &str) -> Value {
        let conn = self.conn.as_mut().expect("bridge");
        download::release(&mut conn.bridge, op, false)
            .await
            .unwrap_or_else(|e| json!({"error":provider_code(&e)}))
    }
    async fn status(&mut self, op: &str) -> Result<StatusFacts> {
        let value = self
            .call("download_status", json!({"op_id":op}))
            .await
            .map_err(err)?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
    pub(super) async fn poll(
        &mut self,
        op: &str,
        timeout: Duration,
        done: impl Fn(&StatusFacts) -> bool,
    ) -> Result<StatusFacts> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let facts = self.status(op).await?;
            if done(&facts) || tokio::time::Instant::now() >= deadline {
                return Ok(facts);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    pub(super) async fn begin(&mut self, op: &str, word: &str, record: &str) -> Result<Value> {
        self.call(
            "download_begin",
            json!({"op_id":op,"surface":word,"record_id":record}),
        )
        .await
        .map_err(err)
    }
    /// В конце сценария у расширения нет перехватчиков и незавершённых операций.
    pub(super) async fn residual_clean(&mut self) -> Result<()> {
        let ping = self.call("ping", json!({})).await.map_err(err)?;
        let clean = ping["listeners"]["determining_filename"] == false
            && ping["listeners"]["web_request"] == false
            && ping["operation"].is_null();
        self.out.check(
            "после операции нет onDeterminingFilename, наблюдателя webRequest и незавершённой операции",
            clean,
            json!({"listeners":ping["listeners"],"operation":ping["operation"]}),
        );
        Ok(())
    }
}

fn smoke_options(
    env: &Env,
    keep: bool,
    interrupt: Option<tokio::sync::watch::Receiver<bool>>,
) -> SmokeOptions {
    SmokeOptions {
        state_dir: env.ctx.state_dir.clone(),
        target: Some(env.ctx.target.clone()),
        wait: Duration::from_secs(30),
        surface: fixture::WORD.into(),
        reading: fixture::READING.into(),
        probe_downloads_api: false,
        close_tab: false,
        keep_download: keep,
        limits: Limits::default(),
        interrupt,
    }
}

fn step<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|s| s["name"] == name))
        .unwrap_or(&Value::Null)
}

// --- Сценарии --------------------------------------------------------------

async fn extension_bridge_tab(env: &mut Env<'_>) -> Result<()> {
    let _home = env.browser.user_tab("https://forvo.com/").await?;
    let _blank = env.browser.user_tab("about:blank").await?;
    let pages_before = env.browser.page_urls().await?;
    env.connect().await?;
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(env.ctx.extension_dir.join("manifest.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let peer = env.conn().peer.clone();
    env.out.check(
        "настоящее MV3-расширение прошло HMAC-сопряжение с bridge",
        peer.version == manifest["version"],
        json!({"extension_id":peer.extension_id,"version":peer.version}),
    );
    env.residual_clean().await?;
    let unknown = env.call("no_such_operation", json!({})).await;
    let invalid = env
        .call(
            "download_begin",
            json!({"op_id":"x","surface":"a","record_id":"1"}),
        )
        .await;
    let foreign = env
        .call(
            "navigate",
            json!({"url":"https://example.com/","timeout_ms":5000}),
        )
        .await;
    env.out.check(
        "командный контракт отвергает неизвестную операцию, неверные аргументы и чужой origin",
        code(&unknown) == "unknown_operation"
            && code(&invalid) == "invalid_arguments"
            && code(&foreign) == "url_not_allowed",
        json!({"unknown":code(&unknown),"invalid":code(&invalid),"foreign_url":code(&foreign)}),
    );
    let before = env.call("snapshot", json!({})).await.map_err(err)?;
    let opened = env.call("open", json!({})).await.map_err(err)?;
    env.out.check(
        "собственная вкладка создана неактивной в свёрнутой группе Anki Deck Toolkit",
        opened["created"] == true
            && opened["active"] == false
            && opened["group_title"] == GROUP_TITLE
            && opened["group_collapsed"] == true,
        opened.clone(),
    );
    let conn = env.conn.as_mut().expect("bridge");
    acceptance::open_word(&mut conn.bridge, fixture::WORD)
        .await
        .map_err(err)?;
    let again = env.call("open", json!({})).await.map_err(err)?;
    env.out.check(
        "повторное открытие переиспользует ту же собственную вкладку",
        again["created"] == false && again["tab_id"] == opened["tab_id"],
        json!({"first":opened["tab_id"],"again":again["tab_id"]}),
    );
    let after = env.call("snapshot", json!({})).await.map_err(err)?;
    let pages_after = env.browser.page_urls().await?;
    let structure = |v: &Value| json!({"tabs":v["tabs"],"groups":v["groups"]});
    let kept = pages_before.iter().all(|url| pages_after.contains(url));
    env.out.check(
        "вкладки и группы пользователя не изменены, добавлена только собственная вкладка",
        structure(&before) == structure(&after)
            && kept
            && pages_after.len() == pages_before.len() + 1,
        json!({"user_pages_before":pages_before,"pages_after":pages_after.len()}),
    );
    Ok(())
}

async fn auth_and_candidates(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let ranked = {
        let conn = env.conn.as_mut().expect("bridge");
        acceptance::open_word(&mut conn.bridge, fixture::WORD)
            .await
            .map_err(err)?;
        acceptance::candidates(&mut conn.bridge, &request(fixture::WORD))
            .await
            .map_err(err)?
    };
    let ids: Vec<_> = ranked.iter().map(|c| c.record_id.clone()).collect();
    env.out.check(
        "авторизованная страница: японские записи и кнопки сопоставлены, ранжирование по голосам",
        ids == [fixture::RECORD_A, fixture::RECORD_B]
            && ranked.iter().all(|c| c.download_available),
        json!({"ranked":ids}),
    );
    let op = download::new_operation_id().map_err(err)?;
    let wrong = env.begin(&op, fixture::WORD, "9999999").await?;
    let ping = env.call("ping", json!({})).await.map_err(err)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.out.check(
        "неверный record_id: кнопка не нажата, операция не создана, загрузок нет",
        wrong["code"] == "candidate_disappeared"
            && ping["operation"].is_null()
            && download_files(&env.downloads()).is_empty(),
        json!({"code":wrong["code"],"operation":ping["operation"]}),
    );
    let mut codes = serde_json::Map::new();
    for word in [
        fixture::WORD_LOGIN,
        fixture::WORD_DOM_CHANGED,
        fixture::WORD_OTHER_LANGUAGE,
        fixture::WORD_PHRASES,
        fixture::WORD_MISMATCHED_BUTTON,
    ] {
        let conn = env.conn.as_mut().expect("bridge");
        let result = match acceptance::open_word(&mut conn.bridge, word).await {
            Ok(_) => acceptance::candidates(&mut conn.bridge, &request(word))
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        };
        codes.insert(word.into(), json!(code(&result)));
    }
    let expected = json!({
        fixture::WORD_LOGIN: "login_required",
        fixture::WORD_DOM_CHANGED: "dom_changed",
        fixture::WORD_OTHER_LANGUAGE: "japanese_pronunciation_not_found",
        fixture::WORD_PHRASES: "dom_changed",
        fixture::WORD_MISMATCHED_BUTTON: "page_identity_mismatch",
    });
    env.out.check(
        "вход, изменённый DOM, другой язык, раздел phrases и чужая кнопка дают типизированные отказы",
        Value::Object(codes.clone()) == expected,
        json!({"observed":codes,"expected":expected}),
    );
    env.residual_clean().await
}

async fn full_download(env: &mut Env<'_>) -> Result<()> {
    let report = acceptance::smoke(smoke_options(env, false, None))
        .await
        .map_err(err)?;
    let report = json!(report);
    let required = [
        "transport",
        "own_tab_group",
        "forvo_page_auth",
        "candidates",
        "selection",
        "site_download",
        "mp3_bytes",
        "download_cleanup",
        "user_tabs_untouched",
        "no_residual_listeners",
    ];
    let failed: Vec<_> = required
        .iter()
        .filter(|n| step(&report, n)["status"] != "PASS")
        .collect();
    env.out.check(
        "smoke (путь CLI) прошёл: клик → загрузка → владение → complete → байты → probe → очистка",
        failed.is_empty(),
        json!({"not_passed":failed.iter().map(|n| step(&report, n)).collect::<Vec<_>>()}),
    );
    let mp3 = &step(&report, "mp3_bytes")["evidence"];
    let sha_a = env.sha_a();
    env.out.check(
        "SHA-256 и длина совпадают с оригинальными байтами выбранной записи, probe_mp3 = mp3",
        mp3["sha256"] == sha_a
            && mp3["record_id"] == fixture::RECORD_A
            && mp3["probe"]["codec"] == "mp3",
        json!({"sha256":mp3["sha256"],"expected":sha_a,"byte_length":mp3["byte_length"]}),
    );
    let release = &step(&report, "download_cleanup")["evidence"]["release"];
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.out.check(
        "собственный файл и запись истории удалены только после проверки",
        release["bound"] == true
            && release["removed_file"] == true
            && release["erased"] == true
            && download_files(&env.downloads()).is_empty(),
        json!({"release":release,"downloads":files_json(&env.downloads())}),
    );
    let kept = json!(
        acceptance::smoke(smoke_options(env, true, None))
            .await
            .map_err(err)?
    );
    let files = settle(&env.downloads(), &[], Duration::from_secs(5)).await;
    env.out.check(
        "--keep-download оставляет проверенный файл в загрузках неизменённым",
        step(&kept, "download_cleanup")["evidence"]["release"]["removed_file"] == false
            && files.len() == 1
            && files[0].2 == sha_a,
        json!({"downloads":files_json(&env.downloads()),"site_download":step(&kept, "site_download"),"cleanup":step(&kept, "download_cleanup")}),
    );
    Ok(())
}

async fn concurrent_downloads(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    // Каждая загрузка пользователя — из своей вкладки: Chromium без жеста
    // пользователя разрешает вкладке только первую загрузку.
    let user = env.browser.user_tab(&word_url(fixture::WORD)).await?;
    let user_other_site = env.browser.user_tab("https://forvo.com/").await?;
    let user_same = env.browser.user_tab(&word_url(fixture::WORD)).await?;
    let downloads = env.downloads();
    let user_b = "pronunciation_ja_5036880.mp3";
    // 1. Собственная загрузка и параллельные загрузки пользователя: другая запись Forvo и чужой сайт.
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let op = download::new_operation_id().map_err(err)?;
    let fixture = env.ctx.fixture.clone();
    let side = async {
        if !fixture
            .wait_request(fixture::RECORD_A, 1, Duration::from_secs(10))
            .await
        {
            return Err("собственный запрос не пришёл".to_owned());
        }
        TestBrowser::click(&user, &format!("a[data-p4=\"{}\"]", fixture::RECORD_B)).await?;
        TestBrowser::user_foreign_download(&user_other_site).await?;
        settle(
            &downloads,
            &[user_b, "user-notes.bin"],
            Duration::from_secs(15),
        )
        .await;
        gate.open();
        Ok(())
    };
    let conn = env.conn.as_mut().expect("bridge");
    let request = request(fixture::WORD);
    let limits = Limits::default();
    let mut no_interrupt = None;
    let (acquired, side) = tokio::join!(
        download::acquire(
            &mut conn.bridge,
            Platform::Linux,
            &request,
            &candidate,
            &op,
            &limits,
            &mut no_interrupt
        ),
        side
    );
    side?;
    let sha_a = env.sha_a();
    let own_ok = acquired.as_ref().is_ok_and(|a| a.sha256 == sha_a);
    env.out.check(
        "собственная загрузка выбранной записи доведена до проверенных байтов при параллельных загрузках пользователя",
        own_ok,
        json!({"result":code(&acquired),"download_id":acquired.as_ref().ok().map(|a| a.item.id)}),
    );
    let release = env.release(&op).await;
    let files = settle(
        &downloads,
        &[user_b, "user-notes.bin"],
        Duration::from_secs(10),
    )
    .await;
    let user_sha = sha256_hex(&env.ctx.fixture.user_file);
    env.out.check(
        "загрузки пользователя (другая запись Forvo и другой сайт) завершились неизменёнными; удалён только собственный файл",
        release["removed_file"] == true
            && files.len() == 2
            && sha_of(&files, user_b) == Some(env.sha_b())
            && sha_of(&files, "user-notes.bin") == Some(user_sha),
        json!({"release":release,"downloads":files_json(&downloads)}),
    );
    // 2. Пользователь скачивает ту же запись, пока собственный ответ ещё не начался: владение неоднозначно.
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let before = env.ctx.fixture.requests_for(fixture::RECORD_A);
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let op = download::new_operation_id().map_err(err)?;
    let user_a = "pronunciation_ja_3661006.mp3";
    let side = async {
        if !fixture
            .wait_request(fixture::RECORD_A, before + 1, Duration::from_secs(10))
            .await
        {
            return Err("собственный запрос не пришёл".to_owned());
        }
        TestBrowser::click(&user_same, &format!("a[data-p4=\"{}\"]", fixture::RECORD_A)).await?;
        settle(&downloads, &[user_a], Duration::from_secs(15)).await;
        Ok(())
    };
    let conn = env.conn.as_mut().expect("bridge");
    let (conflict, side) = tokio::join!(
        download::acquire(
            &mut conn.bridge,
            Platform::Linux,
            &request,
            &candidate,
            &op,
            &limits,
            &mut no_interrupt
        ),
        side
    );
    side?;
    let status = conflict
        .as_ref()
        .err()
        .map(|e| e.details["status"].clone())
        .unwrap_or(Value::Null);
    env.out.check(
        "параллельная загрузка той же записи из другой вкладки → download_conflict, операция остановлена",
        code(&conflict) == "download_conflict" && status["bound_id"].is_null(),
        json!({"result":code(&conflict),"foreign_requests":status["foreign_requests"],"matching_downloads":status["matching_downloads"]}),
    );
    gate.open();
    let late = "pronunciation_ja_3661006 (1).mp3";
    settle(&downloads, &[user_a, late], Duration::from_secs(15)).await;
    let release = env.release(&op).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let files = download_files(&downloads);
    env.out.check(
        "несвязанная операция ничего не удалила: файл пользователя и поздняя загрузка на месте и неизменны",
        release["bound"] == false
            && release["removed_file"] == false
            && sha_of(&files, user_a) == Some(sha_a.clone())
            && sha_of(&files, late) == Some(sha_a.clone())
            && sha_of(&files, user_b) == Some(env.sha_b()),
        json!({"release":release,"downloads":files_json(&downloads)}),
    );
    env.residual_clean().await
}

async fn download_failures(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let downloads = env.downloads();
    let short = Limits {
        start: Duration::from_secs(5),
        prompt: Duration::from_secs(8),
        total: Duration::from_secs(5),
    };
    // Таймаут: заголовки и половина тела, затем тишина.
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let (op, timeout) = env.acquire(fixture::WORD, &candidate, short).await;
    let partial = download_files(&downloads);
    let status = env.status(&op).await?;
    env.out.check(
        "незавершённая загрузка (.crdownload, in_progress) не принимается; по пределу — download_timeout",
        code(&timeout) == "download_timeout"
            && status.item.as_ref().is_some_and(|i| i.state == "in_progress")
            && partial.iter().all(|(n, _, _)| n.ends_with(".crdownload")),
        json!({"result":code(&timeout),"files_during":partial.iter().map(|f| &f.0).collect::<Vec<_>>()}),
    );
    let release = env.release(&op).await;
    drop(gate);
    tokio::time::sleep(Duration::from_millis(800)).await;
    env.out.check(
        "собственная незавершённая загрузка отменена и стёрта, частичный файл не остался",
        release["canceled"] == true
            && release["erased"] == true
            && download_files(&downloads).is_empty(),
        json!({"release":release,"downloads":files_json(&downloads)}),
    );
    // Обрыв соединения сервера.
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    env.ctx.fixture.set(fixture::RECORD_A, Behavior::Abort);
    let (op, aborted) = env
        .acquire(
            fixture::WORD,
            &candidate,
            Limits {
                total: Duration::from_secs(10),
                ..short
            },
        )
        .await;
    let release = env.release(&op).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.out.check(
        "обрыв ответа → download_interrupted, запись стёрта, файла нет",
        code(&aborted) == "download_interrupted"
            && release["erased"] == true
            && download_files(&downloads).is_empty(),
        json!({"result":code(&aborted),"release":release}),
    );
    // Содержимое не MP3.
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    env.ctx.fixture.set(fixture::RECORD_A, Behavior::Garbage);
    let (op, garbage) = env.acquire(fixture::WORD, &candidate, short).await;
    let release = env.release(&op).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.out.check(
        "завершённая собственная загрузка с не-MP3 → probe_failed; файл удалён как собственный",
        code(&garbage) == "probe_failed"
            && release["removed_file"] == true
            && download_files(&downloads).is_empty(),
        json!({"result":code(&garbage),"release":release}),
    );
    // Кнопка ведёт на адрес другой записи: запрос своей вкладки не совпадает с выбранной записью.
    let wrong = env
        .select(fixture::WORD_WRONG_HREF, fixture::RECORD_WRONG_HREF)
        .await?;
    let (op, mismatch) = env.acquire(fixture::WORD_WRONG_HREF, &wrong, short).await;
    let stray = "pronunciation_ja_5036880.mp3";
    let files = settle(&downloads, &[stray], Duration::from_secs(10)).await;
    let release = env.release(&op).await;
    env.out.check(
        "неверный адрес загрузки → download_not_started; посторонняя загрузка не усыновлена и не тронута",
        code(&mismatch) == "download_not_started" && release["bound"] == false && sha_of(&files, stray) == Some(env.sha_b()),
        json!({"result":code(&mismatch),"release":release,"downloads":files_json(&downloads)}),
    );
    // Неверный download ID и чужая операция.
    let mut refused = Vec::new();
    for id in 1..=12 {
        refused.push(code(
            &env.call("download_cleanup", json!({"download_id":id}))
                .await,
        ));
    }
    let foreign_op = download::new_operation_id().map_err(err)?;
    let unknown = env
        .call(
            "download_release",
            json!({"op_id":foreign_op,"action":"remove"}),
        )
        .await
        .map_err(err)?;
    let files = download_files(&downloads);
    env.out.check(
        "очистка по чужому download ID и чужой операции отвергается, файлы не тронуты",
        refused.iter().all(|c| c == "download_not_owned")
            && unknown["released"] == false
            && sha_of(&files, stray) == Some(env.sha_b()),
        json!({"cleanup_codes":refused,"foreign_release":unknown}),
    );
    env.residual_clean().await
}

async fn service_worker_restart(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let extension_id = env.conn().peer.extension_id.clone();
    let downloads = env.downloads();
    let fixture = env.ctx.fixture.clone();
    let request = request(fixture::WORD);
    let limits = Limits::default();
    // 1. Рестарт до связывания: загрузка ещё не появилась.
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let op = download::new_operation_id().map_err(err)?;
    let browser = &env.browser;
    let side = async {
        if !fixture
            .wait_request(fixture::RECORD_A, 1, Duration::from_secs(10))
            .await
        {
            return Err("собственный запрос не пришёл".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let stopped = browser.stop_extension_worker(&extension_id).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        gate.open();
        Ok(stopped)
    };
    let conn = env.conn.as_mut().expect("bridge");
    let mut no_interrupt = None;
    let (unbound, stopped) = tokio::join!(
        download::acquire(
            &mut conn.bridge,
            Platform::Linux,
            &request,
            &candidate,
            &op,
            &limits,
            &mut no_interrupt
        ),
        side
    );
    let stopped = stopped?;
    env.out.check(
        "рестарт service worker до связывания → ownership_unproven (fail closed)",
        stopped >= 1 && code(&unbound) == "ownership_unproven",
        json!({"workers_stopped":stopped,"result":code(&unbound)}),
    );
    let name = "pronunciation_ja_3661006.mp3";
    let files = settle(&downloads, &[name], Duration::from_secs(15)).await;
    let release = env.release(&op).await;
    env.out.check(
        "поздняя загрузка после рестарта не усыновлена и не удалена",
        release["bound"] == false && sha_of(&files, name) == Some(env.sha_a()),
        json!({"release":release,"downloads":files_json(&downloads)}),
    );
    // 2. Рестарт после связывания: состояние операции переживает выгрузку service worker.
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let before = fixture.requests_for(fixture::RECORD_A);
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let op = download::new_operation_id().map_err(err)?;
    let browser = &env.browser;
    let side = async {
        if !fixture
            .wait_request(fixture::RECORD_A, before + 1, Duration::from_secs(10))
            .await
        {
            return Err("собственный запрос не пришёл".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let stopped = browser.stop_extension_worker(&extension_id).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        gate.open();
        Ok(stopped)
    };
    let conn = env.conn.as_mut().expect("bridge");
    let (bound, stopped) = tokio::join!(
        download::acquire(
            &mut conn.bridge,
            Platform::Linux,
            &request,
            &candidate,
            &op,
            &limits,
            &mut no_interrupt
        ),
        side
    );
    let stopped = stopped?;
    env.out.check(
        "рестарт service worker после связывания: переподключение bridge, операция доведена до проверенных байтов",
        stopped >= 1 && bound.as_ref().is_ok_and(|a| a.sha256 == env.sha_a() && a.reconnects >= 1),
        json!({"workers_stopped":stopped,"result":code(&bound),"reconnects":bound.as_ref().ok().map(|a| a.reconnects)}),
    );
    let release = env.release(&op).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let files = download_files(&downloads);
    env.out.check(
        "после восстановления удалён только связанный собственный файл, ранний файл не тронут",
        release["removed_file"] == true
            && files.len() == 1
            && sha_of(&files, name) == Some(env.sha_a()),
        json!({"release":release,"downloads":files_json(&downloads)}),
    );
    env.residual_clean().await
}

async fn bridge_reconnect(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let op = download::new_operation_id().map_err(err)?;
    let begun = env.begin(&op, fixture::WORD, &candidate.record_id).await?;
    let bound = env
        .poll(&op, Duration::from_secs(10), |f| f.bound_id.is_some())
        .await?;
    let started = tokio::time::Instant::now();
    let conn = env.conn.as_mut().expect("bridge");
    conn.bridge.disconnect().await;
    conn.bridge
        .accept(tokio::time::Instant::now() + Duration::from_secs(30))
        .await
        .map_err(err)?;
    let reconnect_ms = started.elapsed().as_millis() as u64;
    let after = env.status(&op).await?;
    env.out.check(
        "разрыв и восстановление bridge не теряют связанную операцию",
        begun["code"] == "activated" && bound.bound_id.is_some() && after.bound_id == bound.bound_id,
        json!({"reconnect_ms":reconnect_ms,"bound_before":bound.bound_id,"bound_after":after.bound_id}),
    );
    gate.open();
    let request = request(fixture::WORD);
    let limits = Limits::default();
    let done = env
        .poll(&op, Duration::from_secs(20), |f| {
            !matches!(
                download::classify(f, &request, &candidate.record_id, &limits),
                Progress::Waiting(_)
            )
        })
        .await?;
    let verified = match download::classify(&done, &request, &candidate.record_id, &limits) {
        Progress::Complete(item) => download::verify_file(Platform::Linux, &item)
            .map(|(_, b)| sha256_hex(&b))
            .map_err(err),
        other => Err(format!("{other:?}")),
    };
    env.out.check(
        "после переподключения загрузка завершена и байты совпадают с выбранной записью",
        verified.as_deref() == Ok(env.sha_a().as_str()),
        json!({"verified":verified}),
    );
    let release = env.release(&op).await;
    env.out.check(
        "очистка после переподключения затронула только собственную загрузку",
        release["removed_file"] == true && release["erased"] == true,
        release,
    );
    env.residual_clean().await
}

async fn own_tab_closed(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let op = download::new_operation_id().map_err(err)?;
    env.begin(&op, fixture::WORD, &candidate.record_id).await?;
    if !env
        .ctx
        .fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(10))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    let closed = env.call("close", json!({})).await.map_err(err)?;
    let request = request(fixture::WORD);
    let limits = Limits::default();
    let facts = env
        .poll(&op, Duration::from_secs(10), |f| {
            matches!(
                download::classify(f, &request, &candidate.record_id, &limits),
                Progress::Failed(_)
            )
        })
        .await?;
    let progress = download::classify(&facts, &request, &candidate.record_id, &limits);
    env.out.check(
        "закрытие собственной вкладки до начала загрузки → own_tab_closed",
        closed["closed"] == true && progress == Progress::Failed("own_tab_closed"),
        json!({"progress":format!("{progress:?}")}),
    );
    gate.open();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let files_before = download_files(&env.downloads());
    let release = env.release(&op).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let files_after = download_files(&env.downloads());
    env.out.check(
        "освобождение несвязанной операции ничего не удалило",
        release["bound"] == false && files_before == files_after,
        json!({"release":release,"downloads":files_json(&env.downloads())}),
    );
    env.residual_clean().await
}

async fn interrupt(env: &mut Env<'_>) -> Result<()> {
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let (tx, irx) = tokio::sync::watch::channel(false);
    let options = smoke_options(env, false, Some(irx));
    let fixture = env.ctx.fixture.clone();
    let side = async {
        if fixture
            .wait_request(fixture::RECORD_A, 1, Duration::from_secs(30))
            .await
        {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        let _ = tx.send(true);
    };
    let (report, ()) = tokio::join!(acceptance::smoke(options), side);
    let report = report.map_err(err)?;
    let value = json!(report);
    let site = step(&value, "site_download");
    let cleanup = step(&value, "download_cleanup");
    let exit = acceptance::report_error(report).err();
    env.out.check(
        "прерывание (как Ctrl+C) → INTERRUPTED, итог CLI с кодом interrupted",
        site["status"] == "INTERRUPTED"
            && exit
                .as_ref()
                .is_some_and(|e| e.code == asset_store::ErrorCode::Interrupted),
        json!({"site_download":site["status"],"exit_code":exit.map(|e| e.code.as_str())}),
    );
    drop(gate);
    tokio::time::sleep(Duration::from_millis(800)).await;
    env.out.check(
        "при прерывании собственная загрузка отменена и стёрта, файлов не осталось",
        cleanup["status"] == "PASS"
            && cleanup["evidence"]["release"]["canceled"] == true
            && download_files(&env.downloads()).is_empty(),
        json!({"cleanup":cleanup["evidence"],"downloads":files_json(&env.downloads())}),
    );
    env.connect().await?;
    env.residual_clean().await
}

async fn save_as_prompt(env: &mut Env<'_>) -> Result<()> {
    let report = acceptance::smoke(smoke_options(env, false, None))
        .await
        .map_err(err)?;
    let value = json!(report);
    let site = step(&value, "site_download");
    let exit = acceptance::report_error(report).err();
    env.out.check(
        "профиль с «Спрашивать, куда сохранять» → BLOCKED save_as_required (Edge: save_as_suspected) с инструкцией",
        site["status"] == "BLOCKED"
            && (site["evidence"]["code"] == "save_as_required"
                || env.ctx.kind == asset_store::forvo::brave::BrowserKind::Edge
                    && site["evidence"]["code"] == "save_as_suspected")
            && exit
                .as_ref()
                .is_some_and(|e| provider_code(e) == "acceptance_blocked"),
        json!({"site_download":site}),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let cleanup = step(&value, "download_cleanup");
    env.out.check(
        "собственная загрузка без выбранного пути стёрта, файлов не осталось",
        cleanup["status"] == "PASS"
            && download_files(&env.downloads())
                .iter()
                .all(|(n, _, _)| !n.ends_with(".mp3")),
        json!({"cleanup":cleanup["evidence"],"downloads":files_json(&env.downloads())}),
    );
    env.connect().await?;
    env.residual_clean().await
}

/// Перезапуск браузера с восстановлением сессии: восстановленная группа адаптера
/// закрывается перед первой командой, вкладка пользователя на forvo.com остаётся.
async fn browser_restart(env: &mut Env<'_>) -> Result<()> {
    env.connect().await?;
    let opened = env.call("open", json!({})).await.map_err(err)?;
    let user_url = word_url(fixture::WORD);
    let _user = env.browser.user_tab(&user_url).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    if let Some(mut conn) = env.conn.take() {
        conn.bridge.disconnect().await;
    }
    let stopped = env
        .browser
        .relaunch(LaunchOptions {
            executable: &env.ctx.chrome,
            dir: &env.dir,
            extensions: std::slice::from_ref(&env.ctx.extension_dir),
            loader: env.ctx.loader,
            fixture_port: env.ctx.fixture.port,
            prompt_for_download: false,
            restore_session: true,
        })
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let restored = loop {
        let pages = env.browser.page_urls().await?;
        let own = pages.iter().filter(|u| *u == "https://forvo.com/").count();
        if (own > 0 && pages.contains(&user_url)) || tokio::time::Instant::now() >= deadline {
            break pages;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    env.out.check(
        "браузер восстановил вкладку адаптера и вкладку пользователя",
        opened["created"] == true
            && restored.iter().any(|u| u == "https://forvo.com/")
            && restored.contains(&user_url),
        json!({"pages_after_restart":restored,"shutdown":stopped}),
    );
    env.connect().await?;
    let ping = env.call("ping", json!({})).await.map_err(err)?;
    let pages = env.browser.page_urls().await?;
    let snapshot = env.call("snapshot", json!({})).await.map_err(err)?;
    env.out.check(
        "первая команда закрыла только восстановленную группу адаптера",
        ping["restored_cleanup"] == json!({"groups_closed":1,"tabs_closed":1,"groups_kept":0})
            && !pages.iter().any(|u| u == "https://forvo.com/")
            && pages.contains(&user_url)
            && snapshot["groups"].as_array().is_some_and(Vec::is_empty),
        json!({"restored_cleanup":ping["restored_cleanup"],"pages":pages,"foreign_groups":snapshot["groups"]}),
    );
    let reopened = env.call("open", json!({})).await.map_err(err)?;
    let snapshot = env.call("snapshot", json!({})).await.map_err(err)?;
    env.out.check(
        "новая собственная вкладка создана в единственной группе Anki Deck Toolkit",
        reopened["created"] == true
            && reopened["group_title"] == GROUP_TITLE
            && snapshot["groups"].as_array().is_some_and(Vec::is_empty),
        json!({"open":reopened,"foreign_groups":snapshot["groups"]}),
    );
    env.residual_clean().await
}
