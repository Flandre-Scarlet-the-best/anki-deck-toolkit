# Performance and resource usage

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)
<!-- i18n-source-git-blob: 7e8844b595691a300c8dbf05b20bfd00ecd4148f -->

This directory provides the common methodology and links to the **last confirmed figures** for the performance and
resource usage of the repository's modules: time, CPU, memory, disk. Each
module has its own page with its method, metric definitions and snapshots per
scenario and profile in an explicitly named reference environment. This page
sets the common rules for taking, storing and publishing measurements.

A snapshot is evidence of actual behaviour in a specific environment, not a
contract, not a speed norm for other systems and not a CI threshold. Its numbers
are not pinned in code, tests or checks: different hardware, OS or Docker give
different values.

## Where the history lives

| What | Where | Who sees it |
|---|---|---|
| The full history of every measurement | the local [`benchmark-history`](../../tools/benchmark-history/README.en.md) history: an immutable archive of original JSON and a SQLite index in the user's data directory | only its owner |
| The public historical record | the `## Производительность` (performance) section of the PR description: real results, baseline, comparability, conditions and conclusions | everyone, through the PR history |
| The current snapshot | the `tools/<module>/performance.md` pages: the last confirmed measurement of each scenario × profile pair | everyone |

The pages do not accumulate entries from every PR: a new confirmed measurement
replaces the snapshot of the same pair, while earlier values remain in the Git
history and in PR descriptions. The public reference is the project owner's
measurements in the environment described in the snapshot. Other users have
their own local results; they never edit the public pages automatically, and a
snapshot from another environment does not replace the reference one without
explicit confirmation.

## Module pages

| Module | Page | Snapshots from JSON results |
|---|---|---|
| `anki-repo` | [anki-repo/performance.en.md](../../tools/anki-repo/performance.en.md) | `anki-repo.validate` · `warm`, `offline-warm`; `anki-repo.stats` · `warm` |
| `asset-store` | [asset-store/performance.en.md](../../tools/asset-store/performance.en.md) | `asset-store.*` · `warm` (`kanji-medium`) |
| `anki-e2e` | [anki-e2e/performance.en.md](../../tools/anki-e2e/performance.en.md) | `anki-e2e.smoke`, `anki-e2e.full` · `warm`; a manual observation without original JSON for the other states |
| `benchmark-runner` | [benchmark-runner/performance.en.md](../../tools/benchmark-runner/performance.en.md) | `benchmark-runner.calibration-*` · `warm` |

A new module gets its own page `tools/<module>/performance.md` with
translations, a `<!-- benchmark-snapshot:anchor -->` line where snapshots go and
a row in this table.

## Scenario catalog and result contract

Scenarios, execution-condition profiles, the boundary of the measured work and
metrics with their scope are declared in machine-readable form in the
`benchmarks/` catalog, and measurement results are recorded as versioned JSON.
How to register a scenario and read a result is described in the
[benchmark catalog](benchmarks.en.md); series are executed by
[`benchmark-runner`](../../tools/benchmark-runner/README.en.md). The rules on this page also apply to
catalog-driven measurements: the catalog requires the exact cache state to be
declared for each scenario, and a snapshot remains evidence of an environment.

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
   optimisation without a measurement that showed the problem is not published
   as an improvement.

How an agent works on an optimisation or a suspected regression — hypothesis,
a baseline of the previous implementation before the change, a comparable
series after it, analysis and report — is described in the
[`performance-benchmarking`](../../.agents/skills/performance-benchmarking/SKILL.md)
skill. It builds on these rules and on the regular `benchmark-runner` and
`benchmark-history` without replacing them.

## Metric sources and accuracy

| Metric | Source | Accuracy |
|---|---|---|
| Phase time | UTC timestamps in the module's log | one second; `0 s` means "under a second" |
| Process memory peak | `VmHWM` in `/proc/<pid>/status` | exact RSS peak of the process, without children |
| Process CPU | `utime`+`stime` and `cutime`+`cstime` in `/proc/<pid>/stat` | exact; children are counted only once the process has waited for them |
| Container memory peak | cgroup v2 `memory.peak` | exact peak tracked by the kernel |
| Container CPU | `usage_usec` in the cgroup `cpu.stat` | exact for the total; average and peak cores come from differences between samples |
| Peak cores over 1 s | difference of `usage_usec` between adjacent samples | lower bound: bursts shorter than the interval are smoothed out |
| Host load and memory | `/proc/stat`, `MemAvailable` in `/proc/meminfo` | the whole system: a host observation, not a bound — it includes other allocations and is offset by concurrent releases |
| Disk | `du -sb`, `docker image ls`, `docker system df -v` | sizes of images with shared layers are not summed |

## How to update a snapshot

1. Run a catalog scenario with the regular `benchmark-runner run`; the accepted
   result goes into the local history by itself.
2. Find the result (`benchmark-history list`) and check its outcome, runs,
   environment and unavailable metrics (`benchmark-history show`).
3. Preview the page change without writing:
   `benchmark-history snapshot <ID> --page tools/<module>/performance.md`, then
   write it with `--apply`. The tool updates the pair's block in all three
   languages at once and refuses if the series is incomplete, the working copy
   has changes, the environment differs from the reference one or the page holds
   a newer measurement.
4. Paste the `benchmark-history report` section for the same result into the PR
   description: the exact code state and record IDs are given there.

Permanent documentation contains no commit SHAs, PR numbers, absolute local
paths, input identities or author names (a rule of
[`AGENTS.md`](../../AGENTS.md)); those details belong in the PR description and
in the local history. Snapshots inside `benchmark-snapshot` markers are not
edited by hand.

Manual observations recorded before the history existed stay on the module
pages, explicitly marked as having no original JSON results: they must not be
presented as a confirmed snapshot and must not be reconstructed as results after
the fact. They are replaced by the first confirmed snapshot of the same pair, or
by a new section with a manual observation if the scenario is not in the catalog.
`snapshot --apply` adds the snapshot before the anchor and leaves sections
without markers untouched, so the outdated manual observation is removed by hand
in the same change as the first snapshot.
