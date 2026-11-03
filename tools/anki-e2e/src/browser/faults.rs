//! Детерминированные отказы настоящего CLI над изолированным store.
//! Байты состояния создаёт только production-путь; тест портит/закрывает доступ
//! к уже созданному временному файлу как внешний отказ и проверяет постусловия.

use super::cli_scenarios::{Cli, MISSING_CLI, signal};
use super::fixture::{self, Behavior, Gate};
use super::scenarios::{Env, Result};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Permissions(Vec<(PathBuf, std::fs::Permissions)>);
impl Permissions {
    fn deny(paths: &[&Path]) -> Result<Self> {
        let mut guard = Self(Vec::new());
        for path in paths {
            let old = path.metadata().map_err(|e| e.to_string())?.permissions();
            guard.0.push((path.to_path_buf(), old));
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))
                .map_err(|e| e.to_string())?;
        }
        Ok(guard)
    }
}
impl Drop for Permissions {
    fn drop(&mut self) {
        for (path, old) in &self.0 {
            let _ = std::fs::set_permissions(path, old.clone());
        }
    }
}

pub(super) async fn run(env: &mut Env<'_>) -> Result<()> {
    let cli = Cli::new(env).ok_or(MISSING_CLI)?;
    let words = [
        (fixture::WORD, fixture::READING),
        (fixture::WORD_3, fixture::READING_3),
    ];
    let plan = cli.plan(&env.dir, "fault-plan", &words)?;
    let plan = plan.to_string_lossy().into_owned();
    let fixture = env.ctx.fixture.clone();
    // Первая запись полностью импортирована настоящим CLI, вторая задержана.
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_3, Behavior::HoldBody(rx));
    let child = cli.spawn(
        true,
        &[
            "batch-start",
            "--batch-id",
            "write-failure",
            "--plan",
            &plan,
            "--close-tab",
        ],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_3, 1, Duration::from_secs(60))
        .await
    {
        return Err("не началась вторая загрузка".into());
    }
    let root = cli.store.join(".runtime/batches/write-failure");
    let state = root.join("state.json");
    let before = std::fs::read(&state).map_err(|e| e.to_string())?;
    // Blob можно надёжно записать; запрещена именно публикация state.json.
    let permissions = Permissions::deny(&[&root])?;
    gate.open();
    let failed = Cli::finish(child).await?;
    let after = std::fs::read(&state).map_err(|e| e.to_string())?;
    drop(permissions);
    let saved_blobs = std::fs::read_dir(root.join("candidates"))
        .map_err(|e| e.to_string())?
        .count();
    env.out.check(
        "ошибка записи checkpoint не уничтожает прежнее состояние и импорт первой записи",
        failed.code != 0 && before == after && saved_blobs==2,
        json!({"exit":failed.code,"error":failed.out["error"],"state_unchanged":before==after,"saved_blobs":saved_blobs,"orphan_reason":"вторые проверенные байты записаны, но не подтверждены state.json; безопасный resume получает их повторно"}),
    );
    fixture.set(fixture::RECORD_3, Behavior::Normal);
    let recovered = cli
        .run(
            true,
            &["batch-resume", "--batch-id", "write-failure", "--close-tab"],
        )
        .await?;
    env.out.check("после восстановления доступа первая запись не скачивается; вторая повторяется один раз (байты ещё не checkpoint)",
        recovered.code==0 && fixture.requests_for(fixture::RECORD_A)==1 && fixture.requests_for(fixture::RECORD_3)==2,
        json!({"exit":recovered.code,"requests_first":fixture.requests_for(fixture::RECORD_A),"requests_second":fixture.requests_for(fixture::RECORD_3),"result":recovered.out["result"]}));

    // Отказ CAS-импорта после реального durable checkpoint: resume обязан
    // использовать подтверждённые байты, а не повторять загрузку.
    let mut importing = Cli::new(env).expect("cli");
    importing.store = env.dir.join("store-import-failure");
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_3, Behavior::HoldBody(rx));
    let counts = (
        fixture.requests_for(fixture::RECORD_A),
        fixture.requests_for(fixture::RECORD_3),
    );
    let child = importing.spawn(
        true,
        &[
            "batch-start",
            "--batch-id",
            "import-failure",
            "--plan",
            &plan,
            "--close-tab",
        ],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_3, counts.1 + 1, Duration::from_secs(60))
        .await
    {
        return Err("нет второй загрузки для отказа импорта".into());
    }
    let permissions = Permissions::deny(&[&importing.store.join(".runtime/assets")])?;
    gate.open();
    let paused = Cli::finish(child).await?;
    drop(permissions);
    let checkpoint = &paused.out["result"]["items"][1]["acquisition"];
    env.out.check(
        "ошибка импорта приостанавливает пакет с настоящими сохранёнными байтами",
        paused.out["outcome"] == "paused"
            && !checkpoint.is_null()
            && paused.out["result"]["items"][1]["outcome"].is_null(),
        json!({"exit":paused.code,"paused":paused.out["result"]["paused"],"checkpoint":checkpoint}),
    );
    fixture.set(fixture::RECORD_3, Behavior::Normal);
    let resumed = importing
        .run(
            true,
            &[
                "batch-resume",
                "--batch-id",
                "import-failure",
                "--close-tab",
            ],
        )
        .await?;
    env.out.check("после отказа импорта resume использует checkpoint без повторных скачиваний",resumed.code==0 && fixture.requests_for(fixture::RECORD_A)==counts.0+1 && fixture.requests_for(fixture::RECORD_3)==counts.1+1,json!({"exit":resumed.code,"downloads_first":fixture.requests_for(fixture::RECORD_A)-counts.0,"downloads_second":fixture.requests_for(fixture::RECORD_3)-counts.1}));

    let mut corrupt = Cli::new(env).expect("cli");
    corrupt.store = env.dir.join("store-corrupt");
    let started = corrupt
        .run(
            true,
            &[
                "batch-start",
                "--batch-id",
                "corrupt",
                "--plan",
                &plan,
                "--max-items",
                "1",
                "--close-tab",
            ],
        )
        .await?;
    if started.code != 0 {
        return Err("не создан настоящий checkpoint".into());
    }
    let root = corrupt.store.join(".runtime/batches/corrupt");
    let state_before = std::fs::read(root.join("state.json")).map_err(|e| e.to_string())?;
    let blob = std::fs::read_dir(root.join("candidates"))
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("нет blob")?
        .map_err(|e| e.to_string())?
        .path();
    let original = std::fs::read(&blob).map_err(|e| e.to_string())?;
    let counts = (
        fixture.requests_for(fixture::RECORD_A),
        fixture.requests_for(fixture::RECORD_3),
    );
    // Внешнее повреждение: production runtime должен отказать до получения/импорта.
    let original_permissions = blob.metadata().map_err(|e| e.to_string())?.permissions();
    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o000))
        .map_err(|e| e.to_string())?;
    let unreadable = corrupt
        .run(true, &["batch-resume", "--batch-id", "corrupt"])
        .await;
    std::fs::set_permissions(&blob, original_permissions).map_err(|e| e.to_string())?;
    let unreadable = unreadable?;
    env.out.check(
        "ошибка чтения blob отклоняется без потери checkpoint и новых загрузок",
        unreadable.code != 0
            && std::fs::read(root.join("state.json")).map_err(|e| e.to_string())? == state_before
            && counts
                == (
                    fixture.requests_for(fixture::RECORD_A),
                    fixture.requests_for(fixture::RECORD_3),
                ),
        json!({"exit":unreadable.code,"error":unreadable.out["error"]}),
    );
    std::fs::write(&blob, &original[..original.len() / 2]).map_err(|e| e.to_string())?;
    let refused = corrupt
        .run(true, &["batch-resume", "--batch-id", "corrupt"])
        .await?;
    let unchanged =
        std::fs::read(root.join("state.json")).map_err(|e| e.to_string())? == state_before;
    std::fs::write(&blob, &original).map_err(|e| e.to_string())?;
    env.out.check(
        "усечённый checkpoint blob отклонён до загрузок/импорта; state.json сохранён",
        refused.code != 0
            && unchanged
            && counts
                == (
                    fixture.requests_for(fixture::RECORD_A),
                    fixture.requests_for(fixture::RECORD_3),
                ),
        json!({"exit":refused.code,"error":refused.out["error"],"state_unchanged":unchanged}),
    );
    let resumed = corrupt
        .run(
            true,
            &["batch-resume", "--batch-id", "corrupt", "--close-tab"],
        )
        .await?;
    env.out.check(
        "после восстановления исходных байтов resume завершает оставшуюся запись",
        resumed.code == 0
            && fixture.requests_for(fixture::RECORD_A) == counts.0
            && fixture.requests_for(fixture::RECORD_3) == counts.1 + 1,
        json!({"exit":resumed.code,"items":resumed.out["result"]["items"]}),
    );

    // SIGKILL после действительного checkpoint первой записи, при загрузке второй.
    let mut killed = Cli::new(env).expect("cli");
    killed.store = env.dir.join("store-killed");
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_3, Behavior::HoldBody(rx));
    let counts = (
        fixture.requests_for(fixture::RECORD_A),
        fixture.requests_for(fixture::RECORD_3),
    );
    let mut child = killed.spawn(
        true,
        &[
            "batch-start",
            "--batch-id",
            "killed",
            "--plan",
            &plan,
            "--close-tab",
        ],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_3, counts.1 + 1, Duration::from_secs(60))
        .await
    {
        return Err("нет активной загрузки для SIGKILL".into());
    }
    signal(&child, rustix::process::Signal::KILL)?;
    let exit = child.wait().await.map_err(|e| e.to_string())?;
    drop(gate);
    fixture.set(fixture::RECORD_3, Behavior::Normal);
    let recovered = killed
        .run(
            true,
            &["batch-resume", "--batch-id", "killed", "--close-tab"],
        )
        .await?;
    env.out.check("SIGKILL при второй загрузке: первая сохранена, второй нужен один повтор; зависания нет",!exit.success() && recovered.code==0 && fixture.requests_for(fixture::RECORD_A)==counts.0+1 && fixture.requests_for(fixture::RECORD_3)==counts.1+2,json!({"killed_status":exit.to_string(),"exit":recovered.code,"downloads_first":fixture.requests_for(fixture::RECORD_A)-counts.0,"downloads_second":fixture.requests_for(fixture::RECORD_3)-counts.1}));
    env.out.evidence.insert("not_tested".into(),json!([
        {"case":"ENOSPC","status":"NOT_TESTED","reason":"отдельный сценарий cli_batch_enospc требует --fault-volume с пустым ограниченным tmpfs; этот сценарий его не заполняет"},
        {"case":"SIGKILL внутри fsync/rename checkpoint или CAS-import","status":"NOT_TESTED","reason":"нет production failpoint для точного синхронного окна; проверены реальные отказы записи и SIGKILL после предыдущего checkpoint, но они не доказывают это окно"}
    ]));
    env.connect().await?;
    env.residual_clean().await
}

