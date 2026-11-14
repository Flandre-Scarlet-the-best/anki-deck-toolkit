//! Детерминированные отказы настоящего CLI над изолированным store.
//! Байты состояния создаёт только штатный путь CLI; тест портит/закрывает доступ
//! к уже созданному временному файлу как внешний отказ и проверяет постусловия.

use super::Ctx;
use super::cli_scenarios::{Cli, MISSING_CLI, signal};
use super::fault_volume::FaultVolume;
use super::fixture::{self, Behavior, Gate};
use super::scenarios::{Env, Result};
use rustix::fs::{Access, AtFlags};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Capability, при которых биты режима не создают EACCES (capabilities(7)).
const DAC_BYPASS: [(u32, &str); 2] = [(1, "CAP_DAC_OVERRIDE"), (2, "CAP_DAC_READ_SEARCH")];

/// Capability обхода DAC в наборах `CapEff`/`CapAmb` из `/proc/<pid>/status`.
/// Набор, который не удалось разобрать, считается привилегированным.
fn dac_bypass(status: &str) -> Vec<String> {
    let mut found = Vec::new();
    for set in ["CapEff", "CapAmb"] {
        let mask = status
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{set}:")))
            .and_then(|v| u64::from_str_radix(v.trim(), 16).ok());
        match mask {
            Some(mask) => found.extend(
                DAC_BYPASS
                    .iter()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, name)| format!("{set}:{name}")),
            ),
            None => found.push(format!("{set}:не определён")),
        }
    }
    found
}

/// `cli_batch_faults` моделирует EACCES битами режима, поэтому запускается
/// только без обхода DAC у стенда и у CLI; проверка до любых изменений состояния.
pub(super) fn prerequisite(ctx: &Ctx) -> Option<String> {
    let mut reasons = Vec::new();
    if rustix::process::geteuid().is_root() {
        reasons.push("эффективный UID 0".to_owned());
    }
    match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => reasons.extend(dac_bypass(&status)),
        Err(e) => reasons.push(format!("/proc/self/status: {e}")),
    }
    if let Some(cli) = &ctx.cli {
        if let Ok(meta) = cli.metadata()
            && meta.permissions().mode() & 0o6000 != 0
        {
            reasons.push("CLI с битом setuid/setgid".into());
        }
        if rustix::fs::getxattr(cli, "security.capability", &mut [0_u8; 64]).is_ok() {
            reasons.push("CLI с файловыми capability".into());
        }
    }
    (!reasons.is_empty()).then(|| {
        format!(
            "запрет прав битами режима не создаёт EACCES при обходе DAC ({}); запустите стенд обычным пользователем",
            reasons.join(", ")
        )
    })
}

/// Временный запрет доступа битами режима с восстановлением в `Drop`. После
/// смены режима отказ подтверждается `faccessat(AT_EACCESS)`: если доступ всё
/// ещё разрешён, режим восстанавливается и сценарий не засчитывается.
struct Permissions(Vec<(PathBuf, std::fs::Permissions)>);
impl Permissions {
    fn deny(paths: &[&Path], mode: u32, denied: Access) -> Result<Self> {
        let mut guard = Self(Vec::new());
        for path in paths {
            let old = path.metadata().map_err(|e| e.to_string())?.permissions();
            guard.0.push((path.to_path_buf(), old));
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
            match rustix::fs::accessat(rustix::fs::CWD, *path, denied, AtFlags::EACCESS) {
                Err(rustix::io::Errno::ACCESS) => (),
                Ok(()) => {
                    return Err(format!(
                        "запрет доступа к {} не действует (обход DAC); отказ не смоделирован",
                        path.display()
                    ));
                }
                Err(e) => return Err(format!("проверка запрета {}: {e}", path.display())),
            }
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
    let permissions = Permissions::deny(&[&root], 0o500, Access::WRITE_OK)?;
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
    // Внешнее повреждение: штатный SafeBatchRuntime должен отказать до получения/импорта.
    let permissions = Permissions::deny(&[&blob], 0o000, Access::READ_OK)?;
    let unreadable = corrupt
        .run(true, &["batch-resume", "--batch-id", "corrupt"])
        .await;
    drop(permissions);
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
        {"case":"SIGKILL внутри fsync/rename checkpoint или CAS-импорта","status":"NOT_TESTED","reason":"в этом сценарии не проверяется; точные окна fsync покрывает отдельный, запускаемый только явно cli_batch_crash_windows (GDB + strace)"}
    ]));
    env.connect().await?;
    env.residual_clean().await
}

