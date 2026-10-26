use std::fs;
use std::process::Command;

#[test]
fn unavailable_docker_reports_cleanup_failure_and_still_clears_local_cache() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "anki-e2e-clean-{}-{:x}",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    let work = dir.join("work");
    for name in ["cache", "build", "runs"] {
        fs::create_dir_all(work.join(name)).unwrap();
        fs::write(work.join(name).join("fixture"), b"fixture").unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_anki-e2e"))
        .args(["clean", "--images", "--cache", "--docker-config", "inherit"])
        .arg("--work-dir")
        .arg(&work)
        .env("PATH", dir.join("no-executables"))
        .env("XDG_RUNTIME_DIR", dir.join("runtime"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("список контейнеров стенда"), "{stderr}");
    assert!(stderr.contains("список образов стенда"), "{stderr}");
    assert!(stderr.contains("BuildKit cache mounts"), "{stderr}");
    assert!(!work.join("cache").exists());
    assert!(!work.join("build").exists());
    assert_eq!(fs::read(work.join("runs/fixture")).unwrap(), b"fixture");
    fs::remove_dir_all(dir).unwrap();
}
