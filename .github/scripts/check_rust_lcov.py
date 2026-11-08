#!/usr/bin/env python3
"""Проверить LCOV текущего checkout перед отправкой во внешние сервисы."""

import subprocess
import sys
from pathlib import Path

# Корень checkout берётся из расположения скрипта, а не из аргумента командной
# строки: путь отчёта не выбирает каталог, в котором выполняется git.
ROOT = Path(__file__).resolve().parents[2]


def tracked_paths(root: Path) -> set[str]:
    """Отслеживаемые пути по фактическому индексу Git."""
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, check=True, stdout=subprocess.PIPE,
    )
    return {part for part in result.stdout.decode("utf-8", "surrogateescape").split("\0") if part}


def resolve_report(argument: str, root: Path = ROOT) -> Path:
    """Отчёт обязан существовать внутри checkout: аргумент не адресует чужие файлы."""
    try:
        resolved = Path(argument).resolve(strict=True)
    except OSError as error:
        raise ValueError("нет файла отчёта") from error
    if root not in resolved.parents:
        raise ValueError("отчёт должен лежать внутри checkout")
    return resolved


def source_path(entry: str, root: Path, tracked: set[str]) -> str:
    """Относительный путь исходника из записи SF; отказ вне checkout и вне индекса."""
    try:
        relative = (root / Path(entry)).resolve(strict=True).relative_to(root).as_posix()
    except (OSError, ValueError) as error:
        raise ValueError("LCOV содержит несуществующий путь или путь вне checkout") from error
    if relative not in tracked or not relative.startswith("tools/") or not relative.endswith(".rs"):
        raise ValueError("LCOV содержит неотслеживаемый или посторонний файл")
    return relative


def check(report: Path, root: Path, tracked: set[str]) -> tuple[int, int, int]:
    sources = set()
    lines = covered = source_covered = 0
    current = None
    for entry in report.read_text(encoding="utf-8").splitlines():
        if entry.startswith("SF:"):
            current = source_path(entry[3:], root, tracked)
            if "/src/" in current:
                sources.add(current)
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
        report = resolve_report(sys.argv[1])
        sources, lines, covered = check(report, ROOT, tracked_paths(ROOT))
    except (OSError, ValueError, IndexError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"LCOV не прошёл проверку: {error}") from error
    print(f"LCOV: исходников src — {sources}, записей строк — {lines}, выполнено — {covered}")


if __name__ == "__main__":
    main()