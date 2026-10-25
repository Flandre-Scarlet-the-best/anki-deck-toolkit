#!/usr/bin/env python3
"""Краткий отчёт по журналу `cargo test` для GitHub Actions (только stdlib).

Скрипт читает сохранённый вывод libtest и пишет в Job Summary компактную сводку:
сколько тестов прошло, упало и пропущено, имена упавших тестов и шаг, в журнале
которого лежат подробности. Для каждого упавшего теста он выводит аннотацию
`::error`. Содержимое журнала, вывод тестов и пути исполнителя в сводку не
попадают: только имена тестов и счётчики.

Без `--require` скрипт только сообщает и всегда завершается кодом 0: провал
теста уже сделал красным шаг `cargo test`, а сбой самого отчёта не должен
ломать успешное задание. С `--require` скрипт становится проверкой: каждый
названный тест обязан быть исполнен и пройти (`ok`). Тест, которого нет в
журнале (переименован или отфильтрован), и тест, оставшийся `ignored`, —
отказ, а не успех.
"""

import argparse
import os
import re
import sys
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
TEST_LINE = re.compile(r"^test (?P<name>\S+) \.\.\. (?P<status>ok|FAILED|ignored)\b")
RESULT_LINE = re.compile(
    r"^test result: (?:ok|FAILED)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; "
    r"(?P<ignored>\d+) ignored; \d+ measured; (?P<filtered>\d+) filtered out"
)
COMPILE_FAILURE = re.compile(r"^error: could not compile `(?P<crate>[^`]+)`")
MAX_LISTED = 30


def parse_log(text):
    """Возвращает статусы тестов, суммарные счётчики и упавшие сборки."""
    statuses = {}
    totals = {"passed": 0, "failed": 0, "ignored": 0, "filtered": 0}
    binaries = 0
    compile_failures = []
    for raw in text.splitlines():
        line = ANSI.sub("", raw).rstrip()
        match = TEST_LINE.match(line)
        if match:
            name, status = match["name"], match["status"]
            # Одно имя в нескольких тестовых целях: провал любой из них — провал.
            if statuses.get(name) != "FAILED":
                statuses[name] = status
            continue
        match = RESULT_LINE.match(line)
        if match:
            binaries += 1
            for key in totals:
                totals[key] += int(match[key])
            continue
        match = COMPILE_FAILURE.match(line.strip())
        if match and match["crate"] not in compile_failures:
            compile_failures.append(match["crate"])
    return statuses, totals, binaries, compile_failures


def escape_data(value):
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_property(value):
    return escape_data(value).replace(":", "%3A").replace(",", "%2C")


def annotation(title, message):
    return f"::error title={escape_property(title)}::{escape_data(message)}"


def build_report(title, step, statuses, totals, binaries, compile_failures, required):
    """Собирает Markdown сводки, аннотации и признак выполнения требований."""
    failed = sorted(name for name, status in statuses.items() if status == "FAILED")
    lines = [f"### {title}", ""]
    annotations = []
    if binaries == 0 and not statuses:
        lines.append("Результаты тестов в журнале не найдены: тесты не запускались.")
    else:
        lines += [
            "| Прошло | Упало | Пропущено (`ignored`) | Отфильтровано | Тестовых целей |",
            "| ---: | ---: | ---: | ---: | ---: |",
            f"| {totals['passed']} | {totals['failed']} | {totals['ignored']} "
            f"| {totals['filtered']} | {binaries} |",
        ]
    for crate in compile_failures:
        lines += ["", f"Сборка `{crate}` не удалась: тесты этой цели не исполнялись."]
        annotations.append(annotation(title, f"Сборка {crate} не удалась; см. шаг «{step}»"))
    if failed:
        lines += ["", "Упавшие тесты:", ""]
        lines += [f"- `{name}`" for name in failed[:MAX_LISTED]]
        if len(failed) > MAX_LISTED:
            lines.append(f"- … и ещё {len(failed) - MAX_LISTED}")
        annotations += [
            annotation(title, f"Тест {name} упал; подробности — в журнале шага «{step}»")
            for name in failed[:MAX_LISTED]
        ]
    satisfied = True
    if required:
        lines += ["", "| Обязательный тест | Результат |", "| --- | --- |"]
        for name in required:
            status = statuses.get(name)
            if status == "ok":
                verdict = "прошёл"
            else:
                satisfied = False
                verdict = {
                    None: "не исполнялся",
                    "ignored": "пропущен (`ignored`)",
                    "FAILED": "упал",
                }[status]
                if status != "FAILED":
                    annotations.append(
                        annotation(title, f"Обязательный тест {name}: {verdict}")
                    )
            lines.append(f"| `{name}` | {verdict} |")
    lines += ["", f"Подробности — в журнале шага «{step}» этого задания.", ""]
    return "\n".join(lines), annotations, satisfied


def write_summary(markdown):
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as summary:
            summary.write(markdown + "\n")
    else:
        print(markdown)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--log", required=True, type=Path, help="сохранённый вывод cargo test")
    parser.add_argument("--title", required=True, help="заголовок сводки и аннотаций")
    parser.add_argument("--step", required=True, help="имя шага с полным журналом")
    parser.add_argument(
        "--require",
        action="append",
        default=[],
        metavar="ТЕСТ",
        help="полное имя теста libtest, который обязан пройти (можно повторять)",
    )
    args = parser.parse_args(argv)
    try:
        text = args.log.read_text(encoding="utf-8", errors="replace")
    except OSError:
        markdown = f"### {args.title}\n\nЖурнал тестов отсутствует: шаг «{args.step}» не выполнялся.\n"
        write_summary(markdown)
        if args.require:
            print(annotation(args.title, f"Шаг «{args.step}» не выполнялся"))
            return 1
        return 0
    try:
        markdown, annotations, satisfied = build_report(
            args.title, args.step, *parse_log(text), args.require
        )
        write_summary(markdown)
    except Exception as error:  # noqa: BLE001 — отчёт не должен ронять успешное задание
        if args.require:
            raise
        print(f"::warning title={escape_property(args.title)}::Сводка не построена: {error}")
        return 0
    for line in annotations:
        print(line)
    return 0 if satisfied else 1


if __name__ == "__main__":
    sys.exit(main())
