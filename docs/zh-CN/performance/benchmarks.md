# 基准目录与结果契约

[Русский](../../ru/performance/benchmarks.md) · [English](../../en/performance/benchmarks.md) · **简体中文**

> **翻译提示：** 本译文可能未及时更新，技术内容以[俄语文档](../../ru/performance/benchmarks.md)为准。欢迎提交翻译和校对改进。

目录是[测量方法](README.md)用文字描述内容的机器可读形式：测量哪些场景、在什么
条件下测量、被测区间包含什么，以及收集哪些指标及其覆盖范围。结果契约是带版本的 JSON，
执行器在其中记录测量值。两个契约都由 `benchmark-contract` 校验
（[CLI 与退出码](../benchmark-contract/README.md)）。

目录及其校验既不运行也不测量任何东西。系列由
[`benchmark-runner`](../benchmark-runner/README.md) 执行：Hyperfine 2.0
与 Linux 系统指标。它使用本契约，而不是另起一套。本地结果历史、基线选择和报告由
[`benchmark-history`](../benchmark-history/README.md) 负责；仓库中没有运行历史
和阈值。模块快照（`docs/ru/<模块>/performance.md`）仍然是特定环境的证据。

## 文件位置

| 路径 | 内容 |
|---|---|
| `benchmarks/catalog.toml` | 模式版本、仓库根目录和共享的配置词汇 |
| `benchmarks/scenarios/<模块>.toml` | 一个模块的场景声明；文件名等于模块名 |
| `tools/benchmark-contract/` | 类型化模型、加载、校验、Hyperfine 导入、CLI |
| `tools/benchmark-runner/` | 系列执行器：Hyperfine 2.0、cgroup v2、`/proc`、模块报告 |
| `tools/benchmark-contract/tests/fixtures/` | 测试用的示例目录与结果（`status = "example"`） |

发现规则是固定的：加载器读取 `catalog.toml` 以及 `scenarios/` 中所有 `*.toml`，不进入
子目录。隐藏文件被跳过；其他任何条目或符号链接都是错误，而不是被悄悄忽略的声明。

截至今天的场景（以声明本身为准，集合会变化）：

| 场景 | 类型 | 配置 |
|---|---|---|
| `anki-e2e.full` | `long-e2e` | `cold`, `warm` |
| `anki-e2e.smoke` | `long-e2e` | `warm`, `offline-warm` |
| `anki-repo.validate` | `short-cli` | `warm`, `offline-warm` |
| `anki-repo.stats` | `short-cli` | `warm` |
| `asset-store.verify-integrity` | `short-cli` | `warm` |
| `asset-store.read-batch` | `short-cli` | `warm` |
| `asset-store.read-single` | `short-cli` | `warm` |
| `asset-store.ingest` | `short-cli` | `warm` |
| `asset-store.ingest-contention` | `short-cli` | `warm` |
| `asset-store.batch-reopen` | `short-cli` | `warm` |
| `asset-store.batch-checkpoint` | `short-cli` | `warm` |
| `benchmark-runner.calibration-noop` | `short-cli` | `warm` |
| `benchmark-runner.calibration-cpu` | `short-cli` | `warm` |
| `benchmark-runner.calibration-cpu-plain` | `short-cli` | `warm` |
| `benchmark-runner.calibration-parallel` | `short-cli` | `warm` |
| `benchmark-runner.calibration-memory` | `short-cli` | `warm` |
| `benchmark-runner.calibration-io` | `short-cli` | `warm` |

## 为新模块注册场景

1. 创建 `benchmarks/scenarios/<模块>.toml`，写入 `schema_version = 1` 和
   `module = "<模块>"`。代码和 `catalog.toml` 都不需要修改。
2. 用 ID `<模块>.<名称>` 声明 `[[scenario]]`。
3. 校验目录并查看它被如何理解：

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- show <模块>.<名称>
```

短 CLI 操作的最小声明：

```toml
schema_version = 1
module = "example-tool"

[[scenario]]
id = "example-tool.check"
title = "检查输入文件"
revision = 1
kind = "short-cli"
status = "executable"
description = "命令做什么，以及为什么值得测量。"
measured = "一次运行，从进程启动到退出；不含构建和预热。"
platforms = ["linux"]

[scenario.target]
program = { kind = "cargo-bin", package = "example-tool", bin = "example-tool" }
args = ["--json", "check", "{input.data}"]
expect_exit_codes = [0]

[scenario.target.env]
inherit = ["PATH"]

