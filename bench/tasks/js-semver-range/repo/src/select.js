// Picking versions out of a list (e.g. the versions a registry has published).

import { parse } from './semver.js';
import { satisfies } from './range.js';

// Registries hand back the odd tag that is not a version ("latest", "1.0",
// "nightly-2024"); those are skipped rather than failing the whole lookup.
function* matching(versions, range) {
  for (const raw of versions) {
    const version = parse(raw);
    if (version === null) continue;
    if (satisfies(version, range)) yield { raw, version };
  }
}

/** The entries of `versions` that satisfy `range`, in their original order. */
export function filterSatisfying(versions, range) {
  return Array.from(matching(versions, range), (m) => m.raw);
}

/**
 * The highest entry of `versions` that satisfies `range`, as given in the
 * list, or null if none does. On a precedence tie the earlier entry wins.
 */
export function maxSatisfying(versions, range) {
  let best = null;
  for (const m of matching(versions, range)) {
    if (best === null || m.version.compare(best.version) > 0) best = m;
  }
  return best === null ? null : best.raw;
}

/**
 * The lowest entry of `versions` that satisfies `range`, as given in the
 * list, or null if none does. On a precedence tie the earlier entry wins.
 */
export function minSatisfying(versions, range) {
  let best = null;
  for (const m of matching(versions, range)) {
    if (best === null || m.version.compare(best.version) < 0) best = m;
  }
  return best === null ? null : best.raw;
}
