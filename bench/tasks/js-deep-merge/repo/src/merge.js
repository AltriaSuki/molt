/**
 * Deep merge for configuration layers.
 *
 * `deepMerge(target, ...sources)` merges the sources into `target` from left
 * to right, so later sources win. Nested objects are merged key by key, any
 * other value from a source replaces what was there.
 */

function isObject(value) {
  return typeof value === 'object';
}

export function deepMerge(target, ...sources) {
  for (const source of sources) {
    for (const key of Object.keys(source)) {
      const value = source[key];
      if (isObject(value) && isObject(target[key])) {
        deepMerge(target[key], value);
      } else if (isObject(value)) {
        target[key] = deepMerge(Array.isArray(value) ? [] : {}, value);
      } else {
        target[key] = value;
      }
    }
  }
  return target;
}
