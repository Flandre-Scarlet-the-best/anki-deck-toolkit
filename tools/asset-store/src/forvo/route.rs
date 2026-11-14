//! Выбор транспорта общего пользовательского пути Forvo.
//!
//! Это только слой маршрутизации: кандидаты, ранжирование, попытки, проверка
//! MP3, `ForvoEvidence`, сохранение оригинала, checkpoint и resume у обоих
//! транспортов общие. Здесь решается лишь, какой транспорт исполняет операцию
//! и допустим ли переход ко второму.
//!
//! Переход `chromium_first` → MV3-адаптер разрешён только при доказанном
//! отказе самого транспорта CDP до опасных побочных эффектов (backend failure):
//! собственный Chromium не запустился или его профиль занят; для одиночного
//! `fetch` также — профиль CDP не авторизован, а загрузка ещё не начиналась.
//! Отказы предметной области (domain failure) — ограничение Forvo, исчерпание
//! кандидатов, ошибки аудио и диска, чужая загрузка, неопределённость
//! владения, незавершённая уборка, прерывание — права на повтор через другую
//! сессию не дают.
use super::provider_code;
use crate::user_config::{ForvoMode, ForvoSettings};
use crate::{AssetError, ErrorCode};
use serde::Serialize;
use serde_json::{Value, json};

/// Этап CDP-попытки, на котором произошёл отказ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Запуск собственного Chromium: в браузере и на сайте ещё ничего не сделано.
    Launch,
    /// Сайт открыт, но загрузка записи не начиналась.
    BeforeDownload,
    /// Загрузка начиналась или оригинал уже сохранён.
    AfterDownload,
}

/// Причина отказа CDP-транспорта, допускающая переход (backend failure).
pub fn backend_failure(
    stage: Stage,
    error: &AssetError,
    cleanup_ok: bool,
) -> Result<&'static str, String> {
    if !cleanup_ok
        || error.details["recovery_required"] == true
        || !error.details["browser_cleanup"].is_null()
    {
        return Err("cleanup_incomplete".into());
    }
    if error.code == ErrorCode::Interrupted {
        return Err("interrupted".into());
    }
    let code = provider_code(error);
    match (stage, code) {
        (
            Stage::Launch,
            "gui_unavailable" | "browser_recovery_required" | "browser_profile_busy",
        ) => Ok("cdp_launch_failed"),
        (Stage::BeforeDownload, "login_required" | "session_expired") => {
            Ok("cdp_not_authenticated")
        }
        _ => Err(code.to_owned()),
    }
}

/// Решение маршрутизации для результата и диагностики.
#[derive(Debug, Clone)]
pub struct Decision {
    pub mode: ForvoMode,
    pub mode_source: &'static str,
    pub browser_target: Option<String>,
    pub attempts: Vec<Value>,
    pub selected: Option<&'static str>,
    pub fallback: Value,
}

impl Decision {
    pub fn new(settings: &ForvoSettings) -> Self {
        Self {
            mode: settings.mode,
            mode_source: settings.sources[0].1,
            browser_target: settings.browser_target.clone(),
            attempts: Vec::new(),
            selected: None,
            fallback: json!({"used":false}),
        }
    }
    pub fn attempt(&mut self, transport: &str, stage: Option<Stage>, error: Option<&AssetError>) {
        self.attempts
            .push(json!({"transport":transport,"stage":stage,
            "outcome":if error.is_some() {"failed"} else {"ok"},
            "code":error.map(provider_code)}));
    }
    pub fn to_value(&self) -> Value {
        json!({"mode":self.mode.id(),"mode_source":self.mode_source,"browser_target":self.browser_target,
            "selected":self.selected,"attempts":self.attempts,"fallback":self.fallback,
            "cdp_attempted":self.attempts.iter().any(|a| a["transport"] == "cdp_chromium")})
    }
}

/// Ошибка режима `extension_only` для команд, которым нужен собственный Chromium.
pub fn cdp_disabled(command: &str) -> AssetError {
    AssetError::with_details(
        ErrorCode::InvalidTransition,
        "Режим extension_only: собственный Chromium не запускается. Используйте адаптер браузера или явно задайте --mode chromium_first",
        json!({"provider":"forvo_web","code":"cdp_disabled_by_mode","command":command,
            "next_action":"pronunciation-assets forvo browser doctor --forvo (или forvo --mode chromium_first …)"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: &str) -> AssetError {
        super::super::error(code, "test")
    }

    #[test]
    fn only_backend_failures_before_side_effects_allow_fallback() {
        for code in [
            "gui_unavailable",
            "browser_recovery_required",
            "browser_profile_busy",
        ] {
            assert_eq!(
                backend_failure(Stage::Launch, &failure(code), true),
                Ok("cdp_launch_failed")
            );
        }
        for code in ["login_required", "session_expired"] {
            assert_eq!(
                backend_failure(Stage::BeforeDownload, &failure(code), true),
                Ok("cdp_not_authenticated")
            );
            assert!(backend_failure(Stage::AfterDownload, &failure(code), true).is_err());
        }
    }

    #[test]
    fn domain_failures_never_allow_fallback() {
        for code in [
            "rate_limited",
            "candidates_exhausted",
            "word_not_found",
            "challenge_required",
            "access_denied",
            "invalid_mp3",
            "technical_validation_failed",
            "download_conflict",
            "browser_profile_not_owned",
            "browser_cleanup_failed",
            "download_staging_failed",
            "io_failure",
        ] {
            for stage in [Stage::Launch, Stage::BeforeDownload, Stage::AfterDownload] {
                assert_eq!(
                    backend_failure(stage, &failure(code), true),
                    Err(code.to_owned()),
                    "{code} {stage:?}"
                );
            }
        }
        // Незавершённая уборка и прерывание запрещают переход даже при отказе запуска.
        assert_eq!(
            backend_failure(Stage::Launch, &failure("browser_recovery_required"), false),
            Err("cleanup_incomplete".into())
        );
        let interrupted = AssetError::new(ErrorCode::Interrupted, "Ctrl+C");
        assert_eq!(
            backend_failure(Stage::Launch, &interrupted, true),
            Err("interrupted".into())
        );
    }
    #[test]
    fn recovery_markers_refuse_even_an_otherwise_eligible_launch_failure() {
        let mut error = failure("browser_recovery_required");
        error.details["recovery_required"] = json!(true);
        assert_eq!(
            backend_failure(Stage::Launch, &error, true),
            Err("cleanup_incomplete".into())
        );
        error.details["recovery_required"] = json!(false);
        error.details["browser_cleanup"] = json!({"code":"browser_cleanup_failed"});
        assert_eq!(
            backend_failure(Stage::Launch, &error, true),
            Err("cleanup_incomplete".into())
        );
    }
}
