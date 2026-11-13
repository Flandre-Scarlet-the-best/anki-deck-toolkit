#!/usr/bin/env python3
"""Проверяет Rust-обновления штатным движком Renovate с параллельными фикстурами."""

import concurrent.futures
import json
import os
import re
import subprocess
import tempfile
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RENOVATE = os.environ.get("RENOVATE_BINARY", str(ROOT / "node_modules/.bin/renovate"))
PIN = re.compile(r"\d+\.\d+\.\d+\Z")
RUST_MANAGERS = ["rust-toolchain", "custom.regex"]


@dataclass(frozen=True)
class Scenario:
    name: str
    root: Path
    mode: str
    managers: list[str]
    expected: str
    expect_custom: bool = True
    expect_update: bool = False


def run_renovate(scenario: Scenario) -> tuple[Scenario, dict, float]:
    env = dict(os.environ)
    env.update({
        "LOG_LEVEL": "debug",
        "LOG_FORMAT": "json",
        "RENOVATE_PLATFORM": "local",
        "RENOVATE_ENABLED_MANAGERS": json.dumps(scenario.managers),
    })
    started = time.monotonic()
    result = subprocess.run(
        [RENOVATE, "--platform=local", f"--dry-run={scenario.mode}"],
        cwd=scenario.root, env=env, capture_output=True, text=True, timeout=240,
    )
    elapsed = time.monotonic() - started
    if result.returncode:
        raise AssertionError(
            f"{scenario.name}: Renovate ({scenario.mode}) завершился с кодом {result.returncode}:\n"
            f"{(result.stderr + result.stdout)[-10000:]}"
        )
    events = []
    for line in (result.stdout + "\n" + result.stderr).splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)
    for target in ("packageFiles with updates", "packageFiles", "Extracted dependencies"):
        for event in reversed(events):
            if event.get("msg") != target:
                continue
            package_files = event.get("packageFiles") if target == "Extracted dependencies" else event.get("config")
            if isinstance(package_files, dict):
                return scenario, package_files, elapsed
    raise AssertionError(
        f"{scenario.name}: Renovate не опубликовал extraction evidence; "
        f"последние события: {[event.get('msg') for event in events[-25:]]}"
    )


def select(package_files: dict, manager: str) -> list[tuple[str, dict]]:
    entries = []
    for name, files in package_files.items():
        if manager == "regex" and name not in ("regex", "custom.regex"):
            continue
        if manager != "regex" and name != manager:
            continue
        for package in files:
            for dep in package["deps"]:
                if dep.get("datasource") == "rust-version":
                    entries.append((package["packageFile"], dep))
    return entries


def verify(scenario: Scenario, package_files: dict) -> dict:
    native = select(package_files, "rust-toolchain")
    custom = select(package_files, "regex")
    assert len(native) == 1 and native[0][0] == "rust-toolchain.toml", (scenario.name, native)
    assert len(custom) == int(scenario.expect_custom), (scenario.name, custom)
    for path, dep in native + custom:
        assert dep.get("depName") == "rust", (scenario.name, path, dep)
        assert dep.get("currentValue") == scenario.expected, (scenario.name, path, dep)
        assert dep.get("skipReason") is None, (scenario.name, path, dep)
    if scenario.name == "repository":
        assert "cargo" in package_files, "Renovate перестал извлекать Cargo"
        assert any(p.get("deps") for p in package_files["cargo"]), "Нет crate-зависимостей"
    if not scenario.expect_update:
        return {}
    assert len(custom) == 1, "Группировка требует обе Rust-записи"
    updates = {}
    for path, dep in native + custom:
        candidates = [
            update for update in dep.get("updates", [])
            if update.get("updateType") in ("major", "minor", "patch")
        ]
        assert len(candidates) == 1, (scenario.name, path, candidates)
        updates[path] = candidates[0]
    assert set(updates) == {"Cargo.toml", "rust-toolchain.toml"}, (scenario.name, updates)
    first, second = updates["Cargo.toml"], updates["rust-toolchain.toml"]
    for update in (first, second):
        assert PIN.fullmatch(update["newValue"]), update
        assert PIN.fullmatch(update["newVersion"]), update
    assert first["newValue"] == second["newValue"], updates
    assert first["branchName"] == second["branchName"], updates
    return {"version": first["newValue"], "branch": first["branchName"]}


def prepare(directory: Path, name: str, config: str, cargo: str, toolchain: str) -> Path:
    target = directory / name
    target.mkdir()
    (target / "renovate.json").write_text(config, encoding="utf-8")
    (target / "Cargo.toml").write_text(cargo, encoding="utf-8")
    (target / "rust-toolchain.toml").write_text(toolchain, encoding="utf-8")
    return target


