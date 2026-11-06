# Производительность: `asset-store`

**Русский** · [English](../../en/asset-store/performance.md) · [简体中文](../../zh-CN/asset-store/performance.md)

Производительность файлового ядра хранилища ресурсов
([контракт хранилища и исполнитель нагрузки](README.md)):
методика, граница измерения, характеристики синтетической нагрузки и последние
подтверждённые снимки по сценариям каталога. Общие правила замера — в
[методике](../performance/README.md), поля сценариев — в [каталоге бенчмарков](../performance/benchmarks.md).

## Что измеряется

Полный браузерный путь Forvo через MV3 и production CLI измеряется
сценарием `anki-e2e.forvo-batch` (10/50/128 разных пар и несколько
последовательных пакетов). Его граница, состояние кэша, запуск и отчёт —
в [каталоге](../performance/benchmarks.md) и
[контракте длительного browser E2E](../anki-e2e/README.md#длительные-изолированные-пакеты).
Файловые сценарии ниже не доказывают скорость получения MP3 и устойчивость
браузерной сессии; синтетический локальный fixture также не измеряет
скорость настоящего Forvo.

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
| `asset-store.batch-checkpoint` | `open`, `load` и `save`; `change` = `unchanged` или `modified` | `modified` — новый blob, ревизия + 1 и атомарная запись состояния с синхронизацией файла и каталога; `unchanged` — потоковое сравнение байтов и `fsync` существующего файла и каталога без новой записи |

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
[`performance-benchmarking`](../../../.agents/skills/performance-benchmarking/SKILL.md)).

## Нормализатор MP3

Сценарий `asset-store.audio-normalizer-batch` измеряет штатный
`pronunciation-assets normalizer batch-start` ([контракт](normalizer.md))
от старта процесса до выхода, включая все дочерние FFmpeg, ffprobe и
`prlimit`. Вход — внешний каталог MP3 (`corpus_path`, путь не публикуется,
идентичность — SHA-256 дерева); рабочий каталог принадлежит сценарию и
помечен маркером `asset-store-bench`. Профиль `cold` перед каждым прогоном
пересоздаёт пустой рабочий каталог: измеряется вся работа. Профиль `warm`
заполняет кэш результатов прогревочным пакетом и перед каждым прогоном
удаляет только состояние пакета: измеряется повторный запуск без повторной
обработки. Постусловие вне интервала — пакет обработал все уникальные входы,
вход не изменился. Параметры `mode` (`analyze`/`normalize`) и `jobs` влияют на
сопоставимость.

Воспроизводимая синтетическая нагрузка:

```bash
cargo run --locked --release -p asset-store --bin asset-store-bench -- \
  audio-corpus --out target/benchmark-workloads/audio-corpus-300 --count 300 --seed 1
cargo run -p benchmark-runner -- run asset-store.audio-normalizer-batch --profile cold \
  --param corpus_path=target/benchmark-workloads/audio-corpus-300 --param mode=normalize
```

Генератор детерминирован по seed: тоны 150–900 Гц с **пиковым** уровнем до
кодирования от −44 до −8 dBFS (амплитуда 1/8 фильтра FFmpeg `sine` учтена;
это уровень пика, а не громкость в LUFS), краевая тишина 0–1.2 с, внутренняя
пауза у части клипов, моно/стерео, 22.05/44.1/48 кГц. Версия генератора 2;
у версии 1 уровни фактически были около −36…0 dBFS, поэтому её результаты —
другая нагрузка и с результатами версии 2 не сравниваются. Скорость единственного Forvo worker этим сценарием не
измеряется и требованием к CPU/DSP-обработке не является.

## Снимки

