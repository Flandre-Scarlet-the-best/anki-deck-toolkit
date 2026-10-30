# Пошаговый CodeRabbit review workflow

Этот файл владеет механикой CodeRabbit review cycle: разбор текущего checkout,
discovery фактического provider interface, invocation, triage findings,
verification, commit/push, накопительное обновление Gist и сводки PR, rate-limit handling
и отчёт.

Контракт и границы (trigger, `target_iterations`, определение completed
iteration, clean iteration, инварианты) задаёт `SKILL.md`. Здесь — только порядок
действий.

Общую процедуру публикации — staging, commit, push, exact remote verification,
PR create/edit и read-back — владеет repository skill `anki-git-workflow`.
Содержательную структуру сводки PR задаёт его `references/pr-body.md`,
формат полного журнала и механику Gist — `references/pr-gist.md`.
Этот файл не дублирует механику Git/GitHub: он только
определяет CodeRabbit-specific данные, которые каждая completed iteration обязана
опубликовать через этого владельца.

## 1. Установить candidate текущего checkout

Cycle всегда работает с текущим checkout. Перед первой iteration зафиксируй:

```bash
git rev-parse --show-toplevel
git remote -v
git rev-parse --abbrev-ref HEAD
git rev-parse --abbrev-ref --symbolic-full-name '@{u}'
git status --porcelain
git rev-parse HEAD
```

Установи repository identity и убедись, что checkout относится к ожидаемому
repository. Если пользователь передал PR URL или номер, сверь его repository и
head с текущим checkout надёжным доступным способом (например `gh pr view` или
GitHub API/MCP). Несовпадение — precondition problem: не переключай ветки молча.

### Опубликованность candidate

Iteration начинается с committed + pushed HEAD, поэтому одной чистоты worktree
недостаточно: нужно убедиться, что текущий HEAD уже опубликован. Устаревший
remote-tracking ref для этого не годится — опубликованность подтверждается тем
точным сравнением удалённой ссылки, которым владеет `anki-git-workflow`
(`references/publication.md`, «Remote postcondition»); не заводи здесь второй
копии этой процедуры.
Если HEAD и upstream расходятся — есть неопубликованные коммиты либо upstream
ушёл вперёд, — это precondition problem, а не материал для review. Не запускай
review неопубликованного candidate и не публикуй неизвестные локальные коммиты
молча: либо явно опубликуй candidate в рамках задачи, либо остановись и сообщи
состояние.

### Base для review

Base нужен как точка сравнения текущего committed diff.

1. Если для текущей branch существует PR и это удаётся установить надёжно —
   используй его фактический base.
2. Если PR нет — используй канонический base repository, однозначно следующий из
   Git state: remote default branch. Как именно он разрешается, владеет
   `anki-git-workflow` (`references/publication.md`, «Ветка»); не заводи здесь
   второй копии этой процедуры.

Официальная документация CodeRabbit CLI **не описывает** автоматическое
определение base открытого PR для локального review: `--base` остаётся
документированным способом задать сравнение, поэтому base резолвит сам workflow,
а не CLI.

Никогда не хардкодь номер PR, имя ветки, SHA, текущую feature или локальные пути.

## 2. Dirty worktree policy

Cycle по умолчанию начинается с committed candidate: `git status --porcelain`
должен быть пустым.

Если до первой iteration есть неожиданные staged/unstaged/untracked изменения:

- не подмешивай их в review commit автоматически;
- не удаляй и не прячь их без явной причины;
- сначала установи, относятся ли они к текущей задаче и можно ли безопасно
  продолжить;
- если это установить нельзя — это operational blocker, а не повод угадывать.

Между iterations ожидается clean worktree после commit + push.

## 3. Discovery фактического CodeRabbit interface

Не полагайся на память о flags и на версию, закреплённую в этом файле.
В начале requested cycle установи фактический interface текущей среды:

```bash
coderabbit --version
coderabbit --help
coderabbit doctor
coderabbit review --help
coderabbit review findings --help
coderabbit auth status
coderabbit usage
coderabbit config validate
```

Установи фактом минимум следующее:

