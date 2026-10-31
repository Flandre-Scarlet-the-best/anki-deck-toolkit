//! Типизированная модель деклараций сценариев (TOML) и разбор с точным путём поля.
//!
//! Serde сообщает об ошибке без пути к полю. Поэтому декларация сначала
//! проверяется по описанию формы (`Shape`): каждое неизвестное поле, пропуск
//! обязательного поля и неверное значение получают собственный путь. Только
//! после этого значение превращается в типы Rust.

use crate::vocabulary::{
    CacheState, MetricId, Network, Os, ScenarioStatus, Scope, Source, Unit, WorkloadKind,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Файл деклараций одного модуля: `scenarios/<модуль>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioFile {
    pub schema_version: u32,
    pub module: String,
    #[serde(default, rename = "scenario")]
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub title: String,
    /// Версия нагрузки и семантики сценария. Меняется при любом изменении,
    /// после которого старые и новые результаты уже не один и тот же бенчмарк.
    pub revision: u32,
    pub kind: WorkloadKind,
    pub status: ScenarioStatus,
    pub description: String,
    /// Что именно входит в измеряемый интервал.
    pub measured: String,
    pub platforms: Vec<Os>,
    pub target: Target,
    #[serde(default, rename = "parameter")]
    pub parameters: Vec<Parameter>,
    #[serde(default, rename = "input")]
    pub inputs: Vec<Input>,
    pub runs: RunPolicy,
    #[serde(default)]
    pub setup: Vec<Step>,
    #[serde(default)]
    pub cleanup: Vec<Step>,
    #[serde(rename = "profile")]
    pub profiles: Vec<ProfileBinding>,
    #[serde(rename = "metric")]
    pub metrics: Vec<MetricRequest>,
    pub environment: EnvironmentSpec,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    /// Машинный отчёт, который цель оставляет после каждого прогона.
    #[serde(default)]
    pub module_report: Option<ModuleReport>,
    /// Контейнер, в котором цель исполняет нагрузку: его cgroup — охват
    /// метрик `cgroup`, если они запрошены.
    #[serde(default)]
    pub container: Option<ContainerSpec>,
}

/// Где цель оставляет машинный отчёт прогона: новый каталог в `runs_dir`
/// (относительно корня репозитория) с файлом `report`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleReport {
    pub runs_dir: String,
    pub report: String,
    pub description: String,
}

/// Как опознать контейнер, созданный измеряемой целью.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSpec {
    /// Метка `<ключ>=<значение>`, которой цель помечает контейнер нагрузки.
    pub label: String,
    /// Метка с PID процесса, создавшего контейнер: контейнер принадлежит
    /// прогону, только если этот PID — процесс самой серии.
    pub owner_pid_label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Program {
    /// Исполняемый файл пакета рабочей области Cargo. Сборка — подготовка,
    /// а не измеряемая работа: исполнитель собирает его до `setup`.
    CargoBin {
        package: String,
        bin: String,
        #[serde(default = "default_cargo_profile")]
        cargo_profile: String,
    },
    /// Исполняемый файл из `PATH`.
    Executable { name: String },
}

fn default_cargo_profile() -> String {
    "release".to_owned()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvSpec {
    /// Переменные, которые исполнитель задаёт явно.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Имена переменных, унаследованных от исполнителя; всё остальное очищается.
    #[serde(default)]
    pub inherit: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub program: Program,
    #[serde(default)]
    pub args: Vec<String>,
    /// Каталог запуска относительно корня репозитория.
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: EnvSpec,
    pub expect_exit_codes: Vec<i32>,
}

fn default_working_dir() -> String {
    ".".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub choices: Vec<String>,
    /// Значение попадает в публикуемый результат.
    pub record: bool,
    /// Результаты с разными значениями не сопоставляются.
    pub affects_comparability: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    /// Отслеживаемое или неигнорируемое дерево рабочей копии.
    RepoTree,
    /// Конкретный отслеживаемый файл репозитория.
    RepoFile,
    /// Путь вне каталога, переданный параметром при запуске.
    ExternalPath,
    /// Загружаемый внешний артефакт.
    DownloadedArtifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputRole {
    /// Предмет сравнения (например, код): разная идентичность ожидаема.
    Subject,
    /// Нагрузка: результаты с разной идентичностью или объёмом несопоставимы.
    Workload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityMethod {
    /// SHA-256 содержимого дерева (байты, права, цели ссылок) по правилу модуля.
    ContentTreeSha256,
    /// SHA-256 одного файла.
    FileSha256,
    /// Явно объявленная версия артефакта.
    DeclaredVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeMetric {
    pub name: String,
    pub unit: Unit,
    pub description: String,
    /// Как исполнитель получает значение.
    #[serde(default)]
    pub measure: Option<SizeMeasure>,
}

/// Способ получить объём входа без собственного кода исполнителя для модуля.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SizeMeasure {
    /// Байты файла внутри входа (`path` относительно входа).
    FileBytes { path: String },
    /// Сумма байт обычных файлов дерева входа без перехода по ссылкам.
    TreeBytes,
    /// Число обычных файлов дерева входа.
    TreeFiles,
    /// Значение из JSON, который печатает `probe` входа.
    Probe { pointer: String },
    /// Значение из машинного отчёта модуля (`module_report`) прогона.
    Report { pointer: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub id: String,
    pub kind: InputKind,
    pub role: InputRole,
    pub identity: IdentityMethod,
    pub description: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub parameter: Option<String>,
    #[serde(default)]
    pub size: Vec<SizeMetric>,
    /// Команда только для чтения, печатающая JSON с характеристиками входа.
    /// Выполняется до серии и в измерение не входит.
    #[serde(default)]
    pub probe: Option<Step>,
    /// Указатель на идентичность артефакта в машинном отчёте модуля.
    #[serde(default)]
    pub report_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPolicy {
    pub warmup: u32,
    pub min_runs: u32,
    pub max_runs: u32,
    pub single_run_allowed: bool,
    pub run_timeout_seconds: u64,
    pub total_budget_seconds: u64,
}

/// Чего касается шаг жизненного цикла. Значения «всё» нет намеренно:
/// глобальная очистка (page cache, общий кэш Docker) декларацией не выражается.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Touches {
    /// Только читает.
    ReadOnly,
    /// Временные данные, созданные самим сценарием.
    ScenarioTemp,
    /// Состояние, которым владеет модуль и которое он очищает штатной командой.
    ModuleOwned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub description: String,
    pub program: Program,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: EnvSpec,
    pub touches: Touches,
    /// Предел одного шага; входит в общий бюджет серии.
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBinding {
    pub id: String,
    /// Точное определение состояния кэша для этого сценария.
    pub cache_definition: String,
    /// Что приводится в нужное состояние (очищается или прогревается).
    pub cleared: Vec<String>,
    /// Что намеренно сохраняется.
    pub retained: Vec<String>,
    #[serde(default)]
    pub network_definition: Option<String>,
    #[serde(default)]
    pub args_append: Vec<String>,
    #[serde(default)]
    pub required_parameters: Vec<String>,
    #[serde(default)]
    pub setup: Vec<Step>,
    #[serde(default)]
    pub prepare: Vec<Step>,
    #[serde(default)]
    pub conclude: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRequest {
    pub metric: MetricId,
    pub scope: Scope,
    pub source: Source,
    #[serde(default)]
    pub label: Option<String>,
    pub required: bool,
    #[serde(default)]
    pub unit: Option<Unit>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sample_interval_seconds: Option<f64>,
    /// Разрешение значения в единицах метрики (для журналов модулей).
    #[serde(default)]
    pub resolution: Option<f64>,
    /// Ресурс `disk_usage` относительно корня репозитория.
    #[serde(default)]
    pub path: Option<String>,
    /// Значение `module_counter` в машинном отчёте модуля; несколько
    /// выбранных значений (`*`) суммируются.
    #[serde(default)]
    pub pointer: Option<String>,
}

impl MetricRequest {
    pub fn key(&self) -> String {
        crate::vocabulary::metric_key(self.metric, self.scope, self.label.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomFact {
    pub name: String,
    pub description: String,
    /// Указатели на значение в машинном отчёте модуля. Одно значение
    /// записывается как есть, несколько — как `<имя>=<значение>` через запятую.
    #[serde(default)]
    pub report: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSpec {
    pub record: Vec<String>,
    pub comparable: Vec<String>,
    #[serde(default, rename = "fact")]
    pub facts: Vec<CustomFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Requirement {
    Executable {
        name: String,
    },
    Docker,
    LinuxCgroupV2,
    PerfEvents,
    Network {
        purpose: String,
        profiles: Vec<String>,
    },
}

/// Каталог: общий словарь профилей.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    pub schema_version: u32,
    /// Корень репозитория относительно каталога (обычно `..`).
    pub repository_root: String,
    #[serde(rename = "profile")]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub cache: CacheState,
    pub network: Network,
    pub description: String,
}

// --- Проверка формы с путём поля ---

/// Ошибка формы: путь поля и причина.
pub type ShapeError = (String, String);

pub trait Shape {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>);
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn leaf<T: DeserializeOwned>(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
    if let Err(error) = value.clone().try_into::<T>() {
        out.push((path.to_owned(), error.message().trim().to_owned()));
    }
}

macro_rules! leaf_shape {
    ($($ty:ty),+ $(,)?) => {
        $(impl Shape for $ty {
            fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
                leaf::<$ty>(value, path, out);
            }
        })+
    };
}

leaf_shape!(
    String,
    u32,
    u64,
    i32,
    bool,
    f64,
    BTreeMap<String, String>,
    WorkloadKind,
    ScenarioStatus,
    CacheState,
    Network,
    Os,
    Unit,
    Scope,
    Source,
    MetricId,
    InputKind,
    InputRole,
    IdentityMethod,
    Touches,
);

impl<T: Shape> Shape for Vec<T> {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
        match value.as_array() {
            Some(items) => {
                for (index, item) in items.iter().enumerate() {
                    T::check(item, &format!("{path}[{index}]"), out);
                }
            }
            None => out.push((path.to_owned(), "ожидается массив".to_owned())),
        }
    }
}

impl<T: Shape> Shape for Option<T> {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
        T::check(value, path, out);
    }
}

fn table<'a>(
    value: &'a toml::Value,
    path: &str,
    out: &mut Vec<ShapeError>,
) -> Option<&'a toml::Table> {
    let table = value.as_table();
    if table.is_none() {
        out.push((path.to_owned(), "ожидается таблица".to_owned()));
    }
    table
}

macro_rules! table_shape {
    ($ty:ty { $($key:literal : $field:ty = $required:literal),+ $(,)? }) => {
        impl Shape for $ty {
            fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
                let Some(table) = table(value, path, out) else { return };
                const KNOWN: &[&str] = &[$($key),+];
                for key in table.keys() {
                    if !KNOWN.contains(&key.as_str()) {
                        out.push((join(path, key), format!(
                            "неизвестное поле; допустимы: {}", KNOWN.join(", ")
                        )));
                    }
                }
                $(
                    match table.get($key) {
                        Some(inner) => <$field as Shape>::check(inner, &join(path, $key), out),
                        None if $required => out.push((join(path, $key), "обязательное поле отсутствует".to_owned())),
                        None => {}
                    }
                )+
            }
        }
    };
}

table_shape!(ScenarioFile {
    "schema_version": u32 = true,
    "module": String = true,
    "scenario": Vec<Scenario> = false,
});

table_shape!(Scenario {
    "id": String = true,
    "title": String = true,
    "revision": u32 = true,
    "kind": WorkloadKind = true,
    "status": ScenarioStatus = true,
    "description": String = true,
    "measured": String = true,
    "platforms": Vec<Os> = true,
    "target": Target = true,
    "parameter": Vec<Parameter> = false,
    "input": Vec<Input> = false,
    "runs": RunPolicy = true,
    "setup": Vec<Step> = false,
    "cleanup": Vec<Step> = false,
    "profile": Vec<ProfileBinding> = true,
    "metric": Vec<MetricRequest> = true,
    "environment": EnvironmentSpec = true,
    "requires": Vec<Requirement> = false,
    "module_report": ModuleReport = false,
    "container": ContainerSpec = false,
});

table_shape!(ModuleReport {
    "runs_dir": String = true,
    "report": String = true,
    "description": String = true,
});

table_shape!(ContainerSpec {
    "label": String = true,
    "owner_pid_label": String = true,
    "description": String = true,
});

table_shape!(EnvSpec {
    "set": BTreeMap<String, String> = false,
    "inherit": Vec<String> = false,
});

table_shape!(Target {
    "program": Program = true,
    "args": Vec<String> = false,
    "working_dir": String = false,
    "env": EnvSpec = false,
    "expect_exit_codes": Vec<i32> = true,
});

table_shape!(Parameter {
    "name": String = true,
    "description": String = true,
    "required": bool = false,
    "default": String = false,
    "choices": Vec<String> = false,
    "record": bool = true,
    "affects_comparability": bool = true,
});

table_shape!(SizeMetric {
    "name": String = true,
    "unit": Unit = true,
    "description": String = true,
    "measure": SizeMeasure = false,
});

table_shape!(Input {
    "id": String = true,
    "kind": InputKind = true,
    "role": InputRole = true,
    "identity": IdentityMethod = true,
    "description": String = true,
    "path": String = false,
    "parameter": String = false,
    "size": Vec<SizeMetric> = false,
    "probe": Step = false,
    "report_identity": String = false,
});

table_shape!(RunPolicy {
    "warmup": u32 = true,
    "min_runs": u32 = true,
    "max_runs": u32 = true,
    "single_run_allowed": bool = true,
    "run_timeout_seconds": u64 = true,
    "total_budget_seconds": u64 = true,
});

table_shape!(Step {
    "description": String = true,
    "program": Program = true,
    "args": Vec<String> = false,
    "working_dir": String = false,
    "env": EnvSpec = false,
    "touches": Touches = true,
    "timeout_seconds": u64 = true,
});

table_shape!(ProfileBinding {
    "id": String = true,
    "cache_definition": String = true,
    "cleared": Vec<String> = true,
    "retained": Vec<String> = true,
    "network_definition": String = false,
    "args_append": Vec<String> = false,
    "required_parameters": Vec<String> = false,
    "setup": Vec<Step> = false,
    "prepare": Vec<Step> = false,
    "conclude": Vec<Step> = false,
});

table_shape!(MetricRequest {
    "metric": MetricId = true,
    "scope": Scope = true,
    "source": Source = true,
    "label": String = false,
    "required": bool = true,
    "unit": Unit = false,
    "description": String = false,
    "sample_interval_seconds": f64 = false,
    "resolution": f64 = false,
    "path": String = false,
    "pointer": String = false,
});

table_shape!(CustomFact {
    "name": String = true,
    "description": String = true,
    "report": Vec<String> = false,
});

table_shape!(EnvironmentSpec {
    "record": Vec<String> = true,
    "comparable": Vec<String> = true,
    "fact": Vec<CustomFact> = false,
});

table_shape!(CatalogFile {
    "schema_version": u32 = true,
    "repository_root": String = true,
    "profile": Vec<Profile> = true,
});

table_shape!(Profile {
    "id": String = true,
    "cache": CacheState = true,
    "network": Network = true,
    "description": String = true,
});

/// Форма объединения с тегом `kind`: сначала тег, затем поля варианта.
fn tagged(
    value: &toml::Value,
    path: &str,
    out: &mut Vec<ShapeError>,
    variants: &[(&str, &[(&str, bool)])],
) {
    let Some(table) = table(value, path, out) else {
        return;
    };
    let names: Vec<&str> = variants.iter().map(|(name, _)| *name).collect();
    let Some(kind) = table.get("kind") else {
        out.push((
            join(path, "kind"),
            format!(
                "обязательное поле отсутствует; допустимы: {}",
                names.join(", ")
            ),
        ));
        return;
    };
    let Some((_, fields)) = kind
        .as_str()
        .and_then(|kind| variants.iter().find(|(name, _)| *name == kind))
    else {
        out.push((
            join(path, "kind"),
            format!("неизвестный вид; допустимы: {}", names.join(", ")),
        ));
        return;
    };
    for key in table.keys().filter(|key| key.as_str() != "kind") {
        match fields.iter().find(|(name, _)| name == key) {
            Some(_) => leaf_by_name(key, &table[key.as_str()], &join(path, key), out),
            None => out.push((
                join(path, key),
                format!(
                    "неизвестное поле для вида {}; допустимы: kind{}",
                    kind.as_str().unwrap_or_default(),
                    fields
                        .iter()
                        .map(|(name, _)| format!(", {name}"))
                        .collect::<String>()
                ),
            )),
        }
    }
    for (name, required) in fields.iter() {
        if *required && !table.contains_key(*name) {
            out.push((join(path, name), "обязательное поле отсутствует".to_owned()));
        }
    }
}

fn leaf_by_name(key: &str, value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
    match key {
        "profiles" => Vec::<String>::check(value, path, out),
        _ => String::check(value, path, out),
    }
}

impl Shape for Program {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
        tagged(
            value,
            path,
            out,
            &[
                (
                    "cargo-bin",
                    &[("package", true), ("bin", true), ("cargo_profile", false)],
                ),
                ("executable", &[("name", true)]),
            ],
        );
    }
}

impl Shape for SizeMeasure {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
        tagged(
            value,
            path,
            out,
            &[
                ("file-bytes", &[("path", true)]),
                ("tree-bytes", &[]),
                ("tree-files", &[]),
                ("probe", &[("pointer", true)]),
                ("report", &[("pointer", true)]),
            ],
        );
    }
}

impl Shape for Requirement {
    fn check(value: &toml::Value, path: &str, out: &mut Vec<ShapeError>) {
        tagged(
            value,
            path,
            out,
            &[
                ("executable", &[("name", true)]),
                ("docker", &[]),
                ("linux-cgroup-v2", &[]),
                ("perf-events", &[]),
                ("network", &[("purpose", true), ("profiles", true)]),
            ],
        );
    }
}
