#!/usr/bin/env python3
"""Проверка структуры описания PR, написанного человеком (только stdlib)."""

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
    """Построчно удалить HTML-комментарии, сохраняя fenced-блоки кода."""
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
    """Извлечь видимые заголовки второго уровня и их текст."""
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
    """Проверить наличие конкретного пояснения вместо шаблонной заглушки."""
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



# Полный журнал хранится в Gist; старые PR могут сохранить прежнюю таблицу.
CODERABBIT_FIELDS = ("Прогресс", "Итог", "Существенное", "Полный журнал")
COMPACT_NAMES = "|".join(map(re.escape, CODERABBIT_FIELDS))
COMPACT_FIELD = re.compile(
    rf"(?m)^[ \t]*[-*+][ \t]+\*\*(?P<name>{COMPACT_NAMES}):\*\*[ \t]*"
)
COMPACT_CANDIDATE = re.compile(
    rf"(?m)^[ \t]*[-*+][ \t]+(?:\*\*)?(?:{COMPACT_NAMES})"
)
LEGACY_HEADER = ("Итерация / reviewed HEAD", "Уровень", "Путь", "Влияние", "Решение", "Исправление")
LEGACY_SEPARATOR = re.compile(r":?-{3,}:?\Z")
LEGACY_ROW_ID = re.compile(r"[1-9]\d*\s*·\s*`[0-9a-f]{7,40}`\Z")
CODERABBIT_GIST = re.compile(
    r"\[(?P<label>[^\[\]\n]+?)\]\(https://gist\.github\.com/"
    r"[A-Za-z0-9-]+/[0-9a-f]{20,64}(?:#[a-zA-Z0-9-]+)?\)"
)


def is_escaped(content: str, index: int) -> bool:
    """Определить, экранирован ли символ нечётным числом обратных слешей."""
    slashes = 0
    index -= 1
    while index >= 0 and content[index] == "\\":
        slashes += 1
        index -= 1
    return slashes % 2 == 1


def without_inline_code(content: str) -> str:
    """Заменить содержимое закрытых inline code span пробелами той же длины."""
    chars = list(content)
    index = 0
    while index < len(content):
        if content[index] != "`" or is_escaped(content, index):
            index += 1
            continue
        end = index + 1
        while end < len(content) and content[end] == "`":
            end += 1
        delimiter = content[index:end]
        close = end
        closing_start = -1
        while close < len(content):
            candidate = content.find("`", close)
            if candidate < 0:
                break
            candidate_end = candidate + 1
            while candidate_end < len(content) and content[candidate_end] == "`":
                candidate_end += 1
            if content[candidate:candidate_end] == delimiter:
                closing_start = candidate
                break
            close = candidate_end
        if closing_start < 0:
            index = end
            continue
        for position in range(index, closing_start + len(delimiter)):
            if chars[position] != "\n":
                chars[position] = " "
        index = closing_start + len(delimiter)
    return "".join(chars)


def mask_indented_code(content: str) -> str:
    """Скрыть код с отступом, учитывая абзацы и отступ содержимого списка.

    Код не прерывает абзац; после пустой строки отступ отсчитывается от
    начала содержимого текущего пункта, а табуляция занимает до четырёх колонок.
    """
    result = []
    item_indent = 0
    paragraph = False
    code = False
    for line in content.splitlines(keepends=True):
        text = line.rstrip("\n")
        expanded = text.expandtabs(4)
        if not expanded.strip():
            paragraph = False
            result.append(line)
            continue
        indent = len(expanded) - len(expanded.lstrip(" "))
        if item_indent and indent < item_indent and not paragraph:
            item_indent = 0
        relative = indent - item_indent if item_indent and indent >= item_indent else indent
        if relative >= 4 and (code or not paragraph):
            code = True
            result.append(" " * len(text) + line[len(text):])
            continue
        code = False
        marker = re.match(r"^ {0,3}[-*+][ \t]+", expanded[item_indent:] if item_indent and indent >= item_indent else expanded)
        if marker:
            base = item_indent if item_indent and indent >= item_indent else 0
            item_indent = base + marker.end()
            paragraph = True
        else:
            paragraph = True
        result.append(line)
    return "".join(result)


def has_legacy_coderabbit_row(content: str) -> bool:
    """Проверить заголовок, разделитель и заполненную строку старой таблицы."""
    lines = content.splitlines()
    visible_lines = without_inline_code(content).splitlines()
    found_table = False
    for index, line in enumerate(visible_lines):
        if not line.lstrip().startswith("|") or table_cells(line) != list(LEGACY_HEADER):
            continue
        found_table = True
        if index + 2 >= len(visible_lines):
            return False
        separator = visible_lines[index + 1]
        if not separator.lstrip().startswith("|"):
            return False
        separator_cells = table_cells(separator)
        if len(separator_cells) != len(LEGACY_HEADER) or not all(
            LEGACY_SEPARATOR.fullmatch(cell.replace(" ", "")) for cell in separator_cells
        ):
            return False
        found_row = False
        for row_index in range(index + 2, len(visible_lines)):
            row = visible_lines[row_index]
            if not row.lstrip().startswith("|"):
                break
            cells = table_cells(lines[row_index])
            if not (
                len(cells) == len(LEGACY_HEADER)
                and LEGACY_ROW_ID.fullmatch(cells[0])
                and all(cell.strip() for cell in cells[1:])
                and len(table_cells(row)) == len(LEGACY_HEADER)
            ):
                return False
            found_row = True
        if not found_row:
            return False
    return found_table


