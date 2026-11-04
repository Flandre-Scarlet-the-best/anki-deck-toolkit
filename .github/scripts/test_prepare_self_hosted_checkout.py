"""Тесты подготовки постоянной рабочей копии self-hosted раннера.

Каждый тест строит синтетическую раскладку раннера во временном каталоге
(``<tmp>/runner/_work/repo/repo`` с собственным Git) и фиктивные файлы.
Настоящие пользовательские данные и рабочие копии не затрагиваются.
"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("prepare_self_hosted_checkout.py")
SPEC = importlib.util.spec_from_file_location("prepare_self_hosted_checkout", SCRIPT)
prepare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare)


def git(cwd, *args):
    subprocess.run(["git", "-C", str(cwd), *args], check=True, capture_output=True)


class RunnerLayout(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runner_workspace = self.root / "runner" / "_work" / "repo"
        self.workspace = self.runner_workspace / "repo"
        self.workspace.mkdir(parents=True)
        git(self.workspace, "init", "-q")
        (self.workspace / ".gitignore").write_text("/target/\n/decks/\n/.asset-store/\n__pycache__/\n")
        (self.workspace / "tracked.txt").write_text("source\n")
        git(self.workspace, "add", ".")
        git(self.workspace, "-c", "user.name=t", "-c", "user.email=t@example.invalid",
            "commit", "-q", "-m", "init")
        (self.workspace / "target" / "debug").mkdir(parents=True)
        (self.workspace / "target" / "debug" / "artifact").write_bytes(b"build")
        (self.workspace / "stray.log").write_text("leftover")
        (self.workspace / "__pycache__").mkdir()
        (self.workspace / "__pycache__" / "x.pyc").write_bytes(b"pyc")
        self.tool_cache = self.root / "tools"

    def run_script(self, *extra, workspace=None, cwd=None, runner_workspace=None):
        workspace = workspace or self.workspace
        env = dict(os.environ, GITHUB_WORKSPACE=str(workspace),
                   RUNNER_WORKSPACE=str(runner_workspace or self.runner_workspace))
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--tool-cache", str(self.tool_cache),
             "--min-free-gib", "0", *extra],
            cwd=cwd or workspace, env=env, capture_output=True, text=True, check=False,
        )

    def assert_untouched(self):
        self.assertTrue((self.workspace / "stray.log").exists())
        self.assertTrue((self.workspace / "__pycache__" / "x.pyc").exists())
        self.assertTrue((self.workspace / "target" / "debug" / "artifact").exists())


class IsolationTests(RunnerLayout):
    def test_dedicated_checkout_is_cleaned_and_target_kept(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.workspace / "stray.log").exists())
        self.assertFalse((self.workspace / "__pycache__").exists())
        self.assertEqual((self.workspace / "target" / "debug" / "artifact").read_bytes(), b"build")
        self.assertEqual((self.workspace / "tracked.txt").read_text(), "source\n")

    def test_user_data_areas_refuse_before_any_deletion(self):
        for area in ("decks", ".asset-store"):
            with self.subTest(area=area):
                private = self.workspace / area / "output" / "Deck" / "deck.json"
                private.parent.mkdir(parents=True)
                private.write_text("{}")
                result = self.run_script()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("пользовательских данных", result.stdout)
                self.assertIn(area, result.stdout)
                self.assertEqual(private.read_text(), "{}")
                self.assert_untouched()
                subprocess.run(["rm", "-rf", str(self.workspace / area)], check=True)

    def test_symlinked_user_data_area_is_refused(self):
        outside = self.root / "outside-decks"
        outside.mkdir()
        (self.workspace / "decks").symlink_to(outside)
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assert_untouched()

    def test_checkout_outside_runner_work_is_refused(self):
        foreign = self.root / "projects" / "repo"
        foreign.parent.mkdir()
        self.workspace.rename(foreign)
        result = self.run_script(workspace=foreign)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("_work", result.stdout)
        self.assertTrue((foreign / "stray.log").exists())

    def test_runner_workspace_without_work_parent_is_refused(self):
        result = self.run_script(runner_workspace=self.workspace.parent.parent)
        self.assertNotEqual(result.returncode, 0)
        self.assert_untouched()

    def test_cwd_other_than_workspace_is_refused(self):
        result = self.run_script(cwd=self.root)
        self.assertNotEqual(result.returncode, 0)
        self.assert_untouched()

    def test_linked_worktree_is_refused(self):
        linked = self.runner_workspace / "linked"
        git(self.workspace, "worktree", "add", "-q", str(linked))
        (linked / "stray.log").write_text("leftover")
        result = self.run_script(workspace=linked)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("worktree", result.stdout)
        self.assertTrue((linked / "stray.log").exists())

    def test_modified_tracked_file_fails_after_clean(self):
        (self.workspace / "tracked.txt").write_text("changed\n")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("не совпадает с HEAD", result.stdout)


class MaintenanceTests(RunnerLayout):
    def setUp(self):
        super().setUp()
        self.addCleanup(setattr, prepare, "log", prepare.log)
        prepare.log = lambda _message: None

    def test_target_over_limit_is_removed(self):
        result = self.run_script("--target-limit-gib", "0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.workspace / "target").exists())

    def test_target_within_limit_and_fresh_tools_are_kept(self):
        self.tool_cache.mkdir()
        (self.tool_cache / "fresh.tar.gz").write_bytes(b"x")
        old = self.tool_cache / "old.tar.gz"
        old.write_bytes(b"y")
        stale = time.time() - 60 * 86400
        os.utime(old, (stale, stale))
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.workspace / "target" / "debug" / "artifact").exists())
        self.assertTrue((self.tool_cache / "fresh.tar.gz").exists())
        self.assertFalse(old.exists())

    def test_low_free_space_drops_target_first(self):
        readings = iter([1, 100])
        prepare.maintain(self.workspace, None, target_limit=1 << 40, min_free=10,
                         retention_seconds=1, free=lambda _path: next(readings))
        self.assertFalse((self.workspace / "target").exists())

    def test_low_free_space_even_after_dropping_target_fails(self):
        with self.assertRaises(prepare.Refused):
            prepare.maintain(self.workspace, None, target_limit=1 << 40, min_free=10,
                             retention_seconds=1, free=lambda _path: 1)
        self.assertFalse((self.workspace / "target").exists())

    def test_enough_free_space_keeps_target(self):
        prepare.maintain(self.workspace, None, target_limit=1 << 40, min_free=10,
                         retention_seconds=1, free=lambda _path: 100)
        self.assertTrue((self.workspace / "target" / "debug" / "artifact").exists())


if __name__ == "__main__":
    unittest.main()
