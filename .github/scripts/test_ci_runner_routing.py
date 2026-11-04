"""Маршрутизация заданий ci.yml и бюджет загрузки инструментов при пустом кэше.

Тест не копирует формулу выбора исполнителя: он читает фактические `if`,
`runs-on` и `timeout-minutes` каждого задания из `.github/workflows/ci.yml`
(с разрешением YAML-якорей) и вычисляет их интерпретатором подмножества
выражений GitHub Actions: литералы, контексты, `!`, сравнения, `&&`/`||`
с возвратом операнда и `fromJSON`. Семантика сравнения — как у GitHub:
строки сравниваются без учёта регистра, при разных типах операнды
приводятся к числу (пустая строка и `null` — 0, нечисловая строка — NaN).

Что этим не доказано: фактическое поведение платформы (наполнение событий,
`updated_at`, `triggering_actor` при повторном запуске) — его фиксирует
контракт GitHub и ручная проверка, описанная в
`.codex/context/03-VERIFICATION.md`.
"""

import json
import math
import re
import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "ci.yml"

LOCAL = ["self-hosted", "home-wsl-x64"]
CLOUD = "ubuntu-latest"


# --- Интерпретатор подмножества выражений GitHub Actions ---------------------

TOKEN = re.compile(r"""\s*(?:
    (?P<str>'(?:[^']|'')*')
  | (?P<num>\d+(?:\.\d+)?)
  | (?P<op>==|!=|<=|>=|&&|\|\||[<>!().,\[\]])
  | (?P<id>[A-Za-z_][A-Za-z0-9_-]*)
)""", re.X)


def tokenize(text):
    tokens, position = [], 0
    text = text.strip()
    while position < len(text):
        match = TOKEN.match(text, position)
        if not match or match.end() == position:
            raise ValueError(f"не разобрано выражение с позиции {position}: {text[position:]!r}")
        position = match.end()
        kind = match.lastgroup
        tokens.append((kind, match.group(kind)))
    return tokens


def to_number(value):
    if value is None:
        return 0.0
    if isinstance(value, bool):
        return 1.0 if value else 0.0
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        stripped = value.strip()
        if stripped == "":
            return 0.0
        try:
            return float(stripped)
        except ValueError:
            return math.nan
    return math.nan


def truthy(value):
    if value is None or value is False:
        return False
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return not (value == 0 or math.isnan(value))
    if isinstance(value, str):
        return value != ""
    return True


def same_kind(left, right):
    kinds = (type(None), bool, str)
    for kind in kinds:
        if isinstance(left, kind) and isinstance(right, kind):
            return True
    numeric = (int, float)
    return (isinstance(left, numeric) and not isinstance(left, bool)
            and isinstance(right, numeric) and not isinstance(right, bool))


def equals(left, right):
    if same_kind(left, right):
        if isinstance(left, str):
            return left.upper() == right.upper()
        return left == right
    if isinstance(left, (dict, list)) or isinstance(right, (dict, list)):
        return left is right
    return to_number(left) == to_number(right)


def compare(left, right, operator):
    if isinstance(left, str) and isinstance(right, str):
        left, right = left.upper(), right.upper()
    else:
        left, right = to_number(left), to_number(right)
        if math.isnan(left) or math.isnan(right):
            return False
    return {"<": left < right, ">": left > right, "<=": left <= right, ">=": left >= right}[operator]


def member(value, key):
    if isinstance(value, dict):
        for name, item in value.items():
            if name.lower() == str(key).lower():
                return item
    if isinstance(value, list) and isinstance(key, (int, float)):
        index = int(key)
        return value[index] if 0 <= index < len(value) else None
    return None


