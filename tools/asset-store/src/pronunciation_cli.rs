//! CLI японских MP3: локальный импорт, Forvo Web и явное подтверждение.
use crate::pronunciation::{self as audio, Pronunciation, PronunciationDomainPolicy, batch};
use crate::{
    AssetError, AssetStore, ErrorCode, HumanAttestationRequest, HumanDecision, StoreOptions,
};
use clap::{Parser, Subcommand, ValueEnum};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "pronunciation-assets",
    about = "Локальные MP3 японских произношений"
)]
pub struct Cli {
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[arg(long, global = true, default_value = ".")]
    repository_root: PathBuf,
    #[arg(long, global = true, value_enum, default_value = "json")]
    output: Output,
    #[command(subcommand)]
    command: Action,
}
#[derive(Clone, Copy, ValueEnum)]
enum Output {
    Json,
    Text,
}
#[derive(Clone, Copy, ValueEnum)]
enum Decision {
    Approve,
    Reject,
}
#[derive(Subcommand)]
enum Action {
    /// Forvo: общий путь выбирает транспорт по `--mode`/конфигурации
    /// (`chromium_first` — собственный профиль Chromium, затем адаптер браузера;
    /// `extension_only` — только адаптер браузера).
    Forvo {
        #[arg(long)]
        profile: Option<PathBuf>,
        #[command(flatten)]
        generic: GenericForvo,
        #[command(subcommand)]
        action: ForvoAction,
    },
    /// Balanced Normalizer и технический QualityGate: только локальные файлы,
    /// без Forvo и сети. Исходники не изменяются.
    Normalizer {
        #[command(subcommand)]
        action: crate::pronunciation_normalizer_cli::NormalizerAction,
    },
    /// Локальное улучшение речи (DeepFilterNet3 на CPU) поверх Balanced:
    /// триаж, Enhancement QC и автоматическая маршрутизация, без сети.
    Enhancer {
        #[command(subcommand)]
        action: crate::pronunciation_enhancer_cli::EnhancerAction,
    },
    /// Локальное распознавание японского произношения (фонемный CTC HuBERT и
    /// независимая вторая модель на CPU): поиск целевого чтения, осведомлённая
    /// о цели обработка, извлечение и повторная проверка, без сети.
    Recognizer {
        #[command(subcommand)]
        action: crate::pronunciation_recognizer_cli::RecognizerAction,
    },
    Init,
    Inspect,
    Verify,
    Probe {
        #[arg(long)]
        file: PathBuf,
    },
    Import {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_source_sha256: Option<String>,
        #[arg(long)]
        replace_expected_sha256: Option<String>,
    },
    Validate {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        sha256: String,
    },
    Attest {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        sha256: String,
        #[arg(long, value_enum)]
        decision: Decision,
        #[arg(long)]
        reason: String,
    },
    /// Проверяет выдачу набора; результат содержит имена/SHA, но не аудиобайты.
    Read {
        #[arg(long)]
        requests: PathBuf,
    },
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
    },
    BatchResume {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
    },
    BatchStatus {
        #[arg(long)]
        batch_id: String,
    },
    BatchCancel {
        #[arg(long)]
        batch_id: String,
    },
}

/// Настройки общего пользовательского пути Forvo (`fetch`, `batch-*`, `config`).
/// Явные `forvo brave|browser` выбирают маршрут собственными аргументами, но
/// читают и строго проверяют общий конфиг для `update_check`; ошибка конфигурации
/// поэтому блокирует получение MP3 до запуска операции.
#[derive(clap::Args, Clone, Default)]
struct GenericForvo {
    /// Стратегия транспорта: chromium_first (по умолчанию) или extension_only;
    /// перекрывает forvo.mode конфигурации.
    #[arg(long, value_enum)]
    mode: Option<crate::user_config::ForvoMode>,
    /// Цель адаптера браузера (перекрывает forvo.browser_target конфигурации).
    #[arg(long)]
    browser_target: Option<String>,
    /// Каталог состояния адаптера браузера (как --state-dir у forvo browser).
    #[arg(long)]
    adapter_state_dir: Option<PathBuf>,
    /// Файл общей пользовательской конфигурации (иначе ANKI_DECK_TOOLKIT_CONFIG
    /// или $XDG_CONFIG_HOME/anki-deck-toolkit/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

impl GenericForvo {
    fn settings(&self) -> Result<crate::user_config::ForvoSettings, AssetError> {
        crate::user_config::forvo(&crate::user_config::ForvoOverrides {
            config: self.config.clone(),
            mode: self.mode,
            browser_target: self.browser_target.clone(),
        })
    }
}

#[derive(Subcommand)]
enum ForvoAction {
    /// Показать итоговую стратегию транспорта, её источники и настроенные цели адаптера.
    Config,
    /// Обычный Brave пользователя через собственное расширение и loopback bridge
    /// (цель `brave`; прежний интерфейс, не зависит от цели по умолчанию).
    Brave {
        /// Приватный каталог сопряжения, журналов и свидетельств вне репозитория.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[command(subcommand)]
        action: BraveAction,
    },
    /// Обычный Chromium-браузер пользователя: Brave, Google Chrome, Microsoft
    /// Edge или Vivaldi. Каждая настроенная цель — отдельное сопряжение.
    Browser {
        /// Приватный каталог сопряжения, журналов и свидетельств вне репозитория.
        #[arg(long, global = true)]
        state_dir: Option<PathBuf>,
        /// Цель (по умолчанию — цель по умолчанию или единственная настроенная).
        #[arg(long, global = true)]
        target: Option<String>,
        #[command(subcommand)]
        action: BrowserAction,
    },
    /// Войдите вручную в открывшемся окне; пароль не передаётся CLI.
    Login {
        #[arg(long, default_value_t = 300, value_parser=clap::value_parser!(u64).range(1..=1800))]
        timeout_seconds: u64,
        #[arg(long)]
        surface: Option<String>,
    },
    Check,
    Candidates {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
    },
    Fetch {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        replace_expected_sha256: Option<String>,
    },
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
    },
    BatchResume {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
    },
    BatchStatus {
        #[arg(long)]
        batch_id: String,
    },
    BatchCancel {
        #[arg(long)]
        batch_id: String,
    },
}

#[derive(Subcommand)]
enum BraveAction {
    /// Подготовить каталог расширения и секрет сопряжения; повтор обновляет код.
    Setup {
        #[arg(long)]
        install_dir: Option<PathBuf>,
        #[arg(long, value_parser=clap::value_parser!(u16).range(1024..))]
        port: Option<u16>,
        #[arg(long)]
        rotate_secret: bool,
        /// Нестандартный путь к Brave (Windows-путь в WSL или Linux-путь).
        #[arg(long)]
        executable: Option<String>,
        /// Снять закрепление экземпляра расширения (оно переустановлено).
        #[arg(long)]
        repin: bool,
    },
    #[command(flatten)]
    Run(RunAction),
}

#[derive(Subcommand)]
enum BrowserAction {
    /// Подготовить расширение и сопряжение выбранного браузера; повтор обновляет код.
    Setup {
        #[arg(long, value_enum)]
        browser: Option<crate::forvo::brave::BrowserKind>,
        /// Нестандартный путь к браузеру (Windows-путь в WSL или Linux-путь).
        #[arg(long)]
        executable: Option<String>,
        #[arg(long)]
        install_dir: Option<PathBuf>,
        #[arg(long, value_parser=clap::value_parser!(u16).range(1024..))]
        port: Option<u16>,
        #[arg(long)]
        rotate_secret: bool,
        /// Снять закрепление экземпляра расширения (оно переустановлено).
        #[arg(long)]
        repin: bool,
        /// Сделать цель целью по умолчанию.
        #[arg(long)]
        make_default: bool,
    },
    /// Найти установленные поддерживаемые браузеры (без изменений).
    Discover,
    /// Показать настроенные цели без секретов.
    Targets,
    /// Сделать --target целью по умолчанию.
    Select,
    #[command(flatten)]
    Run(RunAction),
}

