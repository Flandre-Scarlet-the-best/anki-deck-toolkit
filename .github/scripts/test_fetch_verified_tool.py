"""Тесты загрузки архива инструмента CI: проверка каждой попытки, докачка, кэш и отказы.

Источник — локальный управляемый HTTP-сервер: сценарий задаёт ответ на каждый
запрос (полная передача, обрыв после части байт при полном Content-Length,
порча содержимого, HTTP-ошибка, зависание). Настоящие внешние архивы не нужны.
"""

import hashlib
import importlib.util
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("fetch_verified_tool.py")
SPEC = importlib.util.spec_from_file_location("fetch_verified_tool", SCRIPT)
fetch_verified_tool = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fetch_verified_tool)

BODY = bytes(range(256)) * 64  # 16 КиБ: несколько блоков на чтение не нужны, обрыв — да.
SHA = hashlib.sha256(BODY).hexdigest()


class ScriptedSource:
    """HTTP-источник, отвечающий на запросы по заранее заданному сценарию."""

    def __init__(self, behaviours, *, body=BODY, honour_range=True):
        self.behaviours = list(behaviours)
        self.body = body
        self.honour_range = honour_range
        self.requests = []
        source = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                source.requests.append(self.headers.get("Range"))
                behaviour = source.behaviours.pop(0) if source.behaviours else ("full",)
                source.respond(self, behaviour)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/tool.tar.gz"

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def respond(self, handler, behaviour):
        kind = behaviour[0]
        if kind == "status":
            handler.send_error(behaviour[1])
            return
        body = self.body
        if kind == "corrupt":
            body = bytes([body[0] ^ 0xFF]) + body[1:]
        start = 0
        requested = handler.headers.get("Range")
        if requested and self.honour_range:
            start = int(requested.removeprefix("bytes=").removesuffix("-"))
            handler.send_response(206)
            handler.send_header("Content-Range", f"bytes {start}-{len(body) - 1}/{len(body)}")
        else:
            handler.send_response(200)
        payload = body[start:]
        handler.send_header("Content-Length", str(len(payload)))
        handler.end_headers()
        if kind == "truncate":
            # Объявлено больше, чем передано: соединение закрывается без ошибки протокола.
            handler.wfile.write(payload[: behaviour[1]])
            handler.wfile.flush()
            handler.close_connection = True
            return
        if kind == "stall":
            time.sleep(behaviour[1])
            return
        handler.wfile.write(payload)


class FetchTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.cache = self.root / "tool-cache"
        self.sleeps = []
        for patcher in (mock.patch.dict(os.environ, {"CI_TOOL_CACHE": str(self.cache)}),
                        mock.patch.object(fetch_verified_tool, "log")):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.addCleanup(self.tmp.cleanup)

    def source(self, behaviours, **kwargs):
        source = ScriptedSource(behaviours, **kwargs)
        self.addCleanup(source.close)
        return source

    def fetch(self, url, *, sha=SHA, size=None, attempts=fetch_verified_tool.ATTEMPTS):
        return fetch_verified_tool.fetch(url, sha, "tool.tar.gz", size,
                                         attempts=attempts, sleep=self.sleeps.append)

    def entries(self):
        return sorted(p.name for p in self.cache.iterdir() if p.name != fetch_verified_tool.LOCK_NAME)


class RetryTests(FetchTestCase):
    def test_truncated_transfer_is_resumed_and_verified(self):
        source = self.source([("truncate", 1000), ("full",)])
        path = self.fetch(source.url)
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(source.requests, [None, "bytes=1000-"])
        self.assertEqual(len(self.sleeps), 1)
        self.assertEqual(self.entries(), ["tool.tar.gz"])

    def test_truncation_is_detected_against_pinned_size_without_range_support(self):
        source = self.source([("truncate", 10), ("truncate", 5000), ("full",)], honour_range=False)
        path = self.fetch(source.url, size=len(BODY))
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(len(source.requests), 3)

    def test_corrupted_transfer_is_discarded_and_downloaded_again(self):
        source = self.source([("corrupt",), ("corrupt",), ("full",)])
        path = self.fetch(source.url)
        self.assertEqual(path.read_bytes(), BODY)
        # Испорченный файл не докачивается, а скачивается заново целиком.
        self.assertEqual(source.requests, [None, None, None])

    def test_http_error_is_retried(self):
        source = self.source([("status", 503), ("full",)])
        self.assertEqual(self.fetch(source.url).read_bytes(), BODY)

    def test_stalled_transfer_times_out_and_is_retried(self):
        source = self.source([("stall", 2), ("full",)])
        with mock.patch.object(fetch_verified_tool, "TIMEOUT_SECONDS", 0.3):
            self.assertEqual(self.fetch(source.url).read_bytes(), BODY)

    def test_server_size_disagreeing_with_pinned_size_is_rejected(self):
        source = self.source([("full",)] * 2)
        with self.assertRaises(SystemExit):
            self.fetch(source.url, size=len(BODY) + 1, attempts=2)
        self.assertEqual(self.entries(), [])


