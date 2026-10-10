"""Command line: resolve requirements against an index file, or check a lock.

    python3 -m pkgresolve resolve INDEX.json REQ [REQ ...]
    python3 -m pkgresolve check INDEX.json LOCK.txt REQ [REQ ...]

`resolve` prints a lock file (name==version lines) and exits 0, or prints the
reason to stderr and exits 1. `check` prints "ok" if the lock is a valid
resolution of the requirements, or one problem per line and exits 1.
Unreadable files or invalid input exit 2.
"""

from __future__ import annotations

import argparse
import sys

from .index import PackageIndex
from .lock import format_lock, parse_lock, verify_lock
from .resolver import ResolutionError, resolve


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="pkgresolve", description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("resolve", help="resolve requirements and print a lock file")
    run.add_argument("index", help="index JSON file")
    run.add_argument("requirements", nargs="+", metavar="REQ")

    check = commands.add_parser("check", help="check a lock file against the requirements")
    check.add_argument("index", help="index JSON file")
    check.add_argument("lock", help="lock file")
    check.add_argument("requirements", nargs="+", metavar="REQ")
    return parser


def main(argv=None, stdout=None, stderr=None) -> int:
    stdout = stdout or sys.stdout
    stderr = stderr or sys.stderr
    args = _parser().parse_args(argv)
    try:
        index = PackageIndex.load(args.index)
        if args.command == "resolve":
            try:
                resolution = resolve(index, args.requirements)
            except ResolutionError as exc:
                print(f"error: {exc}", file=stderr)
                return 1
            stdout.write(format_lock(resolution))
            return 0
        with open(args.lock, encoding="utf-8") as handle:
            lock = parse_lock(handle.read())
        problems = verify_lock(index, args.requirements, lock)
    except (OSError, ValueError, TypeError) as exc:
        print(f"error: {exc}", file=stderr)
        return 2
    for problem in problems:
        print(problem, file=stdout)
    if problems:
        return 1
    print("ok", file=stdout)
    return 0
