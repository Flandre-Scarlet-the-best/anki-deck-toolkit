# Производительность: `asset-store`

**Русский** · [English](asset-store.en.md) · [简体中文](asset-store.zh-CN.md)

Производительность файлового ядра хранилища ресурсов
([контракт хранилища и исполнитель нагрузки](../../tools/asset-store/README.md)):
методика, граница измерения, характеристики синтетической нагрузки и последние
подтверждённые снимки по сценариям каталога. Общие правила замера — в
[методике](README.md), поля сценариев — в [каталоге бенчмарков](benchmarks.md).

## Что измеряется

Сценарии `asset-store.*` исполняет `asset-store-bench` — постоянный исполнитель
настоящих операций `AssetStore` и `SafeBatchRuntime`. Каждый сценарий измеряет
один класс операций, чтобы будущая гипотеза об I/O, блокировках, повторной
проверке или масштабировании проверялась на своей границе:

| Сценарий | Операция | Что входит в интервал |
|---|---|---|
| `asset-store.verify-integrity` | `open_existing` и `verify_integrity` | проверка владельца, манифестов и runtime-границы, наличие каждого файла под разделяемой блокировкой; байты не хешируются |
| `asset-store.read-batch` | `visit_verified_variants_with_policy` | одна блокировка и одна проверка манифеста на весь набор, чтение файлов со сверкой размера, формата и SHA-256 |
| `asset-store.read-single` | тот же набор по одному вызову `read_verified_variants_with_policy` | блокировка, разбор и проверка всего манифеста на каждый запрос |
| `asset-store.ingest` | `ingest_verified` новых идентичностей одним писателем | staging, проверка валидатором, атомарная фиксация файла, перезапись манифеста, интент и закрытие журнала, `fsync` |
| `asset-store.ingest-contention` | тот же объём публикации у `writers` независимых дескрипторов | сериализация на штатном `flock` между писателями-потоками одного процесса |
| `asset-store.batch-reopen` | `SafeBatchRuntime::open` и `load` | блокировка пакета, чтение и проверка состояния, полная сверка каждого blob |
| `asset-store.batch-checkpoint` | `open`, `load` и `save`; `change` = `unchanged` или `modified` | атомарная запись состояния с синхронизацией файла и каталога; `modified` — ещё новый blob и ревизия + 1 |

Во всех сценариях интервал — один запуск собранного release-бинарника
`asset-store-bench` от старта процесса до выхода, включая открытие хранилища.
Построение эталона, восстановление рабочей копии и проверка постусловий в
интервал не входят. Метрики: `time_wall_clock` (обязательная), `time_user`,
`time_system`, `memory_peak_resident`, `instructions`, `cpu_time_cgroup` и
`memory_peak_cgroup` подгруппы прогона, для публикации — `disk_usage` рабочей
копии, для конкуренции — `cpu_utilization_mean` подгруппы. В сценарии
`verify-integrity` метрика `cpu_utilization_peak@host` — выборочное наблюдение
загрузки всего хоста по `/proc/stat`, а не верхняя граница загрузки цели.

## Нагрузка

Нагрузку задаёт спецификация `benchmarks/workloads/asset-store/*.toml`, путь к
ней — параметр `workload_spec`; в результат попадают SHA-256 файла и объём из
пробы `asset-store-bench describe`. Генератор детерминирован: настоящие GIF и
PNG 48×48 из блоков псевдослучайных оттенков, blob пакета — псевдослучайные
байты по 4 KiB. Версия генератора входит в спецификацию, поэтому её SHA-256
описывает и содержимое корпуса.

| Спецификация | Домен | Записей корпуса | Чтений | Публикаций за прогон | Записей пакета / blob |
|---|---|---|---|---|---|
| `kanji-small` | `kanji` | 250 (200 GIF + 50 PNG) | 250 | 20 | 200 / 50 |
| `kanji-medium` | `kanji` | 1250 (1000 GIF + 250 PNG) | 1250 | 50 | 1000 / 250 |
| `kanji-large` | `kanji` | 3000 (2400 GIF + 600 PNG) | 1250 | 100 | 4000 / 1000 |
| `generic-medium` | `generic` | 1000 | 1000 | 50 | 1000 / 250 |

