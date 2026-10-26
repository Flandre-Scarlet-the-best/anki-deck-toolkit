//! Каталог известных окон и уведомлений Anki, AnkiConnect и CrowdAnki.
//!
//! Правило идентифицирует окно по роли AT-SPI, заголовку, обязательным
//! фрагментам видимого текста и точному набору кнопок. Изменившийся текст или
//! набор кнопок — это уже другое, неизвестное окно: тест его не нажимает.
//! Поля `meaning`/`action`/`if_ignored` отвечают на вопросы пользователя
//! «Это нормальное сообщение? Нужно ли нажимать? Что будет, если ничего не
//! делать?» и попадают в матрицу отчёта.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    ExpectedInformational,
    RequiredConfirmation,
    WarningOrError,
    DuplicateOrMisleading,
    UnknownBlocking,
    UnexpectedNotice,
}

#[derive(Debug)]
pub struct Rule {
    pub id: &'static str,
    pub origin: &'static str,
    pub class: Class,
    /// Пустой список — любая роль, кроме главного окна.
    pub roles: &'static [&'static str],
    pub title: Option<&'static str>,
    pub text_all: &'static [&'static str],
    /// Точный набор видимых кнопок (порядок не важен); `None` — кнопок нет.
    pub buttons: Option<&'static [&'static str]>,
    pub needs_user_action: bool,
    pub message: &'static str,
    pub meaning: &'static str,
    pub action: &'static str,
    pub if_ignored: &'static str,
}

/// QMessageBox на шине AT-SPI: имя роли зависит от моста Qt.
const ALERT: &[&str] = &["alert", "alert message"];

pub const RULES: &[Rule] = &[
    Rule {
        id: "anki_first_run_language",
        origin: "anki",
        class: Class::RequiredConfirmation,
        roles: &["dialog"],
        title: Some("Anki"),
        text_all: &["Language"],
        buttons: Some(&["OK"]),
        needs_user_action: true,
        message: "Language (выбор языка интерфейса)",
        meaning: "Штатный первый запуск новой базы Anki; не ошибка и не связан с toolkit.",
        action: "Выбрать язык и нажать OK.",
        if_ignored: "Anki не продолжит запуск и не создаст профиль.",
    },
    Rule {
        id: "anki_first_run_language_confirm",
        origin: "anki",
        class: Class::RequiredConfirmation,
        roles: ALERT,
        title: Some("Anki"),
        text_all: &["Are you sure you wish to display Anki's interface in"],
        buttons: Some(&["Yes", "No"]),
        needs_user_action: true,
        message: "Are you sure you wish to display Anki's interface in English (United States)?",
        meaning: "Штатное подтверждение выбора языка при первом запуске.",
        action: "Yes — принять язык; No возвращает к выбору (кнопка по умолчанию — No).",
        if_ignored: "Запуск остановлен до ответа.",
    },
    Rule {
        id: "anki_addon_install_warning",
        origin: "anki",
        class: Class::RequiredConfirmation,
        roles: ALERT,
        title: Some("Install Anki add-on"),
        text_all: &[
            "Are you sure you want to proceed with the installation",
            ".ankiaddon",
        ],
        buttons: Some(&["No", "Yes"]),
        needs_user_action: true,
        message: "Important: As add-ons are programs downloaded from the internet, they are potentially malicious… Are you sure you want to proceed with the installation of the following Anki add-on(s)?",
        meaning: "Штатное предупреждение установщика Anki для файла .ankiaddon, который открыл `anki-repo anki setup --apply` (состояние awaiting_native_installer). Это не ошибка.",
        action: "Yes — установить официальный пакет; No — отказаться (аддон не будет установлен).",
        if_ignored: "Установка не происходит; Anki ждёт ответа, setup остаётся неподтверждённым.",
    },
    Rule {
        id: "anki_addon_installation_complete",
        origin: "anki",
        class: Class::ExpectedInformational,
        roles: ALERT,
        title: Some("Installation complete"),
        text_all: &["Installed"],
        buttons: Some(&["OK"]),
        needs_user_action: true,
        message: "Installation complete — Installed <аддон>.",
        meaning: "Штатный итог установщика при запуске Anki с пакетом: аддон установлен до загрузки аддонов и загрузится в этом же запуске. Нормальное сообщение.",
        action: "OK — только закрыть сообщение; решений не требуется.",
        if_ignored: "Окно модальное: запуск Anki продолжится только после OK.",
    },
    Rule {
        id: "anki_addon_installed_restart_tooltip",
        origin: "anki",
        class: Class::ExpectedInformational,
        roles: &["label", "tool tip"],
        title: None,
        text_all: &["Installed", "restart"],
        buttons: None,
        needs_user_action: false,
        message: "Installed <аддон>. Please restart Anki to complete the installation.",
        meaning: "Штатная всплывающая подсказка Anki после установки во время работы: аддон записан, но загрузится только после перезапуска. Не ошибка.",
        action: "Нажимать ничего не нужно; перезапустите Anki, когда удобно.",
        if_ignored: "Подсказка исчезнет сама; аддон останется незагруженным до перезапуска.",
    },
    Rule {
        id: "anki_addon_startup_failed",
        origin: "anki",
        class: Class::WarningOrError,
        roles: &["dialog"],
        title: Some("Anki"),
        text_all: &["Add-on Startup Failed", "failed to load"],
        buttons: Some(&["Close", "Check for Updates", "Copy Debug Info"]),
        needs_user_action: false,
        message: "Add-on Startup Failed — The following add-ons failed to load: …",
        meaning: "Настоящая ошибка: Anki не смогла загрузить аддон (исключение при импорте). Не связано с toolkit, если сломан upstream-код аддона.",
        action: "Close — закрыть; исправить или переустановить аддон (Check for Updates / setup после удаления).",
        if_ignored: "Окно немодальное, Anki работает без этого аддона; AnkiConnect API недоступен.",
    },
    Rule {
        id: "ankiconnect_port_in_use",
        origin: "ankiconnect",
        class: Class::WarningOrError,
        roles: ALERT,
        title: Some("AnkiConnect"),
        text_all: &["Address already in use"],
        buttons: Some(&["OK"]),
        needs_user_action: true,
        message: "AnkiConnect — [Errno 98] Address already in use",
        meaning: "Настоящая ошибка upstream AnkiConnect: порт 8765 занят другим процессом, API не запущен. Это не сбой Anki и не сообщение toolkit.",
        action: "OK закрывает окно; затем освободите порт (или задайте другой webBindPort) и перезапустите Anki.",
        if_ignored: "Окно модальное; AnkiConnect не работает, `doctor` показывает wrong_service/http_unreachable.",
    },
    Rule {
        id: "crowdanki_import_dialog",
        origin: "crowdanki",
        class: Class::RequiredConfirmation,
        roles: &["dialog"],
        title: Some("CrowdAnki Import Settings"),
        text_all: &["Personal Fields", "Deck Parts to Use"],
        buttons: Some(&["OK", "Cancel"]),
        needs_user_action: true,
        message: "CrowdAnki Import Settings (персональные поля, части колоды, теги)",
        meaning: "Штатный диалог настроек импорта CrowdAnki, открытый `anki-repo anki import --apply` после HTTP admission. Это не ошибка: импорт ждёт решения.",
        action: "OK — импортировать с выбранными настройками; Cancel — отменить (задание станет cancelled, коллекция не изменится).",
        if_ignored: "Задание остаётся running, Anki занята (busy); другие изменяющие запросы отклоняются.",
    },
];