def main() -> None:
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    toolchain = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    config_text = (ROOT / "renovate.json").read_text(encoding="utf-8")
    current = tomllib.loads(cargo)["workspace"]["package"]["rust-version"]
    assert isinstance(current, str) and PIN.fullmatch(current), "MSRV не является точным X.Y.Z"
    assert tomllib.loads(toolchain)["toolchain"]["channel"] == current, "MSRV и rustup различаются"
    config = json.loads(config_text)
    assert config["automerge"] is False
    assert config["prConcurrentLimit"] == 4 and config["prHourlyLimit"] == 2
    assert config["commitHourlyLimit"] == 2

    historical = "1.88.0"
    fixture_cargo = cargo.replace(f'rust-version = "{current}"', f'rust-version = "{historical}"', 1)
    fixture_toolchain = toolchain.replace(f'channel = "{current}"', f'channel = "{historical}"', 1)
    assert f'rust-version = "{historical}"' in fixture_cargo
    assert f'channel = "{historical}"' in fixture_toolchain

    commented = fixture_cargo.replace(
        f'rust-version = "{historical}"',
        f'# MSRV {historical}\nrust-version = "{historical}"', 1,
    )
    multiline = fixture_cargo.replace(
        f'rust-version = "{historical}"',
        f'keywords = [\n  "cli",\n]\nrust-version = "{historical}"', 1,
    )
    assert tomllib.loads(multiline)["workspace"]["package"]["keywords"] == ["cli"]

    def metadata(*, indentation: str, include_msrv: bool) -> str:
        value = f'rust-version = "{historical}"\n' if include_msrv else ""
        return (
            '[workspace]\nresolver = "3"\n\n'
            '[workspace.package]\n'
            f'{value}edition = "2024"\n\n'
            f'{indentation}[workspace.metadata]\n'
            'rust-version = "9.9.9"\n'
        )

    malformed = fixture_cargo.replace(
        f'rust-version = "{historical}"',
        f'rust-version = "{historical}"\nrust-version = "1.89.0"', 1,
    )
    try:
        tomllib.loads(malformed)
    except tomllib.TOMLDecodeError:
        pass
    else:
        raise AssertionError("Двойной workspace rust-version прошёл TOML-парсер")

    with tempfile.TemporaryDirectory(prefix="renovate-rust-fixtures-") as tmp:
        folder = Path(tmp)
        tests = [
            Scenario("repository", ROOT, "extract", ["cargo", *RUST_MANAGERS], current),
            Scenario("historical", prepare(folder, "historical", config_text, fixture_cargo, fixture_toolchain),
                     "lookup", RUST_MANAGERS, historical, expect_update=True),
            Scenario("comment", prepare(folder, "comment", config_text, commented, fixture_toolchain),
                     "extract", RUST_MANAGERS, historical),
            Scenario("multiline", prepare(folder, "multiline", config_text, multiline, fixture_toolchain),
                     "extract", RUST_MANAGERS, historical),
            Scenario("metadata", prepare(folder, "metadata", config_text, metadata(indentation="", include_msrv=True), fixture_toolchain),
                     "extract", RUST_MANAGERS, historical),
            Scenario("indented-metadata", prepare(folder, "indented-metadata", config_text, metadata(indentation="  ", include_msrv=True), fixture_toolchain),
                     "extract", RUST_MANAGERS, historical),
            Scenario("metadata-only", prepare(folder, "metadata-only", config_text, metadata(indentation="  ", include_msrv=False), fixture_toolchain),
                     "extract", RUST_MANAGERS, historical, expect_custom=False),
            Scenario("missing-msrv", prepare(folder, "missing-msrv", config_text, fixture_cargo.replace(
                f'rust-version = "{historical}"', f'minimum-rust-version = "{historical}"', 1,
            ), fixture_toolchain), "extract", RUST_MANAGERS, historical, expect_custom=False),
        ]
        # Изолированные каталоги: сценарии не изменяют ни одну общую фикстуру.
        # Ограничиваем одновременные процессы Renovate, чтобы не перегрузить runner.
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            futures = {pool.submit(run_renovate, scenario): scenario.name for scenario in tests}
            results = {}
            for future in concurrent.futures.as_completed(futures):
                scenario, package_files, duration = future.result()
                proof = verify(scenario, package_files)
                results[scenario.name] = (package_files, proof)
                print(f"Renovate [{scenario.name}]: PASS, {duration:.1f} с", flush=True)

        original = select(results["comment"][0], "regex")[0][1]["replaceString"]
        latest = results["historical"][1]["version"]
        assert f"# MSRV {historical}" not in original, original
        assert f'rust-version = "{historical}"' in original, original
        replaced = commented.replace(original, original.replace(historical, latest, 1), 1)
        assert f"# MSRV {historical}" in replaced
        assert f'rust-version = "{latest}"' in replaced
        assert f"# MSRV {latest}" not in replaced
        print(
            f"Renovate: оба pin обновляются до {latest} в единой ветке "
            f"{results['historical'][1]['branch']}", flush=True,
        )
    print("Renovate: все реальные extraction/lookup и отрицательные сценарии PASS", flush=True)


if __name__ == "__main__":
    main()
