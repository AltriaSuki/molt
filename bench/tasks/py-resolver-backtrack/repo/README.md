# pkgresolve

A small dependency resolver. Give it an index of packages (which versions
exist, and what each version requires) and a list of requirements, and it
picks one version of every package those requirements need.

```python
from pkgresolve import PackageIndex, resolve

index = PackageIndex({
    "web": {"1.0.0": ["http^1"], "2.0.0": ["http^2", "tls>=1.2"]},
    "http": {"1.0.0": [], "2.0.0": ["tls"]},
    "tls": {"1.2.0": [], "1.3.0": []},
})
resolution = resolve(index, ["web"])
{name: str(version) for name, version in resolution.items()}
# {'web': '2.0.0', 'http': '2.0.0', 'tls': '1.3.0'}
```

## Versions and requirements

Versions are `MAJOR.MINOR.PATCH`; missing trailing parts are zero (`1.4` is
`1.4.0`). A requirement is a package name and a constraint:

| constraint      | meaning                                                  |
| --------------- | -------------------------------------------------------- |
| `==1.2`         | exactly 1.2.0                                            |
| `!=1.3`         | anything but 1.3.0                                       |
| `>=1.0` `<2`    | comparisons (also `<=` and `>`)                          |
| `^1.2`          | compatible: `>=1.2.0,<2.0.0` (`^0.3` is `>=0.3.0,<0.4.0`) |
| `>=1.0,<2.0`    | a comma means "and"                                      |
| nothing or `*`  | any version                                              |

So `"b>=1.0,<2.0"`, `"web ^2.1"` and `"tls"` are all requirements.

## Layout

- `pkgresolve/versions.py`: `Version`, `Constraint`, `Requirement` and parsing.
- `pkgresolve/index.py`: `PackageIndex`, the in-memory index (also loads JSON).
- `pkgresolve/resolver.py`: `resolve(index, requirements)` and `ResolutionError`.
- `pkgresolve/lock.py`: lock files (`name==version` lines) and `verify_lock`,
  which checks that a lock really is a resolution of some requirements.
- `pkgresolve/cli.py`: the command line.
- `tests/data/mirror.json`: a snapshot of the staging mirror (30 packages,
  15 versions each), handy for trying the resolver on something realistic.

## Command line

```
python3 -m pkgresolve resolve examples/index.json webapp
python3 -m pkgresolve resolve examples/index.json webapp > app.lock
python3 -m pkgresolve check examples/index.json app.lock webapp
```

`resolve` prints a lock file, or the reason it failed (exit status 1).
`check` prints `ok` or the problems it found.

## Tests

Standard library only (Python 3.11+):

```
python3 -m unittest -q tests.test_versions tests.test_index tests.test_lock tests.test_resolver tests.test_cli
```
