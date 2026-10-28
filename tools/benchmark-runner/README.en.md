# Benchmark runner: `benchmark-runner`

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)
<!-- i18n-source-git-blob: df674dc00203e7451f1a7e5bfb6e01f61eeec110 -->

`benchmark-runner` is a Linux CLI that runs scenarios of the `benchmarks/`
catalog by ID and profile, measures the target with Hyperfine 2.0, adds Linux
system metrics (cgroup v2, `/proc`, the module's machine-readable report, the
filesystem) and writes a result in the `benchmark-contract` format. The result
is validated by `benchmark_contract::result::check` against the catalog before
it is accepted; a result that fails validation never becomes final.

The meaning of scenario fields, profiles and metrics is in the [catalog
guide](../../docs/performance/benchmarks.en.md); the catalog and result
schemas are owned by [`benchmark-contract`](../benchmark-contract/README.en.md);
general measurement rules are in the [methodology](../../docs/performance/README.en.md);
measurements of the runner itself are on its [performance page](../../docs/performance/benchmark-runner.en.md);
the local result history is [`benchmark-history`](../benchmark-history/README.en.md).

The runner does not change user exports or Anki profiles, does not read
`decks/input/`, does not call `sudo`, does not prune Docker caches
(`docker system prune` and the like) and sends no data to the network itself.
It hands an accepted result to the user's local history (see below) but does
not keep that history itself. It receives paths to user data only as run parameters.

## Commands

```bash
cargo run -p benchmark-runner -- list
cargo run -p benchmark-runner -- check anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export
cargo run -p benchmark-runner -- run anki-repo.validate --profile warm --param export_dir=/path/to/export --runs 20 --output validate-warm.json
cargo run -p benchmark-runner -- check anki-e2e.smoke --profile warm --runs 2
cargo run -p benchmark-runner -- --json run anki-e2e.smoke --profile warm --runs 2
```

| Command | What it does |
|---|---|
| `list` | executable scenarios: ID, kind, profiles, allowed number of runs; `example` scenarios are not shown |
| `check <id> --profile P` | real environment readiness for a series: Hyperfine and its metrics via a probe run, executables, the Docker daemon, cgroup v2 and delegation, `perf_event`, network, offline isolation; for each metric — whether it will be measured and why not. Builds nothing and does not run the target |
| `run <id> --profile P` | a full series and a validated result |

Options of `check` and `run`:

| Option | Meaning |
|---|---|
| `--param <name>=<value>` | value of a declared parameter; an unknown name, a value outside `choices` and a missing required parameter are usage errors |
| `--runs N` | number of measured runs; `min_runs` by default. Mandatory for `long-e2e`: a long series is never chosen implicitly. The range is from `min_runs` (or 1 with `single_run_allowed`) to `max_runs` |
| `--no-sampling` | do not poll `/proc` and cgroup during a run; sampled metrics become `unavailable` with a reason. A container group is observed only during a run, so for a scenario with `container` all `cgroup` metrics are unavailable, and a required one leads to outcome `unsupported` |
| `--output FILE` | result file (`run` only); an existing file is never overwritten |
| `--artifacts-dir DIR` | artifacts root (`run` only); `target/benchmark-runner` of the repository by default |
| `--build-timeout S` | limit for building each `cargo-bin`, in seconds (`run` only), 3600 by default |
| `--no-history` | do not store the accepted result in the local history (`run` only) |

Global options: `--catalog DIR` — the directory with `catalog.toml` (by default
`benchmarks/` of the nearest parent of the current directory); `--json` —
machine-readable JSON on stdout. Series progress always goes to stderr and to
`runner.log`.

## Exit codes

| Code | `exit` | Meaning |
|---|---|---|
| `0` | `success` | all runs succeeded and the result was accepted; `check` — the series can be measured |
| `1` | `partial` | the result was accepted but the series is incomplete: a run failed after a successful one, the budget ran out, a required metric is incomplete, fewer than `min_runs` runs, or cleanup failed |
| `2` | `usage` | invalid arguments, unknown scenario or parameter, an example scenario, an existing `--output` |
| `3` | `error` | internal error or unavailable catalog; no result written. If writing an accepted result to the final file fails after the series, a copy is kept as `result.unwritten.json` in the artifacts |
| `4` | `failure` | the result was accepted, no successful runs (including a `setup` failure) |
| `5` | `unsupported` | the environment cannot measure the series honestly (no Hyperfine 2.x, required metric, Docker or network isolation); a result with this outcome is written, with no runs; `check` — the same without writing |
| `6` | `interrupted` | the series was interrupted by a signal; recorded runs are kept and cleanup was performed |
| `7` | `busy` | a series of the same module is already running |
| `8` | `rejected` | the assembled result failed the contract check; it is kept as `result.rejected.json` among the artifacts, there is no final file |