/// Заполняется только явно переданная выделенная ограниченная tmpfs, никогда
/// диск рабочей копии. Монтирование и размонтирование принадлежат оператору;
/// владение томом, проверки и очистка — `FaultVolume`.
pub(super) async fn enospc(env: &mut Env<'_>) -> Result<()> {
    let requested = env
        .ctx
        .fault_volume
        .as_ref()
        .ok_or("нужен --fault-volume")?;
    let mut volume = FaultVolume::acquire(requested, env.dir.join("store-evidence"))?;
    let capacity = volume.capacity();
    env.out
        .evidence
        .insert("volume_capacity_bytes".into(), json!(capacity));
    let mut cli = Cli::new(env).ok_or(MISSING_CLI)?;
    cli.store = volume.store();
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
    volume.track(&child)?;
    if !fixture
        .wait_request(fixture::RECORD_3, 1, Duration::from_secs(60))
        .await
    {
        return Err("не началась загрузка перед ENOSPC".into());
    }
    let state = cli.store.join(".runtime/batches/space/state.json");
    let before = std::fs::read(&state).map_err(|e| e.to_string())?;
    // При ошибке, отмене асинхронной операции или таймауте reservation и store убирает `Drop`.
    let no_space = volume.fill()?;
    if no_space.raw_os_error() != Some(28) {
        return Err(format!("ожидался ENOSPC, получено {no_space}"));
    }
    gate.open();
    let failed = Cli::finish(child).await;
    // Освобождаем место и при ошибке наблюдения/таймауте CLI.
    volume.release_reservation()?;
    let failed = failed?;
    let after = std::fs::read(&state).map_err(|e| e.to_string())?;
    env.out.check("настоящий ENOSPC не уничтожает подтверждённый checkpoint",failed.code!=0 && before==after,json!({"capacity_bytes":capacity,"fill_errno":no_space.raw_os_error(),"exit":failed.code,"state_unchanged":before==after,"error":failed.out["error"]}));
    fixture.set(fixture::RECORD_3, Behavior::Normal);
    let child = cli.spawn(
        true,
        &["batch-resume", "--batch-id", "space", "--close-tab"],
    )?;
    volume.track(&child)?;
    let resumed = Cli::finish(child).await?;
    env.out.check("после освобождения места первая запись сохранена, вторая повторяется явно",resumed.code==0 && fixture.requests_for(fixture::RECORD_A)==1 && fixture.requests_for(fixture::RECORD_3)==2,json!({"exit":resumed.code,"downloads_first":fixture.requests_for(fixture::RECORD_A),"downloads_second":fixture.requests_for(fixture::RECORD_3),"state":resumed.out["result"]}));
    env.out.evidence.insert(
        "state_before_sha256".into(),
        json!(crate::util::sha256_hex(&before)),
    );
    env.out.evidence.insert(
        "state_after_failure_sha256".into(),
        json!(crate::util::sha256_hex(&after)),
    );
    // Штатный отчёт браузерного набора сохраняет свидетельства на диске прогона, а том
    // снова пуст для следующего вызова benchmark-runner.
    volume.save_and_clear_store()?;
    env.out.check(
        "ограниченный том снова пуст, свидетельства сохранены",
        volume.is_empty()?,
        json!({"evidence":"store-evidence"}),
    );
    drop(volume);
    env.connect().await?;
    env.residual_clean().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dac_bypass_is_read_from_effective_and_ambient_sets() {
        let plain = "Name:\tx\nCapInh:\t0000000000000000\nCapEff:\t0000000000000000\nCapAmb:\t0000000000000000\n";
        assert!(dac_bypass(plain).is_empty());
        let root = "CapEff:\t000001ffffffffff\nCapAmb:\t0000000000000000\n";
        assert_eq!(
            dac_bypass(root),
            ["CapEff:CAP_DAC_OVERRIDE", "CapEff:CAP_DAC_READ_SEARCH"]
        );
        let ambient = "CapEff:\t0000000000000004\nCapAmb:\t0000000000000002\n";
        assert_eq!(
            dac_bypass(ambient),
            ["CapEff:CAP_DAC_READ_SEARCH", "CapAmb:CAP_DAC_OVERRIDE"]
        );
        // Другие capability (CAP_CHOWN, CAP_FOWNER) EACCES не отменяют.
        let other = "CapEff:\t0000000000000009\nCapAmb:\t0000000000000000\n";
        assert!(dac_bypass(other).is_empty());
        assert_eq!(
            dac_bypass("CapEff:\tzz\n"),
            ["CapEff:не определён", "CapAmb:не определён"]
        );
    }

    #[test]
    fn denied_permissions_are_proven_or_refused_and_always_restored() {
        let dir = std::env::temp_dir().join(format!("anki-e2e-faults-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let privileged = rustix::process::geteuid().is_root()
            || !dac_bypass(&std::fs::read_to_string("/proc/self/status").unwrap()).is_empty();
        let result = Permissions::deny(&[&dir], 0o500, Access::WRITE_OK);
        // Под обходом DAC запрет не засчитывается; без него отказ доказан.
        assert_eq!(result.is_ok(), !privileged, "{:?}", result.as_ref().err());
        drop(result);
        let mode = dir.metadata().unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(mode, 0o700);
    }
}
