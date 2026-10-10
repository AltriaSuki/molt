// npm-style version ranges: "^1.2.0", "~2.4", ">=1.0.0 <2.0.0 || 3.x", "1.2 - 2.0", ...
//
// maxSatisfying / minSatisfying (select.js) and the CLI's `satisfies`, `max`
// and `min` commands all go through satisfies() below.
//
// A range is parsed into a list of comparator sets (the "||" alternatives),
// each a list of primitive comparators (<, <=, >, >=, = against a full
// version). The shorthands (x-ranges, partial versions, hyphen ranges, ~ and
// ^) are expanded into primitives while parsing; see README.md for the table.

import { SemVer } from './semver.js';

const NUMBER = /^(0|[1-9]\d*)$/;
const WILDCARD = /^[xX*]$/;
const OPERATOR = /^(<=|>=|<|>|=|~|\^)?(.*)$/s;
const OPERATOR_SPACE = /(<=|>=|<|>|=|~|\^)\s+/g;
const HYPHEN = /\s+-\s+/;

function invalidRange(range) {
  return new TypeError(`Invalid range: ${String(range)}`);
}

function version(major, minor, patch) {
  return new SemVer(`${major}.${minor}.${patch}`);
}

class Comparator {
  /**
   * @param {'<'|'<='|'>'|'>='|'='} operator
   * @param {SemVer} semver
   */
  constructor(operator, semver) {
    this.operator = operator;
    this.semver = semver;
  }

  test(v) {
    const c = v.compare(this.semver);
    switch (this.operator) {
      case '<':
        return c < 0;
      case '<=':
        return c <= 0;
      case '>':
        return c > 0;
      case '>=':
        return c >= 0;
      default:
        return c === 0;
    }
  }

  toString() {
    return `${this.operator === '=' ? '' : this.operator}${this.semver.version}`;
  }
}

// Matches every version (still subject to the prerelease rule).
const ANY = { semver: null, test: () => true, toString: () => '*' };
// Matches no version at all (">*", "<x").
const NONE = { semver: null, test: () => false, toString: () => '<0.0.0' };

/**
 * Parse a partial version: an optional "v", then one to three parts, each a
 * number or a wildcard. Missing and wildcard parts come back as null. A
 * prerelease or build suffix is only allowed after three numbers; `full` is
 * then the complete SemVer. Returns null if `text` is not a partial version.
 */
function parsePartial(text) {
  const body = text.startsWith('v') ? text.slice(1) : text;
  const suffixAt = body.search(/[-+]/);
  const main = suffixAt === -1 ? body : body.slice(0, suffixAt);
  const suffix = suffixAt === -1 ? '' : body.slice(suffixAt);

  const parts = main.split('.');
  if (parts.length > 3) return null;
  const nums = [];
  for (const part of parts) {
    if (WILDCARD.test(part)) {
      nums.push(null);
    } else if (NUMBER.test(part) && !nums.includes(null)) {
      const n = Number(part);
      if (!Number.isSafeInteger(n)) return null;
      nums.push(n);
    } else {
      return null; // not a number, or a number after a wildcard ("1.x.3")
    }
  }
  while (nums.length < 3) nums.push(null);
  const [major, minor, patch] = nums;

  let full = null;
  if (patch !== null) {
    try {
      full = new SemVer(main + suffix);
    } catch {
      return null;
    }
  } else if (suffix) {
    return null; // "1.2-beta", "1.x-rc.1"
  }
  return { major, minor, patch, full };
}

// The lower bound of a partial: missing parts filled with zeros.
function floor(p) {
  return p.full ?? version(p.major, p.minor ?? 0, p.patch ?? 0);
}

// The first version above every version a partial covers: "1" -> 2.0.0, "1.2" -> 1.3.0.
// Only for partials with a numeric major and a missing patch.
function ceiling(p) {
  return p.minor === null ? version(p.major + 1, 0, 0) : version(p.major, p.minor + 1, 0);
}

