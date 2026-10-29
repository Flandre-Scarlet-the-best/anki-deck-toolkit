//! Закрытый словарь контракта: виды нагрузки, состояния кэша, единицы, области
//! охвата и источники метрик, а также встроенный реестр метрик.
//!
//! Словарь — часть ядра схемы. Новый модуль регистрирует сценарий декларацией и
//! не меняет этот файл; правка нужна только для нового *вида* измерения, и она
//! сопровождается новой версией схемы, если меняет смысл существующих значений.

use serde::{Deserialize, Serialize};

/// Единственная поддерживаемая версия схемы каталога и деклараций.
pub const CATALOG_SCHEMA_VERSION: u32 = 1;
/// Единственная поддерживаемая версия схемы результатов.
pub const RESULT_SCHEMA_VERSION: u32 = 1;

macro_rules! closed_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

closed_enum!(
    /// Вид нагрузки: от него зависят предельные значения политики прогонов.
    WorkloadKind {
        ShortCli => "short-cli",
        LongE2e => "long-e2e",
    }
);

closed_enum!(
    /// Исполняемый сценарий измеряет настоящую команду; учебный — только
    /// иллюстрирует контракт и никогда не выбирается для измерения.
    ScenarioStatus {
        Executable => "executable",
        Example => "example",
    }
);

closed_enum!(
    /// Базовое состояние кэша профиля. Что именно очищено, каждый сценарий
    /// определяет сам в привязке профиля.
    CacheState {
        Cold => "cold",
        Warm => "warm",
        Incremental => "incremental",
    }
);

closed_enum!(
    /// Сетевая политика профиля.
    Network {
        Online => "online",
        Offline => "offline",
    }
);

closed_enum!(
    /// Операционная система, на которой сценарий может измеряться.
    Os {
        Linux => "linux",
        Macos => "macos",
        Windows => "windows",
    }
);

closed_enum!(
    /// Базовая единица. Других единиц в контракте нет: перевод в миллисекунды
    /// или МиБ — дело отображения.
    Unit {
        Second => "second",
        Byte => "byte",
        Count => "count",
        Core => "core",
    }
);

closed_enum!(
    /// Область охвата метрики: чьё потребление она описывает.
    Scope {
        Command => "command",
        Process => "process",
        ProcessTreeWaited => "process-tree-waited",
        LargestProcessInTree => "largest-process-in-tree",
        Cgroup => "cgroup",
        Host => "host",
        Phase => "phase",
        ModuleReport => "module-report",
        Path => "path",
    }
);

closed_enum!(
    /// Источник метрики.
    Source {
        Hyperfine => "hyperfine",
        Procfs => "procfs",
        CgroupV2 => "cgroup-v2",
        ModuleLog => "module-log",
        ModuleReport => "module-report",
        Filesystem => "filesystem",
    }
);

closed_enum!(
    /// Канонические идентификаторы метрик. Имена метрик Hyperfine 2.0
    /// сохранены без изменений, чтобы импорт его JSON не требовал словаря.
    MetricId {
        TimeWallClock => "time_wall_clock",
        TimeUser => "time_user",
        TimeSystem => "time_system",
        TimeCpu => "time_cpu",
        MemoryPeakResident => "memory_peak_resident",
        MemoryPeakCgroup => "memory_peak_cgroup",
        CpuTimeCgroup => "cpu_time_cgroup",
        CpuUtilizationMean => "cpu_utilization_mean",
        CpuUtilizationPeak => "cpu_utilization_peak",
        MemoryHostPeakIncrease => "memory_host_peak_increase",
        CpuCycles => "cpu_cycles",
        Instructions => "instructions",
        CacheReferences => "cache_references",
        CacheMisses => "cache_misses",
        BranchMisses => "branch_misses",
        PhaseDuration => "phase_duration",
        ModuleCounter => "module_counter",
        DiskUsage => "disk_usage",
    }
);

closed_enum!(
    /// Встроенные факты окружения. Модуль может объявить собственные факты.
    BuiltinFact {
        Os => "os",
        Arch => "arch",
        KernelRelease => "kernel_release",
        Virtualization => "virtualization",
        CpuModel => "cpu_model",
        LogicalCpus => "logical_cpus",
        MemoryTotalBytes => "memory_total_bytes",
        ModuleVersion => "module_version",
        RustcVersion => "rustc_version",
        DockerVersion => "docker_version",
        HyperfineVersion => "hyperfine_version",
    }
);

/// Факты, без которых никакие два результата не сопоставляются.
pub const MANDATORY_FACTS: &[BuiltinFact] = &[
    BuiltinFact::Os,
    BuiltinFact::Arch,
    BuiltinFact::ModuleVersion,
];
/// Факты, которые всегда входят в условие сопоставимости.
pub const MANDATORY_COMPARABLE_FACTS: &[BuiltinFact] = &[BuiltinFact::Os, BuiltinFact::Arch];

