"""Контракт LCOV: пути checkout, реальные строки и отказ от пустого покрытия."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from check_rust_lcov import check, resolve_report, tracked_paths


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
        self.report = self.root / "lcov.info"
        self.tracked = tracked_paths(self.root)

    def run_check(self, contents):
        self.report.write_text(contents, encoding="utf-8")
        return check(self.report, self.root, self.tracked)

    def test_accepts_tracked_source_with_executed_lines(self):
        self.assertEqual(self.run_check("SF:tools/example/src/lib.rs\nDA:1,3\nend_of_record\n"), (1, 1, 1))

    def test_rejects_absent_coverage_and_nonexistent_sources(self):
        for report in ("", "SF:tools/example/src/lib.rs\nDA:1,0\nend_of_record\n",
                       "SF:tools/example/src/missing.rs\nDA:1,1\nend_of_record\n",
                       "DA:1,1\n", "SF:tools/example/src/lib.rs\nDA:1,bad\n"):
            with self.subTest(report=report), self.assertRaises(ValueError):
                self.run_check(report)

    def test_rejects_outside_or_untracked_sources(self):
        untracked = self.root / "tools/example/src/untracked.rs"
        untracked.write_text("", encoding="utf-8")
        for path in ("/etc/passwd", "tools/example/src/untracked.rs"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.run_check(f"SF:{path}\nDA:1,1\nend_of_record\n")

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