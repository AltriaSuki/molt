import unittest
from decimal import Decimal

from ledger import parse_amount


class ThousandsTest(unittest.TestCase):
    def test_separators(self):
        self.assertEqual(parse_amount("1,234.50"), Decimal("1234.50"))
        self.assertEqual(parse_amount("12,345,678"), Decimal("12345678"))
        self.assertEqual(parse_amount("-1,000"), Decimal("-1000"))
        self.assertIsInstance(parse_amount("1,234.50"), Decimal)

    def test_without_separators(self):
        self.assertEqual(parse_amount("12.5"), Decimal("12.5"))
        self.assertEqual(parse_amount("1234"), Decimal("1234"))

    def test_misplaced_separators(self):
        for text in ["12,34", "1,2345", ",123", "1.234,5", "1,,234", "1,234,", "12.x"]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError) as caught:
                    parse_amount(text)
                self.assertEqual(str(caught.exception), f"bad amount {text!r}")


if __name__ == "__main__":
    unittest.main()
