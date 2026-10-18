import re
import unittest
from pathlib import Path


class UsageGuideTest(unittest.TestCase):
    def setUp(self):
        path = Path("docs/USAGE.md")
        self.assertTrue(path.is_file(), "docs/USAGE.md does not exist")
        self.text = path.read_text(encoding="utf-8")

    def test_it_has_a_title(self):
        self.assertRegex(self.text, r"(?m)^# \S")

    def test_every_option_is_explained_with_its_default(self):
        for option in ["--column", "--delimiter", "--skip-header"]:
            self.assertIn(option, self.text)
        self.assertRegex(self.text.lower(), r"default")

    def test_it_shows_an_example(self):
        blocks = re.findall(r"```[^\n]*\n(.*?)```", self.text, re.S)
        self.assertTrue(any("python3 csvsum.py" in b for b in blocks), "no fenced example runs python3 csvsum.py")

    def test_it_lists_the_exit_statuses(self):
        lower = self.text.lower()
        self.assertIn("exit", lower)
        for status in ["0", "1", "2"]:
            self.assertRegex(self.text, rf"(?<![\w.]){status}(?![\w.])")


if __name__ == "__main__":
    unittest.main()
