"""Тесты отчёта по журналу cargo test: счётчики, аннотации и обязательные тесты."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("report_cargo_tests.py")

PASSING = """\
     Running unittests src/lib.rs (target/ci/deps/asset_store-0123)

running 3 tests
test jpdb::tests::browser_capture_takes_one ... ok
test yarxi::tests::acceptance_rotation_is_opt_in ... ok
test jpdb::tests::browser_search_script ... ignored, требует установленного Chromium

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 719 filtered out; finished in 1.20s
"""

FAILING = """\
running 2 tests
test a::first ... ok
test b::second ... FAILED

failures:

---- b::second stdout ----
thread 'b::second' panicked at /home/runner/work/secret/path.rs:1:1

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s

error: test failed, to rerun pass `-p asset-store --lib`
"""


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.summary = self.root / "summary.md"

    def run_report(self, log_text, *extra):
        log = self.root / "test.log"
        if log_text is not None:
            log.write_text(log_text, encoding="utf-8")
        env = {**os.environ, "GITHUB_STEP_SUMMARY": str(self.summary)}
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--log", str(log), "--title", "Тесты", "--step", "Шаг", *extra],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )

    def summary_text(self):
        return self.summary.read_text(encoding="utf-8")

    def test_successful_log_reports_counts_without_annotations(self):
        result = self.run_report(PASSING)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("::error", result.stdout)
        self.assertIn("| 2 | 0 | 1 | 719 | 1 |", self.summary_text())

    def test_failed_test_is_annotated_but_report_mode_does_not_change_exit_code(self):
        result = self.run_report(FAILING)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("::error title=Тесты::Тест b::second упал", result.stdout)
        summary = self.summary_text()
        self.assertIn("- `b::second`", summary)
        # Вывод теста и пути исполнителя в сводку не копируются.
        self.assertNotIn("panicked", summary)
        self.assertNotIn("/home/runner", summary)

    def test_required_tests_must_actually_pass(self):
        ok = self.run_report(
            PASSING, "--require", "jpdb::tests::browser_capture_takes_one",
            "--require", "yarxi::tests::acceptance_rotation_is_opt_in",
        )
        self.assertEqual(ok.returncode, 0, ok.stdout)
        self.assertIn("| `jpdb::tests::browser_capture_takes_one` | прошёл |", self.summary_text())

    def test_ignored_required_test_is_a_failure_not_a_success(self):
        result = self.run_report(PASSING, "--require", "jpdb::tests::browser_search_script")
        self.assertEqual(result.returncode, 1)
        self.assertIn("пропущен (`ignored`)", result.stdout)

    def test_missing_required_test_is_a_failure(self):
        result = self.run_report(PASSING, "--require", "jpdb::tests::renamed")
        self.assertEqual(result.returncode, 1)
        self.assertIn("не исполнялся", result.stdout)

    def test_failed_required_test_is_a_failure(self):
        result = self.run_report(FAILING, "--require", "b::second")
        self.assertEqual(result.returncode, 1)
        self.assertIn("| `b::second` | упал |", self.summary_text())

    def test_missing_log_fails_only_in_require_mode(self):
        self.assertEqual(self.run_report(None).returncode, 0)
        self.assertIn("не выполнялся", self.summary_text())
        self.assertEqual(self.run_report(None, "--require", "a::first").returncode, 1)

    def test_compile_failure_and_color_codes_are_recognised(self):
        log = "\x1b[1m\x1b[91merror\x1b[0m: could not compile `asset-store` (lib test)\n"
        result = self.run_report(log)
        self.assertEqual(result.returncode, 0)
        self.assertIn("Сборка asset-store не удалась", result.stdout)
        self.assertIn("тесты не запускались", self.summary_text())

    def test_annotation_data_is_escaped(self):
        log = "test a::b ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0s\n"
        result = self.run_report(log, "--title", "x:y,z")
        # Последний `--title` побеждает; символы свойства экранируются.
        self.assertIn("::error title=x%3Ay%2Cz::", result.stdout)


if __name__ == "__main__":
    unittest.main()
