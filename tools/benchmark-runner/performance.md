# Производительность: `benchmark-runner`

**Русский** · [English](performance.en.md) · [简体中文](performance.zh-CN.md)

Измерения самого исполнителя бенчмарков
([интерфейс и семантика](README.md)): насколько
точно он измеряет нагрузку с заранее известной ценой, что именно попадает в
значения каждой метрики и сколько стоят опрос `/proc` и собственная cgroup
прогона. Общие правила замера — в [методике](../../docs/performance/README.md).

## Что измеряется

Сценарии `benchmark-runner.calibration-*` каталога запускают
`benchmark-workload` из этого пакета — синтетическую нагрузку, цена которой
известна заранее. Они не описывают производительность `asset-store`,
`anki-repo` или Anki.

| Сценарий | Нагрузка | Ожидаемое |
|---|---|---|
| `calibration-noop` | запуск и выход процесса | нижняя граница времени и памяти, цена Hyperfine в значениях группы |
| `calibration-cpu` | один поток, 50 мс собственного процессорного времени | `time_cpu` ≈ 50 мс плюс запуск процесса |
| `calibration-cpu-plain` | та же цель только с метриками Hyperfine | сравнение показывает цену дополнительного мониторинга |
| `calibration-parallel` | четыре потока по 1000 мс | `time_cpu` ≈ 4 с, время ≈ 1 с, загрузка группы ≈ 4 ядра |
| `calibration-memory` | запись каждой страницы блока 256 MiB, удержание 200 мс | `memory_peak_resident` ≈ 256 MiB плюс база процесса |
| `calibration-io` | новый файл 16 MiB: запись, `fsync`, сброс собственных чистых страниц, проверенное чтение и удаление | положительные учтённые байты и операции; физические итоги зависят от файловой системы и устройств |

## Как замерять

Цена опроса оценивается парами серий одной цели: с опросом `/proc` и cgroup
и с `--no-sampling`, а цена собственной cgroup — сравнением
`calibration-cpu` с `calibration-cpu-plain`. Серии чередуются, чтобы дрейф
частоты процессора и фоновая нагрузка распределялись поровну. Результаты разных
сценариев контракт не сопоставляет: такое сравнение — наблюдение методики, а
не вердикт `compare`.

```bash
cargo run -p benchmark-runner -- check benchmark-runner.calibration-cpu --profile warm
cargo run -p benchmark-runner -- run benchmark-runner.calibration-noop --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu --profile warm --runs 30 --no-sampling
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu-plain --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-parallel --profile warm --runs 10
cargo run -p benchmark-runner -- run benchmark-runner.calibration-parallel --profile warm --runs 10 --no-sampling
cargo run -p benchmark-runner -- run benchmark-runner.calibration-memory --profile warm --runs 10
cargo run -p benchmark-runner -- run benchmark-runner.calibration-io --profile warm --runs 5
cargo run -p benchmark-history -- list --module benchmark-runner
```

## Снимки

Блоки ниже генерирует `benchmark-history snapshot` из проверенных результатов
`benchmark-runner` в эталонном окружении; если блоков нет, подтверждённого снимка
ещё не было.

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-noop","profile":"warm","recorded_at":"2026-10-29T09:23:01Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-noop` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 30 из 30, прогревов 3.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730976256 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

Сценарий не объявляет входов.

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 809.2 µs ± 74.5 µs | 794.4 µs | 710.3 µs – 963.3 µs | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 734.3 µs ± 73.5 µs | 703 µs | 646 µs – 895 µs | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 0 s ± 0 s | 0 s | 0 s – 0 s | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 734.3 µs ± 73.5 µs | 703 µs | 646 µs – 895 µs | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.23 MiB ± 42.86 KiB | 3.24 MiB | 3.12 MiB – 3.27 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 342 173.6 ± 339.9 | 342 133.5 | 341 560 – 342 862 | 30 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.55 ms ± 344.1 µs | 2.62 ms | 1.87 ms – 3.2 ms | 30 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 777.33 KiB ± 46.79 KiB | 768 KiB | 768 KiB – 1 MiB | 30 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.16 ядра ± 0.02 ядра | 0.17 ядра | 0.12 ядра – 0.2 ядра | 30 |
| `memory_host_peak_increase@host` | host / procfs | 0 B ± 0 B | 0 B | 0 B – 0 B | 30 |

Не измерено: `cpu_utilization_peak@host` — `unavailable`: прогон короче интервала сэмплирования 0.05 с: ни одного полного окна.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-cpu","profile":"warm","recorded_at":"2026-10-29T09:23:04Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-cpu` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 30 из 30, прогревов 3.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730976256 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

