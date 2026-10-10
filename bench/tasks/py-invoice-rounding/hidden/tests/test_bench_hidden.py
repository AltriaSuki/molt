import re
import unittest
from decimal import Decimal

from invoicing.models import Invoice, LineItem
from invoicing.money import format_amount, round_cents, to_amount
from invoicing.report import render, summary
from invoicing.tax import TaxTable

D = Decimal


def assert_cents(test, value, expected):
    """``value`` is a Decimal equal to ``expected`` with exactly two places."""
    test.assertIsInstance(value, Decimal)
    test.assertEqual(value, D(expected))
    test.assertEqual(str(value), expected)


def row(report, text):
    matches = [line for line in report.splitlines() if text in line]
    if len(matches) != 1:
        raise AssertionError(f"expected one line containing {text!r}, got {matches}")
    return matches[0].split()


def total_row(report, label):
    """The amount on the summary row whose label is exactly ``label``."""
    for line in report.splitlines():
        m = re.fullmatch(r"\s*(.+?)\s{2,}(\S+)", line)
        if m and m.group(1) == label:
            return m.group(2)
    raise AssertionError(f"no row labelled {label!r} in:\n{report}")


class FloatRejectionTests(unittest.TestCase):
    def assert_float_rejected(self, fn, *args, **kwargs):
        with self.assertRaises(TypeError) as ctx:
            fn(*args, **kwargs)
        message = str(ctx.exception)
        self.assertIn("str", message)
        self.assertIn("Decimal", message)

    def test_to_amount(self):
        for value in (0.1, 19.99, 2.0, 0.0):
            with self.subTest(value=value):
                self.assert_float_rejected(to_amount, value)

    def test_round_cents_and_format_amount(self):
        self.assert_float_rejected(round_cents, 0.125)
        self.assert_float_rejected(format_amount, 1234.5)

    def test_line_item_price_and_discount(self):
        self.assert_float_rejected(LineItem, "Widget", 1, 9.99)
        self.assert_float_rejected(LineItem, "Widget", 1, "9.99", discount_percent=12.5)

    def test_invoice_add(self):
        invoice = Invoice("INV-1", "Acme")
        self.assert_float_rejected(invoice.add, "Widget", 2, 0.3)
        self.assertEqual(invoice.items, [])

    def test_tax_rates_and_tax_on(self):
        self.assert_float_rejected(TaxTable, {"standard": 20.0})
        table = TaxTable()
        self.assert_float_rejected(table.set_rate, "books", 5.5)
        self.assert_float_rejected(table.tax_on, 10.5, "reduced")


class MoneyTests(unittest.TestCase):
    def test_to_amount_returns_exact_decimals(self):
        for value, expected in ((7, D(7)), ("19.99", D("19.99")), ("0.125", D("0.125")),
                                (D("1234.5678"), D("1234.5678")), (" 3.30 ", D("3.30"))):
            with self.subTest(value=value):
                result = to_amount(value)
                self.assertIsInstance(result, Decimal)
                self.assertEqual(result, expected)

    def test_to_amount_rejects_non_finite_decimals(self):
        for value in (D("NaN"), D("Infinity"), D("-Infinity")):
            with self.subTest(value=value), self.assertRaises(ValueError):
                to_amount(value)

    def test_round_cents_rounds_halves_up(self):
        cases = {
            "0.125": "0.13",
            "0.625": "0.63",
            "0.145": "0.15",
            "2.675": "2.68",
            "1.005": "1.01",
            "0.135": "0.14",
            "0.124999": "0.12",
            "1234567.895": "1234567.90",
        }
        for value, expected in cases.items():
            with self.subTest(value=value):
                assert_cents(self, round_cents(value), expected)

    def test_round_cents_accepts_decimal_and_int_and_keeps_two_places(self):
        assert_cents(self, round_cents(D("0.125")), "0.13")
        assert_cents(self, round_cents(7), "7.00")
        assert_cents(self, round_cents("40"), "40.00")
        assert_cents(self, round_cents("2.5"), "2.50")

    def test_format_amount_always_two_decimals_with_separators(self):
        cases = [
            ("1234.5", "1,234.50"),
            (0, "0.00"),
            ("0.3", "0.30"),
            (D("1234567.891"), "1,234,567.89"),
            (1000000, "1,000,000.00"),
            ("999.99", "999.99"),
        ]
        for value, expected in cases:
            with self.subTest(value=value):
                self.assertEqual(format_amount(value), expected)

    def test_format_amount_rounds_half_up(self):
        self.assertEqual(format_amount("0.125"), "0.13")
        self.assertEqual(format_amount("2.675"), "2.68")
        self.assertEqual(format_amount("999.995"), "1,000.00")
        self.assertEqual(format_amount("12345678.905"), "12,345,678.91")


