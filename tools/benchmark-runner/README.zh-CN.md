# 基准执行器：`benchmark-runner`

[Русский](README.md) · [English](README.en.md) · **简体中文**
<!-- i18n-source-git-blob: 91d5e346b0b34b4d959e9aa4956992fc34b902ff -->

`benchmark-runner` 是一个 Linux CLI：按 ID 和配置运行 `benchmarks/` 目录中的场景，用
Hyperfine 2.0 测量目标，加入 Linux 系统指标（cgroup v2、`/proc`、模块的机器可读报告、
文件系统），并以 `benchmark-contract` 格式写出结果。结果在被接受之前由
`benchmark_contract::result::check` 对照目录校验；未通过校验的结果不会成为最终结果。

场景字段、配置和指标的含义见[目录指南](../../docs/performance/benchmarks.zh-CN.md)；
目录和结果的模式归 [`benchmark-contract`](../benchmark-contract/README.zh-CN.md) 所有；
通用测量规则见[测量方法](../../docs/performance/README.zh-CN.md)；执行器自身的测量见其
[性能页面](performance.zh-CN.md)；本地结果历史见
[`benchmark-history`](../benchmark-history/README.zh-CN.md)。

执行器不修改用户导出和 Anki 配置文件，不读取 `decks/input/`，不调用 `sudo`，不清理
Docker 缓存（`docker system prune` 之类），自身不向网络发送数据。它把已接收的结果交给
用户的本地历史（见下文），但自己不维护该历史。
用户数据的路径只通过运行参数传入。

## 命令

```bash
cargo run -p benchmark-runner -- list
cargo run -p benchmark-runner -- check anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export --runs 20 --output validate-warm.json
cargo run -p benchmark-runner -- check anki-e2e.smoke --profile warm --runs 2
cargo run -p benchmark-runner -- --json run anki-e2e.smoke --profile warm --runs 2
```

| 命令 | 作用 |
|---|---|
| `list` | 可执行场景：ID、类型、配置、允许的运行次数；不显示 `example` 场景 |
| `check <id> --profile P` | 环境对系列的真实就绪情况：通过探测运行检查 Hyperfine 及其指标、可执行文件、Docker 守护进程、cgroup v2 与委派、`perf_event`、网络、离线隔离；对每个指标说明是否会被测量以及原因。不构建任何东西，也不运行目标 |
| `run <id> --profile P` | 完整系列和经过校验的结果 |

`check` 和 `run` 的参数：

| 参数 | 含义 |
|---|---|
| `--param <名称>=<值>` | 已声明参数的值；未知名称、`choices` 之外的值以及缺少必需参数都是用法错误 |
| `--runs N` | 测量运行次数；默认 `min_runs`。对 `long-e2e` 是必需的：长系列从不被隐式选择。范围从 `min_runs`（`single_run_allowed` 时为 1）到 `max_runs` |
| `--no-sampling` | 运行期间不轮询 `/proc` 和 cgroup；采样类指标变为 `unavailable` 并附原因。容器组只在运行期间可观测，因此带 `container` 的场景所有 `cgroup` 指标都不可用，其中必需的指标导致结论 `unsupported` |
| `--output FILE` | 结果文件（仅 `run`）；已存在的文件从不覆盖 |
| `--artifacts-dir DIR` | 产物根目录（仅 `run`）；默认是仓库的 `target/benchmark-runner` |
| `--build-timeout S` | 每个 `cargo-bin` 的构建上限，单位秒（仅 `run`），默认 3600 |
| `--no-history` | 不将已接收的结果保存到本地历史（仅 `run`） |

全局参数：`--catalog DIR` —— 包含 `catalog.toml` 的目录（默认是当前目录最近父级中的
`benchmarks/`）；`--json` —— 在 stdout 输出机器可读 JSON。系列进度始终写到 stderr 和
`runner.log`。

## 退出码

