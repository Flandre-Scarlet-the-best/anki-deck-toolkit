# Производительность: `benchmark-runner`

**Русский** · [English](benchmark-runner.en.md) · [简体中文](benchmark-runner.zh-CN.md)

Журнал измерений самого исполнителя бенчмарков
([интерфейс и семантика](../../tools/benchmark-runner/README.md)): насколько
опрос `/proc` и cgroup и перенос в собственную cgroup влияют на время цели и
что именно попадает в значения группы. Общие правила замера — в
[методике](README.md).

## Что измеряется

- **Влияние сэмплера.** Одна и та же короткая нагрузка измеряется попеременными
  сериями с опросом `/proc` хоста, без опроса (`--no-sampling`) и без
  дополнительных метрик вовсе. Сравнивается `time_wall_clock@command`.
- **Влияние собственной cgroup.** Та же нагрузка с метриками `cgroup` (перенос
  Hyperfine в подгруппу прогона) против серии без них.
- **Базовые накладные расходы группы.** `cpu_time_cgroup` и
  `memory_peak_cgroup` для нагрузки с известной ценой процессора и для пустой
  цели: значения группы включают процесс Hyperfine.

## Как замерять

Нагрузка — `benchmark-workload` из этого пакета (`--busy-ms 50`: 50 мс
активной работы одного потока). Синтетический каталог во временном каталоге
объявляет сценарии с одинаковой целью и разным набором метрик; серии чередуются,
чтобы дрейф частоты процессора и фоновая нагрузка распределялись поровну.

```bash
cargo build --release -p benchmark-runner --locked
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30 --no-sampling
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.plain --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.cgroup --profile warm --runs 30
```

## Журнал

### 2026-10-26 · benchmark-runner 0.1.0 · Hyperfine 2.0.0 · серии `--busy-ms 50`

- Окружение: AMD Ryzen 7 5800X3D, 16 логических CPU; WSL2, ядро
  6.18.54.1-microsoft-standard-WSL2, Arch Linux; cgroup v2, делегирование
  через `systemd-run --user --scope`; Docker 29.9.2 (драйвер cgroup `systemd`);
  util-linux 2.42.4; `perf_event_paranoid = 2`. Рабочая копия с изменениями.
- Состояния кэша: тёплое; один прогрев на серию, по 30 измеряемых прогонов,
  серии чередовались.
- Время цели (`time_wall_clock@command`, среднее по серии):

  | Режим | Диапазон средних по сериям | Разброс внутри серии |
  |---|---|---|
  | опрос `/proc` раз в 50 мс | 51,10–51,21 мс | σ ≈ 0,1 мс |
  | `--no-sampling` | 51,12–51,24 мс | σ ≈ 0,1 мс |
  | без дополнительных метрик | 51,15–51,27 мс | σ ≈ 0,1 мс |
  | собственная cgroup (Hyperfine в подгруппе) | 51,23 мс | σ ≈ 0,1 мс |
  | без cgroup в той же паре серий | 51,17 мс | σ ≈ 0,1 мс |

- Ресурсы:

  | Метрика | Значение | Источник |
  |---|---|---|
  | `cpu_time_cgroup` нагрузки 50 мс | ≈ `time_cpu` цели + 3,5 мс | `cpu.stat` подгруппы прогона |
  | `memory_peak_cgroup` пустой цели | ≈ 1,0 МиБ | `memory.peak` подгруппы прогона |

- Выводы: влияние опроса `/proc` и собственной cgroup на время цели в пределах
  шума (≈ 0,1 мс). Значения группы включают Hyperfine: несколько миллисекунд
  процессора и около 1 МиБ памяти на прогон. Первая схема, в которой в
  подгруппу переносилась сама цель, добавляла 20–25 мс ко времени: перенос
  процесса между группами перед созданием потоков стоит около 17 мс ожидания
  ядра. Поэтому переносится Hyperfine до начала измерения.
- Не измерено: влияние на многопоточную нагрузку, сценарии длиннее секунды,
  опрос cgroup контейнера Docker (500 мс поиска) и окружения без WSL2.
