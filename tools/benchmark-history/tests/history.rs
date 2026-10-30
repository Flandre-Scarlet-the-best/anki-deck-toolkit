//! Проверки локальной истории на синтетическом каталоге и синтетических
//! результатах: архив, индекс, восстановление, перенос, выбор базы,
//! сравнение, отчёт для PR и снимок документации. Пользовательские данные,
//! `decks/**` и настоящий каталог данных пользователя не используются: каждое
//! хранилище — свой временный каталог.

use benchmark_contract::catalog::{self, Catalog};
use benchmark_contract::result::{
    Availability, BenchmarkResult, CodeState, Fact, Identity, InputRecord, MetricRecord,
    MetricValue, Observation, Outcome, OutcomeStatus, ProfileRef, Provenance, Quantity, RunRecord,
    RunStatus, ScenarioRef, Tool,
};
use benchmark_contract::scenario::{IdentityMethod, InputRole};
use benchmark_contract::vocabulary::{
    CacheState, MetricId, Network, Scope, Source, Unit, WorkloadKind,
};
use benchmark_history::analysis::{self, Assessment};
use benchmark_history::report::{self, Base, Item};
use benchmark_history::snapshot::{self, Options};
use benchmark_history::store::{Error, Store};
use benchmark_history::{archive, index, location};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_benchmark-history");

/// Команда без унаследованных переменных, привязывающих Git к репозиторию:
/// иначе, например из Git-хука, она ушла бы из временного репозитория в чужой.
fn isolated(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in analysis::GIT_LOCAL_ENV {
        command.env_remove(name);
    }
    command
}

/// Список снимаемых переменных сверяется с установленным Git независимо от
/// [`isolated`], который берёт тот же список: новая репозиторно-локальная
/// переменная Git иначе осталась бы незамеченной. Лишние имена допустимы —
/// их знает более новый Git, чем установленный.
#[test]
fn git_local_env_covers_installed_git() {
    let output = isolated("git")
        .args(["rev-parse", "--local-env-vars"])
        .current_dir(std::env::temp_dir())
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        other => {
            assert!(
                std::env::var_os("CI").is_none(),
                "в CI нужен Git: без него список GIT_LOCAL_ENV не сверяется ({other:?})"
            );
            eprintln!(
                "ПРОПУСК: Git недоступен, GIT_LOCAL_ENV не сверен с git rev-parse --local-env-vars"
            );
            return;
        }
    };
    let installed = String::from_utf8(output.stdout).unwrap();
    let missing: Vec<&str> = installed
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !analysis::GIT_LOCAL_ENV.contains(name))
        .collect();
    assert!(
        missing.is_empty(),
        "установленный Git сообщает репозиторно-локальные переменные, которых нет в analysis::GIT_LOCAL_ENV: {}; добавьте их, иначе унаследованные из Git-хука значения уведут сверку изменений в чужой репозиторий",
        missing.join(", ")
    );
}

const CATALOG: &str = r#"schema_version = 1
repository_root = "../workspace"

[[profile]]
id = "warm"
cache = "warm"
network = "online"
description = "Повторный прогон после прогрева."
"#;

const SCENARIO: &str = r#"schema_version = 1
module = "fixture"

[[scenario]]
id = "fixture.cli"
title = "Синтетический сценарий истории"
revision = 1
kind = "short-cli"
status = "executable"
description = "Синтетические результаты для проверки истории."
measured = "Один запуск."
platforms = ["linux"]

[scenario.target]
program = { kind = "executable", name = "true" }
expect_exit_codes = [0]

[[scenario.input]]
id = "toolkit"
kind = "repo-tree"
role = "subject"
identity = "content-tree-sha256"
description = "Измеряемый код."

[[scenario.input]]
id = "data"
kind = "repo-file"
role = "workload"
identity = "file-sha256"
path = "data.bin"
description = "Нагрузка."
size = [{ name = "bytes", unit = "byte", description = "Размер.", measure = { kind = "tree-bytes" } }]

[scenario.runs]
warmup = 1
min_runs = 2
max_runs = 20
single_run_allowed = false
run_timeout_seconds = 30
total_budget_seconds = 300

[[scenario.profile]]
id = "warm"
cache_definition = "Кэшей нет."
cleared = []
retained = []