def list_item_end(content: str, match: re.Match[str]) -> int:
    """Конец пункта списка: после пустой строки нужен отступ содержимого."""
    marker = re.match(r"[ \t]*[-*+][ \t]+", match.group())
    assert marker is not None
    indent = len(marker.group().expandtabs(4))
    position = content.find("\n", match.end())
    if position < 0:
        return len(content)
    blank = False
    position += 1
    for line in content[position:].splitlines(keepends=True):
        text = line.rstrip("\n")
        expanded = text.expandtabs(4)
        if not expanded.strip():
            blank = True
        else:
            leading = len(expanded) - len(expanded.lstrip(" "))
            if leading < indent and (blank or re.match(r" {0,3}(?:[-*+]\s+|#{1,6}\s|>)", expanded)):
                return position
            blank = False
        position += len(line)
    return len(content)


def validate_compact_coderabbit(content: str, matches: list[re.Match[str]]) -> list[str]:
    """Проверить четыре поля компактной сводки и видимую ссылку на Gist."""
    errors: list[str] = []
    captured: dict[str, list[str]] = {name: [] for name in CODERABBIT_FIELDS}
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(content)
        captured[match.group("name")].append(content[match.end():end].strip())

    for name, values in captured.items():
        if len(values) != 1:
            errors.append(f"CodeRabbit: поле «{name}» должно встречаться ровно один раз")
        elif name != "Полный журнал" and not substantive(values, 4):
            errors.append(f"CodeRabbit: поле «{name}» должно содержать содержательное значение")

    progress_values = captured["Прогресс"]
    if len(progress_values) == 1 and substantive(progress_values, 4):
        progress = progress_values[0]
        shas = set(re.findall(r"`([0-9a-f]{7,40})`", progress))
        iteration = re.search(
            r"(?:\b[1-9]\d*\s+(?:завершённ\w+\s+)?итерац|"
            r"\bитераци\w*\s*[:№#]?\s*[1-9]\d*)",
            progress,
            re.IGNORECASE,
        )
        if not iteration or len(shas) < 2:
            errors.append("CodeRabbit: в «Прогресс» нужны номер итерации, reviewed HEAD и опубликованный HEAD")

    visible = without_inline_code(content)
    fields_visible = [match for match in COMPACT_FIELD.finditer(visible)]
    for index, match in enumerate(fields_visible):
        if match.group("name") != "Полный журнал":
            continue
        end = list_item_end(visible, match)
        field_text = visible[match.end():end]
        links = [
            link for link in CODERABBIT_GIST.finditer(field_text)
            if not is_escaped(field_text, link.start())
        ]
        if len(links) != 1:
            errors.append("CodeRabbit: в «Полный журнал» нужна одна видимая Markdown-ссылка на GitHub Gist")
            break
        label_start = match.end() + links[0].start("label")
        label_end = match.end() + links[0].end("label")
        if not content[label_start:label_end].strip():
            errors.append("CodeRabbit: в «Полный журнал» нужна одна видимая Markdown-ссылка на GitHub Gist")
            break
    else:
        if len(captured["Полный журнал"]) == 1 and not any(
            match.group("name") == "Полный журнал" for match in fields_visible
        ):
            errors.append("CodeRabbit: в «Полный журнал» нужна одна видимая Markdown-ссылка на GitHub Gist")

    return errors


def validate_coderabbit(content: str) -> list[str]:
    """Проверить старую таблицу либо полную компактную сводку CodeRabbit."""
    content = mask_indented_code(content)
    visible = without_inline_code(content)
    legacy_present = any(
        line.lstrip().startswith("|") and table_cells(line) == list(LEGACY_HEADER)
        for line in visible.splitlines()
    )
    compact_candidates = list(COMPACT_CANDIDATE.finditer(visible))
    compact_matches = list(COMPACT_FIELD.finditer(visible))
    format_errors = []
    if len(compact_candidates) != len(compact_matches):
        format_errors.append("CodeRabbit: поля компактной сводки должны иметь точный формат «- **Поле:**»")

    if legacy_present and compact_candidates:
        errors = ["CodeRabbit: старая таблица и компактная сводка не должны сосуществовать"]
        errors.extend(format_errors)
        errors.extend(validate_compact_coderabbit(content, compact_matches))
        return errors
    if legacy_present:
        if has_legacy_coderabbit_row(content):
            return []
        return ["В старой таблице CodeRabbit отсутствует корректная строка завершённой итерации"]
    if not compact_candidates:
        return ["CodeRabbit: нужна накопительная таблица либо полноценная сводка с Gist"]

    errors = validate_compact_coderabbit(content, compact_matches)
    errors.extend(format_errors)
    if errors:
        return ["CodeRabbit: нужна накопительная таблица либо полноценная сводка с Gist"] + errors
    return []



def validate(body: str) -> list[str]:
    """Вернуть детерминированные ошибки; пустой список означает верную структуру."""
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
                errors.extend(validate_coderabbit(content))
    return errors


def is_automation_pr(payload: dict) -> bool:
    """Проверить узкое исключение для dependency-PR от известных bot-аккаунтов."""
    pr = payload.get("pull_request") or {}
    login = (pr.get("user") or {}).get("login", "")
    branch = (pr.get("head") or {}).get("ref", "")
    return branch.startswith(BOT_BRANCHES.get(login, "\0"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--body-file", type=Path, help="Локальный исходный Markdown-файл перед публикацией PR")
    source.add_argument("--event-path", type=Path, help="JSON-событие GitHub pull_request")
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
