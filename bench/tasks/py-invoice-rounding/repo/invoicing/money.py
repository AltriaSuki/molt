"""Helpers for money amounts.

``to_amount`` is the one place where user input becomes an amount, so the
models and the tax table all go through it for validation.
"""

import math

#: Decimal places money is rounded to.
PLACES = 2


def to_amount(value):
    """Return ``value`` as an amount.

    Accepts ints, floats and numeric strings (surrounding whitespace is
    ignored). Raises ``TypeError`` for any other type, bools included, and
    ``ValueError`` for strings that are not numbers and for NaN or infinity.
    """
    if isinstance(value, bool):
        raise TypeError("amount must be a number, not bool")
    if isinstance(value, str):
        try:
            amount = float(value.strip())
        except ValueError:
            raise ValueError(f"invalid amount: {value!r}") from None
    elif isinstance(value, (int, float)):
        amount = float(value)
    else:
        raise TypeError(
            f"amount must be a number or a numeric string, not {type(value).__name__}"
        )
    if not math.isfinite(amount):
        raise ValueError(f"amount must be finite, got {value!r}")
    return amount


def round_cents(value):
    """Round an amount to whole cents."""
    return round(to_amount(value), PLACES)


def format_amount(value):
    """Format an amount for display, with a thousands separator: 1,234.56."""
    return f"{round_cents(value):,}"
