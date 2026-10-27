//! Обычный сайт Forvo в принадлежащей приложению постоянной браузерной сессии.
use crate::{AssetError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
pub mod batch;
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
    pub(crate) browser: BrowserSession,
    was_authenticated: bool,
    download_workspaces: Vec<crate::temp_workspace::TempWorkspace>,
    profile: PathBuf,
}

impl ForvoSession {
    pub async fn launch(profile: &Path) -> Result<Self, AssetError> {
        let launch = Self::launch_owned(profile);
        tokio::pin!(launch);
        tokio::select! {
            result=&mut launch=>result,
            _=tokio::signal::ctrl_c()=>{
                if let Ok(session)=launch.await {session.close().await?;}
                Err(AssetError::new(ErrorCode::Interrupted,"Запуск Forvo прерван; принадлежащий браузер закрыт"))
            }
        }
    }
    async fn launch_owned(profile: &Path) -> Result<Self, AssetError> {
        let browser = BrowserSession::launch_persistent(BrowserRuntimeConfig::default(), profile).await
            .map_err(|e| { let message = e.to_string();
                let code = ["gui_unavailable", "browser_profile_busy", "browser_profile_not_owned"].into_iter().find(|code| message.contains(code)).unwrap_or("browser_recovery_required");
                error(code, "Проверьте GUI, Chromium и свободный собственный профиль; повторите forvo login")
            })?;
        Ok(Self {
            browser,
            was_authenticated: std::fs::symlink_metadata(profile.join(".forvo-authenticated"))
                .is_ok_and(|m| m.is_file()),
            download_workspaces: Vec::new(),
            profile: profile.to_path_buf(),
        })
    }
    pub async fn close(self) -> Result<(), AssetError> {
        let stopped = self.browser.close().await.map_err(|_| {
            error(
                "browser_cleanup_failed",
                "Не удалось подтвердить закрытие принадлежащего Chromium",
            )
        });
        let mut cleanup = Ok(());
        for workspace in self.download_workspaces {
            if workspace.close().is_err() {
                cleanup = Err(error(
                    "download_cleanup_failed",
                    "Не удалось очистить staging после остановки браузера",
                ));
            }
        }
        stopped?;
        cleanup
    }
    pub async fn navigate(&self, url: &str) -> Result<(), AssetError> {
        self.browser.telemetry().begin_epoch();
        // Некоторые страницы не завершают load из-за рекламы или проверки доступа.
        // После ограниченного ожидания решение принимает наблюдаемая DOM/auth readiness.
        let _ = tokio::time::timeout(Duration::from_secs(8), self.browser.page().goto(url)).await;
        Ok(())
    }
    pub async fn auth_state(&mut self) -> Result<AuthState, AssetError> {
        let raw = observe_auth(self.browser.page()).await?;
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
            wait_dom(self.browser.page()).await;
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
            // Ожидание изменения DOM; при навигации старый execution context исчезает.
            wait_dom(self.browser.page()).await;
        }
    }
}

#[derive(Debug, Deserialize)]
struct AuthObservation {
    same_origin: bool,
    status: u16,
    challenge: bool,
    logout: bool,
    login: bool,
    ready: bool,
}
impl AuthObservation {
    fn state(&self) -> AuthState {
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
                "Пройдите проверку Forvo вручную в видимом окне; затем продолжите пакет"
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
            | "download_timeout"
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
