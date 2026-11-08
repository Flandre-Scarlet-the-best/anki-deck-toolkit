#!/usr/bin/env python3
"""Проверить LCOV рабочей копии и подготовить совместимый отчёт для Sonar."""

import re
import subprocess
import sys
from pathlib import Path

from sonar_scope import read_properties, scope

# Корень рабочей копии берётся из расположения скрипта, а не из аргумента командной
# строки: путь отчёта не выбирает каталог, в котором выполняется git.
ROOT = Path(__file__).resolve().parents[2]


def tracked_paths(root: Path) -> set[str]:
    """Отслеживаемые пути по фактическому индексу Git."""
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, check=True, stdout=subprocess.PIPE,
    )
    return {part for part in result.stdout.decode("utf-8", "surrogateescape").split("\0") if part}


def resolve_report(argument: str, root: Path = ROOT) -> Path:
    """Отчёт обязан существовать внутри рабочей копии: аргумент не адресует чужие файлы."""
    try:
        resolved = Path(argument).resolve(strict=True)
    except OSError as error:
        raise ValueError("нет файла отчёта") from error
    if root not in resolved.parents:
        raise ValueError("отчёт должен лежать внутри рабочей копии")
    return resolved


def source_path(entry: str, root: Path, tracked: set[str]) -> str:
    """Относительный путь исходника из записи SF; отказ вне рабочей копии и вне индекса."""
    try:
        relative = (root / Path(entry)).resolve(strict=True).relative_to(root).as_posix()
    except (OSError, ValueError) as error:
        raise ValueError("LCOV содержит несуществующий путь или путь вне рабочей копии") from error
    if relative not in tracked or not relative.startswith("tools/") or not relative.endswith(".rs"):
        raise ValueError("LCOV содержит неотслеживаемый или посторонний файл")
    return relative


def production_paths(root: Path, tracked: set[str]) -> set[str]:
    """Классификация из настроек самого сканера, без второго списка исключений."""
    properties = read_properties(root / "sonar-project.properties")
    return {path for path in tracked
            if path.endswith(".rs") and path.startswith("tools/") and scope(properties, path)[0]}


def check(report: Path, root: Path, tracked: set[str]) -> tuple[int, int, int]:
    production = production_paths(root, tracked)
    sources = set()
    lines = covered = source_covered = 0
    current = None
    for entry in report.read_text(encoding="utf-8").splitlines():
        if entry.startswith("SF:"):
            current = source_path(entry[3:], root, tracked)
            if current in production:
                sources.add(current)
        elif entry.startswith("DA:"):
            if current is None:
                raise ValueError("Строка DA не относится к файлу")
            fields = entry[3:].split(",")
            if (len(fields) < 2 or not re.fullmatch(r"[0-9]+", fields[0])
                    or not re.fullmatch(r"[0-9]+", fields[1]) or int(fields[0]) == 0):
                raise ValueError("Некорректная запись DA")
            lines += 1
            hit = int(int(fields[1]) > 0)
            covered += hit
            if current in production:
                source_covered += hit
        elif entry == "end_of_record":
            current = None
    if not sources or not lines or not covered or not source_covered:
        raise ValueError("Нет производственных исходников Rust с выполненными строками в отчёте")
    return len(sources), lines, covered


def write_sonar_report(report: Path, output: Path) -> int:
    """Адаптер диапазона DA для Sonar; исходные счётчики для Codecov не меняются.

    Импортёр Sonar отвергает числа больше int32. Насыщение сохраняет все строки,
    их координаты и различие нулевого/положительного покрытия, а не исключает
    потерянные строки. Остальные записи LCOV сохраняются без изменения значений.
    Вызывается только после успешного check; старый результат не перезаписывается.
    """
    clipped = 0

    def clamp(match: re.Match) -> str:
        nonlocal clipped
        hits = int(match[2])
        if hits <= 2**31 - 1:
            return match[0]
        clipped += 1
        return f"{match[1]}{2**31 - 1}"

    contents = re.sub(r"(?m)^(DA:[0-9]+,)([0-9]+)", clamp, report.read_text(encoding="utf-8"))
    with output.open("x", encoding="utf-8") as stream:
        stream.write(contents)
    return clipped


def main() -> None:
    try:
        report = resolve_report(sys.argv[1], ROOT)
        sources, lines, covered = check(report, ROOT, tracked_paths(ROOT))
        clipped = write_sonar_report(report, ROOT / "lcov-sonar.info")
    except (OSError, ValueError, IndexError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"LCOV не прошёл проверку: {error}") from error
    print(f"LCOV: производственных исходников — {sources}, записей строк — {lines}, выполнено — {covered}")
    print(f"LCOV для Sonar: ограничено счётчиков DA — {clipped}; покрытие строк не изменено")


if __name__ == "__main__":
    main()