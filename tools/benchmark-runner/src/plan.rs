//! Выбор сценария, профиля, параметров и числа прогонов по каталогу.

use benchmark_contract::catalog::{Catalog, LoadedScenario};
use benchmark_contract::placeholder::{self, Placeholder};
use benchmark_contract::scenario::{EnvSpec, InputKind, Profile, ProfileBinding, Program, Step};
use benchmark_contract::vocabulary::{Network, ScenarioStatus, WorkloadKind};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Ошибка запроса: сценарий, профиль или параметры не подходят декларации.
#[derive(Debug, Clone)]
pub struct UsageError(pub String);

#[derive(Debug, Clone)]
pub struct Selection<'c> {
    pub catalog: &'c Catalog,
    pub item: &'c LoadedScenario,
    pub binding: &'c ProfileBinding,
    pub profile: &'c Profile,
    /// Значения всех параметров, у которых они есть (заданные или по умолчанию).
    pub values: BTreeMap<String, String>,
    /// Пути входов с путём: абсолютные, только для подстановки.
    pub input_paths: BTreeMap<String, PathBuf>,
    pub runs: u32,
}

pub fn select<'c>(
    catalog: &'c Catalog,
    id: &str,
    profile_id: &str,
    params: &[(String, String)],
    runs: Option<u32>,
) -> Result<Selection<'c>, UsageError> {
    let item = catalog
        .scenario(id)
        .ok_or_else(|| UsageError(format!("сценарий «{id}» не найден; список — команда list")))?;
    let scenario = &item.scenario;
    if scenario.status == ScenarioStatus::Example {
        return Err(UsageError(format!(
            "сценарий «{id}» учебный (status = \"example\") и никогда не измеряется"
        )));
    }
    let binding = scenario
        .profiles
        .iter()
        .find(|b| b.id == profile_id)
        .ok_or_else(|| {
            let ids: Vec<&str> = scenario.profiles.iter().map(|b| b.id.as_str()).collect();
            UsageError(format!(
                "профиль «{profile_id}» не привязан к сценарию; доступны: {}",
                ids.join(", ")
            ))
        })?;
    let profile = catalog
        .profile(profile_id)
        .ok_or_else(|| UsageError(format!("профиль «{profile_id}» не объявлен в каталоге")))?;

    let mut values = BTreeMap::new();
    for (name, value) in params {
        let Some(param) = scenario.parameters.iter().find(|p| &p.name == name) else {
            let names: Vec<&str> = scenario
                .parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            return Err(UsageError(format!(
                "параметр «{name}» не объявлен; есть: {}",
                if names.is_empty() {
                    "—".to_owned()
                } else {
                    names.join(", ")
                }
            )));
        };
        if !param.choices.is_empty() && !param.choices.contains(value) {
            return Err(UsageError(format!(
                "значение параметра «{name}» не входит в choices: {}",
                param.choices.join(", ")
            )));
        }
        if values.insert(name.clone(), value.clone()).is_some() {
            return Err(UsageError(format!("параметр «{name}» задан повторно")));
        }
    }
    for param in &scenario.parameters {
        if values.contains_key(&param.name) {
            continue;
        }
        if let Some(default) = &param.default {
            values.insert(param.name.clone(), default.clone());
        } else if param.required || binding.required_parameters.contains(&param.name) {
            return Err(UsageError(format!(
                "параметр «{}» обязателен{}: --param {}=<значение> ({})",
                param.name,
                if param.required {
                    ""
                } else {
                    " для этого профиля"
                },
                param.name,
                param.description
            )));
        }
    }

    let mut input_paths = BTreeMap::new();
    for input in &scenario.inputs {
        let path = match input.kind {
            InputKind::ExternalPath => {
                let Some(raw) = input.parameter.as_ref().and_then(|p| values.get(p)) else {
                    continue;
                };
                std::fs::canonicalize(raw).map_err(|e| {
                    UsageError(format!(
                        "путь входа «{}» (параметр «{}») не открывается: {e}",
                        input.id,
                        input.parameter.as_deref().unwrap_or_default()
                    ))
                })?
            }
            InputKind::RepoFile => match &input.path {
                Some(path) => catalog.repository_root.join(path),
                None => continue,
            },
            InputKind::RepoTree => catalog.repository_root.clone(),
            InputKind::DownloadedArtifact => continue,
        };
        input_paths.insert(input.id.clone(), path);
    }

    let policy = &scenario.runs;
    let runs = match runs {
        Some(n) => n,
        None if scenario.kind == WorkloadKind::LongE2e => {
            return Err(UsageError(format!(
                "длительный сценарий не запускается с числом прогонов по умолчанию: укажите --runs явно ({}…{}{})",
                if policy.single_run_allowed {
                    1
                } else {
                    policy.min_runs
                },
                policy.max_runs,
                if policy.single_run_allowed && policy.min_runs > 1 {
                    format!("; success требует не меньше {}", policy.min_runs)
                } else {
                    String::new()
                }
            )));
        }
        None => policy.min_runs,
    };
    let lowest = if policy.single_run_allowed {
        1
    } else {
        policy.min_runs
    };
    if runs < lowest || runs > policy.max_runs {
        return Err(UsageError(format!(
            "--runs {runs} вне допустимого диапазона {lowest}…{}",
            policy.max_runs
        )));
    }
    Ok(Selection {
        catalog,
        item,
        binding,
        profile,
        values,
        input_paths,
        runs,
    })
}

