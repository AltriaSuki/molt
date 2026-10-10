import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { ConfigError } from '../src/errors.js';
import { loadConfig, readConfigFile } from '../src/loader.js';

function filesystem(files) {
  return (path) => {
    if (!Object.hasOwn(files, path)) {
      const err = new Error(`ENOENT: no such file or directory, open '${path}'`);
      err.code = 'ENOENT';
      throw err;
    }
    return files[path];
  };
}

describe('readConfigFile', () => {
  test('parses a JSON object', () => {
    const readFile = filesystem({ '/etc/app.json': '{"db": {"port": 1}}' });
    assert.deepEqual(readConfigFile('/etc/app.json', readFile), { db: { port: 1 } });
  });

  test('reports unreadable files', () => {
    assert.throws(() => readConfigFile('/missing.json', filesystem({})), {
      name: 'ConfigError',
      message: /cannot read config file \/missing\.json/,
    });
  });

  test('reports invalid JSON', () => {
    const readFile = filesystem({ '/bad.json': '{"db": ' });
    assert.throws(() => readConfigFile('/bad.json', readFile), {
      name: 'ConfigError',
      message: /invalid JSON in config file \/bad\.json/,
    });
  });

  test('requires a JSON object', () => {
    for (const text of ['[1, 2]', '42', 'null', '"text"']) {
      const readFile = filesystem({ '/x.json': text });
      assert.throws(() => readConfigFile('/x.json', readFile), ConfigError);
    }
  });
});

describe('loadConfig', () => {
  test('layers defaults, file, env and overrides in that order', () => {
    const config = loadConfig({
      defaults: { db: { host: 'localhost', port: 5432, name: 'app' }, log: { level: 'info' } },
      file: '/etc/app.json',
      readFile: filesystem({ '/etc/app.json': '{"db": {"host": "db.prod", "port": 6000}, "log": {"level": "warn"}}' }),
      env: { APP__db__port: '7000', APP__log__level: 'error', OTHER: 'x' },
      overrides: { log: { level: 'debug' } },
    });
    assert.deepEqual(config, {
      db: { host: 'db.prod', port: 7000, name: 'app' },
      log: { level: 'debug' },
    });
  });

  test('works without a file', () => {
    const config = loadConfig({ defaults: { port: 8080, name: 'svc' }, env: { APP__port: '9090' } });
    assert.deepEqual(config, { port: 9090, name: 'svc' });
  });

  test('uses the given env prefix', () => {
    const config = loadConfig({
      defaults: { port: 8080 },
      env: { APP__port: '1', WORKER__port: '2' },
      prefix: 'WORKER',
    });
    assert.deepEqual(config, { port: 2 });
  });

  test('works with no options and an empty environment', () => {
    assert.deepEqual(loadConfig({ env: {} }), {});
  });

  test('propagates file errors', () => {
    assert.throws(
      () => loadConfig({ file: '/nope.json', env: {}, readFile: filesystem({}) }),
      ConfigError,
    );
  });
});