/// Заполняется только явно переданная отдельная ограниченная tmpfs, никогда
/// диск рабочей копии. Монтирование и размонтирование принадлежат оператору.
pub(super) async fn enospc(env: &mut Env<'_>) -> Result<()> {
    use std::io::Write;
    let volume = std::path::absolute(
        env.ctx
            .fault_volume
            .as_ref()
            .ok_or("нужен --fault-volume")?,
    )
    .map_err(|e| e.to_string())?;
    if volume
        .symlink_metadata()
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
        || !std::process::Command::new("mountpoint")
            .arg("-q")
            .arg(&volume)
            .status()
            .is_ok_and(|s| s.success())
    {
        return Err(
            "fault-volume должен быть самостоятельной точкой монтирования без символической ссылки"
                .into(),
        );
    }
    let stat = std::process::Command::new("stat")
        .args(["-f", "--format=%T:%S:%b"])
        .arg(&volume)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&stat.stdout);
    let parts: Vec<_> = text.trim().split(':').collect();
    let capacity = parts
        .get(1)
        .and_then(|s| s.parse::<u64>().ok())
        .zip(parts.get(2).and_then(|s| s.parse::<u64>().ok()))
        .and_then(|(a, b)| a.checked_mul(b));
    if !stat.status.success()
        || parts.first() != Some(&"tmpfs")
        || !capacity.is_some_and(|n| n > 0 && n <= 64 * 1024 * 1024)
    {
        return Err("fault-volume должен быть отдельным tmpfs с ёмкостью <=64 MiB; обычный диск не заполняется".into());
    }
    if std::fs::read_dir(&volume)
        .map_err(|e| e.to_string())?
        .next()
        .is_some()
    {
        return Err("fault-volume не пуст; чужие файлы не изменяются".into());
    }
    env.out
        .evidence
        .insert("volume_capacity_bytes".into(), json!(capacity));
    let mut cli = Cli::new(env).ok_or(MISSING_CLI)?;
    cli.store = volume.join("store");
    let words = [
        (fixture::WORD, fixture::READING),
        (fixture::WORD_3, fixture::READING_3),
    ];
    let plan = cli.plan(&env.dir, "space-plan", &words)?;
    let fixture = env.ctx.fixture.clone();
    let (gate, rx) = Gate::new();
    fixture.set(fixture::RECORD_3, Behavior::HoldBody(rx));
    let child = cli.spawn(
        true,
        &[
            "batch-start",
            "--batch-id",
            "space",
            "--plan",
            &plan.to_string_lossy(),
            "--close-tab",
        ],
    )?;
    if !fixture
        .wait_request(fixture::RECORD_3, 1, Duration::from_secs(60))
        .await
    {
        return Err("не началась загрузка перед ENOSPC".into());
    }
    let state = cli.store.join(".runtime/batches/space/state.json");
    let before = std::fs::read(&state).map_err(|e| e.to_string())?;
    let reservation = volume.join("reservation");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&reservation)
        .map_err(|e| e.to_string())?;
    let no_space = loop {
        match file.write_all(&[0_u8; 4096]) {
            Ok(()) => (),
            Err(e) => break e,
        }
    };
    drop(file);
    if no_space.raw_os_error() != Some(28) {
        let _ = std::fs::remove_file(&reservation);
        return Err(format!("ожидался ENOSPC, получено {no_space}"));
    }
    gate.open();
    let failed = Cli::finish(child).await;
    // Освобождаем место и при ошибке наблюдения/таймауте CLI.
    std::fs::remove_file(&reservation).map_err(|e| e.to_string())?;
    let failed = failed?;
    let after = std::fs::read(&state).map_err(|e| e.to_string())?;
    env.out.check("настоящий ENOSPC не уничтожает подтверждённый checkpoint",failed.code!=0 && before==after,json!({"capacity_bytes":capacity,"fill_errno":no_space.raw_os_error(),"exit":failed.code,"state_unchanged":before==after,"error":failed.out["error"]}));
    fixture.set(fixture::RECORD_3, Behavior::Normal);
    let resumed = cli
        .run(
            true,
            &["batch-resume", "--batch-id", "space", "--close-tab"],
        )
        .await?;
    env.out.check("после освобождения места первая запись сохранена, вторая повторяется явно",resumed.code==0 && fixture.requests_for(fixture::RECORD_A)==1 && fixture.requests_for(fixture::RECORD_3)==2,json!({"exit":resumed.code,"downloads_first":fixture.requests_for(fixture::RECORD_A),"downloads_second":fixture.requests_for(fixture::RECORD_3),"state":resumed.out["result"]}));
    env.out.evidence.insert(
        "state_before_sha256".into(),
        json!(crate::util::sha256_hex(&before)),
    );
    env.out.evidence.insert(
        "state_after_failure_sha256".into(),
        json!(crate::util::sha256_hex(&after)),
    );
    // Штатный browser report сохраняет свидетельства на диске прогона, а том
    // снова пуст для следующего вызова benchmark-runner. Удаляется только store,
    // созданный здесь после проверки пустой отдельной tmpfs.
    let copied = std::process::Command::new("cp")
        .args(["-a", "--"])
        .arg(&cli.store)
        .arg(env.dir.join("store-evidence"))
        .status()
        .map_err(|e| e.to_string())?;
    if !copied.success() {
        return Err("не удалось сохранить store ENOSPC в каталоге прогона".into());
    }
    std::fs::remove_dir_all(&cli.store).map_err(|e| e.to_string())?;
    env.out.check(
        "ограниченный том снова пуст, свидетельства сохранены",
        std::fs::read_dir(&volume)
            .map_err(|e| e.to_string())?
            .next()
            .is_none(),
        json!({"evidence":"store-evidence"}),
    );
    env.connect().await?;
    env.residual_clean().await
}
