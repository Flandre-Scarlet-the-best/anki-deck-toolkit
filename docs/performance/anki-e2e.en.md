# Performance: `anki-e2e`

[Русский](anki-e2e.md) · **English** · [简体中文](anki-e2e.zh-CN.md)
<!-- i18n-source-git-blob: 599f34cf2b900545270b22aae03d3e257c8d0c1a -->

Performance and resource-usage journal of the Anki Desktop E2E stand
([stand contract](../../tools/anki-e2e/README.en.md)). The general measurement
rules and source accuracy are in the [methodology](README.en.md).

## What is measured

- **Runner phases** from the `runner.log` timestamps: release selection (the
  GitHub index request, the archive download or the cache SHA-256
  re-verification), source snapshot, Anki image build, run image build
  (`cargo build` in Docker), scenarios, cleanup. Each scenario's duration and
  the number of `anki-repo` starts (`commands_started`) come from `report.json`.
- **Scenario container** — the cgroup v2 of its scope: `memory.peak` and
  `usage_usec`.
- **Image builds.** BuildKit steps run outside the `docker.service` cgroup and
  have no per-run cgroup of their own. Build cost is therefore measured for the
  whole host (`/proc/stat`, `MemAvailable`) — an upper bound.
- **Runner on the host** — `VmHWM` and CPU of the `anki-e2e` process.
- **Disk** — `cache/`, `build/`, `runs/` of the working directory, the stand's
  images and the BuildKit cache mounts `anki-e2e-cargo-*`.

## How to measure

Cache states for this module:

- *cold* — after `clean --images --cache`: the stand's images, the Anki
  archive, source snapshots and Cargo cache mounts are removed. The daemon's
  BuildKit layer cache and the `ubuntu`/`rust` base images remain (they are
  shared, see the methodology);
- *warm* — a repeated `full` with unchanged sources;
- *incremental* — after the source snapshot changes (for example, a new
  non-ignored file): only the run image is rebuilt.

Run:

```bash
cargo build -p anki-e2e --locked
cargo run -p anki-e2e -- clean --images --cache
cargo run -p anki-e2e -- full
```

During the run, read the stand container's cgroup and the runner's `/proc`
once a second. The container is removed at the end of the run, so
`memory.peak` must be sampled rather than read after it finishes. The scope path
below is for the `systemd` cgroup driver; with the `cgroupfs` driver it is
`/sys/fs/cgroup/docker/<ID>`.

```bash
docker ps --filter label=io.github.anki-deck-toolkit.e2e=1 --no-trunc --format '{{.ID}}'
cat /sys/fs/cgroup/system.slice/docker-<ID>.scope/memory.peak
grep usage_usec /sys/fs/cgroup/system.slice/docker-<ID>.scope/cpu.stat
grep VmHWM /proc/<PID>/status
```

Disk after the run:

```bash
du -sb target/anki-e2e/cache target/anki-e2e/build target/anki-e2e/runs
docker image ls --filter label=io.github.anki-deck-toolkit.e2e=1
docker system df -v
```

## Journal

### 2026-10-26 · `anki-e2e` 0.1.0 · Anki 26.09.3 · `full`

- **Environment:** AMD Ryzen 7 5800X3D (8 cores, 16 threads), 16 GB RAM
  (15.6 GB visible in WSL), WSL2 with kernel 6.18 and Arch Linux, Docker Engine
  29.9.2 inside WSL (systemd, cgroup v2, overlayfs), `rustc` 1.99.1. Default
  container limits: `--cpus 4 --memory 6g`. The version was selected as the
  latest stable release. The working copy had no changes; the incremental run
  added one non-ignored file.
- **States:** one cold, two warm and one incremental `full` in a row. All four
  `PASS`, 129 checks each.

Working-copy clarification: the cold and both warm runs had no changes.
One temporary non-ignored file was added after the second warm run, before
starting the incremental run. It remained present throughout that measurement
and entered the source snapshot (293 files instead of 292). It was removed
immediately after the run finished, restoring a clean working copy.

Time per phase, seconds:

| Phase | Cold | Warm 1 | Warm 2 | Incremental |
|---|---|---|---|---|
| Release selection and archive | 13 (191 MB download) | 8 | 9 | 8 |
| Source snapshot (292–293 files) | 1 | 0 | 1 | 1 |
| Anki image | 16 | 0 | 0 | 0 |
| Run image (`cargo build`) | 155 | 1 | 0 | 30 |
| Scenarios | 222 | 221 | 227 | 220 |
| **Total** | **406.7** | **230.0** | **236.5** | **259.3** |

Scenarios (range over the four runs):

| Scenario | Time, s | `anki-repo` starts | Checks | Windows |
|---|---|---|---|---|
| `smoke` | 11.6–12.0 | 6 | 13 | 2 |
| `addons` | 41.4–43.0 | 8 | 15 | 6 |
| `patch` | 50.4–52.6 | 30 | 32 | 0 |
| `crowdanki` | 38.8–40.6 | 28–30 | 37 | 3 |
| `process` | 23.1–24.0 | 15 | 14 | 1 |
| `errors` | 40.0–41.3 | 14 | 18 | 5 |

Resources:

| Metric | Value | Source |
|---|---|---|
| Container: memory peak | 499–505 MiB with a 6 GiB limit | `memory.peak` |
| Container: CPU per run | 203–212 s | `usage_usec` |
| Container: average load | 0.92–0.94 cores with a limit of 4 | `usage_usec` / time |
| Container: peak over 1 s | 1.80–1.88 cores | sample difference |
| Host during scenarios | ≈1.1 cores on average, 242–251 core·s (≈40 s above the container: dockerd, `docker exec`, WSL) | `/proc/stat` |
| Cold image build, host | 471 core·s, 2.75 cores on average, peak 15.5 of 16; host memory up to +2.9 GiB | `/proc/stat`, `MemAvailable` |
| Incremental build, host | 40 core·s, peak 4.6 cores; host memory up to +0.9 GiB | `/proc/stat`, `MemAvailable` |
| dockerd during the build | 0.9–1.0 GiB, ≈0.05 cores (BuildKit steps are not included) | `docker.service` cgroup |
| Runner: RSS peak | 27–28 MiB | `VmHWM` |
| Runner: CPU | 5.3–6.7 s own + 0.7–3.5 s of waited-for children | `/proc/<pid>/stat` |
| Unit tests `cargo test -p anki-e2e` | 0.63–0.65 s for 64 tests (already built) | run time |

Disk:

| What | Size |
|---|---|
| Anki archive in `cache/` | 191 MB |
| Source snapshots in `build/` | 17.7 MB |
| Evidence of one `full` in `runs/` | 2.0–2.1 MB, 62 PNG |
| Run image | 2.46 GB (includes the 1.89 GB Anki image layers; do not sum) |
| Cache mount `anki-e2e-cargo-target` | 3.41 GB after the cold build, 3.65 GB after the incremental one |
| Cache mount `anki-e2e-cargo-registry` | 319 MB |

Conclusions:

- In the warm state ≈96 % of the `full` time is spent in the scenarios
  themselves. Runner preparation takes under 10 s, almost all of it release
  selection; its split into the GitHub request and the SHA-256 re-verification
  was not measured.
- The spread between warm runs is about 3 % (230.0 and 236.5 s).
- The container limits are respected with a large margin: memory peak ≈0.5 GiB
  of 6, CPU at most 1.9 of 4 cores. This is an observation from one
  environment, not a reason to change the defaults.
- The cold run image build is the most expensive phase in CPU and memory.
  `--cpus`/`--memory` do not apply to it: Cargo uses every host core (peak 15.5
  of 16) and up to 2.9 GiB of host memory. This matches the stand README.
- The `anki-e2e-cargo-target` cache mount grew by 0.24 GB after one incremental
  rebuild. One observation is not enough to conclude unbounded growth: check it
  in the next entry with a series of several snapshot changes. Cleanup is
  `clean --cache`.
- `full` starts 101–103 `anki-repo` processes; the `doctor` and `job` waits do
  so at most once per second.

Not measured: a fully cold system without the layer cache and base images;
network traffic other than the Anki archive; disk I/O; other environments
(plain Linux, Docker Desktop, aarch64); BuildKit step resources separately from
the rest of the system.
