#!/usr/bin/env python3
"""Регрессия границ SonarQube по tracked-путям, без чтения пользовательских данных.

Это модель области конфигурации, а не запуск SonarScanner: она не учитывает
настройки сервера, языковые фильтры и другие параметры запуска анализатора.
"""

import subprocess
import tempfile
import unittest
from pathlib import Path


from sonar_scope import codecov_ignore, coverage_excluded, read_properties, scope, sonar_match


ROOT = Path(__file__).resolve().parents[2]


def conventions(path):
    """Независимая классификация по именам, не по Sonar glob."""
    parts = path.split("/")
    name = parts[-1]
    in_src = "src" in parts[:-1]
    return {
        "tests directory": "tests" in parts[:-1],
        "tests.rs": in_src and name == "tests.rs",
        "*_tests.rs": in_src and name.endswith("_tests.rs"),
        "test_*.py": name.startswith("test_") and name.endswith(".py"),
        "*_test.cjs": name.endswith("_test.cjs"),
        "test_support.rs": in_src and name == "test_support.rs",
        "*_test_support.rs": in_src and name.endswith("_test_support.rs"),
        "test_ports.rs": in_src and name == "test_ports.rs",
    }


def excluded_data(path):
    parts = path.split("/")
    return "fixtures" in parts[:-1] or path.endswith((".otf", ".zlib"))


def tracked_paths():
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached"], cwd=ROOT,
        check=True, stdout=subprocess.PIPE,
    )
    return [part.decode("utf-8", errors="surrogateescape")
            for part in result.stdout.split(b"\0") if part]


class SonarGlobTest(unittest.TestCase):
    def test_ambiguous_properties_and_unsupported_globs_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sonar-project.properties"
            for contents in ("key=a\nkey=b\n", "missing-equals\n", "key=value\\\n"):
                with self.subTest(contents=contents), self.assertRaises(ValueError):
                    path.write_text(contents, encoding="utf-8")
                    read_properties(path)
            path.write_text("# комментарий\n! комментарий\n\n key = a=b \n", encoding="utf-8")
            self.assertEqual(read_properties(path), {"key": "a=b"})
        with self.assertRaises(ValueError):
            sonar_match("src/a**b.rs", "src/ab.rs")

    def test_wildcards_do_not_cross_directory_boundaries(self):
        for pattern, path, expected in [
            ("**/tests/**", "tests/check.rs", True),
            ("**/tests/**", "a/b/tests/check.rs", True),
            ("**/src/**/tests.rs", "src/tests.rs", True),
            ("**/src/**/tests.rs", "a/src/deep/nested/tests.rs", True),
            ("**/src/**/*_tests.rs", "a/src/item_tests.rs", True),
            ("**/src/**/*_tests.rs", "a/src/deep/item_tests.rs", True),
            ("**/test_*.py", "test_example.py", True),
            ("**/test_*.py", "a/test_example.py", True),
            ("**/test_*.py", "a/test_dir/example.py", False),
            ("a/*.rs", "a/deep/file.rs", False),
            ("a/?.rs", "a/xy.rs", False),
            ("**/*.otf", "font.otf", True),
        ]:
            with self.subTest(pattern=pattern, path=path):
                self.assertEqual(sonar_match(pattern, path), expected)


class SonarScopeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.properties = read_properties(ROOT / "sonar-project.properties")
        cls.paths = tracked_paths()

    def test_tracked_tree_matches_independent_conventions(self):
        source_controls = workflow_controls = 0
        for path in self.paths:
            if not path.startswith(("tools/", ".github/")):
                continue
            labels = conventions(path)
            excluded = excluded_data(path)
            generated = bool({"target", "node_modules"} & set(path.split("/")[:-1]))
            if excluded:
                expected = (False, False)
            elif generated:
                # Здесь проверяется только source exclusion; Sonar test exclusion
                # для generated-каталогов данной конфигурацией не задаётся.
                self.assertFalse(scope(self.properties, path)[0], path)
                continue
            elif any(labels.values()):
                expected = (False, True)
            else:
                expected = (True, False)
                if "/src/" in path and path.endswith(".rs"):
                    source_controls += 1
                if path.startswith(".github/workflows/") and path.endswith((".yml", ".yaml")):
                    workflow_controls += 1
            with self.subTest(path=path):
                actual = scope(self.properties, path)
                self.assertFalse(all(actual), f"Источник и тест пересекаются: {path}")
                self.assertEqual(actual, expected, path)
        self.assertGreater(source_controls, 0, "Нет контрольных исходников Rust")
        self.assertGreater(workflow_controls, 0, "Нет контрольных процессов GitHub Actions")
        self.assertEqual(scope(self.properties, "Cargo.toml"), (True, False))

    def test_synthetic_conventions_and_exclusions(self):
        # Случаи отдельно проверяют соглашения даже при их пересечении в дереве.
        for suffix in (
            "tests/example.rs", "src/tests.rs", "src/nested/tests.rs",
            "src/example_tests.rs", "src/nested/example_tests.rs",
            "test_example.py", "nested/example_test.cjs", "src/test_support.rs",
            "src/nested/example_test_support.rs", "src/test_ports.rs",
        ):
            path = "tools/synthetic/" + suffix
            with self.subTest(path=path):
                self.assertTrue(any(conventions(path).values()))
                self.assertEqual(scope(self.properties, path), (False, True))
        for suffix in (
            "fixtures/example.rs", "tests/fixtures/example.rs",
            "src/nested/fixtures/tests.rs", "tests/font.otf", "src/example.zlib",
        ):
            path = "tools/synthetic/" + suffix
            with self.subTest(path=path):
                self.assertTrue(excluded_data(path))
                self.assertEqual(scope(self.properties, path), (False, False))
        for suffix in ("src/lib.rs", "src/nested/module.rs", "scripts/helper.py"):
            path = "tools/synthetic/" + suffix
            with self.subTest(path=path):
                self.assertEqual(scope(self.properties, path), (True, False))


