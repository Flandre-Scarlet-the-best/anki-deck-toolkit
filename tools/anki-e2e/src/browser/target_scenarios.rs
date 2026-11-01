//! Цели адаптера в настоящем браузере: диагностика версии, браузера и его
//! возможностей, штатное обновление установленного расширения (в том числе
//! отложенное при незавершённой загрузке) и независимость сопряжений двух
//! целей и двух профилей.

use super::chrome::{LaunchOptions, TestBrowser, download_files};
use super::fixture::{self, Behavior, Gate};
use super::scenarios::{Env, Result, code, err, files_json};
use asset_store::forvo::brave::acceptance::{self, DoctorOptions};
use asset_store::forvo::brave::targets::{self, bundled_version};
use asset_store::forvo::brave::{self, BrowserKind, Platform, SetupOptions, download};
use asset_store::forvo::provider_code;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

fn step<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|s| s["name"] == name))
        .unwrap_or(&Value::Null)
}

async fn disconnect(env: &mut Env<'_>) {
    if let Some(mut conn) = env.conn.take() {
        conn.bridge.disconnect().await;
    }
}

/// Записывает в установленный каталог manifest с другой версией (как у
/// расширения, установленного прежним CLI) и перезагружает расширение командой.
async fn install_older(env: &mut Env<'_>, version: &str) -> Result<Value> {
    let path = env.ctx.extension_dir.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    manifest["version"] = json!(version);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap())
        .map_err(|e| e.to_string())?;
    let conn = env.conn.as_mut().expect("bridge");
    conn.bridge
        .call("reload", json!({}), Duration::from_secs(5))
        .await
        .map_err(err)?;
    let peer = conn
        .bridge
        .accept(tokio::time::Instant::now() + Duration::from_secs(60))
        .await
        .map_err(err)?;
    Ok(json!({"version":peer.version,"instance_reported":peer.instance.is_some()}))
}

pub async fn adapter_doctor(env: &mut Env<'_>) -> Result<()> {
    let report = acceptance::doctor(&DoctorOptions {
        state_dir: env.ctx.state_dir.clone(),
        target: Some(env.ctx.target.clone()),
        wait: Duration::from_secs(30),
        forvo: true,
        reconnect: true,
        reload: true,
    })
    .await
    .map_err(err)?;
    let report = json!(report);
    let status = |name: &str| {
        step(&report, name)["status"]
            .as_str()
            .unwrap_or("absent")
            .to_owned()
    };
    let connected = step(&report, "extension_connected");
    env.out.check(
        "doctor: расширение подключилось из каталога цели и закрепило экземпляр",
        status("extension_connected") == "PASS"
            && connected["evidence"]["from_target_directory"] == true
            && connected["evidence"]["instance_pinned"] == true,
        connected.clone(),
    );
    env.out.check(
        "doctor: версия расширения совпадает с CLI, установка браузера подтверждена по исполняемому файлу",
        status("extension_version") == "PASS" && status("installation") == "PASS",
        json!({"extension_version":step(&report,"extension_version"),"installation":step(&report,"installation")}),
    );
    // Vivaldi по userAgentData неотличим от Chrome: честный NOT_TESTED, не PASS.
    let identity_expected = if env.ctx.kind == BrowserKind::Vivaldi {
        "NOT_TESTED"
    } else {
        "PASS"
    };
    env.out.check(
        format!(
            "doctor: идентичность браузера {} — {identity_expected}",
            env.ctx.kind.id()
        ),
        status("browser_identity") == identity_expected,
        step(&report, "browser_identity").clone(),
    );
    env.out.check(
        "doctor: браузер сам сообщил все нужные API (MV3 service worker, downloads, webRequest, tabs, tabGroups, scripting, storage, alarms, WebSocket+HMAC)",
        status("browser_capabilities") == "PASS",
        step(&report, "browser_capabilities").clone(),
    );
    let live = [
        "own_tab_group",
        "forvo_auth",
        "bridge_reconnect",
        "service_worker_restart",
        "download_listeners",
    ];
    env.out.check(
        "doctor --forvo --reconnect --reload: собственная группа, вход fixture, переподключение bridge и рестарт service worker",
        live.iter().all(|s| status(s) == "PASS"),
        json!(live.iter().map(|s| json!({"step":s,"status":status(s)})).collect::<Vec<_>>()),
    );
    env.out.check(
        "doctor: настройка «Спрашивать, куда сохранять» не выдаётся за проверенную",
        status("download_settings") == "NOT_TESTED",
        step(&report, "download_settings").clone(),
    );
    env.out.evidence.insert("doctor".into(), report.clone());
    Ok(())
}

