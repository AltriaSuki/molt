"""JSON file storage for a TodoList.

The file holds one JSON object, format version 1::

    {"version": 1, "items": [{"id": 1, "text": "Buy milk", "done": false}]}

A file that does not exist yet is an empty list. Saving writes a temporary
file next to the target and renames it into place, so an interrupted save
never leaves a half-written list behind.
"""

from __future__ import annotations

import contextlib
import json
import os
import tempfile

from .models import Item, TodoList

FORMAT_VERSION = 1


class StoreError(Exception):
    """The file exists but cannot be used as a to-do list."""


def load(path: str) -> TodoList:
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
    except FileNotFoundError:
        return TodoList()
    except json.JSONDecodeError as exc:
        raise StoreError(f"{path}: not valid JSON ({exc.msg}, line {exc.lineno})") from exc
    except OSError as exc:
        raise StoreError(f"cannot read {path}: {exc.strerror}") from exc
    return _decode(data, path)


def _decode(data: object, path: str) -> TodoList:
    if not isinstance(data, dict):
        raise StoreError(f"{path}: expected a JSON object at the top level")
    version = data.get("version")
    if version != FORMAT_VERSION or isinstance(version, bool):
        raise StoreError(f"{path}: unsupported format version {version!r}")
    entries = data.get("items")
    if not isinstance(entries, list):
        raise StoreError(f"{path}: 'items' must be a list")
    try:
        return TodoList(Item.from_dict(entry) for entry in entries)
    except ValueError as exc:
        raise StoreError(f"{path}: {exc}") from exc


def _encode(todos: TodoList) -> dict:
    return {
        "version": FORMAT_VERSION,
        "items": [item.to_dict() for item in todos],
    }


def save(path: str, todos: TodoList) -> None:
    directory = os.path.dirname(os.path.abspath(path))
    fd, tmp_path = tempfile.mkstemp(prefix=".todo-", suffix=".tmp", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(_encode(todos), fh, indent=2)
            fh.write("\n")
        os.replace(tmp_path, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp_path)
        raise
