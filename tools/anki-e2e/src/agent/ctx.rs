//! Контекст сценария: настоящие команды `anki-repo`, ожидания под наблюдением
//! UI, независимые проверки и управление процессами Anki в контейнере.

use super::Fail;
use super::display::{KEY_CONTROL_L, KEY_Q};
use super::ui::{Monitor, Policy};
use crate::util;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const PROFILE: &str = "User 1";
const POLL: Duration = Duration::from_millis(150);
/// Интервал между проверками условия, запускающими новый процесс `anki-repo`.
/// Окна при этом опрашиваются с частотой [`POLL`], в том числе пока CLI работает.
pub const CLI_POLL: Duration = Duration::from_secs(1);

/// Расписание проверок условия, отделённое от частоты опроса окон.
#[derive(Debug)]
struct Cadence {
    every: Duration,
    last: Option<Instant>,
}

impl Cadence {
    fn new(every: Duration) -> Cadence {
        Cadence { every, last: None }
    }

    /// Пора ли проверять условие; отметка ставится при положительном ответе.
    fn due(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.every);
        if due {
            self.last = Some(now);
        }
        due
    }
}

fn command_record_label(args: &[&str]) -> String {
    let raw = args
        .iter()
        .filter(|a| !a.starts_with('/') && !a.starts_with("--") && **a != PROFILE)
        .take(3)
        .copied()
        .collect::<Vec<_>>()
        .join("-");
    let label: String = raw
        .chars()
        .take(60)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        "command".into()
    } else {
        label
    }
}

fn command_failure(
    ui_failure: Option<Fail>,
    timed_out: bool,
    command: &str,
    timeout: Duration,
) -> Option<Fail> {
    if let Some(mut failure) = ui_failure {
        if timed_out {
            failure.message.push_str(&format!(
                "; команда {command} остановлена после таймаута {timeout:?}"
            ));
        }
        return Some(failure);
    }
    timed_out.then(|| {
        Fail::failed(
            "command_timeout",
            format!("{command} не завершилась за {timeout:?}"),
        )
    })
}

pub struct Env {
    pub anki: PathBuf,
    pub anki_repo: PathBuf,
    pub display: String,
    pub bus: String,
    pub home: PathBuf,
}

impl Env {
    /// Переменные, которые наследуют `anki-repo` и запущенная им Anki.
    pub fn apply(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("DISPLAY", &self.display)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus)
            .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
            .env("QT_QPA_PLATFORM", "xcb");
    }
}

#[derive(Debug, Clone)]
pub struct Cmd {
    pub exit: i32,
    pub json: Value,
}

impl Cmd {
    pub fn result(&self) -> &Value {
        &self.json["result"]
    }
    pub fn state(&self) -> &str {
        self.json["result"]["state"].as_str().unwrap_or("")
    }
    pub fn error_reason(&self) -> &str {
        self.json["error"]["details"]["reason"]
            .as_str()
            .unwrap_or("")
    }
    pub fn error_message(&self) -> &str {
        self.json["error"]["message"].as_str().unwrap_or("")
    }
}

pub struct Ctx<'a> {
    pub name: String,
    pub dir: PathBuf,
    pub base: PathBuf,
    pub env: &'a Env,
    pub ui: &'a mut Monitor,
    pub checks: Vec<Value>,
    pub notes: Vec<String>,
    /// Число запущенных процессов `anki-repo` за сценарий.
    pub commands_started: u32,
    seq: u32,
}

