#!/usr/bin/env python3
"""Подготовка постоянной рабочей копии self-hosted раннера (только стандартная библиотека).

Выполняется в задании до любого кода репозитория. Порядок:

1. Доказательство изоляции — до любого удаления. Рабочая копия обязана быть
   выделенной копией раннера: текущий каталог совпадает с ``GITHUB_WORKSPACE``,
   тот лежит внутри ``RUNNER_WORKSPACE`` из каталога ``_work`` раннера, ``.git``
   — собственный каталог (не ссылка и не связанный worktree), а областей
   пользовательских данных (``decks/``, ``.asset-store/`` — список владельца
   ``check_local_data_untracked.py``) в ней нет вовсе. Иначе — отказ без
   изменений: такие данные нельзя ни удалять, ни оставлять доступными коду CI.
2. Очистка: ``git clean -ffdx -e /target/`` — всё неотслеживаемое и
   игнорируемое, кроме каталога сборки, затем проверка, что рабочая копия
   совпадает с ``HEAD`` без посторонних файлов.
3. Обслуживание постоянных данных: ``target/`` больше лимита удаляется целиком
   (сборка начнётся заново, sccache остаётся); архивы инструментов, не
   использованные дольше срока удержания, удаляются под блокировкой загрузчика;
   при нехватке свободного места сначала освобождается ``target/``, и если
   места всё равно мало — отказ с диагностикой.
"""

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_local_data_untracked import FORBIDDEN_ROOTS  # noqa: E402
from fetch_verified_tool import prune_stale  # noqa: E402

GIB = 1 << 30
TARGET_LIMIT_GIB = 25
MIN_FREE_GIB = 10
TOOL_RETENTION_DAYS = 30


class Refused(Exception):
    """Подготовка остановлена до разрушительного действия."""


def log(message: str) -> None:
    print(f"prepare_self_hosted_checkout: {message}", file=sys.stderr)


def git(workspace: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(workspace), *args],
                          capture_output=True, text=True, check=False)


def is_within(path: Path, parent: Path) -> bool:
    return path != parent and path.is_relative_to(parent)


def verify_isolation(workspace: Path, runner_workspace: Path, cwd: Path) -> None:
    if workspace.is_symlink() or not workspace.is_dir():
        raise Refused(f"GITHUB_WORKSPACE {workspace} не каталог или символическая ссылка")
    real = workspace.resolve()
    runner_real = runner_workspace.resolve()
    if cwd.resolve() != real:
        raise Refused(f"текущий каталог {cwd} не совпадает с GITHUB_WORKSPACE {workspace}")
    if runner_real.parent.name != "_work" or not is_within(real, runner_real):
        raise Refused(
            f"рабочая копия {real} не лежит внутри RUNNER_WORKSPACE {runner_real} "
            "каталога _work раннера"
        )
    dot_git = workspace / ".git"
    if dot_git.is_symlink() or not dot_git.is_dir():
        raise Refused(f"{dot_git} не собственный каталог Git (ссылка или связанный worktree)")
    top = git(workspace, "rev-parse", "--show-toplevel")
    if top.returncode != 0 or Path(top.stdout.strip()).resolve() != real:
        raise Refused(f"корень Git {top.stdout.strip() or top.stderr.strip()} не совпадает с {real}")
    present = [name for name in FORBIDDEN_ROOTS if os.path.lexists(workspace / name)]
    if present:
        raise Refused(
            f"в рабочей копии раннера есть области пользовательских данных: {', '.join(present)}. "
            "Это не выделенная копия CI или в неё попали приватные данные; очистка не "
            "выполнялась. Владельцу: проверить происхождение и вручную перенести или удалить "
            "эти каталоги из рабочей копии раннера"
        )


def clean(workspace: Path) -> None:
    result = git(workspace, "clean", "-ffdx", "-e", "/target/")
    if result.returncode != 0:
        raise Refused(f"git clean завершился с ошибкой: {result.stderr.strip()}")
    status = git(workspace, "status", "--porcelain", "--untracked-files=all")
    if status.returncode != 0 or status.stdout.strip():
        raise Refused(f"рабочая копия не совпадает с HEAD после очистки:\n{status.stdout}{status.stderr}")


def tree_size(path: Path) -> int:
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except FileNotFoundError:
                pass
    return total


def free_bytes(path: Path) -> int:
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize


def maintain(workspace: Path, tool_cache: Path | None, *, target_limit: int, min_free: int,
             retention_seconds: float, free=free_bytes) -> None:
    target = workspace / "target"
    if target.is_symlink():
        raise Refused(f"{target} — символическая ссылка; ожидается каталог сборки рабочей копии")
    if target.is_dir():
        size = tree_size(target)
        if size > target_limit:
            log(f"target/ занимает {size / GIB:.1f} ГиБ > {target_limit / GIB:.1f} ГиБ — удаляю")
            shutil.rmtree(target)
    if tool_cache is not None:
        removed = prune_stale(tool_cache, retention_seconds)
        if removed is None:
            log(f"{tool_cache} занят загрузкой — удержание архивов пропущено")
        elif removed:
            log(f"удалены неиспользуемые архивы: {', '.join(removed)}")
    available = free(workspace)
    if available < min_free and target.is_dir():
        log(f"свободно {available / GIB:.1f} ГиБ < {min_free / GIB:.1f} ГиБ — удаляю target/")
        shutil.rmtree(target)
        available = free(workspace)
    if available < min_free:
        raise Refused(
            f"свободно {available / GIB:.1f} ГиБ < {min_free / GIB:.1f} ГиБ даже без target/; "
            "владельцу: освободить место (sccache, другие данные машины)"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tool-cache", type=Path, help="каталог проверенных архивов инструментов")
    parser.add_argument("--target-limit-gib", type=float, default=TARGET_LIMIT_GIB)
    parser.add_argument("--min-free-gib", type=float, default=MIN_FREE_GIB)
    parser.add_argument("--tool-retention-days", type=float, default=TOOL_RETENTION_DAYS)
    args = parser.parse_args()
    try:
        workspace = Path(os.environ["GITHUB_WORKSPACE"])
        runner_workspace = Path(os.environ["RUNNER_WORKSPACE"])
    except KeyError as missing:
        log(f"не задана переменная {missing}")
        return 1
    try:
        verify_isolation(workspace, runner_workspace, Path.cwd())
        clean(workspace)
        maintain(workspace, args.tool_cache,
                 target_limit=int(args.target_limit_gib * GIB),
                 min_free=int(args.min_free_gib * GIB),
                 retention_seconds=args.tool_retention_days * 86400)
    except Refused as refusal:
        text = str(refusal).replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::error title=Постоянная рабочая копия::{text}")
        return 1
    log("рабочая копия совпадает с HEAD, постоянные данные в пределах лимитов")
    return 0


if __name__ == "__main__":
    sys.exit(main())
