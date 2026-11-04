//! Среда хранения каталога прогона для фактов окружения `benchmark-runner`.
//!
//! Время импорта `AssetStore` определяется в первую очередь накопителем, поэтому
//! отчёт различает два происхождения его описания:
//!
//! - `observed` — стенд сам прочитал модель накопителя в sysfs, и ядро работает
//!   не в виртуальной машине: блочное устройство гостя и есть физическое;
//! - `operator_declared` — накопитель не наблюдаем (виртуальная машина, в том
//!   числе WSL2, overlay контейнера, сетевая ФС, петлевое устройство) и описан
//!   оператором флагом `--declared-storage`. Это заявление, а не доказательство.
//!
//! Наблюдаемое описание гостевой среды (`guest`) записывается всегда, когда
//! его можно прочитать, независимо от происхождения физического описания.

use super::fault_volume::{MountEntry, parse_line};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Предел длины заявления оператора: это короткое имя накопителя, а не текст.
pub(super) const MAX_DECLARED_CHARS: usize = 120;

/// Проверка значения `--declared-storage`. Пустое значение означает «не заявлено»:
/// так исполнитель серии передаёт параметр по умолчанию.
pub(super) fn parse_declared(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.chars().count() > MAX_DECLARED_CHARS {
        return Err(format!(
            "заявленный накопитель длиннее {MAX_DECLARED_CHARS} символов"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("заявленный накопитель содержит управляющие символы".into());
    }
    Ok(value.to_owned())
}

/// Физическое устройство, наблюдаемое через sysfs.
#[derive(Debug, Clone, PartialEq)]
struct Disk {
    vendor: Option<String>,
    model: String,
    rotational: Option<bool>,
}

impl Disk {
    fn describe(&self) -> String {
        let name = match &self.vendor {
            Some(vendor) => format!("{vendor} {}", self.model),
            None => self.model.clone(),
        };
        match self.rotational {
            Some(true) => format!("{name} · rotational=1"),
            Some(false) => format!("{name} · rotational=0"),
            None => name,
        }
    }
}

/// Что видно изнутри ядра, на котором работает стенд.
#[derive(Debug, Clone, PartialEq)]
enum Guest {
    /// ФС в оперативной памяти.
    Memory { fs_type: String },
    /// ФС на наблюдаемых блочных устройствах.
    Block { fs_type: String, disks: Vec<Disk> },
}

impl Guest {
    fn describe(&self) -> String {
        match self {
            Guest::Memory { fs_type } => format!("{fs_type} (ОЗУ)"),
            Guest::Block { fs_type, disks } => format!(
                "{fs_type}: {}",
                disks
                    .iter()
                    .map(Disk::describe)
                    .collect::<Vec<_>>()
                    .join(" + ")
            ),
        }
    }

    fn physical(&self) -> String {
        match self {
            Guest::Memory { .. } => "ОЗУ".to_owned(),
            Guest::Block { disks, .. } => disks
                .iter()
                .map(Disk::describe)
                .collect::<Vec<_>>()
                .join(" + "),
        }
    }
}

/// Работает ли ядро в виртуальной машине.
#[derive(Debug, Clone, PartialEq)]
enum Machine {
    /// `systemd-detect-virt --vm` ответил `none`, и у CPU нет флага `hypervisor`.
    Bare,
    Virtual(String),
    /// Отсутствие гипервизора не доказано.
    Unknown(String),
}

fn machine() -> Machine {
    let hypervisor_flag = std::fs::read_to_string("/proc/cpuinfo").is_ok_and(|text| {
        text.lines()
            .any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor"))
    });
    let detected = std::process::Command::new("systemd-detect-virt")
        .arg("--vm")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|text| !text.is_empty());
    classify_machine(detected.as_deref(), hypervisor_flag)
}

fn classify_machine(detected: Option<&str>, hypervisor_flag: bool) -> Machine {
    match (detected, hypervisor_flag) {
        (Some("none"), false) => Machine::Bare,
        (Some("none") | None, true) => Machine::Virtual("флаг CPU hypervisor".into()),
        (Some(name), _) => Machine::Virtual(name.to_owned()),
        (None, false) => Machine::Unknown(
            "systemd-detect-virt недоступен: отсутствие гипервизора не доказано".into(),
        ),
    }
}

/// Монтирование, которому принадлежит `path`: самая длинная точка монтирования,
/// которая является префиксом пути по компонентам; при наложении — последняя.
fn mount_for<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    entries
        .iter()
        .filter(|e| path.starts_with(&e.mount_point))
        .max_by_key(|e| Path::new(&e.mount_point).components().count())
        .and_then(|longest| {
            entries
                .iter()
                .rev()
                .find(|e| e.mount_point == longest.mount_point)
        })
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// Физические диски под блочным устройством `device`: раздел поднимается к
/// диску, составное устройство (device-mapper, md) раскрывается по `slaves`.
fn disks(sys: &Path, device: (u32, u32)) -> Result<Vec<Disk>, String> {
    let node = sys
        .join("dev/block")
        .join(format!("{}:{}", device.0, device.1));
    let node = std::fs::canonicalize(&node).map_err(|_| {
        format!(
            "блочное устройство {}:{} не найдено в sysfs",
            device.0, device.1
        )
    })?;
    let mut found = Vec::new();
    collect_disks(&node, &mut found, 0)?;
    Ok(found)
}

fn collect_disks(node: &Path, found: &mut Vec<Disk>, depth: u8) -> Result<(), String> {
    if depth > 8 {
        return Err("цепочка составных устройств слишком глубока".into());
    }
    let disk: PathBuf = if node.join("partition").exists() {
        node.parent()
            .ok_or("у раздела нет родительского диска")?
            .to_owned()
    } else {
        node.to_owned()
    };
    let name = disk
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if disk.join("loop").exists() {
        return Err(format!(
            "{name} — петлевое устройство: его носитель — файл на другой ФС"
        ));
    }
    // Порядок обхода фиксирован: факт обязан совпадать между прогонами.
    let mut slaves: Vec<PathBuf> = std::fs::read_dir(disk.join("slaves"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    slaves.sort();
    if !slaves.is_empty() {
        for slave in slaves {
            let slave = std::fs::canonicalize(&slave).map_err(|e| format!("{name}: {e}"))?;
            collect_disks(&slave, found, depth + 1)?;
        }
        return Ok(());
    }
    let model = read_trimmed(&disk.join("device/model"))
        .ok_or_else(|| format!("у устройства {name} нет модели в sysfs"))?;
    let disk = Disk {
        vendor: read_trimmed(&disk.join("device/vendor"))
            .filter(|v| v != "ATA" && !model.starts_with(v.as_str())),
        model,
        rotational: read_trimmed(&disk.join("queue/rotational")).and_then(|r| match r.as_str() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        }),
    };
    if !found.contains(&disk) {
        found.push(disk);
    }
    Ok(())
}

/// Гостевая среда хранения пути по mountinfo и sysfs.
fn guest(mountinfo: &str, sys: &Path, path: &Path) -> Result<Guest, String> {
    let entries: Vec<MountEntry> = mountinfo.lines().filter_map(parse_line).collect();
    let entry = mount_for(&entries, path)
        .ok_or_else(|| format!("{}: монтирование не найдено в mountinfo", path.display()))?;
    let fs_type = entry.fs_type.clone();
    if matches!(fs_type.as_str(), "tmpfs" | "ramfs") {
        return Ok(Guest::Memory { fs_type });
    }
    // У btrfs и подобных ФС устройство в mountinfo анонимное (major 0):
    // настоящее берётся из узла-источника.
    let device = if entry.device.0 != 0 {
        Some(entry.device)
    } else {
        source_device(&entry.source)
    };
    let Some(device) = device else {
        return Err(format!(
            "ФС {fs_type} ({}) не опирается на видимое блочное устройство",
            entry.source
        ));
    };
    Ok(Guest::Block {
        fs_type,
        disks: disks(sys, device)?,
    })
}

fn source_device(source: &str) -> Option<(u32, u32)> {
    if !source.starts_with("/dev/") {
        return None;
    }
    let stat = rustix::fs::stat(source).ok()?;
    let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
    (kind == rustix::fs::FileType::BlockDevice).then(|| {
        (
            rustix::fs::major(stat.st_rdev),
            rustix::fs::minor(stat.st_rdev),
        )
    })
}

/// Раздел `storage` отчёта для каталога `path`.
pub(super) fn describe(path: &Path, declared: Option<&str>) -> Value {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let observed = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("/proc/self/mountinfo: {e}"))
        .and_then(|info| guest(&info, Path::new("/sys"), &path));
    assemble(observed, machine(), declared)
}

fn assemble(observed: Result<Guest, String>, machine: Machine, declared: Option<&str>) -> Value {
    let declared = declared.map(str::trim).filter(|d| !d.is_empty());
    let unobservable = match (&observed, &machine) {
        (Ok(Guest::Memory { .. }), _) | (Ok(_), Machine::Bare) => None,
        (Err(reason), _) => Some(reason.clone()),
        (Ok(_), Machine::Virtual(name)) => Some(format!(
            "ядро работает в виртуальной машине ({name}): гость видит только виртуальное устройство"
        )),
        (Ok(_), Machine::Unknown(reason)) => Some(reason.clone()),
    };
    let virtualization = match &machine {
        Machine::Bare => json!("none"),
        Machine::Virtual(name) => json!(name),
        Machine::Unknown(_) => Value::Null,
    };
    let (guest, guest_reason) = match &observed {
        Ok(guest) => (json!(guest.describe()), Value::Null),
        Err(reason) => (Value::Null, json!(reason)),
    };
    let (provenance, physical, physical_reason) = match (&observed, unobservable, declared) {
        (Ok(guest), None, _) => (json!("observed"), json!(guest.physical()), Value::Null),
        (_, Some(reason), Some(declared)) => (
            json!("operator_declared"),
            json!(declared),
            json!(format!(
                "{reason}; накопитель описан оператором и не подтверждён стендом"
            )),
        ),
        (_, Some(reason), None) => (
            Value::Null,
            Value::Null,
            json!(format!(
                "{reason}; укажите физический накопитель флагом --declared-storage"
            )),
        ),
        (Err(_), None, _) => unreachable!("ошибка наблюдения всегда даёт причину"),
    };
    json!({
        "guest": guest,
        "guest_reason": guest_reason,
        "virtualization": virtualization,
        "provenance": provenance,
        "physical": physical,
        "physical_reason": physical_reason,
        "declared": declared,
        "declared_used": provenance == "operator_declared",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sys(PathBuf);
    impl Sys {
        fn create(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "anki-e2e-storage-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(path.join("dev/block")).unwrap();
            Self(path)
        }
        /// Диск с разделом `<name>1` и узлами `dev/block` для обоих.
        fn disk(&self, name: &str, major: u32, model: &str, rotational: &str) -> PathBuf {
            let disk = self.0.join("devices").join(name);
            std::fs::create_dir_all(disk.join("device")).unwrap();
            std::fs::create_dir_all(disk.join("queue")).unwrap();
            std::fs::write(disk.join("device/model"), format!("{model}    \n")).unwrap();
            std::fs::write(disk.join("queue/rotational"), rotational).unwrap();
            let part = disk.join(format!("{name}1"));
            std::fs::create_dir_all(&part).unwrap();
            std::fs::write(part.join("partition"), "1").unwrap();
            std::os::unix::fs::symlink(&disk, self.0.join(format!("dev/block/{major}:0"))).unwrap();
            std::os::unix::fs::symlink(&part, self.0.join(format!("dev/block/{major}:1"))).unwrap();
            disk
        }
    }
    impl Drop for Sys {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MOUNTS: &str = "\
22 1 259:1 / / rw - ext4 /dev/nvme0n1p1 rw
30 22 0:40 / /tmp rw - tmpfs tmpfs rw
31 22 0:41 / /work rw - overlay overlay rw
32 22 0:42 / /data rw - xfs /dev/mapper/vg-data rw
";

    #[test]
    fn partition_resolves_to_the_disk_model() {
        let sys = Sys::create("partition");
        sys.disk("nvme0n1", 259, "Samsung SSD 980 1TB", "0");
        let guest = guest(MOUNTS, &sys.0, Path::new("/home/user/run")).unwrap();
        assert_eq!(guest.describe(), "ext4: Samsung SSD 980 1TB · rotational=0");
        assert_eq!(guest.physical(), "Samsung SSD 980 1TB · rotational=0");
    }

    #[test]
    fn composite_device_lists_every_physical_leaf() {
        let sys = Sys::create("dm");
        let a = sys.disk("sda", 8, "HDD A", "1");
        let b = sys.disk("sdb", 9, "SSD B", "0");
        let dm = sys.0.join("devices/dm-0");
        std::fs::create_dir_all(dm.join("slaves")).unwrap();
        std::os::unix::fs::symlink(a.join("sda1"), dm.join("slaves/sda1")).unwrap();
        std::os::unix::fs::symlink(b.join("sdb1"), dm.join("slaves/sdb1")).unwrap();
        std::os::unix::fs::symlink(&dm, sys.0.join("dev/block/253:0")).unwrap();
        let info = "32 22 253:0 / /data rw - xfs /dev/mapper/vg-data rw\n";
        let guest = guest(info, &sys.0, Path::new("/data/run")).unwrap();
        assert_eq!(
            guest.describe(),
            "xfs: HDD A · rotational=1 + SSD B · rotational=0"
        );
    }

    #[test]
    fn loop_overlay_and_missing_model_are_not_observable() {
        let sys = Sys::create("loop");
        let lo = sys.0.join("devices/loop0");
        std::fs::create_dir_all(lo.join("loop")).unwrap();
        std::os::unix::fs::symlink(&lo, sys.0.join("dev/block/7:0")).unwrap();
        let info = "40 22 7:0 / /img rw - ext4 /dev/loop0 rw\n";
        let error = guest(info, &sys.0, Path::new("/img/run")).unwrap_err();
        assert!(error.contains("петлевое"), "{error}");

        let error = guest(MOUNTS, &sys.0, Path::new("/work/run")).unwrap_err();
        assert!(error.contains("overlay"), "{error}");

        let vd = sys.0.join("devices/vda");
        std::fs::create_dir_all(&vd).unwrap();
        std::os::unix::fs::symlink(&vd, sys.0.join("dev/block/252:0")).unwrap();
        let info = "41 22 252:0 / /vm rw - ext4 /dev/vda rw\n";
        let error = guest(info, &sys.0, Path::new("/vm")).unwrap_err();
        assert!(error.contains("нет модели"), "{error}");
    }

    #[test]
    fn nested_and_overmounted_points_pick_the_effective_mount() {
        let entries: Vec<MountEntry> = format!("{MOUNTS}50 30 0:60 / /tmp rw - ext4 /dev/sdz rw\n")
            .lines()
            .filter_map(parse_line)
            .collect();
        assert_eq!(
            mount_for(&entries, Path::new("/tmp/x")).unwrap().device,
            (0, 60)
        );
        assert_eq!(
            mount_for(&entries, Path::new("/tmpx")).unwrap().mount_point,
            "/"
        );
    }

    #[test]
    fn machine_is_bare_only_when_both_signals_agree() {
        assert_eq!(classify_machine(Some("none"), false), Machine::Bare);
        assert!(matches!(
            classify_machine(Some("none"), true),
            Machine::Virtual(_)
        ));
        assert_eq!(
            classify_machine(Some("microsoft"), true),
            Machine::Virtual("microsoft".into())
        );
        assert!(matches!(classify_machine(None, true), Machine::Virtual(_)));
        assert!(matches!(classify_machine(None, false), Machine::Unknown(_)));
    }

    fn block() -> Guest {
        Guest::Block {
            fs_type: "ext4".into(),
            disks: vec![Disk {
                vendor: Some("Msft".into()),
                model: "Virtual Disk".into(),
                rotational: Some(true),
            }],
        }
    }

    #[test]
    fn bare_metal_is_observed_and_a_declaration_does_not_replace_it() {
        let report = assemble(Ok(block()), Machine::Bare, Some("NVMe X"));
        assert_eq!(report["provenance"], "observed");
        assert_eq!(report["physical"], "Msft Virtual Disk · rotational=1");
        assert_eq!(report["declared"], "NVMe X");
        assert_eq!(report["declared_used"], false);
    }

    #[test]
    fn virtual_machine_needs_a_declaration_and_keeps_the_guest_observation() {
        let missing = assemble(Ok(block()), Machine::Virtual("microsoft".into()), None);
        assert!(missing["provenance"].is_null() && missing["physical"].is_null());
        assert_eq!(missing["guest"], "ext4: Msft Virtual Disk · rotational=1");
        assert!(
            missing["physical_reason"]
                .as_str()
                .unwrap()
                .contains("--declared-storage")
        );
        // Пустое значение по умолчанию исполнителя серии — не заявление.
        let empty = assemble(Ok(block()), Machine::Virtual("kvm".into()), Some("  "));
        assert!(empty["physical"].is_null());

        let declared = assemble(
            Ok(block()),
            Machine::Virtual("microsoft".into()),
            Some("Samsung SSD 980 1TB"),
        );
        assert_eq!(declared["provenance"], "operator_declared");
        assert_eq!(declared["physical"], "Samsung SSD 980 1TB");
        assert_eq!(declared["guest"], "ext4: Msft Virtual Disk · rotational=1");
        assert_eq!(declared["declared_used"], true);
    }

    #[test]
    fn unproven_bare_metal_and_unobservable_guest_use_only_the_declaration() {
        let unknown = assemble(
            Ok(block()),
            Machine::Unknown("нет systemd-detect-virt".into()),
            None,
        );
        assert!(unknown["physical"].is_null());
        let overlay = assemble(Err("overlay".into()), Machine::Bare, Some("NVMe X"));
        assert_eq!(overlay["provenance"], "operator_declared");
        assert!(overlay["guest"].is_null());
        assert_eq!(overlay["guest_reason"], "overlay");
    }

    #[test]
    fn memory_filesystem_is_observed_even_inside_a_vm() {
        let report = assemble(
            Ok(Guest::Memory {
                fs_type: "tmpfs".into(),
            }),
            Machine::Virtual("kvm".into()),
            None,
        );
        assert_eq!(report["provenance"], "observed");
        assert_eq!(report["physical"], "ОЗУ");
    }

    #[test]
    fn declaration_is_short_single_line_text() {
        assert_eq!(parse_declared("  NVMe X ").unwrap(), "NVMe X");
        assert_eq!(parse_declared("").unwrap(), "");
        assert!(parse_declared("a\nb").is_err());
        assert!(parse_declared(&"x".repeat(MAX_DECLARED_CHARS + 1)).is_err());
    }

    #[test]
    fn current_run_directory_is_described_without_panicking() {
        let report = describe(&std::env::temp_dir(), None);
        assert!(report["guest"].is_string() || report["guest_reason"].is_string());
        assert!(report["physical"].is_string() || report["physical_reason"].is_string());
    }
}
