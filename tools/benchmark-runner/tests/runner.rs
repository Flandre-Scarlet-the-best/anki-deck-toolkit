//! Интеграционные проверки исполнителя на настоящих сериях.
//!
//! Каждый тест строит собственный каталог с модулем `fixture`, цель которого —
//! управляемая нагрузка `benchmark-workload` с известными памятью, процессорным
//! временем, кодами завершения и потомками. Серии исполняются настоящим
//! Hyperfine 2.0; результат проверяется тем же контрактом `benchmark_contract`.
//! Пользовательские данные и `decks/**` не читаются.
//!
//! Без Hyperfine 2.x такие проверки ничего не доказывают: в CI (`CI` задана)
//! его отсутствие — ошибка теста, локально — явный пропуск с сообщением.
//! Возможности, которых может не быть у окружения (делегированная cgroup,
//! сетевое пространство имён), проверяются обеими ветвями: измерением там, где
//! они есть, и честной причиной отсутствия там, где их нет; вторая ветвь
//! печатает, что метрика здесь не доказана.

use benchmark_contract::catalog;
use benchmark_contract::result::{self, BenchmarkResult, MetricValue, OutcomeStatus, RunStatus};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const RUNNER: &str = env!("CARGO_BIN_EXE_benchmark-runner");
const WORKLOAD: &str = env!("CARGO_BIN_EXE_benchmark-workload");

const PROFILES: &str = r#"schema_version = 1
repository_root = "workspace"

[[profile]]
id = "cold"
cache = "cold"
network = "online"
description = "Состояние модуля сбрасывается перед прогоном."

[[profile]]
id = "warm"
cache = "warm"
network = "online"
description = "Повторный прогон после прогрева."

[[profile]]
id = "offline-warm"
cache = "warm"
network = "offline"
description = "Тёплый прогон без сети."
"#;

const ENVIRONMENT: &str = r#"
[scenario.environment]
record = ["os", "arch", "module_version", "virtualization", "hyperfine_version"]
comparable = ["os", "arch", "virtualization"]
"#;

const WALL: &str = r#"
[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true
"#;

fn hyperfine_ready() -> bool {
    let ok = Command::new("hyperfine")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("hyperfine 2."));
    if !ok {
        assert!(
            std::env::var_os("CI").is_none(),
            "в CI нужен Hyperfine 2.x: без него серии исполнителя не проверяются"
        );
        eprintln!("ПРОПУСК: Hyperfine 2.x не найден, настоящая серия здесь не проверена");
    }
    ok
}

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Каталог с одним модулем `fixture`; `body` — объявления `[[scenario]]`.
fn fixture(body: &str) -> Fixture {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "benchmark-runner-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("catalog/scenarios")).unwrap();
    fs::create_dir_all(root.join("catalog/workspace")).unwrap();
    fs::write(root.join("catalog/catalog.toml"), PROFILES).unwrap();
    fs::write(
        root.join("catalog/scenarios/fixture.toml"),
        format!("schema_version = 1\nmodule = \"fixture\"\n{body}"),
    )
    .unwrap();
    let fixture = Fixture { root };
    let loaded = catalog::load(&fixture.catalog());
    assert!(
        loaded.catalog.is_some(),
        "тестовый каталог некорректен: {:#?}",
        loaded.diagnostics
    );
    fixture
}

/// Сценарий short-cli с целью `benchmark-workload <target>`.
fn scenario(id: &str, target: &str, runs: &str, extra: &str) -> String {
    format!(
        r#"
[[scenario]]
id = "fixture.{id}"
title = "Проверка исполнителя"
revision = 1
kind = "short-cli"
status = "executable"
description = "Управляемая нагрузка benchmark-workload."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = [{target}]
expect_exit_codes = [0]

[scenario.runs]
{runs}
{extra}
{ENVIRONMENT}"#
    )
}

const RUNS: &str = "warmup = 1\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 30\ntotal_budget_seconds = 300";

const WARM: &str = r#"
[[scenario.profile]]
id = "warm"
cache_definition = "Собственных кэшей нет; прогрев одним запуском."
cleared = []
retained = ["page cache ОС"]
"#;

impl Fixture {
    fn catalog(&self) -> PathBuf {
        self.root.join("catalog")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("catalog/workspace")
    }

    fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    fn command(&self, args: &[&str]) -> Command {
        let workload_dir = Path::new(WORKLOAD).parent().unwrap();
        let path = std::env::join_paths(std::iter::once(workload_dir.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let mut command = Command::new(RUNNER);
        command
            .arg("--catalog")
            .arg(self.catalog())
            .arg("--json")
            .args(args)
            .env("PATH", path)
            // Локальная история теста — во временном каталоге, а не в данных пользователя.
            .env("BENCHMARK_HISTORY_DIR", self.root.join("history"))
            .env_remove("BENCHMARK_RUNNER_DELEGATED_SCOPE");
        command
    }

    fn run(&self, id: &str, profile: &str, extra: &[&str]) -> Run {
        let artifacts = self.artifacts();
        let mut args = vec![
            "run",
            id,
            "--profile",
            profile,
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        Run::from(self.command(&args).output().unwrap(), self)
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.workspace().join(relative)).unwrap_or_default()
    }
}

struct Run {
    code: i32,
    summary: Value,
    stderr: String,
    result: Option<BenchmarkResult>,
}

impl Run {
    fn from(output: Output, fixture: &Fixture) -> Run {
        let summary: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout не JSON ({e}): {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        let result = summary["result"].as_str().map(|path| {
            let text = fs::read_to_string(path).unwrap();
            let catalog = catalog::load(&fixture.catalog()).catalog.unwrap();
            let checked = result::check(&text, path, Some(&catalog));
            checked.result.unwrap_or_else(|| {
                panic!("результат не прошёл контракт: {:#?}", checked.diagnostics)
            })
        });
        Run {
            code: output.status.code().unwrap_or(-1),
            summary,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            result,
        }
    }

    fn result(&self) -> &BenchmarkResult {
        self.result
            .as_ref()
            .unwrap_or_else(|| panic!("результата нет: {}\n{}", self.summary, self.stderr))
    }

    fn values(&self, key: &str) -> Vec<&MetricValue> {
        self.result()
            .runs
            .iter()
            .map(|r| r.values.get(key).expect(key))
            .collect()
    }

    fn measured(&self, key: &str) -> Vec<f64> {
        self.values(key)
            .into_iter()
            .map(|v| v.measured().unwrap_or_else(|| panic!("{key}: {v:?}")))
            .collect()
    }
}

/// Процессы, в командной строке которых есть метка теста.
fn processes_with(marker: &str) -> Vec<i32> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let state = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let zombie = state
            .rsplit_once(')')
            .is_some_and(|(_, r)| r.trim_start().starts_with('Z'));
        if !zombie
            && cmdline
                .split(|b| *b == 0)
                .any(|arg| arg == marker.as_bytes())
        {
            found.push(pid);
        }
    }
    found
}

#[test]
fn short_series_measures_real_memory_and_cpu() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "load",
        r#""--alloc-mib", "48", "--busy-ms", "60""#,
        RUNS,
        &format!(
            r#"{WARM}{WALL}
[[scenario.metric]]
metric = "time_cpu"
scope = "process-tree-waited"
source = "hyperfine"
required = false

[[scenario.metric]]
metric = "memory_peak_resident"
scope = "largest-process-in-tree"
source = "hyperfine"
required = false
"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.load", "warm", &["--runs", "3"]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Success);
    assert_eq!(result.runs.len(), 3);
    assert_eq!(result.warmup_runs, 1);
    assert_eq!(
        result.provenance.measurement.format.as_deref(),
        Some("hyperfine-json-v2")
    );
    for rss in run.measured("memory_peak_resident@largest-process-in-tree") {
        assert!(
            rss >= 48.0 * 1024.0 * 1024.0,
            "пик RSS {rss} меньше занятых 48 МиБ"
        );
    }
    for cpu in run.measured("time_cpu@process-tree-waited") {
        assert!(cpu >= 0.05, "процессорное время {cpu} меньше 60 мс работы");
    }
    for wall in run.measured("time_wall_clock@command") {
        assert!((0.05..5.0).contains(&wall), "время {wall}");
    }
    assert_eq!(result.aggregates["time_wall_clock@command"].count, 3);
    // Каталог артефактов хранит исходные экспорты Hyperfine каждого прогона.
    let artifacts = PathBuf::from(run.summary["artifacts"].as_str().unwrap());
    for name in ["warmup-000.json", "run-000.json", "run-002.json"] {
        let text = fs::read_to_string(artifacts.join("hyperfine").join(name)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["schema_version"],
            2
        );
    }
}

