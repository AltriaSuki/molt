// SemVer 2.0.0 versions: parsing, precedence and a few helpers.
// Spec: https://semver.org/spec/v2.0.0.html

const NUMERIC = '0|[1-9]\\d*';
const PRERELEASE_ID = `(?:${NUMERIC}|\\d*[A-Za-z-][0-9A-Za-z-]*)`;
const BUILD_ID = '[0-9A-Za-z-]+';

// An optional leading "v" is accepted ("v1.2.3"), as git tags often carry one.
const VERSION_RE = new RegExp(
  `^v?(${NUMERIC})\\.(${NUMERIC})\\.(${NUMERIC})` +
    `(?:-(${PRERELEASE_ID}(?:\\.${PRERELEASE_ID})*))?` +
    `(?:\\+(${BUILD_ID}(?:\\.${BUILD_ID})*))?$`,
);

const DIGITS = /^\d+$/;

function invalid(input, detail) {
  const shown = typeof input === 'string' ? input : String(input);
  return new TypeError(`Invalid version: ${shown}${detail ? ` (${detail})` : ''}`);
}

function toNumber(text, what, input) {
  const n = Number(text);
  if (!Number.isSafeInteger(n)) throw invalid(input, `${what} is too large`);
  return n;
}

// Precedence of two prerelease identifiers (SemVer 2.0.0, section 11.4).
function compareIdentifiers(a, b) {
  const aNum = typeof a === 'number';
  const bNum = typeof b === 'number';
  if (aNum && bNum) return a === b ? 0 : a < b ? -1 : 1;
  if (aNum) return -1; // numeric identifiers sort before alphanumeric ones
  if (bNum) return 1;
  return a === b ? 0 : a < b ? -1 : 1; // ASCII order
}

export class SemVer {
  /**
   * @param {string|SemVer} input  "1.2.3", "v1.2.3-beta.1+build.5", ...
   * @throws {TypeError} if `input` is not a valid version
   */
  constructor(input) {
    if (input instanceof SemVer) {
      this.major = input.major;
      this.minor = input.minor;
      this.patch = input.patch;
      this.prerelease = [...input.prerelease];
      this.build = [...input.build];
      this.version = input.version;
      this.raw = input.raw;
      return;
    }
    if (typeof input !== 'string') throw invalid(input);
    const m = VERSION_RE.exec(input.trim());
    if (!m) throw invalid(input);

    this.major = toNumber(m[1], 'major', input);
    this.minor = toNumber(m[2], 'minor', input);
    this.patch = toNumber(m[3], 'patch', input);
    this.prerelease = m[4]
      ? m[4].split('.').map((id) => (DIGITS.test(id) ? toNumber(id, 'prerelease', input) : id))
      : [];
    this.build = m[5] ? m[5].split('.') : [];
    this.raw = input;
    this.version = this.format();
  }

  /** major.minor.patch[-prerelease], without build metadata. */
  format() {
    const main = `${this.major}.${this.minor}.${this.patch}`;
    return this.prerelease.length ? `${main}-${this.prerelease.join('.')}` : main;
  }

  toString() {
    return this.version;
  }

  /** Compare major, minor and patch only. Returns -1, 0 or 1. */
  compareMain(other) {
    const o = other instanceof SemVer ? other : new SemVer(other);
    for (const key of ['major', 'minor', 'patch']) {
      if (this[key] !== o[key]) return this[key] < o[key] ? -1 : 1;
    }
    return 0;
  }

