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


class InterpreterTest(unittest.TestCase):
    def test_expected_interpreter_version(self):
        # CI задаёт целевую версию; локальный запуск без переменной её не требует.
        expected = os.environ.get("RESEMBLE_EXPECT_PYTHON")
        if expected:
            self.assertEqual(sys.version.split()[0], expected)


class SourceIdentityTest(unittest.TestCase):
    def test_worker_reports_sha_of_its_own_source(self):
        import hashlib

        info = worker.worker_info()
        self.assertEqual(info["source_path"], str(WORKER))
        self.assertEqual(
            info["source_sha256"], hashlib.sha256(WORKER.read_bytes()).hexdigest()
        )
        self.assertTrue(Path(info["source_path"]).is_absolute())


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

    def test_oversized_file_is_corrupt_without_full_read(self):
        # Разреженный файл 16 GiB: отказ по размеру приходит до чтения,
        # иначе тест упёрся бы в память.
        name = worker.FILES[0][0]
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "wb") as f:
            f.truncate(1 << 34)
        self.expect("model_corrupt")

    def test_symlink_is_not_followed(self):
        name = worker.FILES[0][0]
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        target = self.root / "elsewhere"
        target.write_bytes(b"x")
        path.symlink_to(target)
        self.expect("model_not_regular_file")

    def test_symlink_in_intermediate_directory_is_not_followed(self):
        # Настоящие файлы с верной длиной лежат за ссылкой на промежуточный
        # каталог: отказ приходит по ссылке, а не по содержимому.
        name = worker.FILES[0][0]
        outside = self.root.parent / (self.root.name + "-outside")
        real = outside / "default"
        real.mkdir(parents=True)
        self.addCleanup(lambda: __import__("shutil").rmtree(outside, ignore_errors=True))
        with open(real / Path(name).name, "wb") as f:
            f.truncate(worker.FILES[0][1])
        link_parent = (self.root / name).parent
        link_parent.parent.mkdir(parents=True)
        link_parent.symlink_to(real, target_is_directory=True)
        self.expect("model_not_regular_file")

    def test_symlinked_top_level_directory_is_not_followed(self):
        name = worker.FILES[1][0]
        outside = self.root.parent / (self.root.name + "-top")
        (outside).mkdir()
        self.addCleanup(lambda: __import__("shutil").rmtree(outside, ignore_errors=True))
        (self.root / Path(name).parts[0]).symlink_to(outside, target_is_directory=True)
        self.expect("model_not_regular_file")

    def test_fifo_is_refused_without_blocking(self):
        name = worker.FILES[1][0]
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        os.mkfifo(path)
        with self.assertRaises(worker.WorkerError) as caught:
            worker.read_regular(os.open(self.root, os.O_RDONLY | os.O_DIRECTORY), name, 717)
        self.assertEqual(caught.exception.code, "model_not_regular_file")

    def test_install_refuses_source_with_intermediate_symlink(self):
        source = self.root / "src"
        source.mkdir()
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir()
        (source / "enhancer_stage2").symlink_to(elsewhere, target_is_directory=True)
        models = self.root / "models"
        code, report = run("install-model", "--source", str(source), "--models-root", str(models))
        self.assertEqual(code, worker.EXIT_MODEL)
        self.assertEqual(report["code"], "model_not_regular_file")
        self.assertFalse((models / worker.BACKEND).exists())

    def test_write_tree_creates_only_new_regular_files(self):
        staging = self.root / "staging"
        staging.mkdir()
        files = {name: b"x" * 3 for name, _, _ in worker.FILES}
        worker.write_tree(staging, files)
        for name in files:
            path = staging / name
            self.assertTrue(path.is_file() and not path.is_symlink())
            self.assertEqual(path.read_bytes(), b"xxx")
        # Повторная запись в тот же каталог не перезаписывает файлы.
        with self.assertRaises(FileExistsError):
            worker.write_tree(staging, files)

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


def torch_available() -> bool:
    try:
        import torch  # noqa: F401
    except ImportError:
        if os.environ.get("RESEMBLE_REQUIRE_TORCH"):
            raise
        return False
    return True


@unittest.skipUnless(torch_available(), "нет torch: проверка настоящего torch.load пропущена")
class TorchLoadTest(unittest.TestCase):
    """Настоящий `torch.load` идёт через ограниченный unpickler воркера."""

    def save(self, obj) -> bytes:
        import torch

        buf = io.BytesIO()
        torch.save(obj, buf)
        return buf.getvalue()

    def load(self, data: bytes):
        import torch

        return torch.load(
            io.BytesIO(data),
            map_location="cpu",
            pickle_module=worker.RestrictedPickle,
            weights_only=False,
        )

    def test_half_tensors_and_sets_load(self):
        import torch

        value = {"module": collections.OrderedDict(w=torch.ones(3, dtype=torch.half)), "s": {1}}
        loaded = self.load(self.save(value))
        self.assertTrue(torch.equal(loaded["module"]["w"], value["module"]["w"]))

    def test_code_execution_payload_is_refused(self):
        with self.assertRaises(pickle.UnpicklingError):
            self.load(self.save({"x": Evil()}))


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
