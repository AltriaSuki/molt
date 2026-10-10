"""Aggregation of log entries into a summary."""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass, field
from typing import Iterable

from .parser import Entry

STATUS_CLASSES = ("2xx", "3xx", "4xx", "5xx")
TOP_PATHS = 5


def normalize_path(path: str) -> str:
    """The path a request is counted under: its query string is dropped."""
    return path.split("?", 1)[0]


@dataclass
class Summary:
    """Running totals over a stream of :class:`Entry` objects."""

    requests: int = 0
    bytes: int = 0
    total_duration_ms: int = 0
    max_duration_ms: int = 0
    statuses: Counter = field(default_factory=Counter)
    paths: Counter = field(default_factory=Counter)

    def add(self, entry: Entry) -> None:
        self.requests += 1
        self.bytes += entry.bytes
        self.total_duration_ms += entry.duration_ms
        self.max_duration_ms = max(self.max_duration_ms, entry.duration_ms)
        self.statuses[entry.status_class] += 1
        self.paths[normalize_path(entry.path)] += 1

    def update(self, entries: Iterable[Entry]) -> None:
        for entry in entries:
            self.add(entry)

    def status_counts(self) -> dict[str, int]:
        """Requests per status class, with every class present."""
        return {cls: self.statuses[cls] for cls in STATUS_CLASSES}

    def top_paths(self, n: int = TOP_PATHS) -> list[tuple[str, int]]:
        """The ``n`` most requested paths as ``(path, count)`` pairs.

        Ordered by count, highest first; paths with the same count are
        ordered alphabetically.
        """
        ranked = sorted(self.paths.items(), key=lambda item: (-item[1], item[0]))
        return ranked[:n]

    @property
    def avg_duration_ms(self) -> float | None:
        if not self.requests:
            return None
        return self.total_duration_ms / self.requests


def summarize(entries: Iterable[Entry]) -> Summary:
    summary = Summary()
    summary.update(entries)
    return summary
