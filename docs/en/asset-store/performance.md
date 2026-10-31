# Performance: `asset-store`

[Русский](../../ru/asset-store/performance.md) · **English** · [简体中文](../../zh-CN/asset-store/performance.md)

> **Translation notice:** This translation may be outdated. The [Russian documentation](../../ru/asset-store/performance.md) is authoritative. Corrections, proofreading and translation contributions are welcome.

Performance of the asset store's file core
([store contract and workload executor](README.md)):
method, measurement boundary, synthetic workload characteristics and the last
confirmed snapshots of the catalog scenarios. General measurement rules are in
the [methodology](../performance/README.md), scenario fields in the
[benchmark catalog](../performance/benchmarks.md).

## What is measured

The `asset-store.*` scenarios are executed by `asset-store-bench`, a permanent
executor of real `AssetStore` and `SafeBatchRuntime` operations. Each scenario
measures one class of operations, so a future hypothesis about I/O, locks,
re-validation or scaling is tested at its own boundary:

| Scenario | Operation | What the interval contains |
|---|---|---|
| `asset-store.verify-integrity` | `open_existing` and `verify_integrity` | owner, manifest and runtime-boundary checks, presence of every file under a shared lock; bytes are not hashed |
| `asset-store.read-batch` | `visit_verified_variants_with_policy` | one lock and one manifest check for the whole set, reading files with size, format and SHA-256 checks |
| `asset-store.read-single` | the same set, one `read_verified_variants_with_policy` call each | lock, parsing and full manifest check per request |
| `asset-store.ingest` | `ingest_verified` of new identities by one writer | staging, validator check, atomic file commit, manifest rewrite, journal intent and close, `fsync` |
| `asset-store.ingest-contention` | the same publication volume across `writers` independent handles | serialization on the regular `flock` between writer threads of one process |
| `asset-store.batch-reopen` | `SafeBatchRuntime::open` and `load` | batch lock, state read and check, full verification of every blob |
| `asset-store.batch-checkpoint` | `open`, `load` and `save`; `change` = `unchanged` or `modified` | `modified` — a new blob, revision bump and atomic state write with file and directory sync; `unchanged` — streamed byte comparison and `fsync` of the existing file and directory without a new write |

In every scenario the interval is one run of the built release binary
`asset-store-bench` from process start to exit, including opening the store.
Building the template, restoring the working copy and checking postconditions
are outside the interval. Metrics: `time_wall_clock` (required), `time_user`,
`time_system`, `memory_peak_resident`, `instructions`, the run subgroup's
`cpu_time_cgroup` and `memory_peak_cgroup`, `disk_usage` of the working copy for
publication, the subgroup's `cpu_utilization_mean` for contention, and the
subgroup's `io_read_bytes`, `io_write_bytes`, `io_read_operations` and
`io_write_operations` when the `io` controller is delegated; otherwise these
metrics are `unsupported`. In
`verify-integrity`, `cpu_utilization_peak@host` is a sampled observation of
whole-host load from `/proc/stat`, not an upper bound on the target's load.

## Workload

The workload is a spec in `benchmarks/workloads/asset-store/*.toml`, passed via
the `workload_spec` parameter; the result records the file's SHA-256 and the
size reported by the `asset-store-bench describe` probe. The generator is
deterministic: real 48×48 GIF and PNG images from blocks of pseudo-random
shades, batch blobs of 4 KiB pseudo-random bytes. The generator version is part
of the spec, so its SHA-256 also describes the corpus content.

| Spec | Domain | Corpus records | Reads | Publications per run | Batch entries / blobs |
|---|---|---|---|---|---|
| `kanji-small` | `kanji` | 250 (200 GIF + 50 PNG) | 250 | 20 | 200 / 50 |
| `kanji-medium` | `kanji` | 1250 (1000 GIF + 250 PNG) | 1250 | 50 | 1000 / 250 |
| `kanji-large` | `kanji` | 3000 (2400 GIF + 600 PNG) | 1250 | 100 | 4000 / 1000 |
| `generic-medium` | `generic` | 1000 | 1000 | 50 | 1000 / 250 |

`kanji-large` reads as many records as `kanji-medium`: the pair separates the
effect of manifest size from the number of reads. `generic-medium` exercises the
common core without format variants and with content addressing.