class Expression:
    FUNCTIONS = {"fromjson": lambda text: json.loads(text)}

    def __init__(self, text, contexts):
        self.tokens = tokenize(text)
        self.position = 0
        self.contexts = {name.lower(): value for name, value in contexts.items()}

    def evaluate(self):
        value = self.or_expr()
        if self.position != len(self.tokens):
            raise ValueError(f"лишние лексемы: {self.tokens[self.position:]}")
        return value

    def peek(self):
        return self.tokens[self.position] if self.position < len(self.tokens) else (None, None)

    def take(self, expected=None):
        token = self.peek()
        if expected is not None and token[1] != expected:
            raise ValueError(f"ожидалось {expected!r}, получено {token}")
        self.position += 1
        return token

    def or_expr(self):
        value = self.and_expr()
        while self.peek() == ("op", "||"):
            self.take()
            right = self.and_expr()
            value = value if truthy(value) else right
        return value

    def and_expr(self):
        value = self.equality()
        while self.peek() == ("op", "&&"):
            self.take()
            right = self.equality()
            value = right if truthy(value) else value
        return value

    def equality(self):
        value = self.comparison()
        while self.peek()[1] in ("==", "!="):
            operator = self.take()[1]
            result = equals(value, self.comparison())
            value = result if operator == "==" else not result
        return value

    def comparison(self):
        value = self.unary()
        while self.peek()[1] in ("<", ">", "<=", ">="):
            operator = self.take()[1]
            value = compare(value, self.unary(), operator)
        return value

    def unary(self):
        if self.peek() == ("op", "!"):
            self.take()
            return not truthy(self.unary())
        return self.postfix()

    def postfix(self):
        value = self.primary()
        while self.peek()[1] in (".", "["):
            if self.take()[1] == ".":
                value = member(value, self.take()[1])
            else:
                key = self.or_expr()
                self.take("]")
                value = member(value, key)
        return value

    def primary(self):
        kind, text = self.take()
        if kind == "str":
            return text[1:-1].replace("''", "'")
        if kind == "num":
            return float(text)
        if kind == "op" and text == "(":
            value = self.or_expr()
            self.take(")")
            return value
        if kind == "id":
            lowered = text.lower()
            if lowered in ("true", "false"):
                return lowered == "true"
            if lowered == "null":
                return None
            if self.peek() == ("op", "("):
                self.take()
                arguments = []
                while self.peek() != ("op", ")"):
                    arguments.append(self.or_expr())
                    if self.peek() == ("op", ","):
                        self.take()
                self.take(")")
                if lowered not in self.FUNCTIONS:
                    raise ValueError(f"функция {text} не поддержана интерпретатором теста")
                return self.FUNCTIONS[lowered](*arguments)
            if lowered not in self.contexts:
                raise ValueError(f"контекст {text} не задан сценарием")
            return self.contexts[lowered]
        raise ValueError(f"неожиданная лексема {text!r}")


def evaluate_value(raw, contexts, *, implicit_expression=False):
    """Значение ключа workflow: целиком `${{ }}`, неявное выражение `if` или литерал."""
    stripped = raw.strip()
    match = re.fullmatch(r"\$\{\{(.*)\}\}", stripped, re.S)
    if match:
        return Expression(match.group(1), contexts).evaluate()
    if "${{" in stripped:
        raise ValueError(f"интерполяция внутри строки не поддержана: {raw!r}")
    if implicit_expression:
        return Expression(stripped, contexts).evaluate()
    return stripped


# --- Чтение ci.yml без сторонних библиотек -----------------------------------

def indent_of(line):
    return len(line) - len(line.lstrip(" "))