impl MetricId {
    pub const fn base_unit(self) -> Unit {
        use MetricId::*;
        match self {
            TimeWallClock | TimeUser | TimeSystem | TimeCpu | CpuTimeCgroup | PhaseDuration => {
                Unit::Second
            }
            MemoryPeakResident | MemoryPeakCgroup | MemoryHostPeakIncrease | DiskUsage => {
                Unit::Byte
            }
            CpuUtilizationMean | CpuUtilizationPeak => Unit::Core,
            CpuCycles | Instructions | CacheReferences | CacheMisses | BranchMisses
            | ModuleCounter => Unit::Count,
        }
    }

    /// Метрика различает экземпляры меткой (имя фазы, счётчика или пути).
    pub const fn label_required(self) -> bool {
        matches!(
            self,
            MetricId::PhaseDuration | MetricId::ModuleCounter | MetricId::DiskUsage
        )
    }

    /// Счётчик событий процессора: нужен доступ к `perf_event` и команда без shell.
    pub const fn is_hardware_counter(self) -> bool {
        use MetricId::*;
        matches!(
            self,
            CpuCycles | Instructions | CacheReferences | CacheMisses | BranchMisses
        )
    }

    pub fn definition(self) -> &'static MetricDefinition {
        REGISTRY
            .iter()
            .find(|definition| definition.metric == self)
            .expect("каждая метрика словаря описана в реестре")
    }
}

/// Допустимое сочетание области охвата и источника для метрики.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Measurement {
    pub scope: Scope,
    pub source: Source,
    pub platforms: &'static [Os],
    /// Значение получается сэмплированием: декларация обязана задать интервал.
    pub sampled: bool,
    /// Точная семантика сочетания; повторяется в документации.
    pub meaning: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricDefinition {
    pub metric: MetricId,
    pub unit: Unit,
    pub summary: &'static str,
    pub measurements: &'static [Measurement],
}

impl MetricDefinition {
    pub fn measurement(&self, scope: Scope, source: Source) -> Option<&'static Measurement> {
        self.measurements
            .iter()
            .find(|candidate| candidate.scope == scope && candidate.source == source)
    }
}

const ALL_OS: &[Os] = &[Os::Linux, Os::Macos, Os::Windows];
const UNIX: &[Os] = &[Os::Linux, Os::Macos];
const LINUX: &[Os] = &[Os::Linux];

const fn m(
    scope: Scope,
    source: Source,
    platforms: &'static [Os],
    sampled: bool,
    meaning: &'static str,
) -> Measurement {
    Measurement {
        scope,
        source,
        platforms,
        sampled,
        meaning,
    }
}

const TIME_TREE: &str =
    "Процесс команды и потоки; потомки — только те, которых родитель дождался (rusage детей).";
const COUNTER_TREE: &str = "Потоки и дождавшиеся потомки; на Linux без исполнения в ядре и гипервизоре. Нужен perf_event и запуск без shell.";

