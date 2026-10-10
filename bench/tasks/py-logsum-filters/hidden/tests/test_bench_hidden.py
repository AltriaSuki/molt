import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# One day of traffic around 2024-03-05, with entries exactly on the
# boundaries the tests use (midnight, 10:00:00, 12:00:00).
DAY = """\
2024-03-04T23:59:59 GET /index.html 200 100 5
2024-03-05T00:00:00 GET /index.html 200 200 7
2024-03-05T09:59:59 GET /api/items?page=1 200 300 11
2024-03-05T10:00:00 POST /api/items 201 400 40
2024-03-05T10:30:00 GET /login 302 0 3
2024-03-05T11:59:59 GET /missing 404 50 2
2024-03-05T12:00:00 GET /api/items?page=2 500 60 90
2024-03-05T23:59:59 GET /index.html 304 0 1
2024-03-06T00:00:00 GET /reports 200 700 15
2024-03-06T08:15:00 GET /reports?year=2023 200 800 25
"""

# Two valid entries, five malformed lines and three blank ones.
NOISY = """\
2024-03-05T10:15:00 GET /index.html 200 1000 4
not a log line at all

2024-03-05T10:16:00 GET /index.html 200
   \t
2024-03-05T10:17:00 get /index.html 200 10 1
2024-01-01T00:00:00 GET /old 700 10 1
2024-03-05T10:18:00 GET /about 200 500 6
2024-03-05T25:00:00 GET /about 200 500 6

"""

KEYS = {"requests", "bytes", "statuses", "top_paths", "skipped"}
NO_STATUSES = {"2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0}


def run_logsum(*args, stdin=None):
    env = dict(os.environ)
    env["PYTHONPATH"] = str(REPO_ROOT)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return subprocess.run(
        [sys.executable, "-m", "logsum", *args],
        cwd=REPO_ROOT,
        input=stdin if stdin is not None else "",
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )


class LogsumTestCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)

    def log(self, name, text):
        path = self.tmp / name
        path.write_text(text, encoding="utf-8")
        return str(path)

    def ok(self, *args, stdin=None):
        result = run_logsum(*args, stdin=stdin)
        if result.returncode != 0:
            self.fail(
                f"logsum {' '.join(args)} exited {result.returncode}\n"
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            )
        return result

    def json_of(self, *args, stdin=None):
        out = self.ok("--json", *args, stdin=stdin).stdout
        try:
            return json.loads(out)
        except ValueError:
            self.fail(f"stdout is not a single JSON document:\n{out}")

    def text_lines(self, *args, stdin=None):
        return [line.strip() for line in self.ok(*args, stdin=stdin).stdout.splitlines()]


class TimeWindowTests(LogsumTestCase):
    def setUp(self):
        super().setUp()
        self.day = self.log("day.log", DAY)

    def test_no_window_counts_everything(self):
        data = self.json_of(self.day)
        self.assertEqual(data["requests"], 10)
        self.assertEqual(data["bytes"], 2610)

    def test_since_datetime_is_inclusive(self):
        data = self.json_of("--since", "2024-03-05T10:00:00", self.day)
        self.assertEqual(data["requests"], 7)
        self.assertEqual(data["bytes"], 2010)

    def test_until_datetime_is_exclusive(self):
        data = self.json_of("--until", "2024-03-05T12:00:00", self.day)
        self.assertEqual(data["requests"], 6)
        self.assertEqual(data["bytes"], 1050)

    def test_since_and_until_together(self):
        data = self.json_of(
            "--since", "2024-03-05T10:00:00", "--until", "2024-03-05T12:00:00", self.day
        )
        self.assertEqual(
            data,
            {
                "requests": 3,
                "bytes": 450,
                "statuses": {"2xx": 1, "3xx": 1, "4xx": 1, "5xx": 0},
                "top_paths": [["/api/items", 1], ["/login", 1], ["/missing", 1]],
                "skipped": 0,
            },
        )

    def test_since_date_means_midnight(self):
        data = self.json_of("--since", "2024-03-05", self.day)
        self.assertEqual(data["requests"], 9)
        self.assertEqual(data["bytes"], 2510)

    def test_until_date_excludes_that_whole_day(self):
        data = self.json_of("--until", "2024-03-05", self.day)
        self.assertEqual(data["requests"], 1)
        self.assertEqual(data["bytes"], 100)
        self.assertEqual(data["top_paths"], [["/index.html", 1]])

    def test_one_day_window(self):
        data = self.json_of("--since", "2024-03-05", "--until", "2024-03-06", self.day)
        self.assertEqual(
            data,
            {
                "requests": 7,
                "bytes": 1010,
                "statuses": {"2xx": 3, "3xx": 2, "4xx": 1, "5xx": 1},
                "top_paths": [
                    ["/api/items", 3],
                    ["/index.html", 2],
                    ["/login", 1],
                    ["/missing", 1],
                ],
                "skipped": 0,
            },
        )

    def test_date_equals_midnight_datetime(self):
        by_date = self.json_of("--until", "2024-03-06", self.day)
        by_datetime = self.json_of("--until", "2024-03-06T00:00:00", self.day)
        self.assertEqual(by_date, by_datetime)
        self.assertEqual(by_date["requests"], 8)
        by_date = self.json_of("--since", "2024-03-06", self.day)
        self.assertEqual(by_date["requests"], 2)
        self.assertEqual(by_date["top_paths"], [["/reports", 2]])

    def test_empty_window_is_not_an_error(self):
        for since, until in (
            ("2024-03-06", "2024-03-05"),
            ("2024-03-05T10:00:00", "2024-03-05T10:00:00"),
        ):
            with self.subTest(since=since, until=until):
                data = self.json_of("--since", since, "--until", until, self.day)
                self.assertEqual(
                    data,
                    {
                        "requests": 0,
                        "bytes": 0,
                        "statuses": NO_STATUSES,
                        "top_paths": [],
                        "skipped": 0,
                    },
                )

    def test_window_applies_to_text_report(self):
        lines = self.text_lines(
            "--since", "2024-03-05T10:00:00", "--until", "2024-03-05T12:00:00", self.day
        )
        for expected in ("requests: 3", "bytes: 450", "2xx: 1", "3xx: 1", "4xx: 1", "5xx: 0"):
            self.assertIn(expected, lines)
        self.assertIn("max duration: 40 ms", lines)

    def test_window_spans_several_files(self):
        second = self.log(
            "day2.log",
            "2024-03-06T09:00:00 GET /reports 200 5 1\n"
            "2024-03-07T00:00:00 GET /reports 200 5 1\n",
        )
        data = self.json_of("--since", "2024-03-05T23:00:00", "--until", "2024-03-07", self.day, second)
        self.assertEqual(data["requests"], 4)
        self.assertEqual(data["bytes"], 1505)
        self.assertEqual(data["top_paths"], [["/reports", 3], ["/index.html", 1]])


