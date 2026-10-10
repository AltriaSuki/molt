"""Lock files: the result of a resolution, written down.

A lock file has one "name==version" line per package, sorted by name. Blank
lines and lines starting with "#" are ignored when reading one back.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Iterable, Mapping

from .index import PackageIndex
from .versions import Requirement, Version

__all__ = ["format_lock", "parse_lock", "verify_lock"]


def format_lock(resolution: Mapping[str, Version]) -> str:
    return "".join(f"{name}=={resolution[name]}\n" for name in sorted(resolution))


def parse_lock(text: str) -> dict[str, Version]:
    lock: dict[str, Version] = {}
    for number, line in enumerate(text.splitlines(), start=1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, sep, version = line.partition("==")
        name = name.strip()
        if not sep or not name:
            raise ValueError(f"line {number}: expected name==version, got {line!r}")
        if name in lock:
            raise ValueError(f"line {number}: {name} is locked twice")
        lock[name] = Version.parse(version.strip())
    return lock


def verify_lock(
    index: PackageIndex, requirements: Iterable[str], lock: Mapping[str, Version]
) -> list[str]:
    """Check a lock against the index and the root requirements.

    Returns a list of problems; an empty list means the lock is a valid
    resolution: every locked version exists, the root requirements and the
    requirements of every locked version are satisfied, and every locked
    package is needed (reachable from the root through locked versions).
    """
    problems: list[str] = []
    for name in sorted(lock):
        if not index.has_version(name, lock[name]):
            problems.append(f"{name} {lock[name]} is not in the index")

    roots = [(Requirement.parse(text), "root") for text in requirements]
    queue = deque(roots)
    seen: set[str] = set()
    while queue:
        req, requester = queue.popleft()
        version = lock.get(req.name)
        if version is None:
            problems.append(f"{requester} requires {req}, but {req.name} is not locked")
            continue
        if not req.allows(version):
            problems.append(f"{requester} requires {req}, but {req.name} {version} is locked")
        if req.name in seen or not index.has_version(req.name, version):
            continue
        seen.add(req.name)
        for dep in index.dependencies(req.name, version):
            queue.append((dep, f"{req.name} {version}"))

    for name in sorted(set(lock) - seen):
        if index.has_version(name, lock[name]):
            problems.append(f"{name} is locked but nothing requires it")
    return problems
