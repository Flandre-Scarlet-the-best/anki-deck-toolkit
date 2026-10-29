# 性能：`benchmark-runner`

[Русский](benchmark-runner.md) · [English](benchmark-runner.en.md) · **简体中文**
<!-- i18n-source-git-blob: 8e7d76fbfdb32c0a7a55789d8c99c9b884fcb40d -->

基准执行器自身的测量（[接口与语义](../../tools/benchmark-runner/README.zh-CN.md)）：它对已知开销的
负载测得有多准确、每个指标究竟包含什么，以及轮询 `/proc` 和运行自有 cgroup 的代价。通用测量规则见
[方法论](README.zh-CN.md)。

## 测量内容

目录中的 `benchmark-runner.calibration-*` 场景运行本包的 `benchmark-workload`——开销事先已知的合成
负载。它们不描述 `asset-store`、`anki-repo` 或 Anki 的性能。

| 场景 | 负载 | 预期 |
|---|---|---|
| `calibration-noop` | 进程启动与退出 | 时间与内存下限，组值中的 Hyperfine 开销 |
| `calibration-cpu` | 单线程，50 ms 自身 CPU 时间 | `time_cpu` ≈ 50 ms 加进程启动 |
| `calibration-cpu-plain` | 同一目标，仅 Hyperfine 指标 | 比较可显示额外监控的代价 |
| `calibration-parallel` | 四个线程各 1000 ms | `time_cpu` ≈ 4 s，时间 ≈ 1 s，组负载 ≈ 4 核 |
| `calibration-memory` | 写入 256 MiB 块的每一页并保持 200 ms | `memory_peak_resident` ≈ 256 MiB 加进程基础 |

## 如何测量

轮询代价通过同一目标的成对系列估计：开启 `/proc` 和 cgroup 轮询，以及 `--no-sampling`；自有
cgroup 的代价通过比较 `calibration-cpu` 与 `calibration-cpu-plain` 得出。系列交替进行，使 CPU
频率漂移和背景负载均匀分布。契约不比较不同场景的结果：这种比较是方法上的观测，而不是
`compare` 的结论。

```bash
cargo run -p benchmark-runner -- check benchmark-runner.calibration-cpu --profile warm
cargo run -p benchmark-runner -- run benchmark-runner.calibration-noop --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu --profile warm --runs 30 --no-sampling
cargo run -p benchmark-runner -- run benchmark-runner.calibration-cpu-plain --profile warm --runs 30
cargo run -p benchmark-runner -- run benchmark-runner.calibration-parallel --profile warm --runs 10
cargo run -p benchmark-runner -- run benchmark-runner.calibration-parallel --profile warm --runs 10 --no-sampling
cargo run -p benchmark-runner -- run benchmark-runner.calibration-memory --profile warm --runs 10
cargo run -p benchmark-history -- list --module benchmark-runner
```

## 快照

下方区块由 `benchmark-history snapshot` 根据参考环境中经过校验的 `benchmark-runner` 结果生成；
若没有区块，说明尚无已确认的快照。

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-noop","profile":"warm","recorded_at":"2026-10-29T09:23:01Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-noop` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：30 次运行中 30 次成功，预热 3 次。

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

该场景未声明输入。

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 809.2 µs ± 74.5 µs | 794.4 µs | 710.3 µs – 963.3 µs | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 734.3 µs ± 73.5 µs | 703 µs | 646 µs – 895 µs | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 0 s ± 0 s | 0 s | 0 s – 0 s | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 734.3 µs ± 73.5 µs | 703 µs | 646 µs – 895 µs | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.23 MiB ± 42.86 KiB | 3.24 MiB | 3.12 MiB – 3.27 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 342 173.6 ± 339.9 | 342 133.5 | 341 560 – 342 862 | 30 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.55 ms ± 344.1 µs | 2.62 ms | 1.87 ms – 3.2 ms | 30 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 777.33 KiB ± 46.79 KiB | 768 KiB | 768 KiB – 1 MiB | 30 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.16 核 ± 0.02 核 | 0.17 核 | 0.12 核 – 0.2 核 | 30 |
| `memory_host_peak_increase@host` | host / procfs | 0 B ± 0 B | 0 B | 0 B – 0 B | 30 |

未测量：`cpu_utilization_peak@host` — `unavailable`: прогон короче интервала сэмплирования 0.05 с: ни одного полного окна。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-cpu","profile":"warm","recorded_at":"2026-10-29T09:23:04Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-cpu` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：30 次运行中 30 次成功，预热 3 次。

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

