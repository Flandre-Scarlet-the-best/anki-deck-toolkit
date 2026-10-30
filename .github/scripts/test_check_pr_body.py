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

GIST = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
REPORT = f"\n\n## Подробный отчёт\n[проверки и технические результаты]({GIST})"
MEASURED = """Статус: измерено

### validation warm

- Сценарий: anki-repo.validate warm, одинаковая синтетическая нагрузка.
- База: `aaaaaaaaaaaa` — проверенный исходный commit, warm.
- Кандидат: `bbbbbbbbbbbb` — измеренный commit, warm.
- Результат: wall-clock 100 ms → 80 ms, −20 %, n=10.
- Вывод: улучшение сверх разброса повторов; полная серия в подробном отчёте.
- Регрессии: существенных ухудшений не обнаружено по проверенным метрикам.
"""

NOT_MEASURED = """Статус: не измерено

Причина: `anki-e2e.full` · `warm`: Docker недоступен в окружении исполнителя
Диагностика: benchmark-runner check вернул код 5, итог `unsupported` записан, прогонов 0
Доказательство производительности: отсутствует
"""


def body_with(performance: str, with_gist: bool = True) -> str:
    return "\n\n".join(
        f"## {name}\n{performance if name == 'Производительность' else f'Фактическая информация для раздела {name}.'}"
        for name in HEADINGS
    ) + (REPORT if with_gist else "")


