#!/usr/bin/env python3
"""Сетевая проверка пары PR/Gist; только stdlib, без выполнения содержимого Gist."""

import argparse
import hashlib
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

from check_pr_body import (
    REVIEW_SECTION, canonical_gist, fields, is_automation_pr, normalize_body,
    placeholder, role_shas, REVIEW_ROLE, PUBLISHED_ROLE, sections, validate,
    validate_coderabbit, scenario_blocks, COMPACT_FIELD, list_item_end, table_cells,
)

FULL_SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
FILENAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,150}\Z")
DETAIL_HEADINGS = ("Назначение", "Технические результаты", "Проверки и источники", "Ограничения")
KINDS = {"details", "benchmark-report", "review-log"}
# Ограниченное ожидание согласования опубликованных body/Gist после события CI:
# укладывается в timeout job и стоит не больше десятка чтений PR.
STABILIZATION_SECONDS = 180
POLL_SECONDS = 20
SEPARATOR_CELL = re.compile(r":?-+:?\Z")


class ContractError(ValueError):
    """Отказ с диагностикой без вывода токенов и удалённого содержимого."""


class StaleBinding(ContractError):
    """Опубликованные body/Gist пока описывают предыдущее состояние того же PR.

    Распознаваемое переходное состояние публикации: без ожидания это обычный
    отказ, в проверке события CI — повод перечитать PR в ограниченном окне.
    `stale_sha` — прежний HEAD из manifest, его происхождение проверяется по API.
    """

    def __init__(self, message: str, stale_sha: str | None = None):
        super().__init__(message)
        self.stale_sha = stale_sha


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractError(message)


def sha256(content: str) -> str:
    return hashlib.sha256(content.encode("utf-8")).hexdigest()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ContractError("GitHub API: неожиданное перенаправление; авторизация не передаётся")


class GitHubAPI:
    """Фиксированный официальный API; URL Gist никогда не становится адресом запроса."""

    def __init__(self):
        self.opener = urllib.request.build_opener(NoRedirect)

    def get(self, path: str) -> dict:
        require(path.startswith(("/gists/", "/repos/")), "Недопустимый путь GitHub API")
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if token and path.startswith("/repos/"):
            headers["Authorization"] = f"Bearer {token}"
        request = urllib.request.Request("https://api.github.com" + path, headers=headers)
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(12 * 1024 * 1024 + 1)
            require(len(raw) <= 12 * 1024 * 1024, "GitHub API: ответ слишком велик")
            data = json.loads(raw.decode("utf-8"))
            require(isinstance(data, dict), "GitHub API: ожидался JSON-объект")
            return data
        except urllib.error.HTTPError as exc:
            exc.close()
            raise ContractError(f"GitHub API HTTP {exc.code}: проверьте доступ, полномочия и лимит запросов") from None
        except (OSError, UnicodeError, ValueError, http.client.HTTPException) as exc:
            if isinstance(exc, ContractError):
                raise
            raise ContractError("GitHub API: сеть, неполный ответ или повреждённый JSON") from None


def gist_files(snapshot: dict, url: str) -> tuple[dict[str, str], str]:
    owner, gist_id = url.rsplit("/", 2)[-2:]
    require(snapshot.get("id") == gist_id and snapshot.get("html_url") in (url, f"https://gist.github.com/{gist_id}"),
            "Gist: ID/URL не соответствует канонической ссылке")
    require((snapshot.get("owner") or {}).get("login", "").casefold() == owner.casefold(),
            "Gist: владелец не соответствует ссылке")
    require(snapshot.get("truncated") is False, "Gist: список файлов усечён; нужна полная версия")
    history = snapshot.get("history")
    require(isinstance(history, list) and bool(history) and isinstance(history[0], dict)
            and bool(FULL_SHA.fullmatch(str(history[0].get("version", "")))), "Gist: отсутствует версия снимка")
    entries = snapshot.get("files")
    require(isinstance(entries, dict) and bool(entries), "Gist: нет файлов")
    result = {}
    for name, entry in entries.items():
        require(bool(FILENAME.fullmatch(name)), "Gist: недопустимое имя файла")
        require(isinstance(entry, dict) and entry.get("filename") == name, "Gist: повреждённое описание файла")
        # Намеренный отказ: отдельная загрузка raw для небольших отчётов не нужна.
        # Усечённый ответ не объявляется полным даже при совпадении хеша префикса.
        require(entry.get("truncated") is False, f"Gist: {name} усечён; нужен полный файл, проверка остановлена")
        content = entry.get("content")
        require(isinstance(content, str) and type(entry.get("size")) is int
                and len(content.encode("utf-8")) == entry["size"], f"Gist: неполный файл {name}")
        result[name] = content
    return result, history[0]["version"]


