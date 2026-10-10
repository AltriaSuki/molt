"""Invoices and their line items."""

from dataclasses import dataclass, field
from decimal import Decimal

from .money import CENT, round_cents, to_amount
from .tax import TaxTable

#: Zero, with the exponent of a rounded amount, so sums over no items still
#: come out as ``Decimal("0.00")``.
ZERO = Decimal(0).quantize(CENT)


@dataclass
class LineItem:
    """One billed line: ``quantity`` units at ``unit_price`` each.

    ``discount_percent`` is taken off the line (``10`` means 10% off), and
    ``category`` selects the tax rate (see :mod:`invoicing.tax`). Prices and
    discounts accept ints, strs and Decimals and are stored as Decimals.
    """

    description: str
    quantity: int
    unit_price: Decimal
    category: str = "standard"
    discount_percent: Decimal = Decimal(0)

    def __post_init__(self):
        if not isinstance(self.description, str) or not self.description.strip():
            raise ValueError("description must not be empty")
        if isinstance(self.quantity, bool) or not isinstance(self.quantity, int):
            raise TypeError("quantity must be an int")
        if self.quantity <= 0:
            raise ValueError("quantity must be positive")
        self.unit_price = to_amount(self.unit_price)
        if self.unit_price < 0:
            raise ValueError("unit_price must not be negative")
        self.discount_percent = to_amount(self.discount_percent)
        if not 0 <= self.discount_percent <= 100:
            raise ValueError("discount_percent must be between 0 and 100")

    @property
    def gross(self):
        """Quantity times unit price, before the discount (unrounded)."""
        return self.quantity * self.unit_price

    @property
    def discount(self):
        """The amount taken off by ``discount_percent`` (unrounded)."""
        return self.gross * self.discount_percent / 100

    @property
    def total(self):
        """What the line bills, after the discount, rounded to the cent.

        Rounding happens once, on the discounted amount.
        """
        return round_cents(self.gross - self.discount)


@dataclass
class Invoice:
    """An invoice for one customer, made of line items."""

    number: str
    customer: str
    currency: str = "EUR"
    items: list = field(default_factory=list)
    tax_table: TaxTable = field(default_factory=TaxTable)

    def add(self, description, quantity, unit_price, category="standard",
            discount_percent=0):
        """Create a :class:`LineItem`, append it and return it."""
        item = LineItem(description, quantity, unit_price, category, discount_percent)
        self.add_item(item)
        return item

    def add_item(self, item):
        """Append an existing line item."""
        # Look the rate up now so an unknown category fails here and not
        # when the invoice is rendered.
        self.tax_table.rate(item.category)
        self.items.append(item)

    @property
    def subtotal(self):
        """Sum of the line totals, before tax."""
        return sum((item.total for item in self.items), ZERO)

    def line_tax(self, item):
        """Tax on one line: its rounded total times its rate, rounded."""
        return self.tax_table.tax_on(item.total, item.category)

    def tax_by_category(self):
        """Tax due per category, in the order the categories first appear.

        Each category's tax is the sum of its lines' taxes.
        """
        taxes = {}
        for item in self.items:
            taxes[item.category] = taxes.get(item.category, ZERO) + self.line_tax(item)
        return taxes

    @property
    def tax(self):
        """Total tax due: the sum of every line's tax."""
        return sum(self.tax_by_category().values(), ZERO)

    @property
    def total(self):
        """Subtotal plus tax."""
        return self.subtotal + self.tax
