//! Известные программные причины отсутствия метрик.
//!
//! Контракт результата хранит причину свободным русским текстом без кода.
//! Здесь — единственный словарь таких причин: исполнитель строит текст из
//! тех же записей, а сверка с записанным текстом узнаёт причину с любой
//! подставленной деталью. Снимки документации показывают причину как есть.

use std::fmt::Display;

/// Причина. `{}` отмечает не более одной подставляемой детали (число,
/// текст ошибки ОС).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    pub ru: &'static str,
}

const SLOT: &str = "{}";

impl Known {
    /// Текст причины без детали.
    pub fn text(&self) -> String {
        debug_assert!(!self.ru.contains(SLOT), "{}", self.ru);
        self.ru.to_owned()
    }

    /// Текст причины с подставленной деталью.
    pub fn with(&self, detail: impl Display) -> String {
        debug_assert!(self.ru.contains(SLOT), "{}", self.ru);
        self.ru.replacen(SLOT, &detail.to_string(), 1)
    }

    /// Это ли причина записана (с любой деталью).
    pub fn matches(&self, reason: &str) -> bool {
        match self.ru.split_once(SLOT) {
            None => reason == self.ru,
            Some((prefix, suffix)) => reason
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.ends_with(suffix)),
        }
    }
}

pub const SAMPLING_WINDOW: Known = Known {
    ru: "прогон короче интервала сэмплирования {} с: ни одного полного окна",
};

pub const IO_NOT_DELEGATED: Known = Known {
    ru: "контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно",
};

pub const IO_NOT_ENABLED: Known = Known {
    ru: "контроллер io не включился для подгрупп прогонов ({}); CPU и память измеряются отдельно",
};

pub const IO_NOT_CONFIRMED: Known = Known {
    ru: "запись +io в cgroup.subtree_control принята, но io в нём не появился; CPU и память измеряются отдельно",
};

pub const IO_PROBE_SILENT: Known = Known {
    ru: "проба в группе systemd-run не сообщила состояние контроллера io",
};

pub const IO_STAT_UNREADABLE: Known = Known {
    ru: "io.stat отсутствует, недоступен или некорректен: учёт блочного I/O не подтверждён",
};

pub const OWN_COUNTERS_UNREAD: Known = Known {
    ru: "счётчики подгруппы прогона не прочитаны",
};

pub const CONTAINER_NOT_FOUND: Known = Known {
    ru: "контейнер нагрузки этого прогона не обнаружен",
};

pub const CONTAINER_GROUP_UNREAD: Known = Known {
    ru: "контейнер обнаружен, но его группа не прочитана до удаления",
};

pub const CONTAINER_IO_VANISHED: Known = Known {
    ru: "группа контейнера исчезла: итоговый io.stat не прочитан",
};

pub const CONTAINER_NO_SAMPLING: Known = Known {
    ru: "сэмплирование отключено параметром --no-sampling: группа контейнера наблюдается только во время прогона",
};

pub const CGROUP_NOT_MOUNTED: Known = Known {
    ru: "единая иерархия cgroup v2 не смонтирована",
};

pub const DOCKER_UNAVAILABLE: Known = Known {
    ru: "демон Docker недоступен: контейнер нагрузки не найти",
};

pub const KNOWN: &[Known] = &[
    SAMPLING_WINDOW,
    IO_NOT_DELEGATED,
    IO_NOT_ENABLED,
    IO_NOT_CONFIRMED,
    IO_PROBE_SILENT,
    IO_STAT_UNREADABLE,
    OWN_COUNTERS_UNREAD,
    CONTAINER_NOT_FOUND,
    CONTAINER_GROUP_UNREAD,
    CONTAINER_IO_VANISHED,
    CONTAINER_NO_SAMPLING,
    CGROUP_NOT_MOUNTED,
    DOCKER_UNAVAILABLE,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_reason_is_recognized_with_its_detail() {
        for known in KNOWN {
            let reason = if known.ru.contains(SLOT) {
                known.with("0.05 (os error 16)")
            } else {
                known.text()
            };
            assert!(known.matches(&reason), "{reason}");
            let others = KNOWN.iter().filter(|other| other.matches(&reason)).count();
            assert_eq!(others, 1, "{reason}");
        }
        assert!(!SAMPLING_WINDOW.matches("another diagnostic"));
    }
}
