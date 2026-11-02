use super::*;
use crate::temp_workspace::TempWorkspace;
use std::os::unix::fs::PermissionsExt;

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn pad(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

fn header(bytes: &mut [u8], value_length: usize, kind: u16) {
    let length = bytes.len() as u16;
    bytes[0..2].copy_from_slice(&length.to_le_bytes());
    bytes[2..4].copy_from_slice(&(value_length as u16).to_le_bytes());
    bytes[4..6].copy_from_slice(&kind.to_le_bytes());
}

fn string_entry(key: &str, value: &str) -> Vec<u8> {
    let mut entry = vec![0; 6];
    entry.extend(utf16z(key));
    pad(&mut entry);
    entry.extend(utf16z(value));
    header(&mut entry, value.encode_utf16().count() + 1, 1);
    pad(&mut entry);
    entry
}

/// Минимальный исполняемый файл Windows с ресурсом версии, как у настоящих браузеров.
/// Перед блоком лежит «таблица» с теми же словами: разбор не должен её принять.
fn synthetic_pe(product: &str, version: &str) -> Vec<u8> {
    let mut table = vec![0; 6];
    table.extend(utf16z("StringTable"));
    pad(&mut table);
    table.extend(utf16z("040904b0"));
    pad(&mut table);
    for (key, value) in [
        ("CompanyName", "Example"),
        ("FileVersion", version),
        ("ProductName", product),
        ("ProductVersion", version),
    ] {
        table.extend(string_entry(key, value));
    }
    header(&mut table, 0, 1);
    let mut info = vec![0; 6];
    info.extend(utf16z("StringFileInfo"));
    pad(&mut info);
    info.extend(table);
    header(&mut info, 0, 1);
    let mut root = vec![0; 6];
    root.extend(utf16z("VS_VERSION_INFO"));
    pad(&mut root);
    root.extend([0xbd, 0x04, 0xef, 0xfe]);
    root.extend([0; 48]);
    pad(&mut root);
    root.extend(info);
    header(&mut root, 52, 0);
    let mut file = b"MZ".to_vec();
    file.resize(512, 0);
    // Ложное вхождение ключей вне блока версии (как таблица строк в msedge.exe).
    file.extend(utf16z("ProductName"));
    file.extend(utf16z("CompanyShortName"));
    pad(&mut file);
    file.extend(root);
    file.extend([0; 64]);
    file
}

fn script(path: &std::path::Path, output: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\necho '{output}'\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn pe_version_block_is_parsed_structurally() {
    let strings = pe_version_strings(&synthetic_pe("Microsoft Edge", "156.0.4314.58")).unwrap();
    assert_eq!(strings["ProductName"], "Microsoft Edge");
    assert_eq!(strings["FileVersion"], "156.0.4314.58");
    assert_eq!(strings["CompanyName"], "Example");
    assert!(pe_version_strings(b"MZ no resources").is_none());
}

#[test]
fn version_output_identifies_kind_and_version() {
    for (text, kind, version) in [
        (
            "Google Chrome 156.0.8078.79 ",
            BrowserKind::Chrome,
            "156.0.8078.79",
        ),
        (
            "Microsoft Edge 155.0.4283.62 unknown",
            BrowserKind::Edge,
            "155.0.4283.62",
        ),
        ("Vivaldi 8.2.4133.96", BrowserKind::Vivaldi, "8.2.4133.96"),
        (
            "Brave Browser 141.1.83.118",
            BrowserKind::Brave,
            "141.1.83.118",
        ),
        (
            "Google Chrome for Testing 153.0.8010.12",
            BrowserKind::Chromium,
            "153.0.8010.12",
        ),
        (
            "Chromium 141.0.7390.54 built on Debian",
            BrowserKind::Chromium,
            "141.0.7390.54",
        ),
    ] {
        assert_eq!(
            kind_from_version_output(text),
            Some((kind, Some(version.to_owned()))),
            "{text}"
        );
    }
    for text in ["", "Firefox 140.0", "Google Chromeish 1.2", "Opera 120.0"] {
        assert_eq!(kind_from_version_output(text), None, "{text}");
    }
    // Версию, которую нельзя надёжно прочитать, не выдумываем.
    assert_eq!(
        kind_from_version_output("Vivaldi unknown"),
        Some((BrowserKind::Vivaldi, None))
    );
}

#[test]
fn unpacked_extension_id_follows_chromium_path_hash() {
    // Формула сверена с ID, которые назначили настоящие Windows Brave и Linux Chrome/Edge/Vivaldi.
    assert_eq!(
        unpacked_extension_id(
            Platform::Wsl,
            r"C:\Users\Example\AppData\Local\AnkiDeckToolkit\forvo-chrome-adapter"
        ),
        "pcmomdlmebkkhmdhhcmcpmocgdkepfia"
    );
    assert_eq!(
        unpacked_extension_id(
            Platform::Linux,
            "/home/example/.local/share/anki-deck-toolkit/forvo-chrome-adapter"
        ),
        "hclejjeipikpmobmkophhndogdappmnf"
    );
    // Разные каталоги — разные ID: цели не делят Origin.
    assert_ne!(
        unpacked_extension_id(Platform::Linux, "/a/forvo-chrome-adapter"),
        unpacked_extension_id(Platform::Linux, "/a/forvo-edge-adapter")
    );
}

#[test]
fn expected_ids_include_resolved_symlink() {
    let temp = TempWorkspace::create("browser-ids").unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ids = expected_extension_ids(Platform::Linux, link.to_str().unwrap());
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&unpacked_extension_id(
        Platform::Linux,
        std::fs::canonicalize(&real).unwrap().to_str().unwrap()
    )));
}

