import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { ConfigError } from '../src/errors.js';
import { getPath, requirePaths } from '../src/get.js';
import { REDACTED, redact } from '../src/redact.js';

const config = {
  db: { host: 'localhost', port: 5432, password: 'hunter2', replica: null },
  log: { level: 'info' },
  flags: [{ name: 'beta', token: 't0k' }],
};

describe('getPath', () => {
  test('reads dotted and array paths', () => {
    assert.equal(getPath(config, 'db.port'), 5432);
    assert.equal(getPath(config, ['log', 'level']), 'info');
    assert.deepEqual(getPath(config, 'log'), { level: 'info' });
  });

  test('returns the fallback for missing values', () => {
    assert.equal(getPath(config, 'db.user', 'postgres'), 'postgres');
    assert.equal(getPath(config, 'cache.redis.host'), undefined);
    assert.equal(getPath(config, 'db.port.value', 1), 1);
  });

  test('null is a value', () => {
    assert.equal(getPath(config, 'db.replica', 'fallback'), null);
  });

  test('only follows own properties', () => {
    assert.equal(getPath(config, 'db.toString'), undefined);
    assert.equal(getPath(config, 'constructor'), undefined);
  });

  test('rejects an empty path', () => {
    assert.throws(() => getPath(config, ''), TypeError);
  });
});

describe('requirePaths', () => {
  test('passes when everything is set', () => {
    requirePaths(config, ['db.host', ['log', 'level']]);
  });

  test('names every missing path', () => {
    assert.throws(() => requirePaths(config, ['db.host', 'db.user', ['cache', 'ttl']]), {
      name: 'ConfigError',
      message: 'missing required config: db.user, cache.ttl',
    });
    assert.throws(() => requirePaths(config, ['nope']), ConfigError);
  });
});

describe('redact', () => {
  test('masks secret keys at any depth and in arrays', () => {
    assert.deepEqual(redact(config), {
      db: { host: 'localhost', port: 5432, password: REDACTED, replica: null },
      log: { level: 'info' },
      flags: [{ name: 'beta', token: REDACTED }],
    });
  });

  test('compares keys case-insensitively and accepts custom keys', () => {
    assert.deepEqual(redact({ APIKEY: 'k', user: 'u' }), { APIKEY: REDACTED, user: 'u' });
    assert.deepEqual(redact({ user: 'u', password: 'p' }, { keys: ['user'] }), { user: REDACTED, password: 'p' });
  });

  test('leaves unset secrets alone and does not modify its input', () => {
    const input = { db: { password: null } };
    assert.deepEqual(redact(input), { db: { password: null } });
    redact(config);
    assert.equal(config.db.password, 'hunter2');
  });
});