Isolation and postconditions:

- data is only the synthetic corpus in the working copy's
  `target/benchmark-workloads/asset-store`; the user's `.asset-store/` and decks
  are never read;
- mutating scenarios restore the working copy from the template before every
  run, with `fsync` of every file: the start is identical and already on disk;
- after every run, outside the interval, the executor checks: for reads, the
  template tree's SHA-256 is unchanged; for publication, `verify_integrity`, the
  record count, old and new records' bytes by SHA-256, a clean journal without
  open intents and an empty staging area; for the batch, revision, contents and
  the new blob;
- repeated publication without a restore, corrupted asset or blob bytes, or a
  stray file in the template fail instead of producing a successful run (pinned
  by the executor tests and the declaration-runnability check in CI);
- a template is reused between series only if the same executable built it from
  the same spec and its tree matches the recorded digest.

Not part of the measurement: the domain `KanjiImageValidator` and OCR (the
workload validator only decodes the image), browser and network acquisition
(Chromium, Yarxi, JPDB), local inference, the `pitch_accent` and
`pronunciation_audio` domains, inter-process contention with process start-up.

## How to measure

```bash
cargo run -p benchmark-runner -- check asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml
cargo run -p benchmark-runner -- run asset-store.read-batch --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 20
cargo run -p benchmark-runner -- run asset-store.ingest --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --runs 10
cargo run -p benchmark-runner -- run asset-store.ingest-contention --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param writers=4 --runs 5
cargo run -p benchmark-runner -- run asset-store.batch-checkpoint --profile warm --param workload_spec=benchmarks/workloads/asset-store/kanji-medium.toml --param change=modified --runs 10
cargo run -p benchmark-history -- list --module asset-store
cargo run -p benchmark-history -- baseline <ID>
```

Series of one module do not run in parallel in a working copy (code `7`). While
a series runs, do not change non-ignored files of the working copy, otherwise
the outcome is not `success`. For an optimization, measure the baseline before
the change and the candidate after it with the same scenario, spec and profile
and compare them with `benchmark-history compare` (procedure: skill
[`performance-benchmarking`](../../../.agents/skills/performance-benchmarking/SKILL.md)).

## Snapshots

