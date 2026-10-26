//! Обнаружение, загрузка и семантическая проверка каталога сценариев.
//!
//! Раскладка каталога фиксирована: `<корень>/catalog.toml` задаёт словарь
//! профилей и корень репозитория, а `<корень>/scenarios/<модуль>.toml` —
//! декларации одного модуля. Новый модуль подключается новым файлом; ядро
//! схемы и этот загрузчик при этом не меняются.
//!
//! Загрузка ничего не запускает и не пишет: она читает только TOML каталога,
//! манифесты Cargo и проверяет существование путей, на которые ссылаются
//! декларации.

use crate::diagnostic::{Diagnostic, Diagnostics, Sink, field};
use crate::placeholder::{self, Placeholder};
use crate::scenario::{
    CatalogFile, EnvSpec, IdentityMethod, InputKind, InputRole, MetricRequest, Profile,
    ProfileBinding, Program, Requirement, Scenario, ScenarioFile, Shape, Step, Touches,
};
use crate::sensitive;
use crate::vocabulary::{
    BuiltinFact, CATALOG_SCHEMA_VERSION, CacheState, MANDATORY_COMPARABLE_FACTS, MANDATORY_FACTS,
    MetricId, Network, ScenarioStatus, Scope, Source, Unit, is_identifier, is_scenario_id,
};
use crate::workspace::Workspace;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const CATALOG_FILE: &str = "catalog.toml";
pub const SCENARIOS_DIR: &str = "scenarios";

#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    #[serde(skip)]
    pub root: PathBuf,
    #[serde(skip)]
    pub repository_root: PathBuf,
    pub profiles: Vec<Profile>,
    pub scenarios: Vec<LoadedScenario>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadedScenario {
    pub module: String,
    /// Файл декларации относительно корня каталога.
    pub file: String,
    #[serde(flatten)]
    pub scenario: Scenario,
}

impl Catalog {
    pub fn scenario(&self, id: &str) -> Option<&LoadedScenario> {
        self.scenarios.iter().find(|s| s.scenario.id == id)
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

/// Итог загрузки: каталог есть только при отсутствии ошибок.
#[derive(Debug)]
pub struct Loaded {
    pub catalog: Option<Catalog>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Загрузить и проверить каталог.
pub fn load(root: &Path) -> Loaded {
    let mut diagnostics = Diagnostics::default();
    let catalog = load_into(root, &mut diagnostics);
    let failed = diagnostics.has_errors();
    Loaded {
        catalog: if failed { None } else { catalog },
        diagnostics: diagnostics.into_vec(),
    }
}

fn load_into(root: &Path, diagnostics: &mut Diagnostics) -> Option<Catalog> {
    let catalog_path = root.join(CATALOG_FILE);
    let header: CatalogFile = parse_document(&catalog_path, CATALOG_FILE, diagnostics)?;
    let mut sink = diagnostics.at(CATALOG_FILE, None);
    let repository_root = resolve_repository_root(root, &header.repository_root, &mut sink)?;
    validate_profiles(&header.profiles, &mut sink);
    let workspace = match Workspace::read(&repository_root) {
        Ok(workspace) => workspace,
        Err(error) => {
            sink.error("workspace_unreadable", field("repository_root"), error);
            return None;
        }
    };

    let scenarios_dir = root.join(SCENARIOS_DIR);
    let mut files = Vec::new();
    match fs::read_dir(&scenarios_dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => files.push(entry.path()),
                    Err(error) => sink.error(
                        "unreadable",
                        None,
                        format!("не удалось прочитать запись {SCENARIOS_DIR}/: {error}"),
                    ),
                }
            }
        }
        Err(error) => {
            sink.error(
                "scenarios_missing",
                None,
                format!("не удалось прочитать каталог {SCENARIOS_DIR}/: {error}"),
            );
            return None;
        }
    }
    files.sort();

    let context = Context {
        repository_root: &repository_root,
        workspace: &workspace,
        profiles: &header.profiles,
    };
    let mut loaded = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let display = format!("{SCENARIOS_DIR}/{name}");
        if name.starts_with('.') {
            continue;
        }
        let kind = fs::symlink_metadata(&path).map(|m| m.file_type());
        let is_toml = name.ends_with(".toml");
        match kind {
            Ok(kind) if kind.is_file() && is_toml => {}
            Ok(kind) if kind.is_symlink() => {
                diagnostics.at(&display, None).error(
                    "symlink_forbidden",
                    None,
                    "символическая ссылка в каталоге деклараций запрещена: декларация должна лежать в каталоге",
                );
                continue;
            }
            _ => {
                diagnostics.at(&display, None).error(
                    "unexpected_entry",
                    None,
                    "в scenarios/ допускаются только файлы <модуль>.toml без вложенных каталогов",
                );
                continue;
            }
        }
        let Some(file) = parse_document::<ScenarioFile>(&path, &display, diagnostics) else {
            continue;
        };
        let stem = name.trim_end_matches(".toml");
        let mut sink = diagnostics.at(&display, None);
        if !is_identifier(&file.module) {
            sink.error(
                "invalid_module",
                field("module"),
                "имя модуля — идентификатор из строчных латинских букв, цифр, «-» и «_»",
            );
            continue;
        }
        if file.module != stem {
            sink.error(
                "module_file_mismatch",
                field("module"),
                format!(
                    "модуль «{}» объявлен в файле {name}; ожидается scenarios/{}.toml",
                    file.module, file.module
                ),
            );
            continue;
        }
        if file.scenarios.is_empty() {
            sink.error(
                "no_scenarios",
                field("scenario"),
                "файл модуля не объявляет ни одного сценария",
            );
        }
        for scenario in file.scenarios {
            let mut sink = diagnostics.at(&display, Some(&scenario.id));
            validate_scenario(&context, &file.module, &scenario, &mut sink);
            loaded.push(LoadedScenario {
                module: file.module.clone(),
                file: display.clone(),
                scenario,
            });
        }
    }

    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for item in &loaded {
        if let Some(first) = seen.insert(&item.scenario.id, &item.file) {
            diagnostics.at(&item.file, Some(&item.scenario.id)).error(
                "duplicate_scenario_id",
                field("id"),
                format!("ID уже объявлен в {first}: идентификатор сценария обязан быть уникальным во всём каталоге"),
            );
        }
    }
    if loaded.is_empty() && !diagnostics.has_errors() {
        diagnostics.at(SCENARIOS_DIR, None).error(
            "empty_catalog",
            None,
            "каталог не содержит ни одной декларации",
        );
    }
    loaded.sort_by(|a, b| a.scenario.id.cmp(&b.scenario.id));
    Some(Catalog {
        root: root.to_path_buf(),
        repository_root,
        profiles: header.profiles,
        scenarios: loaded,
    })
}

