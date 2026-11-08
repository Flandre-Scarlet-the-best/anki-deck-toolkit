"""Проверить порядок coverage и запрет загрузки после отказа тестов."""

import json
import os
import subprocess
import unittest
from pathlib import Path

from test_ci_runner_routing import Workflow, evaluate_value, truthy


class CoverageWorkflowTest(unittest.TestCase):
    def test_codecov_runs_only_after_successful_tests_and_validation(self):
        path = Path(__file__).resolve().parents[1] / "workflows/sonarqube.yml"
        steps = Workflow(path.read_text(encoding="utf-8")).jobs["analyze"]["steps"]

        def locate(name):
            matches = [(i, s) for i, s in enumerate(steps)
                       if s["lines"][0].strip() == f"- name: {name}"]
            self.assertEqual(len(matches), 1, name)
            return matches[0]

        python_index, python = locate("Измерить покрытие тестов Python-скриптов CI")
        test_index, test = locate("Выполнить тесты workspace и собрать покрытие")
        check_index, check = locate("Проверить сформированный LCOV")
        scan_index, _ = locate("Запустить анализ SonarQube Cloud")
        gate_index, gate = locate("Дождаться основного CI и подтвердить его успех")
        install_index, install = locate("Установить проверенный Codecov CLI")
        upload_index, upload = locate("Отправить проверенное покрытие в Codecov")
        self.assertLess(python_index, test_index)
        self.assertIn("        id: python_coverage", python["lines"])
        self.assertLess(test_index, check_index)
        self.assertLess(check_index, scan_index)
        self.assertLess(scan_index, gate_index)
        self.assertLess(gate_index, install_index)
        self.assertIn("        id: ci_gate", gate["lines"])
        self.assertLess(install_index, upload_index)
        self.assertIn("        id: coverage", test["lines"])
        self.assertIn("        id: lcov", check["lines"])
        self.assertNotIn("--ignore-run-fail", "\n".join(test["lines"]))
        self.assertNotIn("continue-on-error:", "\n".join(test["lines"]))
        for step in (install, upload):
            expression = next(line.split("if:", 1)[1].strip()
                              for line in step["lines"] if line.strip().startswith("if:"))
            self.assertIn("success()", expression)
            for previous_success in (False, True):
                for coverage in ("failure", "success", "skipped"):
                    for lcov in ("failure", "success", "skipped"):
                        for ci_gate in ("failure", "success", "skipped"):
                            for python_coverage in ("failure", "success", "skipped"):
                                contexts = {"steps": {"coverage": {"outcome": coverage},
                                                      "lcov": {"outcome": lcov},
                                                      "python_coverage": {"outcome": python_coverage},
                                                      "ci_gate": {"outcome": ci_gate}}}
                                value = evaluate_value(expression.replace("success()", str(previous_success).lower()),
                                                       contexts)
                                self.assertEqual(truthy(value), previous_success and
                                                 coverage == lcov == ci_gate == python_coverage == "success")

    def test_ci_gate_waits_then_rejects_failed_or_unrelated_runs(self):
        path = Path(__file__).resolve().parents[1] / "workflows/sonarqube.yml"
        steps = Workflow(path.read_text(encoding="utf-8")).jobs["analyze"]["steps"]
        gate = next(s for s in steps if s["lines"][0].strip() ==
                    "- name: Дождаться основного CI и подтвердить его успех")
        start = gate["lines"].index("          script: |") + 1
        script = "\n".join(line[12:] for line in gate["lines"][start:])
        driver = r"""
            const scenario = JSON.parse(process.argv[1]);
            const script = process.argv[2];
            let poll = 0;
            const github = {rest: {actions: {
              listWorkflowRuns: async () => {
                const index = Math.min(poll++, scenario.poll.length - 1);
                const state = scenario.poll[index];
                return {data: {workflow_runs: [{id: 10, html_url: 'url', status: state.status,
                  conclusion: state.conclusion, pull_requests: [{number: scenario.pr}]}]}};
              },
              listJobsForWorkflowRun: () => {},
            }}, paginate: async () => [{name: 'Код и тесты', conclusion: scenario.tests}]};
            const context = {repo: {owner: 'owner', repo: 'repo'},
                             eventName: 'pull_request', issue: {number: 12}};
            const core = {info: () => {}};
            const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
            new AsyncFunction('github', 'context', 'core', script)(github, context, core)
              .then(() => process.exit(0)).catch(() => process.exit(1));
        """
        done, running = ("completed", "success"), ("in_progress", None)
        # Бюджет ожидания мал, чтобы проверка «сдаётся, если CI не завершился»
        # не занимала минуты; период опроса — 1 мс.
        cases = [
            ("успешный основной CI", [dict(status=done[0], conclusion=done[1])], "success", 12, 20000, 0),
            ("отказ основного CI", [dict(status="completed", conclusion="failure")], "failure", 12, 20000, 1),
            ("тестовый job пропущен", [dict(status=done[0], conclusion=done[1])], "skipped", 12, 20000, 1),
            ("чужой PR", [dict(status=done[0], conclusion=done[1])], "success", 13, 200, 1),
            ("ожидание завершения", [dict(status=running[0], conclusion=running[1]),
                                     dict(status=done[0], conclusion=done[1])], "success", 12, 20000, 0),
            ("бюджет исчерпан", [dict(status=running[0], conclusion=running[1])], "success", 12, 200, 1),
        ]
        for title, poll, tests, pr, budget, expected in cases:
            with self.subTest(title=title):
                scenario = json.dumps(dict(poll=poll, tests=tests, pr=pr))
                environment = {**os.environ, "CI_GATE_BUDGET_MS": str(budget),
                               "CI_GATE_POLL_MS": "1"}
                result = subprocess.run(["node", "-e", driver, scenario, script],
                                        check=False, env=environment)
                self.assertEqual(result.returncode, expected)

    def test_ci_gate_gives_up_on_stale_pending_run(self):
        path = Path(__file__).resolve().parents[1] / "workflows/sonarqube.yml"
        steps = Workflow(path.read_text(encoding="utf-8")).jobs["analyze"]["steps"]
        gate = next(s for s in steps if s["lines"][0].strip() ==
                    "- name: Дождаться основного CI и подтвердить его успех")
        start = gate["lines"].index("          script: |") + 1
        script = "\n".join(line[12:] for line in gate["lines"][start:])
        driver = r"""
            const script = process.argv[1];
            let polls = 0;
            const github = {rest: {actions: {
              listWorkflowRuns: async () => {
                polls++;
                return {data: {workflow_runs: [{id: 10, html_url: 'url', status: 'in_progress',
                  conclusion: null, pull_requests: [{number: 12}]}]}};
              },
              listJobsForWorkflowRun: () => {},
            }}, paginate: async () => []};
            const context = {repo: {owner: 'owner', repo: 'repo'},
                             eventName: 'pull_request', issue: {number: 12}};
            const core = {info: () => {}};
            const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
            new AsyncFunction('github', 'context', 'core', script)(github, context, core)
              .then(() => process.exit(0)).catch(() => process.exit(1));
        """
        environment = {**os.environ, "CI_GATE_BUDGET_MS": "0", "CI_GATE_POLL_MS": "1"}
        result = subprocess.run(["node", "-e", driver, script], check=False, env=environment)
        self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main()
