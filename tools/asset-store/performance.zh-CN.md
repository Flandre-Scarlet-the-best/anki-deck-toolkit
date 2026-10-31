# 性能：`asset-store`

[Русский](performance.md) · [English](performance.en.md) · **简体中文**
<!-- i18n-source-git-blob: 99af1fa4fde5348941f03cd12fec04d0767a2f18 -->

资源存储文件核心的性能（[存储契约与负载执行器](README.zh-CN.md)）：方法、
测量边界、合成负载特征以及目录场景最近一次已确认的快照。通用测量规则见[方法论](../../docs/performance/README.zh-CN.md)，
场景字段见[基准目录](../../docs/performance/benchmarks.zh-CN.md)。

## 测量内容

`asset-store.*` 场景由 `asset-store-bench` 执行——它是真实 `AssetStore` 与 `SafeBatchRuntime`
操作的常驻执行器。每个场景测量一类操作，使未来关于 I/O、锁、重复校验或扩展性的假设能在各自的
边界上验证：

| 场景 | 操作 | 区间包含的内容 |
|---|---|---|
| `asset-store.verify-integrity` | `open_existing` 与 `verify_integrity` | 在共享锁下检查所有者、清单与 runtime 边界及每个文件是否存在；不对字节求哈希 |
| `asset-store.read-batch` | `visit_verified_variants_with_policy` | 整个集合只加一次锁、检查一次清单，读取文件并校验大小、格式与 SHA-256 |
| `asset-store.read-single` | 同一集合，每个请求调用一次 `read_verified_variants_with_policy` | 每个请求都加锁、解析并完整检查清单 |
| `asset-store.ingest` | 单个写入者以 `ingest_verified` 发布新身份 | staging、验证器检查、原子提交文件、重写清单、日志意图与关闭、`fsync` |
| `asset-store.ingest-contention` | 同样的发布量分给 `writers` 个独立句柄 | 同一进程内写入线程之间在常规 `flock` 上的串行化 |
| `asset-store.batch-reopen` | `SafeBatchRuntime::open` 与 `load` | 批处理锁、读取并检查状态、完整校验每个 blob |
| `asset-store.batch-checkpoint` | `open`、`load` 与 `save`；`change` = `unchanged` 或 `modified` | `modified`：写入新 blob、提升修订号，并原子写入状态、同步文件与目录；`unchanged`：流式比较字节，对现有文件和目录执行 `fsync`，不产生新写入 |

所有场景的区间都是已构建的 release 二进制 `asset-store-bench` 从进程启动到退出的一次运行，包括
打开存储。构建模板、恢复工作副本和检查后置条件不在区间内。指标：`time_wall_clock`（必需）、
`time_user`、`time_system`、`memory_peak_resident`、`instructions`、运行子组的 `cpu_time_cgroup`
与 `memory_peak_cgroup`；发布场景另有工作副本的 `disk_usage`，并发场景另有子组的
`cpu_utilization_mean`；若委派了 `io` 控制器，还包括子组的 `io_read_bytes`、
`io_write_bytes`、`io_read_operations` 和 `io_write_operations`，否则这些指标为
`unsupported`。`verify-integrity` 场景中的
`cpu_utilization_peak@host` 是基于 `/proc/stat` 对整台主机负载的采样观测，
不是目标负载的上限。

## 负载

负载由 `benchmarks/workloads/asset-store/*.toml` 规格定义，通过参数 `workload_spec` 传入；结果
记录文件的 SHA-256 以及 `asset-store-bench describe` 探针报告的规模。生成器是确定性的：由伪随机
灰度块组成的真实 48×48 GIF 与 PNG，批处理 blob 为 4 KiB 伪随机字节。生成器版本是规格的一部分，
因此其 SHA-256 同时描述语料内容。

| 规格 | 领域 | 语料记录 | 读取 | 每次运行发布 | 批处理记录 / blob |
|---|---|---|---|---|---|
| `kanji-small` | `kanji` | 250（200 GIF + 50 PNG） | 250 | 20 | 200 / 50 |
| `kanji-medium` | `kanji` | 1250（1000 GIF + 250 PNG） | 1250 | 50 | 1000 / 250 |
| `kanji-large` | `kanji` | 3000（2400 GIF + 600 PNG） | 1250 | 100 | 4000 / 1000 |
| `generic-medium` | `generic` | 1000 | 1000 | 50 | 1000 / 250 |