def table_rows(content: str, columns: list[str], heading: str) -> list[list[str]]:
    """Единственная таблица раздела: заголовок, настоящий разделитель, строки данных.

    Строка с `|` вне непрерывной таблицы, неверная ширина или текст на месте
    разделителя — отказ, а не молчаливая потеря записи.
    """
    lines = content.splitlines()
    starts = [i for i, line in enumerate(lines) if line.lstrip().startswith("|")]
    require(bool(starts), f"Индекс: нет таблицы {heading}")
    first = starts[0]
    end = first
    while end < len(lines) and lines[end].lstrip().startswith("|"):
        end += 1
    require(starts[-1] < end, f"Индекс: строки таблицы {heading} разорваны")
    require(not any(re.search(r"(?<!\\)\|", line) for line in lines[end:end + 1] if line.strip()),
            f"Индекс: строка таблицы {heading} без начальной границы")
    table = lines[first:end]
    require(len(table) >= 2 and table_cells(table[0]) == columns, f"Индекс: неверный заголовок таблицы {heading}")
    separator = table_cells(table[1])
    require(len(separator) == len(columns) and all(SEPARATOR_CELL.match(cell) for cell in separator),
            f"Индекс: вторая строка таблицы {heading} — не разделитель Markdown нужной ширины")
    result = [table_cells(line) for line in table[2:]]
    require(all(len(row) == len(columns) for row in result), f"Индекс: неполная строка {heading}")
    return result


def read_manifest(text: str) -> dict:
    """Читаемый Markdown-индекс: метаданные и обычные таблицы, без JSON-вложения."""
    prefix = fields(text.split("## ", 1)[0])
    def value(name):
        values = prefix.get(name, [])
        require(len(values) == 1, f"Индекс: нужно поле {name}")
        return values[0].strip("`")
    require(value("Схема") == "2", "Индекс: неизвестная схема")
    parts = dict(sections(text))
    def rows(heading, columns):
        require(heading in parts, f"Индекс: нет раздела {heading}")
        return table_rows(parts[heading], columns, heading)
    artifacts = [{"file": r[0], "kind": r[1], "commit_sha": r[2].strip("`"), "sha256": r[3].strip("`")}
                 for r in rows("Файлы", ["Файл", "Тип", "Commit", "SHA-256"])]
    checks = [{"name": r[0], "commit_sha": r[1].strip("`"), "exit_code": int(r[2]), "output_sha256": r[3].strip("`")}
              for r in rows("Проверки", ["Проверка", "Commit", "Код", "SHA-256 локального подтверждения"])]
    benchmarks = [{"report_file": r[0], "scenario": r[1], "baseline_sha": None if r[2] == "нет" else r[2].strip("`"),
                   "candidate_sha": r[3].strip("`"), "result": r[4],
                   "regressions": [] if r[5] == "нет" else r[5].split("; ")}
                  for r in rows("Измерения", ["Файл", "Сценарий", "База", "Кандидат", "Результат", "Регрессии"])]
    reviews = [{"iteration": int(r[0]), "reviewed_sha": r[1].strip("`"), "published_sha": r[2].strip("`"), "findings": int(r[3])}
               for r in rows("CodeRabbit", ["Итерация", "Reviewed commit", "Published commit", "Строк журнала"])]
    return {"schema_version": 2, "repository": value("Репозиторий"),
            "pr_number": None if value("PR") == "не создан" else int(value("PR")),
            "head_sha": value("HEAD"), "base_ref": value("База"), "head_ref": value("Ветка"),
            "task": value("Задача"), "artifacts": artifacts, "checks": checks,
            "benchmarks": benchmarks, "reviews": reviews}