`kanji-large` читает столько же записей, что `kanji-medium`: пара отделяет
влияние размера манифеста от числа чтений. `generic-medium` проверяет общее ядро
без форматных вариантов и с адресацией по содержимому.

Изоляция и постусловия:

- данные — только синтетический корпус в `target/benchmark-workloads/asset-store`
  рабочей копии; пользовательский `.asset-store/` и колоды не читаются;
- изменяющие сценарии перед каждым прогоном восстанавливают рабочую копию из
  эталона с `fsync` каждого файла: старт одинаков и уже записан на диск;
- после каждого прогона вне интервала проверяются: для чтений — неизменность
  SHA-256 дерева эталона; для публикации — `verify_integrity`, число записей,
  байты прежних и новых записей по SHA-256, чистый журнал без открытых
  интентов и пустой staging; для пакета — ревизия, состав и новый blob;
- повторная публикация без восстановления, повреждённые байты ресурса или
  blob, посторонний файл в эталоне дают отказ, а не успешный прогон (это
  закрепляют тесты исполнителя и исполнимость деклараций в CI);
- эталон переиспользуется между сериями, только если его построил тот же
  исполняемый файл по той же спецификации и дерево совпало с записанным.

Не входит в измерение: предметный `KanjiImageValidator` и OCR (валидатор
нагрузки только декодирует изображение), браузерное и сетевое получение
(Chromium, Yarxi, JPDB), локальный inference, домены `pitch_accent` и
`pronunciation_audio`, межпроцессная конкуренция с запуском процессов.

## Как замерять

```bash
cargo run -p benchmark-runner -- check asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml
cargo run -p benchmark-runner -- run asset-store.read-batch --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 20
cargo run -p benchmark-runner -- run asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 10
cargo run -p benchmark-runner -- run asset-store.ingest-contention --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param writers=4 --runs 5
cargo run -p benchmark-runner -- run asset-store.batch-checkpoint --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param change=modified --runs 10
cargo run -p benchmark-history -- list --module asset-store
cargo run -p benchmark-history -- baseline <ID>
```

Серии одного модуля в рабочей копии не идут параллельно (код `7`). Пока идёт
серия, не меняй неигнорируемые файлы рабочей копии: иначе итог не `success`.
Для оптимизации измерь базу до изменения и кандидата после тем же сценарием,
спецификацией и профилем и сравни их `benchmark-history compare`
(порядок — skill
[`performance-benchmarking`](../../.agents/skills/performance-benchmarking/SKILL.md)).

## Снимки

Блоки ниже генерирует `benchmark-history snapshot` из проверенных результатов
`benchmark-runner` в эталонном окружении; если блоков нет, подтверждённого снимка
ещё не было. Снимки опубликованы для спецификации `kanji-medium`; результаты
остальных объёмов — в локальной истории и в описании PR.

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-10-29T09:28:20Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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

| Нагрузка | Значение |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `records` | 1 250 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 132.96 ms ± 4.92 ms | 131.43 ms | 129.07 ms – 150.55 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 43.41 ms ± 9.23 ms | 44.15 ms | 19.5 ms – 66.54 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 85.18 ms ± 7.27 ms | 82.62 ms | 75.24 ms – 105.3 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.47 MiB ± 83.48 KiB | 13.49 MiB | 13.3 MiB – 13.58 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 462 864 770.8 ± 37 649.2 | 462 834 963 | 462 834 579 – 462 910 287 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 130.18 ms ± 4.85 ms | 129.11 ms | 125.33 ms – 147.02 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.46 MiB ± 101 KiB | 10.47 MiB | 10.22 MiB – 10.72 MiB | 20 |
| `cpu_utilization_peak@host` | host / procfs | 1.52 ядра ± 0.74 ядра | 1.27 ядра | 0.98 ядра – 3.92 ядра | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-10-29T09:28:24Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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

