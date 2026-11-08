#!/usr/bin/env python3
"""Проверить LCOV текущего checkout перед отправкой во внешние сервисы."""

import subprocess
import sys
from pathlib import Path


def check(report: Path, root: Path) -> tuple[int, int, int]:
    root = root.resolve()
    tracked = set(subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, check=True, stdout=subprocess.PIPE,
    ).stdout.decode("utf-8").split("\0"))
    sources = set()
    lines = covered = source_covered = 0
    current = None
    for entry in report.read_text(encoding="utf-8").splitlines():
        if entry.startswith("SF:"):
            path = Path(entry[3:])
            try:
                relative = (root / path).resolve(strict=True).relative_to(root).as_posix()
            except (OSError, ValueError) as error:
                raise ValueError("LCOV содержит несуществующий путь или путь вне checkout") from error
            if relative not in tracked or not relative.startswith("tools/") or not relative.endswith(".rs"):
                raise ValueError("LCOV содержит неотслеживаемый или посторонний файл")
            current = relative
            if "/src/" in relative:
                sources.add(relative)
        elif entry.startswith("DA:"):
            if current is None:
                raise ValueError("Строка DA не относится к файлу")
            fields = entry[3:].split(",")
            if len(fields) < 2 or not fields[0].isdigit() or not fields[1].isdigit():
                raise ValueError("Некорректная запись DA")
            lines += 1
            hit = int(int(fields[1]) > 0)
            covered += hit
            if "/src/" in current:
                source_covered += hit
        elif entry == "end_of_record":
            current = None
    if not sources or not lines or not covered or not source_covered:
        raise ValueError("Нет исходников Rust или выполненных строк в отчёте")
    return len(sources), lines, covered


def main() -> None:
    try:
        sources, lines, covered = check(Path(sys.argv[1]), Path.cwd())
    except (OSError, ValueError, IndexError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"LCOV не прошёл проверку: {error}") from error
    print(f"LCOV: исходников src — {sources}, записей строк — {lines}, выполнено — {covered}")


if __name__ == "__main__":
    main()
