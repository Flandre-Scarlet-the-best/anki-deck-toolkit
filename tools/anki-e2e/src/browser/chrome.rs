//! Изолированный тестовый Chromium: собственный временный профиль, загрузки в
//! каталог прогона, без прокси и внешней сети, `forvo.com` — локальный fixture.
//! CDP служит только драйвером тестового браузера (действия «пользователя» в
//! других вкладках и остановка service worker); production-путь работает без CDP.

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::target::{
    CloseTargetParams, GetTargetsParams, TargetInfo,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

use super::fixture::USER_HOST;

/// Выбор исполняемого файла: явный аргумент, затем переменные окружения и PATH.
pub fn find_chrome(explicit: Option<&Path>) -> Option<PathBuf> {
    let executable = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(path) = explicit {
        return executable(path).then(|| path.to_owned());
    }
    for variable in ["CHROME_BIN", "CHROMIUM_BIN"] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
            && executable(&path)
        {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        [
            "chromium",
            "chromium-browser",
            "chrome",
            "google-chrome-for-testing",
        ]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| executable(p))
    })
}

pub struct LaunchOptions<'a> {
    pub executable: &'a Path,
    pub dir: &'a Path,
    pub extension: &'a Path,
    pub fixture_port: u16,
    /// Настройка «Спрашивать, куда сохранять каждый файл» тестового профиля.
    pub prompt_for_download: bool,
}

pub struct TestBrowser {
    child: Child,
    pub browser: Browser,
    pub downloads: PathBuf,
    handler: tokio::task::JoinHandle<()>,
    pub flags: Vec<String>,
}