def require_ci_head(body: str, head_sha: str) -> None:
    """Раздел CI называет текущий HEAD; иной SHA — прежнее описание того же PR."""
    ci_shas = re.findall(r"`([0-9a-f]{12,40})`", dict(sections(body)).get("CI", ""))
    require(bool(ci_shas), "PR: раздел CI не называет проверенный HEAD")
    if not any(head_sha.startswith(s) for s in ci_shas):
        raise StaleBinding("PR: раздел CI не привязан к текущему HEAD")


def scenario_named(scenario: str, text: str) -> bool:
    """ID сценария целиком, а не префикс более длинного ID."""
    return re.search(rf"(?<![\w.-]){re.escape(scenario)}(?![\w.-])", text) is not None


def benchmark_mismatch(benchmark: dict, summary: str) -> str | None:
    """Первое несовпадение записи manifest со сводкой PR или None."""
    summary_fields = fields(summary)
    if not any(scenario_named(benchmark["scenario"], v) for v in summary_fields.get("Сценарий", [])):
        return "сценарий"
    for field, key in (("Кандидат", "candidate_sha"), ("База", "baseline_sha")):
        if benchmark[key] is not None and not any(benchmark[key][:12] in v for v in summary_fields.get(field, [])):
            return key
    if summary_fields.get("Результат") != [benchmark["result"]]:
        return "результат"
    if not all(r in summary_fields.get("Регрессии", [""])[0] for r in benchmark["regressions"]):
        return "регрессия"
    return None


def match_benchmarks(benchmarks: list[dict], summaries: list[str]) -> None:
    """Взаимно однозначное соответствие серий manifest и сводок PR.

    Одинаковый текст результата у разных серий допустим: связь задаёт вся
    совокупность сценария, SHA, результата и регрессий, а каждая сводка
    закрывает ровно одну серию (паросочетание с увеличивающими путями).
    """
    reasons = {"сценарий": "сводки PR не совпадают с инвентарём измерений: нет сводки сценария",
               "candidate_sha": "сводка не указывает проверенный candidate_sha",
               "baseline_sha": "сводка не указывает проверенный baseline_sha",
               "результат": "сводки PR не совпадают с индексом измерений",
               "регрессия": "существенная регрессия потеряна в PR/отчёте"}
    candidates = []
    for benchmark in benchmarks:
        found = [i for i, summary in enumerate(summaries) if benchmark_mismatch(benchmark, summary) is None]
        if not found:
            # Диагностика по самой близкой сводке: дальше всех прошедшей проверки.
            order = list(reasons)
            stages = [order.index(benchmark_mismatch(benchmark, s)) for s in summaries] or [order.index("результат")]
            reason = order[max(stages)]
            suffix = f" {benchmark['scenario']}" if reason == "сценарий" else ""
            raise ContractError("Gist: " + reasons[reason] + suffix)
        candidates.append(found)
    require(len(benchmarks) == len(summaries), "Gist: сводки PR не совпадают с инвентарём измерений")
    owner: dict[int, int] = {}

    def assign(index: int, seen: set[int]) -> bool:
        for summary in candidates[index]:
            if summary not in seen:
                seen.add(summary)
                if summary not in owner or assign(owner[summary], seen):
                    owner[summary] = index
                    return True
        return False

    require(all(assign(index, set()) for index in range(len(benchmarks))),
            "Gist: сводки PR не совпадают с инвентарём измерений: серия без собственной сводки")