[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[[scenario.metric]]
metric = "memory_peak_resident"
scope = "largest-process-in-tree"
source = "hyperfine"
required = false

[scenario.environment]
record = ["os", "arch", "module_version", "cpu_model", "virtualization", "logical_cpus", "memory_total_bytes"]
comparable = ["os", "arch", "cpu_model", "virtualization"]
"#;

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture() -> Fixture {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "benchmark-history-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("catalog/scenarios")).unwrap();
    fs::create_dir_all(root.join("workspace/docs")).unwrap();
    fs::write(root.join("workspace/data.bin"), b"data").unwrap();
    fs::write(root.join("catalog/catalog.toml"), CATALOG).unwrap();
    fs::write(root.join("catalog/scenarios/fixture.toml"), SCENARIO).unwrap();
    let fixture = Fixture { root };
    let loaded = catalog::load(&fixture.catalog_dir());
    assert!(loaded.catalog.is_some(), "{:#?}", loaded.diagnostics);
    fixture
}

impl Fixture {
    fn catalog_dir(&self) -> PathBuf {
        self.root.join("catalog")
    }

    fn catalog(&self) -> Catalog {
        catalog::load(&self.catalog_dir()).catalog.unwrap()
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    fn open(&self) -> Store {
        Store::open(&self.store()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn import(&self, store: &mut Store, result: &BenchmarkResult) -> String {
        let text = serde_json::to_string_pretty(result).unwrap();
        store
            .import(&text, "result.json", Some(&self.catalog()))
            .unwrap_or_else(|e| panic!("{e}"))
            .id
    }

    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let output = isolated(BIN)
            .arg("--catalog")
            .arg(self.catalog_dir())
            .args(args)
            .env("BENCHMARK_HISTORY_DIR", self.store())
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// Параметры синтетического результата.
#[derive(Clone)]
struct Spec {
    recorded_at: &'static str,
    commit: char,
    dirty: bool,
    wall: Vec<f64>,
    memory: Option<Vec<f64>>,
    failed_runs: usize,
    outcome: OutcomeStatus,
    cpu: &'static str,
    data: char,
    /// Объём памяти: аппаратный факт вне условия сопоставимости.
    ram: Option<&'static str>,
    virtualization: &'static str,
    /// Записан ли `source_tree_sha256`.
    tree: bool,
    /// Записан ли признак изменений рабочей копии.
    state: bool,
}

impl Default for Spec {
    fn default() -> Spec {
        Spec {
            recorded_at: "2026-10-01T10:00:00Z",
            commit: 'a',
            dirty: false,
            wall: vec![0.100, 0.102, 0.098],
            memory: Some(vec![10_000_000.0, 10_000_000.0, 10_000_000.0]),
            failed_runs: 0,
            outcome: OutcomeStatus::Success,
            cpu: "Test CPU",
            data: 'd',
            ram: Some("17179869184"),
            virtualization: "none",
            tree: true,
            state: true,
        }
    }
}

const WALL: &str = "time_wall_clock@command";
const MEMORY: &str = "memory_peak_resident@largest-process-in-tree";

fn result(spec: &Spec) -> BenchmarkResult {
    let fact = |v: &str| Fact::Known { value: v.into() };
    let environment = BTreeMap::from([
        ("os".to_owned(), fact("linux")),
        ("arch".to_owned(), fact("x86_64")),
        ("module_version".to_owned(), fact("0.1.0")),
        ("cpu_model".to_owned(), fact(spec.cpu)),
        ("virtualization".to_owned(), fact(spec.virtualization)),
        ("logical_cpus".to_owned(), fact("16")),
        (
            "memory_total_bytes".to_owned(),
            match spec.ram {
                Some(value) => fact(value),
                None => Fact::Unavailable {
                    reason: "/proc/meminfo не читается".into(),
                },
            },
        ),
    ]);
    let mut runs: Vec<RunRecord> = spec
        .wall
        .iter()
        .enumerate()
        .map(|(i, wall)| {
            let memory = match &spec.memory {
                Some(values) => MetricValue::Measured { value: values[i] },
                None => MetricValue::Unavailable {
                    reason: "нет доступа к счётчику".into(),
                },
            };
            RunRecord {
                index: i as u32,
                status: RunStatus::Success,
                exit_code: Some(0),
                reason: None,
                values: BTreeMap::from([
                    (WALL.to_owned(), MetricValue::Measured { value: *wall }),
                    (MEMORY.to_owned(), memory),
                ]),
            }
        })
        .collect();
    for _ in 0..spec.failed_runs {
        let index = runs.len() as u32;
        runs.push(RunRecord {
            index,
            status: RunStatus::Failure,
            exit_code: Some(1),
            reason: Some("цель завершилась с кодом 1".into()),
            values: BTreeMap::from([
                (
                    WALL.to_owned(),
                    MetricValue::Failed {
                        reason: "прогон не удался".into(),
                    },
                ),
                (
                    MEMORY.to_owned(),
                    MetricValue::Failed {
                        reason: "прогон не удался".into(),
                    },
                ),
            ]),
        });
    }
    let unsupported = spec.outcome == OutcomeStatus::Unsupported;
    if unsupported {
        runs.clear();
    }
    let availability = |key: &str, measured_all: bool| {
        let any = runs
            .iter()
            .any(|r| r.values.get(key).and_then(MetricValue::measured).is_some());
        match (unsupported, any, measured_all && spec.failed_runs == 0) {
            (true, _, _) => Availability::Unsupported,
            (_, true, true) => Availability::Measured,
            (_, true, false) => Availability::Partial,
            (_, false, _) if key == WALL => Availability::Failed,
            _ => Availability::Unavailable,
        }
    };
    let wall_availability = availability(WALL, true);
    let memory_availability = availability(MEMORY, spec.memory.is_some());
    let reason = |a: Availability| {
        (a != Availability::Measured).then(|| "значения есть не во всех прогонах".to_owned())
    };
    let mut result = BenchmarkResult {
        schema_version: 1,
        scenario: ScenarioRef {
            id: "fixture.cli".into(),
            module: "fixture".into(),
            revision: 1,
            kind: WorkloadKind::ShortCli,
        },
        profile: ProfileRef {
            id: "warm".into(),
            cache: CacheState::Warm,
            network: Network::Online,
        },
        parameters: BTreeMap::new(),
        inputs: vec![
            InputRecord {
                id: "toolkit".into(),
                role: InputRole::Subject,
                identity: Identity {
                    method: IdentityMethod::ContentTreeSha256,
                    value: spec.commit.to_string().repeat(64),
                },
                size: BTreeMap::new(),
            },
            InputRecord {
                id: "data".into(),
                role: InputRole::Workload,
                identity: Identity {
                    method: IdentityMethod::FileSha256,
                    value: spec.data.to_string().repeat(64),
                },
                size: BTreeMap::from([(
                    "bytes".to_owned(),
                    Quantity {
                        value: 4.0,
                        unit: Unit::Byte,
                    },
                )]),
            },
        ],
        provenance: Provenance {
            recorded_at: spec.recorded_at.into(),
            measurement: Tool {
                name: "hyperfine".into(),
                version: "2.0.0".into(),
                format: Some("hyperfine-json-v2".into()),
            },
            runner: Some(Tool {
                name: "benchmark-runner".into(),
                version: "0.1.0".into(),
                format: None,
            }),
            code: CodeState {
                module_version: "0.1.0".into(),
                source_tree_sha256: spec.tree.then(|| spec.commit.to_string().repeat(64)),
                git_commit: Some(spec.commit.to_string().repeat(40)),
                dirty: spec.state.then_some(spec.dirty),
            },
        },
        environment,
        outcome: Outcome {
            status: spec.outcome,
            reason: match spec.outcome {
                OutcomeStatus::Success => None,
                OutcomeStatus::Unsupported => Some("Hyperfine 2.x не найден в PATH".into()),
                _ => Some("прогон не удался".into()),
            },
        },
        observation: match runs.len() {
            0 => Observation::None,
            1 => Observation::Single,
            _ => Observation::Repeated,
        },
        warmup_runs: 1,
        metrics: vec![
            MetricRecord {
                key: WALL.into(),
                metric: MetricId::TimeWallClock,
                scope: Scope::Command,
                source: Source::Hyperfine,
                label: None,
                unit: Unit::Second,
                availability: wall_availability,
                reason: reason(wall_availability),
            },
            MetricRecord {
                key: MEMORY.into(),
                metric: MetricId::MemoryPeakResident,
                scope: Scope::LargestProcessInTree,
                source: Source::Hyperfine,
                label: None,
                unit: Unit::Byte,
                availability: memory_availability,
                reason: reason(memory_availability),
            },
        ],
        runs,
        aggregates: BTreeMap::new(),
    };
    result.recompute_aggregates();
    result
}

fn assert_valid(fixture: &Fixture, r: &BenchmarkResult) {
    let text = serde_json::to_string(r).unwrap();
    let checked = benchmark_contract::result::check(&text, "x", Some(&fixture.catalog()));
    assert!(checked.result.is_some(), "{:#?}", checked.diagnostics);
}

#[test]
fn synthetic_results_satisfy_the_contract() {
    let f = fixture();
    for spec in [
        Spec::default(),
        Spec {
            memory: None,
            ..Spec::default()
        },
        Spec {
            failed_runs: 1,
            outcome: OutcomeStatus::Partial,
            ..Spec::default()
        },
        Spec {
            wall: vec![],
            memory: Some(vec![]),
            outcome: OutcomeStatus::Unsupported,
            ..Spec::default()
        },
    ] {
        assert_valid(&f, &result(&spec));
    }
}

#[test]
fn import_is_idempotent_and_keeps_original_bytes() {
    let f = fixture();
    let mut store = f.open();
    let r = result(&Spec::default());
    let pretty = serde_json::to_string_pretty(&r).unwrap();
    let compact = serde_json::to_string(&r).unwrap();
    let first = store.import(&pretty, "a.json", Some(&f.catalog())).unwrap();
    assert!(first.created);
    let again = store
        .import(&compact, "b.json", Some(&f.catalog()))
        .unwrap();
    assert_eq!(
        again.id, first.id,
        "форматирование не создаёт второй записи"
    );
    assert!(!again.created);
    assert_eq!(store.index().count().unwrap(), 1);
    let stored = fs::read_to_string(f.store().join(archive::relative_path(&first.id))).unwrap();
    assert_eq!(stored, pretty, "архив хранит исходные байты");

    // Граница идентичности, описанная в README: пробелы и порядок ключей не
    // важны, написание числа — важно (нормализация сменила бы ID записей).
    let id = |text: &str| archive::content_id(text).unwrap();
    assert_eq!(
        id(r#"{"b": 1.0, "a": [1, 2]}"#),
        id(r#"{"a":[1,2],"b":1.0}"#)
    );
    assert_eq!(id(r#"{"a": 1.0}"#), id(r#"{"a": 1.00}"#));
    assert_ne!(id(r#"{"a": 1}"#), id(r#"{"a": 1.0}"#));
}

#[test]
fn invalid_documents_are_rejected_without_trace() {
    let f = fixture();
    let mut store = f.open();
    let mut r = serde_json::to_value(result(&Spec::default())).unwrap();
    r["schema_version"] = 2.into();
    let rejected = store.import(&r.to_string(), "new.json", Some(&f.catalog()));
    assert!(matches!(rejected, Err(Error::Rejected(_))));
    let mut r = serde_json::to_value(result(&Spec::default())).unwrap();
    r["outcome"]["reason"] = "ошибка в /home/user/private/deck.json".into();
    r["outcome"]["status"] = "partial".into();
    assert!(matches!(
        store.import(&r.to_string(), "private.json", Some(&f.catalog())),
        Err(Error::Rejected(_))
    ));
    assert!(matches!(
        store.import("{not json", "broken.json", Some(&f.catalog())),
        Err(Error::Rejected(_))
    ));
    let mut unknown = result(&Spec::default());
    unknown.scenario.id = "fixture.other".into();
    assert!(matches!(
        store.import(
            &serde_json::to_string(&unknown).unwrap(),
            "u.json",
            Some(&f.catalog())
        ),
        Err(Error::Rejected(_))
    ));
    assert_eq!(store.index().count().unwrap(), 0);
    let (entries, _) = archive::scan(&f.store()).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn index_is_rebuilt_from_archive() {
    let f = fixture();
    let ids = {
        let mut store = f.open();
        vec![
            f.import(&mut store, &result(&Spec::default())),
            f.import(
                &mut store,
                &result(&Spec {
                    recorded_at: "2026-10-02T10:00:00Z",
                    commit: 'b',
                    ..Spec::default()
                }),
            ),
        ]
    };
    // Удалённый индекс.
    fs::remove_file(f.store().join("index.sqlite")).unwrap();
    let store = f.open();
    assert_eq!(store.index().count().unwrap(), 2);
    assert_eq!(store.opened_sync().indexed.len(), 2);
    drop(store);

    // Повреждённый индекс сохраняется для разбора и строится заново.
    fs::write(f.store().join("index.sqlite"), b"definitely not sqlite").unwrap();
    let store = f.open();
    assert_eq!(store.index().count().unwrap(), 2);
    assert!(f.store().join("index.sqlite.corrupt").is_file());
    assert!(
        store
            .opened_sync()
            .notes
            .iter()
            .any(|n| n.contains("повреждён"))
    );
    drop(store);

    // Старая схема: индекс перестраивается из архива.
    {
        let conn = rusqlite_connection(&f.store().join("index.sqlite"));
        conn.execute_batch("DROP TABLE metrics; CREATE TABLE legacy (x); PRAGMA user_version = 0;")
            .unwrap();
    }
    let store = f.open();
    assert_eq!(store.index().count().unwrap(), 2);
    drop(store);
    // Неизвестная дочерняя таблица со ссылками на results не мешает пересборке
    // и не выдаётся за повреждение индекса.
    {
        let conn = rusqlite_connection(&f.store().join("index.sqlite"));
        conn.execute_batch(
            "CREATE TABLE legacy_child (result_id TEXT NOT NULL REFERENCES results(id));
             INSERT INTO legacy_child SELECT id FROM results;
             PRAGMA user_version = 0;",
        )
        .unwrap();
    }
    fs::remove_file(f.store().join("index.sqlite.corrupt")).unwrap();
    let store = f.open();
    assert_eq!(store.index().count().unwrap(), 2);
    assert!(
        !f.store().join("index.sqlite.corrupt").exists(),
        "{:?}",
        store.opened_sync().notes
    );
    drop(store);
    {
        let conn = rusqlite_connection(&f.store().join("index.sqlite"));
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
    }
    let store = f.open();
    assert!(
        store
            .opened_sync()
            .notes
            .iter()
            .any(|n| n.contains("устарела")),
        "{:?}",
        store.opened_sync().notes
    );
    assert_eq!(store.index().count().unwrap(), 2);
    drop(store);

    // Схема новее поддерживаемой: отказ, пересборка — явным reindex.
    {
        let conn = rusqlite_connection(&f.store().join("index.sqlite"));
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    }
    let refused = Store::open(&f.store());
    assert!(
        matches!(&refused, Err(Error::Store(m)) if m.contains("более новой")),
        "{:?}",
        refused.err()
    );
    let (store, sync) = Store::rebuild(&f.store()).unwrap();
    assert!(sync.problems.is_empty());
    let mut rebuilt: Vec<String> = store.index().ids().unwrap().into_iter().collect();
    rebuilt.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(rebuilt, expected);
}

fn rusqlite_connection(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

#[test]
fn interrupted_write_is_recovered_on_next_open() {
    let f = fixture();
    drop(f.open());
    // Прерывание после записи архивной копии, до строки индекса.
    let r = result(&Spec::default());
    let text = serde_json::to_string_pretty(&r).unwrap();
    let id = archive::content_id(&text).unwrap();
    let path = f.store().join(archive::relative_path(&id));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &text).unwrap();
    // Незавершённая временная копия и посторонний файл.
    fs::create_dir_all(f.store().join("tmp")).unwrap();
    fs::write(f.store().join("tmp/x.json.1.2.tmp"), "{").unwrap();
    fs::write(f.store().join("results/notes.txt"), "x").unwrap();
    let store = f.open();
    assert_eq!(store.opened_sync().indexed, vec![id.clone()]);
    assert_eq!(store.opened_sync().problems.len(), 1);
    drop(store);
    // Удалённая вручную архивная копия убирается из индекса.
    fs::remove_file(&path).unwrap();
    let store = f.open();
    assert_eq!(store.opened_sync().dropped, vec![id]);
    assert_eq!(store.index().count().unwrap(), 0);
}

#[test]
fn busy_store_is_awaited_only_within_the_limit() {
    let f = fixture();
    let held = f.open();
    let started = std::time::Instant::now();
    let refused = Store::open_waiting(&f.store(), std::time::Duration::from_millis(300));
    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
    match refused {
        Err(Error::Store(reason)) => assert!(reason.contains("занято"), "{reason}"),
        Err(e) => panic!("неожиданная ошибка: {e}"),
        Ok(_) => panic!("блокировка занята, открытие должно отказать"),
    }
    drop(held);
    Store::open_waiting(&f.store(), std::time::Duration::from_millis(300))
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn corrupted_archive_copy_is_detected_and_never_overwritten() {
    let f = fixture();
    let mut store = f.open();
    let r = result(&Spec::default());
    let text = serde_json::to_string_pretty(&r).unwrap();
    let id = f.import(&mut store, &r);
    let path = f.store().join(archive::relative_path(&id));
    let damaged = text.replace("0.102", "0.103");
    fs::write(&path, &damaged).unwrap();
    let report = store.verify().unwrap();
    assert_eq!(report.problems.len(), 1);
    assert_eq!(report.problems[0].item, id);
    assert!(matches!(store.load(&id), Err(Error::Store(_))));
    // Повторный приём того же документа не перезаписывает повреждённую копию.
    let again = store.import(&text, "again.json", Some(&f.catalog()));
    assert!(
        matches!(&again, Err(Error::Store(m)) if m.contains("повреждена")),
        "{:?}",
        again.err()
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), damaged);
    drop(store);
    let (_, sync) = Store::rebuild(&f.store()).unwrap();
    assert_eq!(sync.problems.len(), 1);
}

#[test]
fn backup_restores_into_another_store() {
    let f = fixture();
    let mut store = f.open();
    let a = f.import(&mut store, &result(&Spec::default()));
    let b = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-02T10:00:00Z",
            commit: 'b',
            ..Spec::default()
        }),
    );
    let backup = f.root.join("backup");
    assert_eq!(store.backup(&backup).unwrap(), 2);
    assert!(
        matches!(store.backup(&backup), Err(Error::Usage(_))),
        "непустой каталог не перезаписывается"
    );
    // Другое хранилище — как на другой машине: каталог при восстановлении не нужен.
    let mut other = Store::open(&f.root.join("other")).unwrap();
    let restored = other.restore(&backup).unwrap();
    assert_eq!(restored.added.len(), 2);
    let again = other.restore(&backup).unwrap();
    assert_eq!((again.added.len(), again.present.len()), (0, 2));
    let mut ids: Vec<String> = other.index().ids().unwrap().into_iter().collect();
    ids.sort();
    let mut expected = vec![a.clone(), b];
    expected.sort();
    assert_eq!(ids, expected);
    // Изменённая запись резервной копии не принимается.
    let tampered = backup.join(archive::relative_path(&a));
    let text = fs::read_to_string(&tampered)
        .unwrap()
        .replace("0.098", "0.001");
    fs::write(&tampered, text).unwrap();
    let mut third = Store::open(&f.root.join("third")).unwrap();
    let restored = third.restore(&backup).unwrap();
    assert_eq!(restored.added.len(), 1);
    assert_eq!(restored.problems.len(), 1);
}

/// Все пути дерева с размерами: по ним видно, что отказ ничего не записал.
fn listing(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.path().symlink_metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                if meta.is_file() { meta.len() } else { 0 },
            ));
        }
    }
    out.sort();
    out
}

/// Переменная с каталогом на файловой системе без учёта регистра (например,
/// `/mnt/c/...` в WSL) для проверки пересечения путей. Без неё проверяется
/// системный временный каталог; на файловой системе с учётом регистра
/// проверка пропускается с сообщением.
const CASE_INSENSITIVE_DIR_ENV: &str = "BENCHMARK_HISTORY_TEST_CASE_INSENSITIVE_DIR";

#[test]
fn backup_refuses_overlap_spelled_in_another_case() {
    let f = fixture();
    let base = std::env::var_os(CASE_INSENSITIVE_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("bh-case-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("Probe")).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(base.clone());
    if !base.join("probe").exists() {
        eprintln!(
            "ПРОПУСК: {} учитывает регистр; задайте {CASE_INSENSITIVE_DIR_ENV} с каталогом без учёта регистра",
            base.display()
        );
        return;
    }
    let root = base.join("store");
    let mut store = Store::open(&root).unwrap_or_else(|e| panic!("{e}"));
    f.import(&mut store, &result(&Spec::default()));
    let before = listing(&root);
    let other_case = base.join("STORE");
    for (destination, expected) in [
        (other_case.join("results/backup"), "лежит внутри хранилища"),
        (other_case.clone(), "совпадает с хранилищем"),
    ] {
        match store.backup(&destination) {
            Err(Error::Usage(message)) => {
                assert!(message.contains(expected), "{destination:?}: {message}")
            }
            other => panic!("{destination:?}: ожидался отказ, получено {other:?}"),
        }
    }
    match store.restore(&other_case.join("results")) {
        Err(Error::Usage(message)) => {
            assert!(message.contains("лежит внутри хранилища"), "{message}")
        }
        other => panic!("restore изнутри хранилища: ожидался отказ, получено {other:?}"),
    }
    assert_eq!(listing(&root), before, "отказы ничего не записали");
}

#[test]
fn backup_refuses_destinations_overlapping_the_store() {
    let f = fixture();
    let mut store = f.open();
    f.import(&mut store, &result(&Spec::default()));
    f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-02T10:00:00Z",
            commit: 'b',
            ..Spec::default()
        }),
    );
    let before = listing(&f.store());
    let refused =
        |store: &Store, destination: &Path, expected: &str| match store.backup(destination) {
            Err(Error::Usage(message)) => {
                assert!(message.contains(expected), "{destination:?}: {message}")
            }
            other => panic!("{destination:?}: ожидался отказ, получено {other:?}"),
        };
    // Новый пустой каталог внутри архива отвергается, хотя он и не в Git.
    refused(
        &store,
        &f.store().join("results/backup"),
        "лежит внутри хранилища",
    );
    refused(&store, &f.store(), "совпадает с хранилищем");
    // Предок хранилища отвергается по взаимному расположению, а не только как непустой.
    refused(&store, &f.root, "содержит хранилище");
    #[cfg(unix)]
    {
        // Ссылка вне хранилища, ведущая внутрь него, и путь через неё.
        std::os::unix::fs::symlink(f.store().join("results"), f.root.join("into-store")).unwrap();
        refused(&store, &f.root.join("into-store"), "лежит внутри хранилища");
        refused(
            &store,
            &f.root.join("into-store/new/backup"),
            "лежит внутри хранилища",
        );
        // Ссылка на хранилище целиком, указанная как место назначения.
        std::os::unix::fs::symlink(f.store(), f.root.join("store-link")).unwrap();
        refused(&store, &f.root.join("store-link"), "совпадает с хранилищем");
        // Висячая ссылка: запись через неё ушла бы по её цели.
        std::os::unix::fs::symlink(f.store().join("missing"), f.root.join("dangling")).unwrap();
        refused(
            &store,
            &f.root.join("dangling"),
            "висячую символическую ссылку",
        );
        // Восстановление из каталога внутри хранилища тоже отвергается.
        assert!(matches!(
            store.restore(&f.root.join("into-store")),
            Err(Error::Usage(m)) if m.contains("лежит внутри хранилища")
        ));
    }
    assert_eq!(
        listing(&f.store()),
        before,
        "отказ ничего не записал в хранилище"
    );
    let verification = store.verify().unwrap();
    assert!(
        verification.problems.is_empty() && verification.index_problems.is_empty(),
        "{verification:?}"
    );
    // Внешний пустой каталог по-прежнему принимается, а копия восстанавливается.
    let outside = f.root.join("outside-backup");
    fs::create_dir_all(&outside).unwrap();
    assert_eq!(store.backup(&outside).unwrap(), 2);
    assert_eq!(listing(&f.store()), before, "копия не тронула архив");
    let mut other = Store::open(&f.root.join("other")).unwrap();
    let restored = other.restore(&outside).unwrap();
    assert!(
        restored.complete && restored.added.len() == 2,
        "{restored:?}"
    );
    drop(store);
    let (code, out, err) = f.cli(&["verify"]);
    assert_eq!(code, 0, "{out}{err}");
}

/// Синтетические коммиты в Git не существуют: состав изменений неизвестен.
fn no_git(_: &str, _: &str) -> Result<Vec<String>, String> {
    Err("коммитов нет в репозитории".into())
}

/// Число изменённых Markdown-файлов выводится формой, верной для любого числа.
#[test]
fn documentation_only_reason_reads_correctly_for_any_count() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let base = result(&Spec::default());
    let target = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        ..Spec::default()
    });
    for n in [1, 2, 4, 5] {
        let docs = move |_: &str, _: &str| -> Result<Vec<String>, String> {
            Ok((0..n).map(|i| format!("docs/{i}.md")).collect())
        };
        let list = analysis::candidates(
            scenario,
            "target",
            &target,
            &[("base".to_owned(), base.clone())],
            &docs,
        );
        let expected = format!("изменена только документация (файлов Markdown: {n})");
        assert!(
            list[0].reasons.iter().any(|r| r.contains(&expected)),
            "{n}: {:?}",
            list[0].reasons
        );
    }
}

fn pool(store: &Store, ids: &[String]) -> Vec<(String, BenchmarkResult)> {
    ids.iter()
        .map(|id| (id.clone(), store.load(id).unwrap().1))
        .collect()
}

#[test]
fn baseline_follows_explicit_rules() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let mut store = f.open();
    let old = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-01T10:00:00Z",
            commit: 'a',
            ..Spec::default()
        }),
    );
    let newer = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-02T10:00:00Z",
            commit: 'b',
            ..Spec::default()
        }),
    );
    let partial = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-03T10:00:00Z",
            commit: 'c',
            failed_runs: 1,
            outcome: OutcomeStatus::Partial,
            ..Spec::default()
        }),
    );
    let other_cpu = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-04T10:00:00Z",
            commit: 'c',
            cpu: "Other CPU",
            ..Spec::default()
        }),
    );
    let other_data = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-04T11:00:00Z",
            commit: 'c',
            data: 'e',
            ..Spec::default()
        }),
    );
    let repeat = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-05T10:00:00Z",
            commit: 'f',
            wall: vec![0.2, 0.2, 0.2],
            ..Spec::default()
        }),
    );
    let target_spec = Spec {
        recorded_at: "2026-10-06T10:00:00Z",
        commit: 'f',
        ..Spec::default()
    };
    let target = f.import(&mut store, &result(&target_spec));
    let later = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-07T10:00:00Z",
            commit: '9',
            ..Spec::default()
        }),
    );
    let all = [
        old.clone(),
        newer.clone(),
        partial.clone(),
        other_cpu.clone(),
        other_data.clone(),
        repeat.clone(),
        later.clone(),
    ];
    let list = analysis::candidates(
        scenario,
        &target,
        &result(&target_spec),
        &pool(&store, &all),
        &no_git,
    );
    let find = |id: &str| list.iter().find(|c| c.id == id).unwrap();
    assert!(find(&partial).reasons.iter().any(|r| r.contains("partial")));
    assert!(
        find(&other_cpu)
            .reasons
            .iter()
            .any(|r| r.contains("cpu_model"))
    );
    assert!(
        find(&other_data)
            .reasons
            .iter()
            .any(|r| r.contains("нагрузка data"))
    );
    assert!(
        find(&repeat)
            .reasons
            .iter()
            .any(|r| r.contains("тот же исходный код"))
    );
    assert!(find(&later).reasons.iter().any(|r| r.contains("не раньше")));
    assert!(find(&old).eligible && find(&newer).eligible);
    assert_eq!(
        analysis::choose(&list, None).unwrap().id,
        newer,
        "самая поздняя пригодная"
    );
    assert_eq!(analysis::choose(&list, Some("aaaa")).unwrap().id, old);
    assert!(analysis::choose(&list, Some("cccc")).is_none());
    // Первое измерение: кандидатов нет.
    let empty = analysis::candidates(scenario, &old, &result(&Spec::default()), &[], &no_git);
    assert!(analysis::choose(&empty, None).is_none());
}

