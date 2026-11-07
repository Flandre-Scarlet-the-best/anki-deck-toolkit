#!/usr/bin/env python3
"""Изолированный воркер Resemble Enhance (только denoiser) по протоколу
`speech-enhance-dfn`.

Экспериментальный опциональный backend: один процесс — один клип, веса
проверяются по закреплённому манифесту до загрузки, сеть не используется.
Генеративная часть Resemble Enhance (LCFM + вокодер) намеренно не
поддерживается: она синтезирует речь заново и способна менять фонемы.

Контракт протокола и границы — `docs/ru/asset-store/enhancer.md`.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import pickle
import shutil
import stat
import sys
import tempfile
from pathlib import Path

PROTOCOL_VERSION = 1
WORKER_NAME = "speech-enhance-resemble"
WORKER_VERSION = "1"
BACKEND = "resemble-enhance-denoise"
MODEL_NAME = "Resemble Enhance"
MODEL_VARIANT = "denoiser"
SAMPLE_RATE = 48_000
MODEL_RATE = 44_100
MAX_CHANNELS = 2
MAX_INPUT_BYTES = 64 * 1024 * 1024
# Целевая среда — CPython 3.15 с PyTorch 2.15 (requirements.txt). Модель не
# загружается в более старой среде: прежнее окружение Python 3.11 не
# остаётся скрытым запасным вариантом.
MIN_PYTHON = (3, 15)
SOURCE = {
    "repository": "https://huggingface.co/ResembleAI/resemble-enhance",
    "revision": "4e3510ce4a8391159f665903544c5150bee7b2cb",
}
LICENSE = "MIT"
# Закреплённые файлы весов: путь относительно каталога модели, длина, SHA-256.
FILES = [
    (
        "enhancer_stage2/ds/G/default/mp_rank_00_model_states.pt",
        713176232,
        "f9d035f318de3e6d919bc70cf7ad7d32b4fe92ec5cbe0b30029a27f5db07d9d6",
    ),
    (
        "enhancer_stage2/hparams.yaml",
        717,
        "80c3f15bc5a5b2cacf2c698699a0f6599d62911c0d53e1d6dee895c0d7cbaeac",
    ),
]

# Исходник воркера читается один раз при запуске: его SHA-256 сообщают и
# `identify`, и каждый `process`, а `asset-store` сверяет их, потому что
# запускатель окружения не меняется, когда меняется исполняемый им код.
SOURCE_PATH = Path(__file__).resolve()
SOURCE_SHA256 = hashlib.sha256(SOURCE_PATH.read_bytes()).hexdigest()

EXIT_MODEL = 3
EXIT_INPUT = 4
EXIT_RUNTIME = 5


class WorkerError(Exception):
    def __init__(self, code: str, message: str, exit_code: int):
        super().__init__(message)
        self.code = code
        self.exit_code = exit_code


def fail(code: str, message: str, exit_code: int):
    raise WorkerError(code, message, exit_code)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def manifest_sha256() -> str:
    text = "".join(f"{n}\t{l}\t{d}\n" for n, l, d in FILES)
    return sha256_bytes(text.encode())


DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
FILE_FLAGS = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK


def open_root(model_dir: Path) -> int:
    """Каталог весов — корень доверенного дерева: его открывают как каталог
    (путь до него выбирает пользователь), а всё ниже — без ссылок."""
    try:
        return os.open(model_dir, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    except FileNotFoundError:
        fail("model_missing", "каталог весов не найден", EXIT_MODEL)
    except OSError as e:
        fail("model_missing", f"каталог весов не открывается: {e.strerror}", EXIT_MODEL)


def read_regular(root_fd: int, name: str, length: int) -> bytes:
    """Читает обычный файл закреплённой длины по относительному пути внутри
    каталога весов. Каждый промежуточный каталог открывается относительно
    предыдущего с `O_DIRECTORY | O_NOFOLLOW`, сам файл — с `O_NOFOLLOW`:
    символическая ссылка ни в одном компоненте не уводит за дерево весов, и
    проверка не разделена с открытием (нет окна TOCTOU). Файл другой длины
    отвергается до чтения, а чтение ограничено длиной + 1 байт: подменённый
    большой файл не загружается в память."""
    *dirs, leaf = name.split("/")
    fds = []
    try:
        current = root_fd
        for part in dirs:
            try:
                current = os.open(part, DIR_FLAGS, dir_fd=current)
            except FileNotFoundError:
                fail("model_missing", f"нет файла весов {leaf}", EXIT_MODEL)
            except OSError as e:
                fail(
                    "model_not_regular_file",
                    f"{part}: каталог весов недоступен без ссылок ({e.strerror})",
                    EXIT_MODEL,
                )
            fds.append(current)
        try:
            fd = os.open(leaf, FILE_FLAGS, dir_fd=current)
        except FileNotFoundError:
            fail("model_missing", f"нет файла весов {leaf}", EXIT_MODEL)
        except OSError as e:
            fail("model_not_regular_file", f"{leaf}: {e.strerror}", EXIT_MODEL)
    finally:
        for opened in fds:
            os.close(opened)
    with os.fdopen(fd, "rb") as f:
        info = os.fstat(f.fileno())
        if not stat.S_ISREG(info.st_mode):
            fail("model_not_regular_file", f"{leaf} не обычный файл", EXIT_MODEL)
        if info.st_size != length:
            fail(
                "model_corrupt",
                f"{leaf}: длина не совпала с закреплённым манифестом",
                EXIT_MODEL,
            )
        return f.read(length + 1)


def load_files(model_dir: Path) -> dict[str, bytes]:
    root_fd = open_root(model_dir)
    try:
        out = {}
        for name, length, digest in FILES:
            data = read_regular(root_fd, name, length)
            if len(data) != length or sha256_bytes(data) != digest:
                fail(
                    "model_corrupt",
                    f"{name}: длина или SHA-256 не совпали с закреплённым манифестом",
                    EXIT_MODEL,
                )
            out[name] = data
        return out
    finally:
        os.close(root_fd)


# Ограниченный unpickler: чекпойнт ссылается ровно на эти глобалы.
ALLOWED_GLOBALS = {
    ("collections", "OrderedDict"),
    ("__builtin__", "set"),
    ("builtins", "set"),
    ("torch", "HalfStorage"),
    ("torch._utils", "_rebuild_tensor_v2"),
}


class RestrictedUnpickler(pickle.Unpickler):
    def find_class(self, module, name):
        if (module, name) not in ALLOWED_GLOBALS:
            raise pickle.UnpicklingError(f"запрещённый глобал {module}.{name}")
        return super().find_class(module, name)


class RestrictedPickle:
    Unpickler = RestrictedUnpickler

    @staticmethod
    def load(file, **kwargs):
        return RestrictedUnpickler(file, **kwargs).load()


def parse_hparams(data: bytes):
    import yaml

    class Loader(yaml.SafeLoader):
        pass

    def path_apply(loader, suffix, node):
        if suffix != "pathlib.PosixPath":
            raise yaml.constructor.ConstructorError(
                None, None, f"запрещённый тег python/object/apply:{suffix}"
            )
        return Path(*loader.construct_sequence(node))

    Loader.add_multi_constructor("tag:yaml.org,2002:python/object/apply:", path_apply)
    values = yaml.load(io.BytesIO(data), Loader=Loader)
    from resemble_enhance.enhancer.hparams import HParams

    return HParams(**values)


def device_name() -> str:
    import torch

    forced = os.environ.get("SPEECH_ENHANCE_RESEMBLE_DEVICE")
    if forced in ("cpu", "cuda"):
        if forced == "cuda" and not torch.cuda.is_available():
            fail("runtime_device_unavailable", "CUDA недоступна", EXIT_RUNTIME)
        return forced
    return "cuda" if torch.cuda.is_available() else "cpu"


def load_model(model_dir: Path):
    if sys.version_info[:2] < MIN_PYTHON:
        fail(
            "runtime_python_unsupported",
            f"нужен Python {MIN_PYTHON[0]}.{MIN_PYTHON[1]}+, запущен {sys.version.split()[0]}",
            EXIT_RUNTIME,
        )
    import torch

    torch.manual_seed(0)
    torch.backends.cudnn.deterministic = True
    torch.backends.cudnn.benchmark = False
    files = load_files(model_dir)
    hp = parse_hparams(files["enhancer_stage2/hparams.yaml"])
    from resemble_enhance.denoiser.denoiser import Denoiser
    from resemble_enhance.inference import remove_weight_norm_recursively

    try:
        # Чекпойнт DeepSpeed ссылается на `set` и `HalfStorage`. Разбор идёт
        # собственным ограниченным unpickler с явным списком глобалов (уже
        # списка `weights_only` torch); `weights_only=False` указан явно,
        # потому что `weights_only=True` несовместим с `pickle_module`.
        # Байты к этому моменту сверены по SHA-256 манифеста.
        state = torch.load(
            io.BytesIO(files[FILES[0][0]]),
            map_location="cpu",
            pickle_module=RestrictedPickle,
            weights_only=False,
        )["module"]
    except Exception as e:  # noqa: BLE001 — любой отказ разбора = порча весов
        fail("model_corrupt", f"чекпойнт не разбирается: {e}", EXIT_MODEL)
    prefix = "denoiser."
    denoiser_state = {k[len(prefix) :]: v for k, v in state.items() if k.startswith(prefix)}
    model = Denoiser(hp)
    try:
        model.load_state_dict(denoiser_state, strict=True)
    except RuntimeError as e:
        fail("model_wrong_version", f"веса не подходят к Denoiser: {e}", EXIT_MODEL)
    model.eval()
    remove_weight_norm_recursively(model)
    device = device_name()
    return model.to(device).float(), hp, device


def runtime_info(device: str) -> dict:
    import numpy
    import torch
    import torchaudio
    from importlib.metadata import version

    return {
        "library": "resemble-enhance",
        "library_version": version("resemble-enhance"),
        "torch": torch.__version__,
        "torchaudio": torchaudio.__version__,
        "numpy": numpy.__version__,
        "python": sys.version.split()[0],
        "device": device,
        "cuda": torch.version.cuda if device == "cuda" else None,
        "gpu": torch.cuda.get_device_name(0) if device == "cuda" else None,
    }


def worker_info() -> dict:
    return {
        "name": WORKER_NAME,
        "version": WORKER_VERSION,
        "protocol": PROTOCOL_VERSION,
        "source_sha256": SOURCE_SHA256,
        "source_path": str(SOURCE_PATH),
    }


def identity(model_dir: Path) -> dict:
    _, _, device = load_model(model_dir)
    return {
        "schema_version": PROTOCOL_VERSION,
        "kind": "speech_enhance_identity",
        "backend": BACKEND,
        "worker": worker_info(),
        "runtime": runtime_info(device),
        "model": {
            "name": MODEL_NAME,
            "variant": MODEL_VARIANT,
            "manifest_sha256": manifest_sha256(),
            "files": [
                {"name": n, "byte_length": l, "sha256": d} for n, l, d in FILES
            ],
            "source": SOURCE,
            "license": LICENSE,
            "sample_rate": MODEL_RATE,
        },
        "stream": {
            "sample_rate": SAMPLE_RATE,
            "hop_samples": 1,
            "delay_samples": 0,
            "max_channels": MAX_CHANNELS,
        },
    }


def denoise_channel(model, hp, device, x):
    import torch
    from torchaudio.functional import resample

    def rs(w, a, b):
        return resample(
            w,
            orig_freq=a,
            new_freq=b,
            lowpass_filter_width=64,
            rolloff=0.9475937167399596,
            resampling_method="sinc_interp_kaiser",
            beta=14.769656459379492,
        )

    n = x.shape[-1]
    w = rs(torch.from_numpy(x.copy()), SAMPLE_RATE, MODEL_RATE)
    length = w.shape[-1]
    peak = w.abs().max().clamp(min=1e-7)
    with torch.inference_mode():
        y = model((torch.nn.functional.pad(w / peak, (0, 441)))[None].to(device))[0]
    y = (y.cpu().float()[:length] * peak)
    y = rs(y, MODEL_RATE, SAMPLE_RATE)
    # Двойной ресемплинг может дать ±1 отсчёт; больше — нарушение длины.
    if abs(y.shape[-1] - n) > 2:
        fail("runtime_length_mismatch", "длина после ресемплинга не совпала", EXIT_RUNTIME)
    if y.shape[-1] >= n:
        y = y[:n]
    else:
        y = torch.nn.functional.pad(y, (0, n - y.shape[-1]))
    return y.numpy()


def process(args) -> dict:
    channels = args.channels
    if channels < 1 or channels > MAX_CHANNELS:
        fail("input_channels", "поддерживается 1–2 канала", EXIT_INPUT)
    raw = read_input(Path(args.input))
    if len(raw) == 0 or len(raw) % (4 * channels):
        fail("input_length", "длина входа не кратна кадру f32", EXIT_INPUT)
    import numpy as np

    x = np.frombuffer(raw, dtype="<f4").reshape(-1, channels)
    if not np.isfinite(x).all():
        fail("input_not_finite", "во входе неконечные отсчёты", EXIT_INPUT)
    model, hp, device = load_model(Path(args.model_dir))
    identical = channels == 2 and np.array_equal(x[:, 0], x[:, 1])
    outs = []
    for c in range(channels):
        if identical and c == 1:
            outs.append(outs[0])
            continue
        outs.append(denoise_channel(model, hp, device, x[:, c]))
    y = np.stack(outs, 1).astype("<f4")
    if not np.isfinite(y).all():
        fail("runtime_not_finite", "модель выдала неконечные отсчёты", EXIT_RUNTIME)
    data = y.tobytes()
    out = Path(args.output)
    with open(out, "xb") as f:
        f.write(data)
    frames = x.shape[0]
    return {
        "schema_version": PROTOCOL_VERSION,
        "kind": "speech_enhance_result",
        "model_manifest_sha256": manifest_sha256(),
        "worker": worker_info(),
        "runtime": runtime_info(device),
        "params": {},
        "diagnostics": {
            "channels": channels,
            "input_frames": frames,
            "output_frames": frames,
            "delay_samples": 0,
            "flush_samples": 0,
            "identical_channels": bool(identical),
            "device": device,
            "per_channel": [{} for _ in range(channels)],
        },
        "output_sha256": sha256_bytes(data),
    }


def read_input(path: Path) -> bytes:
    try:
        size = path.stat().st_size
    except OSError as e:
        fail("input_unreadable", f"вход не читается: {e.strerror}", EXIT_INPUT)
    if size > MAX_INPUT_BYTES:
        fail("input_too_large", "вход больше предела", EXIT_INPUT)
    return path.read_bytes()


def write_tree(staging: Path, files: dict[str, bytes]) -> None:
    """Пишет файлы весов в свежий каталог staging относительно его
    дескриптора: каталоги создаются и открываются без перехода по ссылкам,
    файлы — только новые (`O_EXCL`)."""
    root_fd = os.open(staging, DIR_FLAGS)
    try:
        for name, data in files.items():
            *dirs, leaf = name.split("/")
            fds = []
            try:
                current = root_fd
                for part in dirs:
                    try:
                        os.mkdir(part, 0o755, dir_fd=current)
                    except FileExistsError:
                        pass
                    current = os.open(part, DIR_FLAGS, dir_fd=current)
                    fds.append(current)
                fd = os.open(
                    leaf,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                    0o644,
                    dir_fd=current,
                )
                with os.fdopen(fd, "wb") as f:
                    f.write(data)
                    f.flush()
                    os.fsync(f.fileno())
                os.fsync(current)
            finally:
                for opened in fds:
                    os.close(opened)
        os.fsync(root_fd)
    finally:
        os.close(root_fd)


def install(args) -> dict:
    source = Path(args.source)
    root = Path(args.models_root)
    target = root / BACKEND / manifest_sha256()
    files = load_files(source)
    if target.exists():
        load_files(target)
        return {"schema_version": PROTOCOL_VERSION, "kind": "speech_enhance_install",
                "installed": False, "model_dir": str(target)}
    target.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".staging-", dir=target.parent))
    try:
        write_tree(staging, files)
        os.rename(staging, target)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    # Установленное дерево проверяется той же границей, что и обработка.
    load_files(target)
    return {"schema_version": PROTOCOL_VERSION, "kind": "speech_enhance_install",
            "installed": True, "model_dir": str(target)}


def main() -> int:
    parser = argparse.ArgumentParser(prog=WORKER_NAME)
    sub = parser.add_subparsers(dest="command", required=True)
    ident = sub.add_parser("identify")
    ident.add_argument("--model-dir", required=True)
    proc = sub.add_parser("process")
    proc.add_argument("--model-dir", required=True)
    proc.add_argument("--input", required=True)
    proc.add_argument("--output", required=True)
    proc.add_argument("--channels", type=int, required=True)
    inst = sub.add_parser("install-model")
    inst.add_argument("--source", required=True)
    inst.add_argument("--models-root", required=True)
    args = parser.parse_args()
    try:
        if args.command == "identify":
            report = identity(Path(args.model_dir))
        elif args.command == "process":
            report = process(args)
        else:
            report = install(args)
    except WorkerError as e:
        print(json.dumps({"schema_version": PROTOCOL_VERSION, "kind": "speech_enhance_error",
                          "code": e.code, "message": str(e)}, ensure_ascii=False))
        return e.exit_code
    except Exception as e:  # noqa: BLE001 — неожиданный сбой среды
        print(json.dumps({"schema_version": PROTOCOL_VERSION, "kind": "speech_enhance_error",
                          "code": "runtime_failure", "message": f"{type(e).__name__}: {e}"},
                         ensure_ascii=False))
        return EXIT_RUNTIME
    print(json.dumps(report, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