def validate_evidence(body: str, files: dict[str, str], expected: dict,
                      expected_manifest: dict | None = None, source_dir: Path | None = None,
                      check_sources: dict | None = None) -> None:
    """Проверяемый инвентарь, не доказательство истинности семантики и полноты мира."""
    require(not validate(body), "PR body не соответствует локальному контракту")
    require(all(name.endswith(".md") for name in files), "Gist: разрешены только Markdown-файлы; служебные вложения запрещены")
    require("manifest.md" in files and "details.md" in files, "Gist: нужны manifest.md и содержательный details.md")
    manifest = read_manifest(files["manifest.md"])
    require(bool(FULL_SHA.fullmatch(str(manifest.get("head_sha", "")))), "Gist: нужен полный head_sha")
    for key in ("repository", "base_ref", "head_ref"):
        require(manifest.get(key) == expected.get(key), f"Gist: чужая привязка {key}")
    if manifest["pr_number"] is None and expected.get("pr_number") is not None:
        raise StaleBinding("Gist: manifest ещё не дополнен номером созданного PR")
    require(manifest["pr_number"] == expected.get("pr_number"), "Gist: чужая привязка pr_number")
    if manifest["head_sha"] != expected.get("head_sha"):
        raise StaleBinding("Gist: manifest описывает другой HEAD", stale_sha=manifest["head_sha"])
    require_ci_head(body, manifest["head_sha"])
    require(isinstance(manifest.get("task"), str) and not placeholder(manifest["task"]), "Gist: нет назначения задачи")
    if expected_manifest is not None:
        require(manifest == expected_manifest, "Gist: manifest отличается от локального инвентаря исходных доказательств")

    details = files["details.md"]
    require(normalize_body(details).strip() != normalize_body(body).strip(), "Gist: details.md — копия PR body")
    detail_sections = dict(sections(details))
    for heading in DETAIL_HEADINGS:
        text = detail_sections.get(heading, "").strip()
        require(len(text) >= 40 and not placeholder(text), f"Gist: details.md не раскрывает «{heading}»")
    require(manifest["repository"] in details and manifest["head_sha"] in details
            and manifest["task"] in details, "Gist: details.md не связан с repository/task/HEAD")
    if manifest["pr_number"] is not None:
        require(f"https://github.com/{manifest['repository']}/pull/{manifest['pr_number']}" in details,
                "Gist: details.md не связан с PR")

    artifacts = manifest.get("artifacts")
    require(isinstance(artifacts, list) and bool(artifacts), "Gist: нет инвентаря доказательств")
    inventory = {}
    for item in artifacts:
        require(isinstance(item, dict), "Gist: повреждена запись artifacts")
        name = item.get("file")
        require(isinstance(name, str) and name in files and name != "manifest.md" and name not in inventory,
                "Gist: отсутствующий/повторный файл инвентаря")
        require(item.get("kind") in KINDS, f"Gist: неизвестный kind {name}")
        require(bool(DIGEST.fullmatch(str(item.get("sha256", "")))) and sha256(files[name]) == item["sha256"],
                f"Gist: не совпал SHA-256 {name}")
        require(bool(FULL_SHA.fullmatch(str(item.get("commit_sha", "")))), f"Gist: нет проверенного SHA {name}")
        require(bool(files[name].strip()) and not placeholder(files[name]), f"Gist: пустой артефакт {name}")
        if source_dir is not None:
            require(FILENAME.fullmatch(name) is not None, "Недопустимый локальный путь")
            require((source_dir / name).read_bytes() == files[name].encode("utf-8"),
                    f"Gist: {name} отличается от локального исходника")
        inventory[name] = item
    require(set(inventory) == set(files) - {"manifest.md"}, "Gist: файлы не перечислены в manifest")
    require(inventory["details.md"]["kind"] == "details", "Gist: неверный kind details.md")
    require(all(a["kind"] != "details" or a["file"] == "details.md" for a in artifacts),
            "Gist: служебные файлы нельзя маскировать как details")
    checks = manifest.get("checks")
    require(isinstance(checks, list) and bool(checks), "Индекс: нет результатов проверок")
    names = set()
    for check in checks:
        require(isinstance(check, dict) and isinstance(check.get("name"), str)
                and check["name"] not in names and not placeholder(check["name"]), "Индекс: неверное имя проверки")
        names.add(check["name"])
        require(bool(FULL_SHA.fullmatch(str(check.get("commit_sha", ""))))
                and type(check.get("exit_code")) is int
                and bool(DIGEST.fullmatch(str(check.get("output_sha256", "")))), "Индекс: неполный результат проверки")
        if check_sources is not None:
            require(check["name"] in check_sources, "Нет локального источника проверки")
            digest = hashlib.sha256(Path(check_sources[check["name"]]).read_bytes()).hexdigest()
            require(digest == check["output_sha256"], "Не совпал хеш локального подтверждения проверки")
    require(any(c["commit_sha"] == manifest["head_sha"] for c in checks),
            "Gist: нет подтверждения проверок текущего HEAD")

    benchmarks = manifest.get("benchmarks")
    reviews = manifest.get("reviews")
    require(isinstance(benchmarks, list) and isinstance(reviews, list), "Gist: нужны массивы benchmarks/reviews (в том числе пустые)")
    performance = dict(sections(body))["Производительность"]
    measured = fields(performance).get("Статус", [""])[0].strip("`").casefold() == "измерено"
    require(bool(benchmarks) == measured, "Gist: измерения manifest не соответствуют статусу PR")
    blocks = scenario_blocks(performance) or [("", performance)]
    actual_summaries = [b.strip() for h, b in blocks if not h.endswith("— не измерено")] if measured else []
    reports = set()
    rows = set()
    for benchmark in benchmarks:
        require(isinstance(benchmark, dict), "Gist: повреждена запись benchmark")
        report = benchmark.get("report_file")
        require(report in inventory and inventory[report]["kind"] == "benchmark-report", "Gist: отсутствует benchmark-report")
        reports.add(report)
        require(bool(FULL_SHA.fullmatch(str(benchmark.get("candidate_sha", ""))))
                and inventory[report]["commit_sha"] == benchmark["candidate_sha"], "Gist: неверный candidate SHA отчёта")
        require("baseline_sha" in benchmark and (benchmark["baseline_sha"] is None
                or bool(FULL_SHA.fullmatch(str(benchmark["baseline_sha"])))),
                "Gist: неверный baseline SHA")
        result = benchmark.get("result")
        require(isinstance(result, str) and result.strip(), "Индекс: нет результата benchmark")
        require(isinstance(benchmark.get("scenario"), str) and scenario_named(benchmark["scenario"], files[report]),
                "Gist: сценарий отчёта и сводки не совпадает")
        regressions = benchmark.get("regressions")
        require(isinstance(regressions, list), "Gist: нужен список regressions, даже пустой")
        for regression in regressions:
            require(isinstance(regression, str) and len(regression) >= 3 and not placeholder(regression)
                    and regression in files[report], "Gist: существенная регрессия потеряна в PR/отчёте")
        row = (report, benchmark["scenario"], benchmark["baseline_sha"], benchmark["candidate_sha"],
               result, tuple(regressions))
        require(row not in rows, "Gist: повторная запись серии в manifest")
        rows.add(row)
    match_benchmarks(benchmarks, actual_summaries)
    require(reports == {a["file"] for a in artifacts if a["kind"] == "benchmark-report"}, "Gist: потеряна benchmark-серия в manifest")

    review_sections = [text for name, text in sections(body) if name == REVIEW_SECTION]
    require(bool(reviews) == bool(review_sections), "Gist: CodeRabbit-итерации и сводка PR не соответствуют")
    if reviews:
        log = files.get("coderabbit-review.md", "")
        require(inventory.get("coderabbit-review.md", {}).get("kind") == "review-log" and len(log) > 100,
                "Gist: отсутствует полноценный coderabbit-review.md")
        section = review_sections[0]
        progress = next((section[m.end():list_item_end(section, m)].strip()
                         for m in COMPACT_FIELD.finditer(section) if m.group("name") == "Прогресс"), "")
        count = re.search(r"\b([1-9]\d*)\s+завершённ\w*\s+итерац", progress)
        require(count is not None and int(count[1]) == len(reviews), "Gist: неверно количество завершённых CodeRabbit-итераций")
        for number, review in enumerate(reviews, 1):
            require(isinstance(review, dict) and review.get("iteration") == number, "Gist: потеряна CodeRabbit-итерация")
            for key in ("reviewed_sha", "published_sha"):
                require(bool(FULL_SHA.fullmatch(str(review.get(key, "")))), f"Gist: нет {key} итерации")
            matching = []
            for line in log.splitlines():
                row = re.match(rf"\|\s*{number}\s*·\s*`([0-9a-f]{{7,40}})`\s*\|", line)
                if row and review["reviewed_sha"].startswith(row[1]):
                    matching.append(line)
            require(type(review.get("findings")) is int and review["findings"] > 0
                    and len(matching) == review["findings"], "Gist: потеряны строки накопительной истории итерации")
            require(all(len(table_cells(row)) == 6 and all(table_cells(row)) for row in matching),
                    "Gist: неполная строка журнала CodeRabbit")
            fixes = [s for row in matching for s in re.findall(r"`([0-9a-f]{7,40})`", table_cells(row)[-1])]
            require(any(review["published_sha"].startswith(s) for s in fixes),
                    "Gist: нет опубликованного исправления/маркера итерации")
        all_rows = [line for line in log.splitlines() if re.match(r"\|\s*[1-9]\d*\s*·", line)]
        require(len(all_rows) == sum(review["findings"] for review in reviews),
                "Gist: строки журнала не соответствуют полному инвентарю итераций")
        require(any(reviews[-1]["reviewed_sha"].startswith(s) for s in role_shas(progress, REVIEW_ROLE))
                and any(manifest["head_sha"].startswith(s) for s in role_shas(progress, PUBLISHED_ROLE)),
                "Gist: устаревшая CodeRabbit-сводка HEAD")
        require(not validate_coderabbit(review_sections[0]), "Gist: невалидная CodeRabbit-сводка")


