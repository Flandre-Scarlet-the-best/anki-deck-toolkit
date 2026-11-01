//! Production-путь Brave через настоящий CLI `pronunciation-assets`: одиночный
//! `forvo brave fetch` и пакеты `forvo brave batch-*` поверх общего
//! `SafeBatchRuntime`. Каждый сценарий использует собственное тестовое
//! хранилище в каталоге прогона и проверяет его файлы, SHA-256, `ForvoEvidence`,
//! журнал запросов fixture (число сетевых загрузок) и каталог загрузок браузера.

use super::chrome::{TestBrowser, download_files};
use super::fixture::{self, Behavior, Gate};
use super::scenarios::{Env, Result, files_json, settle, sha_of};
use crate::util::sha256_hex;
use asset_store::batch_runtime::SafeBatchRuntime;
use asset_store::forvo::batch::{ForvoBatch, StoredAcquisition};
use asset_store::forvo::{Attempt, ForvoEvidence, RANKING, word_url};
use asset_store::pronunciation::{Pronunciation, PronunciationDomainPolicy};
use asset_store::{AssetStore, StoreOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Запуск CLI с отдельным тестовым хранилищем сценария и каталогом сопряжения прогона.
pub struct Cli {
    bin: PathBuf,
    repo: PathBuf,
    pub store: PathBuf,
    state: PathBuf,
    pub profile: PathBuf,
    log: PathBuf,
    path_override: Option<PathBuf>,
}

pub struct Run {
    pub code: i32,
    pub out: Value,
}

impl Run {
    fn outcome(&self) -> &str {
        self.out["outcome"].as_str().unwrap_or("?")
    }
    fn error_code(&self) -> &str {
        self.out["error"]["details"]["code"]
            .as_str()
            .unwrap_or_default()
    }
    fn summary(&self) -> Value {
        json!({"exit":self.code,"outcome":self.out["outcome"],"error_code":self.out["error"]["details"]["code"],
            "paused":self.out["result"]["paused"]["code"]})
    }
}

impl Cli {
    pub fn new(env: &Env) -> Option<Cli> {
        Some(Cli {
            bin: env.ctx.cli.clone()?,
            repo: env.ctx.repo.clone(),
            store: env.dir.join("store"),
            state: env.ctx.state_dir.clone(),
            profile: env.dir.join("cdp-profile-never-created"),
            log: env.dir.join("cli.log"),
            path_override: None,
        })
    }

    fn command(&self, brave: bool, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.bin);
        command
            .arg("--repository-root")
            .arg(&self.repo)
            .arg("--store")
            .arg(&self.store)
            .arg("forvo");
        if brave {
            command.arg("brave").arg("--state-dir").arg(&self.state);
        } else {
            command.arg("--profile").arg(&self.profile);
        }
        command.args(args);
        if let Some(path) = &self.path_override {
            command.env("PATH", path);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .kill_on_drop(true);
        command
    }

    pub fn spawn(&self, brave: bool, args: &[&str]) -> Result<tokio::process::Child> {
        self.command(brave, args)
            .spawn()
            .map_err(|e| format!("запуск pronunciation-assets: {e}"))
    }

    pub async fn finish(child: tokio::process::Child) -> Result<Run> {
        let output = tokio::time::timeout(Duration::from_secs(150), child.wait_with_output())
            .await
            .map_err(|_| "pronunciation-assets не завершился за 150 с".to_owned())?
            .map_err(|e| e.to_string())?;
        let out = serde_json::from_slice(&output.stdout).map_err(|_| {
            format!(
                "pronunciation-assets вернул не JSON (код {:?})",
                output.status.code()
            )
        })?;
        Ok(Run {
            code: output.status.code().unwrap_or(-1),
            out,
        })
    }

    pub async fn run(&self, brave: bool, args: &[&str]) -> Result<Run> {
        Self::finish(self.spawn(brave, args)?).await
    }

    fn open_store(&self) -> Result<AssetStore> {
        AssetStore::open_existing_with_policy(
            StoreOptions::new(&self.store),
            PronunciationDomainPolicy,
        )
        .map_err(|e| e.message)
    }

    /// SHA и состояние жизненного цикла ресурса точной пары, если он импортирован.
    fn asset(&self, surface: &str, reading: &str) -> Result<Option<(String, String)>> {
        let identity = Pronunciation::new(surface, reading)
            .map_err(|e| e.message)?
            .identity();
        Ok(self
            .open_store()?
            .verify_integrity()
            .map_err(|e| e.message)?
            .into_iter()
            .find(|r| r.identity == identity)
            .map(|r| (r.sha256, r.lifecycle.as_str().to_owned())))
    }

    fn plan(&self, dir: &Path, name: &str, words: &[(&str, &str)]) -> Result<PathBuf> {
        let path = dir.join(format!("{name}.json"));
        let items: Vec<_> = words
            .iter()
            .map(|(s, r)| json!({"surface":s,"reading":r}))
            .collect();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"schema_version":1,"items":items})).expect("JSON"),
        )
        .map_err(|e| e.to_string())?;
        Ok(path)
    }
}

