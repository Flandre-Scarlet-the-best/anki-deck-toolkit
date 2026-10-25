use super::environment::Target;
use super::{Options, Result, error, external_error, io_error, read_json};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) fn client() -> Result<Client> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(io_error)
}

pub(crate) fn endpoint(target: &Target, options: &Options) -> Result<String> {
    let wsl_mode = target.discovery["wsl_networking_mode"].as_str();
    if target.discovery["wsl"] == true && !matches!(wsl_mode, Some("mirrored" | "wsl1")) {
        return Err(external_error(
            "wsl_network_mode_unsupported",
            format!(
                "Windows Anki через loopback не поддерживается в режиме WSL {}; используйте WSL1 или mirrored mode (Windows 11 22H2+), задав [wsl2] networkingMode=mirrored в %UserProfile%\\.wslconfig, затем перезапустите WSL",
                wsl_mode.unwrap_or("unknown")
            ),
        ));
    }
    let port = target.config["webBindPort"].as_u64().unwrap_or(8765);
    let endpoint = options
        .endpoint
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    let u = reqwest::Url::parse(&endpoint).map_err(|e| error("invalid_endpoint", e.to_string()))?;
    let ip = u
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>();
    if u.scheme() != "http"
        || !ip.is_ok_and(|ip| ip.is_loopback())
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
    {
        return Err(error(
            "unsafe_endpoint",
            "Разрешён только HTTP на числовом loopback без пути и credentials",
        ));
    }
    Ok(endpoint)
}

pub(crate) fn call(
    target: &Target,
    options: &Options,
    action: &str,
    params: Value,
) -> Result<Value> {
    let endpoint = endpoint(target, options)?;
    verify_listener(target, &endpoint)?;
    let key = std::env::var(&options.api_key_env)
        .ok()
        .map(Value::String)
        .unwrap_or_else(|| target.config["apiKey"].clone());
    let response = client()?
        .post(endpoint)
        .json(&json!({"action":action,"version":6,"key":key,"params":params}))
        .send()
        .map_err(|request_error| {
            let mut failure = external_error(
                "http_unreachable",
                "AnkiConnect HTTP не отвечает; это не доказывает остановку Anki",
            );
            if !request_error.is_connect() {
                failure.details["submission_state"] = json!("unknown");
            }
            failure
        })?;
    if !response.status().is_success() {
        return Err(unknown_submission(external_error(
            "wrong_service",
            "На порту HTTP-сервис с неожиданным status",
        )));
    }
    // Чужой сервис не должен заставить диагностику выделять неограниченную память.
    use std::io::Read;
    let mut bytes = Vec::new();
    response
        .take(2_000_001)
        .read_to_end(&mut bytes)
        .map_err(|e| unknown_submission(io_error(e)))?;
    if bytes.len() > 2_000_000 {
        return Err(unknown_submission(external_error(
            "wrong_service",
            "Ответ превышает предел",
        )));
    }
    let response: Value = serde_json::from_slice(&bytes).map_err(|_| {
        unknown_submission(external_error(
            "wrong_service",
            "Ответ не является AnkiConnect envelope",
        ))
    })?;
    if response.get("result").is_none() || response.get("error").is_none() {
        return Err(unknown_submission(external_error(
            "wrong_service",
            "На порту не AnkiConnect API v6",
        )));
    }
    if !response["error"].is_null() {
        let mut failure = external_error(
            "api_error",
            response["error"].as_str().unwrap_or("Ошибка AnkiConnect"),
        );
        failure.details["submission_state"] = json!("rejected");
        return Err(failure);
    }
    Ok(response["result"].clone())
}

fn unknown_submission(mut failure: super::DomainError) -> super::DomainError {
    failure.details["submission_state"] = json!("unknown");
    failure
}