def pr_identity(pr: dict, repository: str) -> dict:
    require(isinstance(pr.get("head"), dict) and isinstance(pr.get("base"), dict), "PR: нет refs")
    require((pr["base"].get("repo") or {}).get("full_name") == repository, "PR: чужой репозиторий")
    require(pr.get("state") == "open" and type(pr.get("number")) is int, "PR: недоступен или закрыт")
    return {"repository": repository, "pr_number": pr["number"], "head_sha": pr["head"].get("sha"),
            "base_ref": pr["base"].get("ref"), "head_ref": pr["head"].get("ref")}


def check_gist(body: str, expected: dict, api, expected_manifest=None, source_dir=None, check_sources=None) -> None:
    url = canonical_gist(body)
    gist_path = "/gists/" + url.rsplit("/", 1)[1]
    snapshot = api.get(gist_path)
    files, version = gist_files(snapshot, url)
    validate_evidence(body, files, expected, expected_manifest, source_dir, check_sources)
    reread_files, reread_version = gist_files(api.get(gist_path), url)
    if (files, version) != (reread_files, reread_version):
        raise StaleBinding("Gist: гонка, версия/файлы изменились при проверке")


def check_unpublished(body: str, expected: dict, api=None, expected_manifest=None, source_dir=None, check_sources=None) -> str:
    """Кандидат нового PR: реальная опубликованная ветка и Gist; номера PR ещё нет."""
    require(expected.get("pr_number") is None, "Новый PR ещё не имеет номера")
    require(bool(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", expected["repository"]))
            and bool(FULL_SHA.fullmatch(expected["head_sha"])), "Неверные repository/HEAD")
    from urllib.parse import quote
    api = api or GitHubAPI()
    path = f"/repos/{expected['repository']}/branches/{quote(expected['head_ref'], safe='')}"
    before = api.get(path)
    require((before.get("commit") or {}).get("sha") == expected["head_sha"], "Ветка не соответствует ожидаемому HEAD")
    check_gist(body, expected, api, expected_manifest, source_dir, check_sources)
    require(api.get(path) == before, "Ветка изменилась во время предварительной проверки")
    return (f"Кандидат нового PR согласован с Gist и опубликованной веткой (HEAD {expected['head_sha'][:12]}); "
            "PR ещё не создан, опубликованная пара не подтверждена")


def same_body(published: str, local: str) -> bool:
    """Совпадение с точностью до переводов строк и завершающего перевода строки."""
    return normalize_body(published).rstrip("\n") == normalize_body(local).rstrip("\n")


def require_ancestor(api, repository: str, stale_sha: str, head_sha: str) -> None:
    """Прежний HEAD manifest — предок текущего, а не чужой commit."""
    comparison = api.get(f"/repos/{repository}/compare/{stale_sha}...{head_sha}?per_page=1")
    require(comparison.get("status") == "ahead" and comparison.get("behind_by") == 0,
            "Gist: manifest привязан к commit вне истории текущего HEAD")


def check_remote(body: str | None, repository: str, number: int, api=None,
                 expected_manifest: dict | None = None, source_dir: Path | None = None,
                 event_head: str | None = None, check_sources: dict | None = None,
                 published: bool = False) -> str:
    """Проверить пару PR/Gist и вернуть описание фактически подтверждённого.

    `body is None` — опубликованное описание. Локальный `body` без `published` —
    кандидат до публикации: опубликованное описание может ещё отличаться.
    С `published` опубликованное описание обязано совпасть с локальным кандидатом.
    """
    require(bool(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)) and number > 0, "PR: неверный repository/number")
    api = api or GitHubAPI()
    path = f"/repos/{repository}/pulls/{number}"
    before = api.get(path)
    expected = pr_identity(before, repository)
    if event_head is not None:
        require(event_head == expected["head_sha"], "PR: HEAD изменился после события")
    if is_automation_pr({"pull_request": before}):
        return "PR бота зависимостей: контракт ручных PR не применяется"
    published_body = before.get("body") or ""
    candidate = body is not None and not published
    if body is not None and published:
        require(same_body(published_body, body),
                "PR: опубликованное описание отличается от локального кандидата; публикация не подтверждена")
    checking_body = body if candidate else published_body
    errors = validate(checking_body)
    require(not errors, "; ".join(errors))
    require_ci_head(checking_body, expected["head_sha"])
    try:
        check_gist(checking_body, expected, api, expected_manifest, source_dir, check_sources)
    except StaleBinding as exc:
        if exc.stale_sha is not None:
            require_ancestor(api, repository, exc.stale_sha, expected["head_sha"])
        raise
    after = api.get(path)
    require(pr_identity(after, repository) == expected, "PR: HEAD/refs/state изменились при проверке")
    if after.get("body") != before.get("body") or after.get("draft") != before.get("draft"):
        raise StaleBinding("PR: body/Draft изменились при проверке")
    head = expected["head_sha"][:12]
    if candidate:
        return (f"Кандидат описания согласован с Gist и PR #{number} (HEAD {head}); опубликованное "
                "описание не проверялось — после публикации повторите проверку с --published")
    return f"Опубликованная пара PR #{number}/Gist подтверждена для HEAD {head}"


