#!/usr/bin/env python3
"""Проверка целостности локальных ссылок в отслеживаемом Markdown.

Только стандартная библиотека, без сети и без LLM. Проверяется одно свойство:
существующая относительная ссылка ведёт в существующий файл или каталог
репозитория. Наличие, актуальность и синхронность переводов не проверяются:
переводы необязательны, а ссылка на русский оригинал — допустимая цель.

Внешние адреса (`https:`, `mailto:` и любые другие схемы), чистые якоря
`#раздел`, fenced-блоки и встроенный код пропускаются. Фрагмент и строка
запроса отбрасываются, процентное кодирование раскрывается.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)+?\1")
# Текст ссылки может переноситься на следующую строку, но не через пустую.
INLINE = re.compile(r"!?\[(?:[^\[\]\n]|\n(?![ \t]*\n)|\[[^\[\]\n]*\])*\]\(\s*(<[^<>\n]*>|[^\s()]+(?:\([^\s()]*\)[^\s()]*)*)")
REFERENCE = re.compile(r"^ {0,3}\[[^\]\n]+\]:[ \t]*(<[^<>\n]*>|\S+)", re.MULTILINE)


def visible_text(text: str) -> str:
    """Текст без fenced-блоков и встроенного кода; номера строк сохраняются."""
    lines = []
    fence = ""
    for line in text.splitlines():
        match = FENCE.match(line)
        if fence:
            if match and match.group(1)[0] == fence[0] and len(match.group(1)) >= len(fence) and not match.group(2).strip():
                fence = ""
            lines.append("")
            continue
        if match and not (match.group(1)[0] == "`" and "`" in match.group(2)):
            fence = match.group(1)
            lines.append("")
            continue
        lines.append(CODE_SPAN.sub("", line))
    return "\n".join(lines)


def destinations(text: str) -> list[tuple[int, str]]:
    """Адреса ссылок с номером строки, где адрес записан."""
    visible = visible_text(text)
    found = []
    for pattern in (INLINE, REFERENCE):
        for match in pattern.finditer(visible):
            number = visible.count("\n", 0, match.start(1)) + 1
            found.append((number, match.group(1).removeprefix("<").removesuffix(">")))
    return sorted(found)


def local_target(root: Path, page: Path, address: str) -> Path | None:
    """Путь цели локальной ссылки; `None` — ссылка не локальная или без пути."""
    parsed = urlsplit(address)
    if parsed.scheme or parsed.netloc:
        return None
    relative = unquote(parsed.path)
    if not relative:
        return None
    if relative.startswith("/"):
        return root / relative.lstrip("/")
    return page.parent / relative


def check_page(root: Path, page: Path) -> list[str]:
    errors = []
    text = page.read_text(encoding="utf-8")
    name = page.relative_to(root).as_posix()
    resolved_root = root.resolve()
    for number, address in destinations(text):
        target = local_target(root, page, address)
        if target is None:
            continue
        resolved = target.resolve()
        if not resolved.is_relative_to(resolved_root):
            errors.append(f"{name}:{number}: ссылка ведёт за пределы репозитория: {address}")
        elif not resolved.exists():
            errors.append(f"{name}:{number}: цель ссылки не существует: {address}")
    return errors


def tracked_markdown(root: Path) -> list[Path]:
    output = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z", "--", "*.md"],
        check=True,
        capture_output=True,
    ).stdout
    paths = [root / name for name in output.decode("utf-8").split("\0") if name]
    return sorted(path for path in paths if path.is_file())


def check_repository(root: Path) -> list[str]:
    errors = []
    for page in tracked_markdown(root):
        errors.extend(check_page(root, page))
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    root = args.root.resolve()
    errors = check_repository(root)
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        print(f"Битых локальных ссылок: {len(errors)}", file=sys.stderr)
        return 1
    print("Локальные ссылки Markdown ведут в существующие места.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
