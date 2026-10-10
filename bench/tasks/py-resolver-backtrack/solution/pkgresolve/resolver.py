"""Pick one version of every package a set of requirements needs.

`resolve(index, ["web^2", "db>=1.4"])` returns a dict mapping each needed
package name to the chosen `Version`, such that the root requirements and the
requirements of every chosen version are all satisfied, and the dict holds
exactly the packages reachable from the root through the chosen versions.

The search is depth-first with backtracking. It decides one package at a time
(the needed, undecided package with the fewest versions left), trying its
versions newest first among those that every requirement on it so far allows.
A version is only tried if each of its own requirements can still be met.
When every version of a package fails, the search backs up to the most recent
decision that took part in the failure (conflict-directed backjumping), so
unrelated decisions in between are not retried over and over.
"""

from __future__ import annotations

from collections.abc import Iterable

from .index import PackageIndex
from .versions import Requirement, Version

__all__ = ["ResolutionError", "resolve"]


class ResolutionError(Exception):
    """The requirements cannot be satisfied by any versions in the index."""


ROOT = None  # requester of the root requirements


def _describe(requester: str | None, chosen: dict[str, Version]) -> str:
    if requester is ROOT:
        return "the root requirements"
    return f"{requester} {chosen[requester]}"


class _Search:
    def __init__(self, index: PackageIndex, roots: list[Requirement]):
        self.index = index
        self.chosen: dict[str, Version] = {}
        # name -> [(requirement, requester)] for every requirement in force on
        # that package; requester is ROOT or the name of a chosen package.
        self.demands: dict[str, list[tuple[Requirement, str | None]]] = {}
        self.conflict = "no packages could be resolved"
        for req in roots:
            self._push(req, ROOT)

    # -- bookkeeping -----------------------------------------------------

    def _push(self, req: Requirement, requester: str | None) -> None:
        self.demands.setdefault(req.name, []).append((req, requester))

    def _pop(self, req: Requirement) -> None:
        demands = self.demands[req.name]
        demands.pop()
        if not demands:
            del self.demands[req.name]

    def _requesters(self, name: str) -> set[str]:
        return {who for _, who in self.demands.get(name, ()) if who is not ROOT}

    def _allowed(self, name: str, extra: Requirement | None = None) -> list[Version]:
        if name not in self.index:
            return []
        reqs = [req for req, _ in self.demands.get(name, ())]
        if extra is not None:
            reqs.append(extra)
        return [v for v in self.index.versions(name) if all(r.allows(v) for r in reqs)]

    def _next(self) -> tuple[str, list[Version]] | None:
        best = None
        for name in self.demands:
            if name in self.chosen:
                continue
            allowed = self._allowed(name)
            if best is None or len(allowed) < len(best[1]):
                best = (name, allowed)
                if not allowed:
                    break
        return best

    # -- search ----------------------------------------------------------

    def _reject(self, name: str, version: Version) -> set[str] | None:
        """Why `version` of `name` cannot be chosen now, as the set of chosen
        packages involved; None if it can be tried."""
        for dep in self.index.dependencies(name, version):
            if dep.name not in self.index:
                self.conflict = (
                    f"{name} {version} requires unknown package {dep.name!r}"
                )
                return set()
            if dep.name == name:
                if not dep.allows(version):
                    self.conflict = f"{name} {version} requires {dep}"
                    return set()
                continue
            if dep.name in self.chosen:
                if not dep.allows(self.chosen[dep.name]):
                    self.conflict = (
                        f"{name} {version} requires {dep}, "
                        f"but {dep.name} {self.chosen[dep.name]} is chosen"
                    )
                    return {dep.name}
                continue
            if not self._allowed(dep.name, dep):
                others = ", ".join(
                    f"{req} (from {_describe(who, self.chosen)})"
                    for req, who in self.demands.get(dep.name, ())
                )
                self.conflict = f"{name} {version} requires {dep}" + (
                    f", which conflicts with {others}" if others else
                    f", but no version of {dep.name} matches"
                )
                return self._requesters(dep.name)
        return None

    def _solve(self) -> set[str] | None:
        """Extend the current choices to a full resolution.

        Returns None on success. On failure, returns the set of chosen
        packages whose choices together rule out every extension.
        """
        step = self._next()
        if step is None:
            return None
        name, allowed = step
        culprits = self._requesters(name)
        if not allowed:
            if name not in self.index:
                self.conflict = f"unknown package {name!r} (required by " + ", ".join(
                    _describe(who, self.chosen) for _, who in self.demands[name]
                ) + ")"
            else:
                self.conflict = f"no version of {name} matches " + ", ".join(
                    f"{req} (from {_describe(who, self.chosen)})"
                    for req, who in self.demands[name]
                )
            return culprits
        for version in allowed:
            why = self._reject(name, version)
            if why is not None:
                culprits |= why
                continue
            deps = self.index.dependencies(name, version)
            self.chosen[name] = version
            for dep in deps:
                self._push(dep, name)
            why = self._solve()
            if why is None:
                return None
            for dep in reversed(deps):
                self._pop(dep)
            del self.chosen[name]
            if name not in why:
                # This choice played no part in the failure: no other version
                # of this package can fix it, so back up past it.
                return why
            culprits |= why - {name}
        return culprits

    def run(self) -> dict[str, Version]:
        if self._solve() is not None:
            raise ResolutionError(self.conflict)
        return dict(self.chosen)


def resolve(index: PackageIndex, requirements: Iterable[str]) -> dict[str, Version]:
    """Return {name: Version} satisfying the requirements and all their dependencies.

    Versions are tried newest first. Raises ResolutionError when the
    requirements cannot be satisfied (an unknown package, or no combination
    of versions that works together).
    """
    roots = [Requirement.parse(text) for text in requirements]
    return _Search(index, roots).run()
