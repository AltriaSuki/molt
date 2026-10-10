"""Domain model: a to-do item and the list that holds them."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Iterable, Iterator


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

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.id, "text": self.text, "done": self.done}

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Item":
        """Build an item from its stored form; raises ValueError if malformed."""
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
        return cls(id=item_id, text=text, done=done)


class TodoList:
    """An ordered collection of items, oldest first."""

    def __init__(self, items: Iterable[Item] = ()) -> None:
        self._items: list[Item] = []
        for item in items:
            if any(existing.id == item.id for existing in self._items):
                raise ValueError(f"duplicate id {item.id}")
            self._items.append(item)

    def __iter__(self) -> Iterator[Item]:
        return iter(self._items)

    def __len__(self) -> int:
        return len(self._items)

    def get(self, item_id: int) -> Item:
        for item in self._items:
            if item.id == item_id:
                return item
        raise ItemNotFound(item_id)

    def add(self, text: str) -> Item:
        text = text.strip()
        if not text:
            raise ValueError("text must not be empty")
        item = Item(id=self._next_id(), text=text)
        self._items.append(item)
        return item

    def complete(self, item_id: int) -> Item:
        item = self.get(item_id)
        item.done = True
        return item

    def remove(self, item_id: int) -> Item:
        item = self.get(item_id)
        self._items.remove(item)
        return item

    def open_items(self) -> list[Item]:
        return [item for item in self._items if not item.done]

    def _next_id(self) -> int:
        return max((item.id for item in self._items), default=0) + 1
