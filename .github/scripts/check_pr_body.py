#!/usr/bin/env python3
"""Validate the repository's human-authored PR body contract (stdlib only)."""

import argparse
import json
import re
import sys
from pathlib import Path

REQUIRED = (
    "Цель",
    "Область изменений",
    "Реализация",
    "Проверки",
    "Производительность",
    "CI",
    "Готовность",
    "Ограничения",
)
BOT_BRANCHES = {"renovate[bot]": "renovate/", "dependabot[bot]": "dependabot/"}
# ATX-заголовок: закрывающие `#` — часть оформления, а не имени раздела.
HEADING = re.compile(r"^ {0,3}##\s+(.+?)(?:\s+#+)?\s*$")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})")
# Закрывающая граница не допускает info string: ```` ```python ```` — это строка
# содержимого блока, а не его конец.
FENCE_CLOSE = re.compile(r"^ {0,3}(`{3,}|~{3,})[ \t]*$")

PERFORMANCE = "Производительность"
STATUSES = ("измерено", "не применимо", "не измерено")
# Строка «Поле: значение», в том числе пунктом списка и с полужирным именем.
FIELD = re.compile(r"^\s*(?:[-*+]\s+)?(?:\*\*)?(?P<name>[^:|*\n]+?)(?:\*\*)?:(?:\*\*)?(?P<value>.*)$")
PLACEHOLDERS = {
    "", "-", "—", "–", "n/a", "na", "none", "нет", "todo", "tbd", "...", "…",
    "не применимо", "заполнить", "уточнить", "см. ниже", "см. выше", "?",
}
# Поля отчёта `benchmark-history report` для каждого измеренного сценария.
MEASURED_FIELDS = (
    "Сценарий", "Измеренный код", "Нагрузка", "Окружение", "Прогоны",
    "База", "Сопоставимость", "Вывод", "Ограничения", "Источники",
)
NO_EVIDENCE = ("Доказательство производительности", "отсутствует")
PERFORMANCE_HINT = (
    "добавьте после ## Проверки раздел ## Производительность: отчёт `benchmark-history report` "
    "из проверенных результатов, «Статус: не применимо» с конкретной причиной или "
    "«Статус: не измерено» с причиной, диагностикой и строкой "
    "«Доказательство производительности: отсутствует»"
)


# GitHub скрывает HTML-комментарии, поэтому заголовки внутри них не являются
# видимыми разделами body. Внутри код-блока `<!--` — литерал, а не комментарий.


def strip_comments(lines: list[str]) -> list[str]:
    """Remove HTML comments line by line, leaving fenced code blocks intact."""
    result: list[str] = []
    inside = False
    fence_char = ""
    fence_len = 0
    for line in lines:
        if fence_char:
            closing = FENCE_CLOSE.match(line)
            if closing and closing.group(1)[0] == fence_char and len(closing.group(1)) >= fence_len:
                fence_char = ""
            result.append(line)
            continue
        if opening := FENCE.match(line):
            fence_char, fence_len = opening.group(1)[0], len(opening.group(1))
            result.append(line)
            continue
        visible = ""
        rest = line
        while rest:
            if inside:
                end = rest.find("-->")
                if end < 0:
                    rest = ""
                    continue
                inside = False
                rest = rest[end + 3:]
                continue
            start = rest.find("<!--")
            if start < 0:
                visible += rest
                rest = ""
                continue
            visible += rest[:start]
            inside = True
            rest = rest[start + 4:]
        result.append(visible)
    return result


def sections(body: str) -> list[tuple[str, str]]:
    """Extract visible level-two headings and their text."""
    lines = strip_comments(body.replace("\r\n", "\n").replace("\r", "\n").split("\n"))
    found: list[tuple[str, str]] = []
    active: list[str] = []
    fence_char = ""
    fence_len = 0
    for line in lines:
        if fence_char:
            closing = FENCE_CLOSE.match(line)
            if closing and closing.group(1)[0] == fence_char and len(closing.group(1)) >= fence_len:
                fence_char = ""
            continue
        opening = FENCE.match(line)
        if opening:
            token = opening.group(1)
            fence_char, fence_len = token[0], len(token)
            continue
        match = HEADING.match(line)
        if match:
            if found:
                found[-1] = (found[-1][0], "\n".join(active))
            found.append((match.group(1).strip(), ""))
            active = []
        elif found:
            active.append(line)
    if found:
        found[-1] = (found[-1][0], "\n".join(active))
    return found


def fields(content: str) -> dict[str, list[str]]:
    """Значения строк «Имя: значение» вне таблиц по имени поля."""
    found: dict[str, list[str]] = {}
    for line in content.split("\n"):
        if line.lstrip().startswith("|"):
            continue
        match = FIELD.match(line)
        if match:
            value = match.group("value").strip().strip("*").strip()
            found.setdefault(match.group("name").strip(), []).append(value)
    return found