class Workflow:
    """Минимальное чтение структуры заданий ci.yml: `if`, `runs-on`, тайм-ауты, шаги."""

    def __init__(self, text):
        self.lines = text.splitlines()
        self.anchors = {}
        self.jobs = self.read_jobs()

    def scalar(self, index):
        """Значение `ключ: значение` в строке `index` с поддержкой `>-`, `&якоря` и `*ссылки`."""
        line = self.lines[index]
        value = line.split(":", 1)[1].strip()
        anchor = None
        if value.startswith("&"):
            anchor, _, value = value.partition(" ")
            anchor = anchor[1:]
            value = value.strip()
        if value.startswith("*"):
            return self.anchors[value[1:]]
        if value in (">-", ">", "|", "|-"):
            base = indent_of(line)
            collected = []
            for following in self.lines[index + 1:]:
                if following.strip() and indent_of(following) <= base:
                    break
                collected.append(following.strip())
            value = "\n".join(collected).strip()
        elif len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        if anchor:
            self.anchors[anchor] = value
        return value

    def read_jobs(self):
        start = self.lines.index("jobs:")
        jobs, current = {}, None
        for index in range(start + 1, len(self.lines)):
            line = self.lines[index]
            if line and not line.startswith(" ") and not line.startswith("#"):
                break
            header = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
            if header:
                current = {"name": header.group(1), "keys": {}, "steps": [], "step": None}
                jobs[current["name"]] = current
                continue
            if current is None or not line.strip() or line.strip().startswith("#"):
                continue
            key = re.fullmatch(r"    ([a-z-]+):.*", line)
            if key:
                current["keys"][key.group(1)] = self.scalar(index)
                current["step"] = None
                continue
            if re.match(r"      - ", line):
                current["step"] = {"lines": [line], "timeout": None}
                current["steps"].append(current["step"])
                continue
            step = current["step"]
            if step is not None:
                step["lines"].append(line)
                if re.fullmatch(r"        timeout-minutes:.*", line):
                    step["timeout"] = self.scalar(index)
        return jobs


def contexts(event_name, *, event, actor="owner", triggering_actor=None, run_attempt="1",
             inputs=None, variables=None, ref_name="feature", runner_environment=None):
    github = {
        "event_name": event_name,
        "repository": "owner/repo",
        "repository_owner": "owner",
        "actor": actor,
        "triggering_actor": triggering_actor or actor,
        "run_attempt": run_attempt,
        "ref_name": ref_name,
        "event": event,
    }
    result = {"github": github, "inputs": inputs or {}, "vars": variables or {}}
    if runner_environment is not None:
        result["runner"] = {"environment": runner_environment}
    return result


EVENT_TIME = "2026-11-05T10:00:00Z"
LEASE_ACTIVE = "2026-11-05T10:10:00Z"
LEASE_EXPIRED = "2026-11-05T09:50:00Z"


def pull_request(*, author="owner", head_repo="owner/repo", updated_at=EVENT_TIME):
    return {
        "repository": {"default_branch": "main"},
        "pull_request": {
            "user": {"login": author},
            "head": {"repo": {"full_name": head_repo}},
            "updated_at": updated_at,
        },
    }


def pr(*, mode=None, lease=None, run_attempt="1", actor="owner", triggering_actor=None, **event):
    variables = {}
    if mode is not None:
        variables["CI_RUNNER_MODE"] = mode
    if lease is not None:
        variables["CI_AUTO_LOCAL_UNTIL"] = lease
    return contexts("pull_request", event=pull_request(**event), variables=variables,
                    run_attempt=run_attempt, actor=actor, triggering_actor=triggering_actor,
                    ref_name="28/merge")


def dispatch(runner, *, actor="owner", triggering_actor=None, run_attempt="1", mode="local"):
    return contexts("workflow_dispatch", event={"repository": {"default_branch": "main"}},
                    inputs={"runner": runner}, actor=actor, triggering_actor=triggering_actor,
                    run_attempt=run_attempt, variables={"CI_RUNNER_MODE": mode,
                                                        "CI_AUTO_LOCAL_UNTIL": LEASE_ACTIVE})


def branch_event(event_name, ref_name):
    return contexts(event_name, event={"repository": {"default_branch": "main"}},
                    ref_name=ref_name,
                    variables={"CI_RUNNER_MODE": "local", "CI_AUTO_LOCAL_UNTIL": "9999-12-31T00:00:00Z"})