/// Не передаём apiKey/capability постороннему процессу на локальном порту.
fn verify_listener(t: &Target, endpoint: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint).map_err(io_error)?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| error("unsafe_endpoint", "Не задан порт"))?;
    let ip = url
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .map_err(io_error)?;
    let mut inodes = if cfg!(target_os = "linux") {
        linux_listener_inodes(port, ip)?
    } else {
        Vec::new()
    };
    if t.discovery["platform"] == "windows" {
        if !inodes.is_empty() {
            return Err(external_error(
                "wrong_service",
                "Порт перехвачен процессом Linux в WSL; секреты Windows Anki не отправлены",
            ));
        }
        let script = format!(
            "$ErrorActionPreference='Stop'; $listeners=@(Get-NetTCPConnection -State Listen -LocalPort {port} -ErrorAction SilentlyContinue | ForEach-Object {{$p=Get-CimInstance Win32_Process -Filter (\"ProcessId = \"+$_.OwningProcess); @{{pid=$_.OwningProcess; executable=$p.ExecutablePath; address=$_.LocalAddress}}}}); ConvertTo-Json -InputObject $listeners -Compress"
        );
        let listeners: Value =
            serde_json::from_str(&super::environment::powershell(&script)?).map_err(io_error)?;
        let entries = listeners.as_array().ok_or_else(|| {
            external_error(
                "listener_owner_unverified",
                "Не подтверждён владелец порта Windows",
            )
        })?;
        let entries: Vec<_> = entries
            .iter()
            .filter(|e| {
                e["address"]
                    .as_str()
                    .and_then(|s| s.parse::<std::net::IpAddr>().ok())
                    .is_some_and(|address| address == ip || address.is_unspecified())
            })
            .collect();
        if entries.is_empty() {
            return Err(external_error(
                "http_unreachable",
                "Для этой Windows Anki нет слушателя на выбранном порту",
            ));
        }
        let expected = super::environment::native_path(&t.executable)?.to_lowercase();
        if !entries.iter().all(|e| {
            e["executable"]
                .as_str()
                .is_some_and(|s| s.to_lowercase() == expected)
        }) {
            return Err(external_error(
                "wrong_service",
                "Порт принадлежит другому процессу; секреты не отправлены",
            ));
        }
        return Ok(());
    }
    if inodes.is_empty() {
        return Err(external_error(
            "http_unreachable",
            "Нет слушателя на выбранном порту",
        ));
    }
    let expected = t.executable.canonicalize().map_err(io_error)?;
    for process in t.discovery["processes"].as_array().into_iter().flatten() {
        let correct = process["command_line"].as_array().is_some_and(|args| {
            args.iter().take(3).any(|arg| {
                arg.as_str().is_some_and(|s| {
                    std::path::Path::new(s).canonicalize().ok().as_ref() == Some(&expected)
                })
            })
        });
        if !correct {
            continue;
        }
        let pid = process["pid"].as_str().unwrap_or("");
        if !pid.bytes().all(|b| b.is_ascii_digit()) || pid.is_empty() {
            continue;
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                if let Ok(path) = std::fs::read_link(fd.path()) {
                    inodes.retain(|s| path != std::path::Path::new(s));
                }
            }
        }
    }
    if inodes.is_empty() {
        Ok(())
    } else {
        Err(external_error(
            "wrong_service",
            "Не подтверждено владение портом выбранной Anki; секреты не отправлены",
        ))
    }
}

fn proc_address_matches(hex: &str, ip: std::net::IpAddr) -> bool {
    let address = match hex.len() {
        8 => u32::from_str_radix(hex, 16)
            .ok()
            .map(|n| std::net::IpAddr::V4(n.to_le_bytes().into())),
        32 => {
            let mut bytes = [0u8; 16];
            for (i, part) in hex.as_bytes().chunks(8).enumerate() {
                let Ok(n) = u32::from_str_radix(std::str::from_utf8(part).unwrap_or(""), 16) else {
                    return false;
                };
                bytes[i * 4..i * 4 + 4].copy_from_slice(&n.to_le_bytes());
            }
            Some(std::net::IpAddr::V6(bytes.into()))
        }
        _ => None,
    };
    address.is_some_and(|address| address == ip || address.is_unspecified())
}
fn linux_listener_inodes(port: u16, ip: std::net::IpAddr) -> Result<Vec<String>> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = std::fs::read_to_string(table).map_err(io_error)?;
        for line in text.lines().skip(1) {
            let f: Vec<_> = line.split_whitespace().collect();
            if f.len() <= 9 || f[3] != "0A" {
                continue;
            }
            let Some((address, p)) = f[1].rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(p, 16).ok() == Some(port) && proc_address_matches(address, ip) {
                inodes.push(format!("socket:[{}]", f[9]));
            }
        }
    }
    Ok(inodes)
}

pub(crate) fn patched(
    target: &Target,
    options: &Options,
    action: &str,
    mut params: Value,
) -> Result<Value> {
    let settings = read_json(&target.bridge().join("settings.json"))?;
    params["token"] = settings["token"].clone();
    params["profile"] = options
        .profile
        .clone()
        .map(Value::String)
        .unwrap_or(Value::Null);
    params["installation"] = json!(target.native_data);
    call(target, options, action, params)
}