#[test]
fn linux_discovery_confirms_kind_by_executable() {
    let temp = TempWorkspace::create("browser-discovery").unwrap();
    let bin = temp.path().join("bin");
    script(&bin.join("google-chrome"), "Google Chrome 156.0.8078.79");
    script(
        &bin.join("microsoft-edge"),
        "Microsoft Edge 155.0.4283.62 unknown",
    );
    // Имя Brave, а внутри другой продукт: установкой Brave не считается.
    script(
        &bin.join("brave-browser"),
        "Chromium 141.0.1 built on Debian",
    );
    // Каталог с именем браузера, но без исполняемого файла.
    std::fs::create_dir_all(bin.join("vivaldi")).unwrap();
    // Не исполняемый файл.
    std::fs::write(
        bin.join("vivaldi-stable"),
        "#!/bin/sh\necho 'Vivaldi 8.2'\n",
    )
    .unwrap();
    let found = discover(&DiscoveryRoots {
        windows: vec![],
        linux_path: vec![bin.clone()],
        linux_fixed: false,
    });
    let kinds: Vec<_> = found.iter().map(|i| (i.browser, i.platform)).collect();
    assert_eq!(
        kinds,
        vec![
            (BrowserKind::Chrome, Platform::Linux),
            (BrowserKind::Edge, Platform::Linux)
        ]
    );
    assert_eq!(found[0].chromium_major, Some(156));
    assert_eq!(found[0].identified_by, "version_output");
    // Ноль установок.
    assert!(
        discover(&DiscoveryRoots {
            windows: vec![],
            linux_path: vec![temp.path().join("empty")],
            linux_fixed: false,
        })
        .is_empty()
    );
}

