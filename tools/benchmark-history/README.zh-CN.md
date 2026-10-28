# 本地基准历史：`benchmark-history`

[Русский](README.md) · [English](README.en.md) · **简体中文**
<!-- i18n-source-git-blob: 7f51d4f3f6b7f31c70ee545381e2f53f08521f53 -->

`benchmark-history` 为每个用户保存其自己的已验证基准结果历史，在其中查找比较基线，
对比“之前”与“之后”，并把结果生成为 PR 描述中的 `## Производительность`（性能）章节以及
[`docs/performance/`](../../docs/performance/README.zh-CN.md) 页面的快照。它不做任何测量：
系列由 [`benchmark-runner`](../benchmark-runner/README.zh-CN.md) 执行，结果模型、校验和
可比性规则属于 [`benchmark-contract`](../benchmark-contract/README.zh-CN.md)。这里没有第二种
结果格式，也没有第二个测量引擎。

它完全在本地工作：不联网、不需要 GitHub 令牌、没有服务器也没有共享数据库。一个克隆的
历史不会与他人的同步，任何测量都不会被自动发布：PR 章节通过常规的
[`anki-git-workflow`](../../.agents/skills/anki-git-workflow/SKILL.md) 流程粘贴到 PR 描述中，
文档快照只在显式 `--apply` 时写入。

## 历史存放位置

| 来源 | 路径 |
|---|---|
| `--store DIR` | `DIR` |
| `BENCHMARK_HISTORY_DIR` | 变量的值 |
| `XDG_DATA_HOME` | `$XDG_DATA_HOME/anki-deck-toolkit/benchmark-history` |
| 否则 | `~/.local/share/anki-deck-toolkit/benchmark-history` |

路径必须是绝对路径且不含 `..` 组件；按 XDG 规范，相对的 `XDG_DATA_HOME` 无效并被跳过。位于 Git 工作副本
内部的存储会在创建任何文件之前被拒绝：本地结果不应出现在 `git status` 和提交中。检查的是路径本身——
解析符号链接后最近的已存在祖先，以及其上任何含有 `.git` 的目录（克隆、worktree、子模块）——因此拒绝
与当前目录、场景目录以及路径来自 `--store`、`BENCHMARK_HISTORY_DIR` 还是 XDG 无关。同一规则也适用于
`backup` 的目录。不需要 WSL2、Windows 路径或 systemd。

```text
<存储>/
├── store.json          # 格式与归档布局版本
├── results/<ab>/<id>.json  # 原始结果 —— 事实来源
├── index.sqlite        # SQLite 索引 —— 归档的可重建投影
├── lock                # 操作的排他锁
├── corrupt/            # restore 时被替换的损坏副本
└── tmp/                # 原子写入的临时文件
```

## 归档与索引

- **接收。** 只有当 `benchmark_contract::result::check` 连同 `benchmarks/` 目录一起通过时，
  文档才会被接收（场景的历史修订允许，但给出警告；未知场景不允许）。模式版本、运行、指标
  可用性、provenance、输入、配置和环境按原样保存。
- **记录标识**是 JSON 规范形式（键排序、无空白）的 SHA-256。再次接收同一结果，即使缩进、
  空白或键顺序不同，也不会产生重复。数字不做规范化：`1` 与 `1.0` 是不同的规范形式和不同的 ID；
  规范化会改变已接收记录的 ID。
- **不可变性。** 文件按接收时的字节原样保存。名称通过指向已完整写入并刷新到磁盘的临时文件的
  硬链接获得：写入是原子的，已有副本永不被覆盖。重新导入时损坏的副本不会被替换 —— 这是一个
  指向 `verify` 的错误。
- **系列结果。** `partial`、`failure`、`unsupported` 也会被接收：历史不隐藏失败的运行，但
  这些结果不会成为基线。
- **SQLite 索引**派生自归档。先写归档副本，再写索引行；两者之间的中断会留下没有行的记录，
  下一条命令会为其建立索引。没有归档副本的行会被删除并给出提示。损坏的索引被另存为
  `index.sqlite.corrupt`，并从归档重建。过时的索引模式（`PRAGMA user_version`）通过从归档
  重建来迁移；比所支持更新的模式会被拒绝，之后由 `reindex` 重建索引。
- **投影完整性。** `quick_check` 和模式编号并不能证明投影完好，因此每次打开时都会将文件的模式与预期
  模式比对（缺少或多出表——重新创建模式并从归档填充），并将每条记录的行与其校验和
  `projection_sha256` 比对：校验和不一致的记录会从索引中删除，并从其归档副本重建。该检查只需遍历一次
  索引表，不读取 JSON。`verify` 将索引与归档 JSON 的投影完整比对；不一致由 `reindex` 修复。归档本身
  从不被改写。

## 命令