[[scenario.parameter]]
name = "data_path"
description = "输入路径；不会发布。"
required = true
record = false
affects_comparability = false

[[scenario.input]]
id = "data"
kind = "external-path"
role = "workload"
identity = "content-tree-sha256"
parameter = "data_path"
description = "命令输入。"
size = [{ name = "bytes", unit = "byte", description = "输入大小。" }]

[scenario.runs]
warmup = 3
min_runs = 10
max_runs = 100
single_run_allowed = false
run_timeout_seconds = 60
total_budget_seconds = 900

[[scenario.profile]]
id = "warm"
cache_definition = "没有自身缓存；操作系统页缓存由三次运行预热。"
cleared = []
retained = ["操作系统页缓存"]

[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[scenario.environment]
record = ["os", "arch", "cpu_model", "module_version", "hyperfine_version"]
comparable = ["os", "arch", "cpu_model"]
```

## 声明字段

| 字段 | 必填 | 含义 |
|---|---|---|
| `id` | 是 | 稳定 ID `<模块>.<名称>`；场景不变则 ID 不变 |
| `title`, `description` | 是 | 名称以及到底测量什么 |
| `revision` | 是 | 负载及其语义的版本，从 1 开始 |
| `kind` | 是 | `short-cli` 或 `long-e2e`；决定运行限制 |
| `status` | 是 | `executable` —— 真实命令；`example` —— 教学示例，从不测量 |
| `measured` | 是 | 用文字说明被测区间的边界 |
| `platforms` | 是 | `linux`、`macos`、`windows`：在哪里测量该场景 |
| `target` | 是 | `program`、`args`、`working_dir`（相对仓库根目录，默认 `.`）、`env.set`、`env.inherit`、`expect_exit_codes` |
| `parameter` | 否 | `name`、`description`、`required`、`default`、`choices`、`record`、`affects_comparability` |
| `input` | 否 | 带类型、角色、标识和规模的输入 |
| `runs` | 是 | `warmup`、`min_runs`、`max_runs`、`single_run_allowed`、`run_timeout_seconds`、`total_budget_seconds` |
| `setup`, `cleanup` | 否 | 整个场景的生命周期步骤 |
| `profile` | 是 | 配置绑定，包含确切的缓存定义以及 `setup`、`prepare`、`conclude` 步骤；`args_append` 是该配置追加到目标的参数，`required_parameters` 列出该配置设为必需的参数（例如 `required_parameters = ["anki_version"]`） |
| `metric` | 是 | 请求的指标；scope 为 `command` 的 `time_wall_clock` 为必需 |
| `environment` | 是 | 要记录（`record`）和比较（`comparable`）的事实，以及模块自己的事实（`fact`） |
| `requires` | 否 | `executable`、`docker`、`linux-cgroup-v2`、`perf-events`、`network`（附配置） |
| `module_report` | 否 | 模块的机器可读报告：`runs_dir`（相对仓库根）、文件名 `report`、`description`。`module-report` 来源和 `report` 字段需要它 |
| `container` | 否 | 要测量其 cgroup 的 Docker 容器：`label`（`键=值`）、`owner_pid_label` —— 记录创建者 PID 的标签、`description`。要求 `requires` 中有 `docker` |

`program` 要么是 `{ kind = "cargo-bin", package, bin, cargo_profile }`（默认
`release`；包和可执行文件会与 Cargo 工作区清单核对），要么是来自 `PATH` 的
`{ kind = "executable", name }`。步骤以 argv 表示，而不是 shell 字符串：禁止 `sh`、
`bash`、`env`、`sudo`、`rm`、`dd`、`nice` 和 `timeout` 之类的启动包装器等程序。该列表用于捕获明显的声明错误，
但并不完整，也不是安全边界：声明由维护者编写，其内容通过评审把关。参数和 `env.set` 的值中可使用
`{param.<名称>}` 和 `{input.<id>}` 占位符（后者仅限 `repo-file` 和 `external-path`）；
字面花括号需写两次。没有默认值的可选参数不能被替换，除非配置在
`required_parameters` 中列出它。未被替换进任何命令、也不承载 `external-path`
输入的参数会产生警告：其记录值不会影响运行。

## 标识、修订与测量

三者从不混淆：

- **场景 ID** 是基准的稳定名称；
- **修订**是其负载和语义的版本。当目标参数、输入或其选择方式、测量边界、配置定义或
  指标含义发生变化时，请递增修订。不同修订的结果不可比较；
- **日期和环境**属于单条结果记录，而不属于场景。

## 生命周期与测量边界

一个“场景 × 配置”系列的顺序：

1. 执行器构建 `cargo-bin`（`cargo build --locked -p … --bin … --profile …`）。这属于准备。
2. 场景的 `setup`，然后是配置的 `setup` —— 各一次。
3. 预热运行：`prepare` → 目标 → `conclude`；不保存数值。
4. 测量运行：`prepare` → **目标** → `conclude`。只测量目标。
5. 场景的 `cleanup` —— 总是执行，包括失败和中断之后。

步骤由执行器自己按 argv 执行，不经过 shell；Hyperfine 2.0 只测量目标，每次运行单独调用一次。
`setup` 失败得到结果
`failure` 且没有运行；`prepare` 失败、超时或目标的意外退出码会停止系列，若此前有成功
运行则结果为 `partial`，否则为 `failure`。每个步骤受 `timeout_seconds` 限制，每次运行受
`run_timeout_seconds` 限制，从 `setup` 开始到最后一个 `conclude` 的整个系列受
`total_budget_seconds` 限制；预算耗尽即停止系列。构建 `cargo-bin` 和 `cleanup` 不计入
预算：构建由执行器限制，`cleanup` 即使在预算耗尽后也会执行，受其各步骤的
`timeout_seconds` 限制。

步骤的 `touches` 说明它影响什么：`read-only`、`scenario-temp`（场景自己的临时数据）或
`module-owned`（模块用自身命令清理的模块状态）。没有“全部”这一取值：清空全局页缓存、
不带 `--filter` 的 `docker … prune` 以及清理其他项目的缓存都无法在声明中表达。

## 配置与缓存状态

`catalog.toml` 声明配置 `cold`、`warm`、`offline-warm` 和 `incremental`。配置给出基础
缓存状态和网络；场景的配置绑定给出**确切**定义：`cache_definition`、`cleared`（被置为
所需状态的内容）和 `retained`（有意保留的内容）。“冷”从不隐含清空操作系统页缓存。

- `cold` —— `prepare` 必须用 `module-owned` 步骤重置模块状态，`cleared` 不为空，
  没有预热。对模块是冷的，对系统不是。
- `warm` —— 需要预热：`runs.warmup ≥ 1` 或配置的 `setup`；`prepare` 不重置模块状态。
- `incremental` —— `prepare` 修改输入，引发部分重建。
- 离线配置必须填写 `network_definition`：排除了哪些请求以及如何保证。执行器在无网络
  的情况下运行系列，网络错误即运行失败。离线配置不能出现在 `network` 要求中。

## 运行策略与限制

| 限制 | `short-cli` | `long-e2e` |
|---|---|---|
| 预热 | ≤ 100 | ≤ 3 |
| 运行次数 | ≤ 1000 | ≤ 10 |
| 步骤超时 | 1…1800 秒 | 1…14 400 秒 |
| 单次运行超时 | 1…600 秒 | 1…14 400 秒 |
| 系列预算 | ≤ 3600 秒 | ≤ 86 400 秒 |
| 单次运行 | 禁止 | 允许（`single_run_allowed`） |

未设置 `single_run_allowed` 时要求 `min_runs ≥ 2`。带有热配置的场景始终要求 `min_runs ≥ 2`：热状态至少测量两次。预算至少要容纳一次运行。
`benchmark-contract vocabulary` 输出这些限制。步骤超时适用于 `setup`、`prepare`/`conclude`、
`cleanup` 和输入探测；单次运行超时适用于被测目标。

## 输入与负载规模

| 类型 | 标识 | 路径来源 |
|---|---|---|
| `repo-tree` | `content-tree-sha256` | 工作副本 |
| `repo-file` | `file-sha256`, `content-tree-sha256` | 声明中的 `path`；文件必须存在 |
| `external-path` | `file-sha256`, `content-tree-sha256` | `record = false` 且没有 `default` 的参数：路径仅在运行时给出，从不发布 |
| `downloaded-artifact` | `file-sha256`, `declared-version` | 外部制品 |

角色 `subject` 是比较对象（例如代码）：标识不同是预期的。角色 `workload` 是负载：必须
声明规模特征（`byte` 或 `count`），负载标识或规模不同的结果不可比较。

标识和规模的来源由声明明确给出：

- 规模特征的 `measure`：`file-bytes` 带输入内的 `path`，单位 `byte`；`tree-bytes`
  （`byte`）；`tree-files`（`count`）；`probe` 带指向输入探测 JSON 输出的 `pointer`；
  `report` 带指向模块报告的 `pointer`；
- 输入的 `probe` 是一个 `read-only` 步骤，打印关于输入的 JSON（例如
  `anki-repo --json stats`）；它在准备阶段执行，不计入测量；
- `report_identity` 是模块报告中 `downloaded-artifact` 标识的 JSON Pointer：模块自行
  下载的制品，执行器不会下载。

无法计算标识或规模的输入会产生警告 `unresolvable_input`。在结果中，这样的输入只允许在
没有成功运行的系列中缺失（`unsupported`、`setup` 失败）：成功的运行必须记录所有输入。

## 指标：scope、来源、单位

基本单位：`second`、`byte`、`count`、`core`（平均占用核数：每秒的 CPU 秒）。契约中没有
其他单位。结果中的指标键为 `指标@scope` 或 `指标@scope/标签`。

| 指标 | 单位 | Scope / 来源 | 含义 |
|---|---|---|---|
| `time_wall_clock` | second | `command` / `hyperfine` | 目标单次运行的时长 |
| `time_user`, `time_system`, `time_cpu` | second | `process-tree-waited` / `hyperfine`；`process` 或 `process-tree-waited` / `procfs` | CPU 时间；子进程仅在被等待时计入 |
| `memory_peak_resident` | byte | `largest-process-in-tree` / `hyperfine`；`process` / `procfs` | **单个**进程的峰值 RSS，不是进程树总和 |
| `memory_peak_cgroup` | byte | `cgroup` / `cgroup-v2` | 整个组的同时峰值（`memory.peak`） |
| `cpu_time_cgroup` | second | `cgroup` / `cgroup-v2` | 整个组的 CPU 时间（`usage_usec`） |
| `cpu_utilization_mean`, `cpu_utilization_peak` | core | `cgroup` / `cgroup-v2`；`host` / `procfs` | 观测到的负载；峰值是区间内的下界；运行自有子组的平均值是其 CPU 时间除以其存活时间（整个 Hyperfine 调用） |
| `memory_host_peak_increase` | byte | `host` / `procfs` | 运行期间整个系统 `MemAvailable` 的最大下降：包含其他进程的分配，并会被同时发生的释放（包括上一轮内存的延迟归还）抵消，因此是主机观测值而不是界限 |
| `cpu_cycles`, `instructions`, `cache_references`, `cache_misses`, `branch_misses` | count | `process-tree-waited` / `hyperfine` | CPU 计数器；需要 `perf_event` |
| `phase_duration` | second | `phase` / `module-log` | 依据模块日志得到的目标内部阶段；需要标签和 `resolution` |
| `module_counter` | count | `module-report` / `module-report` | 模块机器报告中的计数器；需要标签 |
| `disk_usage` | byte | `path` / `filesystem` | 指定资源的表观大小，同 `du -sb`；需要标签 |

注册表之外的组合是错误 `metric_scope_source`，而不是新指标。带平台信息的完整注册表见
`benchmark-contract vocabulary`。

来源为 `module-report` 的指标设置 `pointer`，即模块报告中的 JSON Pointer，其中 `*` 段
选择数组或对象的所有元素，并对值求和；`disk_usage` 设置相对仓库根的 `path`。`fact` 中的
模块事实可以设置 `report`，即报告中的指针列表；各次运行之间值不同时，该事实为未知。其他
指标禁止使用这些字段（`unexpected_field`）。

进程的峰值 RSS、cgroup 内存峰值、CPU 时间和观测负载是不同的量，互不替代。Hyperfine
只看到目标进程和被等待的子进程：Docker 容器内的进程和 BuildKit 步骤不计入其
`time_cpu` 和 `memory_peak_resident`。在 Linux 上，Hyperfine 可能用继承自自身的值替代
很小的峰值 RSS。

在来源不可用的平台上不能承诺必需指标：这是错误；对可选指标则是警告，结果会将其标记为
`unsupported`。必需的 CPU 计数器要求 `requires` 中包含 `perf-events`，必需的 cgroup
指标要求 `linux-cgroup-v2`。校验目录不需要 cgroup 或 `perf_event` 权限。

## 环境与可比性

`environment.record` 列出写入结果的事实；`os`、`arch` 和 `module_version` 为必需。
`environment.comparable` 是必须一致的子集；始终包含 `os` 和 `arch`。内置事实：`os`、
`arch`、`kernel_release`、`virtualization`、`cpu_model`、`logical_cpus`、
`memory_total_bytes`、`module_version`、`rustc_version`、`docker_version`、
`hyperfine_version`；模块在 `fact` 中声明自己的事实。不要求收集所有可能的硬件特征。

两个结果只有在场景与修订、配置、负载标识与规模、带 `affects_comparability` 的参数、
被比较事实的已知值以及指标定义都一致时才可比较（`benchmark-contract compare`）。未知
事实不能证明可比性。比较对象输入的差异会单独报告，不妨碍比较。只有当每个结果中某指标
至少有两个来自成功运行的值时，才允许对该指标得出统计结论：失败的尝试不产生值。这类指标
列在 `statistical_metrics` 中；当每个可比较指标都满足这一点时，`statistically_supported`
为真。

## 结果契约

结果是 `schema_version = 1` 的 JSON，独立于 Hyperfine 格式。下面的示例经过删节：省略了部分环境事实、输入规模、指标和运行，因此 `check-result` 不会接受它。完整的有效结果见
`tools/benchmark-contract/tests/fixtures/results/`。

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

状态：

- 结果 `outcome.status`：`success` —— 所有运行成功，必需指标在每次运行中都已测量，且
  运行次数不少于 `min_runs`；`partial` —— 至少有一次成功运行，但系列或指标不完整；
  `failure` —— 没有成功运行；`unsupported` —— 场景无法在此测量，没有运行。除
  `success` 外都需要 `reason`；
- 运行：`success`、`failure`、`timeout`、`interrupted`；失败的运行需要 `reason`。
  成功的运行记录 `exit_code`，按目录检查时该代码必须属于场景的 `expect_exit_codes`，
  否则即为 `failure`；
- 运行中的指标值：带 `value` 的 `measured`，或带 `reason` 的 `unavailable`、
  `unsupported`、`failed`。每个指标出现在每次运行中。缺失从不记录为零；
- 指标的 `availability` 必须由各次运行推出：`measured`、`partial` 或某个缺失状态。

按目录检查时，`warmup_runs` 由生命周期决定：测量运行一旦开始，它就等于 `runs.warmup`；
没有运行时，序列可能停在 `setup` 或预热途中，因此允许更小的数字，但不允许更大。通过配置的
`setup` 进行的预热不计入 `warmup_runs`。

`observation`：没有运行为 `none`，一次运行为 `single` —— 有效观测但没有统计，两次及
以上尝试为 `repeated`。聚合值可选，只根据成功运行的已测量值计算；`stddev` 为样本标准差
（n − 1），单个值时为 `null`。校验会重新计算聚合值，并拒绝不一致以及没有原始值的聚合。

`provenance` 在结果记录本身中保存技术标识（提交 SHA、树哈希）。它们不会写入永久文档，
包括模块页面 —— 这是 [`AGENTS.md`](../../../AGENTS.md) 的规则。

发布的结果不包含主目录内的路径、令牌、私钥、凭据和电子邮件地址：校验会检查文档的所有
字符串和键。`record = false` 的参数值不会进入结果，进程的环境变量也不会。

## Hyperfine 2.0 导入

库模块 `benchmark_contract::hyperfine` 读取 Hyperfine 2.0 的 `--export-json`
（`schema_version: 2`）；1.x 导出及其他版本会被明确拒绝。格式已对照 `v2.0.0` 标签核实。

| Hyperfine | 契约 |
|---|---|
| `measurements[j]` | `runs[j]`；预热不在导出中 |
| `exit_code` | `exit_code`；不在 `expect_exit_codes` 中或为 `null`（信号）—— 运行 `failure` |
| `time_wall_clock` | `time_wall_clock@command` |
| `time_user`, `time_system`, `time_cpu` | `…@process-tree-waited` |
| `memory_peak_resident` | `memory_peak_resident@largest-process-in-tree` |
| CPU 计数器 | `…@process-tree-waited` |
| 省略的指标 | 带原因的 `unavailable` |
| `summary` | 对在每个 `measurements` 中都存在的指标是必需的，并与基于全部测量的重新计算结果核对；缺失或不一致则拒绝导入。值集不完整时 Hyperfine 不写摘要 |
| `metadata.hyperfine_version`, `start_time`, `platform` | `provenance.measurement`、记录时间、`os`/`arch` 事实 |

导出中的 `command` 和 `environment` 不会带入结果：其中可能包含用户路径和环境值，而目标
已由场景、修订和参数完全确定。场景未请求的导出指标会列在 `unrequested` 中，而不是被
悄悄丢弃。

## 校验

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- list
cargo run -p benchmark-contract -- preflight anki-e2e.full
cargo run -p benchmark-contract -- check-result result.json
cargo run -p benchmark-contract -- compare before.json after.json
cargo run -p benchmark-contract -- --json vocabulary
```

`validate` 只检查声明，不需要 Docker、Anki 或 Hyperfine。当前环境的平台和依赖可用性由
`preflight` 单独检查：操作系统不在场景的 `platforms` 中即为否定结果；它只读取 `PATH`、`/sys/fs/cgroup` 和 `/proc/sys`。诊断会给出文件、
场景、字段和稳定的原因代码。`cargo test --workspace` 套件同时检查真实目录和示例夹具。

## 模式演进

- 目录和结果的 `schema_version` 是整数。工具只理解已知版本，对其他版本明确拒绝，而不是
  部分读取文档。
- 旧工具不应悄悄忽略的新字段或枚举值会提升版本。模式是封闭的：未知字段即错误。
- 版本之间的迁移是从版本 N 到 N + 1 的有文档的确定性转换，与版本 N + 1 一起实现；
  不预先构建通用迁移框架。
- 历史存储按其 `schema_version` 保存结果，不会改写。
- 旧工具会作为未知字段拒绝、而不是悄悄忽略的可选字段（`module_report`、`container`、
  `measure`、`probe`、`report_identity`、`path`、`pointer`、事实的 `report`）不提升版本。
- 场景修订与模式版本相互独立：新修订不改变模式，新模式也不改变场景修订。

## 执行器如何使用契约

`benchmark-runner` 通过库加载目录，选择场景和配置，用探测运行检查环境就绪情况，构建
`cargo-bin`，替换参数和输入，计算输入的标识和规模，执行生命周期步骤，用 Hyperfine 2.0
和 `--export-json` 测量目标，通过本库导入导出结果，加入其他来源的指标（cgroup、`/proc`、
模块报告、文件系统），填写环境与 provenance，用 `check-result` 对照目录校验结果，然后才
保存。比较使用 `compare`。

## 边界

这里没有历史存储、没有回归计算，也没有 CI 阈值：历史和报告在
[`benchmark-history`](../benchmark-history/README.md) 中，且没有阈值。模块快照中的
数字不会成为阈值。

## 块 I/O

`io_read_bytes`/`io_write_bytes`（byte）和 `io_read_operations`/`io_write_operations`
（count）的 scope 为 `cgroup`，source 为 `cgroup-v2`：按
[Linux 内核文档](https://docs.kernel.org/admin-guide/cgroup-v2.html#io-interface-files)，
汇总 `io.stat` 各设备的 `rbytes`/`wbytes`/`rios`/`wios`。
新的自有组包含目标、其子进程和 Hyperfine，从进入组到删除组前的最终读取；
setup/prepare/conclude/cleanup 与采样器在组外。容器必须已发现且能获得最终组快照；
组消失时返回 `unavailable`，不把最后一次不完整采样当作总量。

仅在请求这些指标时，才在执行器自己获委派的组启用可选 `io` 控制器。
启用通过实际操作验证：写入 `+io` 后，控制器必须出现在 `cgroup.subtree_control` 中；
经 `systemd-run` 的委派也在一次性组中以同样的激活验证。
`io` 被拒绝或缺失不影响 CPU/RAM：`check` 显示原因，结果以该原因保留 `unsupported`，
必需的 I/O 指标使结果为 `unsupported` 且不执行运行。
可读且为空的 `io.stat` 表示零次**已记账**操作；读取失败、不完整行、重复设备或溢出不能变为零。

这是内核块层记账，不是逻辑读写字节、系统调用、`disk_usage`，也不是 VM/WSL 后面的物理介质。
page cache 命中无需读盘。区间结束后的 writeback 不计入，完成写入要求目标自身执行 `fsync`。
writeback 归属依赖文件系统支持和 inode 所有权；共享文件可能计入其他组。
多层设备可能重复记账，所以总量不代表唯一物理介质字节。此处不发布吞吐量或 IOPS：
不能用 Hyperfine 单个目标的时间去除覆盖范围不同的组计数。

增加指标的场景使用新修订。旧 JSON 仍是原修订的有效独立结果，可从本地历史读取，
但不能按新声明验证；现有快照不会补上虚构的 I/O 列。
