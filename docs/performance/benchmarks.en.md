# Benchmark catalog and result contract

[Русский](benchmarks.md) · **English** · [简体中文](benchmarks.zh-CN.md)
<!-- i18n-source-git-blob: 134363313dd5ec7fcb5dd2a1828a33be9a453571 -->

The catalog is the machine-readable form of what the [methodology](README.en.md)
describes in words: which scenarios are measured, under which conditions, what
the measured interval contains, and which metrics with which scope are
collected. The result contract is versioned JSON in which a runner records
measurements. Both contracts are checked by `benchmark-contract`
([CLI and exit codes](../../tools/benchmark-contract/README.en.md)).

The catalog and its checks run and measure nothing. Series are executed by
[`benchmark-runner`](../../tools/benchmark-runner/README.en.md): Hyperfine 2.0
and Linux system metrics. It uses this contract instead of inventing its own.
The local result history, baseline selection and reports are kept by
[`benchmark-history`](../../tools/benchmark-history/README.en.md); the repository
holds no run history and no thresholds. Module snapshots
(`tools/<module>/performance.md`) remain evidence of specific environments.

## Where things live

| Path | What it is |
|---|---|
| `benchmarks/catalog.toml` | schema version, repository root and the shared profile vocabulary |
| `benchmarks/scenarios/<module>.toml` | scenario declarations of one module; the file name equals the module name |
| `tools/benchmark-contract/` | typed models, loading, validation, Hyperfine import, CLI |
| `tools/benchmark-runner/` | series runner: Hyperfine 2.0, cgroup v2, `/proc`, module report |
| `tools/benchmark-contract/tests/fixtures/` | example catalog and results for tests (`status = "example"`) |

Discovery is fixed: the loader reads `catalog.toml` and every `*.toml` in
`scenarios/` without nested directories. Hidden files are skipped; any other
entry or symbolic link is an error rather than a silently skipped declaration.

Scenarios as of today (the declarations themselves are the source of truth; the set changes):

| Scenario | Kind | Profiles |
|---|---|---|
| `anki-e2e.full` | `long-e2e` | `cold`, `warm` |
| `anki-e2e.smoke` | `long-e2e` | `warm`, `offline-warm` |
| `anki-repo.validate` | `short-cli` | `warm`, `offline-warm` |
| `anki-repo.stats` | `short-cli` | `warm` |
| `asset-store.verify-integrity` | `short-cli` | `warm` |
| `asset-store.read-batch` | `short-cli` | `warm` |
| `asset-store.read-single` | `short-cli` | `warm` |
| `asset-store.ingest` | `short-cli` | `warm` |
| `asset-store.ingest-contention` | `short-cli` | `warm` |
| `asset-store.batch-reopen` | `short-cli` | `warm` |
| `asset-store.batch-checkpoint` | `short-cli` | `warm` |
| `benchmark-runner.calibration-noop` | `short-cli` | `warm` |
| `benchmark-runner.calibration-cpu` | `short-cli` | `warm` |
| `benchmark-runner.calibration-cpu-plain` | `short-cli` | `warm` |
| `benchmark-runner.calibration-parallel` | `short-cli` | `warm` |
| `benchmark-runner.calibration-memory` | `short-cli` | `warm` |
| `benchmark-runner.calibration-io` | `short-cli` | `warm` |

## Registering a scenario for a new module

1. Create `benchmarks/scenarios/<module>.toml` with `schema_version = 1` and
   `module = "<module>"`. Neither code nor `catalog.toml` changes.
2. Declare `[[scenario]]` entries with ID `<module>.<name>`.
3. Validate the catalog and look at how it was understood:

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- show <module>.<name>
```

A minimal declaration of a short CLI operation:

```toml
schema_version = 1
module = "example-tool"

[[scenario]]
id = "example-tool.check"
title = "Check an input file"
revision = 1
kind = "short-cli"
status = "executable"
description = "What the command does and why it is interesting."
measured = "One run from process start to exit; build and warmup are excluded."
platforms = ["linux"]

[scenario.target]
program = { kind = "cargo-bin", package = "example-tool", bin = "example-tool" }
args = ["--json", "check", "{input.data}"]
expect_exit_codes = [0]

[scenario.target.env]
inherit = ["PATH"]

[[scenario.parameter]]
name = "data_path"
description = "Path to the input; never published."
required = true
record = false
affects_comparability = false