def e2e_scenario(path):
    """Независимое соглашение: файл сценария opt-in стенда anki-e2e."""
    if not (path.startswith("tools/anki-e2e/src/") and path.endswith(".rs")):
        return False
    parts = path.split("/")
    return parts[-1].endswith("scenarios.rs") or parts[3:5] == ["agent", "scenarios"]


class CoverageExclusionTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.properties = read_properties(ROOT / "sonar-project.properties")
        cls.paths = tracked_paths()

    def test_only_e2e_scenarios_are_excluded_from_coverage(self):
        excluded = set()
        production = 0
        for path in self.paths:
            if not path.startswith("tools/"):
                continue
            with self.subTest(path=path):
                self.assertEqual(coverage_excluded(self.properties, path), e2e_scenario(path), path)
            if coverage_excluded(self.properties, path):
                excluded.add(path)
                # Исключено только покрытие: файл остаётся исходником для анализа замечаний.
                self.assertEqual(scope(self.properties, path), (True, False), path)
            elif scope(self.properties, path)[0] and path.endswith(".rs"):
                production += 1
        # Вложенные сценарии и сценарии верхнего уровня обоих наборов стенда.
        for nested in ("tools/anki-e2e/src/agent/scenarios/input_export.rs",
                       "tools/anki-e2e/src/agent/scenarios/roundtrip.rs",
                       "tools/anki-e2e/src/agent/scenarios.rs",
                       "tools/anki-e2e/src/browser/scenarios.rs"):
            self.assertIn(nested, excluded)
        self.assertGreater(production, 100, "Производственные файлы Rust должны учитываться")

    def test_production_controls_remain_measured(self):
        for path in (
            "tools/anki-repo/src/desktop/input/publication.rs",
            "tools/anki-repo/src/desktop/input/prepare.rs",
            "tools/anki-repo/src/exchange/state.rs",
            "tools/anki-e2e/src/agent/ctx.rs",
            "tools/anki-e2e/src/agent/ui.rs",
            "tools/anki-e2e/src/host.rs",
            "tools/anki-e2e/src/main.rs",
        ):
            with self.subTest(path=path):
                self.assertIn(path, self.paths)
                self.assertFalse(coverage_excluded(self.properties, path))
                self.assertEqual(scope(self.properties, path), (True, False))

    def test_synthetic_boundaries(self):
        for path, expected in [
            ("tools/anki-e2e/src/agent/scenarios/deep/nested.rs", True),
            ("tools/anki-e2e/src/browser/new_scenarios.rs", True),
            ("tools/anki-e2e/src/agent/scenarios_helper.rs", False),
            ("tools/anki-e2e/src/agent/scenario.rs", False),
            ("tools/anki-repo/src/scenarios.rs", False),
            ("tools/anki-repo/src/desktop/scenarios/roundtrip.rs", False),
            ("tools/asset-store/src/agent/scenarios/x.rs", False),
        ]:
            with self.subTest(path=path):
                self.assertEqual(coverage_excluded(self.properties, path), expected)
                self.assertEqual(e2e_scenario(path), expected)

    def test_codecov_ignore_matches_sonar_and_has_no_other_settings(self):
        self.assertEqual(
            codecov_ignore(ROOT / "codecov.yml"),
            [part.strip() for part in self.properties["sonar.coverage.exclusions"].split(",")],
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "codecov.yml"
            for contents in (
                'coverage:\n  status:\n    patch: off\n',
                'ignore:\n  - "a"\ncomment: false\n',
                'ignore:\n  - unquoted\n',
                'ignore:\n  - "a"\nignore:\n  - "b"\n',
            ):
                with self.subTest(contents=contents), self.assertRaises(ValueError):
                    path.write_text(contents, encoding="utf-8")
                    codecov_ignore(path)


if __name__ == "__main__":
    unittest.main()
