import os
import tempfile
import unittest

from scheduler.config import ConfigError, load_config, load_jobs

EXAMPLE = """
[defaults]
tags = ["prod"]

[[jobs]]
name = "backup"
schedule = "30 2 * * *"
command = "backup.sh --full"

[[jobs]]
name = "report"
schedule = "@weekly"
command = "report.py"
enabled = false
tags = ["mail"]
"""


class LoadJobsTests(unittest.TestCase):
    def test_example(self):
        reg = load_jobs(EXAMPLE)
        self.assertEqual([j.name for j in reg], ["backup", "report"])
        backup = reg.get("backup")
        self.assertEqual(backup.schedule, "30 2 * * *")
        self.assertEqual(backup.command, "backup.sh --full")
        self.assertTrue(backup.enabled)
        self.assertEqual(backup.tags, ("prod",))
        report = reg.get("report")
        self.assertFalse(report.enabled)
        self.assertEqual(report.tags, ("mail",))

    def test_empty_file_has_no_jobs(self):
        self.assertEqual(len(load_jobs("")), 0)

    def test_errors(self):
        cases = {
            "syntax": "[[jobs]\nname = 'x'",
            "missing command": "[[jobs]]\nname = 'a'\nschedule = '* * * * *'",
            "unknown key": "[[jobs]]\nname = 'a'\nschedule = '@daily'\ncommand = 'x'\nretries = 3",
            "unknown top-level": "[settings]\nx = 1",
            "bad enabled": "[[jobs]]\nname = 'a'\nschedule = '@daily'\ncommand = 'x'\nenabled = 'yes'",
            "bad tags": "[defaults]\ntags = 'prod'",
            "schedule not a string": "[[jobs]]\nname = 'a'\nschedule = 5\ncommand = 'x'",
            "bad name": "[[jobs]]\nname = 'a b'\nschedule = '@daily'\ncommand = 'x'",
            "jobs not array": "jobs = 3",
        }
        for label, text in cases.items():
            with self.subTest(label), self.assertRaises(ConfigError):
                load_jobs(text)

    def test_duplicate_names(self):
        text = "[[jobs]]\nname='a'\nschedule='@daily'\ncommand='x'\n" * 2
        with self.assertRaisesRegex(ConfigError, "duplicate"):
            load_jobs(text)

    def test_error_mentions_source_and_job(self):
        text = "[[jobs]]\nname = 'nightly'\nschedule = '@daily'\ncommand = ''"
        with self.assertRaisesRegex(ConfigError, r"jobs\.toml: job 'nightly'"):
            load_jobs(text, source="jobs.toml")

    def test_config_error_is_value_error(self):
        self.assertTrue(issubclass(ConfigError, ValueError))


class LoadConfigTests(unittest.TestCase):
    def test_reads_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "jobs.toml")
            with open(path, "w", encoding="utf-8") as f:
                f.write(EXAMPLE)
            self.assertEqual(len(load_config(path)), 2)

    def test_missing_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ConfigError, "cannot read"):
                load_config(os.path.join(tmp, "nope.toml"))


if __name__ == "__main__":
    unittest.main()
