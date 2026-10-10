"""Plain-text rendering of items for the terminal."""

from __future__ import annotations

from typing import Iterable

from .models import Item


def format_item(item: Item) -> str:
    """One line for one item.

    For example ``[ ] 3 Buy milk #home #shop (due 2026-01-05)`` or
    ``[x] 4 Call the bank``: tags in sorted order, the due date last.
    """
    mark = "x" if item.done else " "
    parts = [f"[{mark}] {item.id} {item.text}"]
    parts.extend(f"#{tag}" for tag in sorted(item.tags))
    if item.due is not None:
        parts.append(f"(due {item.due.isoformat()})")
    return " ".join(parts)


def render_list(items: Iterable[Item]) -> str:
    """All items, one line each, every line ending in a newline."""
    return "".join(format_item(item) + "\n" for item in items)