The blocks below are generated by `benchmark-history snapshot` from verified
`benchmark-runner` results in the reference environment; if there are no blocks,
there has been no confirmed snapshot yet. Snapshots are published for the
`kanji-medium` spec; results of the other sizes are available in local history
with `cargo run -p benchmark-history -- list --module asset-store`.

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-10-30T09:11:45Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `records` | 1 250 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 130.08 ms ± 5.18 ms | 127.78 ms | 124.5 ms – 141.49 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 46.63 ms ± 10.64 ms | 47 ms | 31.37 ms – 74.43 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 78.66 ms ± 12.05 ms | 77.27 ms | 50.92 ms – 103.29 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.08 MiB ± 44.21 KiB | 13.08 MiB | 13 MiB – 13.16 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 450 637 253.4 ± 33 378.5 | 450 618 622.5 | 450 617 837 – 450 693 945 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 127.23 ms ± 4.42 ms | 125.64 ms | 122.78 ms – 137.06 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.09 MiB ± 171.73 KiB | 10.17 MiB | 9.92 MiB – 10.43 MiB | 20 |
| `cpu_utilization_peak@host` | host / procfs | 1.42 cores ± 0.3 cores | 1.37 cores | 0.98 cores – 2.16 cores | 20 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-10-30T09:11:55Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 61.69 ms ± 6.11 ms | 60.09 ms | 57.47 ms – 84.47 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 15.09 ms ± 6.81 ms | 14.13 ms | 3.91 ms – 28.13 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 46.11 ms ± 8.1 ms | 45.7 ms | 32.15 ms – 66.92 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.02 MiB ± 53.68 KiB | 8.03 MiB | 7.91 MiB – 8.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 152 367 392.6 ± 7 577.4 | 152 367 272 | 152 359 850 – 152 375 530 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 63.4 ms ± 6.28 ms | 61.77 ms | 58.94 ms – 86.48 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.06 MiB ± 57.59 KiB | 5.05 MiB | 5.05 MiB – 5.3 MiB | 20 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-10-30T09:16:16Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 5 of 5 runs successful, 1 warmup run.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| `workload` · `read_bytes` | 1.16 MiB |
| `workload` · `reads` | 1 250 |
| `workload` · `records` | 1 250 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 42.51 s ± 163.7 ms | 42.466 s | 42.362 s – 42.778 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 14.292 s ± 173.88 ms | 14.361 s | 14.057 s – 14.45 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 28.209 s ± 192.69 ms | 28.237 s | 27.904 s – 28.398 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.59 MiB ± 54.96 KiB | 8.58 MiB | 8.53 MiB – 8.67 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 131 418 249 466.6 ± 8 381 519.9 | 131 414 502 426 | 131 414 496 797 – 131 433 242 784 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 42.504 s ± 163.77 ms | 42.459 s | 42.356 s – 42.772 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.77 MiB ± 153.66 KiB | 5.7 MiB | 5.67 MiB – 6.03 MiB | 5 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-10-30T09:18:53Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 10 of 10 runs successful, 1 warmup run.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 6.511 s ± 501.56 ms | 6.713 s | 5.577 s – 6.882 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.141 s ± 36.3 ms | 1.142 s | 1.096 s – 1.215 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.589 s ± 42.13 ms | 1.582 s | 1.535 s – 1.646 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.88 MiB ± 120.24 KiB | 13.91 MiB | 13.68 MiB – 14 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 115 724 980.5 ± 1 125 063.5 | 12 115 554 476 | 12 114 231 093 – 12 118 333 731 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.733 s ± 41.52 ms | 2.727 s | 2.674 s – 2.792 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 13.7 MiB ± 222.63 KiB | 13.64 MiB | 13.47 MiB – 14.23 MiB | 10 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 10 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-10-30T09:20:26Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 5 of 5 runs successful, 1 warmup run.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| parameter `writers` | `4` |
| `workload` · `corpus_bytes` | 1.16 MiB |
| `workload` · `ingest` | 50 |
| `workload` · `ingest_bytes` | 52.62 KiB |
| `workload` · `records` | 1 250 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 7.243 s ± 672.88 ms | 7.49 s | 6.362 s – 7.833 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.553 s ± 70.45 ms | 1.551 s | 1.453 s – 1.652 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.909 s ± 71.54 ms | 1.932 s | 1.827 s – 1.981 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 57.96 MiB ± 1.31 MiB | 57.8 MiB | 56.43 MiB – 59.65 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 318 162 906.2 ± 3 528 893.4 | 16 320 050 082 | 16 313 859 768 – 16 321 129 316 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.47 s ± 110.69 ms | 3.396 s | 3.394 s – 3.641 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 59.19 MiB ± 1.57 MiB | 60.02 MiB | 57.07 MiB – 60.51 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.48 cores ± 0.05 cores | 0.45 cores | 0.43 cores – 0.54 cores | 5 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-10-30T09:20:35Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 7.02 ms ± 276.4 µs | 6.97 ms | 6.69 ms – 8.01 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 4.1 ms ± 2.66 ms | 3.46 ms | 0 s – 7.23 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 2.6 ms ± 2.76 ms | 3.35 ms | 0 s – 7.85 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.92 MiB ± 86.98 KiB | 5.95 MiB | 5.79 MiB – 6.08 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 28 973 335 ± 312 | 28 973 367 | 28 972 603 – 28 973 906 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 8.5 ms ± 382.5 µs | 8.41 ms | 8.15 ms – 9.84 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.05 MiB ± 3.78 KiB | 3.05 MiB | 3.05 MiB – 3.06 MiB | 20 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-10-30T09:22:17Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · profile `warm`

Last confirmed measurement: 2026-10-30 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r2 · warm · online · outcome `success`: 10 of 10 runs successful, 2 warmup runs.

| Environment fact | Value |
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

| Workload | Value |
|---|---|
| parameter `change` | `modified` |
| `workload` · `batch_blob_bytes` | 1000 KiB |
| `workload` · `batch_blobs` | 250 |
| `workload` · `batch_entries` | 1 000 |

