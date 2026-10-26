//! Интерфейс командной строки: одна основная команда и выбор сценариев.

use crate::agent::AgentArgs;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "anki-e2e",
    version,
    about = "Изолированный E2E-стенд настоящей Linux Anki Desktop для anki-repo anki (только по явной команде)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Полный набор на чистом контейнере: smoke, addons, patch, crowdanki, process, errors.
    Full(RunArgs),
    /// Обнаружение Anki, первый запуск, doctor, launch/stop без аддонов.
    Smoke(RunArgs),
    /// Первоначальная установка AnkiConnect и CrowdAnki через setup и штатный установщик.
    Addons(RunArgs),
    /// Жизненный цикл патча: install/status/repair/remove/restore.
    Patch(RunArgs),
    /// Настоящие export/import CrowdAnki, отмена и отказы предвалидации.
    Crowdanki(RunArgs),
    /// Процессы Anki: launch/restart/stop, модальное окно, смерть процесса, чужой процесс.
    Process(RunArgs),
    /// Отказы и восстановление: отказ установки, disabled, конфликт порта и профиля, сломанный аддон.
    Errors(RunArgs),
    /// Сценарии с основными диалогами и уведомлениями: addons и crowdanki.
    Gui(RunArgs),
    /// Произвольный список сценариев через запятую.
    Run {
        #[arg(long, value_delimiter = ',', required = true)]
        scenarios: Vec<String>,
        #[command(flatten)]
        args: RunArgs,
    },
    /// Только выбрать релиз Anki и получить проверенный архив; Docker не запускается.
    Resolve(ReleaseArgs),
    /// Удалить контейнеры стенда; по флагам — также его образы и кэш.
    Clean {
        #[arg(long)]
        images: bool,
        #[arg(long)]
        cache: bool,
        /// Какую конфигурацию Docker использовать для поиска ресурсов стенда.
        #[arg(long, value_enum, default_value = "isolated")]
        docker_config: DockerConfig,
        #[arg(long)]
        work_dir: Option<PathBuf>,
    },
    /// Внутренний режим исполнителя в контейнере (запускается runner).
    #[command(hide = true)]
    Agent(AgentArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ReleaseArgs {
    /// Конкретная версия для регрессионного прогона; по умолчанию — последняя стабильная.
    #[arg(long)]
    pub anki_version: Option<String>,
    /// Разрешить явно заданный prerelease.
    #[arg(long, requires = "anki_version")]
    pub allow_prerelease: bool,
    /// Без сети: только заранее проверенный артефакт (нужны --anki-version и --sha256).
    #[arg(long, requires_all = ["anki_version", "sha256"])]
    pub offline: bool,
    /// Ожидаемый SHA-256 архива в офлайн-режиме.
    #[arg(long, requires = "offline")]
    pub sha256: Option<String>,
    /// Локальный архив официального релиза для офлайн-режима.
    #[arg(long, requires = "offline")]
    pub anki_archive: Option<PathBuf>,
    /// Запуск без контрольной суммы, опубликованной для релиза (сумма будет только вычисленной).
    #[arg(long)]
    pub allow_unverified_archive: bool,
    /// Каталог кэша, сборки и отчётов (по умолчанию <репозиторий>/target/anki-e2e).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Keep {
    Never,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DockerConfig {
    /// Собственный пустой DOCKER_CONFIG: без пользовательских credentials.
    Isolated,
    /// Конфигурация пользователя (например, для нестандартного контекста Docker).
    Inherit,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub release: ReleaseArgs,
    /// Сохранить контейнер для диагностики (по умолчанию удаляется).
    #[arg(long, value_enum, default_value = "never")]
    pub keep_container: Keep,
    /// Локальный VNC-просмотр дисплея: публикуется только на 127.0.0.1.
    #[arg(long)]
    pub vnc: bool,
    #[arg(long, default_value_t = 5900)]
    pub vnc_port: u16,
    #[arg(long, default_value = "4")]
    pub cpus: String,
    #[arg(long, default_value = "6g")]
    pub memory: String,
    #[arg(long, default_value_t = 4096)]
    pub pids_limit: u32,
    /// Предел исполнения сценариев в контейнере. Сборка образов и загрузка
    /// архива в него не входят: их ограничивают прерывание и контроль зависания.
    #[arg(long, default_value_t = 120)]
    pub timeout_minutes: u64,
    /// Отказаться от прогона при незакоммиченных изменениях.
    #[arg(long)]
    pub require_clean: bool,
    #[arg(long, value_enum, default_value = "isolated")]
    pub docker_config: DockerConfig,
    /// Пересобрать образы без кэша Docker.
    #[arg(long)]
    pub rebuild: bool,
}

pub const FULL: &[&str] = &["smoke", "addons", "patch", "crowdanki", "process", "errors"];
pub const GUI: &[&str] = &["addons", "crowdanki"];

impl Command {
    pub fn suite(&self) -> Option<(&'static str, Vec<String>, &RunArgs)> {
        let one = |n: &str| vec![n.to_owned()];
        Some(match self {
            Command::Full(a) => ("full", FULL.iter().map(|s| (*s).to_owned()).collect(), a),
            Command::Smoke(a) => ("smoke", one("smoke"), a),
            Command::Addons(a) => ("addons", one("addons"), a),
            Command::Patch(a) => ("patch", one("patch"), a),
            Command::Crowdanki(a) => ("crowdanki", one("crowdanki"), a),
            Command::Process(a) => ("process", one("process"), a),
            Command::Errors(a) => ("errors", one("errors"), a),
            Command::Gui(a) => ("gui", GUI.iter().map(|s| (*s).to_owned()).collect(), a),
            Command::Run { scenarios, args } => ("run", scenarios.clone(), args),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_run_selects_latest_and_removes_container() {
        let cli = Cli::try_parse_from(["anki-e2e", "full"]).unwrap();
        let (name, scenarios, args) = cli.command.suite().unwrap();
        assert_eq!(name, "full");
        assert_eq!(scenarios, FULL);
        assert!(args.release.anki_version.is_none());
        assert_eq!(args.keep_container, Keep::Never);
        assert!(!args.vnc);
        assert_eq!(args.docker_config, DockerConfig::Isolated);
    }

    #[test]
    fn offline_requires_version_and_digest() {
        assert!(Cli::try_parse_from(["anki-e2e", "smoke", "--offline"]).is_err());
        assert!(
            Cli::try_parse_from([
                "anki-e2e",
                "smoke",
                "--offline",
                "--anki-version",
                "26.09.2"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["anki-e2e", "smoke", "--sha256", "00"]).is_err());
        let ok = Cli::try_parse_from([
            "anki-e2e",
            "smoke",
            "--offline",
            "--anki-version",
            "26.09.2",
            "--sha256",
            "ab",
        ]);
        assert!(ok.is_ok());
        assert!(Cli::try_parse_from(["anki-e2e", "full", "--allow-prerelease"]).is_err());
    }

    #[test]
    fn run_accepts_scenario_list() {
        let cli = Cli::try_parse_from([
            "anki-e2e",
            "run",
            "--scenarios",
            "smoke,patch",
            "--anki-version",
            "26.09.2",
        ])
        .unwrap();
        let (_, scenarios, args) = cli.command.suite().unwrap();
        assert_eq!(scenarios, ["smoke", "patch"]);
        assert_eq!(args.release.anki_version.as_deref(), Some("26.09.2"));
    }

    #[test]
    fn clean_selects_docker_config() {
        let default = Cli::try_parse_from(["anki-e2e", "clean"]).unwrap();
        let Command::Clean { docker_config, .. } = default.command else {
            panic!("expected clean command");
        };
        assert_eq!(docker_config, DockerConfig::Isolated);

        let inherit =
            Cli::try_parse_from(["anki-e2e", "clean", "--docker-config", "inherit"]).unwrap();
        let Command::Clean { docker_config, .. } = inherit.command else {
            panic!("expected clean command");
        };
        assert_eq!(docker_config, DockerConfig::Inherit);
    }
}
