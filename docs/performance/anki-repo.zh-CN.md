# 性能：`anki-repo`

[Русский](anki-repo.md) · [English](anki-repo.en.md) · **简体中文**
<!-- i18n-source-git-blob: 5c21e3870ce6c5d268bb13513d1a3f79c04533d1 -->

CrowdAnki 导出分析短命令的性能（[命令契约](../../tools/anki-repo/README.zh-CN.md)）：按目录场景
给出的最近确认快照。通用测量规则见[测量方法](README.zh-CN.md)，场景字段见
[基准目录](benchmarks.zh-CN.md)。

## 测量内容

- `anki-repo.validate` —— 对单个导出执行 `anki-repo --json validate`：读取并解析 `deck.json`、
  解析模型、结构检查。
- `anki-repo.stats` —— `anki-repo --json stats`：按牌组树进行类型化加载和汇总。

测量区间是已构建的 release 二进制从进程启动到退出；构建、预热和计算导出标识不计入。指标：
`time_wall_clock`（必需）、`time_cpu`、`memory_peak_resident` 和 `instructions`（无
`perf_event` 访问权限时为带原因的 `unavailable`）。

## 负载

导出通过参数 `export_dir` 传入，不出现在结果中：负载由树内容标识和规模（`deck_json_bytes`、
`notes`、`media_files`）表示。用户牌组不会进入公开快照：快照的参考负载是不含媒体的合成导出，
其规模写在块中。只有负载标识和规模相同的结果才能比较。

## 如何测量

```bash
cargo run -p benchmark-runner -- check anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export --runs 20
cargo run -p benchmark-history -- list --scenario anki-repo.validate
cargo run -p benchmark-history -- snapshot <ID> --page docs/performance/anki-repo.md
```

## 快照

下面的块由 `benchmark-history snapshot` 根据参考环境中经过验证的
`benchmark-runner` 结果生成；如果没有块，说明尚无确认的快照。

<!-- benchmark-snapshot: {"scenario":"anki-repo.validate","profile":"warm","recorded_at":"2026-10-27T19:23:42Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"export":{"deck_json_bytes":{"value":5167663.0,"unit":"byte"},"media_files":{"value":0.0,"unit":"count"},"notes":{"value":20000.0,"unit":"count"}}}} -->
### `anki-repo.validate` · 配置 `warm`

最近一次确认的测量：2026-10-27 UTC · `anki-repo` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · 结果 `success`：10 次运行中 10 次成功，预热 3 次。

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
| `time_wall_clock@command` | command / hyperfine | 54.41 ms ± 1.26 ms | 54.53 ms | 52.02 ms – 56.4 ms | 10 |
| `time_cpu@process-tree-waited` | process-tree-waited / hyperfine | 54.22 ms ± 1.2 ms | 54.44 ms | 51.91 ms – 56.32 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 40.63 MiB ± 101.86 KiB | 40.67 MiB | 40.49 MiB – 40.78 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 416 918 918.6 ± 321.2 | 416 918 810 | 416 918 418 – 416 919 483 | 10 |

★ 参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->
