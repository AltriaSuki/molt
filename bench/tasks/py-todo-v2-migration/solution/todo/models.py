"""Domain model: a to-do item and the list that holds them."""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import date
from typing import Any, Iterable, Iterator

_TAG_RE = re.compile(r"[A-Za-z0-9_-]+")
_DATE_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")


def normalize_tag(raw: str) -> str:
    """The stored form of a tag: lowercase. Raises ValueError if it is invalid.

    A tag is one or more ASCII letters, digits, '-' or '_'.
    """
    if not isinstance(raw, str) or not _TAG_RE.fullmatch(raw):
        raise ValueError(f"invalid tag {raw!r} (use letters, digits, '-' and '_')")
    return raw.lower()


def normalize_tags(raw: Iterable[str]) -> list[str]:
    """Normalize several tags: lowercase, without duplicates, sorted."""
    return sorted({normalize_tag(tag) for tag in raw})


def parse_date(raw: str) -> date:
    """Parse a date written exactly as YYYY-MM-DD. Raises ValueError otherwise."""
    # date.fromisoformat alone also accepts spellings such as 20260105.
    if not isinstance(raw, str) or not _DATE_RE.fullmatch(raw):
        raise ValueError(f"invalid date {raw!r} (expected YYYY-MM-DD)")
    try:
        return date.fromisoformat(raw)
    except ValueError:
        raise ValueError(f"invalid date {raw!r} (no such day)") from None


class ItemNotFound(LookupError):
    """Raised when an id does not name an item in the list."""

    def __init__(self, item_id: int) -> None:
        super().__init__(item_id)
        self.item_id = item_id

    def __str__(self) -> str:
        return f"no item with id {self.item_id}"


@dataclass
class Item:
    id: int
    text: str
    done: bool = False
    tags: list[str] = field(default_factory=list)
    due: date | None = None

    def has_tag(self, tag: str) -> bool:
        return tag.lower() in self.tags

    def is_overdue(self, today: date) -> bool:
        return self.due is not None and self.due < today

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "text": self.text,
            "done": self.done,
            "tags": list(self.tags),
            "due": self.due.isoformat() if self.due is not None else None,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Item":
        """Build an item from its stored form; raises ValueError if malformed.

        ``tags`` and ``due`` may be missing (version 1 items have neither).
        """
        if not isinstance(data, dict):
            raise ValueError(f"expected an object, got {type(data).__name__}")
        item_id = data.get("id")
        text = data.get("text")
        done = data.get("done", False)
        # bool is a subclass of int, so rule it out explicitly.
        if not isinstance(item_id, int) or isinstance(item_id, bool) or item_id < 1:
            raise ValueError(f"bad id {item_id!r}")
        if not isinstance(text, str) or not text.strip():
            raise ValueError(f"item {item_id}: bad text {text!r}")
        if not isinstance(done, bool):
            raise ValueError(f"item {item_id}: bad done flag {done!r}")
        tags = data.get("tags", [])
        if not isinstance(tags, list):
            raise ValueError(f"item {item_id}: tags must be a list")
        due = data.get("due")
        try:
            return cls(
                id=item_id,
                text=text,
                done=done,
                tags=normalize_tags(tags),
                due=parse_date(due) if due is not None else None,
            )
        except ValueError as exc:
            raise ValueError(f"item {item_id}: {exc}") from None


class TodoList:
    """An ordered collection of items, oldest first.

    ``next_id`` is the id the next added item gets. It only ever grows, so an
    id is never handed out twice, even after the item that had it is removed.
    """

    def __init__(self, items: Iterable[Item] = (), next_id: int | None = None) -> None:
        self._items: list[Item] = []
        for item in items:
            if any(existing.id == item.id for existing in self._items):
                raise ValueError(f"duplicate id {item.id}")
            self._items.append(item)
        highest = max((item.id for item in self._items), default=0)
        if next_id is None:
            next_id = highest + 1
        elif next_id <= highest:
            raise ValueError(f"next_id {next_id} is not above the highest id {highest}")
        self._next_id = next_id

    def __iter__(self) -> Iterator[Item]:
        return iter(self._items)

    def __len__(self) -> int:
        return len(self._items)

    @property
    def next_id(self) -> int:
        return self._next_id

    def get(self, item_id: int) -> Item:
        for item in self._items:
            if item.id == item_id:
                return item
        raise ItemNotFound(item_id)

    def add(self, text: str, tags: Iterable[str] = (), due: date | None = None) -> Item:
        text = text.strip()
        if not text:
            raise ValueError("text must not be empty")
        item = Item(id=self._next_id, text=text, tags=normalize_tags(tags), due=due)
        self._items.append(item)
        self._next_id += 1
        return item

    def complete(self, item_id: int) -> Item:
        item = self.get(item_id)
        item.done = True
        return item

    def remove(self, item_id: int) -> Item:
        item = self.get(item_id)
        self._items.remove(item)
        return item

    def tag(self, item_id: int, tags: Iterable[str]) -> Item:
        new = normalize_tags(tags)
        item = self.get(item_id)
        item.tags = sorted(set(item.tags).union(new))
        return item

    def untag(self, item_id: int, tags: Iterable[str]) -> Item:
        gone = set(normalize_tags(tags))
        item = self.get(item_id)
        item.tags = [tag for tag in item.tags if tag not in gone]
        return item

    def open_items(self) -> list[Item]:
        return [item for item in self._items if not item.done]

    def select(
        self,
        *,
        include_done: bool = False,
        tag: str | None = None,
        overdue_on: date | None = None,
    ) -> list[Item]:
        """The items passing every given filter, in list order.

        ``tag`` keeps items carrying that tag (case-insensitive);
        ``overdue_on`` keeps items due strictly before that day.
        """
        wanted = normalize_tag(tag) if tag is not None else None
        return [
            item
            for item in self._items
            if (include_done or not item.done)
            and (wanted is None or wanted in item.tags)
            and (overdue_on is None or item.is_overdue(overdue_on))
        ]
