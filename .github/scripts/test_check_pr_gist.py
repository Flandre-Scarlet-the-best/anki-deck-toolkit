"""Наблюдаемые отказы локальной/сетевой пары PR и Gist; сеть только mock."""

import copy
import io
import http.client
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch

from check_pr_body import validate
from check_pr_gist import (
    ContractError, GitHubAPI, NoRedirect, check_remote, check_unpublished, gist_files, sha256, validate_evidence,
)
from test_check_pr_body import COMPACT_CODERABBIT, GIST, MEASURED, REPORT, body_with, valid_body

REPOSITORY = "example/toolkit"
HEAD = "bbbbbbbbbbbb" + "b" * 28
BASE = "a" * 40
VERSION = "c" * 40
EXPECTED = {"repository": REPOSITORY, "pr_number": 7, "head_sha": HEAD,
            "base_ref": "main", "head_ref": "fix/contract"}


def fixture(measured=False, reviews=False):
    body = body_with(MEASURED) if measured else valid_body()
    body = body.replace("Фактическая информация для раздела CI.", f"Текущий HEAD: `{HEAD}`; CI ещё не запускался.")
    manifest = {"schema_version": 1, **EXPECTED, "task": "Проверка инженерного контракта",
                "artifacts": [], "benchmarks": [], "reviews": []}
    details = (f"{manifest['task']}\n{REPOSITORY}\nHEAD: {HEAD}\n"
               f"https://github.com/{REPOSITORY}/pull/7\n\n"
               + "\n\n".join(f"## {h}\n{t}" for h, t in (
                   ("Назначение", "Обязательная пара инженерного резюме и подтверждений устраняет потерю результатов."),
                   ("Технические результаты", "Форматная проверка отделена от сетевой; полученные файлы сверяются с инвентарём."),
                   ("Проверки и источники", "tests.txt фиксирует запуск синтетических проверок текущей реализации и результат."),
                   ("Ограничения", "Инвентарь не доказывает истинность выводов; смысл проверяет человек по источникам."))))
    files = {"details.md": details, "tests.txt": "python3 -m unittest: 25 проверок успешно; синтетические независимые fixtures.\n"}

    def add(name, content, kind, commit=HEAD):
        files[name] = content
        manifest["artifacts"].append({"file": name, "sha256": sha256(content), "kind": kind,
                                      "commit_sha": commit, "source": "локальный исходный отчёт проверки"})

    add("details.md", details, "details")
    add("tests.txt", files["tests.txt"], "test-log")
    if measured:
        report = "anki-repo.validate: исходная серия 100 ms → 80 ms, n=10; ухудшений не выявлено."
        add("benchmark.md", report, "benchmark-report")
        manifest["benchmarks"].append({"scenario": "anki-repo.validate", "candidate_sha": HEAD,
                                        "baseline_sha": BASE, "report_file": "benchmark.md",
                                        "summary": MEASURED.split("### validation warm\n", 1)[1].strip(), "regressions": []})
    if reviews:
        body = body.replace(REPORT, "") + "\n\n" + COMPACT_CODERABBIT.replace(
            "3 завершённые итерации", "1 завершённая итерация").replace(
            "(итерация 3)", "(итерация 1)").replace("1234567abcde", HEAD[:12])
        reviewed = "abcdef012345" + "d" * 28
        log = ("| Итерация / reviewed HEAD | Уровень | Путь | Влияние | Решение | Исправление |\n"
               "| --- | --- | --- | --- | --- | --- |\n"
               f"| 1 · `{reviewed}` | major | src/run.rs | Ошибка проверки | confirmed | `{HEAD}` |\n")
        add("coderabbit-review.md", log, "review-log")
        manifest["reviews"] = [{"iteration": 1, "reviewed_sha": reviewed, "published_sha": HEAD, "rows": [log.splitlines()[-1]]}]
    files["manifest.json"] = json.dumps(manifest, ensure_ascii=False)
    return body, files, manifest


def snapshot(files):
    return {"id": GIST.rsplit("/", 1)[1], "html_url": GIST, "owner": {"login": "example"},
            "truncated": False, "history": [{"version": VERSION}],
            "files": {n: {"filename": n, "content": t, "size": len(t.encode()), "truncated": False}
                      for n, t in files.items()}}


def pr(body):
    return {"number": 7, "state": "open", "draft": False, "body": body,
            "head": {"sha": HEAD, "ref": "fix/contract"}, "user": {"login": "example"},
            "base": {"ref": "main", "repo": {"full_name": REPOSITORY}}}