fn signal(child: &tokio::process::Child, signal: rustix::process::Signal) -> Result<()> {
    let pid = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
        .ok_or("процесс CLI уже завершён")?;
    rustix::process::kill_process(pid, signal).map_err(|e| e.to_string())
}

fn blocked_without_cli(env: &mut Env) -> Result<Cli> {
    Cli::new(env).ok_or_else(|| {
        "нет pronunciation-assets: соберите cargo build -p asset-store --bin pronunciation-assets или задайте --cli".to_owned()
    })
}

fn sha(env: &Env, record: &str) -> String {
    sha256_hex(&env.ctx.fixture.bytes_for(record))
}

fn requests(env: &Env, records: &[&str]) -> Value {
    json!(
        records
            .iter()
            .map(|r| ((*r).to_owned(), json!(env.ctx.fixture.requests_for(r))))
            .collect::<serde_json::Map<_, _>>()
    )
}

fn item<'a>(run: &'a Run, surface: &str) -> &'a Value {
    run.out["result"]["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["request"]["surface"] == surface))
        .unwrap_or(&Value::Null)
}

fn codes(run: &Run) -> Vec<Value> {
    run.out["result"]["items"]
        .as_array()
        .map(|items| items.iter().map(|i| i["outcome"]["code"].clone()).collect())
        .unwrap_or_default()
}

