use super::environment::Target;
use super::{
    COMPATIBILITY, CONNECT, CROWD, PATCH_VERSION, PatchAction, Result, error, external_error,
    io_error, read_json,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const BOOTSTRAP: &str = concat!(include_str!("bootstrap.py"), "\n");
const ANCHOR: &str = "if __name__ != \"plugin\":";
const PAYLOAD: &str = include_str!("payload.py");

pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hash_file(path: &Path) -> Result<String> {
    check_ancestors(path)?;
    if !fs::symlink_metadata(path).map_err(io_error)?.is_file() {
        return Err(error("unsafe_path", "Ожидается обычный файл для SHA-256"));
    }
    let mut file = fs::File::open(path).map_err(io_error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn tree_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        check_ancestors(dir)?;
        for entry in fs::read_dir(dir).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
            if metadata.file_type().is_symlink() {
                return Err(error("unsafe_path", "Ссылка в spool задания"));
            }
            if metadata.is_dir() {
                visit(root, &path, files)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(io_error)?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, hash_file(&path)?);
            } else {
                return Err(error("unsafe_path", "Специальный файл в spool задания"));
            }
        }
        Ok(())
    }

    check_ancestors(root)?;
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn valid_job_id(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Копирует задания старой версии в постоянное состояние Anki до backup.
/// Исходник остаётся на месте до успешной установки нового payload.
fn migrate_legacy_spool(t: &Target) -> Result<()> {
    let legacy = t.bridge().join("spool");
    let metadata = match fs::symlink_metadata(&legacy) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(e)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(error(
            "unsafe_path",
            "Старый spool должен быть обычным каталогом",
        ));
    }
    check_ancestors(&legacy)?;

    let mut legacy_jobs = Vec::new();
    for entry in fs::read_dir(&legacy).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| error("unsafe_path", "Не-UTF-8 имя задания в старом spool"))?;
        let job_metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if !valid_job_id(&name) || job_metadata.file_type().is_symlink() || !job_metadata.is_dir() {
            return Err(error("unsafe_path", "Недопустимая запись в старом spool"));
        }
        legacy_jobs.push((name, tree_hashes(&entry.path())?));
    }
    if legacy_jobs.is_empty() {
        fs::remove_dir(&legacy).map_err(io_error)?;
        return Ok(());
    }

    let current = t.state().join("spool");
    let current_exists = match fs::symlink_metadata(&current) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(error(
                    "unsafe_path",
                    "Новый spool должен быть обычным каталогом",
                ));
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(io_error(e)),
    };
    if current_exists {
        check_ancestors(&current)?;
        for entry in fs::read_dir(&current).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| error("unsafe_path", "Не-UTF-8 имя задания в новом spool"))?;
            let job_metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
            if !valid_job_id(&name)
                || job_metadata.file_type().is_symlink()
                || !job_metadata.is_dir()
            {
                return Err(error("unsafe_path", "Недопустимая запись в новом spool"));
            }
        }
    }

    for (name, expected) in &legacy_jobs {
        let destination = current.join(name);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || tree_hashes(&destination)? != *expected
                {
                    return Err(error(
                        "spool_migration_conflict",
                        format!("Конфликт данных задания {name}; исходный spool сохранён"),
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
        }
    }

    ensure_private(&t.state())?;
    let stage = t.state().join(format!(".spool-migration-{}", random_id()?));
    ensure_private(&stage)?;
    let result = (|| {
        let mut staged = Vec::new();
        for (name, expected) in &legacy_jobs {
            let destination = current.join(name);
            if destination.exists() {
                continue;
            }
            let staged_job = stage.join(name);
            copy_tree_verified(&legacy.join(name), &staged_job, expected)?;
            if tree_hashes(&staged_job)? != *expected {
                return Err(external_error("spool_migration_verification_failed", name));
            }
            staged.push(name.clone());
        }
        ensure_private(&current)?;
        for name in staged {
            fs::rename(stage.join(&name), current.join(&name)).map_err(io_error)?;
        }
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(&stage).map_err(io_error);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn copy_verified_file(source: &Path, destination: &Path, expected: &str, name: &str) -> Result<()> {
    check_ancestors(source)?;
    check_ancestors(destination)?;
    if !fs::symlink_metadata(source).map_err(io_error)?.is_file() {
        return Err(error("unsafe_path", "Источник должен быть обычным файлом"));
    }
    let mut input = fs::File::open(source).map_err(io_error)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination).map_err(io_error)?;
    let result = (|| {
        copy_stream_verified(&mut input, &mut output, expected, name)?;
        output.sync_all().map_err(io_error)?;
        if hash_file(destination)? != expected {
            return Err(external_error("backup_verification_failed", name));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn copy_stream_verified(
    input: &mut impl Read,
    output: &mut impl Write,
    expected: &str,
    name: &str,
) -> Result<()> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        output.write_all(&buffer[..count]).map_err(io_error)?;
    }
    let actual = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if actual != expected {
        return Err(external_error("backup_verification_failed", name));
    }
    Ok(())
}

/// Потоковая копия с независимым чтением результата до атомарной публикации.
pub(crate) fn copy_file_atomic(source: &Path, destination: &Path) -> Result<String> {
    check_ancestors(source)?;
    check_ancestors(destination)?;
    let expected = hash_file(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| error("unsafe_path", "Нет родителя"))?;
    let temp = parent.join(format!(".toolkit-{}", random_id()?));
    let result = (|| {
        copy_verified_file(source, &temp, &expected, &source.to_string_lossy())?;
        fs::rename(&temp, destination).map_err(io_error)?;
        Ok(expected)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn copy_tree_verified(
    source: &Path,
    destination: &Path,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    fn visit(
        root: &Path,
        source: &Path,
        destination: &Path,
        expected: &BTreeMap<String, String>,
    ) -> Result<()> {
        check_ancestors(source)?;
        ensure_private(destination)?;
        for entry in fs::read_dir(source).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let source_file = entry.path();
            let destination_file = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source_file).map_err(io_error)?;
            if metadata.file_type().is_symlink() {
                return Err(error("unsafe_path", "Ссылка в spool задания"));
            }
            if metadata.is_dir() {
                visit(root, &source_file, &destination_file, expected)?;
            } else if metadata.is_file() {
                let relative = source_file
                    .strip_prefix(root)
                    .map_err(io_error)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let digest = expected.get(&relative).ok_or_else(|| {
                    error(
                        "spool_migration_conflict",
                        "Старый spool изменился при копировании",
                    )
                })?;
                copy_verified_file(&source_file, &destination_file, digest, &relative)?;
            } else {
                return Err(error("unsafe_path", "Специальный файл в spool задания"));
            }
        }
        Ok(())
    }

    visit(source, source, destination, expected)
}

fn remove_legacy_spool_after_upgrade(t: &Target) -> Result<()> {
    let legacy = t.bridge().join("spool");
    if !legacy.exists() {
        return Ok(());
    }
    let current = t.state().join("spool");
    for entry in fs::read_dir(&legacy).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| error("unsafe_path", "Не-UTF-8 имя задания в старом spool"))?;
        if !valid_job_id(&name) {
            return Err(error("unsafe_path", "Недопустимая запись в старом spool"));
        }
        let source = entry.path();
        let destination = current.join(&name);
        if tree_hashes(&source)? != tree_hashes(&destination)? {
            return Err(error(
                "spool_migration_conflict",
                format!("Не удалось подтвердить перенос задания {name}; старый spool сохранён"),
            ));
        }
    }
    fs::remove_dir_all(&legacy).map_err(io_error)
}

pub(crate) fn random_id() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(io_error)?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}
pub(crate) fn ensure_private(path: &Path) -> Result<()> {
    check_ancestors(path)?;
    fs::create_dir_all(path).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}
pub(crate) fn check_ancestors(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(error(
                    "unsafe_path",
                    "Символическая ссылка в управляемом пути",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
        }
    }
    Ok(())
}
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    check_ancestors(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| error("unsafe_path", "Нет родителя"))?;
    let tmp = parent.join(format!(".toolkit-{}", random_id()?));
    let result = (|| {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(io_error)?;
        f.write_all(bytes).map_err(io_error)?;
        f.sync_all().map_err(io_error)?;
        fs::rename(&tmp, path).map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(crate) fn tree_files(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        check_ancestors(dir)?;
        for e in fs::read_dir(dir).map_err(io_error)? {
            let e = e.map_err(io_error)?;
            let p = e.path();
            let m = fs::symlink_metadata(&p).map_err(io_error)?;
            if m.file_type().is_symlink() {
                return Err(error("unsafe_path", "Ссылка внутри дерева аддона"));
            }
            if m.is_dir() {
                if e.file_name() != "user_files" && e.file_name() != "__pycache__" {
                    visit(root, &p, out)?;
                }
            } else if m.is_file() && p.extension().is_some_and(|x| x == "py") {
                let name = p
                    .strip_prefix(root)
                    .map_err(io_error)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let mut bytes = fs::read(&p).map_err(io_error)?;
                if name == "__init__.py" {
                    bytes = strip(&bytes)?;
                }
                out.insert(name, hash(&bytes));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}
fn strip(bytes: &[u8]) -> Result<Vec<u8>> {
    let s = std::str::from_utf8(bytes).map_err(io_error)?;
    let manifest: Value = serde_json::from_str(COMPATIBILITY).map_err(io_error)?;
    let previous = manifest["previous_bootstrap"]
        .as_str()
        .ok_or_else(|| error("invalid_manifest", "Нет прежнего bootstrap"))?;
    if s.matches(BOOTSTRAP).count() + s.matches(previous).count() > 1 {
        return Err(error("unknown_bootstrap", "Повтор bootstrap"));
    }
    Ok(s.replacen(BOOTSTRAP, "", 1)
        .replacen(previous, "", 1)
        .into_bytes())
}
pub(crate) fn addon_status(t: &Target, id: &str) -> Result<Value> {
    let dir = t.addons().join(id);
    if !dir.exists() {
        return Ok(json!({"id":id,"state":"missing","installed":false,"loaded":null}));
    }
    check_ancestors(&dir)?;
    let meta_path = dir.join("meta.json");
    check_ancestors(&meta_path)?;
    let meta = if meta_path.try_exists().map_err(io_error)? {
        read_json(&meta_path)?
    } else {
        json!({})
    };
    let manifest: Value = serde_json::from_str(COMPATIBILITY).map_err(io_error)?;
    let expected = &manifest["addons"][id]["files"];
    let actual = serde_json::to_value(tree_files(&dir)?).map_err(io_error)?;
    let supported = actual == *expected && supports_anki(&t.version, &manifest);
    let version = point_version(&t.version);
    // Anki трактует только ОТРИЦАТЕЛЬНЫЙ max_point_version как верхнюю границу.
    let min = meta["min_point_version"].as_i64().unwrap_or(0);
    let max = meta["max_point_version"].as_i64().unwrap_or(0);
    let compatible = version.is_some_and(|v| v >= min && (max >= 0 || v <= -max));
    let state = if meta["disabled"] == true {
        "disabled"
    } else if !compatible || !supported {
        "unsupported"
    } else {
        "installed_unverified"
    };
    Ok(
        json!({"id":id,"state":state,"installed":true,"enabled":meta["disabled"]!=true,"compatible":compatible,
        "adapter_supported":supported,"loaded":null,"version_mod":meta["mod"],"path":dir}),
    )
}
pub(crate) fn point_version(s: &str) -> Option<i64> {
    let v: Vec<i64> = s
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    match v.as_slice() {
        [a, b] => Some(a * 10000 + b * 100),
        [a, b, c] => Some(a * 10000 + b * 100 + c),
        _ => None,
    }
}
pub(crate) fn supports_anki(version: &str, manifest: &Value) -> bool {
    let Some(actual) = point_version(version) else {
        return false;
    };
    manifest["anki_versions"].as_array().is_some_and(|v| {
        v.iter()
            .any(|v| v.as_str().and_then(point_version) == Some(actual))
    })
}
pub(crate) fn status(t: &Target) -> Result<Value> {
    let init = t.addons().join(CONNECT).join("__init__.py");
    if !init.exists() {
        return Ok(json!({"state":"missing"}));
    }
    check_ancestors(&init)?;
    let source = fs::read_to_string(&init).map_err(io_error)?;
    let settings = t.bridge().join("settings.json");
    for path in [
        settings.clone(),
        t.bridge().join("payload.py"),
        t.bridge().join("compatibility.json"),
    ] {
        check_ancestors(&path)?;
        crate::layout::guard_local_path(&path, "Патч Desktop")?;
    }
    let state = if !source.contains(BOOTSTRAP) {
        if settings.exists() {
            "damaged"
        } else {
            "missing"
        }
    } else if !settings.exists() {
        "damaged"
    } else {
        let s = match read_json(&settings) {
            Ok(s) => s,
            Err(e) => {
                return Ok(
                    json!({"state":"damaged","patch_version":PATCH_VERSION,"disk_only":true,"reason":e.message}),
                );
            }
        };
        if s["patch_version"] != PATCH_VERSION {
            "outdated"
        } else if fs::read(t.bridge().join("payload.py"))
            .ok()
            .is_none_or(|b| b != PAYLOAD.as_bytes())
            || fs::read(t.bridge().join("compatibility.json"))
                .ok()
                .is_none_or(|b| b != COMPATIBILITY.as_bytes())
        {
            "damaged"
        } else if addon_status(t, CONNECT)?["adapter_supported"] != true
            || addon_status(t, CROWD)?["adapter_supported"] != true
        {
            "incompatible"
        } else {
            "installed"
        }
    };
    Ok(json!({"state":state,"patch_version":PATCH_VERSION,"disk_only":true}))
}

pub(crate) struct AddonLock(PathBuf);
impl Drop for AddonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn lock_addons(t: &Target) -> Result<AddonLock> {
    ensure_private(&t.state())?;
    let path = t.state().join("patch.lock");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                error(
                    "patch_locked",
                    "Другая операция или оставшийся lock; сначала проверьте процесс",
                )
            } else {
                io_error(e)
            }
        })?;
    let lock = AddonLock(path);
    super::environment::require_stopped(t)?;
    Ok(lock)
}

pub(crate) fn backup(t: &Target) -> Result<PathBuf> {
    migrate_legacy_spool(t)?;
    let destination = t.state().join("backups").join(random_id()?);
    ensure_private(&destination)?;
    if t.discovery["platform"] == "windows" {
        let quote = super::environment::ps_quote;
        let script = format!(
            "$AddonRoot={}; $Destination={}; $SnapshotOnly=$false;\n{}",
            quote(&super::environment::native_path(&t.addons())?),
            quote(&super::environment::native_path(&destination)?),
            include_str!("windows_backup.ps1")
        );
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(io_error)?;
        if !output.status.success() {
            return Err(external_error(
                "backup_failed",
                "Windows backup не завершён; аддоны не изменены, частичная копия сохранена для диагностики",
            ));
        }
        let manifest: BTreeMap<String, String> =
            serde_json::from_slice(&output.stdout).map_err(io_error)?;
        write_atomic(
            &destination.join("sha256.json"),
            &serde_json::to_vec_pretty(&manifest).map_err(io_error)?,
        )?;
        finish_backup(&destination)?;
        return Ok(destination);
    }
    let mut manifest = BTreeMap::new();
    for id in [CONNECT, CROWD] {
        let root = t.addons().join(id);
        check_ancestors(&root)?;
        if root.exists() {
            copy_tree(&root, &destination.join(id), &mut manifest, id)?;
        }
    }
    write_atomic(
        &destination.join("sha256.json"),
        &serde_json::to_vec_pretty(&manifest).map_err(io_error)?,
    )?;
    // Независимое повторное чтение всех скопированных байтов до первой mutation.
    for (name, expected) in &manifest {
        if hash_file(&destination.join(name))? != *expected {
            return Err(external_error("backup_verification_failed", name));
        }
    }
    finish_backup(&destination)?;
    Ok(destination)
}

fn finish_backup(destination: &Path) -> Result<()> {
    let addons: BTreeMap<_, _> = [CONNECT, CROWD]
        .into_iter()
        .map(|id| (id, destination.join(id).is_dir()))
        .collect();
    write_atomic(&destination.join("snapshot.json"), &serde_json::to_vec_pretty(&json!({
        "schema_version":1, "addons":addons, "manifest_sha256":hash_file(&destination.join("sha256.json"))?
    })).map_err(io_error)?)
}
fn copy_tree(
    src: &Path,
    dst: &Path,
    manifest: &mut BTreeMap<String, String>,
    prefix: &str,
) -> Result<()> {
    check_ancestors(src)?;
    ensure_private(dst)?;
    for e in fs::read_dir(src).map_err(io_error)? {
        let e = e.map_err(io_error)?;
        let m = e.file_type().map_err(io_error)?;
        let name = e.file_name();
        if m.is_symlink() {
            return Err(error("unsafe_path", "Ссылка в backup"));
        }
        if prefix == "2055492159/user_files/toolkit" && name == "spool" {
            continue;
        }
        let key = format!("{prefix}/{}", name.to_string_lossy());
        if m.is_dir() {
            copy_tree(&e.path(), &dst.join(&name), manifest, &key)?;
        } else if m.is_file() {
            let digest = copy_file_atomic(&e.path(), &dst.join(&name))?;
            manifest.insert(key, digest);
        } else {
            return Err(error("unsafe_path", "Специальный файл в backup"));
        }
    }
    Ok(())
}

pub(crate) fn run(t: &Target, op: PatchAction, apply: bool) -> Result<Value> {
    if matches!(op, PatchAction::Status) {
        return status(t);
    }
    if !apply {
        let state = status(t)?;
        return Ok(
            json!({"state":"patch_plan","operation":format!("{op:?}"),"current":state,"ready":false,"consequences":"Меняются bootstrap и user_files/toolkit; задания переносятся в toolkit-desktop/spool; backup аддонов находится вне addons21 и spool не включает"}),
        );
    }
    if t.running {
        return Ok(
            json!({"state":"restart_required","ready":false,"reason":"stop_anki_before_patch"}),
        );
    }
    let _lock = lock_addons(t)?;
    let state = status(t)?;
    let init = t.addons().join(CONNECT).join("__init__.py");
    let old = fs::read(&init).map_err(io_error)?;
    let plain = strip(&old)?;
    if matches!(op, PatchAction::Remove) {
        if old == plain && !t.bridge().exists() {
            return Ok(json!({"state":"already_removed","ready":false}));
        }
        let backup = backup(t)?;
        super::environment::require_stopped(t)?;
        guard_source(&init, &old)?;
        if old != plain {
            write_atomic(&init, &plain)?;
        }
        // Payload и token остаются только в восстановимом backup, не в active addon.
        if t.bridge().exists() {
            check_ancestors(&t.bridge())?;
            fs::remove_dir_all(t.bridge()).map_err(io_error)?;
        }
        return Ok(json!({"state":"removed","ready":false,"backup":backup}));
    }
    for id in [CONNECT, CROWD] {
        let s = addon_status(t, id)?;
        if s["state"] != "installed_unverified" {
            return Err(error("incompatible_addon", format!("{id}: {}", s["state"])));
        }
    }
    if matches!(op, PatchAction::Install) && state["state"] == "installed" {
        return Ok(json!({"state":"already_installed","ready":false,"restart_required":true}));
    }
    if plain
        .windows(b"ANKI DECK TOOLKIT".len())
        .any(|s| s == b"ANKI DECK TOOLKIT")
    {
        return Err(error(
            "unknown_bootstrap",
            "Неизвестный или изменённый bootstrap: отказ без перезаписи",
        ));
    }
    let source = String::from_utf8(plain).map_err(io_error)?;
    if source.matches(ANCHOR).count() != 1 {
        return Err(error(
            "incompatible_hook",
            "Нет единственной проверенной точки подключения",
        ));
    }
    let backup = backup(t)?;
    super::environment::require_stopped(t)?;
    guard_source(&init, &old)?;
    ensure_private(&t.bridge())?;
    let settings = t.bridge().join("settings.json");
    let mut config = if settings.exists() {
        read_json(&settings)?
    } else {
        json!({"token":random_id()?})
    };
    if config["token"].as_str().is_none_or(|s| s.len() != 64) {
        return Err(error(
            "damaged_settings",
            "Повреждён token; сохранён backup, настройка не затёрта",
        ));
    }
    config["patch_version"] = json!(PATCH_VERSION);
    write_atomic(
        &settings,
        &serde_json::to_vec_pretty(&config).map_err(io_error)?,
    )?;
    write_atomic(&t.bridge().join("payload.py"), PAYLOAD.as_bytes())?;
    write_atomic(
        &t.bridge().join("compatibility.json"),
        COMPATIBILITY.as_bytes(),
    )?;
    let patched = source.replacen(ANCHOR, &format!("{BOOTSTRAP}{ANCHOR}"), 1);
    // Bootstrap публикуется последним; при ошибке старый source остаётся восстанавливаемым.
    super::environment::require_stopped(t)?;
    guard_source(&init, &old)?;
    write_atomic(&init, patched.as_bytes())?;
    let after = status(t)?;
    if after["state"] != "installed" {
        guard_source(&init, patched.as_bytes())?;
        write_atomic(&init, &old)?;
        return Err(external_error(
            "patch_postcondition_failed",
            "Bootstrap откачен; payload и backup сохранены для диагностики",
        ));
    }
    let legacy_spool_cleanup = match remove_legacy_spool_after_upgrade(t) {
        Ok(()) => json!({"state":"removed_or_absent"}),
        Err(e) => json!({"state":"retained","reason":e.details["reason"],"message":e.message}),
    };
    Ok(
        json!({"state":"pending_restart","ready":false,"patch":after,"backup":backup,"legacy_spool_cleanup":legacy_spool_cleanup}),
    )
}

/// Backup идентифицируется только локальным ID: произвольный путь не принимается.
pub(crate) fn restore(t: &Target, id: &str, apply: bool) -> Result<Value> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error("invalid_backup_id", "Ожидается ID backup, не путь"));
    }
    if apply && t.running {
        return Ok(
            json!({"state":"restart_required","ready":false,"reason":"stop_anki_before_restore"}),
        );
    }
    let _lock = if apply { Some(lock_addons(t)?) } else { None };
    let source = t.state().join("backups").join(id);
    check_ancestors(&source)?;
    check_ancestors(&source.join("sha256.json"))?;
    let manifest: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(source.join("sha256.json")).map_err(io_error)?)
            .map_err(io_error)?;
    let mut files = Vec::new();
    for (name, digest) in &manifest {
        let segments: Vec<_> = name.split('/').collect();
        if segments.len() < 2
            || ![CONNECT, CROWD].contains(&segments[0])
            || segments
                .iter()
                .any(|s| s.is_empty() || *s == "." || *s == ".." || s.contains(['\\', ':', '\0']))
        {
            return Err(error("unsafe_backup_manifest", "Недопустимый путь backup"));
        }
        let p = source.join(name);
        check_ancestors(&p)?;
        if hash_file(&p)? != *digest {
            return Err(external_error("backup_verification_failed", name));
        }
        files.push((name.clone(), digest.clone()));
    }
    let present = validate_snapshot(&source, &manifest)?;
    if !apply {
        return Ok(
            json!({"state":"restore_plan","backup":source,"files":files.len(),"addons":present,"ready":false}),
        );
    }
    let rollback = backup(t)?;
    let stage = t.state().join("restore").join(random_id()?);
    ensure_private(&stage)?;
    for id in [CONNECT, CROWD] {
        if present[id] {
            let prefix = format!("{id}/");
            let expected = manifest
                .iter()
                .filter_map(|(name, digest)| {
                    name.strip_prefix(&prefix)
                        .map(|name| (name.to_owned(), digest.clone()))
                })
                .collect();
            copy_tree_verified(&source.join(id), &stage.join(id), &expected)?;
            if tree_hashes(&stage.join(id))? != expected {
                return Err(external_error("backup_verification_failed", id));
            }
        }
    }
    let displaced = t.state().join("replaced").join(random_id()?);
    ensure_private(&displaced)?;
    super::environment::require_stopped(t)?;
    publish_restore(t, &stage, &displaced, |from, to| fs::rename(from, to)).map_err(|mut e| {
        e.details["rollback_backup"] = json!(rollback);
        e
    })?;
    // Не удаляем ни прежние деревья, ни backup: восстановление остаётся обратимым.
    Ok(
        json!({"state":"restored","ready":false,"backup":source,"rollback_backup":rollback,"displaced":displaced,"restart_required":true}),
    )
}