`kanji-large` 读取的记录数与 `kanji-medium` 相同：这一对把清单规模的影响与读取次数分开。
`generic-medium` 覆盖没有格式变体、按内容寻址的通用核心。

隔离与后置条件：

- 数据只有工作副本 `target/benchmark-workloads/asset-store` 中的合成语料；从不读取用户的
  `.asset-store/` 和牌组；
- 修改型场景在每次运行前从模板恢复工作副本，并对每个文件 `fsync`：起点相同且已写入磁盘；
- 每次运行之后、在区间之外，执行器检查：读取类——模板树的 SHA-256 不变；发布类——
  `verify_integrity`、记录数、新旧记录字节的 SHA-256、没有未关闭意图的干净日志以及空的 staging；
  批处理——修订号、内容与新 blob；
- 未恢复就重复发布、资源或 blob 字节损坏、模板中出现多余文件，都会失败而不是得到成功的运行
  （由执行器测试和 CI 中的声明可执行性检查固定）；
- 只有当同一可执行文件按同一规格构建、且模板树与记录的摘要一致时，模板才会在系列之间复用。

不在测量范围内：领域验证器 `KanjiImageValidator` 与 OCR（负载验证器只解码图像）、浏览器与网络
获取（Chromium、Yarxi、JPDB）、本地推理、`pitch_accent` 与 `pronunciation_audio` 领域、包含进程
启动的进程间并发。

## 如何测量

```bash
cargo run -p benchmark-runner -- check asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml
cargo run -p benchmark-runner -- run asset-store.read-batch --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 20
cargo run -p benchmark-runner -- run asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 10
cargo run -p benchmark-runner -- run asset-store.ingest-contention --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param writers=4 --runs 5
cargo run -p benchmark-runner -- run asset-store.batch-checkpoint --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param change=modified --runs 10
cargo run -p benchmark-history -- list --module asset-store
cargo run -p benchmark-history -- baseline <ID>
```

同一工作副本中同一模块的系列不会并行运行（代码 `7`）。系列运行期间不要修改工作副本中未被忽略的
文件，否则结果不是 `success`。做优化时，用同一场景、规格和配置分别测量改动前的基线与改动后的
候选，并用 `benchmark-history compare` 比较（流程见技能
[`performance-benchmarking`](../../.agents/skills/performance-benchmarking/SKILL.md)）。

## 快照

