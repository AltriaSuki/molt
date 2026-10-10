import contextlib
import io
import os
import tempfile
import unittest
from unittest import mock

from logsum.cli import main

WEB_01 = """\
2024-03-05T09:58:12 GET /index.html 200 5120 12
2024-03-05T09:58:13 GET /static/app.js 200 20480 30
2024-03-05T09:59:40 POST /login 302 0 85

2024-03-05T10:01:02 GET /index.html 200 5120 9
"""

WEB_02 = """\
2024-03-05T10:02:00 GET /api/items?page=1 200 1834 41
this line is not a log entry
2024-03-05T10:02:05 GET /api/items?page=2 500 120 120
2024-03-05T10:03:30 GET /favicon.ico 404 0 1
"""


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def write(self, name, text):
        path = os.path.join(self.tmp.name, name)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text)
        return path

    def run_main(self, *argv, stdin=""):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), mock.patch("sys.stdin", io.StringIO(stdin)):
            code = main(list(argv))
        return code, out.getvalue()

    def test_single_file(self):
        code, out = self.run_main(self.write("web-01.log", WEB_01))
        self.assertEqual(code, 0)
        lines = out.splitlines()
        self.assertIn("requests: 4", lines)
        self.assertIn("bytes: 30720", lines)
        self.assertIn("2xx: 3", lines)
        self.assertIn("3xx: 1", lines)

    def test_files_are_summarized_together(self):
        code, out = self.run_main(
            self.write("web-01.log", WEB_01), self.write("web-02.log", WEB_02)
        )
        self.assertEqual(code, 0)
        lines = out.splitlines()
        self.assertIn("requests: 7", lines)
        self.assertIn("bytes: 32674", lines)
        self.assertIn("4xx: 1", lines)
        self.assertIn("5xx: 1", lines)
        start = lines.index("top paths:") + 1
        top = lines[start:start + 5]
        self.assertEqual(
            [line.split() for line in top],
            [["2", "/api/items"], ["2", "/index.html"], ["1", "/favicon.ico"],
             ["1", "/login"], ["1", "/static/app.js"]],
        )

    def test_malformed_lines_are_left_out(self):
        code, out = self.run_main(self.write("web-02.log", WEB_02))
        self.assertEqual(code, 0)
        self.assertIn("requests: 3", out.splitlines())

    def test_dash_reads_stdin(self):
        code, out = self.run_main("-", stdin=WEB_01)
        self.assertEqual(code, 0)
        self.assertIn("requests: 4", out.splitlines())

    def test_missing_file(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit) as ctx:
                main([os.path.join(self.tmp.name, "nope.log")])
        self.assertEqual(ctx.exception.code, 2)
        self.assertIn("nope.log", err.getvalue())

    def test_needs_a_file(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as ctx:
                main([])
        self.assertEqual(ctx.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
