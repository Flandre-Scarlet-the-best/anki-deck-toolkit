use super::*;
use crate::temp_workspace::TempWorkspace;
use std::path::PathBuf;

fn file(path: &str, text: &str) -> PackageFile {
    PackageFile {
        path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn old_files() -> Vec<PackageFile> {
    vec![
        file("background.js", "old background"),
        file("forvo_page.js", "old page"),
        file("pairing.json", "{\"secret\":\"kept\"}"),
        file("manifest.json", "{\"version\":\"1.0\"}"),
    ]
}

fn new_files() -> Vec<PackageFile> {
    vec![
        file("background.js", "new background"),
        file("forvo_page.js", "new page"),
        file("icons/a.png", "new icon"),
        file("pairing.json", "{\"secret\":\"kept\"}"),
        file("manifest.json", "{\"version\":\"2.0\"}"),
    ]
}

/// Снимок каталога: все файлы (включая служебные) по относительным путям.
fn snapshot(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .display()
                    .to_string();
                out.push((
                    rel,
                    String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).into_owned(),
                ));
            }
        }
    }
    out.sort();
    out
}

fn installed(name: &str) -> (TempWorkspace, PathBuf) {
    let workspace = TempWorkspace::create(name).unwrap();
    let dir = workspace.path().join("extension");
    install(&dir, &old_files(), (None, Some("1.0".into()))).unwrap();
    (workspace, dir)
}

#[test]
fn fresh_install_then_update_is_idempotent_and_keeps_unlisted_files() {
    let (_w, dir) = installed("install-basic");
    std::fs::write(dir.join("user-note.txt"), "чужой файл").unwrap();
    let changed = install(&dir, &new_files(), (Some("1.0".into()), Some("2.0".into()))).unwrap();
    assert_eq!(
        changed,
        [
            "background.js",
            "forvo_page.js",
            "icons/a.png",
            "manifest.json"
        ]
    );
    assert!(
        install(&dir, &new_files(), (None, None))
            .unwrap()
            .is_empty()
    );
    let files = snapshot(&dir);
    assert!(files.contains(&("user-note.txt".into(), "чужой файл".into())));
    assert!(
        files
            .iter()
            .all(|(n, _)| !n.contains(".update-") && n != JOURNAL)
    );
    assert!(recover(&dir).unwrap().is_none());
}

