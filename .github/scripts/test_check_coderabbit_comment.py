"""Проверки опубликованных данных PR, пагинации и недоверенного Markdown."""

import copy
import http.client
import io
import json
import tempfile
import traceback
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch

import check_coderabbit_comment as check
from test_check_pr_body import CR_DELIMITER, CR_HEADER, CR_ROW, valid_body


SHA = "a" * 40
REPO = "example/project"
NUMBER = 7
MARKED = "\n".join((check.MARKER, "## CodeRabbit review и disposition", CR_HEADER, CR_DELIMITER, CR_ROW))


def comment(identifier, author=1, body=MARKED):
    return {"id": identifier, "user": {"id": author, "login": "any-name"}, "body": body}


def pr():
    return {"number": NUMBER, "user": {"id": 1, "login": "author"},
            "head": {"sha": SHA, "ref": "feature/example"}, "body": valid_body()}


def event():
    return {"repository": {"full_name": REPO},
            "pull_request": {"number": NUMBER, "head": {"sha": SHA}, "body": "устаревшее body"}}


def link(page, host="api.github.com", path=None):
    path = path or f"/repos/{REPO}/issues/{NUMBER}/comments"
    return f'<https://{host}{path}?per_page=100&page={page}>; rel="next"'


class CommentTests(unittest.TestCase):
    def test_absent_valid_and_accumulated(self):
        self.assertEqual(check.validate_comments([], 1), [])
        extra = "| 2 · `abcdef1` | major | `a.rs` | эффект | confirmed | `abcdef2` |"
        same_iteration = "| 2 · `abcdef1` | minor | `b.rs` | эффект | false positive | — |"
        self.assertEqual(check.validate_comments([comment(1, body=MARKED + "\n" + extra + "\n" + same_iteration)], 1), [])

    def test_duplicate_and_author_id_not_login(self):
        self.assertTrue(check.validate_comments([comment(1), comment(2)], 1))
        self.assertTrue(check.validate_comments([comment(1, author=2)], 1))
        self.assertTrue(check.validate_comments([comment(1), comment(2, author=2)], 1))

    def test_marked_comment_requires_valid_author_id(self):
        for user in (None, {}, {"id": True}, {"id": 0}, {"id": "1"}, {"id": 2}):
            marked = comment(1)
            marked["user"] = user
            with self.subTest(user=user):
                self.assertTrue(check.validate_comments([marked], 1))

    def test_bad_marker_and_invisible_table(self):
        for body in (MARKED + "\n" + check.MARKER,
                     "```\n" + MARKED + "\n```",
                     "<!--\n" + check.MARKER + "\n-->\n" + MARKED.replace(check.MARKER, ""),
                     "text " + MARKED,
                     check.MARKER + "\n```\n" + "\n".join((CR_HEADER, CR_DELIMITER, CR_ROW)) + "\n```",
                     check.MARKER + "\n<!--\n" + "\n".join((CR_HEADER, CR_DELIMITER, CR_ROW)) + "\n-->",
                     check.MARKER + "\n<div>\n" + "\n".join((CR_HEADER, CR_DELIMITER, CR_ROW)) + "\n</div>",
                     check.MARKER + "\n<script>\n\n" + "\n".join((CR_HEADER, CR_DELIMITER, CR_ROW)) + "\n</script>"):
            with self.subTest(body=body):
                self.assertTrue(check.validate_comments([comment(1, body=body)], 1))

    def test_bad_table(self):
        for body in (check.MARKER, MARKED.replace(CR_DELIMITER, "| --- | --- |"),
                     MARKED.replace("clean", ""), MARKED + "\n\n" + CR_ROW,
                     MARKED + "\n\n| 2 · `ab` | minor | path | impact | confirmed | fix |",
                     MARKED + "\n\n| повреждённая | строка |",
                     MARKED + "\n\n" + CR_HEADER + "\n" + CR_DELIMITER + "\n" + CR_ROW):
            with self.subTest(body=body):
                self.assertTrue(check.validate_comments([comment(1, body=body)], 1))


