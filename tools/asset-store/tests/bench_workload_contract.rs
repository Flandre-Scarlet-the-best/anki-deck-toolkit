//! Контракт исполнителя нагрузки `asset-store-bench`: настоящие операции
//! хранилища, постусловия, переиспользование эталона, реакция на повреждение
//! и границы удаления. Тесты строят собственные крошечные спецификации во
//! временном каталоге; поставляемые спецификации только разбираются.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use asset_store::bench_workload::{
    self, GENERATOR_VERSION, MARKER_FILE, Operation, SpecDomain, WorkloadSpec,
};
use asset_store::temp_workspace::TempWorkspace;

const BIN: &str = env!("CARGO_BIN_EXE_asset-store-bench");

fn spec_text(domain: &str, seed: u64, png_every: u32) -> String {
    format!(
        "generator = {GENERATOR_VERSION}\ndomain = \"{domain}\"\nseed = {seed}\nassets = 6\n\
         png_every = {png_every}\nimage_side = 16\nreads = 6\ningest = 3\nbatch_entries = 5\n\
         batch_blobs = 2\nblob_bytes = 64\n"
    )
}

struct Fixture {
    workspace: TempWorkspace,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self {
            workspace: TempWorkspace::create(&format!("bench-workload-{label}"))
                .expect("временный каталог создаётся"),
        }
    }

    fn spec(&self, name: &str, text: &str) -> PathBuf {
        let path = self.workspace.path().join(name);
        fs::write(&path, text).expect("спецификация записывается");
        path
    }

    fn workdir(&self) -> PathBuf {
        self.workspace.path().join("work")
    }
}

fn op(name: &str) -> Operation {
    match name {
        "contention" => Operation::IngestContention { writers: 3 },
        "unchanged" => Operation::BatchCheckpoint { modified: false },
        "modified" => Operation::BatchCheckpoint { modified: true },
        other => Operation::parse(other, None, None).expect("операция известна"),
    }
}

fn run_and_verify(spec: &Path, workdir: &Path, operation: Operation) -> u64 {
    if operation.mutates() {
        bench_workload::prepare(spec, workdir).expect("рабочая копия готовится");
    }
    let summary = bench_workload::run(spec, workdir, operation).expect("операция выполняется");
    bench_workload::verify(spec, workdir, operation).expect("постусловия выполнены");
    summary.items
}

#[test]
fn every_operation_passes_its_postconditions_for_both_domains() {
    for (domain, png_every, records) in [("kanji", 2, 9), ("generic", 0, 6)] {
        let fixture = Fixture::new(domain);
        let spec = fixture.spec("spec.toml", &spec_text(domain, 1, png_every));
        let workdir = fixture.workdir();
        let setup = bench_workload::setup(&spec, &workdir).expect("эталон строится");
        assert_eq!(setup.items, records);
        assert!(!setup.reused);
        assert_eq!(
            run_and_verify(&spec, &workdir, op("verify-integrity")),
            records
        );
        assert_eq!(run_and_verify(&spec, &workdir, op("read-batch")), 6);
        assert_eq!(run_and_verify(&spec, &workdir, op("read-single")), 6);
        assert_eq!(run_and_verify(&spec, &workdir, op("batch-reopen")), 5);
        assert_eq!(run_and_verify(&spec, &workdir, op("ingest")), 3);
        assert_eq!(run_and_verify(&spec, &workdir, op("contention")), 3);
        assert_eq!(run_and_verify(&spec, &workdir, op("unchanged")), 5);
        assert_eq!(run_and_verify(&spec, &workdir, op("modified")), 5);
        bench_workload::clean(&workdir, false).expect("каталог удаляется");
        assert!(!workdir.exists());
    }
}

#[test]
fn mutating_operation_requires_equivalent_start_state() {
    let fixture = Fixture::new("start-state");
    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 0));
    let workdir = fixture.workdir();
    bench_workload::setup(&spec, &workdir).unwrap();
    let ingest = op("ingest");
    assert!(
        bench_workload::run(&spec, &workdir, ingest).is_err(),
        "без prepare рабочей копии нет"
    );
    bench_workload::prepare(&spec, &workdir).unwrap();
    bench_workload::run(&spec, &workdir, ingest).unwrap();
    // Повтор без восстановления — уже другая нагрузка: идентичности опубликованы,
    // и исполнитель отказывает, а не измеряет no-op.
    let repeated = bench_workload::run(&spec, &workdir, ingest).unwrap_err();
    assert!(!repeated.usage, "{repeated}");
    bench_workload::prepare(&spec, &workdir).unwrap();
    bench_workload::run(&spec, &workdir, ingest).unwrap();
    bench_workload::verify(&spec, &workdir, ingest).unwrap();
}

