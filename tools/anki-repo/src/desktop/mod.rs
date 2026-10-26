//! Управляемый адаптер Desktop; не владеет ревизиями или публикацией в input.

mod environment;
mod jobs;
mod patch;
mod setup;
mod transport;

use crate::error::{DomainError, ErrorCode};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const CONNECT: &str = "2055492159";
pub const CROWD: &str = "1788670778";
pub const PATCH_VERSION: &str = "1.0.2";
pub(crate) const COMPATIBILITY: &str = include_str!("compatibility.json");
pub(crate) type Result<T> = std::result::Result<T, DomainError>;

#[derive(Debug, Args)]
pub struct Options {
    #[command(subcommand)]
    pub action: Action,
    /// Конкретный исполняемый Anki (в WSL допустим /mnt/.../Anki.exe).
    #[arg(long, global = true)]
    pub executable: Option<PathBuf>,
    /// База именно этой установки; addons21 определяется только внутри неё.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    /// Ожидаемый профиль; CLI никогда не переключает работающий профиль.
    #[arg(long, global = true)]
    pub profile: Option<String>,
    /// Только числовой loopback; по умолчанию порт из конфигурации AnkiConnect.
    #[arg(long, global = true)]
    pub endpoint: Option<String>,
    /// Переменная окружения с upstream apiKey; значение не попадает в вывод.
    #[arg(long, global = true, default_value = "ANKICONNECT_API_KEY")]
    pub api_key_env: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PatchAction {
    Status,
    Install,
    Repair,
    Remove,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Только чтение диска и живого API; ready требует функциональной приёмки.
    Doctor,
    /// План установки отсутствующих аддонов; --apply запускает штатный установщик Anki.
    Setup {
        #[arg(long)]
        apply: bool,
    },
    /// Обратимый bootstrap существующего AnkiConnect. Запись только при --apply.
    Patch {
        #[arg(value_enum)]
        operation: PatchAction,
        #[arg(long)]
        apply: bool,
    },
    /// Запустить остановленную Anki с владением текущей операции.
    Launch {
        #[arg(long)]
        apply: bool,
    },
    /// Перезапустить только процесс, владение которым подтверждено handshake.
    Restart {
        #[arg(long)]
        apply: bool,
    },
    /// Штатно закрыть только подтверждённо принадлежащий CLI процесс.
    Stop {
        #[arg(long)]
        apply: bool,
    },
    /// Адресно восстановить два аддона из проверенного backup вне addons21.
    Restore {
        backup: String,
        #[arg(long)]
        apply: bool,
    },
    /// Экспорт в приватный spool через настоящий CrowdAnki.
    Export {
        #[arg(long)]
        deck: String,
        #[arg(long)]
        apply: bool,
    },
    /// Копия проверенного экспорта в spool и асинхронный импорт с видимым диалогом.
    Import {
        source: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    /// Состояние асинхронного задания: queued/running/succeeded/cancelled/failed/interrupted.
    Job { id: String },
}

impl Options {
    pub fn paths(&self) -> Vec<(&'static str, &Path)> {
        let mut paths = Vec::new();
        if let Some(p) = &self.executable {
            paths.push(("Исполняемый файл Anki", p.as_path()));
        }
        if let Some(p) = &self.data_dir {
            paths.push(("Данные Anki", p.as_path()));
        }
        if let Action::Import { source, .. } = &self.action {
            paths.push(("Источник CrowdAnki", source.as_path()));
        }
        paths
    }
}

pub(crate) fn error(reason: &str, message: impl Into<String>) -> DomainError {
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        message,
        json!({"reason": reason}),
    )
}
pub(crate) fn external_error(reason: &str, message: impl Into<String>) -> DomainError {
    DomainError::with_details(
        ErrorCode::ExternalFailure,
        message,
        json!({"reason": reason}),
    )
}
pub(crate) fn io_error(e: impl std::fmt::Display) -> DomainError {
    external_error("desktop_io", e.to_string())
}
pub(crate) fn read_json(path: &Path) -> Result<Value> {
    crate::layout::guard_local_path(path, "Desktop JSON")?;
    patch::check_ancestors(path)?;
    serde_json::from_slice(&std::fs::read(path).map_err(io_error)?).map_err(io_error)
}

/// Публичная точка оркестрации. Границы input проверяются и для библиотечного вызова.
pub fn execute(options: &Options) -> Result<Value> {
    for (role, p) in options.paths() {
        crate::layout::guard_local_path(p, role)?;
    }
    let target = environment::resolve(options)?;
    match &options.action {
        Action::Doctor => environment::doctor(&target, options),
        Action::Setup { apply } => setup::run(&target, options, *apply),
        Action::Patch { operation, apply } => patch::run(&target, *operation, *apply),
        Action::Launch { apply } => environment::launch(&target, options, *apply),
        Action::Restart { apply } => environment::restart(&target, options, *apply),
        Action::Stop { apply } => environment::stop(&target, options, *apply),
        Action::Restore { backup, apply } => patch::restore(&target, backup, *apply),
        Action::Export { deck, apply } => {
            jobs::submit(&target, options, "export", Some(deck), None, *apply)
        }
        Action::Import { source, apply } => {
            jobs::submit(&target, options, "import", None, Some(source), *apply)
        }
        Action::Job { id } => jobs::status(&target, options, id),
    }
}

pub fn render_text(value: &Value) -> String {
    let mut lines = vec![format!(
        "Anki Desktop — {}",
        state_text(value["state"].as_str().unwrap_or("result"))
    )];
    for (key, label) in [
        ("executable", "Программа"),
        ("version", "Версия"),
        ("target_platform", "ОС Anki"),
        ("cli_platform", "ОС CLI"),
        ("data_dir", "База"),
        ("endpoint", "API"),
        ("backup", "Резервная копия"),
        ("spool", "Каталог задания"),
    ] {
        if let Some(s) = value[key].as_str() {
            lines.push(format!("{label}: {s}"));
        }
    }
    if let Some(profile) = value["live"]["profile"].as_str() {
        lines.push(format!("Активный профиль: {profile}"));
    }
    if let Some(state) = value["live"]["state"].as_str() {
        lines.push(format!("Живая проверка: {}", state_text(state)));
    }
    if let Some(addons) = value["addons"].as_array() {
        for addon in addons {
            let id = addon["id"].as_str().unwrap_or("?");
            let loaded = value["handshake"]["addons"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["id"] == id));
            let state = loaded.map(|a| &a["state"]).unwrap_or(&addon["state"]);
            lines.push(format!(
                "Аддон {id}: {}",
                state_text(state.as_str().unwrap_or("unverified"))
            ));
        }
    }
    if let Some(state) = value["patch"]["state"].as_str() {
        lines.push(format!("Патч: {}", state_text(state)));
    }
    if let Some(job) = value.get("job") {
        lines.push(format!(
            "Задание {}: {}",
            job["id"].as_str().unwrap_or("?"),
            state_text(job["state"].as_str().unwrap_or("unknown"))
        ));
        if let Some(e) = job["error"].as_str() {
            lines.push(format!("Ошибка: {e}"));
        }
    }
    for key in ["missing", "blocked", "sources", "packages"] {
        if let Some(items) = value[key].as_array().filter(|a| !a.is_empty()) {
            lines.push(format!(
                "{key}: {}",
                items
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for key in ["message", "reason", "consequences", "next"] {
        if let Some(s) = value[key].as_str() {
            lines.push(s.into());
        }
    }
    if value["ready"] == false {
        lines.push(
            "Полная функциональная готовность не подтверждена. Подробные свидетельства: --json."
                .into(),
        );
    }
    lines.join("\n") + "\n"
}

fn state_text(state: &str) -> &str {
    match state {
        "ready" => "готова",
        "not_ready" => "требуется завершить проверку",
        "missing" => "не установлен",
        "disabled" => "отключён",
        "unsupported" | "incompatible" => "не поддерживается",
        "damaged" => "повреждён",
        "outdated" => "устарел",
        "pending_restart" | "restart_required" => "требуется перезапуск",
        "installed" => "файлы исправны, загрузка проверяется отдельно",
        "installed_unverified" => "установлен, загрузка не подтверждена",
        "loaded" => "загружен",
        "load_error" => "ошибка загрузки",
        "patch_loaded" => "патч загружен",
        "http_unreachable" => "HTTP не отвечает",
        "wrong_service" => "на порту другой процесс",
        "wrong_profile" => "активен другой профиль",
        "awaiting_native_installer" => "ожидается подтверждение установщика Anki",
        "starting" => "запускается",
        "stopped" => "остановлена",
        "restored" => "аддоны восстановлены",
        "removed" => "патч снят",
        "already_installed" | "no_installation_needed" => "повторная установка не требуется",
        "setup_plan" | "patch_plan" | "restore_plan" | "operation_plan" | "launch_plan"
        | "stop_plan" => "план без изменений",
        "submitted" | "queued" => "задание принято",
        "prepared" => "задание не принято; операция не начиналась",
        "running" => "выполняется",
        "succeeded" => "успешно",
        "cancelled" => "отменено пользователем",
        "failed" => "ошибка",
        "interrupted" => "прервано",
        "submission_unknown" => "отправка не подтверждена; проверьте задание",
        _ => state,
    }
}

pub(crate) fn exit_code(options: &Options, value: &Value) -> u8 {
    if matches!(options.action, Action::Doctor) {
        return if value["ready"] == true { 0 } else { 6 };
    }
    if matches!(options.action, Action::Job { .. }) {
        return if value["state"] == "succeeded" { 0 } else { 6 };
    }
    if matches!(
        options.action,
        Action::Patch {
            operation: PatchAction::Status,
            ..
        }
    ) {
        return if value["state"] == "installed" { 0 } else { 6 };
    }
    if matches!(
        value["state"].as_str(),
        Some("restart_required" | "submission_unknown")
    ) {
        6
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_adapter_failures_are_distinct_from_invalid_requests() {
        let failure = io_error("unavailable");
        assert_eq!(failure.code, ErrorCode::ExternalFailure);
        assert_eq!(failure.code.as_str(), "external_failure");
        assert_eq!(failure.exit_code(), 9);
        assert_eq!(failure.details["reason"], "desktop_io");

        let invalid = error("unsafe_endpoint", "Только loopback");
        assert_eq!(invalid.code, ErrorCode::InvalidRequest);
        assert_eq!(invalid.exit_code(), 3);
    }
}