#[derive(Subcommand)]
enum RunAction {
    /// Проверить соединение, версию, браузер, loopback и (по флагам) Forvo и восстановление.
    Doctor {
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
        #[arg(long)]
        forvo: bool,
        #[arg(long)]
        reconnect: bool,
        #[arg(long)]
        reload: bool,
    },
    /// Живая проверка одной записи: вкладка, вход, кандидаты, загрузка и MP3.
    Smoke {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
        #[arg(long)]
        probe_downloads_api: bool,
        #[arg(long)]
        close_tab: bool,
        /// Оставить собственный скачанный MP3 в загрузках браузера (по умолчанию удаляется после копии в свидетельства).
        #[arg(long)]
        keep_download: bool,
    },
    /// Получить оригинальный MP3 одного слова через браузер цели и импортировать его
    /// в AssetStore (уровень доверия прежний: техническая проверка ≠ Verified).
    Fetch {
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        replace_expected_sha256: Option<String>,
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
        /// Закрыть собственную вкладку адаптера по завершении.
        #[arg(long)]
        close_tab: bool,
    },
    /// Создать пакет расширения и начать его выполнение; выбранная цель
    /// сохраняется до подключения, затем пакет закрепляется за сессией.
    BatchStart {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
        #[arg(long)]
        close_tab: bool,
    },
    /// Продолжить пакет расширения в его браузерной сессии; пакет CDP этой командой не продолжается.
    BatchResume {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value_t = 128)]
        max_items: usize,
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
        #[arg(long)]
        close_tab: bool,
    },
    /// Закрыть собственную вкладку адаптера.
    Close {
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
    },
    /// Проверить доступную версию адаптера в канале выпусков (без установки).
    UpdateCheck {
        /// Каталог скачанного выпуска вместо GitHub Releases.
        #[arg(long)]
        from_dir: Option<PathBuf>,
        /// Файл с открытым ключом Ed25519 (hex) вместо встроенного корня доверия.
        #[arg(long)]
        trusted_public_key: Option<PathBuf>,
    },
    /// Проверить подписанный выпуск и установить его в прежний каталог цели с
    /// перезагрузкой расширения; сопряжение и идентичность сохраняются.
    Update {
        /// Каталог скачанного выпуска вместо GitHub Releases (офлайн).
        #[arg(long)]
        from_dir: Option<PathBuf>,
        /// Версия выпуска; по умолчанию — наибольшая опубликованная.
        #[arg(long)]
        version: Option<String>,
        /// Файл с открытым ключом Ed25519 (hex) вместо встроенного корня доверия.
        #[arg(long)]
        trusted_public_key: Option<PathBuf>,
        /// Переустановить ту же версию, если файлы на диске отличаются от выпуска.
        #[arg(long)]
        reinstall: bool,
        #[arg(long, default_value_t = 45, value_parser=clap::value_parser!(u64).range(1..=600))]
        wait_seconds: u64,
    },
}

/// Команда адаптера в общем виде: каталог состояния, цель и действие.
enum AdapterCommand<'a> {
    Setup(crate::forvo::brave::SetupOptions),
    Discover,
    Targets,
    Select(String),
    Run(&'a RunAction),
}

fn adapter_command(
    action: &ForvoAction,
) -> Result<Option<(PathBuf, Option<String>, AdapterCommand<'_>)>, AssetError> {
    use crate::forvo::brave::{BrowserKind, LEGACY_TARGET, SetupOptions};
    Ok(Some(match action {
        ForvoAction::Brave { state_dir, action } => {
            let dir = brave_state_dir(state_dir.as_deref())?;
            let target = Some(LEGACY_TARGET.to_owned());
            let command = match action {
                BraveAction::Setup {
                    install_dir,
                    port,
                    rotate_secret,
                    executable,
                    repin,
                } => AdapterCommand::Setup(SetupOptions {
                    target: target.clone(),
                    browser: Some(BrowserKind::Brave),
                    executable: executable.clone(),
                    install_dir: install_dir.clone(),
                    port: *port,
                    rotate_secret: *rotate_secret,
                    repin: *repin,
                    ..SetupOptions::new(dir.clone())
                }),
                BraveAction::Run(run) => AdapterCommand::Run(run),
            };
            (dir, target, command)
        }
        ForvoAction::Browser {
            state_dir,
            target,
            action,
        } => {
            let dir = brave_state_dir(state_dir.as_deref())?;
            let command = match action {
                BrowserAction::Setup {
                    browser,
                    executable,
                    install_dir,
                    port,
                    rotate_secret,
                    repin,
                    make_default,
                } => AdapterCommand::Setup(SetupOptions {
                    target: target.clone(),
                    browser: *browser,
                    executable: executable.clone(),
                    install_dir: install_dir.clone(),
                    port: *port,
                    rotate_secret: *rotate_secret,
                    repin: *repin,
                    make_default: *make_default,
                    ..SetupOptions::new(dir.clone())
                }),
                BrowserAction::Discover => AdapterCommand::Discover,
                BrowserAction::Targets => AdapterCommand::Targets,
                BrowserAction::Select => {
                    AdapterCommand::Select(target.clone().ok_or_else(|| {
                        crate::forvo::brave::bridge_error(
                            "target_required",
                            "Укажите --target <имя> для select",
                        )
                    })?)
                }
                BrowserAction::Run(run) => AdapterCommand::Run(run),
            };
            (dir, target.clone(), command)
        }
        _ => return Ok(None),
    }))
}

