#!/usr/bin/env python3
"""Загрузка закреплённого архива инструмента CI с проверкой SHA-256 (только стандартная библиотека).

Каталог хранения — ``CI_TOOL_CACHE``, если он задан (постоянное окружение
self-hosted раннера), иначе ``RUNNER_TEMP`` (GitHub-hosted: каталог задания).
Уже лежащий там файл используется только при совпадении SHA-256 (и размера,
если он закреплён); иначе архив скачивается заново.

Единица повторяемой операции — попытка, завершённая полной проверкой
полученного файла: число байт против ``Content-Length``/``Content-Range``
сервера и закреплённого ``--size``, затем SHA-256. Оборванная передача
(в том числе преждевременный EOF, который ``http.client`` не считает ошибкой)
и сетевые сбои повторяются ограниченное число раз; следующая попытка
докачивает неполный файл запросом ``Range``, а файл с неверной суммой
отбрасывается целиком. Неполные данные лежат только под скрытым именем
``.<имя>.<sha>.part`` и валидным кэшем не считаются: под итоговым именем
файл появляется атомарно и только после проверки. После исчерпания попыток —
явный отказ, неполный файл удаляется. Если шаг прерван по тайм-ауту, неполный
файл остаётся и следующий запуск докачивает его (с той же итоговой проверкой).

Параллельные загрузки в один каталог сериализуются блокировкой ``.fetch.lock``;
её же берёт обслуживание кэша (``prune_stale``), чтобы не удалить файл,
который сейчас скачивается или используется.

Единственная строка stdout — путь к проверенному архиву.
"""

import argparse
import fcntl
import hashlib
import http.client
import os
import re
import sys
import time
import urllib.error
import urllib.request
from contextlib import contextmanager
from pathlib import Path

ATTEMPTS = 5
BACKOFF_SECONDS = (2, 5, 10, 20)
# Тайм-аут одной сетевой операции (соединение, чтение блока), а не всей передачи:
# полный бюджет загрузки задаёт `timeout-minutes` шага.
TIMEOUT_SECONDS = 60
CHUNK = 1 << 20
LOCK_NAME = ".fetch.lock"
PART_SUFFIX = ".part"


class TransferError(Exception):
    """Попытка завершилась, но полученный файл не прошёл проверку."""


def log(message: str) -> None:
    print(f"fetch_verified_tool: {message}", file=sys.stderr)


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


@contextmanager
def directory_lock(directory: Path, *, blocking: bool = True):
    """Исключительная блокировка каталога хранения; без ожидания — ``None``, если занят."""
    with (directory / LOCK_NAME).open("a") as handle:
        flags = fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB)
        try:
            fcntl.flock(handle, flags)
        except BlockingIOError:
            yield None
            return
        try:
            yield handle
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


def partial_path(directory: Path, name: str, expected: str) -> Path:
    return directory / f".{name}.{expected[:16]}{PART_SUFFIX}"


def is_verified(path: Path, expected: str, size: int | None) -> bool:
    if not path.is_file():
        return False
    if size is not None and path.stat().st_size != size:
        return False
    return sha256_of(path) == expected


def response_total(response, offset: int) -> int | None:
    """Полный размер файла по заголовкам ответа или ``None``, если сервер его не сообщил."""
    if response.status == 206:
        match = re.fullmatch(r"bytes (\d+)-\d+/(\d+|\*)", response.headers.get("Content-Range", ""))
        if not match or int(match.group(1)) != offset:
            raise TransferError(f"сервер вернул неожиданный диапазон: {response.headers.get('Content-Range')!r}")
        return None if match.group(2) == "*" else int(match.group(2))
    length = response.headers.get("Content-Length")
    return int(length) if length and length.isdigit() else None


