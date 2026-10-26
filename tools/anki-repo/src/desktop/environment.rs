use super::{
    CONNECT, CROWD, DomainError, Options, Result, error, external_error, io_error, read_json,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

const POWERSHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Наблюдаемые признаки окружения. Ни один из них в отдельности не доказывает
/// платформу: маркеры WSL видны и при отключённом interop, а контейнер на ядре
/// WSL2 видит то же `osrelease`, что и дистрибутив.
#[derive(Debug, Clone, Copy, Default)]
struct Signals {
    wsl_kernel: bool,
    /// `/.dockerenv` или `/run/.containerenv`.
    container: bool,
    /// `WSL_DISTRO_NAME`, `WSL_INTEROP` или `/run/WSL` штатного дистрибутива.
    wsl_distro: bool,
}

impl Signals {
    fn current() -> Signals {
        let osrelease = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        Signals {
            wsl_kernel: is_wsl_kernel(&osrelease),
            container: Path::new("/.dockerenv").exists()
                || Path::new("/run/.containerenv").exists(),
            wsl_distro: std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::env::var_os("WSL_INTEROP").is_some()
                || Path::new("/run/WSL").is_dir(),
        }
    }
}

fn is_wsl_kernel(osrelease: &str) -> bool {
    osrelease.to_lowercase().contains("microsoft")
}

/// Результат короткого запуска `powershell.exe` из Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    Available,
    /// Запуск невозможен: нет программы или binfmt interop отключён.
    Unavailable(String),
    Failed(String),
    TimedOut,
}

impl Probe {
    fn json(&self) -> Value {
        match self {
            Probe::Available => json!({"state":"available"}),
            Probe::Unavailable(detail) => json!({"state":"unavailable","detail":detail}),
            Probe::Failed(detail) => json!({"state":"failed","detail":detail}),
            Probe::TimedOut => {
                json!({"state":"timed_out","timeout_s":POWERSHELL_PROBE_TIMEOUT.as_secs()})
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Environment {
    Linux,
    WindowsNative,
    /// Штатный WSL с работающим interop: целевая установка — Windows Anki.
    WslInterop,
    /// Изолированный Linux-контейнер на ядре WSL2 без признаков дистрибутива.
    LinuxContainerOnWsl,
    /// Штатный WSL, но Windows interop не ответил.
    WslInteropUnavailable,
    /// Ядро WSL, interop не ответил, признаки противоречивы или отсутствуют.
    Undetermined,
}

impl Environment {
    fn as_str(self) -> &'static str {
        match self {
            Environment::Linux => "linux",
            Environment::WindowsNative => "windows",
            Environment::WslInterop => "wsl_interop",
            Environment::LinuxContainerOnWsl => "linux_container_on_wsl_kernel",
            Environment::WslInteropUnavailable => "wsl_interop_unavailable",
            Environment::Undetermined => "undetermined_wsl_kernel",
        }
    }

    /// Linux-установку можно выбрать без явных `--executable` и `--data-dir`.
    fn linux_selection_automatic(self) -> bool {
        matches!(self, Environment::Linux | Environment::LinuxContainerOnWsl)
    }
}

/// Контейнер без признаков дистрибутива не пробует interop: там его нет по
/// построению. В остальных случаях на ядре WSL решает фактический запуск
/// PowerShell, а его отказ никогда не превращается в обычный Linux.
fn classify(signals: Signals, probe: impl FnOnce() -> Probe) -> (Environment, Option<Probe>) {
    if cfg!(target_os = "windows") {
        return (Environment::WindowsNative, None);
    }
    if !signals.wsl_kernel {
        return (Environment::Linux, None);
    }
    if signals.container && !signals.wsl_distro {
        return (Environment::LinuxContainerOnWsl, None);
    }
    let result = probe();
    let environment = match result {
        Probe::Available => Environment::WslInterop,
        _ if signals.wsl_distro && !signals.container => Environment::WslInteropUnavailable,
        _ => Environment::Undetermined,
    };
    (environment, Some(result))
}

fn probe_windows_powershell() -> Probe {
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    run_probe(command, POWERSHELL_PROBE_TIMEOUT)
}

/// Запуск пробы с ограниченным сроком: зависший interop не блокирует CLI.
fn run_probe(mut command: Command, timeout: Duration) -> Probe {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Probe::Unavailable(e.to_string()),
    };
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = pipe.read_to_end(&mut text);
            text
        })
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Probe::Failed(e.to_string()),
        }
    };
    if status.success() {
        return Probe::Available;
    }
    let text = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let text: String = String::from_utf8_lossy(&text)
        .trim()
        .chars()
        .take(500)
        .collect();
    Probe::Failed(format!("{status}: {text}"))
}

