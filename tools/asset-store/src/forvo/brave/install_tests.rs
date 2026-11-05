use super::*;
use crate::temp_workspace::TempWorkspace;

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
fn crash_at_any_step_recovers_to_previous_or_new_consistent_state() {
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
        // …и следующее восстановление возвращает прежнюю версию целиком.
        let recovered = recover(&dir).unwrap().unwrap();
        assert!(
            matches!(
                recovered["recovered"].as_str(),
                Some("discarded_staging" | "rolled_back")
            ),
            "{crash:?}: {recovered}"
        );
        assert_eq!(snapshot(&dir), before, "{crash:?}");
        // Повтор идемпотентен, а повторная установка после сбоя завершается.
        assert!(recover(&dir).unwrap().is_none());
        install(&dir, &new_files(), (None, None)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
            "{\"version\":\"2.0\"}"
        );
    }
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
fn committed_journal_and_stray_files_are_cleaned_up() {
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
    // Служебные файлы без журнала (сбой до его записи) удаляются, свои файлы — нет.
    std::fs::write(dir.join("manifest.json.update-new"), "x").unwrap();
    std::fs::write(dir.join("icons/a.png.update-old"), "x").unwrap();
    assert_eq!(
        recover(&dir).unwrap().unwrap()["recovered"],
        "stray_files_removed"
    );
    assert!(snapshot(&dir).iter().all(|(n, _)| !n.contains(".update-")));
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
    // Прежняя копия подменена: откат не может доказать прежнее содержимое.
    std::fs::write(dir.join("background.js.update-old"), "подмена").unwrap();
    let error = recover(&dir).unwrap_err();
    assert_eq!(
        crate::forvo::provider_code(&error),
        "install_recovery_failed"
    );
    assert_eq!(error.details["reason"], "old_sha_mismatch");
}