| Нагрузка | Значение |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 62.93 ms ± 1.49 ms | 62.37 ms | 61.83 ms – 67.44 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 18.75 ms ± 8.32 ms | 18.19 ms | 4.15 ms – 29.39 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 44.06 ms ± 8.45 ms | 45.57 ms | 32.9 ms – 58.14 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.36 MiB ± 78.93 KiB | 8.33 MiB | 8.24 MiB – 8.61 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 165 084 275.4 ± 7 631.5 | 165 084 461.5 | 165 076 141 – 165 092 234 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 64.2 ms ± 1.48 ms | 63.69 ms | 62.94 ms – 68.72 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.76 MiB ± 56.71 KiB | 5.75 MiB | 5.74 MiB – 5.99 MiB | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-10-29T09:32:37Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

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

| Нагрузка | Значение |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 42.017 s ± 242.61 ms | 42.028 s | 41.767 s – 42.388 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 14.022 s ± 120.57 ms | 13.985 s | 13.925 s – 14.232 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 27.984 s ± 287.21 ms | 27.838 s | 27.779 s – 28.453 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.62 MiB ± 67.85 KiB | 8.63 MiB | 8.55 MiB – 8.7 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 131 427 996 154.4 ± 8 366 164.3 | 131 431 737 418 | 131 413 030 305 – 131 431 739 634 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 42.007 s ± 242.32 ms | 42.019 s | 41.759 s – 42.38 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.86 MiB ± 163.75 KiB | 5.75 MiB | 5.75 MiB – 6.07 MiB | 5 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-10-29T09:37:54Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 1.

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

| Нагрузка | Значение |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 5.626 s ± 448.34 ms | 5.485 s | 5.386 s – 6.886 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.103 s ± 53.34 ms | 1.1 s | 1.005 s – 1.21 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.573 s ± 60.28 ms | 1.564 s | 1.454 s – 1.659 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 15.11 MiB ± 67.04 KiB | 15.13 MiB | 14.95 MiB – 15.18 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 133 819 476 ± 770 993.2 | 12 133 903 097.5 | 12 132 326 506 – 12 134 762 910 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.678 s ± 23.91 ms | 2.668 s | 2.65 s – 2.723 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 14.87 MiB ± 134.94 KiB | 14.83 MiB | 14.7 MiB – 15.12 MiB | 10 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 10 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-10-29T09:41:57Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

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

| Нагрузка | Значение |
|---|---|
| параметр `writers` | `4` |
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 6.233 s ± 24.3 ms | 6.222 s | 6.211 s – 6.268 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.461 s ± 44.93 ms | 1.463 s | 1.408 s – 1.524 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.783 s ± 35.48 ms | 1.791 s | 1.722 s – 1.809 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 59.46 MiB ± 592.76 KiB | 59.38 MiB | 58.64 MiB – 60.18 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 329 060 762.6 ± 4 866 892.8 | 16 330 284 016 | 16 321 154 190 – 16 334 340 069 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.25 s ± 29.05 ms | 3.253 s | 3.219 s – 3.281 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 60.16 MiB ± 740.64 KiB | 60.23 MiB | 59.09 MiB – 61 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.52 ядра ± 0 ядер | 0.52 ядра | 0.51 ядра – 0.52 ядра | 5 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-10-29T09:32:39Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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

| Нагрузка | Значение |
|---|---|
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 12.59 ms ± 291.7 µs | 12.58 ms | 12.1 ms – 13.21 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 3.5 ms ± 3.32 ms | 4.16 ms | 0 s – 8.33 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 9 ms ± 3.48 ms | 8.39 ms | 4 ms – 13.13 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.37 MiB ± 96.11 KiB | 8.39 MiB | 8.16 MiB – 8.49 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 41 304 131.1 ± 328.2 | 41 304 149 | 41 303 487 – 41 304 652 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 13.9 ms ± 303.8 µs | 13.83 ms | 13.4 ms – 14.52 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.75 MiB ± 8.24 KiB | 5.75 MiB | 5.74 MiB – 5.77 MiB | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-10-29T09:35:31Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

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

| Нагрузка | Значение |
|---|---|
| параметр `change` | `modified` |
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 151.07 ms ± 383.47 ms | 29.76 ms | 26.91 ms – 1.242 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 6.71 ms ± 4.48 ms | 7.78 ms | 0 s – 11.86 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 8.34 ms ± 5.15 ms | 6.91 ms | 3.57 ms – 18.3 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.34 MiB ± 109.9 KiB | 8.32 MiB | 8.16 MiB – 8.49 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 46 214 832.5 ± 316.2 | 46 214 737.5 | 46 214 439 – 46 215 403 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 16.68 ms ± 1.27 ms | 16.27 ms | 15.87 ms – 19.97 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.75 MiB ± 1.26 KiB | 5.75 MiB | 5.74 MiB – 5.75 MiB | 10 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Исходные наблюдения