#[test]
fn template_is_reused_only_when_identical() {
    let fixture = Fixture::new("reuse");
    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 0));
    let workdir = fixture.workdir();
    assert!(!bench_workload::setup(&spec, &workdir).unwrap().reused);
    assert!(bench_workload::setup(&spec, &workdir).unwrap().reused);

    // Изменённое дерево эталона строится заново, а не принимается на веру.
    fs::write(workdir.join("template/store/.tmp/stray"), b"x").unwrap();
    assert!(!bench_workload::setup(&spec, &workdir).unwrap().reused);
    assert!(!workdir.join("template/store/.tmp/stray").exists());

    // Другая спецификация того же объёма — другой эталон.
    let other = fixture.spec("other.toml", &spec_text("kanji", 2, 0));
    assert!(!bench_workload::setup(&other, &workdir).unwrap().reused);
    assert!(bench_workload::run(&spec, &workdir, op("read-batch")).is_err());

    bench_workload::prepare(&other, &workdir).unwrap();
    bench_workload::clean(&workdir, true).unwrap();
    assert!(!workdir.join("run").exists());
    assert!(bench_workload::setup(&other, &workdir).unwrap().reused);
}

#[test]
fn read_only_operations_detect_changes_and_corruption() {
    let fixture = Fixture::new("corruption");
    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 0));
    let workdir = fixture.workdir();
    bench_workload::setup(&spec, &workdir).unwrap();
    let read = op("read-batch");
    bench_workload::run(&spec, &workdir, read).unwrap();

    // Посторонний файл в эталоне: постусловие только читающей операции ловит его.
    let stray = workdir.join("template/store/.tmp/stray");
    fs::write(&stray, b"x").unwrap();
    assert!(bench_workload::verify(&spec, &workdir, read).is_err());
    fs::remove_file(&stray).unwrap();
    bench_workload::verify(&spec, &workdir, read).unwrap();

    // Повреждённые байты ресурса: чтение отказывает сверкой SHA-256.
    let gif_dir = workdir.join("template/store/assets/gif");
    let victim = fs::read_dir(&gif_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = fs::read(&victim).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    // Хранилище публикует файлы только для чтения: повреждение моделируется в обход прав.
    make_writable(&victim);
    fs::write(&victim, &bytes).unwrap();
    assert!(bench_workload::run(&spec, &workdir, read).is_err());
    assert!(bench_workload::run(&spec, &workdir, op("read-single")).is_err());
}

fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn tamper(path: &Path) {
    make_writable(path);
    fs::write(path, b"tampered").unwrap();
}

#[test]
fn corrupted_batch_blob_fails_reopen_and_checkpoint() {
    let fixture = Fixture::new("batch-corruption");
    let spec = fixture.spec("spec.toml", &spec_text("generic", 1, 0));
    let workdir = fixture.workdir();
    bench_workload::setup(&spec, &workdir).unwrap();
    bench_workload::prepare(&spec, &workdir).unwrap();
    let blobs = workdir.join("run/store/.runtime/batches/bench-batch/candidates");
    let victim = fs::read_dir(&blobs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    tamper(&victim);
    assert!(bench_workload::run(&spec, &workdir, op("modified")).is_err());
    let template_blobs = workdir.join("template/store/.runtime/batches/bench-batch/candidates");
    let victim = fs::read_dir(&template_blobs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    tamper(&victim);
    assert!(bench_workload::run(&spec, &workdir, op("batch-reopen")).is_err());
}

#[test]
fn clean_and_setup_never_remove_foreign_directories() {
    let fixture = Fixture::new("foreign");
    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 0));
    let workdir = fixture.workdir();
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("user-data"), b"keep").unwrap();
    for error in [
        bench_workload::clean(&workdir, false).unwrap_err(),
        bench_workload::clean(&workdir, true).unwrap_err(),
        bench_workload::setup(&spec, &workdir).unwrap_err(),
    ] {
        assert!(error.usage, "{error}");
    }
    assert_eq!(fs::read(workdir.join("user-data")).unwrap(), b"keep");
    assert!(!workdir.join(MARKER_FILE).exists());
    // Отсутствующий каталог — успешная уборка.
    bench_workload::clean(&fixture.workspace.path().join("absent"), false).unwrap();
}