/// Встроенный реестр. Сочетание вне реестра — противоречие, а не новая метрика.
pub static REGISTRY: &[MetricDefinition] = &[
    MetricDefinition {
        metric: MetricId::TimeWallClock,
        unit: Unit::Second,
        summary: "Время от старта до завершения измеряемой команды, включая ожидание.",
        measurements: &[m(
            Scope::Command,
            Source::Hyperfine,
            ALL_OS,
            false,
            "Интервал одного запуска цели; подготовка и очистка не входят.",
        )],
    },
    MetricDefinition {
        metric: MetricId::TimeUser,
        unit: Unit::Second,
        summary: "Процессорное время в пользовательском режиме, сумма по потокам.",
        measurements: &[
            m(
                Scope::ProcessTreeWaited,
                Source::Hyperfine,
                ALL_OS,
                false,
                TIME_TREE,
            ),
            m(
                Scope::Process,
                Source::Procfs,
                LINUX,
                false,
                "utime из /proc/<pid>/stat: только сам процесс, без потомков.",
            ),
            m(
                Scope::ProcessTreeWaited,
                Source::Procfs,
                LINUX,
                false,
                "utime+cutime из /proc/<pid>/stat: потомки — только дождавшиеся.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::TimeSystem,
        unit: Unit::Second,
        summary: "Процессорное время в ядре от имени процесса, сумма по потокам.",
        measurements: &[
            m(
                Scope::ProcessTreeWaited,
                Source::Hyperfine,
                ALL_OS,
                false,
                TIME_TREE,
            ),
            m(
                Scope::Process,
                Source::Procfs,
                LINUX,
                false,
                "stime из /proc/<pid>/stat: только сам процесс, без потомков.",
            ),
            m(
                Scope::ProcessTreeWaited,
                Source::Procfs,
                LINUX,
                false,
                "stime+cstime из /proc/<pid>/stat: потомки — только дождавшиеся.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::TimeCpu,
        unit: Unit::Second,
        summary: "time_user + time_system одного запуска; может превышать время по часам.",
        measurements: &[
            m(
                Scope::ProcessTreeWaited,
                Source::Hyperfine,
                ALL_OS,
                false,
                TIME_TREE,
            ),
            m(
                Scope::Process,
                Source::Procfs,
                LINUX,
                false,
                "utime+stime из /proc/<pid>/stat без потомков.",
            ),
            m(
                Scope::ProcessTreeWaited,
                Source::Procfs,
                LINUX,
                false,
                "utime+stime+cutime+cstime из /proc/<pid>/stat.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::MemoryPeakResident,
        unit: Unit::Byte,
        summary: "Пиковый RSS одного процесса, а не одновременная сумма дерева процессов.",
        measurements: &[
            m(
                Scope::LargestProcessInTree,
                Source::Hyperfine,
                UNIX,
                false,
                "Наибольший пиковый RSS среди команды и дождавшихся потомков; на Linux очень малый пик может быть подменён пиком, унаследованным от hyperfine.",
            ),
            m(
                Scope::Process,
                Source::Procfs,
                LINUX,
                false,
                "VmHWM из /proc/<pid>/status: пик RSS самого процесса без потомков.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::MemoryPeakCgroup,
        unit: Unit::Byte,
        summary: "Пик памяти всей группы процессов, который ведёт ядро.",
        measurements: &[m(
            Scope::Cgroup,
            Source::CgroupV2,
            LINUX,
            false,
            "memory.peak cgroup v2: одновременное потребление всех процессов группы, включая page cache группы.",
        )],
    },
    MetricDefinition {
        metric: MetricId::CpuTimeCgroup,
        unit: Unit::Second,
        summary: "Процессорное время всех процессов группы.",
        measurements: &[m(
            Scope::Cgroup,
            Source::CgroupV2,
            LINUX,
            false,
            "usage_usec из cpu.stat cgroup v2, переведённый в секунды.",
        )],
    },
    MetricDefinition {
        metric: MetricId::CpuUtilizationMean,
        unit: Unit::Core,
        summary: "Наблюдаемая средняя загрузка: процессорное время, делённое на время по часам.",
        measurements: &[
            m(
                Scope::Cgroup,
                Source::CgroupV2,
                LINUX,
                false,
                "Разность usage_usec за интервал прогона, делённая на его длительность.",
            ),
            m(
                Scope::Host,
                Source::Procfs,
                LINUX,
                true,
                "/proc/stat всей системы: верхняя оценка для процесса без своей cgroup.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::CpuUtilizationPeak,
        unit: Unit::Core,
        summary: "Наибольшая загрузка за один интервал сэмплирования; нижняя оценка истинного пика.",
        measurements: &[
            m(
                Scope::Cgroup,
                Source::CgroupV2,
                LINUX,
                true,
                "Наибольшая разность usage_usec соседних сэмплов, делённая на интервал.",
            ),
            m(
                Scope::Host,
                Source::Procfs,
                LINUX,
                true,
                "Наибольшая загрузка системы по /proc/stat за интервал.",
            ),
        ],
    },
    MetricDefinition {
        metric: MetricId::MemoryHostPeakIncrease,
        unit: Unit::Byte,
        summary: "Максимальное снижение MemAvailable по выборочным наблюдениям с начала прогона; включает влияние посторонних процессов и не является верхней границей потребления цели.",
        measurements: &[m(
            Scope::Host,
            Source::Procfs,
            LINUX,
            true,
            "MemAvailable из /proc/meminfo; включает посторонние процессы.",
        )],
    },
    MetricDefinition {
        metric: MetricId::CpuCycles,
        unit: Unit::Count,
        summary: "Такты процессора.",
        measurements: &[m(
            Scope::ProcessTreeWaited,
            Source::Hyperfine,
            UNIX,
            false,
            COUNTER_TREE,
        )],
    },
    MetricDefinition {
        metric: MetricId::Instructions,
        unit: Unit::Count,
        summary: "Выполненные инструкции.",
        measurements: &[m(
            Scope::ProcessTreeWaited,
            Source::Hyperfine,
            UNIX,
            false,
            COUNTER_TREE,
        )],
    },
    MetricDefinition {
        metric: MetricId::CacheReferences,
        unit: Unit::Count,
        summary: "Обращения к кэшу по общему событию процессора; определение зависит от CPU.",
        measurements: &[m(
            Scope::ProcessTreeWaited,
            Source::Hyperfine,
            LINUX,
            false,
            COUNTER_TREE,
        )],
    },
    MetricDefinition {
        metric: MetricId::CacheMisses,
        unit: Unit::Count,
        summary: "Промахи кэша; определение события зависит от CPU.",
        measurements: &[m(
            Scope::ProcessTreeWaited,
            Source::Hyperfine,
            LINUX,
            false,
            COUNTER_TREE,
        )],
    },
    MetricDefinition {
        metric: MetricId::BranchMisses,
        unit: Unit::Count,
        summary: "Неверно предсказанные переходы.",
        measurements: &[m(
            Scope::ProcessTreeWaited,
            Source::Hyperfine,
            LINUX,
            false,
            COUNTER_TREE,
        )],
    },
    MetricDefinition {
        metric: MetricId::PhaseDuration,
        unit: Unit::Second,
        summary: "Длительность названной фазы внутри измеряемой команды по журналу модуля.",
        measurements: &[m(
            Scope::Phase,
            Source::ModuleLog,
            ALL_OS,
            false,
            "Разность меток времени журнала модуля; точность задаёт декларация (resolution).",
        )],
    },
    MetricDefinition {
        metric: MetricId::ModuleCounter,
        unit: Unit::Count,
        summary: "Счётчик, который модуль сам пишет в машинный отчёт прогона.",
        measurements: &[m(
            Scope::ModuleReport,
            Source::ModuleReport,
            ALL_OS,
            false,
            "Значение поля машинного отчёта модуля; смысл задаёт метка и описание декларации.",
        )],
    },
    MetricDefinition {
        metric: MetricId::DiskUsage,
        unit: Unit::Byte,
        summary: "Размер названного каталога или ресурса после прогона.",
        measurements: &[m(
            Scope::Path,
            Source::Filesystem,
            ALL_OS,
            false,
            "Выделенные байты дерева без перехода по символическим ссылкам.",
        )],
    },
];

/// Предельные значения политики прогонов и шагов для вида нагрузки. Они
/// защищают от случайно бесконечного или неоправданно дорогого сценария.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunLimits {
    pub max_warmup: u32,
    pub max_runs: u32,
    pub max_step_timeout_seconds: u64,
    pub max_run_timeout_seconds: u64,
    pub max_total_budget_seconds: u64,
    pub single_run_allowed: bool,
}

impl WorkloadKind {
    pub const fn limits(self) -> RunLimits {
        match self {
            WorkloadKind::ShortCli => RunLimits {
                max_warmup: 100,
                max_runs: 1000,
                max_step_timeout_seconds: 1800,
                max_run_timeout_seconds: 600,
                max_total_budget_seconds: 3600,
                single_run_allowed: false,
            },
            WorkloadKind::LongE2e => RunLimits {
                max_warmup: 3,
                max_runs: 10,
                max_step_timeout_seconds: 4 * 3600,
                max_run_timeout_seconds: 4 * 3600,
                max_total_budget_seconds: 24 * 3600,
                single_run_allowed: true,
            },
        }
    }
}

/// Канонический ключ метрики в результате: `метрика@scope` или `метрика@scope/метка`.
pub fn metric_key(metric: MetricId, scope: Scope, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("{metric}@{scope}/{label}"),
        None => format!("{metric}@{scope}"),
    }
}

/// Идентификатор: строчные латинские буквы, цифры, `-`, `_`, без пустых сегментов.
pub fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && value.as_bytes()[0].is_ascii_lowercase()
}

/// Стабильный ID сценария: `<модуль>.<имя>`, где обе части — идентификаторы.
pub fn is_scenario_id(value: &str, module: &str) -> bool {
    match value.split_once('.') {
        Some((prefix, name)) => prefix == module && is_identifier(name),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_metric_once_with_its_base_unit() {
        for metric in MetricId::ALL {
            let found: Vec<_> = REGISTRY.iter().filter(|d| d.metric == *metric).collect();
            assert_eq!(found.len(), 1, "{metric}");
            assert_eq!(found[0].unit, metric.base_unit(), "{metric}");
            assert!(!found[0].measurements.is_empty(), "{metric}");
        }
    }

    #[test]
    fn identifiers_are_strict() {
        assert!(is_identifier("anki-e2e"));
        assert!(!is_identifier("Anki"));
        assert!(!is_identifier("1abc"));
        assert!(!is_identifier(""));
        assert!(is_scenario_id("anki-e2e.full", "anki-e2e"));
        assert!(!is_scenario_id("anki-e2e.full", "anki-repo"));
        assert!(!is_scenario_id("anki-e2e", "anki-e2e"));
        assert!(!is_scenario_id("anki-e2e.a.b", "anki-e2e"));
    }
}
