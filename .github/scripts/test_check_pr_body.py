"""Negative and positive contract tests for PR body formatting."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_pr_body import is_automation_pr, validate

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

- Сценарий: `anki-repo.validate` · warm · 4321 notes · n=10
- База: commit `bbbbbbbbbbbb`, проверенный сопоставимый результат
- Кандидат: commit `cccccccccccc`, проверенный результат
- Результат: время 100 ms → 80 ms (−20 %); разброс ±2 ms
- Вывод: сравнимые условия, улучшение больше разброса
- Регрессии: нет существенных
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
        self.assertTrue(any("таблицы" in e for e in validate(body)))

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
        self.assertTrue(any("таблицы" in e for e in validate(body)))
        body += "\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 findings | clean | `def` |"
        self.assertEqual(validate(body), [])

    def test_coderabbit_row_requires_reviewed_sha(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 | — | — | 0 findings | clean | unknown |"
        self.assertTrue(any("строка" in e for e in validate(body)))

    def test_coderabbit_row_requires_filled_cells(self):
        # Строка из одних разделителей не является записью disposition.
        table = "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n"
        self.assertTrue(any("строка" in e for e in validate(valid_body() + table + "| 1 · `abcdef0` | | | | | |")))
        self.assertEqual(validate(valid_body() + table + "| 1 · `abcdef0` | — | — | 0 findings | clean | `abcdef0` |"), [])

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

    def test_compact_measured_requires_all_decision_fields(self):
        for name in ("Сценарий", "База", "Кандидат", "Результат", "Вывод", "Регрессии"):
            missing = "\n".join(line for line in MEASURED.splitlines() if not line.startswith("- " + name + ":"))
            self.assertTrue(any(name in problem for problem in validate(body_with(missing))), name)
        bad = MEASURED.replace("100 ms → 80 ms (−20 %)", "вроде быстрее")
        self.assertTrue(any("числового" in e for e in validate(body_with(bad))))

    def test_long_performance_is_rejected_without_gist_requirement(self):
        performance = MEASURED + "\nПримечание: " + ("подтверждение " * 190)
        errors = validate(body_with(performance))
        self.assertTrue(any("2200" in e for e in errors), errors)
        self.assertEqual(validate(body_with(MEASURED)), [])

    def test_no_full_raw_metric_tables_in_performance(self):
        report = MEASURED + "\n| Метрика | До | После |\n|---|---|---|\n| time | 100 ms | 80 ms |"
        self.assertTrue(any("полную таблицу" in e for e in validate(body_with(report))))

    def test_each_measured_scenario_requires_own_fields(self):
        secondary = "\n### `anki-repo.stats` · профиль `warm`\n- Сценарий: `anki-repo.stats` · warm"
        problems = validate(body_with(MEASURED + secondary))
        self.assertTrue(any("anki-repo.stats" in e and "Регрессии" in e for e in problems), problems)
        unmeasured = "\n### `anki-e2e.full` — не измерено\n- Причина: Docker недоступен\n- Диагностика: unsupported, прогонов 0\n- Доказательство производительности: отсутствует"
        self.assertEqual(validate(body_with(MEASURED + unmeasured)), [])

    def test_body_cap_unicode_and_line_endings(self):
        base = valid_body()
        self.assertLess(len(base), 10_000)
        exact = base + "😀" * (10_000 - len(base))
        self.assertEqual(validate(exact), [])
        self.assertTrue(any("10000" in error for error in validate(exact + "😀")))
        self.assertEqual(validate(base.replace("\n", "\r\n")), [])
        self.assertEqual(validate(base.replace("\n", "\r")), [])

    def test_gist_is_not_required(self):
        self.assertEqual(validate(valid_body()), [])
        self.assertFalse(any("Gist" in error for error in validate(valid_body())))

    def test_open_pr_without_performance_section_gets_actionable_error(self):
        # Требование действует для уже открытых PR со следующего события
        # edited/synchronize: зелёной проверки без решения о применимости нет.
        legacy = "\n\n".join(f"## {n}\nФактическая информация." for n in HEADINGS if n != "Производительность")
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "synchronize", "pull_request": {"body": legacy, "user": {"login": "someone"}, "head": {"ref": "feat/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 1)
            self.assertIn("компактную", proc.stderr)
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
