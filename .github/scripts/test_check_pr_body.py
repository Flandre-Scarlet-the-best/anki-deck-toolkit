"""Позитивные и негативные тесты контракта форматирования описания PR."""

import json
import subprocess
import sys
import tempfile
import unittest
from itertools import product
from pathlib import Path

from check_pr_body import is_automation_pr, validate, without_inline_code

HEADINGS = (
    "Цель",
    "Область изменений",
    "Реализация",
    "Проверки",
    "Производительность",
    "CI",
    "Готовность",
    "Ограничения",
)


NOT_APPLICABLE = (
    "Статус: не применимо\n"
    "Причина: изменены только тексты документации и проверка описания PR, исполняемый код инструментов не затронут."
)

MEASURED = """Статус: измерено

### `anki-repo.validate` · профиль `warm`

- Сценарий: `anki-repo.validate`, ревизия 1, `short-cli`; профиль `warm` (кэш `warm`, сеть `online`)
- Измеренный код: `anki-repo` 0.1.0 · коммит `bbbbbbbbbbbb` · рабочая копия без изменений
- Нагрузка: вход `export` (workload, `content-tree-sha256` `dddddddddddd`): `notes` 4 321
- Окружение: `arch`=x86_64, `os`=linux, `virtualization`=wsl2
- Прогоны: 10 успешных из 10, прогрев 3; итог `success`; hyperfine 2.0.0
- База: результат `2222222222222222` · записан 2026-10-01T10:00:00Z — выбрана автоматически
- Сопоставимость: сопоставимы по контракту `benchmark-contract` по метрикам `time_wall_clock@command`

| Метрика | Scope / источник | До: среднее ± σ (n) | После: среднее ± σ (n) | Δ | Δ % | Оценка |
|---|---|---|---|---|---|---|
| `time_wall_clock` | command / hyperfine | 100 ms ± 2 ms (n=10) | 80 ms ± 1 ms (n=10) | −20 ms | −20 % | лучше сверх разброса |

- Вывод: `time_wall_clock@command` — улучшение сверх разброса повторов (−20 %).
- Ограничения: существенных нет
- Источники: проверяемый результат `1111111111111111`, база `2222222222222222` в локальной истории
"""

NOT_MEASURED = """Статус: не измерено

Причина: `anki-e2e.full` · `warm`: Docker недоступен в окружении исполнителя
Диагностика: benchmark-runner check вернул код 5, итог `unsupported` записан, прогонов 0
Доказательство производительности: отсутствует
"""


def body_with(performance: str) -> str:
    return "\n\n".join(
        f"## {name}\n{performance if name == 'Производительность' else f'Фактическая информация для раздела {name}.'}"
        for name in HEADINGS
    )


def valid_body() -> str:
    return body_with(NOT_APPLICABLE)



COMPACT_CODERABBIT = """## CodeRabbit review и disposition

- **Прогресс:** 3 завершённые итерации; последнее ревью \x60abcdef012345\x60
  (итерация 3), опубликован \x601234567abcde\x60 (этот HEAD ещё не проверен).
- **Итог:** confirmed 4 исправлены, false positive 1, stale/repeated 2, clean 1; нерешённых блокирующих нет.
- **Существенное:** исправлена гонка в \x60src/example.rs\x60, commit \x601234567abcde\x60.
- **Полный журнал:** [все результаты ревью](https://gist.github.com/example/0123456789abcdef0123456789abcdef).
"""

LEGACY_CODERABBIT = """| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |
| --- | --- | --- | --- | --- | --- |
| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |
"""


