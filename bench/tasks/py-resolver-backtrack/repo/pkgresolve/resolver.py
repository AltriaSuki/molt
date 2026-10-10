"""Pick one version of every package a set of requirements needs.

`resolve(index, ["web^2", "db>=1.4"])` returns a dict mapping each needed
package name to the chosen `Version`. Requirements are processed in the order
they are discovered (the root's first, then the requirements of each chosen
version), and each package gets the newest version that satisfies the
requirement that first asked for it.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Iterable

from .index import PackageIndex
from .versions import Requirement, Version

__all__ = ["ResolutionError", "resolve"]


class ResolutionError(Exception):
    """The requirements cannot be satisfied by any versions in the index."""


def _describe(requester: tuple[str, Version] | None) -> str:
    if requester is None:
        return "the root requirements"
    name, version = requester
    return f"{name} {version}"


def _pick(index: PackageIndex, req: Requirement, requester) -> Version:
    if req.name not in index:
        raise ResolutionError(f"unknown package {req.name!r} (required by {_describe(requester)})")
    candidates = index.matching(req)
    if not candidates:
        raise ResolutionError(
            f"no version of {req.name} matches {req.constraint} "
            f"(required by {_describe(requester)})"
        )
    return candidates[0]


def resolve(index: PackageIndex, requirements: Iterable[str]) -> dict[str, Version]:
    """Return {name: Version} satisfying the requirements and all their dependencies.

    Raises ResolutionError when the requirements cannot be satisfied.
    """
    pending = deque((Requirement.parse(text), None) for text in requirements)
    chosen: dict[str, Version] = {}
    while pending:
        req, requester = pending.popleft()
        current = chosen.get(req.name)
        if current is not None:
            if not req.allows(current):
                raise ResolutionError(
                    f"{_describe(requester)} requires {req}, "
                    f"but {req.name} {current} was already chosen"
                )
            continue
        version = _pick(index, req, requester)
        chosen[req.name] = version
        for dep in index.dependencies(req.name, version):
            pending.append((dep, (req.name, version)))
    return chosen
