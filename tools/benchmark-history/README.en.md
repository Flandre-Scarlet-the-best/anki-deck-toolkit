# Local benchmark history: `benchmark-history`

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)
<!-- i18n-source-git-blob: 46d18e6615e67b6460ec6de63d28ad00fe8e6db4 -->

`benchmark-history` keeps each user's own history of validated benchmark
results, finds a baseline in it, compares "before" and "after", and turns the
results into the `## Производительность` (performance) section of a PR
description and into snapshots for the
[`docs/performance/`](../../docs/performance/README.en.md) pages. It measures
nothing: series are run by [`benchmark-runner`](../benchmark-runner/README.en.md),
while the result model, its validation and the comparability rules belong to
[`benchmark-contract`](../benchmark-contract/README.en.md). There is no second
result format and no second measurement engine here.

It works entirely locally: no network, no GitHub tokens, no server and no shared
database. One clone's history is never synchronised with anyone else's, and no
measurement is published automatically: the PR section is pasted into the PR
description through the normal
[`anki-git-workflow`](../../.agents/skills/anki-git-workflow/SKILL.md) process,
and a documentation snapshot is written only with an explicit `--apply`.

## Where the history lives

| Source | Path |
|---|---|
| `--store DIR` | `DIR` |
| `BENCHMARK_HISTORY_DIR` | the variable's value |
| `XDG_DATA_HOME` | `$XDG_DATA_HOME/anki-deck-toolkit/benchmark-history` |
| otherwise | `~/.local/share/anki-deck-toolkit/benchmark-history` |

The path must be absolute and free of `..` components; a relative
`XDG_DATA_HOME` is invalid per the XDG specification and is skipped. A store inside a Git working copy is
refused before any file is created: local results must not show up in `git status` or in a commit.
The path itself is checked — its nearest existing ancestor with symbolic links
resolved and any directory above it that holds `.git` (a clone, a worktree, a
submodule) — so the refusal does not depend on the current directory, the
scenario catalog, or whether the path comes from `--store`,
`BENCHMARK_HISTORY_DIR` or XDG. The same rule applies to the `backup`
directory.
Neither WSL2, nor Windows paths, nor systemd are required.

```text
<store>/
├── store.json          # format and archive layout version
├── results/<ab>/<id>.json  # original results — the source of truth
├── index.sqlite        # SQLite index — a rebuildable projection of the archive
├── lock                # exclusive lock for operations
├── corrupt/            # damaged copies replaced by restore
└── tmp/                # temporary files of atomic writes
```

## Archive and index

- **Acceptance.** A document is accepted only if
  `benchmark_contract::result::check` passes it together with the
  `benchmarks/` catalog (a historical scenario revision is allowed with a
  warning, an unknown scenario is not). Schema version, runs, metric
  availability, provenance, inputs, profile and environment are kept as they are.
- **The identifier** of a record is the SHA-256 of the canonical JSON form
  (sorted keys, no whitespace). Accepting the same result again, even with
  different indentation, whitespace or key order, creates no duplicate. Numbers
  are not normalized: `1` and `1.0` are different canonical forms and different
  IDs; normalizing them would change the IDs of already accepted records.
- **Immutability.** The file is stored byte for byte as accepted. The name is
  taken with a hard link to a fully written and flushed temporary file: the
  write is atomic and an existing copy is never overwritten. A damaged copy is
  not replaced on re-import — that is an error pointing to `verify`.
- **Series outcomes.** `partial`, `failure` and `unsupported` are accepted too:
  the history does not hide unsuccessful runs, but such results never become a
  baseline.
- **The SQLite index** is derived from the archive. The archive copy is written
  first, then the index row; an interruption in between leaves a record without
  a row, and the next command indexes it. A row without an archive copy is
  removed with a message. A damaged index is set aside as
  `index.sqlite.corrupt` and rebuilt from the archive. An outdated index schema
  (`PRAGMA user_version`) migrates by rebuilding from the archive; a schema newer
  than supported is refused, after which `reindex` rebuilds the index.
- **Projection integrity.** `quick_check` and the schema number do not prove
  that the projection is intact, so on every open the file's schema is checked
  against the expected one (a missing or extra table — the schema is created
  anew and filled from the archive), and the rows of each record against its
  `projection_sha256` checksum: a record whose checksum does not match is
  removed from the index and rebuilt from its archive copy. The check costs one
  pass over the index tables without reading JSON. `verify` performs the full
  check of the index against the projection of the archive JSON; `reindex`
  fixes a mismatch. The archive is never rewritten.

