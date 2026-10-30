# Производительность: `asset-store`

**Русский** · [English](performance.en.md) · [简体中文](performance.zh-CN.md)

Производительность файлового ядра хранилища ресурсов
([контракт хранилища и исполнитель нагрузки](README.md)):
методика, граница измерения, характеристики синтетической нагрузки и последние
подтверждённые снимки по сценариям каталога. Общие правила замера — в
[методике](../../docs/performance/README.md), поля сценариев — в [каталоге бенчмарков](../../docs/performance/benchmarks.md).

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
копии, для конкуренции — `cpu_utilization_mean` подгруппы, а также
`io_read_bytes`, `io_write_bytes`, `io_read_operations` и
`io_write_operations` подгруппы, если контроллер `io` делегирован; иначе эти
метрики имеют статус `unsupported`. В сценарии
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
остальных объёмов доступны в локальной истории командой
`cargo run -p benchmark-history -- list --module asset-store`.

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-10-30T09:11:45Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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
| `time_wall_clock@command` | command / hyperfine | 130.08 ms ± 5.18 ms | 127.78 ms | 124.5 ms – 141.49 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 46.63 ms ± 10.64 ms | 47 ms | 31.37 ms – 74.43 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 78.66 ms ± 12.05 ms | 77.27 ms | 50.92 ms – 103.29 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.08 MiB ± 44.21 KiB | 13.08 MiB | 13 MiB – 13.16 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 450 637 253.4 ± 33 378.5 | 450 618 622.5 | 450 617 837 – 450 693 945 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 127.23 ms ± 4.42 ms | 125.64 ms | 122.78 ms – 137.06 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.09 MiB ± 171.73 KiB | 10.17 MiB | 9.92 MiB – 10.43 MiB | 20 |
| `cpu_utilization_peak@host` | host / procfs | 1.42 ядра ± 0.3 ядра | 1.37 ядра | 0.98 ядра – 2.16 ядра | 20 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-10-30T09:11:55Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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
| `time_wall_clock@command` | command / hyperfine | 61.69 ms ± 6.11 ms | 60.09 ms | 57.47 ms – 84.47 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 15.09 ms ± 6.81 ms | 14.13 ms | 3.91 ms – 28.13 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 46.11 ms ± 8.1 ms | 45.7 ms | 32.15 ms – 66.92 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.02 MiB ± 53.68 KiB | 8.03 MiB | 7.91 MiB – 8.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 152 367 392.6 ± 7 577.4 | 152 367 272 | 152 359 850 – 152 375 530 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 63.4 ms ± 6.28 ms | 61.77 ms | 58.94 ms – 86.48 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.06 MiB ± 57.59 KiB | 5.05 MiB | 5.05 MiB – 5.3 MiB | 20 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-10-30T09:16:16Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

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
| `time_wall_clock@command` | command / hyperfine | 42.51 s ± 163.7 ms | 42.466 s | 42.362 s – 42.778 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 14.292 s ± 173.88 ms | 14.361 s | 14.057 s – 14.45 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 28.209 s ± 192.69 ms | 28.237 s | 27.904 s – 28.398 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.59 MiB ± 54.96 KiB | 8.58 MiB | 8.53 MiB – 8.67 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 131 418 249 466.6 ± 8 381 519.9 | 131 414 502 426 | 131 414 496 797 – 131 433 242 784 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 42.504 s ± 163.77 ms | 42.459 s | 42.356 s – 42.772 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.77 MiB ± 153.66 KiB | 5.7 MiB | 5.67 MiB – 6.03 MiB | 5 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-10-30T09:18:53Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 1.

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
| `time_wall_clock@command` | command / hyperfine | 6.511 s ± 501.56 ms | 6.713 s | 5.577 s – 6.882 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.141 s ± 36.3 ms | 1.142 s | 1.096 s – 1.215 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.589 s ± 42.13 ms | 1.582 s | 1.535 s – 1.646 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.88 MiB ± 120.24 KiB | 13.91 MiB | 13.68 MiB – 14 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 115 724 980.5 ± 1 125 063.5 | 12 115 554 476 | 12 114 231 093 – 12 118 333 731 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.733 s ± 41.52 ms | 2.727 s | 2.674 s – 2.792 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 13.7 MiB ± 222.63 KiB | 13.64 MiB | 13.47 MiB – 14.23 MiB | 10 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-10-30T09:20:26Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

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
| `time_wall_clock@command` | command / hyperfine | 7.243 s ± 672.88 ms | 7.49 s | 6.362 s – 7.833 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.553 s ± 70.45 ms | 1.551 s | 1.453 s – 1.652 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.909 s ± 71.54 ms | 1.932 s | 1.827 s – 1.981 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 57.96 MiB ± 1.31 MiB | 57.8 MiB | 56.43 MiB – 59.65 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 318 162 906.2 ± 3 528 893.4 | 16 320 050 082 | 16 313 859 768 – 16 321 129 316 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.47 s ± 110.69 ms | 3.396 s | 3.394 s – 3.641 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 59.19 MiB ± 1.57 MiB | 60.02 MiB | 57.07 MiB – 60.51 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.48 ядра ± 0.05 ядра | 0.45 ядра | 0.43 ядра – 0.54 ядра | 5 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-10-30T09:20:35Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 2.

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
| `time_wall_clock@command` | command / hyperfine | 7.02 ms ± 276.4 µs | 6.97 ms | 6.69 ms – 8.01 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 4.1 ms ± 2.66 ms | 3.46 ms | 0 s – 7.23 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 2.6 ms ± 2.76 ms | 3.35 ms | 0 s – 7.85 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.92 MiB ± 86.98 KiB | 5.95 MiB | 5.79 MiB – 6.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 28 973 335 ± 312 | 28 973 367 | 28 972 603 – 28 973 906 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 8.5 ms ± 382.5 µs | 8.41 ms | 8.15 ms – 9.84 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.05 MiB ± 3.78 KiB | 3.05 MiB | 3.05 MiB – 3.06 MiB | 20 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-10-30T09:22:17Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

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
| `time_wall_clock@command` | command / hyperfine | 904.93 ms ± 966.88 ms | 1.149 s | 23.5 ms – 3.145 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 2.81 ms ± 2.59 ms | 3.38 ms | 0 s – 6.08 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 8.83 ms ± 6.72 ms | 7.91 ms | 3.04 ms – 26.65 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.91 MiB ± 74.61 KiB | 5.92 MiB | 5.79 MiB – 6.02 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 33 898 460.2 ± 328.2 | 33 898 486.5 | 33 898 020 – 33 899 002 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 13.86 ms ± 5.75 ms | 11.49 ms | 10.75 ms – 29.16 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.3 MiB ± 2.53 KiB | 3.3 MiB | 3.29 MiB – 3.3 MiB | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Исходные наблюдения

Наблюдения ниже получены из принятых JSON-результатов текущей, ещё не
оптимизированной реализации в эталонном окружении снимков (WSL2 с ext4 на
виртуальном диске, AMD Ryzen 7 5800X3D, 16 логических CPU). Это первое
измерение: сравнивать не с чем, и выводов «быстрее/медленнее» здесь нет.
Числа — средние `time_wall_clock` серий; полные результаты и их ID доступны
в локальной истории: `cargo run -p benchmark-history -- list --module asset-store`.

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