SCENARIOS = {
    # Явный режим и Auto.
    "PR владельца, режим не задан, аренды нет": (pr(), "cloud"),
    "PR владельца, Auto, аренда действует": (pr(mode="auto", lease=LEASE_ACTIVE), "local"),
    "PR владельца, режим не задан, аренда действует": (pr(lease=LEASE_ACTIVE), "local"),
    "PR владельца, пустой режим, аренда действует": (pr(mode="", lease=LEASE_ACTIVE), "local"),
    "PR владельца, Auto, аренда истекла": (pr(mode="auto", lease=LEASE_EXPIRED), "cloud"),
    "PR владельца, Auto, пустая аренда": (pr(mode="auto", lease=""), "cloud"),
    "PR владельца, github, аренда действует": (pr(mode="github", lease=LEASE_ACTIVE), "cloud"),
    "PR владельца, неизвестный режим, аренда действует": (pr(mode="cloud", lease=LEASE_ACTIVE), "cloud"),
    "PR владельца, local без аренды": (pr(mode="local"), "local"),
    # Старый PR и повторный запуск после истечения аренды.
    "повторный запуск старого события, аренда новее события": (
        pr(mode="auto", lease=LEASE_ACTIVE, run_attempt="2"), "cloud"),
    "новый push в старый PR после истечения аренды": (
        pr(mode="auto", lease=LEASE_EXPIRED, updated_at="2026-11-05T11:00:00Z"), "cloud"),
    "повторный запуск владельцем в режиме local": (pr(mode="local", run_attempt="3"), "local"),
    # Инициаторы и авторы.
    "повторный запуск PR владельца другим пользователем": (
        pr(mode="local", run_attempt="2", triggering_actor="collaborator"), "cloud"),
    "повторный запуск PR соавтора владельцем": (
        pr(mode="local", run_attempt="2", actor="collaborator", triggering_actor="owner"), "cloud"),
    "push соавтора в ветку PR владельца": (pr(mode="local", actor="collaborator"), "cloud"),
    "PR другого автора из ветки репозитория": (
        pr(mode="local", author="collaborator", actor="collaborator"), "cloud"),
    "PR другого автора, повторно запущенный владельцем": (
        pr(mode="local", author="collaborator", actor="owner"), "cloud"),
    "PR из форка владельца": (pr(mode="local", head_repo="owner-alt/repo"), "cloud"),
    "PR из чужого форка": (
        pr(mode="local", lease=LEASE_ACTIVE, author="stranger", head_repo="stranger/repo",
           actor="stranger"), "cloud"),
    "PR бота": (
        pr(mode="local", lease=LEASE_ACTIVE, author="dependabot[bot]", actor="dependabot[bot]"),
        "cloud"),
    # Ручной запуск.
    "ручной local владельца": (dispatch("local"), "local"),
    "ручной github-hosted владельца": (dispatch("github-hosted"), "cloud"),
    "ручной local не владельца": (dispatch("local", actor="collaborator"), "cloud"),
    "повторный ручной local владельца": (dispatch("local", run_attempt="2"), "local"),
    "ручной local владельца, повторно запущенный другим": (
        dispatch("local", run_attempt="2", triggering_actor="collaborator"), "cloud"),
    "ручной local владельца при режиме github": (dispatch("local", mode="github"), "local"),
    # События без доверенного маршрута.
    "push в основную ветку": (branch_event("push", "main"), "cloud"),
    "push в рабочую ветку": (branch_event("push", "feature"), "skipped"),
    "schedule": (branch_event("schedule", "main"), "cloud"),
}


class RoutingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = Workflow(WORKFLOW.read_text(encoding="utf-8"))

    def route(self, job, scenario):
        condition = job["keys"].get("if")
        if condition is not None and not truthy(
                evaluate_value(condition, scenario, implicit_expression=True)):
            return "skipped"
        runs_on = evaluate_value(job["keys"]["runs-on"], scenario)
        if runs_on == LOCAL:
            return "local"
        if runs_on == CLOUD:
            return "cloud"
        self.fail(f"задание {job['name']}: неожиданный исполнитель {runs_on!r}")

    def test_jobs_are_found(self):
        self.assertGreaterEqual(len(self.workflow.jobs), 5)
        for job in self.workflow.jobs.values():
            self.assertIn("runs-on", job["keys"], job["name"])

    def test_every_job_routes_as_expected(self):
        for title, (scenario, expected) in SCENARIOS.items():
            for job in self.workflow.jobs.values():
                with self.subTest(scenario=title, job=job["name"]):
                    self.assertEqual(self.route(job, scenario), expected)

    def test_unknown_contexts_fail_loudly(self):
        with self.assertRaises(ValueError):
            evaluate_value("${{ secrets.TOKEN }}", {"github": {}})

    def test_interpreter_follows_github_comparison_rules(self):
        scenario = {"vars": {"A": "AUTO", "N": ""}}
        self.assertIs(evaluate_value("${{ vars.A == 'auto' }}", scenario), True)
        self.assertIs(evaluate_value("${{ vars.MISSING == '' }}", scenario), True)
        self.assertIs(evaluate_value("${{ vars.MISSING > '2026-01-01T00:00:00Z' }}", scenario), False)
        self.assertIs(evaluate_value("${{ vars.N > '2026-01-01T00:00:00Z' }}", scenario), False)
        self.assertEqual(evaluate_value("${{ vars.A && 'x' || 'y' }}", scenario), "x")
        self.assertEqual(evaluate_value("${{ vars.N && 'x' || 'y' }}", scenario), "y")
        self.assertIs(evaluate_value("${{ '1' == 1 }}", scenario), True)


# --- Бюджет загрузки при пустом кэше -----------------------------------------

# Самый медленный наблюдавшийся канал self-hosted раннера при первой загрузке
# FFmpeg; запас покрывает повторные попытки и распаковку.
SLOW_BYTES_PER_SECOND = 230 * 1024
MARGIN = 1.5


class ColdCacheBudgetTests(unittest.TestCase):
    """Модель, а не замер: закреплённый размер при медленном канале против тайм-аутов шага и задания."""

    @classmethod
    def setUpClass(cls):
        cls.workflow = Workflow(WORKFLOW.read_text(encoding="utf-8"))

    def fetch_steps(self):
        for job in self.workflow.jobs.values():
            for step in job["steps"]:
                text = "\n".join(step["lines"])
                if "fetch_verified_tool.py" in text:
                    yield job, step, text

    def minutes(self, raw, environment):
        value = evaluate_value(raw, {"runner": {"environment": environment}})
        return float(value)

    def test_every_fetch_has_pinned_size_within_budget(self):
        steps = list(self.fetch_steps())
        self.assertGreaterEqual(len(steps), 4)
        for job, step, text in steps:
            with self.subTest(job=job["name"], step=step["lines"][0].strip()):
                self.assertIn("--size", text)
                sizes = re.findall(r"^\s+[A-Z_]+_SIZE: (\d+)$", text, re.M)
                self.assertEqual(len(sizes), 1, "размер архива закрепляется в env шага")
                job_minutes = float(job["keys"]["timeout-minutes"])
                step_minutes = (self.minutes(step["timeout"], "self-hosted")
                                if step["timeout"] is not None else job_minutes)
                needed = int(sizes[0]) / SLOW_BYTES_PER_SECOND * MARGIN / 60
                self.assertLessEqual(needed, step_minutes)
                self.assertLess(step_minutes, job_minutes, "после загрузки остаётся время на работу")

    def test_former_three_minute_budget_would_not_fit_ffmpeg(self):
        ffmpeg = [text for _job, _step, text in self.fetch_steps() if "FFMPEG_SIZE" in text]
        self.assertEqual(len(ffmpeg), 1)
        size = int(re.search(r"FFMPEG_SIZE: (\d+)", ffmpeg[0]).group(1))
        self.assertGreater(size / SLOW_BYTES_PER_SECOND / 60, 3)


if __name__ == "__main__":
    unittest.main()
