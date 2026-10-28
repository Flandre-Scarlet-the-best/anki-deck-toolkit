# Anki Desktop E2E stand

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)

<!-- i18n-source-git-blob: dd5dc88e75c8122f15b9edd5496a554b1ff704b3 -->

`anki-e2e` is a permanent local E2E stand for the `anki-repo anki` group
([contract](../anki-repo/desktop.md)). A Rust runner on the host builds a
reproducible Docker image with the **official Linux Anki Desktop**, starts
**one isolated container** with an Xvfb virtual display and runs scenarios in it
through the **real** `anki-repo`, Anki's native installer, AnkiConnect and
CrowdAnki. Anki windows, confirmations and pop-up notifications are verified
separately and on a par with functionality.

The stand runs **only on an explicit command**. It is not part of `cargo test`,
CI, build scripts or hooks; the runner's own unit tests do not start Docker.
There are no project shell scripts: orchestration, waits, clicks, screenshots,
the report and cleanup are written in Rust.

## Running

```bash
cargo run -p anki-e2e -- full
```

Scenario selections:

| Command | Scenarios | What it proves |
|---|---|---|
| `full` | all below | a full run on a clean stand |
| `smoke` | `smoke` | Anki discovery, first launch, `doctor` stopped/running, `launch`, `stop` refused without ownership, unsafe endpoint refused |
| `addons` | `addons` | clean Anki without add-ons → `setup --apply` → native installer → second package through the running Anki → no-op repeated setup |
| `patch` | `patch` | `patch install/status/repair/remove`, erased bootstrap, damaged payload, changed upstream, `restore`, loading after restart |
| `crowdanki` | `crowdanki` | real export/import with PNG, the CrowdAnki dialog (OK and Cancel), prevalidation refusals, transfer into a clean base preserving GUIDs, `ready` |
| `process` | `process` | `launch/restart/stop`, refusal while a modal window is open and while busy, process death, a foreign process |
| `errors` | `errors` | refusal in the installer, a disabled add-on, a foreign port listener, a foreign profile, a broken add-on |
| `gui` | `addons`, `crowdanki` | the main native dialogs and notifications |
| `run --scenarios a,b` | list | an arbitrary selection |

Useful options (in order): reproduce a specific release; keep the container
after a failure; watch the screen via `vnc://127.0.0.1:5900`; set resources and
the overall timeout; only select the release and verify the archive; remove the
stand's resources.

```bash
cargo run -p anki-e2e -- full --anki-version 26.09.3
cargo run -p anki-e2e -- addons --keep-container on-failure
cargo run -p anki-e2e -- gui --vnc --keep-container always
cargo run -p anki-e2e -- full --cpus 4 --memory 6g --timeout-minutes 120
cargo run -p anki-e2e -- resolve
cargo run -p anki-e2e -- clean --images --cache
```

`--require-clean` refuses to run with uncommitted changes; without it the
working copy snapshot is still recorded in the report (`dirty`, the list of
paths and the tree SHA-256).

`--timeout-minutes` limits only the scenario execution in the container. It does
not cover the archive download or the image builds: a download that receives
no bytes for 60 s fails, and Ctrl+C promptly cancels both a stalled network read
and `docker build` (SIGINT first, then SIGKILL). The partial archive is removed
and never enters the cache. `--cpus`, `--memory` and `--pids-limit` limit the
scenario container, not the BuildKit image builds.
There is no overall archive-transfer deadline while data keeps arriving;
waiting for HTTP headers is also covered by progress and interruption checks.

## Anki version

- **The default is the latest stable release.** On every run the runner reads
  the GitHub Releases API of `ankitects/anki` **completely** (up to a partial
  page), excludes drafts, prereleases and betas, compares versions numerically
  and picks the highest stable release that has the official
  `anki-<version>-linux-<arch>.tar.zst` artifact. GitHub orders releases by
  publication date, so a later-published backport with a lower number does not
  affect the choice. If the index exceeds the network budget (5 pages of 100),
  the maximum is not considered proven: `ERROR`, with no choice from the part
  that was read. The version number is not pinned in the Dockerfile, the code or
  the configuration.
- If the highest stable release has no Linux artifact, or the network or the index is
  unavailable — `BLOCKED`/`ERROR` with no fallback to an earlier version.
- `--anki-version X` is an explicit regression run; the default does not change.
  A prerelease requires `--allow-prerelease`.
- Integrity: the URL must match the official
  `https://github.com/ankitects/anki/releases/download/<tag>/<asset>`; the
  download's SHA-256 is compared with the `digest` GitHub publishes for the
  release asset. This is the release platform's checksum, not a maintainer
  signature. If no checksum is published for the release the run is blocked;
  `--allow-unverified-archive` permits it, but the report marks the checksum as
  self-computed.