| Metric | Scope / source | Mean ± σ | Median | Min–max | n |
|---|---|---|---|---|---|
| `time_wall_clock@command` | command / hyperfine | 904.93 ms ± 966.88 ms | 1.149 s | 23.5 ms – 3.145 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 2.81 ms ± 2.59 ms | 3.38 ms | 0 s – 6.08 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 8.83 ms ± 6.72 ms | 7.91 ms | 3.04 ms – 26.65 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 5.91 MiB ± 74.61 KiB | 5.92 MiB | 5.79 MiB – 6.02 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 33 898 460.2 ± 328.2 | 33 898 486.5 | 33 898 020 – 33 899 002 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 13.86 ms ± 5.75 ms | 11.49 ms | 10.75 ms – 29.16 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 3.3 MiB ± 2.53 KiB | 3.3 MiB | 3.29 MiB – 3.3 MiB | 10 |

Not measured: `io_read_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_bytes@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_read_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately; `io_write_operations@cgroup` — `unsupported`: the io controller is not delegated to the runner's cgroup; CPU and memory are available separately.

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Baseline observations

The observations below come from accepted JSON results of the current,
not yet optimized implementation in the snapshots' reference environment (WSL2
with ext4 on a virtual disk, AMD Ryzen 7 5800X3D, 16 logical CPUs). This is a
first measurement: there is nothing to compare with and no "faster/slower"
conclusions here. Numbers are series means of `time_wall_clock`; full results
and their IDs are available in local history with
`cargo run -p benchmark-history -- list --module asset-store`.

| Operation | `kanji-small` (250) | `kanji-medium` (1250) | `kanji-large` (3000) | `generic-medium` (1000) |
|---|---|---|---|---|
| `verify-integrity` | 36 ms | 133 ms | 300 ms | 108 ms |
| `read-batch` | 18 ms (250 reads) | 63 ms (1250) | 113 ms (1250) | 51 ms (1000) |
| `read-single` | 1.72 s (250) | 42.0 s (1250) | 100.2 s (1250) | not measured |
| `ingest`, per asset | 66 ms (20) | 113 ms (50) | 192 ms (100) | 80 ms (50) |
| `batch-reopen` | 8.0 ms (200 / 50) | 12.6 ms (1000 / 250) | 28.4 ms (4000 / 1000) | 12.2 ms |
| `batch-checkpoint` `modified`, minimum | 21 ms | 27 ms | 61 ms | 28 ms |

Hypotheses for the next line of work (observations, not proven causes):

- **A series of N single reads over N records has quadratic total cost.** One `read_verified_variants_with_policy` call
  costs 6.9 / 33.6 / 80 ms at 250 / 1250 / 3000 records — proportional to the
  manifest size, even with the same number of reads for medium and large. The
  batch API on the same requests is 93 times faster for small and 670–890 times
  for medium and large. Two thirds of the time is system time (28 of 42 s for
  medium): the likely cause is reopening, parsing and checking the whole
  manifest with file accesses on every request. Consumers that read many assets
  need the batch path; optimizing the single path is a separate task.
- **Publication gets more expensive with manifest size.** One `ingest_verified`
  grows from 66 to 192 ms, and waiting exceeds CPU time (5.6 s against 2.7 s of
  CPU for medium): likely causes are rewriting the whole manifest and several
  `fsync` calls per publication. Building an N-asset corpus one by one is O(N²);
  the `kanji-large` template takes about 6 minutes.
- **Writer contention does not scale.** 1 / 2 / 4 / 8 writers publish the same
  volume in 5.54 / 6.41 / 6.23 / 6.83 s: `flock` serializes publications, and
  every writer adds CPU time (2.73 → 3.61 s) and the memory of its handle
  (process RSS 16 → 118 MB). The group's mean load is ≈ 0.5 cores for any
  number of writers.
- **`fsync` tails.** `batch-checkpoint` has a minimum of 21–61 ms, but single
  runs take up to 1.2 s (σ above the mean). The likely cause is WSL2 virtual
  disk synchronization; compare checkpoints by minimum and run quantiles and
  with more runs.
- **System time dominates** in all read and check operations (`time_system` ≈
  2/3 of `time_cpu`): candidates are the number of system calls per record and
  the manifest size.

Not everything is measured: the `pitch_accent` and `pronunciation_audio`
domains, the kanji domain validator, browser acquisition, inter-process
contention, a cold page cache (no global flush is performed) and native Linux
without WSL2.
