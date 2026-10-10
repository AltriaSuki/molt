# invoicing

A small library for building customer invoices: line items with a quantity,
a unit price and an optional percentage discount, tax rates per product
category, and a fixed-width text rendering used in invoice emails and the
billing log.

```python
from invoicing import Invoice, render

invoice = Invoice("INV-2024-017", "Northwind Traders")
invoice.add("Espresso machine", 1, "1249.99")
invoice.add("Coffee beans 1kg", 4, "18.25", category="reduced", discount_percent=10)
print(render(invoice))
```

Amounts are `decimal.Decimal`. Prices, discounts and tax rates accept an
`int`, a `str` or a `Decimal`; floats are refused with a `TypeError`.
Rounding to the cent is half up (0.125 becomes 0.13), once per line total and
once per line tax.

## Modules

- `invoicing/money.py`: turning input into amounts (`to_amount`), rounding to
  the cent (`round_cents`) and display formatting (`format_amount`).
- `invoicing/tax.py`: `TaxTable`, the tax rate (in percent) for each product
  category. The defaults are `standard` 20%, `reduced` 5% and `zero` 0%.
- `invoicing/models.py`: `LineItem` and `Invoice`, with the subtotal, the
  tax per category and the total.
- `invoicing/report.py`: `render(invoice)`, the full text invoice, and
  `summary(invoice)`, a one-line description for listings.

## Running the tests

Python 3.11 or newer, standard library only:

```
python3 -m unittest -q tests.test_money tests.test_models tests.test_tax tests.test_report
```