#[test]
fn comparison_respects_units_zero_base_and_spread() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let base = result(&Spec {
        commit: 'a',
        memory: Some(vec![0.0, 0.0, 0.0]),
        ..Spec::default()
    });
    let faster = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.080, 0.081, 0.079],
        memory: Some(vec![5.0, 5.0, 5.0]),
        ..Spec::default()
    });
    let c = analysis::compare(scenario, &base, &faster);
    assert!(c.verdict.comparable, "{:?}", c.verdict.blockers);
    let wall = c.rows.iter().find(|r| r.key == WALL).unwrap();
    let delta = wall.delta.as_ref().unwrap();
    assert!((delta.absolute + 0.02).abs() < 1e-9);
    assert!((delta.relative.unwrap() + 0.2).abs() < 1e-9);
    assert_eq!(
        wall.assessment,
        Assessment::BeyondSpread { better: Some(true) }
    );
    let memory = c.rows.iter().find(|r| r.key == MEMORY).unwrap();
    assert!(
        memory.delta.as_ref().unwrap().relative.is_none(),
        "нулевая база не даёт процента"
    );
    assert_eq!(
        memory.assessment,
        Assessment::BeyondSpread {
            better: Some(false)
        }
    );

    let noisy = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.090, 0.110, 0.101],
        ..Spec::default()
    });
    let c = analysis::compare(scenario, &result(&Spec::default()), &noisy);
    assert_eq!(
        c.rows.iter().find(|r| r.key == WALL).unwrap().assessment,
        Assessment::WithinSpread
    );

    let single = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.05],
        memory: Some(vec![1.0]),
        ..Spec::default()
    });
    let c = analysis::compare(scenario, &result(&Spec::default()), &single);
    assert!(c.verdict.comparable && !c.verdict.statistically_supported);
    assert_eq!(
        c.rows.iter().find(|r| r.key == WALL).unwrap().assessment,
        Assessment::InsufficientRepeats
    );

    let unavailable = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        memory: None,
        ..Spec::default()
    });
    let c = analysis::compare(scenario, &result(&Spec::default()), &unavailable);
    let memory = c.rows.iter().find(|r| r.key == MEMORY).unwrap();
    assert!(memory.delta.is_none());
    assert!(matches!(memory.assessment, Assessment::NotCompared { .. }));

    let other = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        cpu: "Other",
        ..Spec::default()
    });
    let c = analysis::compare(scenario, &result(&Spec::default()), &other);
    assert!(!c.verdict.comparable);
    assert!(c.rows.iter().all(|r| r.delta.is_none()));
}

/// Тело PR с разделом производительности проходит проверку репозитория.
fn check_pr_body(section: &str) -> (bool, String) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/scripts/check_pr_body.py");
    let body = ["Цель", "Область изменений", "Реализация", "Проверки"]
        .iter()
        .map(|h| format!("## {h}\nФактическое содержание раздела {h}.\n"))
        .chain(std::iter::once(section.to_owned()))
        .chain(
            ["CI", "Готовность", "Ограничения"]
                .iter()
                .map(|h| format!("## {h}\nФактическое содержание раздела {h}.\n")),
        )
        .collect::<Vec<_>>()
        .join("\n");
    static BODY: AtomicUsize = AtomicUsize::new(0);
    let file = std::env::temp_dir().join(format!(
        "bh-body-{}-{}.md",
        std::process::id(),
        BODY.fetch_add(1, Ordering::SeqCst)
    ));
    fs::write(&file, &body).unwrap();
    let output = Command::new("python3")
        .arg(&script)
        .arg("--body-file")
        .arg(&file)
        .output();
    let _ = fs::remove_file(&file);
    // Как Hyperfine у benchmark-runner: локально без python3 проверка
    // пропускается с сообщением, в CI его отсутствие — ошибка.
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "в CI нужен python3 для check_pr_body.py: {e}"
            );
            eprintln!("ПРОПУСК: python3 недоступен ({e}), проверка PR body не выполнена");
            return (true, String::new());
        }
    };
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn pr_reports_cover_first_comparable_missing_incomparable_and_unmeasured() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let base = result(&Spec::default());
    let target = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.080, 0.081, 0.079],
        memory: None,
        ..Spec::default()
    });
    let other = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        cpu: "Other",
        ..Spec::default()
    });
    let unsupported = result(&Spec {
        wall: vec![],
        memory: Some(vec![]),
        outcome: OutcomeStatus::Unsupported,
        ..Spec::default()
    });
    let id = "1".repeat(64);
    let base_id = "2".repeat(64);

    let first = report::pr_section(&[Item {
        id: &id,
        result: &target,
        base: Base::First,
    }]);
    assert!(
        first.contains("Статус: измерено") && first.contains("первое измерение"),
        "{first}"
    );
    assert!(first.contains("`memory_peak_resident`"), "{first}");

    // Объявленная версия входа — произвольный текст: многобайтные символы
    // и короткое значение не ломают отчёт.
    for version in ["версия-α", "v1", "ä".repeat(20).as_str()] {
        let mut declared = target.clone();
        declared.inputs[1].identity = Identity {
            method: IdentityMethod::DeclaredVersion,
            value: version.into(),
        };
        let section = report::pr_section(&[Item {
            id: &id,
            result: &declared,
            base: Base::First,
        }]);
        let shown: String = version.chars().take(12).collect();
        assert!(
            section.contains(&format!("`declared-version` `{shown}`")),
            "{section}"
        );
    }
    assert!(
        first
            .contains("`memory_peak_resident@largest-process-in-tree` не измерена (`unavailable`)"),
        "недоступная метрика видна: {first}"
    );

    let comparable = report::pr_section(&[Item {
        id: &id,
        result: &target,
        base: Base::Chosen {
            id: &base_id,
            result: &base,
            comparison: analysis::compare(scenario, &base, &target),
            how: "указана явно".into(),
            notes: vec![],
        },
    }]);
    assert!(
        comparable.contains("−20 %") && comparable.contains("лучше сверх разброса"),
        "{comparable}"
    );
    assert!(comparable.contains("не является проверкой статистической значимости"));
    assert!(comparable.contains(&base_id) && comparable.contains(&id));

    let incomparable = report::pr_section(&[Item {
        id: &id,
        result: &other,
        base: Base::Chosen {
            id: &base_id,
            result: &base,
            comparison: analysis::compare(scenario, &base, &other),
            how: "указана явно".into(),
            notes: vec![],
        },
    }]);
    assert!(
        incomparable.contains("**несопоставимы**")
            && incomparable.contains("вывод об изменении не делается"),
        "{incomparable}"
    );
    assert!(!incomparable.contains("лучше сверх"));

    let missing = report::pr_section(&[Item {
        id: &id,
        result: &target,
        base: Base::Missing { reasons: vec![] },
    }]);
    assert!(
        missing.contains("База: не найдена") && missing.contains("сравнение до/после не выполнено")
    );

    let unmeasured = report::pr_section(&[Item {
        id: &id,
        result: &unsupported,
        base: Base::Missing { reasons: vec![] },
    }]);
    assert!(unmeasured.contains("Статус: не измерено"), "{unmeasured}");
    assert!(unmeasured.contains("Hyperfine 2.x не найден"));
    assert!(unmeasured.contains(report::NO_EVIDENCE));

    for section in [&first, &comparable, &incomparable, &missing, &unmeasured] {
        let (ok, stderr) = check_pr_body(section);
        assert!(
            ok,
            "сгенерированный раздел отвергнут проверкой PR body: {stderr}\n{section}"
        );
    }
}

/// Более поздний результат с той же нагрузкой, что и `base`.
fn newer_same_workload(base: &BenchmarkResult) -> BenchmarkResult {
    let mut next = base.clone();
    next.provenance.recorded_at = "2026-10-06T12:00:00Z".into();
    next
}

const PAGE_RU: &str = "# Производительность: `fixture`\n\n**Русский** · [English](fixture.en.md) · [简体中文](fixture.zh-CN.md)\n\n## Снимки\n\n<!-- benchmark-snapshot:anchor -->\n\n## Методика\n\nТекст.\n";

fn page(lang: &str, ru_sha: &str) -> String {
    let nav = match lang {
        "en" => "[Русский](fixture.md) · **English** · [简体中文](fixture.zh-CN.md)",
        _ => "[Русский](fixture.md) · [English](fixture.en.md) · **简体中文**",
    };
    format!(
        "# Performance: `fixture`\n\n{nav}\n<!-- i18n-source-git-blob: {ru_sha} -->\n\n## Snapshots\n\n<!-- benchmark-snapshot:anchor -->\n\n## Method\n\nText.\n"
    )
}