With `--json`, `run` prints a summary: `ok`, `exit_code`, `exit`, `scenario`,
`profile`, `outcome`, `reason`, `result`, `artifacts`, `runs`,
`successful_runs`, `cleanup`, `history`, `warnings`, `diagnostics`. The same
summary is stored as `summary.json` in the artifacts directory.

## Local history

After writing an accepted result, `run` hands it to the local
[`benchmark-history`](../benchmark-history/README.en.md) history with the same
contract and catalog, without measuring again: `BENCHMARK_HISTORY_DIR`, otherwise
the user's XDG data directory. A store inside the working copy is refused. The
`history` field of the summary: `status` — `archived`, `already-archived`,
`disabled` (`--no-history`), `failed` (with `reason`) or `not-recorded` (no
accepted result), and the record `id`. The runner waits at most 10 seconds for a store held by another `benchmark-history` command; after that the save is `failed`. A history failure does not delete the
result and does not change the series exit code, but it is printed as a warning
and added to `warnings`; the result can be stored later with
`benchmark-history import`.

## Installing Hyperfine 2.0

Hyperfine 2.x is required: export `schema_version = 2`, shell-less mode (`-N`)
and `--metrics`. The runner recognizes Hyperfine 1.x by its version and
refuses honestly with code `5`. If the distribution package is older, the
static release works:

```bash
curl -fsSLO https://github.com/sharkdp/hyperfine/releases/download/v2.0.0/hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
sha256sum hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf hyperfine-v2.0.0-x86_64-unknown-linux-musl.tar.gz
install -D -m 0755 hyperfine-v2.0.0-x86_64-unknown-linux-musl/hyperfine ~/.local/bin/hyperfine
hyperfine --version
```

Compare the checksum with the digest on the release page; CI installs the same
archive with a pinned checksum. CPU counters (`instructions`, `cpu_cycles`…)
need access to `perf_event` (usually `kernel.perf_event_paranoid ≤ 2` for your
own processes): without it Hyperfine does not export them and the metrics
become `unsupported`.

## Series order

1. Selecting the scenario, profile, parameters and number of runs; the module
   lock inside the working copy itself
   (`target/benchmark-runner/locks/<module>.lock`). It does not depend on
   `--artifacts-dir`: series of one module in one working copy exclude each
   other with any artifacts directory and any path to that copy, while
   independent working copies do not interfere.
2. Readiness check; with blockers — outcome `unsupported` with no runs.
3. Preparation: a snapshot of code provenance (commit, dirty flag,
   `source_tree_sha256`), building `cargo-bin` (`cargo build --locked`),
   resolving executables from `PATH`, input identity and size, then a second
   snapshot. If the working copy changed during preparation, or the
   `repo-tree` identity disagrees with the snapshot, the series is not
   measured (outcome `failure`). Preparation is not measured and does not
   count towards the series budget.
4. Scenario `setup`, then profile `setup`.
5. Warmups: `prepare` → target → `conclude`; values are not kept.
6. Measured runs: `prepare` → **target** → `conclude`.
7. Scenario `cleanup` — always, after success, failure and interruption.

The runner executes lifecycle steps itself from argv without a shell, each in
its own process group and with its own `timeout_seconds`. Hyperfine measures
the target: a separate `-N --runs 1 --export-json` invocation per run, because
Hyperfine 2.0 writes no export at all when a run fails in the middle of a
series. Target arguments are substituted once (`{param.*}`, `{input.*}`,
doubled braces are literal), and the target environment is only the
declaration's `env.inherit` and `env.set`. `short-cli` target output is
discarded, `long-e2e` output is kept among the artifacts.

