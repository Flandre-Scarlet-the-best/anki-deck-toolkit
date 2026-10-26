//! Docker CLI без командной оболочки: каждый вызов — отдельный argv.
//!
//! Все ресурсы стенда помечаются меткой [`LABEL`]; уборка и `clean` трогают
//! только их. По умолчанию клиент получает изолированный `DOCKER_CONFIG`, чтобы
//! не задействовать пользовательские credentials и credential helpers: стенду
//! нужны только публичные образы.

use crate::report::Stop;
use crate::util::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

pub const LABEL: &str = "io.github.anki-deck-toolkit.e2e";
pub const LABEL_RUN: &str = "io.github.anki-deck-toolkit.e2e.run";
pub const LABEL_PID: &str = "io.github.anki-deck-toolkit.e2e.host-pid";
pub const LABEL_KEEP: &str = "io.github.anki-deck-toolkit.e2e.keep";
pub const LABEL_KIND: &str = "io.github.anki-deck-toolkit.e2e.kind";
/// Идентичность процесса runner: `<machine-id>:<boot_id>:<время старта>`.
pub const LABEL_RUNNER: &str = "io.github.anki-deck-toolkit.e2e.host-runner";
/// Идентификаторы BuildKit cache mounts из `docker/run.Dockerfile`.
pub const CACHE_MOUNTS: [&str; 2] = ["anki-e2e-cargo-registry", "anki-e2e-cargo-target"];
/// Сколько ждать штатной отмены `docker build` после SIGINT до SIGKILL.
const BUILD_CANCEL_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Docker {
    config: Option<PathBuf>,
}

impl Docker {
    pub fn new(config: Option<PathBuf>) -> Docker {
        Docker { config }
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new("docker");
        if let Some(dir) = &self.config {
            c.env("DOCKER_CONFIG", dir);
        }
        c.stdin(Stdio::null());
        c
    }

    pub fn output(&self, args: &[&str]) -> Result<Output> {
        self.command()
            .args(args)
            .output()
            .map_err(|e| format!("docker {}: {e}", args.first().unwrap_or(&"")))
    }

