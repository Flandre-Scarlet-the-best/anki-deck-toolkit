//! Статусы прогона и машинно-читаемый отчёт с человекочитаемой сводкой.

use serde::Serialize;
use serde_json::Value;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Pass,
    Skipped,
    Blocked,
    Fail,
    Interrupted,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Skipped => "SKIPPED",
            Status::Fail => "FAIL",
            Status::Blocked => "BLOCKED",
            Status::Interrupted => "INTERRUPTED",
            Status::Error => "ERROR",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "PASS" => Status::Pass,
            "SKIPPED" => Status::Skipped,
            "FAIL" => Status::Fail,
            "BLOCKED" => Status::Blocked,
            "INTERRUPTED" => Status::Interrupted,
            "ERROR" => Status::Error,
            _ => return None,
        })
    }

    /// Код завершения runner: успех только при полностью зелёном прогоне.
    pub fn exit_code(self) -> u8 {
        match self {
            Status::Pass => 0,
            Status::Fail => 1,
            Status::Blocked | Status::Skipped => 2,
            Status::Error => 3,
            Status::Interrupted => 130,
        }
    }
}

/// Остановка прогона до или вне сценариев: стадия, машинная причина и пояснение.
#[derive(Debug, Clone, Serialize)]
pub struct Stop {
    pub status: Status,
    pub stage: &'static str,
    pub reason: String,
    pub message: String,
}

impl Stop {
    pub fn blocked(
        stage: &'static str,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Stop {
        Stop {
            status: Status::Blocked,
            stage,
            reason: reason.into(),
            message: message.into(),
        }
    }

