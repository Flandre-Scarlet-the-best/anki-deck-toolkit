use super::{
    CONNECT, CROWD, DomainError, Options, Result, error, external_error, io_error, read_json,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct Target {
    pub executable: PathBuf,
    pub data: PathBuf,
    pub native_data: String,
    pub version: String,
    pub config: Value,
    pub discovery: Value,
    pub running: bool,
    pub process_probe: fn() -> Result<Value>,
}
impl Target {
    pub fn addons(&self) -> PathBuf {
        self.data.join("addons21")
    }
    pub fn bridge(&self) -> PathBuf {
        self.addons().join(CONNECT).join("user_files/toolkit")
    }
    pub fn state(&self) -> PathBuf {
        self.data.join("toolkit-desktop")
    }
}

pub(crate) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
pub(crate) fn powershell(script: &str) -> Result<String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(io_error)?;
    if !out.status.success() {
        return Err(external_error(
            "windows_probe_failed",
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned())
}

pub(crate) fn native_path(p: &Path) -> Result<String> {
    if p.starts_with("/mnt") {
        let out = Command::new("wslpath")
            .arg("-w")
            .arg(p)
            .output()
            .map_err(io_error)?;
        if !out.status.success() {
            return Err(external_error("path_mapping_failed", "wslpath отказал"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().into())
    } else {
        Ok(p.to_string_lossy().into_owned())
    }
}
fn local_path(s: &str) -> Result<PathBuf> {
    if s.as_bytes().get(1) == Some(&b':') && cfg!(target_os = "linux") {
        let out = Command::new("wslpath")
            .args(["-u", s])
            .output()
            .map_err(io_error)?;
        if !out.status.success() {
            return Err(external_error("path_mapping_failed", "wslpath отказал"));
        }
        Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    } else {
        Ok(PathBuf::from(s))
    }
}

fn wsl_networking_mode() -> Option<String> {
    let output = Command::new("wslinfo")
        .arg("--networking-mode")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mode = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    (!mode.is_empty()).then_some(mode)
}

const WINDOWS_DISCOVERY_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $paths=@(); $c=Get-Command anki.exe -ErrorAction SilentlyContinue; if($c){$paths+=$c.Source}; $paths+=Join-Path $env:LOCALAPPDATA 'Programs\Anki\Anki.exe'; $paths+=Join-Path $env:ProgramFiles 'Anki\anki.exe'; foreach($r in @('HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*')){Get-ItemProperty $r -ErrorAction SilentlyContinue | Where-Object {$_.DisplayName -eq 'Anki' -and $_.InstallLocation} | ForEach-Object {$paths+=Join-Path $_.InstallLocation 'anki.exe'}}; $installs=@($paths | Where-Object {Test-Path -LiteralPath $_} | ForEach-Object {@{executable=$_; version=(Get-Item -LiteralPath $_).VersionInfo.ProductVersion}}); $processes=@(Get-CimInstance Win32_Process | Where-Object {$_.Name -match '^(anki|anki-console|anki-launcher)\.exe$' -or ($_.Name -match '^pythonw?\.exe$' -and $_.ExecutablePath -like '*\Anki\*')} | ForEach-Object {@{pid=$_.ProcessId; executable=$_.ExecutablePath; command_line=$_.CommandLine}}); @{platform='windows'; wsl=$true; installs=$installs; processes=$processes; default_data=(Join-Path $env:APPDATA 'Anki2')} | ConvertTo-Json -Depth 5 -Compress"#;

fn deduplicate_windows_installations(installs: &mut Vec<Value>) {
    let mut seen = HashSet::new();
    installs.retain(|install| {
        install["executable"]
            .as_str()
            .is_some_and(|path| seen.insert(path.to_lowercase()))
    });
}

fn linux_installations(directories: impl IntoIterator<Item = PathBuf>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut installs = Vec::new();
    for directory in directories {
        let executable = directory.join("anki");
        if !executable.is_file() {
            continue;
        }
        let identity = fs::canonicalize(&executable).unwrap_or_else(|_| executable.clone());
        if seen.insert(identity) {
            installs.push(json!({"executable":executable,"version":null}));
        }
    }
    installs
}

pub(crate) fn discover() -> Result<Value> {
    let wsl = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .to_lowercase()
        .contains("microsoft");
    if wsl || cfg!(target_os = "windows") {
        let mut discovery: Value =
            serde_json::from_str(&powershell(WINDOWS_DISCOVERY_SCRIPT)?).map_err(io_error)?;
        discovery["wsl"] = json!(wsl);
        if wsl {
            discovery["wsl_networking_mode"] = json!(wsl_networking_mode());
        }
        if let Some(installs) = discovery["installs"].as_array_mut() {
            deduplicate_windows_installations(installs);
        }
        if let Some(processes) = discovery["processes"].as_array_mut() {
            sort_processes(processes);
        }
        return Ok(discovery);
    }
    let installs = linux_installations(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>(),
    );
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").map_err(io_error)?.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let cmd = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let args = String::from_utf8_lossy(&cmd);
        let args: Vec<_> = args.split('\0').filter(|s| !s.is_empty()).collect();
        if args
            .iter()
            .take(3)
            .any(|a| Path::new(a).file_name().is_some_and(|n| n == "anki") || *a == "aqt")
        {
            processes.push(json!({"pid":entry.file_name().to_string_lossy(),"command_line":args}));
        }
    }
    sort_processes(&mut processes);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("Anki2");
    Ok(
        json!({"platform":"linux","wsl":false,"installs":installs,"processes":processes,"default_data":data}),
    )
}

fn sort_processes(processes: &mut [Value]) {
    processes.sort_by_key(|process| {
        process["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .or_else(|| {
                process["pid"]
                    .as_str()
                    .and_then(|pid| pid.parse::<u32>().ok())
            })
            .unwrap_or(u32::MAX)
    });
}

pub(crate) fn resolve(o: &Options) -> Result<Target> {
    let discovery = discover()?;
    let installs = discovery["installs"]
        .as_array()
        .ok_or_else(|| external_error("probe_failed", "Нет списка установок"))?;
    let executable = match &o.executable {
        Some(p) => fs::canonicalize(p).map_err(io_error)?,
        None if installs.len() == 1 => {
            local_path(installs[0]["executable"].as_str().unwrap_or(""))?
        }
        _ => return Err(DomainDiscovery::failure(&discovery)),
    };
    crate::layout::guard_local_path(&executable, "Исполняемый файл Anki")?;
    let data = match &o.data_dir {
        Some(p) => p.clone(),
        None if installs.len() == 1
            && local_path(installs[0]["executable"].as_str().unwrap_or(""))?
                .canonicalize()
                .ok()
                == executable.canonicalize().ok() =>
        {
            local_path(discovery["default_data"].as_str().unwrap_or(""))?
        }
        _ => {
            return Err(error(
                "data_mapping_required",
                "Для этой установки явно задайте --data-dir",
            ));
        }
    };
    crate::layout::guard_local_path(&data, "Данные Anki")?;
    // Ничего не создаём: отсутствие базы — отдельная диагностика.
    let data = fs::canonicalize(data).map_err(io_error)?;
    let windows = discovery["platform"] == "windows";
    let version = if windows {
        powershell(&format!(
            "(Get-Item -LiteralPath {}).VersionInfo.ProductVersion",
            ps_quote(&native_path(&executable)?)
        ))?
    } else {
        let out = Command::new(&executable)
            .arg("--version")
            .output()
            .map_err(io_error)?;
        if !out.status.success() {
            return Err(external_error(
                "version_probe_failed",
                "Anki --version завершился ошибкой",
            ));
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_start_matches("Anki ")
            .into()
    };
    let addon = data.join("addons21").join(CONNECT);
    let mut config = if addon.join("config.json").exists() {
        read_json(&addon.join("config.json"))?
    } else {
        json!({})
    };
    if addon.join("meta.json").exists() {
        let meta = read_json(&addon.join("meta.json"))?;
        if let Some(overrides) = meta["config"].as_object() {
            for (k, v) in overrides {
                config[k] = v.clone();
            }
        }
    }
    let running = discovery["processes"]
        .as_array()
        .is_some_and(|p| !p.is_empty());
    Ok(Target {
        native_data: native_path(&data)?,
        executable,
        data,
        version,
        config,
        discovery,
        running,
        process_probe: discover,
    })
}

pub(crate) fn require_stopped(t: &Target) -> Result<()> {
    let current = (t.process_probe)()?;
    if current["processes"]
        .as_array()
        .is_some_and(|p| p.is_empty())
    {
        Ok(())
    } else {
        Err(error(
            "restart_required",
            "Anki запущена во время операции; изменение аддонов отменено",
        ))
    }
}
struct DomainDiscovery;
impl DomainDiscovery {
    fn failure(d: &Value) -> crate::error::DomainError {
        crate::error::DomainError::with_details(
            crate::error::ErrorCode::InvalidRequest,
            "Anki не найдена или найдено несколько установок; задайте --executable",
            json!({"reason":"installation_selection_required","discovery":d}),
        )
    }
}

pub(crate) fn doctor(t: &Target, o: &Options) -> Result<Value> {
    let addons: Vec<Value> = [CONNECT, CROWD]
        .iter()
        .map(|id| super::patch::addon_status(t, id))
        .collect::<Result<_>>()?;
    let patch = super::patch::status(t)?;
    let (endpoint, live, mut handshake) = live_diagnostics(t, o);
    let ready = live["state"] == "patch_loaded"
        && patch["state"] == "installed"
        && handshake["functional_evidence"]["export"] == true
        && handshake["functional_evidence"]["import"] == true
        && handshake["addons_ready"] == true
        && super::patch::point_version(&t.version)
            == handshake["anki_version"]
                .as_str()
                .and_then(super::patch::point_version);
    let state = doctor_state(ready, &patch["state"], &live["state"]);
    if let Some(fields) = handshake.as_object_mut() {
        let owner = fields.remove("owner");
        let managed = owner.as_ref().is_some_and(|v| {
            v.as_str().is_some_and(|s| !s.is_empty())
                && read_json(&t.state().join("owner.json"))
                    .ok()
                    .is_some_and(|local| local["owner"] == *v)
        });
        fields.insert("managed_process".into(), json!(managed));
    }
    Ok(
        json!({"state":state,"ready":ready,"executable":t.executable,"version":t.version,"data_dir":t.data,
        "launch_arguments":["-b",t.native_data],"processes":t.discovery["processes"],"running":t.running,
        "installations":t.discovery["installs"],"endpoint":endpoint,"addons":addons,"patch":patch,"live":live,"handshake":handshake,
        "target_platform":t.discovery["platform"],"cli_platform":std::env::consts::OS,
        "limitations":["Остановленная Anki: загрузка модулей и активный профиль не подтверждаются по диску", "Готовность требует настоящих export и import в текущем процессе и профиле"]}),
    )
}

fn live_diagnostics(t: &Target, o: &Options) -> (Value, Value, Value) {
    let mut live = json!({"state":"unverified","profile":null});
    let mut handshake = Value::Null;
    let (endpoint, version) = match super::transport::endpoint(t, o) {
        Ok(endpoint) => (
            json!(endpoint),
            super::transport::call(t, o, "version", json!({})),
        ),
        Err(e) => (Value::Null, Err(e)),
    };
    match version {
        Ok(v) if v.as_u64().is_some_and(|n| n >= 6) => {
            match super::transport::call(t, o, "getActiveProfile", json!({})) {
                Ok(profile) => {
                    live =
                        json!({"state":"ankiconnect_responding","api_version":v,"profile":profile});
                    if o.profile.as_ref().is_some_and(|p| json!(p) != profile) {
                        live["state"] = json!("wrong_profile");
                    }
                    match super::transport::handshake(t, o) {
                        Ok(h) => {
                            handshake = h;
                            if live["state"] != "wrong_profile" {
                                live["state"] = json!("patch_loaded");
                            }
                        }
                        Err(e) => {
                            live["patch_error"] = json!({"code":e.code.as_str(),"details":e.details,"message":e.message})
                        }
                    }
                }
                Err(e) => {
                    live["api_version"] = json!(v);
                    set_live_error(&mut live, e);
                }
            }
        }
        Ok(_) => set_live_error(
            &mut live,
            external_error(
                "wrong_service",
                "AnkiConnect вернула неподдерживаемую версию API",
            ),
        ),
        Err(e) => set_live_error(&mut live, e),
    }
    (endpoint, live, handshake)
}

fn set_live_error(live: &mut Value, error: DomainError) {
    live["state"] = error.details["reason"].clone();
    live["code"] = json!(error.code.as_str());
    live["message"] = json!(error.message);
}

fn doctor_state(ready: bool, patch_state: &Value, live_state: &Value) -> &'static str {
    if ready {
        "ready"
    } else if patch_state == "installed"
        && matches!(
            live_state.as_str(),
            Some("ankiconnect_responding" | "http_unreachable")
        )
    {
        "pending_restart"
    } else {
        "not_ready"
    }
}

pub(crate) fn launch(t: &Target, o: &Options, apply: bool) -> Result<Value> {
    if t.running {
        return Ok(
            json!({"state":"restart_required","ready":false,"reason":"user_process_not_owned"}),
        );
    }
    let profile = o.profile.as_deref().ok_or_else(|| {
        error(
            "profile_required",
            "Укажите --profile для управляемого запуска",
        )
    })?;
    if !apply {
        return Ok(
            json!({"state":"launch_plan","executable":t.executable,"profile":profile,"ready":false}),
        );
    }
    super::patch::ensure_private(&t.state())?;
    let owner = super::patch::random_id()?;
    super::patch::write_atomic(
        &t.state().join("owner.json"),
        &serde_json::to_vec(&json!({"owner":owner,"profile":profile})).map_err(io_error)?,
    )?;
    spawn(t, &[], Some((&owner, profile)))?;
    Ok(json!({"state":"starting","ready":false,"next":"Повторите doctor после загрузки профиля"}))
}

pub(crate) fn spawn(t: &Target, files: &[PathBuf], owner: Option<(&str, &str)>) -> Result<()> {
    if t.discovery["platform"] == "windows" && cfg!(target_os = "linux") {
        let mut args = vec!["-b".to_string(), t.native_data.clone()];
        let prefix = if let Some((token, profile)) = owner {
            args.extend(["-p".into(), profile.into()]);
            format!("$env:ANKI_TOOLKIT_OWNER={};", ps_quote(token))
        } else {
            String::new()
        };
        for p in files {
            args.push(native_path(p)?);
        }
        // Start-Process объединяет ArgumentList: каждый аргумент заключён в кавычки Windows.
        if args.iter().any(|a| a.contains(['"', '\r', '\n'])) {
            return Err(error(
                "unsafe_launch_argument",
                "Кавычки и переводы строк в аргументе запуска не поддержаны",
            ));
        }
        let joined = args
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(" ");
        powershell(&format!(
            "{}$ErrorActionPreference='Stop'; Start-Process -FilePath {} -ArgumentList {}",
            prefix,
            ps_quote(&native_path(&t.executable)?),
            ps_quote(&joined)
        ))?;
    } else {
        let mut c = Command::new(&t.executable);
        c.arg("-b").arg(&t.data);
        if let Some((token, profile)) = owner {
            c.env("ANKI_TOOLKIT_OWNER", token).args(["-p", profile]);
        }
        c.args(files)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(io_error)?;
    }
    Ok(())
}

pub(crate) fn restart(t: &Target, o: &Options, apply: bool) -> Result<Value> {
    let stopped = stop(t, o, apply)?;
    if !apply || stopped["state"] != "stopped" {
        return Ok(stopped);
    }
    let owner = read_json(&t.state().join("owner.json"))?;
    spawn(
        t,
        &[],
        Some((
            owner["owner"].as_str().unwrap_or(""),
            owner["profile"].as_str().unwrap_or(""),
        )),
    )?;
    Ok(
        json!({"state":"starting","ready":false,"next":"Выполните doctor и проверку export/import в перезапущенном процессе"}),
    )
}

pub(crate) fn stop(t: &Target, o: &Options, apply: bool) -> Result<Value> {
    let owner_path = t.state().join("owner.json");
    if !t.running || !owner_path.is_file() {
        return Ok(user_process_not_owned());
    }
    let Ok(owner) = read_json(&owner_path) else {
        return Ok(user_process_not_owned());
    };
    let Ok(h) = super::transport::handshake(t, o) else {
        return Ok(user_process_not_owned());
    };
    if owner["owner"].as_str().is_none_or(|s| s.is_empty())
        || owner["owner"] != h["owner"]
        || owner["profile"] != h["profile"]
    {
        return Ok(user_process_not_owned());
    }
    if !apply {
        return Ok(json!({"state":"stop_plan","ready":false}));
    }
    super::transport::patched(t, o, "toolkitShutdown", json!({"owner":owner["owner"]}))?;
    let end = Instant::now() + Duration::from_secs(45);
    loop {
        let d = discover()?;
        if d["processes"].as_array().is_some_and(|a| a.is_empty()) {
            break;
        }
        if Instant::now() > end {
            return Ok(
                json!({"state":"restart_required","ready":false,"reason":"shutdown_not_confirmed"}),
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(json!({"state":"stopped","ready":false}))
}

fn user_process_not_owned() -> Value {
    json!({"state":"restart_required","ready":false,"reason":"user_process_not_owned"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{Action, Options};

    fn unused_process_probe() -> Result<Value> {
        panic!("Этот тест не должен опрашивать процесс")
    }

    fn options(action: Action) -> Options {
        Options {
            action,
            executable: None,
            data_dir: None,
            profile: Some("Test".into()),
            endpoint: None,
            api_key_env: "ANKICONNECT_API_KEY".into(),
        }
    }

    fn target(tmp: &crate::test_support::TempDir, running: bool, discovery: Value) -> Target {
        Target {
            executable: tmp.path().join("anki"),
            data: tmp.path().join("data"),
            native_data: "test-data".into(),
            version: "26.09.2".into(),
            config: json!({}),
            discovery,
            running,
            process_probe: unused_process_probe,
        }
    }

    #[cfg(unix)]
    #[test]
    fn linux_discovery_deduplicates_canonical_executable_aliases_and_keeps_first_path() {
        let tmp = crate::test_support::TempDir::new("desktop-discovery-path-alias");
        let primary = tmp.path().join("usr/bin");
        let alias = tmp.path().join("bin");
        fs::create_dir_all(&primary).unwrap();
        fs::create_dir_all(&alias).unwrap();
        let executable = primary.join("anki");
        fs::write(&executable, b"synthetic Anki executable").unwrap();
        std::os::unix::fs::symlink(&executable, alias.join("anki")).unwrap();

        let installs = linux_installations([primary, alias]);

        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0]["executable"], json!(executable));
    }

    #[test]
    fn doctor_keeps_diagnostics_when_endpoint_is_unsupported() {
        let tmp = crate::test_support::TempDir::new("desktop-doctor-endpoint-error");
        let target = target(&tmp, true, json!({"wsl":true,"wsl_networking_mode":"nat"}));
        let (endpoint, live, handshake) = live_diagnostics(&target, &options(Action::Doctor));

        assert!(endpoint.is_null());
        assert_eq!(live["state"], "wsl_network_mode_unsupported");
        assert_eq!(live["code"], "external_failure");
        assert!(handshake.is_null());
        assert_eq!(
            doctor_state(false, &json!("installed"), &live["state"]),
            "not_ready"
        );
        assert_eq!(
            doctor_state(false, &json!("installed"), &json!("ankiconnect_responding")),
            "pending_restart"
        );
    }

    #[test]
    fn process_list_is_sorted_by_numeric_pid() {
        let mut processes = vec![json!({"pid":20}), json!({"pid":"3"}), json!({"pid":11})];
        sort_processes(&mut processes);
        assert_eq!(
            processes
                .iter()
                .map(|p| p["pid"]
                    .as_u64()
                    .or_else(|| p["pid"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap())
                .collect::<Vec<_>>(),
            [3, 11, 20]
        );
    }

    #[test]
    fn windows_install_discovery_deduplicates_paths_case_insensitively() {
        let first = json!({"executable":r"C:\Apps\Anki\Anki.exe", "version":"26.9.2"});
        let other = json!({"executable":r"D:\Anki\anki.exe", "version":"26.9.2"});
        let mut installs = vec![
            first.clone(),
            other.clone(),
            first.clone(),
            json!({"executable":r"c:\apps\ANKI\anki.EXE", "version":"duplicate"}),
            json!({"executable":r"d:\anki\ANKI.exe", "version":"duplicate"}),
        ];
        deduplicate_windows_installations(&mut installs);
        assert_eq!(installs, vec![first, other]);
        deduplicate_windows_installations(&mut installs);
        assert_eq!(installs.len(), 2);
    }

    #[test]
    fn stop_returns_restart_required_when_owner_record_is_missing() {
        let tmp = crate::test_support::TempDir::new("desktop-stop-unowned");
        let target = target(&tmp, true, json!({"wsl":true,"wsl_networking_mode":"nat"}));
        let result = stop(&target, &options(Action::Stop { apply: true }), true).unwrap();

        assert_eq!(result["state"], "restart_required");
        assert_eq!(result["reason"], "user_process_not_owned");
    }

    #[test]
    fn stop_returns_restart_required_when_handshake_fails() {
        let tmp = crate::test_support::TempDir::new("desktop-stop-no-handshake");
        let target = target(&tmp, true, json!({"wsl":true,"wsl_networking_mode":"nat"}));
        fs::create_dir_all(target.state()).unwrap();
        fs::write(
            target.state().join("owner.json"),
            br#"{"owner":"owned","profile":"Test"}"#,
        )
        .unwrap();
        let result = stop(&target, &options(Action::Stop { apply: true }), true).unwrap();

        assert_eq!(result["state"], "restart_required");
        assert_eq!(result["reason"], "user_process_not_owned");
    }
}