/// Разбор одного TOML-документа: синтаксис, версия схемы, форма, типы.
fn parse_document<T: Shape + serde::de::DeserializeOwned>(
    path: &Path,
    display: &str,
    diagnostics: &mut Diagnostics,
) -> Option<T> {
    let mut sink = diagnostics.at(display, None);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            sink.error(
                "unreadable",
                None,
                format!("не удалось прочитать файл: {error}"),
            );
            return None;
        }
    };
    let value: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            sink.error("toml_syntax", None, error.to_string().trim().to_owned());
            return None;
        }
    };
    if !check_schema_version(
        value.get("schema_version"),
        CATALOG_SCHEMA_VERSION,
        &mut sink,
    ) {
        return None;
    }
    // Ошибки формы привязываются к сценарию, если его ID удаётся прочитать.
    let mut errors = Vec::new();
    T::check(&value, "", &mut errors);
    if !errors.is_empty() {
        let ids: Vec<Option<String>> = value
            .get("scenario")
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.get("id")
                            .and_then(toml::Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (path, message) in errors {
            let (scenario, field_path) = split_scenario_path(&path, &ids);
            diagnostics.at(display, scenario.as_deref()).error(
                "invalid_field",
                Some(field_path),
                message,
            );
        }
        return None;
    }
    match value.try_into::<T>() {
        Ok(document) => Some(document),
        Err(error) => {
            diagnostics.at(display, None).error(
                "invalid_document",
                None,
                error.message().trim().to_owned(),
            );
            None
        }
    }
}

/// `scenario[2].runs.max_runs` → (ID сценария, `runs.max_runs`).
fn split_scenario_path(path: &str, ids: &[Option<String>]) -> (Option<String>, String) {
    if let Some(rest) = path.strip_prefix("scenario[")
        && let Some((index, tail)) = rest.split_once(']')
        && let Ok(index) = index.parse::<usize>()
    {
        let tail = tail.trim_start_matches('.');
        let id = ids.get(index).cloned().flatten();
        let field = if tail.is_empty() {
            path.to_owned()
        } else {
            tail.to_owned()
        };
        return match id {
            Some(id) => (Some(id), field),
            None => (None, path.to_owned()),
        };
    }
    (None, path.to_owned())
}

/// Явный отказ для неподдерживаемой версии схемы до разбора остальных полей.
pub fn check_schema_version(
    value: Option<&toml::Value>,
    supported: u32,
    sink: &mut Sink<'_>,
) -> bool {
    match value.and_then(toml::Value::as_integer) {
        Some(version) if version == i64::from(supported) => true,
        Some(version) => {
            sink.error(
                "unsupported_schema_version",
                field("schema_version"),
                format!(
                    "версия схемы {version} не поддерживается: этот инструмент понимает только {supported}. \
                     Обновите benchmark-contract или переведите декларацию на поддерживаемую версию по правилам эволюции схемы"
                ),
            );
            false
        }
        None => {
            sink.error(
                "unsupported_schema_version",
                field("schema_version"),
                format!(
                    "обязательное целое поле schema_version отсутствует; поддерживается {supported}"
                ),
            );
            false
        }
    }
}

fn resolve_repository_root(root: &Path, relative: &str, sink: &mut Sink<'_>) -> Option<PathBuf> {
    let candidate = root.join(relative);
    match candidate.canonicalize() {
        Ok(path) if path.is_dir() => Some(path),
        _ => {
            sink.error(
                "unresolved_path",
                field("repository_root"),
                format!("корень репозитория «{relative}» не найден относительно каталога"),
            );
            None
        }
    }
}

fn validate_profiles(profiles: &[Profile], sink: &mut Sink<'_>) {
    if profiles.is_empty() {
        sink.error(
            "no_profiles",
            field("profile"),
            "каталог обязан объявить хотя бы один профиль",
        );
    }
    let mut seen = BTreeSet::new();
    for (index, profile) in profiles.iter().enumerate() {
        let at = |name: &str| field(format!("profile[{index}].{name}"));
        if !is_identifier(&profile.id) {
            sink.error(
                "invalid_identifier",
                at("id"),
                "ID профиля — идентификатор из строчных латинских букв, цифр, «-» и «_»",
            );
        }
        if !seen.insert(&profile.id) {
            sink.error(
                "duplicate_profile",
                at("id"),
                format!("профиль «{}» объявлен повторно", profile.id),
            );
        }
        if profile.description.trim().is_empty() {
            sink.error(
                "empty_text",
                at("description"),
                "описание профиля обязательно",
            );
        }
    }
}

struct Context<'a> {
    repository_root: &'a Path,
    workspace: &'a Workspace,
    profiles: &'a [Profile],
}

