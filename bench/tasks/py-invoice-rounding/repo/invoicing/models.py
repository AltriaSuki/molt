"""Invoices and their line items."""

from dataclasses import dataclass, field

from .money import round_cents, to_amount
from .tax import TaxTable


@dataclass
class LineItem:
    """One billed line: ``quantity`` units at ``unit_price`` each.

    ``discount_percent`` is taken off the line (``10`` means 10% off), and
    ``category`` selects the tax rate (see :mod:`invoicing.tax`).
    """

    description: str
    quantity: int
    unit_price: float
    category: str = "standard"
    discount_percent: float = 0

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
        """Quantity times unit price, before the discount."""
        return self.quantity * self.unit_price

    @property
    def discount(self):
        """The amount taken off by ``discount_percent``."""
        return self.gross * self.discount_percent / 100

    @property
    def total(self):
        """What the line bills, after the discount, rounded to the cent."""
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
        return sum(item.total for item in self.items)

    def tax_by_category(self):
        """Tax due per category, in the order the categories first appear."""
        bases = {}
        for item in self.items:
            bases[item.category] = bases.get(item.category, 0.0) + item.total
        return {
            category: self.tax_table.tax_on(base, category)
            for category, base in bases.items()
        }

    @property
    def tax(self):
        """Total tax due."""
        return sum(self.tax_by_category().values())

    @property
    def total(self):
        """Subtotal plus tax."""
        return self.subtotal + self.tax