class PaginationTests(unittest.TestCase):
    @patch.object(check, "get_json")
    def test_unrelated_null_user_on_same_or_different_pages(self, get):
        ordinary = {"id": 2, "user": None, "body": "Обычный комментарий"}
        for pages in (([ordinary, comment(1)],), ([ordinary], [comment(1)]),
                      ([comment(1)], [ordinary])):
            with self.subTest(pages=pages):
                get.reset_mock()
                get.side_effect = [(page, link(index + 2) if index + 1 < len(pages) else None)
                                   for index, page in enumerate(pages)]
                comments = check.list_comments(REPO, NUMBER, "token")
                self.assertEqual(len(comments), 2)
                self.assertEqual(check.validate_comments(comments, 1), [])
                self.assertEqual(get.call_count, len(pages))

    @patch.object(check, "get_json")
    def test_duplicates_on_different_pages_rejected_before_author_check(self, get):
        other = comment(2, author=2)
        get.side_effect = [([comment(1)], link(2)), ([other], None)]
        comments = check.list_comments(REPO, NUMBER, "token")
        self.assertEqual(check.validate_comments(comments, 1),
                         ["Найдено несколько служебных комментариев CodeRabbit"])

    @patch.object(check, "get_json")
    def test_follows_next_even_short_page_and_finds_duplicate(self, get):
        get.side_effect = [([comment(1)], link(2)), ([comment(2)], None)]
        comments = check.list_comments(REPO, NUMBER, "token")
        self.assertEqual(len(comments), 2)
        self.assertTrue(check.validate_comments(comments, 1))
        self.assertIn("page=2", get.call_args_list[1].args[0])

    @patch.object(check, "get_json")
    def test_canonical_repository_id_link_fetches_original_repository(self, get):
        get.side_effect = [([comment(1)], link(2, path=f"/repositories/123/issues/{NUMBER}/comments")),
                           ([comment(2, body="другой комментарий")], None)]
        self.assertEqual(len(check.list_comments(REPO, NUMBER, "token")), 2)
        self.assertEqual(get.call_args_list[1].args[0], f"/repos/{REPO}/issues/{NUMBER}/comments?per_page=100&page=2")

    @patch.object(check, "get_json")
    def test_full_page_without_link_fetches_next(self, get):
        get.side_effect = [([comment(i, body="other") for i in range(1, 101)], None), ([comment(101)], None)]
        self.assertEqual(len(check.list_comments(REPO, NUMBER, "token")), 101)
        self.assertEqual(get.call_count, 2)

    @patch.object(check, "get_json")
    def test_malformed_and_duplicate_comment_fail_closed(self, get):
        for response in [({}, None), ([{"id": 1}], None), ([comment(1), comment(1)], None),
                         ([comment(1, body=None)], None)]:
            with self.subTest(response=response):
                get.return_value = response
                with self.assertRaises(check.CheckError):
                    check.list_comments(REPO, NUMBER, "token")

    @patch.object(check, "get_json")
    def test_untrusted_links_and_errors_fail_closed(self, get):
        for value in (link(2, host="attacker.invalid"), link(3), link(2, path="/wrong"),
                      link(2, path="/repositories/0/issues/7/comments"),
                      link(2, path="/repositories/123/issues/8/comments"), "bad", link(2) + ", " + link(2)):
            with self.subTest(link=value):
                get.return_value = ([comment(1)], value)
                with self.assertRaises(check.CheckError):
                    check.list_comments(REPO, NUMBER, "token")
        get.side_effect = check.CheckError("Ошибка API")
        with self.assertRaises(check.CheckError):
            check.list_comments(REPO, NUMBER, "token")