| 代码 | `exit` | 含义 |
|---|---|---|
| `0` | `success` | 所有运行成功，结果已被接受；`check` —— 系列可以在此测量 |
| `1` | `partial` | 结果已被接受，但系列不完整：成功运行之后有运行失败、预算耗尽、必需指标不完整、运行次数少于 `min_runs`，或清理失败 |
| `2` | `usage` | 参数无效、未知场景或参数、示例场景、`--output` 已存在 |
| `3` | `error` | 内部错误或目录不可用；未写出结果。若序列结束后已接受的结果写入最终文件失败，其副本保存为工件中的 `result.unwritten.json` |
| `4` | `failure` | 结果已被接受，没有成功运行（包括 `setup` 失败） |
| `5` | `unsupported` | 环境无法如实测量该系列（没有 Hyperfine 2.x、必需指标、Docker 或网络隔离）；写出带此结论且没有运行的结果；`check` —— 同样情况但不写文件 |
| `6` | `interrupted` | 系列被信号中断；已记录的运行被保留，清理已执行 |
| `7` | `busy` | 同一模块的系列已在运行 |
| `8` | `rejected` | 组装出的结果未通过契约校验；它以 `result.rejected.json` 保存在产物中，没有最终文件 |

带 `--json` 时，`run` 打印摘要：`ok`、`exit_code`、`exit`、`scenario`、`profile`、
`outcome`、`reason`、`result`、`artifacts`、`runs`、`successful_runs`、`cleanup`、
`history`、`warnings`、`diagnostics`。同样的摘要保存在产物目录的 `summary.json` 中。

## 本地历史

写出已接收的结果后，`run` 用同一契约和目录将其交给本地
[`benchmark-history`](../benchmark-history/README.zh-CN.md) 历史，无需重新测量：
`BENCHMARK_HISTORY_DIR`，否则为用户的 XDG 数据目录。位于工作副本内的存储会被拒绝。摘要的
`history` 字段：`status` —— `archived`、`already-archived`、`disabled`（`--no-history`）、
`failed`（带 `reason`）或 `not-recorded`（没有已接收的结果），以及记录 `id`。若存储被另一个 `benchmark-history` 命令占用，执行器最多等待 10 秒，之后保存为 `failed`。历史失败不会删除
结果，也不改变系列的退出码，但会以警告输出并加入 `warnings`；之后可以用
`benchmark-history import` 保存结果。

## 安装 Hyperfine 2.0

需要 Hyperfine 2.x：导出 `schema_version = 2`、无 shell 模式（`-N`）和 `--metrics`。
执行器按版本识别 Hyperfine 1.x，并以代码 `5` 如实拒绝。如果发行版的软件包较旧，可以使用
静态发布版：

```bash
curl -fsSLO https://github.com/sharkdp/hyperfine/releases/download/v2.0.0/hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
sha256sum hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
install -D -m 0755 hyperfine-v2.0.0-x86_64-unknown-linux-musl/hyperfine ~/.local/bin/hyperfine
hyperfine --version
```

请将校验和与发布页面上的 digest 核对；CI 安装的是同一个归档并固定校验和。处理器计数器
（`instructions`、`cpu_cycles`……）需要访问 `perf_event`（对自己的进程通常要求
`kernel.perf_event_paranoid ≤ 2`）：否则 Hyperfine 不会导出它们，这些指标变为
`unsupported`。

## 系列顺序

1. 选择场景、配置、参数和运行次数；模块锁位于工作副本自身中
   （`target/benchmark-runner/locks/<模块>.lock`）。它不依赖 `--artifacts-dir`：同一工作副本
   中同一模块的系列，无论使用哪个产物目录、经由哪条路径访问该副本，都会互相排斥；而相互
   独立的工作副本互不干扰。
2. 就绪检查；存在阻碍时 —— 结论 `unsupported`，没有运行。
3. 准备：代码来源快照（提交、改动标志、`source_tree_sha256`），构建 `cargo-bin`
   （`cargo build --locked`），从 `PATH` 解析可执行文件，计算输入的标识和规模，然后再做一次
   快照。如果准备期间工作副本发生了变化，或 `repo-tree` 标识与快照不一致，则不测量该系列
   （结论 `failure`）。准备不计入测量，也不计入系列预算。