#[test]
fn snapshots_update_one_pair_and_guard_reference_environment() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let docs = f.root.join("workspace/docs");
    let ru = docs.join("fixture.md");
    fs::write(&ru, PAGE_RU).unwrap();
    let sha = snapshot::git_blob_sha(PAGE_RU.as_bytes());
    fs::write(docs.join("fixture.en.md"), page("en", &sha)).unwrap();
    fs::write(docs.join("fixture.zh-CN.md"), page("zh", &sha)).unwrap();

    let first = result(&Spec::default());
    let newer = result(&Spec {
        recorded_at: "2026-10-05T10:00:00Z",
        commit: 'b',
        wall: vec![0.05, 0.05],
        memory: Some(vec![1.0, 1.0]),
        ..Spec::default()
    });
    // Локальная история с результатами снимков: доказательство тождества нагрузки.
    let options = Options {
        history: [&first, &newer]
            .map(|r| snapshot::Recorded::of(scenario, r))
            .to_vec(),
        ..Options::default()
    };
    let plan = snapshot::plan(&ru, scenario, &first, &options).unwrap();
    assert_eq!(plan.action, snapshot::Action::Added);
    snapshot::apply(&plan).unwrap();
    let text = fs::read_to_string(&ru).unwrap();
    assert!(text.contains("### `fixture.cli` · профиль `warm`"));
    assert!(
        !text.contains(&"a".repeat(40)),
        "коммит не попадает в документацию"
    );
    assert!(
        !text.contains(&"d".repeat(64)),
        "идентичность входа не попадает в документацию"
    );
    let new_sha = snapshot::git_blob_sha(text.as_bytes());
    for lang in ["en", "zh-CN"] {
        let translated = fs::read_to_string(docs.join(format!("fixture.{lang}.md"))).unwrap();
        assert!(
            translated.contains(&new_sha),
            "{lang}: отметка перевода обновлена"
        );
        assert!(translated.contains("benchmark-snapshot: {"));
    }
    assert!(
        fs::read_to_string(docs.join("fixture.en.md"))
            .unwrap()
            .contains("Last confirmed measurement")
    );
    assert!(
        fs::read_to_string(docs.join("fixture.zh-CN.md"))
            .unwrap()
            .contains("最近一次确认的测量：")
    );

    // Повтор того же результата ничего не меняет, но сам результат своего
    // происхождения не доказывает: без другого результата истории с тем же
    // блоком нужен --replace-workload.
    let refused = snapshot::plan(&ru, scenario, &first, &options).unwrap_err();
    assert!(refused.contains("сам публикуемый результат"), "{refused}");
    let repeat = Options {
        replace_workload: true,
        ..options.clone()
    };
    assert_eq!(
        snapshot::plan(&ru, scenario, &first, &repeat)
            .unwrap()
            .action,
        snapshot::Action::Unchanged
    );
    // Страница с CRLF: повтор тоже без изменений, вставка сохраняет CRLF.
    let lf = fs::read_to_string(&ru).unwrap();
    fs::write(&ru, lf.replace('\n', "\r\n")).unwrap();
    assert_eq!(
        snapshot::plan(&ru, scenario, &first, &repeat)
            .unwrap()
            .action,
        snapshot::Action::Unchanged
    );
    let mut other = first.clone();
    other.profile.id = "crlf".into();
    let plan = snapshot::plan(&ru, scenario, &other, &options).unwrap();
    assert_eq!(plan.action, snapshot::Action::Added);
    let crlf = &plan.pages[0].after;
    assert_eq!(crlf.matches('\n').count(), crlf.matches("\r\n").count());
    fs::write(&ru, lf).unwrap();

    // Новый результат того же окружения заменяет блок той же пары.
    let plan = snapshot::plan(&ru, scenario, &newer, &options).unwrap();
    assert_eq!(plan.action, snapshot::Action::Replaced);
    assert_eq!(plan.workload, snapshot::WorkloadBasis::ProvenByHistory);
    snapshot::apply(&plan).unwrap();
    let text = fs::read_to_string(&ru).unwrap();
    assert_eq!(text.matches("benchmark-snapshot: {").count(), 1);
    assert!(text.contains("2026-10-05") && !text.contains("2026-10-01"));
    assert!(
        text.contains("## Методика"),
        "остальная страница не тронута"
    );

    // Более старый результат не заменяет более новый.
    assert!(
        snapshot::plan(&ru, scenario, &first, &options)
            .unwrap_err()
            .contains("более новое")
    );
    // Другое окружение не заменяет эталон без явного подтверждения.
    let elsewhere = result(&Spec {
        recorded_at: "2026-10-06T10:00:00Z",
        commit: 'c',
        cpu: "Laptop CPU",
        ..Spec::default()
    });
    let refused = snapshot::plan(&ru, scenario, &elsewhere, &options).unwrap_err();
    assert!(refused.contains("другом эталонном окружении"), "{refused}");
    let replace = Options {
        replace_environment: true,
        ..options.clone()
    };
    assert_eq!(
        snapshot::plan(&ru, scenario, &elsewhere, &replace)
            .unwrap()
            .action,
        snapshot::Action::Replaced
    );
    // Нагрузка другого объёма не заменяет эталон без явного подтверждения.
    let mut larger = result(&Spec {
        recorded_at: "2026-10-06T10:00:00Z",
        commit: 'c',
        ..Spec::default()
    });
    larger.inputs[1].size.get_mut("bytes").unwrap().value = 8.0;
    let refused = snapshot::plan(&ru, scenario, &larger, &options).unwrap_err();
    assert!(refused.contains("--replace-workload"), "{refused}");
    let replace = Options {
        replace_workload: true,
        ..Options::default()
    };
    assert_eq!(
        snapshot::plan(&ru, scenario, &larger, &replace)
            .unwrap()
            .action,
        snapshot::Action::Replaced
    );
    // Маркер без записанной нагрузки читается, но не сверяется.
    let pages: Vec<PathBuf> = ["fixture.md", "fixture.en.md", "fixture.zh-CN.md"]
        .iter()
        .map(|name| docs.join(name))
        .collect();
    let current: Vec<String> = pages
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    for (path, text) in pages.iter().zip(&current) {
        let legacy = text
            .split('\n')
            .map(
                |line| match line.strip_prefix("<!-- benchmark-snapshot: ") {
                    Some(rest) => {
                        let mut meta: serde_json::Value =
                            serde_json::from_str(rest.strip_suffix(" -->").unwrap()).unwrap();
                        assert!(meta.as_object_mut().unwrap().remove("workload").is_some());
                        format!("<!-- benchmark-snapshot: {meta} -->")
                    }
                    None => line.to_owned(),
                },
            )
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, legacy).unwrap();
    }
    // Неизвестная нагрузка совпадающей не считается: и другая, и та же
    // нагрузка требуют явного подтверждения.
    for candidate in [&larger, &newer_same_workload(&newer)] {
        let refused = snapshot::plan(&ru, scenario, candidate, &options).unwrap_err();
        assert!(
            refused.contains("эталонная нагрузка не записана")
                && refused.contains("--replace-workload"),
            "{refused}"
        );
    }
    let replace = Options {
        replace_workload: true,
        ..Options::default()
    };
    let plan = snapshot::plan(&ru, scenario, &larger, &replace).unwrap();
    assert_eq!(plan.action, snapshot::Action::Replaced);
    assert!(
        plan.pages[0].block.contains("\"workload\":{"),
        "после подтверждённой замены маркер записывает нагрузку"
    );
    for (path, text) in pages.iter().zip(&current) {
        fs::write(path, text).unwrap();
    }
    // Неполная серия и изменённая рабочая копия не публикуются.
    let partial = result(&Spec {
        recorded_at: "2026-10-07T10:00:00Z",
        failed_runs: 1,
        outcome: OutcomeStatus::Partial,
        ..Spec::default()
    });
    assert!(
        snapshot::plan(&ru, scenario, &partial, &options)
            .unwrap_err()
            .contains("успешной серии")
    );
    let dirty = result(&Spec {
        recorded_at: "2026-10-07T10:00:00Z",
        dirty: true,
        ..Spec::default()
    });
    assert!(
        snapshot::plan(&ru, scenario, &dirty, &options)
            .unwrap_err()
            .contains("--allow-dirty")
    );
    // Другой сценарий или профиль добавляется рядом и прежний блок не трогает.
    let mut other_profile = result(&Spec {
        recorded_at: "2026-10-07T10:00:00Z",
        ..Spec::default()
    });
    other_profile.profile.id = "cold".into();
    let plan = snapshot::plan(&ru, scenario, &other_profile, &options).unwrap();
    assert_eq!(plan.action, snapshot::Action::Added);
    snapshot::apply(&plan).unwrap();
    let text = fs::read_to_string(&ru).unwrap();
    assert_eq!(text.matches("benchmark-snapshot: {").count(), 2);
    assert!(text.contains("2026-10-05"));
}

#[test]
fn cli_end_to_end_with_isolated_store() {
    let f = fixture();
    let dir = f.root.join("results");
    fs::create_dir_all(dir.join("run-1")).unwrap();
    fs::create_dir_all(dir.join("run-2")).unwrap();
    fs::write(
        dir.join("run-1/result.json"),
        serde_json::to_string_pretty(&result(&Spec::default())).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("run-1/summary.json"), "{\"ok\": true}").unwrap();
    fs::write(dir.join("run-1/result.rejected.json"), "{}").unwrap();
    let target = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.08, 0.081, 0.079],
        ..Spec::default()
    });
    fs::write(
        dir.join("run-2/result.json"),
        serde_json::to_string_pretty(&target).unwrap(),
    )
    .unwrap();

    // Ссылка на предка внутри обхода не повторяет обход дерева.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dir, dir.join("run-1/loop")).unwrap();

    let (code, out, err) = f.cli(&["import", dir.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.matches("принят").count(), 2, "{out}");
    assert_eq!(out.matches("уже есть").count(), 0, "{out}");
    let (code, out, _) = f.cli(&["import", dir.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(out.matches("уже есть").count(), 2, "{out}");

    let (code, out, _) = f.cli(&[
        "--json",
        "list",
        "--scenario",
        "fixture.cli",
        "--since",
        "2026-10-02",
    ]);
    assert_eq!(code, 0);
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(rows["results"].as_array().unwrap().len(), 1);
    let target_id = rows["results"][0]["id"].as_str().unwrap().to_owned();
    let (_, out, _) = f.cli(&["--json", "list", "--env", "cpu_model=Other"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (_, out, _) = f.cli(&["--json", "list", "--input", "dddd", "--outcome", "success"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Префиксы сравниваются буквально: `%` и `_` не шаблоны LIKE.
    for args in [["--commit", "%"], ["--input", "_"]] {
        let (_, out, _) = f.cli(&["--json", "list", args[0], args[1]]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "{args:?}"
        );
    }

    let (code, out, _) = f.cli(&["baseline", &target_id[..8]]);
    assert_eq!(code, 0, "{out}");
    let (code, out, _) = f.cli(&["report", &target_id[..8]]);
    assert_eq!(code, 0);
    assert!(
        out.contains("выбрана автоматически") && out.contains("−20 %"),
        "{out}"
    );
    let (ok, stderr) = check_pr_body(&out);
    assert!(ok, "{stderr}");
    // Подходящая база, отсечённая только `--commit`, называется с причиной.
    let (_, out, _) = f.cli(&["report", &target_id[..8], "--commit", "ccc"]);
    assert!(
        out.contains("с коммитом, начинающимся с «ccc»")
            && out.contains("коммит не начинается с «ccc»"),
        "{out}"
    );
    // Явная база не может совпадать с проверяемым результатом.
    let (code, _, err) = f.cli(&["report", &target_id[..8], "--baseline", &target_id[..8]]);
    assert_eq!(code, 2);
    assert!(err.contains("совпадает с проверяемым"), "{err}");
    let (code, out, _) = f.cli(&["report", &target_id[..8], "--first"]);
    assert_eq!(code, 0);
    assert!(out.contains("новая функциональность"));

    let (code, out, _) = f.cli(&["verify"]);
    assert_eq!(code, 0, "{out}");
    // verify не создаёт хранилище по опечатке в пути.
    let missing = f.root.join("missing-store");
    let (code, _, err) = f.cli(&["--store", missing.to_str().unwrap(), "verify"]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("нет"), "{err}");
    assert!(!missing.exists());
    let (code, _, err) = f.cli(&["--store", missing.to_str().unwrap(), "reindex"]);
    assert_eq!(
        code, 3,
        "недоступное хранилище — код 3, как у verify: {err}"
    );
    let (code, _, _) = f.cli(&["show", "zz"]);
    assert_eq!(code, 2);

    // Каталог без результатов — код 2 и ровно один JSON-документ в stdout.
    let empty = f.root.join("empty-results");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("notes.txt"), "не результат").unwrap();
    let (code, out, _) = f.cli(&["--json", "import", empty.to_str().unwrap()]);
    assert_eq!(code, 2);
    let mut documents = serde_json::Deserializer::from_str(&out).into_iter::<serde_json::Value>();
    let only = documents.next().unwrap().unwrap();
    assert_eq!(only["ok"], false, "{out}");
    assert!(documents.next().is_none(), "{out}");

    // Отвергнутый документ — код 4, и в хранилище ничего не добавлено.
    let bad = f.root.join("bad.json");
    fs::write(&bad, "{\"schema_version\": 1}").unwrap();
    let (code, _, err) = f.cli(&["import", bad.to_str().unwrap()]);
    assert_eq!(code, 4, "{err}");

    // Хранилище внутри рабочей копии репозитория отвергается.
    let output = isolated(BIN)
        .arg("--catalog")
        .arg(f.catalog_dir())
        .args([
            "--store",
            f.root.join("workspace/history").to_str().unwrap(),
            "status",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!f.root.join("workspace/history").exists());

    let resolved = location::resolve_with(None, &|name| {
        (name == "HOME").then(|| "/home/someone".into())
    })
    .unwrap();
    assert!(resolved.root.ends_with(location::DATA_SUBDIR));
    assert!(index::bound("2026-13-01", false).is_err());
}

#[test]
fn baseline_requires_proven_different_code() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let mut store = f.open();
    let target_spec = Spec {
        recorded_at: "2026-10-09T10:00:00Z",
        commit: 'f',
        ..Spec::default()
    };
    let target_id = f.import(&mut store, &result(&target_spec));
    // Тот же коммит без записанного дерева: повтор измерения, а не «до».
    let same_commit = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-01T10:00:00Z",
            commit: 'f',
            tree: false,
            ..Spec::default()
        }),
    );
    // Дерево неизвестно, а состояние рабочей копии не записано: отличие не доказано.
    let unknown = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-02T10:00:00Z",
            commit: 'a',
            tree: false,
            state: false,
            ..Spec::default()
        }),
    );
    // Разные коммиты, оба без изменений, дерево записано не в обоих: отличие доказано с оговоркой.
    let other_commit = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-03T10:00:00Z",
            commit: 'b',
            tree: false,
            ..Spec::default()
        }),
    );
    // Тот же коммит, изменённая рабочая копия, деревьев нет: тоже не доказано.
    let dirty_same = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-04T10:00:00Z",
            commit: 'f',
            tree: false,
            dirty: true,
            ..Spec::default()
        }),
    );
    // Одинаковые деревья — повтор независимо от коммитов.
    let same_tree = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-05T10:00:00Z",
            commit: 'f',
            ..Spec::default()
        }),
    );
    let ids = [
        same_commit.clone(),
        unknown.clone(),
        other_commit.clone(),
        dirty_same.clone(),
        same_tree.clone(),
    ];
    let list = analysis::candidates(
        scenario,
        &target_id,
        &result(&target_spec),
        &pool(&store, &ids),
        &no_git,
    );
    let find = |id: &str| list.iter().find(|c| c.id == id).unwrap();
    let says = |id: &str, text: &str| find(id).reasons.iter().any(|r| r.contains(text));
    assert!(says(&same_commit, "тот же коммит без изменений"));
    assert!(says(&unknown, "отличие исходного кода не доказано"));
    assert!(says(&dirty_same, "отличие исходного кода не доказано"));
    assert!(says(&same_tree, "тот же исходный код"));
    assert!(find(&other_commit).eligible);
    assert!(
        find(&other_commit)
            .notes
            .iter()
            .any(|n| n.contains("только разными коммитами"))
    );
    assert_eq!(analysis::choose(&list, None).unwrap().id, other_commit);
    drop(store);

    // CLI называет причины, а явная база вне правила видна в отчёте.
    let (code, out, _) = f.cli(&["baseline", &target_id[..12]]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("не подходит: тот же коммит без изменений"),
        "{out}"
    );
    assert!(
        out.contains("не подходит: отличие исходного кода не доказано"),
        "{out}"
    );
    assert!(out.contains(&format!("база: {other_commit}")), "{out}");
    let (code, out, _) = f.cli(&["report", &target_id[..12], "--baseline", &same_commit[..12]]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("указана явно")
            && out.contains("не соответствует правилу автоматического выбора — тот же коммит"),
        "{out}"
    );
    // База, равная проверяемому результату, отвергается и в report, и в compare.
    let (code, _, err) = f.cli(&["report", &target_id[..12], "--baseline", &target_id[..12]]);
    assert_eq!(code, 2, "{err}");
    let (code, _, err) = f.cli(&["compare", &target_id[..12], &target_id[..12]]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("совпадает с проверяемым"), "{err}");
}