def valid_body(with_gist: bool = True) -> str:
    return body_with(NOT_APPLICABLE, with_gist)



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
        self.assertEqual(len(validate("## Что изменено\nСделано.")), len(HEADINGS) + 2)

    def test_headings_in_code_fence_are_not_real(self):
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n```")), len(HEADINGS) + 2)

    def test_inner_info_string_does_not_close_fence(self):
        # Строка с info string внутри открытого блока — содержимое, а не его конец,
        # поэтому заголовки примера не становятся разделами реального body.
        nested = "```text\n```md\n" + valid_body() + "\n```\n"
        self.assertEqual(len(validate(nested)), len(HEADINGS) + 2)

    def test_closing_fence_requires_bare_marker(self):
        body = "```\n" + valid_body() + "\n```python\n```\n"
        self.assertEqual(len(validate(body)), len(HEADINGS) + 2)

    def test_tilde_and_longer_backtick_fences(self):
        self.assertEqual(len(validate("~~~md\n" + valid_body() + "\n~~~")), len(HEADINGS) + 2)
        # Более длинный маркер открывает блок, который не закрывается более коротким,
        # поэтому заголовки внутри остаются примером, а не разделами.
        self.assertEqual(len(validate("````md\n```\n" + valid_body() + "\n````")), len(HEADINGS) + 2)
        # Более длинный маркер способен закрыть блок, открытый коротким, но всё,
        # что уже лежало внутри, остаётся примером, а не разделом body.
        self.assertEqual(len(validate("```md\n" + valid_body() + "\n````")), len(HEADINGS) + 2)

    def test_headings_in_html_comment_are_not_real(self):
        # GitHub скрывает HTML-комментарии, поэтому шаблон внутри них не является телом PR.
        self.assertEqual(len(validate(f"<!--\n{valid_body()}\n-->")), len(HEADINGS) + 2)

    def test_unterminated_comment_hides_rest(self):
        body = f"## Цель\nИнформация.\n<!-- {chr(10)}" + "\n\n".join(f"## {n}\nИнформация." for n in HEADINGS[1:])
        self.assertTrue(any("Область изменений" in e for e in validate(body)))

    def test_inline_comment_keeps_section_text(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "<!-- пояснение -->\nФакт CI.")
        self.assertEqual(validate(body), [])

    def test_comment_marker_inside_fence_is_literal(self):
        # Внутри код-блока `<!--` — пример, а не комментарий, поэтому описанный
        # в примере заголовок не становится разделом реального body.
        self.assertEqual(len(validate("```md\n<!--\n" + valid_body() + "\n-->\n```")), len(HEADINGS) + 2)
        documented = valid_body() + "\n\n```md\n<!-- скрытый пример -->\n```"
        self.assertEqual(validate(documented), [])

    def test_atx_closing_hashes_are_not_part_of_heading_name(self):
        # `## Цель ##` — корректный ATX-заголовок, и GitHub показывает имя «Цель».
        text = {n: NOT_APPLICABLE if n == "Производительность" else "Информация." for n in HEADINGS}
        self.assertEqual(validate("\n\n".join(f"## {n} ##\n{text[n]}" for n in HEADINGS) + REPORT), [])
        self.assertEqual(validate("\n\n".join(f"## {n} ###\n{text[n]}" for n in HEADINGS) + REPORT), [])

    def test_fenced_content_does_not_make_section_nonempty(self):
        body = valid_body().replace("Фактическая информация для раздела CI.", "```md\nThis is a sample, not CI evidence.\n```")
        self.assertTrue(any("Пустой" in e for e in validate(body)))

    def test_coderabbit_table_in_fence_is_not_accepted(self):
        body = valid_body(False) + "\n\n## CodeRabbit review и disposition\n```markdown\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| 1 · x | — | — | 0 | clean | x |\n```"
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
        body = valid_body(False) + "\n\n## CodeRabbit review и disposition\nПроверено."
        self.assertTrue(any("CodeRabbit" in e for e in validate(body)))
        body += "\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `def` |"
        self.assertTrue(validate(body))

    def test_coderabbit_row_requires_reviewed_sha(self):
        body = valid_body(False) + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 | — | — | 0 findings | clean | unknown |"
        self.assertTrue(any("CodeRabbit" in e for e in validate(body)))

    def test_coderabbit_row_requires_filled_cells(self):
        # Строка из одних разделителей не является записью disposition.
        table = "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n"
        self.assertTrue(any("CodeRabbit" in e for e in validate(valid_body(False) + table + "| 1 · `abcdef0` | | | | | |")))
        valid_row = "| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"
        partial_row = "| 2 · `abcdef1` | | | | | |"
        self.assertTrue(validate(valid_body(False) + table + valid_row))
        invalid_table = validate(valid_body(False) + table + valid_row + "\n" + partial_row)
        self.assertTrue(any("CodeRabbit" in error for error in invalid_table), invalid_table)
        dangling_table = (
            valid_body(False)
            + table
            + valid_row
            + "\n\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n"
            + "| --- | --- | --- | --- | --- | --- |"
        )
        self.assertTrue(
            any("CodeRabbit" in error for error in validate(dangling_table))
        )

    def test_coderabbit_compact_summary_with_gist(self):
        self.assertEqual(validate(valid_body(False) + "\n\n" + COMPACT_CODERABBIT), [])

    def test_coderabbit_compact_summary_with_measurements(self):
        self.assertEqual(validate(body_with(MEASURED, False) + "\n\n" + COMPACT_CODERABBIT), [])

    def test_coderabbit_compact_summary_requires_valid_gist_url(self):
        invalid = COMPACT_CODERABBIT.replace(
            "https://gist.github.com/example/0123456789abcdef0123456789abcdef",
            "https://example.com/not-a-gist",
        )
        self.assertTrue(any("Gist" in e for e in validate(valid_body(False) + "\n\n" + invalid)))

    def test_coderabbit_compact_summary_requires_both_head_shas(self):
        invalid = COMPACT_CODERABBIT.replace(
            "\x601234567abcde\x60 (этот HEAD ещё не проверен)", "неизвестный HEAD"
        )
        self.assertTrue(any("HEAD" in e for e in validate(valid_body(False) + "\n\n" + invalid)))

    def test_coderabbit_summary_requires_quantitative_result(self):
        line = next(line for line in COMPACT_CODERABBIT.splitlines() if line.startswith("- **Итог:**"))
        for result in ("Подтверждённые замечания исправлены, блокирующих рисков нет.",
                       "Все замечания рассмотрены, оставшихся проблем не обнаружено."):
            with self.subTest(result=result):
                summary = COMPACT_CODERABBIT.replace(line, f"- **Итог:** {result}")
                self.assertTrue(any("количественные" in e for e in validate(valid_body(False) + "\n\n" + summary)))
        for result in ("Подтверждено 2 замечания, исправлены оба.",
                       "Найдено 0 замечаний; блокирующих рисков нет.",
                       "Подтверждено:\n  2 исправленных замечания, блокирующих нет."):
            with self.subTest(result=result):
                summary = COMPACT_CODERABBIT.replace(line, f"- **Итог:** {result}")
                self.assertEqual(validate(valid_body(False) + "\n\n" + summary), [])

    def test_coderabbit_progress_binds_shas_to_roles(self):
        original = ("3 завершённые итерации; последнее ревью `abcdef012345`\n"
                    "  (итерация 3), опубликован `1234567abcde` (этот HEAD ещё не проверен).")
        for progress in (
            "3 итерации; reviewed HEAD неизвестен, опубликованный HEAD неизвестен; `abcdef012345` и `1234567abcde`.",
            "3 итерации; reviewed HEAD `abcdef012345`, другой SHA `1234567abcde`.",
            "3 итерации; опубликован `abcdef012345`, другой SHA `1234567abcde`.",
            "3 итерации; reviewed HEAD `abcdef012345`, опубликован `abcdef012345`, другой `1234567abcde`.",
        ):
            with self.subTest(progress=progress):
                summary = COMPACT_CODERABBIT.replace(original, progress)
                self.assertTrue(any("HEAD" in e for e in validate(valid_body(False) + "\n\n" + summary)))
        for progress in (
            "3 итерации; reviewed HEAD: `abcdef012345`; опубликованный HEAD:\n  `1234567abcde`.",
            "3 итерации; `abcdef012345` — reviewed HEAD; `1234567abcde` — published HEAD.",
            "3 завершённые итерации; последнее ревью: `abcdef012345`\n  (итерация 3); опубликован `1234567abcde`.",
        ):
            with self.subTest(progress=progress):
                summary = COMPACT_CODERABBIT.replace(original, progress)
                self.assertEqual(validate(valid_body(False) + "\n\n" + summary), [])

    def test_coderabbit_compact_summary_requires_all_fields(self):
        for label in ("Прогресс", "Итог", "Существенное", "Полный журнал"):
            invalid = COMPACT_CODERABBIT.replace(f"**{label}:**", f"**Пропущено_{label}:**")
            self.assertTrue(
                any(f"«{label}»" in e for e in validate(valid_body(False) + "\n\n" + invalid)),
                label,
            )

    def test_legacy_coderabbit_table_requires_migration(self):
        body = valid_body(False) + "\n\n## CodeRabbit review и disposition\n" + LEGACY_CODERABBIT
        self.assertTrue(validate(body))

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
                    valid_body(False)
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
            valid_body(False)
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
                errors = validate(valid_body(False) + "\n\n" + duplicate)
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
                errors = validate(valid_body(False) + "\n\n" + malformed_duplicate)
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
                errors = validate(valid_body(False) + "\n\n" + summary)
                self.assertTrue(any(field in error for error in errors), errors)

    def test_empty_summary_field_cannot_borrow_next_list_item(self):
        for field in ("Итог", "Существенное"):
            line = next(line for line in COMPACT_CODERABBIT.splitlines() if line.startswith(f"- **{field}:**"))
            invalid = COMPACT_CODERABBIT.replace(line, f"- **{field}:**\n- Посторонний пункт содержит текст")
            with self.subTest(field=field):
                self.assertTrue(any(field in error for error in validate(valid_body(False) + "\n\n" + invalid)))

    def test_coderabbit_gist_link_must_be_in_full_log_field(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        misplaced = COMPACT_CODERABBIT.replace(
            f"- **Полный журнал:** [все результаты ревью]({url}).",
            "- **Полный журнал:** подробности в поле Итог.",
        ).replace(
            "- **Итог:** confirmed", f"- **Итог:** [журнал]({url}); confirmed"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body(False) + "\n\n" + misplaced)))

    def test_coderabbit_log_link_respects_list_item_boundary(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        link = f"[журнал]({url})"
        prefix = COMPACT_CODERABBIT.replace(f"[все результаты ревью]({url}).", "см. ниже.").rstrip()
        for suffix in (f"\n\n{link}", f"\n- Другое: {link}"):
            with self.subTest(suffix=suffix):
                self.assertTrue(any("Gist" in e for e in validate(valid_body(False) + "\n\n" + prefix + suffix)))
        for suffix in (f"\n  {link}", f"\n\n  {link}", f"\n{link}", f"\n\n\t{link}"):
            with self.subTest(suffix=suffix):
                self.assertEqual(validate(valid_body(False) + "\n\n" + prefix + suffix), [])
        with_link = COMPACT_CODERABBIT.rstrip() + f"\n\n{link}"
        self.assertTrue(validate(valid_body(False) + "\n\n" + with_link))

    def test_indented_code_is_not_coderabbit_summary_or_table(self):
        heading = "\n\n## CodeRabbit review и disposition\n\n"
        compact = COMPACT_CODERABBIT.split("\n", 1)[1].strip("\n")
        for source in (compact, LEGACY_CODERABBIT.strip("\n")):
            for indent in ("    ", "\t", "  \t"):
                with self.subTest(source=source[:20], indent=indent):
                    code = "\n".join(indent + line for line in source.splitlines())
                    self.assertTrue(validate(valid_body(False) + heading + code))
        # Отступленная строка без пустой строки — продолжение абзаца, не код.
        continued = COMPACT_CODERABBIT.replace("  (итерация 3)", "    (итерация 3)")
        self.assertEqual(validate(valid_body(False) + "\n\n" + continued), [])

    def test_coderabbit_gist_link_inside_inline_code_is_not_accepted(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        inline_code = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"`[все результаты ревью]({url})`"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body(False) + "\n\n" + inline_code)))

    def test_coderabbit_image_is_not_a_gist_link(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        image = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"![все результаты ревью]({url})"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body(False) + "\n\n" + image)))
        escaped_bang = image.replace("![все результаты", "\\![все результаты")
        self.assertEqual(validate(valid_body(False) + "\n\n" + escaped_bang), [])

    def test_coderabbit_escaped_gist_link_is_not_accepted(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        escaped = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"\\[все результаты ревью]({url})"
        )
        self.assertTrue(any("Gist" in error for error in validate(valid_body(False) + "\n\n" + escaped)))

    def test_coderabbit_gist_link_with_inline_code_label_remains_clickable(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        formatted_label = COMPACT_CODERABBIT.replace(
            f"[все результаты ревью]({url})", f"[`все результаты ревью`]({url})"
        )
        self.assertEqual(validate(valid_body(False) + "\n\n" + formatted_label), [])

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
        self.assertTrue(any("Gist" in error for error in validate(valid_body(False) + "\n\n" + commented)))

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
        self.assertTrue(any("Gist" in e for e in validate(valid_body(False) + "\n\n" + fake)))

    def test_nested_list_fences_do_not_supply_coderabbit_evidence(self):
        url = "https://gist.github.com/example/0123456789abcdef0123456789abcdef"
        original = f"[все результаты ревью]({url})."
        for marker in ("~~~", "```", "~~~~"):
            for indent in ("    ", "     ", "\t"):
                with self.subTest(marker=marker, indent=indent):
                    # Внутренние обратные кавычки и комментарий — литералы кода.
                    example = (f"нет ссылки\n{indent}{marker}md\n"
                               f"{indent}<!-- `пример`\n{indent}[журнал]({url})\n"
                               f"{indent}{marker}")
                    body = COMPACT_CODERABBIT.replace(original, example)
                    self.assertTrue(any("Gist" in e for e in validate(valid_body(False) + "\n\n" + body)))

        compact = COMPACT_CODERABBIT.split("\n", 1)[1].strip("\n")
        for source in (compact, LEGACY_CODERABBIT.strip("\n")):
            for opener in ("- пример\n    ~~~~md", "1. пример\n     ~~~~md", "- ~~~~md"):
                # Короткая граница не закрывает четырёхсимвольный блок.
                code = "\n".join("    " + line for line in source.splitlines())
                nested = f"{opener}\n    ~~~\n{code}\n    ~~~~"
                with self.subTest(opener=opener, source=source[:20]):
                    body = valid_body(False) + "\n\n## CodeRabbit review и disposition\n" + nested
                    self.assertTrue(any("CodeRabbit" in e for e in validate(body)))
        # Настоящая ссылка после закрытия блока остаётся видимой.
        body = COMPACT_CODERABBIT.replace(original, f"\n    ~~~\n    пример\n    ~~~\n  {original}")
        self.assertEqual(validate(valid_body(False) + "\n\n" + body), [])
        # Незакрытый блок прекращается при выходе из пункта списка.
        body = COMPACT_CODERABBIT.replace("- **Итог:**", "    ~~~\n    пример\n- **Итог:**")
        self.assertEqual(validate(valid_body(False) + "\n\n" + body), [])
        # Маркер внутри комментария не скрывает видимую сводку после него.
        comment = "<!--\n- пример\n    ~~~\n-->\n"
        self.assertEqual(validate(valid_body(False) + "\n\n" + comment + COMPACT_CODERABBIT), [])
        # Обычный кодовый блок не должен менять состояние внешнего списка.
        self.assertEqual(validate(valid_body(False) + "\n\n```md\n- пример\n```\n" + COMPACT_CODERABBIT), [])

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

    def test_measured_needs_compact_fields_and_numeric_result(self):
        for field in ("Сценарий", "База", "Кандидат", "Результат", "Вывод", "Регрессии"):
            broken = "\n".join(l for l in MEASURED.splitlines() if not l.startswith(f"- {field}:"))
            self.assertTrue(validate(body_with(broken)), field)
        broken = MEASURED.replace("wall-clock 100 ms → 80 ms, −20 %, n=10.", "нет числовых результатов")
        self.assertTrue(validate(body_with(broken)))

    def test_full_benchmark_report_must_move_to_gist(self):
        table = "\n| Метрика | После: среднее |\n|---|---|\n| wall | 80 ms |"
        self.assertTrue(any("полную таблицу" in e for e in validate(body_with(MEASURED + table))))

    def test_each_measured_scenario_needs_its_own_fields(self):
        self.assertTrue(validate(body_with(MEASURED + "\n### another\nРезультат: 1 ms")))
        unmeasured = "\n### slow — не измерено\nПричина: Docker недоступен\nДиагностика: check unsupported\nДоказательство производительности: отсутствует"
        self.assertEqual(validate(body_with(MEASURED + unmeasured)), [])
        self.assertTrue(validate(body_with(MEASURED + unmeasured.replace("Диагностика:", "Пропуск:"))))

    def test_open_pr_without_performance_section_gets_actionable_error(self):
        # Требование действует для уже открытых PR со следующего события
        # edited/synchronize: зелёной проверки без решения о применимости нет.
        legacy = "\n\n".join(f"## {n}\nФактическая информация." for n in HEADINGS if n != "Производительность")
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "synchronize", "pull_request": {"body": legacy, "user": {"login": "someone"}, "head": {"ref": "feat/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 1)
            self.assertIn("компактную сводку", proc.stderr)
            self.assertIn("не применимо", proc.stderr)
            # Исключение dependency-ботов сохраняется и для нового раздела.
            event.write_text(json.dumps({"pull_request": {"body": legacy, "user": {"login": "renovate[bot]"}, "head": {"ref": "renovate/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0)

    def test_performance_section_rejects_legacy_coderabbit_table(self):
        body = body_with(MEASURED, False) + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"
        self.assertTrue(validate(body))

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
