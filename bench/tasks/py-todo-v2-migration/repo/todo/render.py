"""Plain-text rendering of items for the terminal."""

from __future__ import annotations

from typing import Iterable

from .models import Item


def format_item(item: Item) -> str:
    """One line for one item, e.g. ``[ ] 3 Buy milk`` or ``[x] 4 Call the bank``."""
    mark = "x" if item.done else " "
    return f"[{mark}] {item.id} {item.text}"


def render_list(items: Iterable[Item]) -> str:
    """All items, one line each, every line ending in a newline."""
    return "".join(format_item(item) + "\n" for item in items)
