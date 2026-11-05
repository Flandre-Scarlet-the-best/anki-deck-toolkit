use super::*;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

fn raw_zip(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes, mode) in entries {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(DateTime::DEFAULT)
            .unix_permissions(*mode);
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn bundled_entries() -> Vec<(String, Vec<u8>)> {
    bundled_files()
        .into_iter()
        .map(|f| (f.path, f.bytes))
        .collect()
}

fn zip_with(extra: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let owned = bundled_entries();
    let mut entries: Vec<(&str, &[u8], u32)> = owned
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice(), 0o644))
        .collect();
    entries.extend_from_slice(extra);
    raw_zip(&entries)
}

fn code(error: &AssetError) -> &str {
    crate::forvo::provider_code(error)
}

#[test]
fn bundled_package_is_complete_has_no_pairing_and_reuses_dom_contract() {
    let files = bundled_files();
    let facts = validate_files(&files).unwrap();
    assert_eq!(facts.version, bundled_version());
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    for required in REQUIRED_FILES {
        assert!(paths.contains(&required), "{required}");
    }
    assert!(!paths.contains(&PAIRING_FILE));
    let page = &files
        .iter()
        .find(|f| f.path == "forvo_page.js")
        .unwrap()
        .bytes;
    let page = String::from_utf8_lossy(page);
    assert!(page.contains(include_str!("../dom.js").trim_end()));
    assert!(page.contains(include_str!("../auth.js").trim_end()));
    assert_eq!(files.last().unwrap().path, "manifest.json");
}

#[test]
fn zip_is_deterministic_and_round_trips() {
    let files = bundled_files();
    let first = build_zip(&files).unwrap();
    let second = build_zip(&files).unwrap();
    assert_eq!(first, second, "одинаковые файлы дают одинаковый ZIP");
    let mut extracted = extract_zip(&first).unwrap();
    let mut original = files.clone();
    extracted.sort_by(|a, b| a.path.cmp(&b.path));
    original.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(extracted, original);
    // pairing.json не может попасть в пакет даже по ошибке сборки.
    let mut with_pairing = files;
    with_pairing.push(PackageFile {
        path: PAIRING_FILE.into(),
        bytes: b"{}".to_vec(),
    });
    assert_eq!(
        code(&build_zip(&with_pairing).unwrap_err()),
        "package_path_rejected"
    );
}

#[test]
fn chromium_versions_parse_and_order_numerically() {
    let v = |t: &str| Version::parse(t).unwrap();
    assert!(v("0.3.10") > v("0.3.4"));
    assert!(v("1.0") == v("1.0.0"));
    assert!(v("0.3.4.1") > v("0.3.4"));
    for bad in ["", "1.", "01.2", "1.2.3.4.5", "1.-2", "65536", "1.2a", " 1"] {
        assert!(Version::parse(bad).is_none(), "{bad}");
    }
    assert_eq!(v("0.3.4").to_string(), "0.3.4");
}

#[test]
fn manifest_policy_rejects_identity_update_and_permission_changes() {
    let ours: Value = serde_json::from_str(MANIFEST).unwrap();
    let with = |key: &str, value: Value| {
        let mut m = ours.clone();
        m[key] = value;
        serde_json::to_vec(&m).unwrap()
    };
    assert!(validate_manifest(MANIFEST.as_bytes()).is_ok());
    for (key, value, expected) in [
        ("key", json!("AAAA"), "package_manifest_invalid"),
        (
            "update_url",
            json!("https://example.com/u.xml"),
            "package_manifest_invalid",
        ),
        ("content_scripts", json!([]), "package_manifest_invalid"),
        (
            "externally_connectable",
            json!({}),
            "package_manifest_invalid",
        ),
        (
            "name",
            json!("Другое расширение"),
            "package_manifest_invalid",
        ),
        ("manifest_version", json!(2), "package_manifest_invalid"),
        ("version", json!("1.02"), "package_manifest_invalid"),
        ("incognito", json!("split"), "package_manifest_invalid"),
        (
            "background",
            json!({"service_worker":"evil.js"}),
            "package_manifest_invalid",
        ),
        (
            "permissions",
            json!(["downloads", "cookies"]),
            "package_permission_not_allowed",
        ),
        (
            "host_permissions",
            json!(["<all_urls>"]),
            "package_permission_not_allowed",
        ),
    ] {
        let error = validate_manifest(&with(key, value)).unwrap_err();
        assert_eq!(code(&error), expected, "{key}");
    }
}

