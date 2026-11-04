#!/usr/bin/env python3
"""Загрузка закреплённого архива инструмента CI с проверкой SHA-256 (только stdlib).

Каталог хранения — ``CI_TOOL_CACHE``, если он задан (постоянное окружение
self-hosted раннера), иначе ``RUNNER_TEMP`` (GitHub-hosted: каталог задания).
Уже лежащий там файл используется только при совпадении SHA-256; иначе архив
скачивается заново во временный файл того же каталога, проверяется и лишь
затем атомарно занимает место назначения. Непроверенный файл под итоговым
именем не остаётся. Единственная строка stdout — путь к проверенному архиву.
"""

import argparse
import hashlib
import os
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ATTEMPTS = 3
TIMEOUT_SECONDS = 120
CHUNK = 1 << 20


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(CHUNK):
            digest.update(block)
    return digest.hexdigest()


def storage_dir() -> Path:
    for variable in ("CI_TOOL_CACHE", "RUNNER_TEMP"):
        value = os.environ.get(variable)
        if value:
            return Path(value)
    raise SystemExit("fetch_verified_tool: не заданы ни CI_TOOL_CACHE, ни RUNNER_TEMP")


def download(url: str, destination: Path) -> None:
    last_error = None
    for attempt in range(1, ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(url, timeout=TIMEOUT_SECONDS) as response:
                with destination.open("wb") as stream:
                    while block := response.read(CHUNK):
                        stream.write(block)
            return
        except OSError as error:
            last_error = error
            print(f"fetch_verified_tool: попытка {attempt} не удалась: {error}", file=sys.stderr)
            if attempt < ATTEMPTS:
                time.sleep(2 * attempt)
    raise SystemExit(f"fetch_verified_tool: не удалось скачать {url}: {last_error}")


def fetch(url: str, expected: str, name: str) -> Path:
    expected = expected.lower()
    directory = storage_dir()
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / name
    if target.is_file():
        if sha256_of(target) == expected:
            print(f"fetch_verified_tool: {name} взят из {directory}, SHA-256 совпал", file=sys.stderr)
            return target
        print(f"fetch_verified_tool: SHA-256 {target} не совпал, скачиваю заново", file=sys.stderr)
    handle, temporary = tempfile.mkstemp(prefix=f".{name}.", suffix=".part", dir=directory)
    os.close(handle)
    partial = Path(temporary)
    try:
        download(url, partial)
        actual = sha256_of(partial)
        if actual != expected:
            raise SystemExit(
                f"fetch_verified_tool: SHA-256 {name} {actual} не совпадает с ожидаемым {expected}"
            )
        os.replace(partial, target)
    finally:
        partial.unlink(missing_ok=True)
    return target


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--name", required=True, help="имя файла в каталоге хранения")
    args = parser.parse_args()
    if "/" in args.name or args.name in ("", ".", ".."):
        raise SystemExit("fetch_verified_tool: --name должно быть именем файла без каталогов")
    print(fetch(args.url, args.sha256, args.name))
    return 0


if __name__ == "__main__":
    sys.exit(main())
