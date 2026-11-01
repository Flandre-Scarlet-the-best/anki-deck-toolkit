//! Real-Site стенд Forvo-Brave: Ubuntu-контейнер с настоящим Brave Stable для
//! Linux, графической сессией noVNC и `pronunciation-assets`, собранным из
//! снимка исходников. Стенд ходит на настоящий `forvo.com` без моков; вход
//! в аккаунт и проверки сайта пользователь проходит вручную через noVNC.
//!
//! Профиль Brave с авторизацией, сопряжение и пароль VNC живут в именованных
//! volumes стенда и удаляются только явным `real-site purge`. Ресурсы помечены
//! собственной меткой [`LABEL_REAL_SITE`], поэтому `clean` их не трогает.
//! В CI и `cargo test` стенд не запускается.

use crate::docker::{self, Docker};
use crate::report::Status;
use crate::source;
use crate::util::{
    Result, sha256_hex, unix_now, utc_compact, utc_rfc3339, write_atomic, write_json,
};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const CONTAINER: &str = "anki-e2e-real-site";
pub const LABEL_REAL_SITE: &str = "io.github.anki-deck-toolkit.e2e.real-site";
const IMAGE_REPO: &str = "anki-e2e/real-site";
/// Volume → точка монтирования. Профиль хранит авторизацию: это чувствительные данные.
pub const VOLUMES: [(&str, &str); 3] = [
    (
        "anki-e2e-real-site-profile",
        "/home/tester/.config/BraveSoftware",
    ),
    ("anki-e2e-real-site-state", "/home/tester/state"),
    ("anki-e2e-real-site-downloads", "/home/tester/Downloads"),
];
const DOCKERFILE: &str = include_str!("../docker/real-site.Dockerfile");
const ENTRYPOINT: &str = include_str!("../docker/real-site-entrypoint.sh");
const POLICY: &str = include_str!("../docker/real-site-policy.json");
/// Профиль seccomp Docker по умолчанию (moby/profiles, Apache-2.0), закреплённый
/// коммитом и SHA-256: стенд добавляет к нему одно правило, а не хранит копию.
const SECCOMP_URL: &str = "https://raw.githubusercontent.com/moby/profiles/6fe7deb1b9fb7c0397a4593480d7d22b9ee8caef/seccomp/default.json";
const SECCOMP_SHA256: &str = "6416b47770785a41ac59073cdc77d9fe98517df2799dc83ef207e622de3053f6";
/// `CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC`: песочнице Brave
/// нужны только user-, PID- и network-namespaces, остальные остаются запрещены.
const FORBIDDEN_NAMESPACES: u64 = 0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000;
const STATE_JSON: &str = "/home/tester/state/config/anki-deck-toolkit/forvo-brave/state.json";

#[derive(Debug, Args)]
pub struct RealSiteArgs {
    #[command(subcommand)]
    pub action: Action,
    /// Рабочий каталог (по умолчанию target/anki-e2e)
    #[arg(long, global = true)]
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Собрать образ, запустить контейнер и Brave; разовая настройка — через noVNC
    Up {
        /// Пересобрать образ без кэша
        #[arg(long)]
        rebuild: bool,
        /// Порт noVNC на 127.0.0.1 хоста
        #[arg(long, default_value_t = 6080)]
        vnc_port: u16,
        /// Порт VNC для нативного клиента на 127.0.0.1 хоста
        #[arg(long, default_value_t = 5900)]
        rfb_port: u16,
        /// HTTP-прокси для Brave; по умолчанию — прокси Docker из `docker info`, если он задан
        #[arg(long)]
        proxy: Option<String>,
        /// Не задавать Brave прокси
        #[arg(long, conflicts_with = "proxy")]
        no_proxy: bool,
    },
    /// doctor --forvo, smoke и production fetch/пакет на настоящем forvo.com; отчёт JSON/Markdown
    Run {
        #[arg(long, default_value = "元気")]
        surface: String,
        #[arg(long, default_value = "げんき")]
        reading: String,
        /// Второе слово небольшого пакета (вместе с --surface)
        #[arg(long, default_value = "猫")]
        batch_surface: String,
        #[arg(long, default_value = "ねこ")]
        batch_reading: String,
        /// Только doctor и smoke, без production fetch и пакета
        #[arg(long)]
        acceptance_only: bool,
    },
    /// Состояние контейнера, Brave и volumes
    Status,
    /// Штатно закрыть Brave и удалить контейнер; volumes сохраняются
    Down,
    /// Удалить контейнер, volumes (профиль с авторизацией) и образы стенда
    Purge {
        /// Подтверждение удаления авторизации и профиля
        #[arg(long)]
        yes: bool,
    },
}