#[test]
fn crash_at_any_step_recovers_or_blocks_without_mutating_unknown_state() {
    let steps: Vec<Step> = (0..4)
        .map(Step::Staged)
        .chain((0..4).map(Step::Swapped))
        .chain([Step::Verified])
        .collect();
    for crash in steps {
        let (_w, dir) = installed("install-crash");
        let before = snapshot(&dir);
        let error = install_with(
            &dir,
            &new_files(),
            (Some("1.0".into()), Some("2.0".into())),
            &mut |step| {
                if step == crash {
                    Err(Fault::Crash)
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(crate::forvo::provider_code(&error), "install_interrupted");
        // Полусостояние после «сбоя питания» существует…
        assert!(dir.join(JOURNAL).exists(), "{crash:?}");
        match crash {
            Step::Staged(_) => {
                let recovered = recover(&dir).unwrap().unwrap();
                assert_eq!(recovered["recovered"], "discarded_staging", "{crash:?}");
                let after_recovery = snapshot(&dir);
                for original in &before {
                    assert!(after_recovery.contains(original), "{crash:?}: {original:?}");
                }
                assert!(!dir.join(JOURNAL).exists(), "{crash:?}");
                // Непроверяемые остатки staging сохраняются; новый ID позволяет
                // безопасно продолжить обычную установку.
                assert!(recover(&dir).unwrap().is_none());
                install(&dir, &new_files(), (None, None)).unwrap();
                assert_eq!(
                    std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
                    "{\"version\":\"2.0\"}"
                );
            }
            Step::Swapped(index) if index < 3 => {
                let before_recovery = snapshot(&dir);
                let error = recover(&dir).unwrap_err();
                assert_eq!(
                    error.details["reason"], "swapping_recovery_requires_manual_repair",
                    "{crash:?}: {error}"
                );
                assert_eq!(error.details["status"], "MANUAL_REPAIR_REQUIRED");
                assert!(
                    error.details["user_action"]
                        .as_str()
                        .is_some_and(|action| action.contains("полного доверенного выпуска"))
                );
                assert_eq!(snapshot(&dir), before_recovery, "{crash:?}");
                assert!(dir.join(JOURNAL).exists(), "{crash:?}");
            }
            Step::Swapped(_) | Step::Verified => {
                let recovered = recover(&dir).unwrap().unwrap();
                assert_eq!(recovered["recovered"], "completed_commit", "{crash:?}");
                for file in new_files() {
                    assert_eq!(
                        std::fs::read(dir.join(&file.path)).unwrap(),
                        file.bytes,
                        "{crash:?}: {}",
                        file.path
                    );
                }
                assert!(!dir.join(JOURNAL).exists(), "{crash:?}");
                assert!(recover(&dir).unwrap().is_none());
            }
        }
    }
}

#[test]
fn staging_recovery_preserves_foreign_files_named_by_untrusted_transaction_id() {
    let (_w, dir) = installed("install-staging-foreign-temp");
    let transaction_id = "victim";
    let new_temp = temp_path("background.js", NEW_SUFFIX, Some(transaction_id));
    let old_temp = temp_path("background.js", OLD_SUFFIX, Some(transaction_id));
    std::fs::write(dir.join(&new_temp), "foreign partial new file").unwrap();
    std::fs::write(dir.join(&old_temp), "foreign backup file").unwrap();
    std::fs::write(
        dir.join(JOURNAL),
        serde_json::to_vec(&json!({
            "schema_version":1,
            "phase":"staging",
            "from_version":"1.0",
            "to_version":"2.0",
            "transaction_id":transaction_id,
            "entries":[{
                "path":"background.js",
                // Даже поддельный SHA совпадает с чужим файлом, но не даёт
                // recovery права удалить его.
                "new_sha256":sha256_hex("foreign partial new file"),
                "old_sha256":sha256_hex("old background")
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        recover(&dir).unwrap().unwrap()["recovered"],
        "discarded_staging"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "old background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(new_temp)).unwrap(),
        "foreign partial new file"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(old_temp)).unwrap(),
        "foreign backup file"
    );
}

#[test]
fn swapping_recovery_blocks_mixed_state_and_preserves_untrusted_temp_files() {
    let (_w, dir) = installed("install-swapping-foreign-temp");
    std::fs::write(dir.join("background.js"), "new background").unwrap();
    let transaction_id = "victim";
    let new_temp = temp_path("forvo_page.js", NEW_SUFFIX, Some(transaction_id));
    let old_temp = temp_path("background.js", OLD_SUFFIX, Some(transaction_id));
    std::fs::write(dir.join(&new_temp), "foreign staged file").unwrap();
    std::fs::write(dir.join(&old_temp), "foreign backup file").unwrap();
    std::fs::write(
        dir.join(JOURNAL),
        serde_json::to_vec(&json!({
            "schema_version":1,
            "phase":"swapping",
            "from_version":"1.0",
            "to_version":"2.0",
            "transaction_id":transaction_id,
            "entries":[
                {"path":"background.js","new_sha256":sha256_hex("new background"),"old_sha256":sha256_hex("foreign backup file")},
                {"path":"forvo_page.js","new_sha256":sha256_hex("foreign staged file"),"old_sha256":sha256_hex("old page")}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let before = snapshot(&dir);

    let error = recover(&dir).unwrap_err();
    assert_eq!(
        error.details["reason"],
        "swapping_recovery_requires_manual_repair"
    );
    assert_eq!(error.details["status"], "MANUAL_REPAIR_REQUIRED");
    assert_eq!(snapshot(&dir), before);
    assert!(dir.join(JOURNAL).exists());
}

#[test]
fn committed_recovery_preserves_temp_files_even_when_journal_hash_matches() {
    let (_w, dir) = installed("install-committed-foreign-temp");
    let transaction_id = "victim";
    let new_temp = temp_path("background.js", NEW_SUFFIX, Some(transaction_id));
    std::fs::write(dir.join(&new_temp), "old background").unwrap();
    std::fs::write(
        dir.join(JOURNAL),
        serde_json::to_vec(&json!({
            "schema_version":1,
            "phase":"committed",
            "from_version":"1.0",
            "to_version":"2.0",
            "transaction_id":transaction_id,
            "entries":[{
                "path":"background.js",
                "new_sha256":sha256_hex("old background"),
                "old_sha256":null
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        recover(&dir).unwrap().unwrap()["recovered"],
        "completed_commit"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(new_temp)).unwrap(),
        "old background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "old background"
    );
    assert!(!dir.join(JOURNAL).exists());
}

#[test]
fn io_failure_during_staging_or_swap_leaves_previous_version() {
    for failing in [Step::Staged(2), Step::Swapped(1), Step::Swapped(3)] {
        let (_w, dir) = installed("install-io");
        let before = snapshot(&dir);
        let error = install_with(&dir, &new_files(), (None, None), &mut |step| {
            if step == failing {
                Err(Fault::Io(std::io::Error::from_raw_os_error(28)))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(
            crate::forvo::provider_code(&error),
            "install_io_failed",
            "{failing:?}"
        );
        if matches!(failing, Step::Swapped(_)) {
            assert_eq!(error.details["rolled_back"], true);
        } else {
            assert_eq!(error.details["changed"], false);
        }
        assert_eq!(snapshot(&dir), before, "{failing:?}");
    }
}

#[test]
fn committed_journal_is_removed_but_unowned_temp_files_are_preserved() {
    let (_w, dir) = installed("install-committed");
    install_with(&dir, &new_files(), (None, None), &mut |_| Ok(())).unwrap();
    // Фиксация записана, уборка не успела: прежние копии лежат рядом.
    std::fs::write(dir.join("background.js.update-old"), "old background").unwrap();
    std::fs::write(
        dir.join(JOURNAL),
        serde_json::to_vec(&json!({"schema_version":1,"phase":"committed","from_version":"1.0","to_version":"2.0",
            "entries":[{"path":"background.js","new_sha256":sha256_hex("new background"),"old_sha256":sha256_hex("old background")}]}))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        recover(&dir).unwrap().unwrap()["recovered"],
        "completed_commit"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "new background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js.update-old")).unwrap(),
        "old background"
    );
    // Без журнала нельзя доказать принадлежность временных файлов установщику.
    std::fs::write(dir.join("manifest.json.update-new"), "x").unwrap();
    std::fs::write(dir.join("icons/a.png.update-old"), "x").unwrap();
    assert!(recover(&dir).unwrap().is_none());
    assert_eq!(
        std::fs::read_to_string(dir.join("manifest.json.update-new")).unwrap(),
        "x"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("icons/a.png.update-old")).unwrap(),
        "x"
    );
    // Повреждённый журнал — отказ без изменения файлов.
    std::fs::write(dir.join(JOURNAL), "{").unwrap();
    assert_eq!(
        crate::forvo::provider_code(&recover(&dir).unwrap_err()),
        "install_journal_corrupt"
    );
}

#[test]
fn rollback_refuses_when_previous_files_were_changed_meanwhile() {
    let (_w, dir) = installed("install-tampered");
    let _ = install_with(&dir, &new_files(), (None, None), &mut |step| {
        if step == Step::Swapped(0) {
            Err(Fault::Crash)
        } else {
            Ok(())
        }
    });
    // Более поздняя копия подменена: проверка должна завершиться до замены
    // первой пригодной резервной копии.
    let journal: Journal =
        serde_json::from_slice(&std::fs::read(dir.join(JOURNAL)).unwrap()).unwrap();
    let damaged_backup = temp_path(
        "forvo_page.js",
        OLD_SUFFIX,
        journal.transaction_id.as_deref(),
    );
    let good_backup = temp_path(
        "background.js",
        OLD_SUFFIX,
        journal.transaction_id.as_deref(),
    );
    std::fs::write(dir.join(&damaged_backup), "подмена").unwrap();
    let error = recover(&dir).unwrap_err();
    assert_eq!(
        crate::forvo::provider_code(&error),
        "install_recovery_failed"
    );
    assert_eq!(error.details["reason"], "old_sha_mismatch");
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "new background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(&good_backup)).unwrap(),
        "old background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(damaged_backup)).unwrap(),
        "подмена"
    );
    assert!(dir.join(JOURNAL).exists());
}

#[test]
fn malicious_journal_paths_are_rejected_before_any_rollback_operation() {
    for bad_path in [
        "../outside.json",
        "/tmp/outside.json",
        "icons/../../outside.js",
    ] {
        let (_w, dir) = installed("install-malicious-journal");
        std::fs::write(dir.join("background.js"), "new background").unwrap();
        std::fs::write(dir.join("background.js.update-old"), "old background").unwrap();
        std::fs::write(
            dir.join(JOURNAL),
            serde_json::to_vec(&json!({
                "schema_version":1,
                "phase":"swapping",
                "from_version":"1.0",
                "to_version":"2.0",
                "entries":[
                    {"path":"background.js","new_sha256":sha256_hex("new background"),"old_sha256":sha256_hex("old background")},
                    {"path":bad_path,"new_sha256":sha256_hex("new"),"old_sha256":sha256_hex("old")}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let error = recover(&dir).unwrap_err();
        assert_eq!(
            crate::forvo::provider_code(&error),
            "install_journal_corrupt"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("background.js")).unwrap(),
            "new background",
            "{bad_path}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("background.js.update-old")).unwrap(),
            "old background",
            "{bad_path}"
        );
    }
}

#[test]
fn install_refuses_symlink_parent_without_writing_outside_the_extension() {
    let (_w, dir) = installed("install-symlink-parent");
    let outside = dir.parent().unwrap().join("outside-icons");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("icons")).unwrap();

    let error = install(&dir, &new_files(), (None, None)).unwrap_err();
    assert_eq!(crate::forvo::provider_code(&error), "install_io_failed");
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "old background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("forvo_page.js")).unwrap(),
        "old page"
    );
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    assert!(
        std::fs::symlink_metadata(dir.join("icons"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn recovery_preflights_symlinked_paths_before_restoring_other_entries() {
    let (_w, dir) = installed("install-recovery-symlink");
    let outside = dir.parent().unwrap().join("outside-icons");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("icons")).unwrap();
    std::fs::write(dir.join("background.js"), "new background").unwrap();
    let transaction_id = "test-transaction";
    let old_temp = temp_path("background.js", OLD_SUFFIX, Some(transaction_id));
    std::fs::write(dir.join(&old_temp), "old background").unwrap();
    std::fs::write(
        dir.join(JOURNAL),
        serde_json::to_vec(&json!({
            "schema_version":1,
            "phase":"swapping",
            "from_version":"1.0",
            "to_version":"2.0",
            "transaction_id":transaction_id,
            "entries":[
                {"path":"background.js","new_sha256":sha256_hex("new background"),"old_sha256":sha256_hex("old background")},
                {"path":"icons/a.png","new_sha256":sha256_hex("new icon"),"old_sha256":null}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let error = recover(&dir).unwrap_err();
    assert_eq!(
        crate::forvo::provider_code(&error),
        "install_recovery_failed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("background.js")).unwrap(),
        "new background"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(old_temp)).unwrap(),
        "old background"
    );
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn existing_root_alias_is_supported_without_following_inner_symlinks() {
    let workspace = TempWorkspace::create("install-symlink-root").unwrap();
    let outside = workspace.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let link = workspace.path().join("extension-link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    install(&link, &old_files(), (None, None)).unwrap();
    assert_eq!(differing(&link, &old_files()), Vec::<String>::new());
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(outside.join("manifest.json").is_file());
}