#[test]
fn binary_reports_usage_and_failure_with_distinct_codes() {
    let fixture = Fixture::new("binary");
    let wrong = fixture.spec(
        "wrong.toml",
        &spec_text("kanji", 1, 0).replace(
            &format!("generator = {GENERATOR_VERSION}"),
            &format!("generator = {}", GENERATOR_VERSION + 1),
        ),
    );
    let workdir = fixture.workdir();
    let status = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .output()
            .expect("исполнитель запускается")
    };
    let describe = status(&["describe", "--spec", wrong.to_str().unwrap()]);
    assert_eq!(describe.status.code(), Some(2));

    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 0));
    let spec = spec.to_str().unwrap();
    let workdir = workdir.to_str().unwrap();
    let missing = status(&["run", "ingest", "--spec", spec, "--workdir", workdir]);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "эталона нет — отказ операции"
    );
    let bad = status(&[
        "run",
        "ingest",
        "--spec",
        spec,
        "--workdir",
        workdir,
        "--writers",
        "2",
    ]);
    assert_eq!(bad.status.code(), Some(2));
    let described = status(&["describe", "--spec", spec]);
    assert_eq!(described.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&described.stdout).unwrap();
    assert_eq!(json["records"], 6);
    assert_eq!(json["ingest"], 3);
    assert_eq!(json["batch_blob_bytes"], 128);
}

/// Изменение байтов генератора при той же спецификации обязано повысить
/// `GENERATOR_VERSION`: иначе SHA-256 файла спецификации перестанет
/// описывать корпус. Тест фиксирует отпечаток содержимого генератора.
#[test]
fn generator_output_is_pinned_to_its_version() {
    assert_eq!(GENERATOR_VERSION, 1);
    for (domain, png_every, expected) in [
        (
            "kanji",
            2,
            "6b20969f5999c39535686cbac65c49c3371e15ea9116ac08825db06ace3f70ff",
        ),
        (
            "generic",
            0,
            "3417d3cde18aac07d3468968c476c427174770d1a4ac6490a53eb8fb88fda27d",
        ),
    ] {
        let spec: WorkloadSpec = toml::from_str(&spec_text(domain, 1, png_every)).unwrap();
        assert_eq!(
            bench_workload::describe(&spec).content_sha256,
            expected,
            "содержимое генератора {domain} изменилось: повысьте GENERATOR_VERSION и generator в спецификациях benchmarks/workloads/asset-store"
        );
    }
}

#[test]
fn shipped_workload_specs_are_valid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/workloads/asset-store");
    let mut seen = 0;
    for entry in fs::read_dir(&dir).expect("каталог спецификаций есть") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            let spec = WorkloadSpec::load(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(matches!(
                spec.domain,
                SpecDomain::Kanji | SpecDomain::Generic
            ));
            seen += 1;
        }
    }
    assert!(seen >= 3, "нужно несколько объёмов нагрузки");
}

