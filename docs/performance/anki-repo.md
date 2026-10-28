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
`time_wall_clock` (обязательная), `time_cpu`, `memory_peak_resident` и
`instructions` (без доступа к `perf_event` — `unavailable` с причиной).

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

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"warm","recorded_at":"2026-10-27T19:23:42Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-27 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 10 из 10, прогревов 3.

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
| `time_wall_clock@command` | command / hyperfine | 54.41 ms ± 1.26 ms | 54.53 ms | 52.02 ms – 56.4 ms | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 54.22 ms ± 1.2 ms | 54.44 ms | 51.91 ms – 56.32 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.63 MiB ± 101.86 KiB | 40.67 MiB | 40.49 MiB – 40.78 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 918 918.6 ± 321.2 | 416 918 810 | 416 918 418 – 416 919 483 | 10 |

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->