class InvalidBoundTests(LogsumTestCase):
    def setUp(self):
        super().setUp()
        self.day = self.log("day.log", DAY)

    def assert_rejected(self, value, *extra):
        for option in ("--since", "--until"):
            with self.subTest(option=option, value=value):
                result = run_logsum(*extra, option, value, self.day)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertIn(value, result.stderr)

    def test_accepts_both_forms(self):
        for value in ("2024-03-05", "2024-03-05T10:00:00"):
            for option in ("--since", "--until"):
                with self.subTest(option=option, value=value):
                    self.ok(option, value, self.day)

    def test_rejects_missing_seconds(self):
        self.assert_rejected("2024-03-05T10:00")

    def test_rejects_space_instead_of_t(self):
        self.assert_rejected("2024-03-05 10:00:00")

    def test_rejects_time_zones(self):
        self.assert_rejected("2024-03-05T10:00:00Z")
        self.assert_rejected("2024-03-05T10:00:00+01:00")

    def test_rejects_fractional_seconds(self):
        self.assert_rejected("2024-03-05T10:00:00.250")

    def test_rejects_impossible_dates_and_times(self):
        self.assert_rejected("2024-02-30")
        self.assert_rejected("2024-13-01")
        self.assert_rejected("2024-03-05T24:00:00")

    def test_rejects_other_formats(self):
        for value in ("20240305", "05/03/2024", "yesterday", "2024-03"):
            self.assert_rejected(value)

    def test_rejected_with_json_prints_nothing(self):
        self.assert_rejected("2024-03-05T10:00", "--json")


class SkippedLineTests(LogsumTestCase):
    def test_text_report_shows_skipped(self):
        lines = self.text_lines(self.log("noisy.log", NOISY))
        self.assertIn("skipped: 5", lines)
        self.assertIn("requests: 2", lines)
        self.assertIn("bytes: 1500", lines)

    def test_text_report_shows_zero_skipped(self):
        lines = self.text_lines(self.log("day.log", DAY))
        self.assertIn("skipped: 0", lines)

    def test_blank_lines_are_not_skipped(self):
        path = self.log(
            "blanks.log",
            "\n\n2024-03-05T10:00:00 GET / 200 1 1\n   \n\t\n2024-03-05T10:00:01 GET / 200 1 1\n\n",
        )
        data = self.json_of(path)
        self.assertEqual(data["requests"], 2)
        self.assertEqual(data["skipped"], 0)

    def test_skipped_counts_all_files(self):
        other = self.log(
            "other.log",
            "2024-03-05T10:00:00 GET / 200 1 1\n"
            "2024-03-05T10:00:01 GET / 2OO 1 1\n"
            "2024-03-05T10:00:02 GET / 200 1",
        )
        data = self.json_of(self.log("noisy.log", NOISY), other)
        self.assertEqual(data["skipped"], 7)
        self.assertEqual(data["requests"], 3)
        lines = self.text_lines(self.log("noisy.log", NOISY), other)
        self.assertIn("skipped: 7", lines)

    def test_skipped_ignores_the_window(self):
        noisy = self.log("noisy.log", NOISY)
        for window in (("--since", "2024-03-06"), ("--until", "2024-01-02"),
                       ("--since", "2024-03-05T10:16:00", "--until", "2024-03-05T10:17:00")):
            with self.subTest(window=window):
                data = self.json_of(*window, noisy)
                self.assertEqual(data["requests"], 0)
                self.assertEqual(data["skipped"], 5)

    def test_entries_outside_the_window_are_not_skipped(self):
        day = self.log("day.log", DAY)
        data = self.json_of("--since", "2024-03-06", day)
        self.assertEqual(data["skipped"], 0)
        lines = self.text_lines("--until", "2024-03-05", day)
        self.assertIn("skipped: 0", lines)
        self.assertIn("requests: 1", lines)