- какая версия установлена и является ли она latest;
- какие review-flags реально доступны, включая наличие и форму deep;
- какова доступность включённых review прямо сейчас (`usage`);
- валиден ли repository config (`config validate`).

Latest-версию не угадывай по внешним источникам и не считай закреплённой:
официальный способ проверки — сам CLI.

```bash
coderabbit update
```

Если он сообщает, что установлена уже последняя версия, граница latest
подтверждена. Если он готов поставить новую версию, это меняет provider
interface: не обновляй CLI молча посреди cycle. Сообщи пользователю и либо
обнови до начала cycle, либо продолжай на текущей версии, явно зафиксировав её
в отчёте.

Проверить, не появилась ли новая версия, **не устанавливая** её, можно по тому
же указателю, из которого читает сам updater:

```bash
curl -s https://cli.coderabbit.ai/releases/latest/VERSION
```

Так безопаснее: при наличии новой версии `coderabbit update` не ограничивается
проверкой, а сразу её ставит.

Repository `.coderabbit.yaml` проверяй штатным для текущей версии способом,
например:

```bash
coderabbit config validate
```

`coderabbit config validate` возвращает `0`, если файл валиден против актуальной
официальной schema, и `1`, если файл отсутствует, нечитаем, невалиден, либо
schema не удалось загрузить. Если repository config невалиден — это отдельная
задача про конфигурацию, а не побочный эффект review: не правь `.coderabbit.yaml`
ради того, чтобы cycle поехал.

`coderabbit config` **не является** read-only командой начиная с 0.8.x: вызов
без подкоманды запускает guided setup и может записать `.coderabbit.yaml`, а
`coderabbit config apply` применяет готовое YAML-предложение (с `--dry-run`
для preview и `--yes` для применения без подтверждения). В рамках review cycle
допустим только `config validate`; генерация и применение конфигурации
остаются отдельной задачей про repository configuration и не входят в этот
workflow.

`coderabbit doctor` возвращает `1`, если хотя бы одна проверка провалена;
warnings не дают ненулевой код возврата. Неспособность CLI работать не из-за
обычного rate limit — operational blocker.

### Что считается фактическим interface

Источник истины для доступных flags — фактический `review --help` установленной
версии. `finding triage` ниже описывает контракт agent-режима и может отличаться
в деталях от конкретной установленной версии: сверяйся с фактическим выводом.

Наблюдения для CLI `0.8.1` в этой среде, снятые в начале cycle (не вечный
контракт). `0.8.1` подтверждён как latest официальным `coderabbit update`
(«Already on the latest version»), а не по внешнему источнику; перед следующим
cycle перепроверь эту границу заново:

- `coderabbit review --agent` — структурированный вывод для agent workflows;
- `--committed` / `--uncommitted` / `--include-untracked` — выбор scope; явно
  взаимоисключающие комбинации отвергаются до старта review;
- `--base <branch>` / `--base-commit <commit>` — точка сравнения;
- `--deep [focus]` — full pull request review policy; focus-текст требует early
  access (см. §4.1а);
- `-c, --config <files...>` — **дополнительные инструкции** для CodeRabbit AI
  (например `CLAUDE.md`), а не способ загрузить repository config;
- `coderabbit review findings [--clear]` — повторное чтение либо dismiss
  сохранённых findings в текущем review scope (см. §4.2);
- `--show-prompts`, `--dir`, `--api-key`, `--region`, `--usage`,
  `--use-credits`;
- `--remote <owner/repo>` вместе с обязательными `--base` и `--source-branch` —
  server-side review без локального checkout; для этого skill out of scope
  (см. «Границы этого skill»).

`--light` в `0.8.1` **отсутствует** в `review --help` и является скрытым legacy
алиасом: в релизной сборке он зарегистрирован с описанием «Alias for normal
review» и скрыт из help. Это не отдельный review-режим, поэтому не описывай
`--light` как поддерживаемую возможность и не строй на нём поведение cycle.

