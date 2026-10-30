# Производительность: `anki-e2e`

**Русский** · [English](performance.en.md) · [简体中文](performance.zh-CN.md)

Производительность и потребление ресурсов E2E-стенда Anki Desktop: последний
подтверждённый снимок и прежнее ручное наблюдение
([контракт стенда](README.md)). Общие правила замера и
точность источников — в [методике](../../docs/performance/README.md).

## Что измеряется

- **Длительности фаз** сейчас `unsupported`: `anki-e2e` не публикует
  машиночитаемый формат меток, поэтому длительности не выводятся из
  человекочитаемого `runner.log`. `report.json` содержит число запусков
  `anki-repo` (`commands_started`).
- **Контейнер сценариев** — cgroup v2 его scope: `memory.peak` и `usage_usec`.
- **Сборка образов.** Шаги BuildKit исполняются вне cgroup `docker.service`, и
  отдельной cgroup на прогон у них нет. `/proc/stat` и `MemAvailable` дают
  выборочное наблюдение всего хоста, а не верхнюю границу ресурсов сборки.
- **Runner на хосте** — `VmHWM` и CPU процесса `anki-e2e`.
- **Диск** — `cache/`, `build/`, `runs/` рабочего каталога, образы стенда и
  BuildKit cache mounts `anki-e2e-cargo-*`.

## Как замерять

Состояния кэша для этого модуля:

- *холодный* — после `clean --images --cache`: удалены образы стенда, архив
  Anki, снимки исходников и cache mounts Cargo. Слоевой кэш BuildKit демона и
  базовые образы `ubuntu`/`rust` остаются (они общие, см. методику);
- *тёплый* — повторный `full` без изменений исходников;
- *инкрементальный* — после изменения снимка исходников (например, нового
  неигнорируемого файла): пересобирается только образ запуска.

Прогон:

```bash
cargo build -p anki-e2e --locked
cargo run -p anki-e2e -- clean --images --cache
cargo run -p anki-e2e -- full
```

Во время прогона раз в секунду читай cgroup контейнера стенда и `/proc`
процесса runner. Контейнер удаляется в конце прогона, поэтому `memory.peak`
нужно сэмплировать, а не читать после завершения. При одном активном прогоне
ID контейнера и PID runner задаются командами ниже. Путь scope — для
cgroup-драйвера `systemd`; при драйвере `cgroupfs` это
`/sys/fs/cgroup/docker/<ID>`.

```bash
container_id=$(docker ps --filter label=io.github.anki-deck-toolkit.e2e=1 --no-trunc --format '{{.ID}}' | head -n 1)
runner_pid=$(pgrep -n -x anki-e2e)
cat "/sys/fs/cgroup/system.slice/docker-${container_id}.scope/memory.peak"
grep usage_usec "/sys/fs/cgroup/system.slice/docker-${container_id}.scope/cpu.stat"
grep VmHWM "/proc/${runner_pid}/status"
```

Диск после прогона:

```bash
du -sb target/anki-e2e/cache target/anki-e2e/build target/anki-e2e/runs
docker image ls --filter label=io.github.anki-deck-toolkit.e2e=1
docker system df -v
```

## Снимки

Блоки ниже генерирует `benchmark-history snapshot` из проверенных результатов
`benchmark-runner` в эталонном окружении; если блоков нет, подтверждённого снимка
ещё не было.

Профиль `offline-warm` сценария `anki-e2e.smoke` в эталонном окружении не
измеряется: `benchmark-runner check` возвращает `unsupported`, потому что
сетевое пространство имён серии не охватывает демона Docker и BuildKit,
работающих в сети хоста, и отсутствие сети у нагрузки не обеспечено. Профиль
`cold` сценария `anki-e2e.full` по каталогу ещё не измерялся.

<!-- benchmark-snapshot: {"scenario":"anki-e2e.smoke","profile":"warm","recorded_at":"2026-10-29T10:36:16Z","environment":{"anki_version":"26.09.3","arch":"x86_64","container_limits":"nano_cpus=4000000000,memory=6442450944,pids_limit=4096","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","docker_version":"29.9.2","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"anki-archive":{"archive_bytes":{"value":191305871.0,"unit":"byte"}}}} -->
### `anki-e2e.smoke` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `anki-e2e` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 2 из 2, прогревов 1.

| Факт окружения | Значение |
|---|---|
| `anki_version` ★ | 26.09.3 |
| `arch` ★ | x86_64 |
| `container_limits` ★ | nano_cpus=4000000000,memory=6442450944,pids_limit=4096 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `docker_version` ★ | 29.9.2 |
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
| `anki-archive` · `archive_bytes` | 182.44 MiB |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 26.148 s ± 54.16 ms | 26.148 s | 26.11 s – 26.187 s | 2 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 1.193 s ± 66.21 ms | 1.193 s | 1.147 s – 1.24 s | 2 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 309.13 MiB ± 370.52 KiB | 309.13 MiB | 308.87 MiB – 309.38 MiB | 2 |

Не измерено: `phase_duration@phase/scenarios` — `unsupported`: модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-e2e.full","profile":"warm","recorded_at":"2026-10-29T10:34:34Z","environment":{"anki_version":"26.09.3","arch":"x86_64","container_limits":"nano_cpus=4000000000,memory=6442450944,pids_limit=4096","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","docker_version":"29.9.2","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"anki-archive":{"archive_bytes":{"value":191305871.0,"unit":"byte"}}}} -->
### `anki-e2e.full` · профиль `warm`