#[test]
fn linux_package_wrapper_and_binary_are_one_installation() {
    let temp = TempWorkspace::create("browser-discovery-wrapper").unwrap();
    let opt = temp.path().join("opt/brave");
    script(&opt.join("brave"), "Brave Browser 156.1.98.63");
    script(&opt.join("brave-browser"), "Brave Browser 156.1.98.63");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(opt.join("brave-browser"), bin.join("brave-browser")).unwrap();
    let found = discover(&DiscoveryRoots {
        windows: vec![],
        linux_path: vec![bin.clone(), opt.clone()],
        linux_fixed: false,
    });
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].browser, BrowserKind::Brave);
    assert_eq!(
        found[0].executable,
        bin.join("brave-browser").display().to_string()
    );
    // Разные каталоги одного браузера — по-прежнему разные установки.
    script(&temp.path().join("other/brave"), "Brave Browser 155.1.97.1");
    let found = discover(&DiscoveryRoots {
        windows: vec![],
        linux_path: vec![bin, opt, temp.path().join("other")],
        linux_fixed: false,
    });
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn windows_discovery_reads_product_name_not_directory() {
    let temp = TempWorkspace::create("browser-discovery-win").unwrap();
    let program_files = temp.path().join("Program Files");
    let local = temp.path().join("Local");
    let put = |root: &std::path::Path, relative: &str, bytes: Vec<u8>| {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    put(
        &program_files,
        "Google/Chrome/Application/chrome.exe",
        synthetic_pe("Google Chrome", "156.0.8078.76"),
    );
    // Каталог Edge, а продукт другой: не Edge.
    put(
        &program_files,
        "Microsoft/Edge/Application/msedge.exe",
        synthetic_pe("Something Else", "1.0.0.0"),
    );
    put(
        &local,
        "Vivaldi/Application/vivaldi.exe",
        synthetic_pe("Vivaldi", "8.2.4133.96"),
    );
    // Каталог Brave без исполняемого файла.
    std::fs::create_dir_all(program_files.join("BraveSoftware/Brave-Browser/Application")).unwrap();
    let found = discover(&DiscoveryRoots {
        windows: vec![program_files, local],
        linux_path: vec![],
        linux_fixed: false,
    });
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].browser, BrowserKind::Chrome);
    assert_eq!(found[0].platform, Platform::Wsl);
    assert_eq!(found[0].chromium_major, Some(156));
    assert_eq!(found[0].identified_by, "pe_version_info");
    assert_eq!(found[1].browser, BrowserKind::Vivaldi);
    assert_eq!(found[1].version.as_deref(), Some("8.2.4133.96"));
    // У Vivaldi своя нумерация: версия Chromium неизвестна, а не 8.
    assert_eq!(found[1].chromium_major, None);
}

#[test]
fn explicit_paths_report_precise_errors() {
    let temp = TempWorkspace::create("browser-identify").unwrap();
    let missing = temp.path().join("nope/chrome");
    assert_eq!(
        crate::forvo::provider_code(&identify(missing.to_str().unwrap(), false).unwrap_err()),
        "browser_missing"
    );
    let plain = temp.path().join("plain");
    std::fs::write(&plain, "x").unwrap();
    assert_eq!(
        crate::forvo::provider_code(&identify(plain.to_str().unwrap(), false).unwrap_err()),
        "browser_not_executable"
    );
    let other = temp.path().join("firefox");
    script(&other, "Mozilla Firefox 140.0");
    assert_eq!(
        crate::forvo::provider_code(&identify(other.to_str().unwrap(), false).unwrap_err()),
        "browser_unrecognized"
    );
    let custom = temp.path().join("custom/edge-dev");
    script(&custom, "Microsoft Edge 157.0.1.0 dev");
    let found = identify(custom.to_str().unwrap(), false).unwrap();
    assert_eq!(found.browser, BrowserKind::Edge);
    assert_eq!(found.version.as_deref(), Some("157.0.1.0"));
    // Исполняемый файл Windows вне WSL не используется.
    assert_eq!(
        crate::forvo::provider_code(&identify(r"C:\Browser\chrome.exe", false).unwrap_err()),
        "browser_platform_unsupported"
    );
    let fake_exe = temp.path().join("chrome.exe");
    std::fs::write(&fake_exe, b"not a pe").unwrap();
    assert_eq!(
        crate::forvo::provider_code(&identify(fake_exe.to_str().unwrap(), true).unwrap_err()),
        "browser_unrecognized"
    );
    let pe = temp.path().join("browser.exe");
    std::fs::write(&pe, synthetic_pe("Brave Browser", "156.1.98.63")).unwrap();
    let brave = identify(pe.to_str().unwrap(), true).unwrap();
    assert_eq!(brave.browser, BrowserKind::Brave);
    assert_eq!(brave.chromium_major, Some(156));
}

#[test]
fn kinds_have_exact_user_pages() {
    assert_eq!(BrowserKind::Edge.extensions_uri(), "edge://extensions");
    assert_eq!(
        BrowserKind::Vivaldi.extensions_uri(),
        "vivaldi://extensions"
    );
    assert_eq!(BrowserKind::Chrome.extensions_uri(), "chrome://extensions");
    assert_eq!(BrowserKind::Brave.extensions_uri(), "brave://extensions");
    let action = BrowserKind::Edge.load_unpacked_action(r"C:\X\forvo-edge-adapter");
    assert!(action.contains("edge://extensions"));
    assert!(action.contains(r"C:\X\forvo-edge-adapter"));
    for kind in BrowserKind::ALL {
        assert_eq!(BrowserKind::parse(kind.id()), Some(kind));
    }
}