/// Старый формат допускается только при непустом, полном проверенном manifest.
/// Новый завершающий маркер доказывает также намеренно пустой снимок.
fn validate_snapshot(
    source: &Path,
    manifest: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, bool>> {
    let mut present = BTreeMap::new();
    let mut actual = BTreeMap::new();
    for entry in fs::read_dir(source).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if ![CONNECT, CROWD, "sha256.json", "snapshot.json"]
            .iter()
            .any(|allowed| name == *allowed)
        {
            return Err(error(
                "unsafe_backup_manifest",
                "Неизвестная запись в снимке",
            ));
        }
        check_ancestors(&entry.path())?;
    }
    for id in [CONNECT, CROWD] {
        let root = source.join(id);
        check_ancestors(&root)?;
        let exists = root.try_exists().map_err(io_error)?;
        present.insert(id.to_owned(), exists);
        if exists {
            for (name, digest) in tree_hashes(&root)? {
                actual.insert(format!("{id}/{name}"), digest);
            }
        }
    }
    if actual != *manifest {
        return Err(error(
            "unsafe_backup_manifest",
            "Manifest не описывает полное содержимое снимка",
        ));
    }
    let snapshot_path = source.join("snapshot.json");
    check_ancestors(&snapshot_path)?;
    if snapshot_path.try_exists().map_err(io_error)? {
        let snapshot = read_json(&snapshot_path)?;
        if snapshot["schema_version"] != 1
            || snapshot["addons"] != json!(present)
            || snapshot["manifest_sha256"] != hash_file(&source.join("sha256.json"))?
        {
            return Err(error(
                "unsafe_backup_manifest",
                "Повреждён маркер полноты снимка",
            ));
        }
    } else if manifest.is_empty() {
        return Err(error(
            "unsafe_backup_manifest",
            "Пустой старый снимок не доказывает отсутствие аддонов",
        ));
    }
    Ok(present)
}

