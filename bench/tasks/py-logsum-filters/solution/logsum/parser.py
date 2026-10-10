"""Parsing of access log lines.

Every non-blank line of an access log describes one request, as six fields
separated by whitespace::

    2024-03-05T14:02:11 GET /index.html 200 5120 12

timestamp, method, path, status, bytes and duration in milliseconds. See the
README for the exact rules each field must follow.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from typing import Callable, Iterable, Iterator

TIMESTAMP_FORMAT = "%Y-%m-%dT%H:%M:%S"
FIELDS = ("timestamp", "method", "path", "status", "bytes", "duration_ms")
MIN_STATUS = 200
MAX_STATUS = 599


class ParseError(ValueError):
    """A line that is not a valid log entry."""


@dataclass(frozen=True)
class Entry:
    """One request from an access log."""

    timestamp: datetime
    method: str
    path: str
    status: int
    bytes: int
    duration_ms: int

    @property
    def status_class(self) -> str:
        """The status class, e.g. ``"4xx"`` for a 404."""
        return f"{self.status // 100}xx"


def _parse_count(text: str, field: str) -> int:
    if not (text.isascii() and text.isdigit()):
        raise ParseError(f"{field} must be a non-negative integer, got {text!r}")
    return int(text)


def parse_line(line: str) -> Entry:
    """Parse one log line into an :class:`Entry`.

    Raises :class:`ParseError` when the line does not follow the format.
    """
    fields = line.split()
    if len(fields) != len(FIELDS):
        raise ParseError(f"expected {len(FIELDS)} fields, got {len(fields)}")
    stamp, method, path, status, size, duration = fields

    try:
        timestamp = datetime.strptime(stamp, TIMESTAMP_FORMAT)
    except ValueError:
        raise ParseError(f"invalid timestamp {stamp!r}") from None

    if not (method.isascii() and method.isalpha() and method.isupper()):
        raise ParseError(f"invalid method {method!r}")
    if not path.startswith("/"):
        raise ParseError(f"path must start with '/', got {path!r}")

    code = _parse_count(status, "status")
    if not MIN_STATUS <= code <= MAX_STATUS:
        raise ParseError(f"status must be {MIN_STATUS}-{MAX_STATUS}, got {code}")

    return Entry(
        timestamp=timestamp,
        method=method,
        path=path,
        status=code,
        bytes=_parse_count(size, "bytes"),
        duration_ms=_parse_count(duration, "duration_ms"),
    )


def iter_entries(
    lines: Iterable[str],
    on_error: Callable[[str, ParseError], None] | None = None,
) -> Iterator[Entry]:
    """Yield the entries of ``lines``.

    Blank lines are ignored. Lines that do not parse are dropped; when
    ``on_error`` is given it is called with each such line and its error.
    """
    for line in lines:
        if not line.strip():
            continue
        try:
            entry = parse_line(line)
        except ParseError as exc:
            if on_error is not None:
                on_error(line, exc)
            continue
        yield entry