Step timeouts, `run_timeout_seconds` per run and `total_budget_seconds` per
series are enforced strictly. The budget counts from the start of `setup` and
covers `setup`, warmups and runs with their `prepare` and `conclude`.
Preparation (bounded by `--build-timeout` per `cargo-bin` and by the
`timeout_seconds` of input probes) and `cleanup` stay outside the budget;
`cleanup` runs even after the budget is exhausted, bounded by the
`timeout_seconds` of each of its steps. The upper bound of the runner's work is
therefore the sum of these limits, not `total_budget_seconds` alone.
Termination is graduated: SIGINT, then SIGTERM, then SIGKILL of the whole
process group. The runner becomes a subreaper, so a descendant that left the
group is still found and removed; such an "escaped" process makes the run
unsuccessful, and during a warmup it stops the series. A lifecycle step that
leaves its processes behind after a successful exit is not considered done
either: cleanup kills those descendants in the middle of their work, so the
step's effect is not proven. Such a `setup`, `prepare` or `conclude` stops the
series, and such a `cleanup` makes cleanup incomplete.

A failed run (unexpected code, timeout, leftover processes) stops the series.
The outcome is `partial` if a successful run happened before, otherwise
`failure`. A failed cleanup downgrades `success` to `partial` and is reflected
in `cleanup`.

## Signals and cancellation

The first SIGINT, SIGTERM or SIGHUP interrupts the current step or run and
stops the series; `cleanup` still runs afterwards. A second signal interrupts
cleanup as well. The interrupted run is recorded with status `interrupted`,
the exit code is `6`. The final file is written atomically (a temporary file
and a hard link): there is no partial result, and an existing file is never
overwritten.

## Linux metrics and their scope

| Source | How it is collected | Scope |
|---|---|---|
| `hyperfine` | Hyperfine export of each run | the target process and waited-for descendants |
| `procfs` / `host` | polling `/proc/stat` and `MemAvailable` during a run | the whole system: an upper bound |
| `cgroup-v2` / `cgroup` without a container | an own subgroup per run, `cpu.stat`, `memory.peak` | the target, all its descendants and the Hyperfine process itself |
| `cgroup-v2` / `cgroup` with `container` | the Docker container's cgroup found during the run | the container's processes |
| `module-report` | the module's machine-readable report that appeared during this run | values at the declared JSON Pointer |
| `filesystem` | path size after the run | only the named path |

A module report belongs to exactly one run: it is the only new directory in
`runs_dir` that appeared during that run. If the declaration takes an input's
identity or size from the report, every run must confirm them with its own
report: without a new, parseable and complete report the run is `failure`,
even if earlier runs already recorded the input. A change of identity or size
between runs is `failure` too. A report used only for optional metrics and
facts may be absent: the metric is then incomplete and the run stays
successful.

A missing value is never recorded as zero: the metric becomes `unavailable` or
`unsupported` with a reason. Metrics without a machine-readable source in the
repository (for example, `phase_duration` with a human-readable module log) are
`unsupported`.

### Own cgroup

For `cgroup` metrics without a container the runner creates a subgroup per run.
Without root, subgroups can only be created in a delegated group, so `run`
re-executes itself through `systemd-run --user --scope -p Delegate=yes` when
needed (the command runs in place, without a separate service) and moves
itself into the `benchmark-runner` subgroup to follow the cgroup v2 "no
internal processes" rule. Without a user systemd or delegation, optional
metrics become `unsupported` with a reason, and required ones lead to outcome
`unsupported`.

The Hyperfine process is moved into the run group before measurement starts,
not the target: moving a process between groups before it creates threads
costs about 17 ms of kernel waiting and would distort the target time. The
group values therefore include a small overhead of Hyperfine itself (a few
milliseconds of CPU and about 1 MiB of memory), which the scope description
states.

### Docker container

If a scenario declares `container`, the runner looks for the container by the
declared label during the run and accepts it only when the creator's PID from
the `owner_pid_label` label is a descendant of the current run and the cgroup
path of the container's process contains its ID. Other containers with the
same label are never attributed. The readiness check (`check`, the `docker`
requirement), the `docker_version` fact and container polling call the Docker
CLI only with the target environment (the declaration's `env.inherit` and
`env.set`): the runner's `DOCKER_HOST`, `DOCKER_CONTEXT` and `DOCKER_CONFIG`
that the target did not receive cannot point them at another daemon. If the
target's daemon is unreachable, the series is `unsupported` even when the
runner's daemon answers. Values are the last observation before the
container is removed: the systemd scope is removed right after it exits.
Hyperfine CPU time for such a scenario covers only the host processes, not the
container.

### Offline profile

