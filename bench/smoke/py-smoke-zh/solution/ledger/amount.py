"""金额的解析。"""

import re
from decimal import Decimal

# 整数部分要么不带逗号，要么每三位一个逗号。
_AMOUNT = re.compile(r"-?(\d+|\d{1,3}(,\d{3})+)(\.\d+)?")


def parse_amount(text):
    """把 `text` 解析为 Decimal，比如 "1,234.50" 解析为 Decimal("1234.50")。"""
    if not _AMOUNT.fullmatch(text):
        raise ValueError(f"bad amount {text!r}")
    return Decimal(text.replace(",", ""))