```bash
cargo run -p benchmark-history -- status
cargo run -p benchmark-history -- import target/benchmark-runner/runs
cargo run -p benchmark-history -- list --scenario anki-repo.validate --profile warm --since 2026-10-01
cargo run -p benchmark-history -- show 1a2b3c4d
cargo run -p benchmark-history -- baseline 1a2b3c4d
cargo run -p benchmark-history -- compare 5e6f7a8b 1a2b3c4d
cargo run -p benchmark-history -- report 1a2b3c4d
cargo run -p benchmark-history -- snapshot 1a2b3c4d --page docs/performance/anki-repo.md
cargo run -p benchmark-history -- verify
cargo run -p benchmark-history -- backup /path/to/backup
cargo run -p benchmark-history -- restore /path/to/backup
```

| 命令 | 作用 |
|---|---|
| `status` | 存储路径、路径来源和记录数；不创建存储 |
| `import <路径>...` | 接收结果文件；目录递归遍历，只取看起来像结果的文档，执行器的 `*.rejected.json` 以及遍历中指向目录的符号链接被跳过 |
| `list` | 从新到旧列出记录，过滤器：`--scenario`、`--module`、`--revision`、`--profile`、`--outcome`、`--since`、`--until`、`--module-version`、`--commit`、`--input`（输入标识前缀）、`--env 名称=值`、`--limit` |
| `show <ID>` | 记录摘要；`--raw` 输出归档副本的原始 JSON |
| `baseline <ID>` | 每个基线候选及其适合或不适合的原因，以及选中的基线；`--commit` 缩小选择范围 |
| `compare <基线> <结果>` | `benchmark-contract` 的结论、之前/之后、差值以及相对离散度的评估 |
| `report <ID>...` | PR 的 Markdown `## Производительность` 章节；`--baseline ID`（每个场景 × 配置一对一个）、`--first` 或 `--commit` |
| `snapshot <ID> --page P` | 为 `docs/performance/` 页面生成三种语言的快照；没有 `--apply` 时只显示 |
| `verify` | 按标识和契约检查每个归档副本，并将索引与归档投影比对；不修改归档，也不创建存储（存储不存在时返回代码 3）；打开时索引同步在比对前修复的内容会被报告，并返回代码 1 |
| `reindex` | 从归档重建索引 |
| `backup <目录>` | 将归档复制到新目录或空目录，以便迁移到另一台 Linux 机器 |
| `restore <目录>` | 按 `backup.json` 清单接收备份中的记录：每条记录的名称必须等于其内容的 SHA-256 |

ID 可以完整给出，也可以给出不少于六个字符的前缀。全局参数：`--store`、`--catalog`
（默认是当前目录最近父目录中的 `benchmarks/`）、`--json` —— 在 stdout 输出稳定的 JSON。

迁移：在一台机器上 `backup`，用任意方式（`tar`、`rsync`）传输目录，在另一台上 `restore`。
恢复时不需要目录：名称与内容一致证明记录自接收以来未被修改。`backup` 最后以原子方式写入
`backup.json`——格式、归档布局版本和完整的记录清单；没有它的目录（包括中断的复制）不是备份，单独的
`result.json` 由 `import` 接收。`restore` 将副本与清单比对：文件缺失、被修改或未列入清单的记录不会被
接收，并在报告中列出（`missing`、`problems`）；完好的记录照常接收，迁移被标记为不完整
（`complete: false`，代码 `4`）。`restore` 会用备份中经过验证的副本替换记录的损坏本地副本（`replaced`），并将旧文件移到 `corrupt/` 而不删除。重复执行 `restore` 是幂等的。

## 由 `benchmark-runner` 自动保存

`benchmark-runner run` 在写出已接收的结果后，会用同一契约和目录自行将其交给历史，无需重新
测量。结果是摘要中的 `history` 字段：`archived`、`already-archived`、`disabled`
（`--no-history`）、带原因的 `failed`，或在没有已接收结果时为 `not-recorded`。历史失败不会
删除 `result.json`，也不改变系列的退出码，但会以警告形式输出并加入 `warnings`：结果之后可以
用 `import` 保存。

## 基线与比较

可比性只由 `benchmark_contract::comparability::compare` 决定：同一场景和修订、同一配置、
相同的指标定义、负载以及重要的环境事实。`subject` 输入不同（代码已修改）是预期差异；
`workload` 输入不同会使结果不可比。

自动选择基线的规则：**同一场景和配置中、早于被检查结果记录、源代码经证明不同且按契约可比的最新
成功结果**。源代码不同由不同的 `source_tree_sha256` 证明；若并非两个结果都记录了源码树，则由两个
都在无修改工作副本中测量的不同提交证明（报告中附带说明）。缺少信息不算作不同。`baseline` 显示每个
候选及其被拒绝的原因：`partial`/`failure`/`unsupported`、记录时间更晚、相同的
`source_tree_sha256` 或无修改的相同提交（是重复测量，而不是“之前”）、代码不同未得到证明、可比性
阻碍。可以显式指定具体基线（`report --baseline ID`）；与规则的偏差会在报告中列出，与被检查结果相同
的基线在报告和 `compare` 中都会被拒绝。