fn named_anki(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == "anki")
}

/// `anki`, `python [опции] -m aqt` или `python [опции] …/anki`.
fn is_anki_command_line(args: &[&str]) -> bool {
    let Some(executable) = args.first() else {
        return false;
    };
    if named_anki(executable) {
        return true;
    }
    let python = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("python"));
    python && python_runs_anki(&args[1..])
}

/// Разбор опций интерпретатора до модуля или скрипта: аргументы самого
/// приложения (например, `anki-repo … anki`) не рассматриваются.
fn python_runs_anki(args: &[&str]) -> bool {
    let mut rest = args.iter().copied();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            return rest.next().is_some_and(named_anki);
        }
        if let Some(long) = arg.strip_prefix("--") {
            // Единственная длинная опция интерпретатора со значением; остальные
            // (`--help`, `--version`) приложение не запускают.
            if long == "check-hash-based-pycs" {
                rest.next();
                continue;
            }
            if long.starts_with("check-hash-based-pycs=") {
                continue;
            }
            return false;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            return named_anki(arg);
        };
        for (index, flag) in flags.char_indices() {
            let value = &flags[index + flag.len_utf8()..];
            match flag {
                'm' => {
                    let module = if value.is_empty() {
                        rest.next().unwrap_or("")
                    } else {
                        value
                    };
                    return module == "aqt";
                }
                'c' => return false,
                'W' | 'X' => {
                    if value.is_empty() {
                        rest.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    false
}

/// `anki --version` официальной Linux-сборки печатает также строку запуска.
fn parse_version_output(stdout: &str) -> String {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Anki "))
        .unwrap_or_else(|| stdout.trim())
        .trim()
        .to_owned()
}

pub(crate) fn discover() -> Result<Value> {
    let (environment, probe) = classify(Signals::current(), probe_windows_powershell);
    let interop = probe.as_ref().map_or(Value::Null, Probe::json);
    if matches!(
        environment,
        Environment::WslInterop | Environment::WindowsNative
    ) {
        let wsl = environment == Environment::WslInterop;
        let mut discovery: Value =
            serde_json::from_str(&powershell(WINDOWS_DISCOVERY_SCRIPT)?).map_err(io_error)?;
        discovery["wsl"] = json!(wsl);
        discovery["environment"] = json!(environment.as_str());
        discovery["windows_interop"] = interop;
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
    let processes = linux_processes()?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("Anki2");
    Ok(
        json!({"platform":"linux","wsl":false,"environment":environment.as_str(),
        "windows_interop":interop,
        "explicit_selection_required":!environment.linux_selection_automatic(),
        "installs":installs,"processes":processes,"default_data":data}),
    )
}

fn linux_processes() -> Result<Vec<Value>> {
    let mut processes = Vec::new();
    let own = std::env::current_exe().and_then(fs::canonicalize).ok();
    for entry in fs::read_dir("/proc").map_err(io_error)?.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        // Сам CLI (`anki-repo anki …`) и его параллельные запуски — не процесс Anki.
        let exe = fs::read_link(entry.path().join("exe")).ok();
        if own.is_some() && exe == own {
            continue;
        }
        let cmd = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let args = String::from_utf8_lossy(&cmd);
        let args: Vec<_> = args.split('\0').filter(|s| !s.is_empty()).collect();
        if is_anki_command_line(&args) {
            processes.push(json!({"pid":entry.file_name().to_string_lossy(),"command_line":args}));
        }
    }
    sort_processes(&mut processes);
    Ok(processes)
}

/// В WSL с отказавшим interop или в неопределённом окружении Linux Anki не
/// выбирается молча: нужна явная пара `--executable` и `--data-dir`.
fn require_valid_selection(discovery: &Value, o: &Options) -> Result<()> {
    if discovery["explicit_selection_required"] != true
        || (o.executable.is_some() && o.data_dir.is_some())
    {
        return Ok(());
    }
    Err(crate::error::DomainError::with_details(
        crate::error::ErrorCode::InvalidRequest,
        "Ядро WSL, но Windows interop не подтверждён; Windows Anki недоступна, а Linux Anki автоматически не выбирается. Восстановите interop или явно задайте --executable и --data-dir",
        json!({"reason":"windows_interop_unavailable","environment":discovery["environment"],
            "windows_interop":discovery["windows_interop"],"discovery":discovery}),
    ))
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
    require_valid_selection(&discovery, o)?;
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
        parse_version_output(&String::from_utf8_lossy(&out.stdout))
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
    // Пустой список процессов другой платформы не доказывает остановку целевой Anki.
    if current["platform"] != t.discovery["platform"] {
        return Err(crate::error::DomainError::with_details(
            crate::error::ErrorCode::InvalidRequest,
            "Окружение изменилось после выбора установки; изменение аддонов отменено",
            json!({"reason":"environment_changed","selected":t.discovery["platform"],
                "current":current["platform"],"environment":current["environment"],
                "windows_interop":current["windows_interop"]}),
        ));
    }
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
        "environment":t.discovery["environment"],"windows_interop":t.discovery["windows_interop"],
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
    wait_for_shutdown(t)
}

fn wait_for_shutdown(t: &Target) -> Result<Value> {
    let end = Instant::now() + Duration::from_secs(45);
    loop {
        let d = (t.process_probe)()?;
        if d["platform"] != t.discovery["platform"] {
            return Ok(json!({"state":"restart_required","ready":false,
                "reason":"environment_changed","selected":t.discovery["platform"],
                "current":d["platform"],"environment":d["environment"],
                "windows_interop":d["windows_interop"]}));
        }
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
    fn linux_version_output_with_startup_line_yields_the_version() {
        assert_eq!(
            parse_version_output("Starting Anki 26.09.3...\nAnki 26.09.3\n"),
            "26.09.3"
        );
        assert_eq!(parse_version_output("Anki 26.09.2"), "26.09.2");
        assert_eq!(parse_version_output("26.9.2\n"), "26.9.2");
    }

    #[test]
    fn cli_arguments_are_not_mistaken_for_anki_but_anki_forms_are() {
        assert!(is_anki_command_line(&[
            "/usr/local/bin/anki",
            "-b",
            "/data"
        ]));
        assert!(is_anki_command_line(&["python3", "-m", "aqt"]));
        assert!(is_anki_command_line(&["python3", "/opt/anki"]));
        assert!(!is_anki_command_line(&[
            "/usr/local/bin/anki-repo",
            "--json",
            "anki"
        ]));
        assert!(!is_anki_command_line(&["/usr/bin/cat", "/home/user/anki"]));
        assert!(!is_anki_command_line(&["python3", "-m", "other", "aqt"]));
        assert!(!is_anki_command_line(&["/usr/bin/Xvfb", ":99"]));
        assert!(!is_anki_command_line(&[]));
    }

    #[test]
    fn python_interpreter_options_before_anki_are_recognised() {
        for args in [
            &["python3", "-u", "-m", "aqt"][..],
            &["python3", "-I", "-m", "aqt", "-b", "/data"],
            &["/usr/bin/python3.12", "-uB", "-maqt"],
            &["python3", "-X", "dev", "-W", "ignore", "-m", "aqt"],
            &["python3", "-Xutf8", "-Wdefault", "-m", "aqt"],
            &["python3", "--check-hash-based-pycs", "never", "-m", "aqt"],
            &["python3", "-O", "/opt/anki/bin/anki"],
            &["python3", "-s", "--", "/usr/local/bin/anki"],
        ] {
            assert!(is_anki_command_line(args), "{args:?}");
        }
        for args in [
            &["python3", "-c", "import aqt; aqt.run()"][..],
            &["python3", "-uc", "-m aqt"],
            &["python3", "-m", "aqtx"],
            &["python3", "-m", "pip", "install", "aqt"],
            &["python3", "-W", "aqt"],
            &["python3", "-X", "-m", "aqt"],
            &["python3", "script.py", "anki"],
            &["python3", "-", "anki"],
            &["python3", "--version", "-m", "aqt"],
            &["python3", "-u"],
            &["/usr/local/bin/anki-repo", "anki", "-m", "aqt"],
        ] {
            assert!(!is_anki_command_line(args), "{args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn running_python_aqt_process_with_interpreter_options_is_found_in_proc() {
        let Ok(python) = Command::new("python3")
            .args(["-I", "-c", "import sys; print(sys.executable)"])
            .output()
        else {
            eprintln!("python3 недоступен; проверка реального процесса пропущена");
            return;
        };
        if !python.status.success() {
            return;
        }
        let tmp = crate::test_support::TempDir::new("desktop-python-aqt-process");
        fs::create_dir_all(tmp.path().join("aqt")).unwrap();
        fs::write(
            tmp.path().join("aqt/__main__.py"),
            b"import time\ntime.sleep(30)\n",
        )
        .unwrap();
        // Без -I: `-m` ищет модуль в текущем каталоге, как у разработческого запуска.
        let mut child = Command::new("python3")
            .args(["-u", "-B", "-m", "aqt"])
            .current_dir(tmp.path())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id().to_string();
        let found = (0..50).any(|_| {
            let seen = linux_processes()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == pid.as_str());
            if !seen {
                std::thread::sleep(Duration::from_millis(20));
            }
            seen
        });
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "python3 -u -B -m aqt (pid {pid}) не распознан");
    }

    fn signals(wsl_kernel: bool, container: bool, wsl_distro: bool) -> Signals {
        Signals {
            wsl_kernel,
            container,
            wsl_distro,
        }
    }

    #[test]
    fn environment_matrix_never_falls_back_to_linux_when_wsl_interop_fails() {
        let failed = || Probe::Failed("exit 1".into());
        // Обычный WSL с работающим interop.
        assert_eq!(
            classify(signals(true, false, true), || Probe::Available).0,
            Environment::WslInterop
        );
        // Обычный WSL с отключённым или отказавшим interop.
        for probe in [
            Probe::Unavailable("Exec format error".into()),
            Probe::Failed("exit 1".into()),
            Probe::TimedOut,
        ] {
            let (environment, observed) = classify(signals(true, false, true), || probe.clone());
            assert_eq!(environment, Environment::WslInteropUnavailable);
            assert_eq!(observed, Some(probe));
            assert!(!environment.linux_selection_automatic());
        }
        // E2E-контейнер на ядре WSL2: interop не пробуется, выбор Linux штатный.
        let (environment, observed) =
            classify(signals(true, true, false), || panic!("проба в контейнере"));
        assert_eq!(environment, Environment::LinuxContainerOnWsl);
        assert!(observed.is_none() && environment.linux_selection_automatic());
        // Ядро WSL без признаков либо с противоречивыми признаками.
        assert_eq!(
            classify(signals(true, false, false), failed).0,
            Environment::Undetermined
        );
        assert_eq!(
            classify(signals(true, true, true), failed).0,
            Environment::Undetermined
        );
        assert_eq!(
            classify(signals(true, true, true), || Probe::Available).0,
            Environment::WslInterop
        );
        // Обычный Linux не опрашивает PowerShell.
        let (environment, observed) =
            classify(signals(false, false, false), || panic!("проба вне WSL"));
        assert_eq!(environment, Environment::Linux);
        assert!(observed.is_none() && environment.linux_selection_automatic());
    }

    #[test]
    fn single_linux_anki_in_wsl_without_interop_requires_explicit_selection() {
        let discovery = json!({"platform":"linux","environment":"wsl_interop_unavailable",
            "windows_interop":{"state":"failed"},"explicit_selection_required":true,
            "installs":[{"executable":"/usr/bin/anki","version":null}]});
        let mut o = options(Action::Setup { apply: true });
        let error = require_valid_selection(&discovery, &o).unwrap_err();
        assert_eq!(error.details["reason"], "windows_interop_unavailable");
        assert_eq!(error.details["windows_interop"]["state"], "failed");

        o.executable = Some("/usr/bin/anki".into());
        assert!(require_valid_selection(&discovery, &o).is_err());
        o.data_dir = Some("/home/user/.local/share/Anki2".into());
        assert!(require_valid_selection(&discovery, &o).is_ok());

        let container = json!({"platform":"linux","explicit_selection_required":false});
        assert!(require_valid_selection(&container, &options(Action::Doctor)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn powershell_probe_is_bounded_and_keeps_failure_details() {
        let started = Instant::now();
        let mut hanging = Command::new("sleep");
        hanging.arg("30").stdin(Stdio::null()).stdout(Stdio::null());
        assert_eq!(
            run_probe(hanging, Duration::from_millis(200)),
            Probe::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let missing = Command::new("/nonexistent/powershell.exe");
        assert!(matches!(
            run_probe(missing, Duration::from_secs(5)),
            Probe::Unavailable(_)
        ));

        let mut failing = Command::new("sh");
        failing
            .args(["-c", "echo interop отключён >&2; exit 3"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let Probe::Failed(detail) = run_probe(failing, Duration::from_secs(5)) else {
            panic!("ожидался отказ пробы");
        };
        assert!(detail.contains("interop отключён"));
    }

    fn running_linux_probe() -> Result<Value> {
        Ok(
            json!({"platform":"linux","processes":[{"pid":"42","command_line":["python3","-u","-m","aqt"]}]}),
        )
    }

    fn linux_probe_without_processes() -> Result<Value> {
        Ok(json!({"platform":"linux","environment":"wsl_interop_unavailable","processes":[]}))
    }

    #[test]
    fn running_anki_or_changed_environment_blocks_addon_mutation() {
        let tmp = crate::test_support::TempDir::new("desktop-require-stopped");
        let mut linux = target(&tmp, false, json!({"platform":"linux"}));
        linux.process_probe = running_linux_probe;
        assert_eq!(
            require_stopped(&linux).unwrap_err().details["reason"],
            "restart_required"
        );
        linux.process_probe = linux_probe_without_processes;
        assert!(require_stopped(&linux).is_ok());

        // Windows Anki выбрана, затем interop отказал: пустой Linux-список не доказательство.
        let mut windows = target(&tmp, false, json!({"platform":"windows","wsl":true}));
        windows.process_probe = linux_probe_without_processes;
        let error = require_stopped(&windows).unwrap_err();
        assert_eq!(error.details["reason"], "environment_changed");
        assert_eq!(error.details["environment"], "wsl_interop_unavailable");
    }

    #[test]
    fn shutdown_requires_an_empty_process_list_from_the_selected_platform() {
        let tmp = crate::test_support::TempDir::new("desktop-shutdown-environment");
        let mut t = target(&tmp, true, json!({"platform":"windows","wsl":true}));
        t.process_probe = linux_probe_without_processes;
        let result = wait_for_shutdown(&t).unwrap();
        assert_eq!(result["state"], "restart_required");
        assert_eq!(result["reason"], "environment_changed");
        assert_eq!(result["environment"], "wsl_interop_unavailable");
        assert_eq!(result["selected"], "windows");
        assert_eq!(result["current"], "linux");
        assert_eq!(result["ready"], false);

        t.discovery = json!({"platform":"linux"});
        assert_eq!(wait_for_shutdown(&t).unwrap()["state"], "stopped");
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