class JsonOutputTests(LogsumTestCase):
    def test_keys(self):
        data = self.json_of(self.log("day.log", DAY))
        self.assertIsInstance(data, dict)
        self.assertEqual(set(data), KEYS)

    def test_full_object(self):
        data = self.json_of(self.log("day.log", DAY), self.log("noisy.log", NOISY))
        self.assertEqual(
            data,
            {
                "requests": 12,
                "bytes": 4110,
                "statuses": {"2xx": 8, "3xx": 2, "4xx": 1, "5xx": 1},
                "top_paths": [
                    ["/index.html", 4],
                    ["/api/items", 3],
                    ["/reports", 2],
                    ["/about", 1],
                    ["/login", 1],
                ],
                "skipped": 5,
            },
        )

    def test_value_types(self):
        data = self.json_of(self.log("noisy.log", NOISY))
        for key in ("requests", "bytes", "skipped"):
            self.assertIs(type(data[key]), int, key)
        for value in data["statuses"].values():
            self.assertIs(type(value), int)
        for pair in data["top_paths"]:
            self.assertIsInstance(pair, list)
            self.assertEqual(len(pair), 2)
            self.assertIs(type(pair[0]), str)
            self.assertIs(type(pair[1]), int)

    def test_statuses_always_has_all_four_classes(self):
        path = self.log(
            "ok.log",
            "2024-03-05T10:00:00 GET /a 200 1 1\n2024-03-05T10:00:01 GET /b 204 1 1\n",
        )
        self.assertEqual(
            self.json_of(path)["statuses"], {"2xx": 2, "3xx": 0, "4xx": 0, "5xx": 0}
        )
        path = self.log("errors.log", "2024-03-05T10:00:00 GET /a 503 1 1\n")
        self.assertEqual(
            self.json_of(path)["statuses"], {"2xx": 0, "3xx": 0, "4xx": 0, "5xx": 1}
        )

    def test_top_paths_limit_and_tie_order(self):
        counts = (("/zeta", 3), ("/eta", 3), ("/omega", 1), ("/gamma", 2),
                  ("/delta", 2), ("/beta", 2), ("/alpha", 1))
        lines = []
        for second, (path, count) in enumerate(counts):
            lines += [f"2024-03-05T10:00:{second:02d} GET {path} 200 1 1"] * count
        data = self.json_of(self.log("paths.log", "\n".join(lines) + "\n"))
        self.assertEqual(
            data["top_paths"],
            [["/eta", 3], ["/zeta", 3], ["/beta", 2], ["/delta", 2], ["/gamma", 2]],
        )
        self.assertEqual(data["requests"], 14)

    def test_top_paths_ignore_query_strings(self):
        path = self.log(
            "search.log",
            "2024-03-05T10:00:00 GET /search?q=a 200 1 1\n"
            "2024-03-05T10:00:01 GET /search?q=b 200 1 1\n"
            "2024-03-05T10:00:02 GET /home 200 1 1\n"
            "2024-03-05T10:00:03 GET /home?ref=x 200 1 1\n"
            "2024-03-05T10:00:04 GET /search 200 1 1\n",
        )
        self.assertEqual(self.json_of(path)["top_paths"], [["/search", 3], ["/home", 2]])

    def test_empty_log(self):
        data = self.json_of(self.log("empty.log", ""))
        self.assertEqual(
            data,
            {"requests": 0, "bytes": 0, "statuses": NO_STATUSES, "top_paths": [], "skipped": 0},
        )

    def test_only_malformed_lines(self):
        data = self.json_of(self.log("junk.log", "junk\nmore junk\n"))
        self.assertEqual(
            data,
            {"requests": 0, "bytes": 0, "statuses": NO_STATUSES, "top_paths": [], "skipped": 2},
        )

    def test_stdin(self):
        data = self.json_of("-", stdin=NOISY)
        self.assertEqual(data["requests"], 2)
        self.assertEqual(data["skipped"], 5)

    def test_text_report_is_unchanged_without_json(self):
        out = self.ok(self.log("day.log", DAY)).stdout
        self.assertIn("requests: 10", out.splitlines())
        self.assertIn("top paths:", out.splitlines())
        self.assertNotIn("{", out)


if __name__ == "__main__":
    unittest.main()