    /// Успешный вызов с текстом stdout; иначе stderr в ошибке.
    pub fn text(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            return Err(format!(
                "docker {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn json(&self, args: &[&str]) -> Result<Value> {
        serde_json::from_str(&self.text(args)?)
            .map_err(|e| format!("docker {}: {e}", args.join(" ")))
    }

    /// Предпосылки: CLI, daemon, права, Linux daemon и поддерживаемая архитектура.
    pub fn preflight(&self) -> std::result::Result<Value, Stop> {
        const STAGE: &str = "docker_preflight";
        let out = match self
            .command()
            .args(["version", "--format", "{{json .}}"])
            .output()
        {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Stop::blocked(
                    STAGE,
                    "docker_cli_missing",
                    "Команда docker не найдена в PATH",
                ));
            }
            Err(e) => return Err(Stop::blocked(STAGE, "docker_cli_failed", e.to_string())),
        };
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            let lower = stderr.to_lowercase();
            let reason = if lower.contains("permission denied") {
                "docker_permission_denied"
            } else if [
                "cannot connect",
                "failed to connect",
                "is the docker daemon running",
                "no such file or directory",
            ]
            .iter()
            .any(|m| lower.contains(m))
            {
                "docker_daemon_unavailable"
            } else {
                "docker_unavailable"
            };
            return Err(Stop::blocked(STAGE, reason, stderr.trim().to_owned()));
        }
        let info = self
            .json(&["info", "--format", "{{json .}}"])
            .map_err(|e| Stop::blocked(STAGE, "docker_info_failed", e))?;
        if info["OSType"] != "linux" {
            return Err(Stop::blocked(
                STAGE,
                "docker_not_linux",
                format!("Docker daemon работает с OSType={}", info["OSType"]),
            ));
        }
        let version: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        Ok(serde_json::json!({
            "client": version["Client"]["Version"],
            "server": version["Server"]["Version"],
            "architecture": info["Architecture"],
            "os": info["OperatingSystem"],
            "security_options": info["SecurityOptions"],
            "rootless": info["SecurityOptions"].as_array().is_some_and(|o| o.iter().any(|s| s.as_str().is_some_and(|s| s.contains("rootless")))),
            "cgroup_version": info["CgroupVersion"],
            "isolated_client_config": self.config.is_some(),
        }))
    }

    pub fn image_id(&self, tag: &str) -> Option<String> {
        self.text(&["image", "inspect", "--format", "{{.Id}}", tag])
            .ok()
    }

    /// Сборка с потоковым журналом; при неудаче возвращает хвост журнала.
    pub fn build(&self, spec: &Build) -> Result<()> {
        let Build {
            dockerfile,
            context,
            tag,
            args,
            labels,
            platform,
            log,
            no_cache,
            interrupted,
        } = spec;
        let mut c = self.command();
        c.env("DOCKER_BUILDKIT", "1")
            .args(["build", "--progress", "plain", "--platform", platform, "-f"])
            .arg(dockerfile)
            .args(["-t", tag]);
        if *no_cache {
            c.arg("--no-cache");
        }
        for (k, v) in args.iter() {
            c.arg("--build-arg").arg(format!("{k}={v}"));
        }
        for (k, v) in labels.iter() {
            c.arg("--label").arg(format!("{k}={v}"));
        }
        c.arg(context);
        let file = std::fs::File::create(log).map_err(|e| e.to_string())?;
        // Собственная группа процессов: отмену доставляет runner, а не терминал.
        let child = c
            .stdout(file.try_clone().map_err(|e| e.to_string())?)
            .stderr(file)
            .process_group(0)
            .spawn()
            .map_err(|e| format!("docker build: {e}"))?;
        let Some(status) = wait_interruptible(child, *interrupted, BUILD_CANCEL_GRACE)
            .map_err(|e| format!("docker build: {e}"))?
        else {
            return Err(format!(
                "docker build прерван пользователем; журнал {}",
                log.display()
            ));
        };
        if status.success() {
            return Ok(());
        }
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(25).collect();
        Err(format!(
            "docker build завершился {status}; журнал {}:\n{}",
            log.display(),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ))
    }

    pub fn remove_container(&self, id: &str) -> Result<()> {
        let out = self.output(&["rm", "-f", "-v", id])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() || stderr.contains("No such container") {
            Ok(())
        } else {
            Err(stderr.trim().to_owned())
        }
    }

    pub fn container_exists(&self, id: &str) -> bool {
        self.output(&["container", "inspect", id])
            .is_ok_and(|o| o.status.success())
    }

    /// Контейнеры стенда (включая остановленные) с их метками.
    pub fn owned_containers(&self) -> Result<Vec<Value>> {
        let text = self.text(&[
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL}=1"),
            "--format",
            "{{json .}}",
        ])?;
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// Теги образов стенда заданного вида (`anki` или `run`).
    pub fn owned_image_tags(&self, kind: &str) -> Result<Vec<String>> {
        let text = self.text(&[
            "images",
            "--filter",
            &format!("label={LABEL}=1"),
            "--filter",
            &format!("label={LABEL_KIND}={kind}"),
            "--format",
            "{{.Repository}}:{{.Tag}}",
        ])?;
        Ok(text
            .lines()
            .filter(|t| !t.ends_with(":<none>"))
            .map(str::to_owned)
            .collect())
    }

    /// Прицельное удаление cache mounts стенда: только `exec.cachemount` с
    /// точным идентификатором в конце описания, без общего prune.
    pub fn prune_cache_mounts(&self) -> Result<()> {
        let ids = CACHE_MOUNTS
            .iter()
            .map(|id| id.strip_prefix("anki-e2e-cargo-").unwrap_or(id))
            .collect::<Vec<_>>()
            .join("|");
        let description = format!(r#"description~="with id \"/anki-e2e-cargo-({ids})\"$""#);
        self.text(&[
            "builder",
            "prune",
            "--force",
            "--filter",
            "type=exec.cachemount",
            "--filter",
            &description,
        ])
        .map(|_| ())
    }

    /// Cache mounts стенда, оставшиеся в кэше сборки демона.
    pub fn remaining_cache_mounts(&self) -> Result<Vec<String>> {
        let records = self.json(&["system", "df", "-v", "--format", "{{json .BuildCache}}"])?;
        Ok(records
            .as_array()
            .into_iter()
            .flatten()
            .filter(|record| is_stand_cache_mount(record))
            .map(|record| {
                format!(
                    "{} ({})",
                    record["ID"].as_str().unwrap_or(""),
                    record["Description"].as_str().unwrap_or("")
                )
            })
            .collect())
    }

    pub fn owned_images(&self) -> Result<Vec<String>> {
        let text = self.text(&[
            "images",
            "--filter",
            &format!("label={LABEL}=1"),
            "--format",
            "{{.ID}}",
        ])?;
        let mut ids: Vec<String> = text.lines().map(str::to_owned).collect();
        ids.dedup();
        Ok(ids)
    }
}