pub async fn extension_update(env: &mut Env<'_>) -> Result<()> {
    let bundled = bundled_version();
    env.connect().await?;
    let first = env.conn.as_ref().expect("bridge").peer.clone();
    let secret_before = targets::load_target(&env.ctx.state_dir, Some(&env.ctx.target))
        .map_err(err)?
        .secret;
    // 1. Установлена старая версия; следующая команда обновляет файлы и штатно перезагружает расширение.
    let older = install_older(env, "0.2.9").await?;
    disconnect(env).await;
    let conn = acceptance::connect(
        &env.ctx.state_dir,
        Some(&env.ctx.target),
        Duration::from_secs(30),
    )
    .await
    .map_err(err)?;
    let state = targets::load_target(&env.ctx.state_dir, Some(&env.ctx.target)).map_err(err)?;
    env.out.check(
        "старое расширение обновлено без участия пользователя: файлы, reload, та же версия CLI",
        older["version"] == "0.2.9"
            && conn.update["reload"] == "reloaded"
            && conn.peer.version == bundled
            && conn.update["files_changed"] == json!(["manifest.json"]),
        json!({"installed":older,"update":conn.update}),
    );
    env.out.check(
        "обновление сохранило ID расширения, экземпляр профиля и секрет; повторная установка не нужна",
        conn.peer.extension_id == first.extension_id
            && conn.peer.instance == first.instance
            && first.instance.is_some()
            && state.secret == secret_before
            && state.instance_id == first.instance,
        json!({"extension_id":conn.peer.extension_id,"same_instance":conn.peer.instance == first.instance,
            "secret_unchanged":state.secret == secret_before}),
    );
    env.conn = Some(conn);
    // 2. Обновление во время незавершённой собственной загрузки откладывается.
    install_older(env, "0.2.8").await?;
    let candidate = env.select(fixture::WORD, fixture::RECORD_A).await?;
    let (gate, rx) = Gate::new();
    env.ctx
        .fixture
        .set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let op = download::new_operation_id().map_err(err)?;
    let begun = env.begin(&op, fixture::WORD, &candidate.record_id).await?;
    // Как production-путь: опрос до связывания, иначе освобождение по
    // контракту не трогает несвязанную загрузку.
    let bound = env
        .poll(&op, Duration::from_secs(15), |f| f.bound_id.is_some())
        .await?;
    disconnect(env).await;
    let conn = acceptance::connect(
        &env.ctx.state_dir,
        Some(&env.ctx.target),
        Duration::from_secs(30),
    )
    .await
    .map_err(err)?;
    env.out.check(
        "при незавершённой собственной загрузке reload отложен, операция не потеряна",
        begun["code"] == "activated"
            && bound.bound_id.is_some()
            && conn.update["reload"] == "deferred_operation_pending"
            && conn.peer.version == "0.2.8",
        json!({"begin":begun,"bound_id":bound.bound_id,"update":conn.update}),
    );
    env.conn = Some(conn);
    let ping = env.call("ping", json!({})).await.map_err(err)?;
    let recovered =
        acceptance::recover_previous(&mut env.conn.as_mut().expect("bridge").bridge, &ping).await;
    gate.open();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let released = recovered
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|v| v["release"].clone())
        .unwrap_or(Value::Null);
    env.out.check(
        "отложенная связанная операция освобождена штатным восстановлением; собственный файл не оставлен",
        ping["operation"]["op_id"] == op.as_str()
            && ping["operation"]["bound"] == true
            && released["released"] == true
            && released["bound"] == true
            && download_files(&env.downloads()).is_empty(),
        json!({"operation":ping["operation"],"recovered":recovered.map(|r| r.map_err(|e| provider_code(&e).to_owned())),
            "downloads":files_json(&env.downloads())}),
    );
    disconnect(env).await;
    // 3. Следующее подключение без операции завершает обновление.
    let conn = acceptance::connect(
        &env.ctx.state_dir,
        Some(&env.ctx.target),
        Duration::from_secs(30),
    )
    .await
    .map_err(err)?;
    env.out.check(
        "после освобождения операции обновление выполнено при следующей команде",
        conn.update["reload"] == "reloaded"
            && conn.peer.version == bundled
            && conn.peer.instance == first.instance,
        conn.update.clone(),
    );
    env.conn = Some(conn);
    env.residual_clean().await
}

