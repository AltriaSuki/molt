import unittest

from logsum.parser import parse_line
from logsum.summary import Summary, normalize_path, summarize


def entries(*lines):
    return [parse_line(line) for line in lines]


class SummaryTests(unittest.TestCase):
    def test_empty(self):
        summary = Summary()
        self.assertEqual(summary.requests, 0)
        self.assertEqual(summary.bytes, 0)
        self.assertEqual(summary.status_counts(), {"2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0})
        self.assertEqual(summary.top_paths(), [])
        self.assertIsNone(summary.avg_duration_ms)

    def test_totals(self):
        summary = summarize(
            entries(
                "2024-03-05T10:00:00 GET /a 200 100 10",
                "2024-03-05T10:00:01 GET /b 404 20 30",
                "2024-03-05T10:00:02 POST /a 201 5 5",
            )
        )
        self.assertEqual(summary.requests, 3)
        self.assertEqual(summary.bytes, 125)
        self.assertEqual(summary.max_duration_ms, 30)
        self.assertAlmostEqual(summary.avg_duration_ms, 15.0)

    def test_status_counts_has_every_class_in_order(self):
        summary = summarize(
            entries(
                "2024-03-05T10:00:00 GET /a 500 0 1",
                "2024-03-05T10:00:00 GET /a 200 0 1",
                "2024-03-05T10:00:00 GET /a 204 0 1",
            )
        )
        counts = summary.status_counts()
        self.assertEqual(list(counts), ["2xx", "3xx", "4xx", "5xx"])
        self.assertEqual(counts, {"2xx": 2, "3xx": 0, "4xx": 0, "5xx": 1})

    def test_top_paths_order_and_limit(self):
        lines = []
        for path, count in (("/f", 1), ("/e", 2), ("/d", 2), ("/c", 3), ("/b", 1), ("/a", 1)):
            lines += [f"2024-03-05T10:00:00 GET {path} 200 0 1"] * count
        summary = summarize(entries(*lines))
        self.assertEqual(
            summary.top_paths(),
            [("/c", 3), ("/d", 2), ("/e", 2), ("/a", 1), ("/b", 1)],
        )
        self.assertEqual(summary.top_paths(2), [("/c", 3), ("/d", 2)])

    def test_query_strings_are_ignored_for_paths(self):
        summary = summarize(
            entries(
                "2024-03-05T10:00:00 GET /search?q=a 200 0 1",
                "2024-03-05T10:00:00 GET /search?q=b 200 0 1",
                "2024-03-05T10:00:00 GET /search 200 0 1",
            )
        )
        self.assertEqual(summary.top_paths(), [("/search", 3)])

    def test_normalize_path(self):
        self.assertEqual(normalize_path("/a/b?x=1&y=2"), "/a/b")
        self.assertEqual(normalize_path("/a/b"), "/a/b")
        self.assertEqual(normalize_path("/?"), "/")

    def test_update_adds_to_existing_totals(self):
        summary = Summary()
        summary.update(entries("2024-03-05T10:00:00 GET /a 200 10 1"))
        summary.update(entries("2024-03-06T10:00:00 GET /a 302 0 1"))
        self.assertEqual(summary.requests, 2)
        self.assertEqual(summary.top_paths(), [("/a", 2)])


if __name__ == "__main__":
    unittest.main()