impl Selection<'_> {
    pub fn offline(&self) -> bool {
        self.profile.network == Network::Offline
    }

    pub fn substitute(&self, text: &str) -> Result<String, String> {
        placeholder::substitute(text, |p| match p {
            Placeholder::Param(name) => self.values.get(name).cloned(),
            Placeholder::Input(id) => self
                .input_paths
                .get(id)
                .map(|p| p.to_string_lossy().into_owned()),
        })
    }

    pub fn substitute_all(&self, items: &[String]) -> Result<Vec<String>, String> {
        items.iter().map(|i| self.substitute(i)).collect()
    }

    /// Аргументы цели с добавкой профиля.
    pub fn target_args(&self) -> Result<Vec<String>, String> {
        let target = &self.item.scenario.target;
        let mut args = self.substitute_all(&target.args)?;
        args.extend(self.substitute_all(&self.binding.args_append)?);
        Ok(args)
    }

    /// Окружение процесса: только унаследованные по списку и заданные явно.
    pub fn environment(&self, spec: &EnvSpec) -> Result<Vec<(String, String)>, String> {
        let mut env = Vec::new();
        for name in &spec.inherit {
            if let Some(value) = std::env::var_os(name) {
                env.push((name.clone(), value.to_string_lossy().into_owned()));
            }
        }
        for (name, value) in &spec.set {
            env.push((name.clone(), self.substitute(value)?));
        }
        Ok(env)
    }

    pub fn working_dir(&self, relative: &str) -> PathBuf {
        self.catalog.repository_root.join(relative)
    }

    /// Все программы серии: цель, шаги жизненного цикла и пробы входов.
    pub fn programs(&self) -> Vec<&Program> {
        let scenario = &self.item.scenario;
        let mut programs: Vec<&Program> = vec![&scenario.target.program];
        let steps = self
            .lifecycle_steps()
            .into_iter()
            .chain(scenario.inputs.iter().filter_map(|i| i.probe.as_ref()));
        for step in steps {
            if !programs.contains(&&step.program) {
                programs.push(&step.program);
            }
        }
        programs
    }

    pub fn lifecycle_steps(&self) -> Vec<&Step> {
        let scenario = &self.item.scenario;
        scenario
            .setup
            .iter()
            .chain(&scenario.cleanup)
            .chain(&self.binding.setup)
            .chain(&self.binding.prepare)
            .chain(&self.binding.conclude)
            .collect()
    }

    pub fn repository_root(&self) -> &Path {
        &self.catalog.repository_root
    }
}
