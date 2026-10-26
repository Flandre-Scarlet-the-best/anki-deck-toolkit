# Каталог бенчмарков и контракт результатов

**Русский** · [English](benchmarks.en.md) · [简体中文](benchmarks.zh-CN.md)

Каталог — машинная форма того, что [методика](README.md) описывает словами:
какие сценарии измеряются, в каких условиях, что входит в измеряемый интервал и
какие метрики с каким охватом собираются. Контракт результата — версионируемый
JSON, в котором исполнитель записывает измерения. Оба контракта проверяет
`benchmark-contract` ([CLI и коды возврата](../../tools/benchmark-contract/README.md)).

Каталог и проверка ничего не запускают и не измеряют. Исполнитель на Rust с
Hyperfine 2.0, сбор метрик cgroup, история прогонов и пороги — отдельные
компоненты, которые используют этот контракт, а не создают свой. Журналы модулей
(`docs/performance/<модуль>.md`) остаются свидетельствами конкретных окружений.

## Где что лежит

| Путь | Что это |
|---|---|
| `benchmarks/catalog.toml` | версия схемы, корень репозитория и общий словарь профилей |
| `benchmarks/scenarios/<модуль>.toml` | декларации сценариев одного модуля; имя файла равно имени модуля |
| `tools/benchmark-contract/` | типизированные модели, загрузка, проверка, импорт Hyperfine, CLI |
| `tools/benchmark-contract/tests/fixtures/` | учебный каталог и результаты для тестов (`status = "example"`) |

Обнаружение фиксировано: загрузчик читает `catalog.toml` и все `*.toml` в
`scenarios/` без вложенных каталогов. Скрытые файлы пропускаются, любые другие
записи и символические ссылки — ошибка, а не молча пропущенная декларация.

Сценарии на сегодня (источник истины — сами декларации, состав меняется):

| Сценарий | Вид | Профили |
|---|---|---|
| `anki-e2e.full` | `long-e2e` | `cold`, `warm` |
| `anki-e2e.smoke` | `long-e2e` | `warm`, `offline-warm` |
| `anki-repo.validate` | `short-cli` | `warm`, `offline-warm` |
| `anki-repo.stats` | `short-cli` | `warm` |

## Как зарегистрировать сценарий нового модуля

1. Создайте `benchmarks/scenarios/<модуль>.toml` с `schema_version = 1` и
   `module = "<модуль>"`. Код и `catalog.toml` не меняются.
2. Объявите сценарии `[[scenario]]` с ID `<модуль>.<имя>`.
3. Проверьте каталог и посмотрите, как он понят:

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- show <модуль>.<имя>
```

Минимальная декларация короткой CLI-операции:

```toml
schema_version = 1
module = "example-tool"

[[scenario]]
id = "example-tool.check"
title = "Проверка входного файла"
revision = 1
kind = "short-cli"
status = "executable"
description = "Что делает команда и чем она интересна."
measured = "Один запуск от старта процесса до выхода; сборка и прогрев не входят."
platforms = ["linux"]

[scenario.target]
program = { kind = "cargo-bin", package = "example-tool", bin = "example-tool" }
args = ["--json", "check", "{input.data}"]
expect_exit_codes = [0]

[scenario.target.env]
inherit = ["PATH"]

[[scenario.parameter]]
name = "data_path"
description = "Путь к входу; не публикуется."
required = true
record = false
affects_comparability = false

[[scenario.input]]
id = "data"
kind = "external-path"
role = "workload"
identity = "content-tree-sha256"
parameter = "data_path"
description = "Вход команды."
size = [{ name = "bytes", unit = "byte", description = "Размер входа." }]

[scenario.runs]
warmup = 3
min_runs = 10
max_runs = 100
single_run_allowed = false
run_timeout_seconds = 60
total_budget_seconds = 900

[[scenario.profile]]
id = "warm"
cache_definition = "Собственных кэшей нет; page cache ОС прогрет тремя запусками."
cleared = []
retained = ["page cache ОС"]

