# 性能：`anki-e2e`

[Русский](anki-e2e.md) · [English](anki-e2e.en.md) · **简体中文**
<!-- i18n-source-git-blob: 856c6c540dff9c82fd4352280b4af98c3aab6845 -->

Anki Desktop E2E 测试台的性能与资源消耗：最近确认的快照和此前的手动观测（[测试台契约](../../tools/anki-e2e/README.zh-CN.md)）。
通用测量规则和来源精度见[方法说明](README.zh-CN.md)。

## 测量内容

- **阶段耗时**目前为 `unsupported`：`anki-e2e` 未发布机器可读的阶段标记，
  因此不会从人类可读的 `runner.log` 推导耗时。`report.json` 提供
  `anki-repo` 启动次数（`commands_started`）。
- **场景容器** —— 其 scope 的 cgroup v2：`memory.peak` 和 `usage_usec`。
- **镜像构建。** BuildKit 步骤在 `docker.service` cgroup 之外执行，也没有按运行划分的
  独立 cgroup。`/proc/stat` 和 `MemAvailable` 只提供整个主机的采样观测值，并非构建资源的上限。
- **主机上的 runner** —— `anki-e2e` 进程的 `VmHWM` 和 CPU。
- **磁盘** —— 工作目录的 `cache/`、`build/`、`runs/`，测试台镜像以及 BuildKit cache
  mount `anki-e2e-cargo-*`。

## 如何测量

本模块的缓存状态：

- *冷* —— 执行 `clean --images --cache` 之后：测试台镜像、Anki 归档、源码快照和 Cargo
  cache mount 已删除。守护进程的 BuildKit 层缓存以及 `ubuntu`/`rust` 基础镜像保留
  （它们是共享的，见方法说明）；
- *热* —— 源码不变时重复运行 `full`；
- *增量* —— 源码快照改变之后（例如新增一个未被忽略的文件）：只重建运行镜像。

运行：

```bash
cargo build -p anki-e2e --locked
cargo run -p anki-e2e -- clean --images --cache
cargo run -p anki-e2e -- full
```

运行期间每秒读取一次测试台容器的 cgroup 和 runner 的 `/proc`。容器在运行结束时被删除，
因此 `memory.peak` 必须采样，而不能在结束后读取。下面的 scope 路径适用于 `systemd`
cgroup 驱动；使用 `cgroupfs` 驱动时为 `/sys/fs/cgroup/docker/<ID>`。

```bash
docker ps --filter label=io.github.anki-deck-toolkit.e2e=1 --no-trunc --format '{{.ID}}'
cat /sys/fs/cgroup/system.slice/docker-<ID>.scope/memory.peak
grep usage_usec /sys/fs/cgroup/system.slice/docker-<ID>.scope/cpu.stat
grep VmHWM /proc/<PID>/status
```

运行后的磁盘：

```bash
du -sb target/anki-e2e/cache target/anki-e2e/build target/anki-e2e/runs
docker image ls --filter label=io.github.anki-deck-toolkit.e2e=1
docker system df -v
```

## 快照

下面的块由 `benchmark-history snapshot` 根据参考环境中经过验证的
`benchmark-runner` 结果生成；如果没有块，说明尚无确认的快照。

参考环境中不测量 `anki-e2e.smoke` 场景的 `offline-warm` 配置：`benchmark-runner check`
返回 `unsupported`，因为系列的网络命名空间不覆盖运行在主机网络中的 Docker 守护进程和
BuildKit，无法保证负载没有网络。`anki-e2e.full` 场景的 `cold` 配置尚未通过目录测量。

