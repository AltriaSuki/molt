import io
import json
import os
import tempfile
import unittest

from pkgresolve.cli import main

INDEX = {
    "web": {"1.0.0": ["http^1"], "2.0.0": ["http^2", "tls>=1.2"]},
    "http": {"1.0.0": [], "2.0.0": ["tls"]},
    "tls": {"1.2.0": [], "1.3.0": []},
}


class CliTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.index_path = self.write("index.json", json.dumps(INDEX))

    def write(self, name, text):
        path = os.path.join(self.tmp.name, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        code = main(list(argv), stdout=out, stderr=err)
        return code, out.getvalue(), err.getvalue()

    def test_resolve_prints_lock(self):
        code, out, err = self.run_cli("resolve", self.index_path, "web")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "http==2.0.0\ntls==1.3.0\nweb==2.0.0\n")

    def test_resolve_failure(self):
        code, out, err = self.run_cli("resolve", self.index_path, "web", "tls>=2")
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertTrue(err.startswith("error: "))
        self.assertIn("tls", err)

    def test_resolve_bad_requirement(self):
        code, _, err = self.run_cli("resolve", self.index_path, "web >> 1")
        self.assertEqual(code, 2)
        self.assertIn("error:", err)

    def test_check_ok(self):
        lock = self.write("app.lock", "http==1.0.0\nweb==1.0.0\n")
        code, out, _ = self.run_cli("check", self.index_path, lock, "web^1")
        self.assertEqual((code, out), (0, "ok\n"))

    def test_check_reports_problems(self):
        lock = self.write("app.lock", "http==1.0.0\nweb==1.0.0\ntls==1.3.0\n")
        code, out, _ = self.run_cli("check", self.index_path, lock, "web^1")
        self.assertEqual(code, 1)
        self.assertEqual(out, "tls is locked but nothing requires it\n")

    def test_missing_index_file(self):
        code, _, err = self.run_cli("resolve", os.path.join(self.tmp.name, "nope.json"), "web")
        self.assertEqual(code, 2)
        self.assertIn("error:", err)


if __name__ == "__main__":
    unittest.main()
