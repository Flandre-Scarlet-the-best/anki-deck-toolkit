# 性能：`anki-repo`

[Русский](../../ru/anki-repo/performance.md) · [English](../../en/anki-repo/performance.md) · **简体中文**

> **翻译提示：** 本译文可能未及时更新，技术内容以[俄语文档](../../ru/anki-repo/performance.md)为准。欢迎提交翻译和校对改进。
CrowdAnki 导出分析短命令的性能（[命令契约](README.md)）：按目录场景
给出的最近确认快照。通用测量规则见[测量方法](../performance/README.md)，场景字段见
[基准目录](../performance/benchmarks.md)。

## 测量内容

- `anki-repo.validate` —— 对单个导出执行 `anki-repo --json validate`：读取并解析 `deck.json`、
  解析模型、结构检查。
- `anki-repo.stats` —— `anki-repo --json stats`：按牌组树进行类型化加载和汇总。

测量区间是已构建的 release 二进制从进程启动到退出；构建、预热和计算导出标识不计入。指标：
两个场景均声明 `time_wall_clock`（必需）、`time_cpu` 和 `memory_peak_resident`；
`anki-repo.validate` 还声明 `instructions`（无 `perf_event` 访问权限时为带原因的 `unsupported`）。

## 负载

导出通过参数 `export_dir` 传入，不出现在结果中：负载由树内容标识和规模（`deck_json_bytes`、
`notes`、`media_files`）表示。用户牌组不会进入公开快照：快照的参考负载是不含媒体的合成导出，
其规模写在块中。只有负载标识和规模相同的结果才能比较。

## 如何测量

```bash
cargo run -p benchmark-runner -- check anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export --runs 20
cargo run -p benchmark-history -- list --scenario anki-repo.validate
cargo run -p benchmark-history -- snapshot <ID> --page docs/ru/anki-repo/performance.md
```

## 快照

下面的块由 `benchmark-history snapshot` 根据参考环境中经过验证的
`benchmark-runner` 结果生成；如果没有块，说明尚无确认的快照。

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"warm","recorded_at":"2026-10-29T09:25:46Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：20 次运行中 20 次成功，预热 3 次。

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `media_files` | 0 |
| `export` · `notes` | 20 000 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 55.44 ms ± 2.66 ms | 54.7 ms | 51.78 ms – 63.92 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 55.34 ms ± 2.66 ms | 54.62 ms | 51.65 ms – 63.81 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.75 MiB ± 117.19 KiB | 40.78 MiB | 40.52 MiB – 40.92 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 886 426.5 ± 449.7 | 416 886 413 | 416 885 686 – 416 887 467 | 20 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"offline-warm","recorded_at":"2026-10-29T09:25:49Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · 配置 `offline-warm`

最近一次确认的测量：2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · offline · 结果 `success`：20 次运行中 20 次成功，预热 3 次。

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `media_files` | 0 |
| `export` · `notes` | 20 000 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 55.63 ms ± 1.61 ms | 55.58 ms | 53.04 ms – 59.22 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 55.53 ms ± 1.62 ms | 55.49 ms | 52.93 ms – 59.15 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.71 MiB ± 107.95 KiB | 40.72 MiB | 40.49 MiB – 40.89 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 886 564.2 ± 506.6 | 416 886 537.5 | 416 885 652 – 416 887 666 | 20 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-repo.stats","profile":"warm","recorded_at":"2026-10-29T09:25:51Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.stats` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：20 次运行中 20 次成功，预热 3 次。

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
| `export` · `deck_json_bytes` | 4.93 MiB |
| `export` · `notes` | 20 000 |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 50.69 ms ± 1.65 ms | 50.5 ms | 48.56 ms – 54.17 ms | 20 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.59 ms ± 1.65 ms | 50.42 ms | 48.44 ms – 54.09 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 38.77 MiB ± 60.73 KiB | 38.76 MiB | 38.65 MiB – 38.88 MiB | 20 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->
