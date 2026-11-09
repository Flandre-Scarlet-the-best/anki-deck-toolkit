//! CLI `pronunciation-assets selection`: приём аудио без чтения, связывание
//! с распознаванием, пакетный выбор, адресные решения, локальная
//! аудиоэкспертиза, офлайн-обзор, явная публикация и чтение по политике.
//!
//! CLI не открывает браузер, не ждёт ввода и не открывает `decks/**`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};

use crate::pronunciation::selection::{self, review, *};
use crate::pronunciation::{MAX_AUDIO_BYTES, Pronunciation};
use crate::{AssetError, AssetStore, ErrorCode};

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AuthorityArg {
    /// Агент по тексту ASR и метрикам (запись не слушал).
    AgentEvidence,
    /// Агент по поддерживающей локальной аудиоэкспертизе (`--audio-review-id`).
    AgentAudio,
    /// Явный ответ пользователя, переданный агентом.
    UserViaAgent,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum PolicyArg {
    HumanDecision,
    HumanOrAudioModel,
    AgentSelected,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ReviewModeArg {
    /// Агент сам принимает допустимые решения в пределах --agent-trust.
    Delegated,
    /// Пользователь сам слушает и решает через review-export.
    Manual,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AgentPolicyArg {
    #[value(name = "agent-evidence-v1")]
    AgentEvidenceV1,
}

#[derive(Subcommand)]
pub(crate) enum SelectionAction {
    /// Принимает MP3 без чтения и без колоды (локальные файлы или Forvo-пакет).
    Intake {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        file: Vec<PathBuf>,
        /// Каталог: все обычные файлы `*.mp3` первого уровня.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Известное написание (подсказка, не чтение).
        #[arg(long)]
        surface: Option<String>,
        /// Исходники завершённых получений Forvo-пакета этого хранилища.
        #[arg(long)]
        from_forvo_batch: Option<String>,
    },
    /// Связывает исходники с готовыми результатами пакета распознавания.
    LinkRecognizer {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        recognizer_batch: String,
        /// Принять отсутствующие исходники из входа пакета распознавания.
        #[arg(long)]
        intake_missing: bool,
    },
    /// Явная связь исходника с целью без распознавания.
    LinkTarget {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        source_sha256: String,
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long = "competing-reading")]
        competing: Vec<String>,
        /// Индекс моры чтения, которая является грамматической частицей.
        #[arg(long = "particle-mora")]
        particle_morae: Vec<usize>,
    },
    /// Пакет свидетельств для агента: сводка, классы и отфильтрованные цели.
    Status {
        #[arg(long)]
        batch_id: String,
        /// all, undecided, unresolved или класс (needs-user-confirmation и т. п.).
        #[arg(long, default_value = "undecided")]
        filter: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Принимает все бесспорные решения; спорные остаются без остановки пакета.
    AutoSelect {
        #[arg(long)]
        batch_id: String,
        /// Явная политика агентского выбора; без неё — только детерминированный.
        #[arg(long, value_enum, requires = "agent_id")]
        agent_policy: Option<AgentPolicyArg>,
        #[arg(long, requires = "agent_policy")]
        agent_id: Option<String>,
    },
    /// Адресное решение по одной цели и одному точному кандидату.
    Decide {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long, required_unless_present = "defer")]
        candidate_id: Option<String>,
        /// SHA байтов кандидата (для интервала — SHA исходника).
        #[arg(long, required_unless_present = "defer")]
        sha256: Option<String>,
        #[arg(long, conflicts_with = "defer")]
        reject: bool,
        #[arg(long)]
        defer: bool,
        #[arg(long, value_enum)]
        authority: AuthorityArg,
        #[arg(long)]
        agent_id: String,
        #[arg(long)]
        audio_review_id: Option<String>,
        /// Канал ответа пользователя (например, `agent-dialog`).
        #[arg(long)]
        channel: Option<String>,
        /// Точная реплика пользователя: сохраняется только её SHA-256.
        #[arg(long)]
        statement: Option<String>,
        /// Пользователь сам сказал, что прослушал запись.
        #[arg(long)]
        user_listened: bool,
        #[arg(long)]
        reason: String,
        /// Отпечаток группы из status: защита от решения по устаревшим данным.
        #[arg(long)]
        expected_fingerprint: Option<String>,
    },
    /// Локальная аудиоэкспертиза кандидата: реальные байты → обе модели CTC.
    AudioReview {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        candidate_id: String,
        #[arg(long = "competing-reading")]
        competing: Vec<String>,
        #[command(flatten)]
        model: crate::pronunciation_recognizer_cli::ModelArgs,
    },
    /// Снимок для адресной проверки: snapshot.json, index.html, decisions.tsv.
    ReviewExport {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value = "unresolved")]
        filter: String,
        #[arg(long, default_value_t = 0)]
        page: usize,
        #[arg(long, default_value_t = 50)]
        page_size: usize,
    },
    /// Применяет первую колонку decisions.tsv снимка атомарно.
    ReviewApply {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        review_id: String,
        /// Файл решений, сохранённый страницей обзора (`decisions-<id>.tsv`);
        /// без него читается `decisions.tsv` каталога снимка.
        #[arg(long)]
        decisions_file: Option<PathBuf>,
    },
    /// Фиксирует выбранный пользователем режим проверки пакета.
    ReviewMode {
        #[arg(long)]
        batch_id: String,
        #[arg(long, value_enum)]
        mode: ReviewModeArg,
        /// Агент, которому делегированы решения (только delegated).
        #[arg(long)]
        agent_id: Option<String>,
        /// Верхняя граница полномочий агента (только delegated).
        #[arg(long, value_enum)]
        agent_trust: Option<PolicyArg>,
        #[arg(long, default_value = "agent-dialog")]
        channel: String,
        /// Точная реплика пользователя с выбором режима (хранится SHA-256).
        #[arg(long)]
        statement: String,
    },
    /// Материализует выбранные интервалы в самостоятельные проверенные MP3.
    Materialize {
        #[arg(long)]
        batch_id: String,
        #[arg(long, requires = "reading")]
        surface: Option<String>,
        #[arg(long, requires = "surface")]
        reading: Option<String>,
    },
    /// Явная публикация выбранного готового MP3 в канонический pronunciation_audio.
    Promote {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        surface: String,
        #[arg(long)]
        reading: String,
        #[arg(long)]
        replace_expected_sha256: Option<String>,
        /// Создать HumanAttestation — только для решения пользователя,
        /// заявившего прослушивание именно публикуемых байтов.
        #[arg(long)]
        attest_heard_decision: bool,
        /// Политика полномочий решения; по умолчанию — прежнее поведение
        /// (любое действующее решение). Запись не становится `Verified`.
        #[arg(long, value_enum, default_value = "agent-selected")]
        policy: PolicyArg,
    },
    /// Проверяет выдачу выбранных кандидатов по явной политике потребителя.
    Read {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        requests: PathBuf,
        #[arg(long, value_enum)]
        policy: PolicyArg,
    },
    /// Явно записывает выбранные байты в каталог потребителя.
    Export {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        requests: PathBuf,
        #[arg(long, value_enum)]
        policy: PolicyArg,
        /// Каталог вывода: запись сюда и есть разрешение потребить байты.
        #[arg(long)]
        out_dir: PathBuf,
    },
}