Сценарий не объявляет входов.

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 51.14 ms ± 74.2 µs | 51.13 ms | 51 ms – 51.27 ms | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 36.78 ms ± 7.74 ms | 36.84 ms | 21.25 ms – 50.88 ms | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 14.21 ms ± 7.73 ms | 14.21 ms | 0 s – 29.74 ms | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.99 ms ± 67 µs | 50.98 ms | 50.88 ms – 51.18 ms | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.22 MiB ± 49.01 KiB | 3.22 MiB | 3.12 MiB – 3.3 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 297 994 746 ± 4 162 825.9 | 298 483 162.5 | 281 630 938 – 303 915 923 | 30 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 52.41 ms ± 156.9 µs | 52.4 ms | 52.09 ms – 52.78 ms | 30 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 1.03 MiB ± 88.3 KiB | 1020 KiB | 1020 KiB – 1.25 MiB | 30 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.62 ядра ± 0.06 ядра | 0.61 ядра | 0.51 ядра – 0.68 ядра | 30 |
| `cpu_utilization_peak@host` | host / procfs | 1 ядро ± 0.45 ядра | 0.8 ядра | 0.55 ядра – 2.29 ядра | 30 |
| `memory_host_peak_increase@host` | host / procfs | 107.6 KiB ± 404.87 KiB | 0 B | 0 B – 2.16 MiB | 30 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-cpu-plain","profile":"warm","recorded_at":"2026-10-29T09:23:09Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-cpu-plain` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 30 из 30, прогревов 3.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730976256 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

Сценарий не объявляет входов.

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 51.13 ms ± 95.4 µs | 51.1 ms | 50.97 ms – 51.36 ms | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 35.63 ms ± 6.65 ms | 35.3 ms | 23.5 ms – 47.03 ms | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 15.35 ms ± 6.65 ms | 15.68 ms | 3.92 ms – 27.54 ms | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.98 ms ± 86.4 µs | 50.96 ms | 50.85 ms – 51.16 ms | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.23 MiB ± 42.78 KiB | 3.24 MiB | 3.12 MiB – 3.27 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 297 858 672.8 ± 4 313 498.9 | 299 127 087.5 | 282 486 820 – 303 040 058 | 30 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-parallel","profile":"warm","recorded_at":"2026-10-29T09:23:30Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-parallel` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730976256 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