fn validate_scenario(ctx: &Context<'_>, module: &str, scenario: &Scenario, sink: &mut Sink<'_>) {
    if !is_scenario_id(&scenario.id, module) {
        sink.error(
            "invalid_scenario_id",
            field("id"),
            format!("ID сценария обязан иметь вид «{module}.<имя>», где имя — идентификатор"),
        );
    }
    for (name, text) in [
        ("title", &scenario.title),
        ("description", &scenario.description),
        ("measured", &scenario.measured),
    ] {
        if text.trim().is_empty() {
            sink.error(
                "empty_text",
                field(name),
                "поле обязано содержать содержательный текст",
            );
        }
    }
    if scenario.revision == 0 {
        sink.error(
            "invalid_revision",
            field("revision"),
            "ревизия нагрузки начинается с 1",
        );
    }
    if scenario.platforms.is_empty() {
        sink.error(
            "no_platforms",
            field("platforms"),
            "укажите хотя бы одну платформу измерения",
        );
    }
    if has_duplicates(&scenario.platforms) {
        sink.error(
            "duplicate_value",
            field("platforms"),
            "платформа повторяется",
        );
    }

    let params: BTreeMap<&str, &crate::scenario::Parameter> = scenario
        .parameters
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    validate_parameters(scenario, sink);
    validate_inputs(ctx, scenario, &params, sink);
    let path_inputs: BTreeSet<&str> = scenario
        .inputs
        .iter()
        .filter(|i| matches!(i.kind, InputKind::ExternalPath | InputKind::RepoFile))
        .map(|i| i.id.as_str())
        .collect();
    let refs = Refs {
        params: &params,
        path_inputs: &path_inputs,
        all_inputs: scenario.inputs.iter().map(|i| i.id.as_str()).collect(),
        input_params: scenario
            .inputs
            .iter()
            .filter(|i| i.kind == InputKind::ExternalPath)
            .filter_map(|i| Some((i.id.as_str(), i.parameter.as_deref()?)))
            .collect(),
        used: RefCell::default(),
    };

    // Цель запуска.
    validate_program(ctx, &scenario.target.program, "target.program", sink);
    validate_args(
        &scenario.target.program,
        &scenario.target.args,
        "target.args",
        sink,
    );
    validate_working_dir(
        ctx,
        &scenario.target.working_dir,
        "target.working_dir",
        sink,
    );
    validate_env(&scenario.target.env, "target.env", sink);
    let always = BTreeSet::new();
    refs.check_all(&scenario.target.args, "target.args", &always, sink);
    refs.check_env(&scenario.target.env, "target.env", &always, sink);
    if scenario.target.expect_exit_codes.is_empty() {
        sink.error(
            "no_expected_result",
            field("target.expect_exit_codes"),
            "укажите хотя бы один ожидаемый код завершения",
        );
    }
    if has_duplicates(&scenario.target.expect_exit_codes) {
        sink.error(
            "duplicate_value",
            field("target.expect_exit_codes"),
            "код завершения повторяется",
        );
    }

    validate_runs(scenario, sink);
    let limits = scenario.kind.limits();
    for (list, name) in [(&scenario.setup, "setup"), (&scenario.cleanup, "cleanup")] {
        for (index, step) in list.iter().enumerate() {
            let at = format!("{name}[{index}]");
            validate_step(ctx, step, &at, limits.max_run_timeout_seconds, sink);
            refs.check_step(step, &at, &always, sink);
        }
    }

    validate_profile_bindings(ctx, scenario, &refs, sink);
    // Параметр без подстановки и без входа external-path не доходит до
    // команды: записанное значение противоречило бы фактическому прогону.
    let used = refs.used.into_inner();
    for (index, param) in scenario.parameters.iter().enumerate() {
        let carries_input = scenario
            .inputs
            .iter()
            .any(|i| i.parameter.as_deref() == Some(param.name.as_str()));
        if !used.contains(&param.name) && !carries_input {
            sink.warning(
                "unused_parameter",
                field(format!("parameter[{index}]")),
                format!(
                    "параметр «{}» не подставляется ни в одну команду",
                    param.name
                ),
            );
        }
    }
    validate_requirements(ctx, scenario, sink);
    validate_metrics(scenario, sink);
    validate_environment(scenario, sink);

    if scenario.status == ScenarioStatus::Example
        && !scenario.description.to_lowercase().contains("учебн")
    {
        sink.warning(
            "example_not_marked",
            field("description"),
            "учебный сценарий должен прямо называть себя учебным в описании",
        );
    }
}

fn has_duplicates<T: Ord>(items: &[T]) -> bool {
    let mut seen = BTreeSet::new();
    items.iter().any(|item| !seen.insert(item))
}

fn validate_parameters(scenario: &Scenario, sink: &mut Sink<'_>) {
    let mut seen = BTreeSet::new();
    for (index, param) in scenario.parameters.iter().enumerate() {
        let at = |name: &str| field(format!("parameter[{index}].{name}"));
        if !is_identifier(&param.name) {
            sink.error(
                "invalid_identifier",
                at("name"),
                "имя параметра — идентификатор",
            );
        }
        if !seen.insert(&param.name) {
            sink.error(
                "duplicate_parameter",
                at("name"),
                format!("параметр «{}» объявлен повторно", param.name),
            );
        }
        if param.description.trim().is_empty() {
            sink.error(
                "empty_text",
                at("description"),
                "описание параметра обязательно",
            );
        }
        if param.required && param.default.is_some() {
            sink.error(
                "contradictory_parameter",
                at("default"),
                "обязательный параметр не может иметь значение по умолчанию",
            );
        }
        if let Some(default) = &param.default
            && !param.choices.is_empty()
            && !param.choices.contains(default)
        {
            sink.error(
                "contradictory_parameter",
                at("default"),
                "значение по умолчанию не входит в choices",
            );
        }
        if has_duplicates(&param.choices) {
            sink.error(
                "duplicate_value",
                at("choices"),
                "значение choices повторяется",
            );
        }
        if param.record && sensitive::secret_like_name(&param.name) {
            sink.error(
                "sensitive_parameter",
                at("record"),
                "параметр с именем секрета не может публиковаться в результатах",
            );
        }
        if param.affects_comparability && !param.record {
            sink.error(
                "contradictory_parameter",
                at("affects_comparability"),
                "неопубликованное значение не может участвовать в сопоставлении результатов; сопоставимость входа выражает его идентичность",
            );
        }
        if let Some(default) = &param.default
            && let Some(reason) = sensitive::finding(default)
        {
            sink.error(
                "sensitive_value",
                at("default"),
                format!("значение по умолчанию недопустимо: {reason}"),
            );
        }
    }
}

