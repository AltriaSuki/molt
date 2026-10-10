import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { envPath, parseEnv, parseValue } from '../src/env.js';

describe('parseValue', () => {
  test('parses JSON values', () => {
    assert.equal(parseValue('5432'), 5432);
    assert.equal(parseValue('1.5'), 1.5);
    assert.equal(parseValue('true'), true);
    assert.equal(parseValue('false'), false);
    assert.equal(parseValue('"quoted"'), 'quoted');
    assert.deepEqual(parseValue('["a","b"]'), ['a', 'b']);
    assert.deepEqual(parseValue('{"x":1}'), { x: 1 });
  });

  test('keeps anything else as a string', () => {
    assert.equal(parseValue('localhost'), 'localhost');
    assert.equal(parseValue('0123'), '0123');
    assert.equal(parseValue(''), '');
    assert.equal(parseValue('{oops'), '{oops');
  });
});

describe('envPath', () => {
  test('splits the name after the prefix on double underscores', () => {
    assert.deepEqual(envPath('APP__db__port', 'APP'), ['db', 'port']);
    assert.deepEqual(envPath('APP__logLevel', 'APP'), ['logLevel']);
    assert.deepEqual(envPath('APP__db_name', 'APP'), ['db_name']);
  });

  test('returns null for other variables and empty segments', () => {
    assert.equal(envPath('PATH', 'APP'), null);
    assert.equal(envPath('APP_db', 'APP'), null);
    assert.equal(envPath('APPLE__x', 'APP'), null);
    assert.equal(envPath('APP__', 'APP'), null);
    assert.equal(envPath('APP__db____port', 'APP'), null);
  });
});

describe('parseEnv', () => {
  test('builds nested overrides from prefixed variables', () => {
    const env = {
      APP__db__port: '5432',
      APP__db__host: 'db.internal',
      APP__features__beta: 'true',
      HOME: '/home/svc',
      PATH: '/usr/bin',
    };
    assert.deepEqual(parseEnv(env), {
      db: { port: 5432, host: 'db.internal' },
      features: { beta: true },
    });
  });

  test('uses a custom prefix', () => {
    const env = { BILLING__currency: 'EUR', APP__currency: 'USD' };
    assert.deepEqual(parseEnv(env, { prefix: 'BILLING' }), { currency: 'EUR' });
  });

  test('keeps segment names as written', () => {
    assert.deepEqual(parseEnv({ APP__Db__maxPool: '10' }), { Db: { maxPool: 10 } });
  });

  test('combines a JSON object value with deeper variables', () => {
    const env = { APP__db__port: '6543', APP__db: '{"host":"x","port":1}' };
    assert.deepEqual(parseEnv(env), { db: { host: 'x', port: 6543 } });
  });

  test('ignores variables with empty segments', () => {
    assert.deepEqual(parseEnv({ APP__db____port: '1', APP__ok: 'yes' }), { ok: 'yes' });
  });

  test('returns an empty object when nothing matches', () => {
    assert.deepEqual(parseEnv({}), {});
    assert.deepEqual(parseEnv({ HOME: '/root' }), {});
  });

  test('rejects an invalid prefix', () => {
    assert.throws(() => parseEnv({}, { prefix: '' }), TypeError);
    assert.throws(() => parseEnv({}, { prefix: 'A__B' }), TypeError);
  });
});
