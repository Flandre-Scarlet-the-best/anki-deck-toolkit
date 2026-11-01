//! Обычный сайт Forvo в принадлежащей приложению постоянной браузерной сессии.
use crate::{AssetError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
pub mod batch;
pub mod brave;
mod candidates;
mod download;
mod evidence;
use crate::browser_runtime::{BrowserRuntimeConfig, BrowserSession};
pub use candidates::{Candidate, Gender, RANKING, rank, word_url};
pub use evidence::{Acquisition, Attempt, ForvoEvidence, acquire, import_acquisition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    ChallengeRequired,
    LoginRequired,
    Authenticated,
    SessionExpired,
    AccessDenied,
    SiteUnavailable,
    PageUnstable,
}

pub fn error(code: &str, action: &str) -> AssetError {
    AssetError::with_details(
        ErrorCode::InvalidTransition,
        action,
        serde_json::json!({"provider":"forvo_web", "code":code,"next_action":action}),
    )
}
pub struct ForvoSession {
    browser: Option<BrowserSession>,
    pending_stop: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pending_launch: Option<tokio::task::JoinHandle<Result<BrowserSession, AssetError>>>,
    was_authenticated: bool,
    download_used: bool,
    profile: PathBuf,
    visible: bool,
}

impl ForvoSession {
    pub async fn launch(profile: &Path) -> Result<Self, AssetError> {
        Self::launch_mode(profile, false).await
    }
    pub async fn launch_interactive(profile: &Path) -> Result<Self, AssetError> {
        Self::launch_mode(profile, true).await
    }
    async fn launch_mode(profile: &Path, visible: bool) -> Result<Self, AssetError> {
        let launch = Self::launch_owned(profile, visible);
        tokio::pin!(launch);
        tokio::select! {
            result=&mut launch=>result,
            _=tokio::signal::ctrl_c()=>{
                match launch.await {
                    Ok(session) => session.close().await?,
                    Err(e) if provider_code(&e)=="browser_cleanup_failed" => return Err(e),
                    Err(_) => (),
                }
                Err(AssetError::new(ErrorCode::Interrupted,"Запуск Forvo прерван; принадлежащий браузер закрыт"))
            }
        }
    }
    async fn launch_owned(profile: &Path, visible: bool) -> Result<Self, AssetError> {
        let browser = Self::open_browser(profile, visible).await?;
        Ok(Self {
            browser: Some(browser),
            pending_stop: None,
            pending_launch: None,
            was_authenticated: std::fs::symlink_metadata(profile.join(".forvo-authenticated"))
                .is_ok_and(|m| m.is_file()),
            download_used: false,
            profile: profile.to_path_buf(),
            visible,
        })
    }
    async fn open_browser(profile: &Path, visible: bool) -> Result<BrowserSession, AssetError> {
        tracing::info!(
            event = "forvo_browser",
            stage = "browser_launch",
            mode = if visible { "visible" } else { "headless" },
            "Запуск браузера Forvo"
        );
        if visible {
            BrowserSession::launch_persistent(BrowserRuntimeConfig::default(), profile).await
        } else {
            BrowserSession::launch_persistent_background(BrowserRuntimeConfig::default(), profile)
                .await
        }
        .map_err(|e| {
            let cleanup_failed = matches!(
                e,
                crate::browser_runtime::BrowserLaunchError::CleanupFailed { .. }
            );
            let message = e.to_string();
            let code = if cleanup_failed {
                "browser_cleanup_failed"
            } else {
                [
                    "gui_unavailable",
                    "browser_profile_busy",
                    "browser_profile_not_owned",
                ]
                .into_iter()
                .find(|code| message.contains(code))
                .unwrap_or("browser_recovery_required")
            };
            error(
                code,
                "Проверьте GUI, Chromium и свободный собственный профиль; повторите forvo login",
            )
        })
    }
    fn browser(&self) -> &BrowserSession {
        self.browser.as_ref().expect("открытый сеанс Forvo")
    }
    async fn stop_browser(&mut self) -> Result<(), AssetError> {
        use tracing::instrument::WithSubscriber;
        if self.pending_stop.is_none()
            && let Some(browser) = self.browser.take()
        {
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            self.pending_stop = Some(tokio::spawn(
                async move { browser.close().await }.with_subscriber(dispatch),
            ));
        }
        let result = match self.pending_stop.as_mut() {
            Some(task) => match task.await {
                Ok(result) => result.map_err(|_| {
                    error(
                        "browser_cleanup_failed",
                        "Не удалось подтвердить закрытие принадлежащего Chromium",
                    )
                }),
                Err(_) => Err(error(
                    "browser_cleanup_failed",
                    "Задача остановки Chromium завершилась аварийно",
                )),
            },
            None => Ok(()),
        };
        self.pending_stop.take();
        result
    }
    async fn start_browser(&mut self, visible: bool) -> Result<(), AssetError> {
        use tracing::instrument::WithSubscriber;
        if self.pending_launch.is_none() {
            let profile = self.profile.clone();
            let dispatch = tracing::dispatcher::get_default(Clone::clone);
            self.pending_launch = Some(tokio::spawn(
                async move { Self::open_browser(&profile, visible).await }
                    .with_subscriber(dispatch),
            ));
        }
        let result = match self.pending_launch.as_mut().expect("запуск браузера").await
        {
            Ok(result) => result,
            Err(_) => Err(error(
                "browser_cleanup_failed",
                "Задача запуска Chromium завершилась аварийно",
            )),
        };
        self.pending_launch.take();
        self.browser = Some(result?);
        self.visible = visible;
        Ok(())
    }
    async fn recycle_after_download(&mut self) -> Result<(), AssetError> {
        if self.download_used {
            self.stop_browser().await?;
            self.start_browser(self.visible).await?;
            self.download_used = false;
        }
        Ok(())
    }
    pub(crate) async fn recover_interactively(
        &mut self,
        surface: Option<&str>,
    ) -> Result<bool, AssetError> {
        if std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty())
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty())
        {
            return Ok(false);
        }
        self.stop_browser().await?;
        tracing::info!(
            event = "forvo_interaction",
            stage = "authentication",
            "Требуется ручное взаимодействие с Forvo"
        );
        eprintln!("Для продолжения войдите в Forvo или завершите проверку в открывшемся окне.");
        self.start_browser(true).await?;
        let result = self
            .check_at(true, Duration::from_secs(300), surface)
            .await
            .and_then(|state| {
                if state == AuthState::Authenticated {
                    Ok(())
                } else {
                    Err(auth_error(state))
                }
            });
        let stopped = self.stop_browser().await;
        match (result, stopped) {
            (Err(mut original), Err(cleanup)) => {
                original.details["browser_cleanup"] =
                    serde_json::json!({"code":provider_code(&cleanup), "message":cleanup.message});
                return Err(original);
            }
            (Err(original), _) => return Err(original),
            (_, Err(cleanup)) => return Err(cleanup),
            _ => (),
        }
        self.start_browser(false).await?;
        self.was_authenticated = true;
        self.download_used = false;
        Ok(true)
    }
    pub async fn close(mut self) -> Result<(), AssetError> {
        let launch_result = if let Some(task) = self.pending_launch.take() {
            match task.await {
                Ok(Ok(browser)) => {
                    self.browser = Some(browser);
                    Ok(())
                }
                Ok(Err(error)) if provider_code(&error) != "browser_cleanup_failed" => Ok(()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(error(
                    "browser_cleanup_failed",
                    "Задача запуска Chromium завершилась аварийно",
                )),
            }
        } else {
            Ok(())
        };
        let stopped = self.stop_browser().await;
        tracing::info!(
            event = "forvo_browser",
            stage = "browser_stop",
            success = stopped.is_ok(),
            "Остановка браузера Forvo завершена"
        );
        launch_result.and(stopped)
    }
    pub async fn navigate(&self, url: &str) -> Result<(), AssetError> {
        self.browser().telemetry().begin_epoch();
        // Некоторые страницы не завершают load из-за рекламы или проверки доступа.
        // После ограниченного ожидания решение принимается по наблюдаемой готовности DOM и признакам авторизации.
        let _ = tokio::time::timeout(Duration::from_secs(8), self.browser().page().goto(url)).await;
        Ok(())
    }
    pub async fn auth_state(&mut self) -> Result<AuthState, AssetError> {
        let raw = observe_auth(self.browser().page()).await?;
        tracing::debug!(event="forvo_auth", stage="authentication", state=?raw, "Проверено состояние входа Forvo");
        if raw == AuthState::Authenticated {
            self.was_authenticated = true;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.profile.join(".forvo-authenticated"))
            {
                Ok(file) => file.sync_all().map_err(|_| {
                    error(
                        "session_state_write_failed",
                        "Не удалось сохранить факт установленной сессии",
                    )
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(_) => {
                    return Err(error(
                        "session_state_write_failed",
                        "Не удалось сохранить факт установленной сессии",
                    ));
                }
            }
        }
        Ok(
            if raw == AuthState::LoginRequired && self.was_authenticated {
                AuthState::SessionExpired
            } else {
                raw
            },
        )
    }
    pub(crate) async fn require_authenticated(&mut self) -> Result<(), AssetError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            let state = self.auth_state().await?;
            if state == AuthState::Authenticated {
                return Ok(());
            }
            if state != AuthState::PageUnstable || tokio::time::Instant::now() >= deadline {
                return Err(auth_error(state));
            }
            wait_dom(self.browser().page()).await;
        }
    }
    pub async fn check(&mut self, login: bool, wait: Duration) -> Result<AuthState, AssetError> {
        self.check_at(login, wait, None).await
    }
    pub async fn check_at(
        &mut self,
        login: bool,
        wait: Duration,
        surface: Option<&str>,
    ) -> Result<AuthState, AssetError> {
        let target = surface.map(word_url).unwrap_or_else(|| {
            if login {
                "https://forvo.com/login/".into()
            } else {
                "https://forvo.com/".into()
            }
        });
        self.navigate(&target).await?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let state = self.auth_state().await?;
            if state == AuthState::Authenticated
                || (!login && state != AuthState::PageUnstable)
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(state);
            }
            // Ожидание изменения DOM; при навигации прежний контекст выполнения исчезает.
            wait_dom(self.browser().page()).await;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthObservation {
    pub same_origin: bool,
    pub status: u16,
    pub challenge: bool,
    pub logout: bool,
    pub login: bool,
    pub ready: bool,
}
impl AuthObservation {
    pub(crate) fn state(&self) -> AuthState {
        if !self.same_origin {
            return AuthState::SiteUnavailable;
        }
        if self.challenge {
            return AuthState::ChallengeRequired;
        }
        if self.status == 401 || self.status == 403 {
            return AuthState::AccessDenied;
        }
        if self.status >= 500 {
            return AuthState::SiteUnavailable;
        }
        if !self.ready {
            return AuthState::PageUnstable;
        }
        if self.logout {
            AuthState::Authenticated
        } else if self.login {
            AuthState::LoginRequired
        } else {
            AuthState::PageUnstable
        }
    }
}
pub(crate) async fn observe_auth(page: &chromiumoxide::Page) -> Result<AuthState, AssetError> {
    let evaluated = match page
        .evaluate_expression(include_str!("forvo/auth.js"))
        .await
    {
        Ok(value) => value,
        Err(e)
            if [
                "Cannot find context",
                "Execution context was destroyed",
                "Inspected target navigated",
            ]
            .into_iter()
            .any(|m| e.to_string().contains(m)) =>
        {
            return Ok(AuthState::PageUnstable);
        }
        Err(_) => {
            return Err(error(
                "browser_recovery_required",
                "Страница или браузер закрыты; повторите login/check",
            ));
        }
    };
    let observed = evaluated
        .into_value::<AuthObservation>()
        .map_err(|_| error("dom_changed", "Неизвестные признаки авторизации Forvo"))?;
    if observed.status == 429 && !observed.challenge {
        return Err(error(
            "rate_limited",
            "Forvo ограничил запросы; продолжите позже",
        ));
    }
    Ok(observed.state())
}
pub(crate) async fn wait_dom(page: &chromiumoxide::Page) {
    let _=page.evaluate_expression(r#"new Promise(resolve => {
        const observer=new MutationObserver(()=>{observer.disconnect();clearTimeout(timer);resolve(true);});
        const timer=setTimeout(()=>{observer.disconnect();resolve(false);},500);
        observer.observe(document,{childList:true,subtree:true,attributes:true});
    })"#).await;
}
pub(crate) fn auth_error(state: AuthState) -> AssetError {
    let code = serde_json::to_value(state).expect("state");
    error(
        code.as_str().expect("string"),
        match state {
            AuthState::ChallengeRequired => {
                "Запустите forvo login в среде с GUI, пройдите проверку вручную и продолжите пакет"
            }
            AuthState::LoginRequired | AuthState::SessionExpired => {
                "Выполните forvo login, войдите вручную и продолжите пакет"
            }
            AuthState::AccessDenied => {
                "Forvo отказал в доступе; проверьте страницу вручную, продолжите позже"
            }
            _ => "Проверьте доступность Forvo и Chromium; затем продолжите пакет",
        },
    )
}
pub fn provider_code(error: &AssetError) -> &str {
    error.details["code"]
        .as_str()
        .unwrap_or(error.code.as_str())
}
pub(crate) fn requires_interaction(error: &AssetError) -> bool {
    matches!(
        provider_code(error),
        "challenge_required" | "login_required" | "session_expired"
    )
}
/// Только доказанный отказ записи допускает переход к следующему диктору.
/// Неизвестный сбой сохраняет свой код вместо маскировки исчерпанием кандидатов.
fn rejects_candidate(error: &AssetError) -> bool {
    matches!(
        provider_code(error),
        "download_aborted"
            | "download_mismatch"
            | "download_invalid"
            | "download_incomplete"
            | "download_button_unavailable"
            | "invalid_mp3"
    )
}

pub fn pauses_batch(error: &AssetError) -> bool {
    matches!(
        provider_code(error),
        "challenge_required"
            | "login_required"
            | "session_expired"
            | "access_denied"
            | "site_unavailable"
            | "page_unstable"
            | "rate_limited"
            | "navigation_timeout"
            | "browser_recovery_required"
            | "browser_cleanup_failed"
            | "download_timeout"
            | "download_staging_failed"
            | "download_listener_failed"
            | "download_not_supported"
            | "download_file_missing"
            | "download_read_failed"
            | "candidate_changed"
            | "candidate_disappeared"
            | "dom_changed"
            | "page_identity_mismatch"
            | "gui_unavailable"
            | "browser_profile_busy"
            | "browser_profile_not_owned"
            | "download_button_obscured"
            | "technical_validation_failed"
            | "session_state_write_failed"
    )
}

#[cfg(test)]
mod tests;