class LineItemTests(unittest.TestCase):
    def test_inputs_are_stored_as_decimals(self):
        item = LineItem("Widget", 2, 5, discount_percent=10)
        self.assertIsInstance(item.unit_price, Decimal)
        self.assertIsInstance(item.discount_percent, Decimal)
        self.assertEqual(item.unit_price, D(5))
        self.assertEqual(item.discount_percent, D(10))
        item = LineItem("Widget", 2, D("5.10"), discount_percent="12.5")
        self.assertEqual(item.unit_price, D("5.10"))
        self.assertEqual(item.discount_percent, D("12.5"))

    def test_half_cent_line_totals_round_up(self):
        cases = [
            (1, "0.125", "0.13"),
            (1, "0.625", "0.63"),
            (1, "2.675", "2.68"),
            (3, "0.335", "1.01"),
            (5, "0.025", "0.13"),
        ]
        for quantity, price, expected in cases:
            with self.subTest(quantity=quantity, price=price):
                assert_cents(self, LineItem("Widget", quantity, price).total, expected)

    def test_whole_amounts_have_two_places(self):
        assert_cents(self, LineItem("Widget", 4, 10).total, "40.00")
        assert_cents(self, LineItem("Widget", 3, "0.10").total, "0.30")

    def test_discount_rounds_once_at_the_end(self):
        # 10.05 less 50% is 5.025: rounding the discount on its own would give 5.02.
        assert_cents(self, LineItem("Widget", 1, "10.05", discount_percent=50).total, "5.03")
        # 7 x 1.235 = 8.645, less 10% is 7.7805: rounding the gross first would give 7.79.
        assert_cents(self, LineItem("Widget", 7, "1.235", discount_percent=10).total, "7.78")

    def test_fractional_discount(self):
        # 3 x 19.99 = 59.97, less 12.5% is 52.47375.
        assert_cents(self, LineItem("Widget", 3, "19.99", discount_percent="12.5").total, "52.47")
        # 2 x 4.25 = 8.50, less 15% is 7.225.
        assert_cents(self, LineItem("Widget", 2, "4.25", discount_percent=D("15")).total, "7.23")

    def test_full_discount(self):
        assert_cents(self, LineItem("Sample", 3, "9.99", discount_percent=100).total, "0.00")

    def test_large_amounts(self):
        assert_cents(self, LineItem("Plant", 1000, "12345.675").total, "12345675.00")
        assert_cents(self, LineItem("Plant", 3, "3333333.335").total, "10000000.01")
        assert_cents(self, LineItem("Plant", 1, "12345678.905").total, "12345678.91")


class TaxTests(unittest.TestCase):
    def test_rates_are_decimals(self):
        table = TaxTable({"standard": "20", "books": D("5.5"), "zero": 0})
        for category, expected in (("standard", D(20)), ("books", D("5.5")), ("zero", D(0))):
            with self.subTest(category=category):
                self.assertIsInstance(table.rate(category), Decimal)
                self.assertEqual(table.rate(category), expected)
        self.assertIsInstance(TaxTable().rate("reduced"), Decimal)

    def test_tax_on_rounds_half_up(self):
        table = TaxTable()
        assert_cents(self, table.tax_on("10.50", "reduced"), "0.53")
        assert_cents(self, table.tax_on("4.50", "reduced"), "0.23")
        assert_cents(self, table.tax_on(D("0.10"), "reduced"), "0.01")
        assert_cents(self, table.tax_on(100, "standard"), "20.00")
        assert_cents(self, table.tax_on("55.55", "zero"), "0.00")

    def test_tax_on_with_fractional_rate(self):
        table = TaxTable({"books": "5.5"})
        assert_cents(self, table.tax_on("10.00", "books"), "0.55")
        assert_cents(self, table.tax_on("15.00", "books"), "0.83")