下方区块由 `benchmark-history snapshot` 根据参考环境中经过校验的 `benchmark-runner` 结果生成；
若没有区块，说明尚无已确认的快照。快照针对 `kanji-medium` 规格发布；其他规模的结果可通过
`cargo run -p benchmark-history -- list --module asset-store` 在本地历史中查看。

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-10-30T09:11:45Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：20 次运行中 20 次成功，预热 2 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `records` | 1 250 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 130.08 ms ± 5.18 ms | 127.78 ms | 124.5 ms – 141.49 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 46.63 ms ± 10.64 ms | 47 ms | 31.37 ms – 74.43 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 78.66 ms ± 12.05 ms | 77.27 ms | 50.92 ms – 103.29 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.08 MiB ± 44.21 KiB | 13.08 MiB | 13 MiB – 13.16 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 450 637 253.4 ± 33 378.5 | 450 618 622.5 | 450 617 837 – 450 693 945 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 127.23 ms ± 4.42 ms | 125.64 ms | 122.78 ms – 137.06 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.09 MiB ± 171.73 KiB | 10.17 MiB | 9.92 MiB – 10.43 MiB | 20 |
| `cpu_utilization_peak@host` | host / procfs | 1.42 核 ± 0.3 核 | 1.37 核 | 0.98 核 – 2.16 核 | 20 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-10-30T09:11:55Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：20 次运行中 20 次成功，预热 2 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 61.69 ms ± 6.11 ms | 60.09 ms | 57.47 ms – 84.47 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 15.09 ms ± 6.81 ms | 14.13 ms | 3.91 ms – 28.13 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 46.11 ms ± 8.1 ms | 45.7 ms | 32.15 ms – 66.92 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.02 MiB ± 53.68 KiB | 8.03 MiB | 7.91 MiB – 8.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 152 367 392.6 ± 7 577.4 | 152 367 272 | 152 359 850 – 152 375 530 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 63.4 ms ± 6.28 ms | 61.77 ms | 58.94 ms – 86.48 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.06 MiB ± 57.59 KiB | 5.05 MiB | 5.05 MiB – 5.3 MiB | 20 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-10-30T09:16:16Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：5 次运行中 5 次成功，预热 1 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 42.51 s ± 163.7 ms | 42.466 s | 42.362 s – 42.778 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 14.292 s ± 173.88 ms | 14.361 s | 14.057 s – 14.45 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 28.209 s ± 192.69 ms | 28.237 s | 27.904 s – 28.398 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.59 MiB ± 54.96 KiB | 8.58 MiB | 8.53 MiB – 8.67 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 131 418 249 466.6 ± 8 381 519.9 | 131 414 502 426 | 131 414 496 797 – 131 433 242 784 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 42.504 s ± 163.77 ms | 42.459 s | 42.356 s – 42.772 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.77 MiB ± 153.66 KiB | 5.7 MiB | 5.67 MiB – 6.03 MiB | 5 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-10-30T09:18:53Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：10 次运行中 10 次成功，预热 1 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 6.511 s ± 501.56 ms | 6.713 s | 5.577 s – 6.882 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.141 s ± 36.3 ms | 1.142 s | 1.096 s – 1.215 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.589 s ± 42.13 ms | 1.582 s | 1.535 s – 1.646 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.88 MiB ± 120.24 KiB | 13.91 MiB | 13.68 MiB – 14 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 115 724 980.5 ± 1 125 063.5 | 12 115 554 476 | 12 114 231 093 – 12 118 333 731 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.733 s ± 41.52 ms | 2.727 s | 2.674 s – 2.792 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 13.7 MiB ± 222.63 KiB | 13.64 MiB | 13.47 MiB – 14.23 MiB | 10 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 10 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-10-30T09:20:26Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：5 次运行中 5 次成功，预热 1 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| 参数 `writers` | `4` |
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 7.243 s ± 672.88 ms | 7.49 s | 6.362 s – 7.833 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.553 s ± 70.45 ms | 1.551 s | 1.453 s – 1.652 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.909 s ± 71.54 ms | 1.932 s | 1.827 s – 1.981 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 57.96 MiB ± 1.31 MiB | 57.8 MiB | 56.43 MiB – 59.65 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 318 162 906.2 ± 3 528 893.4 | 16 320 050 082 | 16 313 859 768 – 16 321 129 316 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.47 s ± 110.69 ms | 3.396 s | 3.394 s – 3.641 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 59.19 MiB ± 1.57 MiB | 60.02 MiB | 57.07 MiB – 60.51 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.48 核 ± 0.05 核 | 0.45 核 | 0.43 核 – 0.54 核 | 5 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-10-30T09:20:35Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：20 次运行中 20 次成功，预热 2 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 7.02 ms ± 276.4 µs | 6.97 ms | 6.69 ms – 8.01 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 4.1 ms ± 2.66 ms | 3.46 ms | 0 s – 7.23 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 2.6 ms ± 2.76 ms | 3.35 ms | 0 s – 7.85 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.92 MiB ± 86.98 KiB | 5.95 MiB | 5.79 MiB – 6.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 28 973 335 ± 312 | 28 973 367 | 28 972 603 – 28 973 906 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 8.5 ms ± 382.5 µs | 8.41 ms | 8.15 ms – 9.84 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.05 MiB ± 3.78 KiB | 3.05 MiB | 3.05 MiB – 3.06 MiB | 20 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-10-30T09:22:17Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · 配置 `warm`

