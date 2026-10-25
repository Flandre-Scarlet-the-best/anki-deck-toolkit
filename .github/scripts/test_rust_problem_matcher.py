"""Регрессионные тесты problem matcher rustc и rustfmt на образцах фактического вывода."""

import json
import re
import unittest
from pathlib import Path

MATCHER = Path(__file__).resolve().parents[1] / "rust-problem-matcher.json"

# Раннер GitHub перед сопоставлением снимает не ESC-последовательности, а префикс
# NUL + "33[" (в C# нет восьмеричных escape), поэтому при `CARGO_TERM_COLOR: always`
# matcher получает строки с настоящими ANSI-кодами и обязан их пропускать.
COLORED_WARNING = [
    "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `x`\x1b[0m",
    " \x1b[1m\x1b[94m--> \x1b[0msrc/main.rs:1:15",
]
COLORED_ERROR = [
    "\x1b[1m\x1b[91merror[E0425]\x1b[0m\x1b[1m: cannot find value `y` in this scope\x1b[0m",
    " \x1b[1m\x1b[94m--> \x1b[0msrc/main.rs:1:20",
]
PLAIN_WARNING = [
    "warning: unused variable: `x`",
    " --> src/main.rs:1:15",
]
RUSTFMT_DIFF = "Diff in /work/src/main.rs:1:"


def load_owner(owner):
    config = json.loads(MATCHER.read_text(encoding="utf-8"))
    for matcher in config["problemMatcher"]:
        if matcher["owner"] == owner:
            return matcher["pattern"]
    raise AssertionError(f"нет matcher {owner}")


def match_rustc(lines):
    header, location = load_owner("rustc")
    first = re.match(header["regexp"], lines[0])
    second = re.match(location["regexp"], lines[1])
    if first is None or second is None:
        return None
    return {
        "severity": first.group(header["severity"]),
        "code": first.group(header["code"]),
        "message": first.group(header["message"]),
        "file": second.group(location["file"]),
        "line": second.group(location["line"]),
        "column": second.group(location["column"]),
    }


class RustcMatcherTests(unittest.TestCase):
    def test_plain_warning(self):
        self.assertEqual(
            match_rustc(PLAIN_WARNING),
            {
                "severity": "warning",
                "code": None,
                "message": "unused variable: `x`",
                "file": "src/main.rs",
                "line": "1",
                "column": "15",
            },
        )

    def test_colored_warning_without_escape_codes_in_groups(self):
        self.assertEqual(
            match_rustc(COLORED_WARNING),
            {
                "severity": "warning",
                "code": None,
                "message": "unused variable: `x`",
                "file": "src/main.rs",
                "line": "1",
                "column": "15",
            },
        )

    def test_colored_error_with_code(self):
        result = match_rustc(COLORED_ERROR)
        self.assertIsNotNone(result)
        self.assertEqual(result["severity"], "error")
        self.assertEqual(result["code"], "E0425")
        self.assertEqual(result["message"], "cannot find value `y` in this scope")
        self.assertEqual(result["column"], "20")

    def test_cargo_status_line_is_not_a_diagnostic(self):
        header, _ = load_owner("rustc")
        self.assertIsNone(
            re.match(header["regexp"], "\x1b[1m\x1b[92m   Compiling\x1b[0m m v0.1.0 (/work)")
        )


class RustfmtMatcherTests(unittest.TestCase):
    def test_diff_header(self):
        (pattern,) = load_owner("rustfmt")
        found = re.match(pattern["regexp"], RUSTFMT_DIFF)
        self.assertIsNotNone(found)
        self.assertEqual(found.group(pattern["file"]), "/work/src/main.rs")
        self.assertEqual(found.group(pattern["line"]), "1")


if __name__ == "__main__":
    unittest.main()