fn validate_inputs(
    ctx: &Context<'_>,
    scenario: &Scenario,
    params: &BTreeMap<&str, &crate::scenario::Parameter>,
    sink: &mut Sink<'_>,
) {
    let mut seen = BTreeSet::new();
    for (index, input) in scenario.inputs.iter().enumerate() {
        let at = |name: &str| field(format!("input[{index}].{name}"));
        if !is_identifier(&input.id) {
            sink.error("invalid_identifier", at("id"), "ID входа — идентификатор");
        }
        if !seen.insert(&input.id) {
            sink.error(
                "duplicate_input",
                at("id"),
                format!("вход «{}» объявлен повторно", input.id),
            );
        }
        if input.description.trim().is_empty() {
            sink.error(
                "empty_text",
                at("description"),
                "описание входа обязательно",
            );
        }
        let identity_ok = match input.kind {
            InputKind::RepoTree => input.identity == IdentityMethod::ContentTreeSha256,
            InputKind::RepoFile | InputKind::ExternalPath => matches!(
                input.identity,
                IdentityMethod::ContentTreeSha256 | IdentityMethod::FileSha256
            ),
            InputKind::DownloadedArtifact => matches!(
                input.identity,
                IdentityMethod::FileSha256 | IdentityMethod::DeclaredVersion
            ),
        };
        if !identity_ok {
            sink.error(
                "invalid_identity",
                at("identity"),
                "способ идентификации не подходит виду входа: дерево — content-tree-sha256, файл — content-tree-sha256 или file-sha256, артефакт — file-sha256 или declared-version",
            );
        }
        match input.kind {
            InputKind::RepoFile => {
                match &input.path {
                    Some(path) => {
                        if let Some(reason) = unsafe_relative(path) {
                            sink.error("invalid_path", at("path"), reason);
                        } else if !inside(ctx.repository_root, path, false) {
                            sink.error(
                                "unresolved_path",
                                at("path"),
                                format!("файл «{path}» не найден в репозитории"),
                            );
                        }
                    }
                    None => sink.error(
                        "missing_field",
                        at("path"),
                        "вход repo-file обязан указать path",
                    ),
                }
                if input.parameter.is_some() {
                    sink.error(
                        "contradictory_input",
                        at("parameter"),
                        "вход repo-file не задаётся параметром",
                    );
                }
            }
            InputKind::ExternalPath => {
                match input.parameter.as_deref().map(|name| (name, params.get(name))) {
                    Some((_, Some(param))) if param.record => sink.error(
                        "path_published",
                        at("parameter"),
                        format!("параметр «{}» несёт внешний путь и обязан иметь record = false: путь пользователя не публикуется", param.name),
                    ),
                    Some((_, Some(param))) if param.default.is_some() => sink.error(
                        "contradictory_input",
                        at("parameter"),
                        format!("параметр «{}» несёт внешний путь: внешний путь не фиксируется в декларации, default запрещён", param.name),
                    ),
                    Some((_, Some(_))) => {}
                    Some((name, None)) => sink.error("unresolved_reference", at("parameter"), format!("параметр «{name}» не объявлен")),
                    None => sink.error("missing_field", at("parameter"), "вход external-path задаётся параметром"),
                }
                if input.path.is_some() {
                    sink.error(
                        "contradictory_input",
                        at("path"),
                        "внешний путь не фиксируется в декларации",
                    );
                }
            }
            InputKind::RepoTree | InputKind::DownloadedArtifact => {
                if input.path.is_some() || input.parameter.is_some() {
                    sink.error(
                        "contradictory_input",
                        at("path"),
                        "этот вид входа не принимает path и parameter",
                    );
                }
            }
        }
        if input.role == InputRole::Workload && input.size.is_empty() {
            sink.error(
                "missing_size",
                at("size"),
                "вход-нагрузка обязан объявить характеристики объёма: несходные объёмы нельзя считать одной нагрузкой",
            );
        }
        let mut names = BTreeSet::new();
        for (size_index, size) in input.size.iter().enumerate() {
            let at = |name: &str| field(format!("input[{index}].size[{size_index}].{name}"));
            if !is_identifier(&size.name) {
                sink.error(
                    "invalid_identifier",
                    at("name"),
                    "имя характеристики объёма — идентификатор",
                );
            }
            if !names.insert(&size.name) {
                sink.error(
                    "duplicate_value",
                    at("name"),
                    "характеристика объёма повторяется",
                );
            }
            if !matches!(size.unit, Unit::Byte | Unit::Count) {
                sink.error(
                    "unit_mismatch",
                    at("unit"),
                    "объём входа измеряется в byte или count",
                );
            }
            if size.description.trim().is_empty() {
                sink.error(
                    "empty_text",
                    at("description"),
                    "опишите, что измеряет характеристика",
                );
            }
        }
    }
}

struct Refs<'a> {
    params: &'a BTreeMap<&'a str, &'a crate::scenario::Parameter>,
    path_inputs: &'a BTreeSet<&'a str>,
    all_inputs: BTreeSet<&'a str>,
    /// Вход external-path → параметр, который задаёт его путь.
    input_params: BTreeMap<&'a str, &'a str>,
    /// Параметры, на которые сослалась хотя бы одна подстановка.
    used: RefCell<BTreeSet<String>>,
}

impl Refs<'_> {
    /// Проверить подстановки. `guaranteed` — параметры, которые профиль
    /// дополнительно объявил обязательными для себя.
    fn check(&self, text: &str, at: &str, guaranteed: &BTreeSet<&str>, sink: &mut Sink<'_>) {
        let found = match placeholder::parse(text) {
            Ok(found) => found,
            Err(reason) => {
                sink.error("invalid_placeholder", field(at), reason);
                return;
            }
        };
        for item in found {
            match item {
                Placeholder::Param(name) => match self.params.get(name.as_str()).inspect(|_| {
                    self.used.borrow_mut().insert(name.clone());
                }) {
                    None => sink.error(
                        "unresolved_reference",
                        field(at),
                        format!("параметр «{name}» не объявлен"),
                    ),
                    Some(param)
                        if !param.required
                            && param.default.is_none()
                            && !guaranteed.contains(name.as_str()) =>
                    {
                        sink.error(
                            "unresolvable_placeholder",
                            field(at),
                            format!("необязательный параметр «{name}» без значения по умолчанию может остаться пустым; объявите его required, задайте default или перечислите в required_parameters профиля"),
                        );
                    }
                    Some(_) => {}
                },
                Placeholder::Input(id) => {
                    if !self.all_inputs.contains(id.as_str()) {
                        sink.error(
                            "unresolved_reference",
                            field(at),
                            format!("вход «{id}» не объявлен"),
                        );
                    } else if !self.path_inputs.contains(id.as_str()) {
                        sink.error("unresolved_reference", field(at), format!("вход «{id}» не имеет пути: подставлять можно только repo-file и external-path"));
                    } else if let Some(name) = self.input_params.get(id.as_str())
                        && self.params.get(name).is_some_and(|param| {
                            !param.required && param.default.is_none() && !guaranteed.contains(name)
                        })
                    {
                        sink.error(
                            "unresolvable_placeholder",
                            field(at),
                            format!("путь входа «{id}» задаёт необязательный параметр «{name}» без значения по умолчанию; объявите его required или перечислите в required_parameters профиля"),
                        );
                    }
                }
            }
        }
    }

    fn check_all(
        &self,
        items: &[String],
        at: &str,
        guaranteed: &BTreeSet<&str>,
        sink: &mut Sink<'_>,
    ) {
        for (index, item) in items.iter().enumerate() {
            self.check(item, &format!("{at}[{index}]"), guaranteed, sink);
        }
    }

    fn check_env(&self, env: &EnvSpec, at: &str, guaranteed: &BTreeSet<&str>, sink: &mut Sink<'_>) {
        for (name, value) in &env.set {
            self.check(value, &format!("{at}.set.{name}"), guaranteed, sink);
        }
    }

    fn check_step(&self, step: &Step, at: &str, guaranteed: &BTreeSet<&str>, sink: &mut Sink<'_>) {
        self.check_all(&step.args, &format!("{at}.args"), guaranteed, sink);
        self.check_env(&step.env, &format!("{at}.env"), guaranteed, sink);
    }
}