pub async fn target_isolation(env: &mut Env<'_>) -> Result<()> {
    let second_dir = env.dir.join("second-extension");
    let second = brave::setup(&SetupOptions {
        target: Some("second".into()),
        browser: Some(env.ctx.kind),
        install_dir: Some(second_dir.clone()),
        port: None,
        platform: Some(Platform::Linux),
        discovery: Some(brave::browsers::DiscoveryRoots::default()),
        ..SetupOptions::new(env.ctx.state_dir.clone())
    })
    .map_err(err)?;
    let first = targets::load_target(&env.ctx.state_dir, Some(&env.ctx.target)).map_err(err)?;
    let other = targets::load_target(&env.ctx.state_dir, Some("second")).map_err(err)?;
    env.out.check(
        "вторая цель получила свой каталог, порт, секрет и ID расширения; цель по умолчанию не изменилась",
        first.port != other.port
            && first.secret != other.secret
            && first.expected_extension_ids() != other.expected_extension_ids()
            && second["default_target"] != "second",
        json!({"first":{"port":first.port,"expected_id":first.expected_extension_ids()},
            "second":{"port":other.port,"expected_id":other.expected_extension_ids()},"default_target":second["default_target"]}),
    );
    // Оба расширения в одном профиле: каждая цель получает только своё.
    let extensions: Vec<PathBuf> = vec![env.ctx.extension_dir.clone(), second_dir.clone()];
    env.browser
        .relaunch(LaunchOptions {
            executable: &env.ctx.chrome,
            dir: &env.dir,
            extensions: &extensions,
            loader: env.ctx.loader,
            fixture_port: env.ctx.fixture.port,
            prompt_for_download: false,
            restore_session: false,
        })
        .await?;
    let state_dir = env.ctx.state_dir.clone();
    let (a, b) = tokio::join!(
        acceptance::connect(&state_dir, Some(&env.ctx.target), Duration::from_secs(30)),
        acceptance::connect(&state_dir, Some("second"), Duration::from_secs(30)),
    );
    let (mut a, mut b) = (a.map_err(err)?, b.map_err(err)?);
    let ping_a = a
        .bridge
        .call("ping", json!({}), Duration::from_secs(5))
        .await
        .map_err(err)?;
    let ping_b = b
        .bridge
        .call("ping", json!({}), Duration::from_secs(5))
        .await
        .map_err(err)?;
    env.out.check(
        "два расширения в одном профиле: каждая цель подключила своё расширение своего каталога",
        a.peer.extension_id != b.peer.extension_id
            && first
                .expected_extension_ids()
                .contains(&a.peer.extension_id)
            && other
                .expected_extension_ids()
                .contains(&b.peer.extension_id)
            && ping_a["extension_id"] == a.peer.extension_id.as_str()
            && ping_b["extension_id"] == b.peer.extension_id.as_str()
            && a.peer.instance.is_some()
            && a.peer.instance != b.peer.instance,
        json!({"first":a.peer.extension_id,"second":b.peer.extension_id,
            "loaded_by_browser":env.browser.loaded_extensions}),
    );
    a.bridge.disconnect().await;
    b.bridge.disconnect().await;
    drop((a, b));
    // Тот же каталог первой цели в другом профиле: тот же ID и секрет, но
    // другой экземпляр. Пока работает только он, цель к нему не подключается.
    let foreign = TestBrowser::launch(LaunchOptions {
        executable: &env.ctx.chrome,
        dir: &env.dir.join("other-profile"),
        extensions: std::slice::from_ref(&env.ctx.extension_dir),
        loader: env.ctx.loader,
        fixture_port: env.ctx.fixture.port,
        prompt_for_download: false,
        restore_session: false,
    })
    .await?;
    let main = std::mem::replace(&mut env.browser, foreign);
    let stopped = main.shutdown().await;
    let attempt =
        acceptance::connect(&state_dir, Some(&env.ctx.target), Duration::from_secs(20)).await;
    let rejections = attempt
        .as_ref()
        .err()
        .map(|e| e.details["rejections"].clone())
        .unwrap_or(Value::Null);
    env.out.check(
        "расширение того же каталога из другого профиля отклонено (instance_mismatch), цель не переключилась",
        code(&attempt) == "instance_mismatch" && rejections["instance_mismatch"].as_u64() >= Some(1),
        json!({"result":code(&attempt),"rejections":rejections,"main_browser_stopped":stopped}),
    );
    drop(attempt);
    // Явная переустановка: --repin снимает закрепление, секрет и каталог прежние.
    brave::setup(&SetupOptions {
        repin: true,
        ..env.ctx.setup.clone()
    })
    .map_err(err)?;
    let repinned = acceptance::connect(&state_dir, Some(&env.ctx.target), Duration::from_secs(30))
        .await
        .map_err(err)?;
    let after = targets::load_target(&state_dir, Some(&env.ctx.target)).map_err(err)?;
    env.out.check(
        "после явного setup --repin цель закрепила новый экземпляр с прежним секретом",
        after.secret == first.secret
            && after.instance_id == repinned.peer.instance
            && repinned.peer.instance.is_some(),
        json!({"secret_unchanged":after.secret == first.secret}),
    );
    env.conn = Some(repinned);
    env.residual_clean().await
}