class FailureTests(FetchTestCase):
    def test_retries_are_bounded_and_leave_no_files(self):
        source = self.source([("truncate", 100)] * 10)
        with self.assertRaises(SystemExit) as raised:
            self.fetch(source.url, attempts=3)
        self.assertIn("не удалось получить", str(raised.exception))
        self.assertEqual(len(source.requests), 3)
        self.assertEqual(len(self.sleeps), 2)
        self.assertEqual(self.entries(), [])

    def test_persistent_checksum_mismatch_fails_without_publishing(self):
        source = self.source([("full",)] * 3)
        with self.assertRaises(SystemExit) as raised:
            self.fetch(source.url, sha="0" * 64, attempts=3)
        self.assertIn("не совпадает", str(raised.exception))
        self.assertEqual(self.entries(), [])

    def test_repeated_corruption_exhausts_attempts(self):
        source = self.source([("corrupt",)] * 3)
        with self.assertRaises(SystemExit):
            self.fetch(source.url, attempts=3)
        self.assertEqual(self.entries(), [])


class CacheTests(FetchTestCase):
    def test_corrupted_cached_file_is_replaced(self):
        self.cache.mkdir()
        (self.cache / "tool.tar.gz").write_bytes(b"corrupted")
        source = self.source([("full",)])
        self.assertEqual(self.fetch(source.url).read_bytes(), BODY)
        self.assertEqual(self.entries(), ["tool.tar.gz"])

    def test_verified_cache_is_reused_without_request(self):
        source = self.source([("full",)])
        self.fetch(source.url)
        self.fetch(source.url)
        self.assertEqual(len(source.requests), 1)

    def test_partial_from_interrupted_run_is_resumed_not_trusted(self):
        self.cache.mkdir()
        partial = fetch_verified_tool.partial_path(self.cache, "tool.tar.gz", SHA)
        partial.write_bytes(BODY[:4000])
        source = self.source([("full",)])
        self.assertEqual(self.fetch(source.url).read_bytes(), BODY)
        self.assertEqual(source.requests, ["bytes=4000-"])

    def test_corrupted_partial_is_discarded_after_checksum(self):
        self.cache.mkdir()
        partial = fetch_verified_tool.partial_path(self.cache, "tool.tar.gz", SHA)
        partial.write_bytes(b"\0" * 4000)
        source = self.source([("full",), ("full",)])
        self.assertEqual(self.fetch(source.url).read_bytes(), BODY)
        self.assertEqual(source.requests, ["bytes=4000-", None])


class PruneTests(FetchTestCase):
    def test_stale_files_are_pruned_and_fresh_kept(self):
        self.cache.mkdir()
        old, fresh = self.cache / "old.tar.gz", self.cache / "fresh.tar.gz"
        old.write_bytes(b"x")
        fresh.write_bytes(b"y")
        now = time.time()
        os.utime(old, (now - 40 * 86400, now - 40 * 86400))
        removed = fetch_verified_tool.prune_stale(self.cache, 30 * 86400, now=now)
        self.assertEqual(removed, ["old.tar.gz"])
        self.assertEqual(self.entries(), ["fresh.tar.gz"])

    def test_cache_hit_refreshes_last_use(self):
        source = self.source([("full",)])
        path = self.fetch(source.url)
        os.utime(path, (0, 0))
        self.fetch(source.url)
        self.assertGreater(path.stat().st_mtime, time.time() - 60)

    def test_busy_cache_is_not_pruned(self):
        self.cache.mkdir()
        (self.cache / "old.tar.gz").write_bytes(b"x")
        os.utime(self.cache / "old.tar.gz", (0, 0))
        with fetch_verified_tool.directory_lock(self.cache):
            self.assertIsNone(fetch_verified_tool.prune_stale(self.cache, 1))
        self.assertEqual(self.entries(), ["old.tar.gz"])


class CommandLineTests(unittest.TestCase):
    """Контракт командной строки: одна строка stdout с путём, отказ при неверных аргументах."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.source = root / "source.tar.gz"
        self.source.write_bytes(BODY)
        self.runner_temp = root / "runner-temp"
        self.runner_temp.mkdir()

    def run_script(self, *extra, name="tool.tar.gz", cache=None):
        env = {k: v for k, v in os.environ.items() if k not in ("CI_TOOL_CACHE", "RUNNER_TEMP")}
        env["RUNNER_TEMP"] = str(self.runner_temp)
        if cache is not None:
            env["CI_TOOL_CACHE"] = str(cache)
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--url", self.source.as_uri(), "--sha256", SHA,
             "--name", name, *extra],
            env=env, capture_output=True, text=True, check=False,
        )

    def test_stdout_is_single_verified_path_in_runner_temp(self):
        result = self.run_script("--size", str(len(BODY)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [str(self.runner_temp / "tool.tar.gz")])
        self.assertEqual((self.runner_temp / "tool.tar.gz").read_bytes(), BODY)

    def test_tool_cache_takes_precedence_over_runner_temp(self):
        cache = Path(self.tmp.name) / "cache"
        result = self.run_script(cache=cache)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(cache / "tool.tar.gz"))

    def test_invalid_names_are_rejected(self):
        for name in ("../escape", ".hidden", ""):
            with self.subTest(name=name):
                self.assertNotEqual(self.run_script(name=name).returncode, 0)
        self.assertFalse((Path(self.tmp.name) / "escape").exists())


if __name__ == "__main__":
    unittest.main()