- 差值基于成功运行的均值计算；相对差值只在基线非零、单位和范围一致时计算。
- 改善方向：对于时间、内存、CPU 时间、磁盘、指令、周期和未命中，越低越好；对于核心利用率、
  缓存访问和模块计数器，不评估方向。
- 评估：若 |Δ| 不超过两侧标准差之和，为 `в пределах разброса`（在离散度之内）；否则为
  `сверх разброса`（超出离散度）；若任一侧测量少于两次，为 `мало повторов`（重复次数不足）。
  这**不是**统计显著性检验，契约中的 `statistically_supported` 仅表示测量次数足以估计离散度。
- 不可比的结果不得出结论；`unavailable`、`unsupported`、`failed` 以带原因的状态显示，而不是零。

## PR 章节

`report` 按 [`pr-body.md`](../../.agents/skills/anki-git-workflow/references/pr-body.md) 构建
章节：`Статус: измерено`，包含字段 «Сценарий»、«Измеренный код»（提交、修改标记、源代码树）、
«Нагрузка»（输入标识和规模）、«Окружение»、«Прогоны»、«База»、«Сопоставимость»、指标表、
«Вывод»、«Ограничения» 和 «Источники»（完整的记录 ID）。没有基线时，章节区分新功能（`--first`：
不存在“之前”）与缺少可用基线。`failure` 或 `unsupported` 结果给出 `Статус: не измерено`，附带原因、
诊断以及 «Доказательство производительности: отсутствует» 一行。章节输出到 stdout，并手动粘贴到
PR 描述中；工具本身从不写入 GitHub。

## 文档快照

`snapshot` 替换模块页面上同一场景 × 配置对的块，同时替换其翻译（`<模块>.en.md`、
`<模块>.zh-CN.md`）；新的一对添加在 `<!-- benchmark-snapshot:anchor -->` 行之前。块中包含日期、
版本、环境事实、负载规模和指标，但不包含提交、记录 ID、输入标识或路径。以下情况拒绝且不写入：

- 结果不是 `success`，或结果属于过时的场景修订；
- 工作副本有修改或未记录状态（`--allow-dirty` 允许，且块中会注明）；
- 参考环境未确定：硬件或平台事实（`os`、`arch`、`virtualization`、`cpu_model`、`logical_cpus`、
  `memory_total_bytes`）或场景的可比性事实未知；
- 页面上有来自其他参考环境的快照（`--replace-environment` 确认更换参考）、来自不同规模负载的快照（`--replace-workload` 确认）或更新的测量。

快照的参考环境比可比性条件更宽：硬件和平台事实始终属于参考环境，即使场景不比较它们，因此在其他
处理器上、以其他内存容量或从 WSL2 换到无虚拟化 Linux 后得到的测量不会静默替换原有公开参考，而结果
之间的比较并不会因此变得更严格。旧标记中没有的事实，以及未记录负载的旧格式标记，都不算作一致：替换
由相应标志确认。

三个页面一致地写入：在三份临时副本全部准备好之前，任何页面都不会改变；随后按 RU、EN、zh-CN 的顺序
逐个通过重命名替换，若某次替换失败，已替换的页面会恢复为原文本。若恢复也失败，错误会列出已更新和
未更新的页面。准备计划后被修改的页面不会被覆盖。在两次重命名之间被中断的进程会让翻译保留旧俄语
文本的 `i18n-source-git-blob` 标记，文档检查会发现这一点。

只有当翻译此前已针对旧的俄语文本确认时，才会更新翻译的 `i18n-source-git-blob` 标记。

## 退出码

| 代码 | 含义 |
|---|---|
| `0` | 成功 |
| `1` | 否定答复：不可比、没有可用基线、`verify` 或 `reindex` 发现问题、快照被拒绝 |
| `2` | 参数错误、ID 未知或有歧义、存储或备份位于 Git 工作副本内、基线与被检查结果相同 |
| `3` | 存储、索引或目录不可用 |
| `4` | 文档被契约拒绝，未被接收；备份迁移不完整 |
| `5` | 归档副本已写入，索引未更新：请运行 `reindex` |

## 隐私

被接收的结果已通过契约检查：其中没有绝对路径、密钥或原始输出。存储是用户在仓库之外的本地
数据；它既不被提交也不被发布。PR 章节包含提交和输入标识（在 PR 中注明它们是允许且有用的），
文档中则不包含。

## 测试

测试（`tests/history.rs`）在临时目录中构建合成目录和结果：幂等接收与原始字节；拒绝无效、敏感
和未知文档；被删除、损坏、过时和过新的索引，缺少必需表和行被篡改的索引；中断的写入和无关文件；
损坏的归档副本；通过 `backup`/`restore` 迁移到另一个存储，以及不完整或被修改的副本；从外部目录运行时
位于 Git 工作副本内的存储；基线规则与代码不同的证明、单位、零基线和离散度；所有分支的 PR 章节，并由
真实的 `.github/scripts/check_pr_body.py` 校验；带独立参考硬件、旧格式标记和三页之一写入失败的文档
快照；以及完整 CLI。测试不读取 `decks/**`，也不读取
用户的数据目录。
