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
# Поля компактной сводки; полный benchmark-history report хранится в Gist.
MEASURED_FIELDS = ("Сценарий", "База", "Кандидат", "Результат", "Вывод", "Регрессии")
MAX_BODY_LENGTH = 6000
REPORT_SECTION = "Подробный отчёт"
REVIEW_SECTION = "CodeRabbit review и disposition"


def normalize_body(body: str) -> str:
    """Текст для подсчёта кодовых точек Unicode после единственной нормализации CRLF/CR → LF."""
    return body.replace("\r\n", "\n").replace("\r", "\n")

NO_EVIDENCE = ("Доказательство производительности", "отсутствует")
PERFORMANCE_HINT = (
    "добавьте после ## Проверки раздел ## Производительность: компактную сводку измерений "
    "из проверенных результатов, «Статус: не применимо» с конкретной причиной или "
    "«Статус: не измерено» с причиной, диагностикой и строкой "
    "«Доказательство производительности: отсутствует»"
)


# GitHub скрывает HTML-комментарии, поэтому заголовки внутри них не являются
# видимыми разделами body. Внутри код-блока `<!--` — литерал, а не комментарий.


def mask_list_fences(lines: list[str]) -> list[str]:
    """Скрыть fenced-блоки внутри пунктов списка до разбора комментариев.

    Отступ маркера блока считается относительно содержимого пункта; при выходе
    из списка незакрытый блок заканчивается вместе с контейнером.
    """
    result: list[str] = []
    indents: list[int] = []
    fence_char = ""
    fence_len = 0
    fence_indent = 0
    outer_char = ""
    outer_len = 0
    in_comment = False
    for line in lines:
        expanded = line.expandtabs(4)
        indent = len(expanded) - len(expanded.lstrip(" "))
        if outer_char:
            closing = FENCE_CLOSE.match(expanded)
            if closing and closing.group(1)[0] == outer_char and len(closing.group(1)) >= outer_len:
                outer_char = ""
            result.append(line)
            continue
        if fence_char:
            if expanded.strip() and indent < fence_indent:
                fence_char = ""
            else:
                closing = FENCE_CLOSE.match(expanded[fence_indent:])
                if closing and closing.group(1)[0] == fence_char and len(closing.group(1)) >= fence_len:
                    fence_char = ""
                result.append(" " * len(line))
                continue
        # Маркеры внутри комментария не открывают блоки. Сохраняем исходную
        # строку: окончательное удаление комментариев выполняет strip_comments.
        was_in_comment = in_comment
        visible = ""
        rest = expanded
        while rest:
            if in_comment:
                end = rest.find("-->")
                if end < 0:
                    break
                in_comment = False
                rest = rest[end + 3:]
            else:
                start = rest.find("<!--")
                if start < 0:
                    visible += rest
                    break
                visible += rest[:start]
                in_comment = True
                rest = rest[start + 4:]
        expanded = visible
        indent = len(expanded) - len(expanded.lstrip(" "))
        if expanded.strip():
            while indents and indent < indents[-1]:
                indents.pop()
        base = indents[-1] if indents else 0
        relative = expanded[base:]
        marker = re.match(r"^ {0,3}(?:[-*+]|\d{1,9}[.)]) +", relative)
        if marker:
            base += marker.end()
            indents.append(base)
            relative = expanded[base:]
        opening = FENCE.match(relative)
        if opening:
            token = opening.group(1)
            # У backtick-fence info string не может содержать обратную кавычку.
            if token[0] != "`" or "`" not in relative[opening.end():]:
                in_comment = was_in_comment
                if indents:
                    fence_char, fence_len, fence_indent = token[0], len(token), base
                    result.append(" " * len(line))
                    continue
                outer_char, outer_len = token[0], len(token)
        result.append(line)
    return result


def strip_comments(lines: list[str]) -> list[str]:
    """Построчно удалить HTML-комментарии, сохраняя fenced-блоки кода."""
    result: list[str] = []
    inside = False
    fence_char = ""
    fence_len = 0
    for line in mask_list_fences(lines):
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


def table_cells(row: str) -> list[str]:
    """Ячейки Markdown-таблицы с неэкранированной границей."""
    return [c.strip() for c in re.split(r"(?<!\\)\|", row.strip().strip("|"))]


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
                if len(values) != 1 or any(placeholder(v) for v in values):
                    errors.append(prefix + f"для измерения нужна строка «{name}:» с фактическими сведениями{where}")
            result = block_fields.get("Результат", [])
            if not any(re.search(r"\d[\d.,\s]*\s*(?:ms|µs|ns|s|%|KiB|MiB|GiB|bytes|count|инструкц\w*|мс|с)\b|\d\s*%", value) and not placeholder(value) for value in result):
                errors.append(prefix + "«Результат» требует числового значения с единицей")
            if re.search(r"(?m)^\s*\|\s*Метрика\s*\|", block):
                errors.append(prefix + "полную таблицу benchmark-history report перенесите в Gist")

    return errors



