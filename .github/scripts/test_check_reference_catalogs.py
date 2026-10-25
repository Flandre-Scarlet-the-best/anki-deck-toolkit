"""Тесты разбора атрибуции, повторов загрузки и безопасной распаковки для проверки эталонов кандзи."""

import http.client
import importlib.util
import io
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("check_reference_catalogs.py")
SPEC = importlib.util.spec_from_file_location("check_reference_catalogs", SCRIPT)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

REPO_ROOT = SCRIPT.parents[2]
ATTRIBUTION = """\
Source archive:
  https://example.invalid/kanji.zip
Source archive SHA-256:
  {source}

Generated file: catalog.maskdb.zlib
Generated file SHA-256:
  {generated}
"""


class AttributionTests(unittest.TestCase):
    def test_parses_pinned_values(self):
        pins = checker.parse_attribution(ATTRIBUTION.format(source="a" * 64, generated="b" * 64))
        self.assertEqual(pins.source_url, "https://example.invalid/kanji.zip")
        self.assertEqual(pins.source_sha256, "a" * 64)
        self.assertEqual(pins.generated_file, "catalog.maskdb.zlib")
        self.assertEqual(pins.generated_sha256, "b" * 64)

    def test_ambiguous_or_missing_value_is_rejected(self):
        text = ATTRIBUTION.format(source="a" * 64, generated="b" * 64)
        with self.assertRaises(checker.CatalogError):
            checker.parse_attribution(text + "\nGenerated file: other.zlib\n")
        with self.assertRaises(checker.CatalogError):
            checker.parse_attribution(text.replace("a" * 64, "short"))

    def test_tracked_attribution_files_are_parseable(self):
        # Атрибуция — единственный источник закреплённых значений проверки.
        for catalog in checker.CATALOGS:
            text = (REPO_ROOT / checker.DATA_DIR / catalog.attribution).read_text(encoding="utf-8")
            pins = checker.parse_attribution(text)
            self.assertTrue((REPO_ROOT / checker.DATA_DIR / pins.generated_file).is_file())


class ExtractionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def archive(self, members):
        path = self.root / "source.zip"
        with zipfile.ZipFile(path, "w") as bundle:
            for name, data in members.items():
                bundle.writestr(name, data)
        return path

    def test_extracts_only_flat_kanji_svg_members(self):
        archive = self.archive(
            {
                "kanji/04e00.svg": "<svg/>",
                "kanji/04e00-Kaisho.svg": "<svg/>",
                "kanji/../escape.svg": "<svg/>",
                "README.md": "text",
            }
        )
        target = self.root / "out"
        target.mkdir()
        self.assertEqual(checker.extract_kanjivg(archive, target), 2)
        self.assertEqual(sorted(p.name for p in target.iterdir()), ["04e00-Kaisho.svg", "04e00.svg"])
        self.assertFalse((self.root / "escape.svg").exists())

    def test_archive_without_kanji_is_rejected(self):
        target = self.root / "out"
        target.mkdir()
        with self.assertRaises(checker.CatalogError):
            checker.extract_kanjivg(self.archive({"other/04e00.svg": "<svg/>"}), target)


class DownloadTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.target = Path(temp.name) / "source.zip"
        sleep = mock.patch.object(checker.time, "sleep")
        sleep.start()
        self.addCleanup(sleep.stop)

    @staticmethod
    def truncated():
        response = mock.MagicMock()
        response.__enter__.return_value.read.side_effect = http.client.IncompleteRead(b"partial")
        return response

    @staticmethod
    def http_response(body, declared_length):
        """Настоящий `HTTPResponse`: сервер объявляет длину и закрывает поток после `body`."""
        raw = b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n" % declared_length + body

        class Socket:
            def makefile(self, mode):
                return io.BytesIO(raw)

        response = http.client.HTTPResponse(Socket())
        response.begin()
        return response

    def test_silent_premature_eof_is_retried(self):
        truncated = self.http_response(b"pay", 7)
        self.assertEqual(truncated.read(1 << 20), b"pay")
        self.assertEqual(truncated.read(1 << 20), b"")
        responses = [self.http_response(b"pay", 7), self.http_response(b"payload", 7)]
        with mock.patch.object(checker.urllib.request, "urlopen", side_effect=responses) as urlopen:
            checker.download("https://example.invalid/source.zip", self.target)
        self.assertEqual(urlopen.call_count, 2)
        self.assertEqual(self.target.read_bytes(), b"payload")

    def test_persistent_silent_premature_eof_is_catalog_error(self):
        responses = [self.http_response(b"pay", 7) for _ in range(checker.DOWNLOAD_ATTEMPTS)]
        with mock.patch.object(checker.urllib.request, "urlopen", side_effect=responses) as urlopen:
            with self.assertRaises(checker.CatalogError):
                checker.download("https://example.invalid/source.zip", self.target)
        self.assertEqual(urlopen.call_count, checker.DOWNLOAD_ATTEMPTS)

    def test_truncated_response_is_retried(self):
        with mock.patch.object(
            checker.urllib.request, "urlopen", side_effect=[self.truncated(), io.BytesIO(b"payload")]
        ):
            checker.download("https://example.invalid/source.zip", self.target)
        self.assertEqual(self.target.read_bytes(), b"payload")

    def test_persistently_truncated_response_is_catalog_error(self):
        responses = [self.truncated() for _ in range(checker.DOWNLOAD_ATTEMPTS)]
        with mock.patch.object(checker.urllib.request, "urlopen", side_effect=responses):
            with self.assertRaises(checker.CatalogError):
                checker.download("https://example.invalid/source.zip", self.target)


if __name__ == "__main__":
    unittest.main()