/// Шаги одной декларации в порядке жизненного цикла исполнителя.
struct DeclaredSteps {
    id: String,
    choices: Vec<(String, Vec<String>)>,
    steps: Vec<(&'static str, Vec<String>)>,
}

fn argv(step: &toml::Value) -> Vec<String> {
    let program = &step["program"];
    assert_eq!(program["kind"].as_str(), Some("cargo-bin"));
    assert_eq!(program["package"].as_str(), Some("asset-store"));
    assert_eq!(program["bin"].as_str(), Some("asset-store-bench"));
    step["args"]
        .as_array()
        .expect("args — массив")
        .iter()
        .map(|arg| arg.as_str().expect("аргумент — строка").to_owned())
        .collect()
}

fn declared_scenarios() -> Vec<DeclaredSteps> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/scenarios/asset-store.toml");
    let catalog: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let list = |value: &toml::Value, key: &str| -> Vec<toml::Value> {
        value
            .get(key)
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    list(&catalog, "scenario")
        .iter()
        .map(|scenario| {
            let choices = list(scenario, "parameter")
                .iter()
                .filter_map(|parameter| {
                    let values: Vec<String> = parameter
                        .get("choices")?
                        .as_array()?
                        .iter()
                        .map(|choice| choice.as_str().unwrap().to_owned())
                        .collect();
                    Some((parameter["name"].as_str().unwrap().to_owned(), values))
                })
                .collect();
            let mut steps = Vec::new();
            for input in list(scenario, "input") {
                steps.push(("probe", argv(&input["probe"])));
            }
            for step in list(scenario, "setup") {
                steps.push(("setup", argv(&step)));
            }
            for profile in list(scenario, "profile") {
                for step in list(&profile, "prepare") {
                    steps.push(("prepare", argv(&step)));
                }
                steps.push(("target", argv(&scenario["target"])));
                for step in list(&profile, "conclude") {
                    steps.push(("conclude", argv(&step)));
                }
            }
            for step in list(scenario, "cleanup") {
                steps.push(("cleanup", argv(&step)));
            }
            DeclaredSteps {
                id: scenario["id"].as_str().unwrap().to_owned(),
                choices,
                steps,
            }
        })
        .collect()
}

/// Циклическое покрытие: max(число choices) запусков вместо произведения.
/// Это покрывает значения каждого параметра, но не обещает все сочетания.
fn covering_variants(choices: &[(String, Vec<String>)]) -> Vec<Vec<(String, String)>> {
    let count = choices
        .iter()
        .map(|(_, values)| values.len())
        .max()
        .unwrap_or(1);
    (0..count)
        .map(|index| {
            choices
                .iter()
                .map(|(name, values)| {
                    assert!(!values.is_empty(), "choices не пусты");
                    (name.clone(), values[index % values.len()].clone())
                })
                .collect()
        })
        .collect()
}

#[test]
fn multiple_choices_have_bounded_complete_value_coverage() {
    let choices = vec![
        ("first".into(), vec!["1".into(), "2".into()]),
        ("second".into(), vec!["a".into(), "b".into(), "c".into()]),
    ];
    let variants = covering_variants(&choices);
    assert_eq!(variants.len(), 3);
    for (name, values) in &choices {
        for value in values {
            assert!(
                variants
                    .iter()
                    .any(|variant| variant.contains(&(name.clone(), value.clone())))
            );
        }
    }
    assert_eq!(covering_variants(&[]), vec![vec![]]);
}

/// Декларации каталога исполнимы настоящим исполнителем: каждая подстановка,
/// каждое значение `choices` и каждый шаг жизненного цикла в том порядке, в
/// котором их выполняет `benchmark-runner`, на крошечной спецификации. Рабочий
/// каталог декларации заменяется временным, остальные аргументы — как есть.
#[test]
fn catalog_declarations_run_end_to_end_with_the_real_executor() {
    let fixture = Fixture::new("catalog");
    let spec = fixture.spec(
        "spec.toml",
        &spec_text("kanji", 3, 2).replace("ingest = 3", "ingest = 8"),
    );
    let workdir = fixture.workdir();
    let mut scenarios = declared_scenarios();
    // Два независимых перечисления с настоящими подстановками и CLI-шагами.
    // Второе выбирает допустимые алиасы одной и той же спецификации.
    let mut multiple = DeclaredSteps {
        id: "multiple-choices".into(),
        choices: vec![
            (
                "operation".into(),
                vec!["read-batch".into(), "read-single".into()],
            ),
            (
                "spec".into(),
                vec!["a.toml".into(), "b.toml".into(), "c.toml".into()],
            ),
        ],
        steps: vec![],
    };
    for name in ["a.toml", "b.toml", "c.toml"] {
        fixture.spec(name, &fs::read_to_string(&spec).unwrap());
    }
    for (phase, command) in [
        ("probe", "describe"),
        ("setup", "setup"),
        ("prepare", "prepare"),
        ("target", "run"),
        ("conclude", "verify"),
        ("cleanup", "clean"),
    ] {
        let mut args = vec![command.to_owned()];
        if matches!(command, "run" | "verify") {
            args.push("{param.operation}".into());
        }
        if command != "clean" {
            args.extend([
                "--spec".into(),
                fixture
                    .workspace
                    .path()
                    .join("{param.spec}")
                    .to_str()
                    .unwrap()
                    .into(),
            ]);
        }
        if command != "describe" {
            args.extend(["--workdir".into(), workdir.to_str().unwrap().into()]);
        }
        multiple.steps.push((phase, args));
    }
    scenarios.push(multiple);
    assert!(
        scenarios.len() >= 5,
        "сценарии asset-store зарегистрированы"
    );
    for scenario in scenarios {
        let variants = covering_variants(&scenario.choices);
        for variant in variants {
            for (phase, args) in &scenario.steps {
                let args: Vec<String> = args
                    .iter()
                    .map(|arg| {
                        let mut arg = arg.replace("{input.workload}", spec.to_str().unwrap());
                        for (name, value) in &variant {
                            arg = arg.replace(&format!("{{param.{name}}}"), value);
                        }
                        if arg == "target/benchmark-workloads/asset-store" {
                            arg = workdir.to_str().unwrap().to_owned();
                        }
                        assert!(
                            !arg.contains('{'),
                            "{}: неподставленный аргумент {arg}",
                            scenario.id
                        );
                        arg
                    })
                    .collect();
                let output = Command::new(BIN).args(&args).output().unwrap();
                assert!(
                    output.status.success(),
                    "{} {variant:?} {phase}: {}",
                    scenario.id,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    bench_workload::clean(&workdir, false).unwrap();
}

#[test]
fn expected_metadata_is_bound_to_completed_setup() {
    let fixture = Fixture::new("expected-tampering");
    let spec = fixture.spec("spec.toml", &spec_text("kanji", 1, 2));
    let workdir = fixture.workdir();
    bench_workload::setup(&spec, &workdir).unwrap();
    for field in [
        "reads",
        "ingest",
        "records",
        "store_digest",
        "batch_revision",
    ] {
        let path = workdir.join("template/expected.json");
        let mut expected: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match field {
            "reads" | "ingest" => {
                expected[field].as_array_mut().unwrap().pop();
            }
            "store_digest" => expected[field] = "changed".into(),
            _ => expected[field] = 0.into(),
        }
        fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();
        assert!(
            bench_workload::run(&spec, &workdir, op("read-batch")).is_err(),
            "{field}"
        );
        assert!(bench_workload::prepare(&spec, &workdir).is_err(), "{field}");
        assert!(
            !bench_workload::setup(&spec, &workdir).unwrap().reused,
            "{field}"
        );
        assert_eq!(run_and_verify(&spec, &workdir, op("read-batch")), 6);
        assert_eq!(run_and_verify(&spec, &workdir, op("ingest")), 3);
        assert!(bench_workload::setup(&spec, &workdir).unwrap().reused);
    }
    // Маркер прежнего формата остаётся своим для cleanup, но не подтверждает эталон.
    let path = workdir.join(MARKER_FILE);
    let mut marker: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    marker.as_object_mut().unwrap().remove("expected_sha256");
    fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
    assert!(!bench_workload::setup(&spec, &workdir).unwrap().reused);
}

#[test]
fn every_contention_writer_must_have_work() {
    let fixture = Fixture::new("active-writers");
    let small = fixture.spec(
        "small.toml",
        &spec_text("generic", 1, 0).replace("ingest = 3", "ingest = 1"),
    );
    let workdir = fixture.workdir();
    bench_workload::setup(&small, &workdir).unwrap();
    bench_workload::prepare(&small, &workdir).unwrap();
    let error = bench_workload::run(&small, &workdir, Operation::IngestContention { writers: 8 })
        .unwrap_err();
    assert!(error.usage && error.message.contains("ingest"), "{error}");
    // Отказ до публикации: один писатель всё ещё может опубликовать запись.
    assert_eq!(
        run_and_verify(&small, &workdir, Operation::IngestContention { writers: 1 }),
        1
    );
    let spec = fixture.spec(
        "spec.toml",
        &spec_text("generic", 1, 0).replace("ingest = 3", "ingest = 8"),
    );
    bench_workload::setup(&spec, &workdir).unwrap();
    for writers in [1, 2, 4, 8] {
        assert_eq!(
            run_and_verify(&spec, &workdir, Operation::IngestContention { writers }),
            8
        );
    }
}
