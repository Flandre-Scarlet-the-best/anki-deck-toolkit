//! Настоящий SIGKILL в наблюдённых syscall-окнах production CLI. GDB только
//! останавливает процесс, читает уже записанное состояние и посылает сигнал;
//! ни результата, ни байтов состояния/медиа он не создаёт и не подменяет.

use super::cli_scenarios::{Cli, MISSING_CLI};
use super::fixture;
use super::observation;
use super::scenarios::{Env, Result};
use asset_store::batch_runtime::SafeBatchRuntime;
use asset_store::forvo::batch::ForvoBatch;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;

pub(super) fn prerequisite() -> Option<String> {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return Some(
            "точные fsync-окна драйвера требуют Linux x86_64 (аргумент syscall в rdi)".into(),
        );
    }
    if !std::process::Command::new("strace")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Some("нужен strace для независимого подтверждения wait4/SIGKILL; trace без -f наблюдает только GDB".into());
    }
    let output = std::process::Command::new("gdb")
        .args([
            "--batch",
            "-nx",
            "-iex",
            "set auto-load off",
            "-iex",
            "set debuginfod enabled off",
            "-ex",
            "python import json; print('gdb-python-ready')",
        ])
        .output();
    if !output.is_ok_and(|o| {
        o.status.success() && String::from_utf8_lossy(&o.stdout).contains("gdb-python-ready")
    }) {
        return Some(
            "нужен GDB с Python и поддержкой catch syscall; init и debuginfod отключены".into(),
        );
    }
    None
}

struct DebuggerGroup(rustix::process::Pid);
impl Drop for DebuggerGroup {
    fn drop(&mut self) {
        // Только явно созданная этим тестом группа GDB и его inferior.
        let _ = rustix::process::kill_process_group(self.0, rustix::process::Signal::KILL);
    }
}