  /** Compare prerelease tags only. A version without one ranks higher. */
  comparePre(other) {
    const o = other instanceof SemVer ? other : new SemVer(other);
    const a = this.prerelease;
    const b = o.prerelease;
    if (a.length && !b.length) return -1;
    if (!a.length && b.length) return 1;
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      if (i >= a.length) return -1; // a is a prefix of b
      if (i >= b.length) return 1;
      const c = compareIdentifiers(a[i], b[i]);
      if (c !== 0) return c;
    }
    return 0;
  }

  /** SemVer precedence: build metadata is ignored. Returns -1, 0 or 1. */
  compare(other) {
    const o = other instanceof SemVer ? other : new SemVer(other);
    return this.compareMain(o) || this.comparePre(o);
  }

  /** Like compare, but versions that only differ in build metadata are ordered by it. */
  compareBuild(other) {
    const o = other instanceof SemVer ? other : new SemVer(other);
    const c = this.compare(o);
    if (c !== 0) return c;
    const a = this.build.join('.');
    const b = o.build.join('.');
    if (a === b) return 0;
    if (!a) return -1;
    if (!b) return 1;
    return a < b ? -1 : 1;
  }
}

/** Parse a version, returning null instead of throwing. */
export function parse(input) {
  if (input instanceof SemVer) return input;
  try {
    return new SemVer(input);
  } catch (err) {
    if (err instanceof TypeError) return null;
    throw err;
  }
}

/** The normalized version string ("v1.2.3+b" -> "1.2.3"), or null if invalid. */
export function valid(input) {
  const v = parse(input);
  return v === null ? null : v.version;
}

export function compare(a, b) {
  return new SemVer(a).compare(new SemVer(b));
}

export function rcompare(a, b) {
  return compare(b, a);
}

export const eq = (a, b) => compare(a, b) === 0;
export const neq = (a, b) => compare(a, b) !== 0;
export const gt = (a, b) => compare(a, b) > 0;
export const gte = (a, b) => compare(a, b) >= 0;
export const lt = (a, b) => compare(a, b) < 0;
export const lte = (a, b) => compare(a, b) <= 0;

/** A new array with the versions in ascending order (build metadata breaks ties). */
export function sort(versions) {
  return [...versions].sort((a, b) => new SemVer(a).compareBuild(new SemVer(b)));
}

/** A new array with the versions in descending order. */
export function rsort(versions) {
  return [...versions].sort((a, b) => new SemVer(b).compareBuild(new SemVer(a)));
}

const RELEASES = new Set(['major', 'minor', 'patch', 'prerelease']);

/**
 * The next version for a release type.
 *
 *   inc('1.2.3', 'minor')              -> '1.3.0'
 *   inc('1.3.0-rc.1', 'minor')         -> '1.3.0'   (finishes the prerelease)
 *   inc('1.2.3', 'prerelease', 'rc')   -> '1.2.4-rc.0'
 *   inc('1.2.4-rc.0', 'prerelease')    -> '1.2.4-rc.1'
 *
 * Returns null if `version` is not valid.
 */
export function inc(version, release, identifier) {
  if (!RELEASES.has(release)) throw new TypeError(`Unknown release type: ${release}`);
  const v = parse(version);
  if (v === null) return null;
  let { major, minor, patch } = v;
  let prerelease = [...v.prerelease];
  const isPre = prerelease.length > 0;

  switch (release) {
    case 'major':
      if (!(isPre && minor === 0 && patch === 0)) major += 1;
      minor = 0;
      patch = 0;
      prerelease = [];
      break;
    case 'minor':
      if (!(isPre && patch === 0)) minor += 1;
      patch = 0;
      prerelease = [];
      break;
    case 'patch':
      if (!isPre) patch += 1;
      prerelease = [];
      break;
    case 'prerelease':
      if (!isPre) {
        patch += 1;
        prerelease = identifier ? [identifier, 0] : [0];
      } else if (identifier && prerelease[0] !== identifier) {
        prerelease = [identifier, 0];
      } else {
        let i = prerelease.length - 1;
        while (i >= 0 && typeof prerelease[i] !== 'number') i--;
        if (i >= 0) prerelease[i] += 1;
        else prerelease.push(0);
      }
      break;
  }
  const main = `${major}.${minor}.${patch}`;
  return prerelease.length ? `${main}-${prerelease.join('.')}` : main;
}
