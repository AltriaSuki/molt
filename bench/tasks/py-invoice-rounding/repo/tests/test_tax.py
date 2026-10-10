import unittest

from invoicing.tax import DEFAULT_RATES, TaxTable, UnknownCategoryError


class TaxTableTests(unittest.TestCase):
    def test_default_rates(self):
        table = TaxTable()
        self.assertEqual(table.categories(), list(DEFAULT_RATES))
        self.assertEqual(table.rate("standard"), 20)
        self.assertEqual(table.rate("reduced"), 5)
        self.assertEqual(table.rate("zero"), 0)

    def test_unknown_category(self):
        table = TaxTable()
        with self.assertRaises(UnknownCategoryError) as ctx:
            table.rate("luxury")
        self.assertIsInstance(ctx.exception, KeyError)
        self.assertIn("luxury", str(ctx.exception))

    def test_custom_rates_replace_the_defaults(self):
        table = TaxTable({"books": 7})
        self.assertEqual(table.categories(), ["books"])
        with self.assertRaises(UnknownCategoryError):
            table.rate("standard")

    def test_set_rate_adds_or_replaces(self):
        table = TaxTable()
        table.set_rate("books", 7)
        table.set_rate("standard", 21)
        self.assertEqual(table.rate("books"), 7)
        self.assertEqual(table.rate("standard"), 21)

    def test_rate_must_be_a_percentage(self):
        for rate in (-1, 101, "250"):
            with self.subTest(rate=rate), self.assertRaises(ValueError):
                TaxTable({"odd": rate})

    def test_category_must_be_a_non_empty_string(self):
        with self.assertRaises(ValueError):
            TaxTable({"": 5})

    def test_tax_on(self):
        table = TaxTable()
        self.assertEqual(table.tax_on(100, "standard"), 20)
        self.assertEqual(table.tax_on(80, "reduced"), 4)
        self.assertEqual(table.tax_on(55, "zero"), 0)


if __name__ == "__main__":
    unittest.main()
