import unittest
from decimal import Decimal

from invoicing.models import Invoice, LineItem
from invoicing.tax import TaxTable, UnknownCategoryError


class LineItemTests(unittest.TestCase):
    def test_total_is_quantity_times_price(self):
        item = LineItem("Widget", 3, "19.99")
        self.assertEqual(item.total, Decimal("59.97"))

    def test_discount_is_taken_off(self):
        item = LineItem("Widget", 2, 50, discount_percent=10)
        self.assertEqual(item.total, Decimal("90.00"))

    def test_float_price_is_rejected(self):
        with self.assertRaises(TypeError):
            LineItem("Widget", 1, 0.1)

    def test_category_defaults_to_standard(self):
        self.assertEqual(LineItem("Widget", 1, 5).category, "standard")

    def test_quantity_must_be_a_positive_int(self):
        for quantity in (0, -1):
            with self.subTest(quantity=quantity), self.assertRaises(ValueError):
                LineItem("Widget", quantity, 5)
        for quantity in (1.5, "2", True):
            with self.subTest(quantity=quantity), self.assertRaises(TypeError):
                LineItem("Widget", quantity, 5)

    def test_negative_price_is_rejected(self):
        with self.assertRaises(ValueError):
            LineItem("Refund", 1, -5)

    def test_discount_must_be_a_percentage(self):
        for discount in (-5, 101):
            with self.subTest(discount=discount), self.assertRaises(ValueError):
                LineItem("Widget", 1, 5, discount_percent=discount)

    def test_description_must_not_be_blank(self):
        with self.assertRaises(ValueError):
            LineItem("   ", 1, 5)


class InvoiceTests(unittest.TestCase):
    def test_add_returns_the_item_and_keeps_order(self):
        invoice = Invoice("INV-1", "Acme")
        first = invoice.add("Consulting", 10, 100)
        second = invoice.add("Book", 2, 15, category="reduced")
        self.assertEqual(invoice.items, [first, second])
        self.assertEqual(second.category, "reduced")

    def test_totals(self):
        invoice = Invoice("INV-1", "Acme")
        invoice.add("Consulting", 10, 100)
        invoice.add("Book", 2, 15, category="reduced")
        invoice.add("Bread", 3, 4, category="zero")
        self.assertEqual(invoice.subtotal, Decimal("1042.00"))
        self.assertEqual(invoice.tax, Decimal("201.50"))
        self.assertEqual(invoice.total, Decimal("1243.50"))

    def test_unknown_category_is_rejected_when_added(self):
        invoice = Invoice("INV-1", "Acme")
        with self.assertRaises(UnknownCategoryError):
            invoice.add("Yacht", 1, 100, category="luxury")
        self.assertEqual(invoice.items, [])

    def test_custom_tax_table(self):
        invoice = Invoice("INV-2", "Acme", tax_table=TaxTable({"books": 7}))
        invoice.add("Novel", 1, 100, category="books")
        self.assertEqual(invoice.tax, 7)
        self.assertEqual(invoice.total, 107)


if __name__ == "__main__":
    unittest.main()
