"""Helpers for money amounts.

Amounts are :class:`decimal.Decimal` values. ``to_amount`` is the one place
where user input becomes an amount, so the models and the tax table all go
through it for validation. Floats are refused: they cannot hold most cent
values exactly, which is how invoices end up a cent off.
"""

from decimal import ROUND_HALF_UP, Decimal, InvalidOperation

#: Decimal places money is rounded to.
PLACES = 2

#: The quantum amounts are rounded to.
CENT = Decimal(1).scaleb(-PLACES)


def to_amount(value):
    """Return ``value`` as a :class:`~decimal.Decimal`, unrounded.

    Accepts ints, numeric strings (surrounding whitespace is ignored) and
    Decimals. Raises ``TypeError`` for floats and any other type, bools
    included, and ``ValueError`` for strings that are not numbers and for
    NaN or infinity.
    """
    if isinstance(value, bool):
        raise TypeError("amount must be a number, not bool")
    if isinstance(value, float):
        raise TypeError(
            f"float amounts are not accepted ({value!r} cannot be represented "
            "exactly); pass a str or Decimal instead"
        )
    if isinstance(value, Decimal):
        amount = value
    elif isinstance(value, int):
        amount = Decimal(value)
    elif isinstance(value, str):
        try:
            amount = Decimal(value.strip())
        except InvalidOperation:
            raise ValueError(f"invalid amount: {value!r}") from None
    else:
        raise TypeError(
            "amount must be an int, a str or a Decimal, "
            f"not {type(value).__name__}"
        )
    if not amount.is_finite():
        raise ValueError(f"amount must be finite, got {value!r}")
    return amount


def round_cents(value):
    """Round an amount to whole cents, halves away from zero (0.125 -> 0.13)."""
    return to_amount(value).quantize(CENT, rounding=ROUND_HALF_UP)


def format_amount(value):
    """Format an amount for display: two decimals and a thousands separator."""
    return f"{round_cents(value):,.{PLACES}f}"