pub struct Build<'a> {
    pub dockerfile: &'a Path,
    pub context: &'a Path,
    pub tag: &'a str,
    pub args: &'a [(String, String)],
    pub labels: &'a [(String, String)],
    pub platform: &'a str,
    pub log: &'a Path,
    pub no_cache: bool,
    pub interrupted: &'a dyn Fn() -> bool,
}

fn is_stand_cache_mount(record: &Value) -> bool {
    let description = record["Description"].as_str().unwrap_or("");
    record["CacheType"] == "exec.cachemount"
        && CACHE_MOUNTS
            .iter()
            .any(|id| description.ends_with(&format!("with id \"/{id}\"")))
}

/// Ожидание дочернего процесса с отменой: SIGINT группе (штатная отмена Docker
/// CLI и BuildKit-сессии), затем SIGKILL по истечении `grace`. `None` — прервано.
pub fn wait_interruptible(
    mut child: Child,
    interrupted: &dyn Fn() -> bool,
    grace: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if interrupted() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let signal_group = |signal| {
        if let Some(group) = group {
            let _ = rustix::process::kill_process_group(group, signal);
        }
    };
    signal_group(rustix::process::Signal::INT);
    let end = Instant::now() + grace;
    while Instant::now() < end {
        if child.try_wait()?.is_some() {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    signal_group(rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    Ok(None)
}

/// Выполнение `docker exec` с построчной трансляцией stdout и общим дедлайном.
pub struct Exec {
    child: Child,
}

pub enum ExecEnd {
    Exited(i32),
    TimedOut,
    Interrupted,
}

impl Exec {
    pub fn spawn(docker: &Docker, container: &str, argv: &[String]) -> Result<Exec> {
        let mut c = docker.command();
        c.args(["exec", container])
            .args(argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = c.spawn().map_err(|e| format!("docker exec: {e}"))?;
        Ok(Exec { child })
    }

    /// Строки stdout/stderr передаются `on_line`; ожидание прерывается флагом или дедлайном.
    pub fn wait(
        mut self,
        deadline: Instant,
        interrupted: &dyn Fn() -> bool,
        on_line: &(dyn Fn(&str, bool) + Sync),
    ) -> ExecEnd {
        let stdout = self.child.stdout.take();
        let stderr = self.child.stderr.take();
        std::thread::scope(|scope| {
            if let Some(out) = stdout {
                scope.spawn(move || {
                    for line in BufReader::new(out)
                        .lines()
                        .map_while(std::result::Result::ok)
                    {
                        on_line(&line, false);
                    }
                });
            }
            if let Some(err) = stderr {
                scope.spawn(move || {
                    for line in BufReader::new(err)
                        .lines()
                        .map_while(std::result::Result::ok)
                    {
                        on_line(&line, true);
                    }
                });
            }
            loop {
                if interrupted() {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return ExecEnd::Interrupted;
                }
                match self.child.try_wait() {
                    Ok(Some(status)) => return ExecEnd::Exited(status.code().unwrap_or(-1)),
                    Ok(None) => {}
                    Err(_) => return ExecEnd::Exited(-1),
                }
                if Instant::now() > deadline {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return ExecEnd::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    }
}

fn selected_context_requires_inherit(
    docker_context: Option<&str>,
    docker_host_is_set: bool,
    configured_context: Option<&str>,
) -> Option<String> {
    if let Some(context) = docker_context.filter(|context| !context.is_empty()) {
        return non_default_context(Some(context));
    }
    if docker_host_is_set {
        return None;
    }
    non_default_context(configured_context)
}

fn non_default_context(context: Option<&str>) -> Option<String> {
    context
        .filter(|context| !context.is_empty() && *context != "default")
        .map(str::to_owned)
}

/// Пользовательский контекст теряется при изолированном конфиге.
pub fn user_context_requires_inherit() -> Option<String> {
    let docker_context = std::env::var("DOCKER_CONTEXT").ok();
    let docker_host_is_set = std::env::var_os("DOCKER_HOST").is_some();
    if docker_context
        .as_deref()
        .is_some_and(|context| !context.is_empty())
    {
        return selected_context_requires_inherit(
            docker_context.as_deref(),
            docker_host_is_set,
            None,
        );
    }
    if docker_host_is_set {
        return None;
    }
    let dir = std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker")))?;
    let config: Value =
        serde_json::from_slice(&std::fs::read(dir.join("config.json")).ok()?).ok()?;
    selected_context_requires_inherit(
        docker_context.as_deref(),
        docker_host_is_set,
        config["currentContext"].as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn explicit_docker_context_is_preserved_with_isolated_config() {
        assert_eq!(
            selected_context_requires_inherit(Some("colima"), false, Some("default")),
            Some("colima".into())
        );
        // По документации Docker `DOCKER_CONTEXT` важнее `DOCKER_HOST`.
        assert_eq!(
            selected_context_requires_inherit(Some("colima"), true, None),
            Some("colima".into())
        );
    }

    #[test]
    fn default_or_host_context_does_not_require_user_config() {
        assert_eq!(
            selected_context_requires_inherit(Some("default"), false, Some("colima")),
            None
        );
        assert_eq!(
            selected_context_requires_inherit(None, true, Some("colima")),
            None
        );
        assert_eq!(
            selected_context_requires_inherit(None, false, Some("colima")),
            Some("colima".into())
        );
        assert_eq!(
            selected_context_requires_inherit(None, false, Some("default")),
            None
        );
    }

    #[test]
    fn stand_cache_mounts_are_matched_exactly() {
        let record = |kind: &str, id: &str| {
            serde_json::json!({"CacheType":kind,
                "Description":format!("cached mount /src/target from exec /bin/sh -c cargo build with id \"/{id}\"")})
        };
        assert!(is_stand_cache_mount(&record(
            "exec.cachemount",
            "anki-e2e-cargo-target"
        )));
        assert!(is_stand_cache_mount(&record(
            "exec.cachemount",
            "anki-e2e-cargo-registry"
        )));
        assert!(!is_stand_cache_mount(&record(
            "exec.cachemount",
            "anki-e2e-cargo-target-other"
        )));
        assert!(!is_stand_cache_mount(&record(
            "exec.cachemount",
            "other-anki-e2e-cargo-target"
        )));
        assert!(!is_stand_cache_mount(&record(
            "regular",
            "anki-e2e-cargo-target"
        )));
    }

    #[test]
    fn long_build_is_cancelled_promptly_on_interrupt() {
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let result = wait_interruptible(
            child,
            &|| started.elapsed() > Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));

        // Дочерний процесс, игнорирующий SIGINT, завершается SIGKILL после grace.
        let stubborn = Command::new("sh")
            .args(["-c", "trap '' INT; sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let started = Instant::now();
        assert!(
            wait_interruptible(stubborn, &|| true, Duration::from_millis(300))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let done = Command::new("true").spawn().unwrap();
        assert!(
            wait_interruptible(done, &|| false, Duration::from_secs(1))
                .unwrap()
                .is_some_and(|s| s.success())
        );
    }

    #[test]
    fn interruption_wins_when_child_exits_between_poll_cycles() {
        let child = Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let checks = AtomicUsize::new(0);
        let result = Exec { child }.wait(
            Instant::now() + Duration::from_secs(2),
            &|| checks.fetch_add(1, Ordering::SeqCst) > 0,
            &|_, _| {},
        );

        assert!(matches!(result, ExecEnd::Interrupted));
    }
}
