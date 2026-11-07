#!/usr/bin/env python3
"""Регрессия границ SonarQube по tracked-путям, без чтения пользовательских данных.

Это модель области конфигурации, а не запуск SonarScanner: она не учитывает
настройки сервера, языковые фильтры и другие параметры запуска анализатора.
"""

import re
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def read_properties(path):
    """Прочитать используемый здесь поднабор Java properties, отказывая при неоднозначности."""
    properties = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith(("#", "!")):
            continue
        if line.endswith("\\") or "=" not in line:
            raise ValueError(f"Неподдерживаемая строка properties: {raw!r}")
        key, value = (part.strip() for part in line.split("=", 1))
        if key in properties:
            raise ValueError(f"Повторное свойство: {key}")
        properties[key] = value
    return properties


def sonar_match(pattern, path):
    """Sonar wildcards: * и ? внутри компонента, ** для нуля и более каталогов."""
    parts = pattern.split("/")
    expressions = []
    for index, part in enumerate(parts):
        if part == "**":
            expressions.append(".*" if index == len(parts) - 1 else "(?:[^/]+/)*")
        else:
            if "**" in part:
                raise ValueError(f"Неподдерживаемый составной **: {pattern!r}")
            component = "".join(
                "[^/]*" if char == "*" else "[^/]" if char == "?" else re.escape(char)
                for char in part
            )
            expressions.append(component + ("/" if index < len(parts) - 1 else ""))
    return re.fullmatch("".join(expressions), path) is not None


def patterns(properties, key):
    return [part.strip() for part in properties.get(key, "").split(",") if part.strip()]


def in_roots(path, roots):
    return any(root == "." or path == root or path.startswith(root.rstrip("/") + "/")
               for root in roots)


def matches_any(path, globs):
    return any(sonar_match(pattern, path) for pattern in globs)


def scope(properties, path):
    """Рассчитать локальные source/test-фильтры как отдельные множества."""
    source = (in_roots(path, patterns(properties, "sonar.sources"))
              and not matches_any(path, patterns(properties, "sonar.exclusions")))
    inclusions = patterns(properties, "sonar.test.inclusions")
    test = (in_roots(path, patterns(properties, "sonar.tests"))
            and (not inclusions or matches_any(path, inclusions))
            and not matches_any(path, patterns(properties, "sonar.test.exclusions")))
    return source, test


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


if __name__ == "__main__":
    unittest.main()
