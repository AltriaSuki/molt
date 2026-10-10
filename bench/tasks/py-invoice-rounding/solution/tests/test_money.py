import unittest
from decimal import Decimal

from invoicing.money import format_amount, round_cents, to_amount


class ToAmountTests(unittest.TestCase):
    def test_int(self):
        self.assertEqual(to_amount(12), Decimal(12))

    def test_numeric_string(self):
        self.assertEqual(to_amount("19.99"), Decimal("19.99"))

    def test_decimal(self):
        self.assertEqual(to_amount(Decimal("0.125")), Decimal("0.125"))

    def test_surrounding_whitespace_is_ignored(self):
        self.assertEqual(to_amount("  7.5\n"), Decimal("7.5"))

    def test_rejects_floats(self):
        with self.assertRaises(TypeError):
            to_amount(19.99)

    def test_rejects_bool(self):
        with self.assertRaises(TypeError):
            to_amount(True)

    def test_rejects_other_types(self):
        for value in (None, [1], {"amount": 1}):
            with self.subTest(value=value), self.assertRaises(TypeError):
                to_amount(value)

    def test_rejects_strings_that_are_not_numbers(self):
        for value in ("", "abc", "1,000", "12.3.4"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                to_amount(value)

    def test_rejects_non_finite_values(self):
        for value in ("nan", "inf", "-Infinity"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                to_amount(value)


class RoundCentsTests(unittest.TestCase):
    def test_rounds_to_two_places(self):
        self.assertEqual(round_cents("2.499"), Decimal("2.50"))
        self.assertEqual(round_cents("10.004"), Decimal("10.00"))

    def test_whole_numbers_are_unchanged(self):
        self.assertEqual(round_cents(3), Decimal("3.00"))

    def test_halves_round_up(self):
        self.assertEqual(round_cents("0.125"), Decimal("0.13"))


class FormatAmountTests(unittest.TestCase):
    def test_thousands_separator(self):
        self.assertEqual(format_amount("1234567.89"), "1,234,567.89")

    def test_small_amount(self):
        self.assertEqual(format_amount("0.99"), "0.99")

    def test_always_two_decimals(self):
        self.assertEqual(format_amount("1234.5"), "1,234.50")
        self.assertEqual(format_amount(0), "0.00")


if __name__ == "__main__":
    unittest.main()