4. 场景的 `setup`，然后是配置的 `setup`。
5. 预热：`prepare` → 目标 → `conclude`；不保留数值。
6. 测量运行：`prepare` → **目标** → `conclude`。
7. 场景的 `cleanup` —— 总是执行，无论成功、失败还是中断。

生命周期步骤由执行器自己按 argv 执行，不经过 shell，每个步骤在自己的进程组中并有自己的
`timeout_seconds`。目标由 Hyperfine 测量：每次运行单独调用一次 `-N --runs 1
--export-json`，因为 Hyperfine 2.0 在系列中途失败时根本不写导出。目标参数只替换一次
（`{param.*}`、`{input.*}`，双写的花括号是字面量），目标环境只包含声明的 `env.inherit`
和 `env.set`。`short-cli` 目标的输出被丢弃，`long-e2e` 的输出保存在产物中。

步骤超时、每次运行的 `run_timeout_seconds` 和每个系列的 `total_budget_seconds` 都被严格
执行。预算从 `setup` 开始计算，覆盖 `setup`、预热以及带有 `prepare` 和 `conclude` 的运行。
准备（由每个 `cargo-bin` 的 `--build-timeout` 和输入探测的 `timeout_seconds` 限制）和
`cleanup` 不计入预算；`cleanup` 即使在预算耗尽后也会执行，受其每个步骤的
`timeout_seconds` 限制。因此执行器工作的上限是这些限制之和，而不只是
`total_budget_seconds`。终止是分级的：先 SIGINT，再 SIGTERM，最后对整个进程组 SIGKILL。
执行器成为 subreaper，因此离开进程组的后代也会被找到并清除；这种“逃逸”进程会使该次运行
失败，在预热中则停止系列。生命周期步骤在成功退出后若留下了自己的进程，也不视为已完成：
清理会在这些后代工作中途将其终止，步骤的效果因此无法证明。这样的 `setup`、`prepare` 或
`conclude` 会停止系列，这样的 `cleanup` 会使清理不完整。

运行失败（意外退出码、超时、残留进程）会停止系列。若此前有成功运行，结论为 `partial`，
否则为 `failure`。清理失败会把 `success` 降为 `partial`，并反映在 `cleanup` 中。

## 信号与取消

第一个 SIGINT、SIGTERM 或 SIGHUP 会中断当前步骤或运行并停止系列；之后 `cleanup` 仍会执行。
第二个信号也会中断清理。被中断的运行以状态 `interrupted` 记录，退出码为 `6`。最终文件以
原子方式写入（临时文件加硬链接）：不存在部分结果，已存在的文件从不覆盖。

## Linux 指标及其覆盖范围

| 来源 | 采集方式 | 覆盖范围 |
|---|---|---|
| `hyperfine` | 每次运行的 Hyperfine 导出 | 目标进程及被等待的后代 |
| `procfs` / `host` | 运行期间轮询 `/proc/stat` 和 `MemAvailable` | 整个系统：主机观测值而非界限——包含其他分配，并会被同时发生的释放抵消 |
| 无容器时的 `cgroup-v2` / `cgroup` | 每次运行一个自有子组，`cpu.stat`、`memory.peak` | 目标、其全部后代以及 Hyperfine 进程本身；平均负载为该组 CPU 时间除以其存活时间 |
| 带 `container` 的 `cgroup-v2` / `cgroup` | 运行期间找到的 Docker 容器 cgroup | 容器内的进程 |
| `module-report` | 本次运行期间出现的模块机器可读报告 | 声明的 JSON Pointer 处的值 |
| `filesystem` | 运行后路径的大小 | 仅限指定路径 |