## Commands

```bash
cargo run -p benchmark-history -- status
cargo run -p benchmark-history -- import target/benchmark-runner/runs
cargo run -p benchmark-history -- list --scenario anki-repo.validate --profile warm --since 2026-10-01
cargo run -p benchmark-history -- show 1a2b3c4d
cargo run -p benchmark-history -- baseline 1a2b3c4d
cargo run -p benchmark-history -- compare 5e6f7a8b 1a2b3c4d
cargo run -p benchmark-history -- report 1a2b3c4d
cargo run -p benchmark-history -- snapshot 1a2b3c4d --page tools/anki-repo/performance.md
cargo run -p benchmark-history -- verify
cargo run -p benchmark-history -- backup /path/to/backup
cargo run -p benchmark-history -- restore /path/to/backup
```

| Command | What it does |
|---|---|
| `status` | store path, where the path came from and the number of records; does not create the store |
| `import <path>...` | accepts result files; directories are walked recursively and only documents that look like results are taken, while the runner's `*.rejected.json` and symbolic links to directories inside the walk are skipped |
| `list` | records from newest to oldest with filters `--scenario`, `--module`, `--revision`, `--profile`, `--outcome`, `--since`, `--until`, `--module-version`, `--commit`, `--input` (input identity prefix), `--env name=value`, `--limit` |
| `show <ID>` | record summary; `--raw` prints the original JSON of the archive copy |
| `baseline <ID>` | every baseline candidate with reasons why it fits or not, and the chosen baseline; `--commit` narrows the choice |
| `compare <base> <result>` | the `benchmark-contract` verdict, before/after, deltas and the assessment against the spread |
| `report <ID>...` | Markdown `## Производительность` section for a PR; `--baseline ID` (one per scenario × profile pair), `--first` or `--commit` |
| `snapshot <ID> --page P` | snapshot for a `docs/performance/` page in three languages; without `--apply` it only shows it |
| `verify` | checks every archive copy against its identifier and the contract, and the index against the archive projection; does not change the archive or create a store (a missing store is code 3); whatever index reconciliation on opening fixed before the check is reported and gives code 1 |
| `reindex` | rebuilds the index from the archive |
| `backup <dir>` | copies the archive into a new or empty directory to move it to another Linux machine |
| `restore <dir>` | accepts the records of a backup by the `backup.json` inventory: the name of each record must equal the SHA-256 of its content |

An ID is accepted in full or as a prefix of at least six characters. Global
options: `--store`, `--catalog` (defaults to `benchmarks/` of the nearest parent
of the current directory), `--json` — stable JSON on stdout.

Moving: `backup` on one machine, move the directory any way you like (`tar`,
`rsync`), `restore` on the other. No catalog is needed for restoring: the match
between name and content proves the record has not changed since it was
accepted. `backup` writes `backup.json` last and atomically — the format, the
archive layout version and the full inventory of records; a directory without
it (including an interrupted copy) is not a backup, and separate `result.json`
files are accepted by `import`. `restore` checks the copy against the
inventory: a record whose file is missing, that was modified or that the
inventory does not list is not accepted and is listed in the report
(`missing`, `problems`); intact records are accepted, and the transfer is
marked incomplete (`complete: false`, code `4`). `restore` replaces a damaged
local copy of a record with the verified copy from the backup (`replaced`) and
moves the old file to `corrupt/` without deleting it. Repeating `restore` is
idempotent.

The `backup` directory and the `restore` source must not overlap the store: a
path equal to it, inside it (for example `<store>/results/backup`) or containing
it is refused with code `2` before anything is written. Paths are compared by
components after resolving symbolic links, including a not-yet-existing tail of
the path, and by the identity of existing directories: on a case-insensitive file
system (`/mnt/c` in WSL) `STORE/results` is the same directory as
`store/results`. A dangling link is refused, because writing through it would go
to its target. The archive, the index and the destination stay unchanged on
refusal.

## Automatic archiving from `benchmark-runner`

After writing an accepted result, `benchmark-runner run` hands it to the history
itself with the same contract and catalog, without measuring again. The outcome
is the `history` field of the summary: `archived`, `already-archived`,
`disabled` (`--no-history`), `failed` with a reason, or `not-recorded` when there
is no accepted result. A history failure does not delete `result.json` and does
not change the series exit code, but it is printed as a warning and added to
`warnings`: the result can be archived later with `import`.

