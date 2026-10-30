# Performance: `asset-store`

[Русский](performance.md) · **English** · [简体中文](performance.zh-CN.md)
<!-- i18n-source-git-blob: 39d4ec7279d8db8a137d3ffbd19d302bedeeed1c -->

Performance of the asset store's file core
([store contract and workload executor](README.en.md)):
method, measurement boundary, synthetic workload characteristics and the last
confirmed snapshots of the catalog scenarios. General measurement rules are in
the [methodology](../../docs/performance/README.en.md), scenario fields in the
[benchmark catalog](../../docs/performance/benchmarks.en.md).

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
| `asset-store.batch-checkpoint` | `open`, `load` and `save`; `change` = `unchanged` or `modified` | atomic state write with file and directory sync; `modified` also writes a new blob and bumps the revision |

In every scenario the interval is one run of the built release binary
`asset-store-bench` from process start to exit, including opening the store.
Building the template, restoring the working copy and checking postconditions
are outside the interval. Metrics: `time_wall_clock` (required), `time_user`,
`time_system`, `memory_peak_resident`, `instructions`, the run subgroup's
`cpu_time_cgroup` and `memory_peak_cgroup`, `disk_usage` of the working copy for
publication and the subgroup's `cpu_utilization_mean` for contention. In
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
[`performance-benchmarking`](../../.agents/skills/performance-benchmarking/SKILL.md)).

## Snapshots

The blocks below are generated by `benchmark-history snapshot` from verified
`benchmark-runner` results in the reference environment; if there are no blocks,
there has been no confirmed snapshot yet. Snapshots are published for the
`kanji-medium` spec; results of the other sizes are in the local history and in
the PR description.

<!-- benchmark-snapshot: {"scenario":"asset-store.verify-integrity","profile":"warm","recorded_at":"2026-10-29T09:28:20Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.verify-integrity` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

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
| `time_wall_clock@command` | command / hyperfine | 132.96 ms ± 4.92 ms | 131.43 ms | 129.07 ms – 150.55 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 43.41 ms ± 9.23 ms | 44.15 ms | 19.5 ms – 66.54 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 85.18 ms ± 7.27 ms | 82.62 ms | 75.24 ms – 105.3 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 13.47 MiB ± 83.48 KiB | 13.49 MiB | 13.3 MiB – 13.58 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 462 864 770.8 ± 37 649.2 | 462 834 963 | 462 834 579 – 462 910 287 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 130.18 ms ± 4.85 ms | 129.11 ms | 125.33 ms – 147.02 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 10.46 MiB ± 101 KiB | 10.47 MiB | 10.22 MiB – 10.72 MiB | 20 |
| `cpu_utilization_peak@host` | host / procfs | 1.52 cores ± 0.74 cores | 1.27 cores | 0.98 cores – 3.92 cores | 20 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-batch","profile":"warm","recorded_at":"2026-10-29T09:28:24Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-batch` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

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
| `time_wall_clock@command` | command / hyperfine | 62.93 ms ± 1.49 ms | 62.37 ms | 61.83 ms – 67.44 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 18.75 ms ± 8.32 ms | 18.19 ms | 4.15 ms – 29.39 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 44.06 ms ± 8.45 ms | 45.57 ms | 32.9 ms – 58.14 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.36 MiB ± 78.93 KiB | 8.33 MiB | 8.24 MiB – 8.61 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 165 084 275.4 ± 7 631.5 | 165 084 461.5 | 165 076 141 – 165 092 234 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 64.2 ms ± 1.48 ms | 63.69 ms | 62.94 ms – 68.72 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.76 MiB ± 56.71 KiB | 5.75 MiB | 5.74 MiB – 5.99 MiB | 20 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.read-single","profile":"warm","recorded_at":"2026-10-29T09:32:37Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"read_bytes":{"value":1213680.0,"unit":"byte"},"reads":{"value":1250.0,"unit":"count"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.read-single` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 5 of 5 runs successful, 1 warmup run.

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
| `time_wall_clock@command` | command / hyperfine | 42.017 s ± 242.61 ms | 42.028 s | 41.767 s – 42.388 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 14.022 s ± 120.57 ms | 13.985 s | 13.925 s – 14.232 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 27.984 s ± 287.21 ms | 27.838 s | 27.779 s – 28.453 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.62 MiB ± 67.85 KiB | 8.63 MiB | 8.55 MiB – 8.7 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 131 427 996 154.4 ± 8 366 164.3 | 131 431 737 418 | 131 413 030 305 – 131 431 739 634 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 42.007 s ± 242.32 ms | 42.019 s | 41.759 s – 42.38 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.86 MiB ± 163.75 KiB | 5.75 MiB | 5.75 MiB – 6.07 MiB | 5 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest","profile":"warm","recorded_at":"2026-10-29T09:37:54Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}}} -->
### `asset-store.ingest` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 10 of 10 runs successful, 1 warmup run.

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
| `time_wall_clock@command` | command / hyperfine | 5.626 s ± 448.34 ms | 5.485 s | 5.386 s – 6.886 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.103 s ± 53.34 ms | 1.1 s | 1.005 s – 1.21 s | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.573 s ± 60.28 ms | 1.564 s | 1.454 s – 1.659 s | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 15.11 MiB ± 67.04 KiB | 15.13 MiB | 14.95 MiB – 15.18 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 12 133 819 476 ± 770 993.2 | 12 133 903 097.5 | 12 132 326 506 – 12 134 762 910 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 2.678 s ± 23.91 ms | 2.668 s | 2.65 s – 2.723 s | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 14.87 MiB ± 134.94 KiB | 14.83 MiB | 14.7 MiB – 15.12 MiB | 10 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 10 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.ingest-contention","profile":"warm","recorded_at":"2026-10-29T09:41:57Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"corpus_bytes":{"value":1213680.0,"unit":"byte"},"ingest":{"value":50.0,"unit":"count"},"ingest_bytes":{"value":53887.0,"unit":"byte"},"records":{"value":1250.0,"unit":"count"}}},"parameters":{"writers":"4"}} -->
### `asset-store.ingest-contention` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 5 of 5 runs successful, 1 warmup run.

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
| `time_wall_clock@command` | command / hyperfine | 6.233 s ± 24.3 ms | 6.222 s | 6.211 s – 6.268 s | 5 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 1.461 s ± 44.93 ms | 1.463 s | 1.408 s – 1.524 s | 5 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 1.783 s ± 35.48 ms | 1.791 s | 1.722 s – 1.809 s | 5 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 59.46 MiB ± 592.76 KiB | 59.38 MiB | 58.64 MiB – 60.18 MiB | 5 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 16 329 060 762.6 ± 4 866 892.8 | 16 330 284 016 | 16 321 154 190 – 16 334 340 069 | 5 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 3.25 s ± 29.05 ms | 3.253 s | 3.219 s – 3.281 s | 5 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 60.16 MiB ± 740.64 KiB | 60.23 MiB | 59.09 MiB – 61 MiB | 5 |
| `disk_usage@path/store` | path / filesystem | 5.3 MiB ± 0 B | 5.3 MiB | 5.3 MiB – 5.3 MiB | 5 |
| `cpu_utilization_mean@cgroup` | cgroup / cgroup-v2 | 0.52 cores ± 0 cores | 0.52 cores | 0.51 cores – 0.52 cores | 5 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-reopen","profile":"warm","recorded_at":"2026-10-29T09:32:39Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}}} -->
### `asset-store.batch-reopen` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 20 of 20 runs successful, 2 warmup runs.

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
| `time_wall_clock@command` | command / hyperfine | 12.59 ms ± 291.7 µs | 12.58 ms | 12.1 ms – 13.21 ms | 20 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 3.5 ms ± 3.32 ms | 4.16 ms | 0 s – 8.33 ms | 20 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 9 ms ± 3.48 ms | 8.39 ms | 4 ms – 13.13 ms | 20 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.37 MiB ± 96.11 KiB | 8.39 MiB | 8.16 MiB – 8.49 MiB | 20 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 41 304 131.1 ± 328.2 | 41 304 149 | 41 303 487 – 41 304 652 | 20 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 13.9 ms ± 303.8 µs | 13.83 ms | 13.4 ms – 14.52 ms | 20 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.75 MiB ± 8.24 KiB | 5.75 MiB | 5.74 MiB – 5.77 MiB | 20 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot: {"scenario":"asset-store.batch-checkpoint","profile":"warm","recorded_at":"2026-10-29T09:35:31Z","environment":{"arch":"x86_64","cpu_model":"AMD Ryzen 7 5800X3D 8-Core Processor","logical_cpus":"16","memory_total_bytes":"16730976256","os":"linux","virtualization":"wsl2"},"workload":{"workload":{"batch_blob_bytes":{"value":1024000.0,"unit":"byte"},"batch_blobs":{"value":250.0,"unit":"count"},"batch_entries":{"value":1000.0,"unit":"count"}}},"parameters":{"change":"modified"}} -->
### `asset-store.batch-checkpoint` · profile `warm`