def transfer(url: str, partial: Path, size: int | None) -> None:
    """Одна передача: докачивает ``partial`` и проверяет число полученных байт."""
    offset = partial.stat().st_size if partial.exists() else 0
    if size is not None and offset > size:
        partial.unlink()
        offset = 0
    if size is not None and offset == size:
        return
    request = urllib.request.Request(url)
    if offset:
        request.add_header("Range", f"bytes={offset}-")
    try:
        response = urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS)
    except urllib.error.HTTPError as error:
        if error.code == 416:
            partial.unlink(missing_ok=True)
            raise TransferError("сервер отверг докачку (416), неполный файл отброшен") from error
        raise
    with response:
        if response.status != 206:
            # Сервер прислал файл целиком (Range не поддержан или не запрошен).
            offset = 0
        total = response_total(response, offset)
        if size is not None and total is not None and total != size:
            raise TransferError(f"сервер сообщает размер {total} байт, закреплён {size}")
        with partial.open("r+b" if offset else "wb") as stream:
            stream.seek(offset)
            stream.truncate()
            while block := response.read(CHUNK):
                stream.write(block)
            received = stream.tell()
    expected_total = size if size is not None else total
    if expected_total is not None and received != expected_total:
        raise TransferError(f"передача оборвана: получено {received} из {expected_total} байт")


def fetch(url: str, expected: str, name: str, size: int | None = None, *,
          attempts: int = ATTEMPTS, sleep=time.sleep) -> Path:
    expected = expected.lower()
    directory = storage_dir()
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / name
    with directory_lock(directory):
        if target.is_file():
            if is_verified(target, expected, size):
                # Время изменения — отметка последнего использования для обслуживания кэша.
                os.utime(target)
                log(f"{name} взят из {directory}, SHA-256 совпал")
                return target
            log(f"{target} не прошёл проверку, скачиваю заново")
        partial = partial_path(directory, name, expected)
        last_error: BaseException | None = None
        for attempt in range(1, attempts + 1):
            try:
                transfer(url, partial, size)
                actual = sha256_of(partial)
                if actual != expected:
                    partial.unlink()
                    raise TransferError(f"SHA-256 {actual} не совпадает с ожидаемым {expected}")
                os.replace(partial, target)
                return target
            except (OSError, http.client.HTTPException, TransferError) as error:
                last_error = error
                log(f"попытка {attempt}/{attempts} не удалась: {error}")
                if attempt < attempts:
                    sleep(BACKOFF_SECONDS[min(attempt, len(BACKOFF_SECONDS)) - 1])
        partial.unlink(missing_ok=True)
    raise SystemExit(f"fetch_verified_tool: не удалось получить проверенный {name} из {url}: {last_error}")


def prune_stale(directory: Path, max_age_seconds: float, *, now: float | None = None) -> list[str] | None:
    """Удаляет архивы и неполные файлы, не использованные дольше ``max_age_seconds``.

    Возвращает удалённые имена или ``None``, если каталог сейчас занят загрузкой.
    """
    if not directory.is_dir():
        return []
    now = time.time() if now is None else now
    with directory_lock(directory, blocking=False) as lock:
        if lock is None:
            return None
        removed = []
        for entry in sorted(directory.iterdir()):
            if entry.name == LOCK_NAME or not entry.is_file() or entry.is_symlink():
                continue
            if now - entry.stat().st_mtime > max_age_seconds:
                entry.unlink()
                removed.append(entry.name)
        return removed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--name", required=True, help="имя файла в каталоге хранения")
    parser.add_argument("--size", type=int, help="закреплённый размер архива в байтах")
    args = parser.parse_args()
    if "/" in args.name or args.name.startswith(".") or args.name == "":
        raise SystemExit("fetch_verified_tool: --name должно быть именем файла без каталогов и точки в начале")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", args.sha256):
        raise SystemExit("fetch_verified_tool: --sha256 должно быть 64 шестнадцатеричными символами")
    if args.size is not None and args.size <= 0:
        raise SystemExit("fetch_verified_tool: --size должно быть положительным")
    print(fetch(args.url, args.sha256, args.name, args.size))
    return 0


if __name__ == "__main__":
    sys.exit(main())
