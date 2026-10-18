"""Sum one column of a CSV file."""

import argparse
import csv
import sys
from decimal import Decimal, InvalidOperation


def total(rows, column):
    """The sum of `column` (counted from 1) over `rows`, as a Decimal."""
    return sum((Decimal(row[column - 1]) for row in rows), Decimal(0))


def main(argv=None):
    parser = argparse.ArgumentParser(prog="csvsum", description="Sum one column of a CSV file.")
    parser.add_argument("file", help="the CSV file; - for standard input")
    parser.add_argument("--column", type=int, default=1, help="which column to sum, from 1 (default 1)")
    parser.add_argument("--delimiter", default=",", help="the field separator (default ,)")
    parser.add_argument("--skip-header", action="store_true", help="leave out the first row")
    args = parser.parse_args(argv)  # exits with status 2 on bad options
    try:
        handle = sys.stdin if args.file == "-" else open(args.file, newline="")
        with handle:
            rows = list(csv.reader(handle, delimiter=args.delimiter))
        if args.skip_header:
            rows = rows[1:]
        print(total(rows, args.column))
    except (OSError, IndexError, InvalidOperation) as e:
        print(f"csvsum: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