Последнее подтверждённое измерение: 2026-10-29 UTC · `anki-e2e` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · итог `success`: успешных прогонов 2 из 2, прогревов 0.

| Факт окружения | Значение |
|---|---|
| `anki_version` ★ | 26.09.3 |
| `arch` ★ | x86_64 |
| `container_limits` ★ | nano_cpus=4000000000,memory=6442450944,pids_limit=4096 |
| `cpu_model` ★ | AMD Ryzen 7 5800X3D 8-Core Processor |
| `docker_version` ★ | 29.9.2 |
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
| `anki-archive` · `archive_bytes` | 182.44 MiB |

| Метрика | Scope / источник | Среднее ± σ | Медиана | Мин–макс | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 235.474 s ± 18.393 s | 235.474 s | 222.468 s – 248.48 s | 2 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 1.626 s ± 253.12 ms | 1.626 s | 1.447 s – 1.805 s | 2 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 120.58 MiB ± 316.78 KiB | 120.58 MiB | 120.36 MiB – 120.8 MiB | 2 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 503.51 MiB ± 7.86 MiB | 503.51 MiB | 497.95 MiB – 509.07 MiB | 2 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 200.145 s ± 30.88 ms | 200.145 s | 200.124 s – 200.167 s | 2 |
| `cpu_utilization_peak@cgroup` | cgroup / cgroup-v2 | 1.86 ядра ± 0.01 ядра | 1.86 ядра | 1.85 ядра – 1.86 ядра | 2 |
| `cpu_utilization_peak@host` | host / procfs | 2.59 ядра ± 0.41 ядра | 2.59 ядра | 2.3 ядра – 2.88 ядра | 2 |
| `memory_host_peak_increase@host` | host / procfs | 472.87 MiB ± 121.82 MiB | 472.87 MiB | 386.73 MiB – 559 MiB | 2 |
| `module_counter@module-report/commands_started` | module-report / module-report | 103 ± 0 | 103 | 103 – 103 | 2 |
| `disk_usage@path/work-cache` | path / filesystem | 182.44 MiB ± 0 B | 182.44 MiB | 182.44 MiB – 182.44 MiB | 2 |

Не измерено: `phase_duration@phase/release` — `unsupported`: модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста; `phase_duration@phase/anki-image` — `unsupported`: модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста; `phase_duration@phase/run-image` — `unsupported`: модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста; `phase_duration@phase/scenarios` — `unsupported`: модуль не объявляет машинного формата меток фаз в журнале: длительность не выводится из человекочитаемого текста.

★ Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Ручное наблюдение без первичных JSON

Наблюдение ниже записано вручную до появления локальной истории. Первичных
JSON-результатов у него нет: это не подтверждённый снимок, его значения не
сравниваются с результатами `benchmark-history` и не служат базой. Оно сохраняет
контекст до первого подтверждённого снимка тех же условий. Тёплый `full` теперь
описывает снимок выше, включая наблюдения CPU и памяти хоста и размер
`work-cache`. Холодное и инкрементальное состояния и разложение по фазам
подтверждённым снимком пока не покрыты.

### 2026-10-26 · `anki-e2e` 0.1.0 · Anki 26.09.3 · `full`

- **Окружение:** AMD Ryzen 7 5800X3D (8 ядер, 16 потоков), 16 ГБ RAM (15,6 ГБ
  видно в WSL), WSL2 с ядром 6.18 и Arch Linux, Docker Engine 29.9.2 внутри
  WSL (systemd, cgroup v2, overlayfs), `rustc` 1.99.1. Лимиты контейнера по
  умолчанию: `--cpus 4 --memory 6g`. Версия выбрана как последняя стабильная.
  Рабочая копия без изменений; в инкрементальном прогоне добавлен один
  неигнорируемый файл.
- **Состояния:** один холодный, два тёплых и один инкрементальный `full` подряд.
  Все четыре — `PASS`, по 129 проверок.

Уточнение состояния рабочей копии: холодный и оба тёплых прогона выполнены
без изменений. После второго тёплого, до запуска инкрементального, добавлен
один временный неигнорируемый файл. Он присутствовал на протяжении всего
инкрементального замера и вошёл в снимок исходников (293 файла вместо 292);
сразу после завершения прогона его удалили, вернув чистую рабочую копию.

Время по фазам, секунды:

| Фаза | Холодный | Тёплый 1 | Тёплый 2 | Инкрементальный |
|---|---|---|---|---|
| Выбор релиза и архив | 13 (загрузка 191 МБ) | 8 | 9 | 8 |
| Снимок исходников (292–293 файла) | 1 | 0 | 1 | 1 |
| Образ Anki | 16 | 0 | 0 | 0 |
| Образ запуска (`cargo build`) | 155 | 1 | 0 | 30 |
| Сценарии | 222 | 221 | 227 | 220 |
| **Итого** | **406,7** | **230,0** | **236,5** | **259,3** |

Сценарии (диапазон по четырём прогонам):

| Сценарий | Время, с | Запусков `anki-repo` | Проверок | Окон |
|---|---|---|---|---|
| `smoke` | 11,6–12,0 | 6 | 13 | 2 |
| `addons` | 41,4–43,0 | 8 | 15 | 6 |
| `patch` | 50,4–52,6 | 30 | 32 | 0 |
| `crowdanki` | 38,8–40,6 | 28–30 | 37 | 3 |
| `process` | 23,1–24,0 | 15 | 14 | 1 |
| `errors` | 40,0–41,3 | 14 | 18 | 5 |

Ресурсы:

| Метрика | Значение | Источник |
|---|---|---|
| Контейнер: пик памяти | 499–505 МиБ при лимите 6 ГиБ | `memory.peak` |
| Контейнер: CPU за прогон | 203–212 с | `usage_usec` |
| Контейнер: средняя загрузка | 0,92–0,94 ядра при лимите 4 | `usage_usec` / время |
| Контейнер: пик за 1 с | 1,80–1,88 ядра | разность сэмплов |
| Хост во время сценариев | ≈1,1 ядра в среднем, 242–251 ядро·с (≈40 с сверх контейнера: dockerd, `docker exec`, WSL) | `/proc/stat` |
| Холодная сборка образов, хост | 471 ядро·с, в среднем 2,75 ядра, пик 15,5 из 16; память хоста до +2,9 ГиБ | `/proc/stat`, `MemAvailable` |
| Инкрементальная сборка, хост | 40 ядро·с, пик 4,6 ядра; память хоста до +0,9 ГиБ | `/proc/stat`, `MemAvailable` |
| dockerd во время сборки | 0,9–1,0 ГиБ, ≈0,05 ядра (шаги BuildKit сюда не входят) | cgroup `docker.service` |
| Runner: пик RSS | 27–28 МиБ | `VmHWM` |
| Runner: CPU | 5,3–6,7 с своего + 0,7–3,5 с дождавшихся потомков | `/proc/<pid>/stat` |
| Модульные тесты `cargo test -p anki-e2e` | 0,63–0,65 с на 64 теста (уже собранные) | время запуска |

Диск:

| Что | Размер |
|---|---|
| Архив Anki в `cache/` | 191 МБ |
| Снимки исходников в `build/` | 17,7 МБ |
| Свидетельства одного `full` в `runs/` | 2,0–2,1 МБ, 62 PNG |
| Образ запуска | 2,46 ГБ (включает слои образа Anki 1,89 ГБ; не суммировать) |
| Cache mount `anki-e2e-cargo-target` | 3,41 ГБ после холодной сборки, 3,65 ГБ после инкрементальной |
| Cache mount `anki-e2e-cargo-registry` | 319 МБ |

Выводы:

- В тёплом состоянии ≈96 % времени `full` занимают сами сценарии.
  Подготовка runner — меньше 10 с, почти всё — выбор релиза; его разложение на
  запрос к GitHub и повторную сверку SHA-256 не измерялось.
- Разброс тёплых прогонов — около 3 % (230,0 и 236,5 с).
- Лимиты контейнера соблюдаются с большим запасом: пик памяти ≈0,5 ГиБ из 6,
  CPU не выше 1,9 ядра из 4. Это наблюдение одного окружения, а не основание
  менять значения по умолчанию.
- Холодная сборка образа запуска — самая дорогая фаза по CPU и памяти. Лимиты
  `--cpus`/`--memory` на неё не действуют: Cargo занимает все ядра хоста
  (пик 15,5 из 16) и до 2,9 ГиБ памяти хоста. Это согласуется с README стенда.
- Cache mount `anki-e2e-cargo-target` после одной инкрементальной пересборки
  вырос на 0,24 ГБ. Одного наблюдения мало для вывода о неограниченном росте:
  стоит проверить в следующей записи серией из нескольких изменений снимка.
  Уборка — `clean --cache`.
- `full` запускает 101–103 процесса `anki-repo`; ожидания `doctor` и `job`
  делают это не чаще раза в секунду.

Не измерено: полностью холодная система без слоевого кэша и базовых образов;
сетевой трафик, кроме архива Anki; дисковый ввод-вывод; другие окружения
(обычный Linux, Docker Desktop, aarch64); ресурсы шагов BuildKit отдельно от
остальной системы.