- The cache `target/anki-e2e/cache/anki/<version>/linux-<arch>/<sha256>/` is used
  only after the release is selected and the SHA-256 is re-verified; a damaged
  copy is moved to `*.corrupt-*` and downloaded again.
- Offline mode is explicit only and only with a previously verified artifact:
  `--offline --anki-version X --sha256 <hex> [--anki-archive path]`. Stable
  freshness is not checked in it, and the report shows that.

Compatibility is the **subject** of E2E, not a criterion for choosing an older
Anki. If the newest stable is not in the allowlist
`tools/anki-repo/src/desktop/compatibility.json`, the add-on scenarios end with
`BLOCKED` with the reason `anki_version_unsupported_by_desktop_adapter`, the
actual version and the stage.

The allowlist must not be extended just to get a green run: a new version is
added only after checking upstream changes (Anki code touching the add-on
installer, loading and restart; the bytes of AnkiWeb packages for that version)
and a full `full` run on it. Until the version is added, the report honestly
shows `BLOCKED` for scenarios that need installed add-ons.

## Image and container

Two images labelled `io.github.anki-deck-toolkit.e2e=1`:

- `anki-e2e/anki:<version>-<arch>-<sha12>-<key>` from
  [`docker/anki.Dockerfile`](docker/anki.Dockerfile): Ubuntu 24.04 pinned by
  digest, the official build's dependencies, Xvfb, D-Bus and at-spi2-core, and
  the unpacked official archive (its SHA-256 is verified once more).
  **There are no add-ons in the image**: the initial installation is checked by
  a scenario.
- `anki-e2e/run:<tree12>-<key>` from [`docker/run.Dockerfile`](docker/run.Dockerfile):
  `anki-repo` and `anki-e2e` are built inside Docker (`rust:<MSRV>-bookworm`,
  MSRV from the root `Cargo.toml`) from a snapshot of the working copy files
  that Git tracks or does not ignore. The tree hash is passed into the image
  and reported by the agent; the runner compares it with the snapshot, tying
  the tested binary to the revision under test. The stand's working directory
  (`--work-dir`: caches, snapshots, reports) never enters the snapshot wherever it is, even inside the repository
  without a `.gitignore` entry; a directory with tracked files or one containing
  the repository itself is rejected. The agent reports the tree hash in its
  result, including on an early failure: `PASS` requires a positive match
  (`provenance`), a proven mismatch is the separate error `provenance_mismatch`,
  and missing evidence after an already known failure does not replace its
  primary cause. The shared BuildKit Cargo target cache uses one stable directory
  protected by a cache lock. Before building, legacy SHA-256 snapshot directories
  are removed, then `cargo clean -p` clears artifacts only for `anki-repo` and
  `anki-e2e`. Cargo rebuilds those packages from the current snapshot while
  retaining cached artifacts for unchanged dependencies.

Container: `--init`, user `10001`, `--cap-drop ALL`, `no-new-privileges`,
CPU/memory/PID limits, a bridge network with no published ports (except an
explicit `--vnc` on `127.0.0.1`), **no mounts** — no home, no Docker socket,
no working copy, no `decks/**`, no `.asset-store/`. Evidence is collected with
`docker cp`. The report records the actual `docker inspect` and separate checks
of these boundaries. By default the Docker client gets its own empty
`DOCKER_CONFIG` (`--docker-config inherit` is for a non-default context). Each
scenario works with its own empty `anki -b` base; AnkiConnect listens on
loopback **inside** the container.

After a normal finish the container is removed, and this is verified.
`--keep-container on-failure|always` keeps it for diagnostics; `clean` removes
finished containers and skips containers whose runner is still alive. The owner
is identified by the `…e2e.host-runner` label (machine-id, boot_id and the
runner process start time), not by a PID alone: a reused PID is not a live
runner, containers from an earlier kernel boot are stale, and containers of a
runner on another host are not touched. A container of the earlier scheme
without that label is treated as active only while `anki-e2e` runs under its
PID. Containers left by interrupted runs without the keep mark are removed on
the next run. Ctrl+C gives `INTERRUPTED` and cleanup.

`clean` removes the stand's containers; `--images` also removes images carrying
the stand label; `--cache` removes the working directory's `cache/` (Anki
archives) and `build/` (source snapshots) and the Cargo BuildKit cache mounts
`anki-e2e-cargo-registry` and `anki-e2e-cargo-target`. The cache mounts are
removed selectively (`docker builder prune` filtered by type `exec.cachemount`
and the exact id) and then verified with `docker system df -v`; the daemon's
shared BuildKit layer cache carries no stand label and is not removed (pruning
it is a deliberate `docker builder prune` outside the stand). With
`--docker-config inherit` and a non-default buildx builder the verification sees
only the daemon's own build cache. Any required resource left in place gives
exit code `3`. After building
a new run image the runner removes run images and source snapshots of earlier
revisions; Anki images and the archive cache stay until `clean`. With a
non-default Docker context, pass `clean --docker-config inherit` as you do for
a run; isolated mode refuses to use an active user context. Concurrent commands
serialize access to shared caches and Docker images until the container starts;
the scenarios can then run concurrently.