# Полный журнал хранится в Gist; legacy-таблица не обходит новый контракт.
CODERABBIT_FIELDS = ("Прогресс", "Итог", "Существенное", "Полный журнал")
COMPACT_NAMES = "|".join(map(re.escape, CODERABBIT_FIELDS))
COMPACT_FIELD = re.compile(
    rf"(?m)^[ \t]*[-*+][ \t]+\*\*(?P<name>{COMPACT_NAMES}):\*\*[ \t]*"
)
COMPACT_CANDIDATE = re.compile(
    rf"(?m)^[ \t]*[-*+][ \t]+(?:\*\*)?(?:{COMPACT_NAMES})"
)
LEGACY_HEADER = ("Итерация / reviewed HEAD", "Уровень", "Путь", "Влияние", "Решение", "Исправление")
# Роль должна непосредственно относиться к SHA, а не просто встречаться рядом
# с двумя произвольными идентификаторами. Перенос строки внутри пункта допустим.
SHA = r"`([0-9a-f]{7,40})`"
REVIEW_ROLE = r"(?:reviewed(?:\s+head)?|(?:последн\w+\s+)?ревью|проверенн\w+\s+head)"
PUBLISHED_ROLE = r"(?:опубликован\w*(?:\s+(?:после\s+\w+\s+)?head)?|published(?:\s+head)?)"


def role_shas(text: str, role: str) -> set[str]:
    """SHA после названия роли либо перед ним через тире/скобку."""
    before = re.compile(rf"\b{role}\s*[:=—–-]?\s*{SHA}", re.IGNORECASE)
    after = re.compile(rf"{SHA}\s*(?:[—–-]\s*|\(\s*){role}\b", re.IGNORECASE)
    return set(before.findall(text)) | set(after.findall(text))


CODERABBIT_GIST = re.compile(
    r"\[(?P<label>[^\[\]\n]+?)\]\(https://gist\.github\.com/"
    r"(?P<owner>[A-Za-z0-9-]+)/(?P<id>[0-9a-f]{32})\)"
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
    for match in matches:
        end = list_item_end(content, match)
        captured[match.group("name")].append(content[match.end():end].strip())

    for name, values in captured.items():
        if len(values) != 1:
            errors.append(f"CodeRabbit: поле «{name}» должно встречаться ровно один раз")
        elif name != "Полный журнал" and not substantive(values, 4):
            errors.append(f"CodeRabbit: поле «{name}» должно содержать содержательное значение")

    progress_values = captured["Прогресс"]
    if len(progress_values) == 1 and substantive(progress_values, 4):
        progress = progress_values[0]
        reviewed = role_shas(progress, REVIEW_ROLE)
        published = role_shas(progress, PUBLISHED_ROLE)
        iteration = re.search(
            r"(?:\b[1-9]\d*\s+(?:завершённ\w+\s+)?итерац|"
            r"\bитераци\w*\s*[:№#]?\s*[1-9]\d*)",
            progress,
            re.IGNORECASE,
        )
        if not iteration or not any(r != p for r in reviewed for p in published):
            errors.append("CodeRabbit: в «Прогресс» нужны номер итерации, reviewed HEAD и опубликованный HEAD")

    result_values = captured["Итог"]
    if (len(result_values) == 1 and substantive(result_values, 4)
            and not re.search(r"(?<!\w)\d+(?!\w)", re.sub(SHA, "", result_values[0]))):
        errors.append("CodeRabbit: в «Итог» нужны количественные данные")

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
            and not (link.start() > 0 and field_text[link.start() - 1] == "!"
                     and not is_escaped(field_text, link.start() - 1))
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
        return ["CodeRabbit: перенесите старую таблицу в coderabbit-review.md и оставьте компактную сводку с Gist"]
    if not compact_candidates:
        return ["CodeRabbit: нужна полноценная сводка с Gist"]

    errors = validate_compact_coderabbit(content, compact_matches)
    errors.extend(format_errors)
    if errors:
        return ["CodeRabbit: нужна полноценная сводка с Gist"] + errors
    return []



def gist_links(content: str) -> list[re.Match[str]]:
    """Только канонические видимые ссылки: не в коде, не изображения и не экранированные."""
    visible = without_inline_code(mask_indented_code(content))
    return [
        link for link in CODERABBIT_GIST.finditer(visible)
        if not is_escaped(visible, link.start())
        and not (link.start() and visible[link.start() - 1] == "!"
                 and not is_escaped(visible, link.start() - 1))
        and content[link.start("label"):link.end("label")].strip()
        and not placeholder(content[link.start("label"):link.end("label")])
        and len(set(link.group("id"))) > 1
    ]


def visible_body(body: str) -> str:
    """Включить преамбулу при поиске ссылок; комментарии/fences уже скрыты."""
    return "\n".join(text for _, text in sections("## __preamble__\n" + body))


def canonical_gist(body: str) -> str:
    """URL проверенной локальной структурой единственной ссылки."""
    links = gist_links(visible_body(body))
    if len(links) != 1:
        raise ValueError("Нужна ровно одна видимая каноническая Markdown-ссылка на Gist")
    return f"https://gist.github.com/{links[0].group('owner')}/{links[0].group('id')}"


def validate_gist_link(body: str, blocks: list[tuple[str, str]]) -> list[str]:
    links = gist_links(visible_body(body))
    target = REVIEW_SECTION if any(name == REVIEW_SECTION for name, _ in blocks) else REPORT_SECTION
    errors = []
    if len(links) != 1:
        errors.append("PR: нужна ровно одна видимая каноническая Markdown-ссылка на Gist")
    targets = [text for name, text in blocks if name == target]
    if len(targets) != 1 or len(gist_links(targets[0])) != 1:
        errors.append(f"Gist: единственную ссылку разместите в ## {target}")
    return errors


def validate(body: str) -> list[str]:
    """Вернуть детерминированные ошибки; пустой список означает верную структуру."""
    body = normalize_body(body)
    if len(body) > MAX_BODY_LENGTH:
        return [f"PR body: {len(body)} символов, максимум {MAX_BODY_LENGTH}"]
    blocks = sections(body)
    names = [heading for heading, _ in blocks]
    errors: list[str] = []
    errors.extend(validate_gist_link(body, blocks))
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