class RequestTests(unittest.TestCase):
    class Response(io.BytesIO):
        status = 200
        headers = {}

    @patch("urllib.request.build_opener")
    def test_request_headers_and_decode(self, opener):
        opener.return_value.open.return_value = self.Response(b'{"ok": true}')
        self.assertEqual(check.get_json("/test", "secret"), ({"ok": True}, None))
        request = opener.return_value.open.call_args.args[0]
        self.assertEqual(request.full_url, "https://api.github.com/test")
        self.assertEqual(request.get_header("Authorization"), "Bearer secret")
        self.assertIs(opener.call_args.args[0], check.NoRedirect)
        self.assertIsNone(check.NoRedirect().redirect_request(None, None, 302, "", {}, "https://attacker.invalid"))

    @staticmethod
    def http_error(code):
        return urllib.error.HTTPError("https://example.invalid/secret", code, "secret", {}, io.BytesIO(b"secret"))

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_transient_failures_then_success(self, opener, sleep):
        for failure in (self.http_error(500), self.http_error(503),
                        urllib.error.URLError("unavailable"), TimeoutError(), OSError("network")):
            with self.subTest(failure=failure):
                opener.return_value.open.reset_mock(side_effect=True)
                sleep.reset_mock()
                opener.return_value.open.side_effect = [failure, self.Response(b'{"ok": true}')]
                self.assertEqual(check.get_json("/test", "secret"), ({"ok": True}, None))
                self.assertEqual(opener.return_value.open.call_count, 2)
                sleep.assert_called_once_with(0.1)
                if isinstance(failure, urllib.error.HTTPError):
                    self.assertTrue(failure.closed)

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_http_protocol_failures_retry_without_exposing_response(self, opener, sleep):
        for failure in (http.client.IncompleteRead(b"secret"),
                        http.client.BadStatusLine("secret"), http.client.LineTooLong("secret")):
            with self.subTest(failure=type(failure).__name__):
                opener.return_value.open.reset_mock(side_effect=True)
                sleep.reset_mock()
                opener.return_value.open.side_effect = [failure, self.Response(b'{}')]
                self.assertEqual(check.get_json("/test", "token"), ({}, None))
                self.assertEqual(opener.return_value.open.call_count, 2)
                sleep.assert_called_once_with(0.1)
                opener.return_value.open.reset_mock(side_effect=True)
                sleep.reset_mock()
                opener.return_value.open.side_effect = [failure, failure, failure]
                with self.assertRaises(check.CheckError) as caught:
                    check.get_json("/test", "token")
                self.assertNotIn("secret", "".join(traceback.format_exception(caught.exception)))
                self.assertEqual(opener.return_value.open.call_count, 3)
                self.assertEqual([call.args for call in sleep.call_args_list], [(0.1,), (0.2,)])

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_incomplete_http_response_read_retries_and_closes(self, opener, sleep):
        class IncompleteResponse(self.Response):
            def read(self, *args):
                raise http.client.IncompleteRead(b"secret")

        response = IncompleteResponse(b'')
        opener.return_value.open.side_effect = [response, self.Response(b'{}')]
        self.assertEqual(check.get_json("/test", "token"), ({}, None))
        self.assertTrue(response.closed)
        self.assertEqual(opener.return_value.open.call_count, 2)
        sleep.assert_called_once_with(0.1)

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_transport_failure_while_reading_retries_and_closes_response(self, opener, sleep):
        class BrokenResponse(self.Response):
            def read(self, *args):
                raise TimeoutError("secret")

        response = BrokenResponse(b'')
        opener.return_value.open.side_effect = [response, self.Response(b'{}')]
        self.assertEqual(check.get_json("/test", "secret"), ({}, None))
        self.assertTrue(response.closed)
        self.assertEqual(opener.return_value.open.call_count, 2)
        sleep.assert_called_once_with(0.1)

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_persistent_failures_are_bounded_and_safe(self, opener, sleep):
        for failures in ([self.http_error(503) for _ in range(3)],
                         [urllib.error.URLError("secret") for _ in range(3)],
                         [TimeoutError("secret") for _ in range(3)],
                         [OSError("secret") for _ in range(3)]):
            with self.subTest(failure=type(failures[0]).__name__):
                opener.return_value.open.reset_mock(side_effect=True)
                sleep.reset_mock()
                opener.return_value.open.side_effect = failures
                with self.assertRaises(check.CheckError) as caught:
                    check.get_json("/test", "secret")
                self.assertNotIn("secret", "".join(traceback.format_exception(caught.exception)))
                self.assertEqual(opener.return_value.open.call_count, 3)
                self.assertEqual([call.args for call in sleep.call_args_list], [(0.1,), (0.2,)])
                for failure in failures:
                    if isinstance(failure, urllib.error.HTTPError):
                        self.assertTrue(failure.closed)

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_http_4xx_and_redirect_fail_without_retry(self, opener, sleep):
        for code in (302, 400, 403, 404, 429):
            with self.subTest(code=code):
                opener.return_value.open.reset_mock(side_effect=True)
                error = self.http_error(code)
                opener.return_value.open.side_effect = error
                with self.assertRaises(check.CheckError) as caught:
                    check.get_json("/test", "secret")
                self.assertNotIn("secret", "".join(traceback.format_exception(caught.exception)))
                self.assertTrue(error.closed)
                self.assertEqual(opener.return_value.open.call_count, 1)
        sleep.assert_not_called()

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_invalid_json_and_unicode_fail_without_retry(self, opener, sleep):
        for body in (b'bad-json secret', b'"\xff secret"'):
            with self.subTest(body=body):
                opener.return_value.open.reset_mock(side_effect=True)
                response = self.Response(body)
                opener.return_value.open.return_value = response
                with self.assertRaises(check.CheckError) as caught:
                    check.get_json("/test", "secret")
                self.assertNotIn("secret", "".join(traceback.format_exception(caught.exception)))
                self.assertTrue(response.closed)
                self.assertEqual(opener.return_value.open.call_count, 1)
        sleep.assert_not_called()

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_non_200_response_statuses(self, opener, sleep):
        response = self.Response(b'')
        response.status = 503
        opener.return_value.open.side_effect = [response, self.Response(b'{}')]
        self.assertEqual(check.get_json("/test", "secret"), ({}, None))
        self.assertTrue(response.closed)
        self.assertEqual(opener.return_value.open.call_count, 2)
        sleep.assert_called_once_with(0.1)
        opener.return_value.open.reset_mock(side_effect=True)
        sleep.reset_mock()
        response = self.Response(b'')
        response.status = 302
        opener.return_value.open.return_value = response
        with self.assertRaises(check.CheckError):
            check.get_json("/test", "secret")
        self.assertTrue(response.closed)
        self.assertEqual(opener.return_value.open.call_count, 1)
        sleep.assert_not_called()


class PublishedPrTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.event_path = Path(self.tmp.name) / "event.json"
        self.event_path.write_text(json.dumps(event()), encoding="utf-8")
        self.env = {"GITHUB_REPOSITORY": REPO, "PR_NUMBER": str(NUMBER), "GITHUB_TOKEN": "token", "GITHUB_EVENT_PATH": str(self.event_path)}

    def execute(self, actual, comments=None):
        responses = [(actual, None), (comments or [], None), (copy.deepcopy(actual), None)]
        with patch.object(check, "get_json", side_effect=responses):
            return check.run(self.env)

    def test_actual_body_not_event_and_absent_comment(self):
        self.assertEqual(self.execute(pr()), [])
        actual = pr()
        actual["body"] = "некорректно"
        self.assertTrue(self.execute(actual))

    def test_bot_exception_uses_actual_login_and_branch(self):
        actual = pr()
        actual["body"] = ""
        actual["user"]["login"] = "renovate[bot]"
        actual["head"]["ref"] = "renovate/example"
        self.assertEqual(self.execute(actual), [])
        actual["head"]["ref"] = "feature/fake"
        self.assertTrue(self.execute(actual))

    def test_head_and_identity_are_validated(self):
        for mutate in (lambda p: p["head"].update(sha="b" * 40), lambda p: p["user"].update(id=True),
                       lambda p: p.update(number=8), lambda p: p.update(body=[]), lambda p: p.update(user=None)):
            actual = pr()
            mutate(actual)
            with self.subTest(actual=actual), self.assertRaises(check.CheckError):
                self.execute(actual)
        invalid = copy.deepcopy(event())
        invalid["repository"]["full_name"] = "other/project"
        self.event_path.write_text(json.dumps(invalid), encoding="utf-8")
        with self.assertRaises(check.CheckError):
            self.execute(pr())

    def test_comment_and_api_failure_propagate(self):
        self.assertTrue(self.execute(pr(), [comment(1, author=2)]))
        marked_without_user = comment(1)
        marked_without_user["user"] = None
        self.assertTrue(self.execute(pr(), [marked_without_user]))
        ordinary_without_user = {"id": 2, "user": None, "body": "Обычный комментарий"}
        self.assertEqual(self.execute(pr(), [ordinary_without_user, comment(1)]), [])
        self.assertEqual(self.execute(pr(), [ordinary_without_user]), [])
        self.assertEqual(self.execute(pr(), [comment(1)]), [])
        with patch.object(check, "get_json", side_effect=[(pr(), None), check.CheckError("API")]):
            with self.assertRaises(check.CheckError):
                check.run(self.env)

    @patch.object(check.time, "sleep")
    @patch("urllib.request.build_opener")
    def test_protocol_failure_main_returns_sanitized_error(self, opener, sleep):
        opener.return_value.open.side_effect = http.client.BadStatusLine("secret")
        stderr = io.StringIO()
        with patch.dict(check.os.environ, self.env, clear=True), patch("sys.stderr", stderr):
            self.assertEqual(check.main(), 2)
        self.assertEqual(opener.return_value.open.call_count, 3)
        self.assertEqual(sleep.call_count, 2)
        self.assertNotIn("secret", stderr.getvalue())
        self.assertIn("Не удалось проверить PR", stderr.getvalue())

    def test_pr_changes_during_pagination_are_rejected(self):
        changed = pr()
        changed["head"]["sha"] = "b" * 40
        with patch.object(check, "get_json", side_effect=[(pr(), None), ([], None), (changed, None)]):
            with self.assertRaises(check.CheckError):
                check.run(self.env)
        changed = pr()
        changed["body"] = "изменилось"
        with patch.object(check, "get_json", side_effect=[(pr(), None), ([], None), (changed, None)]):
            with self.assertRaises(check.CheckError):
                check.run(self.env)

    def test_missing_environment_fails(self):
        for key in self.env:
            env = dict(self.env)
            del env[key]
            with self.subTest(key=key), self.assertRaises(check.CheckError):
                check.run(env)


if __name__ == "__main__":
    unittest.main()