Last confirmed measurement: 2026-10-29 UTC · `asset-store` 0.1.0 · hyperfine 2.0.0 · benchmark-runner 0.1.0 · r1 · warm · online · outcome `success`: 10 of 10 runs successful, 2 warmup runs.

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
| `time_wall_clock@command` | command / hyperfine | 151.07 ms ± 383.47 ms | 29.76 ms | 26.91 ms – 1.242 s | 10 |
| `time_user@process-tree-waited` | process-tree-waited / hyperfine | 6.71 ms ± 4.48 ms | 7.78 ms | 0 s – 11.86 ms | 10 |
| `time_system@process-tree-waited` | process-tree-waited / hyperfine | 8.34 ms ± 5.15 ms | 6.91 ms | 3.57 ms – 18.3 ms | 10 |
| `memory_peak_resident@largest-process-in-tree` | largest-process-in-tree / hyperfine | 8.34 MiB ± 109.9 KiB | 8.32 MiB | 8.16 MiB – 8.49 MiB | 10 |
| `instructions@process-tree-waited` | process-tree-waited / hyperfine | 46 214 832.5 ± 316.2 | 46 214 737.5 | 46 214 439 – 46 215 403 | 10 |
| `cpu_time_cgroup@cgroup` | cgroup / cgroup-v2 | 16.68 ms ± 1.27 ms | 16.27 ms | 15.87 ms – 19.97 ms | 10 |
| `memory_peak_cgroup@cgroup` | cgroup / cgroup-v2 | 5.75 MiB ± 1.26 KiB | 5.75 MiB | 5.74 MiB – 5.75 MiB | 10 |

★ The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.
<!-- /benchmark-snapshot -->

<!-- benchmark-snapshot:anchor -->

## Baseline observations

The observations below come from accepted JSON results of the current,
not yet optimized implementation in the snapshots' reference environment (WSL2
with ext4 on a virtual disk, AMD Ryzen 7 5800X3D, 16 logical CPUs). This is a
first measurement: there is nothing to compare with and no "faster/slower"
conclusions here. Numbers are series means of `time_wall_clock`; full results
and their IDs are in the local history and the PR description.

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