impl<'a> Ctx<'a> {
    pub fn new(
        name: &str,
        dir: PathBuf,
        base: PathBuf,
        env: &'a Env,
        ui: &'a mut Monitor,
    ) -> Ctx<'a> {
        ui.begin(name, dir.clone());
        Ctx {
            name: name.to_owned(),
            dir,
            base,
            env,
            ui,
            checks: Vec::new(),
            notes: Vec::new(),
            commands_started: 0,
            seq: 0,
        }
    }

    pub fn step(&mut self, step: &str) {
        self.ui.set_step(step);
        println!("[agent] {} · {step}", self.name);
    }

    /// Утверждение с доказательством; ложное — немедленный FAIL сценария.
    pub fn check(&mut self, what: &str, ok: bool, evidence: Value) -> Result<(), Fail> {
        self.checks
            .push(json!({"what":what,"ok":ok,"step":self.ui.step,"evidence":evidence}));
        if ok {
            Ok(())
        } else {
            Err(Fail::failed(
                "assertion_failed",
                format!("{what}; свидетельство: {evidence}"),
            ))
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// `anki-repo --json anki --data-dir <база> --profile 'User 1' …`
    pub fn anki_repo(&mut self, args: &[&str], policy: &Policy) -> Result<Cmd, Fail> {
        let base = self.base.to_string_lossy().into_owned();
        let mut argv = vec!["--json", "anki", "--data-dir", &base, "--profile", PROFILE];
        argv.extend_from_slice(args);
        self.run(
            &self.env.anki_repo.clone(),
            &argv,
            policy,
            Duration::from_secs(240),
        )
    }

    /// Тот же CLI без автоматических глобальных параметров.
    pub fn anki_repo_raw(&mut self, args: &[&str], policy: &Policy) -> Result<Cmd, Fail> {
        self.run(
            &self.env.anki_repo.clone(),
            args,
            policy,
            Duration::from_secs(240),
        )
    }

    fn run(
        &mut self,
        program: &Path,
        args: &[&str],
        policy: &Policy,
        timeout: Duration,
    ) -> Result<Cmd, Fail> {
        self.seq += 1;
        let label = command_record_label(args);
        let record = format!("commands/{:03}-{label}.json", self.seq);
        let mut c = Command::new(program);
        self.env.apply(&mut c);
        c.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = c
            .spawn()
            .map_err(|e| Fail::error("command_spawn_failed", e.to_string()))?;
        self.commands_started += 1;
        let mut out = child.stdout.take().expect("stdout");
        let mut err = child.stderr.take().expect("stderr");
        let reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = out.read_to_end(&mut s);
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = err.read_to_end(&mut s);
            s
        });
        let deadline = started + timeout;
        let mut ui_failure = None;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| Fail::error("command_wait_failed", e.to_string()))?
            {
                break Some(status);
            }
            if ui_failure.is_none()
                && let Err(f) = self.ui.poll(policy)
            {
                ui_failure = Some(f);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(POLL);
        };
        // Итог команды не опирается на устаревший снимок окон.
        if ui_failure.is_none()
            && !self.ui.fresh()
            && let Err(f) = self.ui.settle(policy)
        {
            ui_failure = Some(f);
        }
        let stdout = reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        let json: Value = serde_json::from_slice(&stdout)
            .unwrap_or_else(|_| json!({"raw_stdout":String::from_utf8_lossy(&stdout)}));
        let exit = status.and_then(|s| s.code()).unwrap_or(-1);
        let mut argv = vec![program.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        if let Err(e) = util::write_json(
            &self.dir.join(&record),
            &json!({"argv":argv,"step":self.ui.step,"exit":exit,"timed_out":status.is_none(),
                "duration_ms":started.elapsed().as_millis(),"stdout":json,"stderr":stderr}),
        ) {
            self.ui
                .evidence_errors
                .push(format!("запись команды {record}: {e}"));
        }
        println!("[agent]   $ {} → exit {exit}", args.join(" "));
        if let Some(f) = command_failure(ui_failure, status.is_none(), &argv.join(" "), timeout) {
            return Err(f);
        }
        Ok(Cmd { exit, json })
    }

    /// Ожидание условия с наблюдением UI; таймаут — FAIL с последним состоянием окон.
    pub fn wait(
        &mut self,
        what: &str,
        timeout: Duration,
        policy: &Policy,
        cond: impl FnMut(&mut Ctx<'a>) -> Result<bool, Fail>,
    ) -> Result<(), Fail> {
        self.wait_every(what, timeout, policy, Duration::ZERO, cond)
    }

    /// Как [`Ctx::wait`], но условие проверяется не чаще `every`: так дорогие
    /// проверки (новый процесс `anki-repo`) не привязаны к частоте опроса окон.
    /// Условие оценивается только на свежем снимке окон.
    pub fn wait_every(
        &mut self,
        what: &str,
        timeout: Duration,
        policy: &Policy,
        every: Duration,
        mut cond: impl FnMut(&mut Ctx<'a>) -> Result<bool, Fail>,
    ) -> Result<(), Fail> {
        let deadline = Instant::now() + timeout;
        let mut cadence = Cadence::new(every);
        loop {
            self.ui.poll(policy)?;
            if self.ui.fresh() && cadence.due(Instant::now()) && cond(self)? {
                return Ok(());
            }
            if Instant::now() > deadline {
                let shot = self.ui.screenshot("timeout");
                let observation = if self.ui.fresh() {
                    String::new()
                } else {
                    format!(
                        "; наблюдение окон деградировано: {}",
                        self.ui
                            .last_error
                            .as_deref()
                            .unwrap_or("нет успешного опроса")
                    )
                };
                return Err(Fail::failed(
                    "wait_timeout",
                    format!(
                        "Не дождались: {what} за {timeout:?}; окна: {}; скриншот {shot:?}{observation}",
                        json!(self.ui.leftover())
                    ),
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Наблюдение без условия: убедиться, что за период не появилось ничего лишнего.
    pub fn observe(&mut self, period: Duration, policy: &Policy) -> Result<(), Fail> {
        let end = Instant::now() + period;
        while Instant::now() < end {
            self.ui.poll(policy)?;
            std::thread::sleep(POLL);
        }
        // Период не подтверждён, если последний снимок окон не свежий.
        self.ui.settle(policy)
    }

    pub fn wait_main_window(&mut self, policy: &Policy) -> Result<(), Fail> {
        self.wait(
            "главное окно профиля",
            Duration::from_secs(120),
            policy,
            |c| Ok(c.ui.main_window().is_some() && c.ui.dialogs().is_empty()),
        )
    }

    /// Прямой запуск установленной Anki (как ярлык пользователя), журнал — в свидетельства.
    pub fn spawn_anki(&mut self, extra: &[&str], owner: Option<&str>) -> Result<(), Fail> {
        self.seq += 1;
        let log = self.dir.join(format!("logs/{:03}-anki.log", self.seq));
        std::fs::create_dir_all(log.parent().expect("parent"))
            .map_err(|e| Fail::error("io", e.to_string()))?;
        let file = std::fs::File::create(&log).map_err(|e| Fail::error("io", e.to_string()))?;
        let mut c = Command::new(&self.env.anki);
        self.env.apply(&mut c);
        if let Some(token) = owner {
            c.env("ANKI_TOOLKIT_OWNER", token);
        }
        c.arg("-b").arg(&self.base).args(extra).stdin(Stdio::null());
        c.stdout(
            file.try_clone()
                .map_err(|e| Fail::error("io", e.to_string()))?,
        )
        .stderr(file);
        c.spawn()
            .map_err(|e| Fail::error("anki_spawn_failed", e.to_string()))?;
        println!(
            "[agent]   запуск anki -b {} {}",
            self.base.display(),
            extra.join(" ")
        );
        Ok(())
    }

    /// Штатное закрытие пользователем: Ctrl+Q в главном окне без модальных окон.
    pub fn quit_via_gui(&mut self, policy: &Policy) -> Result<(), Fail> {
        self.wait(
            "Anki без открытых диалогов перед закрытием",
            Duration::from_secs(30),
            policy,
            |c| Ok(c.ui.main_window().is_some() && c.ui.dialogs().is_empty()),
        )?;
        if let Some(rect) = self.ui.main_window().and_then(|w| w.rect) {
            // Фокус главного окна: клик по пустой полосе меню справа от пунктов.
            let _ = self.ui.display.click(rect.x + rect.w - 20, rect.y + 8);
        }
        self.ui
            .display
            .key_combo(&[KEY_CONTROL_L, KEY_Q])
            .map_err(|e| Fail::error("gui_input_failed", e))?;
        self.wait_no_anki(
            "Anki завершилась после Ctrl+Q",
            Duration::from_secs(60),
            policy,
        )
    }

    pub fn wait_no_anki(
        &mut self,
        what: &str,
        timeout: Duration,
        policy: &Policy,
    ) -> Result<(), Fail> {
        self.wait(what, timeout, policy, |_| Ok(anki_processes().is_empty()))
    }

    /// Прямой вызов upstream AnkiConnect для фикстур и независимой проверки.
    pub fn connect(&mut self, action: &str, params: Value) -> Result<Value, Fail> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| Fail::error("http_client", e.to_string()))?;
        let response: Value = client
            .post("http://127.0.0.1:8765")
            .json(&json!({"action":action,"version":6,"params":params}))
            .send()
            .and_then(|r| r.json())
            .map_err(|e| Fail::failed("ankiconnect_unreachable", format!("{action}: {e}")))?;
        if !response["error"].is_null() {
            return Err(Fail::failed(
                "ankiconnect_error",
                format!("{action}: {}", response["error"]),
            ));
        }
        Ok(response["result"].clone())
    }

    pub fn shot(&mut self, label: &str) -> Option<String> {
        self.ui.screenshot(label)
    }
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
}

/// Процессы Anki по `/proc`: argv0 — исполняемый `anki` (не QtWebEngineProcess).
pub fn anki_processes() -> Vec<Proc> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let raw = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let zombie = std::fs::read_to_string(e.path().join("stat")).is_ok_and(|s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        });
        if !zombie
            && argv
                .first()
                .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "anki"))
        {
            out.push(Proc { pid });
        }
    }
    out
}

