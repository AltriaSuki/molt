"""Versions, version constraints and requirements.

A version is MAJOR.MINOR.PATCH, three non-negative integers. Missing trailing
parts count as zero, so "1.4" is 1.4.0 and "2" is 2.0.0.

A constraint is a comma-separated list of clauses that must all hold:

    ==1.2.0        exactly this version
    !=1.3          anything but this version
    >=1.0  <=1.9   comparisons (also > and <)
    ^1.2           compatible with 1.2: at least 1.2.0 and below the next
                   breaking release, 2.0.0. For 0.x versions the left-most
                   non-zero part is the breaking one: ^0.3 is <0.4.0, ^0.0.3
                   is <0.0.4, ^0.0 is <0.1.0 and ^0 is <1.0.0.
    *              any version (an empty constraint means the same)

A requirement is a package name followed by a constraint, with optional
spaces in between: "b>=1.0,<2.0", "web ^2.1", or just "b" for any version.
"""

from __future__ import annotations

import operator
import re
from dataclasses import dataclass

__all__ = [
    "Clause",
    "Constraint",
    "InvalidRequirement",
    "InvalidVersion",
    "Requirement",
    "Version",
]


class InvalidVersion(ValueError):
    """A version or constraint string could not be parsed."""


class InvalidRequirement(ValueError):
    """A requirement string could not be parsed."""


_VERSION_RE = re.compile(r"(\d+)(?:\.(\d+))?(?:\.(\d+))?")
_NAME_RE = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?")
_CLAUSE_RE = re.compile(r"(==|!=|>=|<=|>|<|\^)\s*(\S+)")

_COMPARE = {
    "==": operator.eq,
    "!=": operator.ne,
    ">=": operator.ge,
    "<=": operator.le,
    ">": operator.gt,
    "<": operator.lt,
}


def _parse_parts(text: str) -> tuple["Version", int]:
    """Parse a version string; also return how many parts were written."""
    if not isinstance(text, str):
        raise InvalidVersion(f"version must be a string, not {type(text).__name__}")
    match = _VERSION_RE.fullmatch(text.strip())
    if match is None:
        raise InvalidVersion(f"invalid version: {text!r}")
    given = [part for part in match.groups() if part is not None]
    numbers = [int(part) for part in given] + [0] * (3 - len(given))
    return Version(*numbers), len(given)


@dataclass(frozen=True, order=True)
class Version:
    major: int
    minor: int = 0
    patch: int = 0

    def __post_init__(self) -> None:
        for part in (self.major, self.minor, self.patch):
            if isinstance(part, bool) or not isinstance(part, int) or part < 0:
                raise InvalidVersion(f"version parts must be non-negative integers: {part!r}")

    @classmethod
    def parse(cls, text: "str | Version") -> "Version":
        if isinstance(text, Version):
            return text
        version, _ = _parse_parts(text)
        return version

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"


@dataclass(frozen=True)
class Clause:
    op: str
    version: Version

    def allows(self, version: Version) -> bool:
        return _COMPARE[self.op](version, self.version)

    def __str__(self) -> str:
        return f"{self.op}{self.version}"


def _caret(version: Version, given: int) -> tuple[Clause, Clause]:
    major, minor, patch = version.major, version.minor, version.patch
    if major > 0 or given == 1:
        upper = Version(major + 1)
    elif minor > 0 or given == 2:
        upper = Version(0, minor + 1)
    else:
        upper = Version(0, 0, patch + 1)
    return Clause(">=", version), Clause("<", upper)


class Constraint:
    """A set of clauses that a version must all satisfy."""

    __slots__ = ("clauses",)

    def __init__(self, clauses=()):
        self.clauses = tuple(clauses)

    @classmethod
    def parse(cls, text: str) -> "Constraint":
        text = text.strip()
        if text in ("", "*"):
            return cls()
        clauses: list[Clause] = []
        for item in text.split(","):
            match = _CLAUSE_RE.fullmatch(item.strip())
            if match is None:
                raise InvalidVersion(f"invalid constraint {item.strip()!r} in {text!r}")
            op, raw = match.groups()
            version, given = _parse_parts(raw)
            if op == "^":
                clauses.extend(_caret(version, given))
            else:
                clauses.append(Clause(op, version))
        return cls(clauses)

    def allows(self, version: Version) -> bool:
        return all(clause.allows(version) for clause in self.clauses)

    def filter(self, versions):
        """The versions this constraint allows, in the order given."""
        return [version for version in versions if self.allows(version)]

    def is_any(self) -> bool:
        return not self.clauses

    def __and__(self, other: "Constraint") -> "Constraint":
        if not isinstance(other, Constraint):
            return NotImplemented
        return Constraint(self.clauses + other.clauses)

    def __eq__(self, other) -> bool:
        if not isinstance(other, Constraint):
            return NotImplemented
        return self.clauses == other.clauses

    def __hash__(self) -> int:
        return hash(self.clauses)

    def __str__(self) -> str:
        return ",".join(str(clause) for clause in self.clauses) or "*"

    def __repr__(self) -> str:
        return f"Constraint({str(self)!r})"


@dataclass(frozen=True)
class Requirement:
    name: str
    constraint: Constraint

    @classmethod
    def parse(cls, text: "str | Requirement") -> "Requirement":
        if isinstance(text, Requirement):
            return text
        if not isinstance(text, str):
            raise InvalidRequirement(f"requirement must be a string, not {type(text).__name__}")
        stripped = text.strip()
        match = _NAME_RE.match(stripped)
        if match is None:
            raise InvalidRequirement(f"invalid requirement: {text!r}")
        try:
            constraint = Constraint.parse(stripped[match.end():])
        except InvalidVersion as exc:
            raise InvalidRequirement(f"invalid requirement {text!r}: {exc}") from None
        return cls(match.group(), constraint)

    def allows(self, version: Version) -> bool:
        return self.constraint.allows(version)

    def __str__(self) -> str:
        if self.constraint.is_any():
            return self.name
        return f"{self.name}{self.constraint}"
