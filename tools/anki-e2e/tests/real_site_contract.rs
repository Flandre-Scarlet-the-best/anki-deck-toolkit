use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const LABEL: &str = "io.github.anki-deck-toolkit.e2e.real-site";
const VOLUMES: [&str; 3] = [
    "anki-e2e-real-site-profile",
    "anki-e2e-real-site-state",
    "anki-e2e-real-site-downloads",
];

struct Fixture(PathBuf);
impl Fixture {
    fn new(state: &Value) -> Self {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let dir = std::env::temp_dir().join(format!("real-site-{:x}", u64::from_ne_bytes(random)));
        fs::create_dir_all(dir.join("work/real-site")).unwrap();
        fs::write(
            dir.join("docker"),
            include_str!("fixtures/real_site_docker.py"),
        )
        .unwrap();
        fs::set_permissions(dir.join("docker"), fs::Permissions::from_mode(0o755)).unwrap();
        // Путь к уже подготовленному профилю: тест не обращается в сеть.
        fs::write(
            dir.join("work/real-site/seccomp-brave-6416b4777078.json"),
            "{}",
        )
        .unwrap();
        fs::write(dir.join("state"), serde_json::to_vec(state).unwrap()).unwrap();
        Self(dir)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_anki-e2e"))
            .arg("real-site")
            .args(args)
            .arg("--work-dir")
            .arg(self.0.join("work"))
            .env(
                "PATH",
                format!("{}:{}", self.0.display(), std::env::var("PATH").unwrap()),
            )
            .env("DOCKER_FIXTURE_STATE", self.0.join("state"))
            .output()
            .unwrap()
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("state")).unwrap()).unwrap()
    }
    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.0.join("state.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn owned() -> Value {
    json!({"container": {"Config":{"Labels":{LABEL:"1"}},"State":{"Status":"running"},
        "NetworkSettings":{"Ports":{"6080/tcp":[{"HostIp":"127.0.0.1","HostPort":"6090"}],
        "5900/tcp":[{"HostIp":"127.0.0.1","HostPort":"5910"}]}}},
        "volumes": VOLUMES.into_iter().map(|v| (v.to_owned(), json!({LABEL:"1"}))).collect::<serde_json::Map<_,_>>()})
}
fn success(out: &Output) {
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn foreign_resources_are_never_used_or_removed() {
    for resource in 0..4 {
        for label in [Value::Null, json!("wrong")] {
            for args in [
                vec!["up"],
                vec!["down"],
                vec!["purge", "--yes"],
                vec!["status"],
            ] {
                let mut state = owned();
                let labels = if resource == 0 {
                    &mut state["container"]["Config"]["Labels"]
                } else {
                    &mut state["volumes"][VOLUMES[resource - 1]]
                };
                if label.is_null() {
                    labels.as_object_mut().unwrap().remove(LABEL);
                } else {
                    labels[LABEL] = label.clone();
                }
                let f = Fixture::new(&state);
                let out = f.run(&args);
                assert!(!out.status.success());
                assert!(String::from_utf8_lossy(&out.stderr).contains("конфликт владения"));
                assert_eq!(f.state(), state);
                assert!(
                    f.calls().iter().all(|a| matches!(
                        a[0].as_str(),
                        "version" | "info" | "container" | "volume"
                    ))
                );
            }
        }
    }
}

#[test]
fn running_up_and_status_use_actual_ports_and_reject_explicit_configuration() {
    let f = Fixture::new(&owned());
    for args in [vec!["up"], vec!["status"]] {
        let out = f.run(&args);
        success(&out);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("127.0.0.1:6090/vnc.html") && text.contains("127.0.0.1:5910"));
    }
    for args in [
        vec!["up", "--rebuild"],
        vec!["up", "--vnc-port", "6091"],
        vec!["up", "--rfb-port", "5911"],
        vec!["up", "--proxy", "http://proxy:8080"],
        vec!["up", "--no-proxy"],
    ] {
        let out = f.run(&args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("real-site down"));
    }
    assert_eq!(f.state(), owned());
    assert!(
        !f.calls()
            .iter()
            .any(|a| matches!(a[0].as_str(), "rm" | "run" | "build"))
    );
}

#[test]
fn owned_down_preserves_volumes_and_purge_removes_them() {
    let f = Fixture::new(&owned());
    success(&f.run(&["down"]));
    assert!(f.state()["container"].is_null());
    assert_eq!(f.state()["volumes"], owned()["volumes"]);
    success(&f.run(&["purge", "--yes"]));
    assert_eq!(f.state()["volumes"], json!({}));
}

#[test]
fn owned_creation_reuse_and_failed_container_removal() {
    let f = Fixture::new(&json!({"container":null,"volumes":{}}));
    success(&f.run(&[
        "up",
        "--vnc-port",
        "6090",
        "--rfb-port",
        "5910",
        "--no-proxy",
    ]));
    assert_eq!(f.state(), owned());
    success(&f.run(&["up"]));
    assert_eq!(f.calls().iter().filter(|a| a[0] == "run").count(), 1);
    // Сохранённые собственные тома повторно используются после down.
    success(&f.run(&["down"]));
    success(&f.run(&["up"]));
    assert_eq!(
        f.calls()
            .iter()
            .filter(|a| a.starts_with(&["volume".into(), "create".into()]))
            .count(),
        3
    );
    let mut state = owned();
    state["container"]["State"]["Status"] = json!("exited");
    state["remove_failure"] = json!(true);
    let f = Fixture::new(&state);
    let out = f.run(&["up"]);
    assert!(!out.status.success());
    assert_eq!(f.state(), state);
    assert!(!f.calls().iter().any(|a| a[0] == "run"));
}

#[test]
fn unreadable_or_non_loopback_ports_do_not_produce_an_address() {
    for ports in [
        Value::Null,
        json!({"6080/tcp":[{"HostIp":"0.0.0.0","HostPort":"6080"}]}),
    ] {
        let mut state = owned();
        state["container"]["NetworkSettings"]["Ports"] = ports;
        let f = Fixture::new(&state);
        for args in [vec!["up"], vec!["status"]] {
            let out = f.run(&args);
            assert!(!out.status.success());
            assert!(!String::from_utf8_lossy(&out.stdout).contains("http://"));
        }
    }
}
