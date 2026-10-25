#!/usr/bin/env python3
"""Воспроизводимость встроенных каталогов эталонов кандзи (только stdlib).

Встроенные каталоги масок KanjiVG и Noto Serif JP в `tools/asset-store/src/data/`
построены генераторами возможности Cargo `reference-generator`. Их файлы атрибуции —
единственный источник закреплённых данных: URL исходника, его SHA-256, имя
сгенерированного файла и его SHA-256. Скрипт скачивает закреплённые исходники,
проверяет их SHA-256, запускает генераторы во временный каталог и требует
побайтового совпадения результата и с атрибуцией, и с фактическим встроенным
файлом. Генераторы однопоточные, поэтому два каталога строятся параллельно.

Сеть нужна только для закреплённых публичных исходников. Недоступность источника,
несовпадение SHA-256 и расхождение результата — отказ с аннотацией `::error`,
а не пропуск.
"""

import argparse
import concurrent.futures
import hashlib
import http.client
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

DATA_DIR = Path("tools/asset-store/src/data")
SHA256 = r"([0-9a-f]{64})"
# Только плоские имена `kanji/<имя>.svg`: путь назначения строится из имени файла.
KANJIVG_MEMBER = re.compile(r"^kanji/[^/\\]+\.svg$")
DOWNLOAD_TIMEOUT = 120
DOWNLOAD_ATTEMPTS = 3


class CatalogError(Exception):
    """Отказ проверки с понятным сообщением для аннотации."""


@dataclass(frozen=True)
class Catalog:
    title: str
    attribution: str
    binary: str
    source_flag: str


CATALOGS = (
    Catalog("KanjiVG", "kanjivg-ATTRIBUTION.txt", "build-kanjivg-reference", "--source-dir"),
    Catalog("Noto Serif JP", "noto-serif-jp-ATTRIBUTION.txt", "build-noto-font-reference", "--font-file"),
)


@dataclass(frozen=True)
class Pins:
    source_url: str
    source_sha256: str
    generated_file: str
    generated_sha256: str


def parse_attribution(text):
    """Извлекает закреплённые значения из файла атрибуции."""

    def single(pattern, what):
        found = re.findall(pattern, text, flags=re.MULTILINE)
        if len(found) != 1:
            raise CatalogError(f"в атрибуции нет однозначного значения: {what}")
        return found[0]

    return Pins(
        source_url=single(r"^Source (?:archive|font):\n\s+(https://\S+)$", "URL исходника"),
        source_sha256=single(rf"^Source (?:archive|font) SHA-256:\n\s+{SHA256}$", "SHA-256 исходника"),
        generated_file=single(r"^Generated file: (\S+)$", "имя сгенерированного файла"),
        generated_sha256=single(rf"^Generated file SHA-256:\n\s+{SHA256}$", "SHA-256 результата"),
    )


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download(url, target):
    last_error = None
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(url, timeout=DOWNLOAD_TIMEOUT) as response, open(target, "wb") as out:
                while chunk := response.read(1 << 20):
                    out.write(chunk)
                # `HTTPResponse.read(n)` при досрочном закрытии потока возвращает `b''` без
                # исключения; `length` — недополученный остаток объявленного `Content-Length`.
                missing = getattr(response, "length", None)
                if missing:
                    raise http.client.IncompleteRead(b"", missing)
            return
        # Обрыв потока посреди ответа — `http.client.IncompleteRead`, а это не `OSError`.
        except (OSError, http.client.HTTPException) as error:
            last_error = error
            if attempt < DOWNLOAD_ATTEMPTS:
                time.sleep(5 * attempt)
    raise CatalogError(f"закреплённый исходник недоступен ({url}): {last_error}")


def extract_kanjivg(archive, target):
    """Распаковывает только `kanji/*.svg`; иные пути архива не трогаются."""
    count = 0
    with zipfile.ZipFile(archive) as bundle:
        for member in bundle.infolist():
            if member.is_dir() or not KANJIVG_MEMBER.match(member.filename):
                continue
            destination = target / Path(member.filename).name
            destination.write_bytes(bundle.read(member))
            count += 1
    if count == 0:
        raise CatalogError("в архиве KanjiVG нет файлов kanji/*.svg")
    return count


def regenerate(catalog, pins, repo_root, work, profile):
    source = work / Path(pins.source_url).name
    download(pins.source_url, source)
    actual = sha256_file(source)
    if actual != pins.source_sha256:
        raise CatalogError(f"SHA-256 исходника {actual} не совпадает с закреплённым {pins.source_sha256}")
    if catalog.source_flag == "--source-dir":
        directory = work / "kanji"
        directory.mkdir()
        extract_kanjivg(source, directory)
        source = directory
    output = work / pins.generated_file
    command = [
        "cargo", "run", "--locked", "--quiet", "--profile", profile, "-p", "asset-store",
        "--features", "reference-generator", "--bin", catalog.binary, "--",
        catalog.source_flag, str(source), "--output", str(output),
    ]
    completed = subprocess.run(command, cwd=repo_root, check=False)
    if completed.returncode != 0:
        raise CatalogError(f"генератор {catalog.binary} завершился с кодом {completed.returncode}")
    return sha256_file(output)


def check_catalog(catalog, repo_root, work, profile):
    """Возвращает строку таблицы сводки; бросает CatalogError при отказе."""
    pins = parse_attribution((repo_root / DATA_DIR / catalog.attribution).read_text(encoding="utf-8"))
    embedded = sha256_file(repo_root / DATA_DIR / pins.generated_file)
    if embedded != pins.generated_sha256:
        raise CatalogError(
            f"встроенный {pins.generated_file} ({embedded}) не совпадает с атрибуцией ({pins.generated_sha256})"
        )
    regenerated = regenerate(catalog, pins, repo_root, work, profile)
    if regenerated != embedded:
        raise CatalogError(f"регенерация дала {regenerated}, встроенный файл — {embedded}")
    return f"| {catalog.title} | `{pins.generated_file}` | `{embedded[:16]}…` | совпадает |"


def write_summary(lines):
    markdown = "\n".join(lines) + "\n"
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as summary:
            summary.write(markdown)
    else:
        print(markdown)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    parser.add_argument(
        "--profile",
        default="dev",
        help="профиль Cargo генераторов; результат от профиля не зависит, это и проверяется",
    )
    args = parser.parse_args(argv)
    rows = []
    failures = 0
    with tempfile.TemporaryDirectory(prefix="reference-catalogs-") as temp:
        works = {}
        for catalog in CATALOGS:
            works[catalog] = Path(temp) / catalog.binary
            works[catalog].mkdir()
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(CATALOGS)) as pool:
            futures = {
                catalog: pool.submit(check_catalog, catalog, args.repo_root, works[catalog], args.profile)
                for catalog in CATALOGS
            }
            for catalog, future in futures.items():
                try:
                    rows.append(future.result())
                except (CatalogError, OSError, zipfile.BadZipFile) as error:
                    failures += 1
                    rows.append(f"| {catalog.title} | — | — | отказ: {error} |")
                    print(f"::error title=Эталон {catalog.title}::{error}")
    write_summary(
        [
            "### Воспроизводимость эталонов кандзи",
            "",
            "| Каталог | Файл | SHA-256 | Регенерация |",
            "| --- | --- | --- | --- |",
            *rows,
            "",
        ]
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