def placeholder(value: str) -> bool:
    text = value.strip().strip("`*_ ").casefold()
    return text in PLACEHOLDERS or re.fullmatch(r"<[^<>]*>", text) is not None


def substantive(values: list[str], minimum: int) -> bool:
    """Есть хотя бы одно конкретное пояснение, а не шаблонная заглушка."""
    return any(not placeholder(v) and len(v.strip()) >= minimum and len(v.split()) >= 3 for v in values)


# Колонки значений проверяемого результата в таблицах `benchmark-history report`:
# с базой — «После: …», без базы — «Значение: …», «Медиана», «Мин–макс».
# Название метрики, scope/источник, состояние «до», разности, оценка и
# пояснения измерением не являются.
MEASUREMENT_COLUMNS = ("После:", "Значение:", "Медиана", "Мин–макс")
# Измеренное значение начинается с числа: «54.41 ms ± 1.26 ms (n=10)», «20 000».
MEASURED_VALUE = re.compile(r"^[-−+]?\d")


def table_cells(row: str) -> list[str]:
    """Ячейки строки таблицы Markdown: делит только неэкранированная «|»."""
    return [c.strip() for c in re.split(r"(?<!\\)\|", row.strip().strip("|"))]


def metric_table(content: str) -> bool:
    """Есть таблица с заголовком «Метрика» и хотя бы одним измеренным значением.

    Значение засчитывается только в колонке значения проверяемого результата
    (`MEASUREMENT_COLUMNS`) и только если оно начинается с числа: цифры в
    названии метрики, scope, версии инструмента, оценке или пояснении вроде
    «n=0: данных нет» строку из прочерков измерением не делают.
    """
    lines = [line.strip() for line in content.split("\n")]
    for n, row in enumerate(lines):
        cells = table_cells(row) if row.startswith("|") else []
        if cells and cells[0] == "Метрика" and n + 1 < len(lines) and lines[n + 1].startswith("|") and set(lines[n + 1]) <= set("|-: "):
            measured = [i for i, c in enumerate(cells) if c.startswith(MEASUREMENT_COLUMNS)]
            # Только строки этой же таблицы: она кончается первой строкой без «|».
            for data in lines[n + 2:]:
                if not data.startswith("|"):
                    break
                values = table_cells(data)
                if any(i < len(values) and not placeholder(values[i]) and MEASURED_VALUE.match(values[i]) for i in measured):
                    return True
    return False


def scenario_blocks(content: str) -> list[tuple[str, str]]:
    """Блоки `### …` отдельных сценариев раздела производительности по порядку."""
    blocks: list[tuple[str, list[str]]] = []
    for line in content.split("\n"):
        if line.startswith("### "):
            blocks.append((line[4:].strip(), []))
        elif blocks:
            blocks[-1][1].append(line)
    return [(heading, "\n".join(lines)) for heading, lines in blocks]


def validate_performance(content: str) -> list[str]:
    """Структура решения о производительности; подлинность цифр проверяет рецензент."""
    prefix = "## Производительность: "
    found = fields(content)
    statuses = [v.strip("`").casefold() for v in found.get("Статус", [])]
    if len(statuses) != 1 or statuses[0] not in STATUSES:
        return [prefix + "нужна ровно одна строка «Статус: измерено», «Статус: не применимо» или «Статус: не измерено»"]
    status = statuses[0]
    errors: list[str] = []
    if status == "не применимо":
        if not substantive(found.get("Причина", []), 40):
            errors.append(prefix + "«не применимо» требует конкретной причины в строке «Причина:» (не короче 40 символов, не шаблон)")
    elif status == "не измерено":
        if not substantive(found.get("Причина", []), 20):
            errors.append(prefix + "«не измерено» требует блокирующей причины в строке «Причина:»")
        if not substantive(found.get("Диагностика", []), 20):
            errors.append(prefix + "«не измерено» требует проверенной диагностики в строке «Диагностика:»")
        if NO_EVIDENCE[1] not in [v.casefold() for v in found.get(NO_EVIDENCE[0], [])]:
            errors.append(prefix + "«не измерено» требует строки «Доказательство производительности: отсутствует»")
    else:
        # Поля одного сценария не закрывают требования другого: каждый
        # измеренный блок `### …` проверяется отдельно; отчёт без подзаголовков —
        # один блок.
        blocks = scenario_blocks(content)
        measured = [(h, b) for h, b in blocks if not h.endswith("— не измерено")] if blocks else [("", content)]
        # Неизмеренный сценарий рядом с измеренными несёт те же строки, что и
        # статус «не измерено»; длина причины не требуется — её пишет исполнитель.
        for heading, block in blocks:
            if not heading.endswith("— не измерено"):
                continue
            block_fields = fields(block)
            for name in ("Причина", "Диагностика"):
                values = block_fields.get(name, [])
                if not values or all(placeholder(v) for v in values):
                    errors.append(prefix + f"неизмеренный сценарий требует строки «{name}:» ({heading})")
            if NO_EVIDENCE[1] not in [v.casefold() for v in block_fields.get(NO_EVIDENCE[0], [])]:
                errors.append(prefix + f"неизмеренный сценарий требует строки «Доказательство производительности: отсутствует» ({heading})")
        if not measured:
            errors.append(prefix + "«измерено» требует хотя бы одного измеренного сценария")
        for heading, block in measured:
            where = f" ({heading})" if heading else ""
            block_fields = fields(block)
            for name in MEASURED_FIELDS:
                values = block_fields.get(name, [])
                if not values or all(placeholder(v) for v in values):
                    errors.append(prefix + f"для измерения нужна строка «{name}:» с фактическими сведениями{where}")
            if not metric_table(block):
                errors.append(prefix + f"для измерения нужна таблица с заголовком «| Метрика |» и строкой значений{where}")
    return errors


