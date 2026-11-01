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
    /// Forvo в собственном постоянном профиле Chromium; окно требуется для входа.
    Forvo {
        #[arg(long)]
        profile: Option<PathBuf>,
        #[command(subcommand)]
        action: ForvoAction,
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

#[derive(Subcommand)]
enum ForvoAction {
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
    /// Создать пакет расширения и начать его выполнение; пакет закрепляется
    /// за первой подключённой браузерной сессией.
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
fn read_local(
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
    if let Action::Forvo { profile, action } = &cli.command {
        let profile = std::path::absolute(
            profile
                .clone()
                .unwrap_or_else(|| repository.join(".codex/local/forvo-profile")),
        )
        .map_err(|e| AssetError::io("путь профиля", e))?;
        if protected.iter().any(|p| profile.starts_with(p)) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "профиль Forvo не может находиться внутри decks/**",
            ));
        }
        return execute_forvo(&root, &profile, action, &protected, cli.output);
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
    let runtime = tokio::runtime::Runtime::new().map_err(|e| AssetError::io("runtime", e))?;
    let result=runtime.block_on(async {
        match action {
            ForvoAction::Brave{..}|ForvoAction::Browser{..}=>{
                let (state_dir,target,command)=adapter_command(action)?.expect("команда адаптера");
                match command {
                    AdapterCommand::Run(RunAction::BatchStart{batch_id,plan,max_items,wait_seconds,close_tab})=>{
                        let options=brave_options(&state_dir,target,*wait_seconds,*close_tab);
                        let plan=read_plan(plan,protected)?;
                        let store=open_store(true)?;
                        forvo::batch::start(&store,batch_id,plan,forvo::batch::Transport::BraveExtension)?;
                        run_forvo_batch_logged(&store,batch_id,forvo::batch::Backend::Brave(&options),*max_items,output).await
                    }
                    AdapterCommand::Run(RunAction::BatchResume{batch_id,max_items,wait_seconds,close_tab})=>{
                        let options=brave_options(&state_dir,target,*wait_seconds,*close_tab);
                        run_forvo_batch_logged(&open_store(false)?,batch_id,forvo::batch::Backend::Brave(&options),*max_items,output).await
                    }
                    command=>run_brave(&state_dir,target,command,output,&open_store).await,
                }
            }
            ForvoAction::BatchStatus{batch_id}=>Ok(batch_json(forvo::batch::status(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchCancel{batch_id}=>Ok(batch_json(forvo::batch::cancel(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchStart{batch_id,plan,max_items}=>{
                let plan=read_plan(plan,protected)?;
                let store=open_store(true)?;
                forvo::batch::start(&store,batch_id,plan,forvo::batch::Transport::CdpChromium)?;
                run_forvo_batch_logged(&store,batch_id,forvo::batch::Backend::Cdp{profile},*max_items,output).await
            }
            ForvoAction::BatchResume{batch_id,max_items}=>run_forvo_batch_logged(&open_store(false)?,batch_id,forvo::batch::Backend::Cdp{profile},*max_items,output).await,
            _=>{
                // Проверяем входные пары до запуска браузера.
                let request=match action {ForvoAction::Candidates{surface,reading}|ForvoAction::Fetch{surface,reading,..}=>Some(Pronunciation::new(surface,reading)?),_=>None};
                let store=if matches!(action,ForvoAction::Fetch{..}) {Some(open_store(true)?)} else {None};
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
                            ForvoAction::Fetch{replace_expected_sha256,..}=>{
                                let request=request.as_ref().expect("request");
                                let acquisition=forvo::acquire(&mut session,request).await?;
                                let asset=forvo::import_acquisition(store.as_ref().expect("store"),request,&acquisition,replace_expected_sha256.clone())?;
                                Ok(serde_json::json!({"asset":asset,"acquisition":acquisition.evidence}))
                            }
                            _=>unreachable!(),
                        }}.await;
                        match attempt {
                            Err(error) if !retried && !matches!(action, ForvoAction::Login{..} | ForvoAction::Fetch{..}) && forvo::requires_interaction(&error) => {
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
) -> Result<serde_json::Value, AssetError> {
    use crate::batch_runtime::SafeBatchRuntime;
    use crate::diagnostics::{OutputMode, RunLogGuard};
    use tracing::instrument::WithSubscriber;

    let log = SafeBatchRuntime::open(store.root(), id)?.create_run_log()?;
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
        let result = crate::forvo::batch::resume(store, id, backend, max_items).await;
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
        .map_err(|e| AssetError::io("журнал адаптера Brave", e))?;
    let guard = RunLogGuard::new(
        file,
        match output {
            Output::Json => OutputMode::Json,
            Output::Text => OutputMode::Human,
        },
    );
    let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(false);
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
            AdapterCommand::Run(RunAction::BatchStart { .. } | RunAction::BatchResume { .. }) => {
                unreachable!("пакет расширения исполняется общим forvo::batch")
            }
        }
    };
    // Ctrl+C: работа получает сигнал и освобождает собственную операцию загрузки;
    // повторный Ctrl+C или истечение отсрочки закрывают только соединение bridge.
    // Brave, вкладки пользователя и настройки не трогаются.
    let work = work.with_subscriber(guard.dispatch());
    tokio::pin!(work);
    let result = tokio::select! {
        result = &mut work => result,
        _ = tokio::signal::ctrl_c() => {
            let _ = interrupt_tx.send(true);
            tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(20), &mut work) => match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(AssetError::new(ErrorCode::Interrupted, "Проверка Brave прервана; соединение с расширением закрыто, браузер не изменён")),
                },
                _ = tokio::signal::ctrl_c() => Err(AssetError::new(ErrorCode::Interrupted, "Проверка Brave прервана повторно; соединение с расширением закрыто")),
            }
        }
    };
    let logged = guard.finish();
    let mut value = result.map_err(|mut error| {
        error.details["diagnostic_log"] = serde_json::json!(path);
        error
    })?;
    if logged.is_err() {
        return Err(AssetError::new(
            ErrorCode::IoFailure,
            "Не удалось сохранить диагностический журнал адаптера Brave",
        ));
    }
    value["diagnostic_log"] = serde_json::json!(path);
    Ok(value)
}
