"""Контракт LCOV: пути рабочей копии, реальные строки и отказ от пустого покрытия."""

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import check_rust_lcov

from check_rust_lcov import check, resolve_report, tracked_paths, write_sonar_report


class RustLcovTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        source = self.root / "tools/example/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text("fn example() {}\n", encoding="utf-8")
        subprocess.run(["git", "add", "tools/example/src/lib.rs"], cwd=self.root, check=True)
        (self.root / "sonar-project.properties").write_text(
            (Path(__file__).resolve().parents[2] / "sonar-project.properties").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        self.report = self.root / "lcov.info"
        self.tracked = tracked_paths(self.root)

    def run_check(self, contents):
        self.report.write_text(contents, encoding="utf-8")
        return check(self.report, self.root, self.tracked)

    def test_accepts_tracked_source_with_executed_lines(self):
        self.assertEqual(self.run_check("SF:tools/example/src/lib.rs\nDA:1,3\nend_of_record\n"), (1, 1, 1))

    def test_executed_tests_do_not_replace_production_coverage(self):
        paths = ("src/tests.rs", "src/test_support.rs", "src/nested/tests.rs",
                 "src/nested/parser_tests.rs", "src/nested/fake_test_support.rs",
                 "src/nested/test_ports.rs", "tests/integration.rs", "src/fixtures/sample.rs")
        for path in paths:
            relative = f"tools/example/{path}"
            source = self.root / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text("fn example() {}\n", encoding="utf-8")
            subprocess.run(["git", "add", relative], cwd=self.root, check=True)
            self.tracked = tracked_paths(self.root)
            test_report = f"SF:{relative}\nDA:1,5\nend_of_record\n"
            with self.subTest(path=path):
                with self.assertRaises(ValueError):
                    self.run_check(test_report)
                with self.assertRaises(ValueError):
                    self.run_check(test_report + "SF:tools/example/src/lib.rs\nDA:1,0\nend_of_record\n")
                self.assertEqual(self.run_check(
                    test_report + "SF:tools/example/src/lib.rs\nDA:1,1\nend_of_record\n"), (1, 2, 2))

    def test_uses_scanner_classification_instead_of_a_second_list(self):
        config = self.root / "sonar-project.properties"
        lines = config.read_text(encoding="utf-8").splitlines()
        config.write_text("\n".join("sonar.exclusions=tools/example/src/lib.rs"
                                   if line.startswith("sonar.exclusions=") else line
                                   for line in lines) + "\n", encoding="utf-8")
        with self.assertRaises(ValueError):
            self.run_check("SF:tools/example/src/lib.rs\nDA:1,1\nend_of_record\n")

    def test_rejects_absent_coverage_and_nonexistent_sources(self):
        for report in ("", "SF:tools/example/src/lib.rs\nDA:1,0\nend_of_record\n",
                       "SF:tools/example/src/missing.rs\nDA:1,1\nend_of_record\n",
                       "DA:1,1\n", "SF:tools/example/src/lib.rs\nDA:1,bad\n",
                       "SF:tools/example/src/lib.rs\nDA:0,1\nend_of_record\n",
                       "SF:tools/example/src/lib.rs\nDA:1,١\nend_of_record\n"):
            with self.subTest(report=report), self.assertRaises(ValueError):
                self.run_check(report)

    def test_rejects_outside_or_untracked_sources(self):
        untracked = self.root / "tools/example/src/untracked.rs"
        untracked.write_text("", encoding="utf-8")
        for path in ("/etc/passwd", "tools/example/src/untracked.rs"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.run_check(f"SF:{path}\nDA:1,1\nend_of_record\n")

    def test_sonar_int32_adapter_preserves_line_coverage_and_original_counts(self):
        maximum = 2**31 - 1
        raw = (f"SF:tools/example/src/lib.rs\nFN:1,example\nFNDA:{2**64 - 1},example\n"
               f"DA:1,0\nDA:2,1\nDA:3,{maximum}\nDA:4,{maximum + 1},checksum\n"
               f"DA:5,{2**64 - 1}\nLF:5\nLH:4\nend_of_record\n")
        self.report.write_text(raw, encoding="utf-8")
        expected = check(self.report, self.root, self.tracked)
        output = self.root / "lcov-sonar.info"
        self.assertEqual(write_sonar_report(self.report, output), 2)
        normalized = output.read_text(encoding="utf-8")
        self.assertEqual(self.report.read_text(encoding="utf-8"), raw)
        self.assertEqual(check(output, self.root, self.tracked), expected)
        self.assertEqual(normalized, raw.replace(f"DA:4,{maximum + 1}", f"DA:4,{maximum}")
                         .replace(f"DA:5,{2**64 - 1}", f"DA:5,{maximum}"))
        with self.assertRaises(FileExistsError):
            write_sonar_report(self.report, output)
        self.assertEqual(output.read_text(encoding="utf-8"), normalized)

    def test_cli_validates_before_creating_sonar_report_and_refuses_stale_output(self):
        output = self.root / check_rust_lcov.read_properties(
            self.root / "sonar-project.properties")["sonar.rust.lcov.reportPaths"]
        with patch.object(check_rust_lcov, "ROOT", self.root), patch("sys.argv", ["check", str(self.report)]):
            self.report.write_text("SF:tools/example/src/lib.rs\nDA:1,0\nend_of_record\n", encoding="utf-8")
            with self.assertRaises(SystemExit):
                check_rust_lcov.main()
            self.assertFalse(output.exists())
            self.report.write_text("SF:tools/example/src/lib.rs\nDA:1,1\nend_of_record\n", encoding="utf-8")
            check_rust_lcov.main()
            self.assertTrue(output.is_file())
            with self.assertRaises(SystemExit):
                check_rust_lcov.main()

    def test_report_must_stay_inside_checkout(self):
        inside = self.root / "lcov.info"
        inside.write_text("", encoding="utf-8")
        self.assertEqual(resolve_report(str(inside), self.root), inside.resolve())
        outside_dir = tempfile.TemporaryDirectory()
        self.addCleanup(outside_dir.cleanup)
        outside = Path(outside_dir.name) / "lcov.info"
        outside.write_text("", encoding="utf-8")
        with self.assertRaises(ValueError):
            resolve_report(str(outside), self.root)
        with self.assertRaises(ValueError):
            resolve_report(str(self.root / "missing-lcov.info"), self.root)


if __name__ == "__main__":
    unittest.main()