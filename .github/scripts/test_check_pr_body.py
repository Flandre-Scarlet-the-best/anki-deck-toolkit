"""Отрицательные и положительные тесты контракта описания PR и доверенной проверки."""

import json
import re
import shutil
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


CR_HEADER = "| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |"
CR_DELIMITER = "| --- | --- | --- | --- | --- | --- |"
CR_ROW = "| 1 · `abcdef0` | — | — | 0 замечаний | clean | `abcdef0` |"


def coderabbit(*lines: str) -> str:
    return valid_body() + "\n\n## CodeRabbit review и disposition\n" + "\n".join(lines)


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
        body = valid_body().replace("Фактическая информация для раздела CI.", "```md\nЭто пример, а не свидетельство CI.\n```")
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
        body += "\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 замечаний | clean | `def` |"
        self.assertEqual(validate(body), [])

    def test_coderabbit_row_requires_reviewed_sha(self):
        body = valid_body() + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 | — | — | 0 замечаний | clean | unknown |"
        self.assertTrue(any("строка" in e for e in validate(body)))

    def test_coderabbit_row_requires_filled_cells(self):
        # Строка из одних разделителей не является записью disposition.
        table = "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n"
        self.assertTrue(any("строка" in e for e in validate(valid_body() + table + "| 1 · `abcdef0` | | | | | |")))
        self.assertEqual(validate(valid_body() + table + "| 1 · `abcdef0` | — | — | 0 замечаний | clean | `abcdef0` |"), [])

    def test_coderabbit_every_row_is_checked(self):
        # Некорректная строка не прячется за соседней корректной.
        for bad in ("| 2 | major | `a.rs` | x | confirmed | `abcdef0` |", "| 2 · `abcdef0` | major | | | | |"):
            errors = validate(coderabbit(CR_HEADER, CR_DELIMITER, CR_ROW, bad))
            self.assertTrue(any("не связана" in e for e in errors), (bad, errors))

    def test_coderabbit_table_requires_delimiter_row(self):
        # Без строки-разделителя GitHub показывает две строки текста, а не таблицу.
        errors = validate(coderabbit(CR_HEADER, CR_ROW))
        self.assertTrue(any("разделителя" in e for e in errors), errors)
        errors = validate(coderabbit(CR_HEADER, "Пояснение.", CR_DELIMITER, CR_ROW))
        self.assertTrue(any("разделителя" in e for e in errors), errors)

    def test_coderabbit_delimiter_must_match_header_columns(self):
        for delimiter in ("| --- | --- | --- | --- | --- |", "| --- | --- | --- | --- | --- | --- | --- |", "| --- | --- | x | --- | --- | --- |"):
            errors = validate(coderabbit(CR_HEADER, delimiter, CR_ROW))
            self.assertTrue(any("разделителя" in e for e in errors), (delimiter, errors))

    def test_coderabbit_rows_must_have_all_columns(self):
        short = "| 1 · `abcdef0` | — | — | clean | `abcdef0` |"
        errors = validate(coderabbit(CR_HEADER, CR_DELIMITER, short))
        self.assertTrue(any("5 столбцов" in e for e in errors), errors)
        # Неполная строка не прячется за соседней корректной.
        errors = validate(coderabbit(CR_HEADER, CR_DELIMITER, CR_ROW, short))
        self.assertTrue(any("5 столбцов" in e for e in errors), errors)

    def test_coderabbit_row_must_belong_to_the_table(self):
        # Пустая строка или абзац завершают таблицу: строка итерации после них
        # уже не связана с заголовком.
        for gap in ("", "Отдельный абзац без разделителей"):
            errors = validate(coderabbit(CR_HEADER, CR_DELIMITER, gap, CR_ROW))
            self.assertTrue(any("строка завершённой итерации" in e for e in errors), (gap, errors))
        # Отступ в четыре пробела делает строки код-блоком, а не таблицей.
        errors = validate(coderabbit("    " + CR_HEADER, "    " + CR_DELIMITER, "    " + CR_ROW))
        self.assertTrue(any("заголовок" in e for e in errors), errors)

    def test_coderabbit_single_cumulative_table(self):
        errors = validate(coderabbit(CR_HEADER, CR_DELIMITER, CR_ROW, "", CR_HEADER, CR_DELIMITER, CR_ROW))
        self.assertTrue(any("повторяется" in e for e in errors), errors)

    def test_coderabbit_valid_gfm_variants_are_accepted(self):
        second = "| 2 · `0123456789abcdef0123456789abcdef01234567` | major | `src/a.rs` | потеря данных | confirmed | исправлено `1234567` |"
        self.assertEqual(validate(coderabbit("Накопительный журнал ревью.", CR_HEADER, CR_DELIMITER, CR_ROW, second, "", "Итог: блокирующих нет.")), [])
        aligned = "|:---|:---:|---:|---|---|---|"
        self.assertEqual(validate(coderabbit(CR_HEADER, aligned, CR_ROW)), [])
        bare = [line.strip().strip("|").strip() for line in (CR_HEADER, CR_DELIMITER, CR_ROW)]
        self.assertEqual(validate(coderabbit(*bare)), [])
        escaped = "| 1 · `abcdef0` | minor | `a\\|b.md` | разметка | false positive | — |"
        self.assertEqual(validate(coderabbit(CR_HEADER, CR_DELIMITER, escaped)), [])

    def test_coderabbit_section_stays_conditional(self):
        self.assertEqual(validate(valid_body()), [])

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
        bad = MEASURED.replace("- Результат: время 100 ms → 80 ms (−20 %); разброс ±2 ms", "- Результат: вроде быстрее")
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
        body = body_with(MEASURED) + "\n\n## CodeRabbit review и disposition\n| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n| --- | --- | --- | --- | --- | --- |\n| 1 · `abcdef0` | — | — | 0 замечаний | clean | `abcdef0` |"
        self.assertEqual(validate(body), [])

    def test_bot_exception_is_limited_to_account_and_branch(self):
        def payload(login, branch):
            return {"pull_request": {"user": {"login": login}, "head": {"ref": branch}}}
        self.assertTrue(is_automation_pr(payload("renovate[bot]", "renovate/rust-sha2")))
        self.assertTrue(is_automation_pr(payload("dependabot[bot]", "dependabot/github_actions/action")))
        self.assertFalse(is_automation_pr(payload("someone", "renovate/fake")))
        self.assertFalse(is_automation_pr(payload("renovate[bot]", "feat/other")))

    def test_cli_accepts_pull_request_target_payload(self):
        # Событие `pull_request_target` несёт тот же объект `pull_request`.
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "edited", "pull_request": {"body": valid_body(), "user": {"login": "someone"}, "head": {"ref": "feat/x"}}}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_cli_rejects_event_without_pull_request(self):
        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "edited"}), encoding="utf-8")
            proc = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")), "--event-path", str(event)], capture_output=True, text=True)
            self.assertEqual(proc.returncode, 2)
            self.assertIn("pull_request", proc.stderr)


