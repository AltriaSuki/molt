import unittest
from decimal import Decimal

from ledger import parse_amount


class AmountTest(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(parse_amount("12.5"), Decimal("12.5"))
        self.assertEqual(parse_amount("-3"), Decimal("-3"))

    def test_bad(self):
        with self.assertRaises(ValueError) as caught:
            parse_amount("12.x")
        self.assertEqual(str(caught.exception), "bad amount '12.x'")


if __name__ == "__main__":
    unittest.main()
