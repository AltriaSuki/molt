/**
 * Environment overrides.
 *
 * A variable named `<PREFIX>__a__b__c` sets the config path `a.b.c`: the
 * double underscore separates levels and segment names are used as written.
 * Values are parsed as JSON when they parse (`5432`, `true`, `null`,
 * `["a","b"]`, `{"x":1}`, `"quoted"`), and kept as plain strings otherwise.
 *
 * A variable whose path goes through `__proto__`, `constructor` or
 * `prototype` is ignored, and those keys are dropped from JSON values too.
 */
import { deepMerge, isIgnoredKey } from './merge.js';

export const SEPARATOR = '__';

/** Parses one environment value: JSON when it parses, the raw string otherwise. */
export function parseValue(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/**
 * The config path a variable name addresses, or null when the name does not
 * start with `<prefix>__`, has an empty segment (`APP__db____port`) or goes
 * through a key that is never merged (`APP__constructor__prototype__x`).
 */
export function envPath(name, prefix) {
  const lead = prefix + SEPARATOR;
  if (!name.startsWith(lead)) return null;
  const path = name.slice(lead.length).split(SEPARATOR);
  if (path.some((segment) => segment === '' || isIgnoredKey(segment))) return null;
  return path;
}

function nest(path, value) {
  return path.reduceRight((inner, key) => ({ [key]: inner }), value);
}

/**
 * Collects the overrides in `env` (e.g. `process.env`) for `prefix` into one
 * nested object. Variables are applied in name order, so `APP__db='{"host":"x"}'`
 * and `APP__db__port=5432` combine into `{ db: { host: 'x', port: 5432 } }`.
 */
export function parseEnv(env, { prefix = 'APP' } = {}) {
  if (typeof prefix !== 'string' || prefix === '' || prefix.includes(SEPARATOR)) {
    throw new TypeError(`invalid env prefix: ${JSON.stringify(prefix)}`);
  }
  const layers = [];
  for (const name of Object.keys(env).sort()) {
    const path = envPath(name, prefix);
    if (path === null) continue;
    const raw = env[name];
    if (typeof raw !== 'string') continue;
    layers.push(nest(path, parseValue(raw)));
  }
  return deepMerge({}, ...layers);
}