模块报告只属于一次运行：它是该次运行期间在 `runs_dir` 中出现的唯一新目录。如果声明从报告
中获取输入的标识或规模，则每次运行都必须用自己的报告确认它们：没有新的、可解析且完整的
报告时，该次运行为 `failure`，即使之前的运行已经记录了该输入。运行之间标识或规模发生变化
同样为 `failure`。仅用于可选指标和事实的报告可以缺失：此时指标不完整，运行仍然成功。

缺失的值从不记为零：指标变为 `unavailable` 或 `unsupported` 并附原因。仓库中没有机器可读
来源的指标（例如模块日志为人类可读文本时的 `phase_duration`）为 `unsupported`。

### 自有 cgroup

对于不带容器的 `cgroup` 指标，执行器为每次运行创建一个子组。没有 root 权限时，只能在
被委派的组中创建子组，因此 `run` 在需要时会通过 `systemd-run --user --scope -p
Delegate=yes` 重新执行自身（命令就地运行，没有单独的服务），并把自己移入
`benchmark-runner` 子组，以遵守 cgroup v2 的“无内部进程”规则。如果没有用户 systemd
或委派，可选指标变为 `unsupported` 并附原因，必需指标则导致结论 `unsupported`。

在测量开始之前移入运行组的是 Hyperfine 进程，而不是目标：在进程创建线程之前在组之间移动
它，要花约 17 ms 等待内核，会扭曲目标时间。因此组的值包含 Hyperfine 自身的少量开销（几毫秒
CPU 和约 1 MiB 内存），覆盖范围描述中已说明这一点。

### Docker 容器

如果场景声明了 `container`，执行器在运行期间按声明的标签查找容器，并且仅当标签
`owner_pid_label` 中的创建者 PID 是当前运行的后代、且容器进程的 cgroup 路径包含其 ID 时
才接受它。带相同标签的其他容器从不被计入。就绪检查（`check`、`docker` 要求）、
`docker_version` 事实和容器轮询只使用目标环境（声明的 `env.inherit` 和 `env.set`）调用
Docker CLI：目标未获得的执行器 `DOCKER_HOST`、`DOCKER_CONTEXT` 和 `DOCKER_CONFIG` 不会把
它们引向其他守护进程。如果目标的守护进程不可达，即使执行器的守护进程有响应，系列也为
`unsupported`。数值是容器被删除前的最后一次观测：容器退出后
systemd scope 会立即被删除。对这类场景，Hyperfine 的 CPU 时间只覆盖主机上的进程，不包括
容器。

### 离线配置

只有在隔离确实生效时才运行离线配置：目标在单独的网络命名空间中运行（`unshare --user
--map-current-user --net`），探测确认只有 `lo` 可用。Docker 守护进程和 BuildKit 不在
这种隔离范围内，因此需要 `docker` 的场景的离线配置结论为 `unsupported`。没有用户命名空间
时结论同样为 `unsupported`。

### 采样器的影响

只在测量运行期间按 `sample_interval` 轮询 `/proc` 和 cgroup。对目标时间影响的测量见
[执行器页面](performance.zh-CN.md)；`--no-sampling` 完全
关闭轮询。

## 发布的内容

结果包含场景 ID 和修订、配置、记录的参数、输入的标识和规模、环境事实和 provenance：
提交、工作副本是否有改动、`source_tree_sha256` 以及执行器和 Hyperfine 的版本。提交、改动
标志和 `source_tree_sha256` 是测量前确认的快照——程序由它构建、`repo-tree` 标识由它计算
——而不是系列结束后的状态。获取快照时调用 Git 前会移除继承的、把 Git 绑定到其他仓库的变量
（`git rev-parse --local-env-vars`：`GIT_DIR`、`GIT_WORK_TREE`、`GIT_INDEX_FILE` 等），因此从 Git 钩子
启动也不会用别的仓库的提交或索引替换来源信息。清理之后会再次比对快照：如果提交或已跟踪、未被忽略的文件发生
了变化，结论就不是 `success`（`partial` 或更差）并附原因。被 Git 忽略的文件（例如
`target/`）不参与比对，因此场景的临时数据在 `cleanup` 之后不应留在未被忽略的路径中。绝对路径、
`record = false` 的参数、机密和目标的原始输出从不进入结果；包含此类数据的原因会被替换为
中性文本。