class FakeAPI:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.paths = []

    def get(self, path):
        self.paths.append(path)
        response = next(self.responses)
        if isinstance(response, Exception):
            raise response
        return copy.deepcopy(response)


class GistContractTests(unittest.TestCase):
    def test_valid_small_pair_and_measured_pair(self):
        for measured, reviews in ((False, False), (True, False), (False, True), (True, True)):
            with self.subTest(measured=measured, reviews=reviews):
                body, files, manifest = fixture(measured, reviews)
                self.assertLess(len(body), 2500)
                validate_evidence(body, files, EXPECTED, manifest)
                api = FakeAPI([pr(body), snapshot(files), snapshot(files), pr(body)])
                check_remote(None, REPOSITORY, 7, api=api)
                self.assertEqual(api.paths[0], "/repos/example/toolkit/pulls/7")

    def test_original_failure_length_and_missing_gist(self):
        # Синтетическое воспроизведение: исходный пользовательский body не fixture.
        for include_gist in (False, True):
            base = valid_body(include_gist)
            text = base + "я" * (32493 - len(base))
            self.assertEqual(len(text), 32493)
            errors = validate(text)
            self.assertTrue(any("максимум 6000" in e for e in errors))
        text = valid_body(False)
        text += "я" * (3000 - len(text))
        self.assertTrue(any("Gist" in e for e in validate(text)))

    def test_unicode_lf_length_boundaries_in_cli_and_library(self):
        base = valid_body()
        text = base + "😀" * (6000 - len(base))
        self.assertEqual(validate(text), [])
        self.assertTrue(validate(text + "字"))
        self.assertEqual(validate(text.replace("\n", "\r\n")), [])
        self.assertEqual(validate(text.replace("\n", "\r")), [])
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "body.md"
            for suffix, code in (("", 0), ("字", 1)):
                path.write_bytes((text + suffix).replace("\n", "\r\n").encode())
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("check_pr_body.py")),
                                         "--body-file", str(path)], capture_output=True)
                self.assertEqual(result.returncode, code)

    def test_single_visible_gist_in_correct_section(self):
        body = valid_body()
        for fake in (f"`[отчёт]({GIST})`", f"<!-- [отчёт]({GIST}) -->", f"![отчёт]({GIST})",
                     f"\\[отчёт]({GIST})", f"```\n[отчёт]({GIST})\n```", GIST,
                     "[TODO](https://gist.github.com/example/" + "0" * 32 + ")"):
            with self.subTest(fake=fake):
                self.assertTrue(validate(body.replace(f"[проверки и технические результаты]({GIST})", fake)))
        self.assertTrue(validate(body + "\n" + f"[повтор]({GIST})"))
        self.assertTrue(validate(f"[повтор]({GIST})\n\n" + body))
        self.assertTrue(validate(body.replace("## Подробный отчёт", "## Неподходящее поле")))

    def test_missing_empty_unrelated_or_copy_details(self):
        body, files, _ = fixture()
        for name in ("details.md", "manifest.json", "tests.txt"):
            with self.subTest(name=name):
                broken = dict(files)
                del broken[name]
                with self.assertRaises(ContractError):
                    validate_evidence(body, broken, EXPECTED)
        for text in ("", "TODO", body, "Полезные подробности" * 100):
            with self.subTest(text=text[:10]), self.assertRaises(ContractError):
                validate_evidence(body, {**files, "details.md": text}, EXPECTED)

    def test_stale_or_foreign_manifest_and_local_inventory(self):
        body, files, manifest = fixture()
        for key, value in (("repository", "foreign/repo"), ("pr_number", 9), ("head_sha", BASE),
                           ("head_ref", "other"), ("base_ref", "release")):
            with self.subTest(key=key):
                broken = {**manifest, key: value}
                with self.assertRaises(ContractError):
                    validate_evidence(body, {**files, "manifest.json": json.dumps(broken)}, EXPECTED)
        with self.assertRaisesRegex(ContractError, "локального инвентаря"):
            validate_evidence(body, files, EXPECTED, {**manifest, "task": "other"})

    def test_real_local_sources_and_hashes(self):
        body, files, manifest = fixture()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, text in files.items():
                (root / name).write_bytes(text.encode())
            validate_evidence(body, files, EXPECTED, manifest, root)
            (root / "tests.txt").write_text("другой вывод")
            with self.assertRaisesRegex(ContractError, "локального исходника"):
                validate_evidence(body, files, EXPECTED, manifest, root)
        with self.assertRaisesRegex(ContractError, "SHA-256"):
            validate_evidence(body, {**files, "tests.txt": "повреждение"}, EXPECTED)

    def test_body_cannot_claim_stale_ci_head(self):
        body, files, manifest = fixture()
        with self.assertRaisesRegex(ContractError, "CI не привязан"):
            validate_evidence(body.replace(HEAD, BASE), files, EXPECTED)

    def test_no_current_head_evidence(self):
        body, files, manifest = fixture()
        manifest["artifacts"][1]["commit_sha"] = BASE
        files["manifest.json"] = json.dumps(manifest)
        with self.assertRaisesRegex(ContractError, "текущего HEAD"):
            validate_evidence(body, files, EXPECTED)

    def test_four_expected_benchmark_series_only_one_present(self):
        body, files, manifest = fixture(True)
        for number in range(2, 5):
            artifact = {**manifest["artifacts"][-1], "file": f"benchmark-{number}.md"}
            manifest["artifacts"].append(artifact)
            manifest["benchmarks"].append({**manifest["benchmarks"][0], "report_file": artifact["file"]})
        files["manifest.json"] = json.dumps(manifest)
        with self.assertRaisesRegex(ContractError, "отсутствующий"):
            validate_evidence(body, files, EXPECTED, manifest)

    def test_missing_series_index_summary_and_regression(self):
        body, files, manifest = fixture(True)
        regression = "RSS +15 % (наблюдаемое ухудшение)"
        manifest["benchmarks"][0]["regressions"] = [regression]
        files["manifest.json"] = json.dumps(manifest)
        with self.assertRaisesRegex(ContractError, "регрессия"):
            validate_evidence(body, files, EXPECTED)
        body, files, manifest = fixture(True)
        manifest["benchmarks"] = []
        files["manifest.json"] = json.dumps(manifest)
        with self.assertRaises(ContractError):
            validate_evidence(body, files, EXPECTED)
        body, files, manifest = fixture(True)
        with self.assertRaisesRegex(ContractError, "сводки"):
            validate_evidence(body.replace("80 ms", "70 ms"), files, EXPECTED)

    def test_review_requires_complete_accumulative_history(self):
        body, files, manifest = fixture(reviews=True)
        for missing in ("", "только последняя итерация", files["coderabbit-review.md"].replace("1 ·", "3 ·")):
            with self.subTest(missing=missing[:12]):
                broken = {**files, "coderabbit-review.md": missing}
                m = copy.deepcopy(manifest)
                m["artifacts"][-1]["sha256"] = sha256(missing)
                broken["manifest.json"] = json.dumps(m)
                with self.assertRaises(ContractError):
                    validate_evidence(body, broken, EXPECTED)
        with self.assertRaises(ContractError):
            validate_evidence(body.replace("1 завершённая", "3 завершённые"), files, EXPECTED)

    def test_truncated_partial_wrong_identity_and_version_fail_closed(self):
        _, files, _ = fixture()
        good = snapshot(files)
        variants = []
        for key, value in (("id", "d" * 32), ("html_url", GIST + "/"), ("owner", {"login": "other"}),
                           ("history", []), ("truncated", True), ("files", {})):
            variants.append({**good, key: value})
        for key, value in (("truncated", True), ("content", "prefix"), ("size", None), ("filename", "other.md")):
            broken = copy.deepcopy(good)
            broken["files"]["details.md"][key] = value
            variants.append(broken)
        for variant in variants:
            with self.subTest(variant=str(variant)[:60]), self.assertRaises(ContractError):
                gist_files(variant, GIST)

    def test_missing_gist_network_failures_and_races(self):
        body, files, _ = fixture()
        body += "я" * (3000 - len(body))
        self.assertEqual(len(body), 3000)
        good = snapshot(files)
        changed = {**good, "history": [{"version": BASE}]}
        for response in (ContractError("HTTP 404"), ContractError("HTTP 403"), ContractError("rate limit"),
                         ContractError("сеть недоступна"), {"message": "Not Found"}, snapshot({})):
            with self.subTest(response=str(response)), self.assertRaises(ContractError):
                check_remote(None, REPOSITORY, 7, FakeAPI([pr(body), response]))
        with self.assertRaisesRegex(ContractError, "гонка"):
            check_remote(None, REPOSITORY, 7, FakeAPI([pr(body), good, changed]))
        for changed_pr in ({**pr(body), "head": {"sha": BASE, "ref": "fix/contract"}},
                           {**pr(body), "body": body + " changed"}, {**pr(body), "draft": True}):
            with self.subTest(changed_pr=changed_pr["head"]), self.assertRaisesRegex(ContractError, "изменились"):
                check_remote(None, REPOSITORY, 7, FakeAPI([pr(body), good, good, changed_pr]))
        with self.assertRaisesRegex(ContractError, "после события"):
            check_remote(None, REPOSITORY, 7, FakeAPI([pr(body)]), event_head=BASE)

    def test_dependency_exception_uses_live_verified_identity(self):
        dependency = {**pr(""), "user": {"login": "dependabot[bot]"},
                      "head": {"sha": HEAD, "ref": "dependabot/rust"}}
        api = FakeAPI([dependency])
        check_remote(None, REPOSITORY, 7, api)
        self.assertEqual(len(api.paths), 1)
        dependency["user"] = {"login": "human"}
        with self.assertRaises(ContractError):
            check_remote(None, REPOSITORY, 7, FakeAPI([dependency]))

    def test_new_pr_preflight_without_fictitious_pr_number(self):
        body, files, manifest = fixture()
        manifest["pr_number"] = None
        files["manifest.json"] = json.dumps(manifest)
        expected = {**EXPECTED, "pr_number": None}
        branch = {"commit": {"sha": HEAD}}
        api = FakeAPI([branch, snapshot(files), snapshot(files), branch])
        check_unpublished(body, expected, api)
        self.assertEqual(api.paths[0], "/repos/example/toolkit/branches/fix%2Fcontract")
        with self.assertRaisesRegex(ContractError, "ожидаемому HEAD"):
            check_unpublished(body, expected, FakeAPI([{"commit": {"sha": BASE}}]))

    def test_migration_preserves_abbreviated_original_review_rows(self):
        body, files, manifest = fixture(reviews=True)
        review = manifest["reviews"][0]
        short_log = files["coderabbit-review.md"].replace(review["reviewed_sha"], review["reviewed_sha"][:7]).replace(HEAD, HEAD[:12])
        files["coderabbit-review.md"] = short_log
        review["rows"] = [short_log.splitlines()[-1]]
        manifest["artifacts"][-1]["sha256"] = sha256(short_log)
        files["manifest.json"] = json.dumps(manifest)
        validate_evidence(body, files, EXPECTED)

    def test_review_cannot_drop_a_finding_within_an_iteration(self):
        body, files, manifest = fixture(reviews=True)
        extra = manifest["reviews"][0]["rows"][0].replace("Ошибка проверки", "Вторая ошибка")
        manifest["reviews"][0]["rows"].append(extra)
        files["manifest.json"] = json.dumps(manifest)
        with self.assertRaisesRegex(ContractError, "потеряны строки"):
            validate_evidence(body, files, EXPECTED)

    def test_declared_regression_must_remain_in_both_artifacts(self):
        body, files, manifest = fixture(True)
        regression = "RSS +15 %"
        body = body.replace("существенных ухудшений не обнаружено по проверенным метрикам.", regression)
        benchmark = manifest["benchmarks"][0]
        benchmark["summary"] = benchmark["summary"].replace("существенных ухудшений не обнаружено по проверенным метрикам.", regression)
        benchmark["regressions"] = [regression]
        files["benchmark.md"] += " " + regression
        manifest["artifacts"][-1]["sha256"] = sha256(files["benchmark.md"])
        files["manifest.json"] = json.dumps(manifest)
        validate_evidence(body, files, EXPECTED)
        with self.assertRaisesRegex(ContractError, "регрессия|сводки"):
            validate_evidence(body.replace(regression, "улучшение"), files, EXPECTED)

    def test_http_errors_bad_json_partial_response_no_redirect(self):
        api = GitHubAPI()
        for error in (urllib.error.HTTPError("url", 403, "Forbidden", {}, None),
                      urllib.error.HTTPError("url", 404, "Not Found", {}, None),
                      urllib.error.HTTPError("url", 429, "Rate limit", {}, None), OSError("network"),
                      http.client.IncompleteRead(b"partial", 100)):
            with patch.object(api.opener, "open", side_effect=error), self.assertRaises(ContractError):
                api.get("/gists/" + "a" * 32)
        for raw in (b'{"id":', b'[]', b'\xff'):
            with patch.object(api.opener, "open", return_value=io.BytesIO(raw)), self.assertRaises(ContractError):
                api.get("/gists/" + "a" * 32)
        with self.assertRaisesRegex(ContractError, "redirect"):
            NoRedirect().redirect_request(None, None, 302, "Found", {}, "https://other.example")


if __name__ == "__main__":
    unittest.main()
