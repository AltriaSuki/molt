# logsum

A small command line tool that summarizes web access logs: how many
requests there were, how many of them fall in each status class, the total
bytes sent, request durations and the most requested paths.

```
$ python3 -m logsum logs/web-01.log logs/web-02.log
requests: 7
bytes: 48213
2xx: 4
3xx: 1
4xx: 1
5xx: 1
avg duration: 41.3 ms
max duration: 120 ms
top paths:
  3  /index.html
  2  /api/items
  1  /login
  1  /static/app.js
```

Several files are summarized together. `-` reads standard input.

## Log format

Each line is one request, six fields separated by spaces or tabs:

```
2024-03-05T14:02:11 GET /index.html 200 5120 12
```

| field         | rule                                                          |
|---------------|---------------------------------------------------------------|
| timestamp     | `YYYY-MM-DDTHH:MM:SS`, server local time, no time zone         |
| method        | uppercase ASCII letters (`GET`, `POST`, ...)                  |
| path          | starts with `/`; may carry a query string (`/search?q=x`)     |
| status        | the final HTTP status, an integer from 200 to 599             |
| bytes         | response size, a non-negative integer                         |
| duration_ms   | time to serve the request in milliseconds, non-negative integer |

Blank lines are ignored. A line that does not follow the format is dropped
from the summary.

## Report

- `requests` and `bytes`: number of requests and their total size.
- `2xx` ... `5xx`: requests per status class; all four are always shown.
- `avg duration` / `max duration`: over all requests (`n/a` if there are none).
- `top paths`: the five most requested paths, most requested first, ties in
  alphabetical order. Query strings are ignored here, so `/search?q=a` and
  `/search?q=b` both count as `/search`.

## Layout

- `logsum/parser.py`: `parse_line` (one line to an `Entry`, or `ParseError`)
  and `iter_entries`.
- `logsum/summary.py`: `Summary`, the running totals.
- `logsum/report.py`: `render_text`, the text report.
- `logsum/cli.py`: argument handling; `python3 -m logsum` runs `main()`.

## Running the tests

Python 3.11 or newer, standard library only:

```
python3 -m unittest -q tests.test_parser tests.test_summary tests.test_report tests.test_cli
```