class InvoiceTests(unittest.TestCase):
    def test_tax_is_rounded_per_line(self):
        invoice = Invoice("INV-1", "Acme")
        invoice.add("Book", 1, "10.50", category="reduced")
        invoice.add("Book", 1, "10.50", category="reduced")
        # 0.525 rounds to 0.53 on each line; 5% of 21.00 would be 1.05.
        self.assertEqual(invoice.tax_by_category(), {"reduced": D("1.06")})
        assert_cents(self, invoice.tax, "1.06")
        assert_cents(self, invoice.subtotal, "21.00")
        assert_cents(self, invoice.total, "22.06")

    def test_tax_is_computed_on_the_rounded_line_total(self):
        invoice = Invoice("INV-1", "Acme")
        invoice.add("Book", 1, "10.495", category="reduced")
        # The line bills 10.50, and 5% of that is 0.525 -> 0.53
        # (5% of the unrounded 10.495 would round to 0.52).
        assert_cents(self, invoice.subtotal, "10.50")
        assert_cents(self, invoice.tax, "0.53")
        assert_cents(self, invoice.total, "11.03")

    def test_several_categories_with_discounts(self):
        invoice = Invoice("INV-7", "Acme")
        invoice.add("Laptop", 2, "899.995", discount_percent="7.5")   # 1664.99075 -> 1664.99
        invoice.add("Cable", 3, "4.15")                                # 12.45
        invoice.add("Manual", 1, "12.30", category="reduced")          # 12.30
        invoice.add("Guide", 3, "2.95", category="reduced", discount_percent=50)  # 4.425 -> 4.43
        invoice.add("Apples", 5, "0.45", category="zero")              # 2.25
        # standard: 1664.99 * 20% = 332.998 -> 333.00, 12.45 * 20% = 2.49
        # reduced: 12.30 * 5% = 0.615 -> 0.62, 4.43 * 5% = 0.2215 -> 0.22
        self.assertEqual(
            invoice.tax_by_category(),
            {"standard": D("335.49"), "reduced": D("0.84"), "zero": D("0.00")},
        )
        assert_cents(self, invoice.subtotal, "1696.42")
        assert_cents(self, invoice.tax, "336.33")
        assert_cents(self, invoice.total, "2032.75")
        for value in invoice.tax_by_category().values():
            self.assertEqual(len(str(value).split(".")[1]), 2)

    def test_custom_table_with_string_and_decimal_rates(self):
        invoice = Invoice("INV-8", "Acme", tax_table=TaxTable({"books": "5.5", "food": D("7")}))
        invoice.add("Atlas", 1, "15.00", category="books")    # 0.825 -> 0.83
        invoice.add("Cheese", 1, "3.50", category="food")     # 0.245 -> 0.25
        self.assertEqual(invoice.tax_by_category(), {"books": D("0.83"), "food": D("0.25")})
        assert_cents(self, invoice.total, "19.58")

    def test_sums_have_no_float_drift(self):
        invoice = Invoice("INV-2", "Acme")
        invoice.add("A", 1, "0.10", category="zero")
        invoice.add("B", 1, "0.20", category="zero")
        assert_cents(self, invoice.subtotal, "0.30")
        assert_cents(self, invoice.total, "0.30")

        many = Invoice("INV-3", "Acme")
        for _ in range(1000):
            many.add("Stamp", 1, "0.10", category="zero")
        assert_cents(self, many.subtotal, "100.00")
        assert_cents(self, many.total, "100.00")

    def test_large_invoice(self):
        invoice = Invoice("INV-4", "Acme")
        invoice.add("Plant", 1, "12345678.905")       # 12345678.91
        invoice.add("Service", 3, "3333333.335")      # 10000000.01
        # 20% of each line: 2469135.782 -> 2469135.78 and 2000000.002 -> 2000000.00
        assert_cents(self, invoice.subtotal, "22345678.92")
        assert_cents(self, invoice.tax, "4469135.78")
        assert_cents(self, invoice.total, "26814814.70")

    def test_empty_invoice_totals_are_decimal_zero(self):
        invoice = Invoice("INV-0", "Acme")
        assert_cents(self, invoice.subtotal, "0.00")
        assert_cents(self, invoice.tax, "0.00")
        assert_cents(self, invoice.total, "0.00")
        self.assertEqual(invoice.tax_by_category(), {})

    def test_totals_are_consistent(self):
        invoice = Invoice("INV-5", "Acme")
        invoice.add("A", 3, "0.335")
        invoice.add("B", 1, "0.625", category="reduced")
        invoice.add("C", 2, "1.125", category="reduced", discount_percent=10)
        self.assertEqual(invoice.subtotal, sum((item.total for item in invoice.items), D(0)))
        self.assertEqual(invoice.tax, sum(invoice.tax_by_category().values(), D(0)))
        self.assertEqual(invoice.total, invoice.subtotal + invoice.tax)
        # 1.005 -> 1.01, 0.625 -> 0.63, 2.025 -> 2.03
        assert_cents(self, invoice.subtotal, "3.67")
        # 1.01 * 20% = 0.202 -> 0.20; 0.63 * 5% = 0.0315 -> 0.03; 2.03 * 5% = 0.1015 -> 0.10
        assert_cents(self, invoice.tax, "0.33")


AMOUNT = re.compile(r"\d{1,3}(,\d{3})*\.\d{2}")


def sample_invoice():
    invoice = Invoice("INV-2024-017", "Northwind Traders")
    invoice.add("Espresso machine", 1, "1249.99")
    invoice.add("Coffee beans 1kg", 4, "18.25", category="reduced", discount_percent=10)
    invoice.add("Descaling service", 1, 85)
    return invoice


