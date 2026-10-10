"""Command line interface: ``python3 -m logsum FILE...``."""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from datetime import datetime
from typing import Sequence, TextIO

from .parser import TIMESTAMP_FORMAT, Entry, ParseError, iter_entries
from .report import render_text
from .summary import Summary

_BOUND_FORMATS = (
    (re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}"), "%Y-%m-%d"),
    (re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"), TIMESTAMP_FORMAT),
)


def parse_bound(value: str) -> datetime:
    """A ``--since``/``--until`` value: ``YYYY-MM-DD`` or ``YYYY-MM-DDTHH:MM:SS``.

    A date alone means midnight at the start of that day.
    """
    for pattern, fmt in _BOUND_FORMATS:
        if pattern.fullmatch(value):
            try:
                return datetime.strptime(value, fmt)
            except ValueError:
                break
    raise argparse.ArgumentTypeError(
        f"invalid date/time {value!r} (expected YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)"
    )


@dataclass(frozen=True)
class Window:
    """The time range entries are counted in: ``since <= t < until``."""

    since: datetime | None = None
    until: datetime | None = None

    def __contains__(self, entry: Entry) -> bool:
        if self.since is not None and entry.timestamp < self.since:
            return False
        if self.until is not None and entry.timestamp >= self.until:
            return False
        return True


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="logsum",
        description="Summarize web access logs: requests per status class, "
        "total bytes, durations and the most requested paths.",
    )
    parser.add_argument(
        "files",
        nargs="+",
        metavar="FILE",
        help="access log to read; '-' reads standard input",
    )
    parser.add_argument(
        "--since",
        type=parse_bound,
        metavar="WHEN",
        help="only count requests at or after WHEN (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)",
    )
    parser.add_argument(
        "--until",
        type=parse_bound,
        metavar="WHEN",
        help="only count requests before WHEN (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print the summary as one JSON object",
    )
    return parser


def _read(summary: Summary, stream: TextIO, window: Window) -> None:
    def skip(line: str, error: ParseError) -> None:
        summary.skipped += 1

    for entry in iter_entries(stream, on_error=skip):
        if entry in window:
            summary.add(entry)


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    window = Window(args.since, args.until)

    summary = Summary()
    for name in args.files:
        if name == "-":
            _read(summary, sys.stdin, window)
            continue
        try:
            with open(name, encoding="utf-8", errors="replace") as stream:
                _read(summary, stream, window)
        except OSError as exc:
            parser.error(f"cannot read {name}: {exc.strerror}")

    if args.json:
        sys.stdout.write(json.dumps(summary.to_dict(), indent=2) + "\n")
    else:
        sys.stdout.write(render_text(summary))
    return 0
