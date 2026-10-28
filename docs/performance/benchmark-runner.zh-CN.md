# 性能：`benchmark-runner`

[Русский](benchmark-runner.md) · [English](benchmark-runner.en.md) · **简体中文**
<!-- i18n-source-git-blob: bf095966dec3207a44a0339065e0156b617d2fcf -->

基准执行器自身的测量：最近确认的快照和此前的手动观测（[接口与语义](../../tools/benchmark-runner/README.zh-CN.md)）：
轮询 `/proc` 和 cgroup 以及移入自有 cgroup 对目标时间影响多大，以及组的数值究竟包含什么。
通用测量规则见[测量方法](README.zh-CN.md)。

## 测量内容

- **采样器的影响。** 同一短负载在交替的系列中测量：轮询主机 `/proc`、不轮询
  （`--no-sampling`）以及完全没有附加指标。比较 `time_wall_clock@command`。
- **自有 cgroup 的影响。** 同一负载带 `cgroup` 指标（Hyperfine 移入运行子组）与不带它们
  的系列对比。
- **组的基础开销。** 对已知 CPU 成本的负载和空目标测量 `cpu_time_cgroup` 和
  `memory_peak_cgroup`：组的值包含 Hyperfine 进程。

## 如何测量

负载是本包中的 `benchmark-workload`（`--busy-ms 50`：单线程主动工作 50 ms）。临时目录中的
合成目录声明目标相同、指标集合不同的场景；系列交替进行，使 CPU 频率漂移和后台负载均匀分布。

```bash
cargo build --release -p benchmark-runner --locked
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30 --no-sampling
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.plain --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.cgroup --profile warm --runs 30
```

## 快照

下面的块由 `benchmark-history snapshot` 根据参考环境中经过验证的
`benchmark-runner` 结果生成；如果没有块，说明尚无确认的快照。

<!-- benchmark-snapshot:anchor -->

## 无原始 JSON 的手动观测

下面的观测是在本地历史出现之前手动记录的。它没有原始 JSON 结果：不是已确认的快照，其数值
不与 `benchmark-history` 的结果比较，也不作为基线。它在出现相同条件的首个确认快照之前保留
背景信息。

### 2026-10-26 · benchmark-runner 0.1.0 · Hyperfine 2.0.0 · `--busy-ms 50` 系列

- 环境：AMD Ryzen 7 5800X3D，16 个逻辑 CPU；WSL2，内核
  6.18.54.1-microsoft-standard-WSL2，Arch Linux；cgroup v2，通过
  `systemd-run --user --scope` 委派；Docker 29.9.2（cgroup 驱动 `systemd`）；
  util-linux 2.42.4；`perf_event_paranoid = 2`。工作副本有改动。
- 缓存状态：热；每个系列一次预热，各 30 次测量运行，系列交替进行。
- 目标时间（`time_wall_clock@command`，系列均值）：

  | 模式 | 各系列均值范围 | 系列内离散 |
  |---|---|---|
  | 每 50 ms 轮询 `/proc` | 51.10–51.21 ms | σ ≈ 0.1 ms |
  | `--no-sampling` | 51.12–51.24 ms | σ ≈ 0.1 ms |
  | 无附加指标 | 51.15–51.27 ms | σ ≈ 0.1 ms |
  | 自有 cgroup（Hyperfine 在子组中） | 51.23 ms | σ ≈ 0.1 ms |
  | 同一对系列中不带 cgroup | 51.17 ms | σ ≈ 0.1 ms |

- 资源：

  | 指标 | 值 | 来源 |
  |---|---|---|
  | 50 ms 负载的 `cpu_time_cgroup` | ≈ 目标 `time_cpu` + 3.5 ms | 运行子组的 `cpu.stat` |
  | 空目标的 `memory_peak_cgroup` | ≈ 1.0 MiB | 运行子组的 `memory.peak` |

- 结论：轮询 `/proc` 和自有 cgroup 对目标时间的影响在噪声范围内（≈ 0.1 ms）。组的值包含
  Hyperfine：每次运行几毫秒 CPU 和约 1 MiB 内存。最初把目标本身移入子组的方案使时间增加
  20–25 ms：进程在创建线程之前在组之间移动，要花约 17 ms 等待内核。因此在测量开始前移动的是
  Hyperfine。
- 未测量：对多线程负载的影响、长于一秒的场景、Docker 容器 cgroup 轮询（500 ms 查找）以及
  WSL2 以外的环境。