Сценарий не объявляет входов.

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 1.002 s ± 128.9 µs | 1.002 s | 1.002 s – 1.002 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 2.797 s ± 62.96 ms | 2.791 s | 2.713 s – 2.905 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.205 s ± 62.91 ms | 1.211 s | 1.097 s – 1.289 s | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 4.002 s ± 136.4 µs | 4.002 s | 4.002 s – 4.002 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.2 MiB ± 60.25 KiB | 3.21 MiB | 3.12 MiB – 3.27 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 23 045 047 645.4 ± 216 056 843.4 | 23 085 225 076.5 | 22 692 549 917 – 23 309 911 676 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 4.004 s ± 239.4 µs | 4.004 s | 4.003 s – 4.004 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 1.97 MiB ± 80.95 KiB | 2 MiB | 1.75 MiB – 2 MiB | 10 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 3.87 ядра ± 0.03 ядра | 3.87 ядра | 3.79 ядра – 3.91 ядра | 10 |
| `cpu_utilization_peak@host` | host / procfs | 6.01 ядра ± 1 ядро | 5.89 ядра | 4.52 ядра – 8.1 ядра | 10 |
| `memory_host_peak_increase@host` | host / procfs | 84.8 KiB ± 111.79 KiB | 0 B | 0 B – 244 KiB | 10 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-memory","profile":"warm","recorded_at":"2026-10-29T09:24:09Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-memory` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730976256 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

Сценарий не объявляет входов.

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 325.77 ms ± 2.04 ms | 325.39 ms | 322.97 ms – 329.49 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 11.74 ms ± 6.23 ms | 12.01 ms | 0 s – 20.42 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 113.82 ms ± 6.79 ms | 113.81 ms | 105.86 ms – 129.17 ms | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 125.56 ms ± 2.04 ms | 125.2 ms | 122.76 ms – 129.17 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 258.18 MiB ± 92.11 KiB | 258.19 MiB | 258.07 MiB – 258.31 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 869 123.4 ± 363.9 | 869 130 | 868 555 – 869 797 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 126.99 ms ± 2 ms | 126.63 ms | 124.28 ms – 130.64 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 257.25 MiB ± 127.31 KiB | 257.25 MiB | 257.11 MiB – 257.5 MiB | 10 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.35 ядра ± 0.01 ядра | 0.35 ядра | 0.35 ядра – 0.36 ядра | 10 |
| `cpu_utilization_peak@host` | host / procfs | 1.55 ядра ± 0.71 ядра | 1.18 ядра | 0.98 ядра – 2.94 ядра | 10 |
| `memory_host_peak_increase@host` | host / procfs | 212.91 MiB ± 52.52 MiB | 233.39 MiB | 93.84 MiB – 266.87 MiB | 10 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Выводы калибровки

Выводы ниже получены из принятых JSON-результатов этих сценариев в эталонном
окружении снимков (WSL2, AMD Ryzen 7 5800X3D, 16 логических CPU,
`perf_event_paranoid = 2`, делегирование cgroup через `systemd-run --user`):
чередующиеся серии по 30 прогонов для 50 мс и по 10 для параллельной и
памяти. Полные результаты и их ID — в локальной истории и в описании PR.

- **Процессорное время точно.** `time_cpu` нагрузки 50 мс — 50,91–51,03 мс:
  на 0,9–1,0 мс больше заданного, что сопоставимо с ценой запуска пустого
  процесса (0,73 мс) и создания рабочего потока.
  Четыре потока по 1 с дают 4,0018–4,0019 с, `time_wall_clock` — 1,0017 с.
- **Значения группы включают Hyperfine.** `cpu_time_cgroup` больше `time_cpu`
  цели на ≈ 1,4 мс (52,27–52,48 мс против 50,96–51,03 мс); для пустой цели
  группа тратит 2,5 мс при 0,73 мс самой цели.
- **Средняя загрузка группы** — процессорное время группы за время её жизни,
  то есть за весь вызов Hyperfine. Для коротких целей время жизни группы
  заметно больше времени цели, поэтому значение — свойство пары «цель и
  Hyperfine»: 0,16 ядра у пустой цели, 0,62–0,67 у однопоточной 50 мс, 3,85–3,88
  у четырёх потоков по 1 с. До исправления знаменателем было время одной цели,
  и пустая однопоточная цель получала 3,35 ядра.
- **Резидентная память точна, охваты различаются.** Блок 256 MiB
  (268,4 МБ) дал `memory_peak_resident` 270,6–270,9 МБ. `memory_peak_cgroup` того
  же прогона на ≈ 1 МБ меньше: страницы исполняемого файла, уже находящиеся в
  page cache, начислены другой группе. У пустой цели RSS 3,4 МБ, а пик группы
  0,8 МБ — это разные величины, а не погрешность.
- **`memory_host_peak_increase` ненадёжна для выводов.** Для того же блока
  256 MiB метрика дала 98–280 МБ (223 ± 55 МБ): память завершившегося процесса
  возвращается системе с задержкой, поэтому базовая точка следующего прогона
  смещена, а одновременное освобождение поднимает `MemAvailable`. При
  удержании 1,5 с и частом опросе падение составило 242–256 МиБ. Метрика —
  наблюдение хоста, не граница; сравнивать по ней серии нельзя.
- **Цена опроса и cgroup в пределах шума.** Средние `time_wall_clock` цели
  50 мс по чередующимся сериям: с опросом 51,14 и 51,16 мс, `--no-sampling`
  51,18 и 51,12 мс, только метрики Hyperfine 51,13 и 51,06 мс при σ 0,06–0,12 мс.
  Для четырёх потоков по 1 с: 1,00172 и 1,00176 с с опросом, 1,00175 и
  1,00183 с без него. Различия меньше разброса серий; влияние меньше 0,2 %
  на коротком и не обнаружено на секундном прогоне.
- **Разрешение.** Опрос `/proc` с интервалом 0,05 с не даёт ни одного окна
  на цели короче интервала: `cpu_utilization_peak@host` пустой цели —
  `unavailable` с причиной, а не ноль. Счётчик `instructions` доступен при
  `perf_event_paranoid = 2`. Относительный разброс зависит от нагрузки:
  около 1,4 % у `calibration-cpu`, 0,9 % у `calibration-parallel`, 10⁻³ у
  `calibration-noop` и 4×10⁻⁴ у `calibration-memory`.

Не измерено: окружение без WSL2, многопроцессная нагрузка, cgroup контейнера
Docker (500 мс поиска) и влияние опроса на цели длиннее нескольких секунд.

Прежнее ручное наблюдение без первичных JSON (2026-10-26, синтетический
каталог `overhead.*`) заменено выводами выше. Из него сохраняется
конструктивное решение: в подгруппу прогона переносится Hyperfine до начала
измерения, а не цель — перенос процесса между группами перед созданием потоков
стоил около 17 мс ожидания ядра и добавлял к времени цели 20–25 мс.