Различай уровень доказательства явно. `review --help` установленной версии —
наблюдаемый interface, и он источник истины для flags. Детали, которых в help
нет (скрытые options, внутренние поля запроса, точные тексты отказов),
подтверждаются только релизной сборкой этой же версии; «флаг принимается
парсером» само по себе не доказывает его смысл. Не поднимай деталь до
«поддерживаемой возможности», если она подтверждена лишь косвенно.

Правила использования:

- Не придумывай flags, которых нет в установленной версии.
- Не передавай `--config .coderabbit.yaml` только ради загрузки repository
  config: auto-discovery `.coderabbit.yaml` в repository root документирован и
  работает без этого flag.
- Не мутируй `.coderabbit.yaml` как побочный эффект review.
- Не запускай `coderabbit skills`: это user-level skills самого CodeRabbit для
  поддерживаемых агентов, а не механизм установки repository skill этого
  репозитория.

## 4. Iteration loop

Для `i` от 1 до `target_iterations` (см. `SKILL.md`):

### 4.1 Invocation

Запускай agent-oriented review **текущего committed branch diff** относительно
установленного base:

```bash
coderabbit review --agent --committed --base <base>
```

Если предпочтительнее явный commit вместо branch name, используй
`--base-commit "$(git merge-base <base> HEAD)"`.

Не комбинируй взаимоисключающие scope flags (`--committed` вместе с
`--uncommitted` или `--include-untracked`): такие комбинации отвергаются до
старта review. Не добавляй `--include-untracked`/`--uncommitted`, если работаешь по
committed candidate.

Review — длительная операция: официальная документация оценивает её в единицы и
десятки минут в зависимости от объёма изменений. Запускай её как длительную
операцию (background job либо большой timeout) и не перезапускай параллельно.

Обычный cycle запускает **обычный** review. Не добавляй `--deep` по своей
инициативе: переход на full pull request review policy делается только по
явному запросу пользователя (см. §4.1а).

### 4.1а Deep review

`coderabbit review --deep [focus]` включает full pull request review policy;
focus-текст, по описанию самого CLI, требует early access. Это единственная
review-возможность, которая меняет характер review, поэтому правила такие:

- обычный CodeRabbit cycle **не** становится deep по умолчанию; `--deep`
  добавляется только тогда, когда пользователь явно запросил deep review — как
  без focus, так и с ним (то есть явно запросил deep или deep с focus);
  одиночный запрос тематического focus обычного review не разрешает `--deep`;
- focus передаётся **дословно**, без переформулирования, сокращения и потери
  значения: если пользователь задал смысл, его нельзя заменить своим;
- нельзя молча отбросить `--deep` и прогнать обычный review вместо запрошенного;
- нельзя молча откатиться на обычный review, если запрошенный deep недоступен.

Форма вызова: значение позиционное и необязательное, многословный текст допустим
и по словам не режется; значение trim'ится, ограничено 2000 символами, NUL
запрещён, а при нарушении CLI отказывает текстом «Review focus must be non-empty
text of at most 2000 characters». Голый `--deep` без значения допустим.

`--deep` не переключает `reviews.profile` на стороне CLI: сборка лишь помечает
запрос как deep (`cliReviewDeep`) и передаёт focus (`cliReviewFocus`). Связь
между `--deep` и `reviews.profile` из repo config остаётся серверной и
недокументированной, поэтому **не утверждай**, что `--deep` равен профилю или
переопределяет его.

Доступность focus-части определяется **не** repo config: gate — это удалённый
feature flag (`cli_review_focus`), а не `early_access` в `.coderabbit.yaml` (эта
строка в CLI не встречается вовсе). Когда gate закрыт, CLI вообще не запускает
review и отказывает сообщением «Focused reviews are not enabled yet. Use --deep
without focus text.». Поэтому:

- не связывай доступность focus с `.coderabbit.yaml` и не включай `early_access`
  ради того, чтобы запрос прошёл: repo config не мутируется как побочный эффект
  review;
- не выводи доступность deep из того, что флаг разобрался: синтаксис разбора и
  entitlement — разные вещи;