pub fn rule(id: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.id == id)
}

/// Подходящее правило для окна: роль, заголовок, текст и точный набор кнопок.
pub fn find(role: &str, title: &str, text: &str, buttons: &[String]) -> Option<&'static Rule> {
    RULES.iter().find(|r| {
        (r.roles.is_empty() || r.roles.contains(&role))
            && r.title.is_none_or(|t| t == title)
            && r.text_all
                .iter()
                .all(|fragment| title.contains(fragment) || text.contains(fragment))
            && match r.buttons {
                None => buttons.is_empty(),
                Some(expected) => {
                    let mut a: Vec<&str> = buttons.iter().map(String::as_str).collect();
                    let mut b: Vec<&str> = expected.to_vec();
                    a.sort_unstable();
                    b.sort_unstable();
                    a == b
                }
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ids_are_unique_and_explained() {
        let mut ids: Vec<&str> = RULES.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), RULES.len());
        for r in RULES {
            assert!(
                !r.meaning.is_empty() && !r.action.is_empty() && !r.if_ignored.is_empty(),
                "{}",
                r.id
            );
        }
    }

    #[test]
    fn changed_buttons_or_text_do_not_match_a_known_dialog() {
        let text = "Are you sure you want to proceed with the installation of the following Anki add-on(s)? 2055492159.ankiaddon";
        let yes_no = vec!["Yes".to_owned(), "No".to_owned()];
        assert_eq!(
            find("alert", "Install Anki add-on", text, &yes_no).map(|r| r.id),
            Some("anki_addon_install_warning")
        );
        let extra = vec!["Yes".to_owned(), "No".to_owned(), "Yes to All".to_owned()];
        assert!(find("alert", "Install Anki add-on", text, &extra).is_none());
        assert!(find("alert", "Install Anki add-on", "Something else", &yes_no).is_none());
        assert!(find("dialog", "Install Anki add-on", text, &yes_no).is_none());
    }
}