最近一次确认的测量：2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · 结果 `success`：10 次运行中 10 次成功，预热 2 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| 参数 `change` | `modified` |
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 904.93 ms ± 966.88 ms | 1.149 s | 23.5 ms – 3.145 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 2.81 ms ± 2.59 ms | 3.38 ms | 0 s – 6.08 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 8.83 ms ± 6.72 ms | 7.91 ms | 3.04 ms – 26.65 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.91 MiB ± 74.61 KiB | 5.92 MiB | 5.79 MiB – 6.02 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 33 898 460.2 ± 328.2 | 33 898 486.5 | 33 898 020 – 33 899 002 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 13.86 ms ± 5.75 ms | 11.49 ms | 10.75 ms – 29.16 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.3 MiB ± 2.53 KiB | 3.3 MiB | 3.29 MiB – 3.3 MiB | 10 |

未测量：`io_read_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_bytes@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_read_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取; `io_write_operations@cgroup` — `unsupported`: 执行器 cgroup 未委派 io 控制器；CPU 和内存仍可单独读取。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## 基线观测

以下观测来自当前尚未优化的实现在快照参考环境（WSL2，虚拟磁盘上的 ext4，AMD Ryzen 7 5800X3D，
16 个逻辑 CPU）中被接受的 JSON 结果。这是首次测量：没有可比较的对象，这里不做"更快/更慢"的结论。
数字为各系列 `time_wall_clock` 的平均值；完整结果及其 ID 可通过 `cargo run -p benchmark-history -- list --module asset-store` 在本地历史中查看。

| 操作 | `kanji-small`（250） | `kanji-medium`（1250） | `kanji-large`（3000） | `generic-medium`（1000） |
|---|---|---|---|---|
| `verify-integrity` | 36 ms | 133 ms | 300 ms | 108 ms |
| `read-batch` | 18 ms（250 次读取） | 63 ms（1250） | 113 ms（1250） | 51 ms（1000） |
| `read-single` | 1.72 s（250） | 42.0 s（1250） | 100.2 s（1250） | 未测量 |
| `ingest`，每个资源 | 66 ms（20） | 113 ms（50） | 192 ms（100） | 80 ms（50） |
| `batch-reopen` | 8.0 ms（200 / 50） | 12.6 ms（1000 / 250） | 28.4 ms（4000 / 1000） | 12.2 ms |
| `batch-checkpoint` `modified`，最小值 | 21 ms | 27 ms | 61 ms | 28 ms |

供下一阶段工作参考的假设（观测，而非已证实的原因）：

- **对 N 条记录逐条读取 N 次的总成本呈二次方增长。** 一次 `read_verified_variants_with_policy` 调用在 250 / 1250 / 3000
  条记录时分别耗时 6.9 / 33.6 / 80 ms——与清单规模成正比，即使 medium 与 large 的读取次数相同。
  同样的请求用批量 API，small 快 93 倍，medium 与 large 快 670–890 倍。三分之二是系统时间
  （medium 为 42 s 中的 28 s）：可能原因是每个请求都重新打开、解析并检查整个清单并访问文件。
  读取大量资源的使用者需要批量路径；优化单次路径是另一项任务。
- **发布成本随清单规模增长。** 一次 `ingest_verified` 从 66 ms 增至 192 ms，等待时间超过 CPU 时间
  （medium 为 5.6 s 对 2.7 s CPU）：可能原因是整体重写清单以及每次发布多次 `fsync`。逐个构建
  N 个资源的语料是 O(N²)；`kanji-large` 模板约需 6 分钟。
- **写入者并发无法扩展。** 1 / 2 / 4 / 8 个写入者发布同样的量分别需要 5.54 / 6.41 / 6.23 / 6.83 s：
  `flock` 串行化发布，而每个写入者都增加 CPU 时间（2.73 → 3.61 s）及其句柄的内存（进程 RSS
  16 → 118 MB）。无论写入者多少，组的平均负载都约为 0.5 核。
- **`fsync` 长尾。** `batch-checkpoint` 最小值为 21–61 ms，但个别运行长达 1.2 s（σ 大于均值）。
  可能原因是 WSL2 虚拟磁盘的同步；比较 checkpoint 应看最小值和运行分位数，并增加运行次数。
- **系统时间占主导**：所有读取与检查操作中 `time_system` ≈ `time_cpu` 的 2/3；候选因素是每条记录
  的系统调用次数和清单规模。

尚未测量：`pitch_accent` 与 `pronunciation_audio` 领域、kanji 领域验证器、浏览器获取、进程间并发、
冷 page cache（不执行全局清空）以及非 WSL2 的原生 Linux。