- сверяй фактическое поведение установленной версии, а не текст этого файла;
- отказ по gate не даёт completed iteration и не позволяет выдать обычный review
  за запрошенный deep. Если пользователь хочет deep без focus, это его отдельное
  решение, а не твой автоматический fallback.

Несовместимость CLI и сервера, отказ по focus gate, недоступность deep-политики
и любая ошибка самого deep-прогона — это **состояние, требующее решения**, а не
completed iteration и не clean review:

```text
completed_iterations не меняется
clean pass не выдаётся
marker commit не создаётся
```

Сообщи, что именно запрошено, что ответил провайдер и какое решение нужно от
пользователя — например выяснить, доступен ли focus в этом окружении, или
согласиться на deep без focus. Не подменяй запрошенный deep обычным review и не
выдавай обычный результат за deep.

Не выдумывай отдельные quota/rate-limit правила для deep: если провайдер не
подтверждает отдельную квоту, действуют обычные правила §5.

### 4.2 Чтение результата

Agent-режим отдаёт поток событий (в документации — NDJSON, один JSON object на
строку). Разбирай поток построчно и обрабатывай события по их типу:

- `review_context` — что именно сравнивалось (ветки/base/директория);
- `status` — промежуточные статусы, включая пропуск review без изменений;
- `heartbeat` — keep-alive: не finding, не прогресс для пользователя, не повод
  писать heartbeat-сообщения;
- `finding` — findings;
- `complete` — terminal завершение;
- `error` — ошибка провайдера.

Review считается **authoritative завершённым** только если выполнены все условия
сразу:

- поток разобран до конца, а не остановлен на первой похожей на terminal строке;
- terminal событие присутствует, и его completion status — то значение, которое
  считается «завершено» в установленной версии;
- в терминальном контракте нет признаков неполного/проваленного review: при
  наличии `outcome` он означает завершение, а `message` не сообщает о
  незавершённости;
- не было `error`-событий;
- process exit code не указывает на failure.

`type=complete` вместе с `status=review_completed` **само по себе
недостаточно**: это только одна из проверок. Не доказывает завершение и то,
что findings пришли — набор findings не говорит о полноте review. Не
доказывает завершение и exit code `0` сам по себе.

Про exit code: официальная документация связывает код `1` с failed или
incomplete review **начиная с CLI 0.7.7**. Проверяй фактическую установленную
версию: `0.8.1` уже подпадает под это правило, поэтому в ней ненулевой код —
признак failed/incomplete review, а не безобидная деталь. На более старой
версии код `1` может не нести этой семантики, и тогда опирайся на остальные
признаки.

Если поле completion-контракта, которое установленная версия **фактически
отдаёт**, отсутствует, не превращай отсутствие данных в доказательство успеха:
это не «всё хорошо», а неизвестное/неподтверждённое состояние. Например, когда
версия сообщает `unreviewedFileCount` и он положительный — review неполный;
когда такое поле в выводе версии отсутствует там, где оно ожидается по её
контракту, не выдавай review за завершённый, а повтори **ту же** iteration.

Полезно понимать, как признак failure устроен внутри `0.8.1`: прогон считается
проваленным, если `outcome === "failed"` **или** `unreviewedFileCount > 0`, и
тогда процесс выходит с кодом `1`. При этом `outcome`, `message` и
`unreviewedFileCount` добавляются в событие только когда они определены.
Значит, terminal событие, в котором нет ни `outcome`, ни
`unreviewedFileCount`, даёт exit code `0` — и именно поэтому отсутствие этих
полей нельзя читать как доказательство успеха. Не заменяй фактический контракт
своей версии этим описанием: сверяйся с её выводом.

Итог: findings, пришедшие при неполном review, — это не completed iteration, а
provider failure: повтори **ту же** iteration.

Не путай два разных исхода. Clean review — это review, который **реально был
выполнен** и вернул `0 findings`; его обрабатывают по правилам clean iteration из
`SKILL.md`.