    pub fn error(
        stage: &'static str,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Stop {
        Stop {
            status: Status::Error,
            stage,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

/// Итог прогона — худший из статусов сценариев, остановки и уборки.
pub fn overall(statuses: impl IntoIterator<Item = Status>) -> Status {
    statuses.into_iter().max().unwrap_or(Status::Skipped)
}

/// Markdown-сводка строится из того же JSON, что сохраняется для анализа.
pub fn render_markdown(report: &Value) -> String {
    let mut out = String::new();
    let s = |v: &Value| v.as_str().unwrap_or("—").to_owned();
    let _ = writeln!(out, "# Отчёт E2E Anki Desktop\n");
    let _ = writeln!(out, "- Итог: **{}**", s(&report["status"]));
    let _ = writeln!(out, "- Прогон: `{}`", s(&report["run_id"]));
    let _ = writeln!(
        out,
        "- Начало / конец: {} / {}",
        s(&report["started_at"]),
        s(&report["finished_at"])
    );
    if let Some(stop) = report.get("stop").filter(|v| !v.is_null()) {
        let _ = writeln!(
            out,
            "- Остановка: **{}** на стадии `{}` — `{}`: {}",
            s(&stop["status"]),
            s(&stop["stage"]),
            s(&stop["reason"]),
            s(&stop["message"])
        );
    }
    for stop in report["secondary_stops"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "- Дополнительно: {} на стадии `{}` — `{}`: {}",
            s(&stop["status"]),
            s(&stop["stage"]),
            s(&stop["reason"]),
            s(&stop["message"])
        );
    }
    for error in report["report_write_errors"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let _ = writeln!(out, "- Отчёт записан не полностью: {}", s(error));
    }
    if let Some(state) = report["provenance"]["state"].as_str() {
        let _ = writeln!(out, "- Происхождение бинарника агента: `{state}`");
    }
    let anki = &report["anki"];
    let _ = writeln!(out, "\n## Anki\n");
    let _ = writeln!(out, "- Режим выбора версии: `{}`", s(&anki["mode"]));
    let _ = writeln!(
        out,
        "- Обнаруженная актуальная стабильная версия: `{}`",
        s(&anki["latest_stable"])
    );
    let _ = writeln!(out, "- Выбранная версия: `{}`", s(&anki["version"]));
    let _ = writeln!(
        out,
        "- Фактически установленная: `{}`",
        s(&report["installed_anki_version"])
    );
    let _ = writeln!(out, "- Источник: {}", s(&anki["asset_url"]));
    let _ = writeln!(
        out,
        "- SHA-256 архива: `{}` ({})",
        s(&report["artifact"]["sha256"]),
        s(&report["artifact"]["verification"])
    );
    let _ = writeln!(out, "- Решение: {}", s(&anki["decision"]));
    let images = &report["images"];
    let _ = writeln!(out, "\n## Образы и сборка\n");
    let _ = writeln!(
        out,
        "- Образ Anki: `{}` `{}`",
        s(&images["anki"]["tag"]),
        s(&images["anki"]["id"])
    );
    let _ = writeln!(
        out,
        "- Образ запуска: `{}` `{}`",
        s(&images["run"]["tag"]),
        s(&images["run"]["id"])
    );
    let src = &report["source"];
    let _ = writeln!(
        out,
        "- Исходники toolkit: commit `{}`, dirty={}, tree `{}`",
        s(&src["commit"]),
        src["dirty"],
        s(&src["tree_sha256"])
    );
    let _ = writeln!(
        out,
        "- `anki-repo` в контейнере: `{}`",
        s(&report["agent"]["build"]["anki_repo_sha256"])
    );
    if let Some(scenarios) = report["scenarios"].as_array() {
        let _ = writeln!(out, "\n## Сценарии\n");
        let _ = writeln!(out, "| Сценарий | Статус | Длительность, с | Причина |");
        let _ = writeln!(out, "|---|---|---|---|");
        for sc in scenarios {
            let _ = writeln!(
                out,
                "| `{}` | **{}** | {:.1} | {} |",
                s(&sc["name"]),
                s(&sc["status"]),
                sc["duration_s"].as_f64().unwrap_or(0.0),
                s(&sc["reason"]).replace('|', "\\|")
            );
        }
        for sc in scenarios {
            let _ = writeln!(out, "\n### `{}` — {}\n", s(&sc["name"]), s(&sc["status"]));
            for check in sc["checks"].as_array().into_iter().flatten() {
                let mark = if check["ok"] == true { "✔" } else { "✘" };
                let _ = writeln!(out, "- {mark} {}", s(&check["what"]));
            }
            let events: Vec<&Value> = sc["ui_events"].as_array().into_iter().flatten().collect();
            if !events.is_empty() {
                let _ = writeln!(out, "\nUI-события:\n");
                let _ = writeln!(
                    out,
                    "| Шаг | Окно | Класс | Нужно действие | Действие теста | Видно, мс | Скриншот |"
                );
                let _ = writeln!(out, "|---|---|---|---|---|---|---|");
                for e in events {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} | {} | {} | {} |",
                        s(&e["step"]),
                        markdown_cell(&format!("{} — {}", s(&e["title"]), s(&e["text"]))),
                        s(&e["classification"]),
                        e["needs_user_action"],
                        s(&e["test_action"]),
                        e["visible_ms"],
                        s(&e["screenshot"])
                    );
                }
            }
        }
    }
    if let Some(matrix) = report["ux_matrix"].as_array().filter(|m| !m.is_empty()) {
        let _ = writeln!(out, "\n## Матрица уведомлений\n");
        let _ = writeln!(
            out,
            "| Сообщение | Источник | Класс | Нормально? | Нужно нажать? | Если ничего не делать |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for row in matrix {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                markdown_cell(&s(&row["message"])),
                s(&row["origin"]),
                s(&row["classification"]),
                markdown_cell(&s(&row["meaning"])),
                markdown_cell(&s(&row["action"])),
                markdown_cell(&s(&row["if_ignored"]))
            );
        }
    }
    let _ = writeln!(out, "\n## Изоляция и уборка\n");
    let _ = writeln!(
        out,
        "```json\n{}\n```",
        serde_json::to_string_pretty(&report["isolation"]).unwrap_or_default()
    );
    let _ = writeln!(
        out,
        "- Уборка: {}",
        serde_json::to_string(&report["cleanup"]).unwrap_or_default()
    );
    out
}

fn markdown_cell(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = flat.chars().take(220).collect();
    short.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn overall_status_never_hides_a_failure_behind_passes() {
        assert_eq!(overall([Status::Pass, Status::Pass]), Status::Pass);
        assert_eq!(
            overall([Status::Pass, Status::Fail, Status::Pass]),
            Status::Fail
        );
        assert_eq!(overall([Status::Pass, Status::Blocked]), Status::Blocked);
        assert_eq!(overall([Status::Fail, Status::Blocked]), Status::Fail);
        assert_eq!(overall([Status::Fail, Status::Error]), Status::Error);
        assert_eq!(overall([]), Status::Skipped);
        assert_ne!(overall([Status::Skipped]).exit_code(), 0);
    }

    #[test]
    fn markdown_keeps_stop_reason_and_escapes_cells() {
        let report = json!({
            "status":"BLOCKED","run_id":"r","stop":{"status":"BLOCKED","stage":"resolve","reason":"x","message":"m"},
            "scenarios":[{"name":"a","status":"FAIL","duration_s":1.0,"reason":"a|b","checks":[],"ui_events":[]}]
        });
        let text = render_markdown(&report);
        assert!(text.contains("**BLOCKED** на стадии `resolve`"));
        assert!(text.contains("a\\|b"));
    }
}
