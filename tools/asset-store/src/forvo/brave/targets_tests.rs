use super::*;
use crate::forvo::provider_code;
use crate::temp_workspace::TempWorkspace;

fn options(state_dir: &Path, kind: BrowserKind, dir: &Path) -> SetupOptions {
    SetupOptions {
        browser: Some(kind),
        install_dir: Some(dir.to_owned()),
        platform: Some(Platform::Linux),
        discovery: Some(DiscoveryRoots::default()),
        ..SetupOptions::new(state_dir.to_owned())
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Состояние версии 1 так, как его записал однобраузерный адаптер.
fn legacy_state(state_dir: &Path, dir: &Path, port: u16) -> Vec<u8> {
    private_dir(state_dir).unwrap();
    let secret = "ab".repeat(32);
    write_extension_files(dir, port, &secret).unwrap();
    let bytes = serde_json::to_vec_pretty(&json!({
        "schema_version": 1, "port": port, "secret": secret, "install_dir": dir,
        "load_unpacked_path": dir.display().to_string(), "platform": "linux",
        "extension_id": "akgdejebaomoemodpghpkpadijllophk",
    }))
    .unwrap();
    write_atomic(&state_dir.join("state.json"), &bytes, 0o600).unwrap();
    bytes
}

#[test]
fn legacy_brave_state_keeps_pairing_through_migration() {
    let temp = TempWorkspace::create("targets-legacy").unwrap();
    let state_dir = temp.path().join("state");
    let dir = temp.path().join("forvo-brave-adapter");
    let port = free_port();
    let original = legacy_state(&state_dir, &dir, port);
    // Чтение старого состояния ничего не переписывает.
    let loaded = load_state(&state_dir).unwrap();
    assert_eq!(loaded.target, LEGACY_TARGET);
    assert_eq!(loaded.browser, BrowserKind::Brave);
    assert_eq!(loaded.port, port);
    assert_eq!(loaded.secret, "ab".repeat(32));
    assert_eq!(
        std::fs::read(state_dir.join("state.json")).unwrap(),
        original
    );
    // Повторный setup прежней командой: тот же секрет, ID, каталог и порт.
    let out = setup(&SetupOptions {
        target: Some(LEGACY_TARGET.into()),
        port: None,
        install_dir: None,
        ..options(&state_dir, BrowserKind::Brave, &dir)
    })
    .unwrap();
    assert_eq!(out["pairing"], "kept");
    assert_eq!(out["migrated_from_v1"], true);
    assert_eq!(
        out["pinned_extension_id"],
        "akgdejebaomoemodpghpkpadijllophk"
    );
    assert_eq!(out["user_action"], Value::Null);
    assert!(!out.to_string().contains(&"ab".repeat(32)));
    let after = load_state(&state_dir).unwrap();
    assert_eq!(
        (
            after.port,
            &after.secret,
            &after.install_dir,
            &after.extension_id
        ),
        (
            port,
            &loaded.secret,
            &loaded.install_dir,
            &loaded.extension_id
        )
    );
    let file: Value =
        serde_json::from_slice(&std::fs::read(state_dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(file["schema_version"], 2);
    assert_eq!(file["default_target"], LEGACY_TARGET);
    assert_eq!(
        std::fs::read(state_dir.join("state.v1.json")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::metadata(state_dir.join("state.v1.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn targets_have_independent_pairings_and_explicit_selection() {
    let temp = TempWorkspace::create("targets-multi").unwrap();
    let state_dir = temp.path().join("state");
    let brave_dir = temp.path().join("brave");
    let chrome_dir = temp.path().join("chrome");
    let brave_port = free_port();
    setup(&SetupOptions {
        port: Some(brave_port),
        ..options(&state_dir, BrowserKind::Brave, &brave_dir)
    })
    .unwrap();
    // Общий каталог дал бы общий ID и секрет; общий порт — подключение к чужой цели.
    let conflict = setup(&options(&state_dir, BrowserKind::Chrome, &brave_dir)).unwrap_err();
    assert_eq!(provider_code(&conflict), "install_dir_conflict");
    let conflict = setup(&SetupOptions {
        port: Some(brave_port),
        ..options(&state_dir, BrowserKind::Chrome, &chrome_dir)
    })
    .unwrap_err();
    assert_eq!(provider_code(&conflict), "port_conflict");
    let chrome = setup(&options(&state_dir, BrowserKind::Chrome, &chrome_dir)).unwrap();
    assert_eq!(chrome["target"], "chrome");
    assert_eq!(chrome["default_target"], LEGACY_TARGET);
    let brave = load_target(&state_dir, Some("brave")).unwrap();
    let chrome = load_target(&state_dir, Some("chrome")).unwrap();
    assert_ne!(brave.secret, chrome.secret);
    assert_ne!(brave.port, chrome.port);
    assert_ne!(
        brave.expected_extension_ids(),
        chrome.expected_extension_ids()
    );
    // Без --target — только цель по умолчанию, не «первая ответившая».
    assert_eq!(load_target(&state_dir, None).unwrap().target, "brave");
    assert_eq!(
        provider_code(&load_target(&state_dir, Some("edge")).unwrap_err()),
        "target_unknown"
    );
    let described = describe(&state_dir).unwrap();
    assert_eq!(described["targets"].as_array().unwrap().len(), 2);
    assert!(!described.to_string().contains(&brave.secret));
    assert!(!described.to_string().contains(&chrome.secret));
    select(&state_dir, "chrome").unwrap();
    assert_eq!(load_target(&state_dir, None).unwrap().target, "chrome");
    // Существующая цель не меняет браузер молча.
    let mismatch = setup(&SetupOptions {
        target: Some("chrome".into()),
        ..options(&state_dir, BrowserKind::Edge, &chrome_dir)
    })
    .unwrap_err();
    assert_eq!(provider_code(&mismatch), "target_browser_mismatch");
    // Две цели без выбора по умолчанию — отказ с перечнем.
    let mut file = load_file(&state_dir).unwrap().unwrap();
    file.default_target = None;
    assert_eq!(
        provider_code(&file.resolve(None).unwrap_err()),
        "target_ambiguous"
    );
}

#[test]
fn repeated_setup_is_idempotent_and_repin_keeps_secret() {
    let temp = TempWorkspace::create("targets-idempotent").unwrap();
    let state_dir = temp.path().join("state");
    let dir = temp.path().join("edge");
    let port = free_port();
    let options = |state_dir: &Path, kind, dir: &Path| SetupOptions {
        port: Some(port),
        ..options(state_dir, kind, dir)
    };
    let first = setup(&options(&state_dir, BrowserKind::Edge, &dir)).unwrap();
    assert_eq!(
        first["extension_files"]["changed"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let state = load_target(&state_dir, Some("edge")).unwrap();
    pin_peer(
        &state_dir,
        &state,
        &state.expected_extension_ids()[0],
        Some(&"cd".repeat(32)),
    )
    .unwrap();
    let before = std::fs::read(state_dir.join("state.json")).unwrap();
    let again = setup(&options(&state_dir, BrowserKind::Edge, &dir)).unwrap();
    assert_eq!(again["pairing"], "kept");
    assert_eq!(again["extension_files"]["changed"], json!([]));
    assert_eq!(again["user_action"], Value::Null);
    assert_eq!(std::fs::read(state_dir.join("state.json")).unwrap(), before);
    // Изменённый файл расширения восстанавливается, остальное не трогается.
    std::fs::write(dir.join("background.js"), "// старая версия").unwrap();
    let pinned = load_target(&state_dir, Some("edge")).unwrap();
    assert_eq!(files_status(&pinned)["differs"], json!(["background.js"]));
    assert_eq!(
        sync_installed_files(&pinned).unwrap(),
        Some(vec!["background.js"])
    );
    let repinned = setup(&SetupOptions {
        repin: true,
        ..options(&state_dir, BrowserKind::Edge, &dir)
    })
    .unwrap();
    assert_eq!(repinned["pairing"], "kept");
    let after = load_target(&state_dir, Some("edge")).unwrap();
    assert_eq!(after.secret, state.secret);
    assert_eq!((after.extension_id, after.instance_id), (None, None));
}

#[test]
fn corrupt_state_and_busy_port_never_create_new_pairing() {
    let temp = TempWorkspace::create("targets-refuse").unwrap();
    let state_dir = temp.path().join("state");
    let dir = temp.path().join("brave");
    let port = free_port();
    setup(&SetupOptions {
        port: Some(port),
        ..options(&state_dir, BrowserKind::Brave, &dir)
    })
    .unwrap();
    let files_before = std::fs::read(dir.join("pairing.json")).unwrap();
    // Порт цели занят (идёт fetch/пакет этой цели): файлы не меняются.
    let busy = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::fs::write(dir.join("background.js"), "// старая").unwrap();
    let error = setup(&SetupOptions {
        install_dir: None,
        ..options(&state_dir, BrowserKind::Brave, &dir)
    })
    .unwrap_err();
    assert_eq!(provider_code(&error), "adapter_busy");
    assert_eq!(
        std::fs::read(dir.join("background.js")).unwrap(),
        "// старая".as_bytes()
    );
    drop(busy);
    write_atomic(&state_dir.join("state.json"), b"{", 0o600).unwrap();
    let error = setup(&options(&state_dir, BrowserKind::Brave, &dir)).unwrap_err();
    assert_eq!(provider_code(&error), "state_corrupt");
    assert_eq!(std::fs::read(state_dir.join("state.json")).unwrap(), b"{");
    assert_eq!(
        std::fs::read(dir.join("pairing.json")).unwrap(),
        files_before
    );
    // Чужое расширение в каталоге не перезаписывается.
    let foreign = temp.path().join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("manifest.json"), r#"{"name":"Other"}"#).unwrap();
    assert_eq!(
        provider_code(&write_extension_files(&foreign, port, &"00".repeat(32)).unwrap_err()),
        "install_dir_foreign"
    );
}

#[test]
fn pinning_refuses_concurrent_pairing_change() {
    let temp = TempWorkspace::create("targets-pin").unwrap();
    let state_dir = temp.path().join("state");
    let dir = temp.path().join("chrome");
    let port = free_port();
    let options = |state_dir: &Path, kind, dir: &Path| SetupOptions {
        port: Some(port),
        ..options(state_dir, kind, dir)
    };
    setup(&options(&state_dir, BrowserKind::Chrome, &dir)).unwrap();
    let stale = load_target(&state_dir, None).unwrap();
    setup(&SetupOptions {
        rotate_secret: true,
        ..options(&state_dir, BrowserKind::Chrome, &dir)
    })
    .unwrap();
    let id = stale.expected_extension_ids()[0].clone();
    assert_eq!(
        provider_code(&pin_peer(&state_dir, &stale, &id, None).unwrap_err()),
        "pairing_changed"
    );
    let fresh = load_target(&state_dir, None).unwrap();
    let pinned = pin_peer(&state_dir, &fresh, &id, Some(&"ef".repeat(32))).unwrap();
    assert_eq!(pinned.extension_id.as_deref(), Some(id.as_str()));
    // Второй экземпляр не перезаписывает закрепление.
    assert_eq!(
        provider_code(&pin_peer(&state_dir, &pinned, &id, Some(&"12".repeat(32))).unwrap_err()),
        "pairing_changed"
    );
}
