"""Проверки воркера Resemble Enhance без PyTorch и весов: граница доверия
к файлам модели, ограниченный unpickler и отказы протокола до загрузки
модели. Запускаются из `cargo test` (`tools/speech-enhance/tests/resemble.rs`).
"""

import collections
import io
import json
import os
import pickle
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import speech_enhance_resemble as worker  # noqa: E402

WORKER = HERE / "speech_enhance_resemble.py"


def run(*args):
    done = subprocess.run(
        [sys.executable, "-I", str(WORKER), *args], capture_output=True, text=True
    )
    return done.returncode, json.loads(done.stdout)


class Evil:
    def __reduce__(self):
        return (os.system, ("true",))


class ManifestTest(unittest.TestCase):
    def test_manifest_matches_pinned_value_of_rust_side(self):
        expected = os.environ.get("RESEMBLE_DENOISE_MANIFEST_SHA256")
        self.assertEqual(len(worker.manifest_sha256()), 64)
        if expected:
            self.assertEqual(worker.manifest_sha256(), expected)

    def test_every_pinned_file_has_length_and_digest(self):
        for name, length, digest in worker.FILES:
            self.assertGreater(length, 0, name)
            self.assertEqual(len(digest), 64, name)


class UnpicklerTest(unittest.TestCase):
    def load(self, obj):
        return worker.RestrictedPickle.load(io.BytesIO(pickle.dumps(obj, protocol=2)))

    def test_allowed_containers_load(self):
        value = collections.OrderedDict(a={1, 2})
        self.assertEqual(self.load(value), value)

    def test_code_execution_payload_is_refused(self):
        with self.assertRaises(pickle.UnpicklingError):
            self.load(Evil())

    def test_arbitrary_global_is_refused(self):
        with self.assertRaises(pickle.UnpicklingError):
            self.load(Path("x"))


class ModelFilesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def expect(self, code):
        with self.assertRaises(worker.WorkerError) as caught:
            worker.load_files(self.root)
        self.assertEqual(caught.exception.code, code)
        self.assertEqual(caught.exception.exit_code, worker.EXIT_MODEL)

    def test_missing_directory_and_missing_file(self):
        with self.assertRaises(worker.WorkerError) as caught:
            worker.load_files(self.root / "absent")
        self.assertEqual(caught.exception.code, "model_missing")
        self.expect("model_missing")

    def test_wrong_bytes_are_corrupt(self):
        for name, _, _ in worker.FILES:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"not the pinned weights")
        self.expect("model_corrupt")

    def test_symlink_is_not_followed(self):
        name = worker.FILES[0][0]
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        target = self.root / "elsewhere"
        target.write_bytes(b"x")
        path.symlink_to(target)
        self.expect("model_not_regular_file")

    def test_install_from_corrupt_source_refuses_without_target(self):
        source = self.root / "src"
        for name, _, _ in worker.FILES:
            path = source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"bad")
        models = self.root / "models"
        code, report = run("install-model", "--source", str(source), "--models-root", str(models))
        self.assertEqual(code, worker.EXIT_MODEL)
        self.assertEqual(report["code"], "model_corrupt")
        self.assertFalse((models / worker.BACKEND).exists())


class ProtocolTest(unittest.TestCase):
    def test_bad_channel_count_is_input_error_before_model_load(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, report = run(
                "process",
                "--model-dir", str(Path(tmp) / "absent"),
                "--input", str(Path(tmp) / "in.f32"),
                "--output", str(Path(tmp) / "out.f32"),
                "--channels", "3",
            )
        self.assertEqual(code, worker.EXIT_INPUT)
        self.assertEqual(report["kind"], "speech_enhance_error")
        self.assertEqual(report["code"], "input_channels")

    def test_unaligned_input_is_input_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "in.f32"
            src.write_bytes(b"\x00" * 6)
            code, report = run(
                "process",
                "--model-dir", tmp,
                "--input", str(src),
                "--output", str(Path(tmp) / "out.f32"),
                "--channels", "1",
            )
        self.assertEqual(code, worker.EXIT_INPUT)
        self.assertEqual(report["code"], "input_length")


if __name__ == "__main__":
    unittest.main()
