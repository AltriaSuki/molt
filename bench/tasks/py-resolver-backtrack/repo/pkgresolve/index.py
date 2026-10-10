"""An in-memory package index.

The index knows which versions of each package exist and what each version
requires. It can be built in code, from a dict, or from a JSON file of the
same shape:

    {
        "web": {"1.0.0": ["http^1"], "2.0.0": ["http^2", "tls>=1.2"]},
        "http": {"1.0.0": [], "2.0.0": ["tls"]},
        "tls": {"1.2.0": [], "1.3.0": []}
    }
"""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping

from .versions import Requirement, Version

__all__ = ["PackageIndex"]


class PackageIndex:
    def __init__(self, packages: Mapping[str, Mapping[str, Iterable[str]]] | None = None):
        # name -> {Version: (Requirement, ...)}
        self._packages: dict[str, dict[Version, tuple[Requirement, ...]]] = {}
        # name -> versions, newest first; rebuilt lazily after add()
        self._sorted: dict[str, list[Version]] = {}
        for name, versions in (packages or {}).items():
            for version, requires in versions.items():
                self.add(name, version, requires)

    @classmethod
    def from_json(cls, text: str) -> "PackageIndex":
        data = json.loads(text)
        if not isinstance(data, dict):
            raise ValueError("index JSON must be an object mapping package names to versions")
        return cls(data)

    @classmethod
    def load(cls, path) -> "PackageIndex":
        with open(path, encoding="utf-8") as handle:
            return cls.from_json(handle.read())

    def add(self, name: str, version: "str | Version", requires: Iterable[str] = ()) -> None:
        """Add one version of a package and the requirements of that version."""
        check = Requirement.parse(name)
        if check.name != name or not check.constraint.is_any():
            raise ValueError(f"invalid package name: {name!r}")
        version = Version.parse(version)
        if isinstance(requires, str):
            raise TypeError("requires must be a list of requirement strings, not a string")
        parsed = tuple(Requirement.parse(text) for text in requires)
        versions = self._packages.setdefault(name, {})
        if version in versions:
            raise ValueError(f"{name} {version} is already in the index")
        versions[version] = parsed
        self._sorted.pop(name, None)

    def __contains__(self, name: object) -> bool:
        return name in self._packages

    def __len__(self) -> int:
        return len(self._packages)

    def names(self) -> list[str]:
        return sorted(self._packages)

    def versions(self, name: str) -> list[Version]:
        """All versions of a package, newest first. KeyError if it is unknown."""
        if name not in self._packages:
            raise KeyError(name)
        if name not in self._sorted:
            self._sorted[name] = sorted(self._packages[name], reverse=True)
        return list(self._sorted[name])

    def has_version(self, name: str, version: Version) -> bool:
        return version in self._packages.get(name, {})

    def dependencies(self, name: str, version: Version) -> tuple[Requirement, ...]:
        """What one version of a package requires. KeyError if it is unknown."""
        try:
            return self._packages[name][version]
        except KeyError:
            raise KeyError(f"{name} {version}") from None

    def matching(self, requirement: Requirement) -> list[Version]:
        """Versions that satisfy a requirement, newest first ([] if the package is unknown)."""
        if requirement.name not in self._packages:
            return []
        return requirement.constraint.filter(self.versions(requirement.name))

    def to_dict(self) -> dict[str, dict[str, list[str]]]:
        return {
            name: {
                str(version): [str(req) for req in self._packages[name][version]]
                for version in self.versions(name)
            }
            for name in self.names()
        }