def check_event(repository: str, number: int, event_head: str, api=None, *,
                window: float = STABILIZATION_SECONDS, interval: float = POLL_SECONDS,
                clock=time.monotonic, sleep=time.sleep, log=None) -> str:
    """Проверка события CI по опубликованному состоянию с ограниченной стабилизацией.

    Данные события могут содержать прежнее описание, а публикация commit и
    актуализация body/Gist — отдельные шаги. Поэтому распознанное переходное
    состояние (`StaleBinding`) перечитывается до `window` секунд; успех только
    у полной проверки того же HEAD, что и событие. Смена HEAD, невалидное
    описание, порча Gist, сеть и полномочия — немедленный отказ; по истечении
    окна переходное состояние тоже становится отказом.
    """
    api = api or GitHubAPI()
    log = log or (lambda message: print(message, file=sys.stderr))
    deadline = clock() + window
    while True:
        try:
            return check_remote(None, repository, number, api, event_head=event_head)
        except StaleBinding as exc:
            remaining = deadline - clock()
            if remaining <= 0:
                raise ContractError(f"{exc}; не согласовано за {window:g} с после события") from None
            log(f"PR/Gist: {exc}; повторное чтение через {min(interval, remaining):g} с")
            sleep(min(interval, remaining))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--event-path", type=Path, help="событие CI: опубликованная пара с ограниченной стабилизацией")
    source.add_argument("--body-file", type=Path, help="локальный кандидат описания")
    parser.add_argument("--published", action="store_true",
                        help="проверить опубликованное описание; с --body-file оно обязано совпасть с кандидатом")
    parser.add_argument("--repository")
    parser.add_argument("--pr-number", type=int)
    parser.add_argument("--head-sha")
    parser.add_argument("--base-ref")
    parser.add_argument("--head-ref")
    parser.add_argument("--expected-manifest", type=Path)
    parser.add_argument("--source-dir", type=Path)
    parser.add_argument("--check-sources", type=Path)
    args = parser.parse_args()
    try:
        if args.event_path:
            require(not args.published, "--published не сочетается с --event-path")
            event = json.loads(args.event_path.read_text(encoding="utf-8"))
            # Событие задаёт только идентичность; проверяется опубликованное состояние.
            message = check_event(event["repository"]["full_name"], event["pull_request"]["number"],
                                  event["pull_request"]["head"]["sha"])
            print(message + "; семантическая проверка источников остаётся за ревьюером")
            return 0
        require(bool(args.repository), "Нужен --repository")
        repository, number = args.repository, args.pr_number
        require(args.body_file is not None or (args.published and number is not None),
                "Нужен --body-file (кандидат) или --published с --pr-number (опубликованное описание)")
        require(not args.published or number is not None, "--published требует --pr-number")
        body = args.body_file.read_text(encoding="utf-8") if args.body_file else None
        manifest = read_manifest(args.expected_manifest.read_text(encoding="utf-8")) if args.expected_manifest else None
        check_sources = json.loads(args.check_sources.read_text(encoding="utf-8")) if args.check_sources else None
        if number is None:
            require(bool(args.head_sha and args.base_ref and args.head_ref),
                    "До создания PR нужны --head-sha, --base-ref и --head-ref")
            expected = {"repository": repository, "pr_number": None, "head_sha": args.head_sha,
                        "base_ref": args.base_ref, "head_ref": args.head_ref}
            message = check_unpublished(body, expected, expected_manifest=manifest,
                                        source_dir=args.source_dir, check_sources=check_sources)
        else:
            message = check_remote(body, repository, number, expected_manifest=manifest,
                                   source_dir=args.source_dir, check_sources=check_sources,
                                   published=args.published)
    except (OSError, UnicodeError, ValueError, KeyError, TypeError, AttributeError) as exc:
        # Произвольное содержимое ответа не попадает в команды GitHub Actions.
        print("PR/Gist: " + str(exc).replace("\n", " ").replace("\r", " "), file=sys.stderr)
        return 1
    print(message + "; семантическая проверка источников остаётся за ревьюером")
    return 0


if __name__ == "__main__":
    sys.exit(main())
