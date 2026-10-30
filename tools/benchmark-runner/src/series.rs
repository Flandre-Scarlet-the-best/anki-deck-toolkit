//! Серия «сценарий × профиль»: подготовка, жизненный цикл, прогоны и запись
//! результата.
//!
//! Порядок: сборка `cargo-bin` и вычисление входов (подготовка, вне бюджета
//! и измерения) → `setup` сценария и профиля → прогревы (`prepare` → цель →
//! `conclude`) → измеряемые прогоны (`prepare` → цель → `conclude`) →
//! `cleanup` сценария всегда, в том числе после отказа и сигнала. Бюджет
//! серии отсчитывается от начала `setup` и ограничивает каждый шаг и прогон до
//! последнего `conclude`; подготовку ограничивают `--build-timeout` и
//! `timeout_seconds` проб, уборку — `timeout_seconds` её шагов.
//! Отказ `setup` или прогрева даёт `failure` без прогонов; отказ `prepare`,
//! `conclude`, таймаут, неожиданный код цели или сигнал останавливают серию.
//!
//! Происхождение кода снимается до сборки и повторно после вычисления входов:
//! измеряется только та рабочая копия, из которой собраны программы и
//! вычислена идентичность `repo-tree`. После уборки снимок сверяется ещё раз,
//! и изменение исходников за серию не даёт итога `success`.