产物目录 `<产物>/runs/<id>/<UTC>-<配置>-<随机>/` 包含 `runner.log`、`summary.json`、
步骤日志（`steps/`）、Hyperfine 导出和日志（`hyperfine/`）、目标输出（`output/`）、
样本（`samples/`）以及找到的模块报告（`reports/`）。产物可能包含本地路径和程序输出：
它们是本地运行数据，不发布也不提交。

## Linux 与 WSL2

支持带 cgroup v2 和 `/proc` 的 Linux；不支持 macOS 和 Windows。WSL2 是普通的 Linux
环境：执行器把它记录为事实 `virtualization = wsl2`，但不依赖 Windows 路径、`/mnt/c` 或
Docker Desktop。自有 cgroup 需要 `systemd --user`，容器指标需要使用 cgroup v2 的
Docker，离线配置需要 util-linux 中的 `unshare`。

## 诊断

| 现象 | 原因与处理 |
|---|---|
| `hyperfine` 为 `unsupported`，代码 `5` | `PATH` 中没有 Hyperfine 2.x；安装 2.0 发布版 |
| 处理器计数器为 `unsupported` | 无法访问 `perf_event`：检查 `kernel.perf_event_paranoid` |
| `cgroup` 指标为 `unsupported` | 没有委派：检查 `systemctl --user status` 和 `systemd-run --user --scope true` |
| 离线配置为 `unsupported` | 没有用户命名空间，或场景需要 Docker |
| 代码 `7` | 本工作副本中同一模块的系列已在运行（无论 `--artifacts-dir` 为何）；等待其结束 |
| 代码 `8` | 结果违反契约：查看摘要中的 `diagnostics` 和 `result.rejected.json` |
| 运行 `failure` 且有残留进程 | 目标退出后留下了进程；它们已被清除，详情见 `runner.log` |
| 系列停止：“步骤退出后仍留有其进程” | 生命周期步骤留下了后代；它们已被终止，步骤的效果无法证明 |
| `partial`：“系列期间工作副本发生了变化” | 系列期间提交或未被忽略的文件发生了变化；provenance 描述的是测量前的快照 |
| `docker` ——“在目标环境中不可用” | 目标的 `env.inherit`/`env.set` 所指向的守护进程没有响应；不使用执行器的 Docker 变量 |

## 检查

集成测试（`tests/runner.rs`）在临时目录中构建合成目录和模块，并用 `benchmark-workload`
负载运行真实的 Hyperfine：内存和 CPU、生命周期顺序、每个步骤的失败和步骤的残留进程、超时
和 SIGINT 且无孤儿进程、离线、自有 cgroup、每次运行的模块报告、provenance 快照及其在 Git
工作副本中的漂移、使用替身 `docker` 的目标 Docker 环境、对 Hyperfine 1.x 的拒绝、禁止覆盖、
使用不同产物目录的并发系列、结果中没有私有路径、只有所有成功运行一致时才确认的模块报告事实，
以及保存到本地历史（包括禁用和失败）。没有 Hyperfine 2.x 时，它们在本地被跳过，在 CI 中失败。测试不读取
`decks/**`。该测试集属于 `cargo test --workspace --locked`。

## 块 I/O

`io_read_bytes`、`io_write_bytes`、`io_read_operations` 和 `io_write_operations`
来自自有委派 cgroup 或已发现容器的 `io.stat`。未委派 `io` 不影响 CPU/RAM。
覆盖范围、单位、writeback 归属、多层/虚拟设备限制和声明修订见
[指标契约](../../docs/performance/benchmarks.zh-CN.md#块-io)。
`benchmark-runner.calibration-io` 执行包含 `fsync` 的受控文件循环，仅请求驱逐自身干净页面。
已有文件不会被覆盖；强制终止进程可能留下文件，下一次运行会拒绝，直至所有者显式清理。