class ReportTests(unittest.TestCase):
    def test_item_rows_show_two_decimals(self):
        report = render(sample_invoice())
        espresso = row(report, "Espresso machine")
        self.assertIn("1,249.99", espresso)
        self.assertEqual(espresso[-1], "1,249.99")
        coffee = row(report, "Coffee beans")
        self.assertIn("18.25", coffee)
        self.assertEqual(coffee[-1], "65.70")
        descaling = row(report, "Descaling service")
        self.assertIn("85.00", descaling)
        self.assertEqual(descaling[-1], "85.00")

    def test_summary_rows(self):
        report = render(sample_invoice())
        self.assertEqual(total_row(report, "Subtotal"), "1,400.69")
        self.assertEqual(total_row(report, "standard tax (20%)"), "267.00")
        self.assertEqual(total_row(report, "reduced tax (5%)"), "3.29")
        self.assertEqual(total_row(report, "Tax"), "270.29")
        self.assertEqual(total_row(report, "Total"), "1,670.98")

    def test_no_float_artifacts(self):
        invoice = Invoice("INV-6", "Acme")
        invoice.add("A", 1, "0.10")
        invoice.add("B", 1, "0.20", category="zero")
        report = render(invoice)
        self.assertNotIn("0000", report)
        self.assertEqual(total_row(report, "Subtotal"), "0.30")
        self.assertEqual(total_row(report, "standard tax (20%)"), "0.02")
        self.assertEqual(total_row(report, "zero tax (0%)"), "0.00")
        self.assertEqual(total_row(report, "Tax"), "0.02")
        self.assertEqual(total_row(report, "Total"), "0.32")

    def test_unit_prices_and_amounts_round_half_up(self):
        invoice = Invoice("INV-7", "Acme")
        invoice.add("Screw", 1, "0.125")
        report = render(invoice)
        screw = row(report, "Screw")
        self.assertEqual(screw[-2:], ["0.13", "0.13"])
        self.assertEqual(total_row(report, "Subtotal"), "0.13")
        # 0.13 * 20% = 0.026
        self.assertEqual(total_row(report, "Tax"), "0.03")
        self.assertEqual(total_row(report, "Total"), "0.16")

    def test_large_amounts_get_separators(self):
        invoice = Invoice("INV-8", "Acme")
        invoice.add("Plant", 1, "12345678.905")
        invoice.add("Service", 3, "3333333.335")
        report = render(invoice)
        self.assertEqual(row(report, "Plant")[-2:], ["12,345,678.91", "12,345,678.91"])
        self.assertEqual(row(report, "Service")[-2:], ["3,333,333.34", "10,000,000.01"])
        self.assertEqual(total_row(report, "Subtotal"), "22,345,678.92")
        self.assertEqual(total_row(report, "Tax"), "4,469,135.78")
        self.assertEqual(total_row(report, "Total"), "26,814,814.70")

    def test_every_number_with_a_point_is_a_formatted_amount(self):
        invoice = sample_invoice()
        invoice.add("Paper", 7, "1.235", category="reduced", discount_percent=10)
        invoice.add("Gift", 1, 0, category="zero")
        invoice.add("Rounding", 3, "0.335")
        report = render(invoice)
        for line in report.splitlines()[4:]:
            for token in line.split():
                token = token.strip("()")
                if token.endswith("%") or not re.fullmatch(r"[\d,.]*\d[\d,.]*", token):
                    continue
                if "." in token:
                    with self.subTest(line=line, token=token):
                        self.assertRegex(token, r"^\d{1,3}(,\d{3})*\.\d{2}$")
        self.assertEqual(row(report, "Gift")[-2:], ["0.00", "0.00"])
        self.assertEqual(row(report, "Rounding")[-1], "1.01")
        self.assertEqual(row(report, "Paper")[-1], "7.78")

    def test_empty_invoice(self):
        report = render(Invoice("INV-0", "Acme"))
        self.assertEqual(total_row(report, "Subtotal"), "0.00")
        self.assertEqual(total_row(report, "Tax"), "0.00")
        self.assertEqual(total_row(report, "Total"), "0.00")

    def test_summary_line_total(self):
        self.assertTrue(summary(sample_invoice()).endswith("total EUR 1,670.98"))
        invoice = Invoice("INV-9", "Acme")
        invoice.add("A", 1, "0.10", category="zero")
        invoice.add("B", 1, "0.20", category="zero")
        self.assertTrue(summary(invoice).endswith("total EUR 0.30"))
        self.assertTrue(summary(Invoice("INV-0", "Acme")).endswith("total EUR 0.00"))


if __name__ == "__main__":
    unittest.main()
