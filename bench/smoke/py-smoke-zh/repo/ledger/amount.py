"""金额的解析。"""

import re
from decimal import Decimal

_AMOUNT = re.compile(r"-?\d+(\.\d+)?")


def parse_amount(text):
    """把 `text` 解析为 Decimal，比如 "12.5" 解析为 Decimal("12.5")。"""
    if not _AMOUNT.fullmatch(text):
        raise ValueError(f"bad amount {text!r}")
    return Decimal(text)
