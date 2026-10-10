"""Command line interface: ``python3 -m logsum FILE...``."""

from __future__ import annotations

import argparse
import sys
from typing import Sequence, TextIO

from .parser import iter_entries
from .report import render_text
from .summary import Summary


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
    return parser


def _read(summary: Summary, stream: TextIO) -> None:
    summary.update(iter_entries(stream))


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    summary = Summary()
    for name in args.files:
        if name == "-":
            _read(summary, sys.stdin)
            continue
        try:
            with open(name, encoding="utf-8", errors="replace") as stream:
                _read(summary, stream)
        except OSError as exc:
            parser.error(f"cannot read {name}: {exc.strerror}")

    sys.stdout.write(render_text(summary))
    return 0