/// Коммит синтетического репозитория; `None`, если Git недоступен.
fn commit_all(repo: &Path, message: &str) -> Option<String> {
    let git = |args: &[&str]| {
        isolated("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", message])?;
    git(&["rev-parse", "HEAD"]).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

#[test]
fn baseline_distinguishes_documentation_only_changes_from_code_changes() {
    let f = fixture();
    let repo = f.root.join("workspace");
    let initialized = isolated("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .is_ok_and(|s| s.success());
    // Как python3 и Hyperfine: локально без Git проверка пропускается с
    // сообщением, в CI его отсутствие — ошибка.
    if !initialized {
        assert!(
            std::env::var_os("CI").is_none(),
            "в CI нужен Git: без него состав изменений не проверяется"
        );
        eprintln!("ПРОПУСК: Git недоступен, проверка состава изменений не выполнена");
        return;
    }
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.join("README.md"), "# Модуль\n").unwrap();
    let Some(original) = commit_all(&repo, "исходная реализация") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "в CI нужен рабочий git commit: без него состав изменений не проверяется"
        );
        eprintln!("ПРОПУСК: git commit недоступен, проверка состава изменений не выполнена");
        return;
    };
    fs::write(repo.join("README.md"), "# Модуль\n\nОписание.\n").unwrap();
    fs::write(repo.join("docs/guide.md"), "Руководство.\n").unwrap();
    let docs_only = commit_all(&repo, "только документация").unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() { let _ = 1; }\n").unwrap();
    let optimized = commit_all(&repo, "оптимизация").unwrap();

    let at = |commit: &str, tree: char, recorded_at: &'static str| {
        let mut r = result(&Spec {
            recorded_at,
            commit: tree,
            ..Spec::default()
        });
        r.provenance.code.git_commit = Some(commit.to_owned());
        r
    };
    let mut store = f.open();
    let before = f.import(&mut store, &at(&original, 'a', "2026-10-01T10:00:00Z"));
    let after_docs_result = at(&docs_only, 'b', "2026-10-02T10:00:00Z");
    let after_docs = f.import(&mut store, &after_docs_result);
    let after_code_result = at(&optimized, 'c', "2026-10-03T10:00:00Z");
    let after_code = f.import(&mut store, &after_code_result);
    // Коммит, которого нет в этом клоне: происхождение неизвестно. В хранилище
    // не принимается, чтобы не стать базой в проверках CLI ниже.
    let foreign = "foreign".to_owned();
    let foreign_result = at(&"9".repeat(40), '9', "2026-10-01T12:00:00Z");
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let git = |a: &str, b: &str| -> Result<Vec<String>, String> {
        let out = isolated("git")
            .args(["diff", "--name-only", "-z", "--no-renames", a, b])
            .current_dir(&repo)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    };

    // Изменена только документация: разные деревья, но не состояние «до».
    let list = analysis::candidates(
        scenario,
        &after_docs,
        &after_docs_result,
        &pool(&store, std::slice::from_ref(&before)),
        &git,
    );
    assert!(!list[0].eligible, "{:?}", list[0]);
    assert_eq!(
        list[0].code,
        analysis::CodeChange::DocumentationOnly { changed: 2 }
    );
    assert!(
        list[0]
            .reasons
            .iter()
            .any(|r| r.contains("только документация")),
        "{:?}",
        list[0]
    );
    // Настоящее изменение кода: база выбирается, но с оговоркой о составе.
    let list = analysis::candidates(
        scenario,
        &after_code,
        &after_code_result,
        &[
            pool(&store, &[before.clone(), after_docs.clone()]),
            vec![(foreign.clone(), foreign_result)],
        ]
        .concat(),
        &git,
    );
    let find = |id: &str| list.iter().find(|c| c.id == id).unwrap();
    assert_eq!(
        find(&after_docs).code,
        analysis::CodeChange::Files {
            changed: 1,
            documentation: 0
        }
    );
    assert!(find(&after_docs).eligible && find(&before).eligible);
    assert!(
        find(&after_docs)
            .notes
            .iter()
            .any(|n| n.contains("записанные данные не доказывают"))
    );
    // Неизвестный коммит: различие только состояния репозитория, с оговоркой.
    assert!(matches!(
        find(&foreign).code,
        analysis::CodeChange::Unverified { .. }
    ));
    assert!(
        find(&foreign)
            .notes
            .iter()
            .any(|n| n.contains("состав изменений не проверен")),
        "{:?}",
        find(&foreign)
    );
    assert_eq!(analysis::choose(&list, None).unwrap().id, after_docs);
    drop(store);

    // CLI сверяет коммиты по Git репозитория каталога.
    let (code, out, _) = f.cli(&["baseline", &after_docs[..12]]);
    assert_eq!(code, 1, "{out}");
    // Унаследованный GIT_DIR (запуск из Git-хука) не уводит сверку в чужой
    // репозиторий.
    let inherited = isolated(BIN)
        .arg("--catalog")
        .arg(f.catalog_dir())
        .args(["baseline", &after_docs[..12]])
        .env("BENCHMARK_HISTORY_DIR", f.store())
        .env_remove("XDG_DATA_HOME")
        .env("GIT_DIR", f.root.join("elsewhere.git"))
        .env("GIT_OBJECT_DIRECTORY", f.root.join("elsewhere.git/objects"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&inherited.stdout), out);
    assert!(
        out.contains(
            "не подходит: между коммитами изменена только документация (файлов Markdown: 2)"
        ),
        "{out}"
    );
    assert!(
        out.contains("пригодной базы нет") && out.contains("оговорка:"),
        "{out}"
    );
    let (code, out, _) = f.cli(&["--json", "baseline", &after_code[..12]]);
    assert_eq!(code, 0, "{out}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["chosen"], after_docs.as_str());
    let chosen = json["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == after_docs.as_str())
        .unwrap();
    assert_eq!(chosen["code"]["kind"], "files", "{out}");
    // Отчёт без явной базы не находит её и вывода об изменении не делает.
    let (code, out, _) = f.cli(&["report", &after_docs[..12]]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("База: не найдена") && out.contains("только документация"),
        "{out}"
    );
    assert!(!out.contains("улучшение сверх разброса"), "{out}");
    // Осознанная явная база остаётся возможной, ограничение видно в отчёте,
    // а сопоставимость по-прежнему решает контракт.
    let (code, out, _) = f.cli(&["report", &after_docs[..12], "--baseline", &before[..12]]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("указана явно")
            && out.contains("не соответствует правилу автоматического выбора — между коммитами изменена только документация")
            && out.contains("сопоставимы по контракту `benchmark-contract`")
            && out.contains(analysis::IMPLEMENTATION_CAVEAT),
        "{out}"
    );
    let (code, out, _) = f.cli(&["compare", &before[..12], &after_docs[..12]]);
    assert_eq!(code, 0, "{out}");
}

/// Каталог, отмеченный как рабочая копия Git: настоящий `git init`, если Git
/// доступен, иначе каталог `.git` — проверка опирается только на него.
fn git_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let initialized = isolated("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .is_ok_and(|s| s.success());
    if !initialized {
        fs::create_dir_all(path.join(".git")).unwrap();
    }
}

#[test]
fn store_inside_git_is_refused_from_any_directory() {
    let f = fixture();
    let repo = f.root.join("repo");
    git_repository(&repo);
    fs::create_dir_all(repo.join("sub")).unwrap();
    let outside = f.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let run = |args: &[&str], env: Option<&Path>| {
        let mut command = isolated(BIN);
        command
            .args(args)
            .current_dir(&outside)
            .env_remove("BENCHMARK_HISTORY_DIR")
            .env("XDG_DATA_HOME", f.root.join("xdg"));
        if let Some(path) = env {
            command.env("BENCHMARK_HISTORY_DIR", path);
        }
        let output = command.output().unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let nested = repo.join("sub/./new/history");
    let nested = nested.to_str().unwrap();
    let missing_catalog = f.root.join("no-catalog");
    fs::create_dir_all(&missing_catalog).unwrap();
    // Каталога сценариев нет выше текущего каталога, либо --catalog некорректен.
    for args in [
        vec!["--store", nested, "list"],
        vec!["--store", nested, "status"],
        vec!["--store", nested, "reindex"],
        vec![
            "--store",
            nested,
            "--catalog",
            missing_catalog.to_str().unwrap(),
            "list",
        ],
    ] {
        let (code, err) = run(&args, None);
        assert_eq!(code, 2, "{args:?}: {err}");
        assert!(err.contains("внутри рабочей копии Git"), "{err}");
    }
    let (code, err) = run(&["list"], Some(Path::new(nested)));
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("внутри рабочей копии Git"), "{err}");
    // Корень репозитория и путь через символическую ссылку тоже внутри.
    let (code, _) = run(&["--store", repo.to_str().unwrap(), "list"], None);
    assert_eq!(code, 2);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(repo.join("sub"), f.root.join("link")).unwrap();
        let via_link = f.root.join("link/history");
        let (code, _) = run(&["--store", via_link.to_str().unwrap(), "list"], None);
        assert_eq!(code, 2);
    }
    assert!(
        !repo.join("sub/new").exists(),
        "после отказа ничего не создано"
    );
    let untracked = isolated("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&repo)
        .output();
    if let Ok(status) = untracked
        && status.status.success()
    {
        assert_eq!(
            String::from_utf8_lossy(&status.stdout),
            "",
            "в git status нет данных истории"
        );
    }
    // Абсолютный путь и XDG вне Git работают из того же каталога.
    let elsewhere = f.root.join("elsewhere/history");
    let (code, err) = run(&["--store", elsewhere.to_str().unwrap(), "list"], None);
    assert_eq!(code, 0, "{err}");
    assert!(elsewhere.join("store.json").is_file());
    let (code, err) = run(&["list"], None);
    assert_eq!(code, 0, "{err}");
    assert!(
        f.root
            .join("xdg/anki-deck-toolkit/benchmark-history/store.json")
            .is_file()
    );
    // Резервная копия в рабочую копию Git тоже не пишется.
    let (code, err) = run(
        &[
            "--store",
            elsewhere.to_str().unwrap(),
            "backup",
            repo.join("sub/backup").to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(code, 2, "{err}");
    assert!(!repo.join("sub/backup").exists());
}

#[test]
fn restore_checks_backup_inventory() {
    let f = fixture();
    let mut store = f.open();
    let a = f.import(&mut store, &result(&Spec::default()));
    let b = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-02T10:00:00Z",
            commit: 'b',
            ..Spec::default()
        }),
    );
    let backup = f.root.join("backup");
    assert_eq!(store.backup(&backup).unwrap(), 2);
    let copy = |name: &str| {
        let target = f.root.join(name);
        copy_tree(&backup, &target);
        target
    };

    // Нормальный перенос и идемпотентный повтор.
    let mut other = Store::open(&f.root.join("other")).unwrap();
    let restored = other.restore(&backup).unwrap();
    assert!(restored.complete && restored.expected == 2 && restored.added.len() == 2);
    let again = other.restore(&backup).unwrap();
    assert!(again.complete);
    assert_eq!((again.added.len(), again.present.len()), (0, 2));
    assert_eq!(other.index().count().unwrap(), 2);

    // Повреждённая локальная копия заменяется проверенной из резервной, а
    // прежний файл откладывается; остальные записи обрабатываются.
    let damaged = f.root.join("other").join(archive::relative_path(&a));
    fs::write(&damaged, "{испорчено").unwrap();
    let repaired = other.restore(&backup).unwrap();
    assert!(repaired.complete, "{:?}", repaired.problems);
    assert_eq!(repaired.replaced, vec![a.clone()]);
    assert_eq!(repaired.present, vec![b.clone()]);
    assert_eq!(
        archive::content_id(&fs::read_to_string(&damaged).unwrap()).unwrap(),
        a
    );
    let aside: Vec<_> = fs::read_dir(f.root.join("other").join(archive::CORRUPT_DIR))
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(aside, vec!["{испорчено".to_owned()]);
    assert!(other.verify().unwrap().problems.is_empty());

    // Пропавшая запись: исправная принимается, перенос не полный.
    let lost = copy("lost");
    fs::remove_file(lost.join(archive::relative_path(&b))).unwrap();
    let mut third = Store::open(&f.root.join("third")).unwrap();
    let restored = third.restore(&lost).unwrap();
    assert!(!restored.complete);
    assert_eq!(restored.added, vec![a.clone()]);
    assert_eq!(restored.missing, vec![b.clone()]);
    drop(third);
    let cli = |source: &Path, store: &str| {
        let output = isolated(BIN)
            .args(["restore", source.to_str().unwrap()])
            .env("BENCHMARK_HISTORY_DIR", f.root.join(store))
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };
    let (code, out) = cli(&lost, "cli-lost");
    assert_eq!(code, 4, "{out}");
    assert!(
        out.contains("перенесена не полностью") && out.contains("файла в копии нет"),
        "{out}"
    );

    // Повреждённая запись не принимается, и перенос не полный.
    let damaged = copy("damaged");
    let path = damaged.join(archive::relative_path(&a));
    let text = fs::read_to_string(&path).unwrap().replace("0.098", "0.001");
    fs::write(&path, text).unwrap();
    let restored = Store::open(&f.root.join("fourth"))
        .unwrap()
        .restore(&damaged)
        .unwrap();
    assert!(!restored.complete);
    assert_eq!(restored.added, vec![b.clone()]);

    // Запись, которой нет в инвентаре, не принимается.
    let shrunk = copy("shrunk");
    let manifest_path = shrunk.join("backup.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["results"] = serde_json::json!([a.clone()]);
    fs::write(&manifest_path, manifest.to_string()).unwrap();
    let restored = Store::open(&f.root.join("fifth"))
        .unwrap()
        .restore(&shrunk)
        .unwrap();
    assert!(!restored.complete);
    assert_eq!(restored.added, vec![a.clone()]);
    assert!(
        restored
            .problems
            .iter()
            .any(|p| p.item == b && p.reason.contains("не перечислена"))
    );

    // Без манифеста, с другой версией, повтором или мусором — это не резервная копия.
    let broken = copy("broken");
    let manifest_path = broken.join("backup.json");
    let original = fs::read_to_string(&manifest_path).unwrap();
    let variants = [
        None,
        Some(original.replace("\"archive_version\": 1", "\"archive_version\": 2")),
        Some(original.replace(&b, &a)),
        Some("{\"format\": ".into()),
    ];
    for variant in variants {
        match &variant {
            Some(text) => fs::write(&manifest_path, text).unwrap(),
            None => fs::remove_file(&manifest_path).unwrap(),
        }
        let mut target = Store::open(&f.root.join("sixth")).unwrap();
        assert!(
            matches!(target.restore(&broken), Err(Error::Usage(_))),
            "{variant:?}"
        );
        assert_eq!(target.index().count().unwrap(), 0, "ничего не принято");
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn index_projection_is_checked_and_rebuilt_from_archive() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let mut store = f.open();
    let base = f.import(&mut store, &result(&Spec::default()));
    let target_spec = Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        ..Spec::default()
    };
    let target = f.import(&mut store, &result(&target_spec));
    drop(store);
    let archive_bytes = || -> Vec<Vec<u8>> {
        [&base, &target]
            .iter()
            .map(|id| fs::read(f.store().join(archive::relative_path(id))).unwrap())
            .collect()
    };
    let before = archive_bytes();
    let index_path = f.store().join("index.sqlite");

    // quick_check = ok и текущая версия, но таблицы results нет.
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE results;")
            .unwrap();
        let check: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!((check.as_str(), version), ("ok", index::SCHEMA_VERSION));
    }
    let store = f.open();
    assert_eq!(store.index().count().unwrap(), 2);
    assert!(
        store
            .opened_sync()
            .notes
            .iter()
            .any(|n| n.contains("не совпадает с ожидаемой") && n.contains("results")),
        "{:?}",
        store.opened_sync().notes
    );
    drop(store);

    // Подмена scenario_id у существующего ID: строка перестраивается из архива,
    // и база снова находится в серии.
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute(
            "UPDATE results SET scenario_id = 'fixture.other' WHERE id = ?1",
            [&base],
        )
        .unwrap();
    }
    let store = f.open();
    assert_eq!(store.opened_sync().repaired, vec![base.clone()]);
    assert_eq!(
        store.index().series("fixture.cli", "warm").unwrap().len(),
        2
    );
    let ids = store.index().series("fixture.cli", "warm").unwrap();
    let list = analysis::candidates(
        scenario,
        &target,
        &result(&target_spec),
        &pool(&store, &ids),
        &no_git,
    );
    assert_eq!(analysis::choose(&list, None).unwrap().id, base);
    assert!(store.verify().unwrap().index_problems.is_empty());

    // Порча строки при открытом хранилище видна полной сверке verify.
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute(
            "UPDATE metrics SET mean = 1.0 WHERE result_id = ?1",
            [&target],
        )
        .unwrap();
    }
    let report = store.verify().unwrap();
    assert!(report.problems.is_empty(), "архив цел");
    assert_eq!(report.index_problems.len(), 1);
    assert_eq!(report.index_problems[0].item, target);
    drop(store);
    // Открытие перестраивает строку до сверки, но verify не выдаёт это за
    // целый индекс: исправление видно, код 1; следующая проверка — чистая.
    let (code, out, err) = f.cli(&["verify"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("индекс исправлен при открытии"), "{out}");
    assert!(!out.contains("индекс совпадает с архивом"), "{out}");
    assert!(err.contains(&target), "{err}");
    let (code, out, _) = f.cli(&["verify"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("индекс совпадает с архивом"), "{out}");
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute(
            "UPDATE metrics SET mean = 1.0 WHERE result_id = ?1",
            [&target],
        )
        .unwrap();
    }
    let (code, out, _) = f.cli(&["--json", "verify"]);
    assert_eq!(code, 1, "{out}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["opened_sync"]["repaired"][0], target.as_str());

    // Устаревшая схема мигрирует, более новая — отказ с поддерживаемым reindex.
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute_batch("PRAGMA user_version = 1;").unwrap();
    }
    let store = f.open();
    assert!(
        store
            .opened_sync()
            .notes
            .iter()
            .any(|n| n.contains("устарела"))
    );
    assert_eq!(store.index().count().unwrap(), 2);
    drop(store);
    {
        let conn = rusqlite_connection(&index_path);
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    }
    assert!(matches!(Store::open(&f.store()), Err(Error::Store(m)) if m.contains("более новой")));
    let (code, out, _) = f.cli(&["reindex"]);
    assert_eq!(code, 0, "{out}");
    assert!(f.open().verify().unwrap().index_problems.is_empty());
    assert_eq!(archive_bytes(), before, "архив не переписан");
}

