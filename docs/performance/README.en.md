# Performance and resource usage

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)
<!-- i18n-source-git-blob: 4e9e2474c00c1fd1a02949c4844d7e8f5b0da142 -->

This directory holds the performance journals of the repository's modules:
time per phase, CPU, memory, disk and the number of spawned processes. Each
module has its own page with a module-specific method and a journal of
entries. This page sets the common rules for taking and recording such
measurements.

A journal is evidence of actual behaviour in a specific environment, not a
contract and not a CI threshold. Its numbers are not pinned in code, tests or
checks: different hardware, OS or Docker give different values.

## Module journals

| Module | Journal | Latest entry |
|---|---|---|
| `anki-e2e` | [anki-e2e.en.md](anki-e2e.en.md) | 2026-10-26 |
| `benchmark-runner` | [benchmark-runner.en.md](benchmark-runner.en.md) | 2026-10-26 |

A new module gets its own page `docs/performance/<module>.md` with
translations and a row in this table.

## Scenario catalog and result contract

Scenarios, execution-condition profiles, the boundary of the measured work and
metrics with their scope are declared in machine-readable form in the
`benchmarks/` catalog, and measurement results are recorded as versioned JSON.
How to register a scenario and read a result is described in the
[benchmark catalog](benchmarks.en.md); series are executed by
[`benchmark-runner`](../../tools/benchmark-runner/README.en.md). The rules on this page also apply to
catalog-driven measurements: the catalog requires the exact cache state to be
declared for each scenario, and the log remains evidence of an environment.

## Measurement rules

1. **A question before the measurement.** First state what you need to learn
   (for example, "what does a repeated run cost" or "does the container stay
   within its limit") and pick metrics for that question. Do not collect
   numbers without a purpose.
2. **Environment.** Record the CPU model and thread count, RAM, OS and kernel
   (including WSL2), the versions of Docker, `rustc` and the module's external
   dependencies, the active limits (`--cpus`, `--memory`) and the state of the
   working copy (clean or with changes).
3. **Cache state.** Every run belongs to an explicitly named state:
   - *cold* — the module's own caches are removed by its regular command;
   - *warm* — a repeat with unchanged input;
   - *incremental* — only the input that should cause a partial rebuild is
     changed.

   Record precisely what was cleared and what was not. Shared caches the module
   does not own (for example, the daemon's BuildKit layer cache or base images)
   are not removed for a "fully cold" measurement: that would affect other
   projects. Such a run is marked cold for the module, but not for the system.
4. **Repeats.** Measure the warm state at least twice and report the range.
   Do not compare a single number with a single number from another entry.
5. **No unrelated load.** Do not run builds, tests or other heavy processes
   during a measurement. The sampler must be cheap: reading `/proc` and cgroups
   once a second puts practically no load on the system.
6. **The real implementation.** Measure the module's regular commands on the
   regular build, not a simplified copy. Disabling checks, caches or cleanup for
   a better time is not allowed.
7. **Honesty.** State what was not measured. Do not present an estimate as a
   measurement, or a property of the environment as a property of the code. An
   optimisation without a measurement that showed the problem is not recorded in
   the journal as an improvement.

## Metric sources and accuracy

| Metric | Source | Accuracy |
|---|---|---|
| Phase time | UTC timestamps in the module's log | one second; `0 s` means "under a second" |
| Process memory peak | `VmHWM` in `/proc/<pid>/status` | exact RSS peak of the process, without children |
| Process CPU | `utime`+`stime` and `cutime`+`cstime` in `/proc/<pid>/stat` | exact; children are counted only once the process has waited for them |
| Container memory peak | cgroup v2 `memory.peak` | exact peak tracked by the kernel |
| Container CPU | `usage_usec` in the cgroup `cpu.stat` | exact for the total; average and peak cores come from differences between samples |
| Peak cores over 1 s | difference of `usage_usec` between adjacent samples | lower bound: bursts shorter than the interval are smoothed out |
| Host load and memory | `/proc/stat`, `MemAvailable` in `/proc/meminfo` | the whole system: an upper bound when the process has no cgroup of its own |
| Disk | `du -sb`, `docker image ls`, `docker system df -v` | sizes of images with shared layers are not summed |

## How to write an entry

An entry is identified by the UTC date, the module version from `Cargo.toml`,
the versions of significant external dependencies and a description of the
environment. Commit SHAs, PR numbers, absolute local paths and author names are
not written into permanent documentation (a rule of
[`AGENTS.md`](../../AGENTS.md)). The exact source state a measurement was taken
on is given in the description of the PR that adds the entry.

Entries in a module journal go from newest to oldest and are not rewritten. If
an entry turns out to be wrong, it is marked and the corrected one is added
next to it.

Entry template:

```markdown
### <UTC date> · <module> <version> · <key dependency and version> · <command>

- Environment: …
- Cache states: …
- Time per phase: a table per run
- Resources: a "metric — value — source" table
- Disk: …
- Conclusions: what was confirmed, what needs attention
- Not measured: …
```
