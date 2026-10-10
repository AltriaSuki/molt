/**
 * Loads the effective configuration from up to four layers, later layers
 * winning: `defaults`, the JSON config `file`, environment overrides
 * (`<prefix>__...` variables, see env.js) and programmatic `overrides`
 * (command-line flags, test fixtures, injected services).
 */
import { readFileSync } from 'node:fs';

import { parseEnv } from './env.js';
import { ConfigError } from './errors.js';
import { deepMerge } from './merge.js';

function defaultReadFile(path) {
  return readFileSync(path, 'utf8');
}

/** Reads and parses a JSON config file, which must hold a JSON object. */
export function readConfigFile(path, readFile = defaultReadFile) {
  let text;
  try {
    text = readFile(path);
  } catch (err) {
    throw new ConfigError(`cannot read config file ${path}: ${err.message}`, { cause: err });
  }
  let data;
  try {
    data = JSON.parse(text);
  } catch (err) {
    throw new ConfigError(`invalid JSON in config file ${path}: ${err.message}`, { cause: err });
  }
  if (data === null || typeof data !== 'object' || Array.isArray(data)) {
    throw new ConfigError(`config file ${path} must contain a JSON object`);
  }
  return data;
}

/**
 * Options:
 * - `defaults`: the base configuration (default `{}`).
 * - `file`: path of a JSON config file; skipped when undefined.
 * - `env`: the environment to read overrides from (default `process.env`).
 * - `prefix`: the env variable prefix (default `"APP"`).
 * - `overrides`: values that win over everything else (default `{}`).
 * - `readFile`: `(path) => string`, for tests (default `fs.readFileSync`).
 */
export function loadConfig(options = {}) {
  const {
    defaults = {},
    file,
    env = process.env,
    prefix = 'APP',
    overrides = {},
    readFile = defaultReadFile,
  } = options;

  const layers = [defaults];
  if (file !== undefined) layers.push(readConfigFile(file, readFile));
  layers.push(parseEnv(env, { prefix }));
  layers.push(overrides);
  return deepMerge(...layers);
}
