"""Runs registered jobs when their schedules come due.

The runner never reads the system clock itself: it is given a ``clock``
callable, which makes it easy to drive from tests or from a simulated
timeline. A real deployment passes ``datetime.now``.

Missed runs are not caught up: when a job is overdue (say the process was
suspended), it runs once and its next run is computed from the current time.
"""

from __future__ import annotations

import shlex
import subprocess
from dataclasses import dataclass
from datetime import datetime
from typing import Callable, Optional

from . import cron
from .jobs import Job, JobRegistry

Clock = Callable[[], datetime]
NextFire = Callable[[str, datetime], datetime]
Execute = Callable[[Job], object]


@dataclass(frozen=True)
class RunRecord:
    """The outcome of one run of a job."""

    job: str
    due: datetime
    started: datetime
    ok: bool
    error: Optional[str] = None


def run_command(job: Job) -> None:
    """Run the job's command without a shell; a non-zero exit is an error."""
    subprocess.run(shlex.split(job.command), check=True)


class Runner:
    def __init__(
        self,
        registry: JobRegistry,
        clock: Clock,
        *,
        execute: Optional[Execute] = None,
        next_fire: Optional[NextFire] = None,
    ) -> None:
        self._registry = registry
        self._clock = clock
        self._execute = execute or run_command
        self._next_fire = next_fire or cron.next_fire
        self._due: dict[str, datetime] = {}
        self.history: list[RunRecord] = []

    def next_due(self, name: str) -> datetime:
        """When the named job is next due.

        The first time a job is asked about, this is computed from the
        current time; after each run it is recomputed from the time of the run.
        """
        if name not in self._due:
            job = self._registry.get(name)
            self._due[name] = self._next_fire(job.schedule, self._clock())
        return self._due[name]

    def upcoming(self, limit: Optional[int] = None) -> list[tuple[datetime, Job]]:
        """Enabled jobs with their next due time, soonest first (ties by name)."""
        items = [(self.next_due(job.name), job) for job in self._registry.enabled()]
        items.sort(key=lambda item: (item[0], item[1].name))
        return items if limit is None else items[:limit]

    def forget(self, name: str) -> None:
        """Drop the cached due time, e.g. after a job's schedule changed."""
        self._due.pop(name, None)

    def tick(self) -> list[RunRecord]:
        """Run every enabled job that is due now; return what ran."""
        now = self._clock()
        ran = []
        for job in self._registry.enabled():
            due = self.next_due(job.name)
            if due > now:
                continue
            ran.append(self._run(job, due, now))
            self._due[job.name] = self._next_fire(job.schedule, now)
        return ran

    def _run(self, job: Job, due: datetime, now: datetime) -> RunRecord:
        try:
            self._execute(job)
        except Exception as exc:  # a failing job must not stop the others
            record = RunRecord(job.name, due, now, ok=False, error=f"{type(exc).__name__}: {exc}")
        else:
            record = RunRecord(job.name, due, now, ok=True)
        self.history.append(record)
        return record
