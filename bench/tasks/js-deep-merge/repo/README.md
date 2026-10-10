# confmerge

Layered configuration loading for Node services, with no dependencies.

A service's effective configuration is built from up to four layers, each
one winning over the ones before it:

1. `defaults` shipped with the service,
2. a JSON config file,
3. environment overrides such as `APP__db__port=5432`,
4. programmatic `overrides` (command-line flags, test fixtures, injected
   services such as a logger or a clock).

## Usage

```js
import { loadConfig, getPath, redact } from './src/index.js';

const config = loadConfig({
  defaults: { db: { host: 'localhost', port: 5432 }, log: { level: 'info' } },
  file: process.env.CONFIG_FILE,      // optional
  env: process.env,                   // the default
  prefix: 'APP',                      // the default
  overrides: { log: { level: argv.verbose ? 'debug' : undefined } },
});

getPath(config, 'db.port');           // 5432, or what the file / env said
console.log(redact(config));          // passwords and tokens masked
```

## Modules

- `src/merge.js`: `deepMerge(target, ...sources)`, the layer merge. Plain
  objects are merged key by key; for any other value the later layer wins.
- `src/env.js`: `parseEnv(env, { prefix })` turns `APP__a__b=value` variables
  into `{ a: { b: value } }`. The double underscore separates levels; values
  are parsed as JSON when they parse (`5432`, `true`, `null`, `["a","b"]`) and
  kept as strings otherwise. Variables with an empty segment are ignored.
- `src/loader.js`: `loadConfig(options)` and `readConfigFile(path)`. File
  problems raise a `ConfigError`.
- `src/get.js`: `getPath(config, path, fallback)` and
  `requirePaths(config, paths)`.
- `src/redact.js`: `redact(config)` for logging the effective config.
- `src/errors.js`: `ConfigError`.

## Tests

Requires Node 20 or newer.

```sh
npm test
# or
node --test test/merge.test.js test/env.test.js test/loader.test.js test/util.test.js
```
