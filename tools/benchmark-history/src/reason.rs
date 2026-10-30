//! Известные программные причины отсутствия метрик и их переводы.
//!
//! Контракт результата хранит причину свободным русским текстом без кода, а
//! снимки документации показывают её на трёх языках. Здесь — единственный
//! словарь таких причин: исполнитель строит текст из тех же записей, поэтому
//! перевод не расходится с источником, а прежние результаты с тем же текстом
//! переводятся так же. Неизвестная причина остаётся как есть: произвольный
//! текст не переводится.

use crate::snapshot::Lang;
use std::fmt::Display;

/// Причина на трёх языках. `{}` отмечает не более одной подставляемой детали
/// (число, текст ошибки ОС), которая переносится в перевод без изменений.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    pub ru: &'static str,
    pub en: &'static str,
    pub zh_cn: &'static str,
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
        self.translate(reason, Lang::Ru).is_some()
    }

    fn translate(&self, reason: &str, lang: Lang) -> Option<String> {
        let target = match lang {
            Lang::Ru => self.ru,
            Lang::En => self.en,
            Lang::ZhCn => self.zh_cn,
        };
        match self.ru.split_once(SLOT) {
            None => (reason == self.ru).then(|| target.to_owned()),
            Some((prefix, suffix)) => {
                let detail = reason.strip_prefix(prefix)?.strip_suffix(suffix)?;
                Some(target.replacen(SLOT, detail, 1))
            }
        }
    }
}

pub const SAMPLING_WINDOW: Known = Known {
    ru: "прогон короче интервала сэмплирования {} с: ни одного полного окна",
    en: "run shorter than the sampling interval {} s: no complete window",
    zh_cn: "运行时长短于 {} 秒的采样间隔：没有完整采样窗口",
};

pub const IO_NOT_DELEGATED: Known = Known {
    ru: "контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно",
    en: "the io controller is not delegated to the runner's cgroup; CPU and memory are available separately",
    zh_cn: "执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取",
};

pub const IO_NOT_ENABLED: Known = Known {
    ru: "контроллер io не включился для подгрупп прогонов ({}); CPU и память измеряются отдельно",
    en: "the io controller could not be enabled for run subgroups ({}); CPU and memory are measured separately",
    zh_cn: "无法为运行子组启用 io 控制器（{}）；CPU 和内存仍单独测量",
};

pub const IO_NOT_CONFIRMED: Known = Known {
    ru: "запись +io в cgroup.subtree_control принята, но io в нём не появился; CPU и память измеряются отдельно",
    en: "writing +io to cgroup.subtree_control was accepted, but io did not appear there; CPU and memory are measured separately",
    zh_cn: "向 cgroup.subtree_control 写入 +io 已被接受，但其中未出现 io；CPU 和内存仍单独测量",
};

pub const IO_PROBE_SILENT: Known = Known {
    ru: "проба в группе systemd-run не сообщила состояние контроллера io",
    en: "the probe in the systemd-run cgroup did not report the io controller state",
    zh_cn: "systemd-run cgroup 中的探测未报告 io 控制器状态",
};

pub const IO_STAT_UNREADABLE: Known = Known {
    ru: "io.stat отсутствует, недоступен или некорректен: учёт блочного I/O не подтверждён",
    en: "io.stat is missing, unreadable or malformed: block I/O accounting is not confirmed",
    zh_cn: "io.stat 缺失、不可读或格式不正确：块 I/O 记账未得到确认",
};

pub const OWN_COUNTERS_UNREAD: Known = Known {
    ru: "счётчики подгруппы прогона не прочитаны",
    en: "the run subgroup's counters were not read",
    zh_cn: "未读取到运行子组的计数器",
};

pub const CONTAINER_NOT_FOUND: Known = Known {
    ru: "контейнер нагрузки этого прогона не обнаружен",
    en: "the workload container of this run was not found",
    zh_cn: "未发现本次运行的负载容器",
};

pub const CONTAINER_GROUP_UNREAD: Known = Known {
    ru: "контейнер обнаружен, но его группа не прочитана до удаления",
    en: "the container was found, but its cgroup was not read before removal",
    zh_cn: "已发现容器，但在其删除前未读取到其 cgroup",
};

pub const CONTAINER_IO_VANISHED: Known = Known {
    ru: "группа контейнера исчезла: итоговый io.stat не прочитан",
    en: "the container's cgroup disappeared: the final io.stat was not read",
    zh_cn: "容器的 cgroup 已消失：未读取到最终的 io.stat",
};

pub const CONTAINER_NO_SAMPLING: Known = Known {
    ru: "сэмплирование отключено параметром --no-sampling: группа контейнера наблюдается только во время прогона",
    en: "sampling is disabled by --no-sampling: the container's cgroup is observed only during the run",
    zh_cn: "采样已被 --no-sampling 禁用：容器的 cgroup 只能在运行期间观测",
};

pub const CGROUP_NOT_MOUNTED: Known = Known {
    ru: "единая иерархия cgroup v2 не смонтирована",
    en: "the unified cgroup v2 hierarchy is not mounted",
    zh_cn: "未挂载统一的 cgroup v2 层级",
};

pub const DOCKER_UNAVAILABLE: Known = Known {
    ru: "демон Docker недоступен: контейнер нагрузки не найти",
    en: "the Docker daemon is unavailable: the workload container cannot be found",
    zh_cn: "Docker 守护进程不可用：无法找到负载容器",
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

/// Причина на языке страницы; неизвестная причина возвращается без изменений.
pub fn localize(reason: &str, lang: Lang) -> String {
    KNOWN
        .iter()
        .find_map(|known| known.translate(reason, lang))
        .unwrap_or_else(|| reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_reason_round_trips_in_every_language() {
        for known in KNOWN {
            let (reason, detail) = if known.ru.contains(SLOT) {
                (known.with("0.05 (os error 16)"), Some("0.05 (os error 16)"))
            } else {
                (known.text(), None)
            };
            assert_eq!(localize(&reason, Lang::Ru), reason);
            for (lang, target) in [(Lang::En, known.en), (Lang::ZhCn, known.zh_cn)] {
                assert_eq!(known.ru.matches(SLOT).count(), target.matches(SLOT).count());
                let expected = match detail {
                    Some(detail) => target.replacen(SLOT, detail, 1),
                    None => target.to_owned(),
                };
                let localized = localize(&reason, lang);
                assert_eq!(localized, expected);
                // Перевод не оставляет кириллицы, кроме подставленной детали.
                assert!(
                    !localized
                        .chars()
                        .any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')),
                    "{localized}"
                );
            }
        }
    }

    #[test]
    fn previously_recorded_texts_keep_their_translation() {
        // Тексты, уже записанные прежними результатами, переводятся по-прежнему.
        assert_eq!(
            localize(
                "прогон короче интервала сэмплирования 0.05 с: ни одного полного окна",
                Lang::En
            ),
            "run shorter than the sampling interval 0.05 s: no complete window"
        );
        assert_eq!(
            localize(
                "контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно",
                Lang::ZhCn
            ),
            "执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取"
        );
        assert_eq!(
            localize("another diagnostic", Lang::En),
            "another diagnostic"
        );
        assert_eq!(
            localize("io.stat отсутствует, недоступен или некорректен", Lang::En),
            "io.stat отсутствует, недоступен или некорректен"
        );
    }
}
