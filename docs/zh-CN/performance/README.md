# 性能与资源消耗

[Русский](../../ru/performance/README.md) · [English](../../en/performance/README.md) · **简体中文**

> **翻译提示：** 本译文可能未及时更新，技术内容以[俄语文档](../../ru/performance/README.md)为准。欢迎提交翻译和校对改进。

本目录提供通用方法及指向各模块页面的链接，展示仓库各模块性能与资源消耗的**最近确认指标**：时间、CPU、内存、磁盘。每个模块有
自己的页面，包含测量方法、指标定义，以及在明确命名的参考环境中按场景和配置划分的快照。
本页规定测量的进行、保存和发布的通用规则。

快照是特定环境中实际行为的证据，不是契约，不是对其他系统的速度标准，也不是 CI 阈值。其中的
数字不会固定到代码、测试或检查中：不同的硬件、操作系统或 Docker 会得到不同的值。

## 历史存放在哪里

| 内容 | 位置 | 谁能看到 |
|---|---|---|
| 每次测量的完整历史 | 本地 [`benchmark-history`](../benchmark-history/README.md) 历史：用户数据目录中原始 JSON 的不可变归档和 SQLite 索引 | 仅其所有者 |
| 任务的完整报告 | 本地保留未经修改的 `benchmark-history report` | 执行者及获取报告的审阅者 |
| 公开的历史记录 | PR 的 `## Производительность`（性能）章节：场景、基线、候选版本、结果、结论和显著退化的简要摘要 | 所有人，通过 PR 历史 |
| 当前快照 | `docs/ru/<模块>/performance.md` 页面：每个场景 × 配置对的最近确认测量 | 所有人 |

页面不会累积所有 PR 的记录：新的确认测量会替换同一对的快照，而旧值保留在 Git 历史和 PR
描述中。公开参考是项目所有者在快照所描述环境中的测量。其他用户有自己的本地结果；它们不会
自动修改公开页面，来自其他环境的快照也不会在未经明确确认的情况下替换参考快照。

## 模块页面

| 模块 | 页面 | 基于 JSON 结果的快照 |
|---|---|---|
| `anki-repo` | [anki-repo/performance.md](../anki-repo/performance.md) | `anki-repo.validate` · `warm`、`offline-warm`；`anki-repo.stats` · `warm` |
| `asset-store` | [asset-store/performance.md](../asset-store/performance.md) | `asset-store.*` · `warm`（`kanji-medium`） |
| `anki-e2e` | [anki-e2e/performance.md](../anki-e2e/performance.md) | `anki-e2e.smoke`、`anki-e2e.full` · `warm`；其他状态为无原始 JSON 的手动观测 |
| `benchmark-runner` | [benchmark-runner/performance.md](../benchmark-runner/performance.md) | `benchmark-runner.calibration-*` · `warm` |

新模块需要俄语原文页面 `docs/ru/<模块>/performance.md`，在放置快照的位置加入
`<!-- benchmark-snapshot:anchor -->` 一行，并在此表中增加一行。

## 场景目录与结果契约

场景、执行条件配置、被测工作的边界以及带覆盖范围的指标，以机器可读的形式声明在
`benchmarks/` 目录中；测量结果以带版本的 JSON 记录。如何注册场景以及如何解读
结果，见[基准目录](benchmarks.md)；系列由
[`benchmark-runner`](../benchmark-runner/README.md) 执行。本页规则同样适用于按目录进行的测量：
目录要求每个场景声明确切的缓存状态，而快照仍然是环境的证据。

## 测量规则

1. **先提问再测量。** 先明确需要了解什么（例如“重复运行的代价是多少”或“容器是否在
   限制之内”），再为这个问题选择指标。不要漫无目的地收集数字。
2. **环境。** 记录 CPU 型号和线程数、内存容量、操作系统和内核（包括 WSL2）、Docker、
   `rustc` 及模块外部依赖的版本、生效的限制（`--cpus`、`--memory`）以及工作副本状态
   （干净或有改动）。