[[scenario.input]]
id = "data"
kind = "external-path"
role = "workload"
identity = "content-tree-sha256"
parameter = "data_path"
description = "Command input."
size = [{ name = "bytes", unit = "byte", description = "Input size." }]

[scenario.runs]
warmup = 3
min_runs = 10
max_runs = 100
single_run_allowed = false
run_timeout_seconds = 60
total_budget_seconds = 900

[[scenario.profile]]
id = "warm"
cache_definition = "No caches of its own; the OS page cache is warmed by three runs."
cleared = []
retained = ["OS page cache"]

[[scenario.metric]]
metric = "time_wall_clock"
scope = "command"
source = "hyperfine"
required = true

[scenario.environment]
record = ["os", "arch", "cpu_model", "module_version", "hyperfine_version"]
comparable = ["os", "arch", "cpu_model"]
```

## Declaration fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | stable ID `<module>.<name>`; unchanged while the scenario stays the same |
| `title`, `description` | yes | name and what exactly is measured |
| `revision` | yes | version of the workload and its semantics, starting at 1 |
| `kind` | yes | `short-cli` or `long-e2e`; selects run limits |
| `status` | yes | `executable` — a real command; `example` — a teaching example, never measured |
| `measured` | yes | the boundary of the measured interval in words |
| `platforms` | yes | `linux`, `macos`, `windows`: where the scenario is measured |
| `target` | yes | `program`, `args`, `working_dir` (from the repository root, `.` by default), `env.set`, `env.inherit`, `expect_exit_codes` |
| `parameter` | no | `name`, `description`, `required`, `default`, `choices`, `record`, `affects_comparability` |
| `input` | no | inputs with kind, role, identity and size |
| `runs` | yes | `warmup`, `min_runs`, `max_runs`, `single_run_allowed`, `run_timeout_seconds`, `total_budget_seconds` |
| `setup`, `cleanup` | no | lifecycle steps of the whole scenario |
| `profile` | yes | profile bindings with an exact cache definition and `setup`, `prepare`, `conclude` steps; `args_append` holds arguments appended to the target in this profile, `required_parameters` lists parameters the profile makes mandatory (for example, `required_parameters = ["anki_version"]`) |
| `metric` | yes | requested metrics; `time_wall_clock` with scope `command` is mandatory |
| `environment` | yes | facts that are recorded (`record`) and compared (`comparable`), plus the module's own facts (`fact`) |
| `requires` | no | `executable`, `docker`, `linux-cgroup-v2`, `perf-events`, `network` (with profiles) |
| `module_report` | no | the module's machine-readable report: `runs_dir` (from the repository root), file name `report`, `description`. Needed for the `module-report` source and `report` fields |
| `container` | no | the Docker container whose cgroup is measured: `label` (`key=value`), `owner_pid_label` — the label holding the creator's PID, `description`. Requires `docker` in `requires` |

`program` is either `{ kind = "cargo-bin", package, bin, cargo_profile }`
(`release` by default; the package and binary are checked against the Cargo
workspace manifests) or `{ kind = "executable", name }` from `PATH`. Steps are
argv, not a shell string: `sh`, `bash`, `env`, `sudo`, `rm`, `dd`, launch wrappers
such as `nice` and `timeout` and similar programs are forbidden. The list catches
obvious declaration mistakes but is not exhaustive and is not a security boundary:
declarations are written by maintainers and their content is checked in review. Arguments and `env.set` values may contain
`{param.<name>}` and `{input.<id>}` placeholders (the latter only for
`repo-file` and `external-path`); a literal brace is doubled. An optional
parameter without a default cannot be substituted unless the profile lists it in
`required_parameters`. A parameter that is substituted into no command and does
not carry an `external-path` input produces a warning: its recorded value would
not affect the run.

## Identity, revision and measurement

Three things are never mixed:

- the **scenario ID** is the stable name of the benchmark;
- the **revision** is the version of its workload and semantics. Increase it when
  target arguments, inputs or the way they are chosen, the measurement boundary,
  a profile definition or the meaning of a metric changes. Results of different
  revisions are not comparable;
- the **date and environment** belong to one result record, not to the scenario.

## Lifecycle and measurement boundary

Order for one "scenario × profile" series:

1. The runner builds the `cargo-bin` (`cargo build --locked -p … --bin … --profile …`). This is preparation.
2. Scenario `setup`, then profile `setup` — once.
3. Warmup runs: `prepare` → target → `conclude`; values are not stored.
4. Measured runs: `prepare` → **target** → `conclude`. Only the target is measured.
5. Scenario `cleanup` — always, including after a failure or interruption.

The runner executes the steps itself from argv, without a shell; Hyperfine 2.0
measures only the target, with a separate invocation per run. A `setup` failure yields outcome `failure` with no runs; a `prepare` failure, a
timeout or an unexpected exit code of the target stops the series, and the
outcome becomes `partial` if a successful run happened before, otherwise
`failure`. Each step is bounded by `timeout_seconds`, a run by
`run_timeout_seconds`, the whole series from the start of `setup` to the last
`conclude` by `total_budget_seconds`; an exhausted budget stops the series.
Building `cargo-bin` and `cleanup` are outside the budget: the runner bounds
the build, and `cleanup` runs even after the budget is exhausted, bounded by
the `timeout_seconds` of its steps.

A step's `touches` states what it affects: `read-only`, `scenario-temp` (the
scenario's own temporary data) or `module-owned` (module state that the module
cleans with its own command). There is no "everything" value: dropping the
global page cache, `docker … prune` without `--filter` and wiping other
projects' caches cannot be expressed in a declaration.

## Profiles and cache state

`catalog.toml` declares the profiles `cold`, `warm`, `offline-warm` and
`incremental`. A profile sets the base cache state and network; the scenario's
profile binding gives the **exact** definition: `cache_definition`, `cleared`
(what is brought into the required state) and `retained` (what is kept on
purpose). "Cold" never implicitly means dropping the OS page cache.

- `cold` — `prepare` must reset module state with a `module-owned` step,
  `cleared` is not empty, there is no warmup. Cold for the module, not for the system.
- `warm` — needs priming: `runs.warmup ≥ 1` or a profile `setup`; `prepare`
  does not reset module state.
- `incremental` — `prepare` makes an input change that triggers a partial rebuild.
- an offline profile must fill in `network_definition`: which requests are
  excluded and how that is enforced. The runner executes the series without
  network, and a network error is a failed run. An offline profile cannot be
  listed in a `network` requirement.

## Run policy and limits

| Limit | `short-cli` | `long-e2e` |
|---|---|---|
| warmup | ≤ 100 | ≤ 3 |
| runs | ≤ 1000 | ≤ 10 |
| step timeout | 1…1800 s | 1…14 400 s |
| run timeout | 1…600 s | 1…14 400 s |
| series budget | ≤ 3600 s | ≤ 86 400 s |
| single run | forbidden | allowed (`single_run_allowed`) |

Without `single_run_allowed`, `min_runs ≥ 2` is required. A scenario with a warm
profile always requires `min_runs ≥ 2`: the warm state is measured at least twice. The budget must fit at
least one run. `benchmark-contract vocabulary` prints the limits. A step
timeout bounds `setup`, `prepare`/`conclude`, `cleanup` and input probes; the run
timeout bounds the measured target.

## Inputs and workload size

| Kind | Identity | Where the path comes from |
|---|---|---|
| `repo-tree` | `content-tree-sha256` | working copy |
| `repo-file` | `file-sha256`, `content-tree-sha256` | `path` in the declaration; the file must exist |
| `external-path` | `file-sha256`, `content-tree-sha256` | a parameter with `record = false` and no `default`: the path is given only at run time and is never published |
| `downloaded-artifact` | `file-sha256`, `declared-version` | external artifact |

Role `subject` is what is being compared (for example, code): a different
identity is expected. Role `workload` is the load: it must declare size
characteristics (`byte` or `count`), and results with a different workload
identity or size are not comparable.

The declaration states explicitly where identity and size come from:

- `measure` of a size characteristic: `file-bytes` with a `path` inside the
  input and unit `byte`, `tree-bytes` (`byte`), `tree-files` (`count`), `probe`
  with a `pointer` into the input probe's JSON output, `report` with a
  `pointer` into the module report;
- an input `probe` is a `read-only` step that prints JSON about the input (for
  example, `anki-repo --json stats`); it runs during preparation and is not
  measured;
- `report_identity` is a JSON Pointer to a `downloaded-artifact` identity in
  the module report: the runner does not download an artifact that the module
  downloads itself.

An input with no way to compute its identity or size produces the warning
`unresolvable_input`. In a result such an input may be missing only in a series
without successful runs (`unsupported`, a `setup` failure): a successful run
must record every input.

## Metrics: scope, source, unit

Base units: `second`, `byte`, `count`, `core` (average busy cores: CPU seconds
per second). The contract has no other units. The metric key in a result is
`metric@scope` or `metric@scope/label`.

| Metric | Unit | Scope / source | What it is |
|---|---|---|---|
| `time_wall_clock` | second | `command` / `hyperfine` | duration of one target run |
| `time_user`, `time_system`, `time_cpu` | second | `process-tree-waited` / `hyperfine`; `process` or `process-tree-waited` / `procfs` | CPU time; children only when waited for |
| `memory_peak_resident` | byte | `largest-process-in-tree` / `hyperfine`; `process` / `procfs` | peak RSS of **one** process, not a tree total |
| `memory_peak_cgroup` | byte | `cgroup` / `cgroup-v2` | simultaneous peak of the whole group (`memory.peak`) |
| `cpu_time_cgroup` | second | `cgroup` / `cgroup-v2` | CPU time of the whole group (`usage_usec`) |
| `cpu_utilization_mean`, `cpu_utilization_peak` | core | `cgroup` / `cgroup-v2`; `host` / `procfs` | observed load; the peak is a lower bound per interval; the mean of the run's own subgroup is its CPU time over its lifetime (the whole Hyperfine invocation) |
| `memory_host_peak_increase` | byte | `host` / `procfs` | largest drop of `MemAvailable` during the run across the whole system: it includes other processes' allocations and is offset by concurrent releases (including delayed return of the previous run's memory), so it is a host observation, not a bound |
| `cpu_cycles`, `instructions`, `cache_references`, `cache_misses`, `branch_misses` | count | `process-tree-waited` / `hyperfine` | CPU counters; need `perf_event` |
| `phase_duration` | second | `phase` / `module-log` | a phase inside the target from the module log; needs a label and `resolution` |
| `module_counter` | count | `module-report` / `module-report` | a counter from the module's machine report; needs a label |
| `disk_usage` | byte | `path` / `filesystem` | apparent size of a named resource, as `du -sb`; needs a label |

A combination outside the registry is the error `metric_scope_source`, not a new
metric. The full registry with platforms is `benchmark-contract vocabulary`.

A metric with the `module-report` source sets `pointer`, a JSON Pointer into the
module report in which a `*` segment selects every element of an array or object
and the values are summed; `disk_usage` sets `path` from the repository root. A
module fact in `fact` may set `report`, a list of pointers into the report;
different values between runs make the fact unknown. These fields are forbidden
for other metrics (`unexpected_field`).

Peak RSS of a process, cgroup memory peak, CPU time and observed load are
different quantities; none substitutes another. Hyperfine only sees the target
process and waited-for children: Docker container processes and BuildKit steps
are not part of its `time_cpu` and `memory_peak_resident`. On Linux, Hyperfine
may replace a very small peak RSS with the value inherited from itself.

A required metric cannot be promised on a platform where its source is
unavailable: that is an error; for an optional metric it is a warning, and the
result marks it `unsupported`. Required CPU counters need `requires` with
`perf-events`, required cgroup metrics need `linux-cgroup-v2`. Validating the
catalog needs no cgroup or `perf_event` permissions.

## Environment and comparability

`environment.record` lists facts written into a result; `os`, `arch` and
`module_version` are mandatory. `environment.comparable` is the subset that
must match; `os` and `arch` are always included. Built-in facts: `os`, `arch`,
`kernel_release`, `virtualization`, `cpu_model`, `logical_cpus`,
`memory_total_bytes`, `module_version`, `rustc_version`, `docker_version`,
`hyperfine_version`; a module declares its own in `fact`. Collecting every
possible hardware characteristic is not required.

Two results are comparable (`benchmark-contract compare`) only if the scenario
and revision, profile, workload identity and size, parameters with
`affects_comparability`, known values of compared facts and metric definitions
match. An unknown fact does not prove comparability. A difference in a subject
input is reported separately and does not block the comparison. Statistical
conclusions about a metric are allowed only if each result has at least two
values of it from successful runs: a failed attempt yields no value. Such metrics
are listed in `statistical_metrics`, and `statistically_supported` is true when
this holds for every comparable metric.

## Result contract

A result is JSON with `schema_version = 1`, separate from the Hyperfine format. The
example below is abridged: some environment facts, input sizes, metrics and runs are
omitted, so `check-result` rejects it. Complete valid results live in
`tools/benchmark-contract/tests/fixtures/results/`.

```json
{
  "schema_version": 1,
  "scenario": { "id": "anki-repo.validate", "module": "anki-repo", "revision": 1, "kind": "short-cli" },
  "profile": { "id": "warm", "cache": "warm", "network": "online" },
  "inputs": [{ "id": "export", "role": "workload",
               "identity": { "method": "content-tree-sha256", "value": "<64 hex>" },
               "size": { "deck_json_bytes": { "value": 5242880.0, "unit": "byte" } } }],
  "provenance": {
    "recorded_at": "2026-10-27T12:00:00Z",
    "measurement": { "name": "hyperfine", "version": "2.0.0", "format": "hyperfine-json-v2" },
    "code": { "module_version": "0.1.0", "git_commit": "<40 hex>", "dirty": false }
  },
  "environment": { "os": { "status": "known", "value": "linux" },
                   "cpu_model": { "status": "unavailable", "reason": "…" } },
  "outcome": { "status": "success" },
  "observation": "repeated",
  "warmup_runs": 3,
  "metrics": [{ "key": "time_wall_clock@command", "metric": "time_wall_clock", "scope": "command",
                "source": "hyperfine", "unit": "second", "availability": "measured" }],
  "runs": [{ "index": 0, "status": "success", "exit_code": 0,
             "values": { "time_wall_clock@command": { "status": "measured", "value": 0.0123 } } }],
  "aggregates": { "time_wall_clock@command": { "count": 10, "mean": 0.0125, "stddev": 0.0004,
                                               "median": 0.0124, "min": 0.0119, "max": 0.0133 } }
}
```

Statuses:

- outcome `outcome.status`: `success` — all runs succeeded, required metrics are
  measured in every run and there are at least `min_runs` runs; `partial` — at
  least one successful run, but the series or the metrics are incomplete;
  `failure` — no successful run; `unsupported` — the scenario cannot be measured
  here and there are no runs. Everything except `success` needs a `reason`;
- run: `success`, `failure`, `timeout`, `interrupted`; a failed run needs a `reason`.
  A successful run records `exit_code`, and when checked against the catalog the
  code must be in the scenario's `expect_exit_codes`; otherwise it is `failure`;
- a metric value in a run: `measured` with `value`, or `unavailable`,
  `unsupported`, `failed` with `reason`. Every metric is present in every run.
  Absence is never recorded as zero;
- a metric's `availability` must follow from the runs: `measured`, `partial` or
  one of the absence statuses.

When checked against the catalog, `warmup_runs` follows from the lifecycle: once
measured runs have started it equals `runs.warmup`; without runs the series may
have stopped in `setup` or during the warm-up, so a smaller number is allowed,
but never a larger one. Warming up through the profile's `setup` is not counted
in `warmup_runs`.

`observation`: `none` without runs, `single` for one run — a valid observation
without statistics, `repeated` for two or more attempts. Aggregates are optional and are
computed only from measured values of successful runs; `stddev` is the sample
deviation (n − 1) and `null` for a single value. Validation recomputes the
aggregates and rejects a mismatch and an aggregate without primary values.

`provenance` keeps technical identifiers (commit SHA, tree hash) in the result
record itself. They are not copied into permanent documentation, including
module pages — that is a rule of [`AGENTS.md`](../../AGENTS.md).

A published result contains no paths inside home directories, tokens, private
keys, credentials or email addresses: validation inspects every string and key
of the document. Values of parameters with `record = false` never reach the
result, nor do the process environment variables.

## Hyperfine 2.0 import

The library module `benchmark_contract::hyperfine` reads Hyperfine 2.0
`--export-json` (`schema_version: 2`); a 1.x export and other versions are
refused explicitly. The format was checked against the `v2.0.0` tag.

| Hyperfine | Contract |
|---|---|
| `measurements[j]` | `runs[j]`; warmups are not exported |
| `exit_code` | `exit_code`; outside `expect_exit_codes` or `null` (signal) — run `failure` |
| `time_wall_clock` | `time_wall_clock@command` |
| `time_user`, `time_system`, `time_cpu` | `…@process-tree-waited` |
| `memory_peak_resident` | `memory_peak_resident@largest-process-in-tree` |
| CPU counters | `…@process-tree-waited` |
| omitted metric | `unavailable` with a reason |
| `summary` | required for a metric present in every `measurement` and checked against the recomputation over all of them; absence or a mismatch refuses the import. Hyperfine writes no summary for an incomplete set of values |
| `metadata.hyperfine_version`, `start_time`, `platform` | `provenance.measurement`, record time, `os`/`arch` facts |

The export's `command` and `environment` are not carried into the result: they
may contain user paths and environment values, while the target is fully defined
by the scenario, revision and parameters. Export metrics that the scenario did
not request are listed in `unrequested` instead of being silently lost.

## Validation

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- list
cargo run -p benchmark-contract -- preflight anki-e2e.full
cargo run -p benchmark-contract -- check-result result.json
cargo run -p benchmark-contract -- compare before.json after.json
cargo run -p benchmark-contract -- --json vocabulary
```