#[test]
fn extraction_rejects_unsafe_archives_before_any_write() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("traversal", zip_with(&[("../evil.js", b"x", 0o644)])),
        ("absolute", zip_with(&[("/evil.js", b"x", 0o644)])),
        ("backslash", zip_with(&[("a\\evil.js", b"x", 0o644)])),
        ("pairing", zip_with(&[(PAIRING_FILE, b"{}", 0o644)])),
        ("foreign_type", zip_with(&[("run.sh", b"x", 0o644)])),
        ("hidden", zip_with(&[(".evil.js", b"x", 0o644)])),
        (
            "installer_name",
            zip_with(&[("background.js.update-new", b"x", 0o644)]),
        ),
        ("executable", zip_with(&[("tool.js", b"x", 0o755)])),
        ("deep", zip_with(&[("a/b/c.js", b"x", 0o644)])),
    ];
    for (name, bytes) in cases {
        let error = extract_zip(&bytes).unwrap_err();
        assert!(
            code(&error).starts_with("package_"),
            "{name}: {}",
            code(&error)
        );
    }
    // Ссылка.
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (path, bytes) in bundled_entries() {
        writer
            .start_file(
                path,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer
        .add_symlink("link.js", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    let link = writer.finish().unwrap().into_inner();
    assert_eq!(
        code(&extract_zip(&link).unwrap_err()),
        "package_link_rejected"
    );
    // Каталог.
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_directory("icons/", SimpleFileOptions::default())
        .unwrap();
    let dir = writer.finish().unwrap().into_inner();
    assert_eq!(
        code(&extract_zip(&dir).unwrap_err()),
        "package_path_rejected"
    );
    // Неполный пакет без генерируемого страничного модуля.
    let owned = bundled_entries();
    let without_page: Vec<(&str, &[u8], u32)> = owned
        .iter()
        .filter(|(n, _)| n != "forvo_page.js")
        .map(|(n, b)| (n.as_str(), b.as_slice(), 0o644))
        .collect();
    let error = extract_zip(&raw_zip(&without_page)).unwrap_err();
    assert_eq!(code(&error), "package_incomplete");
    assert_eq!(error.details["missing"], "forvo_page.js");
}

#[test]
fn extraction_rejects_duplicates_bombs_and_excess_entries() {
    // Повтор имени: второй файл переименован на уровне байтов архива.
    let twice = raw_zip(&[
        ("manifest.json", MANIFEST.as_bytes(), 0o644),
        ("a_page.js", b"1", 0o644),
        ("b_page.js", b"2", 0o644),
    ]);
    let mut twice = twice;
    let positions: Vec<usize> = twice
        .windows(9)
        .enumerate()
        .filter(|(_, w)| *w == b"b_page.js")
        .map(|(i, _)| i)
        .collect();
    for i in positions {
        twice[i..i + 9].copy_from_slice(b"a_page.js");
    }
    let error = extract_zip(&twice).unwrap_err();
    assert_eq!(code(&error), "package_duplicate_entry");
    // Распаковка не дальше предела: файл больше MAX_FILE_BYTES отвергается по заголовку.
    let big = vec![0_u8; (MAX_FILE_BYTES + 1) as usize];
    assert_eq!(
        code(&extract_zip(&zip_with(&[("big.js", &big, 0o644)])).unwrap_err()),
        "package_limits_exceeded"
    );
    let many: Vec<(String, Vec<u8>)> = (0..MAX_FILES + 1)
        .map(|i| (format!("f{i}.js"), vec![b'x']))
        .collect();
    let refs: Vec<(&str, &[u8], u32)> = many
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice(), 0o644))
        .collect();
    assert_eq!(
        code(&extract_zip(&raw_zip(&refs)).unwrap_err()),
        "package_limits_exceeded"
    );
    assert_eq!(
        code(&extract_zip(b"not a zip").unwrap_err()),
        "package_archive_invalid"
    );
}
