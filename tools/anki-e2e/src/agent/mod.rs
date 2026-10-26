//! Режим агента внутри контейнера: виртуальный дисплей, шина доступности,
//! последовательные сценарии и свидетельства в `/work/out`.
//!
//! Агент запускается только runner через `docker exec`; вручную его вызывать
//! не нужно. Он не создаёт собственного orchestrator поверх Anki: команды
//! выполняет настоящий `anki-repo`, окна показывает настоящая Anki.

mod atspi;
mod catalog;
mod ctx;
mod display;
mod fixtures;
mod scenarios;
mod ui;

use crate::report::Status;
use crate::util::{self, sha256_file};
use ctx::{Ctx, Env, anki_processes, signal_anki};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Fail {
    pub status: Status,
    pub reason: String,
    pub message: String,
}

impl Fail {
    pub fn failed(reason: &str, message: impl Into<String>) -> Fail {
        Fail {
            status: Status::Fail,
            reason: reason.into(),
            message: message.into(),
        }
    }
    pub fn error(reason: &str, message: impl Into<String>) -> Fail {
        Fail {
            status: Status::Error,
            reason: reason.into(),
            message: message.into(),
        }
    }
    pub fn blocked(reason: &str, message: impl Into<String>) -> Fail {
        Fail {
            status: Status::Blocked,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct AgentArgs {
    /// Сценарии в порядке выполнения.
    #[arg(long, value_delimiter = ',', required = true)]
    pub scenarios: Vec<String>,
    #[arg(long, default_value = "/work")]
    pub work: PathBuf,
    /// Версия, выбранная runner; сверяется с `anki --version`.
    #[arg(long)]
    pub expect_anki_version: Option<String>,
    /// Диагностический VNC-просмотр дисплея (только по явному флагу runner).
    #[arg(long)]
    pub vnc: bool,
}

const DISPLAY: &str = ":99";

struct Services {
    children: Vec<(&'static str, Child)>,
}

impl Drop for Services {
    fn drop(&mut self) {
        for (_, child) in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) -> Result<(), Fail> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(Fail::error(
        "environment_not_ready",
        format!("Не готово за {timeout:?}: {what}"),
    ))
}

fn spawn_service(
    name: &'static str,
    program: &str,
    args: &[&str],
    log: &Path,
    services: &mut Services,
) -> Result<(), Fail> {
    let file = std::fs::File::create(log).map_err(|e| Fail::error("io", e.to_string()))?;
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(
            file.try_clone()
                .map_err(|e| Fail::error("io", e.to_string()))?,
        )
        .stderr(file)
        .spawn()
        .map_err(|e| Fail::error("service_spawn_failed", format!("{program}: {e}")))?;
    services.children.push((name, child));
    Ok(())
}

fn version_of(env: &Env) -> Option<String> {
    let mut c = Command::new(&env.anki);
    env.apply(&mut c);
    let out = c.arg("--version").stdin(Stdio::null()).output().ok()?;
    parse_version(out.status.success(), &String::from_utf8_lossy(&out.stdout))
}

fn parse_version(success: bool, text: &str) -> Option<String> {
    if !success {
        return None;
    }
    text.lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("Anki ").map(|v| v.trim().to_owned()))
        .or_else(|| {
            let t = text.trim();
            (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
                .then(|| t.to_owned())
        })
}

/// Совпадение версии без учёта ведущих нулей: «26.09.3» и «26.9.3».
fn same_version(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> Vec<String> {
        s.split('.')
            .map(|p| p.trim_start_matches('0').to_owned())
            .collect()
    };
    norm(a) == norm(b)
}

fn verify_installed_version(expected: Option<&str>, actual: Option<&str>) -> Result<(), Fail> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let Some(actual) = actual else {
        return Err(Fail::error(
            "installed_version_unavailable",
            format!("Не удалось подтвердить установленную версию Anki; ожидалась {expected}"),
        ));
    };
    if same_version(expected, actual) {
        Ok(())
    } else {
        Err(Fail::error(
            "installed_version_mismatch",
            format!("В образе Anki {actual}, ожидалась {expected}"),
        ))
    }
}

pub fn run(args: AgentArgs) -> ExitCode {
    let out = args.work.join("out");
    let _ = std::fs::create_dir_all(&out);
    let env = Env {
        anki: PathBuf::from("/usr/local/bin/anki"),
        anki_repo: PathBuf::from("/usr/local/bin/anki-repo"),
        display: DISPLAY.into(),
        bus: format!("unix:path={}", args.work.join("run").join("bus").display()),
        home: PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/home/anki".into())),
    };
    // Происхождение фиксируется до запуска служб: и ранний отказ несёт
    // положительное свидетельство, с которым runner сверяет снимок исходников.
    let build = build_provenance(&env);
    let result = agent_result(execute(&args, &out, &env), build);
    let status = result["status"]
        .as_str()
        .and_then(Status::parse)
        .unwrap_or(Status::Error);
    if let Err(e) = util::write_json(&out.join("agent-result.json"), &result) {
        // Без результата runner не может подтвердить итог: это ошибка, а не PASS.
        eprintln!("[agent] результат не записан: {e}");
        println!("[agent] итог: ERROR (первичный итог {})", status.as_str());
        return ExitCode::from(Status::Error.exit_code());
    }
    println!("[agent] итог: {}", status.as_str());
    ExitCode::from(status.exit_code())
}

fn build_provenance(env: &Env) -> Value {
    json!({
        "source_commit": std::env::var("ANKI_E2E_SOURCE_COMMIT").ok(),
        "source_tree_sha256": std::env::var("ANKI_E2E_SOURCE_TREE_SHA256").ok(),
        "anki_repo_sha256": sha256_file(&env.anki_repo).ok(),
        "anki_e2e_sha256": std::env::current_exe().ok().and_then(|p| sha256_file(&p).ok()),
        "anki_executable": std::fs::canonicalize(&env.anki).ok(),
    })
}

fn agent_result(outcome: Result<Value, Fail>, build: Value) -> Value {
    let mut result = match outcome {
        Ok(v) => v,
        Err(f) => {
            json!({"status":f.status,"stop":{"reason":f.reason,"message":f.message},"scenarios":[]})
        }
    };
    result["build"] = build;
    result
}

fn execute(args: &AgentArgs, out: &Path, env: &Env) -> Result<Value, Fail> {
    let run_dir = args.work.join("run");
    std::fs::create_dir_all(&run_dir).map_err(|e| Fail::error("io", e.to_string()))?;
    let logs = out.join("services");
    std::fs::create_dir_all(&logs).map_err(|e| Fail::error("io", e.to_string()))?;
    let bus_path = run_dir.join("bus");

    let mut services = Services {
        children: Vec::new(),
    };
    println!("[agent] запуск Xvfb {DISPLAY}");
    spawn_service(
        "xvfb",
        "Xvfb",
        &[
            DISPLAY,
            "-screen",
            "0",
            "1280x800x24",
            "-nolisten",
            "tcp",
            "-noreset",
        ],
        &logs.join("xvfb.log"),
        &mut services,
    )?;
    wait_for("сокет X11", Duration::from_secs(20), || {
        Path::new("/tmp/.X11-unix/X99").exists()
    })?;
    let mut display = None;
    wait_for(
        "подключение к X11",
        Duration::from_secs(20),
        || {
            display = display::Display::connect(DISPLAY).ok();
            display.is_some()
        },
    )?;
    let display = display.expect("display");
    println!("[agent] запуск сеансовой D-Bus");
    let address = format!("--address={}", env.bus);
    spawn_service(
        "dbus",
        "dbus-daemon",
        &["--session", &address, "--nofork", "--nopidfile"],
        &logs.join("dbus.log"),
        &mut services,
    )?;
    wait_for("сокет D-Bus", Duration::from_secs(20), || {
        bus_path.exists()
    })?;
    let mut atspi = None;
    let mut last_error = String::new();
    wait_for(
        "шина доступности AT-SPI",
        Duration::from_secs(30),
        || match atspi::Atspi::connect(&env.bus) {
            Ok(a) => {
                atspi = Some(a);
                true
            }
            Err(e) => {
                last_error = e;
                false
            }
        },
    )
    .map_err(|f| Fail::error(&f.reason, format!("{}: {last_error}", f.message)))?;
    if args.vnc {
        println!("[agent] диагностический VNC на порту 5900 контейнера");
        spawn_service(
            "x11vnc",
            "x11vnc",
            &[
                "-display", DISPLAY, "-forever", "-shared", "-nopw", "-rfbport", "5900", "-quiet",
            ],
            &logs.join("x11vnc.log"),
            &mut services,
        )?;
    }
    let installed = version_of(env);
    println!("[agent] Anki --version: {installed:?}");
    verify_installed_version(args.expect_anki_version.as_deref(), installed.as_deref())?;
    let mut monitor = ui::Monitor::new(atspi.expect("atspi"), display);
    let mut fx = scenarios::Fixtures::new(args.work.join("fixtures"), installed.clone());
    let mut results = Vec::new();
    for name in &args.scenarios {
        let Some(scenario) = scenarios::by_name(name) else {
            results.push(json!({"name":name,"status":Status::Error,"reason":"unknown_scenario","message":"Нет такого сценария","checks":[],"ui_events":[]}));
            continue;
        };
        results.push(run_scenario(
            name,
            scenario,
            &args.work,
            out,
            env,
            &mut monitor,
            &mut fx,
        ));
    }
    let statuses = results
        .iter()
        .filter_map(|r| r["status"].as_str().and_then(Status::parse));
    let status = crate::report::overall(statuses);
    let alive: Vec<&'static str> = services
        .children
        .iter_mut()
        .filter_map(|(n, c)| c.try_wait().ok().flatten().is_none().then_some(*n))
        .collect();
    Ok(json!({
        "status": status,
        "installed_anki_version": installed,
        "environment": {"display":DISPLAY,"accessibility":"AT-SPI (QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1)","services_alive_before_shutdown":alive,"vnc":args.vnc},
        "scenarios": results,
        "ux_matrix": ux_matrix(&results),
    }))
}

fn run_scenario(
    name: &str,
    scenario: fn(&mut Ctx, &mut scenarios::Fixtures) -> scenarios::Outcome,
    work: &Path,
    out: &Path,
    env: &Env,
    monitor: &mut ui::Monitor,
    fx: &mut scenarios::Fixtures,
) -> Value {
    println!("[agent] ▶ сценарий {name}");
    let dir = out.join("scenarios").join(name);
    let base = work.join("scenarios").join(name).join("Anki2");
    let _ = std::fs::create_dir_all(base.parent().expect("parent"));
    let mut pre = Vec::new();
    if !anki_processes().is_empty() {
        pre.push(format!(
            "До сценария оставалась Anki: {:?}",
            signal_anki(true)
        ));
        std::thread::sleep(Duration::from_secs(2));
    }
    let mut ctx = Ctx::new(name, dir.clone(), base, env, monitor);
    let started = Instant::now();
    let mut outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scenario(&mut ctx, fx)))
            .unwrap_or_else(|p| {
                let message = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                Err(Fail::error("agent_panic", message))
            });
    // Итоговое наблюдение: поздние окна тоже входят в результат сценария, а
    // прежний снимок без свежего опроса не подтверждает их отсутствие.
    ctx.ui.set_step("завершение сценария");
    if let Err(f) = ctx.ui.settle(&ui::Policy::none())
        && outcome.is_ok()
    {
        outcome = Err(f);
    }
    let leftover = ctx.ui.leftover();
    let final_shot = ctx.ui.screenshot("final");
    if outcome.is_ok() && !leftover.is_empty() {
        outcome = Err(Fail::failed(
            "unexplained_window_left",
            format!("После сценария остались окна: {}", json!(leftover)),
        ));
    }
    let mut cleanup = json!({"method":"none"});
    if !anki_processes().is_empty() {
        if outcome.is_ok() {
            outcome = Err(Fail::failed(
                "anki_left_running",
                "Сценарий завершился, оставив процесс Anki",
            ));
        }
        let term = signal_anki(false);
        let end = Instant::now() + Duration::from_secs(15);
        while !anki_processes().is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(200));
        }
        let killed = if anki_processes().is_empty() {
            Vec::new()
        } else {
            signal_anki(true)
        };
        cleanup = json!({"method":"signal","sigterm":term,"sigkill":killed});
    }
    let duration = started.elapsed().as_secs_f64();
    let events = ctx.ui.events.clone();
    let lines: String = events.iter().map(|e| format!("{e}\n")).collect();
    if let Err(e) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("events.jsonl"), lines))
    {
        ctx.ui
            .evidence_errors
            .push(format!("запись events.jsonl: {e}"));
    }
    let evidence_errors = ctx.ui.evidence_errors.clone();
    let outcome = require_evidence(outcome, &evidence_errors);
    let (status, reason, message) = match &outcome {
        Ok(()) => (Status::Pass, String::new(), String::new()),
        Err(f) => (f.status, f.reason.clone(), f.message.clone()),
    };
    println!(
        "[agent] ■ {name}: {} {reason} ({duration:.1} с)",
        status.as_str()
    );
    let ui_events: Vec<Value> = events
        .iter()
        .filter(|e| e["event"] == "appeared" && e["classification"] != "main_window")
        .cloned()
        .collect();
    let notes = [pre, ctx.notes.clone()].concat();
    let result = json!({
        "name": name, "status": status, "reason": reason, "message": message, "duration_s": duration,
        "checks": ctx.checks, "notes": notes, "ui_events": ui_events,
        "leftover_windows": leftover, "final_screenshot": final_shot, "cleanup": cleanup,
        "observer_last_error": ctx.ui.last_error, "evidence_errors": evidence_errors,
        "commands_started": ctx.commands_started,
    });
    if let Err(e) = util::write_json(&dir.join("result.json"), &result) {
        return without_result_file(result, e);
    }
    result
}