Наблюдения ниже получены из принятых JSON-результатов текущей, ещё не
оптимизированной реализации в эталонном окружении снимков (WSL2 с ext4 на
виртуальном диске, AMD Ryzen 7 5800X3D, 16 логических CPU). Это первое
измерение: сравнивать не с чем, и выводов «быстрее/медленнее» здесь нет.
Числа — средние `time_wall_clock` серий; полные результаты и их ID — в
локальной истории и в описании PR.

| Операция | `kanji-small` (250) | `kanji-medium` (1250) | `kanji-large` (3000) | `generic-medium` (1000) |
|---|---|---|---|---|
| `verify-integrity` | 36 мс | 133 мс | 300 мс | 108 мс |
| `read-batch` | 18 мс (250 чтений) | 63 мс (1250) | 113 мс (1250) | 51 мс (1000) |
| `read-single` | 1,72 с (250) | 42,0 с (1250) | 100,2 с (1250) | не измерялось |
| `ingest`, на ресурс | 66 мс (20) | 113 мс (50) | 192 мс (100) | 80 мс (50) |
| `batch-reopen` | 8,0 мс (200 / 50) | 12,6 мс (1000 / 250) | 28,4 мс (4000 / 1000) | 12,2 мс |
| `batch-checkpoint` `modified`, минимум | 21 мс | 27 мс | 61 мс | 28 мс |

Гипотезы для следующей линии разработки (наблюдения, не доказанные причины):

- **Серия из N одиночных чтений N записей имеет квадратичную стоимость.** Один вызов `read_verified_variants_with_policy`
  стоит 6,9 / 33,6 / 80 мс при 250 / 1250 / 3000 записях — пропорционально
  размеру манифеста, даже при одинаковом числе чтений у medium и large.
  Пакетный API на тех же запросах быстрее в 93 раза у small и в 670–890 раз у
  medium и large. Две трети времени — системное (28 из 42 с у medium): вероятная причина — повторные открытие, разбор и
  проверка всего манифеста с обращением к файлам на каждый запрос. Потребителям,
  которые читают много ресурсов, нужен пакетный путь; оптимизация одиночного —
  отдельная задача.
- **Публикация дорожает с размером манифеста.** Цена одного `ingest_verified`
  растёт с 66 до 192 мс, а время ожидания превышает процессорное (у medium
  5,6 с при 2,7 с CPU): вероятные причины — перезапись манифеста целиком и
  несколько `fsync` на публикацию. Построение корпуса из N ресурсов по одному —
  O(N²); эталон `kanji-large` строится около 6 минут.
- **Конкуренция писателей не масштабируется.** 1 / 2 / 4 / 8 писателей
  публикуют тот же объём за 5,54 / 6,41 / 6,23 / 6,83 с: `flock` сериализует
  публикации, а каждый писатель добавляет процессорное время (2,73 → 3,61 с) и
  память своего дескриптора (RSS процесса 16 → 118 МБ). Средняя загрузка
  группы ≈ 0,5 ядра при любом числе писателей.
- **Хвосты `fsync`.** У `batch-checkpoint` минимум 21–61 мс, но отдельные
  прогоны длятся до 1,2 с (σ больше среднего). Вероятная причина —
  синхронизация виртуального диска WSL2; сравнивать checkpoint следует по
  минимуму и квантилям прогонов и с большим числом прогонов.
- **Время системы преобладает** во всех операциях чтения и проверки
  (`time_system` ≈ 2/3 `time_cpu`): кандидаты — число системных вызовов на
  запись и размер манифеста.

Измерено не всё: домены `pitch_accent` и `pronunciation_audio`, предметный
валидатор kanji, браузерное получение, межпроцессная конкуренция, холодный
page cache (глобальный сброс не выполняется) и нативный Linux без WSL2.
