#!/usr/bin/env python3
"""Сетевая проверка пары PR/Gist; только stdlib, без выполнения содержимого Gist."""

import argparse
import hashlib
import http.client
import json
import os
import re
import sys
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


class ContractError(ValueError):
    """Fail closed с диагностикой без вывода токенов и удалённого содержимого."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractError(message)


def sha256(content: str) -> str:
    return hashlib.sha256(content.encode("utf-8")).hexdigest()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ContractError("GitHub API: неожиданный redirect; авторизация не перенаправлена")


class GitHubAPI:
    """Фиксированный официальный API; URL Gist никогда не становится адресом запроса."""

    def __init__(self):
        self.opener = urllib.request.build_opener(NoRedirect)

    def get(self, path: str) -> dict:
        require(path.startswith(("/gists/", "/repos/")), "Недопустимый API path")
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
            raise ContractError(f"GitHub API HTTP {exc.code}: проверьте доступ, полномочия и rate limit") from None
        except (OSError, UnicodeError, ValueError, http.client.HTTPException) as exc:
            if isinstance(exc, ContractError):
                raise
            raise ContractError("GitHub API: сеть, неполный ответ или повреждённый JSON") from None


def gist_files(snapshot: dict, url: str) -> tuple[dict[str, str], str]:
    owner, gist_id = url.rsplit("/", 2)[-2:]
    require(snapshot.get("id") == gist_id and snapshot.get("html_url") == url,
            "Gist: ID/URL не соответствует канонической ссылке")
    require((snapshot.get("owner") or {}).get("login", "").casefold() == owner.casefold(),
            "Gist: владелец не соответствует ссылке")
    require(snapshot.get("truncated") is False, "Gist: truncated/неполный список файлов; получите полную версию")
    history = snapshot.get("history")
    require(isinstance(history, list) and bool(history) and isinstance(history[0], dict)
            and bool(FULL_SHA.fullmatch(str(history[0].get("version", "")))), "Gist: отсутствует версия снимка")
    entries = snapshot.get("files")
    require(isinstance(entries, dict) and bool(entries), "Gist: нет файлов")
    result = {}
    for name, entry in entries.items():
        require(bool(FILENAME.fullmatch(name)), "Gist: недопустимое имя файла")
        require(isinstance(entry, dict) and entry.get("filename") == name, "Gist: повреждённое описание файла")
        # Намеренный fail closed: raw-fetch не нужен для небольших отчётов. CLI не
        # объявляет усечённый ответ полным даже при совпадении хеша его префикса.
        require(entry.get("truncated") is False, f"Gist: {name} truncated; нужен полный файл, проверка остановлена")
        content = entry.get("content")
        require(isinstance(content, str) and type(entry.get("size")) is int
                and len(content.encode("utf-8")) == entry["size"], f"Gist: неполный файл {name}")
        result[name] = content
    return result, history[0]["version"]


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
        lines = [line for line in parts[heading].splitlines() if line.startswith("|")]
        require(len(lines) >= 2 and table_cells(lines[0]) == columns, f"Индекс: неверная таблица {heading}")
        result = [table_cells(line) for line in lines[2:]]
        require(all(len(row) == len(columns) for row in result), f"Индекс: неполная строка {heading}")
        return result
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


def validate_evidence(body: str, files: dict[str, str], expected: dict,
                      expected_manifest: dict | None = None, source_dir: Path | None = None,
                      check_sources: dict | None = None) -> None:
    """Проверяемый инвентарь, не доказательство истинности семантики и полноты мира."""
    require(not validate(body), "PR body не соответствует локальному контракту")
    require(all(name.endswith(".md") for name in files), "Gist: разрешены только Markdown-файлы; служебные вложения запрещены")
    require("manifest.md" in files and "details.md" in files, "Gist: нужны manifest.md и содержательный details.md")
    manifest = read_manifest(files["manifest.md"])
    for key in ("repository", "pr_number", "head_sha", "base_ref", "head_ref"):
        require(manifest.get(key) == expected.get(key), f"Gist: устаревшая или чужая привязка {key}")
    require(bool(FULL_SHA.fullmatch(str(manifest.get("head_sha", "")))), "Gist: нужен полный head_sha")
    ci_shas = re.findall(r"`([0-9a-f]{12,40})`", dict(sections(body))["CI"])
    require(any(manifest["head_sha"].startswith(s) for s in ci_shas),
            "PR: раздел CI не привязан к текущему HEAD")
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
    expected_results = []
    reports = set()
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
        expected_results.append(result)
        matching_summaries = [text for text in actual_summaries if fields(text).get("Результат") == [result]]
        require(len(matching_summaries) == 1, "Gist: сводки PR не совпадают с индексом измерений")
        summary = matching_summaries[0]
        summary_fields = fields(summary)
        for field, key in (("Кандидат", "candidate_sha"), ("База", "baseline_sha")):
            if benchmark[key] is not None:
                require(any(benchmark[key][:12] in v for v in summary_fields.get(field, [])),
                        f"Gist: сводка не указывает проверенный {key}")
        require(isinstance(benchmark.get("scenario"), str)
                and benchmark["scenario"] in files[report] and benchmark["scenario"] in summary,
                "Gist: сценарий отчёта и сводки не совпадает")
        regressions = benchmark.get("regressions")
        require(isinstance(regressions, list), "Gist: нужен список regressions, даже пустой")
        for regression in regressions:
            require(isinstance(regression, str) and len(regression) >= 3 and not placeholder(regression)
                    and regression in summary_fields.get("Регрессии", [""])[0]
                    and regression in files[report], "Gist: существенная регрессия потеряна в PR/отчёте")
    actual_results = [fields(text).get("Результат", [""])[0] for text in actual_summaries]
    require(sorted(actual_results) == sorted(expected_results), "Gist: сводки PR не совпадают с инвентарём измерений")
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
    require((files, version) == (reread_files, reread_version), "Gist: гонка, версия/файлы изменились при проверке")


def check_unpublished(body: str, expected: dict, api=None, expected_manifest=None, source_dir=None, check_sources=None) -> None:
    """До создания PR проверяем реальную опубликованную ветку; номер ещё неизвестен."""
    require(expected.get("pr_number") is None, "Новый PR ещё не имеет номера")
    require(bool(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", expected["repository"]))
            and bool(FULL_SHA.fullmatch(expected["head_sha"])), "Неверные repository/HEAD")
    from urllib.parse import quote
    api = api or GitHubAPI()
    path = f"/repos/{expected['repository']}/branches/{quote(expected['head_ref'], safe='')}"
    before = api.get(path)
    require((before.get("commit") or {}).get("sha") == expected["head_sha"], "Ветка не соответствует ожидаемому HEAD")
    check_gist(body, expected, api, expected_manifest, source_dir, check_sources)
    require(api.get(path) == before, "Ветка изменилась во время preflight")


def check_remote(body: str | None, repository: str, number: int, api=None,
                 expected_manifest: dict | None = None, source_dir: Path | None = None,
                 event_head: str | None = None, check_sources: dict | None = None) -> None:
    require(bool(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)) and number > 0, "PR: неверный repository/number")
    api = api or GitHubAPI()
    path = f"/repos/{repository}/pulls/{number}"
    before = api.get(path)
    expected = pr_identity(before, repository)
    if event_head is not None:
        require(event_head == expected["head_sha"], "PR: HEAD изменился после события")
    if is_automation_pr({"pull_request": before}):
        return
    published_body = before.get("body") or ""
    checking_body = published_body if body is None else body
    errors = validate(checking_body)
    require(not errors, "; ".join(errors))
    check_gist(checking_body, expected, api, expected_manifest, source_dir, check_sources)
    after = api.get(path)
    require(pr_identity(after, repository) == expected and after.get("body") == before.get("body")
            and after.get("draft") == before.get("draft"), "PR: HEAD/body/refs/state изменились при проверке")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--event-path", type=Path)
    source.add_argument("--body-file", type=Path)
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
        event_head = None
        if args.event_path:
            event = json.loads(args.event_path.read_text(encoding="utf-8"))
            event_head = event["pull_request"]["head"]["sha"]
            repository = event["repository"]["full_name"]
            number = event["pull_request"]["number"]
            body = None  # Проверяем опубликованный body, событие — только identity.
        else:
            require(bool(args.repository), "Нужен --repository")
            repository, number = args.repository, args.pr_number
            body = args.body_file.read_text(encoding="utf-8")
        manifest = read_manifest(args.expected_manifest.read_text(encoding="utf-8")) if args.expected_manifest else None
        check_sources = json.loads(args.check_sources.read_text(encoding="utf-8")) if args.check_sources else None
        if number is None:
            require(bool(args.head_sha and args.base_ref and args.head_ref),
                    "До создания PR нужны --head-sha, --base-ref и --head-ref")
            expected = {"repository": repository, "pr_number": None, "head_sha": args.head_sha,
                        "base_ref": args.base_ref, "head_ref": args.head_ref}
            check_unpublished(body, expected, expected_manifest=manifest, source_dir=args.source_dir, check_sources=check_sources)
        else:
            check_remote(body, repository, number, expected_manifest=manifest,
                         source_dir=args.source_dir, event_head=event_head, check_sources=check_sources)
    except (OSError, UnicodeError, ValueError, KeyError, TypeError, AttributeError) as exc:
        # Не выводим произвольное содержимое ответа в workflow commands.
        print("PR/Gist: " + str(exc).replace("\n", " ").replace("\r", " "), file=sys.stderr)
        return 1
    print("Пара PR/Gist подтверждена; семантическая проверка источников остаётся за ревьюером")
    return 0


if __name__ == "__main__":
    sys.exit(main())
