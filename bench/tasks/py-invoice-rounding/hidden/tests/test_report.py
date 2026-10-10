import unittest

from invoicing.models import Invoice
from invoicing.report import render, summary


def sample_invoice():
    invoice = Invoice("INV-2024-017", "Northwind Traders")
    invoice.add("Espresso machine", 1, "1249.99")
    invoice.add("Coffee beans 1kg", 4, "18.25", category="reduced", discount_percent=10)
    invoice.add("Descaling service", 1, 85)
    return invoice


def line_with(text, report):
    matches = [line for line in report.splitlines() if text in line]
    if len(matches) != 1:
        raise AssertionError(f"expected one line with {text!r}, got {matches}")
    return matches[0]


class RenderTests(unittest.TestCase):
    def test_header(self):
        lines = render(sample_invoice()).splitlines()
        self.assertEqual(lines[0], "INVOICE INV-2024-017")
        self.assertEqual(lines[1], "Customer: Northwind Traders")
        self.assertEqual(lines[2], "Currency: EUR")

    def test_one_row_per_item(self):
        report = render(sample_invoice())
        for description in ("Espresso machine", "Coffee beans 1kg", "Descaling service"):
            line_with(description, report)

    def test_item_row_shows_quantity_and_unit_price(self):
        row = line_with("Espresso machine", render(sample_invoice())).split()
        self.assertIn("1", row)
        self.assertIn("1,249.99", row)

    def test_discount_column(self):
        self.assertIn("10%", line_with("Coffee beans", render(sample_invoice())))
        self.assertNotIn("%", line_with("Espresso", render(sample_invoice())))

    def test_tax_breakdown_per_category(self):
        report = render(sample_invoice())
        line_with("standard tax (20%)", report)
        line_with("reduced tax (5%)", report)

    def test_summary_rows_come_last(self):
        lines = render(sample_invoice()).splitlines()
        self.assertTrue(lines[-1].startswith("Total "))
        self.assertTrue(lines[-2].startswith("Tax "))

    def test_long_descriptions_are_clipped(self):
        invoice = Invoice("INV-9", "Acme")
        invoice.add("A very long description that does not fit the column", 1, 1)
        row = render(invoice).splitlines()[5]
        self.assertTrue(row.startswith("A very long description t..."))

    def test_invoice_without_items(self):
        lines = render(Invoice("INV-0", "Acme")).splitlines()
        self.assertEqual(lines[4].split(), ["Description", "Qty", "Unit", "price", "Disc", "Amount"])
        self.assertTrue(lines[-1].startswith("Total "))


class SummaryTests(unittest.TestCase):
    def test_mentions_number_customer_and_item_count(self):
        text = summary(sample_invoice())
        self.assertTrue(text.startswith("INV-2024-017 for Northwind Traders: 3 items, total EUR "))

    def test_single_item(self):
        invoice = Invoice("INV-3", "Acme")
        invoice.add("Widget", 1, 5)
        self.assertIn(": 1 item,", summary(invoice))


if __name__ == "__main__":
    unittest.main()
