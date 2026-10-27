# Performance: `benchmark-runner`

[Русский](benchmark-runner.md) · **English** · [简体中文](benchmark-runner.zh-CN.md)
<!-- i18n-source-git-blob: 9f56b6a90e46a60c11e6306d6adf7b1da5e126d3 -->

Log of measurements of the benchmark runner itself
([interface and semantics](../../tools/benchmark-runner/README.en.md)): how
much polling `/proc` and cgroup and moving into an own cgroup affect the target
time, and what exactly the group values contain. General measurement rules are
in the [methodology](README.en.md).

## What is measured

- **Sampler impact.** The same short load is measured in alternating series
  with host `/proc` polling, without polling (`--no-sampling`) and with no
  additional metrics at all. `time_wall_clock@command` is compared.
- **Own cgroup impact.** The same load with `cgroup` metrics (Hyperfine moved
  into the run subgroup) versus a series without them.
- **Baseline group overhead.** `cpu_time_cgroup` and `memory_peak_cgroup` for a
  load with a known CPU cost and for an empty target: group values include the
  Hyperfine process.

## How to measure

The load is `benchmark-workload` from this package (`--busy-ms 50`: 50 ms of
active work on one thread). A synthetic catalog in a temporary directory
declares scenarios with the same target and different metric sets; series
alternate so that CPU frequency drift and background load are spread evenly.

```bash
cargo build --release -p benchmark-runner --locked
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.host --profile warm --runs 30 --no-sampling
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.plain --profile warm --runs 30
cargo run --release -p benchmark-runner -- --catalog /path/to/overhead/catalog run overhead.cgroup --profile warm --runs 30
```

## Log

### 2026-10-26 · benchmark-runner 0.1.0 · Hyperfine 2.0.0 · `--busy-ms 50` series

- Environment: AMD Ryzen 7 5800X3D, 16 logical CPUs; WSL2, kernel
  6.18.54.1-microsoft-standard-WSL2, Arch Linux; cgroup v2, delegation through
  `systemd-run --user --scope`; Docker 29.9.2 (`systemd` cgroup driver);
  util-linux 2.42.4; `perf_event_paranoid = 2`. Working copy with changes.
- Cache states: warm; one warmup per series, 30 measured runs each, series
  alternated.
- Target time (`time_wall_clock@command`, series mean):

  | Mode | Range of series means | Spread within a series |
  |---|---|---|
  | `/proc` polling every 50 ms | 51.10–51.21 ms | σ ≈ 0.1 ms |
  | `--no-sampling` | 51.12–51.24 ms | σ ≈ 0.1 ms |
  | no additional metrics | 51.15–51.27 ms | σ ≈ 0.1 ms |
  | own cgroup (Hyperfine in the subgroup) | 51.23 ms | σ ≈ 0.1 ms |
  | no cgroup in the same pair of series | 51.17 ms | σ ≈ 0.1 ms |

- Resources:

  | Metric | Value | Source |
  |---|---|---|
  | `cpu_time_cgroup` of a 50 ms load | ≈ target `time_cpu` + 3.5 ms | `cpu.stat` of the run subgroup |
  | `memory_peak_cgroup` of an empty target | ≈ 1.0 MiB | `memory.peak` of the run subgroup |

- Conclusions: the impact of `/proc` polling and an own cgroup on target time
  is within noise (≈ 0.1 ms). Group values include Hyperfine: a few
  milliseconds of CPU and about 1 MiB of memory per run. The first design,
  which moved the target itself into the subgroup, added 20–25 ms to the time:
  moving a process between groups before it creates threads costs about 17 ms
  of kernel waiting. That is why Hyperfine is moved before measurement starts.
- Not measured: impact on a multithreaded load, scenarios longer than a second,
  Docker container cgroup polling (500 ms lookup) and environments other than
  WSL2.