#[test]
fn lifecycle_steps_run_in_declared_order() {
    if !hyperfine_ready() {
        return;
    }
    let step = |name: &str, phase: &str| {
        format!(
            r#"
[[{name}]]
description = "Метка {phase}."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--append", "lifecycle.log", "{phase}"]
touches = "scenario-temp"
timeout_seconds = 10
"#
        )
    };
    let body = scenario(
        "order",
        r#""--append", "lifecycle.log", "target""#,
        "warmup = 2\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 30\ntotal_budget_seconds = 300",
        &format!(
            "{}{}{WARM}{}{}{}{WALL}",
            step("scenario.setup", "setup"),
            step("scenario.cleanup", "cleanup"),
            step("scenario.profile.setup", "profile-setup"),
            step("scenario.profile.prepare", "prepare"),
            step("scenario.profile.conclude", "conclude"),
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.order", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let cycle = "prepare\ntarget\nconclude\n";
    let expected = format!("setup\nprofile-setup\n{}cleanup\n", cycle.repeat(4));
    assert_eq!(fixture.read("lifecycle.log"), expected);
    assert_eq!(run.result().warmup_runs, 2);
}

#[test]
fn prepare_failure_stops_series_as_partial_and_cleanup_still_runs() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "prepare",
        r#""--busy-ms", "5""#,
        "warmup = 0\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 30\ntotal_budget_seconds = 300",
        &format!(
            r#"
[[scenario.cleanup]]
description = "Метка уборки."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--append", "cleanup.log", "cleanup"]
touches = "scenario-temp"
timeout_seconds = 10

[[scenario.profile]]
id = "warm"
cache_definition = "Прогрев шагом setup профиля."
cleared = []
retained = ["page cache ОС"]

[[scenario.profile.setup]]
description = "Прогрев."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
touches = "read-only"
timeout_seconds = 10

[[scenario.profile.prepare]]
description = "Третий вызов завершается неуспехом."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--counter", "prepare.count", "--fail-on-call", "3"]
touches = "scenario-temp"
timeout_seconds = 10
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.prepare", "warm", &["--runs", "5"]);
    assert_eq!(run.code, 1, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Partial);
    assert_eq!(result.runs.len(), 3);
    assert_eq!(result.runs[2].status, RunStatus::Failure);
    assert!(
        result.runs[2]
            .reason
            .as_deref()
            .unwrap()
            .contains("prepare")
    );
    assert!(
        result.runs[2]
            .values
            .values()
            .all(|v| matches!(v, MetricValue::Failed { .. }))
    );
    assert_eq!(fixture.read("cleanup.log"), "cleanup\n");
}

#[test]
fn setup_failure_is_failure_without_runs() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "setup",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!(
            r#"
[[scenario.setup]]
description = "Подготовка, которая не удаётся."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--exit", "3"]
touches = "scenario-temp"
timeout_seconds = 10

[[scenario.cleanup]]
description = "Метка уборки."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--append", "cleanup.log", "cleanup"]
touches = "scenario-temp"
timeout_seconds = 10
{WARM}{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.setup", "warm", &[]);
    assert_eq!(run.code, 4, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert!(result.runs.is_empty());
    assert_eq!(result.warmup_runs, 0);
    assert!(
        result
            .outcome
            .reason
            .as_deref()
            .unwrap()
            .contains("setup[0]")
    );
    assert_eq!(fixture.read("cleanup.log"), "cleanup\n");
}

#[test]
fn unexpected_exit_code_is_a_failed_run_not_success() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "exit",
        r#""--exit", "2""#,
        "warmup = 0\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 30\ntotal_budget_seconds = 300",
        &format!(
            r#"
[[scenario.profile]]
id = "warm"
cache_definition = "Прогрев шагом setup профиля."
cleared = []
retained = ["page cache ОС"]

[[scenario.profile.setup]]
description = "Прогрев."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
touches = "read-only"
timeout_seconds = 10
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.exit", "warm", &[]);
    assert_eq!(run.code, 4, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert_eq!(
        result.runs.len(),
        1,
        "серия останавливается на первом неуспехе"
    );
    assert_eq!(result.runs[0].exit_code, Some(2));
    assert_eq!(result.runs[0].status, RunStatus::Failure);
    // Измеренное время неуспешного прогона сохранено, но в агрегаты не входит.
    assert!(
        run.values("time_wall_clock@command")[0]
            .measured()
            .is_some()
    );
    assert!(result.aggregates.is_empty());
}

#[test]
fn timeout_terminates_the_whole_process_tree() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "timeout",
        r#""--orphan-sleep-ms", "30011", "--sleep-ms", "30011""#,
        "warmup = 0\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 1\ntotal_budget_seconds = 300",
        &format!(
            r#"
[[scenario.profile]]
id = "warm"
cache_definition = "Прогрев шагом setup профиля."
cleared = []
retained = ["page cache ОС"]

[[scenario.profile.setup]]
description = "Прогрев."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
touches = "read-only"
timeout_seconds = 10
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let started = Instant::now();
    let run = fixture.run("fixture.timeout", "warm", &[]);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(run.code, 4, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.runs.len(), 1);
    assert_eq!(result.runs[0].status, RunStatus::Timeout);
    assert!(
        processes_with("30011").is_empty(),
        "после таймаута остались процессы"
    );
}

#[test]
fn process_escaping_the_group_is_reaped_and_fails_the_run() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "escape",
        r#""--escape-sleep-ms", "20017""#,
        RUNS,
        &format!("{WARM}{WALL}"),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.escape", "warm", &[]);
    assert!(processes_with("20017").is_empty(), "сирота пережил серию");
    // Прогрев оставил сироту: серия останавливается до измерений.
    assert_eq!(run.code, 4, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert!(
        result.runs.is_empty()
            || matches!(result.runs[0].reason.as_deref(), Some(r) if r.contains("процессы")),
        "{:?}",
        result.runs
    );
}

#[test]
fn conclude_and_cleanup_failures_are_reported() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "conclude",
        r#""--busy-ms", "2""#,
        RUNS,
        &format!(
            r#"
[[scenario.cleanup]]
description = "Уборка, которая не удаётся."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--exit", "5"]
touches = "scenario-temp"
timeout_seconds = 10
{WARM}
[[scenario.profile.conclude]]
description = "Четвёртый вызов (после второго измеряемого прогона) не удаётся."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--counter", "conclude.count", "--fail-on-call", "3"]
touches = "scenario-temp"
timeout_seconds = 10
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.conclude", "warm", &["--runs", "4"]);
    assert_eq!(run.code, 1, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Partial);
    assert_eq!(
        result.runs.len(),
        2,
        "серия остановлена после conclude второго прогона"
    );
    let reason = result.outcome.reason.as_deref().unwrap();
    assert!(reason.contains("conclude[0]"), "{reason}");
    assert!(reason.contains("уборка не завершена"), "{reason}");
    assert_eq!(run.summary["cleanup"]["ok"], false);
}

#[test]
fn signal_interrupts_series_cleans_up_and_keeps_evidence() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "signal",
        r#""--orphan-sleep-ms", "40023", "--sleep-ms", "40023""#,
        "warmup = 0\nmin_runs = 2\nmax_runs = 5\nsingle_run_allowed = false\nrun_timeout_seconds = 120\ntotal_budget_seconds = 300",
        &format!(
            r#"
[[scenario.cleanup]]
description = "Метка уборки."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--append", "cleanup.log", "cleanup"]
touches = "scenario-temp"
timeout_seconds = 10

[[scenario.profile]]
id = "warm"
cache_definition = "Прогрев шагом setup профиля."
cleared = []
retained = ["page cache ОС"]

[[scenario.profile.setup]]
description = "Прогрев."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
touches = "read-only"
timeout_seconds = 10
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let artifacts = fixture.artifacts();
    let child = fixture
        .command(&[
            "run",
            "fixture.signal",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while processes_with("40023").is_empty() {
        assert!(Instant::now() < deadline, "цель не запустилась");
        std::thread::sleep(Duration::from_millis(50));
    }
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::INT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let run = Run::from(output, &fixture);
    assert_eq!(run.code, 6, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert_eq!(result.runs.len(), 1);
    assert_eq!(result.runs[0].status, RunStatus::Interrupted);
    assert_eq!(fixture.read("cleanup.log"), "cleanup\n");
    assert!(
        processes_with("40023").is_empty(),
        "после сигнала остались процессы"
    );
}

#[test]
fn offline_profile_runs_without_network_or_refuses() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "offline",
        r#""--require-offline""#,
        RUNS,
        &format!(
            r#"
[[scenario.profile]]
id = "offline-warm"
cache_definition = "Собственных кэшей нет."
cleared = []
retained = ["page cache ОС"]
network_definition = "Нагрузка не обращается к сети; исполнитель исполняет серию в новом сетевом пространстве имён."
{WALL}"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.offline", "offline-warm", &[]);
    match run.code {
        0 => {
            let result = run.result();
            assert_eq!(result.outcome.status, OutcomeStatus::Success);
            assert!(result.runs.iter().all(|r| r.exit_code == Some(0)));
        }
        5 => {
            let result = run.result();
            assert_eq!(result.outcome.status, OutcomeStatus::Unsupported);
            assert!(
                result
                    .outcome
                    .reason
                    .as_deref()
                    .unwrap()
                    .contains("offline-isolation")
            );
            eprintln!(
                "ОГРАНИЧЕНИЕ: сетевое пространство имён здесь недоступно, офлайн-изоляция не доказана"
            );
        }
        code => panic!("код {code}: {}\n{}", run.summary, run.stderr),
    }
}

#[test]
fn own_cgroup_metrics_are_measured_or_honestly_absent() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "cgroup",
        r#""--alloc-mib", "96", "--threads", "2", "--busy-ms", "400""#,
        RUNS,
        &format!(
            r#"{WARM}{WALL}
[[scenario.metric]]
metric = "memory_peak_resident"
scope = "largest-process-in-tree"
source = "hyperfine"
required = false

[[scenario.metric]]
metric = "memory_peak_cgroup"
scope = "cgroup"
source = "cgroup-v2"
required = false

[[scenario.metric]]
metric = "cpu_time_cgroup"
scope = "cgroup"
source = "cgroup-v2"
required = false

[[scenario.metric]]
metric = "cpu_utilization_peak"
scope = "cgroup"
source = "cgroup-v2"
required = false
sample_interval_seconds = 0.1

[[scenario.metric]]
metric = "cpu_utilization_mean"
scope = "cgroup"
source = "cgroup-v2"
required = false
"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.cgroup", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let peak = run.values("memory_peak_cgroup@cgroup");
    if peak.iter().all(|v| v.measured().is_some()) {
        for value in run.measured("memory_peak_cgroup@cgroup") {
            assert!(value >= 96.0 * 1024.0 * 1024.0, "пик группы {value}");
        }
        for value in run.measured("cpu_time_cgroup@cgroup") {
            assert!(
                value >= 0.6,
                "процессорное время группы {value} при 2×400 мс"
            );
        }
        for value in run.measured("cpu_utilization_peak@cgroup") {
            assert!(value > 1.0, "пик загрузки двух потоков {value}");
        }
        // Средняя загрузка группы — её процессорное время за время её жизни:
        // два потока не дают больше двух ядер.
        for value in run.measured("cpu_utilization_mean@cgroup") {
            assert!(
                (1.0..=2.2).contains(&value),
                "средняя загрузка двух потоков {value}"
            );
        }
        // Без сэмплирования группа читается только после прогона: пик
        // загрузки отсутствует, итоговые значения остаются.
        let quiet = fixture.run("fixture.cgroup", "warm", &["--no-sampling"]);
        assert_eq!(quiet.code, 0, "{}\n{}", quiet.summary, quiet.stderr);
        for value in quiet.values("cpu_utilization_peak@cgroup") {
            assert!(
                matches!(value, MetricValue::Unavailable { .. }),
                "{value:?}"
            );
        }
        for value in quiet.values("memory_peak_cgroup@cgroup") {
            assert!(value.measured().is_some(), "{value:?}");
        }
    } else {
        for value in peak {
            assert!(
                matches!(
                    value,
                    MetricValue::Unsupported { .. } | MetricValue::Unavailable { .. }
                ),
                "{value:?}"
            );
        }
        eprintln!(
            "ОГРАНИЧЕНИЕ: делегированная cgroup здесь недоступна, метрики группы не доказаны: {:?}",
            run.result().metrics[2].reason
        );
    }
}

/// Регрессия: процессорное время подгруппы включает Hyperfine за всё время
/// его жизни, поэтому делить его на время одной короткой цели нельзя —
/// пустая однопоточная цель получала загрузку в несколько ядер.
#[test]
fn own_cgroup_mean_utilization_of_short_target_stays_physical() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "short-mean",
        r#""--exit", "0""#,
        RUNS,
        &format!(
            r#"{WARM}{WALL}
[[scenario.metric]]
metric = "cpu_utilization_mean"
scope = "cgroup"
source = "cgroup-v2"
required = false
"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.short-mean", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let values = run.measured("cpu_utilization_mean@cgroup");
    if values.is_empty() {
        eprintln!(
            "ОГРАНИЧЕНИЕ: делегированная cgroup здесь недоступна, загрузка группы не проверена"
        );
    }
    for value in values {
        assert!(
            value <= 1.5,
            "однопоточная цель и Hyperfine не занимают {value} ядра в среднем"
        );
    }
}

#[test]
fn host_sampling_reports_observed_load_and_can_be_disabled() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "host",
        r#""--alloc-mib", "128", "--threads", "2", "--busy-ms", "500""#,
        RUNS,
        &format!(
            r#"{WARM}{WALL}
[[scenario.metric]]
metric = "cpu_utilization_peak"
scope = "host"
source = "procfs"
required = false
sample_interval_seconds = 0.1

[[scenario.metric]]
metric = "memory_host_peak_increase"
scope = "host"
source = "procfs"
required = false
sample_interval_seconds = 0.05
"#
        ),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.host", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    for value in run.measured("cpu_utilization_peak@host") {
        assert!(
            value > 0.8,
            "наблюдаемый пик хоста {value} при двух занятых потоках"
        );
    }
    let increases = run.measured("memory_host_peak_increase@host");
    assert!(
        increases.iter().any(|v| *v >= 64.0 * 1024.0 * 1024.0),
        "снижение MemAvailable {increases:?} при 128 МиБ"
    );

    let quiet = fixture.run("fixture.host", "warm", &["--no-sampling"]);
    assert_eq!(quiet.code, 0, "{}\n{}", quiet.summary, quiet.stderr);
    for value in quiet.values("cpu_utilization_peak@host") {
        assert!(
            matches!(value, MetricValue::Unavailable { .. }),
            "{value:?}"
        );
    }
}

#[test]
fn module_report_inputs_facts_counters_and_disk_usage() {
    if !hyperfine_ready() {
        return;
    }
    let report = r#"{"artifact":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":4242},"items":[{"n":2},{"n":3}],"tool":{"version":"1.2.3"}}"#;
    // Литеральные скобки в аргументах каталога удваиваются.
    let escaped = report.replace('{', "{{").replace('}', "}}");
    let target = format!(
        r#""--report-dir", "runs", "--report", '{escaped}', "--write-bytes", "cache/blob", "10000""#
    );
    let body = format!(
        r#"
[[scenario]]
id = "fixture.report"
title = "Отчёт модуля"
revision = 1
kind = "short-cli"
status = "executable"
description = "Управляемая нагрузка пишет машинный отчёт."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = [{target}]
expect_exit_codes = [0]

[[scenario.input]]
id = "artifact"
kind = "downloaded-artifact"
role = "workload"
identity = "file-sha256"
description = "Артефакт, о котором сообщает отчёт."
report_identity = "/artifact/sha256"
size = [{{ name = "bytes", unit = "byte", description = "Размер.", measure = {{ kind = "report", pointer = "/artifact/size" }} }}]

[scenario.runs]
{RUNS}

[scenario.module_report]
runs_dir = "runs"
report = "report.json"
description = "Каждый запуск создаёт runs/<id>/report.json."
{WARM}{WALL}
[[scenario.metric]]
metric = "module_counter"
scope = "module-report"
source = "module-report"
label = "items"
required = false
pointer = "/items/*/n"

[[scenario.metric]]
metric = "disk_usage"
scope = "path"
source = "filesystem"
label = "cache"
required = false
path = "cache"

[scenario.environment]
record = ["os", "arch", "module_version", "tool_version"]
comparable = ["os", "arch", "tool_version"]

[[scenario.environment.fact]]
name = "tool_version"
description = "Версия из отчёта."
report = ["/tool/version"]
"#
    );
    let fixture = fixture(&body);
    fs::create_dir_all(fixture.workspace().join("runs")).unwrap();
    let run = fixture.run("fixture.report", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.inputs.len(), 1);
    assert_eq!(result.inputs[0].identity.value, "a".repeat(64));
    assert_eq!(result.inputs[0].size["bytes"].value, 4242.0);
    assert_eq!(
        result.environment["tool_version"],
        benchmark_contract::result::Fact::Known {
            value: "1.2.3".into()
        }
    );
    assert!(
        run.measured("module_counter@module-report/items")
            .iter()
            .all(|v| *v == 5.0)
    );
    assert!(
        run.measured("disk_usage@path/cache")
            .iter()
            .all(|v| *v >= 10000.0)
    );
}

#[test]
fn input_with_path_and_report_size_fails_run_without_report() {
    if !hyperfine_ready() {
        return;
    }
    let body = format!(
        r#"
[[scenario]]
id = "fixture.noreport"
title = "Без отчёта"
revision = 1
kind = "short-cli"
status = "executable"
description = "Объём входа обещан отчётом, но нагрузка его не пишет."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
expect_exit_codes = [0]

[[scenario.parameter]]
name = "data_dir"
description = "Каталог входа."
required = true
record = false
affects_comparability = false

[[scenario.input]]
id = "data"
kind = "external-path"
role = "workload"
identity = "content-tree-sha256"
parameter = "data_dir"
description = "Вход."
size = [
    {{ name = "files", unit = "count", description = "Файлы дерева.", measure = {{ kind = "tree-files" }} }},
    {{ name = "notes", unit = "count", description = "Из отчёта.", measure = {{ kind = "report", pointer = "/notes" }} }},
]

[scenario.runs]
{RUNS}

[scenario.module_report]
runs_dir = "runs"
report = "report.json"
description = "Каждый запуск должен создать runs/<id>/report.json."
{WARM}{WALL}{ENVIRONMENT}"#
    );
    let fixture = fixture(&body);
    fs::create_dir_all(fixture.workspace().join("runs")).unwrap();
    let data = fixture.root.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("a"), "x").unwrap();
    let param = format!("data_dir={}", data.display());
    let run = fixture.run("fixture.noreport", "warm", &["--param", &param]);
    assert_eq!(run.code, 4, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert_eq!(result.runs[0].status, RunStatus::Failure);
    assert!(
        result.runs[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("«data»")),
        "{:?}",
        result.runs[0].reason
    );
}

#[test]
fn readiness_refuses_missing_or_incompatible_hyperfine() {
    let body = scenario(
        "refuse",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!("{WARM}{WALL}"),
    );
    let fixture = fixture(&body);
    let fake = fixture.root.join("fake-bin");
    fs::create_dir_all(&fake).unwrap();
    let script = fake.join("hyperfine");
    fs::write(&script, "#!/bin/sh\necho 'hyperfine 1.18.0'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let workload_dir = Path::new(WORKLOAD).parent().unwrap();
    let artifacts = fixture.artifacts();
    for (path, expected) in [
        (
            std::env::join_paths([
                fake.as_path(),
                workload_dir,
                Path::new("/usr/bin"),
                Path::new("/bin"),
            ])
            .unwrap(),
            "1.18.0",
        ),
        (std::env::join_paths([workload_dir]).unwrap(), "не найден"),
    ] {
        let output = fixture
            .command(&[
                "run",
                "fixture.refuse",
                "--profile",
                "warm",
                "--artifacts-dir",
                artifacts.to_str().unwrap(),
            ])
            .env("PATH", path)
            .output()
            .unwrap();
        let run = Run::from(output, &fixture);
        assert_eq!(run.code, 5, "{}\n{}", run.summary, run.stderr);
        let result = run.result();
        assert_eq!(result.outcome.status, OutcomeStatus::Unsupported);
        assert!(result.runs.is_empty());
        let reason = result.outcome.reason.as_deref().unwrap();
        assert!(reason.contains(expected), "{reason}");
    }
}

#[test]
fn accepted_result_survives_failed_output_write() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "unwritten",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!("{WARM}{WALL}"),
    );
    let fixture = fixture(&body);
    // Родитель итогового файла — обычный файл: каталог не создаётся.
    let blocker = fixture.root.join("blocker");
    fs::write(&blocker, "").unwrap();
    let output = blocker.join("result.json");
    let artifacts = fixture.artifacts();
    let run = fixture
        .command(&[
            "run",
            "fixture.unwritten",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(3));
    let copies: Vec<_> = fs::read_dir(artifacts.join("runs/fixture.unwritten"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("result.unwritten.json"))
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(copies.len(), 1, "копия принятого результата не сохранена");
    let text = fs::read_to_string(&copies[0]).unwrap();
    let result: BenchmarkResult = serde_json::from_str(&text).unwrap();
    assert!(!result.runs.is_empty());
    let summary: Value = serde_json::from_str(
        &fs::read_to_string(copies[0].with_file_name("summary.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["exit"], "error");
}

#[test]
fn accepted_result_is_archived_in_local_history() {
    if !hyperfine_ready() {
        return;
    }
    let body = scenario(
        "history",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!("{WARM}{WALL}"),
    );
    let fixture = fixture(&body);
    let run = fixture.run("fixture.history", "warm", &[]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    assert_eq!(
        run.summary["history"]["status"], "archived",
        "{}",
        run.summary
    );
    let id = run.summary["history"]["id"].as_str().unwrap();
    let archived = fixture
        .root
        .join("history/results")
        .join(&id[..2])
        .join(format!("{id}.json"));
    let original = fs::read_to_string(run.summary["result"].as_str().unwrap()).unwrap();
    assert_eq!(fs::read_to_string(&archived).unwrap(), original);

    let disabled = fixture.run("fixture.history", "warm", &["--no-history"]);
    assert_eq!(disabled.code, 0);
    assert_eq!(disabled.summary["history"]["status"], "disabled");

    // Недоступное хранилище не стирает измерение и не выдаётся за сохранение.
    let blocker = fixture.root.join("not-a-dir");
    fs::write(&blocker, "").unwrap();
    let artifacts = fixture.artifacts();
    let output = fixture
        .command(&[
            "run",
            "fixture.history",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .env("BENCHMARK_HISTORY_DIR", &blocker)
        .output()
        .unwrap();
    let failed = Run::from(output, &fixture);
    assert_eq!(failed.code, 0, "{}\n{}", failed.summary, failed.stderr);
    assert_eq!(failed.summary["history"]["status"], "failed");
    assert!(failed.summary["history"]["reason"].as_str().is_some());
    assert!(
        failed.summary["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("локальную историю"))
    );
    assert!(Path::new(failed.summary["result"].as_str().unwrap()).is_file());

    // Хранилище внутри рабочей копии отвергается до записи.
    let inside = fixture.workspace().join("history");
    let output = fixture
        .command(&[
            "run",
            "fixture.history",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .env("BENCHMARK_HISTORY_DIR", &inside)
        .output()
        .unwrap();
    let refused = Run::from(output, &fixture);
    assert_eq!(refused.summary["history"]["status"], "failed");
    assert!(!inside.exists());
}

#[test]
fn existing_result_is_never_overwritten_and_runs_are_bounded() {
    let body = scenario(
        "bounds",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!("{WARM}{WALL}"),
    );
    let fixture = fixture(&body);
    let existing = fixture.root.join("result.json");
    fs::write(&existing, "прежний результат").unwrap();
    let run = fixture
        .command(&[
            "run",
            "fixture.bounds",
            "--profile",
            "warm",
            "--output",
            existing.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert_eq!(fs::read_to_string(&existing).unwrap(), "прежний результат");
    for runs in ["1", "6"] {
        let out = fixture
            .command(&["run", "fixture.bounds", "--profile", "warm", "--runs", runs])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "--runs {runs}");
    }
    let unknown = fixture
        .command(&[
            "check",
            "fixture.bounds",
            "--profile",
            "warm",
            "--param",
            "x=1",
        ])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
}

#[test]
fn parallel_series_of_one_module_are_refused() {
    if !hyperfine_ready() {
        return;
    }
    let body = [
        scenario(
            "busy",
            r#""--sleep-ms", "1500""#,
            RUNS,
            &format!("{WARM}{WALL}"),
        ),
        scenario(
            "quick",
            r#""--busy-ms", "1""#,
            RUNS,
            &format!("{WARM}{WALL}"),
        ),
        scenario("fail", r#""--exit", "1""#, RUNS, &format!("{WARM}{WALL}")),
    ]
    .concat();
    // Независимая рабочая копия с тем же модулем: её серии не конкурируют.
    let independent = fixture(&body);
    let fixture = fixture(&body);
    let run_in = |fixture: &Fixture, id: &str, artifacts: &Path| {
        fixture.command(&[
            "run",
            id,
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
    };
    let mut first = run_in(&fixture, "fixture.busy", &fixture.artifacts())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lock = fixture
        .workspace()
        .join("target/benchmark-runner/locks/fixture.lock");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !lock.exists() || processes_with("1500").is_empty() {
        assert!(Instant::now() < deadline, "первая серия не началась");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Другой каталог артефактов не даёт второй блокировки той же рабочей копии.
    let other_artifacts = fixture.root.join("artifacts-other");
    let second = run_in(&fixture, "fixture.quick", &other_artifacts)
        .output()
        .unwrap();
    assert_eq!(
        second.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    let summary: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(summary["exit"], "busy");
    assert!(
        first.try_wait().unwrap().is_none(),
        "первая серия уже завершилась"
    );
    let parallel = Run::from(
        run_in(&independent, "fixture.quick", &independent.artifacts())
            .output()
            .unwrap(),
        &independent,
    );
    assert_eq!(
        parallel.code, 0,
        "{}\n{}",
        parallel.summary, parallel.stderr
    );
    assert!(first.try_wait().unwrap().is_none(), "серии не пересеклись");
    let first = first.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(0));
    // Блокировка освобождается и после успеха, и после отказа.
    let failed = run_in(&fixture, "fixture.fail", &other_artifacts)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(4));
    let again = run_in(&fixture, "fixture.quick", &other_artifacts)
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(0));
}

const REPORT: &str = r#"{"artifact":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":4242},"items":[{"n":2}]}"#;

/// Сценарий, цель которого пишет отчёт из файла `report-<id>.json` рабочей
/// копии; `conclude` после каждого прогона может испортить этот файл.
fn report_scenario(id: &str, with_input: bool, conclude: Option<&str>) -> String {
    let input = if with_input {
        r#"
[[scenario.input]]
id = "artifact"
kind = "downloaded-artifact"
role = "workload"
identity = "file-sha256"
description = "Артефакт, о котором сообщает отчёт."
report_identity = "/artifact/sha256"
size = [{ name = "bytes", unit = "byte", description = "Размер.", measure = { kind = "report", pointer = "/artifact/size" } }]
"#
    } else {
        ""
    };
    let conclude = conclude
        .map(|args| {
            format!(
                r#"
[[scenario.profile.conclude]]
description = "Меняет отчёт следующего прогона."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = [{args}]
touches = "scenario-temp"
timeout_seconds = 10
"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"
[[scenario]]
id = "fixture.{id}"
title = "Отчёт каждого прогона"
revision = 1
kind = "short-cli"
status = "executable"
description = "Нагрузка пишет отчёт из файла."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--report-dir", "runs", "--report-from", "report-{id}.json"]
expect_exit_codes = [0]
{input}
[scenario.runs]
warmup = 0
min_runs = 2
max_runs = 5
single_run_allowed = false
run_timeout_seconds = 30
total_budget_seconds = 300

[scenario.module_report]
runs_dir = "runs"
report = "report.json"
description = "Каждый запуск создаёт runs/<id>/report.json."
{WARM}
[[scenario.profile.setup]]
description = "Тёплый профиль без прогрева: кэшей у нагрузки нет."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
touches = "read-only"
timeout_seconds = 10
{conclude}{WALL}
[[scenario.metric]]
metric = "module_counter"
scope = "module-report"
source = "module-report"
label = "items"
required = false
pointer = "/items/*/n"
{ENVIRONMENT}"#
    )
}

#[test]
fn every_run_must_prove_its_input_with_its_own_report() {
    if !hyperfine_ready() {
        return;
    }
    let without_size = r#"{"artifact":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#
        .replace('{', "{{")
        .replace('}', "}}");
    let broken = [
        (
            "missing",
            r#""--write-bytes", "report-missing.json", "0""#.to_owned(),
        ),
        (
            "corrupt",
            r#""--write-bytes", "report-corrupt.json", "3""#.to_owned(),
        ),
        (
            "nosize",
            format!(
                r#""--write-bytes", "report-nosize.json", "0", "--append", "report-nosize.json", '{without_size}'"#
            ),
        ),
    ];
    let mut body = String::new();
    for (id, args) in &broken {
        body.push_str(&report_scenario(id, true, Some(args)));
    }
    body.push_str(&report_scenario("valid", true, None));
    // Факт серии из отчёта: второй успешный прогон отчёта не оставил, поэтому
    // значение первого не подтверждает факт для всей серии.
    body.push_str(
        &report_scenario(
            "optional",
            false,
            Some(r#""--write-bytes", "report-optional.json", "0""#),
        )
        .replace(
            ENVIRONMENT,
            r#"
[scenario.environment]
record = ["os", "arch", "module_version", "artifact_size"]
comparable = ["os", "arch", "artifact_size"]

[[scenario.environment.fact]]
name = "artifact_size"
description = "Размер артефакта из отчёта."
report = ["/artifact/size"]
"#,
        ),
    );
    let fixture = fixture(&body);
    fs::create_dir_all(fixture.workspace().join("runs")).unwrap();
    for id in ["missing", "corrupt", "nosize", "valid", "optional"] {
        fs::write(
            fixture.workspace().join(format!("report-{id}.json")),
            REPORT,
        )
        .unwrap();
    }

    // Первый прогон доказал вход, второй — нет: накопленная идентичность
    // не делает второй прогон успешным.
    for (id, _) in &broken {
        let run = fixture.run(&format!("fixture.{id}"), "warm", &[]);
        assert_eq!(run.code, 1, "{id}: {}\n{}", run.summary, run.stderr);
        let result = run.result();
        assert_eq!(result.outcome.status, OutcomeStatus::Partial, "{id}");
        assert_eq!(result.runs.len(), 2, "{id}");
        assert_eq!(result.runs[0].status, RunStatus::Success, "{id}");
        assert_eq!(result.runs[1].status, RunStatus::Failure, "{id}");
        let reason = result.runs[1].reason.as_deref().unwrap_or_default();
        assert!(reason.contains("«artifact»"), "{id}: {reason}");
    }

    let valid = fixture.run("fixture.valid", "warm", &[]);
    assert_eq!(valid.code, 0, "{}\n{}", valid.summary, valid.stderr);
    assert_eq!(valid.result().inputs[0].size["bytes"].value, 4242.0);

    // Отчёт, из которого берутся только необязательные метрики, может
    // отсутствовать: метрика неполна, прогон и серия успешны.
    let optional = fixture.run("fixture.optional", "warm", &[]);
    assert_eq!(
        optional.code, 0,
        "{}\n{}",
        optional.summary, optional.stderr
    );
    let result = optional.result();
    assert!(result.runs.iter().all(|r| r.status == RunStatus::Success));
    let counter = result
        .metrics
        .iter()
        .find(|m| m.key == "module_counter@module-report/items")
        .unwrap();
    assert_eq!(counter.availability, result::Availability::Partial);
    match &result.environment["artifact_size"] {
        benchmark_contract::result::Fact::Unavailable { reason } => {
            assert!(reason.contains("прогон 2"), "{reason}")
        }
        other => panic!("факт без отчёта второго прогона подтверждён: {other:?}"),
    }
}

/// Git тестового репозитория — тем же помощником, что и исполнитель: без
/// унаследованных переменных, иначе из Git-хука команды ушли бы в чужой репозиторий.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = benchmark_runner::inputs::git(dir)
        .args([
            "-c",
            "user.name=benchmark-runner-test",
            "-c",
            "user.email=benchmark-runner-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Сценарий с входом `repo-tree`; `extra` дописывается после профиля.
fn tree_scenario(id: &str, probe: Option<&str>, extra: &str) -> String {
    let probe = probe
        .map(|args| {
            format!(
                r#"size = [{{ name = "marker", unit = "count", description = "Из пробы.", measure = {{ kind = "probe", pointer = "/n" }} }}]

[scenario.input.probe]
description = "Проба; здесь моделирует правку рабочей копии во время подготовки."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = [{args}]
touches = "read-only"
timeout_seconds = 10
"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"
[[scenario]]
id = "fixture.{id}"
title = "Происхождение кода"
revision = 1
kind = "short-cli"
status = "executable"
description = "Нагрузка над рабочей копией Git."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
expect_exit_codes = [0]

[[scenario.input]]
id = "tree"
kind = "repo-tree"
role = "subject"
identity = "content-tree-sha256"
description = "Снимок рабочей копии."
{probe}
[scenario.runs]
{RUNS}
{WARM}{extra}{WALL}{ENVIRONMENT}"#
    )
}

#[test]
fn provenance_is_the_snapshot_that_was_measured() {
    if !hyperfine_ready() {
        return;
    }
    let body = [
        tree_scenario("stable", None, ""),
        tree_scenario(
            "drift",
            None,
            r#"
[[scenario.profile.conclude]]
description = "Правит отслеживаемый файл во время серии."
program = { kind = "executable", name = "benchmark-workload" }
args = ["--append", "tracked.txt", "drift"]
touches = "scenario-temp"
timeout_seconds = 10
"#,
        ),
        tree_scenario(
            "preparation",
            Some(r#""--append", "tracked.txt", "probe", "--print-json", '{{"n": 1}}'"#),
            "",
        ),
    ]
    .concat();
    let fixture = fixture(&body);
    let repo = fixture.workspace();
    fs::write(repo.join(".gitignore"), "/target/\n/runs/\n").unwrap();
    fs::write(repo.join("tracked.txt"), "start\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "start"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let stable = fixture.run("fixture.stable", "warm", &[]);
    assert_eq!(stable.code, 0, "{}\n{}", stable.summary, stable.stderr);
    let result = stable.result();
    let code = &result.provenance.code;
    assert_eq!(code.git_commit.as_deref(), Some(head.as_str()));
    assert_eq!(code.dirty, Some(false));
    let tree = code.source_tree_sha256.clone().expect("source_tree_sha256");
    assert_eq!(result.inputs[0].identity.value, tree);

    // Унаследованные переменные Git (запуск из хука другого репозитория) не
    // подменяют происхождение: коммит, признак изменений и дерево — те же.
    let foreign = fixture.root.join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    git(&foreign, &["init", "-q"]);
    fs::write(foreign.join("other.txt"), "other\n").unwrap();
    git(&foreign, &["add", "."]);
    git(&foreign, &["commit", "-qm", "foreign"]);
    assert_ne!(git(&foreign, &["rev-parse", "HEAD"]), head);
    let artifacts = fixture.artifacts();
    let output = fixture
        .command(&[
            "run",
            "fixture.stable",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .env("GIT_DIR", foreign.join(".git"))
        .env("GIT_WORK_TREE", &foreign)
        .env("GIT_INDEX_FILE", foreign.join(".git/index"))
        .output()
        .unwrap();
    let hooked = Run::from(output, &fixture);
    assert_eq!(hooked.code, 0, "{}\n{}", hooked.summary, hooked.stderr);
    let result = hooked.result();
    let code = &result.provenance.code;
    assert_eq!(code.git_commit.as_deref(), Some(head.as_str()));
    assert_eq!(code.dirty, Some(false));
    assert_eq!(code.source_tree_sha256.as_deref(), Some(tree.as_str()));
    assert_eq!(result.inputs[0].identity.value, tree);

    // Правка во время серии: provenance остаётся снимком до измерений,
    // а итог не success.
    let drift = fixture.run("fixture.drift", "warm", &[]);
    assert_eq!(drift.code, 1, "{}\n{}", drift.summary, drift.stderr);
    let result = drift.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Partial);
    assert!(
        result
            .outcome
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("рабочая копия изменилась за серию")),
        "{:?}",
        result.outcome.reason
    );
    let code = &result.provenance.code;
    assert_eq!(code.git_commit.as_deref(), Some(head.as_str()));
    assert_eq!(code.dirty, Some(false));
    assert_eq!(code.source_tree_sha256.as_deref(), Some(tree.as_str()));
    assert_eq!(result.inputs[0].identity.value, tree);
    assert!(!git(&repo, &["status", "--porcelain"]).is_empty());
    git(&repo, &["checkout", "--", "tracked.txt"]);

    // Правка между снимком до сборки и вычислением входов: измерять нечего.
    let preparation = fixture.run("fixture.preparation", "warm", &[]);
    assert_eq!(
        preparation.code, 4,
        "{}\n{}",
        preparation.summary, preparation.stderr
    );
    let result = preparation.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Failure);
    assert!(result.runs.is_empty());
    assert!(
        result
            .outcome
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("во время сборки и вычисления входов")),
        "{:?}",
        result.outcome.reason
    );
    git(&repo, &["checkout", "--", "tracked.txt"]);
}

#[test]
fn lifecycle_step_leaving_processes_is_not_a_completed_step() {
    if !hyperfine_ready() {
        return;
    }
    let step = |table: &str, marker: &str| {
        format!(
            r#"
[[{table}]]
description = "Работа шага продолжается в потомке после выхода."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--spawn-rest", "--sleep-ms", "700", "--touch", "{marker}"]
touches = "scenario-temp"
timeout_seconds = 10
"#
        )
    };
    let body = [
        scenario(
            "setup-lingers",
            r#""--busy-ms", "1""#,
            RUNS,
            &format!("{}{WARM}{WALL}", step("scenario.setup", "setup-effect")),
        ),
        scenario(
            "cleanup-lingers",
            r#""--busy-ms", "1""#,
            RUNS,
            &format!("{}{WARM}{WALL}", step("scenario.cleanup", "cleanup-effect")),
        ),
    ]
    .concat();
    let fixture = fixture(&body);

    let setup = fixture.run("fixture.setup-lingers", "warm", &[]);
    assert_eq!(setup.code, 4, "{}\n{}", setup.summary, setup.stderr);
    let result = setup.result();
    assert!(result.runs.is_empty());
    let reason = result.outcome.reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("setup[0]") && reason.contains("эффект шага не доказан"),
        "{reason}"
    );

    let cleanup = fixture.run("fixture.cleanup-lingers", "warm", &[]);
    assert_eq!(cleanup.code, 1, "{}\n{}", cleanup.summary, cleanup.stderr);
    assert_eq!(cleanup.summary["cleanup"]["ok"], false);
    assert_eq!(cleanup.result().outcome.status, OutcomeStatus::Partial);

    // Работа, оборванная уборкой, действительно не выполнена: success был бы
    // утверждением о состоянии, которого нет.
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!fixture.workspace().join("setup-effect").exists());
    assert!(!fixture.workspace().join("cleanup-effect").exists());
}

#[test]
fn published_result_contains_no_private_paths() {
    if !hyperfine_ready() {
        return;
    }
    let body = format!(
        r#"
[[scenario]]
id = "fixture.private"
title = "Внешний вход"
revision = 1
kind = "short-cli"
status = "executable"
description = "Внешний путь задаётся параметром и не публикуется."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--fail-if-exists", "{{input.data}}/missing", "--busy-ms", "1"]
expect_exit_codes = [0]

[[scenario.parameter]]
name = "data_dir"
description = "Каталог входа."
required = true
record = false
affects_comparability = false

[[scenario.input]]
id = "data"
kind = "external-path"
role = "workload"
identity = "content-tree-sha256"
parameter = "data_dir"
description = "Вход."
size = [
    {{ name = "bytes", unit = "byte", description = "Байты дерева.", measure = {{ kind = "tree-bytes" }} }},
    {{ name = "files", unit = "count", description = "Файлы дерева.", measure = {{ kind = "tree-files" }} }},
    {{ name = "deck", unit = "byte", description = "Один файл.", measure = {{ kind = "file-bytes", path = "deck.json" }} }},
    {{ name = "notes", unit = "count", description = "Из пробы.", measure = {{ kind = "probe", pointer = "/count" }} }},
]

[scenario.input.probe]
description = "Проба сообщает объём."
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--print-json", '{{{{"count": 7}}}}']
touches = "read-only"
timeout_seconds = 10

[scenario.runs]
{RUNS}
{WARM}{WALL}{ENVIRONMENT}"#
    );
    let fixture = fixture(&body);
    let private = fixture.root.join("private-export-7f3a");
    fs::create_dir_all(private.join("media")).unwrap();
    fs::write(private.join("deck.json"), "{}").unwrap();
    fs::write(private.join("media/a.bin"), vec![1u8; 300]).unwrap();
    let param = format!("data_dir={}", private.display());
    let run = fixture.run("fixture.private", "warm", &["--param", &param]);
    assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
    let text = fs::read_to_string(run.summary["result"].as_str().unwrap()).unwrap();
    assert!(
        !text.contains("private-export-7f3a"),
        "путь входа попал в результат"
    );
    assert!(
        !text.contains("data_dir"),
        "непубликуемый параметр попал в результат"
    );
    let input = &run.result().inputs[0];
    assert_eq!(input.size["bytes"].value, 302.0);
    assert_eq!(input.size["files"].value, 2.0);
    assert_eq!(input.size["deck"].value, 2.0);
    assert_eq!(input.size["notes"].value, 7.0);
    assert_eq!(input.identity.value.len(), 64);
}

/// PATH только с Hyperfine и нагрузкой: без systemd-run и Docker, даже если
/// они лежат в одном системном каталоге с Hyperfine.
fn minimal_path(fixture: &Fixture) -> std::ffi::OsString {
    let hyperfine = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("hyperfine"))
        .find(|p| p.is_file())
        .unwrap();
    let bin = fixture.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&hyperfine, bin.join("hyperfine")).unwrap();
    std::env::join_paths([Path::new(WORKLOAD).parent().unwrap(), bin.as_path()]).unwrap()
}

#[test]
fn required_cgroup_metric_without_delegation_is_unsupported() {
    if !hyperfine_ready() {
        return;
    }
    if benchmark_runner::cgroup::probe().is_ok() {
        eprintln!(
            "ПРОПУСК: группа теста уже делегирована, отказ без прав здесь не воспроизводится"
        );
        return;
    }
    let body = scenario(
        "nocgroup",
        r#""--busy-ms", "1""#,
        RUNS,
        &format!(
            r#"{WARM}{WALL}
[[scenario.metric]]
metric = "memory_peak_cgroup"
scope = "cgroup"
source = "cgroup-v2"
required = true

[[scenario.requires]]
kind = "linux-cgroup-v2"
"#
        ),
    );
    let fixture = fixture(&body);
    let artifacts = fixture.artifacts();
    let output = fixture
        .command(&[
            "run",
            "fixture.nocgroup",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .env("PATH", minimal_path(&fixture))
        .output()
        .unwrap();
    let run = Run::from(output, &fixture);
    assert_eq!(run.code, 5, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Unsupported);
    assert!(result.runs.is_empty());
    assert!(
        result
            .outcome
            .reason
            .as_deref()
            .unwrap()
            .contains("linux-cgroup-v2")
    );
}

/// Сценарий, требующий Docker; окружение цели задаёт `DOCKER_HOST` явно.
fn docker_scenario(id: &str, docker_host: &str) -> String {
    format!(
        r#"
[[scenario]]
id = "fixture.{id}"
title = "Docker цели"
revision = 1
kind = "short-cli"
status = "executable"
description = "Нагрузка, которой нужен демон Docker."
measured = "Один запуск нагрузки."
platforms = ["linux"]

[scenario.target]
program = {{ kind = "executable", name = "benchmark-workload" }}
args = ["--busy-ms", "1"]
expect_exit_codes = [0]

[scenario.target.env]
inherit = ["PATH"]
set = {{ DOCKER_HOST = "{docker_host}" }}

[scenario.runs]
{RUNS}
{WARM}{WALL}
[[scenario.requires]]
kind = "docker"

[scenario.environment]
record = ["os", "arch", "module_version", "docker_version"]
comparable = ["os", "arch"]
"#
    )
}

#[test]
fn unreachable_docker_daemon_makes_series_unsupported() {
    let body = docker_scenario("docker", "unix:///nonexistent/benchmark-runner-test.sock");
    let fixture = fixture(&body);
    let artifacts = fixture.artifacts();
    let output = fixture
        .command(&[
            "run",
            "fixture.docker",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let run = Run::from(output, &fixture);
    assert_eq!(run.code, 5, "{}\n{}", run.summary, run.stderr);
    let result = run.result();
    assert_eq!(result.outcome.status, OutcomeStatus::Unsupported);
    assert!(result.outcome.reason.as_deref().unwrap().contains("docker"));
}

/// Готовность, факт версии и цель обращаются к демону из окружения цели, а
/// доступный демон исполнителя не подменяет недоступный демон цели.
#[test]
fn docker_readiness_uses_the_target_environment_only() {
    use std::os::unix::fs::PermissionsExt;
    let reachable = "unix:///reachable.sock";
    let dead = "unix:///dead.sock";
    let body = [
        docker_scenario("docker-target", reachable),
        docker_scenario("docker-dead", dead),
    ]
    .concat();
    let fixture = fixture(&body);
    let bin = fixture.root.join("fake-docker");
    fs::create_dir_all(&bin).unwrap();
    let log = fixture.root.join("docker-hosts.log");
    let script = format!(
        "#!/bin/sh\necho \"$DOCKER_HOST\" >> '{}'\nif [ \"$DOCKER_HOST\" = '{reachable}' ]; then echo 99.0-fake; exit 0; fi\necho 'Cannot connect to the Docker daemon' >&2\nexit 1\n",
        log.display()
    );
    fs::write(bin.join("docker"), script).unwrap();
    fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o755)).unwrap();
    let with_fake = |command: &mut Command, runner_host: &str| {
        let path = std::env::join_paths(
            std::iter::once(bin.clone())
                .chain(std::iter::once(
                    Path::new(WORKLOAD).parent().unwrap().to_path_buf(),
                ))
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )),
        )
        .unwrap();
        command.env("PATH", path).env("DOCKER_HOST", runner_host);
    };
    let hosts = || {
        let text = fs::read_to_string(&log).unwrap_or_default();
        let _ = fs::remove_file(&log);
        text.lines().map(str::to_owned).collect::<Vec<_>>()
    };

    // Исполнитель направлен к мёртвому демону, цель — к рабочему.
    let mut check = fixture.command(&["check", "fixture.docker-target", "--profile", "warm"]);
    with_fake(&mut check, dead);
    let output = check.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(hosts().iter().all(|h| h == reachable));

    if hyperfine_ready() {
        let artifacts = fixture.artifacts();
        let mut run = fixture.command(&[
            "run",
            "fixture.docker-target",
            "--profile",
            "warm",
            "--artifacts-dir",
            artifacts.to_str().unwrap(),
        ]);
        with_fake(&mut run, dead);
        let run = Run::from(run.output().unwrap(), &fixture);
        assert_eq!(run.code, 0, "{}\n{}", run.summary, run.stderr);
        assert_eq!(
            run.result().environment["docker_version"],
            benchmark_contract::result::Fact::Known {
                value: "99.0-fake".into()
            }
        );
        let seen = hosts();
        assert!(
            !seen.is_empty() && seen.iter().all(|h| h == reachable),
            "{seen:?}"
        );
    }

    // Цель направлена к мёртвому демону, исполнитель — к рабочему: запасного
    // перехода на демона исполнителя нет.
    let mut check = fixture.command(&["check", "fixture.docker-dead", "--profile", "warm"]);
    with_fake(&mut check, reachable);
    let output = check.output().unwrap();
    assert_eq!(output.status.code(), Some(5));
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        summary["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b.as_str().unwrap().contains("окружении цели")),
        "{summary}"
    );
    let seen = hosts();
    assert!(
        !seen.is_empty() && seen.iter().all(|h| h == dead),
        "{seen:?}"
    );
}
