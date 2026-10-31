#!/usr/bin/env python3
"""Проверка контракта описания ручного PR репозитория (только стандартная библиотека)."""

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
MEASURED_FIELDS = ("Сценарий", "База", "Кандидат", "Результат", "Вывод", "Регрессии")
MAX_BODY_LENGTH = 10_000
MAX_PERFORMANCE_LENGTH = 2_200
MAX_SIMPLE_PERFORMANCE_LENGTH = 650
NO_EVIDENCE = ("Доказательство производительности", "отсутствует")
PERFORMANCE_HINT = (
    "добавьте после ## Проверки раздел ## Производительность: компактную сводку проверенных измерений "
    "из проверенных результатов, «Статус: не применимо» с конкретной причиной или "
    "«Статус: не измерено» с причиной, диагностикой и строкой "
    "«Доказательство производительности: отсутствует»"
)


# GitHub скрывает HTML-комментарии, поэтому заголовки внутри них не являются
# видимыми разделами body. Внутри код-блока `<!--` — литерал, а не комментарий.


def strip_comments(lines: list[str]) -> list[str]:
    """Убрать HTML-комментарии построчно, не трогая содержимое код-блоков."""
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
    """Видимые заголовки второго уровня и текст их разделов без код-блоков."""
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


def table_cells(row: str) -> list[str]:
    """Ячейки строки таблицы Markdown: делит только неэкранированная «|»."""
    return [c.strip() for c in re.split(r"(?<!\\)\|", row.strip().strip("|"))]


CODERABBIT = "CodeRabbit review и disposition"
CODERABBIT_HEADER = ("Итерация / reviewed HEAD", "Уровень", "Путь", "Влияние", "Решение", "Исправление")
# Ячейка строки-разделителя GFM: дефисы с необязательным выравниванием «:».
DELIMITER_CELL = re.compile(r"^:?-+:?$")
# Первая ячейка завершённой итерации: номер и reviewed HEAD в code span.
ITERATION_CELL = re.compile(r"^[1-9]\d*\s*·\s*`[0-9a-f]{7,40}`$")


def table_row(line: str) -> list[str] | None:
    """Ячейки строки таблицы GFM; None — строка не может быть строкой таблицы.

    Строка с отступом от четырёх пробелов — код-блок, пустая строка завершает
    таблицу. Внешние «|» необязательны, ячейки делит только неэкранированная «|».
    """
    if not line.strip() or len(line) - len(line.lstrip(" ")) > 3 or not re.search(r"(?<!\\)\|", line):
        return None
    return table_cells(line)


