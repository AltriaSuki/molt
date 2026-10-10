"""Jobs and the registry that holds them."""

from __future__ import annotations

import re
from dataclasses import dataclass, replace
from typing import Iterable, Iterator

_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*\Z")


class DuplicateJobError(ValueError):
    """A job with the same name is already registered."""


@dataclass(frozen=True)
class Job:
    """A command to run on a cron schedule."""

    name: str
    schedule: str
    command: str
    enabled: bool = True
    tags: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not _NAME.match(self.name):
            raise ValueError(f"invalid job name: {self.name!r}")
        if not isinstance(self.schedule, str) or not self.schedule.strip():
            raise ValueError(f"job {self.name}: schedule must be a non-empty string")
        if not isinstance(self.command, str) or not self.command.strip():
            raise ValueError(f"job {self.name}: command must be a non-empty string")
        # Accept any iterable of tags but store a tuple, so jobs stay hashable.
        object.__setattr__(self, "tags", tuple(self.tags))


class JobRegistry:
    """Jobs by name. Iteration is in name order."""

    def __init__(self, jobs: Iterable[Job] = ()) -> None:
        self._jobs: dict[str, Job] = {}
        for job in jobs:
            self.add(job)

    def add(self, job: Job) -> None:
        if job.name in self._jobs:
            raise DuplicateJobError(f"duplicate job name: {job.name}")
        self._jobs[job.name] = job

    def remove(self, name: str) -> Job:
        try:
            return self._jobs.pop(name)
        except KeyError:
            raise KeyError(f"no such job: {name}") from None

    def get(self, name: str) -> Job:
        try:
            return self._jobs[name]
        except KeyError:
            raise KeyError(f"no such job: {name}") from None

    def set_enabled(self, name: str, enabled: bool) -> Job:
        job = replace(self.get(name), enabled=enabled)
        self._jobs[name] = job
        return job

    def enabled(self) -> list[Job]:
        return [job for job in self if job.enabled]

    def with_tag(self, tag: str) -> list[Job]:
        return [job for job in self if tag in job.tags]

    def __contains__(self, name: object) -> bool:
        return name in self._jobs

    def __len__(self) -> int:
        return len(self._jobs)

    def __iter__(self) -> Iterator[Job]:
        return iter(sorted(self._jobs.values(), key=lambda job: job.name))
