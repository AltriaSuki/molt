"""Show when the jobs in a config file run next.

    python3 -m scheduler jobs.toml [--after 2024-05-01T09:30] [-n 10]
"""

from __future__ import annotations

import argparse
import sys
from datetime import datetime
from typing import Optional, Sequence, TextIO

from .config import ConfigError, load_config
from .cron import CronError
from .runner import Runner


def _timestamp(text: str) -> datetime:
    try:
        return datetime.strptime(text, "%Y-%m-%dT%H:%M")
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected YYYY-MM-DDTHH:MM, got {text!r}") from None


def main(argv: Optional[Sequence[str]] = None, out: TextIO = sys.stdout) -> int:
    parser = argparse.ArgumentParser(prog="python3 -m scheduler", description=__doc__.splitlines()[0])
    parser.add_argument("config", help="TOML file with [[jobs]] tables")
    parser.add_argument("--after", type=_timestamp, help="start time (default: now)")
    parser.add_argument("-n", type=int, default=10, help="how many jobs to list (default 10)")
    args = parser.parse_args(argv)

    start = args.after or datetime.now().replace(second=0, microsecond=0)
    try:
        registry = load_config(args.config)
        upcoming = Runner(registry, clock=lambda: start).upcoming(limit=args.n)
    except (ConfigError, CronError) as exc:
        print(f"scheduler: {exc}", file=sys.stderr)
        return 1

    if not upcoming:
        print("no enabled jobs", file=out)
    for when, job in upcoming:
        print(f"{when:%Y-%m-%d %H:%M}  {job.name}  {job.command}", file=out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
