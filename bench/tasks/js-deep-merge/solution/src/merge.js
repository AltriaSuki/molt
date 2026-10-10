/**
 * Deep merge for configuration layers.
 *
 * `deepMerge(target, ...sources)` merges all of its arguments, `target`
 * included, from left to right into a new object, so later layers win. It
 * never modifies its arguments.
 *
 * - Only plain objects (prototype `Object.prototype` or `null`) are layers or
 *   merged recursively; any other argument is ignored.
 * - Only own enumerable string keys count, and the keys `__proto__`,
 *   `constructor` and `prototype` are ignored at every depth.
 * - `undefined` values are skipped; `null` overrides.
 * - Arrays replace what was there with a shallow copy.
 * - Any other value (primitives, functions, Dates, Maps, class instances, ...)
 *   replaces what was there, by reference.
 * - Plain objects in the result are always fresh ordinary objects, so the
 *   result shares no plain object or array with the arguments.
 */

const IGNORED_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

/** True for the keys deepMerge (and the env loader) never copy. */
export function isIgnoredKey(key) {
  return IGNORED_KEYS.has(key);
}

/** True for objects whose prototype is `Object.prototype` or `null`. */
export function isPlainObject(value) {
  if (value === null || typeof value !== 'object') return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

function copyValue(value) {
  if (isPlainObject(value)) return mergeInto({}, value);
  if (Array.isArray(value)) return value.slice();
  return value;
}

// Merges `source` into `out`, an object this module created and owns.
function mergeInto(out, source) {
  for (const key of Object.keys(source)) {
    if (IGNORED_KEYS.has(key)) continue;
    const value = source[key];
    if (value === undefined) continue;
    if (isPlainObject(value) && Object.hasOwn(out, key) && isPlainObject(out[key])) {
      mergeInto(out[key], value);
    } else {
      out[key] = copyValue(value);
    }
  }
  return out;
}

export function deepMerge(target, ...sources) {
  const result = {};
  for (const layer of [target, ...sources]) {
    if (isPlainObject(layer)) mergeInto(result, layer);
  }
  return result;
}
