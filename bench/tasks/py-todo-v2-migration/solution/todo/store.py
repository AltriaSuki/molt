"""JSON file storage for a TodoList.

The file holds one JSON object, format version 2::

    {"version": 2, "next_id": 3, "items": [
        {"id": 1, "text": "Buy milk", "done": false, "tags": ["home"], "due": "2026-01-05"}
    ]}

Version 1 files (``{"version": 1, "items": [{"id", "text", "done"}]}``) are
still read: their items get no tags and no due date, and ``next_id`` is the
highest id plus 1. The first save over a version 1 file copies it to
``PATH + ".v1.bak"`` (unless that backup already exists) before writing
version 2.

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

FORMAT_VERSION = 2
READABLE_VERSIONS = (1, 2)
BACKUP_SUFFIX = ".v1.bak"


class StoreError(Exception):
    """The file exists but cannot be used as a to-do list."""


class UnsupportedVersion(StoreError):
    """The file is in a format version this program does not know."""

    def __init__(self, path: str, version: int) -> None:
        super().__init__(
            f"{path}: unsupported format version {version} "
            f"(this todo reads versions {', '.join(map(str, READABLE_VERSIONS))})"
        )
        self.version = version


def backup_path(path: str) -> str:
    return path + BACKUP_SUFFIX


def _read(path: str) -> bytes | None:
    try:
        with open(path, "rb") as fh:
            return fh.read()
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise StoreError(f"cannot read {path}: {exc.strerror}") from exc


def _parse(raw: bytes, path: str) -> dict:
    try:
        data = json.loads(raw.decode("utf-8"))
    except UnicodeDecodeError as exc:
        raise StoreError(f"{path}: not UTF-8 text") from exc
    except json.JSONDecodeError as exc:
        raise StoreError(f"{path}: not valid JSON ({exc.msg}, line {exc.lineno})") from exc
    if not isinstance(data, dict):
        raise StoreError(f"{path}: expected a JSON object at the top level")
    return data


def _version(data: dict, path: str) -> int:
    version = data.get("version")
    if not isinstance(version, int) or isinstance(version, bool):
        raise StoreError(f"{path}: missing or malformed format version {version!r}")
    if version not in READABLE_VERSIONS:
        raise UnsupportedVersion(path, version)
    return version


def load(path: str) -> TodoList:
    raw = _read(path)
    if raw is None:
        return TodoList()
    data = _parse(raw, path)
    return _decode(data, _version(data, path), path)


def _decode(data: dict, version: int, path: str) -> TodoList:
    entries = data.get("items")
    if not isinstance(entries, list):
        raise StoreError(f"{path}: 'items' must be a list")
    if version == 1:
        next_id = None  # migrated: one past the highest id
    else:
        next_id = data.get("next_id")
        if not isinstance(next_id, int) or isinstance(next_id, bool) or next_id < 1:
            raise StoreError(f"{path}: bad next_id {next_id!r}")
    try:
        return TodoList((Item.from_dict(entry) for entry in entries), next_id=next_id)
    except ValueError as exc:
        raise StoreError(f"{path}: {exc}") from exc


def _encode(todos: TodoList) -> dict:
    return {
        "version": FORMAT_VERSION,
        "next_id": todos.next_id,
        "items": [item.to_dict() for item in todos],
    }


def _back_up_version_1(path: str) -> None:
    """Copy a version 1 file at ``path`` to its backup, unless one exists."""
    backup = backup_path(path)
    if os.path.lexists(backup):
        return
    raw = _read(path)
    if raw is None:
        return
    try:
        version = _version(_parse(raw, path), path)
    except StoreError:
        return
    if version == 1:
        with open(backup, "xb") as fh:
            fh.write(raw)


def save(path: str, todos: TodoList) -> None:
    _back_up_version_1(path)
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