3. **缓存状态。** 每次运行都属于明确命名的状态：
   - *冷* —— 模块自身的缓存已由其常规命令删除；
   - *热* —— 输入不变的重复运行；
   - *增量* —— 只修改应触发部分重建的输入。

   准确记录哪些已清理、哪些没有。模块不拥有的共享缓存（例如守护进程的 BuildKit 层缓存
   或基础镜像）不会为了“完全冷”的测量而删除：那会影响其他项目。这样的运行对模块而言
   标记为冷，但对系统而言不是。
4. **重复。** 热状态至少测量两次并给出范围。不要拿单个数字与另一条记录的单个数字比较。
5. **无无关负载。** 测量期间不要运行构建、测试或其他重负载进程。采样器必须足够轻量：
   每秒读取一次 `/proc` 和 cgroup 几乎不会给系统带来负载。
6. **真实实现。** 在常规构建上测量模块的常规命令，而不是简化副本。不允许为了更好的
   耗时而关闭检查、缓存或清理。
7. **诚实。** 说明哪些没有测量。不要把估算当作测量，也不要把环境的属性当作代码的属性。
   没有测量证明存在问题的优化，不作为改进发布。

智能体处理优化或疑似回归的流程——假设、变更前旧实现的基线、变更后可比的系列、分析和报告——见
skill [`performance-benchmarking`](../../../.agents/skills/performance-benchmarking/SKILL.md)。
它以这些规则以及常规的 `benchmark-runner` 和 `benchmark-history` 为基础，而不取代它们。

## 指标来源与精度

| 指标 | 来源 | 精度 |
|---|---|---|
| 阶段耗时 | 模块日志中的 UTC 时间戳 | 一秒；`0 秒` 表示“不到一秒” |
| 进程内存峰值 | `/proc/<pid>/status` 中的 `VmHWM` | 进程的精确 RSS 峰值，不含子进程 |
| 进程 CPU | `/proc/<pid>/stat` 中的 `utime`+`stime` 和 `cutime`+`cstime` | 精确；只有进程等待过的子进程才被计入 |
| 容器内存峰值 | cgroup v2 的 `memory.peak` | 内核记录的精确峰值 |
| 容器 CPU | cgroup `cpu.stat` 中的 `usage_usec` | 总量精确；平均和峰值核数由采样之间的差值得出 |
| 1 秒内峰值核数 | 相邻采样 `usage_usec` 的差值 | 下限：短于间隔的突发会被平滑 |
| 主机负载与内存 | `/proc/stat`、`/proc/meminfo` 中的 `MemAvailable` | 整个系统：主机观测值而非界限——包含其他分配，并会被同时发生的释放抵消 |
| 磁盘 | `du -sb`、`docker image ls`、`docker system df -v` | 共享层的镜像大小不相加 |

## 如何更新快照

1. 用常规的 `benchmark-runner run` 运行目录中的场景；已接收的结果会自动进入本地历史。
2. 找到结果（`benchmark-history list`），检查其结果、运行、环境和不可用指标
   （`benchmark-history show`）。
3. 不写入地预览页面变更：
   `benchmark-history snapshot <ID> --page docs/ru/<模块>/performance.md`，然后用 `--apply` 写入。
   工具只更新俄语页面中该对的块，译文不变；若系列不完整、工作副本有修改、环境与参考环境不同或页面
   上有更新的测量，则拒绝。
4. 将同一结果完整且未经修改的 `benchmark-history report` 保存在本地以供核查。
   PR 正文保留简洁摘要：场景、测量的基线和候选提交、关键数值、结论及所有显著回退。

永久文档中不包含提交 SHA、PR 编号、本地绝对路径、输入标识和作者姓名
（[`AGENTS.md`](../../../AGENTS.md) 的规则）；这些细节属于 PR 摘要和本地历史。`benchmark-snapshot`
标记内的快照不手动编辑。

在历史出现之前记录的手动观测保留在模块页面上，并明确标注其没有原始 JSON 结果：不得将其当作
已确认的快照，也不得事后将其重建为结果。它们会被同一对的首个确认快照替换；如果场景不在目录
中，则被新的手动观测章节替换。`snapshot --apply` 在锚点之前添加快照，不改动没有标记的章节，因此过时的手动观测需在
添加首个快照的同一变更中手动删除。