Блоки ниже генерирует `benchmark-history snapshot` из проверенных результатов
`benchmark-runner` в эталонном окружении; если блоков нет, подтверждённого снимка
ещё не было. Снимки опубликованы для спецификации `kanji-medium`; результаты
остальных объёмов доступны в локальной истории командой
`cargo run -p benchmark-history -- list --module asset-store`.

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-11-04T11:51:13Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 120.37 ms ± 1.04 ms | 120.32 ms | 118.62 ms – 121.7 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 45.9 ms ± 9.39 ms | 44.14 ms | 35.37 ms – 59.55 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 71.99 ms ± 9.68 ms | 73.78 ms | 58.37 ms – 83.16 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.14 MiB ± 35.67 KiB | 13.15 MiB | 13.07 MiB – 13.18 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 448 770 495.5 ± 31 810.6 | 448 755 487.5 | 448 754 826 – 448 830 936 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 119.27 ms ± 836.3 µs | 119.15 ms | 118.07 ms – 120.4 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.18 MiB ± 4.62 KiB | 10.18 MiB | 10.17 MiB – 10.18 MiB | 10 |
| `cpu_utilization_peak@host` | host / procfs | 1.32 ядра ± 0.23 ядра | 1.37 ядра | 0.98 ядра – 1.77 ядра | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-11-04T11:51:16Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 57.18 ms ± 1.61 ms | 56.49 ms | 55.02 ms – 60.05 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 17.39 ms ± 6.55 ms | 16.02 ms | 11.77 ms – 32.22 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 39.69 ms ± 6.96 ms | 41.73 ms | 24.17 ms – 45.91 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.19 MiB ± 73.4 KiB | 8.21 MiB | 8.03 MiB – 8.32 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 149 192 383.6 ± 8 038.9 | 149 192 529.5 | 149 184 238 – 149 200 121 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 58.5 ms ± 1.62 ms | 57.83 ms | 56.39 ms – 61.31 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.05 MiB ± 4.32 KiB | 5.05 MiB | 5.05 MiB – 5.06 MiB | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-11-04T11:55:21Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 40.753 s ± 193.59 ms | 40.75 s | 40.548 s – 41.065 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 13.369 s ± 258.14 ms | 13.333 s | 13.096 s – 13.677 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 27.373 s ± 176.51 ms | 27.436 s | 27.063 s – 27.491 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.71 MiB ± 63.54 KiB | 8.7 MiB | 8.65 MiB – 8.79 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 126 928 363 376.4 ± 10 270 752 | 126 935 859 767 | 126 917 109 576 – 126 935 868 133 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 40.743 s ± 196.04 ms | 40.741 s | 40.534 s – 41.059 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.5 MiB ± 171.73 KiB | 5.43 MiB | 5.43 MiB – 5.8 MiB | 5 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-11-04T11:55:59Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 3.468 s ± 50.58 ms | 3.483 s | 3.398 s – 3.532 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.123 s ± 23.43 ms | 1.127 s | 1.095 s – 1.147 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.531 s ± 51.79 ms | 1.529 s | 1.462 s – 1.603 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 14.09 MiB ± 145.04 KiB | 14.15 MiB | 13.89 MiB – 14.25 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 098 853 576.6 ± 666 360.9 | 12 098 888 596 | 12 098 111 478 – 12 099 876 790 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.656 s ± 36.1 ms | 2.654 s | 2.607 s – 2.701 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 13.85 MiB ± 413.72 KiB | 13.71 MiB | 13.55 MiB – 14.55 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-11-04T11:56:40Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 5 из 5, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 4.057 s ± 37.13 ms | 4.068 s | 4 s – 4.101 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.457 s ± 57.71 ms | 1.484 s | 1.393 s – 1.524 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.748 s ± 58.68 ms | 1.754 s | 1.657 s – 1.82 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 58.64 MiB ± 573.19 KiB | 58.57 MiB | 58.23 MiB – 59.59 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 294 236 125.6 ± 1 948 157.7 | 16 294 423 896 | 16 291 948 643 – 16 297 145 643 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.211 s ± 40.07 ms | 3.227 s | 3.155 s – 3.251 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 59.09 MiB ± 528.73 KiB | 58.97 MiB | 58.49 MiB – 59.91 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.79 ядра ± 0 ядер | 0.79 ядра | 0.78 ядра – 0.79 ядра | 5 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-11-04T11:56:42Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 6.78 ms ± 153.5 µs | 6.76 ms | 6.52 ms – 7.02 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 4.27 ms ± 3.17 ms | 6.51 ms | 0 s – 6.79 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 2.03 ms ± 3.27 ms | 0 s | 0 s – 6.87 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 6.27 MiB ± 113.34 KiB | 6.28 MiB | 6.04 MiB – 6.43 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 29 222 644.1 ± 799.3 | 29 222 983.5 | 29 220 565 – 29 223 251 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 8.1 ms ± 164.7 µs | 8.07 ms | 7.85 ms – 8.37 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.08 MiB ± 80.46 KiB | 3.05 MiB | 3.05 MiB – 3.3 MiB | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-11-04T11:57:12Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730968064","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-04 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 2.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730968064 |
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
| `time_wall_clock@command` | command / hyperfine | 16.11 ms ± 4.05 ms | 14.78 ms | 14.05 ms – 27.49 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.65 ms ± 3.48 ms | 0 s | 0 s – 8.38 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 6.91 ms ± 3.67 ms | 8.4 ms | 0 s – 9.28 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 6.17 MiB ± 133.15 KiB | 6.12 MiB | 6.04 MiB – 6.35 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 34 152 710.7 ± 251 | 34 152 761 | 34 152 118 – 34 153 049 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 10.41 ms ± 323.4 µs | 10.45 ms | 10.01 ms – 11 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.3 MiB ± 0 B | 3.3 MiB | 3.3 MiB – 3.3 MiB | 10 |

