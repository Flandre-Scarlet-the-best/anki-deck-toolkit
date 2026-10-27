//! Изоляция офлайн-серии: новое сетевое пространство имён без интерфейсов,
//! кроме выключенного loopback.
//!
//! Пространство создаёт `unshare --user --map-current-user --net` (util-linux)
//! без root: непривилегированное user namespace, в котором UID процесса
//! остаётся прежним. Изоляция подтверждается пробой: внутри должен быть виден
//! только `lo`. Она охватывает процессы серии, но не демонов вне её — Docker
//! и BuildKit работают в сети хоста, поэтому сценарий, которому нужен Docker,
//! офлайн-изоляции этим способом не получает.

use crate::hyperfine_exec::which;
use crate::procs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const ARGS: [&str; 4] = ["--user", "--map-current-user", "--net", "--"];

#[derive(Debug, Clone)]
pub struct Isolation {
    pub unshare: PathBuf,
}

/// Проверить изоляцию действием: запустить пробу в новом пространстве имён.
pub fn probe(runner: &std::path::Path) -> Result<Isolation, String> {
    let unshare = which("unshare")
        .ok_or("unshare (util-linux) не найден: сетевое пространство имён без root не создаётся")?;
    let mut command = Command::new(&unshare);
    command.args(ARGS).arg(runner).arg("__net-probe");
    match procs::output(&mut command, Duration::from_secs(10)) {
        Ok((procs::Ending::Exited(0), out, _)) => {
            let interfaces = String::from_utf8_lossy(&out).trim().to_owned();
            if interfaces == "lo" {
                Ok(Isolation { unshare })
            } else {
                Err(format!(
                    "в новом сетевом пространстве видны интерфейсы «{interfaces}»: изоляция не подтверждена"
                ))
            }
        }
        Ok((ending, _, err)) => Err(format!(
            "непривилегированное сетевое пространство имён не создаётся ({}): {}; возможно, user namespaces запрещены (kernel.unprivileged_userns_clone, AppArmor) или окружение-песочница их не допускает",
            ending.describe(),
            String::from_utf8_lossy(&err).trim()
        )),
        Err(error) => Err(format!("unshare не запускается: {error}")),
    }
}

impl Isolation {
    /// argv, исполняемый внутри изолированного пространства.
    pub fn wrap(&self, argv: Vec<String>) -> Vec<String> {
        let mut wrapped = vec![self.unshare.to_string_lossy().into_owned()];
        wrapped.extend(ARGS.iter().map(|a| (*a).to_owned()));
        wrapped.extend(argv);
        wrapped
    }
}

/// Тело пробы: перечислить сетевые интерфейсы текущего пространства.
/// `/sys/class/net` показывает пространство того, кто смонтировал sysfs,
/// поэтому источник — `/proc/self/net/dev`.
pub fn list_interfaces() -> String {
    let mut names: Vec<String> = std::fs::read_to_string("/proc/self/net/dev")
        .map(|text| {
            text.lines()
                .skip(2)
                .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.join(",")
}