def validate_coderabbit(content: str) -> list[str]:
    """Проверить, что раздел содержит связанную накопительную таблицу GFM.

    Заголовок, строка-разделитель с тем же числом столбцов и строки данных должны
    идти подряд: только так GitHub собирает их в одну таблицу. Каждая строка
    данных обязана иметь все шесть столбцов, и хотя бы одна из них описывает
    завершённую итерацию с номером, reviewed HEAD и заполненными ячейками.
    """
    lines = content.split("\n")
    header = [n for n, line in enumerate(lines) if table_row(line) == list(CODERABBIT_HEADER)]
    if not header:
        return ["В разделе CodeRabbit отсутствует заголовок накопительной таблицы"]
    if len(header) > 1:
        return ["В разделе CodeRabbit заголовок накопительной таблицы повторяется: нужна одна таблица"]
    start = header[0]
    delimiter = table_row(lines[start + 1]) if start + 1 < len(lines) else None
    if (
        delimiter is None
        or len(delimiter) != len(CODERABBIT_HEADER)
        or not all(DELIMITER_CELL.match(cell) for cell in delimiter)
    ):
        return [
            "В разделе CodeRabbit за заголовком таблицы нет строки-разделителя "
            f"из {len(CODERABBIT_HEADER)} столбцов (`| --- | … |`): GitHub не соберёт таблицу"
        ]
    errors: list[str] = []
    completed = False
    for line in lines[start + 2:]:
        row = table_row(line)
        if row is None:
            break
        if len(row) != len(CODERABBIT_HEADER):
            errors.append(
                f"Строка таблицы CodeRabbit содержит {len(row)} столбцов вместо {len(CODERABBIT_HEADER)}: {line.strip()}"
            )
            continue
        if ITERATION_CELL.match(row[0]) and all(row[1:]):
            completed = True
    if not completed:
        errors.append("В разделе CodeRabbit отсутствует строка завершённой итерации")
    return errors


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
    """Проверить компактный вывод; достоверность результатов требует отдельной проверки."""
    prefix = "## Производительность: "
    errors: list[str] = []
    if len(content) > MAX_PERFORMANCE_LENGTH:
        errors.append(prefix + f"{len(content)} символов, максимум {MAX_PERFORMANCE_LENGTH}; сократите отчёт до выводов")
    found = fields(content)
    statuses = [v.strip("`").casefold() for v in found.get("Статус", [])]
    if len(statuses) != 1 or statuses[0] not in STATUSES:
        return errors + [prefix + "нужна ровно одна строка «Статус: измерено», «Статус: не применимо» или «Статус: не измерено»"]
    status = statuses[0]
    if status != "измерено" and len(content) > MAX_SIMPLE_PERFORMANCE_LENGTH:
        errors.append(prefix + f"краткое решение занимает более {MAX_SIMPLE_PERFORMANCE_LENGTH} символов")
    if status == "не применимо":
        if not substantive(found.get("Причина", []), 40):
            errors.append(prefix + "«не применимо» требует конкретной причины не короче 40 символов")
    elif status == "не измерено":
        for name in ("Причина", "Диагностика"):
            if not substantive(found.get(name, []), 20):
                errors.append(prefix + f"«не измерено» требует фактической строки «{name}:»")
        if NO_EVIDENCE[1] not in [v.casefold() for v in found.get(NO_EVIDENCE[0], [])]:
            errors.append(prefix + "«не измерено» требует строки «Доказательство производительности: отсутствует»")
    else:
        blocks = scenario_blocks(content)
        measured = [(heading, block) for heading, block in blocks if not heading.endswith("— не измерено")] if blocks else [("", content)]
        for heading, block in blocks:
            if heading.endswith("— не измерено"):
                info = fields(block)
                for name in ("Причина", "Диагностика"):
                    if not info.get(name) or all(placeholder(value) for value in info[name]):
                        errors.append(prefix + f"неизмеренный сценарий требует строки «{name}:» ({heading})")
                if NO_EVIDENCE[1] not in [v.casefold() for v in info.get(NO_EVIDENCE[0], [])]:
                    errors.append(prefix + f"неизмеренный сценарий требует отсутствия доказательства ({heading})")
        if not measured:
            errors.append(prefix + "«измерено» требует хотя бы один измеренный сценарий")
        for heading, block in measured:
            where = f" ({heading})" if heading else ""
            info = fields(block)
            for name in MEASURED_FIELDS:
                values = info.get(name, [])
                if len(values) != 1 or any(placeholder(v) for v in values):
                    errors.append(prefix + f"для измерения нужна фактическая строка «{name}:»{where}")
            result = info.get("Результат", [])
            if not any(re.search(r"\d[\d.,\s]*\s*(?:ms|µs|ns|s|%|KiB|MiB|GiB|bytes|count|мс|с)\b|\d\s*%", value) for value in result):
                errors.append(prefix + f"«Результат» требует числового значения с единицей{where}")
            if re.search(r"(?m)^\s*\|\s*Метрика\s*\|", block):
                errors.append(prefix + f"не вставляйте полную таблицу отчёта: оставьте компактный вывод{where}")
    return errors

def validate(body: str) -> list[str]:
    """Детерминированный список ошибок; пустой список — структура соответствует контракту."""
    body = body.replace("\r\n", "\n").replace("\r", "\n")
    if len(body) > MAX_BODY_LENGTH:
        return [f"PR body: {len(body)} символов, максимум {MAX_BODY_LENGTH}"]
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
    if CODERABBIT in names:
        if names.count(CODERABBIT) > 1:
            errors.append("Раздел CodeRabbit повторяется")
        for name, content in blocks:
            if name == CODERABBIT:
                errors.extend(validate_coderabbit(content))
    return errors


def is_automation_pr(payload: dict) -> bool:
    """Узкое исключение: PR зависимостей от проверенного аккаунта бота из его префикса ветки."""
    pr = payload.get("pull_request") or {}
    login = (pr.get("user") or {}).get("login", "")
    branch = (pr.get("head") or {}).get("ref", "")
    return branch.startswith(BOT_BRANCHES.get(login, "\0"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--body-file", type=Path, help="локальный Markdown-источник body до публикации PR")
    source.add_argument("--event-path", type=Path, help="JSON события GitHub pull_request или pull_request_target")
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