## Baseline and comparison

Comparability is decided only by `benchmark_contract::comparability::compare`:
one scenario and revision, one profile, identical metric definitions, workload
and significant environment facts. A different `subject` input (changed code)
is an expected difference; a different `workload` input makes the results
incomparable.

Automatic baseline rule: **the latest successful result of the same scenario
and profile, recorded before the checked one, of a provably different code
state, unless only documentation changed between the commits, and comparable
under the contract**. A different state is proven by different
`source_tree_sha256` values or, if the tree is not recorded in both results, by
different commits both measured without working-copy changes (with a caveat in
the report). Missing information does not count as a difference.

`source_tree_sha256` and the commit describe the whole repository, not the
built program: the result carries no identity of the executable artifact. So
for a pair of commits of clean working copies the history checks the set of
changes with the Git of the catalog repository (`git diff --name-only`),
without using the network. Every candidate gets a `code` field:

| `code.kind` | Meaning | Automatic baseline |
|---|---|---|
| `documentation-only` | only Markdown files changed, or nothing: the same implementation, a repeated measurement | no |
| `files` | other files changed too; whether the measured implementation is affected is not proven by the data | yes, with a caveat |
| `unverified` | the commit is not in the clone, Git is unavailable or a working copy with changes was measured | yes, with a caveat |
| `not-proven` | a different state is not proven | no |

`baseline` shows every candidate and the reason it is rejected:
`partial`/`failure`/`unsupported`, recorded later, the same
`source_tree_sha256` or the same commit without changes (a repeat, not a
"before"), a difference in code not proven, only documentation changed,
comparability blockers — and a general caveat: a different repository state
does not prove that the measured implementation was replaced. The same caveat
is part of the "Вывод" line of the PR section. A specific baseline can be given
explicitly (`report --baseline ID`), also outside the rule; deviations from the
rule are listed in the report, and a baseline equal to the checked result is
refused both there and in `compare`. Comparability is still decided only by
`benchmark-contract`.

- The delta is computed on means of successful runs; the relative delta only
  for a non-zero base, in one unit and one scope.
- Direction of improvement: for time, memory, CPU time, disk, instructions,
  cycles and misses lower is better; for core utilisation, cache references and
  module counters the direction is not assessed.
- Assessment: `в пределах разброса` (within the spread) if |Δ| does not exceed
  the sum of standard deviations; `сверх разброса` (beyond the spread)
  otherwise; `мало повторов` (too few repeats) if either side has fewer than two
  measurements. This is **not** a statistical significance test, and the
  contract's `statistically_supported` only means there are enough measurements
  to estimate the spread.
- Incomparable results get no conclusions; `unavailable`, `unsupported` and
  `failed` are shown as a status with a reason, never as zero.

## PR section

`report` builds the section following
[`pr-body.md`](../../.agents/skills/anki-git-workflow/references/pr-body.md):
`Статус: измерено` with the fields «Сценарий», «Измеренный код» (commit,
changes flag, source tree), «Нагрузка» (input identity and size), «Окружение»,
«Прогоны», «База», «Сопоставимость», the metric table, «Вывод»,
«Ограничения» and «Источники» (full record IDs). Without a baseline the section
distinguishes new functionality (`--first`: there is no "before") from the
absence of a usable baseline. A `failure` or `unsupported` result yields
`Статус: не измерено` with a reason, diagnostics and the line
«Доказательство производительности: отсутствует». The section is printed to
stdout and pasted into the PR description by hand; the tool never writes to
GitHub.

## Documentation snapshot

`snapshot` replaces the block of the same scenario × profile pair on a module
page together with its translations (`<module>.en.md`, `<module>.zh-CN.md`); a
new pair is added before the `<!-- benchmark-snapshot:anchor -->` line. The block
holds the date, versions, environment facts, workload size, recorded
parameters that affect comparability (for example `writers`) and metrics, but
not the commit, the record ID, the input identity or paths. Parameters are part
of the marker as part of the workload definition: a snapshot with other values
does not replace the previous one without `--replace-workload`. Refusals that
write nothing:

- the outcome is not `success`, or the result belongs to an outdated scenario
  revision;
- a working copy with changes or without a recorded state (allowed by
  `--allow-dirty`, and the block says so);
- the reference environment is undefined: a hardware or platform fact (`os`,
  `arch`, `virtualization`, `cpu_model`, `logical_cpus`, `memory_total_bytes`)
  or a comparability fact of the scenario is unknown;
