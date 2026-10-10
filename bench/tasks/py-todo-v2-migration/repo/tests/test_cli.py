import contextlib
import io
import json
import os
import tempfile
import unittest

from todo.cli import main


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = os.path.join(self.tmp.name, "todo.json")

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = main(["--file", self.path, *argv])
            except SystemExit as exc:  # argparse usage errors
                code = exc.code
        return code, out.getvalue(), err.getvalue()

    def test_add_and_list(self):
        self.assertEqual(self.run_cli("add", "Buy milk"), (0, "added 1\n", ""))
        self.assertEqual(self.run_cli("add", "Call the bank"), (0, "added 2\n", ""))
        code, out, _ = self.run_cli("list")
        self.assertEqual(code, 0)
        self.assertEqual(out, "[ ] 1 Buy milk\n[ ] 2 Call the bank\n")

    def test_list_on_a_missing_file(self):
        self.assertEqual(self.run_cli("list"), (0, "", ""))
        self.assertFalse(os.path.exists(self.path))

    def test_done_hides_the_item_unless_all(self):
        self.run_cli("add", "Buy milk")
        self.run_cli("add", "Call the bank")
        self.assertEqual(self.run_cli("done", "1"), (0, "done 1\n", ""))
        self.assertEqual(self.run_cli("list")[1], "[ ] 2 Call the bank\n")
        self.assertEqual(self.run_cli("list", "--all")[1], "[x] 1 Buy milk\n[ ] 2 Call the bank\n")

    def test_remove(self):
        self.run_cli("add", "Buy milk")
        self.run_cli("add", "Call the bank")
        self.assertEqual(self.run_cli("remove", "1"), (0, "removed 1\n", ""))
        self.assertEqual(self.run_cli("list", "--all")[1], "[ ] 2 Call the bank\n")

    def test_unknown_id(self):
        self.run_cli("add", "Buy milk")
        with open(self.path, "rb") as fh:
            before = fh.read()
        for command in ("done", "remove"):
            with self.subTest(command=command):
                code, out, err = self.run_cli(command, "5")
                self.assertEqual(code, 1)
                self.assertEqual(out, "")
                self.assertIn("5", err)
        with open(self.path, "rb") as fh:
            self.assertEqual(fh.read(), before)

    def test_empty_text_is_a_usage_error(self):
        code, out, err = self.run_cli("add", "  ")
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertFalse(os.path.exists(self.path))

    def test_id_must_be_a_number(self):
        code, _, _ = self.run_cli("done", "one")
        self.assertEqual(code, 2)

    def test_unreadable_file(self):
        with open(self.path, "w", encoding="utf-8") as fh:
            fh.write("{oops")
        code, out, err = self.run_cli("list")
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertIn("todo:", err)

    def test_file_contents(self):
        self.run_cli("add", "Buy milk")
        with open(self.path, encoding="utf-8") as fh:
            data = json.load(fh)
        self.assertEqual([item["text"] for item in data["items"]], ["Buy milk"])


if __name__ == "__main__":
    unittest.main()