fn guard_source(path: &Path, expected: &[u8]) -> Result<()> {
    check_ancestors(path)?;
    if fs::read(path).map_err(io_error)? == expected {
        Ok(())
    } else {
        Err(error(
            "upstream_changed",
            "AnkiConnect изменился после проверки; bootstrap не перезаписан",
        ))
    }
}

fn publish_restore(
    t: &Target,
    stage: &Path,
    displaced: &Path,
    mut move_path: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let mut moved = Vec::new();
    let mut published = Vec::new();
    let result: Result<()> = (|| {
        for id in [CONNECT, CROWD] {
            let ready = stage.join(id);
            check_ancestors(&ready)?;
            let dest = t.addons().join(id);
            check_ancestors(&dest)?;
            if dest.exists() {
                move_path(&dest, &displaced.join(id)).map_err(io_error)?;
                moved.push(id);
            }
            if ready.is_dir() {
                move_path(&ready, &dest).map_err(io_error)?;
                published.push(id);
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let mut recovery_errors = Vec::new();
        for id in published.into_iter().rev() {
            if let Err(e) = move_path(&t.addons().join(id), &stage.join(id)) {
                recovery_errors.push(e.to_string());
            }
        }
        for id in moved.into_iter().rev() {
            let dest = t.addons().join(id);
            if dest.exists() {
                recovery_errors.push(format!("{id}: целевой каталог всё ещё существует"));
                continue;
            }
            if let Err(e) = move_path(&displaced.join(id), &dest) {
                recovery_errors.push(e.to_string());
            }
        }
        return Err(crate::error::DomainError::with_details(
            crate::error::ErrorCode::ExternalFailure,
            format!("Восстановление прервано: {e}"),
            json!({"reason":"restore_failed","stage":stage,"displaced":displaced,
                "rollback_complete":recovery_errors.is_empty(),"recovery_errors":recovery_errors}),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn target(root: &Path) -> Target {
        Target {
            executable: root.join("anki"),
            data: root.to_path_buf(),
            native_data: root.to_string_lossy().into_owned(),
            version: "26.9.2".into(),
            config: json!({}),
            discovery: json!({"platform":"linux","processes":[]}),
            running: false,
            process_probe: || Ok(json!({"platform":"linux","processes":[]})),
        }
    }
    #[test]
    fn bootstrap_roundtrip_preserves_all_upstream_bytes() {
        let original = format!("# arbitrary upstream\n\n{ANCHOR}\n    pass\n");
        let patched = original.replacen(ANCHOR, &format!("{BOOTSTRAP}{ANCHOR}"), 1);
        assert_eq!(strip(patched.as_bytes()).unwrap(), original.as_bytes());
        assert!(strip(format!("{BOOTSTRAP}{BOOTSTRAP}").as_bytes()).is_err());
        let manifest: Value = serde_json::from_str(COMPATIBILITY).unwrap();
        let previous = manifest["previous_bootstrap"].as_str().unwrap();
        assert_eq!(
            strip(format!("{previous}{original}").as_bytes()).unwrap(),
            original.as_bytes()
        );
        assert!(strip(format!("{previous}{BOOTSTRAP}").as_bytes()).is_err());
    }

    #[test]
    fn manifest_bootstrap_matches_rust_bootstrap() {
        let manifest: Value = serde_json::from_str(COMPATIBILITY).unwrap();
        assert_eq!(manifest["bootstrap"].as_str(), Some(BOOTSTRAP));
    }

    #[test]
    fn remove_cleans_payload_after_upstream_erased_the_bootstrap() {
        let tmp = TempDir::new("desktop-remove-missing-bootstrap");
        let t = target(tmp.path());
        let connect = t.addons().join(CONNECT);
        let crowd = t.addons().join(CROWD);
        ensure_private(&connect).unwrap();
        ensure_private(&crowd).unwrap();
        let original = format!("{ANCHOR}\n    pass\n");
        write_atomic(&connect.join("__init__.py"), original.as_bytes()).unwrap();
        write_atomic(&connect.join("meta.json"), b"{} ").unwrap();
        write_atomic(&crowd.join("meta.json"), b"{} ").unwrap();
        let payload = t.bridge().join("payload.py");
        ensure_private(payload.parent().unwrap()).unwrap();
        write_atomic(&payload, b"private payload").unwrap();
        write_atomic(&t.bridge().join("settings.json"), b"private token").unwrap();
        let job_id = "e".repeat(64);
        let old_job = t.bridge().join("spool").join(&job_id);
        ensure_private(&old_job).unwrap();
        write_atomic(&old_job.join("request.json"), b"survives remove").unwrap();

        let result = run(&t, PatchAction::Remove, true).unwrap();

        assert_eq!(result["state"], "removed");
        assert!(!t.bridge().exists());
        assert_eq!(
            fs::read(connect.join("__init__.py")).unwrap(),
            original.as_bytes()
        );
        let backup = PathBuf::from(result["backup"].as_str().unwrap());
        assert_eq!(
            fs::read(backup.join(CONNECT).join("user_files/toolkit/payload.py")).unwrap(),
            b"private payload"
        );
        assert_eq!(
            fs::read(
                backup
                    .join(CONNECT)
                    .join("user_files/toolkit/settings.json")
            )
            .unwrap(),
            b"private token"
        );
        assert_eq!(
            fs::read(t.state().join("spool").join(&job_id).join("request.json")).unwrap(),
            b"survives remove"
        );
    }
    #[test]
    fn changed_upstream_and_new_process_abort_before_publication() {
        let tmp = TempDir::new("desktop-preconditions");
        let mut t = target(tmp.path());
        let source = tmp.path().join("__init__.py");
        fs::write(&source, b"foreign update").unwrap();
        assert_eq!(
            guard_source(&source, b"original").unwrap_err().details["reason"],
            "upstream_changed"
        );
        assert_eq!(fs::read(&source).unwrap(), b"foreign update");
        t.process_probe = || Ok(json!({"platform":"linux","processes":[{"pid":123}]}));
        assert_eq!(
            super::super::environment::require_stopped(&t)
                .unwrap_err()
                .details["reason"],
            "restart_required"
        );
    }
    #[test]
    fn backup_restores_exact_bytes_including_meta_and_user_files() {
        let tmp = TempDir::new("desktop-backup");
        let t = target(tmp.path());
        for id in [CONNECT, CROWD] {
            let dir = t.addons().join(id);
            ensure_private(&dir.join("user_files")).unwrap();
            write_atomic(
                &dir.join("meta.json"),
                br#"{"config":{"secret":"private"}}"#,
            )
            .unwrap();
            write_atomic(&dir.join("user_files/state.bin"), b"\0\xff private").unwrap();
        }
        let id = "a".repeat(64);
        let legacy_job = t.bridge().join("spool").join(&id);
        ensure_private(&legacy_job).unwrap();
        write_atomic(&legacy_job.join("request.json"), b"legacy ticket").unwrap();
        ensure_private(&legacy_job.join("export")).unwrap();
        write_atomic(&legacy_job.join("export/media.bin"), b"legacy media").unwrap();

        let b = backup(&t).unwrap();
        assert!(!b.starts_with(t.addons()));
        assert_eq!(
            fs::read(t.state().join("spool").join(&id).join("request.json")).unwrap(),
            b"legacy ticket"
        );
        assert_eq!(
            fs::read(t.state().join("spool").join(&id).join("export/media.bin")).unwrap(),
            b"legacy media"
        );
        assert!(t.bridge().join("spool").exists());
        assert!(!b.join(CONNECT).join("user_files/toolkit/spool").exists());
        for id in [CONNECT, CROWD] {
            for name in ["meta.json", "user_files/state.bin"] {
                assert_eq!(
                    fs::read(t.addons().join(id).join(name)).unwrap(),
                    fs::read(b.join(id).join(name)).unwrap()
                );
            }
        }
    }

    #[test]
    fn legacy_spool_copy_merges_distinct_jobs_and_rejects_conflicts_without_loss() {
        let tmp = TempDir::new("desktop-spool-migration");
        let t = target(tmp.path());
        let existing_id = "a".repeat(64);
        let legacy_id = "b".repeat(64);
        let existing = t.state().join("spool").join(&existing_id);
        let legacy = t.bridge().join("spool").join(&legacy_id);
        ensure_private(&existing).unwrap();
        ensure_private(&legacy).unwrap();
        write_atomic(&existing.join("request.json"), b"current").unwrap();
        write_atomic(&legacy.join("request.json"), b"legacy").unwrap();

        migrate_legacy_spool(&t).unwrap();

        assert_eq!(
            fs::read(
                t.state()
                    .join("spool")
                    .join(&legacy_id)
                    .join("request.json")
            )
            .unwrap(),
            b"legacy"
        );
        assert!(legacy.join("request.json").exists());

        let conflict_id = "c".repeat(64);
        let current_conflict = t.state().join("spool").join(&conflict_id);
        let legacy_conflict = t.bridge().join("spool").join(&conflict_id);
        ensure_private(&current_conflict).unwrap();
        ensure_private(&legacy_conflict).unwrap();
        write_atomic(&current_conflict.join("request.json"), b"current copy").unwrap();
        write_atomic(&legacy_conflict.join("request.json"), b"legacy copy").unwrap();

        let error = migrate_legacy_spool(&t).unwrap_err();

        assert_eq!(error.details["reason"], "spool_migration_conflict");
        assert_eq!(
            fs::read(legacy_conflict.join("request.json")).unwrap(),
            b"legacy copy"
        );
        assert_eq!(
            fs::read(current_conflict.join("request.json")).unwrap(),
            b"current copy"
        );
    }

    #[test]
    fn successful_upgrade_cleanup_removes_only_verified_legacy_spool() {
        let tmp = TempDir::new("desktop-spool-upgrade-cleanup");
        let t = target(tmp.path());
        let id = "d".repeat(64);
        let legacy_job = t.bridge().join("spool").join(&id);
        ensure_private(&legacy_job).unwrap();
        write_atomic(&legacy_job.join("request.json"), b"ticket").unwrap();
        migrate_legacy_spool(&t).unwrap();

        remove_legacy_spool_after_upgrade(&t).unwrap();

        assert!(!t.bridge().join("spool").exists());
        assert_eq!(
            fs::read(t.state().join("spool").join(&id).join("request.json")).unwrap(),
            b"ticket"
        );
    }
    #[test]
    fn missing_disabled_unsupported_do_not_become_loaded_from_disk() {
        let tmp = TempDir::new("desktop-states");
        let t = target(tmp.path());
        assert_eq!(addon_status(&t, CONNECT).unwrap()["state"], "missing");
        let dir = t.addons().join(CONNECT);
        ensure_private(&dir).unwrap();
        write_atomic(&dir.join("meta.json"), br#"{"disabled":true}"#).unwrap();
        assert_eq!(addon_status(&t, CONNECT).unwrap()["state"], "disabled");
        write_atomic(&dir.join("meta.json"), br#"{"disabled":false}"#).unwrap();
        let s = addon_status(&t, CONNECT).unwrap();
        assert_eq!(s["state"], "unsupported");
        assert!(s["loaded"].is_null());
        write_atomic(&dir.join("__init__.py"), b"# unknown upstream").unwrap();
        let before = fs::read(dir.join("__init__.py")).unwrap();
        assert!(run(&t, PatchAction::Repair, true).is_err());
        assert_eq!(before, fs::read(dir.join("__init__.py")).unwrap());
    }
    #[test]
    fn addon_status_uses_default_metadata_when_meta_file_is_absent() {
        let tmp = TempDir::new("desktop-addon-missing-meta");
        let t = target(tmp.path());
        let dir = t.addons().join(CONNECT);
        ensure_private(&dir).unwrap();

        let status = addon_status(&t, CONNECT).unwrap();

        assert_eq!(status["installed"], true);
        assert_eq!(status["enabled"], true);
        assert_eq!(status["compatible"], true);
        assert_eq!(status["state"], "unsupported");
        write_atomic(&dir.join("meta.json"), b"not JSON").unwrap();
        assert!(addon_status(&t, CONNECT).is_err());
    }
    #[test]
    fn restore_verifies_before_mutation_and_keeps_displaced_files() {
        let tmp = TempDir::new("desktop-restore");
        let t = target(tmp.path());
        let dir = t.addons().join(CONNECT);
        ensure_private(&dir).unwrap();
        write_atomic(&dir.join("meta.json"), b"original settings").unwrap();
        let b = backup(&t).unwrap();
        let id = b.file_name().unwrap().to_str().unwrap();
        write_atomic(&dir.join("meta.json"), b"changed settings").unwrap();
        let result = restore(&t, id, true).unwrap();
        assert_eq!(result["state"], "restored");
        assert_eq!(
            fs::read(dir.join("meta.json")).unwrap(),
            b"original settings"
        );
        let displaced = PathBuf::from(result["displaced"].as_str().unwrap());
        assert_eq!(
            fs::read(displaced.join(CONNECT).join("meta.json")).unwrap(),
            b"changed settings"
        );
        write_atomic(&b.join(CONNECT).join("meta.json"), b"tampered").unwrap();
        assert!(restore(&t, id, true).is_err());
        assert_eq!(
            fs::read(dir.join("meta.json")).unwrap(),
            b"original settings"
        );
    }
    #[test]
    fn restore_preserves_presence_absence_and_addressable_rollback() {
        for present in [[false, false], [false, true], [true, false], [true, true]] {
            let tmp = TempDir::new("desktop-restore-presence");
            let t = target(tmp.path());
            for (id, exists) in [CONNECT, CROWD].into_iter().zip(present) {
                if exists {
                    ensure_private(&t.addons().join(id).join("empty")).unwrap();
                    fs::write(t.addons().join(id).join("state"), b"original").unwrap();
                }
            }
            let original = backup(&t).unwrap();
            let id = original.file_name().unwrap().to_str().unwrap();
            for addon in [CONNECT, CROWD, "other-addon"] {
                ensure_private(&t.addons().join(addon)).unwrap();
                fs::write(t.addons().join(addon).join("state"), b"installed").unwrap();
            }
            ensure_private(&t.state().join("spool")).unwrap();
            fs::write(t.state().join("spool/untouched"), b"job").unwrap();
            let restored = restore(&t, id, true).unwrap();
            let displaced = PathBuf::from(restored["displaced"].as_str().unwrap());
            for (addon, exists) in [CONNECT, CROWD].into_iter().zip(present) {
                assert_eq!(t.addons().join(addon).exists(), exists);
                if exists {
                    assert_eq!(
                        fs::read(t.addons().join(addon).join("state")).unwrap(),
                        b"original"
                    );
                    assert!(t.addons().join(addon).join("empty").is_dir());
                }
                assert_eq!(
                    fs::read(displaced.join(addon).join("state")).unwrap(),
                    b"installed"
                );
            }
            assert_eq!(
                fs::read(t.addons().join("other-addon/state")).unwrap(),
                b"installed"
            );
            assert_eq!(fs::read(t.state().join("spool/untouched")).unwrap(), b"job");
            let rollback = PathBuf::from(restored["rollback_backup"].as_str().unwrap());
            restore(&t, rollback.file_name().unwrap().to_str().unwrap(), true).unwrap();
            for addon in [CONNECT, CROWD] {
                assert_eq!(
                    fs::read(t.addons().join(addon).join("state")).unwrap(),
                    b"installed"
                );
            }
        }
    }

    #[test]
    fn legacy_snapshot_requires_complete_nonempty_inventory() {
        let tmp = TempDir::new("desktop-legacy-backup");
        let t = target(tmp.path());
        ensure_private(&t.addons().join(CONNECT)).unwrap();
        fs::write(t.addons().join(CONNECT).join("meta.json"), b"original").unwrap();
        let source = backup(&t).unwrap();
        fs::remove_file(source.join("snapshot.json")).unwrap();
        ensure_private(&t.addons().join(CROWD)).unwrap();
        fs::write(t.addons().join(CROWD).join("new"), b"new").unwrap();
        let id = source.file_name().unwrap().to_str().unwrap();
        restore(&t, id, true).unwrap();
        assert!(!t.addons().join(CROWD).exists());
        fs::write(source.join(CONNECT).join("unlisted"), b"unknown").unwrap();
        assert_eq!(
            restore(&t, id, true).unwrap_err().details["reason"],
            "unsafe_backup_manifest"
        );
        fs::remove_file(source.join(CONNECT).join("unlisted")).unwrap();
        fs::write(source.join("sha256.json"), b"{}").unwrap();
        fs::remove_dir_all(source.join(CONNECT)).unwrap();
        assert_eq!(
            restore(&t, id, true).unwrap_err().details["reason"],
            "unsafe_backup_manifest"
        );
        assert_eq!(
            fs::read(t.addons().join(CONNECT).join("meta.json")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn incomplete_or_inconsistent_snapshot_never_removes_addons() {
        let tmp = TempDir::new("desktop-incomplete-backup");
        let t = target(tmp.path());
        let source = backup(&t).unwrap();
        let id = source.file_name().unwrap().to_str().unwrap();
        ensure_private(&t.addons().join(CONNECT)).unwrap();
        fs::write(t.addons().join(CONNECT).join("state"), b"keep").unwrap();
        fs::write(source.join("sha256.json"), b"{ } ").unwrap();
        assert!(restore(&t, id, true).is_err());
        fs::remove_file(source.join("snapshot.json")).unwrap();
        assert!(restore(&t, id, true).is_err());
        fs::remove_file(source.join("sha256.json")).unwrap();
        assert!(restore(&t, id, true).is_err());
        assert_eq!(
            fs::read(t.addons().join(CONNECT).join("state")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn failure_after_removing_absent_addon_rolls_back_both_trees() {
        let tmp = TempDir::new("desktop-restore-absent-failure");
        let t = target(tmp.path());
        let stage = tmp.path().join("stage");
        let displaced = tmp.path().join("displaced");
        ensure_private(&stage.join(CROWD)).unwrap();
        ensure_private(&displaced).unwrap();
        for addon in [CONNECT, CROWD] {
            ensure_private(&t.addons().join(addon)).unwrap();
            fs::write(t.addons().join(addon).join("state"), b"current").unwrap();
        }
        let mut calls = 0;
        let failure = publish_restore(&t, &stage, &displaced, |from, to| {
            calls += 1;
            if calls == 2 {
                return Err(std::io::Error::other("Ошибка перемещения"));
            }
            fs::rename(from, to)
        })
        .unwrap_err();
        assert_eq!(failure.details["rollback_complete"], true);
        for addon in [CONNECT, CROWD] {
            assert_eq!(
                fs::read(t.addons().join(addon).join("state")).unwrap(),
                b"current"
            );
        }
    }

    #[test]
    fn shared_addon_lock_blocks_all_mutators_in_another_process_and_releases() {
        const CHILD_ROOT: &str = "ANKI_TOOLKIT_LOCK_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let t = target(Path::new(&root));
            let options = super::super::Options {
                action: super::super::Action::Doctor,
                executable: None,
                data_dir: None,
                profile: None,
                endpoint: None,
                api_key_env: "ANKICONNECT_API_KEY".into(),
            };
            for result in [
                super::super::setup::run(&t, &options, true),
                run(&t, PatchAction::Repair, true),
                restore(&t, &"a".repeat(64), true),
            ] {
                assert_eq!(result.unwrap_err().details["reason"], "patch_locked");
            }
            assert!(!t.state().join("backups").exists());
            fs::write(t.data.join("lock-child-completed"), b"setup patch restore").unwrap();
            return;
        }
        let tmp = TempDir::new("desktop-shared-lock");
        let mut t = target(tmp.path());
        let lock = lock_addons(&t).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "desktop::patch::tests::shared_addon_lock_blocks_all_mutators_in_another_process_and_releases"])
            .env(CHILD_ROOT, tmp.path()).output().unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stdout)
        );
        assert_eq!(
            fs::read(t.data.join("lock-child-completed")).unwrap(),
            b"setup patch restore"
        );
        drop(lock);
        assert!(!t.state().join("patch.lock").exists());
        t.process_probe = || Ok(json!({"platform":"linux","processes":[{"pid":1}]}));
        assert!(lock_addons(&t).is_err());
        assert!(!t.state().join("patch.lock").exists());
        t.process_probe = || Ok(json!({"platform":"linux","processes":[]}));
        assert!(lock_addons(&t).is_ok());
    }

    #[test]
    fn streaming_copy_verifies_multiple_chunks_and_cleans_failed_publication() {
        let tmp = TempDir::new("desktop-streaming-copy");
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        let bytes: Vec<u8> = (0..300_001).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        assert_eq!(
            copy_file_atomic(&source, &destination).unwrap(),
            hash(&bytes)
        );
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        let mismatch = tmp.path().join("mismatch");
        assert!(copy_verified_file(&source, &mismatch, &hash(b"wrong"), "test").is_err());
        assert!(!mismatch.exists());
        let directory = tmp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(copy_file_atomic(&source, &directory).is_err());
        assert!(directory.is_dir());
        assert!(fs::read_dir(tmp.path()).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".toolkit-")
        }));
        let t = target(tmp.path());
        ensure_private(&t.addons().join(CONNECT)).unwrap();
        fs::copy(&source, t.addons().join(CONNECT).join("large.bin")).unwrap();
        let saved = backup(&t).unwrap();
        assert_eq!(
            hash_file(&saved.join(CONNECT).join("large.bin")).unwrap(),
            hash(&bytes)
        );
    }

    #[test]
    fn streaming_copy_propagates_read_and_write_errors_and_handles_short_writes() {
        struct Reader(usize);
        impl Read for Reader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(buffer.len() <= 64 * 1024);
                if self.0 == 0 {
                    return Err(std::io::Error::other("Ошибка чтения"));
                }
                let count = self.0.min(buffer.len());
                buffer[..count].fill(42);
                self.0 -= count;
                Ok(count)
            }
        }
        struct Writer {
            bytes: Vec<u8>,
            fail_after: usize,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.bytes.len() >= self.fail_after {
                    return Err(std::io::Error::other("Ошибка записи"));
                }
                let count = bytes.len().min(997).min(self.fail_after - self.bytes.len());
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = vec![42; 200_003];
        let expected = hash(&bytes);
        let mut writer = Writer {
            bytes: vec![],
            fail_after: usize::MAX,
        };
        assert!(
            copy_stream_verified(&mut Reader(131_073), &mut writer, &expected, "test").is_err()
        );
        let mut writer = Writer {
            bytes: vec![],
            fail_after: 131_073,
        };
        assert!(
            copy_stream_verified(&mut bytes.as_slice(), &mut writer, &expected, "test").is_err()
        );
        let mut writer = Writer {
            bytes: vec![],
            fail_after: usize::MAX,
        };
        copy_stream_verified(&mut bytes.as_slice(), &mut writer, &expected, "test").unwrap();
        assert_eq!(writer.bytes, bytes);
    }
    #[test]
    fn failure_moving_second_addon_rolls_back_the_first() {
        let tmp = TempDir::new("desktop-restore-failure");
        let t = target(tmp.path());
        let stage = tmp.path().join("stage");
        let displaced = tmp.path().join("displaced");
        ensure_private(&displaced).unwrap();
        for id in [CONNECT, CROWD] {
            ensure_private(&t.addons().join(id)).unwrap();
            ensure_private(&stage.join(id)).unwrap();
            write_atomic(&t.addons().join(id).join("meta.json"), b"old").unwrap();
            write_atomic(&stage.join(id).join("meta.json"), b"new").unwrap();
        }
        let mut calls = 0;
        let e = publish_restore(&t, &stage, &displaced, |from, to| {
            calls += 1;
            if calls == 3 {
                return Err(std::io::Error::other("Искусственный отказ перемещения"));
            }
            fs::rename(from, to)
        })
        .unwrap_err();
        assert_eq!(e.details["reason"], "restore_failed");
        assert_eq!(e.code, crate::error::ErrorCode::ExternalFailure);
        assert_eq!(e.details["rollback_complete"], true);
        assert!(e.details["recovery_errors"].is_array());
        for id in [CONNECT, CROWD] {
            assert_eq!(
                fs::read(t.addons().join(id).join("meta.json")).unwrap(),
                b"old"
            );
        }
    }
    #[test]
    fn positive_max_version_is_not_an_upper_bound() {
        assert_eq!(point_version("26.9.2"), Some(260902));
        assert_eq!(point_version("25.9"), Some(250900));
        assert_eq!(point_version("unknown"), None);
        assert!(supports_anki(
            "26.09.2",
            &json!({"anki_versions":["26.9.2"]})
        ));
        assert!(!supports_anki(
            "26.09.3",
            &json!({"anki_versions":["26.9.2"]})
        ));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_backup_is_rejected_before_any_addon_change() {
        let tmp = TempDir::new("desktop-symlink");
        let t = target(tmp.path());
        ensure_private(&t.addons().join(CONNECT)).unwrap();
        std::os::unix::fs::symlink(tmp.path(), t.addons().join(CONNECT).join("escape")).unwrap();
        assert!(backup(&t).is_err());
        assert!(t.addons().join(CONNECT).join("escape").is_symlink());
    }
}