/// `PASS` требует всех обязательных свидетельств; более ранняя причина отказа
/// сценария остаётся первичной.
fn require_evidence(outcome: Result<(), Fail>, missing: &[String]) -> Result<(), Fail> {
    match outcome {
        Ok(()) if !missing.is_empty() => Err(Fail::error(
            "evidence_missing",
            format!(
                "Не сохранены обязательные свидетельства: {}",
                missing.join("; ")
            ),
        )),
        other => other,
    }
}

/// `result.json` сценария не записан: успешный сценарий становится ошибкой.
fn without_result_file(mut result: Value, error: String) -> Value {
    let message = format!("result.json не записан: {error}");
    if result["status"] == json!(Status::Pass) {
        result["status"] = json!(Status::Error);
        result["reason"] = json!("evidence_missing");
        result["message"] = json!(message);
    }
    if let Some(list) = result["evidence_errors"].as_array_mut() {
        list.push(json!(message));
    }
    result
}

/// Матрица UX: каждое наблюдённое окно с интерпретацией из каталога.
fn ux_matrix(results: &[Value]) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for sc in results {
        for e in sc["ui_events"].as_array().into_iter().flatten() {
            let id = e["rule"].as_str().unwrap_or("");
            let key = if id.is_empty() {
                format!("unknown:{}:{}", e["role"], e["title"])
            } else {
                id.to_owned()
            };
            if let Some(row) = rows.iter_mut().find(|r| r["key"] == key.as_str()) {
                row["seen"] = json!(row["seen"].as_u64().unwrap_or(0) + 1);
                if let Some(list) = row["scenarios"].as_array_mut()
                    && !list.contains(&sc["name"])
                {
                    list.push(sc["name"].clone());
                }
                continue;
            }
            let rule = catalog::rule(id);
            // У всплывающих подсказок Qt текст приходит в имени объекта, а не в Text.
            let message = match e["text"].as_str() {
                Some(t) if !t.trim().is_empty() => e["text"].clone(),
                _ => e["title"].clone(),
            };
            rows.push(json!({
                "key": key, "rule": rule.map(|r| r.id), "origin": rule.map_or("unknown", |r| r.origin),
                "classification": e["classification"], "message": message, "title": e["title"],
                "catalog_message": rule.map(|r| r.message),
                "meaning": rule.map_or("Неизвестное окно: сценарий остановлен, требуется разбор", |r| r.meaning),
                "action": rule.map_or("Не нажимать вслепую; см. скриншот", |r| r.action),
                "if_ignored": rule.map_or("Неизвестно", |r| r.if_ignored),
                "needs_user_action": e["needs_user_action"], "modal": e["modal"],
                "seen": 1, "scenarios": [sc["name"]], "example_screenshot": e["screenshot"],
            }));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_failure_keeps_primary_reason_and_build_provenance() {
        let build = json!({"source_tree_sha256":"abc"});
        let result = agent_result(
            Err(Fail::error(
                "installed_version_mismatch",
                "В образе Anki 26.09.2",
            )),
            build.clone(),
        );
        assert_eq!(result["status"], "ERROR");
        assert_eq!(result["stop"]["reason"], "installed_version_mismatch");
        assert_eq!(result["build"], build);

        let ok = agent_result(Ok(json!({"status":"PASS","scenarios":[]})), build.clone());
        assert_eq!(ok["build"], build);
    }

    #[test]
    fn lost_mandatory_evidence_prevents_pass_but_not_primary_failure() {
        let missing = vec!["скриншот screenshots/001-final.png: X11".to_owned()];
        let error = require_evidence(Ok(()), &missing).unwrap_err();
        assert_eq!(
            (error.status, error.reason.as_str()),
            (Status::Error, "evidence_missing")
        );
        assert!(error.message.contains("001-final.png"));

        let primary =
            require_evidence(Err(Fail::failed("wait_timeout", "окно")), &missing).unwrap_err();
        assert_eq!(primary.reason, "wait_timeout");
        assert!(require_evidence(Ok(()), &[]).is_ok());
    }

    #[test]
    fn unwritten_scenario_result_turns_pass_into_error_only() {
        let pass = json!({"status":"PASS","reason":"","evidence_errors":[]});
        let result = without_result_file(pass, "No space left on device".into());
        assert_eq!(result["status"], "ERROR");
        assert_eq!(result["reason"], "evidence_missing");
        assert_eq!(result["evidence_errors"].as_array().unwrap().len(), 1);

        let fail = json!({"status":"FAIL","reason":"wait_timeout","evidence_errors":[]});
        let result = without_result_file(fail, "EIO".into());
        assert_eq!(
            (result["status"].as_str(), result["reason"].as_str()),
            (Some("FAIL"), Some("wait_timeout"))
        );
    }

    #[test]
    fn versions_compare_without_leading_zeroes() {
        assert!(same_version("26.09.3", "26.9.3"));
        assert!(!same_version("26.09.3", "26.9.2"));
    }

    #[test]
    fn expected_version_requires_successful_observation() {
        let error = verify_installed_version(Some("26.09.3"), None).unwrap_err();
        assert_eq!(error.reason, "installed_version_unavailable");
        assert_eq!(error.status, Status::Error);
    }

    #[test]
    fn expected_version_must_match() {
        assert!(verify_installed_version(Some("26.09.3"), Some("26.9.3")).is_ok());
        let error = verify_installed_version(Some("26.09.3"), Some("26.09.2")).unwrap_err();
        assert_eq!(error.reason, "installed_version_mismatch");
    }

    #[test]
    fn failed_version_command_is_not_parsed_as_installed_version() {
        assert_eq!(parse_version(false, "Anki 26.09.3"), None);
        assert_eq!(
            parse_version(true, "Anki 26.09.3\n"),
            Some("26.09.3".into())
        );
    }
}