class PrBodyContractTests(unittest.TestCase):
    def test_valid_body(self):
        self.assertEqual(validate(valid_body()), [])

    def test_absent_headings(self):
        self.assertEqual(len(validate("## Что изменено\nСделано.")), len(HEADINGS))

    def test_headings_in_code_fence_are_not_real(self):
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n```")), len(HEADINGS))

    def test_inner_info_string_does_not_close_fence(self):
        # Строка с info string внутри открытого блока — содержимое, а не его конец,
        # поэтому заголовки примера не становятся разделами реального body.
        nested = "```text\n```md\n" + valid_body() + "\n```\n"
        self.assertEqual(len(validate(nested)), len(HEADINGS))

    def test_closing_fence_requires_bare_marker(self):
        body = "```\n" + valid_body() + "\n```python\n```\n"
        self.assertEqual(len(validate(body)), len(HEADINGS))

    def test_tilde_and_longer_backtick_fences(self):
        self.assertEqual(len(validate("~~~md\n" + valid_body() + "\n~~~")), len(HEADINGS))
        # Более длинный маркер открывает блок, который не закрывается более коротким,
        # поэтому заголовки внутри остаются примером, а не разделами.
        self.assertEqual(len(validate("````md\n```\n" + valid_body() + "\n````")), len(HEADINGS))
        # Более длинный маркер способен закрыть блок, открытый коротким, но всё,
        # что уже лежало внутри, остаётся примером, а не разделом body.
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n````")), len(HEADINGS))

    def test_headings_in_html_comment_are_not_real(self):
        # GitHub скрывает HTML-комментарии, поэтому шаблон внутри них не является телом PR.
        self.assertEqual(len(validate(f"<!--\n{valid_body()}\n-->")), len(HEADINGS))

    def test_unterminated_comment_hides_rest(self):
        body = f"## Цель\nИнформация.\n<!-- {chr(10)}" + "\n\n".join(f"## {n}\nИнформация." for n in HEADINGS[1:])
        self.assertTrue(any("Область изменений" in e for e in validate(body)))

    def test_inline_comment_keeps_section_text(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "<!-- пояснение -->\nФакт CI.")
        self.assertEqual(validate(body), [])

    def test_comment_marker_inside_fence_is_literal(self):
        # Внутри код-блока `<!--` — пример, а не комментарий, поэтому описанный
        # в примере заголовок не становится разделом реального body.
        self.assertEqual(len(validate("```md\n<!--\n" + valid_body() + "\n-->\n```")), len(HEADINGS))
        documented = valid_body() + "\n\n```md\n<!-- скрытый пример -->\n```"
        self.assertEqual(validate(documented), [])

    def test_atx_closing_hashes_are_not_part_of_heading_name(self):
        # `## Цель ##` — корректный ATX-заголовок, и GitHub показывает имя «Цель».
        text = {n: NOT_APPLICABLE if n == "Производительность" else "Информация." for n in HEADINGS}
        self.assertEqual(validate("\n\n".join(f"## {n} ##\n{text[n]}" for n in HEADINGS)), [])
        self.assertEqual(validate("\n\n".join(f"## {n} ###\n{text[n]}" for n in HEADINGS)), [])

    def test_fenced_content_does_not_make_section_nonempty(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "```md\nThis is a sample, not CI evidence.\n```")
        self.assertTrue(any("Пустой" in e for e in validate(body)))

    def test_coderabbit_table_in_fence_is_not_accepted(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n```markdown\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| 1 · x | — | — | 0 | clean | x |\n```"
        self.assertTrue(any("CodeRabbit" in e for e in validate(body)))

    def test_duplicate_and_reordered_headings(self):
        body = valid_body() + "\n\n## Цель\nПовторный раздел."
        self.assertTrue(any("повторяется" in e for e in validate(body)))
        a, b = "## Цель\n", "## Реализация\n"
        self.assertTrue(any("порядке" in e for e in validate(valid_body().replace(a, "TEMP\n").replace(b, a).replace("TEMP\n", b))))

    def test_empty_and_placeholder_sections(self):
        self.assertTrue(any("Пустой" in e for e in validate(valid_body().replace("Фактическая информация для раздела CI.", ""))))
        self.assertTrue(any("заглушку" in e for e in validate(valid_body().replace("Фактическая информация для раздела CI.", "TODO"))))

    def test_conditional_coderabbit_table(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\nПроверено."
        self.assertTrue(any("CodeRabbit" in e for e in validate(body)))
        body += "\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `def` |"
        self.assertEqual(validate(body), [])

    def test_coderabbit_row_requires_reviewed_sha(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 | — | — | 0 findings | clean | unknown |"
        self.assertTrue(any("строка" in e for e in validate(body)))

    def test_coderabbit_row_requires_filled_cells(self):
        # Строка из одних разделителей не является записью disposition.
        table = "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n"
        self.assertTrue(any("строка" in e for e in validate(valid_body() + table + "| 1 · `abcdef0` | | | | | |")))
        valid_row = "| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"
        partial_row = "| 2 · `abcdef1` | | | | | |"
        self.assertEqual(validate(valid_body() + table + valid_row), [])
        invalid_table = validate(valid_body() + table + valid_row + "\n" + partial_row)
        self.assertTrue(any("корректная строка" in error for error in invalid_table), invalid_table)
        dangling_table = (
            valid_body()
            + table
            + valid_row
            + "\n\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n"
            + "| --- | --- | --- | --- | --- | --- |"
        )
        self.assertTrue(
            any("корректная строка" in error for error in validate(dangling_table))
        )

    def test_coderabbit_compact_summary_with_gist(self):
        self.assertEqual(validate(valid_body() + "\n\n" + COMPACT_CODERABBIT), [])

    def test_coderabbit_compact_summary_with_measurements(self):
        self.assertEqual(validate(body_with(MEASURED) + "\n\n" + COMPACT_CODERABBIT), [])

    def test_coderabbit_compact_summary_requires_valid_gist_url(self):
        invalid = COMPACT_CODERABBIT.replace(
            "https://gist.github.com/example/0123456789abcdef0123456789abcdef",
            "https://example.com/not-a-gist",
        )
        self.assertTrue(any("Gist" in e for e in validate(valid_body() + "\n\n" + invalid)))

    def test_coderabbit_compact_summary_requires_both_head_shas(self):
        invalid = COMPACT_CODERABBIT.replace(
            "\x601234567abcde\x60 (этот HEAD ещё не проверен)", "неизвестный HEAD"
        )
        self.assertTrue(any("HEAD" in e for e in validate(valid_body() + "\n\n" + invalid)))

    def test_coderabbit_compact_summary_requires_all_fields(self):
        for label in ("Прогресс", "Итог", "Существенное", "Полный журнал"):
            invalid = COMPACT_CODERABBIT.replace(f"**{label}:**", f"**Пропущено_{label}:**")
            self.assertTrue(
                any(f"«{label}»" in e for e in validate(valid_body() + "\n\n" + invalid)),
                label,
            )

    def test_legacy_coderabbit_table_remains_supported(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n" + LEGACY_CODERABBIT
        self.assertEqual(validate(body), [])

    def test_mixed_legacy_table_does_not_mask_partial_or_invalid_summary(self):
        incomplete = COMPACT_CODERABBIT.replace("**Итог:**", "**Сводный итог:**")
        invalid_link = COMPACT_CODERABBIT.replace(
            "https://gist.github.com/example/0123456789abcdef0123456789abcdef",
            "https://example.com/not-a-gist",
        )
        malformed_label = COMPACT_CODERABBIT.replace("**Итог:**", "Итог:")
        missing_colon = COMPACT_CODERABBIT.replace("**Итог:**", "**Итог**")
        for summary in (incomplete, invalid_link, malformed_label, missing_colon):
            with self.subTest(summary=summary):
                compact_fields = summary.split("\n", 1)[1]
                body = (
                    valid_body()
                    + "\n\n## CodeRabbit review и disposition\n"
                    + LEGACY_CODERABBIT
                    + "\n"
                    + compact_fields
                )
                errors = validate(body)
                self.assertTrue(any("сосуществовать" in error for error in errors), errors)
                self.assertTrue(any("Итог" in error or "Gist" in error for error in errors), errors)

    def test_legacy_table_and_complete_compact_summary_cannot_coexist(self):
        compact_fields = COMPACT_CODERABBIT.split("\n", 1)[1]
        body = (
            valid_body()
            + "\n\n## CodeRabbit review и disposition\n"
            + LEGACY_CODERABBIT
            + "\n"
            + compact_fields
        )
        self.assertTrue(any("сосуществовать" in error for error in validate(body)))

    def test_coderabbit_compact_summary_rejects_duplicate_fields(self):
        cases = (
            ("Прогресс", "TBD"),
            ("Итог", "подтверждённых находок 4, блокирующих нет."),
        )
        for label, value in cases:
            duplicate = COMPACT_CODERABBIT.replace(
                f"- **{label}:**", f"- **{label}:** {value}\n- **{label}:**", 1
            )
            with self.subTest(label=label):
                errors = validate(valid_body() + "\n\n" + duplicate)
                self.assertTrue(
                    any("ровно один раз" in error and label in error for error in errors), errors
                )

        for malformed_label in ("- **Прогресс**:", "- **Прогресс2:**", "- Прогресс:"):
            malformed_duplicate = COMPACT_CODERABBIT.replace(
                "- **Прогресс:**",
                f"- **Прогресс:** duplicate\n{malformed_label}",
                1,
            )
            with self.subTest(malformed_label=malformed_label):
                errors = validate(valid_body() + "\n\n" + malformed_duplicate)
                self.assertTrue(any("точный формат" in error for error in errors), errors)

    def test_coderabbit_compact_summary_rejects_placeholder_and_vague_values(self):
        invalid_progress = COMPACT_CODERABBIT.replace(
            "- **Прогресс:** 3 завершённые итерации; последнее ревью `abcdef012345`\n"
            "  (итерация 3), опубликован `1234567abcde` (этот HEAD ещё не проверен).",
            "- **Прогресс:** TBD",
        )
        invalid_result = COMPACT_CODERABBIT.replace(
            "- **Итог:** confirmed 4 исправлены, false positive 1, stale/repeated 2, clean 1; нерешённых блокирующих нет.",
            "- **Итог:** ok",
        )
        for summary, field in ((invalid_progress, "Прогресс"), (invalid_result, "Итог")):
            with self.subTest(field=field):
                errors = validate(valid_body() + "\n\n" + summary)
                self.assertTrue(any(field in error for error in errors), errors)

    def test_coderabbit_gist_link_must_be_in_full_log_field(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        misplaced = COMPACT_CODERABBIT.replace(
            f"- **Полный журнал:** [все результаты ревью]({url}).",
            "- **Полный журнал:** подробности в поле Итог.",
        ).replace(
            "- **Итог:** confirmed", f"- **Итог:** [журнал]({url}); confirmed"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body() + "\n\n" + misplaced)))

    def test_coderabbit_log_link_respects_list_item_boundary(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        link = f"[журнал]({url})"
        prefix = COMPACT_CODERABBIT.replace(f"[все результаты ревью]({url}).", "см. ниже.").rstrip()
        for suffix in (f"\n\n{link}", f"\n- Другое: {link}"):
            with self.subTest(suffix=suffix):
                self.assertTrue(any("Gist" in e for e in validate(valid_body() + "\n\n" + prefix + suffix)))
        for suffix in (f"\n  {link}", f"\n\n  {link}", f"\n{link}", f"\n\n\t{link}"):
            with self.subTest(suffix=suffix):
                self.assertEqual(validate(valid_body() + "\n\n" + prefix + suffix), [])
        with_link = COMPACT_CODERABBIT.rstrip() + f"\n\n{link}"
        self.assertEqual(validate(valid_body() + "\n\n" + with_link), [])

    def test_indented_code_is_not_coderabbit_summary_or_table(self):
        heading = "\n\n## CodeRabbit review и disposition\n\n"
        compact = COMPACT_CODERABBIT.split("\n", 1)[1].strip("\n")
        for source in (compact, LEGACY_CODERABBIT.strip("\n")):
            for indent in ("    ", "\t", "  \t"):
                with self.subTest(source=source[:20], indent=indent):
                    code = "\n".join(indent + line for line in source.splitlines())
                    self.assertTrue(validate(valid_body() + heading + code))
        # Отступленная строка без пустой строки — продолжение абзаца, не код.
        continued = COMPACT_CODERABBIT.replace("  (итерация 3)", "    (итерация 3)")
        self.assertEqual(validate(valid_body() + "\n\n" + continued), [])

    def test_coderabbit_gist_link_inside_inline_code_is_not_accepted(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        inline_code = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"`[все результаты ревью]({url})`"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body() + "\n\n" + inline_code)))

    def test_coderabbit_escaped_gist_link_is_not_accepted(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        escaped = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"\\[все результаты ревью]({url})"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body() + "\n\n" + escaped)))

    def test_coderabbit_gist_link_with_inline_code_label_remains_clickable(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        formatted_label = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"[`все результаты ревью`]({url})"
        )
        self.assertEqual(validate(valid_body() + "\n\n" + formatted_label), [])

    def test_inline_code_masking_preserves_shape_and_is_idempotent(self):
        alphabet = ("`", "\\", "x", "\n")
        for length in range(6):
            for symbols in product(alphabet, repeat=length):
                content = "".join(symbols)
                masked = without_inline_code(content)
                message = f"input={content!r}"
                self.assertEqual(len(masked), len(content), message)
                self.assertEqual(
                    [index for index, char in enumerate(masked) if char == "\n"],
                    [index for index, char in enumerate(content) if char == "\n"],
                    message,
                )
                self.assertEqual(without_inline_code(masked), masked, message)

    def test_coderabbit_gist_link_in_html_comment_is_not_accepted(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        commented = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"<!-- [все результаты ревью]({url}) --> ссылка отсутствует"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body() + "\n\n" + commented)))

    def test_coderabbit_gist_link_in_fence_is_not_accepted(self):
        fake = (
            "## CodeRabbit review и disposition\n"
            "- **Прогресс:** 1 итерация, ревью \x60abcdef012345\x60, опубликован \x601234567abcde\x60.\n"
            "- **Итог:** 0 findings, clean 1, блокирующих нет.\n"
            "- **Существенное:** важных ошибок нет.\n"
            "- **Полный журнал:** пример, фактической ссылки нет.\n"
            "\x60\x60\x60markdown\n"
            "[журнал](https://gist.github.com/example/0123456789abcdef0123456789abcdef)\n"
            "\x60\x60\x60"
        )
        self.assertTrue(any("Gist" in e for e in validate(valid_body() + "\n\n" + fake)))

    def test_performance_decisions_are_accepted(self):
        for performance in (NOT_APPLICABLE, MEASURED, NOT_MEASURED, "**Статус:** не применимо\n- **Причина:** перевод README на английский, код и сборка не меняются."):
            self.assertEqual(validate(body_with(performance)), [], performance)

    def test_performance_requires_one_known_status(self):
        for performance in ("Измерения выполнены.", "Статус: N/A", "Статус: готово", NOT_APPLICABLE + "\nСтатус: измерено", "<!-- Статус: не применимо -->\nПричина: изменены только тексты документации, исполняемый код не затронут."):
            self.assertTrue(any("Статус" in e for e in validate(body_with(performance))), performance)

    def test_not_applicable_needs_concrete_reason(self):
        for reason in ("", "Причина: N/A", "Причина: не применимо", "Причина: <причина>", "Причина: нет изменений", "Причина: —"):
            errors = validate(body_with("Статус: не применимо\n" + reason))
            self.assertTrue(any("причины" in e for e in errors), reason)

    def test_not_measured_needs_reason_diagnostics_and_missing_evidence(self):
        for line in ("Причина:", "Диагностика:", "Доказательство производительности:"):
            broken = "\n".join(l for l in NOT_MEASURED.splitlines() if not l.startswith(line))
            self.assertTrue(validate(body_with(broken)), line)
        claimed = NOT_MEASURED.replace("отсутствует", "есть")
        self.assertTrue(any("Доказательство" in e for e in validate(body_with(claimed))))

    def test_measured_needs_fields_and_metric_table(self):
        for field in ("Сценарий", "Измеренный код", "Нагрузка", "Окружение", "Прогоны", "База", "Сопоставимость", "Вывод", "Ограничения", "Источники"):
            broken = "\n".join(l for l in MEASURED.splitlines() if not l.startswith(f"- {field}:"))
            self.assertTrue(any(field in e for e in validate(body_with(broken))), field)
        placeholder = MEASURED.replace("- База: результат `2222222222222222` · записан 2026-10-01T10:00:00Z — выбрана автоматически", "- База: N/A")
        self.assertTrue(any("База" in e for e in validate(body_with(placeholder))))
        table = [l for l in MEASURED.splitlines() if l.startswith("|")]
        without_table = "\n".join(l for l in MEASURED.splitlines() if not l.startswith("|"))
        self.assertTrue(any("таблица" in e for e in validate(body_with(without_table))))
        # Таблица-пример внутри код-блока не является результатом.
        fenced = without_table + "\n```md\n" + "\n".join(table) + "\n```"
        self.assertTrue(any("таблица" in e for e in validate(body_with(fenced))))
        empty_row = without_table + "\n" + "\n".join(table[:2]) + "\n| `time_wall_clock` | — | — | — | — | — | — |"
        self.assertTrue(any("таблица" in e for e in validate(body_with(empty_row))))
        # Число из посторонней таблицы после таблицы прочерков не засчитывается.
        foreign = empty_row + "\n\nТекст.\n\n| Прочее | Тип | Значение |\n|---|---|---|\n| x | y | 42 |"
        self.assertTrue(any("таблица" in e for e in validate(body_with(foreign))))
        # Версия инструмента в колонке источника не делает строку прочерков результатом.
        source_only = without_table + "\n" + "\n".join(table[:2]) + "\n| `time_wall_clock` | command / hyperfine 2.0 | — | — | — | — | — |"
        self.assertTrue(any("таблица" in e for e in validate(body_with(source_only))))

    def test_only_measurement_columns_count_as_measured(self):
        fields = "\n".join(l for l in MEASURED.splitlines() if not l.startswith("|"))
        with_base = "| Метрика | Область (scope) / источник | До: среднее ± σ (n) | После: среднее ± σ (n) | Δ | Δ % | Оценка |\n|---|---|---|---|---|---|---|\n"
        alone = "| Метрика | Область (scope) / источник | Значение: среднее ± σ (n) | Медиана | Мин–макс |\n|---|---|---|---|---|\n"
        rejected = {
            "цифра в оценке": with_base + "| `time_wall_clock` | command / hyperfine | — | — | — | — | — (n=0: данных нет) |",
            "только состояние «до»": with_base + "| `time_wall_clock` | command / hyperfine | 100 ms (n=10) | `failed`: код 1 | — | — | — |",
            "версия в scope": alone + "| `time_wall_clock` | command / hyperfine 2.0.0 | — | — | — |",
            "n=0 в пояснении": alone + "| `time_wall_clock` | command / hyperfine | n=0: данных нет | — | — |",
            "причина с цифрой": alone + "| `time_wall_clock` | command / hyperfine | `unavailable`: нет perf_event 4 | — | — |",
            "экранированная черта": alone + "| `time_wall_clock` | command \\| 2 / hyperfine | — | — | — |",
            "таблица в fenced-блоке": "```md\n" + alone + "| `time_wall_clock` | command / hyperfine | 54 ms (n=10) | 54 ms | 52 ms – 56 ms |\n```",
            "таблица в HTML-комментарии": "<!--\n" + alone + "| `time_wall_clock` | command / hyperfine | 54 ms (n=10) | 54 ms | 52 ms – 56 ms |\n-->",
        }
        for name, table in rejected.items():
            errors = validate(body_with(fields + "\n" + table))
            self.assertTrue(any("таблица" in e and "строкой значений" in e for e in errors), (name, errors))
        accepted = {
            "с базой": with_base + "| `time_wall_clock` | command / hyperfine | 100 ms (n=10) | 80 ms ± 1 ms (n=10) | −20 ms | −20 % | лучше сверх разброса |",
            "без базы": alone + "| `time_wall_clock` | command / hyperfine | 54.41 ms ± 1.26 ms (n=10) | 54.53 ms | 52.02 ms – 56.4 ms |",
            "прежний заголовок": alone.replace("Область (scope)", "Scope") + "| `notes` | module / module | 20 000 (n=1) | 20 000 | 20 000 – 20 000 |",
        }
        for name, table in accepted.items():
            self.assertEqual(validate(body_with(fields + "\n" + table)), [], name)
        # Каждый блок `###` проверяется своей таблицей: значение первого не
        # засчитывается второму.
        second = "\n### `anki-repo.stats` · профиль `warm`\n" + fields.split("\n", 3)[3].replace("anki-repo.validate", "anki-repo.stats") + "\n" + rejected["цифра в оценке"]
        errors = validate(body_with(fields + "\n" + accepted["с базой"] + "\n" + second))
        self.assertTrue(any("anki-repo.stats" in e and "таблица" in e for e in errors), errors)
        self.assertFalse(any("anki-repo.validate" in e for e in errors), errors)

    def test_each_measured_scenario_needs_its_own_fields(self):
        second = "\n### `anki-repo.qa` · профиль `warm`\n\n- Сценарий: `anki-repo.qa`, ревизия 1\n"
        errors = validate(body_with(MEASURED + second))
        self.assertTrue(any("anki-repo.qa" in e and "Источники" in e for e in errors), errors)
        self.assertTrue(any("anki-repo.qa" in e and "таблица" in e for e in errors), errors)
        # Неизмеренный сценарий рядом с измеренным проверяется по своим правилам.
        unmeasured = "\n### `anki-e2e.full` · профиль `warm` — не измерено\n\n- Сценарий: `anki-e2e.full`\n- Причина: Docker недоступен\n- Диагностика: исполнитель записал итог `unsupported`: прогонов 0, успешных 0\n- Доказательство производительности: отсутствует\n"
        self.assertEqual(validate(body_with(MEASURED + unmeasured)), [])
        for line in ("- Причина:", "- Диагностика:", "- Доказательство производительности:"):
            incomplete = "\n".join(l for l in unmeasured.split("\n") if not l.startswith(line))
            errors = validate(body_with(MEASURED + incomplete))
            self.assertTrue(any("anki-e2e.full" in e and "неизмеренный" in e for e in errors), (line, errors))

    def test_open_pr_without_performance_section_gets_actionable_error(self):
        # Требование действует для уже открытых PR со следующего события
        # edited/synchronize: зелёной проверки без решения о применимости нет.
        legacy = "\n\n".join(f"## {n}\nФактическая информация." for n in HEADINGS if n != "Производительность")
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "synchronize", "pull_request": {"body": legacy, "user": {"login": "someone"}, "head": {"ref": "feat/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 1)
            self.assertIn("benchmark-history report", proc.stderr)
            self.assertIn("не применимо", proc.stderr)
            # Исключение dependency-ботов сохраняется и для нового раздела.
            event.write_text(json.dumps({"pull_request": {"body": legacy, "user": {"login": "renovate[bot]"}, "head": {"ref": "renovate/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0)

    def test_performance_section_coexists_with_coderabbit_table(self):
        body = body_with(MEASURED) + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"
        self.assertEqual(validate(body), [])

    def test_bot_exception_is_limited_to_account_and_branch(self):
        def payload(login, branch):
            return {"pull_request": {"user": {"login": login}, "head": {"ref": branch}}}
        self.assertTrue(is_automation_pr(payload("renovate[bot]", "renovate/rust-sha2")))
        self.assertTrue(is_automation_pr(payload("dependabot[bot]", "dependabot/github_actions/action")))
        self.assertFalse(is_automation_pr(payload("someone", "renovate/fake")))
        self.assertFalse(is_automation_pr(payload("renovate[bot]", "feat/other")))

    def test_cli_rejects_event_without_pull_request(self):
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "edited"}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 2)
            self.assertIn("pull_request", proc.stderr)


if __name__ == "__main__":
    unittest.main()