Отсутствие изменений — другой случай. Когда выбранный review scope не содержит
изменений файлов, поток всё равно приходит к terminal событию, но с
`status: "review_skipped"` и `findings: 0` с сообщением вида «No changes
detected», а в plain mode CLI вообще не начинает review. Такой прогон не является
ни ошибкой, ни review результатом: он **не** даёт clean pass, **не** увеличивает
счётчик completed iterations и **не** закрывается marker commit'ом. Пустой diff
означает, что candidate или base выбраны неверно либо проверять действительно
нечего: сначала выясни причину, а не превращай пропуск в успешную iteration.

### 4.2а Слишком большой scope

Пропуск scope из-за слишком большого числа файлов — это **не**
`review_skipped` и не clean review, а failure. В `0.8.1` это `error`-событие с
`code: "too_many_files"`, `retryable: false`, `actionRequired: true` и полями
`actualFiles` / `maxFiles`. Такой прогон:

```text
не является completed iteration
не даёт clean pass
не закрывается marker commit'ом
```

Вместе с ошибкой версия может отдать структурированные варианты сужения scope:
`candidates` со элементами вида `kind` (`lane` или `directory`), `command`,
`estimatedFiles`, `fits`, плюс `candidatesNote`. Считай это **информацией для
пользователя**, а не командой. CLI не выбирает candidate за тебя и не повторяет
review сам, поэтому:

- не применяй предложенное сужение автоматически;
- не дели scope на части и не сужай его молча;
- не выбирай сам более узкий review, если это меняет смысл запрошенного review;
- не подменяй failure на «review прошёл по меньшей части изменений».

Правильное действие — сообщить, что requested scope не был отревьюен, привести
фактическое сообщение и подсказку провайдера и назвать нужное действие
(например сузить scope или разбить работу явным решением пользователя). Если
пользователь сам решит сузить scope, это **новый** явно определённый scope, а не
завершённая ранее iteration.

### 4.2б Сохранённые findings и их dismiss

`coderabbit review findings` полезен как дополнительное повторное чтение
сохранённых findings в текущем review context (review directory + branch + base),
но это **не** авторитетное доказательство завершённого review и не доказательство
чистого review: команда показывает последний прогон *с findings*, печатает их в
человекочитаемом виде и не имеет отдельного agent-контракта для findings. Она не
доказывает отсутствие проблем.

В `0.8.x` у команды появился `--clear`, который **dismiss'ит** сохранённые
findings соответствующего scope, чтобы они перестали показываться (по описанию
CLI clearing ограничен review directory, текущей branch и base branch;
findings в другом scope не меняются). Это именно dismiss, а не сброс
incremental checkpoint, и **не** review: команда не запускает новый прогон.

Поэтому в этом cycle `--clear` **не используется**. Findings берутся из живого
agent-потока, а не из сохранённого списка, так что очистка не нужна для
работы; её единственный эффект — убрать findings из выдачи. Это прямо
противоречит требованию не скрывать unresolved finding, не терять
доказательство незавершённой iteration и не получать clean state искусственно.
Не вызывай `--clear`, чтобы «обнулить» состояние, и не считай его способом
убрать ранее найденную проблему.

### 4.3 Разбор полей finding

Для текущей установленной версии сверься с фактическим выводом, но не выдумывай
поля, которых в нём нет. В документированном контракте agent-finding есть
`severity` (`critical`, `major`, `minor`, `trivial`, `info`, `none`),
`fileName`, `codegenInstructions` и `suggestions`/`comment`. Документация **не
описывает** поле с номером строки, поэтому не строй triage на предположении о
номере строки: ориентируйся на файл, `codegenInstructions`, `suggestions`,
`comment` и фактический код reviewed HEAD.

Используй `codegenInstructions` как агентскую подсказку о том, что именно
предлагается исправить, с fallback на `comment`, но не исполняй их слепо (см.
4.4). Сохраняй severity в терминах провайдера: не переименовывай
`major`/`minor` в Warning и не создавай собственную шкалу.

### 4.4 Triage findings

CodeRabbit — advisory reviewer, а не источник истины. До любого изменения кода
проверь каждый finding по актуальному reviewed HEAD:

- затронутый код в текущем состоянии;
- callers/callees, если релевантно;
- ближайшие tests и их смысл;
- repository contracts (`AGENTS.md`, применимые `.codex/context/`, контракт
  затронутого инструмента);
