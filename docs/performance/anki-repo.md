# Производительность: `anki-repo`

**Русский** · [English](anki-repo.en.md) · [简体中文](anki-repo.zh-CN.md)

Производительность коротких команд анализа CrowdAnki-экспорта
([контракт команд](../../tools/anki-repo/README.md)): последний подтверждённый
снимок по сценариям каталога. Общие правила замера — в [методике](README.md),
поля сценариев — в [каталоге бенчмарков](benchmarks.md).

## Что измеряется

- `anki-repo.validate` — `anki-repo --json validate` над одним экспортом: чтение
  и разбор `deck.json`, разрешение моделей, структурные проверки.
- `anki-repo.stats` — `anki-repo --json stats`: типизированная загрузка и
  агрегирование по дереву колод.

Измеряется собранный release-бинарник от старта процесса до выхода; сборка,
прогревы и вычисление идентичности экспорта в интервал не входят. Метрики:
`time_wall_clock` (обязательная), `time_cpu` и `memory_peak_resident` для обоих
сценариев; `instructions` дополнительно объявлена у `anki-repo.validate`
(без доступа к `perf_event` — `unsupported` с причиной).

## Нагрузка

Экспорт передаётся параметром `export_dir` и в результат не попадает: нагрузку
представляют идентичность содержимого дерева и объём (`deck_json_bytes`,
`notes`, `media_files`). Пользовательские колоды в публичный снимок не
попадают: эталонная нагрузка снимка — синтетический экспорт без медиа, его
объём указан в блоке. Сравнивать можно только результаты с той же
идентичностью и тем же объёмом нагрузки.

## Как замерять

```bash
cargo run -p benchmark-runner -- check anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export --runs 20
cargo run -p benchmark-history -- list --scenario anki-repo.validate
cargo run -p benchmark-history -- snapshot <ID> --page docs/performance/anki-repo.md
```

## Снимки

Блоки ниже генерирует `benchmark-history snapshot` из проверенных результатов
`benchmark-runner` в эталонном окружении; если блоков нет, подтверждённого снимка
ещё не было.

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"warm","recorded_at":"2026-10-29T09:25:46Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 3.

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `media_files` | 0 |
| `export` · `notes` | 20 000 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 55.44 ms ± 2.66 ms | 54.7 ms | 51.78 ms – 63.92 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 55.34 ms ± 2.66 ms | 54.62 ms | 51.65 ms – 63.81 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.75 MiB ± 117.19 KiB | 40.78 MiB | 40.52 MiB – 40.92 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 886 426.5 ± 449.7 | 416 886 413 | 416 885 686 – 416 887 467 | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"offline-warm","recorded_at":"2026-10-29T09:25:49Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · профиль `offline-warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · offline · итог `success`: успешных прогонов 20 из 20, прогревов 3.

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `media_files` | 0 |
| `export` · `notes` | 20 000 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 55.63 ms ± 1.61 ms | 55.58 ms | 53.04 ms – 59.22 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 55.53 ms ± 1.62 ms | 55.49 ms | 52.93 ms – 59.15 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.71 MiB ± 107.95 KiB | 40.72 MiB | 40.49 MiB – 40.89 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 886 564.2 ± 506.6 | 416 886 537.5 | 416 885 652 – 416 887 666 | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-repo.stats","profile":"warm","recorded_at":"2026-10-29T09:25:51Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.stats` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 20 из 20, прогревов 3.

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `notes` | 20 000 |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 50.69 ms ± 1.65 ms | 50.5 ms | 48.56 ms – 54.17 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.59 ms ± 1.65 ms | 50.42 ms | 48.44 ms – 54.09 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 38.77 MiB ± 60.73 KiB | 38.76 MiB | 38.65 MiB – 38.88 MiB | 20 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->
