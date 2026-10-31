# Measurement contract: `benchmark-contract`

[Русский](../../ru/benchmark-contract/docs/ru/README.md) · **English** · [简体中文](../../zh-CN/benchmark-contract/docs/ru/README.md)

> **Translation notice:** This translation is maintained on a best-effort basis and may be outdated. The [Russian documentation](../../ru/benchmark-contract/docs/ru/README.md) is authoritative. Corrections and translation contributions are welcome.

`benchmark-contract` is a library and CLI that load the `benchmarks/`
benchmark scenario catalog, validate declarations and measurement results,
import Hyperfine 2.0 exports and answer whether two results are comparable.
The meaning of fields, profiles, metrics and the rules for registering a
scenario are in the [catalog guide](../performance/benchmarks.md);
general measurement rules are in the [methodology](../performance/docs/ru/README.md).

The tool runs and measures nothing: it reads the catalog TOML, Cargo manifests
and result JSON. It writes no files, uses no network and does not touch decks,
media, Anki profiles, `decks/**` or `.asset-store/`. Catalog series are
executed by [`benchmark-runner`](../benchmark-runner/docs/ru/README.md), which uses
this library.

## Commands

```bash
cargo run -p benchmark-contract -- validate
cargo run -p benchmark-contract -- list
cargo run -p benchmark-contract -- show anki-repo.validate
cargo run -p benchmark-contract -- preflight anki-e2e.full
cargo run -p benchmark-contract -- check-result result.json
cargo run -p benchmark-contract -- check-result --no-catalog result.json
cargo run -p benchmark-contract -- compare before.json after.json
cargo run -p benchmark-contract -- vocabulary
```

| Command | What it does |
|---|---|
| `validate` | loads the catalog and validates every declaration; prints diagnostics |
| `list` | scenarios: ID, revision, kind, status, profiles |
| `show <id>` | the typed declaration as the loader understood it |
| `preflight <id>` | whether the scenario can be measured here: an OS from `platforms`, `PATH`, cgroup v2, `perf_event_paranoid`; the network and the Docker daemon are not checked |
| `check-result <file>…` | validates results; by default also against the catalog declaration |
| `compare <a> <b>` | comparability of two valid results of the same scenario |
| `vocabulary` | registry of metrics, scopes, sources, units and run limits |

Global options: `--catalog <dir>` — the directory with `catalog.toml` (by
default `benchmarks/` of the nearest parent of the current directory);
`--json` — stable JSON instead of text. Text diagnostics go to stderr.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | the catalog or results are valid; `preflight` — the platform is supported and every checked dependency is available; `compare` — the results are comparable |
| `1` | contract errors found; `preflight` — the platform is unsupported, or a dependency is missing or restricted; `compare` — the results are not comparable or invalid |
| `2` | invalid command-line arguments |
| `3` | the catalog is not found, a file cannot be read, no scenario has the given ID (`show`, `preflight`), or the catalog is invalid for a command that needs a valid catalog |

## Diagnostics

Each entry has a level (`error` or `warning`), a stable machine code, the file
relative to the catalog root or the document name, the scenario ID and the
field path (`profile[1].prepare[0].program.name`, `runs[2].values.<key>`).
Warnings do not make the catalog invalid. An unsupported `schema_version` is the
error `unsupported_schema_version`, reported before any other field is parsed.

## Library

| Module | Purpose |
|---|---|
| `catalog` | discovery, loading and semantic validation of the catalog |
| `scenario` | declaration types and shape checks with field paths |
| `vocabulary` | closed vocabulary, metric registry, run limits |
| `result` | result types and validation, aggregate recomputation |
| `hyperfine` | parsing and import of Hyperfine 2.0 `--export-json` |
| `comparability` | comparability rules |
| `preflight` | read-only platform and dependency availability checks |

## Checks

Tests build their own catalogs from the example fixtures in `tests/fixtures/`
and also check the real `benchmarks/`; they need no Docker, Anki or Hyperfine.
They are part of `cargo test --workspace --locked`.
