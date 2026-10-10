import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { deepMerge } from '../src/merge.js';

describe('deepMerge', () => {
  test('merges nested objects key by key', () => {
    const merged = deepMerge(
      { db: { host: 'localhost', port: 5432 }, log: { level: 'info' } },
      { db: { port: 6543 } },
    );
    assert.deepEqual(merged, { db: { host: 'localhost', port: 6543 }, log: { level: 'info' } });
  });

  test('later sources win', () => {
    const merged = deepMerge({ level: 'info' }, { level: 'warn' }, { level: 'debug' });
    assert.deepEqual(merged, { level: 'debug' });
  });

  test('adds keys the earlier layers do not have', () => {
    const merged = deepMerge({ a: 1 }, { b: { c: 2 } }, { d: 'x' });
    assert.deepEqual(merged, { a: 1, b: { c: 2 }, d: 'x' });
  });

  test('merges several levels deep', () => {
    const merged = deepMerge(
      { cache: { redis: { host: 'r1', tls: { enabled: false, ca: 'none' } } } },
      { cache: { redis: { tls: { enabled: true } } } },
      { cache: { redis: { db: 3 } } },
    );
    assert.deepEqual(merged, {
      cache: { redis: { host: 'r1', db: 3, tls: { enabled: true, ca: 'none' } } },
    });
  });

  test('a scalar replaces an object and an object replaces a scalar', () => {
    assert.deepEqual(deepMerge({ db: { host: 'h' } }, { db: 'postgres://h/app' }), {
      db: 'postgres://h/app',
    });
    assert.deepEqual(deepMerge({ db: 'postgres://h/app' }, { db: { host: 'h' } }), {
      db: { host: 'h' },
    });
  });

  test('keeps falsy values from later layers', () => {
    const merged = deepMerge({ retries: 3, verbose: true, name: 'svc' }, { retries: 0, verbose: false, name: '' });
    assert.deepEqual(merged, { retries: 0, verbose: false, name: '' });
  });

  test('copies functions as values', () => {
    const onStart = () => 'start';
    const onError = () => 'error';
    const merged = deepMerge({ hooks: { onStart } }, { hooks: { onError } });
    assert.equal(merged.hooks.onStart, onStart);
    assert.equal(merged.hooks.onError, onError);
  });
});
