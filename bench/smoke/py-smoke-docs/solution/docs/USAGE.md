# Using csvsum

`csvsum.py` adds up one column of a CSV file and prints the total. It reads
numbers as decimals, so `0.1` and `0.2` add up to exactly `0.3`.

```sh
python3 csvsum.py FILE [--column N] [--delimiter D] [--skip-header]
```

`FILE` is the CSV file to read, or `-` to read standard input.

## Options

- `--column N`: which column to sum, counted from 1. Default: 1.
- `--delimiter D`: the character between fields. Default: `,`.
- `--skip-header`: leave out the first row, for a file with a header.
  Default: off, every row is summed.

## Examples

Sum the second column of a file with a header row:

```sh
python3 csvsum.py --column 2 --skip-header expenses.csv
```

Sum the first column of semicolon-separated data from standard input:

```sh
printf '1;a\n2;b\n' | python3 csvsum.py --delimiter ';' -
```

## Exit statuses

- `0`: the total was printed.
- `1`: the file could not be read, a row has no such column, or a value is
  not a number; the reason is printed to standard error.
- `2`: the command line was wrong, such as an unknown option.
