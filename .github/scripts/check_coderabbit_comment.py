#!/usr/bin/env python3
"""Проверить опубликованные PR body и комментарий CodeRabbit через GitHub API."""

import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from check_pr_body import FENCE, FENCE_CLOSE, is_automation_pr, strip_comments, validate, validate_coderabbit

MARKER = "<!-- coderabbit-disposition -->"
API = "https://api.github.com"
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
SHA = re.compile(r"[0-9a-f]{40}\Z")
LINK = re.compile(r'\s*<([^<>]+)>\s*;\s*rel="([a-z]+)"\s*')


class CheckError(Exception):
    """Невозможно достоверно проверить опубликованное состояние."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    # Токен никогда не отправляется на иной адрес после перенаправления.
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def positive_id(value):
    return type(value) is int and value > 0


def get_json(path, token):
    request = urllib.request.Request(API + path, headers={
        "Accept": "application/vnd.github+json",
        "Authorization": "Bearer " + token,
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "pr-disposition-validator",
    })
    try:
        with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
            if response.status != 200:
                raise CheckError("GitHub API вернул неожиданный HTTP status")
            data = json.load(response)
            link = response.headers.get("Link")
    except (OSError, ValueError, UnicodeError) as exc:
        # Не выводить URL ответа, тела и токен из исключений.
        raise CheckError("Ошибка запроса или JSON GitHub API") from exc
    return data, link


def next_page(link, path, current):
    """Проверить Link, не переходя по недоверенному URL с токеном."""
    if link is None:
        return None
    links = link.split(",")
    relations = {}
    for item in links:
        match = LINK.fullmatch(item)
        if not match or match.group(2) in relations:
            raise CheckError("Некорректный заголовок пагинации GitHub API")
        relations[match.group(2)] = match.group(1)
    if "next" not in relations:
        return None
    url = urllib.parse.urlsplit(relations["next"])
    try:
        params = urllib.parse.parse_qs(url.query, strict_parsing=True)
    except ValueError as exc:
        raise CheckError("Некорректная ссылка следующей страницы") from exc
    resource = re.fullmatch(r"/repos/[^/]+/[^/]+(/issues/[1-9][0-9]*/comments)", path)
    # GitHub может вернуть каноническую ссылку /repositories/{id}/... вместо
    # /repos/{owner}/{repo}/.... По ссылке не переходим: следующий запрос всё
    # равно строится из проверенного исходного path.
    canonical = resource and re.fullmatch(r"/repositories/[1-9][0-9]*" + re.escape(resource.group(1)), url.path)
    if (url.scheme != "https" or url.netloc != "api.github.com" or (url.path != path and not canonical)
            or url.fragment or set(params) != {"page", "per_page"}
            or params["page"] != [str(current + 1)] or params["per_page"] != ["100"]):
        raise CheckError("Недопустимая ссылка следующей страницы")
    return current + 1


def list_comments(repository, number, token):
    path = f"/repos/{repository}/issues/{number}/comments"
    page = 1
    comments = []
    ids = set()
    while True:
        if page > 10000:
            raise CheckError("Превышен предел страниц комментариев")
        data, link = get_json(f"{path}?per_page=100&page={page}", token)
        if not isinstance(data, list) or len(data) > 100:
            raise CheckError("Некорректная страница комментариев GitHub API")
        for comment in data:
            if not isinstance(comment, dict) or not positive_id(comment.get("id")):
                raise CheckError("Некорректный ID комментария GitHub API")
            if comment["id"] in ids:
                raise CheckError("Повторный ID комментария при пагинации")
            ids.add(comment["id"])
            user = comment.get("user")
            if not isinstance(user, dict) or not positive_id(user.get("id")) or not isinstance(comment.get("body"), str):
                raise CheckError("Некорректные автор или тело комментария GitHub API")
            comments.append(comment)
        following = next_page(link, path, page)
        if following is None:
            # Страница ровно на 100 записей может быть последней; запросить ещё
            # одну, чтобы не пропустить записи при отсутствующем Link.
            if len(data) < 100:
                return comments
            following = page + 1
        if not data:
            raise CheckError("Пустая страница содержит ссылку продолжения")
        page = following


def visible_comment(body):
    """Оставить только видимый Markdown вне fenced-блоков и HTML-комментариев."""
    lines = strip_comments(body.replace("\r\n", "\n").replace("\r", "\n").split("\n"))
    result = []
    char = ""
    length = 0
    html_block = False
    raw_tag = ""
    for line in lines:
        if raw_tag:
            if re.search(rf"</{raw_tag}\s*>", line, re.IGNORECASE):
                raw_tag = ""
            result.append("")
            continue
        if html_block:
            if not line.strip():
                html_block = False
            result.append("")
            continue
        if char:
            close = FENCE_CLOSE.match(line)
            if close and close.group(1)[0] == char and len(close.group(1)) >= length:
                char = ""
            result.append("")
        elif opening := FENCE.match(line):
            char, length = opening.group(1)[0], len(opening.group(1))
            result.append("")
        elif tag := re.match(r"^ {0,3}<(?P<closing>/)?(?P<name>[A-Za-z][A-Za-z0-9-]*)(?:\s|/?>)", line):
            name = tag.group("name").lower()
            if name in {"script", "pre", "style", "textarea"} and not tag.group("closing"):
                if not re.search(rf"</{name}\s*>", line, re.IGNORECASE):
                    raw_tag = name
            else:
                html_block = True
            result.append("")
        else:
            result.append(line)
    return "\n".join(result)


def validate_comments(comments, author_id):
    marked = [comment for comment in comments if MARKER in comment["body"]]
    if len(marked) > 1:
        return ["Найдено несколько служебных комментариев CodeRabbit"]
    if not marked:
        return []
    comment = marked[0]
    if comment["user"]["id"] != author_id:
        return ["Автор служебного комментария CodeRabbit не является автором PR"]
    body = comment["body"].replace("\r\n", "\n").replace("\r", "\n")
    if body.count(MARKER) != 1 or MARKER not in body.split("\n"):
        return ["Маркер CodeRabbit должен встречаться один раз отдельной строкой"]
    # Проверить, что маркер не вложен в иной HTML-комментарий или код-блок.
    sentinel = "CODERABBIT_MARKER_LOCATION_SENTINEL"
    marker_line = body.split("\n").index(MARKER)
    if visible_comment(body.replace(MARKER, sentinel)).split("\n")[marker_line] != sentinel:
        return ["Маркер CodeRabbit скрыт внутри комментария или блока кода"]
    return validate_coderabbit(visible_comment(body))


def run(env):
    repository = env.get("GITHUB_REPOSITORY", "")
    number_text = env.get("PR_NUMBER", "")
    token = env.get("GITHUB_TOKEN", "")
    event_path = env.get("GITHUB_EVENT_PATH", "")
    if (not REPOSITORY.fullmatch(repository) or any(part in {".", ".."} for part in repository.split("/"))
            or not re.fullmatch(r"[1-9][0-9]{0,14}", number_text)):
        raise CheckError("Некорректные GITHUB_REPOSITORY или PR_NUMBER")
    if not token or not event_path or any(not 33 <= ord(c) <= 126 for c in token):
        raise CheckError("Отсутствуют или некорректны GITHUB_TOKEN или GITHUB_EVENT_PATH")
    try:
        event = json.loads(Path(event_path).read_text(encoding="utf-8"))
        event_pr = event["pull_request"]
        event_repo = event["repository"]["full_name"]
        event_number = event_pr["number"]
        event_sha = event_pr["head"]["sha"]
    except (OSError, UnicodeError, ValueError, KeyError, TypeError) as exc:
        raise CheckError("Некорректный файл события pull_request") from exc
    number = int(number_text)
    if (event_repo != repository or not positive_id(event_number) or event_number != number
            or not isinstance(event_sha, str) or not SHA.fullmatch(event_sha)):
        raise CheckError("Номер, репозиторий или HEAD события некорректны")
    pr, _ = get_json(f"/repos/{repository}/pulls/{number}", token)
    try:
        author = pr["user"]
        author_id = author["id"]
        login = author["login"]
        head = pr["head"]
        branch = head["ref"]
        sha = head["sha"]
        body = pr["body"]
        pr_number = pr["number"]
    except (KeyError, TypeError) as exc:
        raise CheckError("Некорректный объект PR из GitHub API") from exc
    if (not positive_id(pr_number) or pr_number != number or not positive_id(author_id)
            or not isinstance(login, str) or not login or not isinstance(branch, str) or not branch
            or not isinstance(sha, str) or not SHA.fullmatch(sha)
            or (body is not None and not isinstance(body, str))):
        raise CheckError("Некорректные поля PR из GitHub API")
    if sha != event_sha:
        raise CheckError("HEAD PR изменился после события: требуется новая проверка")
    problems = [] if is_automation_pr({"pull_request": pr}) else validate(body or "")
    comments = list_comments(repository, number, token)
    problems.extend(validate_comments(comments, author_id))
    # PR мог измениться во время постраничного чтения комментариев; не выдавать
    # результат на основе смешанных версий опубликованных метаданных и body.
    current, _ = get_json(f"/repos/{repository}/pulls/{number}", token)
    try:
        before = (pr["number"], pr["user"]["id"], pr["user"]["login"], pr["head"]["sha"], pr["head"]["ref"], pr["body"])
        after = (current["number"], current["user"]["id"], current["user"]["login"], current["head"]["sha"], current["head"]["ref"], current["body"])
    except (KeyError, TypeError) as exc:
        raise CheckError("Некорректный PR при повторном чтении") from exc
    if not positive_id(after[0]) or not positive_id(after[1]) or after != before:
        raise CheckError("PR изменился во время проверки: запустите её повторно")
    return problems


def safe_message(message):
    # Не позволять данным из PR сформировать workflow command или управляющие строки.
    return "".join(c if ord(c) >= 32 and ord(c) != 127 else " " for c in str(message))


def main():
    try:
        problems = run(os.environ)
    except CheckError as exc:
        print("Не удалось проверить PR: " + safe_message(exc), file=sys.stderr)
        return 2
    if problems:
        for problem in problems:
            print("Ошибка: " + safe_message(problem), file=sys.stderr)
        return 1
    print("Опубликованные PR body и комментарий CodeRabbit соответствуют контракту")
    return 0


if __name__ == "__main__":
    sys.exit(main())