/// Все компоненты источника открываются с NOFOLLOW. Проверяется как
/// лексическая граница, так и фактический объект по открытому дескриптору.
pub(crate) fn read_local(
    path: &Path,
    protected: &BTreeSet<PathBuf>,
    maximum: u64,
) -> Result<Vec<u8>, AssetError> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::fs::File;
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| AssetError::io("cwd", e))?
            .join(path)
    };
    if absolute
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "компоненты .. запрещены",
        ));
    }
    // Защищённые корни отсекаются до открытия даже их каталогов.
    if protected.iter().any(|root| {
        absolute.starts_with(root)
            || std::fs::canonicalize(root)
                .ok()
                .is_some_and(|root| absolute.starts_with(root))
    }) {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "аудио CLI не открывает decks/**",
        ));
    }
    let fail = |e| AssetError::io("безопасное открытие локального файла", e);
    let mut current = File::from(
        open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| fail(e.into()))?,
    );
    let parts: Vec<_> = absolute
        .components()
        .filter(|c| !matches!(c, Component::RootDir | Component::CurDir))
        .collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(AssetError::new(
                ErrorCode::PathTraversal,
                "компоненты .. запрещены",
            ));
        };
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if i + 1 != parts.len() {
            flags |= OFlags::DIRECTORY;
        }
        current =
            File::from(openat(&current, *name, flags, Mode::empty()).map_err(|e| fail(e.into()))?);
    }
    if !current.metadata().map_err(fail)?.is_file() {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "нужен обычный файл",
        ));
    }
    let canonical = std::fs::read_link(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&current)
    ))
    .map_err(fail)?;
    for root in protected {
        if canonical.starts_with(root)
            || std::fs::canonicalize(root)
                .ok()
                .is_some_and(|r| canonical.starts_with(r))
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "аудио CLI не читает защищённые данные",
            ));
        }
    }
    let mut bytes = Vec::new();
    current
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(fail)?;
    if bytes.len() as u64 > maximum {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            "превышен предел размера файла",
        ));
    }
    Ok(bytes)
}
// Существующий предок разрешается до создания каталогов. Так учитываются
// symlink-алиасы корня хранилища и отсутствующие конечные компоненты пути.
fn resolved_location(path: &Path) -> Result<PathBuf, AssetError> {
    let absolute = std::path::absolute(path).map_err(|e| AssetError::io("абсолютный путь", e))?;
    if absolute
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "путь не может содержать ..",
        ));
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(ancestor)
                    .map_err(|e| AssetError::io("разрешение существующего предка", e))?;
                for name in missing.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or_else(|| {
                    AssetError::io(
                        "предок пути",
                        std::io::Error::other("нет существующего предка"),
                    )
                })?);
                ancestor = ancestor.parent().expect("абсолютный путь имеет родителя");
            }
            Err(e) => return Err(AssetError::io("проверка предка пути", e)),
        }
    }
}
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn execute(cli: &Cli) -> Result<serde_json::Value, AssetError> {
    let repository = std::fs::canonicalize(&cli.repository_root)
        .map_err(|e| AssetError::io("корень репозитория", e))?;
    let root = cli
        .store
        .clone()
        .unwrap_or_else(|| repository.join(".asset-store/pronunciation-audio"));
    let mut protected = BTreeSet::from([repository.join("decks")]);
    // Общая проверка защищает хранилище, а read_local не открывает защищённые корни источников.
    for parent in root.ancestors() {
        if parent.join("AGENTS.md").is_file() {
            protected.insert(parent.join("decks"));
        }
    }
    if let Action::Forvo {
        profile,
        generic,
        action,
    } = &cli.command
    {
        let profile = std::path::absolute(
            profile
                .clone()
                .unwrap_or_else(|| repository.join(".codex/local/forvo-profile")),
        )
        .map_err(|e| AssetError::io("путь профиля", e))?;
        let resolved_profile = resolved_location(&profile)?;
        let resolved_store = resolved_location(&root)?;
        if overlaps(&resolved_profile, &resolved_store) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "профиль Forvo и аудиохранилище должны находиться в отдельных каталогах без вложенности",
            ));
        }
        for path in &protected {
            if overlaps(
                &profile,
                &std::path::absolute(path).map_err(|e| AssetError::io("защищённый путь", e))?,
            ) || overlaps(&resolved_profile, &resolved_location(path)?)
            {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "профиль Forvo не может пересекаться с decks/**",
                ));
            }
        }
        return execute_forvo(&root, &profile, generic, action, &protected, cli.output);
    }
    if let Action::Normalizer { action } = &cli.command {
        return crate::pronunciation_normalizer_cli::execute(action, &root, &protected);
    }
    if let Action::Enhancer { action } = &cli.command {
        return crate::pronunciation_enhancer_cli::execute(action, &protected);
    }
    if let Action::Recognizer { action } = &cli.command {
        return crate::pronunciation_recognizer_cli::execute(action, &protected);
    }
    if let Action::Probe { file } = &cli.command {
        return Ok(
            serde_json::json!({"schema_version":1,"outcome":"ok","result":audio::probe_mp3(&read_local(file,&protected,audio::MAX_AUDIO_BYTES)?)?}),
        );
    }
    let options = protected
        .iter()
        .fold(StoreOptions::new(&root), |options, path| {
            options.protect_from(path)
        });
    let store = if matches!(
        cli.command,
        Action::Init | Action::Import { .. } | Action::BatchStart { .. }
    ) {
        AssetStore::open_with_policy(options, PronunciationDomainPolicy)?
    } else {
        AssetStore::open_existing_with_policy(options, PronunciationDomainPolicy)?
    };
    let read_audio = |path: &Path| read_local(path, &protected, audio::MAX_AUDIO_BYTES);
    let result = match &cli.command {
        Action::Forvo { .. } => unreachable!("Forvo обработан до открытия хранилища"),
        Action::Normalizer { .. } => unreachable!("нормализатор обработан до открытия хранилища"),
        Action::Enhancer { .. } => unreachable!("улучшение обработано до открытия хранилища"),
        Action::Recognizer { .. } => {
            unreachable!("распознавание обработано до открытия хранилища")
        }
        Action::Init => {
            serde_json::json!({"store_id":store.store_id(),"initialized":store.initialized_on_open()})
        }
        Action::Inspect => serde_json::json!({"assets":store.verify_integrity()?}),
        Action::Verify => {
            let records = store.verify_integrity()?;
            // Перепроверяет фактические потоки, а не только SHA/манифест.
            let report =
                store.validate(crate::SelectionMode::Full, &audio::AudioTechnicalValidator)?;
            if !report.blockers.is_empty() {
                return Err(AssetError::with_details(
                    ErrorCode::ValidatorFailure,
                    "техническая проверка корпуса не завершена",
                    serde_json::json!(report),
                ));
            }
            serde_json::json!({"assets_checked":records.len(),"report":report})
        }
        Action::Probe { .. } => unreachable!("probe выполнен до открытия хранилища"),
        Action::Import {
            surface,
            reading,
            file,
            expected_source_sha256,
            replace_expected_sha256,
        } => {
            let request = Pronunciation::new(surface, reading)?;
            let bytes = read_audio(file)?;
            if expected_source_sha256
                .as_ref()
                .is_some_and(|sha| sha != &crate::hashing::sha256_hex(&bytes))
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "источник отличается от ожидаемого SHA",
                ));
            }
            let source = file.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
                AssetError::new(ErrorCode::InvalidIdentity, "нет имени источника")
            })?;
            serde_json::json!({"asset":audio::import_bytes(&store,&request,&bytes,source,replace_expected_sha256.clone())?})
        }
        Action::Validate {
            surface,
            reading,
            sha256,
        } => {
            let report = store.validate_exact(
                &Pronunciation::new(surface, reading)?.identity(),
                sha256,
                &audio::AudioTechnicalValidator,
            )?;
            if !report.blockers.is_empty() {
                return Err(AssetError::with_details(
                    ErrorCode::ValidatorFailure,
                    "техническая проверка кандидата не завершена",
                    serde_json::json!(report),
                ));
            }
            serde_json::json!(report)
        }
        Action::Attest {
            surface,
            reading,
            sha256,
            decision,
            reason,
        } => {
            let outcome = store.attest(HumanAttestationRequest {
                identity: Pronunciation::new(surface, reading)?.identity(),
                expected_sha256: sha256.clone(),
                decision: match decision {
                    Decision::Approve => HumanDecision::Approve,
                    Decision::Reject => HumanDecision::Reject,
                },
                reason: reason.clone(),
            })?;
            serde_json::json!({"asset":outcome.asset,"changed":outcome.changed})
        }
        Action::Read { requests } => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Requests {
                schema_version: u32,
                items: Vec<Pronunciation>,
            }
            let request: Requests =
                serde_json::from_slice(&read_local(requests, &protected, 1024 * 1024)?)
                    .map_err(|e| AssetError::new(ErrorCode::InvalidIdentity, e.to_string()))?;
            if request.schema_version != 1 || request.items.len() > batch::MAX_BATCH_ITEMS {
                return Err(AssetError::new(
                    ErrorCode::UnsupportedSchemaVersion,
                    "неподдерживаемая схема/размер выбора",
                ));
            }
            let items = request
                .items
                .iter()
                .map(|r| Pronunciation::new(&r.surface, &r.reading))
                .collect::<Result<Vec<_>, _>>()?;
            let requests = items
                .iter()
                .map(|item| (item.identity(), crate::DetectedFormat::Mp3))
                .collect::<Vec<_>>();
            let mut records: Vec<crate::AssetRecord> = Vec::with_capacity(items.len());
            AssetStore::visit_verified_variants_with_policy(
                store.root(),
                &requests,
                &audio::technical_validator_identity(),
                &PronunciationDomainPolicy,
                |_, read| {
                    // Ошибка любого элемента прерывает весь ответ; байты сразу освобождаются.
                    records.push(read?.record);
                    Ok(())
                },
            )?;
            serde_json::json!({"assets":records})
        }
        Action::BatchStart {
            batch_id,
            plan,
            max_items,
        } => {
            let parsed: batch::ImportPlan =
                serde_json::from_slice(&read_local(plan, &protected, 1024 * 1024)?)
                    .map_err(|e| AssetError::new(ErrorCode::InvalidIdentity, e.to_string()))?;
            batch::start(
                &store,
                batch_id,
                parsed,
                plan.parent().unwrap_or(Path::new(".")),
                read_audio,
            )?;
            serde_json::json!(batch::resume(&store, batch_id, *max_items)?)
        }
        Action::BatchResume {
            batch_id,
            max_items,
        } => serde_json::json!(batch::resume(&store, batch_id, *max_items)?),
        Action::BatchStatus { batch_id } => serde_json::json!(batch::status(&store, batch_id)?),
        Action::BatchCancel { batch_id } => serde_json::json!(batch::cancel(&store, batch_id)?),
    };
    Ok(serde_json::json!({"schema_version":1,"outcome":"ok","result":result}))
}
pub fn main() -> u8 {
    let cli = Cli::parse();
    let (value, code) = match execute(&cli) {
        Ok(value) => {
            let code = if value["outcome"] == "cancelled" {
                130
            } else if value["outcome"] == "paused" {
                if value["result"]["paused"]["code"] == "interrupted" {
                    130
                } else {
                    3
                }
            } else {
                0
            };
            (value, code)
        }
        Err(error) => (
            serde_json::json!({"schema_version":1,"outcome":"error","error":{"code":error.code.as_str(),"message":error.message,"details":error.details}}),
            error.exit_code(),
        ),
    };
    match cli.output {
        Output::Json => println!("{}", serde_json::to_string(&value).expect("JSON")),
        Output::Text => println!("{}", serde_json::to_string_pretty(&value).expect("JSON")),
    }
    code
}