An offline profile runs only when isolation is actually in place: the target
runs in a separate network namespace (`unshare --user --map-current-user
--net`), and a probe confirms that only `lo` is available. The Docker daemon
and BuildKit are not covered by this isolation, so the offline profile of a
scenario that requires `docker` yields outcome `unsupported`. Without user
namespaces the outcome is also `unsupported`.

### Sampler impact

`/proc` and cgroup are polled only during a measured run, once per
`sample_interval`. Measurements of the impact on target time are in the
[runner page](../../docs/performance/benchmark-runner.en.md); `--no-sampling`
turns polling off completely.

## What is published

A result contains the scenario ID and revision, the profile, recorded
parameters, input identity and size, environment facts and provenance: the
commit, the dirty flag of the working copy, `source_tree_sha256` and the
runner and Hyperfine versions. The commit, dirty flag and
`source_tree_sha256` are the snapshot confirmed before measurement, from which
the programs were built and the `repo-tree` identity computed, not the state
after the series. Git is called for the snapshot without inherited variables
that bind it to another repository (`git rev-parse --local-env-vars`: `GIT_DIR`,
`GIT_WORK_TREE`, `GIT_INDEX_FILE` and others), so a run from a Git hook does
not replace the provenance with another repository's commit or index. After
cleanup the snapshot is compared once more: if the
commit or tracked or non-ignored files changed, the outcome is not `success`
(`partial` or worse) with a reason. Files ignored by Git (for example
`target/`) are not compared, so scenario temporary data must not remain in
non-ignored paths after `cleanup`. Absolute paths, parameters with
`record = false`, secrets and raw target output never enter the result;
reasons containing such data are replaced with neutral text.

The artifacts directory `<artifacts>/runs/<id>/<UTC>-<profile>-<random>/`
contains `runner.log`, `summary.json`, step logs (`steps/`), Hyperfine exports
and logs (`hyperfine/`), target output (`output/`), samples (`samples/`) and
the module reports found (`reports/`). Artifacts may contain local paths and
program output: they are local run data, not to be published or committed.

## Linux and WSL2

Linux with cgroup v2 and `/proc` is supported; macOS and Windows are not.
WSL2 is an ordinary Linux environment: the runner records it as the fact
`virtualization = wsl2` but does not rely on Windows paths, `/mnt/c` or Docker
Desktop. An own cgroup needs `systemd --user`, container metrics need Docker
with cgroup v2, and the offline profile needs `unshare` from util-linux.

## Diagnostics

| Symptom | Cause and what to do |
|---|---|
| `hyperfine` is `unsupported`, code `5` | no Hyperfine 2.x in `PATH`; install release 2.0 |
| CPU counters are `unsupported` | no access to `perf_event`: check `kernel.perf_event_paranoid` |
| `cgroup` metrics are `unsupported` | no delegation: check `systemctl --user status` and `systemd-run --user --scope true` |
| the offline profile is `unsupported` | no user namespaces, or the scenario requires Docker |
| code `7` | a series of the same module in this working copy is already running (with any `--artifacts-dir`); wait for it to finish |
| code `8` | the result violated the contract: see `diagnostics` in the summary and `result.rejected.json` |
| a `failure` run with leftover processes | the target left processes behind after exiting; they were removed, details in `runner.log` |
| the series stopped: "processes of the step remained after it exited" | a lifecycle step left descendants behind; they were killed, the step's effect is not proven |
| `partial`: "the working copy changed during the series" | the commit or non-ignored files changed during the series; provenance describes the snapshot before measurement |
| `docker` — "unavailable in the target environment" | the daemon that the target's `env.inherit`/`env.set` lead to does not answer; the runner's Docker variables are not used |

## Checks

Integration tests (`tests/runner.rs`) build a synthetic catalog and module in a
temporary directory and run the real Hyperfine with the `benchmark-workload`
load: memory and CPU, lifecycle order, failures of every step and leftover
processes of steps, timeout and SIGINT without orphan processes, offline, own
cgroup, the module report of every run, the provenance snapshot and its drift
in a Git working copy, the target's Docker environment with a stub `docker`,
refusal for Hyperfine 1.x, no overwriting, concurrent series with different
artifacts directories, no private paths in the result, module report facts only
when all successful runs agree, and storing in the local history, including
disabling it and its failure. Without Hyperfine 2.x they are skipped locally and fail in CI. The
tests do not read `decks/**`. The suite is part of
`cargo test --workspace --locked`.