/// Журнал NDJSON запуска (путь из `diagnostic_log` результата или ошибки).
fn run_log(run: &Run) -> String {
    run.out["result"]["diagnostic_log"]
        .as_str()
        .or_else(|| run.out["error"]["details"]["diagnostic_log"].as_str())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Число файлов в `evidence/` каталога сопряжения: его наполняет только `smoke`
/// предыдущих сценариев прогона, production-путь копий не создаёт.
fn evidence_files(env: &Env) -> usize {
    std::fs::read_dir(env.ctx.state_dir.join("evidence"))
        .map(|d| d.count())
        .unwrap_or(0)
}

// --- Сценарии --------------------------------------------------------------

/// Одиночный production fetch и недоступность технической проверки.
pub async fn cli_fetch(env: &mut Env<'_>) -> Result<()> {
    let mut cli = blocked_without_cli(env)?;
    let _user = env.browser.user_tab("https://forvo.com/").await?;
    let evidence_before = evidence_files(env);
    let run = cli
        .run(
            true,
            &[
                "fetch",
                "--surface",
                fixture::WORD,
                "--reading",
                fixture::READING,
                "--wait-seconds",
                "30",
            ],
        )
        .await?;
    let result = &run.out["result"];
    let sha_a = sha(env, fixture::RECORD_A);
    let asset = cli.asset(fixture::WORD, fixture::READING)?;
    env.out.check(
        "forvo brave fetch импортировал оригинальный MP3 лучшей записи в отдельное тестовое хранилище",
        run.code == 0
            && result["transport"] == "brave_extension"
            && result["acquisition"]["selected_record_id"] == fixture::RECORD_A
            && result["acquisition"]["downloaded_sha256"] == sha_a.as_str()
            && asset.as_ref().is_some_and(|(s, _)| *s == sha_a),
        json!({"run":run.summary(),"asset":asset,"expected_sha256":sha_a}),
    );
    env.out.check(
        "техническая проверка не дала фонетического доверия (ресурс не Verified)",
        asset.as_ref().is_some_and(|(_, l)| l != "verified"),
        json!({"lifecycle":asset.as_ref().map(|a| &a.1)}),
    );
    env.out.check(
        "одна сетевая загрузка, собственный файл удалён, smoke-копий в evidence/ нет, вкладки пользователя не изменены",
        env.ctx.fixture.requests_for(fixture::RECORD_A) == 1
            && download_files(&env.downloads()).is_empty()
            && evidence_files(env) == evidence_before
            && result["browser"]["user_tabs_untouched"] == true
            && result["browser"]["operations"][0]["release"]["removed_file"] == true,
        json!({"requests":requests(env,&[fixture::RECORD_A]),"downloads":files_json(&env.downloads()),
            "browser":result["browser"]}),
    );
    // Повтор тех же байтов: ресурс и доверие не меняются.
    let again = cli
        .run(
            true,
            &[
                "fetch",
                "--surface",
                fixture::WORD,
                "--reading",
                fixture::READING,
            ],
        )
        .await?;
    env.out.check(
        "повторный fetch тех же байтов успешен и не создаёт второго ресурса",
        again.code == 0 && cli.asset(fixture::WORD, fixture::READING)? == asset,
        again.summary(),
    );
    // Без ffprobe/ffmpeg проверка MP3 недоступна: блокер, а не «плохая запись».
    let empty = env.dir.join("empty-path");
    std::fs::create_dir_all(&empty).map_err(|e| e.to_string())?;
    cli.path_override = Some(empty);
    let unverifiable = cli
        .run(
            true,
            &[
                "fetch",
                "--surface",
                fixture::WORD_3,
                "--reading",
                fixture::READING_3,
            ],
        )
        .await?;
    cli.path_override = None;
    tokio::time::sleep(Duration::from_millis(500)).await;
    env.out.check(
        "недоступная проверка MP3 → technical_validation_failed, ресурс не импортирован, файл удалён",
        unverifiable.code == 3
            && unverifiable.error_code() == "technical_validation_failed"
            && cli.asset(fixture::WORD_3, fixture::READING_3)?.is_none()
            && download_files(&env.downloads()).is_empty(),
        json!({"run":unverifiable.summary(),"downloads":files_json(&env.downloads())}),
    );
    env.connect().await?;
    env.residual_clean().await
}

/// Пакет из трёх слов: переход к следующему кандидату, CAS-импорт, resume без
/// повторных загрузок, закреплённый транспорт и совместимость состояния v1.
pub async fn cli_batch(env: &mut Env<'_>) -> Result<()> {
    let cli = blocked_without_cli(env)?;
    let words = [
        (fixture::WORD, fixture::READING),
        (fixture::WORD_2, fixture::READING_2),
        (fixture::WORD_3, fixture::READING_3),
    ];
    let plan = cli.plan(&env.dir, "plan", &words)?;
    let plan = plan.to_string_lossy().into_owned();
    env.ctx
        .fixture
        .set(fixture::RECORD_2_TOP, Behavior::Garbage);
    let records = [
        fixture::RECORD_A,
        fixture::RECORD_2_TOP,
        fixture::RECORD_2_NEXT,
        fixture::RECORD_3,
    ];
    let evidence_before = evidence_files(env);
    let run = cli
        .run(
            true,
            &["batch-start", "--batch-id", "words", "--plan", &plan],
        )
        .await?;
    let expected = [
        (fixture::WORD, fixture::READING, fixture::RECORD_A),
        (fixture::WORD_2, fixture::READING_2, fixture::RECORD_2_NEXT),
        (fixture::WORD_3, fixture::READING_3, fixture::RECORD_3),
    ];
    let mut stored = Vec::new();
    let mut store_ok = true;
    for (surface, reading, record) in expected {
        let asset = cli.asset(surface, reading)?;
        store_ok &= asset
            .as_ref()
            .is_some_and(|(s, l)| *s == sha(env, record) && l != "verified");
        stored.push(
            json!({"surface":surface,"record":record,"asset":asset,"expected":sha(env, record)}),
        );
    }
    env.out.check(
        "пакет из трёх разных слов импортировал оригинальные MP3 выбранных записей (не Verified)",
        run.code == 0
            && run.outcome() == "ok"
            && run.out["result"]["effective_transport"] == "brave_extension"
            && codes(&run).iter().all(|c| c == "imported")
            && store_ok,
        json!({"run":run.summary(),"items":codes(&run),"store":stored}),
    );
    let water = item(&run, fixture::WORD_2);
    let evidence = &water["acquisition"]["evidence"];
    env.out.check(
        "не-MP3 лучшей записи → переход к следующей; ForvoEvidence фиксирует обе попытки",
        evidence["selected_record_id"] == fixture::RECORD_2_NEXT
            && evidence["attempts"]
                == json!([{"record_id":fixture::RECORD_2_TOP,"code":"invalid_mp3"},{"record_id":fixture::RECORD_2_NEXT,"code":"downloaded"}])
            && evidence["ranking_policy"] == RANKING,
        json!({"evidence":evidence}),
    );
    let before = requests(env, &records);
    env.out.check(
        "ровно одна сетевая загрузка на попытку, собственные файлы удалены",
        before == json!({fixture::RECORD_A:1,fixture::RECORD_2_TOP:1,fixture::RECORD_2_NEXT:1,fixture::RECORD_3:1})
            && download_files(&env.downloads()).is_empty()
            && evidence_files(env) == evidence_before,
        json!({"requests":before,"downloads":files_json(&env.downloads())}),
    );
    let resumed = cli
        .run(true, &["batch-resume", "--batch-id", "words"])
        .await?;
    env.out.check(
        "resume завершённого пакета ничего не скачивает и не меняет ревизию",
        resumed.code == 0
            && requests(env, &records) == before
            && resumed.out["result"]["revision"] == run.out["result"]["revision"],
        json!({"run":resumed.summary(),"requests":requests(env,&records)}),
    );
    let cdp = cli
        .run(false, &["batch-resume", "--batch-id", "words"])
        .await?;
    let status = cli
        .run(false, &["batch-status", "--batch-id", "words"])
        .await?;
    env.out.check(
        "CDP batch-resume пакета Brave → transport_mismatch без запуска Chromium; batch-status без браузера",
        cdp.code == 3
            && cdp.error_code() == "transport_mismatch"
            && !cli.profile.exists()
            && status.code == 0
            && status.out["result"]["effective_transport"] == "brave_extension",
        json!({"cdp":cdp.summary(),"next_action":cdp.out["error"]["details"]["next_action"],
            "status_transport":status.out["result"]["effective_transport"]}),
    );
    // max-items: обрабатывается ровно один элемент, остальные ждут.
    let small = cli.plan(
        &env.dir,
        "small",
        &[(fixture::WORD_3, fixture::READING_3), ("元気", "げんき")],
    )?;
    let small_store = env.dir.join("store-max-items");
    let mut limited_cli = Cli::new(env).expect("cli");
    limited_cli.store = small_store;
    let limited = limited_cli
        .run(
            true,
            &[
                "batch-start",
                "--batch-id",
                "limited",
                "--plan",
                &small.to_string_lossy(),
                "--max-items",
                "1",
            ],
        )
        .await?;
    env.out.check(
        "--max-items 1 обрабатывает ровно один элемент; остальные остаются незавершёнными",
        limited.code == 0 && codes(&limited) == [json!("imported"), Value::Null],
        json!({"run":limited.summary(),"items":codes(&limited)}),
    );
    // Состояние версии 1 так, как его сохранял прежний CDP-код.
    let legacy = json!({"schema_version":1,"provider":"forvo_web","batch_id":"legacy","revision":0,
        "cancelled":false,"paused":null,"items":[{"request":Pronunciation::new(fixture::WORD,fixture::READING).map_err(|e| e.message)?,
        "replace_expected_sha256":null,"acquisition":null,"outcome":null}]});
    let state: ForvoBatch = serde_json::from_value(legacy).map_err(|e| e.to_string())?;
    {
        let store = cli.open_store()?;
        let mut runtime = SafeBatchRuntime::open(store.root(), "legacy").map_err(|e| e.message)?;
        runtime.save(&state).map_err(|e| e.message)?;
    }
    let requests_before = requests(env, &records);
    let brave_resume = cli
        .run(true, &["batch-resume", "--batch-id", "legacy"])
        .await?;
    let legacy_status = cli
        .run(false, &["batch-status", "--batch-id", "legacy"])
        .await?;
    env.out.check(
        "пакет v1 (CDP) читается, остаётся CDP и не продолжается через Brave",
        brave_resume.code == 3
            && brave_resume.error_code() == "transport_mismatch"
            && legacy_status.code == 0
            && legacy_status.out["result"]["schema_version"] == 1
            && legacy_status.out["result"]["effective_transport"] == "cdp_chromium"
            && requests(env, &records) == requests_before,
        json!({"brave_resume":brave_resume.summary(),"status":legacy_status.summary(),
            "schema_version":legacy_status.out["result"]["schema_version"]}),
    );
    env.connect().await?;
    env.residual_clean().await
}

fn checkpoint(
    cli: &Cli,
    id: &str,
    surface: &str,
    reading: &str,
    bytes: &[u8],
    record: &str,
    import: bool,
) -> Result<()> {
    let store = cli.open_store()?;
    let request = Pronunciation::new(surface, reading).map_err(|e| e.message)?;
    let mut runtime = SafeBatchRuntime::open(store.root(), id).map_err(|e| e.message)?;
    let mut state: ForvoBatch = runtime
        .load()
        .map_err(|e| e.message)?
        .ok_or("состояние пакета отсутствует")?;
    let blob = runtime.persist_blob(bytes, "mp3").map_err(|e| e.message)?;
    let candidate = asset_store::forvo::Candidate {
        record_id: record.into(),
        username: None,
        votes: Some(5),
        gender: Some(asset_store::forvo::Gender::Female),
        country: Some("Japan".into()),
        download_available: true,
    };
    let evidence = ForvoEvidence {
        schema_version: 1,
        page_url: word_url(surface),
        downloaded_sha256: blob.sha256.clone(),
        selected_record_id: record.into(),
        ranking_policy: RANKING.into(),
        candidates: vec![candidate],
        attempts: vec![Attempt {
            record_id: record.into(),
            code: "downloaded".into(),
        }],
    };
    if import {
        asset_store::forvo::import_acquisition(
            &store,
            &request,
            &asset_store::forvo::Acquisition {
                bytes: bytes.to_vec(),
                evidence: evidence.clone(),
            },
            None,
        )
        .map_err(|e| e.message)?;
    }
    let item = state
        .items
        .iter_mut()
        .find(|i| i.request == request)
        .ok_or("нет элемента")?;
    item.acquisition = Some(StoredAcquisition { blob, evidence });
    state.revision += 1;
    runtime.save(&state).map_err(|e| e.message)
}

/// Ctrl+C и падение во время загрузки, `batch-cancel` и `batch-status` во время
/// работы, два исполнителя одного пакета, восстановление после checkpoint и после импорта.
pub async fn cli_batch_recovery(env: &mut Env<'_>) -> Result<()> {
    let cli = blocked_without_cli(env)?;
    let fixture = env.ctx.fixture.clone();
    let both = cli.plan(
        &env.dir,
        "both",
        &[
            (fixture::WORD, fixture::READING),
            (fixture::WORD_3, fixture::READING_3),
        ],
    )?;
    let both = both.to_string_lossy().into_owned();
    // 1. Ctrl+C, пока собственная загрузка идёт.
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let child = cli.spawn(
        true,
        &["batch-start", "--batch-id", "intr", "--plan", &both],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(40))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    signal(&child, rustix::process::Signal::INT)?;
    let interrupted = Cli::finish(child).await?;
    drop(gate);
    let files = settle(&env.downloads(), &[], Duration::from_secs(5)).await;
    let log = run_log(&interrupted);
    let canceled = log.lines().any(|l| {
        l.contains("brave_download_released") && l.contains("\"canceled\":\"Some(true)\"")
    });
    env.out.check(
        "Ctrl+C во время загрузки → paused interrupted (код 130), собственная незавершённая загрузка отменена и стёрта",
        interrupted.code == 130
            && interrupted.outcome() == "paused"
            && interrupted.out["result"]["paused"]["code"] == "interrupted"
            && codes(&interrupted) == [Value::Null, Value::Null]
            && canceled
            && files.is_empty(),
        json!({"run":interrupted.summary(),"release_canceled_logged":canceled,"downloads":files_json(&env.downloads())}),
    );
    fixture.reset();
    let resumed = cli
        .run(true, &["batch-resume", "--batch-id", "intr"])
        .await?;
    env.out.check(
        "resume после Ctrl+C доводит пакет; прерванная запись скачана заново один раз",
        resumed.code == 0
            && codes(&resumed).iter().all(|c| c == "imported")
            && fixture.requests_for(fixture::RECORD_A) == 1
            && fixture.requests_for(fixture::RECORD_3) == 1,
        json!({"run":resumed.summary(),"requests":requests(env,&[fixture::RECORD_A,fixture::RECORD_3])}),
    );
    // 2. status и cancel во время работы; параллельная загрузка пользователя другой записи.
    fixture.reset();
    let user = env
        .browser
        .user_tab(&format!(
            "https://forvo.com/word/{}/",
            fixture::encode(fixture::WORD)
        ))
        .await?;
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let one = cli.plan(&env.dir, "one", &[(fixture::WORD, fixture::READING)])?;
    let one = one.to_string_lossy().into_owned();
    let mut cancel_cli = Cli::new(env).expect("cli");
    cancel_cli.store = env.dir.join("store-cancel");
    let child = cancel_cli.spawn(
        true,
        &["batch-start", "--batch-id", "cancel", "--plan", &one],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(40))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    TestBrowser::click(&user, &format!("a[data-p4=\"{}\"]", fixture::RECORD_B)).await?;
    let user_b = "pronunciation_ja_5036880.mp3";
    settle(&env.downloads(), &[user_b], Duration::from_secs(10)).await;
    let during = cancel_cli
        .run(false, &["batch-status", "--batch-id", "cancel"])
        .await?;
    let cancel = cancel_cli
        .run(false, &["batch-cancel", "--batch-id", "cancel"])
        .await?;
    let cancelled = Cli::finish(child).await?;
    drop(gate);
    let files = settle(&env.downloads(), &[user_b], Duration::from_secs(5)).await;
    env.out.check(
        "batch-status во время работы читает состояние без браузера; batch-cancel останавливает исполнителя (cancelled, 130)",
        during.code == 0
            && codes(&during) == [Value::Null]
            && cancel.out["result"]["cancelled"] == true
            && cancelled.code == 130
            && cancelled.outcome() == "cancelled",
        json!({"status":during.summary(),"cancel":cancel.summary(),"runner":cancelled.summary()}),
    );
    env.out.check(
        "при отмене собственная загрузка стёрта, а загрузка пользователя другой записи цела",
        files.len() == 1 && sha_of(&files, user_b) == Some(sha(env, fixture::RECORD_B)),
        json!({"downloads":files_json(&env.downloads())}),
    );
    let refused = cancel_cli
        .run(true, &["batch-resume", "--batch-id", "cancel"])
        .await?;
    env.out.check(
        "отменённый пакет не возобновляется",
        refused.code == 3,
        refused.summary(),
    );
    // 3. Два исполнителя одного batch_id.
    fixture.reset();
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let mut lease_cli = Cli::new(env).expect("cli");
    lease_cli.store = env.dir.join("store-lease");
    let first = lease_cli.spawn(
        true,
        &["batch-start", "--batch-id", "lease", "--plan", &one],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(40))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    let second = lease_cli
        .run(true, &["batch-resume", "--batch-id", "lease"])
        .await?;
    gate.open();
    let first = Cli::finish(first).await?;
    env.out.check(
        "второй исполнитель того же пакета отвергнут сразу; первый завершил пакет",
        second.code == 3
            && second.out["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("исполняется"))
            && first.code == 0
            && codes(&first) == [json!("imported")]
            && fixture.requests_for(fixture::RECORD_A) == 1,
        json!({"second":second.summary(),"second_message":second.out["error"]["message"],"first":first.summary()}),
    );
    // 4. Падение исполнителя (SIGKILL) во время загрузки: операция остаётся в
    // расширении и освобождается следующим запуском.
    fixture.reset();
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldBody(rx));
    let mut crash_cli = Cli::new(env).expect("cli");
    crash_cli.store = env.dir.join("store-crash");
    let child = crash_cli.spawn(
        true,
        &["batch-start", "--batch-id", "crash", "--plan", &one],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(40))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    signal(&child, rustix::process::Signal::KILL)?;
    let _ = Cli::finish(child).await;
    drop(gate);
    fixture.reset();
    let recovered = crash_cli
        .run(true, &["batch-resume", "--batch-id", "crash"])
        .await?;
    let files = settle(&env.downloads(), &[user_b], Duration::from_secs(5)).await;
    let recovery_logged = run_log(&recovered).contains("brave_operation_recovery");
    env.out.check(
        "после SIGKILL resume освобождает прошлую собственную операцию и завершает пакет; чужой файл цел",
        recovered.code == 0
            && recovery_logged
            && codes(&recovered) == [json!("imported")]
            && fixture.requests_for(fixture::RECORD_A) == 1
            && files.len() == 1
            && sha_of(&files, user_b) == Some(sha(env, fixture::RECORD_B))
            && crash_cli.asset(fixture::WORD, fixture::READING)?.is_some(),
        json!({"run":recovered.summary(),"recovery_logged":recovery_logged,
            "downloads":files_json(&env.downloads())}),
    );
    // 5. Checkpoint сохранён, импорт не выполнен; 6. импорт выполнен, итог не записан.
    fixture.reset();
    let mut point_cli = Cli::new(env).expect("cli");
    point_cli.store = env.dir.join("store-checkpoint");
    let zero = point_cli
        .run(
            true,
            &[
                "batch-start",
                "--batch-id",
                "points",
                "--plan",
                &both,
                "--max-items",
                "0",
            ],
        )
        .await?;
    let bytes_a = fixture.bytes_for(fixture::RECORD_A);
    let bytes_3 = fixture.bytes_for(fixture::RECORD_3);
    checkpoint(
        &point_cli,
        "points",
        fixture::WORD,
        fixture::READING,
        &bytes_a,
        fixture::RECORD_A,
        false,
    )?;
    checkpoint(
        &point_cli,
        "points",
        fixture::WORD_3,
        fixture::READING_3,
        &bytes_3,
        fixture::RECORD_3,
        true,
    )?;
    let finished = point_cli
        .run(true, &["batch-resume", "--batch-id", "points"])
        .await?;
    env.out.check(
        "resume после checkpoint и после импорта берёт сохранённые байты без браузерных загрузок",
        zero.code == 0
            && finished.code == 0
            && codes(&finished).iter().all(|c| c == "imported")
            && fixture.log().iter().all(|p| !p.starts_with("/download/"))
            && point_cli
                .asset(fixture::WORD, fixture::READING)?
                .map(|a| a.0)
                == Some(sha256_hex(&bytes_a))
            && point_cli
                .asset(fixture::WORD_3, fixture::READING_3)?
                .map(|a| a.0)
                == Some(sha256_hex(&bytes_3)),
        json!({"run":finished.summary(),"fixture_requests":fixture.log()}),
    );
    env.connect().await?;
    env.residual_clean().await
}

/// Чужая загрузка той же записи во время пакета: владение неоднозначно → пакет
/// приостановлен, чужие файлы не тронуты; повторный запуск завершает пакет.
pub async fn cli_foreign_downloads(env: &mut Env<'_>) -> Result<()> {
    let cli = blocked_without_cli(env)?;
    let fixture = env.ctx.fixture.clone();
    let user = env
        .browser
        .user_tab(&format!(
            "https://forvo.com/word/{}/",
            fixture::encode(fixture::WORD)
        ))
        .await?;
    let user_other = env.browser.user_tab("https://forvo.com/").await?;
    let plan = cli.plan(
        &env.dir,
        "plan",
        &[
            (fixture::WORD, fixture::READING),
            (fixture::WORD_3, fixture::READING_3),
        ],
    )?;
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_A, Behavior::HoldHeaders(rx));
    let child = cli.spawn(
        true,
        &[
            "batch-start",
            "--batch-id",
            "foreign",
            "--plan",
            &plan.to_string_lossy(),
        ],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_A, 1, Duration::from_secs(40))
        .await
    {
        return Err("собственный запрос не пришёл".into());
    }
    TestBrowser::user_foreign_download(&user_other).await?;
    TestBrowser::click(&user, &format!("a[data-p4=\"{}\"]", fixture::RECORD_A)).await?;
    let paused = Cli::finish(child).await?;
    gate.open();
    let user_a = "pronunciation_ja_3661006.mp3";
    let files = settle(
        &env.downloads(),
        &[user_a, "user-notes.bin"],
        Duration::from_secs(15),
    )
    .await;
    let user_sha = sha256_hex(&fixture.user_file);
    env.out.check(
        "загрузка той же записи из чужой вкладки → пакет приостановлен (download_conflict), слово не объявлено безнадёжным",
        paused.code == 3
            && paused.outcome() == "paused"
            && paused.out["result"]["paused"]["code"] == "download_conflict"
            && codes(&paused) == [Value::Null, Value::Null],
        paused.summary(),
    );
    env.out.check(
        "файлы пользователя (та же запись и чужой сайт) не тронуты при остановке",
        sha_of(&files, user_a) == Some(sha(env, fixture::RECORD_A))
            && sha_of(&files, "user-notes.bin") == Some(user_sha.clone()),
        json!({"downloads":files_json(&env.downloads())}),
    );
    fixture.reset();
    let resumed = cli
        .run(true, &["batch-resume", "--batch-id", "foreign"])
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let files = download_files(&env.downloads());
    env.out.check(
        "повторный запуск завершил пакет; удалены только собственные файлы, файлы пользователя целы",
        resumed.code == 0
            && codes(&resumed).iter().all(|c| c == "imported")
            && files.len() >= 2
            && sha_of(&files, user_a) == Some(sha(env, fixture::RECORD_A))
            && sha_of(&files, "user-notes.bin") == Some(user_sha)
            && files.iter().all(|(n, _, _)| n == user_a || n == "user-notes.bin" || n.starts_with("pronunciation_ja_3661006 (")),
        json!({"run":resumed.summary(),"downloads":files_json(&env.downloads())}),
    );
    env.connect().await?;
    env.residual_clean().await
}