fn input_error(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

pub(crate) fn needs_store_creation(action: &SelectionAction) -> bool {
    matches!(action, SelectionAction::Intake { .. })
}

pub(crate) fn execute(
    action: &SelectionAction,
    store: &AssetStore,
    protected: &BTreeSet<PathBuf>,
) -> Result<serde_json::Value, AssetError> {
    let read = |path: &Path| crate::pronunciation_cli::read_local(path, protected, MAX_AUDIO_BYTES);
    Ok(match action {
        SelectionAction::Intake {
            batch_id,
            file,
            dir,
            surface,
            from_forvo_batch,
        } => {
            if let Some(forvo) = from_forvo_batch {
                if !file.is_empty() || dir.is_some() || surface.is_some() {
                    return Err(input_error(
                        "--from-forvo-batch не сочетается с --file/--dir/--surface",
                    ));
                }
                serde_json::json!(selection::intake_forvo_batch(store, batch_id, forvo)?)
            } else {
                let mut paths = file.clone();
                if let Some(dir) = dir {
                    crate::pronunciation::normalizer_batch::ensure_outside(
                        dir,
                        &crate::pronunciation::normalizer_batch::protected_roots(protected),
                        "аудио не читается из decks/**",
                    )?;
                    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
                        .map_err(|e| AssetError::io("каталог исходников", e))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| AssetError::io("чтение каталога исходников", e))?
                        .into_iter()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
                        })
                        .collect();
                    found.sort();
                    paths.extend(found);
                }
                if paths.is_empty() {
                    return Err(input_error("нужны --file, --dir или --from-forvo-batch"));
                }
                // Файлы читаются по одному перед своей обработкой: набор не
                // требует держать байты всех MP3 в памяти одновременно.
                let mut reader = |path: &Path| read(path);
                serde_json::json!(selection::intake_paths(
                    store,
                    batch_id,
                    &paths,
                    &mut reader,
                    surface.as_deref()
                )?)
            }
        }
        SelectionAction::LinkRecognizer {
            batch_id,
            workspace,
            recognizer_batch,
            intake_missing,
        } => {
            crate::pronunciation::normalizer_batch::ensure_outside(
                workspace,
                &crate::pronunciation::normalizer_batch::protected_roots(protected),
                "рабочий каталог распознавания не может лежать в decks/**",
            )?;
            let mut reader = |path: &Path| read(path);
            serde_json::json!(selection::link_recognizer(
                store,
                batch_id,
                workspace,
                recognizer_batch,
                intake_missing.then_some(&mut reader as &mut dyn FnMut(&Path) -> _),
            )?)
        }
        SelectionAction::LinkTarget {
            batch_id,
            source_sha256,
            surface,
            reading,
            competing,
            particle_morae,
        } => serde_json::json!({"link": selection::link_target(
            store,
            batch_id,
            source_sha256,
            surface,
            reading,
            competing.clone(),
            particle_morae.clone(),
        )?}),
        SelectionAction::Status {
            batch_id,
            filter,
            limit,
        } => selection::status_value(store, batch_id, GroupFilter::parse(filter)?, *limit)?,
        SelectionAction::AutoSelect {
            batch_id,
            agent_policy,
            agent_id,
        } => {
            let policy = match (agent_policy, agent_id) {
                (Some(AgentPolicyArg::AgentEvidenceV1), Some(agent)) => Some(AgentPolicy {
                    agent: agent.clone(),
                }),
                _ => None,
            };
            serde_json::json!(selection::auto_select(store, batch_id, policy.as_ref())?)
        }
        SelectionAction::Decide {
            batch_id,
            surface,
            reading,
            candidate_id,
            sha256,
            reject,
            defer,
            authority,
            agent_id,
            audio_review_id,
            channel,
            statement,
            user_listened,
            reason,
            expected_fingerprint,
        } => {
            if !matches!(authority, AuthorityArg::UserViaAgent)
                && (*user_listened || statement.is_some() || channel.is_some())
            {
                return Err(input_error(
                    "--user-listened, --statement и --channel относятся только к ответу пользователя",
                ));
            }
            if !matches!(authority, AuthorityArg::AgentAudio) && audio_review_id.is_some() {
                return Err(input_error(
                    "--audio-review-id относится только к полномочию agent-audio",
                ));
            }
            let authority = match authority {
                AuthorityArg::AgentEvidence => DecisionAuthority::AgentEvidence {
                    agent: agent_id.clone(),
                    policy: AGENT_EVIDENCE_POLICY.into(),
                },
                AuthorityArg::AgentAudio => DecisionAuthority::AgentAudioModel {
                    agent: agent_id.clone(),
                    audio_review_id: audio_review_id
                        .clone()
                        .ok_or_else(|| input_error("agent-audio требует --audio-review-id"))?,
                    backend: selection::ops::LOCAL_AUDIO_BACKEND.into(),
                },
                AuthorityArg::UserViaAgent => {
                    let statement = statement.as_deref().ok_or_else(|| {
                        input_error(
                            "user-via-agent требует --statement с точной репликой пользователя",
                        )
                    })?;
                    if statement.trim().is_empty() {
                        return Err(input_error("реплика пользователя пуста"));
                    }
                    DecisionAuthority::UserViaAgent {
                        agent: agent_id.clone(),
                        channel: channel
                            .clone()
                            .ok_or_else(|| input_error("user-via-agent требует --channel"))?,
                        statement_sha256: crate::hashing::sha256_hex(statement.as_bytes()),
                        user_listened: *user_listened,
                    }
                }
            };
            let action = if *defer {
                DecideAction::Defer
            } else {
                let candidate_id = candidate_id.clone().expect("clap требует --candidate-id");
                let expected_sha256 = sha256.clone().expect("clap требует --sha256");
                if *reject {
                    DecideAction::Reject {
                        candidate_id,
                        expected_sha256,
                    }
                } else {
                    DecideAction::Select {
                        candidate_id,
                        expected_sha256,
                    }
                }
            };
            serde_json::json!(selection::decide(
                store,
                batch_id,
                DecideRequest {
                    surface: surface.clone(),
                    reading: reading.clone(),
                    action,
                    authority,
                    reason: reason.clone(),
                    expected_fingerprint: expected_fingerprint.clone(),
                },
            )?)
        }
        SelectionAction::AudioReview {
            batch_id,
            candidate_id,
            competing,
            model,
        } => {
            let setup = crate::pronunciation_recognizer_cli::setup(model, protected)?;
            serde_json::json!({"audio_review": selection::ops::audio_review_local(
                store,
                batch_id,
                candidate_id,
                competing,
                &setup,
            )?})
        }
        SelectionAction::ReviewExport {
            batch_id,
            filter,
            page,
            page_size,
        } => serde_json::json!(review::export(
            store,
            batch_id,
            GroupFilter::parse(filter)?,
            *page,
            *page_size
        )?),
        SelectionAction::ReviewApply {
            batch_id,
            review_id,
            decisions_file,
        } => {
            let text = decisions_file
                .as_ref()
                .map(|path| {
                    let bytes =
                        crate::pronunciation_cli::read_local(path, protected, 4 * 1024 * 1024)?;
                    String::from_utf8(bytes)
                        .map_err(|_| input_error("файл решений должен быть UTF-8"))
                })
                .transpose()?;
            serde_json::json!(review::apply_with(
                store,
                batch_id,
                review_id,
                text.as_deref()
            )?)
        }
        SelectionAction::ReviewMode {
            batch_id,
            mode,
            agent_id,
            agent_trust,
            channel,
            statement,
        } => serde_json::json!(selection::set_review_mode(
            store,
            batch_id,
            ReviewModeRequest {
                mode: match mode {
                    ReviewModeArg::Delegated => ReviewMode::DelegatedAgent,
                    ReviewModeArg::Manual => ReviewMode::ManualUser,
                },
                agent: agent_id.clone(),
                agent_trust: agent_trust.map(trust_policy),
                channel: channel.clone(),
                statement: statement.clone(),
            },
        )?),
        SelectionAction::Materialize {
            batch_id,
            surface,
            reading,
        } => serde_json::json!(selection::materialize::materialize(
            store,
            batch_id,
            surface.as_deref().zip(reading.as_deref()),
        )?),
        SelectionAction::Promote {
            batch_id,
            surface,
            reading,
            replace_expected_sha256,
            attest_heard_decision,
            policy,
        } => serde_json::json!(selection::promote(
            store,
            batch_id,
            surface,
            reading,
            replace_expected_sha256.clone(),
            *attest_heard_decision,
            trust_policy(*policy),
        )?),
        SelectionAction::Read {
            batch_id,
            requests,
            policy,
        } => {
            let items = read_requests(requests, protected)?;
            let policy = trust_policy(*policy);
            // Метаданные просмотра не требуют удерживать байты набора в памяти.
            serde_json::json!({"assets": selection::read_selected_info(store.root(), batch_id, &items, policy)?})
        }
        SelectionAction::Export {
            batch_id,
            requests,
            policy,
            out_dir,
        } => {
            let items = read_requests(requests, protected)?;
            let policy = trust_policy(*policy);
            let directory = crate::pronunciation::normalizer_batch::ensure_outside(
                out_dir,
                &crate::pronunciation::normalizer_batch::protected_roots(protected),
                "выбранные аудио не записываются в decks/**",
            )?;
            serde_json::json!({"exported": selection::export_selected(
                store.root(),
                batch_id,
                &items,
                policy,
                &directory,
            )?})
        }
    })
}

/// Файл запросов потребителя: явный список точных пар, не «всё подряд».
fn read_requests(
    path: &Path,
    protected: &BTreeSet<PathBuf>,
) -> Result<Vec<Pronunciation>, AssetError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Requests {
        schema_version: u32,
        items: Vec<Pronunciation>,
    }
    let parsed: Requests = serde_json::from_slice(&crate::pronunciation_cli::read_local(
        path,
        protected,
        1024 * 1024,
    )?)
    .map_err(|e| input_error(e.to_string()))?;
    if parsed.schema_version != 1 || parsed.items.len() > MAX_SOURCES {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "неподдерживаемая схема/размер выбора",
        ));
    }
    Ok(parsed.items)
}

fn trust_policy(policy: PolicyArg) -> ConsumerTrustPolicy {
    match policy {
        PolicyArg::HumanDecision => ConsumerTrustPolicy::HumanDecision,
        PolicyArg::HumanOrAudioModel => ConsumerTrustPolicy::HumanOrAudioModel,
        PolicyArg::AgentSelected => ConsumerTrustPolicy::AgentSelected,
    }
}