该场景未声明输入。

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 51.14 ms ± 74.2 µs | 51.13 ms | 51 ms – 51.27 ms | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 36.78 ms ± 7.74 ms | 36.84 ms | 21.25 ms – 50.88 ms | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 14.21 ms ± 7.73 ms | 14.21 ms | 0 s – 29.74 ms | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.99 ms ± 67 µs | 50.98 ms | 50.88 ms – 51.18 ms | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.22 MiB ± 49.01 KiB | 3.22 MiB | 3.12 MiB – 3.3 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 297 994 746 ± 4 162 825.9 | 298 483 162.5 | 281 630 938 – 303 915 923 | 30 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 52.41 ms ± 156.9 µs | 52.4 ms | 52.09 ms – 52.78 ms | 30 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 1.03 MiB ± 88.3 KiB | 1020 KiB | 1020 KiB – 1.25 MiB | 30 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.62 核 ± 0.06 核 | 0.61 核 | 0.51 核 – 0.68 核 | 30 |
| `cpu_utilization_peak@host` | host / procfs | 1 核 ± 0.45 核 | 0.8 核 | 0.55 核 – 2.29 核 | 30 |
| `memory_host_peak_increase@host` | host / procfs | 107.6 KiB ± 404.87 KiB | 0 B | 0 B – 2.16 MiB | 30 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-cpu-plain","profile":"warm","recorded_at":"2026-10-29T09:23:09Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-cpu-plain` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：30 次运行中 30 次成功，预热 3 次。

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

该场景未声明输入。

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 51.13 ms ± 95.4 µs | 51.1 ms | 50.97 ms – 51.36 ms | 30 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 35.63 ms ± 6.65 ms | 35.3 ms | 23.5 ms – 47.03 ms | 30 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 15.35 ms ± 6.65 ms | 15.68 ms | 3.92 ms – 27.54 ms | 30 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 50.98 ms ± 86.4 µs | 50.96 ms | 50.85 ms – 51.16 ms | 30 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.23 MiB ± 42.78 KiB | 3.24 MiB | 3.12 MiB – 3.27 MiB | 30 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 297 858 672.8 ± 4 313 498.9 | 299 127 087.5 | 282 486 820 – 303 040 058 | 30 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-parallel","profile":"warm","recorded_at":"2026-10-29T09:23:30Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-parallel` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：10 次运行中 10 次成功，预热 1 次。

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

该场景未声明输入。

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 1.002 s ± 128.9 µs | 1.002 s | 1.002 s – 1.002 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 2.797 s ± 62.96 ms | 2.791 s | 2.713 s – 2.905 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.205 s ± 62.91 ms | 1.211 s | 1.097 s – 1.289 s | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 4.002 s ± 136.4 µs | 4.002 s | 4.002 s – 4.002 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 3.2 MiB ± 60.25 KiB | 3.21 MiB | 3.12 MiB – 3.27 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 23 045 047 645.4 ± 216 056 843.4 | 23 085 225 076.5 | 22 692 549 917 – 23 309 911 676 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 4.004 s ± 239.4 µs | 4.004 s | 4.003 s – 4.004 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 1.97 MiB ± 80.95 KiB | 2 MiB | 1.75 MiB – 2 MiB | 10 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 3.87 核 ± 0.03 核 | 3.87 核 | 3.79 核 – 3.91 核 | 10 |
| `cpu_utilization_peak@host` | host / procfs | 6.01 核 ± 1 核 | 5.89 核 | 4.52 核 – 8.1 核 | 10 |
| `memory_host_peak_increase@host` | host / procfs | 84.8 KiB ± 111.79 KiB | 0 B | 0 B – 244 KiB | 10 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"benchmark-runner.calibration-memory","profile":"warm","recorded_at":"2026-10-29T09:24:09Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{}} -->
### `benchmark-runner.calibration-memory` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `benchmark-runner` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：10 次运行中 10 次成功，预热 1 次。

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