<!-- benchmark-snapshot: {"scenario":"anki-e2e.smoke","profile":"warm","recorded_at":"2026-10-29T10:36:16Z","environment":{"anki_version":"26.09.3","arch":"x86_64","container_limits":"nano_cpus=4000000000,memory=6442450944,pids_limit=4096","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","docker_version":"29.9.2","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"anki-archive":{"archive_bytes":{"value":191305871.0,"unit":"byte"}}}} -->
### `anki-e2e.smoke` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `anki-e2e` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：2 次运行中 2 次成功，预热 1 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `anki-archive` · `archive_bytes` | 182.44 MiB |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 26.148 s ± 54.16 ms | 26.148 s | 26.11 s – 26.187 s | 2 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 1.193 s ± 66.21 ms | 1.193 s | 1.147 s – 1.24 s | 2 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 309.13 MiB ± 370.52 KiB | 309.13 MiB | 308.87 MiB – 309.38 MiB | 2 |

未测量：`phase_duration@phase/scenarios` — `unsupported`: 模块未声明日志标记的机器可读格式，无法从人类可读文本推导阶段耗时。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"anki-e2e.full","profile":"warm","recorded_at":"2026-10-29T10:34:34Z","environment":{"anki_version":"26.09.3","arch":"x86_64","container_limits":"nano_cpus=4000000000,memory=6442450944,pids_limit=4096","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","docker_version":"29.9.2","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"anki-archive":{"archive_bytes":{"value":191305871.0,"unit":"byte"}}}} -->
### `anki-e2e.full` · 配置 `warm`

最近一次确认的测量：2026-10-29 UTC · `anki-e2e` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：2 次运行中 2 次成功，预热 0 次。

| 环境事实 | 值 |
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

| 负载 | 值 |
|---|---|
| `anki-archive` · `archive_bytes` | 182.44 MiB |

| 指标 | 范围 / 来源 | 均值 ± σ | 中位数 | 最小–最大 | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 235.474 s ± 18.393 s | 235.474 s | 222.468 s – 248.48 s | 2 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 1.626 s ± 253.12 ms | 1.626 s | 1.447 s – 1.805 s | 2 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 120.58 MiB ± 316.78 KiB | 120.58 MiB | 120.36 MiB – 120.8 MiB | 2 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 503.51 MiB ± 7.86 MiB | 503.51 MiB | 497.95 MiB – 509.07 MiB | 2 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 200.145 s ± 30.88 ms | 200.145 s | 200.124 s – 200.167 s | 2 |
| `cpu_utilization_peak@cgroup` | cgroup / cgroup-v2 | 1.86 核 ± 0.01 核 | 1.86 核 | 1.85 核 – 1.86 核 | 2 |
| `cpu_utilization_peak@host` | host / procfs | 2.59 核 ± 0.41 核 | 2.59 核 | 2.3 核 – 2.88 核 | 2 |
| `memory_host_peak_increase@host` | host / procfs | 472.87 MiB ± 121.82 MiB | 472.87 MiB | 386.73 MiB – 559 MiB | 2 |
| `module_counter@module-report/commands_started` | module-report / module-report | 103 ± 0 | 103 | 103 – 103 | 2 |
| `disk_usage@path/work-cache` | path / filesystem | 182.44 MiB ± 0 B | 182.44 MiB | 182.44 MiB – 182.44 MiB | 2 |

未测量：`phase_duration@phase/release` — `unsupported`: 模块未声明日志标记的机器可读格式，无法从人类可读文本推导阶段耗时; `phase_duration@phase/anki-image` — `unsupported`: 模块未声明日志标记的机器可读格式，无法从人类可读文本推导阶段耗时; `phase_duration@phase/run-image` — `unsupported`: 模块未声明日志标记的机器可读格式，无法从人类可读文本推导阶段耗时; `phase_duration@phase/scenarios` — `unsupported`: 模块未声明日志标记的机器可读格式，无法从人类可读文本推导阶段耗时。

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## 无原始 JSON 的手动观测

下面的观测是在本地历史出现之前手动记录的。它没有原始 JSON 结果：不是已确认的快照，其数值
不与 `benchmark-history` 的结果比较，也不作为基线。它在出现相同条件的首个确认快照之前保留
背景信息。温态 `full` 现在由上面的快照描述，包含主机 CPU、内存观测值及 `work-cache` 大小；
冷态和增量状态以及按阶段的分解尚未被确认快照覆盖。

### 2026-10-26 · `anki-e2e` 0.1.0 · Anki 26.09.3 · `full`

- **环境：** AMD Ryzen 7 5800X3D（8 核 16 线程）、16 GB 内存（WSL 中可见 15.6 GB）、
  内核 6.18 的 WSL2 和 Arch Linux、WSL 内的 Docker Engine 29.9.2（systemd、cgroup v2、
  overlayfs）、`rustc` 1.99.1。默认容器限制：`--cpus 4 --memory 6g`。版本按最新稳定版
  选择。工作副本没有改动；增量运行添加了一个未被忽略的文件。
- **状态：** 连续运行一次冷、两次热和一次增量 `full`。四次均为 `PASS`，每次 129 项检查。

工作副本状态说明：冷运行和两次热运行均无改动。第二次热运行结束后、增量运行开始前，
添加了一个未被忽略的临时文件。该文件在整个增量测量期间一直存在，并进入源码快照
（293 个文件，而非 292 个）。运行结束后立即删除该文件，恢复干净的工作副本。

各阶段耗时（秒）：

| 阶段 | 冷 | 热 1 | 热 2 | 增量 |
|---|---|---|---|---|
| 版本选择与归档 | 13（下载 191 MB） | 8 | 9 | 8 |
| 源码快照（292–293 个文件） | 1 | 0 | 1 | 1 |
| Anki 镜像 | 16 | 0 | 0 | 0 |
| 运行镜像（`cargo build`） | 155 | 1 | 0 | 30 |
| 场景 | 222 | 221 | 227 | 220 |
| **合计** | **406.7** | **230.0** | **236.5** | **259.3** |

场景（四次运行的范围）：

| 场景 | 耗时（秒） | `anki-repo` 启动次数 | 检查数 | 窗口数 |
|---|---|---|---|---|
| `smoke` | 11.6–12.0 | 6 | 13 | 2 |
| `addons` | 41.4–43.0 | 8 | 15 | 6 |
| `patch` | 50.4–52.6 | 30 | 32 | 0 |
| `crowdanki` | 38.8–40.6 | 28–30 | 37 | 3 |
| `process` | 23.1–24.0 | 15 | 14 | 1 |
| `errors` | 40.0–41.3 | 14 | 18 | 5 |

资源：

| 指标 | 值 | 来源 |
|---|---|---|
| 容器：内存峰值 | 499–505 MiB，限制 6 GiB | `memory.peak` |
| 容器：每次运行的 CPU | 203–212 秒 | `usage_usec` |
| 容器：平均负载 | 0.92–0.94 核，限制 4 核 | `usage_usec` / 时间 |
| 容器：1 秒内峰值 | 1.80–1.88 核 | 采样差值 |
| 场景期间的主机 | 平均约 1.1 核，242–251 核·秒（比容器多约 40 秒：dockerd、`docker exec`、WSL） | `/proc/stat` |
| 冷镜像构建（主机） | 471 核·秒，平均 2.75 核，峰值 15.5/16；主机内存最多 +2.9 GiB | `/proc/stat`、`MemAvailable` |
| 增量构建（主机） | 40 核·秒，峰值 4.6 核；主机内存最多 +0.9 GiB | `/proc/stat`、`MemAvailable` |
| 构建期间的 dockerd | 0.9–1.0 GiB，约 0.05 核（不含 BuildKit 步骤） | `docker.service` cgroup |
| runner：RSS 峰值 | 27–28 MiB | `VmHWM` |
| runner：CPU | 自身 5.3–6.7 秒 + 已等待子进程 0.7–3.5 秒 | `/proc/<pid>/stat` |
| 单元测试 `cargo test -p anki-e2e` | 64 个测试 0.63–0.65 秒（已构建） | 运行时间 |

磁盘：

| 内容 | 大小 |
|---|---|
| `cache/` 中的 Anki 归档 | 191 MB |
| `build/` 中的源码快照 | 17.7 MB |
| 一次 `full` 在 `runs/` 中的证据 | 2.0–2.1 MB，62 个 PNG |
| 运行镜像 | 2.46 GB（包含 1.89 GB 的 Anki 镜像层；不要相加） |
| cache mount `anki-e2e-cargo-target` | 冷构建后 3.41 GB，增量构建后 3.65 GB |
| cache mount `anki-e2e-cargo-registry` | 319 MB |

结论：

- 热状态下约 96% 的 `full` 时间花在场景本身。runner 准备不到 10 秒，几乎全部是版本选择；
  其中 GitHub 请求与 SHA-256 重新校验的占比没有测量。
- 热运行之间的差异约为 3%（230.0 和 236.5 秒）。
- 容器限制留有很大余量：内存峰值约 0.5 GiB/6 GiB，CPU 不超过 1.9/4 核。这只是一个环境
  中的观察，不是修改默认值的理由。
- 冷构建运行镜像是 CPU 和内存代价最高的阶段。`--cpus`/`--memory` 对它不起作用：Cargo
  占用主机所有核心（峰值 15.5/16）和最多 2.9 GiB 主机内存。这与测试台 README 一致。
- cache mount `anki-e2e-cargo-target` 在一次增量重建后增长了 0.24 GB。一次观察不足以
  得出无限增长的结论：应在下一条记录中通过多次快照变更的系列来检查。清理方法是
  `clean --cache`。
- `full` 启动 101–103 个 `anki-repo` 进程；`doctor` 和 `job` 的等待每秒最多启动一次。

未测量：没有层缓存和基础镜像的完全冷系统；Anki 归档以外的网络流量；磁盘 I/O；其他环境
（普通 Linux、Docker Desktop、aarch64）；与系统其余部分分开的 BuildKit 步骤资源。