pub(crate) fn handshake(target: &Target, options: &Options) -> Result<Value> {
    let nonce = super::patch::random_id()?;
    let v = patched(target, options, "toolkitStatus", json!({"nonce":nonce}))?;
    if v["nonce"] != nonce || v["patch_version"] != super::PATCH_VERSION || v["protocol"] != 1 {
        return Err(error(
            "invalid_handshake",
            "Патч другого поколения или неверный challenge",
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn proc_addresses_distinguish_loopback_and_specific_addresses() {
        let v4 = "127.0.0.1".parse().unwrap();
        assert!(proc_address_matches("0100007F", v4));
        assert!(proc_address_matches("00000000", v4));
        assert!(!proc_address_matches("0200007F", v4));
        assert!(proc_address_matches(
            "00000000000000000000000001000000",
            "::1".parse().unwrap()
        ));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_listener_receives_no_request_or_secret() {
        let tmp = crate::test_support::TempDir::new("desktop-listener");
        let exe = tmp.path().join("anki");
        std::fs::write(&exe, b"synthetic executable").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let t = Target {
            executable: exe,
            data: tmp.path().to_path_buf(),
            native_data: String::new(),
            version: String::new(),
            config: json!({"apiKey":"must-not-be-sent"}),
            discovery: json!({"platform":"linux","processes":[]}),
            running: false,
            process_probe: || Ok(json!({"processes":[]})),
        };
        let url = format!("http://{address}");
        let cli =
            crate::cli::Cli::try_parse_from(["anki-repo", "anki", "--endpoint", &url, "doctor"])
                .unwrap();
        let crate::cli::Command::Anki { options } = cli.command else {
            unreachable!()
        };
        assert_eq!(
            call(&t, &options, "version", json!({}))
                .unwrap_err()
                .details["reason"],
            "wrong_service"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn remote_or_credentialed_endpoints_never_receive_the_key() {
        let t = Target {
            executable: "/unused".into(),
            data: "/unused".into(),
            native_data: "/unused".into(),
            version: String::new(),
            config: json!({}),
            discovery: json!({}),
            running: false,
            process_probe: || Ok(json!({"processes":[]})),
        };
        for url in [
            "https://127.0.0.1:8765",
            "http://example.com:8765",
            "http://127.0.0.1:8765/path",
            "http://secret@127.0.0.1:8765",
            "http://127.0.0.1:8765?key=x",
        ] {
            let cli =
                crate::cli::Cli::try_parse_from(["anki-repo", "anki", "--endpoint", url, "doctor"])
                    .unwrap();
            let crate::cli::Command::Anki { options } = cli.command else {
                unreachable!()
            };
            assert!(endpoint(&t, &options).is_err(), "{url}");
        }
    }

    #[test]
    fn wsl_unsupported_modes_are_rejected_with_a_mirrored_mode_diagnostic() {
        let mut t = Target {
            executable: "/unused".into(),
            data: "/unused".into(),
            native_data: "/unused".into(),
            version: String::new(),
            config: json!({}),
            discovery: json!({"wsl":true,"wsl_networking_mode":"nat"}),
            running: false,
            process_probe: || Ok(json!({"processes":[]})),
        };
        let cli = crate::cli::Cli::try_parse_from(["anki-repo", "anki", "doctor"]).unwrap();
        let crate::cli::Command::Anki { options } = cli.command else {
            unreachable!()
        };
        let failure = endpoint(&t, &options).unwrap_err();
        assert_eq!(failure.details["reason"], "wsl_network_mode_unsupported");
        assert_eq!(failure.code.as_str(), "external_failure");
        assert_eq!(failure.exit_code(), 9);
        assert!(failure.message.contains("networkingMode=mirrored"));

        t.discovery["wsl_networking_mode"] = Value::Null;
        let failure = endpoint(&t, &options).unwrap_err();
        assert!(failure.message.contains("unknown"));

        t.discovery["wsl_networking_mode"] = json!("mirrored");
        assert_eq!(endpoint(&t, &options).unwrap(), "http://127.0.0.1:8765");

        t.discovery["wsl"] = json!(false);
        t.discovery["wsl_networking_mode"] = Value::Null;
        assert_eq!(endpoint(&t, &options).unwrap(), "http://127.0.0.1:8765");
    }
}
