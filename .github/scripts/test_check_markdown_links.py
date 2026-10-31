"""Синтетические фикстуры проверки локальных ссылок Markdown."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from check_markdown_links import check_page, check_repository


class MarkdownLinkTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        for name in ("docs/ru/module/README.md", "docs/ru/module/notes.md", "docs/en/module/README.md"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("# Страница\n", encoding="utf-8")

    def tearDown(self):
        self.temp.cleanup()

    def errors(self, name: str, text: str) -> list[str]:
        page = self.root / name
        page.parent.mkdir(parents=True, exist_ok=True)
        page.write_text(text, encoding="utf-8")
        return check_page(self.root, page)

    def test_valid_relative_links_pass(self):
        text = (
            "[соседняя](notes.md), [с якорем](notes.md#раздел), [каталог](../module/),\n"
            "[от корня](/docs/ru/module/README.md), ![картинка](<notes.md>),\n"
            "[кодированная](%6Eotes.md?plain=1), [перенос\nтекста](notes.md)\n\n"
            "[ref]: ../../en/module/README.md\n"
        )
        self.assertEqual(self.errors("docs/ru/module/page.md", text), [])

    def test_missing_target_is_reported_with_file_line_and_address(self):
        errors = self.errors("docs/ru/module/page.md", "Текст.\n\nСм. [нет](missing.md#x).\n")
        self.assertEqual(len(errors), 1)
        self.assertIn("docs/ru/module/page.md:3", errors[0])
        self.assertIn("missing.md#x", errors[0])

    def test_wrapped_link_text_and_reference_definitions_are_checked(self):
        errors = self.errors(
            "docs/ru/module/page.md",
            "[текст\nссылки](../../performance/benchmarks.md)\n\n[r]: gone.md\n",
        )
        self.assertEqual(len(errors), 2, errors)
        self.assertIn(":2:", errors[0])
        self.assertIn(":4:", errors[1])

    def test_link_outside_repository_is_reported(self):
        errors = self.errors("README.md", "[наружу](../outside.md)\n")
        self.assertEqual(len(errors), 1)
        self.assertIn("за пределы репозитория", errors[0])

    def test_external_links_and_anchors_are_not_checked(self):
        text = "[сайт](https://example.com/x.md) [почта](mailto:a@b.c) [якорь](#раздел) [ftp](ftp://h/f)\n"
        self.assertEqual(self.errors("docs/ru/module/page.md", text), [])

    def test_missing_translation_is_not_required(self):
        # Китайского перевода нет; ссылка на русский оригинал допустима.
        text = "[оригинал](../../ru/module/notes.md)\n"
        self.assertEqual(self.errors("docs/zh-CN/module/README.md", text), [])
        self.assertFalse((self.root / "docs/zh-CN/module/notes.md").exists())

    def test_code_examples_are_not_links(self):
        text = (
            "Пример `[x](missing.md)` в коде.\n\n"
            "```markdown\n[y](missing.md)\n```\n\n"
            "~~~~\n[z](missing.md)\n~~~~\n"
        )
        self.assertEqual(self.errors("docs/ru/module/page.md", text), [])

    def test_multiline_code_span_is_not_a_link_and_keeps_line_numbers(self):
        text = "Пример `кода\n[x](missing.md)\n` здесь.\n[нет](gone.md)\n\nОдин `обратный\n\n[y](lost.md)\n"
        errors = self.errors("docs/ru/module/page.md", text)
        self.assertEqual(len(errors), 2, errors)
        self.assertIn(":4: цель ссылки не существует: gone.md", errors[0])
        # Пустая строка завершает абзац, поэтому одиночная кавычка не скрывает ссылку.
        self.assertIn(":8: цель ссылки не существует: lost.md", errors[1])

    def test_reference_definition_with_destination_on_next_line_is_checked(self):
        errors = self.errors("docs/ru/module/page.md", "[r]:\n   gone.md\n\n[ok]:\n  notes.md\n")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn(":2: цель ссылки не существует: gone.md", errors[0])

    def test_repository_check_reads_only_tracked_markdown(self):
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        (self.root / "docs/ru/module/README.md").write_text("[битая](gone.md)\n", encoding="utf-8")
        (self.root / "untracked.md").write_text("[битая](gone.md)\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(self.root), "add", "docs"], check=True)
        errors = check_repository(self.root)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("docs/ru/module/README.md:1", errors[0])


if __name__ == "__main__":
    unittest.main()