#[test]
fn snapshot_reference_hardware_is_separate_from_comparability() {
    let f = fixture();
    let catalog = f.catalog();
    // Процессор записывается, но не входит в условие сопоставимости.
    let mut scenario = catalog.scenario("fixture.cli").unwrap().scenario.clone();
    scenario
        .environment
        .comparable
        .retain(|name| name != "cpu_model");
    let docs = f.root.join("workspace/docs");
    let ru = docs.join("fixture.md");
    fs::write(&ru, PAGE_RU).unwrap();
    let sha = snapshot::git_blob_sha(PAGE_RU.as_bytes());
    fs::write(docs.join("fixture.en.md"), page("en", &sha)).unwrap();
    fs::write(docs.join("fixture.zh-CN.md"), page("zh", &sha)).unwrap();
    let first = result(&Spec::default());
    let options = Options {
        history: vec![snapshot::Recorded::of(&scenario, &first)],
        ..Options::default()
    };
    snapshot::apply(&snapshot::plan(&ru, &scenario, &first, &options).unwrap()).unwrap();
    let published = fs::read_to_string(&ru).unwrap();
    assert!(
        published.contains("| `memory_total_bytes` ★ |"),
        "{published}"
    );

    let later = |spec: Spec| {
        result(&Spec {
            recorded_at: "2026-10-03T10:00:00Z",
            commit: 'c',
            ..spec
        })
    };
    let cases = [
        (
            "другой процессор",
            later(Spec {
                cpu: "Other CPU",
                ..Spec::default()
            }),
        ),
        (
            "другая память",
            later(Spec {
                ram: Some("8589934592"),
                ..Spec::default()
            }),
        ),
        (
            "WSL2 вместо Linux",
            later(Spec {
                virtualization: "wsl2",
                ..Spec::default()
            }),
        ),
    ];
    for (name, candidate) in &cases {
        // Для сопоставимости процессор и память не важны…
        if *name != "WSL2 вместо Linux" {
            assert!(
                benchmark_contract::comparability::compare(&scenario, &first, candidate).comparable,
                "{name}"
            );
        }
        // …но публичный эталон другой машины молча не заменяет.
        let refused = snapshot::plan(&ru, &scenario, candidate, &options).unwrap_err();
        assert!(
            refused.contains("--replace-environment"),
            "{name}: {refused}"
        );
        assert_eq!(fs::read_to_string(&ru).unwrap(), published, "{name}");
    }
    // Неизвестный обязательный факт совпадающим не считается.
    let unknown = later(Spec {
        ram: None,
        ..Spec::default()
    });
    let refused = snapshot::plan(&ru, &scenario, &unknown, &options).unwrap_err();
    assert!(
        refused.contains("memory_total_bytes неизвестен"),
        "{refused}"
    );
    // Прежний маркер без аппаратного факта: совпадение не доказано.
    let legacy = published.replace(",\"memory_total_bytes\":\"17179869184\"", "");
    assert_ne!(legacy, published);
    fs::write(&ru, &legacy).unwrap();
    let same = later(Spec::default());
    let refused = snapshot::plan(&ru, &scenario, &same, &options).unwrap_err();
    assert!(
        refused.contains("не записаны memory_total_bytes"),
        "{refused}"
    );
    fs::write(&ru, &published).unwrap();
    // Та же машина обновляет снимок, явная замена эталона разрешена.
    assert_eq!(
        snapshot::plan(&ru, &scenario, &same, &options)
            .unwrap()
            .action,
        snapshot::Action::Replaced
    );
    let replace = Options {
        replace_environment: true,
        ..options.clone()
    };
    let plan = snapshot::plan(&ru, &scenario, &cases[0].1, &replace).unwrap();
    snapshot::apply(&plan).unwrap();
    assert!(fs::read_to_string(&ru).unwrap().contains("Other CPU"));
}

#[test]
fn snapshot_requires_proven_workload_identity_not_equal_size() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let docs = f.root.join("workspace/docs");
    let ru = docs.join("fixture.md");
    fs::write(&ru, PAGE_RU).unwrap();
    let sha = snapshot::git_blob_sha(PAGE_RU.as_bytes());
    fs::write(docs.join("fixture.en.md"), page("en", &sha)).unwrap();
    fs::write(docs.join("fixture.zh-CN.md"), page("zh", &sha)).unwrap();
    let pages: Vec<PathBuf> = ["fixture.md", "fixture.en.md", "fixture.zh-CN.md"]
        .iter()
        .map(|name| docs.join(name))
        .collect();
    let read_all = || -> Vec<String> {
        pages
            .iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect()
    };

    let first = result(&Spec::default());
    let history = vec![snapshot::Recorded::of(scenario, &first)];
    let with_history = Options {
        history: history.clone(),
        ..Options::default()
    };
    let plan = snapshot::plan(&ru, scenario, &first, &with_history).unwrap();
    assert_eq!(plan.workload, snapshot::WorkloadBasis::New);
    snapshot::apply(&plan).unwrap();
    let published = read_all();

    let later = |data: char| {
        result(&Spec {
            recorded_at: "2026-10-04T10:00:00Z",
            commit: 'b',
            data,
            ..Spec::default()
        })
    };
    // Тот же объём (4 байта), но другое содержимое: идентичность входа другая.
    let changed = later('e');
    assert_eq!(
        snapshot::reference_workload(&changed),
        snapshot::reference_workload(&first)
    );
    let refused = snapshot::plan(&ru, scenario, &changed, &with_history).unwrap_err();
    assert!(
        refused.contains("с другим содержимым")
            && refused.contains("data")
            && refused.contains("--replace-workload"),
        "{refused}"
    );
    assert!(
        !refused.contains(&"d".repeat(64)) && !refused.contains(&"e".repeat(64)),
        "сообщение не раскрывает идентичности: {refused}"
    );
    // Та же нагрузка, но результата прежнего снимка в истории нет (другая
    // машина, очищенная история): одинаковый объём тождества не доказывает.
    let same = later('d');
    let refused = snapshot::plan(&ru, scenario, &same, &Options::default()).unwrap_err();
    assert!(
        refused.contains("в локальной истории нет") && refused.contains("--replace-workload"),
        "{refused}"
    );
    // Тот же момент записи у результата с другой нагрузкой не доказательство.
    let mut impostor = first.clone();
    impostor.inputs[1].identity.value = "f".repeat(64);
    let mixed = Options {
        history: vec![
            snapshot::Recorded::of(scenario, &first),
            snapshot::Recorded::of(scenario, &impostor),
        ],
        ..Options::default()
    };
    assert!(snapshot::plan(&ru, scenario, &same, &mixed).is_err());
    // Тот же момент и нагрузка, но другое эталонное окружение: этот результат
    // не мог дать прежний снимок и тождества не доказывает.
    let mut elsewhere = first.clone();
    elsewhere.environment.insert(
        snapshot::REFERENCE_FACTS[0].as_str().to_owned(),
        Fact::Known {
            value: "elsewhere".into(),
        },
    );
    let foreign = Options {
        history: vec![snapshot::Recorded::of(scenario, &elsewhere)],
        ..Options::default()
    };
    let refused = snapshot::plan(&ru, scenario, &same, &foreign).unwrap_err();
    assert!(refused.contains("в локальной истории нет"), "{refused}");
    assert_eq!(read_all(), published, "отказы ничего не записали");
    // Подлинно та же нагрузка: доказательство из локальной истории.
    let plan = snapshot::plan(&ru, scenario, &same, &with_history).unwrap();
    assert_eq!(
        (plan.action, plan.workload),
        (
            snapshot::Action::Replaced,
            snapshot::WorkloadBasis::ProvenByHistory
        )
    );
    // Явное подтверждение смены: видно в плане и обновляет все три языка.
    let confirmed = Options {
        replace_workload: true,
        ..with_history.clone()
    };
    // Блока нет только на последней странице: её добавление не скрывает
    // замену и её основание на остальных.
    fs::write(&pages[2], page("zh", &sha)).unwrap();
    let plan = snapshot::plan(&ru, scenario, &changed, &confirmed).unwrap();
    assert_eq!(
        (plan.action, plan.workload),
        (
            snapshot::Action::Replaced,
            snapshot::WorkloadBasis::ConfirmedReplacement
        )
    );
    fs::write(&pages[2], &published[2]).unwrap();
    let plan = snapshot::plan(&ru, scenario, &changed, &confirmed).unwrap();
    assert_eq!(plan.workload, snapshot::WorkloadBasis::ConfirmedReplacement);
    snapshot::apply(&plan).unwrap();
    let updated = read_all();
    let ru_sha = snapshot::git_blob_sha(updated[0].as_bytes());
    for (n, text) in updated.iter().enumerate() {
        assert!(text.contains("2026-10-04"), "{n}: {text}");
        assert_eq!(text.matches("benchmark-snapshot: {").count(), 1);
        for forbidden in [
            "d".repeat(64),
            "e".repeat(64),
            "b".repeat(40),
            "b".repeat(64),
        ] {
            assert!(!text.contains(&forbidden), "{n}: в странице {forbidden}");
        }
        if n > 0 {
            assert!(text.contains(&ru_sha), "{n}: отметка перевода обновлена");
        }
    }
    // CLI: доказательство берётся из локальной истории пользователя.
    for (path, text) in pages.iter().zip(&published) {
        fs::write(path, text).unwrap();
    }
    let mut store = f.open();
    let first_id = f.import(&mut store, &first);
    let same_id = f.import(&mut store, &same);
    let changed_id = f.import(
        &mut store,
        &result(&Spec {
            recorded_at: "2026-10-04T11:00:00Z",
            commit: 'b',
            data: 'e',
            ..Spec::default()
        }),
    );
    drop(store);
    assert_ne!(first_id, same_id);
    let page_arg = ru.to_str().unwrap();
    let (code, out, err) = f.cli(&["snapshot", &same_id[..12], "--page", page_arg]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("нагрузка: тождество нагрузки доказано"),
        "{out}"
    );
    let (code, out, err) = f.cli(&["snapshot", &changed_id[..12], "--page", page_arg]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("с другим содержимым"), "{err}");
    let (code, out, err) = f.cli(&[
        "--json",
        "snapshot",
        &changed_id[..12],
        "--page",
        page_arg,
        "--replace-workload",
    ]);
    assert_eq!(code, 0, "{out}{err}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["plan"]["workload"], "confirmed-replacement", "{out}");
    assert_eq!(read_all(), published, "без --apply ничего не записано");
}

