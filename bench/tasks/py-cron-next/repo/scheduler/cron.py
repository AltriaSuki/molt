"""Cron expressions: parsing and finding the next time a schedule fires.

An expression has five fields, separated by spaces or tabs::

    minute  hour  day-of-month  month  day-of-week

The table below (``FIELDS``) gives each field's bounds and, for month and
day-of-week, the three-letter names that may be used instead of numbers.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime


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


def parse(expr: str) -> Schedule:
    """Parse a cron expression into a Schedule."""
    raise NotImplementedError("cron expressions are not supported yet")


def next_fire(expr: str | Schedule, after: datetime) -> datetime:
    """The first minute strictly after ``after`` at which ``expr`` fires."""
    raise NotImplementedError("cron expressions are not supported yet")