pub(super) async fn run(env: &mut Env<'_>) -> Result<()> {
    let script = env.dir.join("crash-window.gdb");
    std::fs::write(&script, include_str!("crash-window.gdb")).map_err(|e| e.to_string())?;
    let user = env.browser.user_tab("https://forvo.com/").await?;
    super::chrome::TestBrowser::user_foreign_download(&user).await?;
    let user_files = super::scenarios::settle(
        &env.downloads(),
        &["user-notes.bin"],
        Duration::from_secs(10),
    )
    .await;
    if user_files.len() != 1
        || user_files[0].2 != crate::util::sha256_hex(&env.ctx.fixture.user_file)
    {
        return Err("контрольная чужая загрузка не получена".into());
    }
    for window in [
        "blob_sync",
        "checkpoint_sync",
        "import_sync",
        "outcome_sync",
    ] {
        env.ctx.fixture.reset();
        println!("  аварийное окно {window}");
        let mut cli = Cli::new(env).ok_or(MISSING_CLI)?;
        cli.store = env.dir.join(format!("store-{window}"));
        let plan = cli.plan(
            &env.dir,
            window,
            &[
                (fixture::WORD, fixture::READING),
                (fixture::WORD_3, fixture::READING_3),
            ],
        )?;
        let invocation = cli.command(
            true,
            &[
                "batch-start",
                "--batch-id",
                "window",
                "--plan",
                &plan.to_string_lossy(),
                "--close-tab",
            ],
        );
        let stderr = std::fs::File::create(env.dir.join(format!("gdb-{window}.stderr.log")))
            .map_err(|e| e.to_string())?;
        let expected_sha = crate::util::sha256_hex(&env.ctx.fixture.bytes_for(fixture::RECORD_3));
        let trace_path = env.dir.join(format!("gdb-{window}.wait.log"));
        let child = tokio::process::Command::new("strace")
            .args(["-qq", "-e", "trace=kill,tkill,tgkill,wait4,waitid", "-o"])
            .arg(&trace_path)
            .args(["--", "gdb"])
            .args([
                "--batch",
                "-nx",
                "-iex",
                "set auto-load off",
                "-iex",
                "set debuginfod enabled off",
                "-x",
            ])
            .arg(&script)
            .arg("--args")
            .arg(invocation.as_std().get_program())
            .args(invocation.as_std().get_args())
            .env("ANKI_E2E_CRASH_STORE", &cli.store)
            .env("ANKI_E2E_CRASH_WINDOW", window)
            .env("ANKI_E2E_CRASH_SHA256", &expected_sha)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let group = DebuggerGroup(
            rustix::process::Pid::from_raw(child.id().ok_or("нет PID GDB")? as i32)
                .ok_or("PID GDB вне диапазона")?,
        );
        let output = tokio::time::timeout(Duration::from_secs(100), child.wait_with_output()).await;
        drop(group);
        let output = output
            .map_err(|_| {
                format!("{window}: GDB/CLI не завершился за 100 с; собственная группа остановлена")
            })?
            .map_err(|e| e.to_string())?;
        std::fs::write(
            env.dir.join(format!("gdb-{window}.stdout.log")),
            &output.stdout,
        )
        .map_err(|e| e.to_string())?;
        let marker: Value = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("ANKI_E2E_CRASH="))
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or_else(|| format!("{window}: нет доказательства GDB; см. gdb-{window}.*.log"))?;
        let checkpointed = matches!(window, "import_sync" | "outcome_sync");
        let state_path = cli.store.join(".runtime/batches/window/state.json");
        let bytes = std::fs::read(&state_path).map_err(|e| e.to_string())?;
        let actual_state_sha = crate::util::sha256_hex(&bytes);
        let inferior = marker["point"]["inferior_pid"].as_u64();
        let wait_log = std::fs::read_to_string(&trace_path).map_err(|e| e.to_string())?;
        let kernel_status = inferior.and_then(|pid| {
            wait_log
                .lines()
                .find(|line| {
                    line.contains("si_code=CLD_KILLED")
                        && line.contains(&format!("si_pid={pid},"))
                        && line.contains("si_status=SIGKILL")
                })
                .map(str::to_owned)
        });
        let signalled = kernel_status.is_some();
        let reached = signalled
            && marker["point"]["window"] == window
            && marker["point"]["first_imported"] == true
            && marker["point"]["second_checkpointed"] == checkpointed
            && marker["point"]["state_sha256"] == actual_state_sha;
        env.out.check(format!("{window}: SIGKILL в подтверждённом fsync-окне, прежний state цел"),reached,json!({"gdb_exit":output.status.code(),"marker":marker,"kernel_sigchld":kernel_status,"state_sha256":actual_state_sha}));
        if !reached {
            return Err(format!("{window}: нужное окно не доказано"));
        }
        {
            let store = cli.open_store()?;
            let mut runtime =
                SafeBatchRuntime::open(store.root(), "window").map_err(|e| e.message)?;
            let state: ForvoBatch = runtime.load().map_err(|e| e.message)?.ok_or("нет state")?;
            env.out.check(format!("{window}: первый импорт и подтверждённые blobs читаются после SIGKILL"),state.items[0].outcome.as_ref().is_some_and(|o|o.code=="imported") && state.items[1].acquisition.is_some()==checkpointed,json!({"revision":state.revision,"second_checkpointed":state.items[1].acquisition.is_some()}));
        }
        let resumed = cli
            .run(
                true,
                &["batch-resume", "--batch-id", "window", "--close-tab"],
            )
            .await?;
        let second_downloads = if checkpointed { 1 } else { 2 };
        let imports = resumed.out["result"]["items"]
            .as_array()
            .is_some_and(|items| {
                items.len() == 2
                    && items
                        .iter()
                        .all(|item| item["outcome"]["code"] == "imported")
            });
        env.out.check(format!("{window}: resume завершает пакет с объяснимым числом загрузок"),resumed.code==0 && imports && env.ctx.fixture.requests_for(fixture::RECORD_A)==1 && env.ctx.fixture.requests_for(fixture::RECORD_3)==second_downloads,json!({"exit":resumed.code,"downloads_first":env.ctx.fixture.requests_for(fixture::RECORD_A),"downloads_second":env.ctx.fixture.requests_for(fixture::RECORD_3),"expected_second":second_downloads,"reason":if checkpointed {"подтверждённый checkpoint исключает скачивание"} else {"вторые байты ещё не подтверждены state.json; повтор необходим"}}));
        let again = cli
            .run(true, &["batch-resume", "--batch-id", "window"])
            .await?;
        let files = observation::files(&cli.store)?;
        let batch_files = observation::files(&cli.store.join(".runtime/batches/window"))?;
        let staging = ingest_staging(&cli.store)?;
        env.out.check(format!("{window}: повтор resume идемпотентен, временных файлов пакета нет, чужой файл цел"),again.code==0 && again.out["result"]["revision"]==resumed.out["result"]["revision"] && env.ctx.fixture.requests_for(fixture::RECORD_3)==second_downloads && batch_files["temporary"]==0 && super::chrome::download_files(&env.downloads())==user_files,json!({"store_files":files,"batch_files":batch_files,"revision":again.out["result"]["revision"]}));
        // Staging импорта создаётся до блокировки хранилища, поэтому владелец
        // такого файла после SIGKILL недоказуем и он не удаляется автоматически.
        // Ограничение фиксируется явно: не больше одного файла на убитый импорт.
        let staging_allowed = if window == "import_sync" { 1 } else { 0 };
        env.out.check(
            format!("{window}: остаток staging импорта хранилища не превышает одного файла убитого импорта"),
            staging.len() <= staging_allowed,
            json!({"ingest_staging":staging,"allowed":staging_allowed,
                "reason":"staging AssetStore создаётся до блокировки хранилища; владелец после SIGKILL недоказуем — известное ограничение, не удаляется автоматически"}),
        );
        env.connect().await?;
        let ping = env
            .call("ping", json!({}))
            .await
            .map_err(super::scenarios::err)?;
        // Resume сохранённых байтов работает офлайн и браузер не подключает:
        // вкладку убитого процесса закроет следующая подключённая сессия.
        let tab_expected = checkpointed;
        env.out.check(
            format!("{window}: после resume нет наблюдателей и операций; собственная вкладка — по ожидаемому правилу"),
            ping["own_tab"]["exists"] == tab_expected
                && ping["listeners"]["active_observers"] == 0
                && ping["operation"].is_null(),
            json!({"ping":ping,"own_tab_expected":tab_expected,
                "reason":if checkpointed {"resume без скачивания не подключает браузер (офлайн-импорт); вкладку убитого процесса освобождает следующая сессия"} else {"resume скачивал заново и закрыл вкладку по --close-tab"}}),
        );
        env.residual_clean().await?;
        if let Some(mut conn) = env.conn.take() {
            conn.bridge.disconnect().await;
        }
    }
    // Вкладку, оставленную убитым процессом, закрывает следующая сессия с
    // --close-tab: накопления собственных вкладок нет.
    env.ctx.fixture.reset();
    let mut cli = Cli::new(env).ok_or(MISSING_CLI)?;
    cli.store = env.dir.join("store-release");
    let plan = cli.plan(&env.dir, "release", &[(fixture::WORD, fixture::READING)])?;
    let released = cli
        .run(
            true,
            &[
                "batch-start",
                "--batch-id",
                "release",
                "--plan",
                &plan.to_string_lossy(),
                "--close-tab",
            ],
        )
        .await?;
    env.connect().await?;
    let ping = env
        .call("ping", json!({}))
        .await
        .map_err(super::scenarios::err)?;
    env.out.check(
        "следующая сессия с --close-tab освободила вкладку убитого процесса",
        released.code == 0
            && ping["own_tab"]["exists"] == false
            && ping["listeners"]["active_observers"] == 0
            && ping["operation"].is_null()
            && super::chrome::download_files(&env.downloads()) == user_files,
        json!({"exit":released.code,"own_tab":ping["own_tab"],"listeners":ping["listeners"]}),
    );
    env.residual_clean().await?;
    if let Some(mut conn) = env.conn.take() {
        conn.bridge.disconnect().await;
    }
    Ok(())
}

/// Файлы staging импорта AssetStore (`.runtime/.tmp/ingest-*.tmp`).
fn ingest_staging(store: &std::path::Path) -> Result<Vec<String>> {
    let directory = store.join(".runtime/.tmp");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(|e| e.to_string())?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("ingest-") && name.ends_with(".tmp") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}
