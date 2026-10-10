# pkgver

Small, dependency-free [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html)
toolkit for Node, used by our release scripts and the internal package
mirror to decide which published version a dependency spec resolves to.

- `src/semver.js`: the `SemVer` class (parsing and precedence) plus helpers:
  `parse`, `valid`, `compare`, `rcompare`, `eq`/`neq`/`gt`/`gte`/`lt`/`lte`,
  `sort`, `rsort` and `inc`.
- `src/range.js`: `satisfies(version, range)` for npm-style ranges
  (`^1.2.0`, `~2.4`, `>=1.0.0 <2.0.0 || 3.x`, ...).
- `src/select.js`: `maxSatisfying`, `minSatisfying` and `filterSatisfying`,
  which pick entries out of a list of published versions using `satisfies`.
- `src/cli.js` / `bin/pkgver.js`: the `pkgver` command line.
- `src/index.js` re-exports all of the above.

## Versions

```js
import { SemVer, compare, sort, inc } from './src/index.js';

const v = new SemVer('v1.4.0-rc.2+build.11');
v.major;       // 1
v.prerelease;  // ['rc', 2]   (numeric identifiers are numbers)
v.build;       // ['build', '11']
v.version;     // '1.4.0-rc.2' (normalized, no build metadata)

compare('1.0.0-beta.11', '1.0.0-beta.2'); // 1
sort(['1.10.0', '1.9.0', '1.9.0-rc.1']);  // ['1.9.0-rc.1', '1.9.0', '1.10.0']
inc('1.4.0-rc.2', 'prerelease');          // '1.4.0-rc.3'
```

`new SemVer(string)` accepts exactly a SemVer 2.0.0 version, optionally with
a leading `v` and surrounding whitespace, and throws a `TypeError`
(`Invalid version: ...`) otherwise. `parse` and `valid` return `null`
instead of throwing.

Precedence follows the spec: major, minor and patch compare numerically; a
version with a prerelease tag ranks below the same version without one;
prerelease identifiers compare left to right, numeric ones numerically and
below alphanumeric ones, alphanumeric ones in ASCII order, and a shorter tag
that is a prefix of a longer one ranks lower. Build metadata is ignored
(`compareBuild` and `sort` use it only to break ties).

## Ranges

```js
import { satisfies } from './src/index.js';

satisfies('1.4.2', '^1.2.0');               // true
satisfies('2.0.0', '>=1.0.0 <2.0.0 || 3.x'); // false
satisfies('1.3.0-rc.1', '^1.2.0');          // false (see prereleases below)
```

A range is one or more comparator sets joined by `||`; a version satisfies
the range if it satisfies any set. A set is whitespace-separated comparators,
all of which must hold; an empty set matches everything. A comparator is an
optional operator (`<`, `<=`, `>`, `>=`, `=`, `~`, `^`; none means `=`) and a
version, which may start with `v` and may be partial (`1`, `1.2`) or use
wildcards (`*`, `x`, `X`). The shorthands expand like this:

| range             | means                     |
| ----------------- | ------------------------- |
| `*`, `x`, empty   | any version               |
| `1`, `1.x`        | `>=1.0.0 <2.0.0`          |
| `1.2`, `1.2.x`    | `>=1.2.0 <1.3.0`          |
| `>1.2`            | `>=1.3.0`                 |
| `<=1.2`           | `<1.3.0`                  |
| `>*`, `<*`        | nothing                   |
| `1.2.3 - 2.3.4`   | `>=1.2.3 <=2.3.4`         |
| `1.2 - 2.3`       | `>=1.2.0 <2.4.0`          |
| `~1.2.3`          | `>=1.2.3 <1.3.0`          |
| `~1.2`            | `>=1.2.0 <1.3.0`          |
| `~1`              | `>=1.0.0 <2.0.0`          |
| `^1.2.3`          | `>=1.2.3 <2.0.0`          |
| `^0.2.3`          | `>=0.2.3 <0.3.0`          |
| `^0.0.3`          | `>=0.0.3 <0.0.4`          |
| `^1.x`            | `>=1.0.0 <2.0.0`          |
| `^0.x`            | `>=0.0.0 <1.0.0`          |
| `^0.0`            | `>=0.0.0 <0.1.0`          |

Prereleases: a version with a prerelease tag only satisfies a set if some
comparator in that set names a prerelease of the same major.minor.patch, so
`1.3.0-rc.1` satisfies `>=1.3.0-rc.0 <1.4.0` but not `^1.2.0`.

An invalid range throws `TypeError('Invalid range: <range>')`; an invalid
version throws the `TypeError` from `new SemVer`.

## Picking versions

```js
import { maxSatisfying } from './src/index.js';

maxSatisfying(['1.2.3', '1.2.4', '1.3.0', '2.0.0'], '~1.2.0'); // '1.2.4'
```

`maxSatisfying(versions, range)` / `minSatisfying(versions, range)` return
the highest / lowest entry of `versions` (as given) that satisfies `range`,
or `null`. Entries that are not valid versions are skipped.

## CLI

```sh
node bin/pkgver.js sort 1.10.0 1.9.0
node bin/pkgver.js satisfies 1.2.3 '^1.0.0'
node bin/pkgver.js max '~1.2.0' 1.2.3 1.2.4 1.3.0
```

Run `node bin/pkgver.js help` for the full list.

## Tests

Requires Node 20 or newer; no dependencies.

```sh
npm test
# or
node --test test/semver.test.js test/select.test.js test/cli.test.js
```