/// Страницы снимка модуля `stem` на трёх языках с якорем: RU, EN, zh-CN.
fn snapshot_pages(f: &Fixture, stem: &str) -> Vec<PathBuf> {
    let docs = f.root.join("workspace/docs");
    let sha = snapshot::git_blob_sha(PAGE_RU.as_bytes());
    let pages: Vec<PathBuf> = ["md", "en.md", "zh-CN.md"]
        .iter()
        .map(|ext| docs.join(format!("{stem}.{ext}")))
        .collect();
    fs::write(&pages[0], PAGE_RU).unwrap();
    fs::write(&pages[1], page("en", &sha)).unwrap();
    fs::write(&pages[2], page("zh", &sha)).unwrap();
    pages
}

#[test]
fn snapshot_never_takes_the_published_result_as_proof_of_the_previous_one() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let pages = snapshot_pages(&f, "fixture");
    let ru = &pages[0];
    let read_all = || -> Vec<String> {
        pages
            .iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect()
    };
    let forbidden = [
        "d".repeat(64),
        "e".repeat(64),
        "b".repeat(40),
        "c".repeat(40),
    ];
    let leaks = |text: &str| forbidden.iter().any(|f| text.contains(f.as_str()));

    let first = result(&Spec::default());
    let plan = snapshot::plan(ru, scenario, &first, &Options::default()).unwrap();
    snapshot::apply(&plan).unwrap();
    let published = read_all();

    // Результат прежнего снимка из истории удалён; новый записан в ту же
    // секунду, в том же окружении и на нагрузке того же объёма, но с другим
    // содержимым. Сам он происхождение прежнего снимка не доказывает.
    let successor = result(&Spec {
        commit: 'b',
        data: 'e',
        wall: vec![0.05, 0.05],
        ..Spec::default()
    });
    assert_eq!(
        successor.provenance.recorded_at,
        first.provenance.recorded_at
    );
    assert_eq!(
        snapshot::reference_workload(&successor),
        snapshot::reference_workload(&first)
    );
    let only_new = Options {
        history: vec![snapshot::Recorded::of(scenario, &successor)],
        ..Options::default()
    };
    let refused = snapshot::plan(ru, scenario, &successor, &only_new).unwrap_err();
    assert!(
        refused.contains("в локальной истории нет") && refused.contains("--replace-workload"),
        "{refused}"
    );
    assert!(!leaks(&refused), "{refused}");

    // Тот же видимый блок, другая нагрузка: совпадение текста не обходит
    // проверку тождества — ни при прежнем результате в истории, ни без него.
    let mut twin = first.clone();
    twin.inputs[1].identity.value = "e".repeat(64);
    twin.provenance.code.git_commit = Some("b".repeat(40));
    let with_origin = Options {
        history: vec![
            snapshot::Recorded::of(scenario, &first),
            snapshot::Recorded::of(scenario, &twin),
        ],
        ..Options::default()
    };
    let refused = snapshot::plan(ru, scenario, &twin, &with_origin).unwrap_err();
    assert!(
        refused.contains("с другим содержимым") && refused.contains("--replace-workload"),
        "{refused}"
    );
    assert!(!leaks(&refused), "{refused}");
    let without_origin = Options {
        history: vec![snapshot::Recorded::of(scenario, &twin)],
        ..Options::default()
    };
    let refused = snapshot::plan(ru, scenario, &twin, &without_origin).unwrap_err();
    assert!(refused.contains("сам публикуемый результат"), "{refused}");

    // Повтор уже опубликованного результата: маркер не хранит идентичностей,
    // и без другого результата истории происхождение блока не доказано.
    let refused = snapshot::plan(
        ru,
        scenario,
        &first,
        &Options {
            history: vec![snapshot::Recorded::of(scenario, &first)],
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(refused.contains("сам публикуемый результат"), "{refused}");
    let confirmed = snapshot::plan(
        ru,
        scenario,
        &first,
        &Options {
            replace_workload: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        (confirmed.action, confirmed.workload),
        (
            snapshot::Action::Unchanged,
            snapshot::WorkloadBasis::ConfirmedReplacement
        )
    );
    // Другой результат истории с тем же блоком и той же нагрузкой — доказательство.
    let mut remeasured = first.clone();
    remeasured.provenance.code.git_commit = Some("c".repeat(40));
    let proven = snapshot::plan(
        ru,
        scenario,
        &remeasured,
        &Options {
            history: vec![
                snapshot::Recorded::of(scenario, &first),
                snapshot::Recorded::of(scenario, &remeasured),
            ],
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        (proven.action, proven.workload),
        (
            snapshot::Action::Unchanged,
            snapshot::WorkloadBasis::ProvenByHistory
        )
    );
    // Блок, не совпадающий ни с одним результатом истории (изменён вручную
    // или сгенерирован другой версией), происхождения не доказывает.
    let edited: Vec<String> = published
        .iter()
        .map(|text| text.replace("hyperfine 2.0.0", "hyperfine 2.0.1"))
        .collect();
    for (path, text) in pages.iter().zip(&edited) {
        fs::write(path, text).unwrap();
    }
    let later = result(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        ..Spec::default()
    });
    let refused = snapshot::plan(
        ru,
        scenario,
        &later,
        &Options {
            history: vec![
                snapshot::Recorded::of(scenario, &first),
                snapshot::Recorded::of(scenario, &later),
            ],
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(refused.contains("блок на странице"), "{refused}");
    for (path, text) in pages.iter().zip(&published) {
        fs::write(path, text).unwrap();
    }
    assert_eq!(read_all(), published, "отказы ничего не записали");

    // CLI: в локальной истории только публикуемый результат.
    let mut store = f.open();
    let id = f.import(&mut store, &successor);
    drop(store);
    let page_arg = ru.to_str().unwrap();
    let (code, out, err) = f.cli(&["snapshot", &id[..12], "--page", page_arg, "--apply"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("в локальной истории нет"), "{err}");
    assert!(!leaks(&err), "{err}");
    assert_eq!(read_all(), published, "отказ ничего не записал");
    let (code, out, err) = f.cli(&[
        "--json",
        "snapshot",
        &id[..12],
        "--page",
        page_arg,
        "--replace-workload",
    ]);
    assert_eq!(code, 0, "{out}{err}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["plan"]["workload"], "confirmed-replacement", "{out}");
    for text in read_all() {
        assert!(!leaks(&text));
    }
}

/// Синтетический модуль со сценарием без входов роли `workload`.
const BARE_SCENARIO: &str = r#"schema_version = 1
module = "bare"

[[scenario]]
id = "bare.cli"
title = "Синтетический сценарий без нагрузки"
revision = 1
kind = "short-cli"
status = "executable"
description = "Синтетические результаты без входов нагрузки."
measured = "Один запуск."
platforms = ["linux"]

[scenario.target]
program = { kind = "executable", name = "true" }
expect_exit_codes = [0]

[[scenario.input]]
id = "toolkit"
kind = "repo-tree"
role = "subject"
identity = "content-tree-sha256"
description = "Измеряемый код."

[scenario.runs]
warmup = 1
min_runs = 2
max_runs = 20
single_run_allowed = false
run_timeout_seconds = 30
total_budget_seconds = 300

[[scenario.profile]]
id = "warm"
cache_definition = "Кэшей нет."
cleared = []
retained = []

[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[[scenario.metric]]
metric = "memory_peak_resident"
scope = "largest-process-in-tree"
source = "hyperfine"
required = false

[scenario.environment]
record = ["os", "arch", "module_version", "cpu_model", "virtualization", "logical_cpus", "memory_total_bytes"]
comparable = ["os", "arch", "cpu_model", "virtualization"]
"#;

#[test]
fn snapshot_of_a_scenario_without_workload_needs_no_identity_proof() {
    let f = fixture();
    fs::write(f.catalog_dir().join("scenarios/bare.toml"), BARE_SCENARIO).unwrap();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("bare.cli").unwrap().scenario;
    let bare = |spec: &Spec| {
        let mut r = result(spec);
        r.scenario.id = "bare.cli".into();
        r.scenario.module = "bare".into();
        r.inputs.retain(|i| i.role != InputRole::Workload);
        assert_valid(&f, &r);
        r
    };
    let pages = snapshot_pages(&f, "bare");
    let ru = &pages[0];
    let first = bare(&Spec::default());
    let plan = snapshot::plan(ru, scenario, &first, &Options::default()).unwrap();
    assert_eq!(plan.workload, snapshot::WorkloadBasis::New);
    snapshot::apply(&plan).unwrap();
    assert!(
        fs::read_to_string(ru)
            .unwrap()
            .contains("Сценарий не объявляет входов.")
    );
    // Повтор: доказывать нечего, история не нужна.
    let plan = snapshot::plan(ru, scenario, &first, &Options::default()).unwrap();
    assert_eq!(
        (plan.action, plan.workload),
        (
            snapshot::Action::Unchanged,
            snapshot::WorkloadBasis::NoWorkload
        )
    );
    let later = bare(&Spec {
        recorded_at: "2026-10-02T10:00:00Z",
        commit: 'b',
        wall: vec![0.05, 0.05],
        ..Spec::default()
    });
    let plan = snapshot::plan(ru, scenario, &later, &Options::default()).unwrap();
    assert_eq!(
        (plan.action, plan.workload),
        (
            snapshot::Action::Replaced,
            snapshot::WorkloadBasis::NoWorkload
        )
    );
    snapshot::apply(&plan).unwrap();
    for path in &pages {
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("2026-10-02"), "{}", path.display());
        assert!(!text.contains(&"b".repeat(40)) && !text.contains(&"b".repeat(64)));
    }
}

#[test]
fn snapshot_pages_stay_consistent_when_a_write_fails() {
    let f = fixture();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.cli").unwrap().scenario;
    let docs = f.root.join("workspace/docs");
    let ru = docs.join("fixture.md");
    // Русская страница с CRLF и соседним блоком другого профиля.
    fs::write(&ru, PAGE_RU.replace('\n', "\r\n")).unwrap();
    let sha = snapshot::git_blob_sha(fs::read(&ru).unwrap().as_slice());
    fs::write(docs.join("fixture.en.md"), page("en", &sha)).unwrap();
    fs::write(docs.join("fixture.zh-CN.md"), page("zh", &sha)).unwrap();
    let options = Options::default();
    let mut neighbour = result(&Spec::default());
    neighbour.profile.id = "cold".into();
    snapshot::apply(&snapshot::plan(&ru, scenario, &neighbour, &options).unwrap()).unwrap();
    let paths: Vec<PathBuf> = ["fixture.md", "fixture.en.md", "fixture.zh-CN.md"]
        .iter()
        .map(|n| docs.join(n))
        .collect();
    let read = || -> Vec<String> {
        paths
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect()
    };
    let leftovers = || {
        fs::read_dir(&docs)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count()
    };
    let before = read();
    let plan = snapshot::plan(&ru, scenario, &result(&Spec::default()), &options).unwrap();
    let fail_at = |target: snapshot::Step| {
        move |step: snapshot::Step| {
            if step == target {
                Err(std::io::Error::other("внедрённый отказ"))
            } else {
                Ok(())
            }
        }
    };
    use snapshot::Step::{Commit, Prepare, Restore};
    for target in [Prepare(1), Prepare(2), Commit(1), Commit(2)] {
        let error = snapshot::apply_with(&plan, &mut fail_at(target)).unwrap_err();
        assert!(
            error.contains("страницы не изменены"),
            "{target:?}: {error}"
        );
        assert_eq!(read(), before, "{target:?}: все три страницы прежние");
        assert_eq!(leftovers(), 0, "{target:?}: временных файлов нет");
    }
    // Не удался и возврат: ошибка называет рассогласованные страницы.
    let mut faults = [Commit(2), Restore(1)].into_iter().collect::<Vec<_>>();
    let error = snapshot::apply_with(&plan, &mut |step| {
        if let Some(n) = faults.iter().position(|s| *s == step) {
            faults.remove(n);
            Err(std::io::Error::other("внедрённый отказ"))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(error.contains("рассогласованы"), "{error}");
    assert!(
        error.contains("fixture.en.md") && error.contains("fixture.zh-CN.md"),
        "{error}"
    );
    for (path, text) in paths.iter().zip(&before) {
        fs::write(path, text).unwrap();
    }
    // Страница, изменённая после подготовки плана, не перезаписывается.
    fs::write(&paths[2], format!("{}\nправка", before[2])).unwrap();
    let error = snapshot::apply(&plan).unwrap_err();
    assert!(error.contains("изменилась после подготовки"), "{error}");
    fs::write(&paths[2], &before[2]).unwrap();
    // Успешное обновление: CRLF, отметки i18n и соседний блок сохранены.
    snapshot::apply(&plan).unwrap();
    let after = read();
    assert_eq!(
        after[0].matches('\n').count(),
        after[0].matches("\r\n").count()
    );
    let new_sha = snapshot::git_blob_sha(after[0].as_bytes());
    for text in &after[1..] {
        assert!(text.contains(&format!("i18n-source-git-blob: {new_sha} -->")));
        assert_eq!(text.matches("benchmark-snapshot: {").count(), 2);
    }
    assert!(after[0].contains("профиль `cold`") && after[0].contains("профиль `warm`"));
}

/// Параметр, влияющий на сопоставимость (например, число писателей), — часть
/// определения нагрузки снимка: блок его показывает, а результат с другим
/// значением прежний снимок молча не заменяет.
#[test]
fn snapshot_shows_and_guards_comparability_parameters() {
    let f = fixture();
    let scenario_body = SCENARIO
        .split_once("[[scenario]]")
        .map(|(_, body)| body)
        .unwrap();
    let parametrized = format!(
        "{SCENARIO}\n[[scenario]]{}\n[[scenario.parameter]]\nname = \"writers\"\ndescription = \"Число писателей.\"\nrequired = true\nrecord = true\naffects_comparability = true\n",
        scenario_body.replace("id = \"fixture.cli\"", "id = \"fixture.param\"")
    );
    fs::write(f.catalog_dir().join("scenarios/fixture.toml"), parametrized).unwrap();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.param").unwrap().scenario;
    let pages = snapshot_pages(&f, "fixture");
    let ru = &pages[0];
    let with = |writers: &str, recorded_at: &'static str| {
        let mut result = result(&Spec {
            recorded_at,
            ..Spec::default()
        });
        result.scenario.id = "fixture.param".into();
        result.parameters = BTreeMap::from([("writers".to_owned(), writers.to_owned())]);
        result
    };
    let first = with("a|b", "2026-10-03T10:00:00Z");
    let history = vec![snapshot::Recorded::of(scenario, &first)];
    let options = Options {
        history: history.clone(),
        ..Options::default()
    };
    let plan = snapshot::plan(ru, scenario, &first, &options).unwrap();
    snapshot::apply(&plan).unwrap();
    let text = fs::read_to_string(ru).unwrap();
    assert!(text.contains(r#""parameters":{"writers":"a|b"}"#), "{text}");
    let escaped = "| параметр `writers` | `a\\|b` |".to_owned();
    assert!(text.contains(&escaped), "{text}");
    let en = fs::read_to_string(&pages[1]).unwrap();
    let escaped_en = "| parameter `writers` | `a\\|b` |".to_owned();
    assert!(en.contains(&escaped_en), "{en}");

    // Тот же объём и та же нагрузка, но другое значение параметра: несопоставимо.
    let other = with("1", "2026-10-04T10:00:00Z");
    let refused = snapshot::plan(ru, scenario, &other, &options).unwrap_err();
    assert!(
        refused.contains("writers=a|b")
            && refused.contains("writers=1")
            && refused.contains("--replace-workload"),
        "{refused}"
    );
    // Тот же параметр и нагрузка, доказанная историей, заменяются без флага.
    let same = with("a|b", "2026-10-05T10:00:00Z");
    let plan = snapshot::plan(ru, scenario, &same, &options).unwrap();
    assert_eq!(plan.workload, snapshot::WorkloadBasis::ProvenByHistory);
    // Подтверждённая смена параметра разрешена флагом.
    let confirmed = Options {
        replace_workload: true,
        ..options
    };
    let plan = snapshot::plan(ru, scenario, &other, &confirmed).unwrap();
    assert_eq!(plan.workload, snapshot::WorkloadBasis::ConfirmedReplacement);
    for value in ["a`b|c\\d\nnext", "\r\n", " a ", "`*x*<&amp;>_[]"] {
        let arbitrary = with(value, "2026-10-06T10:00:00Z");
        let plan = snapshot::plan(ru, scenario, &arbitrary, &confirmed).unwrap();
        snapshot::apply(&plan).unwrap();
        for page in &pages {
            let text = fs::read_to_string(page).unwrap();
            let encoded = serde_json::to_string(value).unwrap();
            assert!(text.contains(&format!("\"writers\":{encoded}")), "{text}");
            let row = text
                .lines()
                .find(|line| line.starts_with("| ") && line.contains("`writers`"))
                .unwrap();
            assert_eq!(row.matches('|').count(), 3, "{row}");
            assert!(row.contains("<code>"), "{row}");
        }
    }
}

#[test]
fn snapshot_materializes_optional_default_comparability_parameter() {
    let f = fixture();
    let scenario_body = SCENARIO
        .split_once("[[scenario]]")
        .map(|(_, body)| body)
        .unwrap();
    let parametrized = format!(
        "{SCENARIO}\n[[scenario]]{}\n[[scenario.parameter]]\nname = \"writers\"\ndescription = \"Число писателей.\"\nrequired = false\ndefault = \"4\"\nchoices = [\"4\", \"8\"]\nrecord = true\naffects_comparability = true\n",
        scenario_body.replace("id = \"fixture.cli\"", "id = \"fixture.default\"")
    );
    fs::write(f.catalog_dir().join("scenarios/fixture.toml"), parametrized).unwrap();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.default").unwrap().scenario;
    let pages = snapshot_pages(&f, "fixture");
    let ru = &pages[0];

    let mut implicit = result(&Spec {
        recorded_at: "2026-10-06T10:00:00Z",
        ..Spec::default()
    });
    implicit.scenario.id = "fixture.default".into();
    implicit.parameters.clear();
    let mut explicit = result(&Spec {
        recorded_at: "2026-10-07T10:00:00Z",
        ..Spec::default()
    });
    explicit.scenario.id = "fixture.default".into();
    explicit.parameters = BTreeMap::from([("writers".to_owned(), "4".to_owned())]);

    let effective = BTreeMap::from([("writers".to_owned(), "4".to_owned())]);
    assert_eq!(
        snapshot::comparability_parameters(scenario, &implicit),
        effective
    );
    assert_eq!(
        snapshot::comparability_parameters(scenario, &explicit),
        effective
    );
    assert!(benchmark_contract::comparability::compare(scenario, &implicit, &explicit).comparable);

    let options = Options {
        history: vec![snapshot::Recorded::of(scenario, &implicit)],
        ..Options::default()
    };
    let initial = snapshot::plan(ru, scenario, &implicit, &options).unwrap();
    snapshot::apply(&initial).unwrap();
    let text = fs::read_to_string(ru).unwrap();
    assert!(text.contains(r#""parameters":{"writers":"4"}"#), "{text}");

    let update = snapshot::plan(ru, scenario, &explicit, &options).unwrap();
    assert_eq!(update.workload, snapshot::WorkloadBasis::ProvenByHistory);
}

#[test]
fn snapshot_cli_publishes_next_to_the_module() {
    let f = fixture();
    let dir = f.root.join("workspace/tools/fixture");
    fs::create_dir_all(&dir).unwrap();
    let source = PAGE_RU
        .replace("fixture.en.md", "performance.en.md")
        .replace("fixture.zh-CN.md", "performance.zh-CN.md");
    fs::write(dir.join("performance.md"), &source).unwrap();
    let sha = snapshot::git_blob_sha(source.as_bytes());
    for lang in ["en", "zh-CN"] {
        let text = page(if lang == "en" { "en" } else { "zh" }, &sha)
            .replace("fixture.md", "performance.md")
            .replace("fixture.en.md", "performance.en.md")
            .replace("fixture.zh-CN.md", "performance.zh-CN.md");
        fs::write(dir.join(format!("performance.{lang}.md")), text).unwrap();
    }
    let mut store = f.open();
    let id = f.import(&mut store, &result(&Spec::default()));
    drop(store);
    let (code, out, err) = f.cli(&[
        "snapshot",
        &id,
        "--page",
        dir.join("performance.md").to_str().unwrap(),
        "--apply",
    ]);
    assert_eq!(code, 0, "{out}{err}");
    for name in [
        "performance.md",
        "performance.en.md",
        "performance.zh-CN.md",
    ] {
        assert!(
            fs::read_to_string(dir.join(name))
                .unwrap()
                .contains("benchmark-snapshot: {")
        );
    }
}

/// Страница в HTML по правилам GFM: таблицы, автоссылки, зачёркивание,
/// формулы; встроенный HTML разрешён, как на GitHub.
fn gfm_html(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.strikethrough = true;
    options.extension.math_dollars = true;
    options.render.r#unsafe = true;
    comrak::markdown_to_html(markdown, &options)
}

/// Строки всех таблиц HTML: внутренний HTML ячеек по порядку.
fn table_rows(html: &str) -> Vec<Vec<String>> {
    html.split("<tr>")
        .skip(1)
        .map(|row| {
            let row = &row[..row.find("</tr>").unwrap()];
            let mut cells = Vec::new();
            let mut rest = row;
            while let Some(start) = rest.find("<td>").or_else(|| rest.find("<th>")) {
                rest = &rest[start + 4..];
                let end = rest.find("</td>").or_else(|| rest.find("</th>")).unwrap();
                cells.push(rest[..end].to_owned());
                rest = &rest[end + 5..];
            }
            cells
        })
        .collect()
}

/// Видимый текст фрагмента HTML и имена встретившихся в нём тегов.
fn visible(html: &str) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut tags = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let end = rest[start..].find('>').unwrap() + start;
        tags.push(rest[start + 1..end].to_owned());
        rest = &rest[end + 1..];
    }
    text.push_str(rest);
    let mut decoded = String::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let end = rest[start..].find(';').unwrap() + start;
        let entity = &rest[start + 1..end];
        decoded.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            _ => {
                let number = entity.strip_prefix('#').unwrap();
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
                    None => number.parse().unwrap(),
                };
                char::from_u32(code).unwrap()
            }
        });
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    (decoded, tags)
}

/// Значения, которые контракт результата допускает в факте окружения и
/// параметре, но которые Markdown иначе прочитал бы как разметку или как
/// границу ячейки, строки или блока; рядом — обычные значения, которые
/// остаются прежним текстом.
const MARKDOWN_HOSTILE: &[&str] = &[
    "a\\|b",
    "a\\",
    "a`b",
    "``",
    "line1\nline2",
    "a\r\n",
    "x\u{7}y\u{1b}[0m",
    " a ",
    "*em* _u_ ~~s~~",
    "<img src=x onerror=alert(1)>",
    "<script>alert(1)</script>",
    "[l](https://example.com)",
    "www.example.com",
    "https://example.com",
    ":smile:",
    "$x^2$",
    "&amp; &#124;",
    "<!-- /benchmark-snapshot -->",
    "# h",
];
const MARKDOWN_PLAIN: &[&str] = &[
    "x86_64",
    "a|b",
    "AMD Ryzen 7 5800X3D 8-Core Processor",
    "rustc 1.99.1 (58cf8f9c3 2026-10-13)",
    "6.18.54.1-microsoft-standard-WSL2",
    "a|b c",
    "| --- |",
    "Ядро: 4",
];

#[test]
fn snapshot_tables_keep_every_allowed_value_in_one_exact_cell() {
    let f = fixture();
    let scenario_body = SCENARIO
        .split_once("[[scenario]]")
        .map(|(_, body)| body)
        .unwrap();
    let parametrized = format!(
        "{SCENARIO}\n[[scenario]]{}\n[[scenario.parameter]]\nname = \"writers\"\ndescription = \"Число писателей.\"\nrequired = true\nrecord = true\naffects_comparability = true\n",
        scenario_body.replace("id = \"fixture.cli\"", "id = \"fixture.param\"")
    );
    fs::write(f.catalog_dir().join("scenarios/fixture.toml"), parametrized).unwrap();
    let catalog = f.catalog();
    let scenario = &catalog.scenario("fixture.param").unwrap().scenario;
    let reason =
        "строка 1\n\n<!-- /benchmark-snapshot -->\n# `код` *x* [l](u) <b>b</b> | perf_event";
    for (value, plain) in MARKDOWN_HOSTILE
        .iter()
        .map(|v| (*v, false))
        .chain(MARKDOWN_PLAIN.iter().map(|v| (*v, true)))
    {
        let mut result = result(&Spec {
            memory: None,
            ..Spec::default()
        });
        result.scenario.id = "fixture.param".into();
        result.parameters = BTreeMap::from([("writers".to_owned(), value.to_owned())]);
        result.environment.insert(
            "cpu_model".into(),
            Fact::Known {
                value: value.to_owned(),
            },
        );
        result.metrics[1].reason = Some(reason.into());
        assert_valid(&f, &result);
        let pages = snapshot_pages(&f, "fixture");
        let options = Options {
            replace_workload: true,
            ..Options::default()
        };
        let plan = snapshot::plan(&pages[0], scenario, &result, &options).unwrap();
        snapshot::apply(&plan).unwrap();
        let expected = if plain {
            value.to_owned()
        } else {
            serde_json::to_string(value).unwrap()
        };
        for page in &pages {
            let text = fs::read_to_string(page).unwrap();
            // Маркеры блока целы: ровно одно начало и один конец на своих строках.
            assert_eq!(
                text.lines()
                    .filter(|l| l.starts_with("<!-- benchmark-snapshot: "))
                    .count(),
                1,
                "{text}"
            );
            assert_eq!(
                text.lines()
                    .filter(|l| *l == "<!-- /benchmark-snapshot -->")
                    .count(),
                1,
                "{text}"
            );
            let html = gfm_html(&text);
            let rows = table_rows(&html);
            // Окружение: заголовок и семь фактов; нагрузка: заголовок, параметр
            // и объём входа; метрики: заголовок и измеренное время.
            assert_eq!(rows.len(), 8 + 3 + 2, "{value:?}\n{html}");
            for row in &rows[..11] {
                assert_eq!(row.len(), 2, "{value:?}: {row:?}\n{html}");
            }
            let find = |name: &str| {
                rows.iter()
                    .find(|row| row[0].contains(&format!("<code>{name}</code>")))
                    .unwrap_or_else(|| panic!("{name}: {html}"))
            };
            let (shown, tags) = visible(&find("cpu_model")[1]);
            assert_eq!(shown, expected, "{value:?}\n{html}");
            let allowed: &[&str] = if plain { &[] } else { &["code", "/code"] };
            assert_eq!(tags, allowed, "{value:?}\n{html}");
            let (shown, tags) = visible(&find("writers")[1]);
            assert_eq!(shown, expected, "{value:?}\n{html}");
            assert_eq!(tags, ["code", "/code"], "{value:?}\n{html}");
            // Причина отсутствия — одна строка абзаца без разметки.
            let missing = html
                .split("<p>")
                .find(|p| p.contains("memory_peak_resident"))
                .unwrap_or_else(|| panic!("{html}"));
            let missing = &missing[..missing.find("</p>").unwrap()];
            let (shown, tags) = visible(missing);
            assert!(
                shown.contains(
                    "строка 1  <!-- /benchmark-snapshot --> # `код` *x* [l](u) <b>b</b> | perf_event"
                ),
                "{shown}"
            );
            assert!(tags.iter().all(|t| t == "code" || t == "/code"), "{tags:?}");
        }
    }
}