Не измерено: `io_read_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_bytes@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_read_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно; `io_write_operations@cgroup` — `unsupported`: контроллер io не делегирован группе исполнителя; CPU и память доступны отдельно.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.audio-normalizer-batch","profile":"cold","recorded_at":"2026-11-06T09:18:17Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730972160","os":"linux","virtualization":"wsl2"},"workload":{"corpus":{"bytes":{"value":2915273.0,"unit":"byte"},"files":{"value":301.0,"unit":"count"}}},"parameters":{"jobs":"4","mode":"normalize"}} -->
### `asset-store.audio-normalizer-batch` · профиль `cold`

Последнее подтверждённое измерение: 2026-11-06 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · cold · online · итог `success`: успешных прогонов 3 из 3, прогревов 0.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730972160 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

| Нагрузка | Значение |
|---|---|
| параметр `jobs` | `4` |
| параметр `mode` | `normalize` |
| `corpus` · `bytes` | 2.78 MiB |
| `corpus` · `files` | 301 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 36.811 s ± 55.15 ms | 36.792 s | 36.768 s – 36.873 s | 3 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 90.92 s ± 217.22 ms | 91.031 s | 90.669 s – 91.059 s | 3 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 45.209 s ± 86.68 ms | 45.198 s | 45.128 s – 45.3 s | 3 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 51.4 MiB ± 61.1 KiB | 51.39 MiB | 51.35 MiB – 51.46 MiB | 3 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 137.342 s ± 303.48 ms | 137.443 s | 137.001 s – 137.582 s | 3 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 157.63 MiB ± 3.33 MiB | 156.66 MiB | 154.88 MiB – 161.33 MiB | 3 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 3.73 ядра ± 0 ядер | 3.73 ядра | 3.73 ядра – 3.74 ядра | 3 |
| `disk_usage@path/workspace` | path / filesystem | 4.39 MiB ± 0 B | 4.39 MiB | 4.39 MiB – 4.39 MiB | 3 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.audio-normalizer-batch","profile":"warm","recorded_at":"2026-11-06T09:18:56Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730972160","os":"linux","virtualization":"wsl2"},"workload":{"corpus":{"bytes":{"value":2915273.0,"unit":"byte"},"files":{"value":301.0,"unit":"count"}}},"parameters":{"jobs":"4","mode":"normalize"}} -->
### `asset-store.audio-normalizer-batch` · профиль `warm`

Последнее подтверждённое измерение: 2026-11-06 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 3 из 3, прогревов 0.

| Факт окружения | Значение |
|---|---|
| `arch` ★ | x86_64 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `hyperfine_version` | 2.0.0 |
| `kernel_release` | 6.18.54.1-microsoft-standard-WSL2 |
| `logical_cpus` ★ | 16 |
| `memory_total_bytes` ★ | 16730972160 |
| `module_version` | 0.1.0 |
| `os` ★ | linux |
| `rustc_version` | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| `virtualization` ★ | wsl2 |

| Нагрузка | Значение |
|---|---|
| параметр `jobs` | `4` |
| параметр `mode` | `normalize` |
| `corpus` · `bytes` | 2.78 MiB |
| `corpus` · `files` | 301 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 318.58 ms ± 11.19 ms | 320.54 ms | 306.54 ms – 328.67 ms | 3 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 145.11 ms ± 19.16 ms | 137.55 ms | 130.88 ms – 166.89 ms | 3 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 134.61 ms ± 14.89 ms | 136.93 ms | 118.69 ms – 148.21 ms | 3 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 47.91 MiB ± 208.19 KiB | 47.81 MiB | 47.79 MiB – 48.15 MiB | 3 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 284.33 ms ± 5.87 ms | 283.7 ms | 278.8 ms – 290.5 ms | 3 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 18.43 MiB ± 122.57 KiB | 18.48 MiB | 18.29 MiB – 18.52 MiB | 3 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.85 ядра ± 0.05 ядра | 0.86 ядра | 0.8 ядра – 0.89 ядра | 3 |
| `disk_usage@path/workspace` | path / filesystem | 4.39 MiB ± 0 B | 4.39 MiB | 4.39 MiB – 4.39 MiB | 3 |

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

Эти наблюдения относятся к прежнему, более медленному диску, на котором лежал
виртуальный диск WSL2. Снимки выше перемерены на `kanji-medium` после переноса
`ext4.vhdx` на NVMe-накопитель Samsung SSD 980 1TB. Тип и модель накопителя
`benchmark-runner` в факты окружения не записывает, поэтому они указаны здесь.
Операции, ограниченные `fsync`, на NVMe заметно дешевле. Прежние значения
взяты из заменённых снимков тех же сценариев и нагрузок от 2026-10-30
(локальная история). Они не совпадают с таблицей и гипотезами ниже: те описывают
самый первый замер. Сравнение по сценариям:

- `batch-checkpoint` `modified`: 16 мс в среднем (14–27 мс) вместо 0,9 с, хвосты
  до 3,1 с исчезли;
- `ingest`, 50 ресурсов: 3,5 с вместо 6,5 с;
- `ingest-contention`, четыре писателя: 4,1 с вместо 7,2 с.

Операции чтения и проверки изменились мало (в пределах 10 %). Таблица и гипотезы ниже остаются записью первого замера и
с числами снимков напрямую не сравниваются.

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