The snapshot preserves Unix file permissions and symbolic links as links,
without reading their targets. The tree hash includes file bytes, permissions
and link targets.

## Observing windows and notifications

Qt publishes widgets on the AT-SPI accessibility bus
(`QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`). The agent **passively** reads the role,
title, visible text, buttons, selection and coordinates of every top-level
window, and modality and `WM_TRANSIENT_FOR` from X11 properties. There is no
code inside Anki, so windows that Anki shows before add-ons load (language
selection, the `.ankiaddon` installation confirmation) are visible too. Every
new or changed window is immediately captured by a screenshot of the real Xvfb
display and written to `events.jsonl`: step, time, role, title, text, buttons,
modality, classification, the test's action and how long it was shown.

A window is matched against the catalogue `src/agent/catalog.rs` by role, title,
required text fragments and the **exact set of buttons**. A click (XTEST, like a
user action) happens only if the window matched a rule and that rule is
expected by the current step. An unknown window, a known window in another step
(for example "Installed" during a no-op setup) or a repetition beyond what is
expected stops the scenario with `FAIL`, evidence and a reason; such windows are
not closed. Classes: `expected_informational`, `required_confirmation`,
`warning_or_error`, `duplicate_or_misleading`, `unknown_blocking`,
`unexpected_notice`. A screenshot alone is not treated as evidence of the text:
text and modality come from AT-SPI and X11.

A failed AT-SPI or X11 poll does not update the window snapshot: an incomplete
poll does not prove a window closed, and the previous snapshot is not presented
as fresh either. Wait conditions are evaluated only on a fresh snapshot, and the
end of an observation, a command and a scenario requires a successful poll. A
continuous failure longer than 10 s is `ERROR` `ui_observation_lost`;
`events.jsonl` records `observation_degraded` and `observation_restored`.
Windows are polled every 150 ms, while waits on `anki-repo doctor` and
`anki-repo job` start a new CLI at most once per second; the number of starts is
`commands_started` in the scenario result.

## Results

`target/anki-e2e/runs/<id>/` (ignored by Git):

- `report.json` and `report.md` — status `PASS`/`FAIL`/`BLOCKED`/`SKIPPED`/
  `ERROR`/`INTERRUPTED`, version selection mode, detected stable, actually
  installed version, archive URL and SHA-256, image IDs, toolkit commit and tree
  hash, SHA-256 of `anki-repo` in the container, scenarios with checks and UI
  events, the notification matrix ("is it normal", "must it be clicked", "what
  happens without action"), isolation and cleanup;
- `runner.log`, `build-*.log`;
- `agent/scenarios/<scenario>/` — `result.json`, `events.jsonl`,
  `screenshots/`, `commands/` (argv, exit code, JSON output of `anki-repo`),
  Anki logs, copies of the test deck exports.

Mandatory evidence is both reports, `agent-result.json`, each scenario's
`result.json` and `events.jsonl`, the command records and the screenshots of
windows and actions. Losing any of them prevents `PASS` (`evidence_missing`,
`evidence_copy_failed`, `ERROR` for an unwritten report), while an already known
cause of failure remains the primary `stop`; later stops are collected in
`secondary_stops`.

Runner exit codes: `0` PASS, `1` FAIL, `2` BLOCKED/SKIPPED, `3` ERROR,
`130` INTERRUPTED.

## Requirements

Linux or WSL2 with Docker Engine (rootful or rootless) and BuildKit, x86_64 or
aarch64, network access to GitHub, AnkiWeb, Docker Hub and Ubuntu mirrors, and
by default 4 CPUs and 6 GB of memory per container. In the measured environment,
the stand's images, Cargo caches and archive occupied about 6.6 GB of disk;
Docker base images, temporary build data and room for cache growth require
additional space ([measurement details](../../docs/performance/anki-e2e.en.md)).
The first run builds the images; later runs use the cache.

## Performance

Phase timings, CPU, memory and disk of the stand, together with the measurement
method, are on the [performance page](../../docs/performance/anki-e2e.en.md);
the general measurement rules are in the [methodology](../../docs/performance/README.en.md).

## Boundaries

The stand checks **Linux** Anki and does not prove the Windows/WSL integration
from `desktop.md`. It does not read or write the user's `Anki2`, `decks/**` or
`.asset-store/`. The suite is extended with a new scenario in
`src/agent/scenarios.rs` without changing the Docker architecture — for
example, for the future `Anki → input → output → Anki` exchange.
