"""Loading jobs from a TOML file.

Example::

    [defaults]
    enabled = true
    tags = ["prod"]

    [[jobs]]
    name = "backup"
    schedule = "30 2 * * *"
    command = "backup.sh --full"

    [[jobs]]
    name = "report"
    schedule = "@weekly"
    command = "report.py"
    enabled = false
    tags = ["mail"]

``defaults`` may set ``enabled`` and ``tags``; a job's own values win.
Schedules are not checked here: the runner reports a bad schedule when it
first computes that job's next run.
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Any

from .jobs import DuplicateJobError, Job, JobRegistry

_JOB_KEYS = {"name", "schedule", "command", "enabled", "tags"}
_DEFAULT_KEYS = {"enabled", "tags"}


class ConfigError(ValueError):
    """The configuration file is malformed."""


def load_config(path: str | Path) -> JobRegistry:
    path = Path(path)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ConfigError(f"{path}: cannot read: {exc.strerror}") from None
    return load_jobs(text, source=str(path))


def load_jobs(text: str, *, source: str = "<config>") -> JobRegistry:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{source}: {exc}") from None

    unknown = set(data) - {"defaults", "jobs"}
    if unknown:
        raise ConfigError(f"{source}: unknown top-level key(s): {', '.join(sorted(unknown))}")

    defaults = data.get("defaults", {})
    if not isinstance(defaults, dict):
        raise ConfigError(f"{source}: [defaults] must be a table")
    _check_keys(defaults, _DEFAULT_KEYS, f"{source}: [defaults]")
    _check_types(defaults, f"{source}: [defaults]")

    entries = data.get("jobs", [])
    if not isinstance(entries, list):
        raise ConfigError(f"{source}: jobs must be an array of tables ([[jobs]])")

    registry = JobRegistry()
    for index, entry in enumerate(entries, start=1):
        where = f"{source}: job #{index}"
        if not isinstance(entry, dict):
            raise ConfigError(f"{where}: must be a table")
        _check_keys(entry, _JOB_KEYS, where)
        for key in ("name", "schedule", "command"):
            if key not in entry:
                raise ConfigError(f"{where}: missing {key!r}")
        if isinstance(entry["name"], str):
            where = f"{source}: job {entry['name']!r}"
        _check_types(entry, where)
        merged = {**defaults, **entry}
        try:
            job = Job(
                name=merged["name"],
                schedule=merged["schedule"],
                command=merged["command"],
                enabled=merged.get("enabled", True),
                tags=tuple(merged.get("tags", ())),
            )
            registry.add(job)
        except DuplicateJobError as exc:
            raise ConfigError(f"{where}: {exc}") from None
        except ValueError as exc:
            raise ConfigError(f"{where}: {exc}") from None
    return registry


def _check_keys(table: dict[str, Any], allowed: set[str], where: str) -> None:
    unknown = set(table) - allowed
    if unknown:
        raise ConfigError(f"{where}: unknown key(s): {', '.join(sorted(unknown))}")


def _check_types(table: dict[str, Any], where: str) -> None:
    for key in ("name", "schedule", "command"):
        if key in table and not isinstance(table[key], str):
            raise ConfigError(f"{where}: {key} must be a string")
    if "enabled" in table and not isinstance(table["enabled"], bool):
        raise ConfigError(f"{where}: enabled must be true or false")
    if "tags" in table:
        tags = table["tags"]
        if not isinstance(tags, list) or not all(isinstance(t, str) for t in tags):
            raise ConfigError(f"{where}: tags must be a list of strings")
