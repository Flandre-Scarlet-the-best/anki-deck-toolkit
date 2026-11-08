"""Контракт покрытия: идентичность CI и запрет обеих публикаций после отказа."""

import itertools
import json
import os
import re
import subprocess
import unittest
from pathlib import Path

from test_ci_runner_routing import Workflow, evaluate_value, truthy


def coverage_steps():
    path = Path(__file__).resolve().parents[1] / "workflows/sonarqube.yml"
    steps = Workflow(path.read_text(encoding="utf-8")).jobs["analyze"]["steps"]
    return {line.strip().split(": ", 1)[1]: (index, step)
            for index, step in enumerate(steps) for line in step["lines"]
            if line.strip().startswith("id: ")}


def gate_script():
    _, gate = coverage_steps()["ci_gate"]
    start = gate["lines"].index("          script: |") + 1
    return "\n".join(line[12:] for line in gate["lines"][start:])


class CoverageWorkflowTest(unittest.TestCase):
    def test_publications_require_successful_tests_validation_and_same_sha_ci(self):
        steps = coverage_steps()
        ordered = ("python_coverage", "coverage", "lcov", "ci_gate", "sonar",
                   "codecov_install", "codecov_upload")
        self.assertEqual(sorted(ordered, key=lambda key: steps[key][0]), list(ordered))
        test = "\n".join(steps["coverage"][1]["lines"])
        self.assertNotIn("--ignore-run-fail", test)
        self.assertNotIn("continue-on-error:", test)
        self.assertIn("--file lcov.info --file python-coverage.xml",
                      "\n".join(steps["codecov_upload"][1]["lines"]))
        for key in ("sonar", "codecov_install", "codecov_upload"):
            lines = steps[key][1]["lines"]
            self.assertNotIn("continue-on-error:", "\n".join(lines))
            expression = next(line.split("if:", 1)[1].strip()
                              for line in lines if line.strip().startswith("if:"))
            self.assertIn("success()", expression)
            for previous_success in (False, True):
                for outcomes in itertools.product(("failure", "success", "skipped", "cancelled"), repeat=4):
                    contexts = {"steps": {name: {"outcome": outcome} for name, outcome in
                                           zip(ordered[:4], outcomes)}}
                    value = evaluate_value(expression.replace("success()", str(previous_success).lower()), contexts)
                    self.assertEqual(truthy(value), previous_success and all(o == "success" for o in outcomes))

    def test_cache_is_separate_and_does_not_disable_profile_cleanup(self):
        steps = coverage_steps()
        cache = "\n".join(steps["rust_cache"][1]["lines"])
        self.assertIn("shared-key: llvm-cov-v1", cache)
        self.assertIn("workspaces: . -> target/llvm-cov-target", cache)
        self.assertNotIn("cache-workspace-crates: true", cache)
        command = "\n".join(steps["coverage"][1]["lines"])
        for unsafe in ("--no-clean", "--no-report", "--no-run", "--ignore-run-fail"):
            self.assertNotIn(unsafe, command)
        self.assertIn("cargo llvm-cov --workspace --locked --profile ci --no-fail-fast --lcov", command)

    def run_gate(self, scenario, script=None):
        driver = r"""
            const assert = require('node:assert/strict');
            const scenario = JSON.parse(process.argv[1]);
            const script = process.argv[2];
            let polls = 0, jobsCalls = 0;
            const base = {id: 10, html_url: 'url', status: 'completed', conclusion: 'success',
              head_sha: 'tested-sha', event: (scenario.event === 'workflow_dispatch' ? 'push' : scenario.event || 'pull_request'),
              repository: {full_name: 'owner/repo'},
              pull_requests: [{number: 12, head: {sha: 'tested-sha', repo: {id: 123}}}]};
            const github = {rest: {actions: {
              listWorkflowRuns: async args => {
                assert.deepEqual(args, {owner: 'owner', repo: 'repo', workflow_id: 'ci.yml',
                  head_sha: 'tested-sha', event: (scenario.event === 'workflow_dispatch' ? 'push' : scenario.event || 'pull_request'), per_page: 100});
                if (scenario.apiError) throw new Error('API недоступен');
                const state = scenario.poll[Math.min(polls++, scenario.poll.length - 1)];
                return {data: {workflow_runs: state.map(run => ({...base, ...run}))}};
              },
              listJobsForWorkflowRun: () => {},
            }}, paginate: async (method, args) => {
              assert.equal(method, github.rest.actions.listJobsForWorkflowRun);
              assert.deepEqual(args, {owner: 'owner', repo: 'repo', run_id: 10, per_page: 100});
              jobsCalls++;
              return scenario.jobs ?? [{name: 'Код и тесты', status: 'completed', conclusion: 'success'}];
            }};
            const context = {repo: {owner: 'owner', repo: 'repo'},
              eventName: scenario.event || 'pull_request', issue: {number: 12},
              payload: {repository: {id: 123}}};
            const core = {info: () => {}};
            const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
            new AsyncFunction('github', 'context', 'core', script)(github, context, core)
              .then(() => console.log(JSON.stringify({ok: true, polls, jobsCalls})))
              .catch(error => {
                if (error.code === 'ERR_ASSERTION') { console.error(error); process.exit(2); }
                console.log(JSON.stringify({ok: false, polls, jobsCalls}));
              });
        """
        environment = {**os.environ, "COVERAGE_SHA": "tested-sha", "CI_GATE_BUDGET_MS": "20",
                       "CI_GATE_POLL_MS": "1"}
        result = subprocess.run(["node", "-e", driver, json.dumps(scenario), script or gate_script()],
                                check=False, env=environment, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_ci_identity_completion_and_failure_paths(self):
        cases = [
            ("успех PR", {"poll": [[{}]]}, True),
            ("успех push", {"poll": [[{}]], "event": "push"}, True),
            ("ручной запуск ждёт push CI", {"poll": [[{}]], "event": "workflow_dispatch"}, True),
            ("нет запуска", {"poll": [[]]}, False),
            ("ошибка API", {"poll": [[]], "apiError": True}, False),
            ("отказ CI", {"poll": [[{"conclusion": "failure"}]]}, False),
            ("отмена CI", {"poll": [[{"conclusion": "cancelled"}]]}, False),
            ("пропуск CI", {"poll": [[{"conclusion": "skipped"}]]}, False),
            ("чужой SHA", {"poll": [[{"head_sha": "foreign-sha"}]]}, False),
            ("чужое событие", {"poll": [[{"event": "push"}]]}, False),
            ("чужой репозиторий", {"poll": [[{"repository": {"full_name": "other/repo"}}]]}, False),
            ("чужой PR", {"poll": [[{"pull_requests": [{"number": 13}]}]]}, False),
            ("нет PR", {"poll": [[{"pull_requests": []}]]}, False),
            ("чужая ревизия PR", {"poll": [[{"pull_requests": [
                {"number": 12, "head": {"sha": "old", "repo": {"id": 123}}}]}]]}, False),
            ("чужой исходный репозиторий PR", {"poll": [[{"pull_requests": [
                {"number": 12, "head": {"sha": "tested-sha", "repo": {"id": 456}}}]}]]}, False),
            ("бюджет ожидания", {"poll": [[{"status": "in_progress", "conclusion": None}]]}, False),
            ("очередь", {"poll": [[{"status": "queued", "conclusion": None}]]}, False),
            ("нет задания тестов", {"poll": [[{}]], "jobs": []}, False),
            ("другое задание", {"poll": [[{}]], "jobs": [
                {"name": "Другое", "status": "completed", "conclusion": "success"}]}, False),
        ]
        for outcome in ("failure", "skipped", "cancelled", None):
            cases.append((f"задание тестов: {outcome}", {"poll": [[{}]], "jobs": [
                {"name": "Код и тесты", "status": "completed", "conclusion": outcome}]}, False))
        cases.append(("задание ещё выполняется", {"poll": [[{}]], "jobs": [
            {"name": "Код и тесты", "status": "in_progress", "conclusion": "success"}]}, False))
        for title, scenario, expected in cases:
            with self.subTest(title=title):
                result = self.run_gate(scenario)
                self.assertEqual(result["ok"], expected)
                if expected:
                    self.assertEqual(result["jobsCalls"], 1)

    def test_gate_finds_tests_job_by_its_actual_name_in_ci(self):
        path = Path(__file__).resolve().parents[1] / "workflows/ci.yml"
        name = Workflow(path.read_text(encoding="utf-8")).jobs["code"]["keys"]["name"]
        script = gate_script()
        # Барьер ищет задание по отображаемому имени: оно обязано совпадать с ci.yml.
        self.assertIn(f"job.name === {json.dumps(name, ensure_ascii=False)}", script.replace("'", '"'))
        ok = {"poll": [[{}]], "jobs": [{"name": name, "status": "completed", "conclusion": "success"}]}
        self.assertTrue(self.run_gate(ok)["ok"])
        renamed = {"poll": [[{}]], "jobs": [{"name": name + " (старое)", "status": "completed",
                                               "conclusion": "success"}]}
        self.assertFalse(self.run_gate(renamed)["ok"])

    def test_pending_then_success_and_latest_failed_run(self):
        result = self.run_gate({"poll": [[{"status": "in_progress", "conclusion": None}], [{}]]})
        self.assertTrue(result["ok"])
        self.assertEqual(result["polls"], 2)
        result = self.run_gate({"poll": [[{"conclusion": "failure"}, {"id": 9}]]})
        self.assertFalse(result["ok"])
        self.assertEqual(result["jobsCalls"], 0)


class ToolBootstrapWorkflowTest(unittest.TestCase):
    def test_verified_bootstrap_precedes_unchanged_action_only_on_cloud_runner(self):
        path = Path(__file__).resolve().parents[1] / "workflows/ci.yml"
        workflow = Workflow(path.read_text(encoding="utf-8"))
        pins = set()
        for job_name in ("code", "asset-store-extended"):
            with self.subTest(job=job_name):
                steps = workflow.jobs[job_name]["steps"]
                index = next(i for i, step in enumerate(steps) if any(
                    "uses: Mozilla-Actions/sccache-action@" in line for line in step["lines"]))
                action = "\n".join(steps[index]["lines"])
                self.assertIn("Mozilla-Actions/sccache-action@fc920bf0ec8de6ee65d409111f7ec508035751ba", action)
                self.assertIn("version: ${{ env.SCCACHE_VERSION }}", action)
                bootstrap = "\n".join(steps[index - 1]["lines"])
                self.assertIn("fetch_verified_tool.py", bootstrap)
                self.assertIn('--sha256 "$SCCACHE_SHA256" --size "$SCCACHE_SIZE"', bootstrap)
                self.assertIn("$RUNNER_TOOL_CACHE/sccache/${SCCACHE_VERSION#v}/x64", bootstrap)
                self.assertNotIn("continue-on-error", bootstrap + action)
                pins.add(tuple(re.findall(r"^\s+SCCACHE_(?:SHA256|SIZE): (\S+)$", bootstrap, re.M)))
                condition = next(line.split("if:", 1)[1].strip() for line in steps[index - 1]["lines"]
                                 if line.strip().startswith("if:"))
                for runner, expected in (("github-hosted", True), ("self-hosted", False)):
                    self.assertEqual(truthy(evaluate_value("${{ " + condition + " }}", {"runner": {"environment": runner}})), expected)
        # Размер остаётся в env шага ради бюджета загрузки, поэтому копии обоих заданий
        # обязаны совпадать: обновление версии с одной забытой копией ловится здесь.
        self.assertEqual(len(pins), 1, pins)
        self.assertEqual(len(next(iter(pins))), 2)


class SharedToolPinsTest(unittest.TestCase):
    def test_ffmpeg_and_hyperfine_pins_match_between_workflows(self):
        # Копии закреплений в двух workflow обновляются вместе: расхождение ловится здесь.
        pattern = re.compile(r"^\s+((?:FFMPEG|HYPERFINE)_[A-Z0-9]+|key: ffmpeg-[^\s]+)(?:: (\S+))?$", re.M)
        pins = {}
        for workflow in ("ci.yml", "sonarqube.yml"):
            text = (Path(__file__).resolve().parents[1] / "workflows" / workflow).read_text(encoding="utf-8")
            pins[workflow] = sorted(pattern.findall(text))
        self.assertEqual(len(pins["ci.yml"]), 8, pins["ci.yml"])
        self.assertEqual(pins["ci.yml"], pins["sonarqube.yml"])


if __name__ == "__main__":
    unittest.main()