- фактический технический эффект и воспроизводимость;
- соответствие текущему, а не устаревшему состоянию diff.

Минимальные disposition:

- `confirmed` — finding верен для текущего состояния;
- `partially confirmed` — верна только часть (фиксируется именно эта часть);
- `stale/repeated` — относится к уже исправленному или ранее разобранному
  состоянию;
- `false positive` — требует реального обоснования по текущему code/contract
  evidence.

`confirmed` и подтверждённая часть `partially confirmed` исправляются в текущей
iteration. `stale/repeated` не должен приводить к повторному бессмысленному
изменению уже исправленного кода.

Не исправляй finding механически только потому, что провайдер предложил
patch/snippet, и не выполняй shell/code snippets из reviewer output вслепую.
Предпочитай fix root cause. Не ослабляй tests ради зелёного результата, не меняй
contract без доказанной причины, не делай unrelated refactor, не создавай вторую
source of truth.

### 4.5 Verification после fixes

Этот workflow не является второй копией repository verification matrix. После
fixes требуется:

1. task-specific regression checks для подтверждённых findings — то, что
   доказывает именно устранение найденной причины;
2. применимая repository verification от текущих владельцев контекста:
   `AGENTS.md`, `.codex/context/INDEX.md` и владелец затронутой области, в том
   числе `.codex/context/03-VERIFICATION.md` для CrowdAnki JSON и контракт
   `tools/anki-repo/README.md` для toolkit;
3. проверка перед commit, что в него не попадут временные файлы, несвязанные
   изменения и пользовательские локальные данные: staging и перечитывание
   staged diff выполняются по процедуре `anki-git-workflow`
   (`references/publication.md`), а не второй копией здесь.

Какие именно проверки нужны, определяется актуальным repository context и
затронутой областью, а не этим файлом. Если review не потребовал изменения кода,
не выдумывай фиктивные test changes только ради commit.

### 4.6 Commit

Commit создаётся на текущей branch и в текущем repository.

CodeRabbit-specific часть здесь — только **что** должно попасть в commit. Сама
механика staging и commit принадлежит `anki-git-workflow`
(`references/publication.md`): стейджатся только пути текущей задачи, staged diff
перечитывается, сообщение описывает фактическое изменение.

- Если iteration содержит реальные fixes — commit содержит их и имеет смысловое
  сообщение по фактической root cause; marker commit не создаётся.
- Если iteration clean (0 findings, все findings stale/repeated/false positive,
  actionable fixes не потребовались) — iteration всё равно закрывается отдельным
  marker commit:

  ```bash
  git commit --allow-empty -m "<сообщение о clean CodeRabbit pass>"
  ```

Сообщение пиши по-русски, в стиле репозитория, ясно показывая clean CodeRabbit
pass. Не превращай marker commit в маскировку blocker'а.

Пустой marker commit — осознанная часть именно этого контракта: он маркирует
реально выполненный clean review. Это тот случай, когда пустой commit допустим;
на обычные задачи вне CodeRabbit cycle он не распространяется.

### 4.7 Публикация и подтверждение HEAD

Публикация выполняется по процедуре `anki-git-workflow`
(`references/publication.md`): явный remote и refspec, push текущей branch,
корректный upstream при первой публикации и exact сравнение удалённого SHA с
ожидаемым локальным HEAD.

CodeRabbit-specific требование к результату: к началу следующей iteration
опубликованный HEAD должен совпадать с локальным, а worktree — остаться clean.
Только после этого начинается следующая iteration, и только она проверяет новый
опубликованный HEAD. Exit code push сам по себе доказательством не является.

Разрешение на commit и push является частью уже явно запрошенного cycle и
ограничено текущим cycle и текущей branch. Оно **не** разрешает merge,
force-push, rebase опубликованной истории без отдельной причины, создание другой
branch, переключение на другой PR, изменение base PR, закрытие PR и публикацию
несвязанных локальных изменений.


### 4.8 Публикация журнала Gist и сводки PR после iteration