def validate(body: str) -> list[str]:
    """Return deterministic errors; empty list means structurally valid."""
    blocks = sections(body)
    names = [heading for heading, _ in blocks]
    errors: list[str] = []
    for name in REQUIRED:
        count = names.count(name)
        if count == 0 and name == PERFORMANCE:
            errors.append(f"Отсутствует обязательный заголовок: ## {name} — {PERFORMANCE_HINT}")
        elif count == 0:
            errors.append(f"Отсутствует обязательный заголовок: ## {name}")
        elif count > 1:
            errors.append(f"Заголовок повторяется: ## {name}")
    if all(name in names for name in REQUIRED):
        positions = [names.index(name) for name in REQUIRED]
        if positions != sorted(positions):
            errors.append("Обязательные разделы расположены не в установленном порядке")
    for name, content in blocks:
        if name in REQUIRED and not content.strip():
            errors.append(f"Пустой обязательный раздел: ## {name}")
        if name in REQUIRED and content.strip().casefold() in {"todo", "tbd", "заполнить", "уточнить"}:
            errors.append(f"Раздел содержит только заглушку: ## {name}")
        elif name == PERFORMANCE and content.strip() and names.count(PERFORMANCE) == 1:
            errors.extend(validate_performance(content))
    if "CodeRabbit review и disposition" in names:
        if names.count("CodeRabbit review и disposition") > 1:
            errors.append("Раздел CodeRabbit повторяется")
        for name, content in blocks:
            if name == "CodeRabbit review и disposition":
                if "| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |" not in content:
                    errors.append("В разделе CodeRabbit отсутствует заголовок накопительной таблицы")
                if not re.search(r"(?m)^\|\s*[1-9]\d*\s*·\s*`[0-9a-f]{7,40}`\s*\|(?:\s*[^|\n\s][^|\n]*\|){5}\s*$", content):
                    errors.append("В разделе CodeRabbit отсутствует строка завершённой итерации")
    return errors


def is_automation_pr(payload: dict) -> bool:
    """Narrow exception for dependency PRs authored by verified bot identities."""
    pr = payload.get("pull_request") or {}
    login = (pr.get("user") or {}).get("login", "")
    branch = (pr.get("head") or {}).get("ref", "")
    return branch.startswith(BOT_BRANCHES.get(login, "\0"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--body-file", type=Path, help="Local Markdown source before PR publication")
    source.add_argument("--event-path", type=Path, help="GitHub pull_request event JSON")
    args = parser.parse_args()
    try:
        if args.body_file is not None:
            body = args.body_file.read_text(encoding="utf-8")
        else:
            payload = json.loads(args.event_path.read_text(encoding="utf-8"))
            if not isinstance(payload, dict) or not isinstance(payload.get("pull_request"), dict):
                raise ValueError("Нет объекта pull_request в событии")
            if is_automation_pr(payload):
                print("PR dependency-бота: формат ручных PR не применяется")
                return 0
            body = payload["pull_request"].get("body") or ""
        problems = validate(body)
    except (OSError, UnicodeError, ValueError) as exc:
        print(f"Не удалось проверить PR body: {exc}", file=sys.stderr)
        return 2
    if problems:
        for problem in problems:
            print(f"::error::{problem}", file=sys.stderr)
        return 1
    print("Структура PR body соответствует контракту")
    return 0


if __name__ == "__main__":
    sys.exit(main())
