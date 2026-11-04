"""Тесты загрузки архива инструмента CI с проверкой SHA-256 и повторным использованием."""

import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("fetch_verified_tool.py")


class FetchVerifiedToolTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.source = root / "source.tar.gz"
        self.source.write_bytes(b"tool archive v1")
        self.sha = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.url = self.source.as_uri()
        self.runner_temp = root / "runner-temp"
        self.cache = root / "tool-cache"
        self.runner_temp.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def run_script(self, *, sha=None, url=None, cache=None):
        env = {k: v for k, v in os.environ.items() if k not in ("CI_TOOL_CACHE", "RUNNER_TEMP")}
        env["RUNNER_TEMP"] = str(self.runner_temp)
        if cache is not None:
            env["CI_TOOL_CACHE"] = str(cache)
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--url", url or self.url,
             "--sha256", sha or self.sha, "--name", "tool.tar.gz"],
            env=env, capture_output=True, text=True, check=False,
        )

    def leftovers(self, directory):
        return sorted(p.name for p in directory.iterdir() if p.name != "tool.tar.gz")

    def test_without_cache_downloads_into_runner_temp(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        path = Path(result.stdout.strip())
        self.assertEqual(path, self.runner_temp / "tool.tar.gz")
        self.assertEqual(path.read_bytes(), b"tool archive v1")
        self.assertEqual(self.leftovers(self.runner_temp), [])

    def test_cached_verified_file_is_reused_without_download(self):
        self.assertEqual(self.run_script(cache=self.cache).returncode, 0)
        missing = (Path(self.tmp.name) / "absent").as_uri()
        result = self.run_script(cache=self.cache, url=missing)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(result.stdout.strip()), self.cache / "tool.tar.gz")
        self.assertIn("SHA-256 совпал", result.stderr)

    def test_corrupted_cached_file_is_replaced_by_verified_download(self):
        self.cache.mkdir()
        (self.cache / "tool.tar.gz").write_bytes(b"corrupted")
        result = self.run_script(cache=self.cache)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cache / "tool.tar.gz").read_bytes(), b"tool archive v1")
        self.assertEqual(self.leftovers(self.cache), [])

    def test_checksum_mismatch_fails_and_leaves_no_file(self):
        result = self.run_script(cache=self.cache, sha="0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("не совпадает", result.stderr)
        self.assertEqual(sorted(p.name for p in self.cache.iterdir()), [])

    def test_name_with_directory_is_rejected(self):
        env = dict(os.environ, RUNNER_TEMP=str(self.runner_temp))
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--url", self.url, "--sha256", self.sha,
             "--name", "../escape"],
            env=env, capture_output=True, text=True, check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((Path(self.tmp.name) / "escape").exists())


if __name__ == "__main__":
    unittest.main()
