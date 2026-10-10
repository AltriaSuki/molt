import unittest
from datetime import datetime

from logsum.parser import Entry, ParseError, parse_line


class ParseLineTests(unittest.TestCase):
    def test_valid_line(self):
        entry = parse_line("2024-03-05T14:02:11 GET /index.html 200 5120 12\n")
        self.assertEqual(
            entry,
            Entry(
                timestamp=datetime(2024, 3, 5, 14, 2, 11),
                method="GET",
                path="/index.html",
                status=200,
                bytes=5120,
                duration_ms=12,
            ),
        )

    def test_tabs_and_repeated_spaces(self):
        entry = parse_line("2024-03-05T14:02:11\tPOST   /api/items\t201 0  87")
        self.assertEqual(entry.method, "POST")
        self.assertEqual(entry.path, "/api/items")
        self.assertEqual(entry.status, 201)
        self.assertEqual(entry.bytes, 0)
        self.assertEqual(entry.duration_ms, 87)

    def test_path_keeps_query_string(self):
        entry = parse_line("2024-03-05T14:02:11 GET /search?q=logs 200 10 3")
        self.assertEqual(entry.path, "/search?q=logs")

    def test_status_class(self):
        self.assertEqual(parse_line("2024-03-05T14:02:11 GET / 302 0 1").status_class, "3xx")
        self.assertEqual(parse_line("2024-03-05T14:02:11 GET / 503 0 1").status_class, "5xx")

    def test_wrong_number_of_fields(self):
        for line in (
            "2024-03-05T14:02:11 GET /index.html 200 5120",
            "2024-03-05T14:02:11 GET /index.html 200 5120 12 extra",
            "garbage",
        ):
            with self.subTest(line=line):
                with self.assertRaises(ParseError):
                    parse_line(line)

    def test_invalid_timestamp(self):
        for stamp in ("2024-03-05 14:02:11", "2024-13-05T14:02:11", "2024-03-05T14:02", "yesterday"):
            with self.subTest(stamp=stamp):
                with self.assertRaises(ParseError):
                    parse_line(f"{stamp} GET / 200 1 1")

    def test_invalid_method(self):
        for method in ("get", "G3T", "-"):
            with self.subTest(method=method):
                with self.assertRaises(ParseError):
                    parse_line(f"2024-03-05T14:02:11 {method} / 200 1 1")

    def test_path_must_be_absolute(self):
        with self.assertRaises(ParseError):
            parse_line("2024-03-05T14:02:11 GET index.html 200 1 1")

    def test_invalid_status(self):
        for status in ("abc", "20x", "-200", "199", "600", "101"):
            with self.subTest(status=status):
                with self.assertRaises(ParseError):
                    parse_line(f"2024-03-05T14:02:11 GET / {status} 1 1")

    def test_invalid_counts(self):
        for size, duration in (("-1", "1"), ("1.5", "1"), ("1", "fast"), ("1", "-3")):
            with self.subTest(size=size, duration=duration):
                with self.assertRaises(ParseError):
                    parse_line(f"2024-03-05T14:02:11 GET / 200 {size} {duration}")

    def test_parse_error_is_a_value_error(self):
        self.assertTrue(issubclass(ParseError, ValueError))


if __name__ == "__main__":
    unittest.main()