/// Принудительное завершение — только для аварийного сценария и итоговой уборки.
pub fn signal_anki(kill: bool) -> Vec<i32> {
    let signal = if kill {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    let mut pids = Vec::new();
    for p in anki_processes() {
        if let Some(pid) = rustix::process::Pid::from_raw(p.pid) {
            let _ = rustix::process::kill_process(pid, signal);
            pids.push(p.pid);
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_bounds_condition_checks_independently_of_ui_poll() {
        let start = Instant::now();
        let mut cadence = Cadence::new(CLI_POLL);
        // Окна опрашиваются каждые POLL в течение 10 с, условие — не чаще CLI_POLL.
        let polls = (10_000 / POLL.as_millis()) as u32;
        let checks = (0..=polls)
            .filter(|i| cadence.due(start + POLL * *i))
            .count();
        assert_eq!(checks, 10);

        let mut every_poll = Cadence::new(Duration::ZERO);
        assert!((0..5).all(|i| every_poll.due(start + POLL * i)));
    }

    #[test]
    fn command_record_label_cannot_escape_commands_directory() {
        let label = command_record_label(&["restore", "../../etc/passwd"]);
        assert!(!label.contains('/'));
        assert!(!label.contains(".."));
        assert!(label.len() <= 60);
        assert_eq!(label, "restore-______etc_passwd");
    }

    #[test]
    fn command_record_label_is_bounded() {
        let long = "a".repeat(200);
        let label = command_record_label(&[&long]);
        assert_eq!(label.len(), 60);
    }

    #[test]
    fn ui_failure_remains_primary_when_command_times_out() {
        let failure = Fail::failed("unknown_blocking_dialog", "Неизвестное окно Anki");
        let result = command_failure(
            Some(failure),
            true,
            "anki-repo anki doctor",
            Duration::from_secs(240),
        )
        .unwrap();

        assert_eq!(result.reason, "unknown_blocking_dialog");
        assert!(result.message.contains("Неизвестное окно Anki"));
        assert!(result.message.contains("таймаута 240s"));
    }

    #[test]
    fn command_timeout_is_kept_when_ui_poll_succeeds() {
        let result =
            command_failure(None, true, "anki-repo anki doctor", Duration::from_secs(5)).unwrap();

        assert_eq!(result.reason, "command_timeout");
        assert!(result.message.contains("не завершилась за 5s"));
    }

    #[test]
    fn ui_failure_is_returned_without_timeout_details_when_command_exits() {
        let failure = Fail::failed("unknown_blocking_dialog", "Неизвестное окно Anki");
        let result = command_failure(
            Some(failure),
            false,
            "anki-repo anki doctor",
            Duration::from_secs(240),
        )
        .unwrap();

        assert_eq!(result.reason, "unknown_blocking_dialog");
        assert!(!result.message.contains("таймаута"));
    }
}
