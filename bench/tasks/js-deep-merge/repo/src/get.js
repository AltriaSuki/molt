/**
 * Reading values out of a loaded config by dotted path (`"db.port"`) or by
 * an array of segments (`["db", "port"]`). Only own properties are followed.
 */
import { ConfigError } from './errors.js';

function segments(path) {
  if (Array.isArray(path)) return path;
  if (typeof path !== 'string' || path === '') {
    throw new TypeError(`invalid config path: ${JSON.stringify(path)}`);
  }
  return path.split('.');
}

/** The value at `path`, or `fallback` when it is missing or undefined. */
export function getPath(config, path, fallback = undefined) {
  let node = config;
  for (const key of segments(path)) {
    if (node === null || typeof node !== 'object' || !Object.hasOwn(node, key)) {
      return fallback;
    }
    node = node[key];
  }
  return node === undefined ? fallback : node;
}

/** Throws a ConfigError naming every path in `paths` that has no value. */
export function requirePaths(config, paths) {
  const missing = paths.filter((path) => getPath(config, path) === undefined);
  if (missing.length > 0) {
    const names = missing.map((path) => (Array.isArray(path) ? path.join('.') : path));
    throw new ConfigError(`missing required config: ${names.join(', ')}`);
  }
}