После подтверждённого push и exact remote verification **до перехода к
следующей iteration** обнови **полный** журнал CodeRabbit в Gist текущего work
item, затем короткую сводку со ссылкой в body текущего PR.

Порядок:

1. Установи единственный PR текущего repository/base/head. Если PR не создан,
   доведи обычный lifecycle через `anki-git-workflow`, не создавая дубликат.
2. Восстанови **полный** актуальный body PR из remote по
   `anki-git-workflow/references/pr-lifecycle.md`. При наличии старой
   таблицы CodeRabbit сохрани все её строки. Установи имеющийся Gist по
   фактической ссылке (если есть), считай опубликованные файлы и согласуй
   расхождения с локальным ignored-source.
3. Сформируй/обнови в **том же** Gist `coderabbit-review.md` по
   `anki-git-workflow/references/pr-gist.md`. После каждой итерации добавь
   записи для **всех** provider findings: severity, путь, эффект, проверенное
   disposition, реальное исправление/отказ с commit. Для `0 findings`
   запиши одну `clean`-строку с marker commit. Отклонённые и повторные
   findings не превращай в `clean`.
4. При первом переходе с legacy-таблицы PR перенеси **все** прежние записи
   в Gist до сокращения PR. Не создавай отдельный Gist на каждый цикл:
   существующий отчёт дополнить файлом журнала, а новый Gist создавать лишь
   при отсутствии прежнего.
5. Опубликуй Gist **до** изменения PR. Прочитай удалённый файл целиком,
   сравни с локальным source и убедись, что старые строки сохранены, новые
   появились ровно один раз. При неясном исходе сетевого запроса сначала
   установи фактическое удалённое состояние, а не повторяй создание/edit.
6. В `## CodeRabbit review и disposition` PR оставь по
   `anki-git-workflow/references/pr-body.md` только `Прогресс` (итерации,
   reviewed HEAD и новый HEAD), `Итог` (подтверждённые/отклонённые/clean и
   нерешённые риски), `Существенное` (главные исправления/проблемы) и
   `Полный журнал` с **проверенным** URL. Не копируй туда всю таблицу.
7. Проверь локальный body через `check_pr_body.py`, опубликуй его через
   `anki-git-workflow`, затем перечитай **полный** опубликованный PR и Gist.
   Проверь base/head, Draft/Ready, факт сохранения всех данных и совпадение
   обоих документов с локальными источниками.

Только после успешного read-back **двух** публикаций iteration может
увеличить `completed_iterations` и открыть следующую. Если Gist создать
или обновить нельзя, оставь подготовленные файлы локально, сообщи blocker:
ссылка-заглушка, потерянные findings и фиктивное завершение запрещены.

Обновление body/Gist не превращает fix commit текущей iteration в
CodeRabbit-reviewed: его сможет проверить только следующая iteration.

## 5. Rate limit handling

Rate limit CodeRabbit — ограничение provider/environment, а не defect
repository. Он не означает failed code review, clean code review, blocker
реализации или завершённую iteration.

При rate limit до authoritative завершения текущего review:

```text
completed_iterations не меняется
current_iteration остаётся той же
```

Порядок действий:

1. Убедись, что это именно rate limit, а не auth failure, network/provider
   failure или malformed result.
2. Если провайдер сообщает надёжный retry-after/reset interval или timestamp —
   предпочитай его.
3. При возможности дождись одним реальным процессом ожидания, например
   `sleep <duration>`. Во время ожидания не пиши пользователю heartbeat-сообщения,
   не генерируй «ещё жду», не делай polling каждую минуту и не расходуй tokens на
   пустое ожидание. Sleep не является iteration.
4. После ожидания повтори **ту же** незавершённую iteration.

Что именно сообщает провайдер, зависит от версии, и это менялось. В `0.8.1`
команда `coderabbit usage` (и `coderabbit usage --agent`, alias
`coderabbit review --usage`) описана самим CLI как «included-review availability
and billing-period usage» и фактически отдаёт доступность включённых review:
`remaining`, `limit`, `rollingWindowMs`, `nextAvailableInMs`, `fullCapacityInMs`
для rolling-окна, плюс `resetsAt` для billing period. То есть провайдер
**сообщает** время до следующего допустимого review — используй его (пункт 2
выше), а не выдуманную оценку.

