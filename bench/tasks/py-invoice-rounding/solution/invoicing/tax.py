"""Tax rates per product category.

Rates are percentages: ``20`` means 20%. They accept the same input as
amounts (ints, strs and Decimals) and are stored as Decimals. Every line item
names a category, and the invoice looks its rate up in a :class:`TaxTable`.
"""

from .money import round_cents, to_amount

#: Rates used when an invoice is created without its own table.
DEFAULT_RATES = {
    "standard": 20,
    "reduced": 5,
    "zero": 0,
}


class UnknownCategoryError(KeyError):
    """Raised when a category has no tax rate."""

    def __init__(self, category):
        super().__init__(category)
        self.category = category

    def __str__(self):
        return f"no tax rate for category {self.category!r}"


class TaxTable:
    """Maps product categories to tax rates (in percent)."""

    def __init__(self, rates=None):
        self._rates = {}
        source = DEFAULT_RATES if rates is None else rates
        for category, rate in source.items():
            self.set_rate(category, rate)

    def set_rate(self, category, rate):
        """Set (or replace) the rate for ``category``."""
        if not isinstance(category, str) or not category:
            raise ValueError("category must be a non-empty string")
        rate = to_amount(rate)
        if not 0 <= rate <= 100:
            raise ValueError(
                f"tax rate for {category!r} must be between 0 and 100, got {rate}"
            )
        self._rates[category] = rate

    def rate(self, category):
        """The rate for ``category``; raises :class:`UnknownCategoryError`."""
        try:
            return self._rates[category]
        except KeyError:
            raise UnknownCategoryError(category) from None

    def categories(self):
        """Known categories, in the order they were added."""
        return list(self._rates)

    def tax_on(self, amount, category):
        """Tax due on ``amount`` in ``category``, rounded half up to the cent."""
        return round_cents(to_amount(amount) * self.rate(category) / 100)

    def __repr__(self):
        return f"TaxTable({self._rates!r})"