/// Добавляет к профилю seccomp по умолчанию единственное правило: `clone` и
/// `unshare` разрешены, только если не запрошены mount-, cgroup-, UTS- и
/// IPC-namespaces. Остальные запреты профиля не меняются.
pub fn brave_seccomp(default_profile: &Value) -> Result<Value> {
    let mut profile = default_profile.clone();
    if profile["defaultAction"] != "SCMP_ACT_ERRNO" {
        return Err("профиль seccomp по умолчанию должен запрещать всё неразрешённое".into());
    }
    profile["syscalls"]
        .as_array_mut()
        .ok_or("в профиле seccomp нет syscalls")?
        .push(json!({
            "names": ["clone", "unshare"],
            "action": "SCMP_ACT_ALLOW",
            "args": [{"index": 0, "value": FORBIDDEN_NAMESPACES, "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}],
            "comment": "anki-e2e real-site: песочнице Brave нужны только user/pid/net namespaces"
        }));
    Ok(profile)
}

/// Прокси Docker Desktop (`docker info`) в виде URL для `--proxy-server`.
pub fn docker_proxy_url(info: &Value) -> Option<String> {
    ["HttpsProxy", "HttpProxy", "HTTPSProxy", "HTTPProxy"]
        .iter()
        .filter_map(|k| info[k].as_str())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(|v| {
            if v.contains("://") {
                v.to_owned()
            } else {
                format!("http://{v}")
            }
        })
}

/// Итог одной команды `pronunciation-assets` по её машинному выводу.
pub fn command_status(output: &Value) -> Status {
    match (
        output["outcome"].as_str(),
        output["error"]["details"]["code"].as_str(),
    ) {
        (Some("ok"), _) => Status::Pass,
        (_, Some("acceptance_blocked")) => Status::Blocked,
        (_, Some("acceptance_failed")) => Status::Fail,
        (_, Some("acceptance_interrupted")) => Status::Interrupted,
        _ => Status::Error,
    }
}

/// Шаги отчёта `doctor`/`smoke` (из `result` или `error.details.report`).
pub fn steps(output: &Value) -> Vec<Value> {
    let report = if output["outcome"] == "ok" {
        &output["result"]
    } else {
        &output["error"]["details"]["report"]
    };
    report["steps"].as_array().cloned().unwrap_or_default()
}

fn platform(info: &Value) -> Result<&'static str> {
    match info["Architecture"].as_str() {
        Some("x86_64") => Ok("linux/amd64"),
        Some("aarch64") => Ok("linux/arm64"),
        other => Err(format!(
            "архитектура Docker {other:?} не поддерживается Brave для Linux"
        )),
    }
}

struct Stand {
    docker: Docker,
    work: PathBuf,
    repo: PathBuf,
}

impl Stand {
    fn exec(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut full = vec!["exec", CONTAINER];
        full.extend_from_slice(args);
        self.docker.output(&full)
    }

    fn container_state(&self) -> Option<String> {
        self.docker
            .text(&["inspect", "--format", "{{.State.Status}}", CONTAINER])
            .ok()
    }

    fn running(&self) -> bool {
        self.container_state().as_deref() == Some("running")
    }

    fn brave_version(&self) -> Option<String> {
        self.exec(&["real-site-entrypoint", "version"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }

    /// Песочница Brave: renderer в собственном user namespace. Без неё стенд
    /// не выдаётся за рабочий.
    fn sandbox(&self) -> Value {
        let check = || -> Option<Value> {
            for _ in 0..20 {
                let out = self.exec(&["real-site-entrypoint", "sandbox-check"]).ok()?;
                let value: Value = serde_json::from_slice(&out.stdout).ok()?;
                if value["renderers"].as_u64().unwrap_or(0) > 0 {
                    return Some(value);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            None
        };
        let value = check().unwrap_or_else(|| json!({"renderers":0,"isolated":0}));
        let ok = value["renderers"].as_u64() > Some(0) && value["isolated"] == value["renderers"];
        json!({"status": if ok { "PASS" } else { "FAIL" }, "renderers": value["renderers"], "isolated": value["isolated"]})
    }

    fn start_brave(&self) -> Result<()> {
        let out = self.exec(&["real-site-entrypoint", "brave"])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    fn seccomp_profile(&self) -> Result<PathBuf> {
        let dir = self.work.join("real-site");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("seccomp-brave-{}.json", &SECCOMP_SHA256[..12]));
        if path.exists() {
            return Ok(path);
        }
        let client = reqwest::blocking::Client::builder()
            .user_agent("anki-deck-toolkit-e2e")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let bytes = client
            .get(SECCOMP_URL)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("профиль seccomp: {e}"))?;
        let actual = sha256_hex(&bytes);
        if actual != SECCOMP_SHA256 {
            return Err(format!(
                "профиль seccomp: SHA-256 {actual} не совпадает с закреплённым {SECCOMP_SHA256}"
            ));
        }
        let default: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("профиль seccomp: {e}"))?;
        write_json(&path, &brave_seccomp(&default)?)?;
        Ok(path)
    }

    fn build_image(
        &self,
        info: &Value,
        rebuild: bool,
        interrupted: &AtomicBool,
    ) -> Result<(String, source::Provenance)> {
        let build = self.work.join("real-site").join("build");
        let staging = build.join(format!("src-staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let provenance = source::snapshot(&self.repo, &staging.join("src"), &self.work)?;
        let short = |s: &str| s.chars().take(12).collect::<String>();
        let context = build.join(format!("src-{}", short(&provenance.tree_sha256)));
        if context.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        } else {
            std::fs::rename(&staging, &context).map_err(|e| e.to_string())?;
        }
        let rust_image = format!("rust:{}-bookworm", provenance.msrv);
        let key = sha256_hex(
            format!(
                "{DOCKERFILE}{ENTRYPOINT}{POLICY}{}{rust_image}",
                crate::host::BASE_IMAGE
            )
            .as_bytes(),
        );
        let tag = format!(
            "{IMAGE_REPO}:{}-{}",
            short(&provenance.tree_sha256),
            short(&key)
        );
        if !rebuild && self.docker.image_id(&tag).is_some() {
            println!("Образ {tag} уже собран из того же снимка исходников");
            return Ok((tag, provenance));
        }
        for (name, text) in [
            ("Dockerfile", DOCKERFILE),
            ("real-site-entrypoint.sh", ENTRYPOINT),
            ("real-site-policy.json", POLICY),
        ] {
            std::fs::write(context.join(name), text).map_err(|e| e.to_string())?;
        }
        println!("Сборка образа {tag} (Brave Stable и pronunciation-assets из снимка)…");
        let log = self.work.join("real-site").join("build.log");
        self.docker.build(&docker::Build {
            dockerfile: &context.join("Dockerfile"),
            context: &context,
            tag: &tag,
            args: &[
                ("BASE_IMAGE".into(), crate::host::BASE_IMAGE.into()),
                ("RUST_IMAGE".into(), rust_image.clone()),
                ("RUST_VERSION".into(), provenance.msrv.clone()),
                ("SOURCE_COMMIT".into(), provenance.commit.clone()),
                ("SOURCE_TREE_SHA256".into(), provenance.tree_sha256.clone()),
            ],
            labels: &[(LABEL_REAL_SITE.into(), "1".into())],
            platform: platform(info)?,
            log: &log,
            no_cache: rebuild,
            interrupted: &|| interrupted.load(Ordering::SeqCst),
        })?;
        Ok((tag, provenance))
    }

    /// Старые образы стенда, кроме текущего; используемые контейнером Docker не удалит.
    fn prune_images(&self, keep: &str) {
        let keep_id = self.docker.image_id(keep).unwrap_or_default();
        let filter = format!("label={LABEL_REAL_SITE}");
        if let Ok(text) = self
            .docker
            .text(&["images", "--filter", &filter, "--format", "{{.ID}}"])
        {
            for id in text.lines().filter(|id| !keep_id.contains(*id)) {
                let _ = self.docker.text(&["image", "rm", id]);
            }
        }
    }
}

fn stand(args: &RealSiteArgs) -> std::result::Result<(Stand, Value), (Status, String)> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = source::repository_root(&cwd).unwrap_or(cwd);
    let work = args
        .work_dir
        .clone()
        .unwrap_or_else(|| repo.join("target").join("anki-e2e"));
    let docker = Docker::new(None);
    docker
        .preflight()
        .map_err(|stop| (stop.status, format!("{}: {}", stop.reason, stop.message)))?;
    // Полный `docker info`: архитектура и прокси Docker Desktop.
    let info = docker
        .json(&["info", "--format", "{{json .}}"])
        .map_err(|e| (Status::Blocked, e))?;
    Ok((Stand { docker, work, repo }, info))
}

pub fn run(args: &RealSiteArgs, interrupted: Arc<AtomicBool>) -> u8 {
    let (stand, info) = match stand(args) {
        Ok(found) => found,
        Err((status, message)) => {
            eprintln!("Docker недоступен: {message}");
            return status.exit_code();
        }
    };
    let result = match &args.action {
        Action::Up {
            rebuild,
            vnc_port,
            rfb_port,
            proxy,
            no_proxy,
        } => up(
            &stand,
            &info,
            *rebuild,
            (*vnc_port, *rfb_port),
            proxy.clone(),
            *no_proxy,
            &interrupted,
        ),
        Action::Run {
            surface,
            reading,
            batch_surface,
            batch_reading,
            acceptance_only,
        } => acceptance(
            &stand,
            [(surface, reading), (batch_surface, batch_reading)],
            *acceptance_only,
            &interrupted,
        ),
        Action::Status => status(&stand),
        Action::Down => down(&stand),
        Action::Purge { yes } => purge(&stand, *yes),
    };
    match result {
        Ok(status) => status.exit_code(),
        Err((status, message)) => {
            eprintln!("{}: {message}", status.as_str());
            status.exit_code()
        }
    }
}

type Outcome = std::result::Result<Status, (Status, String)>;

fn error(message: impl Into<String>) -> (Status, String) {
    (Status::Error, message.into())
}

fn up(
    stand: &Stand,
    info: &Value,
    rebuild: bool,
    (vnc_port, rfb_port): (u16, u16),
    proxy: Option<String>,
    no_proxy: bool,
    interrupted: &AtomicBool,
) -> Outcome {
    if !stand.running() {
        let (tag, provenance) = stand
            .build_image(info, rebuild, interrupted)
            .map_err(error)?;
        let seccomp = stand.seccomp_profile().map_err(error)?;
        let proxy = if no_proxy {
            None
        } else {
            proxy.or_else(|| docker_proxy_url(info))
        };
        for (volume, _) in VOLUMES {
            if stand.docker.text(&["volume", "inspect", volume]).is_err() {
                stand
                    .docker
                    .text(&[
                        "volume",
                        "create",
                        "--label",
                        &format!("{LABEL_REAL_SITE}=1"),
                        volume,
                    ])
                    .map_err(error)?;
            }
        }
        let _ = stand.docker.remove_container(CONTAINER);
        let mut run: Vec<String> = [
            "run",
            "-d",
            "--init",
            "--name",
            CONTAINER,
            "--hostname",
            CONTAINER,
            "--shm-size",
            "1g",
            // Процесс uid 1000 не получает действующих capabilities; SYS_CHROOT в
            // bounding set нужен песочнице Brave внутри её user namespace.
            "--cap-drop",
            "ALL",
            "--cap-add",
            "SYS_CHROOT",
            "--security-opt",
            "no-new-privileges",
        ]
        .map(str::to_owned)
        .to_vec();
        run.extend([
            "--label".into(),
            format!("{LABEL_REAL_SITE}=1"),
            "--security-opt".into(),
            format!("seccomp={}", seccomp.display()),
            "-p".into(),
            format!("127.0.0.1:{vnc_port}:6080"),
            "-p".into(),
            format!("127.0.0.1:{rfb_port}:5900"),
        ]);
        for (volume, target) in VOLUMES {
            run.extend(["-v".into(), format!("{volume}:{target}")]);
        }
        if let Some(proxy) = &proxy {
            run.extend(["-e".into(), format!("REAL_SITE_PROXY={proxy}")]);
        }
        run.push(tag.clone());
        let refs: Vec<&str> = run.iter().map(String::as_str).collect();
        stand.docker.text(&refs).map_err(error)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let logs = stand.docker.output(&["logs", CONTAINER]).map_err(error)?;
            if String::from_utf8_lossy(&logs.stdout).contains("real-site session ready") {
                break;
            }
            if Instant::now() >= deadline || !stand.running() {
                return Err(error(format!(
                    "графическая сессия не запустилась: {}",
                    String::from_utf8_lossy(&logs.stderr).trim()
                )));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        stand.prune_images(&tag);
        println!(
            "Контейнер {CONTAINER} запущен: образ {tag}, исходники {} (dirty={}), прокси Brave: {}",
            provenance.commit,
            provenance.dirty,
            proxy.as_deref().unwrap_or("нет")
        );
    }
    // setup до запуска Brave: каталог расширения для --load-extension и сопряжение.
    let setup = stand
        .exec(&["pronunciation-assets", "forvo", "brave", "setup"])
        .map_err(error)?;
    let setup: Value = serde_json::from_slice(&setup.stdout)
        .map_err(|_| error("pronunciation-assets setup вернул не JSON"))?;
    if setup["outcome"] != "ok" {
        return Err(error(format!(
            "pronunciation-assets setup: {}",
            setup["error"]
        )));
    }
    stand.start_brave().map_err(error)?;
    let sandbox = stand.sandbox();
    if sandbox["status"] != "PASS" {
        return Err(error(format!("песочница Brave не включена: {sandbox}")));
    }
    println!(
        "Песочница Brave: renderer в собственном user namespace ({})",
        sandbox["isolated"]
    );
    println!("Brave: {}", stand.brave_version().unwrap_or_default());
    println!("noVNC: http://127.0.0.1:{vnc_port}/vnc.html; VNC-клиент: 127.0.0.1:{rfb_port}");
    println!("Пароль VNC: docker exec {CONTAINER} cat /home/tester/state/vnc.txt");
    println!(
        "Расширение загружается автоматически (--load-extension). При первом запуске войдите в Forvo через VNC. Дальше: anki-e2e real-site run"
    );
    Ok(Status::Pass)
}

/// Команда `pronunciation-assets` внутри контейнера с прерыванием по Ctrl+C.
fn exec_cli(
    stand: &Stand,
    args: &[&str],
    interrupted: &AtomicBool,
) -> std::result::Result<Value, String> {
    let mut command = stand.docker.command();
    command
        .args(["exec", CONTAINER, "timeout", "300", "pronunciation-assets"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = command.spawn().map_err(|e| format!("docker exec: {e}"))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    loop {
        if interrupted.load(Ordering::SeqCst) {
            // Ctrl+C терминала не доходит до процесса в контейнере: доставляем SIGINT сами.
            let _ = stand.exec(&["pkill", "-INT", "-f", "^pronunciation-assets "]);
        }
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(output) => {
                let output = output.map_err(|e| e.to_string())?;
                return serde_json::from_slice(&output.stdout).map_err(|_| {
                    format!(
                        "pronunciation-assets вернул не JSON (код {:?})",
                        output.status.code()
                    )
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn hint(status: Status, steps: &[Value]) -> Option<&'static str> {
    let blocked = |name: &str| {
        steps
            .iter()
            .find(|s| s["name"] == name)
            .is_some_and(|s| s["status"] != "PASS" && s["status"] != "NOT_TESTED")
    };
    if status == Status::Pass {
        None
    } else if blocked("extension_connected") || blocked("transport") {
        Some("расширение не подключено: загрузите его через noVNC (см. anki-e2e real-site up)")
    } else if blocked("forvo_auth") || blocked("forvo_page_auth") {
        Some("нужен вход в Forvo или ручное прохождение проверки сайта через noVNC")
    } else {
        None
    }
}

/// Итог production-команды (`fetch`, `batch-*`) по её машинному выводу.
pub fn production_status(output: &Value) -> Status {
    const BLOCKERS: &[&str] = &[
        "challenge_required",
        "login_required",
        "session_expired",
        "access_denied",
        "rate_limited",
        "extension_not_connected",
        "save_as_required",
        "download_conflict",
    ];
    let error = output["error"]["details"]["code"].as_str();
    match output["outcome"].as_str() {
        Some("ok") => Status::Pass,
        Some("paused") if output["result"]["paused"]["code"] == "interrupted" => {
            Status::Interrupted
        }
        Some("paused") => Status::Blocked,
        Some("cancelled") => Status::Interrupted,
        _ if output["error"]["code"] == "interrupted" => Status::Interrupted,
        _ if error.is_some_and(|c| BLOCKERS.contains(&c)) => Status::Blocked,
        _ => Status::Fail,
    }
}

fn step(name: &str, ok: bool, evidence: Value) -> Value {
    json!({"name": name, "status": if ok { "PASS" } else { "FAIL" }, "evidence": evidence})
}

/// Число собственных загрузок, начатых запуском, по его журналу в контейнере.
fn downloads_started(stand: &Stand, output: &Value) -> Option<u64> {
    let log = output["result"]["diagnostic_log"]
        .as_str()
        .or_else(|| output["error"]["details"]["diagnostic_log"].as_str())?;
    let out = stand
        .exec(&["grep", "-c", "brave_download_activated", log])
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn items(output: &Value) -> Vec<Value> {
    output["result"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Production-путь на настоящем Forvo: одиночный fetch и пакет из двух слов
/// в отдельные тестовые хранилища на volume состояния (не в Git и не в
/// пользовательский AssetStore). Пакет начинается с `--max-items 1`, затем
/// продолжается и повторно возобновляется: завершённое не скачивается снова.
fn production(
    stand: &Stand,
    run_id: &str,
    words: [(&str, &str); 2],
    interrupted: &AtomicBool,
) -> Vec<Value> {
    let root = format!("/home/tester/state/real-site-stores/{run_id}");
    let fetch_store = format!("{root}/fetch");
    let batch_store = format!("{root}/batch");
    let mut commands = Vec::new();
    let mut entry = |name: &str, output: &std::result::Result<Value, String>, extra: Vec<Value>| {
        let status = output.as_ref().map_or(Status::Error, production_status);
        let status = if extra.iter().any(|s| s["status"] == "FAIL") && status == Status::Pass {
            Status::Fail
        } else {
            status
        };
        let mut steps = vec![json!({"name": name, "status": status.as_str(),
            "evidence": output.as_ref().map(|o| json!({"outcome": o["outcome"], "code": o["error"]["details"]["code"],
                "paused": o["result"]["paused"]["code"], "next_action": o["error"]["details"]["next_action"]})).unwrap_or(Value::Null)})];
        if status == Status::Pass || !extra.is_empty() {
            steps.extend(extra);
        }
        commands.push(json!({"name": name, "status": status.as_str(), "steps": steps}));
        status
    };
    let (surface, reading) = words[0];
    let fetch = exec_cli(
        stand,
        &[
            "--store",
            &fetch_store,
            "forvo",
            "brave",
            "fetch",
            "--surface",
            surface,
            "--reading",
            reading,
        ],
        interrupted,
    );
    let mut checks = Vec::new();
    if let Ok(out) = &fetch
        && out["outcome"] == "ok"
    {
        let r = &out["result"];
        checks.push(step("fetch_imported", r["asset"]["sha256"] == r["acquisition"]["downloaded_sha256"]
            && r["asset"]["lifecycle"] != "verified" && r["transport"] == "brave_extension",
            json!({"sha256": r["asset"]["sha256"], "record_id": r["acquisition"]["selected_record_id"],
                "attempts": r["acquisition"]["attempts"], "lifecycle": r["asset"]["lifecycle"],
                "byte_length": r["asset"]["byte_length"], "store": fetch_store})));
        checks.push(step("fetch_browser_clean", r["browser"]["user_tabs_untouched"] == true
            && r["browser"]["operations"].as_array().is_some_and(|ops| ops.iter().all(|o| o["release"]["released"] == true)),
            json!({"downloads": r["browser"]["downloads"], "user_tabs_untouched": r["browser"]["user_tabs_untouched"],
                "recovered_previous": r["browser"]["recovered_previous"]})));
        let verify = exec_cli(stand, &["--store", &fetch_store, "verify"], interrupted);
        checks.push(step(
            "store_verify",
            verify.as_ref().is_ok_and(|v| v["outcome"] == "ok"),
            json!({"outcome": verify.as_ref().ok().map(|v| v["outcome"].clone())}),
        ));
    }
    if entry("fetch", &fetch, checks) != Status::Pass || interrupted.load(Ordering::SeqCst) {
        return commands;
    }
    let plan = format!("{root}/plan.json");
    let plan_json = json!({"schema_version": 1, "items": words.iter().map(|(s, r)| json!({"surface": s, "reading": r})).collect::<Vec<_>>()}).to_string();
    if stand
        .exec(&[
            "sh",
            "-c",
            "mkdir -p \"$(dirname \"$1\")\" && printf '%s' \"$2\" > \"$1\"",
            "sh",
            &plan,
            &plan_json,
        ])
        .map(|o| o.status.success())
        != Ok(true)
    {
        entry(
            "batch_start",
            &Err("план пакета не записан".into()),
            Vec::new(),
        );
        return commands;
    }
    let batch = ["--store", batch_store.as_str(), "forvo", "brave"];
    let start = exec_cli(
        stand,
        &[
            &batch[..],
            &[
                "batch-start",
                "--batch-id",
                "real-site",
                "--plan",
                &plan,
                "--max-items",
                "1",
            ],
        ]
        .concat(),
        interrupted,
    );
    let first = start.as_ref().map(items).unwrap_or_default();
    let mut checks = Vec::new();
    if start.as_ref().is_ok_and(|o| o["outcome"] == "ok") {
        checks.push(step("partial_progress", first.len() == 2 && first[0]["outcome"]["code"] == "imported" && first[1]["outcome"].is_null(),
            json!({"items": first.iter().map(|i| &i["outcome"]["code"]).collect::<Vec<_>>(),
                "downloads_started": start.as_ref().ok().and_then(|o| downloads_started(stand, o))})));
    }
    if entry("batch_start", &start, checks) != Status::Pass || interrupted.load(Ordering::SeqCst) {
        return commands;
    }
    let resume = exec_cli(
        stand,
        &[&batch[..], &["batch-resume", "--batch-id", "real-site"]].concat(),
        interrupted,
    );
    let done = resume.as_ref().map(items).unwrap_or_default();
    let mut checks = Vec::new();
    if resume.as_ref().is_ok_and(|o| o["outcome"] == "ok") {
        let started = resume
            .as_ref()
            .ok()
            .and_then(|o| downloads_started(stand, o));
        let attempts = done.get(1).and_then(|i| {
            i["acquisition"]["evidence"]["attempts"]
                .as_array()
                .map(Vec::len)
        });
        checks.push(step("resume_completes_without_redownload",
            done.iter().all(|i| i["outcome"]["code"] == "imported")
                && done.first().map(|i| &i["acquisition"]) == first.first().map(|i| &i["acquisition"])
                && started.is_some() && started.map(|n| n as usize) == attempts,
            json!({"items": done.iter().map(|i| json!({"surface": i["request"]["surface"], "code": i["outcome"]["code"],
                "sha256": i["acquisition"]["blob"]["sha256"], "record_id": i["acquisition"]["evidence"]["selected_record_id"]})).collect::<Vec<_>>(),
                "downloads_started": started, "second_item_attempts": attempts})));
    }
    if entry("batch_resume", &resume, checks) != Status::Pass || interrupted.load(Ordering::SeqCst)
    {
        return commands;
    }
    let again = exec_cli(
        stand,
        &[&batch[..], &["batch-resume", "--batch-id", "real-site"]].concat(),
        interrupted,
    );
    let status = exec_cli(
        stand,
        &[
            "--store",
            &batch_store,
            "forvo",
            "batch-status",
            "--batch-id",
            "real-site",
        ],
        interrupted,
    );
    let checks = vec![step(
        "finished_batch_untouched",
        again.as_ref().is_ok_and(|o| {
            o["result"]["revision"]
                == resume
                    .as_ref()
                    .map(|r| r["result"]["revision"].clone())
                    .unwrap_or_default()
        }) && again
            .as_ref()
            .ok()
            .and_then(|o| downloads_started(stand, o))
            == Some(0)
            && status
                .as_ref()
                .is_ok_and(|s| s["result"]["effective_transport"] == "brave_extension"),
        json!({"revision": again.as_ref().ok().map(|o| o["result"]["revision"].clone()),
            "downloads_started": again.as_ref().ok().and_then(|o| downloads_started(stand, o)),
            "transport": status.as_ref().ok().map(|s| s["result"]["effective_transport"].clone())}),
    )];
    let verify = exec_cli(stand, &["--store", &batch_store, "verify"], interrupted);
    let mut checks = checks;
    checks.push(step("store_verify", verify.as_ref().is_ok_and(|v| v["outcome"] == "ok"),
        json!({"outcome": verify.as_ref().ok().map(|v| v["outcome"].clone()), "store": batch_store})));
    entry("batch_resume_again", &again, checks);
    commands
}

fn acceptance(
    stand: &Stand,
    words: [(&String, &String); 2],
    acceptance_only: bool,
    interrupted: &AtomicBool,
) -> Outcome {
    let (surface, reading) = (words[0].0.as_str(), words[0].1.as_str());
    if !stand.running() {
        return Err((
            Status::Blocked,
            "стенд не запущен: anki-e2e real-site up".into(),
        ));
    }
    stand.start_brave().map_err(error)?;
    let sandbox = stand.sandbox();
    let started = unix_now();
    let run_id = format!(
        "{}-{}",
        utc_compact(started),
        crate::util::random_hex(3).map_err(error)?
    );
    let dir = stand.work.join("real-site-runs").join(&run_id);
    std::fs::create_dir_all(&dir).map_err(|e| error(e.to_string()))?;
    let mut commands = Vec::new();
    let doctor = exec_cli(stand, &["forvo", "brave", "doctor", "--forvo"], interrupted);
    let doctor_status = doctor.as_ref().map_or(Status::Error, command_status);
    commands.push(command_entry("doctor", &doctor, doctor_status));
    if doctor_status == Status::Pass && !interrupted.load(Ordering::SeqCst) {
        let smoke = exec_cli(
            stand,
            &[
                "forvo",
                "brave",
                "smoke",
                "--surface",
                surface,
                "--reading",
                reading,
                "--close-tab",
            ],
            interrupted,
        );
        let status = smoke.as_ref().map_or(Status::Error, command_status);
        commands.push(command_entry("smoke", &smoke, status));
        if status == Status::Pass && !acceptance_only && !interrupted.load(Ordering::SeqCst) {
            commands.extend(production(
                stand,
                &run_id,
                [
                    (surface, reading),
                    (words[1].0.as_str(), words[1].1.as_str()),
                ],
                interrupted,
            ));
        }
    }
    let mut overall = crate::report::overall(
        commands
            .iter()
            .filter_map(|c| c["status"].as_str().and_then(Status::parse)),
    );
    if sandbox["status"] != "PASS" {
        overall = crate::report::overall([overall, Status::Fail]);
    }
    if interrupted.load(Ordering::SeqCst) {
        overall = Status::Interrupted;
    }
    let image = stand
        .docker
        .json(&["inspect", "--format", "{{json .Config}}", CONTAINER])
        .unwrap_or(Value::Null);
    let all_steps: Vec<Value> = commands
        .iter()
        .flat_map(|c| c["steps"].as_array().cloned().unwrap_or_default())
        .collect();
    let mut report = json!({
        "kind": "real_site",
        "run_id": run_id,
        "started": utc_rfc3339(started),
        "finished": utc_rfc3339(unix_now()),
        "status": overall.as_str(),
        "hint": hint(overall, &all_steps),
        "request": {"surface": surface, "reading": reading},
        "batch_words": words.iter().map(|(s, r)| json!({"surface": s, "reading": r})).collect::<Vec<_>>(),
        "test_stores": if acceptance_only { Value::Null } else { json!(format!("/home/tester/state/real-site-stores/{run_id}")) },
        "brave": stand.brave_version(),
        "sandbox": sandbox,
        "image": {"name": image["Image"], "source_commit": image["Labels"]["io.github.anki-deck-toolkit.e2e.source-commit"],
            "source_tree_sha256": image["Labels"]["io.github.anki-deck-toolkit.e2e.source-tree-sha256"]},
        "commands": commands,
    });
    // Секрет сопряжения читается только для сверки и в отчёт не попадает.
    let secret = stand
        .exec(&["jq", "-r", ".secret // empty", STATE_JSON])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| s.len() >= 32);
    let leaked = secret
        .as_deref()
        .is_some_and(|s| report.to_string().contains(s));
    report["secret_check"] = json!(match (&secret, leaked) {
        (_, true) => "FAIL",
        (Some(_), false) => "PASS",
        (None, false) => "NOT_TESTED",
    });
    if leaked {
        return Err(error(
            "секрет сопряжения обнаружен в отчёте; отчёт не записан",
        ));
    }
    write_json(&dir.join("report.json"), &report).map_err(error)?;
    write_atomic(&dir.join("report.md"), render_markdown(&report).as_bytes()).map_err(error)?;
    for command in report["commands"].as_array().into_iter().flatten() {
        println!(
            "{} {}",
            command["status"].as_str().unwrap_or("?"),
            command["name"].as_str().unwrap_or("?")
        );
        for step in command["steps"].as_array().into_iter().flatten() {
            println!(
                "  {} {}",
                step["status"].as_str().unwrap_or("?"),
                step["name"].as_str().unwrap_or("?")
            );
        }
    }
    if let Some(hint) = report["hint"].as_str() {
        println!("Подсказка: {hint}");
    }
    println!(
        "Итог: {} — {}",
        overall.as_str(),
        dir.join("report.md").display()
    );
    Ok(overall)
}

fn command_entry(name: &str, output: &std::result::Result<Value, String>, status: Status) -> Value {
    match output {
        Ok(value) => json!({"name": name, "status": status.as_str(),
            "code": value["error"]["code"], "steps": steps(value)}),
        Err(e) => json!({"name": name, "status": status.as_str(), "error": e, "steps": []}),
    }
}

pub fn render_markdown(report: &Value) -> String {
    let mut out = format!(
        "# Real-Site E2E Forvo-Brave\n\n- Итог: **{}**\n- Прогон: `{}` ({})\n- Brave: {} (песочница {})\n- Образ: `{}`, исходники `{}`\n- Запрос: {} / {}\n- Секрет сопряжения в отчёте: {}\n",
        report["status"].as_str().unwrap_or("?"),
        report["run_id"].as_str().unwrap_or("?"),
        report["started"].as_str().unwrap_or("?"),
        report["brave"].as_str().unwrap_or("?"),
        report["sandbox"]["status"].as_str().unwrap_or("?"),
        report["image"]["name"].as_str().unwrap_or("?"),
        report["image"]["source_commit"].as_str().unwrap_or("?"),
        report["request"]["surface"].as_str().unwrap_or("?"),
        report["request"]["reading"].as_str().unwrap_or("?"),
        report["secret_check"].as_str().unwrap_or("?"),
    );
    if let Some(hint) = report["hint"].as_str() {
        out.push_str(&format!("- Подсказка: {hint}\n"));
    }
    for command in report["commands"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\n## {} — {}\n\n| Шаг | Статус |\n|---|---|\n",
            command["name"].as_str().unwrap_or("?"),
            command["status"].as_str().unwrap_or("?")
        ));
        for step in command["steps"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "| `{}` | {} |\n",
                step["name"].as_str().unwrap_or("?"),
                step["status"].as_str().unwrap_or("?")
            ));
        }
    }
    out
}

fn status(stand: &Stand) -> Outcome {
    let state = stand.container_state();
    println!(
        "Контейнер {CONTAINER}: {}",
        state.as_deref().unwrap_or("отсутствует")
    );
    if stand.running() {
        let brave = stand
            .exec(&["pgrep", "-c", "-f", "^/opt/brave.com/brave/brave"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        println!(
            "Процессов Brave: {brave}; {}",
            stand.brave_version().unwrap_or_default()
        );
        if let Ok(ports) = stand.docker.text(&["port", CONTAINER, "6080"]) {
            println!(
                "noVNC: http://{}/vnc.html",
                ports.lines().next().unwrap_or("")
            );
        }
    }
    for (volume, _) in VOLUMES {
        let exists = stand.docker.text(&["volume", "inspect", volume]).is_ok();
        println!("Volume {volume}: {}", if exists { "есть" } else { "нет" });
    }
    Ok(Status::Pass)
}

fn down(stand: &Stand) -> Outcome {
    if stand.container_state().is_none() {
        println!("Контейнер {CONTAINER} не найден");
        return Ok(Status::Pass);
    }
    if stand.running() {
        // Штатное закрытие сохраняет сессию и профиль; иначе Brave предложит восстановление.
        if let Ok(out) = stand.exec(&["real-site-entrypoint", "stop-brave"])
            && !out.status.success()
        {
            eprintln!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    stand.docker.remove_container(CONTAINER).map_err(error)?;
    println!("Контейнер {CONTAINER} удалён; volumes с профилем сохранены");
    Ok(Status::Pass)
}

fn purge(stand: &Stand, yes: bool) -> Outcome {
    if !yes {
        println!("Будут удалены контейнер, образы и volumes (профиль Brave с авторизацией Forvo):");
        for (volume, _) in VOLUMES {
            println!("  {volume}");
        }
        println!("Повторите с --yes");
        return Ok(Status::Blocked);
    }
    down(stand)?;
    let mut failures = Vec::new();
    for (volume, _) in VOLUMES {
        if stand.docker.text(&["volume", "inspect", volume]).is_ok()
            && let Err(e) = stand.docker.text(&["volume", "rm", volume])
        {
            failures.push(e);
        }
    }
    let filter = format!("label={LABEL_REAL_SITE}");
    if let Ok(ids) = stand
        .docker
        .text(&["images", "--filter", &filter, "--format", "{{.ID}}"])
    {
        for id in ids.lines() {
            if let Err(e) = stand.docker.text(&["image", "rm", "-f", id]) {
                failures.push(e);
            }
        }
    }
    let _ = std::fs::remove_dir_all(stand.work.join("real-site"));
    if failures.is_empty() {
        println!("Стенд удалён полностью");
        Ok(Status::Pass)
    } else {
        Err(error(failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seccomp_adds_only_masked_namespace_rule() {
        let default = json!({"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["read"],"action":"SCMP_ACT_ALLOW"}]});
        let profile = brave_seccomp(&default).unwrap();
        let rules = profile["syscalls"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], default["syscalls"][0]);
        assert_eq!(rules[1]["names"], json!(["clone", "unshare"]));
        assert_eq!(rules[1]["args"][0]["op"], "SCMP_CMP_MASKED_EQ");
        assert_eq!(rules[1]["args"][0]["value"], 0x0E02_0000);
        assert_eq!(rules[1]["args"][0]["valueTwo"], 0);
        assert!(!profile.to_string().contains("setns"));
        let permissive = json!({"defaultAction":"SCMP_ACT_ALLOW","syscalls":[]});
        assert!(brave_seccomp(&permissive).is_err());
    }

    #[test]
    fn docker_desktop_proxy_becomes_url() {
        assert_eq!(
            docker_proxy_url(&json!({"HttpProxy":"http.docker.internal:3128","HttpsProxy":""})),
            Some("http://http.docker.internal:3128".into())
        );
        assert_eq!(
            docker_proxy_url(&json!({"HTTPSProxy":"http://proxy:8080"})),
            Some("http://proxy:8080".into())
        );
        assert_eq!(docker_proxy_url(&json!({"HTTPProxy":""})), None);
    }

    #[test]
    fn production_outcomes_map_to_statuses() {
        assert_eq!(production_status(&json!({"outcome":"ok"})), Status::Pass);
        assert_eq!(
            production_status(
                &json!({"outcome":"paused","result":{"paused":{"code":"challenge_required"}}})
            ),
            Status::Blocked
        );
        assert_eq!(
            production_status(
                &json!({"outcome":"paused","result":{"paused":{"code":"interrupted"}}})
            ),
            Status::Interrupted
        );
        assert_eq!(
            production_status(
                &json!({"outcome":"error","error":{"code":"invalid_transition","details":{"code":"login_required"}}})
            ),
            Status::Blocked
        );
        assert_eq!(
            production_status(
                &json!({"outcome":"error","error":{"code":"invalid_transition","details":{"code":"candidates_exhausted"}}})
            ),
            Status::Fail
        );
    }

    #[test]
    fn command_outcomes_map_to_statuses() {
        let ok = json!({"outcome":"ok","result":{"steps":[{"name":"a","status":"PASS"}]}});
        assert_eq!(command_status(&ok), Status::Pass);
        assert_eq!(steps(&ok).len(), 1);
        let blocked = json!({"outcome":"error","error":{"details":{"code":"acceptance_blocked","report":{"steps":[{"name":"forvo_auth","status":"BLOCKED"}]}}}});
        assert_eq!(command_status(&blocked), Status::Blocked);
        assert_eq!(
            hint(Status::Blocked, &steps(&blocked)),
            Some("нужен вход в Forvo или ручное прохождение проверки сайта через noVNC")
        );
        let failed = json!({"outcome":"error","error":{"details":{"code":"acceptance_failed"}}});
        assert_eq!(command_status(&failed), Status::Fail);
        assert_eq!(command_status(&json!({"outcome":"error"})), Status::Error);
    }

    #[test]
    fn volumes_and_container_carry_only_real_site_label() {
        assert_ne!(LABEL_REAL_SITE, crate::docker::LABEL);
        assert!(
            VOLUMES
                .iter()
                .all(|(v, _)| v.starts_with("anki-e2e-real-site-"))
        );
        assert!(DOCKERFILE.contains("brave-browser"));
        assert!(!DOCKERFILE.contains("--no-sandbox") && !ENTRYPOINT.contains("--no-sandbox"));
        let policy: Value = serde_json::from_str(POLICY).unwrap();
        assert_eq!(
            policy,
            json!({"PasswordManagerEnabled":false,"PromptForDownloadLocation":false})
        );
    }
}