Эти команды read-only: они не запускают review, не расходуют iteration и не
являются доказательством завершённого review. Поэтому:

- можешь однократно проверить доступность перед invocation, чтобы не запускать
  заранее обречённый прогон, но не превращай это в polling;
- предпочитай сообщённые провайдером значения своей оценке; `nextAvailableInMs`
  — время до следующего review, `fullCapacityInMs` — до полного восстановления
  окна;
- не зашивай фиксированное число минут как вечный контракт: `rollingWindowMs` и
  `limit` принадлежат выводу конкретной версии и аккаунта;
- форма сообщения самого rate-limit-отказа, его exit code и HTTP status в
  официальной документации по-прежнему не описаны — не угадывай их;
- при каждом новом rate limit заново смотри фактический вывод установленной
  версии, а не полагайся на этот текст.

Не включай `--use-credits` самостоятельно: это переход на usage-based billed
review, который требует явного согласия пользователя. В agent/headless режиме
провайдер сам возвращает структурированный `action_required`-результат с запросом
подтверждения (`status: "awaiting_confirmation"`) — такой результат является
паузой на согласие, а не завершённым review, не rate limit и не поводом платить
по умолчанию.

Если execution environment физически не позволяет дождаться rate-limit reset
(например процесс или сессия должна завершиться), это не превращается в
repository blocker. Заверши с явным environment-limited состоянием и сообщи
минимум:

- requested iterations;
- completed iterations;
- next unfinished iteration;
- что причина — provider rate limit / environment limitation;
- последний фактически reviewed и pushed HEAD.

Не называй незавершённую rate-limited iteration успешной.

## 6. Operational blockers

Настоящие blockers cycle:

- невозможно однозначно установить repository/branch/base;
- auth CodeRabbit отсутствует и не может быть восстановлен;
- CLI/provider неисправен не из-за обычного rate limit;
- verification после fix остаётся красной;
- commit не создаётся;
- push отвергнут;
- remote divergence требует опасного history rewrite;
- неожиданные пользовательские изменения не позволяют безопасно продолжить.

При blocker не маскируй состояние marker commit'ом: остановись и явно назови
состояние, причину и последний подтверждённый reviewed/pushed HEAD. Repository
blocker и environment-limited завершение по rate limit — разные состояния, и
смешивать их нельзя.

## 7. Отчёт после cycle

После выполнения requested iterations дай компактный итог:

- repository / current branch / base;
- requested iterations;
- completed iterations;
- по каждой iteration: reviewed HEAD, количество provider findings,
  disposition summary, что реально исправлено, commit SHA, push result;
- итоговые результаты verification;
- финальный local/remote HEAD;
- остались ли подтверждённые проблемы;
- были ли rate-limit waits или environment limitations.

Отчёт — не transcript всех команд. При environment-limited или blocker-завершении
отчёт заменяет кладку итога, но обязан содержать перечисленные выше состояния.

## 8. Границы этого skill

CLI умеет заметно больше, чем этот cycle, и расширять scope skill только
потому, что новая версия получила новую команду, не нужно. Осознанно вне
назначения этого skill остаются:

- **`coderabbit review --remote <owner/repo> --source-branch <ref> --base <branch>`** —
  серверный review без локального checkout. Не подменяй им локальный цикл.
- **`coderabbit pullrequest <number-or-url>`** — чтение существующего вывода по PR,
  не новый локальный review и не доказательство его завершённости.
- **Cloud Coding Agent** (`coderabbit code handoff`, `coderabbit code skills import`)
  — облачные задачи и управление их навыками.
- **`coderabbit skills`** — user-level skills CodeRabbit, не skills репозитория.
- **`coderabbit config` / `config apply`** — отдельная задача настройки;
  в этом цикле допустим только `coderabbit config validate`.

Если для одной из этих возможностей появится настоящий пользовательский запрос,
это отдельная задача и, возможно, отдельный skill, а не тихое расширение этого.
