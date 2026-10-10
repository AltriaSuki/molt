// npm-style version ranges: "^1.2.0", "~2.4", ">=1.0.0 <2.0.0 || 3.x", "1.2 - 2.0", ...
//
// maxSatisfying / minSatisfying (select.js) and the CLI's `satisfies`, `max`
// and `min` commands all go through satisfies() below.

/**
 * Does `version` satisfy `range`?
 *
 * @param {string|import('./semver.js').SemVer} version
 * @param {string} range
 * @returns {boolean}
 */
export function satisfies(version, range) {
  throw new Error('not implemented');
}