fn flags(options: &LaunchOptions, profile: &Path) -> Vec<String> {
    let port = options.fixture_port;
    vec![
        "--headless=new".into(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        // Переменные HTTP(S)_PROXY окружения иначе увели бы `forvo.com` на настоящий сайт.
        "--no-proxy-server".into(),
        format!(
            "--host-resolver-rules=MAP forvo.com 127.0.0.1:{port}, MAP {USER_HOST} 127.0.0.1:{port}, MAP * ~NOTFOUND, EXCLUDE 127.0.0.1"
        ),
        // Самоподписанный сертификат fixture допустим только в этом временном профиле.
        "--ignore-certificate-errors".into(),
        format!("--load-extension={}", options.extension.display()),
        format!(
            "--disable-extensions-except={}",
            options.extension.display()
        ),
        "--remote-debugging-port=0".into(),
        "--window-size=1280,900".into(),
        "about:blank".into(),
    ]
}

impl TestBrowser {
    pub async fn launch(options: LaunchOptions<'_>) -> Result<TestBrowser, String> {
        let profile = options.dir.join("profile");
        let downloads = options.dir.join("downloads");
        for dir in [&profile.join("Default"), &downloads] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let preferences = json!({
            "download": {"default_directory": downloads, "prompt_for_download": options.prompt_for_download, "directory_upgrade": true},
            "profile": {"exit_type": "Normal"},
        });
        std::fs::write(
            profile.join("Default/Preferences"),
            serde_json::to_vec(&preferences).expect("JSON"),
        )
        .map_err(|e| format!("Preferences: {e}"))?;
        let flags = flags(&options, &profile);
        let log = std::fs::File::create(options.dir.join("chromium.log"))
            .map_err(|e| format!("chromium.log: {e}"))?;
        let mut child = Command::new(options.executable)
            .args(&flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("запуск Chromium: {e}"))?;
        let active = profile.join("DevToolsActivePort");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let ws = loop {
            if let Ok(text) = std::fs::read_to_string(&active)
                && let (Some(port), Some(path)) = (text.lines().next(), text.lines().nth(1))
            {
                break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("Chromium завершился при запуске: {status}"));
            }
            if tokio::time::Instant::now() >= deadline {
                kill_group(&mut child).await;
                return Err("Chromium не открыл CDP за 30 с".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (browser, mut handler) = match Browser::connect(ws).await {
            Ok(connected) => connected,
            Err(e) => {
                kill_group(&mut child).await;
                return Err(format!("подключение CDP: {e}"));
            }
        };
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        Ok(TestBrowser {
            child,
            browser,
            downloads,
            handler,
            flags,
        })
    }

    /// Закрывает только собственный процесс и его группу.
    pub async fn shutdown(mut self) -> Value {
        let graceful = tokio::time::timeout(Duration::from_secs(5), self.browser.close())
            .await
            .is_ok_and(|r| r.is_ok());
        let exited = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_ok();
        if !exited {
            kill_group(&mut self.child).await;
        }
        self.handler.abort();
        json!({"graceful_close":graceful,"exited_without_kill":exited})
    }

    pub async fn targets(&self) -> Result<Vec<TargetInfo>, String> {
        self.browser
            .execute(GetTargetsParams::default())
            .await
            .map(|r| r.result.target_infos)
            .map_err(|e| format!("CDP targets: {e}"))
    }

    /// Вкладки браузера без вкладки адаптера не различаются по CDP, поэтому
    /// сравниваются только вкладки, которые открыл сам сценарий.
    pub async fn page_urls(&self) -> Result<Vec<String>, String> {
        Ok(self
            .targets()
            .await?
            .into_iter()
            .filter(|t| t.r#type == "page")
            .map(|t| t.url)
            .collect())
    }

    /// Вкладка «пользователя», открытая не расширением.
    pub async fn user_tab(&self, url: &str) -> Result<chromiumoxide::Page, String> {
        let page = self
            .browser
            .new_page(url)
            .await
            .map_err(|e| format!("вкладка пользователя: {e}"))?;
        let _ = page.wait_for_navigation().await;
        Ok(page)
    }

    /// Обычный клик ссылки загрузки на странице пользователя.
    pub async fn click(page: &chromiumoxide::Page, selector: &str) -> Result<(), String> {
        let script = format!(
            "(() => {{ const a = document.querySelector({}); if (!a) return false; a.click(); return true; }})()",
            serde_json::to_string(selector).expect("JSON")
        );
        let clicked: bool = page
            .evaluate(script)
            .await
            .map_err(|e| format!("клик пользователя: {e}"))?
            .into_value()
            .map_err(|e| format!("клик пользователя: {e}"))?;
        if clicked {
            Ok(())
        } else {
            Err(format!("на странице пользователя нет {selector}"))
        }
    }

    /// Пользователь скачивает файл другого сайта: ссылка на `user-files.test`.
    pub async fn user_foreign_download(page: &chromiumoxide::Page) -> Result<(), String> {
        let script = format!(
            "(() => {{ const a = document.createElement('a'); a.href = 'https://{USER_HOST}/notes.bin'; a.id = 'user-file'; document.body.append(a); a.click(); return true; }})()"
        );
        page.evaluate(script)
            .await
            .map_err(|e| format!("загрузка пользователя: {e}"))?;
        Ok(())
    }

    /// Останавливает service worker расширения так же, как браузер выгружает его при простое.
    pub async fn stop_extension_worker(&self, extension_id: &str) -> Result<usize, String> {
        let prefix = format!("chrome-extension://{extension_id}/");
        let workers: Vec<_> = self
            .targets()
            .await?
            .into_iter()
            .filter(|t| t.r#type == "service_worker" && t.url.starts_with(&prefix))
            .collect();
        for worker in &workers {
            self.browser
                .execute(CloseTargetParams::new(worker.target_id.clone()))
                .await
                .map_err(|e| format!("остановка service worker: {e}"))?;
        }
        Ok(workers.len())
    }
}

async fn kill_group(child: &mut Child) {
    if let Some(pid) = child
        .id()
        .and_then(|p| rustix::process::Pid::from_raw(p as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

/// Файлы каталога загрузок с SHA-256: имя → (размер, sha256).
pub fn download_files(dir: &Path) -> Vec<(String, u64, String)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let bytes = std::fs::read(e.path()).ok()?;
            Some((name, bytes.len() as u64, crate::util::sha256_hex(&bytes)))
        })
        .collect();
    files.sort();
    files
}