[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[scenario.environment]
record = ["os", "arch", "cpu_model", "module_version", "hyperfine_version"]
comparable = ["os", "arch", "cpu_model"]
```

## Поля декларации

| Поле | Обязательно | Смысл |
|---|---|---|
| `id` | да | стабильный ID `<модуль>.<имя>`; не меняется, пока сценарий тот же |
| `title`, `description` | да | название и что именно измеряется |
| `revision` | да | версия нагрузки и семантики, с 1 |
| `kind` | да | `short-cli` или `long-e2e`; задаёт пределы прогонов |
| `status` | да | `executable` — настоящая команда; `example` — учебный пример, никогда не измеряется |
| `measured` | да | граница измеряемого интервала словами |
| `platforms` | да | `linux`, `macos`, `windows`: где сценарий измеряется |
| `target` | да | `program`, `args`, `working_dir` (от корня репозитория, по умолчанию `.`), `env.set`, `env.inherit`, `expect_exit_codes` |
| `parameter` | нет | `name`, `description`, `required`, `default`, `choices`, `record`, `affects_comparability` |
| `input` | нет | входы с видом, ролью, идентичностью и объёмом |
| `runs` | да | `warmup`, `min_runs`, `max_runs`, `single_run_allowed`, `run_timeout_seconds`, `total_budget_seconds` |
| `setup`, `cleanup` | нет | шаги жизненного цикла всего сценария |
| `profile` | да | привязки профилей с точным определением кэша и шагами `setup`, `prepare`, `conclude`; `args_append` — аргументы, добавляемые к цели в этом профиле, `required_parameters` — параметры, которые профиль делает обязательными (например, `required_parameters = ["anki_version"]`) |
| `metric` | да | запрошенные метрики; `time_wall_clock` со scope `command` обязательна |
| `environment` | да | факты, которые записываются (`record`) и сравниваются (`comparable`), и собственные факты модуля (`fact`) |
| `requires` | нет | `executable`, `docker`, `linux-cgroup-v2`, `perf-events`, `network` (с профилями) |

`program` — либо `{ kind = "cargo-bin", package, bin, cargo_profile }` (по
умолчанию `release`; пакет и исполняемый файл сверяются с манифестами рабочей
области Cargo), либо `{ kind = "executable", name }` из `PATH`. Шаги задаются
argv, а не строкой shell: `sh`, `bash`, `env`, `sudo`, `rm`, `dd`, обёртки запуска
вроде `nice` и `timeout` и подобные запрещены. Этот список ловит очевидные ошибки
декларации, но не исчерпывающий и не является границей безопасности: декларации
пишут сопровождающие, и их содержимое проверяется ревью. В аргументах и значениях `env.set` допустимы подстановки
`{param.<имя>}` и `{input.<id>}` (только для `repo-file` и `external-path`);
литеральная скобка удваивается. Необязательный параметр без значения по
умолчанию подставлять нельзя, если профиль не перечислил его в
`required_parameters`. Параметр, который не подставляется ни в одну команду и не
задаёт вход `external-path`, даёт предупреждение: его записанное значение не
влияло бы на прогон.

## Идентичность, ревизия и измерение

Три вещи не смешиваются:

- **ID сценария** — стабильное имя бенчмарка;
- **ревизия** — версия его нагрузки и семантики. Увеличьте её, если изменились
  аргументы цели, входы или способ их выбора, граница измерения, определение
  профиля или смысл метрики. Результаты разных ревизий не сопоставляются;
- **дата и окружение** — свойства одной записи результата, а не сценария.

## Жизненный цикл и граница измерения

Порядок для одной серии «сценарий × профиль»:

1. Исполнитель собирает `cargo-bin` (`cargo build --locked -p … --bin … --profile …`). Это подготовка.
2. `setup` сценария, затем `setup` профиля — один раз.
3. Прогревочные прогоны: `prepare` → цель → `conclude`; значения не хранятся.
4. Измеряемые прогоны: `prepare` → **цель** → `conclude`. Измеряется только цель.
5. `cleanup` сценария — всегда, в том числе после отказа и прерывания.

Шаги соответствуют `--setup`, `--prepare`, `--conclude` и `--cleanup`
Hyperfine 2.0, который исполняет их через shell: исполнитель экранирует argv
сам. Отказ `setup` даёт итог `failure` без прогонов; отказ `prepare`, таймаут
или неожиданный код завершения цели прекращают серию, и итог становится
`partial`, если до этого был успешный прогон, иначе `failure`. Каждый шаг
ограничен `timeout_seconds`, прогон — `run_timeout_seconds`, вся серия —
`total_budget_seconds`; исчерпание бюджета прекращает серию.

`touches` шага говорит, чего он касается: `read-only`, `scenario-temp`
(временные данные самого сценария) или `module-owned` (состояние модуля, которое
он очищает штатной командой). Значения «всё» нет: сброс глобального page cache,
`docker … prune` без `--filter` и чистка чужих кэшей декларацией не выражаются.

## Профили и состояние кэша

`catalog.toml` объявляет профили: `cold`, `warm`, `offline-warm`, `incremental`.
Профиль задаёт базовое состояние кэша и сеть, а сценарий в привязке профиля —
**точное** определение: `cache_definition`, `cleared` (что приводится в нужное
состояние) и `retained` (что намеренно сохраняется). «Холодный» никогда не
означает неявную очистку page cache ОС.

- `cold` — `prepare` обязан сбрасывать состояние модуля шагом `module-owned`,
  `cleared` не пуст, прогрева нет. Холодный для модуля, но не для системы.
- `warm` — нужен прогрев: `runs.warmup ≥ 1` или `setup` профиля; `prepare` не
  сбрасывает состояние модуля.
- `incremental` — `prepare` вносит изменение входа, вызывающее частичную пересборку.
- офлайн-профиль обязан заполнить `network_definition`: какие обращения
  исключены и как это обеспечивается. Исполнитель запускает серию без сети, а
  сетевая ошибка — неуспех прогона. Офлайн-профиль не может стоять в
  требовании `network`.

## Политика прогонов и пределы

| Предел | `short-cli` | `long-e2e` |
|---|---|---|
| прогрев | ≤ 100 | ≤ 3 |
| прогонов | ≤ 1000 | ≤ 10 |
| таймаут прогона | 1…600 с | 1…14 400 с |
| бюджет серии | ≤ 3600 с | ≤ 86 400 с |
| одиночный прогон | запрещён | допустим (`single_run_allowed`) |

Без `single_run_allowed` нужно `min_runs ≥ 2`. Сценарий с тёплым профилем требует
`min_runs ≥ 2` всегда: тёплое состояние измеряется минимум дважды. Бюджет обязан вмещать хотя бы
один прогон. Пределы выводит `benchmark-contract vocabulary`.

## Входы и объём нагрузки

| Вид | Идентичность | Где задан путь |
|---|---|---|
| `repo-tree` | `content-tree-sha256` | рабочая копия |
| `repo-file` | `file-sha256`, `content-tree-sha256` | `path` в декларации; файл обязан существовать |
| `external-path` | `file-sha256`, `content-tree-sha256` | параметр с `record = false` и без `default`: путь задаётся только при запуске и не публикуется |
| `downloaded-artifact` | `file-sha256`, `declared-version` | внешний артефакт |

Роль `subject` — предмет сравнения (например, код): разная идентичность
ожидаема. Роль `workload` — нагрузка: она обязана объявить характеристики объёма
(`byte` или `count`), а результаты с разной идентичностью или объёмом нагрузки
несопоставимы.

## Метрики: scope, источник, единица

Базовые единицы: `second`, `byte`, `count`, `core` (среднее число занятых ядер:
процессорные секунды за секунду). Других единиц в контракте нет. Ключ метрики в
результате — `метрика@scope` или `метрика@scope/метка`.

| Метрика | Единица | Scope / источник | Что это |
|---|---|---|---|
| `time_wall_clock` | second | `command` / `hyperfine` | время одного запуска цели |
| `time_user`, `time_system`, `time_cpu` | second | `process-tree-waited` / `hyperfine`; `process` или `process-tree-waited` / `procfs` | процессорное время; потомки — только дождавшиеся |
| `memory_peak_resident` | byte | `largest-process-in-tree` / `hyperfine`; `process` / `procfs` | пиковый RSS **одного** процесса, не сумма дерева |
| `memory_peak_cgroup` | byte | `cgroup` / `cgroup-v2` | одновременный пик всей группы (`memory.peak`) |
| `cpu_time_cgroup` | second | `cgroup` / `cgroup-v2` | процессорное время всей группы (`usage_usec`) |
| `cpu_utilization_mean`, `cpu_utilization_peak` | core | `cgroup` / `cgroup-v2`; `host` / `procfs` | наблюдаемая загрузка; пик — нижняя оценка за интервал |
| `memory_host_peak_increase` | byte | `host` / `procfs` | снижение `MemAvailable`: верхняя оценка по всей системе |
| `cpu_cycles`, `instructions`, `cache_references`, `cache_misses`, `branch_misses` | count | `process-tree-waited` / `hyperfine` | счётчики процессора; нужен `perf_event` |
| `phase_duration` | second | `phase` / `module-log` | фаза внутри цели по журналу модуля; нужны метка и `resolution` |
| `module_counter` | count | `module-report` / `module-report` | счётчик из машинного отчёта модуля; нужна метка |
| `disk_usage` | byte | `path` / `filesystem` | размер названного ресурса; нужна метка |

Сочетание вне реестра — ошибка `metric_scope_source`, а не новая метрика.
Полный реестр с платформами — `benchmark-contract vocabulary`.

Пиковый RSS процесса, пик памяти cgroup, процессорное время и наблюдаемая
загрузка — разные величины, и одна не подменяет другую. Hyperfine видит только
процесс цели и дождавшихся потомков: процессы контейнера Docker и шаги BuildKit
в его `time_cpu` и `memory_peak_resident` не входят. На Linux очень малый пик
RSS Hyperfine может заменить унаследованным от себя значением.

Обязательную метрику нельзя обещать на платформе, где её источник недоступен:
это ошибка; для необязательной — предупреждение, а в результате она будет
`unsupported`. Обязательные счётчики процессора требуют `requires` с
`perf-events`, обязательные метрики cgroup — `linux-cgroup-v2`. Для проверки
каталога права на cgroup и `perf_event` не нужны.

## Окружение и сопоставимость

`environment.record` перечисляет факты, которые записываются в результат;
`os`, `arch` и `module_version` обязательны. `environment.comparable` —
подмножество, которое обязано совпадать; `os` и `arch` входят всегда. Встроенные
факты: `os`, `arch`, `kernel_release`, `virtualization`, `cpu_model`,
`logical_cpus`, `memory_total_bytes`, `module_version`, `rustc_version`,
`docker_version`, `hyperfine_version`; модуль объявляет свои в `fact`.
Собирать все возможные характеристики железа не требуется.

Два результата сопоставимы (`benchmark-contract compare`), только если совпадают
сценарий и ревизия, профиль, идентичность и объём нагрузки, параметры с
`affects_comparability`, известные значения сравниваемых фактов и определения
метрик. Неизвестный факт сопоставимость не доказывает. Различие входа-предмета
показывается отдельно и не мешает сравнению. Статистические выводы по метрике
допустимы, только если в каждом результате у неё не меньше двух значений
успешных прогонов: неуспешная попытка значения не даёт. Такие метрики
перечисляет `statistical_metrics`, а `statistically_supported` истинно, когда
это верно для каждой сопоставимой метрики.

## Контракт результата

Результат — JSON с `schema_version = 1`, отдельный от формата Hyperfine. Пример ниже
сокращён: в нём опущены часть фактов окружения, размеров входа, метрик и прогонов,
поэтому `check-result` его не примет. Полные корректные результаты —
`tools/benchmark-contract/tests/fixtures/results/`.

```json
{
  "schema_version": 1,
  "scenario": { "id": "anki-repo.validate", "module": "anki-repo", "revision": 1, "kind": "short-cli" },
  "profile": { "id": "warm", "cache": "warm", "network": "online" },
  "inputs": [{ "id": "export", "role": "workload",
               "identity": { "method": "content-tree-sha256", "value": "<64 hex>" },
               "size": { "deck_json_bytes": { "value": 5242880.0, "unit": "byte" } } }],
  "provenance": {
    "recorded_at": "2026-10-27T12:00:00Z",
    "measurement": { "name": "hyperfine", "version": "2.0.0", "format": "hyperfine-json-v2" },
    "code": { "module_version": "0.1.0", "git_commit": "<40 hex>", "dirty": false }
  },
  "environment": { "os": { "status": "known", "value": "linux" },
                   "cpu_model": { "status": "unavailable", "reason": "…" } },
  "outcome": { "status": "success" },
  "observation": "repeated",
  "warmup_runs": 3,
  "metrics": [{ "key": "time_wall_clock@command", "metric": "time_wall_clock", "scope": "command",
                "source": "hyperfine", "unit": "second", "availability": "measured" }],
  "runs": [{ "index": 0, "status": "success", "exit_code": 0,
             "values": { "time_wall_clock@command": { "status": "measured", "value": 0.0123 } } }],
  "aggregates": { "time_wall_clock@command": { "count": 10, "mean": 0.0125, "stddev": 0.0004,
                                               "median": 0.0124, "min": 0.0119, "max": 0.0133 } }
}
```

Статусы:

- итог `outcome.status`: `success` — все прогоны успешны и обязательные метрики
  измерены в каждом, прогонов не меньше `min_runs`; `partial` — есть успешный
  прогон, но серия или метрики неполны; `failure` — ни одного успешного прогона;
  `unsupported` — сценарий не может измеряться здесь, прогонов нет. Всё, кроме
  `success`, требует `reason`;
- прогон: `success`, `failure`, `timeout`, `interrupted`; неуспех требует `reason`.
  Успешный прогон фиксирует `exit_code`, и при проверке по каталогу код обязан
  входить в `expect_exit_codes` сценария; иначе это `failure`;
- значение метрики в прогоне: `measured` с `value` либо `unavailable`,
  `unsupported`, `failed` с `reason`. Каждая метрика присутствует в каждом
  прогоне. Отсутствие никогда не записывается нулём;
- `availability` метрики обязана следовать из прогонов: `measured`, `partial`
  или один из статусов отсутствия.

`warmup_runs` при проверке по каталогу следует из жизненного цикла: если
измеряемые прогоны начались, оно равно `runs.warmup`; без прогонов серия могла
прерваться на `setup` или посреди прогрева, поэтому допустимо и меньшее число,
но не большее. Прогрев через `setup` профиля в `warmup_runs` не входит.

`observation`: `none` без прогонов, `single` для одного — валидное наблюдение
без статистики, `repeated` для двух и более попыток. Агрегаты необязательны и считаются
только по измеренным значениям успешных прогонов; `stddev` — выборочное
отклонение (n − 1), `null` для одного значения. Проверка пересчитывает агрегаты
и отвергает расхождение и агрегат без первичных значений.

`provenance` хранит технические идентификаторы (SHA коммита, хэш дерева) в самой
записи результата. В постоянную документацию, в том числе в журналы модулей,
они не переносятся — это правило [`AGENTS.md`](../../AGENTS.md).

Публикуемый результат не содержит путей внутри домашних каталогов, токенов,
закрытых ключей, учётных данных и адресов почты: проверка просматривает все
строки и ключи документа. Значения параметров с `record = false` в результат не
попадают, переменные окружения процесса — тоже.

## Импорт Hyperfine 2.0

Библиотечный `benchmark_contract::hyperfine` читает `--export-json` Hyperfine
2.0 (`schema_version: 2`); экспорт 1.x и другие версии отвергаются явно. Формат
сверен с тегом `v2.0.0`.

| Hyperfine | Контракт |
|---|---|
| `measurements[j]` | `runs[j]`; прогревы в экспорт не входят |
| `exit_code` | `exit_code`; вне `expect_exit_codes` или `null` (сигнал) — прогон `failure` |
| `time_wall_clock` | `time_wall_clock@command` |
| `time_user`, `time_system`, `time_cpu` | `…@process-tree-waited` |
| `memory_peak_resident` | `memory_peak_resident@largest-process-in-tree` |
| счётчики процессора | `…@process-tree-waited` |
| опущенная метрика | `unavailable` с причиной |
| `summary` | обязательна для метрики, которая есть во всех `measurements`, и сверяется с пересчётом по всем им; отсутствие или расхождение — отказ импорта. При неполном наборе значений Hyperfine сводку не пишет |
| `metadata.hyperfine_version`, `start_time`, `platform` | `provenance.measurement`, время записи, факты `os`/`arch` |

`command` и `environment` экспорта в результат не переносятся: в них могут быть
пути и значения окружения пользователя, а цель однозначно задают сценарий,
ревизия и параметры. Метрики экспорта, которые сценарий не запрашивал,
перечисляются в `unrequested`, а не теряются молча.

## Проверка

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- list
cargo run -p benchmark-contract -- preflight anki-e2e.full
cargo run -p benchmark-contract -- check-result result.json
cargo run -p benchmark-contract -- compare before.json after.json
cargo run -p benchmark-contract -- --json vocabulary
```

`validate` проверяет только декларации и не требует Docker, Anki или Hyperfine.
Платформу и доступность зависимостей текущего окружения проверяет отдельно
`preflight`: ОС вне `platforms` сценария — отрицательный итог, и только чтением `PATH`, `/sys/fs/cgroup` и `/proc/sys`. Диагностика называет файл,
сценарий, поле и стабильный код причины. Набор `cargo test --workspace` проверяет
и настоящий каталог, и учебные фикстуры.

## Эволюция схемы

- `schema_version` каталога и результата — целое число. Инструмент понимает
  только известные версии и явно отказывает в остальных, не пытаясь читать
  документ частично.
- Новое поле или значение перечисления, которое старый инструмент не должен
  молча игнорировать, увеличивает версию. Схемы закрыты: неизвестное поле — ошибка.
- Переход между версиями — документированное детерминированное преобразование
  от версии N к N + 1. Его реализуют вместе с версией N + 1; универсального
  механизма миграций заранее нет.
- Хранилище истории сохраняет результаты с их `schema_version` и не
  переписывает их.
- Ревизия сценария и версия схемы независимы: новая ревизия не меняет схему,
  новая схема не меняет ревизии сценариев.

## Как контракт используют следующие компоненты

Исполнитель загружает каталог библиотекой, выбирает сценарий и профиль,
выполняет `preflight`, собирает `cargo-bin`, подставляет параметры и входы,
вычисляет идентичность и объём входов, запускает Hyperfine 2.0 с шагами
жизненного цикла и `--export-json`, импортирует экспорт, добавляет метрики
других источников (cgroup, журнал и отчёт модуля), заполняет окружение и
provenance, проверяет результат и сохраняет его. История и сравнения используют
`compare`, а пороги — только сопоставимые результаты.

## Границы

Здесь нет исполнителя, запуска Hyperfine, управления cgroup, сбора CPU/RAM/I/O,
хранилища истории, расчёта регрессий и порогов CI. Числа журналов модулей не
становятся порогами.
