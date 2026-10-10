import io
import os
import tempfile
import unittest
from contextlib import redirect_stderr

from scheduler.__main__ import main


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def write(self, text):
        path = os.path.join(self.tmp.name, "jobs.toml")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return path

    def test_no_enabled_jobs(self):
        path = self.write("[[jobs]]\nname='a'\nschedule='@daily'\ncommand='x'\nenabled=false\n")
        out = io.StringIO()
        self.assertEqual(main([path, "--after", "2024-05-01T09:30"], out=out), 0)
        self.assertEqual(out.getvalue(), "no enabled jobs\n")

    def test_config_error_exits_1(self):
        err = io.StringIO()
        with redirect_stderr(err):
            code = main([os.path.join(self.tmp.name, "missing.toml")], out=io.StringIO())
        self.assertEqual(code, 1)
        self.assertIn("cannot read", err.getvalue())

    def test_bad_after_is_a_usage_error(self):
        path = self.write("")
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as ctx:
            main([path, "--after", "tomorrow"], out=io.StringIO())
        self.assertEqual(ctx.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