fn execute_forvo(
    root: &Path,
    profile: &Path,
    generic: &GenericForvo,
    action: &ForvoAction,
    protected: &BTreeSet<PathBuf>,
    output: Output,
) -> Result<serde_json::Value, AssetError> {
    use crate::forvo::{self, AuthState, ForvoSession};
    let open_store = |initialize: bool| {
        let options = protected
            .iter()
            .fold(StoreOptions::new(root), |o, p| o.protect_from(p));
        if initialize {
            AssetStore::open_with_policy(options, PronunciationDomainPolicy)
        } else {
            AssetStore::open_existing_with_policy(options, PronunciationDomainPolicy)
        }
    };
    let adapter_dir = || brave_state_dir(generic.adapter_state_dir.as_deref());
    let runtime = tokio::runtime::Runtime::new().map_err(|e| AssetError::io("runtime", e))?;
    let result=runtime.block_on(async {
        match action {
            ForvoAction::Brave{..}|ForvoAction::Browser{..}=>{
                let (state_dir,target,command)=adapter_command(action)?.expect("команда адаптера");
                let update_settings = match &command {
                    AdapterCommand::Run(
                        RunAction::BatchStart { .. }
                        | RunAction::BatchResume { .. }
                        | RunAction::Fetch { .. },
                    ) => Some(generic.settings()?),
                    _ => None,
                };
                match command {
                    AdapterCommand::Run(RunAction::BatchStart{batch_id,plan,max_items,wait_seconds,close_tab})=>{
                        let mut options=brave_options(&state_dir,target,*wait_seconds,*close_tab);
                        let plan=read_plan(plan,protected)?;
                        let resolved_target=forvo::brave::targets::load_target(
                            &state_dir,
                            options.target.as_deref(),
                        )?.target;
                        options.target=Some(resolved_target.clone());
                        let store=open_store(true)?;
                        forvo::batch::start_with_browser_target(&store,batch_id,plan,resolved_target)?;
                        let value=run_forvo_batch_logged(&store,batch_id,forvo::batch::Backend::Brave(&options),*max_items,output,None).await?;
                        Ok(with_update_notice(value,&state_dir,options.target.as_deref(),update_settings.as_ref().expect("настройки проверки обновления")).await)
                    }
                    AdapterCommand::Run(RunAction::BatchResume{batch_id,max_items,wait_seconds,close_tab})=>{
                        let options=brave_options(&state_dir,target,*wait_seconds,*close_tab);
                        let value=run_forvo_batch_logged(&open_store(false)?,batch_id,forvo::batch::Backend::Brave(&options),*max_items,output,None).await?;
                        Ok(with_update_notice(value,&state_dir,options.target.as_deref(),update_settings.as_ref().expect("настройки проверки обновления")).await)
                    }
                    command=>{
                        let fetch=matches!(command,AdapterCommand::Run(RunAction::Fetch{..}));
                        let value=run_brave(&state_dir,target.clone(),command,output,&open_store).await?;
                        Ok(if fetch {with_update_notice(value,&state_dir,target.as_deref(),update_settings.as_ref().expect("настройки проверки обновления")).await} else {value})
                    }
                }
            }
            ForvoAction::Config=>{
                let settings=generic.settings()?;
                let dir=adapter_dir()?;
                let adapter=match forvo::brave::targets::describe(&dir) {
                    Ok(value)=>value,
                    Err(e)=>serde_json::json!({"error":forvo::provider_code(&e),"message":e.message}),
                };
                Ok(serde_json::json!({"forvo":settings.describe(),"adapter_state_dir":dir,"adapter":adapter,
                    "update_check":forvo::brave::update::cached_check(&dir)}))
            }
            ForvoAction::BatchStatus{batch_id}=>Ok(batch_json(forvo::batch::status(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchCancel{batch_id}=>Ok(batch_json(forvo::batch::cancel(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchStart{batch_id,plan,max_items}=>{
                let settings=generic.settings()?;
                let plan=read_plan(plan,protected)?;
                let store=open_store(true)?;
                if forvo::batch::status(&store,batch_id).is_ok() {
                    return Err(AssetError::new(ErrorCode::IdentityConflict,"пакет существует; используйте batch-resume"));
                }
                routed_batch_start(&store,batch_id,plan,*max_items,profile,&adapter_dir()?,&settings,output).await
            }
            ForvoAction::BatchResume{batch_id,max_items}=>{
                let store=open_store(false)?;
                let batch=forvo::batch::status(&store,batch_id)?;
                let settings=generic.settings()?;
                // Пакет продолжается сохранённым транспортом и привязкой, а не текущим режимом.
                let transport=batch.transport();
                let decision=serde_json::json!({"selected":transport,"source":"batch_binding"});
                let value=async { match transport {
                    forvo::batch::Transport::CdpChromium=>{
                        if settings.mode==crate::user_config::ForvoMode::ExtensionOnly {
                            let mut e=forvo::route::cdp_disabled("batch-resume");
                            e.details["code"]=serde_json::json!("transport_disabled_by_mode");
                            e.details["batch_transport"]=serde_json::json!(transport);
                            e.details["next_action"]=serde_json::json!(format!("pronunciation-assets forvo --mode chromium_first batch-resume --batch-id {batch_id}"));
                            return Err(e);
                        }
                        run_forvo_batch_logged(&store,batch_id,forvo::batch::Backend::Cdp{profile},*max_items,output,None).await
                    }
                    forvo::batch::Transport::BraveExtension=>{
                        let dir=adapter_dir()?;
                        let bound_target=batch.browser.as_ref().map(|binding| binding.target.as_str())
                            .or(batch.target.as_deref());
                        // Конфиг задаёт цель для новых действий. Возобновление
                        // пакета сохраняет его собственную привязку; только
                        // явный CLI `--browser-target` может запросить другую.
                        let options=brave_options(&dir,generic.browser_target.clone(),45,false);
                        let value=run_forvo_batch_logged(&store,batch_id,forvo::batch::Backend::Brave(&options),*max_items,output,None).await?;
                        Ok(with_update_notice(value,&dir,bound_target,&settings).await)
                    }
                }}.await;
                match value {Ok(mut v)=>{v["transport_decision"]=decision;Ok(v)},Err(mut e)=>{e.details["transport_decision"]=decision;Err(e)}}
            }
            ForvoAction::Fetch{surface,reading,replace_expected_sha256}=>{
                let settings=generic.settings()?;
                let request=Pronunciation::new(surface,reading)?;
                routed_fetch(profile,&adapter_dir()?,&settings,request,replace_expected_sha256.clone(),output,&open_store).await
            }
            _=>{
                if generic.settings()?.mode==crate::user_config::ForvoMode::ExtensionOnly {
                    return Err(forvo::route::cdp_disabled(match action {ForvoAction::Login{..}=>"login",ForvoAction::Check=>"check",_=>"candidates"}));
                }
                // Проверяем входные пары до запуска браузера.
                let request=match action {ForvoAction::Candidates{surface,reading}=>Some(Pronunciation::new(surface,reading)?),_=>None};
                let mut session=if matches!(action,ForvoAction::Login{..}) { ForvoSession::launch_interactive(profile).await? } else { ForvoSession::launch(profile).await? };
                let result=tokio::select! {
                    result=async {
                      let mut retried = false;
                      loop {
                        let attempt: Result<serde_json::Value, AssetError> = async { match action {
                            ForvoAction::Login{timeout_seconds,surface}=>{
                                eprintln!("Войдите вручную в открытом окне Forvo; пароль не передаётся CLI.");
                                let surface=surface.as_deref().map(|s|Pronunciation::new(s,s).map(|r|r.surface)).transpose()?;
                                let state=session.check_at(true,std::time::Duration::from_secs(*timeout_seconds),surface.as_deref()).await?;
                                if state!=AuthState::Authenticated {return Err(forvo::auth_error(state));}
                                Ok(serde_json::json!({"auth_state":state}))
                            }
                            ForvoAction::Check=>{
                                let state=session.check(false,std::time::Duration::from_secs(12)).await?;
                                if state!=AuthState::Authenticated {return Err(forvo::auth_error(state));}
                                Ok(serde_json::json!({"auth_state":state}))
                            }
                            ForvoAction::Candidates{..}=>Ok(serde_json::json!({"ranking_policy":forvo::RANKING,"candidates":session.candidates(request.as_ref().expect("request")).await?})),
                            _=>unreachable!(),
                        }}.await;
                        match attempt {
                            Err(error) if !retried && !matches!(action, ForvoAction::Login{..}) && forvo::requires_interaction(&error) => {
                                if !session.recover_interactively(request.as_ref().map(|r|r.surface.as_str())).await? { break Err(error); }
                                retried = true;
                            }
                            other => break other,
                        }
                      }
                    }=>result,
                    _=tokio::signal::ctrl_c()=>Err(AssetError::new(ErrorCode::Interrupted,"Forvo прерван пользователем")),
                };
                let stopped = session.close().await;
                match (result, stopped) {
                    (Err(mut original), Err(cleanup)) => { original.details["browser_cleanup"] = serde_json::json!({"code":forvo::provider_code(&cleanup),"message":cleanup.message}); Err(original) },
                    (Err(original), _) => Err(original),
                    (_, Err(cleanup)) => Err(cleanup),
                    (Ok(value), Ok(())) => Ok(value),
                }
            }
        }
    })?;
    let paused = result.get("paused").is_some_and(|v| !v.is_null());
    let cancelled = matches!(
        action,
        ForvoAction::BatchStart { .. }
            | ForvoAction::BatchResume { .. }
            | ForvoAction::Brave {
                action: BraveAction::Run(
                    RunAction::BatchStart { .. } | RunAction::BatchResume { .. }
                ),
                ..
            }
            | ForvoAction::Browser {
                action: BrowserAction::Run(
                    RunAction::BatchStart { .. } | RunAction::BatchResume { .. }
                ),
                ..
            }
    ) && result["cancelled"] == true;
    Ok(
        serde_json::json!({"schema_version":1,"outcome":if cancelled {"cancelled"} else if paused {"paused"} else {"ok"},"result":result}),
    )
}

/// Отказ CDP-попытки одиночного fetch: ошибка, этап и завершена ли уборка Chromium.
struct CdpFailure {
    error: AssetError,
    stage: crate::forvo::route::Stage,
    cleanup_ok: bool,
}

/// Одиночный fetch через собственный Chromium: тот же порядок попыток,
/// `ForvoEvidence` и импорт, что и прежде; дополнительно сообщается этап отказа.
async fn cdp_fetch(
    profile: &Path,
    store: &AssetStore,
    request: &Pronunciation,
    replace: Option<String>,
) -> Result<serde_json::Value, CdpFailure> {
    use crate::forvo::{self, ForvoSession, route::Stage};
    let mut session = ForvoSession::launch(profile)
        .await
        .map_err(|error| CdpFailure {
            cleanup_ok: forvo::provider_code(&error) != "browser_cleanup_failed",
            error,
            stage: Stage::Launch,
        })?;
    let result = tokio::select! {
        result = async {
            let acquisition = forvo::acquire(&mut session, request).await?;
            let asset = forvo::import_acquisition(store, request, &acquisition, replace)?;
            Ok(serde_json::json!({"asset":asset,"acquisition":acquisition.evidence}))
        } => result,
        _ = tokio::signal::ctrl_c() => Err(AssetError::new(ErrorCode::Interrupted, "Forvo прерван пользователем")),
    };
    let stage = if session.download_started() {
        Stage::AfterDownload
    } else {
        Stage::BeforeDownload
    };
    let stopped = session.close().await;
    match (result, stopped) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(mut error), Err(cleanup)) => {
            error.details["browser_cleanup"] = serde_json::json!({"code":forvo::provider_code(&cleanup),"message":cleanup.message});
            Err(CdpFailure {
                error,
                stage,
                cleanup_ok: false,
            })
        }
        (Err(error), Ok(())) => Err(CdpFailure {
            error,
            stage,
            cleanup_ok: true,
        }),
        (Ok(_), Err(cleanup)) => Err(CdpFailure {
            error: cleanup,
            stage: Stage::AfterDownload,
            cleanup_ok: false,
        }),
    }
}

/// Одиночный fetch общего пути: транспорт по стратегии, переход только при
/// доказанном backend failure CDP и настроенном адаптере.
async fn routed_fetch(
    profile: &Path,
    adapter_dir: &Path,
    settings: &crate::user_config::ForvoSettings,
    request: Pronunciation,
    replace: Option<String>,
    output: Output,
    open_store: &dyn Fn(bool) -> Result<AssetStore, AssetError>,
) -> Result<serde_json::Value, AssetError> {
    use crate::forvo::route::{Decision, backend_failure};
    use crate::forvo::{brave, provider_code};
    use crate::user_config::ForvoMode;
    let mut decision = Decision::new(settings);
    let target = settings.browser_target.clone();
    if settings.mode == ForvoMode::ExtensionOnly {
        decision.selected = Some("brave_extension");
        return extension_fetch(
            adapter_dir,
            target,
            &request,
            replace,
            output,
            open_store,
            settings,
            decision,
        )
        .await;
    }
    let store = open_store(true)?;
    let failure = match cdp_fetch(profile, &store, &request, replace.clone()).await {
        Ok(mut value) => {
            decision.attempt("cdp_chromium", None, None);
            decision.selected = Some("cdp_chromium");
            value["transport_decision"] = decision.to_value();
            return Ok(value);
        }
        Err(failure) => failure,
    };
    let CdpFailure {
        mut error,
        stage,
        cleanup_ok,
    } = failure;
    decision.attempt("cdp_chromium", Some(stage), Some(&error));
    let reason = backend_failure(stage, &error, cleanup_ok);
    let adapter = brave::targets::load_target(adapter_dir, target.as_deref());
    match (reason, adapter) {
        (Ok(reason), Ok(state)) => {
            tracing::warn!(event = "forvo_transport_fallback", stage = "route", reason, code = provider_code(&error),
                target = %state.target, "CDP недоступен до побочных эффектов; используется адаптер браузера");
            decision.fallback = serde_json::json!({"used":true,"reason":reason,"failure_kind":"backend",
                "from":"cdp_chromium","failure_code":provider_code(&error)});
            decision.selected = Some("brave_extension");
            extension_fetch(
                adapter_dir,
                Some(state.target),
                &request,
                replace,
                output,
                open_store,
                settings,
                decision,
            )
            .await
        }
        (Ok(reason), Err(unavailable)) => {
            decision.fallback = serde_json::json!({"used":false,"allowed":true,"reason":reason,"failure_kind":"backend",
                "adapter_unavailable":provider_code(&unavailable),"next_action":unavailable.details["next_action"]});
            error.details["transport_decision"] = decision.to_value();
            Err(error)
        }
        (Err(refused), _) => {
            decision.fallback = serde_json::json!({"used":false,"allowed":false,"failure_kind":"domain_or_unsafe",
                "refused_because":refused});
            error.details["transport_decision"] = decision.to_value();
            Err(error)
        }
    }
}

/// Одиночный fetch адаптером браузера тем же production-путём, что `forvo browser fetch`.
// Сохранение общего CLI-пути требует request, выбранных настроек и накопленного решения маршрута.
#[allow(clippy::too_many_arguments)]
async fn extension_fetch(
    adapter_dir: &Path,
    target: Option<String>,
    request: &Pronunciation,
    replace: Option<String>,
    output: Output,
    open_store: &dyn Fn(bool) -> Result<AssetStore, AssetError>,
    settings: &crate::user_config::ForvoSettings,
    decision: crate::forvo::route::Decision,
) -> Result<serde_json::Value, AssetError> {
    let run = RunAction::Fetch {
        surface: request.surface.clone(),
        reading: request.reading.clone(),
        replace_expected_sha256: replace,
        wait_seconds: 45,
        close_tab: false,
    };
    let mut decision = decision;
    decision.attempt("brave_extension", None, None);
    let result = run_brave(
        adapter_dir,
        target.clone(),
        AdapterCommand::Run(&run),
        output,
        open_store,
    )
    .await;
    match result {
        Ok(mut value) => {
            value["transport_decision"] = decision.to_value();
            Ok(with_update_notice(value, adapter_dir, target.as_deref(), settings).await)
        }
        Err(mut error) => {
            if let Some(last) = decision.attempts.last_mut() {
                last["outcome"] = serde_json::json!("failed");
                last["code"] = serde_json::json!(crate::forvo::provider_code(&error));
            }
            error.details["transport_decision"] = decision.to_value();
            Err(error)
        }
    }
}

/// Новый пакет общего пути: транспорт и цель выбираются до создания пакета.
/// `chromium_first` запускает собственный Chromium до записи состояния; если
/// он не запустился (backend failure) и адаптер настроен, создаётся пакет
/// адаптера с целью, сохранённой до первого подключения. Иначе — прежнее
/// поведение: пакет CDP, который при отказе приостанавливается.
#[allow(clippy::too_many_arguments)]
async fn routed_batch_start(
    store: &AssetStore,
    id: &str,
    plan: crate::forvo::batch::FetchPlan,
    max_items: usize,
    profile: &Path,
    adapter_dir: &Path,
    settings: &crate::user_config::ForvoSettings,
    output: Output,
) -> Result<serde_json::Value, AssetError> {
    use crate::forvo::route::{Decision, Stage, backend_failure};
    use crate::forvo::{ForvoSession, batch, brave, provider_code};
    use crate::user_config::ForvoMode;
    let mut decision = Decision::new(settings);
    let target = settings.browser_target.clone();
    let with_decision =
        |result: Result<serde_json::Value, AssetError>, decision: &Decision| match result {
            Ok(mut value) => {
                value["transport_decision"] = decision.to_value();
                Ok(value)
            }
            Err(mut error) => {
                error.details["transport_decision"] = decision.to_value();
                Err(error)
            }
        };
    let start_extension = async |target: String, mut decision: Decision, plan: batch::FetchPlan| {
        decision.selected = Some("brave_extension");
        decision.attempt("brave_extension", None, None);
        let options = brave_options(adapter_dir, Some(target.clone()), 45, false);
        batch::start_with_browser_target(store, id, plan, target.clone())?;
        let result = run_forvo_batch_logged(
            store,
            id,
            batch::Backend::Brave(&options),
            max_items,
            output,
            None,
        )
        .await;
        let result = match result {
            Ok(value) => Ok(with_update_notice(value, adapter_dir, Some(&target), settings).await),
            Err(e) => Err(e),
        };
        with_decision(result, &decision)
    };
    if settings.mode == ForvoMode::ExtensionOnly {
        let state =
            brave::targets::load_target(adapter_dir, target.as_deref()).map_err(|mut e| {
                e.details["transport_decision"] = decision.to_value();
                e
            })?;
        return start_extension(state.target, decision, plan).await;
    }
    match ForvoSession::launch(profile).await {
        Ok(session) => {
            decision.attempt("cdp_chromium", None, None);
            decision.selected = Some("cdp_chromium");
            if let Err(e) = batch::start(store, id, plan, batch::Transport::CdpChromium) {
                return Err(batch_start_failure(e, session, decision.to_value()).await);
            }
            let result = run_forvo_batch_logged(
                store,
                id,
                batch::Backend::Cdp { profile },
                max_items,
                output,
                Some(session),
            )
            .await;
            with_decision(result, &decision)
        }
        Err(error) => {
            decision.attempt("cdp_chromium", Some(Stage::Launch), Some(&error));
            let cleanup_ok = provider_code(&error) != "browser_cleanup_failed";
            if !cleanup_ok || error.code == ErrorCode::Interrupted {
                decision.fallback = serde_json::json!({"used":false,"allowed":false,
                    "refused_because":if cleanup_ok {"interrupted"} else {"cleanup_incomplete"}});
                return with_decision(Err(error), &decision);
            }
            let reason = backend_failure(Stage::Launch, &error, cleanup_ok);
            match (
                reason,
                brave::targets::load_target(adapter_dir, target.as_deref()),
            ) {
                (Ok(reason), Ok(state)) => {
                    tracing::warn!(event = "forvo_transport_fallback", stage = "route", reason, code = provider_code(&error),
                        target = %state.target, "CDP не запустился до создания пакета; пакет создаётся для адаптера браузера");
                    decision.fallback = serde_json::json!({"used":true,"reason":reason,"failure_kind":"backend",
                        "from":"cdp_chromium","failure_code":provider_code(&error)});
                    start_extension(state.target, decision, plan).await
                }
                (reason, adapter) => {
                    decision.fallback = serde_json::json!({"used":false,"allowed":reason.is_ok(),
                        "refused_because":reason.err(),"adapter_unavailable":adapter.err().map(|e| provider_code(&e).to_owned())});
                    decision.selected = Some("cdp_chromium");
                    batch::start(store, id, plan, batch::Transport::CdpChromium)?;
                    let result = run_forvo_batch_logged(
                        store,
                        id,
                        batch::Backend::Cdp { profile },
                        max_items,
                        output,
                        None,
                    )
                    .await;
                    with_decision(result, &decision)
                }
            }
        }
    }
}

async fn batch_start_failure(
    mut error: AssetError,
    session: crate::forvo::ForvoSession,
    transport_decision: serde_json::Value,
) -> AssetError {
    if let Err(cleanup) = session.close().await {
        error.details["browser_cleanup"] = serde_json::json!({
            "code": crate::forvo::provider_code(&cleanup),
            "message": cleanup.message,
            "details": cleanup.details,
        });
    }
    error.details["transport_decision"] = transport_decision;
    error
}

/// Сведения о более новой версии адаптера по редкой кэшированной проверке.
/// Проверка не задерживает получение MP3 (выполняется после него), ограничена
/// по времени, не устанавливает обновление и отключается `forvo.update_check = "off"`.
async fn with_update_notice(
    mut value: serde_json::Value,
    state_dir: &Path,
    target: Option<&str>,
    settings: &crate::user_config::ForvoSettings,
) -> serde_json::Value {
    use crate::user_config::UpdateCheck;
    if settings.update_check != UpdateCheck::Weekly {
        return value;
    }
    let state = crate::forvo::brave::targets::load_target(state_dir, target).ok();
    if let Some(notice) = crate::forvo::brave::update::auto_check(
        state_dir,
        state.as_ref(),
        std::time::Duration::from_secs(7 * 24 * 3600),
        std::time::Duration::from_secs(3),
    )
    .await
    {
        value["adapter_update"] = notice;
    }
    value
}

fn read_plan(
    plan: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<crate::forvo::batch::FetchPlan, AssetError> {
    serde_json::from_slice(&read_local(plan, protected, 1024 * 1024)?).map_err(|_| {
        AssetError::new(
            ErrorCode::InvalidIdentity,
            "неверный строгий JSON-план Forvo",
        )
    })
}

fn brave_state_dir(state_dir: Option<&Path>) -> Result<PathBuf, AssetError> {
    match state_dir {
        Some(dir) => {
            std::path::absolute(dir).map_err(|e| AssetError::io("каталог состояния Brave", e))
        }
        None => crate::forvo::brave::default_state_dir(),
    }
}

fn brave_options(
    state_dir: &Path,
    target: Option<String>,
    wait_seconds: u64,
    close_tab: bool,
) -> crate::forvo::brave::fetch::BraveOptions {
    crate::forvo::brave::fetch::BraveOptions {
        state_dir: state_dir.to_owned(),
        target,
        binding: None,
        wait: std::time::Duration::from_secs(wait_seconds),
        limits: crate::forvo::brave::download::Limits::default(),
        close_tab,
    }
}

/// Состояние пакета и его фактический транспорт (у состояния версии 1 — CDP).
fn batch_json(batch: crate::forvo::batch::ForvoBatch) -> serde_json::Value {
    let transport = batch.transport();
    let mut value = serde_json::to_value(batch).expect("состояние Forvo сериализуется");
    value["effective_transport"] = serde_json::json!(transport);
    value
}

async fn run_forvo_batch_logged(
    store: &AssetStore,
    id: &str,
    backend: crate::forvo::batch::Backend<'_>,
    max_items: usize,
    output: Output,
    prepared: Option<crate::forvo::ForvoSession>,
) -> Result<serde_json::Value, AssetError> {
    use crate::batch_runtime::SafeBatchRuntime;
    use crate::diagnostics::{OutputMode, RunLogGuard};
    use tracing::instrument::WithSubscriber;

    let mut prepared = prepared;
    let log = match SafeBatchRuntime::open(store.root(), id)
        .and_then(|runtime| runtime.create_run_log())
    {
        Ok(log) => log,
        Err(mut error) => {
            if let Some(session) = prepared.take()
                && let Err(cleanup) = session.close().await
            {
                error.details["browser_cleanup"] = serde_json::json!({
                    "code": crate::forvo::provider_code(&cleanup),
                    "message": cleanup.message,
                    "details": cleanup.details,
                });
            }
            return Err(error);
        }
    };
    let path = log.path.display().to_string();
    let guard = RunLogGuard::new(
        log.file,
        match output {
            Output::Json => OutputMode::Json,
            Output::Text => OutputMode::Human,
        },
    );
    let result = async {
        tracing::info!(
            event = "run_started",
            stage = "run",
            code = "forvo_batch_started",
            "Начата обработка пакета Forvo"
        );
        let result =
            crate::forvo::batch::resume_prepared(store, id, backend, max_items, prepared).await;
        match &result {
            Ok(batch) => tracing::info!(
                event = "run_finished",
                stage = "run",
                paused = batch.paused.is_some(),
                cancelled = batch.cancelled,
                "Обработка пакета Forvo завершена"
            ),
            Err(error) => tracing::error!(
                event = "run_stopped",
                stage = "run",
                code = error.code.as_str(),
                "Обработка пакета Forvo остановлена"
            ),
        }
        result
    }
    .with_subscriber(guard.dispatch())
    .await;
    if let Err(logging) = guard.finish() {
        return Err(AssetError::with_details(
            ErrorCode::IoFailure,
            "Не удалось сохранить диагностический журнал Forvo",
            serde_json::json!({"diagnostic_log":path,"logging_error":crate::diagnostics::safe_message(&logging),"original_error":result.as_ref().err().map(|e| serde_json::json!({"code":e.code.as_str(),"message":crate::diagnostics::safe_message(&e.message)}))}),
        ));
    }
    let batch = result.map_err(|mut error| {
        error.details["diagnostic_log"] = serde_json::json!(path);
        error
    })?;
    let mut value = batch_json(batch);
    value["diagnostic_log"] = serde_json::json!(path);
    Ok(value)
}

async fn run_brave(
    state_dir: &Path,
    target: Option<String>,
    action: AdapterCommand<'_>,
    output: Output,
    open_store: &dyn Fn(bool) -> Result<AssetStore, AssetError>,
) -> Result<serde_json::Value, AssetError> {
    use crate::diagnostics::{OutputMode, RunLogGuard};
    use crate::forvo::brave::{self, acceptance};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;
    use tracing::instrument::WithSubscriber;

    let state_dir = state_dir.to_owned();
    let logs = state_dir.join("logs");
    brave::private_dir(&logs)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = logs.join(format!("{stamp}-{}.ndjson", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| AssetError::io("журнал адаптера браузера", e))?;
    let guard = RunLogGuard::new(
        file,
        match output {
            Output::Json => OutputMode::Json,
            Output::Text => OutputMode::Human,
        },
    );
    let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(false);
    let graceful_interrupt = waits_for_brave_download_cleanup(&action);
    let work = async {
        match action {
            AdapterCommand::Setup(options) => brave::setup(&options),
            AdapterCommand::Discover => {
                let wsl = brave::detect_platform() == brave::Platform::Wsl;
                let found =
                    brave::browsers::discover(&brave::browsers::DiscoveryRoots::system(wsl));
                let configured = brave::targets::describe(&state_dir)?;
                Ok(
                    serde_json::json!({"installations":found,"configured":configured,
                    "note":"разновидность подтверждена по исполняемому файлу; версия null — не установлена надёжно"}),
                )
            }
            AdapterCommand::Targets => brave::targets::describe(&state_dir),
            AdapterCommand::Select(name) => brave::targets::select(&state_dir, &name),
            AdapterCommand::Run(RunAction::Doctor {
                wait_seconds,
                forvo,
                reconnect,
                reload,
            }) => acceptance::report_error(
                acceptance::doctor(&acceptance::DoctorOptions {
                    state_dir: state_dir.clone(),
                    target: target.clone(),
                    wait: Duration::from_secs(*wait_seconds),
                    forvo: *forvo,
                    reconnect: *reconnect,
                    reload: *reload,
                })
                .await?,
            ),
            AdapterCommand::Run(RunAction::Smoke {
                surface,
                reading,
                wait_seconds,
                probe_downloads_api,
                close_tab,
                keep_download,
            }) => acceptance::report_error(
                acceptance::smoke(acceptance::SmokeOptions {
                    state_dir: state_dir.clone(),
                    target: target.clone(),
                    wait: Duration::from_secs(*wait_seconds),
                    surface: surface.clone(),
                    reading: reading.clone(),
                    probe_downloads_api: *probe_downloads_api,
                    close_tab: *close_tab,
                    keep_download: *keep_download,
                    limits: brave::download::Limits::default(),
                    interrupt: Some(interrupt_rx.clone()),
                })
                .await?,
            ),
            AdapterCommand::Run(RunAction::Close { wait_seconds }) => {
                acceptance::close(
                    &state_dir,
                    target.as_deref(),
                    Duration::from_secs(*wait_seconds),
                )
                .await
            }
            AdapterCommand::Run(RunAction::Fetch {
                surface,
                reading,
                replace_expected_sha256,
                wait_seconds,
                close_tab,
            }) => {
                // Пара проверяется до подключения к браузеру.
                let request = Pronunciation::new(surface, reading)?;
                let store = open_store(true)?;
                brave::fetch::fetch(
                    &store,
                    &request,
                    replace_expected_sha256.clone(),
                    &brave_options(&state_dir, target.clone(), *wait_seconds, *close_tab),
                    Some(interrupt_rx.clone()),
                )
                .await
            }
            AdapterCommand::Run(RunAction::UpdateCheck {
                from_dir,
                trusted_public_key,
            }) => {
                let trust = trust_root(trusted_public_key.as_deref())?;
                let state = brave::targets::load_target(&state_dir, target.as_deref()).ok();
                brave::update::check(
                    &state_dir,
                    state.as_ref(),
                    &update_source(from_dir.as_deref())?,
                    &trust,
                    &brave::update::Channel::default(),
                )
                .await
            }
            AdapterCommand::Run(RunAction::Update {
                from_dir,
                version,
                trusted_public_key,
                reinstall,
                wait_seconds,
            }) => {
                brave::update::update(&brave::update::UpdateOptions {
                    state_dir: state_dir.clone(),
                    target: target.clone(),
                    source: update_source(from_dir.as_deref())?,
                    version: version.clone(),
                    trust: trust_root(trusted_public_key.as_deref())?,
                    wait: Duration::from_secs(*wait_seconds),
                    reinstall: *reinstall,
                    channel: brave::update::Channel::default(),
                })
                .await
            }
            AdapterCommand::Run(RunAction::BatchStart { .. } | RunAction::BatchResume { .. }) => {
                unreachable!("пакет расширения исполняется общим forvo::batch")
            }
        }
    };
    // Ctrl+C: работа получает сигнал и освобождает собственную операцию загрузки;
    // повторный Ctrl+C или истечение отсрочки закрывают только соединение bridge.
    // Браузер, вкладки пользователя и настройки не трогаются.
    let work = work.with_subscriber(guard.dispatch());
    tokio::pin!(work);
    let result = tokio::select! {
        result = &mut work => result,
        _ = tokio::signal::ctrl_c(), if !graceful_interrupt => Err(adapter_interrupted(false)),
        _ = tokio::signal::ctrl_c(), if graceful_interrupt => {
            let _ = interrupt_tx.send(true);
            tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(20), &mut work) => match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(adapter_interrupted(false)),
                },
                _ = tokio::signal::ctrl_c() => Err(adapter_interrupted(true)),
            }
        }
    };
    finish_brave_run(result, guard.finish(), &path)
}

/// Корень доверия выпусков: встроенный или явный ключ пользователя.
fn trust_root(path: Option<&Path>) -> Result<crate::forvo::brave::release::TrustRoot, AssetError> {
    use crate::forvo::brave::release::{TrustRoot, TrustedKey};
    let Some(path) = path else {
        return Ok(TrustRoot::embedded());
    };
    let text = std::fs::read_to_string(path)
        .ok()
        .filter(|t| t.len() <= 1024)
        .ok_or_else(|| {
            crate::forvo::brave::bridge_error(
                "trusted_key_invalid",
                "Файл открытого ключа не читается",
            )
        })?;
    Ok(TrustRoot::Explicit(TrustedKey::from_public_key_hex(
        &text,
        Some(format!("--trusted-public-key {}", path.display())),
    )?))
}

fn update_source(
    from_dir: Option<&Path>,
) -> Result<crate::forvo::brave::update::Source, AssetError> {
    use crate::forvo::brave::update::Source;
    Ok(match from_dir {
        Some(dir) => Source::Directory(
            std::path::absolute(dir).map_err(|e| AssetError::io("каталог выпуска", e))?,
        ),
        None => Source::GitHub,
    })
}

/// Прерывание команды адаптера: общая ветка `forvo browser` обслуживает любой
/// поддерживаемый браузер, поэтому сообщение не называет Brave.
fn adapter_interrupted(repeated: bool) -> AssetError {
    AssetError::new(
        ErrorCode::Interrupted,
        if repeated {
            "Команда адаптера браузера прервана повторно; соединение с расширением закрыто"
        } else {
            "Команда адаптера браузера прервана; соединение с расширением закрыто, браузер не изменён"
        },
    )
}

fn waits_for_brave_download_cleanup(action: &AdapterCommand<'_>) -> bool {
    matches!(
        action,
        AdapterCommand::Run(RunAction::Smoke { .. } | RunAction::Fetch { .. })
    )
}

fn finish_brave_run(
    result: Result<serde_json::Value, AssetError>,
    logged: Result<(), String>,
    path: impl AsRef<Path>,
) -> Result<serde_json::Value, AssetError> {
    let path = path.as_ref().display().to_string();
    match (result, logged) {
        (Ok(mut value), Ok(())) => {
            value["diagnostic_log"] = serde_json::json!(path);
            Ok(value)
        }
        (Err(mut error), Ok(())) => {
            error.details["diagnostic_log"] = serde_json::json!(path);
            Err(error)
        }
        (Err(mut error), Err(logging)) => {
            error.details["diagnostic_log"] = serde_json::json!(path);
            error.details["logging_error"] =
                serde_json::json!(crate::diagnostics::safe_message(&logging));
            Err(error)
        }
        (Ok(_), Err(logging)) => Err(AssetError::with_details(
            ErrorCode::IoFailure,
            "Не удалось сохранить диагностический журнал адаптера браузера",
            serde_json::json!({
                "diagnostic_log":path,
                "logging_error":crate::diagnostics::safe_message(&logging),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterCommand, RunAction, adapter_interrupted, batch_start_failure, finish_brave_run,
        run_forvo_batch_logged, waits_for_brave_download_cleanup, with_update_notice,
    };
    use crate::forvo::{ForvoSession, batch::Backend};
    use crate::temp_workspace::TempWorkspace;
    use crate::{AssetError, AssetStore, ErrorCode, StoreOptions};
    use serde_json::json;

    #[test]
    fn brave_operation_error_survives_diagnostic_log_failure() {
        let result = Err(AssetError::with_details(
            ErrorCode::Interrupted,
            "операция прервана",
            json!({"stage":"download"}),
        ));
        let error =
            finish_brave_run(result, Err("disk full".into()), "/logs/run.ndjson").unwrap_err();
        assert_eq!(error.code, ErrorCode::Interrupted);
        assert_eq!(error.message, "операция прервана");
        assert_eq!(error.details["stage"], "download");
        assert_eq!(error.details["diagnostic_log"], "/logs/run.ndjson");
        assert!(
            error.details["logging_error"]
                .as_str()
                .unwrap()
                .contains("disk full")
        );
    }

    #[test]
    fn brave_success_with_diagnostic_log_failure_reports_log_details() {
        let error = finish_brave_run(
            Ok(json!({"status":"ok"})),
            Err("disk full".into()),
            "/logs/run.ndjson",
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::IoFailure);
        assert!(!error.message.contains("Brave"), "{}", error.message);
        assert_eq!(error.details["diagnostic_log"], "/logs/run.ndjson");
        assert!(
            error.details["logging_error"]
                .as_str()
                .unwrap()
                .contains("disk full")
        );
    }

    #[tokio::test]
    async fn failed_batch_diagnostic_setup_closes_prepared_session() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let workspace = TempWorkspace::create("forvo-diagnostic-setup").unwrap();
        let store = AssetStore::open_with_policy(
            StoreOptions::new(workspace.path().join("store")),
            crate::pronunciation::PronunciationDomainPolicy,
        )
        .unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let marker = stopped.clone();
        let pending_stop = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            marker.store(true, Ordering::SeqCst);
            Ok(())
        });
        let profile = workspace.path().join("profile");

        let error = run_forvo_batch_logged(
            &store,
            "bad/id",
            Backend::Cdp { profile: &profile },
            1,
            super::Output::Json,
            Some(ForvoSession::with_pending_stop_for_test(pending_stop)),
        )
        .await
        .unwrap_err();

        assert!(error.details["browser_cleanup"].is_null());
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn batch_start_failure_keeps_cleanup_and_transport_details() {
        let session = ForvoSession::with_pending_stop_for_test(tokio::spawn(async {
            Err("stop failed".into())
        }));
        let error = batch_start_failure(
            AssetError::with_details(
                ErrorCode::IoFailure,
                "не удалось создать пакет",
                json!({"source":"batch_start"}),
            ),
            session,
            json!({"selected":"cdp_chromium"}),
        )
        .await;

        assert_eq!(error.code, ErrorCode::IoFailure);
        assert_eq!(error.details["source"], "batch_start");
        assert_eq!(
            error.details["transport_decision"]["selected"],
            "cdp_chromium"
        );
        assert_eq!(
            error.details["browser_cleanup"]["code"],
            "browser_cleanup_failed"
        );
        assert!(error.details["browser_cleanup"]["message"].is_string());
    }

    #[test]
    fn adapter_interruption_does_not_name_a_specific_browser() {
        for repeated in [false, true] {
            let error = adapter_interrupted(repeated);
            assert_eq!(error.code, ErrorCode::Interrupted);
            assert!(!error.message.contains("Brave"), "{}", error.message);
        }
    }

    #[test]
    fn download_actions_wait_for_cleanup_after_ctrl_c() {
        let smoke = RunAction::Smoke {
            surface: "元気".into(),
            reading: "げんき".into(),
            wait_seconds: 45,
            probe_downloads_api: false,
            close_tab: false,
            keep_download: false,
        };
        let doctor = RunAction::Doctor {
            wait_seconds: 45,
            forvo: false,
            reconnect: false,
            reload: false,
        };
        let close = RunAction::Close { wait_seconds: 45 };
        let fetch = RunAction::Fetch {
            surface: "元気".into(),
            reading: "げんき".into(),
            replace_expected_sha256: None,
            wait_seconds: 45,
            close_tab: false,
        };

        assert!(waits_for_brave_download_cleanup(&AdapterCommand::Run(
            &smoke
        )));
        assert!(waits_for_brave_download_cleanup(&AdapterCommand::Run(
            &fetch
        )));
        assert!(!waits_for_brave_download_cleanup(&AdapterCommand::Run(
            &doctor
        )));
        assert!(!waits_for_brave_download_cleanup(&AdapterCommand::Run(
            &close
        )));
        assert!(!waits_for_brave_download_cleanup(&AdapterCommand::Discover));
        assert!(!waits_for_brave_download_cleanup(&AdapterCommand::Targets));
    }

    #[tokio::test]
    async fn update_notice_off_skips_network_and_adapter_state_access() {
        let workspace = TempWorkspace::create("forvo-update-notice-off").unwrap();
        let config = workspace.path().join("config.toml");
        std::fs::write(
            &config,
            "schema_version = 1\n[forvo]\nupdate_check = \"off\"\n",
        )
        .unwrap();
        let settings = crate::user_config::forvo(&crate::user_config::ForvoOverrides {
            config: Some(config),
            ..crate::user_config::ForvoOverrides::default()
        })
        .unwrap();
        let adapter_state = workspace.path().join("adapter-state");
        let result = json!({"status":"completed"});

        let output = with_update_notice(result.clone(), &adapter_state, None, &settings).await;

        assert_eq!(output, result);
        assert!(!adapter_state.exists());
    }
}