`validate` checks only the declarations and needs no Docker, Anki or Hyperfine.
The platform and dependency availability in the current environment are checked
separately by `preflight`: an OS outside the scenario's `platforms` is a negative
result, and it only reads `PATH`, `/sys/fs/cgroup` and `/proc/sys`.
Diagnostics name the file, scenario, field and a stable reason code. The
`cargo test --workspace` suite checks both the real catalog and the example
fixtures.

## Schema evolution

- The catalog and result `schema_version` is an integer. The tool understands
  only known versions and explicitly refuses others instead of reading a
  document partially.
- A new field or enum value that an old tool must not silently ignore increases
  the version. The schemas are closed: an unknown field is an error.
- Moving between versions is a documented deterministic transformation from
  version N to N + 1. It is implemented together with version N + 1; there is no
  general migration framework in advance.
- History storage keeps results with their `schema_version` and does not rewrite them.
- Optional fields that an old tool rejects as unknown rather than silently
  ignoring (`module_report`, `container`, `measure`, `probe`,
  `report_identity`, `path`, `pointer`, a fact's `report`) do not increase the
  version.
- A scenario revision and the schema version are independent: a new revision
  does not change the schema, and a new schema does not change scenario revisions.

## How the runner uses the contract

`benchmark-runner` loads the catalog through the library, selects a scenario
and a profile, checks environment readiness with probe runs, builds the
`cargo-bin`, substitutes parameters and inputs, computes input identity and
size, executes the lifecycle steps, measures the target with Hyperfine 2.0 and
`--export-json`, imports the export through this library, adds metrics from
other sources (cgroup, `/proc`, module report, filesystem), fills in
environment and provenance, validates the result with `check-result` against
the catalog and only then stores it. Comparisons use `compare`.

## Boundaries

There is no history storage, no regression computation and no CI thresholds
here: history and reports live in
[`benchmark-history`](../../tools/benchmark-history/README.en.md), without
thresholds. Numbers from module snapshots do not become thresholds.

## Block I/O

`io_read_bytes`/`io_write_bytes` (byte) and `io_read_operations`/`io_write_operations`
(count) use scope `cgroup`, source `cgroup-v2`: sums of `rbytes`/`wbytes`/`rios`/`wios`
across devices in `io.stat`, per the
[Linux kernel documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html#io-interface-files).
A fresh own group includes the target, its children and Hyperfine from entry to the
final read before removal. Setup/prepare/conclude/cleanup and the sampler stay outside.
Container measurements require a discovered group and an available final sample;
a vanished group yields `unavailable`, never an incomplete final total.

The optional `io` controller is enabled only in the runner's delegated group when
these metrics are requested. Its absence leaves CPU/RAM available: `check` reports
why, and results retain `unsupported`/`unavailable`. A readable empty `io.stat`
means zero **accounted** operations. Read errors, incomplete rows, duplicate devices
and overflow never become zero.

These are kernel block-layer counts, not logical read/write bytes, system calls,
`disk_usage`, or physical media behind a VM/WSL. Page-cache hits need no disk read.
Writeback after the interval is excluded; completed writes require `fsync` in the
target. Writeback attribution depends on filesystem support and inode ownership;
shared files may be charged elsewhere. Stacked devices may count the same I/O more
than once: totals are not unique physical-media bytes. Throughput and IOPS are not
published: dividing these counts by Hyperfine target time would mix different scopes.

Scenarios gaining these metrics have a new revision. Earlier JSON remains valid
for its original revision and available in local history; it must not be validated
against the new declaration. Existing snapshots gain no fabricated I/O columns.