WORKFLOWS = Path(__file__).resolve().parents[1] / "workflows"
TRUSTED = WORKFLOWS / "pr-body-trusted.yml"


class TrustedWorkflowTests(unittest.TestCase):
    """Доверенная проверка не исполняет код PR и берёт валидатор только из base."""

    def setUp(self):
        self.text = TRUSTED.read_text(encoding="utf-8")
        # Комментарии не участвуют в контракте: проверяется только сам YAML.
        self.yaml = "\n".join(line for line in self.text.splitlines() if not line.lstrip().startswith("#"))

    def test_runs_only_on_pull_request_target(self):
        triggers = re.search(r"(?m)^on:\n((?:[ \t]+.*\n)+)", self.yaml + "\n").group(1)
        self.assertEqual(re.findall(r"(?m)^  (\w+):", triggers), ["pull_request_target"])

    def test_checks_out_exact_base_sha_without_credentials(self):
        checkouts = re.findall(r"uses: actions/checkout@", self.yaml)
        self.assertEqual(len(checkouts), 1)
        self.assertIn("ref: ${{ github.event.pull_request.base.sha }}", self.yaml)
        self.assertIn("persist-credentials: false", self.yaml)

    def test_no_head_code_secrets_or_write_access(self):
        for forbidden in ("head.sha", "head.ref", "head.repo", "merge_commit_sha", "refs/pull", "secrets.", "github.token", "GITHUB_TOKEN", ": write", "write-all"):
            self.assertNotIn(forbidden, self.yaml, forbidden)
        permissions = re.search(r"(?m)^permissions:\n((?:[ \t]+.*\n)+)", self.yaml + "\n").group(1)
        self.assertEqual(permissions.split(), ["contents:", "read"])

    def test_runs_validator_from_checked_out_base(self):
        runs = re.findall(r"(?m)^\s+run: (.+)$", self.yaml)
        self.assertEqual(runs, ['python3 .github/scripts/check_pr_body.py --event-path "$GITHUB_EVENT_PATH"'])
        self.assertIn("name: Доверенная структура PR", self.yaml)

    def test_validator_replaced_in_pr_does_not_change_trusted_result(self):
        # PR заменяет валидатор безусловным успехом. Доверенный шаг исполняет
        # команду workflow в рабочей копии base SHA, поэтому невалидное описание
        # по-прежнему отклоняется; та же команда в рабочей копии head доказывает,
        # что подмена сработала бы, если бы источником был сам PR.
        command = re.findall(r"(?m)^\s+run: (.+)$", self.yaml)[0]
        source = Path(__file__).with_name("check_pr_body.py")
        with tempfile.TemporaryDirectory() as tmp:
            base, head = Path(tmp) / "base", Path(tmp) / "head"
            for root in (base, head):
                (root / ".github" / "scripts").mkdir(parents=True)
            shutil.copy(source, base / ".github" / "scripts" / "check_pr_body.py")
            (head / ".github" / "scripts" / "check_pr_body.py").write_text("import sys\nsys.exit(0)\n", encoding="utf-8")
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps({"action": "edited", "pull_request": {"body": "## Цель\nТолько цель.", "user": {"login": "someone"}, "head": {"ref": "feat/x"}}}), encoding="utf-8")
            env = {"GITHUB_EVENT_PATH": str(event), "PATH": "/usr/bin:/bin"}
            command = command.replace("python3", shutil.which("python3") or sys.executable, 1)
            trusted = subprocess.run(command, shell=True, cwd=base, env=env, capture_output=True, text=True)
            tampered = subprocess.run(command, shell=True, cwd=head, env=env, capture_output=True, text=True)
        self.assertEqual(trusted.returncode, 1, trusted.stderr)
        self.assertIn("Отсутствует обязательный заголовок", trusted.stderr)
        self.assertEqual(tampered.returncode, 0)


if __name__ == "__main__":
    unittest.main()