use crate::cgroup::{self, Counters, Delegated, RunGroup};
use crate::environment::{self, known, unavailable};
use crate::hyperfine_exec::{self, Output};
use crate::inputs;
use crate::plan::Selection;
use crate::procs::{self, Bounds, Ending, Finished};
use crate::programs::{self, Resolved};
use crate::readiness::{Collector, Missing, OwnCgroup, Readiness, collector};
use crate::sampler::{self, ContainerQuery, GroupSource, Samples};
use crate::util;
use benchmark_contract::diagnostic::{Diagnostic, Severity};
use benchmark_contract::hyperfine as import;
use benchmark_contract::pointer;
use benchmark_contract::result::{
    self, Availability, BenchmarkResult, CodeState, Fact, Identity, InputRecord, MetricRecord,
    MetricValue, Observation, Outcome, OutcomeStatus, ProfileRef, Provenance, Quantity, RunRecord,
    RunStatus, ScenarioRef, Tool,
};
use benchmark_contract::scenario::{
    IdentityMethod, Input, InputKind, MetricRequest, SizeMeasure, Step,
};
use benchmark_contract::sensitive;
use benchmark_contract::vocabulary::{BuiltinFact, MetricId, RESULT_SCHEMA_VERSION, WorkloadKind};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Сколько сохранение в историю ждёт хранилище, занятое другой командой:
/// побочный шаг не задерживает итог серии и блокировку модуля надолго.
pub const HISTORY_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Стабильные коды завершения `run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exit {
    Success = 0,
    Partial = 1,
    Usage = 2,
    Error = 3,
    Failure = 4,
    Unsupported = 5,
    Interrupted = 6,
    Busy = 7,
    Rejected = 8,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub sampling: bool,
    pub output: Option<PathBuf>,
    /// Корень локальной истории или причина, по которой она не ведётся.
    pub history: Result<PathBuf, HistoryOff>,
    pub artifacts_root: PathBuf,
    pub build_timeout: Duration,
}

/// Почему принятый результат не сохраняется в локальную историю.
#[derive(Debug, Clone)]
pub enum HistoryOff {
    /// Пользователь отключил историю (`--no-history`).
    Disabled,
    /// Расположение хранилища не определено или недопустимо.
    Unavailable(String),
}

/// Что стало с результатом в локальной истории `benchmark-history`.
#[derive(Debug, Clone, Serialize)]
pub struct History {
    pub status: HistoryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryStatus {
    /// Результат принят историей впервые.
    Archived,
    /// Такой результат в истории уже был.
    AlreadyArchived,
    /// История отключена `--no-history`.
    Disabled,
    /// Результат записан, но в историю не сохранён: см. `reason`.
    Failed,
    /// Принятого результата нет — сохранять нечего.
    NotRecorded,
}

impl History {
    fn not_recorded() -> History {
        History {
            status: HistoryStatus::NotRecorded,
            id: None,
            reason: None,
        }
    }
}

/// Машинная сводка серии для агентов.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub ok: bool,
    pub exit_code: i32,
    pub exit: Exit,
    pub scenario: String,
    pub profile: String,
    pub outcome: Option<OutcomeStatus>,
    pub reason: Option<String>,
    pub result: Option<PathBuf>,
    pub artifacts: Option<PathBuf>,
    pub runs: usize,
    pub successful_runs: usize,
    pub cleanup: CleanupState,
    /// Сохранение принятого результата в локальную историю.
    pub history: History,
    pub warnings: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CleanupState {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

struct Log {
    file: Mutex<Option<fs::File>>,
}

impl Log {
    fn line(&self, text: &str) {
        let stamp = util::utc_rfc3339(util::unix_now());
        if let Ok(mut file) = self.file.lock()
            && let Some(file) = file.as_mut()
        {
            let _ = writeln!(file, "{stamp} {text}");
        }
        eprintln!("benchmark-runner: {text}");
    }
}

/// Почему серия остановилась до конца.
#[derive(Debug, Clone)]
struct Stop {
    reason: String,
    interrupted: bool,
}

enum StepError {
    Failed(String),
    Stopped(Stop),
}

/// Итог одного вызова цели под Hyperfine.
struct Invocation {
    finished: Option<Finished>,
    spawn_error: Option<String>,
    export: Option<Result<import::Imported, String>>,
    samples: Samples,
    own_final: Option<Counters>,
    /// Время от входа Hyperfine в подгруппу до чтения итоговых счётчиков.
    /// Процессорное время группы включает Hyperfine за весь этот интервал,
    /// поэтому загрузка группы делится на него, а не на время одной цели.
    own_span: Option<f64>,
    own_leftovers: bool,
    timeout: Duration,
    budget_limited: bool,
}

struct Series<'a> {
    selection: &'a Selection<'a>,
    readiness: &'a Readiness,
    request: &'a Request,
    runner: &'a Path,
    programs: Resolved,
    dir: PathBuf,
    log: Log,
    deadline: Option<Instant>,
    step_counter: usize,
    own: Option<Delegated>,
    warnings: Vec<String>,
}

pub fn execute(
    selection: &Selection<'_>,
    readiness: &Readiness,
    request: &Request,
    runner: &Path,
) -> Result<Summary, String> {
    let scenario = &selection.item.scenario;
    let started_at = util::unix_now();
    let dir = request
        .artifacts_root
        .join("runs")
        .join(&scenario.id)
        .join(format!(
            "{}-{}-{}",
            util::utc_compact(started_at),
            selection.binding.id,
            util::random_suffix()
        ));
    for sub in ["steps", "hyperfine", "output", "samples", "reports"] {
        fs::create_dir_all(dir.join(sub))
            .map_err(|e| format!("каталог артефактов не создан: {e}"))?;
    }
    // Блокировка живёт в самой рабочей копии, а не в каталоге артефактов:
    // `--artifacts-dir` не разводит серии одного модуля по разным блокировкам,
    // а любой путь к той же рабочей копии открывает тот же файл.
    let locks = lock_dir(selection.repository_root());
    fs::create_dir_all(&locks).map_err(|e| format!("каталог блокировок не создан: {e}"))?;
    let lock = fs::File::create(locks.join(format!("{}.lock", selection.item.module)))
        .map_err(|e| format!("файл блокировки не создан: {e}"))?;
    if let Err(error) = lock.try_lock() {
        let busy = matches!(error, fs::TryLockError::WouldBlock);
        let _ = fs::remove_dir_all(&dir);
        return Ok(Summary {
            ok: false,
            exit_code: if busy { Exit::Busy } else { Exit::Error } as i32,
            exit: if busy { Exit::Busy } else { Exit::Error },
            scenario: scenario.id.clone(),
            profile: selection.binding.id.clone(),
            outcome: None,
            reason: Some(if busy {
                format!(
                    "уже идёт другая серия модуля {}: серии одного модуля делят его состояние и не запускаются параллельно",
                    selection.item.module
                )
            } else {
                format!("блокировка модуля не получена: {error:?}")
            }),
            result: None,
            artifacts: None,
            runs: 0,
            successful_runs: 0,
            cleanup: CleanupState {
                ok: true,
                reason: None,
            },
            history: History::not_recorded(),
            warnings: Vec::new(),
            diagnostics: Vec::new(),
        });
    }
    let log = Log {
        file: Mutex::new(fs::File::create(dir.join("runner.log")).ok()),
    };
    log.line(&format!(
        "серия {} · профиль {} · прогонов {} · артефакты {}",
        scenario.id,
        selection.binding.id,
        selection.runs,
        dir.display()
    ));
    let mut series = Series {
        selection,
        readiness,
        request,
        runner,
        programs: Resolved::default(),
        dir,
        log,
        deadline: None,
        step_counter: 0,
        own: None,
        warnings: Vec::new(),
    };
    let summary = series.run();
    drop(lock);
    summary
}

impl Series<'_> {
    fn run(&mut self) -> Result<Summary, String> {
        let scenario = &self.selection.item.scenario;
        let mut state = State {
            environment: self.base_environment(),
            code: CodeSnapshot::take(self.selection.repository_root()),
            ..State::default()
        };

        if !self.readiness.ready() {
            let reason = self.readiness.blockers.join("; ");
            self.log
                .line(&format!("серия не может измеряться здесь: {reason}"));
            state.outcome = Some((OutcomeStatus::Unsupported, reason));
            return self.finish(
                state,
                CleanupState {
                    ok: true,
                    reason: None,
                },
            );
        }

        // Подготовка: сборка и входы. Не входит в бюджет и измерение.
        let programs = self.selection.programs();
        self.log.line("подготовка: разрешение и сборка программ");
        match programs::resolve(
            &programs,
            self.selection.repository_root(),
            &self.dir.join("steps"),
            self.request.build_timeout,
        ) {
            Ok(resolved) => self.programs = resolved,
            Err(reason) => {
                state.stop = Some(Stop {
                    reason: format!("подготовка: {reason}"),
                    interrupted: procs::interrupted(),
                });
                return self.finish(
                    state,
                    CleanupState {
                        ok: true,
                        reason: None,
                    },
                );
            }
        }
        if let Err(reason) = self
            .resolve_inputs(&mut state)
            .and_then(|()| self.confirm_code(&mut state))
        {
            state.stop = Some(Stop {
                reason: format!("подготовка входов: {reason}"),
                interrupted: procs::interrupted(),
            });
            return self.finish(
                state,
                CleanupState {
                    ok: true,
                    reason: None,
                },
            );
        }
        match &self.readiness.own_cgroup {
            OwnCgroup::Ready(delegated) => match delegated.activate(
                self.selection
                    .item
                    .scenario
                    .metrics
                    .iter()
                    .any(|m| m.metric.is_io()),
            ) {
                Ok(()) => self.own = Some(delegated.clone()),
                Err(reason) => self.mark_own_unavailable(&mut state, &reason),
            },
            OwnCgroup::ViaScope(_, _) => self.mark_own_unavailable(
                &mut state,
                "группа исполнителя не делегирована, а перезапуск в systemd-run не выполнен",
            ),
            OwnCgroup::Unavailable(_) | OwnCgroup::NotNeeded => {}
        }

        self.deadline =
            Some(Instant::now() + Duration::from_secs(scenario.runs.total_budget_seconds));
        let lifecycle = self.lifecycle(&mut state);
        if let Err(stop) = lifecycle {
            state.stop = Some(stop);
        }
        let cleanup = self.cleanup();
        let after = CodeSnapshot::take(self.selection.repository_root());
        if let Some(changed) = state.code.difference(&after) {
            let reason = format!(
                "рабочая копия изменилась за серию ({changed}): прогоны могли измерить не тот код, что записан в provenance"
            );
            self.log.line(&reason);
            state.drift = Some(reason);
        }
        self.finish(state, cleanup)
    }

    /// Рабочая копия не изменилась за сборку и вычисление входов, а
    /// идентичность `repo-tree` совпадает со снимком происхождения.
    fn confirm_code(&mut self, state: &mut State) -> Result<(), String> {
        let now = CodeSnapshot::take(self.selection.repository_root());
        if let Some(changed) = state.code.difference(&now) {
            return Err(format!(
                "рабочая копия изменилась во время сборки и вычисления входов ({changed}): измеряемые программы и записанное происхождение относились бы к разным состояниям"
            ));
        }
        for input in &self.selection.item.scenario.inputs {
            if input.kind != InputKind::RepoTree {
                continue;
            }
            let recorded = state.inputs.get(&input.id).map(|r| &r.identity.value);
            if let (Some(recorded), Some(tree)) = (recorded, &now.source_tree_sha256)
                && recorded != tree
            {
                return Err(format!(
                    "идентичность входа «{}» не совпадает со снимком исходников: дерево менялось во время подготовки",
                    input.id
                ));
            }
        }
        state.code = now;
        Ok(())
    }

    fn lifecycle(&mut self, state: &mut State) -> Result<(), Stop> {
        let scenario = &self.selection.item.scenario;
        let binding = self.selection.binding;
        for (index, step) in scenario.setup.iter().enumerate() {
            self.step("setup", index, step, true)
                .map_err(|e| self.step_stop(e))?;
        }
        for (index, step) in binding.setup.iter().enumerate() {
            self.step("profile-setup", index, step, true)
                .map_err(|e| self.step_stop(e))?;
        }
        for warmup in 0..scenario.runs.warmup {
            self.log
                .line(&format!("прогрев {}/{}", warmup + 1, scenario.runs.warmup));
            self.prepare()?;
            let invocation = self.invoke(&format!("warmup-{warmup:03}"), false);
            if let Some(stop) = self.invocation_stop(&invocation) {
                return Err(stop);
            }
            let leftovers = invocation.finished.map_or(0, |f| f.leftovers);
            if leftovers > 0 || invocation.own_leftovers {
                return Err(Stop {
                    reason: format!(
                        "прогрев {}: после выхода цели остались её процессы; они завершены исполнителем",
                        warmup + 1
                    ),
                    interrupted: false,
                });
            }
            match &invocation.export {
                Some(Ok(imported))
                    if imported
                        .runs
                        .first()
                        .is_some_and(|r| r.status == RunStatus::Success) => {}
                Some(Ok(imported)) => {
                    return Err(Stop {
                        reason: format!(
                            "прогрев {}: {}",
                            warmup + 1,
                            imported
                                .runs
                                .first()
                                .and_then(|r| r.reason.clone())
                                .unwrap_or_else(|| "цель завершилась неуспехом".into())
                        ),
                        interrupted: false,
                    });
                }
                _ => {
                    return Err(Stop {
                        reason: format!(
                            "прогрев {}: {}",
                            warmup + 1,
                            invocation_failure(&invocation)
                        ),
                        interrupted: false,
                    });
                }
            }
            self.conclude()?;
            state.warmups += 1;
        }

        for index in 0..self.selection.runs {
            if procs::interrupted() {
                return Err(interrupted_stop());
            }
            if self.remaining().is_some_and(|r| r < Duration::from_secs(1)) {
                return Err(Stop {
                    reason: format!(
                        "бюджет серии {} с исчерпан после {index} прогонов",
                        scenario.runs.total_budget_seconds
                    ),
                    interrupted: false,
                });
            }
            self.log
                .line(&format!("прогон {}/{}", index + 1, self.selection.runs));
            if let Err(error) = self.prepare_steps() {
                let stop = self.step_stop(error);
                state.runs.push(failed_run(
                    state.runs.len(),
                    status_of(&stop),
                    format!("prepare: {}", stop.reason),
                    self.metric_keys(),
                ));
                return Err(stop);
            }
            let reports_before = self.report_snapshot();
            let invocation = self.invoke(&format!("run-{index:03}"), true);
            let report = self.new_report(&reports_before, index);
            let stop = self.invocation_stop(&invocation);
            let record = self.record_run(state, index as usize, &invocation, report);
            let success = record.status == RunStatus::Success;
            let reason = record.reason.clone();
            state.runs.push(record);
            if let Some(stop) = stop {
                return Err(stop);
            }
            if !success {
                return Err(Stop {
                    reason: format!(
                        "прогон {}: {}",
                        index + 1,
                        reason.unwrap_or_else(|| "неуспех".into())
                    ),
                    interrupted: false,
                });
            }
            self.conclude()?;
        }
        Ok(())
    }

    fn step_stop(&self, error: StepError) -> Stop {
        match error {
            StepError::Failed(reason) => Stop {
                reason,
                interrupted: false,
            },
            StepError::Stopped(stop) => stop,
        }
    }

    fn prepare(&mut self) -> Result<(), Stop> {
        self.prepare_steps().map_err(|e| self.step_stop(e))
    }

    fn prepare_steps(&mut self) -> Result<(), StepError> {
        let steps = &self.selection.binding.prepare;
        for (index, step) in steps.iter().enumerate() {
            self.step("prepare", index, step, true)?;
        }
        Ok(())
    }

    fn conclude(&mut self) -> Result<(), Stop> {
        let steps = &self.selection.binding.conclude;
        for (index, step) in steps.iter().enumerate() {
            self.step("conclude", index, step, true)
                .map_err(|e| self.step_stop(e))?;
        }
        Ok(())
    }

    /// `cleanup` выполняется всегда; первый сигнал её не прерывает.
    fn cleanup(&mut self) -> CleanupState {
        let steps = &self.selection.item.scenario.cleanup;
        let mut failures = Vec::new();
        for (index, step) in steps.iter().enumerate() {
            if procs::forced() {
                failures.push(format!(
                    "cleanup[{index}] не выполнен: повторный сигнал прервал уборку"
                ));
                continue;
            }
            self.deadline = None;
            if let Err(error) = self.step("cleanup", index, step, false) {
                failures.push(match error {
                    StepError::Failed(reason) => reason,
                    StepError::Stopped(stop) => stop.reason,
                });
            }
        }
        if failures.is_empty() {
            CleanupState {
                ok: true,
                reason: None,
            }
        } else {
            let reason = failures.join("; ");
            self.log.line(&format!("уборка неполна: {reason}"));
            CleanupState {
                ok: false,
                reason: Some(reason),
            }
        }
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Предел шага или прогона с учётом остатка бюджета.
    fn bounded(&self, own: Duration) -> (Duration, bool) {
        match self.remaining() {
            Some(rest) if rest < own => (rest, true),
            _ => (own, false),
        }
    }

    fn grace(&self) -> Duration {
        match self.selection.item.scenario.kind {
            WorkloadKind::ShortCli => Duration::from_secs(2),
            WorkloadKind::LongE2e => Duration::from_secs(60),
        }
    }

    fn step(
        &mut self,
        phase: &str,
        index: usize,
        step: &Step,
        series: bool,
    ) -> Result<(), StepError> {
        if series && procs::interrupted() {
            return Err(StepError::Stopped(interrupted_stop()));
        }
        let label = format!("{phase}[{index}] «{}»", step.description);
        let argv = self
            .argv(&step.program, &step.args)
            .map_err(StepError::Failed)?;
        let argv = match (&self.readiness.isolation, series) {
            (Some(isolation), true) => isolation.wrap(argv),
            _ => argv,
        };
        let env = self
            .selection
            .environment(&step.env)
            .map_err(|e| StepError::Failed(format!("{label}: {e}")))?;
        self.step_counter += 1;
        let log_path = self
            .dir
            .join("steps")
            .join(format!("{:03}-{phase}-{index}.log", self.step_counter));
        let (timeout, limited) = self.bounded(Duration::from_secs(step.timeout_seconds));
        if timeout.is_zero() {
            return Err(StepError::Stopped(Stop {
                reason: format!("{label}: бюджет серии исчерпан"),
                interrupted: false,
            }));
        }
        let mut command = command_for(&argv, &env, &self.selection.working_dir(&step.working_dir));
        let output = fs::File::create(&log_path)
            .map_err(|e| StepError::Failed(format!("{label}: журнал шага: {e}")))?;
        let error_output = output
            .try_clone()
            .map_err(|e| StepError::Failed(format!("{label}: журнал шага: {e}")))?;
        command.stdout(output).stderr(error_output);
        self.log.line(&label);
        let finished = procs::run(
            &mut command,
            Bounds {
                timeout,
                grace: self.grace(),
                interruptible: series,
            },
        )
        .map_err(|e| StepError::Failed(format!("{label}: не запускается: {e}")))?;
        // Потомок, переживший успешный шаг, завершается уборкой посреди своей
        // работы: объявленный эффект шага (состояние кэша, данные сценария,
        // уборка) тогда не доказан, и шаг не считается выполненным.
        if finished.leftovers > 0 && finished.ending == Ending::Exited(0) {
            return Err(StepError::Failed(format!(
                "{label}: после выхода шага остались его процессы ({}); они завершены исполнителем, поэтому эффект шага не доказан",
                finished.leftovers
            )));
        }
        if finished.leftovers > 0 {
            self.warn(format!(
                "{label}: после шага остались процессы ({}); они завершены",
                finished.leftovers
            ));
        }
        match finished.ending {
            Ending::Exited(0) => Ok(()),
            Ending::Interrupted => Err(StepError::Stopped(Stop {
                reason: format!("{label}: прерван сигналом"),
                interrupted: true,
            })),
            Ending::TimedOut if limited => Err(StepError::Stopped(Stop {
                reason: format!("{label}: бюджет серии исчерпан"),
                interrupted: false,
            })),
            Ending::TimedOut => Err(StepError::Failed(format!(
                "{label}: превышен timeout_seconds = {}",
                step.timeout_seconds
            ))),
            ending => Err(StepError::Failed(format!("{label}: {}", ending.describe()))),
        }
    }

    fn warn(&mut self, text: String) {
        self.log.line(&format!("предупреждение: {text}"));
        self.warnings.push(text);
    }

    fn argv(
        &self,
        program: &benchmark_contract::scenario::Program,
        args: &[String],
    ) -> Result<Vec<String>, String> {
        let path = self
            .programs
            .path(program)
            .ok_or_else(|| format!("программа {} не разрешена", programs::label(program)))?;
        let mut argv = vec![path.to_string_lossy().into_owned()];
        argv.extend(self.selection.substitute_all(args)?);
        Ok(argv)
    }

    /// Один вызов цели под Hyperfine вместе с опросом метрик.
    fn invoke(&mut self, name: &str, measured: bool) -> Invocation {
        let scenario = &self.selection.item.scenario;
        let (timeout, budget_limited) =
            self.bounded(Duration::from_secs(scenario.runs.run_timeout_seconds));
        let mut invocation = Invocation {
            finished: None,
            spawn_error: None,
            export: None,
            samples: Samples::default(),
            own_final: None,
            own_span: None,
            own_leftovers: false,
            timeout,
            budget_limited,
        };
        let Some(hyperfine) = self.readiness.hyperfine.clone() else {
            invocation.spawn_error = Some("Hyperfine не обнаружен".into());
            return invocation;
        };
        // Аргументы цели подставляются ровно один раз: повторная подстановка
        // раскрыла бы удвоенные литеральные скобки.
        let target = match self.selection.target_args().and_then(|args| {
            let path = self
                .programs
                .path(&scenario.target.program)
                .ok_or_else(|| "программа цели не разрешена".to_owned())?;
            let mut argv = vec![path.to_string_lossy().into_owned()];
            argv.extend(args);
            Ok(argv)
        }) {
            Ok(argv) => argv,
            Err(reason) => {
                invocation.spawn_error = Some(reason);
                return invocation;
            }
        };
        // Окружение разрешается до создания подгруппы: ранний выход не должен
        // оставлять пустую группу, которая заняла бы имя следующей серии.
        let env = match self.selection.environment(&scenario.target.env) {
            Ok(env) => env,
            Err(reason) => {
                invocation.spawn_error = Some(reason);
                return invocation;
            }
        };
        let group = match &self.own {
            Some(own) => match own.create(name) {
                Ok(group) => Some(group),
                Err(error) => {
                    self.warn(format!("{name}: подгруппа прогона не создана: {error}"));
                    None
                }
            },
            None => None,
        };
        let started_at_marker = if group.is_some() {
            let marker_dir = match fs::canonicalize(self.dir.join("samples")) {
                Ok(dir) => dir,
                Err(error) => {
                    self.warn(format!(
                        "{name}: каталог метки времени не разрешается: {error}"
                    ));
                    if let Some(group) = &group {
                        group.remove();
                    }
                    invocation.spawn_error = Some("каталог метки времени не разрешается".into());
                    return invocation;
                }
            };
            let marker = marker_dir.join(format!("{name}.cgroup-start"));
            if let Err(error) = fs::remove_file(&marker)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                self.warn(format!("{name}: прежняя метка времени не удалена: {error}"));
                if let Some(group) = &group {
                    group.remove();
                }
                invocation.spawn_error = Some("прежняя метка времени не удалена".into());
                return invocation;
            }
            Some(marker)
        } else {
            None
        };
        let export = self.dir.join("hyperfine").join(format!("{name}.json"));
        let target_output = self.dir.join("output").join(format!("{name}.log"));
        let output = match scenario.kind {
            WorkloadKind::ShortCli => Output::Null,
            WorkloadKind::LongE2e => Output::File(&target_output),
        };
        let metrics: Vec<&str> = if self.readiness.hyperfine_metrics.is_empty() {
            vec!["time_wall_clock"]
        } else {
            self.readiness.hyperfine_metrics.clone()
        };
        let mut argv = hyperfine_exec::argv(&hyperfine, &target, &metrics, &export, output);
        if let Some(isolation) = &self.readiness.isolation {
            argv = isolation.wrap(argv);
        }
        // В подгруппу прогона переносится Hyperfine до начала измерения, а
        // цель рождается уже в ней. Перенос самой цели попал бы в измеряемый
        // интервал: миграция процесса между группами ждёт синхронизации RCU и
        // на практике добавляет к первому fork/clone цели десятки миллисекунд.
        if let (Some(group), Some(started_at_marker)) = (&group, &started_at_marker) {
            let mut wrapped = vec![
                self.runner.to_string_lossy().into_owned(),
                "__enter-cgroup".to_owned(),
                group.dir.to_string_lossy().into_owned(),
                started_at_marker.to_string_lossy().into_owned(),
                "--".to_owned(),
            ];
            wrapped.append(&mut argv);
            argv = wrapped;
        }
        let mut command = command_for(
            &argv,
            &env,
            &self.selection.working_dir(&scenario.target.working_dir),
        );
        let hyperfine_log = self.dir.join("hyperfine").join(format!("{name}.log"));
        match fs::File::create(&hyperfine_log) {
            Ok(file) => {
                let copy = file.try_clone();
                command.stdout(file);
                if let Ok(copy) = copy {
                    command.stderr(copy);
                }
            }
            Err(_) => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }

        let plan =
            (measured && self.request.sampling).then(|| self.sampler_plan(group.as_ref(), &env));
        let handle = plan.map(sampler::start);
        let result = procs::run_observed(
            &mut command,
            Bounds {
                timeout,
                grace: self.grace(),
                interruptible: true,
            },
            |pid| {
                if let Some(handle) = &handle {
                    handle.set_root(pid);
                }
            },
        );
        // Итоговые счётчики и конец интервала снимаются сразу после завершения цели,
        // до остановки сэмплера и записи файла сэмплов: эти затраты зависят от режима
        // и не должны попадать в знаменатель средней загрузки.
        if let Some(group) = &group {
            invocation.own_final = cgroup::read(
                &group.dir,
                scenario.metrics.iter().any(|m| m.metric.is_io()),
            );
            let ended_ns = cgroup::monotonic_ns();
            invocation.own_span = started_at_marker
                .as_deref()
                .and_then(|marker| cgroup::span_since_marker(marker, ended_ns));
            if let Some(marker) = &started_at_marker {
                let _ = fs::remove_file(marker);
            }
        }
        if let Some(handle) = handle {
            invocation.samples = handle.finish();
            let _ = fs::write(
                self.dir.join("samples").join(format!("{name}.json")),
                samples_json(&invocation.samples),
            );
        }
        if let Some(group) = &group {
            invocation.own_leftovers = group.kill();
            group.remove();
        }
        match result {
            Ok(finished) => invocation.finished = Some(finished),
            Err(error) => {
                invocation.spawn_error = Some(format!("Hyperfine не запускается: {error}"))
            }
        }
        if invocation
            .finished
            .is_some_and(|f| matches!(f.ending, Ending::Exited(_)))
        {
            invocation.export = Some(read_export(&export, scenario));
        }
        invocation
    }

    fn sampler_plan(&self, group: Option<&RunGroup>, env: &[(String, String)]) -> sampler::Plan {
        let scenario = &self.selection.item.scenario;
        let wanted = |c: Collector| {
            scenario.metrics.iter().any(|m| {
                collector(self.selection, m) == c
                    && !self.readiness.unavailable.contains_key(&m.key())
            })
        };
        let host = wanted(Collector::HostProcfs);
        let group_source = match (group, &scenario.container) {
            (Some(group), _) => GroupSource::Own(group.dir.clone()),
            (None, Some(container)) if wanted(Collector::ContainerCgroup) => {
                GroupSource::Container(ContainerQuery {
                    label: container.label.clone(),
                    owner_pid_label: container.owner_pid_label.clone(),
                    // Сэмплер обращается к тому же демону Docker, что и цель.
                    docker_env: env.to_vec(),
                })
            }
            _ => GroupSource::None,
        };
        let declared = scenario
            .metrics
            .iter()
            .filter_map(|m| m.sample_interval_seconds)
            .fold(f64::INFINITY, f64::min);
        let group_tick = if matches!(group_source, GroupSource::None) {
            f64::INFINITY
        } else {
            0.25
        };
        let tick = declared.min(group_tick).clamp(0.02, 1.0);
        sampler::Plan {
            host,
            io: scenario.metrics.iter().any(|m| m.metric.is_io()),
            tick: Duration::from_secs_f64(tick),
            group: group_source,
        }
    }

    fn invocation_stop(&self, invocation: &Invocation) -> Option<Stop> {
        let finished = invocation.finished?;
        match finished.ending {
            Ending::Interrupted => Some(interrupted_stop()),
            Ending::TimedOut if invocation.budget_limited => Some(Stop {
                reason: format!(
                    "бюджет серии {} с исчерпан во время прогона",
                    self.selection.item.scenario.runs.total_budget_seconds
                ),
                interrupted: false,
            }),
            Ending::TimedOut => Some(Stop {
                reason: format!(
                    "прогон превысил run_timeout_seconds = {}",
                    self.selection.item.scenario.runs.run_timeout_seconds
                ),
                interrupted: false,
            }),
            _ => None,
        }
    }

    fn report_root(&self) -> Option<PathBuf> {
        let report = self.selection.item.scenario.module_report.as_ref()?;
        Some(self.selection.repository_root().join(&report.runs_dir))
    }

    fn report_snapshot(&self) -> BTreeSet<std::ffi::OsString> {
        self.report_root()
            .and_then(|root| fs::read_dir(root).ok())
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default()
    }

    /// Отчёт модуля, созданный этим прогоном: ровно один новый каталог.
    fn new_report(
        &self,
        before: &BTreeSet<std::ffi::OsString>,
        index: u32,
    ) -> Option<Result<serde_json::Value, String>> {
        let spec = self.selection.item.scenario.module_report.as_ref()?;
        let root = self.report_root()?;
        let after = self.report_snapshot();
        let fresh: Vec<_> = after.difference(before).collect();
        let dir = match fresh.as_slice() {
            [one] => root.join(one),
            [] => return Some(Err("прогон не создал нового каталога отчёта модуля".into())),
            _ => {
                return Some(Err(
                    "за прогон появилось несколько каталогов отчёта: отчёт этого прогона не определён (параллельный запуск модуля?)".into(),
                ));
            }
        };
        let text = match fs::read_to_string(dir.join(&spec.report)) {
            Ok(text) => text,
            Err(_) => return Some(Err("каталог прогона модуля не содержит отчёта".into())),
        };
        let _ = fs::write(
            self.dir
                .join("reports")
                .join(format!("run-{index:03}.json")),
            &text,
        );
        Some(
            serde_json::from_str(&text)
                .map_err(|e| format!("отчёт модуля не разбирается как JSON: {e}")),
        )
    }

    fn metric_keys(&self) -> Vec<String> {
        self.selection
            .item
            .scenario
            .metrics
            .iter()
            .map(MetricRequest::key)
            .collect()
    }

    fn mark_own_unavailable(&mut self, state: &mut State, reason: &str) {
        self.warn(format!("собственная группа прогонов недоступна: {reason}"));
        for request in &self.selection.item.scenario.metrics {
            if collector(self.selection, request) == Collector::OwnCgroup {
                state.missing.insert(
                    request.key(),
                    Missing {
                        unsupported: true,
                        reason: reason.to_owned(),
                    },
                );
            }
        }
    }

    fn record_run(
        &self,
        state: &mut State,
        index: usize,
        invocation: &Invocation,
        report: Option<Result<serde_json::Value, String>>,
    ) -> RunRecord {
        let scenario = &self.selection.item.scenario;
        let keys = self.metric_keys();
        let (mut status, exit_code, mut reason, mut values) = match &invocation.export {
            Some(Ok(imported)) => match imported.runs.first() {
                Some(run) => (
                    run.status,
                    run.exit_code,
                    run.reason.clone(),
                    run.values.clone(),
                ),
                None => (
                    RunStatus::Failure,
                    None,
                    Some("экспорт Hyperfine не содержит измерения".into()),
                    BTreeMap::new(),
                ),
            },
            Some(Err(problem)) => (
                RunStatus::Failure,
                None,
                Some(problem.clone()),
                BTreeMap::new(),
            ),
            None => {
                let (status, reason) = match invocation.finished.map(|f| f.ending) {
                    Some(Ending::TimedOut) => (
                        RunStatus::Timeout,
                        format!(
                            "прогон прерван через {} с{}",
                            invocation.timeout.as_secs(),
                            if invocation.budget_limited {
                                ": исчерпан бюджет серии"
                            } else {
                                ": превышен run_timeout_seconds"
                            }
                        ),
                    ),
                    Some(Ending::Interrupted) => (
                        RunStatus::Interrupted,
                        "прогон прерван сигналом исполнителю".to_owned(),
                    ),
                    _ => (RunStatus::Failure, invocation_failure(invocation)),
                };
                (status, None, Some(reason), BTreeMap::new())
            }
        };
        if self.own.is_some()
            && invocation.export.as_ref().is_none_or(|e| e.is_err())
            && invocation.finished.map(|f| f.ending) == Some(Ending::Exited(125))
        {
            status = RunStatus::Failure;
            reason = Some(
                "обёртка cgroup не поместила Hyperfine в подгруппу прогона (код 125): прогон не измерялся бы в своей группе".into(),
            );
        }
        let leftovers = invocation.finished.map_or(0, |f| f.leftovers);
        if status == RunStatus::Success && (leftovers > 0 || invocation.own_leftovers) {
            status = RunStatus::Failure;
            reason = Some(format!(
                "после выхода цели остались её процессы{}: они завершены исполнителем, их потребление прогон не учёл",
                if leftovers > 0 {
                    format!(" ({leftovers})")
                } else {
                    String::new()
                }
            ));
        }

        let report_values = match &report {
            Some(Ok(value)) => Some(value),
            _ => None,
        };
        let report_problem = match &report {
            Some(Err(problem)) => Some(problem.clone()),
            None => None,
            Some(Ok(_)) => None,
        };
        if let Some(value) = report_values {
            if let Err(problem) = self.record_report_inputs(state, value)
                && status == RunStatus::Success
            {
                status = RunStatus::Failure;
                reason = Some(problem);
            }
        } else if status == RunStatus::Success
            && let Some(input) = self.report_input()
        {
            // Идентичность и объём, накопленные прошлыми прогонами, не
            // доказывают нагрузку этого: подтвердить её может только его отчёт.
            status = RunStatus::Failure;
            reason = Some(format!(
                "идентичность или объём входа «{input}» не подтверждены отчётом этого прогона: {}",
                report_problem
                    .clone()
                    .unwrap_or_else(|| "отчёта нет".into())
            ));
        }
        if state.measurement.is_none()
            && let Some(Ok(imported)) = &invocation.export
        {
            state.measurement = Some(imported.measurement.clone());
        }

        for request in &scenario.metrics {
            let key = request.key();
            if let Some(missing) = state
                .missing
                .get(&key)
                .or(self.readiness.unavailable.get(&key))
            {
                values.insert(key, missing_value(missing));
                continue;
            }
            if values.contains_key(&key) {
                continue;
            }
            let value = match collector(self.selection, request) {
                Collector::Hyperfine => MetricValue::Failed {
                    reason: reason
                        .clone()
                        .unwrap_or_else(|| "Hyperfine не измерил прогон".into()),
                },
                Collector::HostProcfs => host_value(request, &invocation.samples),
                Collector::OwnCgroup => own_value(request, invocation),
                Collector::ContainerCgroup => container_value(request, &invocation.samples),
                Collector::ModuleReport => match (report_values, &report_problem) {
                    (Some(value), _) => report_counter(request, value),
                    (None, Some(problem)) => MetricValue::Unavailable {
                        reason: problem.clone(),
                    },
                    (None, None) => MetricValue::Unavailable {
                        reason: "отчёт модуля не получен".into(),
                    },
                },
                Collector::Filesystem => disk_value(request, self.selection.repository_root()),
                Collector::ModuleLog | Collector::ProcessProcfs => MetricValue::Unsupported {
                    reason: "источник не поддерживается исполнителем".into(),
                },
            };
            values.insert(key, value);
        }
        for key in keys {
            values.entry(key).or_insert_with(|| MetricValue::Failed {
                reason: "значение не получено".into(),
            });
        }
        if status == RunStatus::Success {
            reason = None;
        }
        state.reports.push(RunReport {
            index: index as u32,
            success: status == RunStatus::Success,
            report: report_values.cloned(),
        });
        RunRecord {
            index: index as u32,
            status,
            exit_code,
            reason,
            values,
        }
    }

    /// Вход, идентичность или объём которого декларация берёт из отчёта
    /// модуля: каждый прогон обязан подтвердить его собственным отчётом.
    fn report_input(&self) -> Option<&str> {
        self.selection
            .item
            .scenario
            .inputs
            .iter()
            .find(|input| from_report(input))
            .map(|input| input.id.as_str())
    }

    fn record_report_inputs(
        &self,
        state: &mut State,
        report: &serde_json::Value,
    ) -> Result<(), String> {
        for input in &self.selection.item.scenario.inputs {
            if !from_report(input) {
                continue;
            }
            let identity = match &input.report_identity {
                Some(text) => report_text(report, text)
                    .map_err(|e| format!("идентичность входа «{}»: {e}", input.id))?,
                None => match state.inputs.get(&input.id) {
                    Some(record) => record.identity.value.clone(),
                    None => continue,
                },
            };
            let mut size = BTreeMap::new();
            for metric in &input.size {
                if let Some(SizeMeasure::Report { pointer: text }) = &metric.measure {
                    let value = report_number(report, text).map_err(|e| {
                        format!("объём «{}» входа «{}»: {e}", metric.name, input.id)
                    })?;
                    size.insert(
                        metric.name.clone(),
                        Quantity {
                            value,
                            unit: metric.unit,
                        },
                    );
                }
            }
            let record = match state.inputs.get(&input.id) {
                Some(previous) => {
                    let mut merged = previous.clone();
                    if previous.identity.value != identity {
                        return Err(format!(
                            "идентичность входа «{}» изменилась между прогонами: серия измеряет разные нагрузки",
                            input.id
                        ));
                    }
                    for (name, quantity) in size {
                        match merged.size.get(&name) {
                            Some(old) if old.value != quantity.value => {
                                return Err(format!(
                                    "объём «{name}» входа «{}» изменился между прогонами",
                                    input.id
                                ));
                            }
                            _ => {
                                merged.size.insert(name, quantity);
                            }
                        }
                    }
                    merged
                }
                None => InputRecord {
                    id: input.id.clone(),
                    role: input.role,
                    identity: Identity {
                        method: input.identity,
                        value: identity,
                    },
                    size,
                },
            };
            if record.size.len()
                < input
                    .size
                    .iter()
                    .filter(|s| matches!(s.measure, Some(SizeMeasure::Report { .. })))
                    .count()
            {
                return Err(format!("объём входа «{}» неполон", input.id));
            }
            state.inputs.insert(input.id.clone(), record);
        }
        Ok(())
    }

    fn resolve_inputs(&mut self, state: &mut State) -> Result<(), String> {
        let scenario = &self.selection.item.scenario;
        for input in &scenario.inputs {
            if from_report(input) && input.kind == InputKind::DownloadedArtifact {
                continue;
            }
            let Some(path) = self.selection.input_paths.get(&input.id).cloned() else {
                if input.kind == InputKind::DownloadedArtifact {
                    continue;
                }
                return Err(format!("у входа «{}» нет пути", input.id));
            };
            self.log.line(&format!(
                "вход «{}»: идентичность ({}) · отпечаток пути {}",
                input.id,
                input.identity_name(),
                inputs::path_fingerprint(&path)
            ));
            let value = match (input.kind, input.identity) {
                (InputKind::RepoTree, _) => inputs::repo_tree_sha256(&path)?,
                (_, IdentityMethod::ContentTreeSha256) => inputs::content_tree_sha256(&path)
                    .map_err(|e| format!("вход «{}» не читается: {e}", input.id))?,
                (_, IdentityMethod::FileSha256) => {
                    inputs::file_sha256(&path).map_err(|e| format!("вход «{}»: {e}", input.id))?
                }
                (_, IdentityMethod::DeclaredVersion) => {
                    return Err(format!(
                        "вход «{}»: declared-version для входа с путём не определена",
                        input.id
                    ));
                }
            };
            let probe = match &input.probe {
                Some(step) => Some(self.probe(input, step)?),
                None => None,
            };
            let mut size = BTreeMap::new();
            let mut entries = None;
            for metric in &input.size {
                let value = match &metric.measure {
                    Some(SizeMeasure::FileBytes { path: relative }) => {
                        let file = inputs::inside(&path, relative)
                            .ok_or_else(|| format!("путь «{relative}» выходит за вход"))?;
                        fs::metadata(&file)
                            .map_err(|e| format!("объём «{}»: {relative}: {e}", metric.name))?
                            .len() as f64
                    }
                    Some(SizeMeasure::TreeBytes) => {
                        let entries = tree_entries(&mut entries, &path, input.kind)
                            .map_err(|e| format!("объём «{}»: {e}", metric.name))?;
                        inputs::tree_bytes(&path, entries)
                            .map_err(|e| format!("объём «{}»: {e}", metric.name))?
                            as f64
                    }
                    Some(SizeMeasure::TreeFiles) => {
                        let entries = tree_entries(&mut entries, &path, input.kind)
                            .map_err(|e| format!("объём «{}»: {e}", metric.name))?;
                        inputs::tree_files(&path, entries)
                            .map_err(|e| format!("объём «{}»: {e}", metric.name))?
                            as f64
                    }
                    Some(SizeMeasure::Probe { pointer: text }) => {
                        let probe = probe.as_ref().ok_or("проба входа не выполнена")?;
                        report_number(probe, text)
                            .map_err(|e| format!("объём «{}» из пробы: {e}", metric.name))?
                    }
                    // Дополняется прогонами из отчёта модуля.
                    Some(SizeMeasure::Report { .. }) => continue,
                    None => {
                        return Err(format!(
                            "объём «{}» входа «{}» не имеет measure: исполнитель не может его получить",
                            metric.name, input.id
                        ));
                    }
                };
                size.insert(
                    metric.name.clone(),
                    Quantity {
                        value,
                        unit: metric.unit,
                    },
                );
            }
            let record = InputRecord {
                id: input.id.clone(),
                role: input.role,
                identity: Identity {
                    method: input.identity,
                    value,
                },
                size,
            };
            state.inputs.insert(input.id.clone(), record);
        }
        Ok(())
    }

    fn probe(&mut self, input: &Input, step: &Step) -> Result<serde_json::Value, String> {
        let argv = self.argv(&step.program, &step.args)?;
        let env = self.selection.environment(&step.env)?;
        let out_path = self
            .dir
            .join("steps")
            .join(format!("probe-{}.json", input.id));
        let err_path = self
            .dir
            .join("steps")
            .join(format!("probe-{}.log", input.id));
        let mut command = command_for(&argv, &env, &self.selection.working_dir(&step.working_dir));
        command
            .stdout(fs::File::create(&out_path).map_err(|e| format!("вывод пробы: {e}"))?)
            .stderr(fs::File::create(&err_path).map_err(|e| format!("журнал пробы: {e}"))?);
        self.log
            .line(&format!("проба входа «{}»: {}", input.id, step.description));
        let finished = procs::run(
            &mut command,
            Bounds {
                timeout: Duration::from_secs(step.timeout_seconds),
                grace: inputs::PROBE_GRACE,
                interruptible: true,
            },
        )
        .map_err(|e| format!("проба входа «{}» не запускается: {e}", input.id))?;
        if finished.ending != Ending::Exited(0) {
            return Err(format!(
                "проба входа «{}»: {}",
                input.id,
                finished.ending.describe()
            ));
        }
        let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
        if size as usize > inputs::PROBE_OUTPUT_LIMIT {
            return Err(format!("вывод пробы входа «{}» слишком велик", input.id));
        }
        let text = fs::read_to_string(&out_path).map_err(|e| format!("вывод пробы: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("вывод пробы входа «{}» — не JSON: {e}", input.id))
    }

    fn base_environment(&self) -> BTreeMap<String, Fact> {
        let scenario = &self.selection.item.scenario;
        let mut facts = BTreeMap::new();
        for name in &scenario.environment.record {
            let fact = match BuiltinFact::ALL.iter().find(|b| b.as_str() == name) {
                Some(BuiltinFact::Os) => known(std::env::consts::OS),
                Some(BuiltinFact::Arch) => known(std::env::consts::ARCH),
                Some(BuiltinFact::KernelRelease) => environment::kernel_release(),
                Some(BuiltinFact::Virtualization) => environment::virtualization(),
                Some(BuiltinFact::CpuModel) => environment::cpu_model(),
                Some(BuiltinFact::LogicalCpus) => environment::logical_cpus(),
                Some(BuiltinFact::MemoryTotalBytes) => environment::memory_total_bytes(),
                Some(BuiltinFact::ModuleVersion) => match self.module_version() {
                    Some(version) => known(version),
                    None => unavailable(
                        "у модуля нет пакета в рабочей области Cargo: версия не определяется",
                    ),
                },
                Some(BuiltinFact::RustcVersion) => environment::rustc_version(),
                Some(BuiltinFact::DockerVersion) => {
                    match self.selection.environment(&scenario.target.env) {
                        Ok(env) => environment::docker_version(&env),
                        Err(reason) => unavailable(format!(
                            "окружение цели не разрешается, демон Docker не определён: {reason}"
                        )),
                    }
                }
                Some(BuiltinFact::HyperfineVersion) => match &self.readiness.hyperfine {
                    Some(h) => known(h.version.clone()),
                    None => unavailable("Hyperfine 2.x не обнаружен"),
                },
                None => continue,
            };
            facts.insert(name.clone(), fact);
        }
        facts
    }

    fn module_version(&self) -> Option<String> {
        programs::module_version(
            self.selection.repository_root(),
            &self.selection.item.module,
            &self.selection.item.scenario.target.program,
        )
    }

    fn finish(&mut self, mut state: State, cleanup: CleanupState) -> Result<Summary, String> {
        let result = self.assemble(&mut state, &cleanup);
        let text = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())? + "\n";
        let checked = result::check(&text, "result.json", Some(self.selection.catalog));
        let scenario = &self.selection.item.scenario;
        let successes = result
            .runs
            .iter()
            .filter(|r| r.status == RunStatus::Success)
            .count();
        let mut summary = Summary {
            ok: false,
            exit_code: 0,
            exit: Exit::Success,
            scenario: scenario.id.clone(),
            profile: self.selection.binding.id.clone(),
            outcome: Some(result.outcome.status),
            reason: result.outcome.reason.clone(),
            result: None,
            artifacts: Some(self.dir.clone()),
            runs: result.runs.len(),
            successful_runs: successes,
            cleanup,
            history: History::not_recorded(),
            warnings: self.warnings.clone(),
            diagnostics: checked.diagnostics.clone(),
        };
        if checked.result.is_none() {
            let rejected = self.dir.join("result.rejected.json");
            let _ = util::write_new_atomic(&rejected, text.as_bytes());
            let errors: Vec<String> = checked
                .diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(ToString::to_string)
                .collect();
            self.log.line(&format!(
                "результат отвергнут проверкой контракта и не принят: {}",
                errors.join("; ")
            ));
            summary.exit = Exit::Rejected;
            summary.exit_code = Exit::Rejected as i32;
            self.write_summary(&summary);
            return Ok(summary);
        }
        let path = self
            .request
            .output
            .clone()
            .unwrap_or_else(|| self.dir.join("result.json"));
        if let Err(error) = util::write_new_atomic(&path, text.as_bytes()) {
            // Прогоны уже выполнены и результат принят: не терять его вместе с
            // ошибкой записи итогового файла.
            let fallback = self.dir.join("result.unwritten.json");
            let saved = util::write_new_atomic(&fallback, text.as_bytes()).is_ok();
            let mut message = format!("результат не записан в {}: {error}", path.display());
            if saved {
                message.push_str(&format!(
                    "; принятый результат сохранён в {}",
                    fallback.display()
                ));
            }
            self.log.line(&message);
            summary.exit = Exit::Error;
            summary.exit_code = Exit::Error as i32;
            summary.warnings.push(message.clone());
            self.write_summary(&summary);
            return Err(message);
        }
        let interrupted = state.stop.as_ref().is_some_and(|s| s.interrupted);
        summary.exit = match result.outcome.status {
            _ if interrupted => Exit::Interrupted,
            OutcomeStatus::Success => Exit::Success,
            OutcomeStatus::Partial => Exit::Partial,
            OutcomeStatus::Failure => Exit::Failure,
            OutcomeStatus::Unsupported => Exit::Unsupported,
        };
        summary.exit_code = summary.exit as i32;
        summary.ok = summary.exit == Exit::Success;
        summary.result = Some(path.clone());
        summary.history = self.archive(&text, &path);
        if summary.history.status == HistoryStatus::Failed {
            // Измерение принято и записано; несохранённая история не делает его
            // недействительным, но и не выдаётся за успешное сохранение.
            summary.warnings.push(format!(
                "результат не сохранён в локальную историю: {}",
                summary.history.reason.as_deref().unwrap_or_default()
            ));
        }
        self.log.line(&format!(
            "итог {:?}: {}; результат {}",
            result.outcome.status,
            result
                .outcome
                .reason
                .as_deref()
                .unwrap_or("все прогоны успешны"),
            path.display()
        ));
        self.write_summary(&summary);
        Ok(summary)
    }

    /// Сохранить принятый результат в локальную историю тем же контрактом и
    /// каталогом, которым он только что проверен.
    fn archive(&self, text: &str, path: &Path) -> History {
        let root = match &self.request.history {
            Ok(root) => root,
            Err(HistoryOff::Disabled) => {
                return History {
                    status: HistoryStatus::Disabled,
                    id: None,
                    reason: None,
                };
            }
            Err(HistoryOff::Unavailable(reason)) => {
                self.log
                    .line(&format!("история: результат не сохранён — {reason}"));
                return History {
                    status: HistoryStatus::Failed,
                    id: None,
                    reason: Some(reason.clone()),
                };
            }
        };
        let imported = benchmark_history::store::Store::open_waiting(root, HISTORY_LOCK_WAIT)
            .and_then(|mut store| {
                store.import(
                    text,
                    &path.display().to_string(),
                    Some(self.selection.catalog),
                )
            });
        match imported {
            Ok(imported) => {
                self.log.line(&format!(
                    "история: {} {}",
                    if imported.created {
                        "сохранён"
                    } else {
                        "уже был"
                    },
                    imported.id
                ));
                History {
                    status: if imported.created {
                        HistoryStatus::Archived
                    } else {
                        HistoryStatus::AlreadyArchived
                    },
                    id: Some(imported.id),
                    reason: None,
                }
            }
            Err(error) => {
                let id = match &error {
                    benchmark_history::store::Error::Index { id, .. } => Some(id.clone()),
                    _ => None,
                };
                let reason = error.to_string();
                self.log
                    .line(&format!("история: результат не сохранён — {reason}"));
                History {
                    status: HistoryStatus::Failed,
                    id,
                    reason: Some(reason),
                }
            }
        }
    }

    fn write_summary(&self, summary: &Summary) {
        if let Ok(text) = serde_json::to_string_pretty(summary) {
            let _ = fs::write(self.dir.join("summary.json"), text + "\n");
        }
    }

    fn assemble(&self, state: &mut State, cleanup: &CleanupState) -> BenchmarkResult {
        let selection = self.selection;
        let scenario = &selection.item.scenario;
        let mut environment = state.environment.clone();
        for fact in &scenario.environment.facts {
            if !scenario.environment.record.contains(&fact.name) {
                continue;
            }
            environment.insert(fact.name.clone(), self.report_fact(state, fact));
        }
        let module_version = match environment.get("module_version") {
            Some(Fact::Known { value }) => value.clone(),
            _ => self
                .module_version()
                .unwrap_or_else(|| "не определена".to_owned()),
        };
        let CodeSnapshot {
            git_commit,
            dirty,
            source_tree_sha256,
        } = state.code.clone();

        let runs = std::mem::take(&mut state.runs);
        let successes = runs
            .iter()
            .filter(|r| r.status == RunStatus::Success)
            .count();
        let metrics: Vec<MetricRecord> = scenario
            .metrics
            .iter()
            .map(|request| metric_record(request, &runs))
            .collect();
        let required_incomplete: Vec<&str> = scenario
            .metrics
            .iter()
            .zip(&metrics)
            .filter(|(request, record)| {
                request.required && record.availability != Availability::Measured
            })
            .map(|(_, record)| record.key.as_str())
            .collect();
        let outcome = match (&state.outcome, &state.stop) {
            (Some((status, reason)), _) => Outcome {
                status: *status,
                reason: Some(reason.clone()),
            },
            (None, stop) if successes == 0 => Outcome {
                status: OutcomeStatus::Failure,
                reason: Some(
                    stop.as_ref()
                        .map(|s| s.reason.clone())
                        .unwrap_or_else(|| "ни одного успешного прогона".into()),
                ),
            },
            (None, Some(stop)) => Outcome {
                status: OutcomeStatus::Partial,
                reason: Some(format!(
                    "серия остановлена после {successes} успешных прогонов: {}",
                    stop.reason
                )),
            },
            (None, None) if !required_incomplete.is_empty() => Outcome {
                status: OutcomeStatus::Partial,
                reason: Some(format!(
                    "обязательные метрики измерены не во всех прогонах: {}",
                    required_incomplete.join(", ")
                )),
            },
            (None, None) if (runs.len() as u32) < scenario.runs.min_runs => Outcome {
                status: OutcomeStatus::Partial,
                reason: Some(format!(
                    "выполнено {} прогонов при min_runs = {}: наблюдение валидно, но меньше требуемого для success",
                    runs.len(),
                    scenario.runs.min_runs
                )),
            },
            (None, None) => Outcome {
                status: OutcomeStatus::Success,
                reason: None,
            },
        };
        let outcome = match (&cleanup.reason, outcome.status) {
            (Some(problem), OutcomeStatus::Success) => Outcome {
                status: OutcomeStatus::Partial,
                reason: Some(format!(
                    "все прогоны успешны, но уборка сценария не завершена: {problem}"
                )),
            },
            (Some(problem), _) => Outcome {
                status: outcome.status,
                reason: Some(format!(
                    "{}; уборка не завершена: {problem}",
                    outcome.reason.unwrap_or_default()
                )),
            },
            (None, _) => outcome,
        };
        let outcome = match (&state.drift, outcome.status) {
            (Some(drift), OutcomeStatus::Success) => Outcome {
                status: OutcomeStatus::Partial,
                reason: Some(format!("все прогоны успешны, но {drift}")),
            },
            (Some(drift), _) => Outcome {
                status: outcome.status,
                reason: Some(format!("{}; {drift}", outcome.reason.unwrap_or_default())),
            },
            (None, _) => outcome,
        };

        let parameters = selection
            .values
            .iter()
            .filter(|(name, _)| {
                scenario
                    .parameters
                    .iter()
                    .any(|p| &p.name == *name && p.record)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let inputs_records = scenario
            .inputs
            .iter()
            .filter_map(|input| state.inputs.get(&input.id))
            .filter(|record| {
                // Вход без полного объёма не записывается: запись без объёма нагрузки
                // противоречит декларации.
                let declared = scenario
                    .inputs
                    .iter()
                    .find(|i| i.id == record.id)
                    .map_or(0, |i| i.size.len());
                record.size.len() == declared
            })
            .cloned()
            .collect();
        let measurement = state.measurement.clone().unwrap_or_else(|| Tool {
            name: "hyperfine".to_owned(),
            version: self
                .readiness
                .hyperfine
                .as_ref()
                .map(|h| h.version.clone())
                .unwrap_or_else(|| "не обнаружен".to_owned()),
            format: Some(import::FORMAT.to_owned()),
        });
        let observation = match runs.len() {
            0 => Observation::None,
            1 => Observation::Single,
            _ => Observation::Repeated,
        };
        let mut result = BenchmarkResult {
            schema_version: RESULT_SCHEMA_VERSION,
            scenario: ScenarioRef {
                id: scenario.id.clone(),
                module: selection.item.module.clone(),
                revision: scenario.revision,
                kind: scenario.kind,
            },
            profile: ProfileRef {
                id: selection.binding.id.clone(),
                cache: selection.profile.cache,
                network: selection.profile.network,
            },
            parameters,
            inputs: inputs_records,
            provenance: Provenance {
                recorded_at: util::utc_rfc3339(util::unix_now()),
                measurement,
                runner: Some(Tool {
                    name: env!("CARGO_PKG_NAME").to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    format: None,
                }),
                code: CodeState {
                    module_version,
                    source_tree_sha256,
                    git_commit,
                    dirty,
                },
            },
            environment,
            outcome,
            observation,
            warmup_runs: state.warmups,
            metrics,
            runs,
            aggregates: BTreeMap::new(),
        };
        sanitize(&mut result);
        result.recompute_aggregates();
        result
    }

    fn report_fact(&self, state: &State, fact: &benchmark_contract::scenario::CustomFact) -> Fact {
        series_fact(&fact.report, &state.reports)
    }
}

/// Отчёт модуля одного измеряемого прогона и итог этого прогона.
#[derive(Debug, Clone)]
struct RunReport {
    index: u32,
    success: bool,
    report: Option<serde_json::Value>,
}

/// Факт серии из отчётов модуля. Факт описывает всю серию, поэтому его
/// подтверждает только согласие отчётов *каждого* успешного прогона: одно
/// найденное значение не доказывает факт для прогона без отчёта или без поля.
/// Неуспешные прогоны в измерения не входят и факт не подтверждают.
fn series_fact(pointers: &[String], runs: &[RunReport]) -> Fact {
    if pointers.is_empty() {
        return unavailable("модуль не объявил источник факта");
    }
    let mut agreed: Option<String> = None;
    for run in runs.iter().filter(|r| r.success) {
        let number = run.index + 1;
        let Some(report) = &run.report else {
            return unavailable(format!(
                "успешный прогон {number} не оставил отчёта модуля: факт не подтверждён для всей серии"
            ));
        };
        match fact_value(pointers, report) {
            Err(reason) => {
                return unavailable(format!(
                    "в отчёте модуля прогона {number} нет значения: {reason}"
                ));
            }
            Ok(value) => match &agreed {
                Some(first) if first != &value => {
                    return unavailable("значение различается между прогонами серии");
                }
                Some(_) => {}
                None => agreed = Some(value),
            },
        }
    }
    match agreed {
        Some(value) => known(value),
        None => unavailable("ни один успешный прогон не подтвердил факт отчётом модуля"),
    }
}

/// Значение факта в одном отчёте: одно — как есть, несколько — `<имя>=<значение>`.
fn fact_value(pointers: &[String], report: &serde_json::Value) -> Result<String, String> {
    if let [single] = pointers {
        return report_text(report, single);
    }
    pointers
        .iter()
        .map(|text| {
            let name = text.rsplit('/').next().unwrap_or(text);
            report_text(report, text).map(|v| format!("{name}={v}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join(","))
}

#[derive(Default)]
struct State {
    environment: BTreeMap<String, Fact>,
    inputs: BTreeMap<String, InputRecord>,
    runs: Vec<RunRecord>,
    warmups: u32,
    stop: Option<Stop>,
    outcome: Option<(OutcomeStatus, String)>,
    missing: BTreeMap<String, Missing>,
    reports: Vec<RunReport>,
    measurement: Option<Tool>,
    code: CodeSnapshot,
    drift: Option<String>,
}

/// Происхождение измеряемого кода в одной точке времени.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodeSnapshot {
    git_commit: Option<String>,
    dirty: Option<bool>,
    source_tree_sha256: Option<String>,
}

impl CodeSnapshot {
    fn take(repo: &Path) -> CodeSnapshot {
        let (git_commit, dirty) = inputs::git_state(repo);
        let source_tree_sha256 = git_commit
            .as_ref()
            .and_then(|_| inputs::repo_tree_sha256(repo).ok());
        CodeSnapshot {
            git_commit,
            dirty,
            source_tree_sha256,
        }
    }

    /// Что различается между снимками; `None` — состояние то же.
    fn difference(&self, other: &CodeSnapshot) -> Option<String> {
        let mut changed = Vec::new();
        if self.git_commit != other.git_commit {
            changed.push("коммит");
        }
        if self.dirty != other.dirty {
            changed.push("признак изменений");
        }
        if self.source_tree_sha256 != other.source_tree_sha256 {
            changed.push("дерево исходников");
        }
        (!changed.is_empty()).then(|| changed.join(", "))
    }
}

/// Каталог блокировок модулей рабочей копии.
fn lock_dir(repo: &Path) -> PathBuf {
    repo.join("target/benchmark-runner/locks")
}

trait IdentityName {
    fn identity_name(&self) -> &'static str;
}

impl IdentityName for Input {
    fn identity_name(&self) -> &'static str {
        match self.identity {
            IdentityMethod::ContentTreeSha256 => "content-tree-sha256",
            IdentityMethod::FileSha256 => "file-sha256",
            IdentityMethod::DeclaredVersion => "declared-version",
        }
    }
}

/// Перечень записей входа для объёма, вычисленный один раз на вход.
fn tree_entries<'a>(
    cache: &'a mut Option<Vec<PathBuf>>,
    path: &Path,
    kind: InputKind,
) -> Result<&'a [PathBuf], String> {
    if cache.is_none() {
        *cache = Some(inputs::size_entries(path, kind == InputKind::RepoTree)?);
    }
    Ok(cache.as_deref().unwrap_or_default())
}

fn from_report(input: &Input) -> bool {
    input.report_identity.is_some()
        || input
            .size
            .iter()
            .any(|s| matches!(s.measure, Some(SizeMeasure::Report { .. })))
}

fn interrupted_stop() -> Stop {
    Stop {
        reason: "серия прервана сигналом".into(),
        interrupted: true,
    }
}

fn status_of(stop: &Stop) -> RunStatus {
    if stop.interrupted {
        RunStatus::Interrupted
    } else {
        RunStatus::Failure
    }
}

fn failed_run(index: usize, status: RunStatus, reason: String, keys: Vec<String>) -> RunRecord {
    RunRecord {
        index: index as u32,
        status,
        exit_code: None,
        values: keys
            .into_iter()
            .map(|k| {
                (
                    k,
                    MetricValue::Failed {
                        reason: "цель не запускалась".into(),
                    },
                )
            })
            .collect(),
        reason: Some(reason),
    }
}

fn invocation_failure(invocation: &Invocation) -> String {
    if let Some(error) = &invocation.spawn_error {
        return error.clone();
    }
    match (invocation.finished.map(|f| f.ending), &invocation.export) {
        (_, Some(Err(problem))) => problem.clone(),
        (Some(Ending::Exited(code)), _) => format!(
            "Hyperfine завершился с кодом {code} без экспорта; журнал — в каталоге артефактов"
        ),
        (Some(ending), _) => format!("Hyperfine: {}", ending.describe()),
        (None, _) => "Hyperfine не запускался".into(),
    }
}

fn read_export(
    path: &Path,
    scenario: &benchmark_contract::scenario::Scenario,
) -> Result<import::Imported, String> {
    let text = fs::read_to_string(path).map_err(|_| {
        "Hyperfine не записал экспорт: команда цели не запустилась или Hyperfine завершился ошибкой; журнал — в каталоге артефактов".to_owned()
    })?;
    let export = import::parse(&text).map_err(|e| e.to_string())?;
    import::import(
        &export,
        0,
        &scenario.metrics,
        &scenario.target.expect_exit_codes,
    )
    .map_err(|e| e.to_string())
}

fn command_for(argv: &[String], env: &[(String, String)], dir: &Path) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(dir)
        .stdin(Stdio::null());
    command
}

fn missing_value(missing: &Missing) -> MetricValue {
    if missing.unsupported {
        MetricValue::Unsupported {
            reason: missing.reason.clone(),
        }
    } else {
        MetricValue::Unavailable {
            reason: missing.reason.clone(),
        }
    }
}

fn clock_ticks() -> f64 {
    rustix::param::clock_ticks_per_second() as f64
}

fn host_value(request: &MetricRequest, samples: &Samples) -> MetricValue {
    let host = &samples.host;
    let (Some(first), Some(last)) = (host.first(), host.last()) else {
        return MetricValue::Unavailable {
            reason: "снимков /proc нет".into(),
        };
    };
    let span = last.at - first.at;
    match request.metric {
        MetricId::CpuUtilizationMean if span > 0.0 => MetricValue::Measured {
            value: (last.busy_ticks.saturating_sub(first.busy_ticks)) as f64 / clock_ticks() / span,
        },
        MetricId::CpuUtilizationPeak => {
            let interval = request.sample_interval_seconds.unwrap_or(1.0);
            let points: Vec<(f64, f64)> = host
                .iter()
                .map(|s| (s.at, s.busy_ticks as f64 / clock_ticks()))
                .collect();
            match sampler::peak_rate(&points, interval) {
                Some(value) => MetricValue::Measured { value },
                None => MetricValue::Unavailable {
                    reason: format!(
                        "прогон короче интервала сэмплирования {interval} с: ни одного полного окна"
                    ),
                },
            }
        }
        MetricId::MemoryHostPeakIncrease => match first.mem_available {
            Some(base) if host.len() > 1 => MetricValue::Measured {
                value: host
                    .iter()
                    .filter_map(|s| s.mem_available)
                    .map(|v| base.saturating_sub(v))
                    .max()
                    .unwrap_or(0) as f64,
            },
            _ => MetricValue::Unavailable {
                reason: "MemAvailable не наблюдался дважды за прогон".into(),
            },
        },
        _ => MetricValue::Unavailable {
            reason: "недостаточно снимков /proc за прогон".into(),
        },
    }
}

fn group_points(samples: &Samples) -> Vec<(f64, f64)> {
    samples
        .group
        .iter()
        .filter_map(|s| s.counters.usage_usec.map(|u| (s.at, u as f64 / 1e6)))
        .collect()
}

fn io_value(metric: MetricId, io: Option<cgroup::IoCounters>) -> MetricValue {
    let Some(io) = io else {
        return MetricValue::Unavailable {
            reason:
                "io.stat отсутствует, недоступен или некорректен: учёт блочного I/O не подтверждён"
                    .into(),
        };
    };
    let value = match metric {
        MetricId::IoReadBytes => io.read_bytes,
        MetricId::IoWriteBytes => io.write_bytes,
        MetricId::IoReadOperations => io.read_operations,
        MetricId::IoWriteOperations => io.write_operations,
        _ => unreachable!("проверено is_io"),
    };
    MetricValue::Measured {
        value: value as f64,
    }
}

fn own_value(request: &MetricRequest, invocation: &Invocation) -> MetricValue {
    let Some(last) = invocation.own_final else {
        return MetricValue::Unavailable {
            reason: "счётчики подгруппы прогона не прочитаны".into(),
        };
    };
    if request.metric.is_io() {
        return io_value(request.metric, last.io);
    }
    match request.metric {
        MetricId::MemoryPeakCgroup => match last.memory_peak {
            Some(value) => MetricValue::Measured {
                value: value as f64,
            },
            None => MetricValue::Unsupported {
                reason: "memory.peak отсутствует: ядро Linux старше 5.19".into(),
            },
        },
        MetricId::CpuTimeCgroup => match last.usage_usec {
            Some(usec) => MetricValue::Measured {
                value: usec as f64 / 1e6,
            },
            None => MetricValue::Unavailable {
                reason: "cpu.stat без usage_usec".into(),
            },
        },
        MetricId::CpuUtilizationMean => match (last.usage_usec, invocation.own_span) {
            (Some(usec), Some(span)) if span > 0.0 => MetricValue::Measured {
                value: usec as f64 / 1e6 / span,
            },
            _ => MetricValue::Unavailable {
                reason: "нет процессорного времени группы или времени её жизни".into(),
            },
        },
        MetricId::CpuUtilizationPeak => {
            let interval = request.sample_interval_seconds.unwrap_or(1.0);
            match sampler::peak_rate(&group_points(&invocation.samples), interval) {
                Some(value) => MetricValue::Measured { value },
                None => MetricValue::Unavailable {
                    reason: format!(
                        "прогон короче интервала сэмплирования {interval} с: ни одного полного окна"
                    ),
                },
            }
        }
        _ => MetricValue::Unsupported {
            reason: "метрика не снимается с cgroup".into(),
        },
    }
}

fn container_value(request: &MetricRequest, samples: &Samples) -> MetricValue {
    if samples.container.is_none() {
        return MetricValue::Unavailable {
            reason: samples
                .group_problem
                .clone()
                .unwrap_or_else(|| "контейнер нагрузки этого прогона не обнаружен".into()),
        };
    }
    let Some(last) = samples.group.last() else {
        return MetricValue::Unavailable {
            reason: "контейнер обнаружен, но его группа не прочитана до удаления".into(),
        };
    };
    if request.metric.is_io() {
        // Последний снимок исчезнувшей группы не является итогом её I/O.
        if samples.group_vanished {
            return MetricValue::Unavailable {
                reason: "группа контейнера исчезла: итоговый io.stat не прочитан".into(),
            };
        }
        return io_value(request.metric, last.counters.io);
    }
    match request.metric {
        MetricId::MemoryPeakCgroup => match last.counters.memory_peak {
            Some(value) => MetricValue::Measured {
                value: value as f64,
            },
            None => MetricValue::Unsupported {
                reason: "memory.peak отсутствует: ядро Linux старше 5.19".into(),
            },
        },
        MetricId::CpuTimeCgroup => match last.counters.usage_usec {
            Some(usec) => MetricValue::Measured {
                value: usec as f64 / 1e6,
            },
            None => MetricValue::Unavailable {
                reason: "cpu.stat без usage_usec".into(),
            },
        },
        MetricId::CpuUtilizationMean => {
            let points = group_points(samples);
            match (points.first(), points.last()) {
                (Some(a), Some(b)) if b.0 > a.0 => MetricValue::Measured {
                    value: (b.1 - a.1).max(0.0) / (b.0 - a.0),
                },
                _ => MetricValue::Unavailable {
                    reason: "группа контейнера наблюдалась слишком коротко".into(),
                },
            }
        }
        MetricId::CpuUtilizationPeak => {
            let interval = request.sample_interval_seconds.unwrap_or(1.0);
            match sampler::peak_rate(&group_points(samples), interval) {
                Some(value) => MetricValue::Measured { value },
                None => MetricValue::Unavailable {
                    reason: format!(
                        "контейнер наблюдался меньше интервала сэмплирования {interval} с"
                    ),
                },
            }
        }
        _ => MetricValue::Unsupported {
            reason: "метрика не снимается с cgroup".into(),
        },
    }
}

fn report_counter(request: &MetricRequest, report: &serde_json::Value) -> MetricValue {
    let Some(text) = &request.pointer else {
        return MetricValue::Unsupported {
            reason: "декларация не указала pointer".into(),
        };
    };
    match report_number(report, text) {
        Ok(value) if value.fract() == 0.0 => MetricValue::Measured { value },
        Ok(_) => MetricValue::Failed {
            reason: "счётчик отчёта модуля — не целое число".into(),
        },
        Err(reason) => MetricValue::Unavailable {
            reason: format!("в отчёте модуля нет значения: {reason}"),
        },
    }
}

fn disk_value(request: &MetricRequest, repo: &Path) -> MetricValue {
    let Some(relative) = &request.path else {
        return MetricValue::Unsupported {
            reason: "декларация не указала path".into(),
        };
    };
    let Some(path) = inputs::inside(repo, relative) else {
        return MetricValue::Unsupported {
            reason: "путь выходит за корень репозитория".into(),
        };
    };
    if fs::symlink_metadata(&path).is_err() {
        return MetricValue::Unavailable {
            reason: format!("ресурс {relative} отсутствует после прогона"),
        };
    }
    match inputs::apparent_bytes(&path) {
        Ok(bytes) => MetricValue::Measured {
            value: bytes as f64,
        },
        Err(error) => MetricValue::Failed {
            reason: format!("размер {relative} не прочитан: {}", error.kind()),
        },
    }
}

fn report_values<'a>(
    report: &'a serde_json::Value,
    text: &str,
) -> Result<Vec<&'a serde_json::Value>, String> {
    let segments = pointer::parse(text)?;
    pointer::select(report, &segments).ok_or_else(|| format!("путь {text} отсутствует"))
}

/// Число по указателю; несколько значений (`*`) суммируются.
fn report_number(report: &serde_json::Value, text: &str) -> Result<f64, String> {
    let values = report_values(report, text)?;
    if values.is_empty() {
        return Err(format!("{text} не выбрал ни одного значения"));
    }
    let mut total = 0.0;
    for value in values {
        let number = value
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| format!("{text}: ожидается неотрицательное число"))?;
        total += number;
    }
    Ok(total)
}

fn report_text(report: &serde_json::Value, text: &str) -> Result<String, String> {
    let values = report_values(report, text)?;
    match values.as_slice() {
        [serde_json::Value::String(s)] if !s.trim().is_empty() => Ok(s.clone()),
        [serde_json::Value::Number(n)] => Ok(n.to_string()),
        [serde_json::Value::Bool(b)] => Ok(b.to_string()),
        _ => Err(format!(
            "{text}: ожидается одно строковое или числовое значение"
        )),
    }
}

fn metric_record(request: &MetricRequest, runs: &[RunRecord]) -> MetricRecord {
    let key = request.key();
    let values: Vec<&MetricValue> = runs.iter().filter_map(|r| r.values.get(&key)).collect();
    let measured = values.iter().filter(|v| v.measured().is_some()).count();
    let first_reason = values.iter().find_map(|v| match v {
        MetricValue::Measured { .. } => None,
        MetricValue::Unavailable { reason }
        | MetricValue::Unsupported { reason }
        | MetricValue::Failed { reason } => Some(reason.clone()),
    });
    let uniform =
        |wanted: fn(&MetricValue) -> bool| !values.is_empty() && values.iter().all(|v| wanted(v));
    let (availability, reason) = if runs.is_empty() {
        (Availability::Unavailable, Some("прогонов нет".to_owned()))
    } else if measured == runs.len() {
        (Availability::Measured, None)
    } else if measured > 0 {
        (
            Availability::Partial,
            Some(format!(
                "измерена в {measured} из {} прогонов: {}",
                runs.len(),
                first_reason.clone().unwrap_or_default()
            )),
        )
    } else if uniform(|v| matches!(v, MetricValue::Unsupported { .. })) {
        (Availability::Unsupported, first_reason)
    } else if uniform(|v| matches!(v, MetricValue::Failed { .. })) {
        (Availability::Failed, first_reason)
    } else {
        (Availability::Unavailable, first_reason)
    };
    MetricRecord {
        key,
        metric: request.metric,
        scope: request.scope,
        source: request.source,
        label: request.label.clone(),
        unit: request.metric.base_unit(),
        availability,
        reason: reason
            .or_else(|| (availability != Availability::Measured).then(|| "значения нет".into())),
    }
}

/// Последняя защита публикуемого результата: причины и факты, которые
/// похожи на пути пользователя или секреты, заменяются нейтральной записью.
/// Исходный текст остаётся только в журнале каталога артефактов.
fn sanitize(result: &mut BenchmarkResult) {
    const HIDDEN: &str = "подробности скрыты: текст содержал путь или учётные данные; см. журнал прогона в каталоге артефактов";
    let clean = |text: &mut String| {
        if sensitive::finding(text).is_some() {
            *text = HIDDEN.to_owned();
        }
    };
    if let Some(reason) = result.outcome.reason.as_mut() {
        clean(reason);
    }
    for metric in &mut result.metrics {
        if let Some(reason) = metric.reason.as_mut() {
            clean(reason);
        }
    }
    for run in &mut result.runs {
        if let Some(reason) = run.reason.as_mut() {
            clean(reason);
        }
        for value in run.values.values_mut() {
            match value {
                MetricValue::Unavailable { reason }
                | MetricValue::Unsupported { reason }
                | MetricValue::Failed { reason } => clean(reason),
                MetricValue::Measured { .. } => {}
            }
        }
    }
    for fact in result.environment.values_mut() {
        match fact {
            Fact::Known { value } if sensitive::finding(value).is_some() => {
                *fact = unavailable("значение не публикуется: содержит путь или учётные данные");
            }
            Fact::Unavailable { reason } => clean(reason),
            Fact::Known { .. } => {}
        }
    }
}

fn samples_json(samples: &Samples) -> String {
    let host: Vec<_> = samples
        .host
        .iter()
        .map(|s| serde_json::json!({"t": s.at, "busy_ticks": s.busy_ticks, "mem_available": s.mem_available}))
        .collect();
    let group: Vec<_> = samples
        .group
        .iter()
        .map(|s| serde_json::json!({"t": s.at, "usage_usec": s.counters.usage_usec, "memory_peak": s.counters.memory_peak, "io": s.counters.io}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "clock_ticks_per_second": clock_ticks(),
        "host": host,
        "group": group,
        "container": samples.container,
        "group_vanished": samples.group_vanished,
        "group_problem": samples.group_problem,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchmark_contract::vocabulary::Scope;
    use serde_json::json;

    fn run(index: u32, success: bool, report: Option<serde_json::Value>) -> RunReport {
        RunReport {
            index,
            success,
            report,
        }
    }

    fn pointers(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn reason(fact: Fact) -> String {
        match fact {
            Fact::Unavailable { reason } => reason,
            Fact::Known { value } => panic!("ожидался unavailable, получен known {value}"),
        }
    }

    #[test]
    fn io_values_have_exact_units_and_missing_final_is_not_zero() {
        let io = cgroup::IoCounters {
            read_bytes: 1024,
            write_bytes: 2048,
            read_operations: 3,
            write_operations: 4,
        };
        for (metric, value) in [
            (MetricId::IoReadBytes, 1024.0),
            (MetricId::IoWriteBytes, 2048.0),
            (MetricId::IoReadOperations, 3.0),
            (MetricId::IoWriteOperations, 4.0),
        ] {
            assert_eq!(io_value(metric, Some(io)).measured(), Some(value));
            assert!(matches!(
                io_value(metric, None),
                MetricValue::Unavailable { .. }
            ));
        }
        let mut samples = Samples {
            container: Some("fixture".into()),
            group_vanished: true,
            ..Samples::default()
        };
        samples.group.push(crate::sampler::GroupSample {
            at: 1.0,
            counters: Counters {
                usage_usec: Some(1),
                memory_peak: Some(1),
                io: Some(io),
            },
        });
        // Запрос берём из маленькой декларации, чтобы не дублировать его поля.
        let request: MetricRequest = toml::from_str("metric = \"io_read_bytes\"\nscope = \"cgroup\"\nsource = \"cgroup-v2\"\nrequired = false").unwrap();
        assert!(matches!(
            container_value(&request, &samples),
            MetricValue::Unavailable { .. }
        ));
        samples.group_vanished = false;
        assert_eq!(container_value(&request, &samples).measured(), Some(1024.0));
    }

    #[test]
    fn identical_complete_reports_confirm_fact() {
        let report = json!({"tool": {"version": "1.2.3"}});
        let runs = [
            run(0, true, Some(report.clone())),
            run(1, true, Some(report)),
        ];
        assert_eq!(
            series_fact(&pointers(&["/tool/version"]), &runs),
            known("1.2.3")
        );
    }

    #[test]
    fn fact_missing_in_later_report_is_not_confirmed() {
        let runs = [
            run(0, true, Some(json!({"tool": {"version": "1.2.3"}}))),
            run(1, true, Some(json!({"tool": {}}))),
        ];
        let reason = reason(series_fact(&pointers(&["/tool/version"]), &runs));
        assert!(reason.contains("/tool/version"), "{reason}");
    }

    #[test]
    fn fact_missing_in_earlier_report_is_not_confirmed() {
        let runs = [
            run(0, true, Some(json!({"tool": {}}))),
            run(1, true, Some(json!({"tool": {"version": "1.2.3"}}))),
        ];
        let reason = reason(series_fact(&pointers(&["/tool/version"]), &runs));
        assert!(reason.contains("прогона 1"), "{reason}");
    }

    #[test]
    fn differing_values_are_not_confirmed() {
        let runs = [
            run(0, true, Some(json!({"tool": {"version": "1.2.3"}}))),
            run(1, true, Some(json!({"tool": {"version": "1.2.4"}}))),
        ];
        let reason = reason(series_fact(&pointers(&["/tool/version"]), &runs));
        assert!(reason.contains("различается"), "{reason}");
    }

    #[test]
    fn several_pointers_require_every_part_in_every_report() {
        let both = pointers(&["/tool/name", "/tool/version"]);
        let full = json!({"tool": {"name": "x", "version": "1"}});
        let runs = [
            run(0, true, Some(full.clone())),
            run(1, true, Some(full.clone())),
        ];
        assert_eq!(series_fact(&both, &runs), known("name=x,version=1"));
        let runs = [
            run(0, true, Some(full)),
            run(1, true, Some(json!({"tool": {"name": "x"}}))),
        ];
        let reason = reason(series_fact(&both, &runs));
        assert!(reason.contains("/tool/version"), "{reason}");
    }

    #[test]
    fn successful_run_without_report_leaves_fact_unconfirmed() {
        let runs = [
            run(0, true, Some(json!({"tool": {"version": "1.2.3"}}))),
            run(1, true, None),
        ];
        let reason = reason(series_fact(&pointers(&["/tool/version"]), &runs));
        assert!(reason.contains("прогон 2"), "{reason}");
    }

    #[test]
    fn failed_runs_neither_confirm_nor_refute_fact() {
        let report = json!({"tool": {"version": "1.2.3"}});
        // Неуспешный прогон не входит в измерения серии: его отчёт не
        // подтверждает факт и его отсутствие факт не опровергает.
        let runs = [
            run(0, true, Some(report.clone())),
            run(1, false, None),
            run(2, false, Some(json!({"tool": {"version": "9"}}))),
        ];
        assert_eq!(
            series_fact(&pointers(&["/tool/version"]), &runs),
            known("1.2.3")
        );
        // Отчёт только неуспешного прогона не подтверждает факт серии.
        let runs = [run(0, false, Some(report))];
        reason(series_fact(&pointers(&["/tool/version"]), &runs));
        reason(series_fact(&pointers(&["/tool/version"]), &[]));
    }

    #[test]
    fn missing_fact_blocks_comparability() {
        use benchmark_contract::comparability;
        use benchmark_contract::scenario::Scenario;
        let runs = [
            run(0, true, Some(json!({"tool": {"version": "1.2.3"}}))),
            run(1, true, None),
        ];
        let fact = series_fact(&pointers(&["/tool/version"]), &runs);
        let declaration = r#"
id = "fixture.fact"
title = "Факт"
revision = 1
kind = "short-cli"
status = "executable"
description = "Факт из отчёта."
measured = "Запуск."
platforms = ["linux"]

[target]
program = { kind = "executable", name = "true" }
expect_exit_codes = [0]

[runs]
warmup = 0
min_runs = 2
max_runs = 2
single_run_allowed = false
run_timeout_seconds = 10
total_budget_seconds = 60

[[profile]]
id = "warm"
cache_definition = "Кэшей нет."
cleared = []
retained = []

[[metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[environment]
record = ["os", "arch", "module_version", "tool_version"]
comparable = ["os", "arch", "tool_version"]

[[environment.fact]]
name = "tool_version"
description = "Версия из отчёта."
report = ["/tool/version"]
"#;
        let scenario: Scenario = toml::from_str(declaration).expect("декларация разбирается");
        let result = |fact: Fact| -> BenchmarkResult {
            let mut environment = BTreeMap::new();
            environment.insert("os".to_owned(), known("linux"));
            environment.insert("arch".to_owned(), known("x86_64"));
            environment.insert("module_version".to_owned(), known("0.1.0"));
            environment.insert("tool_version".to_owned(), fact);
            let key = "time_wall_clock@command".to_owned();
            BenchmarkResult {
                schema_version: RESULT_SCHEMA_VERSION,
                scenario: ScenarioRef {
                    id: "fixture.fact".into(),
                    module: "fixture".into(),
                    revision: 1,
                    kind: WorkloadKind::ShortCli,
                },
                profile: ProfileRef {
                    id: "warm".into(),
                    cache: benchmark_contract::vocabulary::CacheState::Warm,
                    network: benchmark_contract::vocabulary::Network::Online,
                },
                parameters: BTreeMap::new(),
                inputs: Vec::new(),
                provenance: Provenance {
                    recorded_at: "2026-10-28T00:00:00Z".into(),
                    measurement: Tool {
                        name: "hyperfine".into(),
                        version: "2.0.0".into(),
                        format: None,
                    },
                    runner: None,
                    code: CodeState {
                        module_version: "0.1.0".into(),
                        source_tree_sha256: None,
                        git_commit: None,
                        dirty: None,
                    },
                },
                environment,
                outcome: Outcome {
                    status: OutcomeStatus::Success,
                    reason: None,
                },
                observation: Observation::Repeated,
                warmup_runs: 0,
                metrics: vec![MetricRecord {
                    key: key.clone(),
                    metric: MetricId::TimeWallClock,
                    scope: Scope::Command,
                    source: benchmark_contract::vocabulary::Source::Hyperfine,
                    label: None,
                    unit: benchmark_contract::vocabulary::Unit::Second,
                    availability: Availability::Measured,
                    reason: None,
                }],
                runs: (0..2)
                    .map(|index| RunRecord {
                        index,
                        status: RunStatus::Success,
                        exit_code: Some(0),
                        reason: None,
                        values: BTreeMap::from([(
                            key.clone(),
                            MetricValue::Measured { value: 0.01 },
                        )]),
                    })
                    .collect(),
                aggregates: BTreeMap::new(),
            }
        };
        let confirmed = result(known("1.2.3"));
        let verdict = comparability::compare(&scenario, &confirmed, &result(fact));
        assert!(!verdict.comparable);
        assert!(
            verdict.blockers.iter().any(|b| b.contains("tool_version")),
            "{:?}",
            verdict.blockers
        );
        assert!(comparability::compare(&scenario, &confirmed, &confirmed.clone()).comparable);
    }
}