function expandTilde(p) {
  const upper = p.minor === null ? version(p.major + 1, 0, 0) : version(p.major, p.minor + 1, 0);
  return [new Comparator('>=', floor(p)), new Comparator('<', upper)];
}

function expandCaret(p) {
  let upper;
  if (p.major > 0 || p.minor === null) {
    upper = version(p.major + 1, 0, 0);
  } else if (p.minor > 0 || p.patch === null) {
    upper = version(0, p.minor + 1, 0);
  } else {
    upper = version(0, 0, p.patch + 1);
  }
  return [new Comparator('>=', floor(p)), new Comparator('<', upper)];
}

function expandComparator(operator, p) {
  if (p.major === null) return operator === '<' || operator === '>' ? [NONE] : [ANY];
  if (operator === '~') return expandTilde(p);
  if (operator === '^') return expandCaret(p);
  if (p.full) return [new Comparator(operator || '=', p.full)];
  switch (operator) {
    case '>':
      return [new Comparator('>=', ceiling(p))];
    case '>=':
      return [new Comparator('>=', floor(p))];
    case '<':
      return [new Comparator('<', floor(p))];
    case '<=':
      return [new Comparator('<', ceiling(p))];
    default: // "", "="
      return [new Comparator('>=', floor(p)), new Comparator('<', ceiling(p))];
  }
}

function expandHyphen(lo, hi) {
  const set = [];
  if (lo.major !== null) set.push(new Comparator('>=', floor(lo)));
  if (hi.major !== null) {
    set.push(hi.full ? new Comparator('<=', hi.full) : new Comparator('<', ceiling(hi)));
  }
  return set.length ? set : [ANY];
}

function parseSet(text, range) {
  const trimmed = text.trim();
  if (trimmed === '') return [ANY];

  const bounds = trimmed.split(HYPHEN);
  if (bounds.length > 1) {
    // A hyphen range is the whole set: exactly two bare partial versions.
    if (bounds.length !== 2 || bounds.some((b) => /\s/.test(b))) throw invalidRange(range);
    const lo = parsePartial(bounds[0]);
    const hi = parsePartial(bounds[1]);
    if (!lo || !hi) throw invalidRange(range);
    return expandHyphen(lo, hi);
  }

  const set = [];
  for (const token of trimmed.replace(OPERATOR_SPACE, '$1').split(/\s+/)) {
    const [, operator = '', rest] = OPERATOR.exec(token);
    const p = parsePartial(rest);
    if (!p) throw invalidRange(range);
    set.push(...expandComparator(operator, p));
  }
  return set;
}

function testSet(set, v) {
  if (!set.every((c) => c.test(v))) return false;
  if (v.prerelease.length === 0) return true;
  // A prerelease only matches if the set names a prerelease of the same
  // major.minor.patch: ^1.2.3 must not pull in 1.3.0-alpha.
  return set.some(
    (c) =>
      c.semver !== null &&
      c.semver.prerelease.length > 0 &&
      c.semver.major === v.major &&
      c.semver.minor === v.minor &&
      c.semver.patch === v.patch,
  );
}

export class Range {
  /**
   * @param {string} range
   * @throws {TypeError} `Invalid range: <range>` if `range` is not a valid range
   */
  constructor(range) {
    if (typeof range !== 'string') throw invalidRange(range);
    this.raw = range;
    this.set = range.split('||').map((part) => parseSet(part, range));
  }

  /** @param {string|SemVer} version */
  test(version) {
    const v = version instanceof SemVer ? version : new SemVer(version);
    return this.set.some((set) => testSet(set, v));
  }

  /** The expanded form, e.g. "^1.2" -> ">=1.2.0 <2.0.0". */
  toString() {
    return this.set.map((set) => set.map(String).join(' ')).join(' || ');
  }
}

/**
 * Does `version` satisfy `range`?
 *
 * @param {string|SemVer} version
 * @param {string} range
 * @returns {boolean}
 * @throws {TypeError} if the version or the range is invalid
 */
export function satisfies(version, range) {
  const v = version instanceof SemVer ? version : new SemVer(version);
  return new Range(range).test(v);
}