该场景未声明输入。

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 325.77 ms ± 2.04 ms | 325.39 ms | 322.97 ms – 329.49 ms | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 11.74 ms ± 6.23 ms | 12.01 ms | 0 s – 20.42 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 113.82 ms ± 6.79 ms | 113.81 ms | 105.86 ms – 129.17 ms | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 125.56 ms ± 2.04 ms | 125.2 ms | 122.76 ms – 129.17 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 258.18 MiB ± 92.11 KiB | 258.19 MiB | 258.07 MiB – 258.31 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 869 123.4 ± 363.9 | 869 130 | 868 555 – 869 797 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 126.99 ms ± 2 ms | 126.63 ms | 124.28 ms – 130.64 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 257.25 MiB ± 127.31 KiB | 257.25 MiB | 257.11 MiB – 257.5 MiB | 10 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.35 核 ± 0.01 核 | 0.35 核 | 0.35 核 – 0.36 核 | 10 |
| `cpu_utilization_peak@host` | host / procfs | 1.55 核 ± 0.71 核 | 1.18 核 | 0.98 核 – 2.94 核 | 10 |
| `memory_host_peak_increase@host` | host / procfs | 212.91 MiB ± 52.52 MiB | 233.39 MiB | 93.84 MiB – 266.87 MiB | 10 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## 校准结论

以下结论来自这些场景在快照参考环境中被接受的 JSON 结果（WSL2，AMD Ryzen 7 5800X3D，16 个逻辑 CPU，
`perf_event_paranoid = 2`，通过 `systemd-run --user` 委派 cgroup）：50 ms 负载为交替的 30 次运行
系列，并行与内存负载为 10 次。完整结果及其 ID 在本地历史和 PR 描述中。

- **CPU 时间准确。** 50 ms 负载的 `time_cpu` 为 50.91–51.03 ms：比目标多 0.9–1.0 ms，与启动空进程
  （0.73 ms）及创建工作线程的开销相当。四个线程各 1 s 得到 4.0018–4.0019 s，`time_wall_clock` 为 1.0017 s。
- **组值包含 Hyperfine。** `cpu_time_cgroup` 比目标的 `time_cpu` 多约 1.4 ms（52.27–52.48 ms 对
  50.96–51.03 ms）；空目标时组耗费 2.5 ms，而目标本身仅 0.73 ms。
- **组的平均负载**是组 CPU 时间除以其存活时间，即整个 Hyperfine 调用。对于短目标，组的存活时间
  明显长于目标，因此该值是"目标加 Hyperfine"这一组合的属性：空目标 0.16 核，50 ms 单线程
  0.62–0.67，四线程各 1 s 为 3.85–3.88。修复前分母是目标自身的时间，空的单线程目标得到 3.35 核。
- **常驻内存准确，范围不同。** 256 MiB 块（268.4 MB）得到 `memory_peak_resident` 270.6–270.9 MB。
  同一运行的 `memory_peak_cgroup` 低约 1 MB：已在 page cache 中的可执行文件页面计入另一个组。空目标
  RSS 为 3.4 MB，组峰值为 0.8 MB——这是不同的量，而非误差。
- **`memory_host_peak_increase` 不宜用于下结论。** 对同一 256 MiB 块，该指标给出 98–280 MB
  （223 ± 55 MB）：已结束进程的内存延迟归还系统，因此下一次运行的基准点偏移，而同时发生的释放会
  抬高 `MemAvailable`。保持 1.5 s 并频繁轮询时下降为 242–256 MiB。该指标是主机观测值而非界限，
  不能据此比较系列。
- **轮询与 cgroup 的代价在噪声范围内。** 50 ms 目标在交替系列中的平均 `time_wall_clock`：开启轮询
  51.14 与 51.16 ms，`--no-sampling` 51.18 与 51.12 ms，仅 Hyperfine 指标 51.13 与 51.06 ms，
  σ 为 0.06–0.12 ms。四线程各 1 s：开启轮询 1.00172 与 1.00176 s，关闭 1.00175 与 1.00183 s。
  差异小于系列离散度；短运行上影响低于 0.2 %，一秒运行上未检出。
- **分辨率。** 以 0.05 s 间隔轮询 `/proc` 时，短于间隔的目标没有任何窗口：空目标的
  `cpu_utilization_peak@host` 为带原因的 `unavailable`，而不是零。在 `perf_event_paranoid = 2`
  下 `instructions` 计数器可用，相对离散度稳定在 10⁻⁴。

未测量：非 WSL2 环境、多进程负载、Docker 容器的 cgroup（500 ms 查找）以及轮询对长于数秒目标的影响。

先前没有原始 JSON 的手动观测（2026-10-26，合成目录 `overhead.*`）已被上述结论取代。其中保留一项
设计决定：在测量开始前移入运行子组的是 Hyperfine 而不是目标——在进程创建线程之前在组间迁移进程
约需 17 ms 的内核等待，会使目标时间增加 20–25 ms。
