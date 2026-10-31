//! Сквозные проверки контракта: декларации → обнаружение → валидация →
//! типизированная модель, результаты и их обратный проход, импорт Hyperfine,
//! сопоставимость и CLI. Учебный каталог лежит в `tests/fixtures`; отрицательные
//! случаи строятся правкой его копии во временном каталоге.

use benchmark_contract::catalog::{self, Catalog};
use benchmark_contract::comparability;
use benchmark_contract::diagnostic::{Diagnostic, Severity};
use benchmark_contract::hyperfine::{self, ImportError};
use benchmark_contract::preflight;
use benchmark_contract::result::{
    self, Aggregate, BenchmarkResult, CodeState, Fact, Identity, InputRecord, Observation, Outcome,
    OutcomeStatus, ProfileRef, Provenance, Quantity, ScenarioRef,
};
use benchmark_contract::scenario::{IdentityMethod, InputRole};
use benchmark_contract::vocabulary::{CacheState, Network, ScenarioStatus, Unit, WorkloadKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn example_catalog() -> PathBuf {
    fixtures().join("example-catalog")
}

fn repository_catalog() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks")
}

/// Временная копия учебного каталога; удаляется при выходе из теста.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "benchmark-contract-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        copy_dir(&example_catalog(), &root);
        Scratch { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn edit(&self, relative: &str, from: &str, to: &str) -> &Self {
        let path = self.path(relative);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "в {relative} нет фрагмента «{from}»");
        fs::write(&path, text.replacen(from, to, 1)).unwrap();
        self
    }

    fn write(&self, relative: &str, text: &str) -> &Self {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        self
    }

    fn load(&self) -> catalog::Loaded {
        catalog::load(&self.root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

const CLI: &str = "scenarios/demo-cli.toml";
const E2E: &str = "scenarios/demo-e2e.toml";

/// Ошибка с кодом есть; сценарий и поле совпадают, если заданы.
#[track_caller]
fn assert_error(
    diagnostics: &[Diagnostic],
    code: &str,
    scenario: Option<&str>,
    field: Option<&str>,
) {
    let found = diagnostics.iter().any(|d| {
        d.severity == Severity::Error
            && d.code == code
            && scenario.is_none_or(|s| d.scenario.as_deref() == Some(s))
            && field.is_none_or(|f| d.field.as_deref() == Some(f))
    });
    assert!(
        found,
        "ожидалась ошибка {code} (сценарий {scenario:?}, поле {field:?}), получено:\n{}",
        diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[track_caller]
fn rejected(scratch: &Scratch, code: &str, scenario: Option<&str>, field: Option<&str>) {
    let loaded = scratch.load();
    assert!(
        loaded.catalog.is_none(),
        "каталог с ошибкой {code} не должен загружаться"
    );
    assert_error(&loaded.diagnostics, code, scenario, field);
}

fn load_example() -> Catalog {
    let loaded = catalog::load(&example_catalog());
    assert!(
        loaded.catalog.is_some(),
        "{}",
        loaded
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
    loaded.catalog.unwrap()
}

// --- Каталог: положительные случаи ---

#[test]
fn example_catalog_discovers_modules_into_typed_model() {
    let loaded = catalog::load(&example_catalog());
    let catalog = loaded.catalog.expect("учебный каталог корректен");
    let ids: Vec<&str> = catalog
        .scenarios
        .iter()
        .map(|s| s.scenario.id.as_str())
        .collect();
    assert_eq!(ids, ["demo-cli.count-bytes", "demo-e2e.suite"]);
    assert_eq!(catalog.profiles.len(), 4);

    let cli = &catalog.scenario("demo-cli.count-bytes").unwrap();
    assert_eq!(cli.module, "demo-cli");
    assert_eq!(cli.file, CLI);
    assert_eq!(cli.scenario.kind, WorkloadKind::ShortCli);
    assert_eq!(cli.scenario.status, ScenarioStatus::Example);
    assert_eq!(
        cli.scenario.inputs[0].kind,
        benchmark_contract::scenario::InputKind::RepoFile
    );
    assert_eq!(cli.scenario.target.working_dir, ".");

    let e2e = &catalog.scenario("demo-e2e.suite").unwrap().scenario;
    assert_eq!(e2e.kind, WorkloadKind::LongE2e);
    let profiles: Vec<&str> = e2e.profiles.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(profiles, ["cold", "warm", "incremental"]);
    assert_eq!(
        catalog.profile("offline-warm").unwrap().network,
        Network::Offline
    );
    assert_eq!(catalog.profile("cold").unwrap().cache, CacheState::Cold);

    // Необязательная метрика, недоступная на одной из платформ, — предупреждение, а не отказ.
    assert!(
        loaded
            .diagnostics
            .iter()
            .all(|d| d.severity == Severity::Warning)
    );
    assert!(
        loaded
            .diagnostics
            .iter()
            .any(|d| d.code == "metric_unavailable_on_platform")
    );
}

#[test]
fn repository_catalog_is_valid_and_contains_only_executable_scenarios() {
    let loaded = catalog::load(&repository_catalog());
    let catalog = loaded.catalog.unwrap_or_else(|| {
        panic!(
            "{}",
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    });
    assert!(!catalog.scenarios.is_empty());
    assert!(
        !loaded
            .diagnostics
            .iter()
            .any(|d| d.code == "unused_parameter"),
        "{:?}",
        loaded.diagnostics
    );
    for item in &catalog.scenarios {
        assert_eq!(
            item.scenario.status,
            ScenarioStatus::Executable,
            "{}",
            item.scenario.id
        );
        assert_eq!(item.file, format!("scenarios/{}.toml", item.module));
    }
}

#[test]
fn new_module_registers_by_adding_one_file() {
    let scratch = Scratch::new();
    let text = fs::read_to_string(scratch.path(CLI))
        .unwrap()
        .replace("demo-cli", "demo-other");
    scratch.write("scenarios/demo-other.toml", &text);
    let catalog = scratch
        .load()
        .catalog
        .expect("новый модуль подключается одним файлом");
    assert!(catalog.scenario("demo-other.count-bytes").is_some());
    assert_eq!(catalog.scenarios.len(), 3);
}

#[test]
fn hidden_files_are_ignored_and_foreign_files_rejected() {
    let scratch = Scratch::new();
    scratch.write("scenarios/.editor-swap", "мусор");
    assert!(scratch.load().catalog.is_some());
    scratch.write("scenarios/README.md", "текст");
    rejected(&scratch, "unexpected_entry", None, None);
}

#[test]
fn loading_never_executes_declared_commands() {
    let scratch = Scratch::new();
    let marker = scratch.path("workspace/created-by-validation");
    scratch.edit(
        E2E,
        "[[scenario.cleanup]]",
        "[[scenario.setup]]\ndescription = \"Создал бы файл, если бы шаг запускался.\"\nprogram = { kind = \"executable\", name = \"touch\" }\nargs = [\"created-by-validation\"]\ntouches = \"scenario-temp\"\ntimeout_seconds = 5\n\n[[scenario.cleanup]]",
    );
    let before = snapshot(&scratch.root);
    assert!(scratch.load().catalog.is_some());
    assert!(!marker.exists(), "валидация не запускает шаги деклараций");
    assert_eq!(before, snapshot(&scratch.root), "валидация ничего не пишет");
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                files.insert(entry.path(), fs::read(entry.path()).unwrap());
            }
        }
    }
    files
}

#[test]
fn cargo_bin_resolves_against_workspace_manifests() {
    let scratch = Scratch::new();
    scratch
        .write("workspace/Cargo.toml", "[workspace]\nmembers = [\"tools/demo\"]\n\n[profile.ci]\ninherits = \"dev\"\n")
        .write("workspace/tools/demo/Cargo.toml", "[package]\nname = \"demo-tool\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"demo\"\npath = \"src/main.rs\"\n")
        .write("workspace/tools/demo/src/main.rs", "fn main() {}\n")
        .edit(
            CLI,
            "program = { kind = \"executable\", name = \"wc\" }",
            "program = { kind = \"cargo-bin\", package = \"demo-tool\", bin = \"demo\", cargo_profile = \"ci\" }",
        );
    assert!(scratch.load().catalog.is_some());

    // Cargo находит и src/bin/<имя>/main.rs без секции [[bin]].
    scratch
        .write(
            "workspace/tools/demo/src/bin/report/main.rs",
            "fn main() {}\n",
        )
        .edit(CLI, "bin = \"demo\"", "bin = \"report\"");
    assert!(scratch.load().catalog.is_some());
    scratch.edit(CLI, "bin = \"report\"", "bin = \"demo\"");

    scratch.edit(CLI, "bin = \"demo\"", "bin = \"demo-tool\"");
    rejected(
        &scratch,
        "unresolved_program",
        Some("demo-cli.count-bytes"),
        Some("target.program.bin"),
    );
    scratch.edit(
        CLI,
        "package = \"demo-tool\", bin = \"demo-tool\"",
        "package = \"missing\", bin = \"demo\"",
    );
    rejected(
        &scratch,
        "unresolved_program",
        Some("demo-cli.count-bytes"),
        Some("target.program.package"),
    );
}

// --- Каталог: версии, синтаксис, форма ---

#[test]
fn unsupported_schema_versions_are_refused_explicitly() {
    let scratch = Scratch::new();
    scratch.edit("catalog.toml", "schema_version = 1", "schema_version = 2");
    rejected(
        &scratch,
        "unsupported_schema_version",
        None,
        Some("schema_version"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "schema_version = 1", "schema_version = 3");
    let loaded = scratch.load();
    assert_error(
        &loaded.diagnostics,
        "unsupported_schema_version",
        None,
        Some("schema_version"),
    );
    let message = &loaded.diagnostics[0].message;
    assert!(
        message.contains("3") && message.contains("только 1"),
        "{message}"
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "schema_version = 1\n", "");
    rejected(
        &scratch,
        "unsupported_schema_version",
        None,
        Some("schema_version"),
    );
}

#[test]
fn shape_errors_name_scenario_and_field() {
    let scratch = Scratch::new();
    scratch.edit(CLI, "kind = \"short-cli\"", "kind = \"medium\"");
    rejected(
        &scratch,
        "invalid_field",
        Some("demo-cli.count-bytes"),
        Some("kind"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "measured =", "measurd =");
    let loaded = scratch.load();
    assert_error(
        &loaded.diagnostics,
        "invalid_field",
        Some("demo-cli.count-bytes"),
        Some("measurd"),
    );
    assert_error(
        &loaded.diagnostics,
        "invalid_field",
        Some("demo-cli.count-bytes"),
        Some("measured"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "touches = \"scenario-temp\"\ntimeout_seconds = 60\n\n[[scenario.profile]]",
        "touches = \"everything\"\ntimeout_seconds = 60\n\n[[scenario.profile]]",
    );
    rejected(
        &scratch,
        "invalid_field",
        Some("demo-e2e.suite"),
        Some("cleanup[0].touches"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "program = { kind = \"executable\", name = \"wc\" }",
        "program = { kind = \"script\", path = \"x.sh\" }",
    );
    rejected(
        &scratch,
        "invalid_field",
        Some("demo-cli.count-bytes"),
        Some("target.program.kind"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "[scenario.runs]\nwarmup = 2",
        "[scenario.runs]\nwarmup = -1",
    );
    rejected(
        &scratch,
        "invalid_field",
        Some("demo-cli.count-bytes"),
        Some("runs.warmup"),
    );
}

#[test]
fn toml_syntax_errors_are_reported_per_file() {
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "title = \"Учебный пример: длительный E2E\"",
        "title = \"незакрытая",
    );
    let loaded = scratch.load();
    assert!(loaded.catalog.is_none());
    let diagnostic = loaded
        .diagnostics
        .iter()
        .find(|d| d.code == "toml_syntax")
        .unwrap();
    assert_eq!(diagnostic.file, E2E);
}

#[test]
fn identifiers_and_module_files_must_agree() {
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "id = \"demo-cli.count-bytes\"",
        "id = \"demo-e2e.count-bytes\"",
    );
    rejected(
        &scratch,
        "invalid_scenario_id",
        Some("demo-e2e.count-bytes"),
        Some("id"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "module = \"demo-cli\"",
        "module = \"demo-cli-renamed\"",
    );
    rejected(&scratch, "module_file_mismatch", None, Some("module"));
}

#[test]
fn duplicate_scenario_ids_are_conflicts() {
    let scratch = Scratch::new();
    let text = fs::read_to_string(scratch.path(CLI)).unwrap();
    let scenario = &text[text.find("[[scenario]]").unwrap()..];
    scratch.write(CLI, &format!("{text}\n{scenario}"));
    rejected(
        &scratch,
        "duplicate_scenario_id",
        Some("demo-cli.count-bytes"),
        Some("id"),
    );
}

// --- Каталог: семантика ---

#[test]
fn run_policy_limits_prevent_unbounded_series() {
    let scratch = Scratch::new();
    scratch.edit(CLI, "max_runs = 20", "max_runs = 100000");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-cli.count-bytes"),
        Some("runs.max_runs"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "run_timeout_seconds = 10", "run_timeout_seconds = 0");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-cli.count-bytes"),
        Some("runs.run_timeout_seconds"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "run_timeout_seconds = 10", "run_timeout_seconds = 601");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-cli.count-bytes"),
        Some("runs.run_timeout_seconds"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "total_budget_seconds = 60", "total_budget_seconds = 5");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-cli.count-bytes"),
        Some("runs.total_budget_seconds"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "max_runs = 20", "max_runs = 2");
    rejected(
        &scratch,
        "invalid_runs",
        Some("demo-cli.count-bytes"),
        Some("runs.max_runs"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "single_run_allowed = false",
        "single_run_allowed = true",
    );
    rejected(
        &scratch,
        "single_run_forbidden",
        Some("demo-cli.count-bytes"),
        Some("runs.single_run_allowed"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "timeout_seconds = 600", "timeout_seconds = 999999");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-e2e.suite"),
        Some("setup[0].timeout_seconds"),
    );
}

#[test]
fn short_cli_lifecycle_step_timeout_has_its_own_bounded_limit() {
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "[scenario.runs]",
        "[[scenario.setup]]\ndescription = \"Подготовка.\"\nprogram = { kind = \"executable\", name = \"touch\" }\nargs = [\"prepared\"]\ntouches = \"scenario-temp\"\ntimeout_seconds = 1800\n\n[scenario.runs]",
    );
    assert!(scratch.load().catalog.is_some());

    scratch.edit(CLI, "timeout_seconds = 1800", "timeout_seconds = 1801");
    rejected(
        &scratch,
        "unbounded_runs",
        Some("demo-cli.count-bytes"),
        Some("setup[0].timeout_seconds"),
    );
}

#[test]
fn profile_semantics_define_cache_state_exactly() {
    // Холодный профиль без сброса состояния модуля.
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "args = [\"reset\", \"--cache\"]\ntouches = \"module-owned\"",
        "args = [\"reset\", \"--cache\"]\ntouches = \"read-only\"",
    );
    rejected(
        &scratch,
        "cold_without_reset",
        Some("demo-e2e.suite"),
        Some("profile[0].prepare"),
    );

    // Прогрев противоречит холодному профилю.
    let scratch = Scratch::new();
    scratch.edit(E2E, "warmup = 0", "warmup = 1");
    rejected(
        &scratch,
        "cold_with_warmup",
        Some("demo-e2e.suite"),
        Some("runs.warmup"),
    );

    // Тёплое состояние измеряется минимум дважды.
    let scratch = Scratch::new();
    scratch.edit(E2E, "min_runs = 2", "min_runs = 1");
    rejected(
        &scratch,
        "warm_single_run",
        Some("demo-e2e.suite"),
        Some("runs.min_runs"),
    );

    // Тёплый профиль без прогрева.
    let scratch = Scratch::new();
    scratch.edit(CLI, "warmup = 2", "warmup = 0");
    rejected(
        &scratch,
        "warm_without_priming",
        Some("demo-cli.count-bytes"),
        Some("profile[0].setup"),
    );

    // Офлайн-профиль обязан определить сетевую границу.
    let scratch = Scratch::new();
    scratch.edit(CLI, "network_definition = \"wc не обращается к сети; серия запускается без сетевого доступа.\"\n", "");
    rejected(
        &scratch,
        "missing_network_definition",
        Some("demo-cli.count-bytes"),
        Some("profile[1].network_definition"),
    );

    // Офлайн-профиль не может требовать сеть.
    let scratch = Scratch::new();
    scratch
        .edit(
            E2E,
            "id = \"incremental\"",
            "id = \"offline-warm\"\nnetwork_definition = \"учебно\"",
        )
        .edit(
            E2E,
            "profiles = [\"cold\"]",
            "profiles = [\"cold\", \"offline-warm\"]",
        );
    let loaded = scratch.load();
    assert_error(
        &loaded.diagnostics,
        "offline_requires_network",
        Some("demo-e2e.suite"),
        Some("requires[3].profiles"),
    );

    // Неизвестный и повторный профили.
    let scratch = Scratch::new();
    scratch.edit(CLI, "id = \"offline-warm\"", "id = \"hot\"");
    rejected(
        &scratch,
        "unknown_profile",
        Some("demo-cli.count-bytes"),
        Some("profile[1].id"),
    );
    let scratch = Scratch::new();
    scratch.edit(CLI, "id = \"offline-warm\"", "id = \"warm\"");
    rejected(
        &scratch,
        "duplicate_profile",
        Some("demo-cli.count-bytes"),
        Some("profile[1].id"),
    );

    // Инкрементальный профиль без изменения входа.
    let scratch = Scratch::new();
    let text = fs::read_to_string(scratch.path(E2E)).unwrap();
    let start = text.rfind("[[scenario.profile.prepare]]").unwrap();
    let end = text[start..].find("[[scenario.metric]]").unwrap() + start;
    scratch.write(E2E, &format!("{}{}", &text[..start], &text[end..]));
    rejected(
        &scratch,
        "incremental_without_change",
        Some("demo-e2e.suite"),
        Some("profile[2].prepare"),
    );
}

#[test]
fn metric_requests_cannot_mix_scopes_sources_and_units() {
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "metric = \"memory_peak_resident\"\nscope = \"largest-process-in-tree\"",
        "metric = \"memory_peak_resident\"\nscope = \"cgroup\"",
    );
    rejected(
        &scratch,
        "metric_scope_source",
        Some("demo-cli.count-bytes"),
        Some("metric[1].scope"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "unit = \"second\"", "unit = \"byte\"");
    rejected(
        &scratch,
        "unit_mismatch",
        Some("demo-cli.count-bytes"),
        Some("metric[0].unit"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "[[scenario.requires]]\nkind = \"linux-cgroup-v2\"\n",
        "",
    );
    rejected(
        &scratch,
        "missing_requirement",
        Some("demo-e2e.suite"),
        Some("metric[1]"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "[[scenario.requires]]\nkind = \"perf-events\"\n", "");
    rejected(
        &scratch,
        "missing_requirement",
        Some("demo-e2e.suite"),
        Some("metric[3]"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "metric = \"cache_misses\"\nscope = \"process-tree-waited\"\nsource = \"hyperfine\"\nrequired = false", "metric = \"cache_misses\"\nscope = \"process-tree-waited\"\nsource = \"hyperfine\"\nrequired = true");
    rejected(
        &scratch,
        "metric_unavailable_on_platform",
        Some("demo-cli.count-bytes"),
        Some("metric[3].source"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "sample_interval_seconds = 1.0\n", "");
    rejected(
        &scratch,
        "missing_sample_interval",
        Some("demo-e2e.suite"),
        Some("metric[2].sample_interval_seconds"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "label = \"build\"\n", "");
    rejected(
        &scratch,
        "missing_label",
        Some("demo-e2e.suite"),
        Some("metric[4].label"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "required = true\nunit = \"second\"",
        "required = false\nunit = \"second\"",
    );
    rejected(
        &scratch,
        "primary_metric_missing",
        Some("demo-cli.count-bytes"),
        Some("metric"),
    );
}

#[test]
fn references_to_inputs_parameters_and_paths_must_resolve() {
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "path = \"data/sample.txt\"",
        "path = \"data/missing.txt\"",
    );
    rejected(
        &scratch,
        "unresolved_path",
        Some("demo-cli.count-bytes"),
        Some("input[0].path"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "path = \"data/sample.txt\"",
        "path = \"../../etc/passwd\"",
    );
    rejected(
        &scratch,
        "invalid_path",
        Some("demo-cli.count-bytes"),
        Some("input[0].path"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "args = [\"-c\", \"{input.sample}\"]",
        "args = [\"-c\", \"{input.other}\"]",
    );
    rejected(
        &scratch,
        "unresolved_reference",
        Some("demo-cli.count-bytes"),
        Some("target.args[1]"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "args = [\"-c\", \"{input.sample}\"]",
        "args = [\"-c\", \"{env.HOME}\"]",
    );
    rejected(
        &scratch,
        "invalid_placeholder",
        Some("demo-cli.count-bytes"),
        Some("target.args[1]"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "default = \"stable\"\n", "");
    rejected(
        &scratch,
        "unresolvable_placeholder",
        Some("demo-e2e.suite"),
        Some("target.args[2]"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "expect_exit_codes = [0]",
        "working_dir = \"/tmp\"\nexpect_exit_codes = [0]",
    );
    rejected(
        &scratch,
        "invalid_path",
        Some("demo-cli.count-bytes"),
        Some("target.working_dir"),
    );
}

#[test]
fn workload_inputs_require_size_and_private_paths() {
    let scratch = Scratch::new();
    scratch.edit(CLI, "size = [{ name = \"bytes\", unit = \"byte\", description = \"Размер файла в байтах.\", measure = { kind = \"tree-bytes\" } }]\n", "");
    rejected(
        &scratch,
        "missing_size",
        Some("demo-cli.count-bytes"),
        Some("input[0].size"),
    );

    let scratch = Scratch::new();
    scratch
        .edit(
            CLI,
            "kind = \"repo-file\"\nrole = \"workload\"\nidentity = \"file-sha256\"\npath = \"data/sample.txt\"",
            "kind = \"external-path\"\nrole = \"workload\"\nidentity = \"file-sha256\"\nparameter = \"file\"",
        )
        .edit(
            CLI,
            "[[scenario.input]]",
            "[[scenario.parameter]]\nname = \"file\"\ndescription = \"Путь к файлу\"\nrequired = true\nrecord = true\naffects_comparability = true\n\n[[scenario.input]]",
        );
    rejected(
        &scratch,
        "path_published",
        Some("demo-cli.count-bytes"),
        Some("input[0].parameter"),
    );

    // Путь external-path задаётся только при запуске: параметр обязан
    // гарантированно иметь значение и не может нести default.
    let external = |parameter: &str| {
        let scratch = Scratch::new();
        scratch
            .edit(
                CLI,
                "kind = \"repo-file\"\nrole = \"workload\"\nidentity = \"file-sha256\"\npath = \"data/sample.txt\"",
                "kind = \"external-path\"\nrole = \"workload\"\nidentity = \"file-sha256\"\nparameter = \"file\"",
            )
            .edit(
                CLI,
                "[[scenario.input]]",
                &format!("[[scenario.parameter]]\nname = \"file\"\ndescription = \"Путь к файлу\"\n{parameter}record = false\naffects_comparability = false\n\n[[scenario.input]]"),
            );
        scratch
    };
    assert!(external("required = true\n").load().catalog.is_some());
    rejected(
        &external(""),
        "unresolvable_placeholder",
        Some("demo-cli.count-bytes"),
        Some("target.args[1]"),
    );
    rejected(
        &external("default = \"sample.txt\"\n"),
        "contradictory_input",
        Some("demo-cli.count-bytes"),
        Some("input[0].parameter"),
    );
}

#[test]
fn unsafe_steps_and_secrets_are_rejected() {
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "args = [\"clean\", \"--runs\"]",
        "args = [\"-rf\", \"/\"]",
    );
    scratch.edit(E2E, "description = \"Удалить временные данные прогонов.\"\nprogram = { kind = \"executable\", name = \"demo-e2e-runner\" }", "description = \"Удалить всё.\"\nprogram = { kind = \"executable\", name = \"rm\" }");
    rejected(
        &scratch,
        "forbidden_program",
        Some("demo-e2e.suite"),
        Some("cleanup[0].program.name"),
    );

    // Обёртка запуска исполняет любую команду и обходит запрет так же.
    let scratch = Scratch::new();
    scratch.edit(E2E, "description = \"Удалить временные данные прогонов.\"\nprogram = { kind = \"executable\", name = \"demo-e2e-runner\" }", "description = \"Удалить всё.\"\nprogram = { kind = \"executable\", name = \"nice\" }");
    rejected(
        &scratch,
        "forbidden_program",
        Some("demo-e2e.suite"),
        Some("cleanup[0].program.name"),
    );

    let scratch = Scratch::new();
    scratch.edit(E2E, "program = { kind = \"executable\", name = \"demo-e2e-runner\" }\nargs = [\"reset\", \"--cache\"]", "program = { kind = \"executable\", name = \"docker\" }\nargs = [\"system\", \"prune\", \"-af\"]");
    rejected(
        &scratch,
        "unsafe_cleanup",
        Some("demo-e2e.suite"),
        Some("profile[0].prepare[0].args"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "set = { LC_ALL = \"C\" }",
        "set = { LC_ALL = \"C\", GITHUB_TOKEN = \"ghp_abcdefghijklmnopqrstu\" }",
    );
    rejected(
        &scratch,
        "sensitive_value",
        Some("demo-cli.count-bytes"),
        Some("target.env.set.GITHUB_TOKEN"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "args = [\"-c\", \"{input.sample}\"]",
        "args = [\"-c\", \"/home/someone/private.txt\"]",
    );
    rejected(
        &scratch,
        "sensitive_value",
        Some("demo-cli.count-bytes"),
        Some("target.args[1]"),
    );
}

#[test]
fn environment_facts_must_cover_comparability() {
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "comparable = [\"os\", \"arch\", \"cpu_model\"]",
        "comparable = [\"os\", \"cpu_model\"]",
    );
    rejected(
        &scratch,
        "missing_fact",
        Some("demo-cli.count-bytes"),
        Some("environment.comparable"),
    );

    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "comparable = [\"os\", \"arch\", \"cpu_model\"]",
        "comparable = [\"os\", \"arch\", \"gpu_model\"]",
    );
    rejected(
        &scratch,
        "unknown_fact",
        Some("demo-cli.count-bytes"),
        Some("environment.comparable[2]"),
    );

    let scratch = Scratch::new();
    scratch.edit(CLI, "\"module_version\", ", "");
    rejected(
        &scratch,
        "missing_fact",
        Some("demo-cli.count-bytes"),
        Some("environment.record"),
    );
}

// --- Результаты ---

fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixtures().join("results").join(name)).unwrap()
}

fn check_value(value: &Value, catalog: Option<&Catalog>) -> result::Checked {
    result::check(
        &serde_json::to_string(value).unwrap(),
        "result.json",
        catalog,
    )
}

#[track_caller]
fn result_rejected(value: &Value, catalog: Option<&Catalog>, code: &str, field: &str) {
    let checked = check_value(value, catalog);
    assert!(
        checked.result.is_none(),
        "результат с ошибкой {code} не должен проходить"
    );
    assert_error(&checked.diagnostics, code, None, Some(field));
}

fn cli_result() -> Value {
    serde_json::from_str(&fixture_text("demo-cli.success.json")).unwrap()
}

fn e2e_result() -> Value {
    serde_json::from_str(&fixture_text("demo-e2e.partial.json")).unwrap()
}

#[test]
fn valid_results_round_trip_without_loss() {
    let catalog = load_example();
    for name in ["demo-cli.success.json", "demo-e2e.partial.json"] {
        let text = fixture_text(name);
        let checked = result::check(&text, name, Some(&catalog));
        let parsed = checked.result.unwrap_or_else(|| {
            panic!(
                "{}",
                checked
                    .diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        let serialized = serde_json::to_string_pretty(&parsed).unwrap();
        let again = result::check(&serialized, name, Some(&catalog))
            .result
            .expect("повторная проверка");
        assert_eq!(parsed, again);
        let original: Value = serde_json::from_str(&text).unwrap();
        let reserialized: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            original, reserialized,
            "{name}: сериализация теряет или добавляет поля"
        );
    }
}

#[test]
fn recorded_at_must_be_strict_rfc3339_utc() {
    let mut value = cli_result();
    value["provenance"]["recorded_at"] = json!("2026-+1-+1T+1:+1:+1Z");
    result_rejected(&value, None, "invalid_timestamp", "provenance.recorded_at");

    // Дата обязана существовать в календаре, а не только попадать в диапазоны полей.
    value["provenance"]["recorded_at"] = json!("2026-02-31T12:00:00Z");
    result_rejected(&value, None, "invalid_timestamp", "provenance.recorded_at");
    for impossible in [
        "2025-02-29T00:00:00Z",
        "1900-02-29T00:00:00Z",
        "2026-04-31T00:00:00Z",
        "2026-00-10T00:00:00Z",
        "2026-01-00T00:00:00Z",
        "2026-01-01T00:00:00+00:00",
        "2026-01-01T00:00:00.Z",
    ] {
        assert!(!result::is_utc_timestamp(impossible), "{impossible}");
    }
    for valid in [
        "2024-02-29T23:59:59Z",
        "2000-02-29T00:00:00Z",
        "2026-12-31T23:59:60.123456789Z",
        "2026-04-30T00:00:00Z",
    ] {
        assert!(result::is_utc_timestamp(valid), "{valid}");
    }
}

#[test]
fn unsupported_result_version_is_refused() {
    let mut value = cli_result();
    value["schema_version"] = json!(2);
    result_rejected(&value, None, "unsupported_schema_version", "schema_version");
    value.as_object_mut().unwrap().remove("schema_version");
    result_rejected(&value, None, "unsupported_schema_version", "schema_version");
}

#[test]
fn missing_values_are_statuses_not_zeros() {
    let mut value = cli_result();
    value["runs"][1]["values"]
        .as_object_mut()
        .unwrap()
        .remove("instructions@process-tree-waited");
    result_rejected(
        &value,
        None,
        "missing_value",
        "runs[1].values.instructions@process-tree-waited",
    );

    let mut value = cli_result();
    value["metrics"][2]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    result_rejected(&value, None, "missing_reason", "metrics[2].reason");

    let mut value = cli_result();
    value["runs"][0]["values"]["instructions@process-tree-waited"] =
        json!({"status": "unavailable", "reason": ""});
    result_rejected(
        &value,
        None,
        "missing_reason",
        "runs[0].values.instructions@process-tree-waited",
    );

    // Нулевое значение при статусе unavailable — противоречие доступности.
    let mut value = cli_result();
    value["runs"][0]["values"]["instructions@process-tree-waited"] =
        json!({"status": "measured", "value": 0});
    result_rejected(
        &value,
        None,
        "inconsistent_availability",
        "metrics[2].availability",
    );

    let mut value = cli_result();
    value["runs"][0]["values"]["time_wall_clock@command"] =
        json!({"status": "measured", "value": -1.0});
    result_rejected(
        &value,
        None,
        "invalid_value",
        "runs[0].values.time_wall_clock@command",
    );

    let mut value = e2e_result();
    value["runs"][0]["values"]["module_counter@module-report/checks"] =
        json!({"status": "measured", "value": 1.5});
    result_rejected(
        &value,
        None,
        "invalid_value",
        "runs[0].values.module_counter@module-report/checks",
    );
}

#[test]
fn aggregates_must_follow_primary_measurements() {
    let mut value = cli_result();
    value["aggregates"]["time_wall_clock@command"]["mean"] = json!(0.5);
    result_rejected(
        &value,
        None,
        "aggregate_mismatch",
        "aggregates.time_wall_clock@command.mean",
    );

    let mut value = cli_result();
    value["aggregates"]["instructions@process-tree-waited"] =
        json!({"count": 1, "mean": 1.0, "stddev": null, "median": 1.0, "min": 1.0, "max": 1.0});
    result_rejected(
        &value,
        None,
        "aggregate_without_values",
        "aggregates.instructions@process-tree-waited",
    );

    let mut value = e2e_result();
    value["aggregates"]["time_wall_clock@command"]["stddev"] = json!(0.0);
    result_rejected(
        &value,
        None,
        "aggregate_mismatch",
        "aggregates.time_wall_clock@command.stddev",
    );

    let mut value = cli_result();
    value["aggregates"]["time_wall_clock@command"]["stddev"] = Value::Null;
    result_rejected(
        &value,
        None,
        "aggregate_mismatch",
        "aggregates.time_wall_clock@command.stddev",
    );
}

#[test]
fn outcome_status_must_match_runs() {
    let mut value = cli_result();
    value["runs"][2]["status"] = json!("timeout");
    value["runs"][2]["reason"] = json!("превышен run_timeout_seconds");
    result_rejected(&value, None, "inconsistent_outcome", "outcome.status");

    // Та же серия как partial корректна.
    value["outcome"] = json!({"status": "partial", "reason": "третий прогон превысил таймаут"});
    let mut parsed: BenchmarkResult = serde_json::from_value(value.clone()).unwrap();
    parsed.recompute_aggregates();
    let checked = check_value(
        &serde_json::to_value(&parsed).unwrap(),
        Some(&load_example()),
    );
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);

    let mut value = cli_result();
    value["outcome"] = json!({"status": "failure"});
    let checked = check_value(&value, None);
    assert_error(
        &checked.diagnostics,
        "inconsistent_outcome",
        None,
        Some("outcome.status"),
    );
    assert_error(
        &checked.diagnostics,
        "missing_reason",
        None,
        Some("outcome.reason"),
    );

    let mut value = cli_result();
    value["outcome"] = json!({"status": "unsupported", "reason": "нет hyperfine"});
    result_rejected(&value, None, "inconsistent_outcome", "outcome.status");

    let mut value = cli_result();
    value["runs"][1]["status"] = json!("failure");
    result_rejected(&value, None, "missing_reason", "runs[1].reason");

    let mut value = cli_result();
    value["observation"] = json!("single");
    result_rejected(&value, None, "inconsistent_observation", "observation");
}

#[test]
fn successful_runs_carry_declared_exit_codes() {
    let catalog = load_example();
    let mut value = cli_result();
    value["runs"][1]["exit_code"] = json!(7);
    result_rejected(
        &value,
        Some(&catalog),
        "unexpected_exit_code",
        "runs[1].exit_code",
    );
    // Без декларации допустимые коды неизвестны: структурная проверка их не выдумывает.
    assert!(check_value(&value, None).result.is_some());

    // Ненулевой код, объявленный ожидаемым, сам по себе не ошибка.
    let scratch = Scratch::new();
    scratch.edit(CLI, "expect_exit_codes = [0]", "expect_exit_codes = [0, 7]");
    let declared = scratch.load().catalog.unwrap();
    let checked = check_value(&value, Some(&declared));
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);

    // Неуспешные прогоны — с неожиданным кодом или сигналом (null) — представимы.
    let mut value = cli_result();
    value["runs"][1]["status"] = json!("failure");
    value["runs"][1]["exit_code"] = json!(7);
    value["runs"][1]["reason"] = json!("код завершения 7 вне ожидаемых");
    value["runs"][2]["status"] = json!("failure");
    value["runs"][2]
        .as_object_mut()
        .unwrap()
        .remove("exit_code");
    value["runs"][2]["reason"] = json!("процесс завершён сигналом");
    value["outcome"] = json!({"status": "partial", "reason": "два прогона неуспешны"});
    let mut parsed: BenchmarkResult = serde_json::from_value(value).unwrap();
    parsed.recompute_aggregates();
    let checked = check_value(&serde_json::to_value(&parsed).unwrap(), Some(&catalog));
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);
}

/// Серия, прекращённая до первого измеряемого прогона.
fn without_runs(mut value: Value, outcome: Value, warmup_runs: u32) -> Value {
    value["outcome"] = outcome;
    value["observation"] = json!("none");
    value["runs"] = json!([]);
    value["warmup_runs"] = json!(warmup_runs);
    value.as_object_mut().unwrap().remove("aggregates");
    for metric in value["metrics"].as_array_mut().unwrap() {
        metric["availability"] = json!("unavailable");
        metric["reason"] = json!("измеряемых прогонов не было");
    }
    value
}

#[test]
fn warmup_runs_follow_the_declared_policy() {
    let catalog = load_example();

    // Измерения начались без объявленных двух прогревов или с лишним.
    for warmup in [0, 1, 3] {
        let mut value = cli_result();
        value["warmup_runs"] = json!(warmup);
        result_rejected(&value, Some(&catalog), "warmup_mismatch", "warmup_runs");
    }

    // Холодный профиль не прогревается, в том числе в partial.
    let mut value = e2e_result();
    value["warmup_runs"] = json!(1);
    result_rejected(&value, Some(&catalog), "warmup_mismatch", "warmup_runs");

    // Тёплое состояние через setup профиля при runs.warmup = 0 корректно.
    let mut value = e2e_result();
    value["profile"] = json!({"id": "warm", "cache": "warm", "network": "online"});
    let checked = check_value(&value, Some(&catalog));
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);

    // Отказ до измерений: прогрев мог не завершиться, но лишним быть не может.
    let failed = json!({"status": "failure", "reason": "прогрев завершился кодом 1"});
    for warmup in [0, 1] {
        let value = without_runs(cli_result(), failed.clone(), warmup);
        let checked = check_value(&value, Some(&catalog));
        assert!(checked.result.is_some(), "{:?}", checked.diagnostics);
    }
    let value = without_runs(cli_result(), failed, 3);
    result_rejected(&value, Some(&catalog), "warmup_mismatch", "warmup_runs");
    let value = without_runs(
        e2e_result(),
        json!({"status": "failure", "reason": "setup завершился кодом 1"}),
        1,
    );
    result_rejected(&value, Some(&catalog), "warmup_mismatch", "warmup_runs");

    // Структурная проверка политику прогрева не знает.
    let mut value = cli_result();
    value["warmup_runs"] = json!(0);
    assert!(check_value(&value, None).result.is_some());
}

#[test]
fn unsupported_and_failed_series_are_representable() {
    let catalog = load_example();
    let mut value = cli_result();
    value["outcome"] = json!({"status": "unsupported", "reason": "hyperfine не найден в PATH"});
    value["observation"] = json!("none");
    value["runs"] = json!([]);
    value["warmup_runs"] = json!(0);
    value.as_object_mut().unwrap().remove("aggregates");
    for metric in value["metrics"].as_array_mut().unwrap() {
        metric["availability"] = json!("unsupported");
        metric["reason"] = json!("серия не запускалась");
    }
    let checked = check_value(&value, Some(&catalog));
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);

    value["outcome"] = json!({"status": "failure", "reason": "setup завершился кодом 1"});
    assert!(check_value(&value, Some(&catalog)).result.is_some());
}

#[test]
fn results_with_sensitive_data_are_rejected() {
    let mut value = cli_result();
    value["runs"][0]["values"]["instructions@process-tree-waited"]["reason"] =
        json!("нет доступа к /home/alice/.config/perf");
    result_rejected(
        &value,
        None,
        "sensitive_value",
        "runs[0].values.instructions@process-tree-waited.reason",
    );

    let mut value = cli_result();
    value["environment"]["cpu_model"] =
        json!({"status": "known", "value": "token ghp_abcdefghijklmnopqrstu"});
    result_rejected(
        &value,
        None,
        "sensitive_value",
        "environment.cpu_model.value",
    );

    let mut value = cli_result();
    value["provenance"]["runner"]["name"] = json!("someone@example.org");
    result_rejected(&value, None, "sensitive_value", "provenance.runner.name");
}

#[test]
fn results_are_checked_against_the_declaration() {
    let catalog = load_example();

    let mut value = cli_result();
    value["scenario"]["id"] = json!("demo-cli.unknown");
    result_rejected(&value, Some(&catalog), "unknown_scenario", "scenario.id");

    let mut value = cli_result();
    value["metrics"].as_array_mut().unwrap().remove(3);
    for run in value["runs"].as_array_mut().unwrap() {
        run["values"]
            .as_object_mut()
            .unwrap()
            .remove("cache_misses@process-tree-waited");
    }
    result_rejected(
        &value,
        Some(&catalog),
        "missing_metric",
        "metrics.cache_misses@process-tree-waited",
    );

    let mut value = cli_result();
    value["inputs"][0]["size"] = json!({});
    result_rejected(
        &value,
        Some(&catalog),
        "missing_size",
        "inputs.sample.size.bytes",
    );

    let mut value = cli_result();
    value["profile"] = json!({"id": "cold", "cache": "cold", "network": "online"});
    result_rejected(&value, Some(&catalog), "unknown_profile", "profile.id");

    let mut value = cli_result();
    value["profile"]["network"] = json!("offline");
    result_rejected(&value, Some(&catalog), "catalog_mismatch", "profile");

    let mut value = cli_result();
    value["environment"]["gpu"] = json!({"status": "known", "value": "x"});
    result_rejected(&value, Some(&catalog), "unknown_fact", "environment.gpu");

    // Меньше min_runs при success.
    let mut value = cli_result();
    value["runs"].as_array_mut().unwrap().truncate(2);
    let mut parsed: BenchmarkResult = serde_json::from_value(value).unwrap();
    parsed.recompute_aggregates();
    result_rejected(
        &serde_json::to_value(&parsed).unwrap(),
        Some(&catalog),
        "inconsistent_outcome",
        "runs",
    );

    // Обязательная метрика измерена не полностью — success запрещён.
    let mut value = e2e_result();
    value["outcome"] = json!({"status": "success"});
    let checked = check_value(&value, Some(&catalog));
    assert_error(
        &checked.diagnostics,
        "inconsistent_outcome",
        None,
        Some("metrics.memory_peak_cgroup@cgroup"),
    );

    // Значение вне choices и необъявленный параметр.
    let mut value = e2e_result();
    value["parameters"]["release"] = json!("nightly");
    result_rejected(
        &value,
        Some(&catalog),
        "invalid_parameter",
        "parameters.release",
    );
    let mut value = e2e_result();
    value["parameters"]["threads"] = json!("4");
    result_rejected(
        &value,
        Some(&catalog),
        "unknown_parameter",
        "parameters.threads",
    );
}

#[test]
fn parameters_that_reach_no_command_are_reported() {
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "affects_comparability = true\n\n[[scenario.input]]",
        "affects_comparability = true\n\n[[scenario.parameter]]\nname = \"build\"\ndescription = \"Учебная сборка, которую никто не подставляет.\"\nrecord = true\naffects_comparability = false\n\n[[scenario.input]]",
    );
    let loaded = scratch.load();
    assert!(
        loaded.catalog.is_some(),
        "неиспользуемый параметр — предупреждение"
    );
    assert!(
        loaded
            .diagnostics
            .iter()
            .any(|d| d.code == "unused_parameter"
                && d.severity == Severity::Warning
                && d.to_string().contains("parameter[1]")),
        "{:?}",
        loaded.diagnostics
    );
    assert!(
        !catalog::load(&example_catalog())
            .diagnostics
            .iter()
            .any(|d| d.code == "unused_parameter"),
        "параметры учебного каталога подставляются"
    );
}

#[test]
fn profile_required_parameters_must_be_recorded() {
    let scratch = Scratch::new();
    scratch
        .edit(
            E2E,
            "affects_comparability = true\n\n[[scenario.input]]",
            "affects_comparability = true\n\n[[scenario.parameter]]\nname = \"build\"\ndescription = \"Учебная сборка для профиля cold.\"\nrecord = true\naffects_comparability = false\n\n[[scenario.input]]",
        )
        .edit(
            E2E,
            "retained = [\"общие кэши системы\"]",
            "retained = [\"общие кэши системы\"]\nargs_append = [\"--build\", \"{param.build}\"]\nrequired_parameters = [\"build\"]",
        );
    let loaded = scratch.load();
    let catalog = loaded.catalog.unwrap_or_else(|| {
        panic!(
            "{}",
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    });

    let value = e2e_result();
    result_rejected(
        &value,
        Some(&catalog),
        "missing_parameter",
        "parameters.build",
    );
    let mut value = e2e_result();
    value["parameters"]["build"] = json!("nightly-1");
    assert!(check_value(&value, Some(&catalog)).result.is_some());
}

#[test]
fn private_parameters_never_reach_published_results() {
    let catalog = catalog::load(&repository_catalog()).catalog.unwrap();
    let mut value = cli_result();
    let revision = catalog
        .scenario("anki-repo.validate")
        .unwrap()
        .scenario
        .revision;
    value["scenario"] = json!({"id": "anki-repo.validate", "module": "anki-repo", "revision": revision, "kind": "short-cli"});
    value["parameters"] = json!({"export_dir": "decks/output/Example"});
    result_rejected(
        &value,
        Some(&catalog),
        "unpublishable_parameter",
        "parameters.export_dir",
    );
}

#[test]
fn historical_revision_is_a_warning_not_a_false_match() {
    let catalog = load_example();
    let mut value = cli_result();
    value["scenario"]["revision"] = json!(7);
    let checked = check_value(&value, Some(&catalog));
    assert!(checked.result.is_some());
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| d.code == "historical_revision" && d.severity == Severity::Warning)
    );
}

// --- Импорт Hyperfine 2.0 ---

fn hyperfine_text() -> String {
    fs::read_to_string(fixtures().join("hyperfine/demo-cli.json")).unwrap()
}

/// Результат, собранный так, как это сделает исполнитель: значения — из
/// экспорта Hyperfine, остальное — из декларации и окружения.
fn result_from_hyperfine(catalog: &Catalog, text: &str) -> BenchmarkResult {
    let scenario = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let export = hyperfine::parse(text).unwrap();
    let imported = hyperfine::import(
        &export,
        0,
        &scenario.metrics,
        &scenario.target.expect_exit_codes,
    )
    .unwrap();
    let known = |value: &str| Fact::Known {
        value: value.to_owned(),
    };
    let all_success = imported
        .runs
        .iter()
        .all(|r| r.status == result::RunStatus::Success);
    BenchmarkResult {
        schema_version: 1,
        scenario: ScenarioRef {
            id: scenario.id.clone(),
            module: "demo-cli".to_owned(),
            revision: scenario.revision,
            kind: scenario.kind,
        },
        profile: ProfileRef {
            id: "warm".to_owned(),
            cache: CacheState::Warm,
            network: Network::Online,
        },
        parameters: BTreeMap::new(),
        inputs: vec![InputRecord {
            id: "sample".to_owned(),
            role: InputRole::Workload,
            identity: Identity {
                method: IdentityMethod::FileSha256,
                value: "39cfd55e2dfac1abb10a02ba706d8131950347a02f6ab025d21553198e690863"
                    .to_owned(),
            },
            size: [(
                "bytes".to_owned(),
                Quantity {
                    value: 102.0,
                    unit: Unit::Byte,
                },
            )]
            .into(),
        }],
        provenance: Provenance {
            recorded_at: imported.start_time.clone().unwrap(),
            measurement: imported.measurement.clone(),
            runner: None,
            code: CodeState {
                module_version: "0.1.0".to_owned(),
                source_tree_sha256: None,
                git_commit: None,
                dirty: None,
            },
        },
        environment: [
            ("os".to_owned(), known("linux")),
            (
                "arch".to_owned(),
                known(imported.platform.architecture.as_deref().unwrap()),
            ),
            ("cpu_model".to_owned(), known("Учебный CPU")),
            ("module_version".to_owned(), known("0.1.0")),
            (
                "hyperfine_version".to_owned(),
                known(&imported.measurement.version),
            ),
        ]
        .into(),
        outcome: if all_success {
            Outcome {
                status: OutcomeStatus::Success,
                reason: None,
            }
        } else {
            Outcome {
                status: OutcomeStatus::Partial,
                reason: Some("часть прогонов неуспешна".to_owned()),
            }
        },
        observation: if imported.runs.len() > 1 {
            Observation::Repeated
        } else {
            Observation::Single
        },
        warmup_runs: 2,
        metrics: imported.metrics,
        runs: imported.runs,
        aggregates: imported.aggregates,
    }
}

#[test]
fn hyperfine_export_imports_into_a_valid_result() {
    let catalog = load_example();
    let built = result_from_hyperfine(&catalog, &hyperfine_text());
    let text = serde_json::to_string_pretty(&built).unwrap();
    let checked = result::check(&text, "imported.json", Some(&catalog));
    let parsed = checked
        .result
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    assert_eq!(parsed, built);

    assert_eq!(parsed.runs.len(), 3);
    assert_eq!(
        parsed.provenance.measurement.format.as_deref(),
        Some(hyperfine::FORMAT)
    );
    let instructions = parsed
        .metrics
        .iter()
        .find(|m| m.key == "instructions@process-tree-waited")
        .unwrap();
    assert_eq!(instructions.availability, result::Availability::Measured);
    // cache_misses нет в экспорте: значение — статус с причиной, а не ноль.
    let misses = parsed
        .metrics
        .iter()
        .find(|m| m.key == "cache_misses@process-tree-waited")
        .unwrap();
    assert_eq!(misses.availability, result::Availability::Unavailable);
    assert!(matches!(
        parsed.runs[0].values["cache_misses@process-tree-waited"],
        result::MetricValue::Unavailable { .. }
    ));
    let aggregate = parsed.aggregates["time_wall_clock@command"];
    assert_eq!(aggregate.count, 3);
    assert!(aggregate.stddev.is_some());
}

#[test]
fn hyperfine_lists_unrequested_metrics_instead_of_dropping_them() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let export = hyperfine::parse(&hyperfine_text()).unwrap();
    let imported = hyperfine::import(&export, 0, &scenario.metrics, &[0]).unwrap();
    assert_eq!(
        imported.unrequested,
        ["time_cpu", "time_system", "time_user"]
    );
}

#[test]
fn hyperfine_failed_runs_become_partial_results() {
    let catalog = load_example();
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["measurements"][1]["exit_code"] = json!(2);
    export["results"][0]["measurements"][2]["exit_code"] = Value::Null;
    let built = result_from_hyperfine(&catalog, &export.to_string());
    assert_eq!(built.outcome.status, OutcomeStatus::Partial);
    assert_eq!(built.runs[1].status, result::RunStatus::Failure);
    assert!(built.runs[2].reason.as_deref().unwrap().contains("сигнал"));
    // Агрегаты — только по успешным прогонам.
    assert_eq!(built.aggregates["time_wall_clock@command"].count, 1);
    let checked = result::check(
        &serde_json::to_string(&built).unwrap(),
        "partial.json",
        Some(&catalog),
    );
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);
}

#[test]
fn hyperfine_versions_and_inconsistent_summaries_are_refused() {
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export.as_object_mut().unwrap().remove("schema_version");
    assert_eq!(
        hyperfine::parse(&export.to_string()).unwrap_err(),
        ImportError::UnsupportedVersion(1)
    );
    export["schema_version"] = json!(3);
    assert_eq!(
        hyperfine::parse(&export.to_string()).unwrap_err(),
        ImportError::UnsupportedVersion(3)
    );

    let catalog = load_example();
    let scenario = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["summary"]["time_wall_clock"]["mean"] = json!(9.0);
    let parsed = hyperfine::parse(&export.to_string()).unwrap();
    assert!(matches!(
        hyperfine::import(&parsed, 0, &scenario.metrics, &[0]),
        Err(ImportError::Invalid(_))
    ));

    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["measurements"][0]["memory_peak_resident"]["unit"] = json!("MiB");
    let parsed = hyperfine::parse(&export.to_string()).unwrap();
    assert!(matches!(
        hyperfine::import(&parsed, 0, &scenario.metrics, &[0]),
        Err(ImportError::Invalid(_))
    ));
}

#[test]
fn hyperfine_summary_is_required_for_complete_metrics_only() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let import = |export: &Value| {
        let parsed = hyperfine::parse(&export.to_string()).unwrap();
        hyperfine::import(&parsed, 0, &scenario.metrics, &[0])
    };

    // Метрика есть во всех measurements: Hyperfine 2.0 пишет сводку всегда.
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["summary"]
        .as_object_mut()
        .unwrap()
        .remove("time_wall_clock");
    match import(&export) {
        Err(ImportError::Invalid(reason)) => {
            assert!(reason.contains("summary.time_wall_clock"), "{reason}")
        }
        other => panic!("удалённая сводка должна отвергаться: {other:?}"),
    }
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]
        .as_object_mut()
        .unwrap()
        .remove("summary");
    assert!(matches!(import(&export), Err(ImportError::Invalid(_))));

    // Неполный набор значений: upstream сводку не создаёт, это не ошибка.
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["measurements"][1]
        .as_object_mut()
        .unwrap()
        .remove("instructions");
    export["results"][0]["summary"]
        .as_object_mut()
        .unwrap()
        .remove("instructions");
    let imported = import(&export).unwrap();
    let instructions = imported
        .metrics
        .iter()
        .find(|m| m.key == "instructions@process-tree-waited")
        .unwrap();
    assert_eq!(instructions.availability, result::Availability::Partial);
    assert_eq!(
        imported.aggregates["instructions@process-tree-waited"].count,
        2
    );

    // Сводка Hyperfine считается по всем measurements, включая неуспешные,
    // а агрегат контракта — только по успешным.
    let mut export: Value = serde_json::from_str(&hyperfine_text()).unwrap();
    export["results"][0]["measurements"][1]["exit_code"] = json!(2);
    let imported = import(&export).unwrap();
    assert_eq!(imported.aggregates["time_wall_clock@command"].count, 2);
    export["results"][0]["summary"]["time_wall_clock"]["count"] = json!(2);
    assert!(matches!(import(&export), Err(ImportError::Invalid(_))));
}

// --- Сопоставимость ---

#[test]
fn comparability_requires_same_workload_profile_and_environment() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let base: BenchmarkResult = serde_json::from_value(cli_result()).unwrap();

    let verdict = comparability::compare(scenario, &base, &base.clone());
    assert!(verdict.comparable, "{:?}", verdict.blockers);
    assert!(verdict.statistically_supported);
    assert_eq!(verdict.statistical_metrics, verdict.metrics);
    assert_eq!(
        verdict.metrics,
        [
            "time_wall_clock@command",
            "memory_peak_resident@largest-process-in-tree"
        ]
    );

    let mut other = base.clone();
    other.inputs[0].size.insert(
        "bytes".to_owned(),
        Quantity {
            value: 4096.0,
            unit: Unit::Byte,
        },
    );
    assert!(!comparability::compare(scenario, &base, &other).comparable);

    let mut other = base.clone();
    other.environment.insert(
        "cpu_model".to_owned(),
        Fact::Known {
            value: "Другой CPU".to_owned(),
        },
    );
    assert!(!comparability::compare(scenario, &base, &other).comparable);

    let mut other = base.clone();
    other.environment.insert(
        "cpu_model".to_owned(),
        Fact::Unavailable {
            reason: "нет /proc/cpuinfo".to_owned(),
        },
    );
    assert!(!comparability::compare(scenario, &base, &other).comparable);

    let mut other = base.clone();
    other.profile = ProfileRef {
        id: "offline-warm".to_owned(),
        cache: CacheState::Warm,
        network: Network::Offline,
    };
    assert!(!comparability::compare(scenario, &base, &other).comparable);

    let mut other = base.clone();
    other.scenario.revision = 2;
    assert!(!comparability::compare(scenario, &base, &other).comparable);
}

#[test]
fn subject_differences_are_expected_and_single_runs_are_flagged() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-e2e.suite").unwrap().scenario;
    let mut base: BenchmarkResult = serde_json::from_value(e2e_result()).unwrap();
    base.environment.insert(
        "runner_flavor".to_owned(),
        Fact::Known {
            value: "учебный".to_owned(),
        },
    );
    let mut other = base.clone();
    other.inputs[0].identity.value = "c".repeat(64);
    let verdict = comparability::compare(scenario, &base, &other);
    assert!(verdict.comparable, "{:?}", verdict.blockers);
    assert_eq!(verdict.subject_differences.len(), 1);
    assert!(
        !verdict.statistically_supported,
        "одиночные наблюдения не дают статистики"
    );

    other
        .parameters
        .insert("release".to_owned(), "beta".to_owned());
    assert!(!comparability::compare(scenario, &base, &other).comparable);
}

/// Серия demo-e2e из успешного и неуспешного прогона: две попытки, одно измерение.
fn e2e_success_and_failure() -> BenchmarkResult {
    let mut result: BenchmarkResult = serde_json::from_value(e2e_result()).unwrap();
    result.environment.insert(
        "runner_flavor".to_owned(),
        Fact::Known {
            value: "учебный".to_owned(),
        },
    );
    let mut failed = result.runs[0].clone();
    failed.index = 1;
    failed.status = result::RunStatus::Failure;
    failed.exit_code = Some(1);
    failed.reason = Some("сценарий завершился кодом 1".to_owned());
    for value in failed.values.values_mut() {
        *value = result::MetricValue::Failed {
            reason: "прогон неуспешен".to_owned(),
        };
    }
    result.runs.push(failed);
    result.observation = Observation::Repeated;
    for metric in &mut result.metrics {
        if metric.availability == result::Availability::Measured {
            metric.availability = result::Availability::Partial;
            metric.reason = Some("второй прогон неуспешен".to_owned());
        }
    }
    result.recompute_aggregates();
    result
}

#[test]
fn statistics_need_two_successful_values_per_metric() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-e2e.suite").unwrap().scenario;
    let series = e2e_success_and_failure();
    let checked = check_value(&serde_json::to_value(&series).unwrap(), Some(&catalog));
    assert!(checked.result.is_some(), "{:?}", checked.diagnostics);

    let verdict = comparability::compare(scenario, &series, &series.clone());
    assert!(verdict.comparable, "{:?}", verdict.blockers);
    assert!(
        !verdict.statistically_supported,
        "две попытки с одним успешным измерением не дают статистики"
    );
    assert!(verdict.statistical_metrics.is_empty());
    assert!(!verdict.metrics.is_empty());

    // Метрика с одним значением не получает статистику, даже если остальные её имеют.
    let cli = &catalog.scenario("demo-cli.count-bytes").unwrap().scenario;
    let base: BenchmarkResult = serde_json::from_value(cli_result()).unwrap();
    let mut other = base.clone();
    let memory = "memory_peak_resident@largest-process-in-tree";
    for run in &mut other.runs[1..] {
        run.values.insert(
            memory.to_owned(),
            result::MetricValue::Unavailable {
                reason: "значение не прочитано".to_owned(),
            },
        );
    }
    let verdict = comparability::compare(cli, &base, &other);
    assert!(verdict.comparable, "{:?}", verdict.blockers);
    assert!(verdict.metrics.iter().any(|m| m == memory));
    assert_eq!(verdict.statistical_metrics, ["time_wall_clock@command"]);
    assert!(!verdict.statistically_supported);
}

#[test]
fn aggregate_uses_sample_standard_deviation() {
    let aggregate = Aggregate::compute(&[1.0, 2.0, 3.0, 4.0]).unwrap();
    assert_eq!(aggregate.median, 2.5);
    assert!((aggregate.stddev.unwrap() - 1.290_994_448_735_805_6).abs() < 1e-12);
    assert_eq!(Aggregate::compute(&[5.0]).unwrap().stddev, None);
    assert!(Aggregate::compute(&[]).is_none());
}

// --- CLI ---

fn cli(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_benchmark-contract"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn cli_validates_lists_and_checks_results() {
    let catalog = example_catalog();
    let catalog = catalog.to_str().unwrap();
    let (code, stdout, _) = cli(&["--catalog", catalog, "--json", "validate"]);
    assert_eq!(code, 0);
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["scenarios"], json!(2));

    let (code, stdout, _) = cli(&["--catalog", catalog, "--json", "list"]);
    assert_eq!(code, 0);
    let list: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(list["scenarios"][1]["id"], json!("demo-e2e.suite"));
    assert_eq!(list["scenarios"][1]["status"], json!("example"));

    let result = fixtures().join("results/demo-cli.success.json");
    let (code, _, _) = cli(&[
        "--catalog",
        catalog,
        "check-result",
        result.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);

    let (code, stdout, _) = cli(&[
        "--catalog",
        catalog,
        "--json",
        "compare",
        result.to_str().unwrap(),
        result.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let verdict: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(verdict["verdict"]["comparable"], json!(true));

    // Одноимённые файлы из разных каталогов различимы в отчёте.
    let scratch = Scratch::new();
    let copy = scratch.path("copy/demo-cli.success.json");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&result, &copy).unwrap();
    let (code, stdout, _) = cli(&[
        "--catalog",
        catalog,
        "--json",
        "check-result",
        result.to_str().unwrap(),
        copy.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_ne!(report["results"][0]["file"], report["results"][1]["file"]);

    let (code, stdout, _) = cli(&["--catalog", catalog, "show", "demo-cli.count-bytes"]);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("# demo-cli.count-bytes"), "{stdout}");
    let (code, stdout, _) = cli(&[
        "--catalog",
        catalog,
        "--json",
        "show",
        "demo-cli.count-bytes",
    ]);
    assert_eq!(code, 0);
    let shown: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(shown["scenario"]["id"], json!("demo-cli.count-bytes"));

    // Итог preflight зависит от окружения, поэтому проверяется форма ответа.
    let (_, stdout, _) = cli(&[
        "--catalog",
        catalog,
        "--json",
        "preflight",
        "demo-cli.count-bytes",
    ]);
    let preflight: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(preflight["scenario"], json!("demo-cli.count-bytes"));
    assert!(preflight["checks"].is_array());

    let (code, stdout, _) = cli(&["--json", "vocabulary"]);
    assert_eq!(code, 0);
    let vocabulary: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(vocabulary["result_schema_version"], json!(1));
    assert!(vocabulary["metrics"].as_array().unwrap().len() > 10);
}

#[test]
fn cli_compare_does_not_promise_statistics_for_single_measurements() {
    let scratch = Scratch::new();
    let file = scratch.path("results/partial.json");
    let text = serde_json::to_string_pretty(&e2e_success_and_failure()).unwrap();
    scratch.write("results/partial.json", &text);
    let catalog = example_catalog();
    let args = |json: bool| {
        let mut args = vec!["--catalog", catalog.to_str().unwrap()];
        if json {
            args.push("--json");
        }
        args.extend(["compare", file.to_str().unwrap(), file.to_str().unwrap()]);
        cli(&args)
    };
    let (code, stdout, _) = args(true);
    assert_eq!(code, 0, "{stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["verdict"]["comparable"], json!(true));
    assert_eq!(report["verdict"]["statistically_supported"], json!(false));
    let (code, stdout, _) = args(false);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("статистические выводы недопустимы")
            && stdout.contains("time_wall_clock@command"),
        "{stdout}"
    );
}

#[test]
fn preflight_reports_unsupported_platform() {
    let catalog = load_example();
    let scenario = &catalog.scenario("demo-e2e.suite").unwrap().scenario;
    let check = preflight::platform(scenario, "windows");
    assert_eq!(check.status, preflight::Status::Unsupported);
    assert!(check.detail.contains("linux"), "{}", check.detail);
    assert_eq!(
        preflight::platform(scenario, "linux").status,
        preflight::Status::Available
    );
    let checks = preflight::run(scenario);
    assert_eq!(checks[0].requirement, "platform");

    // Платформа, отличная от текущей, даёт отрицательный итог независимо от программ.
    let foreign = if std::env::consts::OS == "windows" {
        "linux"
    } else {
        "windows"
    };
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "platforms = [\"linux\", \"macos\"]",
        &format!("platforms = [\"{foreign}\"]"),
    );
    let (code, stdout, stderr) = cli(&[
        "--catalog",
        scratch.root.to_str().unwrap(),
        "--json",
        "preflight",
        "demo-cli.count-bytes",
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["checks"][0]["requirement"], json!("platform"));
    assert_eq!(report["checks"][0]["status"], json!("unsupported"));
}

#[test]
fn cli_reports_invalid_catalogs_and_results_with_exit_code_one() {
    let scratch = Scratch::new();
    scratch.edit(CLI, "max_runs = 20", "max_runs = 100000");
    let (code, _, stderr) = cli(&["--catalog", scratch.root.to_str().unwrap(), "validate"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("unbounded_runs")
            && stderr.contains("demo-cli.count-bytes")
            && stderr.contains("runs.max_runs"),
        "{stderr}"
    );

    let hyperfine_export = fixtures().join("hyperfine/demo-cli.json");
    let (code, _, stderr) = cli(&[
        "check-result",
        "--no-catalog",
        hyperfine_export.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "экспорт Hyperfine — не формат результата проекта");
    assert!(stderr.contains("unsupported_schema_version"), "{stderr}");

    for command in ["validate", "list"] {
        let (code, _, stderr) = cli(&["--catalog", "/nonexistent-benchmark-catalog", command]);
        assert_eq!(
            code, 3,
            "{command}: отсутствующий каталог — не ошибка контракта"
        );
        assert!(stderr.contains("catalog.toml"), "{stderr}");
    }
}

#[test]
fn measurement_sources_must_name_their_origin() {
    // Счётчик отчёта модуля без указателя.
    let scratch = Scratch::new();
    scratch.edit(E2E, "pointer = \"/checks/*/count\"\n", "");
    rejected(
        &scratch,
        "missing_field",
        Some("demo-e2e.suite"),
        Some("metric[5].pointer"),
    );

    // Значение из отчёта без объявленного module_report.
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "[scenario.module_report]\nruns_dir = \"runs\"\nreport = \"report.json\"\ndescription = \"Учебный отчёт: каждый вызов создаёт runs/<id>/report.json.\"\n",
        "",
    );
    rejected(
        &scratch,
        "missing_module_report",
        Some("demo-e2e.suite"),
        Some("metric[5].pointer"),
    );
    assert_error(
        &scratch.load().diagnostics,
        "missing_module_report",
        Some("demo-e2e.suite"),
        Some("input[1].report_identity"),
    );

    // Указатель вне синтаксиса JSON Pointer.
    let scratch = Scratch::new();
    scratch.edit(
        E2E,
        "pointer = \"/checks/*/count\"",
        "pointer = \"checks.count\"",
    );
    rejected(
        &scratch,
        "invalid_pointer",
        Some("demo-e2e.suite"),
        Some("metric[5].pointer"),
    );

    // Значение пробы без пробы входа.
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "measure = { kind = \"tree-bytes\" }",
        "measure = { kind = \"probe\", pointer = \"/bytes\" }",
    );
    rejected(
        &scratch,
        "missing_field",
        Some("demo-cli.count-bytes"),
        Some("input[0].size[0].measure"),
    );

    // file-bytes ищет файл внутри каталога: у входа-файла его нет.
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "measure = { kind = \"tree-bytes\" }",
        "measure = { kind = \"file-bytes\", path = \"sample.txt\" }",
    );
    rejected(
        &scratch,
        "contradictory_input",
        Some("demo-cli.count-bytes"),
        Some("input[0].size[0].measure"),
    );

    // Способ объёма определяет единицу.
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "measure = { kind = \"tree-bytes\" }",
        "measure = { kind = \"tree-files\" }",
    );
    rejected(
        &scratch,
        "unit_mismatch",
        Some("demo-cli.count-bytes"),
        Some("input[0].size[0].unit"),
    );

    // Проба входа только читает.
    let scratch = Scratch::new();
    scratch.edit(
        CLI,
        "measure = { kind = \"tree-bytes\" }",
        "measure = { kind = \"probe\", pointer = \"/bytes\" }",
    );
    let text = fs::read_to_string(scratch.path(CLI)).unwrap();
    let probe = "\n[scenario.input.probe]\ndescription = \"Проба.\"\nprogram = { kind = \"executable\", name = \"demo-cli\" }\nargs = [\"--size\", \"{input.sample}\"]\ntouches = \"module-owned\"\ntimeout_seconds = 10\n";
    let marker = "\n[scenario.runs]";
    scratch.write(CLI, &text.replacen(marker, &format!("{probe}{marker}"), 1));
    rejected(
        &scratch,
        "probe_not_read_only",
        Some("demo-cli.count-bytes"),
        Some("input[0].probe.touches"),
    );

    // disk_usage обязан назвать путь, а путь не выходит за корень.
    let disk = "\n[[scenario.metric]]\nmetric = \"disk_usage\"\nscope = \"path\"\nsource = \"filesystem\"\nlabel = \"cache\"\nrequired = false\n";
    let scratch = Scratch::new();
    let text = fs::read_to_string(scratch.path(E2E)).unwrap();
    let marker = "\n[scenario.module_report]";
    scratch.write(E2E, &text.replacen(marker, &format!("{disk}{marker}"), 1));
    rejected(
        &scratch,
        "missing_field",
        Some("demo-e2e.suite"),
        Some("metric[6].path"),
    );
    let scratch = Scratch::new();
    scratch.write(
        E2E,
        &text.replacen(marker, &format!("{disk}path = \"../outside\"\n{marker}"), 1),
    );
    rejected(
        &scratch,
        "invalid_path",
        Some("demo-e2e.suite"),
        Some("metric[6].path"),
    );

    // Контейнер нагрузки требует Docker и метку вида ключ=значение.
    let scratch = Scratch::new();
    let text = fs::read_to_string(scratch.path(E2E)).unwrap();
    scratch.write(
        E2E,
        &text.replacen(
            marker,
            &format!("\n[scenario.container]\nlabel = \"demo\"\nowner_pid_label = \"demo.pid\"\ndescription = \"Учебный контейнер.\"\n{marker}"),
            1,
        ),
    );
    let loaded = scratch.load();
    assert_error(
        &loaded.diagnostics,
        "invalid_container",
        Some("demo-e2e.suite"),
        Some("container.label"),
    );
    assert_error(
        &loaded.diagnostics,
        "missing_requirement",
        Some("demo-e2e.suite"),
        Some("container"),
    );
}

#[test]
fn inputs_may_stay_unresolved_only_without_successful_runs() {
    let catalog = load_example();
    // Прогон прервался до того, как отчёт сообщил идентичность артефакта.
    let mut value = e2e_result();
    value["inputs"]
        .as_array_mut()
        .unwrap()
        .retain(|input| input["id"] != "fixture-archive");
    value["outcome"] =
        json!({"status": "failure", "reason": "прогон прерван до загрузки артефакта"});
    value["runs"][0]["status"] = json!("interrupted");
    value["runs"][0]["reason"] = json!("прерван сигналом");
    value.as_object_mut().unwrap().remove("aggregates");
    let keys: Vec<String> = value["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap().to_owned())
        .collect();
    for (index, key) in keys.iter().enumerate() {
        value["runs"][0]["values"][key] = json!({"status": "failed", "reason": "прогон прерван"});
        value["metrics"][index]["availability"] = json!("failed");
        value["metrics"][index]["reason"] = json!("прогон прерван");
    }
    let checked = check_value(&value, Some(&catalog));
    assert!(checked.result.is_some(), "{:#?}", checked.diagnostics);

    // Успешный прогон без идентичности нагрузки сопоставлять не с чем.
    let mut value = e2e_result();
    value["inputs"]
        .as_array_mut()
        .unwrap()
        .retain(|input| input["id"] != "fixture-archive");
    result_rejected(
        &value,
        Some(&catalog),
        "missing_input",
        "inputs.fixture-archive",
    );
}
