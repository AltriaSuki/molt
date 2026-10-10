"""Cron expressions: parsing and finding the next time a schedule fires.

An expression has five fields, separated by spaces or tabs::

    minute  hour  day-of-month  month  day-of-week

The table below (``FIELDS``) gives each field's bounds and, for month and
day-of-week, the three-letter names that may be used instead of numbers.

Each field is a comma-separated list of items. An item is ``*``, a value, a
range ``a-b``, or one of those followed by a step: ``*/n``, ``a-b/n``, or
``a/n`` (``a`` through the field's maximum, every ``n``).

When both day-of-month and day-of-week are restricted (neither is a literal
``*``), a day matches if either field matches; otherwise both must match.
"""

from __future__ import annotations

import calendar
import re
from dataclasses import dataclass
from datetime import date, datetime, timedelta


class CronError(ValueError):
    """An invalid cron expression, or a schedule that can never fire."""


@dataclass(frozen=True)
class Field:
    """One field of a cron expression."""

    name: str
    low: int
    high: int
    # Names usable in place of numbers; names[i] stands for low + i.
    names: tuple[str, ...] = ()

    def name_value(self, name: str) -> int | None:
        """The number a name stands for in this field, or None."""
        upper = name.upper()
        if upper in self.names:
            return self.low + self.names.index(upper)
        return None


MINUTE = Field("minute", 0, 59)
HOUR = Field("hour", 0, 23)
DAY_OF_MONTH = Field("day-of-month", 1, 31)
MONTH = Field(
    "month",
    1,
    12,
    ("JAN", "FEB", "MAR", "APR", "MAY", "JUN",
     "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"),
)
# 0 and 7 are both Sunday.
DAY_OF_WEEK = Field(
    "day-of-week",
    0,
    7,
    ("SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"),
)

FIELDS = (MINUTE, HOUR, DAY_OF_MONTH, MONTH, DAY_OF_WEEK)

ALIASES = {
    "@yearly": "0 0 1 1 *",
    "@annually": "0 0 1 1 *",
    "@monthly": "0 0 1 * *",
    "@weekly": "0 0 * * 0",
    "@daily": "0 0 * * *",
    "@midnight": "0 0 * * *",
    "@hourly": "0 * * * *",
}

_FIELD_SEPARATOR = re.compile(r"[ \t]+")
_NUMBER = re.compile(r"[0-9]+\Z")
_NAME = re.compile(r"[A-Za-z]+\Z")

# Any schedule that can fire at all fires within 8 years: the rarest day is
# Feb 29, and across a skipped century leap year (2100) the gap is 8 years.
_SEARCH_YEARS = 9


@dataclass(frozen=True)
class Schedule:
    """A parsed cron expression: the set of values each field allows."""

    minutes: frozenset[int]
    hours: frozenset[int]
    days: frozenset[int]  # days of the month, 1-31
    months: frozenset[int]  # 1-12
    weekdays: frozenset[int]  # 0-6, Sunday is 0 (a 7 in the expression is stored as 0)
    dom_restricted: bool  # day-of-month field was not a literal "*"
    dow_restricted: bool  # day-of-week field was not a literal "*"

    def matches_day(self, day: date) -> bool:
        if day.month not in self.months:
            return False
        dom = day.day in self.days
        dow = (day.weekday() + 1) % 7 in self.weekdays  # Python: Monday is 0
        if self.dom_restricted and self.dow_restricted:
            return dom or dow
        return dom and dow


def parse(expr: str) -> Schedule:
    """Parse a cron expression into a Schedule; raise CronError if invalid."""
    if not isinstance(expr, str):
        raise CronError(f"cron expression must be a string, not {type(expr).__name__}")
    text = expr.strip()
    if text.startswith("@"):
        try:
            text = ALIASES[text.lower()]
        except KeyError:
            raise CronError(f"unknown alias {text!r}") from None
    parts = _FIELD_SEPARATOR.split(text) if text else []
    if len(parts) != len(FIELDS):
        raise CronError(f"expected 5 fields, got {len(parts)} in {expr!r}")

    minutes, hours, days, months, weekdays = (
        _parse_field(part, field) for part, field in zip(parts, FIELDS)
    )
    weekdays = frozenset(d % 7 for d in weekdays)
    return Schedule(
        minutes=minutes,
        hours=hours,
        days=days,
        months=months,
        weekdays=weekdays,
        dom_restricted=parts[2] != "*",
        dow_restricted=parts[4] != "*",
    )


def _parse_field(text: str, field: Field) -> frozenset[int]:
    values: set[int] = set()
    for item in text.split(","):
        base, slash, step_text = item.partition("/")
        step = 1
        if slash:
            if not _NUMBER.match(step_text):
                raise CronError(f"{field.name}: bad step in {item!r}")
            step = int(step_text)
            if step == 0:
                raise CronError(f"{field.name}: step must be at least 1 in {item!r}")
        if base == "*":
            low, high = field.low, field.high
        elif "-" in base:
            first, _, last = base.partition("-")
            low, high = _value(first, field), _value(last, field)
            if low > high:
                raise CronError(f"{field.name}: range {base!r} runs backwards")
        else:
            low = _value(base, field)
            high = field.high if slash else low
        values.update(range(low, high + 1, step))
    return frozenset(values)


def _value(text: str, field: Field) -> int:
    if _NUMBER.match(text):
        value = int(text)
    elif _NAME.match(text) and field.name_value(text) is not None:
        value = field.name_value(text)
    elif _NAME.match(text):
        raise CronError(f"{field.name}: unknown name {text!r}")
    else:
        raise CronError(f"{field.name}: bad value {text!r}")
    if not field.low <= value <= field.high:
        raise CronError(f"{field.name}: {value} is outside {field.low}-{field.high}")
    return value


def next_fire(expr: str | Schedule, after: datetime) -> datetime:
    """The first minute strictly after ``after`` at which ``expr`` fires.

    Raises CronError for an invalid expression or one that never fires.
    """
    schedule = expr if isinstance(expr, Schedule) else parse(expr)
    hours = sorted(schedule.hours)
    minutes = sorted(schedule.minutes)
    try:
        start = after.replace(second=0, microsecond=0) + timedelta(minutes=1)
        day = start.date()
        last_year = start.year + _SEARCH_YEARS
        earliest: datetime | None = start  # only the first day starts mid-day
        while day.year <= last_year:
            if day.month not in schedule.months:
                day = _first_of_next_month(day)
                earliest = None
                continue
            if schedule.matches_day(day):
                found = _first_time(hours, minutes, earliest)
                if found is not None:
                    return datetime(day.year, day.month, day.day, *found)
            day += timedelta(days=1)
            earliest = None
    except OverflowError:
        pass  # ran past datetime.max
    raise CronError(f"schedule {_describe(expr)} never fires")


def _first_time(hours: list[int], minutes: list[int], earliest: datetime | None):
    """The first (hour, minute) of the day at or after ``earliest``'s time."""
    for hour in hours:
        if earliest is not None and hour < earliest.hour:
            continue
        for minute in minutes:
            if earliest is not None and hour == earliest.hour and minute < earliest.minute:
                continue
            return hour, minute
    return None


def _first_of_next_month(day: date) -> date:
    _, length = calendar.monthrange(day.year, day.month)
    return day.replace(day=1) + timedelta(days=length)


def _describe(expr: str | Schedule) -> str:
    return repr(expr) if isinstance(expr, str) else "(parsed)"