/// Команды, через которые декларация могла бы скрыть произвольное или
/// разрушительное действие. Шаги описываются argv, а не строкой shell.
const FORBIDDEN_PROGRAMS: &[&str] = &[
    "sudo", "su", "doas", "pkexec", "rm", "rmdir", "dd", "shred", "mkfs", "wipefs", "sh", "bash",
    "zsh", "dash", "fish", "ksh", "env", "xargs", "eval", "sysctl", "tee", "nice", "nohup",
    "timeout", "stdbuf", "setsid", "chroot", "runuser", "busybox",
];

fn validate_program(ctx: &Context<'_>, program: &Program, at: &str, sink: &mut Sink<'_>) {
    match program {
        Program::CargoBin {
            package,
            bin,
            cargo_profile,
        } => match ctx.workspace.packages.get(package) {
            None => sink.error(
                "unresolved_program",
                field(format!("{at}.package")),
                format!("пакет «{package}» не входит в рабочую область Cargo репозитория"),
            ),
            Some(bins) if !bins.contains(bin) => sink.error(
                "unresolved_program",
                field(format!("{at}.bin")),
                format!(
                    "у пакета «{package}» нет исполняемого файла «{bin}»; есть: {}",
                    bins.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            ),
            Some(_) if !ctx.workspace.profiles.contains(cargo_profile) => sink.error(
                "unresolved_program",
                field(format!("{at}.cargo_profile")),
                format!("профиль сборки «{cargo_profile}» не объявлен"),
            ),
            Some(_) => {}
        },
        Program::Executable { name } => {
            if name.is_empty() || name.contains(['/', '\\']) || name.contains(char::is_whitespace) {
                sink.error(
                    "invalid_program",
                    field(format!("{at}.name")),
                    "исполняемый файл задаётся именем из PATH без пути и пробелов; файлы репозитория — через cargo-bin",
                );
            } else if FORBIDDEN_PROGRAMS.contains(&name.as_str()) {
                sink.error(
                    "forbidden_program",
                    field(format!("{at}.name")),
                    format!("«{name}» запрещён: shell, повышение прав и удаление скрывают семантику шага"),
                );
            }
        }
    }
}

fn validate_args(program: &Program, args: &[String], at: &str, sink: &mut Sink<'_>) {
    for (index, arg) in args.iter().enumerate() {
        if let Some(reason) = sensitive::finding(arg) {
            sink.error(
                "sensitive_value",
                field(format!("{at}[{index}]")),
                format!("аргумент недопустим: {reason}"),
            );
        }
        if arg.contains("drop_caches") {
            sink.error(
                "unsafe_cleanup",
                field(format!("{at}[{index}]")),
                "сброс глобального page cache не выражается декларацией: холодное состояние определяется кэшами самого модуля",
            );
        }
    }
    if let Program::Executable { name } = program
        && name == "docker"
        && args.iter().any(|a| a == "prune")
        && !args
            .iter()
            .any(|a| a == "--filter" || a.starts_with("--filter="))
    {
        sink.error(
            "unsafe_cleanup",
            field(at),
            "docker … prune без --filter удаляет общие ресурсы других проектов; используйте штатную очистку модуля",
        );
    }
}

fn validate_working_dir(ctx: &Context<'_>, path: &str, at: &str, sink: &mut Sink<'_>) {
    if let Some(reason) = unsafe_relative(path) {
        sink.error("invalid_path", field(at), reason);
    } else if !inside(ctx.repository_root, path, true) {
        sink.error(
            "unresolved_path",
            field(at),
            format!("каталог «{path}» не найден в репозитории"),
        );
    }
}

fn validate_env(env: &EnvSpec, at: &str, sink: &mut Sink<'_>) {
    let valid = |name: &str| {
        !name.is_empty()
            && !name.as_bytes()[0].is_ascii_digit()
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    };
    for (name, value) in &env.set {
        if !valid(name) {
            sink.error(
                "invalid_env",
                field(format!("{at}.set.{name}")),
                "имя переменной — заглавные латинские буквы, цифры и «_»",
            );
        }
        if sensitive::secret_like_name(name) {
            sink.error(
                "sensitive_value",
                field(format!("{at}.set.{name}")),
                "секрет не задаётся в декларации",
            );
        }
        if let Some(reason) = sensitive::finding(value) {
            sink.error(
                "sensitive_value",
                field(format!("{at}.set.{name}")),
                format!("значение недопустимо: {reason}"),
            );
        }
        if env.inherit.contains(name) {
            sink.error(
                "contradictory_env",
                field(format!("{at}.inherit")),
                format!("«{name}» одновременно задана и унаследована"),
            );
        }
    }
    for (index, name) in env.inherit.iter().enumerate() {
        if !valid(name) {
            sink.error(
                "invalid_env",
                field(format!("{at}.inherit[{index}]")),
                "имя переменной — заглавные латинские буквы, цифры и «_»",
            );
        }
        if sensitive::secret_like_name(name) {
            sink.warning(
                "secret_env_inherited",
                field(format!("{at}.inherit[{index}]")),
                "наследуется переменная с именем секрета: её значение не попадёт в результат, но убедитесь, что она действительно нужна",
            );
        }
    }
    if has_duplicates(&env.inherit) {
        sink.error(
            "duplicate_value",
            field(format!("{at}.inherit")),
            "переменная повторяется",
        );
    }
}

fn validate_runs(scenario: &Scenario, sink: &mut Sink<'_>) {
    let runs = &scenario.runs;
    let limits = scenario.kind.limits();
    let kind = scenario.kind;
    if runs.warmup > limits.max_warmup {
        sink.error(
            "unbounded_runs",
            field("runs.warmup"),
            format!(
                "прогрев {} превышает предел {} для {kind}",
                runs.warmup, limits.max_warmup
            ),
        );
    }
    if runs.min_runs == 0 {
        sink.error(
            "invalid_runs",
            field("runs.min_runs"),
            "нужен хотя бы один измеряемый прогон",
        );
    }
    if runs.max_runs < runs.min_runs {
        sink.error(
            "invalid_runs",
            field("runs.max_runs"),
            "max_runs меньше min_runs",
        );
    }
    if runs.max_runs > limits.max_runs {
        sink.error(
            "unbounded_runs",
            field("runs.max_runs"),
            format!(
                "{} прогонов превышает предел {} для {kind}",
                runs.max_runs, limits.max_runs
            ),
        );
    }
    if runs.run_timeout_seconds == 0 || runs.run_timeout_seconds > limits.max_run_timeout_seconds {
        sink.error(
            "unbounded_runs",
            field("runs.run_timeout_seconds"),
            format!(
                "таймаут прогона обязан быть в пределах 1…{} с для {kind}",
                limits.max_run_timeout_seconds
            ),
        );
    }
    if runs.total_budget_seconds < runs.run_timeout_seconds
        || runs.total_budget_seconds > limits.max_total_budget_seconds
    {
        sink.error(
            "unbounded_runs",
            field("runs.total_budget_seconds"),
            format!(
                "общий бюджет серии обязан вмещать хотя бы один прогон и не превышать {} с для {kind}",
                limits.max_total_budget_seconds
            ),
        );
    }
    if runs.single_run_allowed && !limits.single_run_allowed {
        sink.error(
            "single_run_forbidden",
            field("runs.single_run_allowed"),
            format!("одиночный прогон допустим только для long-e2e: результат {kind} без повторов не сопоставляется"),
        );
    }
    if !runs.single_run_allowed && runs.min_runs < 2 {
        sink.error(
            "invalid_runs",
            field("runs.min_runs"),
            "без single_run_allowed нужно не меньше двух прогонов",
        );
    }
}

fn validate_step(ctx: &Context<'_>, step: &Step, at: &str, max_timeout: u64, sink: &mut Sink<'_>) {
    if step.description.trim().is_empty() {
        sink.error(
            "empty_text",
            field(format!("{at}.description")),
            "опишите назначение шага",
        );
    }
    validate_program(ctx, &step.program, &format!("{at}.program"), sink);
    validate_args(&step.program, &step.args, &format!("{at}.args"), sink);
    validate_working_dir(ctx, &step.working_dir, &format!("{at}.working_dir"), sink);
    validate_env(&step.env, &format!("{at}.env"), sink);
    if step.timeout_seconds == 0 || step.timeout_seconds > max_timeout {
        sink.error(
            "unbounded_runs",
            field(format!("{at}.timeout_seconds")),
            format!("таймаут шага обязан быть в пределах 1…{max_timeout} с"),
        );
    }
}

fn validate_profile_bindings(
    ctx: &Context<'_>,
    scenario: &Scenario,
    refs: &Refs<'_>,
    sink: &mut Sink<'_>,
) {
    if scenario.profiles.is_empty() {
        sink.error(
            "no_profiles",
            field("profile"),
            "сценарий обязан объявить хотя бы один профиль",
        );
    }
    let limits = scenario.kind.limits();
    let mut seen = BTreeSet::new();
    for (index, binding) in scenario.profiles.iter().enumerate() {
        let at = |name: &str| field(format!("profile[{index}].{name}"));
        if !seen.insert(&binding.id) {
            sink.error(
                "duplicate_profile",
                at("id"),
                format!("профиль «{}» привязан повторно", binding.id),
            );
        }
        let Some(profile) = ctx.profiles.iter().find(|p| p.id == binding.id) else {
            sink.error(
                "unknown_profile",
                at("id"),
                format!(
                    "профиль «{}» не объявлен в catalog.toml; есть: {}",
                    binding.id,
                    ctx.profiles
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            continue;
        };
        if binding.cache_definition.trim().is_empty() {
            sink.error(
                "empty_text",
                at("cache_definition"),
                "точное определение состояния кэша обязательно",
            );
        }
        for (name, list) in [
            ("cleared", &binding.cleared),
            ("retained", &binding.retained),
        ] {
            if list.iter().any(|item| item.trim().is_empty()) {
                sink.error("empty_text", at(name), "пустой элемент списка");
            }
        }
        let guaranteed: BTreeSet<&str> = binding
            .required_parameters
            .iter()
            .map(String::as_str)
            .collect();
        for (param_index, name) in binding.required_parameters.iter().enumerate() {
            if !refs.params.contains_key(name.as_str()) {
                sink.error(
                    "unresolved_reference",
                    field(format!(
                        "profile[{index}].required_parameters[{param_index}]"
                    )),
                    format!("параметр «{name}» не объявлен"),
                );
            }
        }
        refs.check_all(
            &binding.args_append,
            &format!("profile[{index}].args_append"),
            &guaranteed,
            sink,
        );
        validate_args(
            &scenario.target.program,
            &binding.args_append,
            &format!("profile[{index}].args_append"),
            sink,
        );
        for (list, name) in [
            (&binding.setup, "setup"),
            (&binding.prepare, "prepare"),
            (&binding.conclude, "conclude"),
        ] {
            for (step_index, step) in list.iter().enumerate() {
                let at = format!("profile[{index}].{name}[{step_index}]");
                validate_step(ctx, step, &at, limits.max_run_timeout_seconds, sink);
                refs.check_step(step, &at, &guaranteed, sink);
            }
        }
        validate_cache_semantics(scenario, binding, profile.cache, index, sink);
        match profile.network {
            Network::Offline => {
                if binding
                    .network_definition
                    .as_deref()
                    .is_none_or(|t| t.trim().is_empty())
                {
                    sink.error(
                        "missing_network_definition",
                        at("network_definition"),
                        "офлайн-профиль обязан точно сказать, какие сетевые обращения исключены и как это обеспечивается",
                    );
                }
            }
            Network::Online => {}
        }
    }
}

fn validate_cache_semantics(
    scenario: &Scenario,
    binding: &ProfileBinding,
    cache: CacheState,
    index: usize,
    sink: &mut Sink<'_>,
) {
    let at = |name: &str| field(format!("profile[{index}].{name}"));
    let resets = binding
        .prepare
        .iter()
        .any(|s| s.touches == Touches::ModuleOwned);
    match cache {
        CacheState::Cold => {
            if !resets {
                sink.error(
                    "cold_without_reset",
                    at("prepare"),
                    "холодный профиль обязан перед каждым прогоном сбрасывать состояние модуля шагом prepare с touches = \"module-owned\"",
                );
            }
            if binding.cleared.is_empty() {
                sink.error(
                    "cold_without_reset",
                    at("cleared"),
                    "перечислите, что именно очищается перед холодным прогоном",
                );
            }
            if scenario.runs.warmup > 0 {
                sink.error(
                    "cold_with_warmup",
                    field("runs.warmup"),
                    "прогрев противоречит холодному профилю этого сценария",
                );
            }
        }
        CacheState::Warm => {
            if scenario.runs.min_runs < 2 {
                sink.error(
                    "warm_single_run",
                    field("runs.min_runs"),
                    "тёплое состояние измеряется минимум дважды: задайте runs.min_runs ≥ 2",
                );
            }
            if scenario.runs.warmup == 0 && binding.setup.is_empty() {
                sink.error(
                    "warm_without_priming",
                    at("setup"),
                    "тёплому профилю нужен прогрев: runs.warmup ≥ 1 или шаг setup профиля, который приводит кэши в рабочее состояние",
                );
            }
            if resets {
                sink.error(
                    "warm_resets_cache",
                    at("prepare"),
                    "шаг prepare тёплого профиля сбрасывает состояние модуля",
                );
            }
        }
        CacheState::Incremental => {
            if binding.prepare.is_empty() {
                sink.error(
                    "incremental_without_change",
                    at("prepare"),
                    "инкрементальный профиль обязан шагом prepare вносить изменение входа, вызывающее частичную пересборку",
                );
            }
        }
    }
}

fn validate_requirements(ctx: &Context<'_>, scenario: &Scenario, sink: &mut Sink<'_>) {
    let mut seen = Vec::new();
    for (index, requirement) in scenario.requires.iter().enumerate() {
        let at = |name: &str| field(format!("requires[{index}].{name}"));
        if seen.contains(&requirement) {
            sink.warning(
                "duplicate_value",
                field(format!("requires[{index}]")),
                "требование повторяется",
            );
        }
        seen.push(requirement);
        match requirement {
            Requirement::Executable { name } if name.is_empty() || name.contains(['/', '\\']) => {
                sink.error(
                    "invalid_program",
                    at("name"),
                    "исполняемый файл задаётся именем из PATH",
                );
            }
            Requirement::Network { purpose, profiles } => {
                if purpose.trim().is_empty() {
                    sink.error("empty_text", at("purpose"), "опишите, зачем нужна сеть");
                }
                if profiles.is_empty() {
                    sink.error(
                        "missing_field",
                        at("profiles"),
                        "перечислите профили, которым нужна сеть",
                    );
                }
                for profile in profiles {
                    if !scenario.profiles.iter().any(|b| &b.id == profile) {
                        sink.error(
                            "unresolved_reference",
                            at("profiles"),
                            format!("профиль «{profile}» не привязан к сценарию"),
                        );
                    }
                    if ctx
                        .profiles
                        .iter()
                        .any(|p| &p.id == profile && p.network == Network::Offline)
                    {
                        sink.error(
                            "offline_requires_network",
                            at("profiles"),
                            format!("офлайн-профиль «{profile}» не может требовать сеть"),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_metrics(scenario: &Scenario, sink: &mut Sink<'_>) {
    if scenario.metrics.is_empty() {
        sink.error(
            "no_metrics",
            field("metric"),
            "сценарий обязан запросить хотя бы одну метрику",
        );
    }
    let mut keys = BTreeSet::new();
    for (index, request) in scenario.metrics.iter().enumerate() {
        validate_metric(scenario, request, index, sink);
        if !keys.insert(request.key()) {
            sink.error(
                "duplicate_metric",
                field(format!("metric[{index}]")),
                format!("метрика {} запрошена повторно", request.key()),
            );
        }
    }
    let primary = scenario
        .metrics
        .iter()
        .any(|m| m.metric == MetricId::TimeWallClock && m.scope == Scope::Command && m.required);
    if !primary {
        sink.error(
            "primary_metric_missing",
            field("metric"),
            "каждый сценарий обязан запросить обязательную первичную метрику time_wall_clock со scope = \"command\"",
        );
    }
    let requires = |wanted: &Requirement| scenario.requires.contains(wanted);
    for (index, request) in scenario.metrics.iter().enumerate() {
        if !request.required {
            continue;
        }
        if request.metric.is_hardware_counter() && !requires(&Requirement::PerfEvents) {
            sink.error(
                "missing_requirement",
                field(format!("metric[{index}]")),
                "обязательный счётчик процессора требует объявить requires kind = \"perf-events\"",
            );
        }
        if request.source == Source::CgroupV2 && !requires(&Requirement::LinuxCgroupV2) {
            sink.error(
                "missing_requirement",
                field(format!("metric[{index}]")),
                "обязательная метрика cgroup v2 требует объявить requires kind = \"linux-cgroup-v2\"",
            );
        }
    }
}

fn validate_metric(
    scenario: &Scenario,
    request: &MetricRequest,
    index: usize,
    sink: &mut Sink<'_>,
) {
    let at = |name: &str| field(format!("metric[{index}].{name}"));
    let definition = request.metric.definition();
    match (&request.label, request.metric.label_required()) {
        (None, true) => sink.error(
            "missing_label",
            at("label"),
            format!(
                "{} различает экземпляры меткой: укажите label",
                request.metric
            ),
        ),
        (Some(_), false) => sink.error(
            "unexpected_label",
            at("label"),
            format!("{} не принимает label", request.metric),
        ),
        (Some(label), true) if !is_identifier(label) => {
            sink.error("invalid_identifier", at("label"), "метка — идентификатор")
        }
        _ => {}
    }
    if let Some(unit) = request.unit
        && unit != definition.unit
    {
        sink.error(
            "unit_mismatch",
            at("unit"),
            format!(
                "базовая единица {} — {}, а не {unit}; другие единицы — дело отображения",
                request.metric, definition.unit
            ),
        );
    }
    let Some(measurement) = definition.measurement(request.scope, request.source) else {
        let allowed: Vec<String> = definition
            .measurements
            .iter()
            .map(|m| format!("{}/{}", m.scope, m.source))
            .collect();
        sink.error(
            "metric_scope_source",
            at("scope"),
            format!(
                "{} не измеряется в сочетании scope = {}, source = {}; допустимо: {}",
                request.metric,
                request.scope,
                request.source,
                allowed.join(", ")
            ),
        );
        return;
    };
    match (measurement.sampled, request.sample_interval_seconds) {
        (true, None) => sink.error(
            "missing_sample_interval",
            at("sample_interval_seconds"),
            "сэмплируемая метрика обязана задать интервал",
        ),
        (true, Some(interval)) if !(interval.is_finite() && interval > 0.0 && interval <= 60.0) => {
            sink.error(
                "invalid_sample_interval",
                at("sample_interval_seconds"),
                "интервал сэмплирования — от 0 до 60 с",
            )
        }
        (false, Some(_)) => sink.error(
            "unexpected_sample_interval",
            at("sample_interval_seconds"),
            "метрика не сэмплируется",
        ),
        _ => {}
    }
    match (request.source == Source::ModuleLog, request.resolution) {
        (true, None) => sink.error(
            "missing_resolution",
            at("resolution"),
            "метрика журнала модуля обязана задать разрешение",
        ),
        (_, Some(value)) if !(value.is_finite() && value > 0.0) => sink.error(
            "invalid_resolution",
            at("resolution"),
            "разрешение — положительное число",
        ),
        _ => {}
    }
    for platform in &scenario.platforms {
        if !measurement.platforms.contains(platform) {
            if request.required {
                sink.error(
                    "metric_unavailable_on_platform",
                    at("source"),
                    format!("{} из {} недоступна на {platform}: обязательную метрику нельзя обещать на этой платформе", request.metric, request.source),
                );
            } else {
                sink.warning(
                    "metric_unavailable_on_platform",
                    at("source"),
                    format!(
                        "{} из {} недоступна на {platform}: результат отметит её unsupported",
                        request.metric, request.source
                    ),
                );
            }
        }
    }
}

fn validate_environment(scenario: &Scenario, sink: &mut Sink<'_>) {
    let env = &scenario.environment;
    let mut custom = BTreeSet::new();
    for (index, fact) in env.facts.iter().enumerate() {
        let at = |name: &str| field(format!("environment.fact[{index}].{name}"));
        if !is_identifier(&fact.name) {
            sink.error(
                "invalid_identifier",
                at("name"),
                "имя факта — идентификатор",
            );
        }
        if BuiltinFact::ALL.iter().any(|b| b.as_str() == fact.name) {
            sink.error(
                "duplicate_fact",
                at("name"),
                "имя совпадает со встроенным фактом",
            );
        }
        if !custom.insert(fact.name.as_str()) {
            sink.error("duplicate_fact", at("name"), "факт объявлен повторно");
        }
        if fact.description.trim().is_empty() {
            sink.error(
                "empty_text",
                at("description"),
                "опишите факт и его источник",
            );
        }
    }
    let known =
        |name: &str| BuiltinFact::ALL.iter().any(|b| b.as_str() == name) || custom.contains(name);
    for (index, name) in env.record.iter().enumerate() {
        if !known(name) {
            sink.error(
                "unknown_fact",
                field(format!("environment.record[{index}]")),
                format!("факт «{name}» не встроен и не объявлен в environment.fact"),
            );
        }
    }
    for fact in MANDATORY_FACTS {
        if !env.record.iter().any(|name| name == fact.as_str()) {
            sink.error(
                "missing_fact",
                field("environment.record"),
                format!("обязательный факт «{fact}» не записывается"),
            );
        }
    }
    for (index, name) in env.comparable.iter().enumerate() {
        if !env.record.contains(name) {
            sink.error(
                "unknown_fact",
                field(format!("environment.comparable[{index}]")),
                format!("факт «{name}» участвует в сопоставлении, но не записывается"),
            );
        }
    }
    for fact in MANDATORY_COMPARABLE_FACTS {
        if !env.comparable.iter().any(|name| name == fact.as_str()) {
            sink.error(
                "missing_fact",
                field("environment.comparable"),
                format!("факт «{fact}» обязан участвовать в сопоставлении"),
            );
        }
    }
    if has_duplicates(&env.record) || has_duplicates(&env.comparable) {
        sink.error("duplicate_value", field("environment"), "факт повторяется");
    }
    for name in &custom {
        if !env.record.iter().any(|r| r == name) {
            sink.warning(
                "unused_fact",
                field("environment.fact"),
                format!("факт «{name}» объявлен, но не записывается"),
            );
        }
    }
}

/// Причина, по которой путь не является безопасным относительным путём.
fn unsafe_relative(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("путь пуст");
    }
    if path.contains('\\') || path.contains('\0') {
        return Some("путь записывается через «/» без управляющих символов");
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return Some("абсолютный путь запрещён: пути отсчитываются от корня репозитория");
    }
    if parsed.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Some("путь не может выходить за корень репозитория");
    }
    None
}

/// Путь существует и после разрешения ссылок остаётся внутри корня.
fn inside(root: &Path, relative: &str, directory: bool) -> bool {
    match root.join(relative).canonicalize() {
        Ok(path) => {
            path.starts_with(root)
                && if directory {
                    path.is_dir()
                } else {
                    path.is_file()
                }
        }
        Err(_) => false,
    }
}
