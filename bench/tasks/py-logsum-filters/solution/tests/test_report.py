import unittest

from logsum.parser import parse_line
from logsum.report import render_text
from logsum.summary import Summary, summarize


class RenderTextTests(unittest.TestCase):
    def test_report(self):
        summary = summarize(
            parse_line(line)
            for line in (
                "2024-03-05T10:00:00 GET /index.html 200 1000 10",
                "2024-03-05T10:00:01 GET /index.html 200 1000 20",
                "2024-03-05T10:00:02 GET /index.html 304 0 2",
                "2024-03-05T10:00:03 POST /api/items 201 250 120",
                "2024-03-05T10:00:04 GET /api/items?page=2 500 80 45",
                "2024-03-05T10:00:05 GET /missing 404 12 3",
            )
        )
        self.assertEqual(
            render_text(summary),
            "requests: 6\n"
            "bytes: 2342\n"
            "skipped: 0\n"
            "2xx: 3\n"
            "3xx: 1\n"
            "4xx: 1\n"
            "5xx: 1\n"
            "avg duration: 33.3 ms\n"
            "max duration: 120 ms\n"
            "top paths:\n"
            "  3  /index.html\n"
            "  2  /api/items\n"
            "  1  /missing\n",
        )

    def test_counts_are_right_aligned(self):
        lines = ["2024-03-05T10:00:00 GET /busy 200 0 1"] * 12
        lines.append("2024-03-05T10:00:00 GET /quiet 200 0 1")
        report = render_text(summarize(parse_line(line) for line in lines))
        self.assertIn("  12  /busy\n   1  /quiet\n", report)

    def test_skipped_lines(self):
        summary = Summary(skipped=3)
        self.assertIn("\nskipped: 3\n", render_text(summary))

    def test_empty_summary(self):
        self.assertEqual(
            render_text(Summary()),
            "requests: 0\n"
            "bytes: 0\n"
            "skipped: 0\n"
            "2xx: 0\n"
            "3xx: 0\n"
            "4xx: 0\n"
            "5xx: 0\n"
            "avg duration: n/a\n"
            "max duration: n/a\n"
            "top paths: none\n",
        )


if __name__ == "__main__":
    unittest.main()
