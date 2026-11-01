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
    if let Action::Forvo { profile, action } = &cli.command {
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
            ForvoAction::BatchStatus{batch_id}=>Ok(serde_json::json!(forvo::batch::status(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchCancel{batch_id}=>Ok(serde_json::json!(forvo::batch::cancel(&open_store(false)?,batch_id)?)),
            ForvoAction::BatchStart{batch_id,plan,max_items}=>{
                let plan=serde_json::from_slice(&read_local(plan,protected,1024*1024)?).map_err(|_|AssetError::new(ErrorCode::InvalidIdentity,"неверный строгий JSON-план Forvo"))?;
                let store=open_store(true)?;
                forvo::batch::start(&store,batch_id,plan)?;
                run_forvo_batch_logged(&store,batch_id,profile,*max_items,output).await
            }
            ForvoAction::BatchResume{batch_id,max_items}=>run_forvo_batch_logged(&open_store(false)?,batch_id,profile,*max_items,output).await,
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
        ForvoAction::BatchStart { .. } | ForvoAction::BatchResume { .. }
    ) && result["cancelled"] == true;
    Ok(
        serde_json::json!({"schema_version":1,"outcome":if cancelled {"cancelled"} else if paused {"paused"} else {"ok"},"result":result}),
    )
}

async fn run_forvo_batch_logged(
    store: &AssetStore,
    id: &str,
    profile: &Path,
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
        let result = crate::forvo::batch::resume(store, id, profile, max_items).await;
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
    let mut value = serde_json::to_value(batch).expect("состояние Forvo сериализуется");
    value["diagnostic_log"] = serde_json::json!(path);
    Ok(value)
}