- the page holds a snapshot from another reference environment
  (`--replace-environment` confirms the change of reference), on a workload
  whose identity with the new one is not proven (`--replace-workload` confirms
  it) or a newer measurement.

An equal workload size does not prove an identical input: two different exports
can match in bytes and number of notes. Input identities are not stored on the
page, so the proof is the result of the previous snapshot in the local
history: another result of the same pair that produces exactly the block on the
page (the marker with recording time, reference environment and workload size,
versions, metrics), with the same identities of the `workload` inputs. The
result being published is never such proof, even if it is in the history:
otherwise a new result of the same size recorded within the same second would
confirm the previous snapshot by itself. If such a result exists and the
identities match, the snapshot is replaced without a flag. `--replace-workload`
is required if the identities differ; several history results with different
workloads produce the block on the page; the result is not in the history
(another machine, a cleared history); the block matches no result (edited by
hand or produced by another version of the generator); or the marker has no
recorded workload. Matching block text does not bypass the check: a repeated
`snapshot` of an already published result without another history result
producing the same block is refused, and with `--replace-workload` it reports
that the snapshot is already up to date and writes nothing. The refusal names
only input IDs, never identities. The plan action (`plan.action`: `added`,
`replaced`, `unchanged`) and its basis (`--json`: `plan.workload`; text: the
`нагрузка:` line) — `new`, `no-workload`, `proven-by-history` or
`confirmed-replacement`; state the basis in the PR together with the flags
used.

The reference environment of a snapshot is wider than the comparability
condition: hardware and platform facts are always part of it, even if the
scenario does not compare them, so a measurement on another CPU, with another
amount of memory or after switching from WSL2 to Linux without virtualization
never silently replaces the public reference, while comparison of results does
not become stricter. A fact absent from the previous marker and a marker
without a recorded workload do not count as matching: the corresponding flag
confirms the replacement.

The three pages are written consistently: no page changes until temporary
copies of all three are prepared; then they are replaced one by one by rename
(RU, EN, zh-CN), and if a replacement fails, the pages already replaced are
restored to their previous text. If restoring fails too, the error names the
updated and the not updated pages. A page changed after the plan was prepared
is not overwritten. A process interrupted between renames leaves translations
with the `i18n-source-git-blob` mark of the previous Russian text, which the
documentation check detects.

The `i18n-source-git-blob` mark of the translations is updated only if the
translation was confirmed for the previous Russian text.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | negative answer: incomparable, no usable baseline, `verify` or `reindex` found problems, snapshot refused |
| `2` | wrong arguments, unknown or ambiguous ID, store or backup inside a Git working copy, backup overlapping the store, baseline equal to the checked result |
| `3` | store, index or catalog unavailable |
| `4` | document rejected by the contract and not accepted; backup transferred incompletely |
| `5` | archive copy written, index not updated: run `reindex` |

## Privacy

An accepted result has already passed the contract check: it holds no absolute
paths, secrets or raw output. The store is the user's local data outside the
repository; it is neither committed nor published. The PR section includes the
commit and input identities (stating them in a PR is allowed and useful); the
documentation does not.

## Tests

The tests (`tests/history.rs`) build a synthetic catalog and results in a
temporary directory: idempotent acceptance and original bytes; refusal of
invalid, sensitive and unknown documents; deleted, damaged, outdated and too-new
index, an index without a required table and with a tampered row; interrupted
write and stray files; damaged archive copy; moving through `backup`/`restore`
into another store and an incomplete or modified copy; a backup inside the
store, through a symbolic link, through a dangling link, through a path in
another case on a case-insensitive file system (the directory is set by
`BENCHMARK_HISTORY_TEST_CASE_INSENSITIVE_DIR`, otherwise the system temporary
directory is checked, and a case-sensitive one skips the check with a message)
and into an ancestor directory without changing the archive; a store inside a Git working copy when
run from an outside directory; baseline rules and proof of different code, a
documentation-only change versus a code change in a real Git repository, an
unknown commit and an explicit baseline; units, zero base and spread; the PR
section in every branch, validated by the real
`.github/scripts/check_pr_body.py`; documentation snapshot with separate
reference hardware, a marker without a recorded workload, workload identity at
an equal size, refusal to take the result being published as proof of the
previous snapshot, a matching block with a different workload, a scenario
without a workload and a failed write of one of the three pages; and the whole
CLI.
The tests read neither `decks/**` nor the user's data directory.
