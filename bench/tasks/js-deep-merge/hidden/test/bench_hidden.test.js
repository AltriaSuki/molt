import { afterEach, describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { deepMerge } from '../src/merge.js';
import { parseEnv } from '../src/env.js';
import { loadConfig } from '../src/loader.js';

const POLLUTION_KEYS = ['polluted', 'isAdmin', 'benchInherited'];

function assertNotPolluted() {
  for (const key of POLLUTION_KEYS) {
    assert.equal(Object.hasOwn(Object.prototype, key), false, `Object.prototype.${key} was set`);
    assert.equal({}[key], undefined, `{}.${key} is visible`);
  }
}

afterEach(() => {
  // Undo any pollution so one failure does not leak into the next test.
  for (const key of POLLUTION_KEYS) delete Object.prototype[key];
});

function deepFreeze(value) {
  if (value !== null && typeof value === 'object' && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const key of Object.keys(value)) deepFreeze(value[key]);
  }
  return value;
}

function filesystem(files) {
  return (path) => {
    if (!Object.hasOwn(files, path)) throw new Error(`ENOENT: ${path}`);
    return files[path];
  };
}

class Logger {
  constructor(name) {
    this.name = name;
    this.lines = [];
  }

  log(line) {
    this.lines.push(line);
  }
}

describe('deepMerge does not modify its arguments', () => {
  test('returns a new object, not the target', () => {
    const target = { a: 1, nested: { b: 2 } };
    const result = deepMerge(target, { c: 3 });
    assert.notEqual(result, target);
    assert.deepEqual(target, { a: 1, nested: { b: 2 } });
    assert.deepEqual(result, { a: 1, nested: { b: 2 }, c: 3 });
  });

  test('a single argument still gives a new object', () => {
    const target = { a: 1, nested: { b: [1, 2] } };
    const result = deepMerge(target);
    assert.notEqual(result, target);
    assert.notEqual(result.nested, target.nested);
    assert.notEqual(result.nested.b, target.nested.b);
    assert.deepEqual(result, target);
  });

  test('no argument changes at any depth', () => {
    const when = new Date(Date.UTC(2024, 0, 2));
    const args = [
      { db: { host: 'localhost', pool: { min: 1, max: 5 }, replicas: ['r1', 'r2'] }, log: { level: 'info' } },
      { db: { pool: { max: 20 }, replicas: ['r3'] }, extra: { when }, log: null },
      { db: { pool: { idle: 30 } }, extra: { tags: ['a'] }, log: { level: 'debug' } },
      { db: { host: undefined, pool: { min: undefined } }, more: { deep: { x: 1 } } },
      { more: { deep: { y: 2 } }, extra: { when: 5 } },
    ];
    const snapshot = structuredClone(args);
    const result = deepMerge(...args);
    assert.deepEqual(args, snapshot);
    assert.deepEqual(result, {
      db: { host: 'localhost', pool: { min: 1, max: 20, idle: 30 }, replicas: ['r3'] },
      log: { level: 'debug' },
      extra: { when: 5, tags: ['a'] },
      more: { deep: { x: 1, y: 2 } },
    });
  });

  test('a nested object first seen in a source is not merged into later', () => {
    const first = { db: { host: 'a' } };
    const second = { db: { port: 1 } };
    const third = { db: { user: 'u' } };
    const result = deepMerge({}, first, second, third);
    assert.deepEqual(first, { db: { host: 'a' } });
    assert.deepEqual(second, { db: { port: 1 } });
    assert.deepEqual(third, { db: { user: 'u' } });
    assert.deepEqual(result, { db: { host: 'a', port: 1, user: 'u' } });
  });

  test('works with deeply frozen arguments', () => {
    const a = deepFreeze({ db: { host: 'h', pool: { max: 5 } }, list: [1, { x: 1 }] });
    const b = deepFreeze({ db: { pool: { min: 1 } }, list: [2] });
    const c = deepFreeze({ db: { pool: { max: 9 } }, flag: true });
    const result = deepMerge(a, b, c);
    assert.deepEqual(result, { db: { host: 'h', pool: { max: 9, min: 1 } }, list: [2], flag: true });
    assert.equal(Object.isFrozen(result), false);
  });

  test('the result shares no plain object or array with the arguments', () => {
    const a = { db: { host: 'h', opts: { ssl: false } }, only: { inA: true }, list: [1, 2] };
    const b = { db: { port: 1 }, fresh: { inB: { deep: true } }, tags: ['x'] };
    const result = deepMerge(a, b);
    for (const [path, value] of [
      ['db', result.db],
      ['db.opts', result.db.opts],
      ['only', result.only],
      ['fresh', result.fresh],
      ['fresh.inB', result.fresh.inB],
      ['list', result.list],
      ['tags', result.tags],
    ]) {
      for (const inner of [a.db, a.db.opts, a.only, a.list, b.db, b.fresh, b.fresh.inB, b.tags]) {
        assert.notEqual(value, inner, `result.${path} is shared with an argument`);
      }
    }
    result.db.opts.ssl = true;
    result.only.inA = false;
    result.fresh.inB.deep = false;
    result.list.push(3);
    result.tags.push('y');
    assert.deepEqual(a, { db: { host: 'h', opts: { ssl: false } }, only: { inA: true }, list: [1, 2] });
    assert.deepEqual(b, { db: { port: 1 }, fresh: { inB: { deep: true } }, tags: ['x'] });
  });
});

describe('arrays', () => {
  test('a later array replaces an earlier one instead of merging by index', () => {
    assert.deepEqual(deepMerge({ hosts: ['a', 'b', 'c'] }, { hosts: ['x'] }), { hosts: ['x'] });
    assert.deepEqual(deepMerge({ hosts: ['a', 'b'] }, { hosts: [] }), { hosts: [] });
    assert.deepEqual(deepMerge({ deep: { list: [1, 2, 3] } }, { deep: { list: [9] } }), { deep: { list: [9] } });
  });

  test('the copy is shallow: a new array holding the same elements', () => {
    const item = { name: 'beta' };
    const source = { flags: [item, 'x'] };
    const result = deepMerge({ flags: [{ name: 'alpha' }] }, source);
    assert.ok(Array.isArray(result.flags));
    assert.notEqual(result.flags, source.flags);
    assert.equal(result.flags.length, 2);
    assert.equal(result.flags[0], item);
    assert.equal(result.flags[1], 'x');
  });

  test('arrays and plain objects replace each other', () => {
    const toArray = deepMerge({ a: { x: 1 } }, { a: [1, 2] });
    assert.deepEqual(toArray, { a: [1, 2] });
    assert.ok(Array.isArray(toArray.a));
    const toObject = deepMerge({ a: [1, 2] }, { a: { x: 1 } });
    assert.deepEqual(toObject, { a: { x: 1 } });
    assert.equal(Array.isArray(toObject.a), false);
    assert.deepEqual(deepMerge({ a: [1] }, { a: 'none' }), { a: 'none' });
  });
});

describe('objects that are not plain', () => {
  test('a Date is taken by reference', () => {
    const d1 = new Date(Date.UTC(2020, 0, 1));
    const d2 = new Date(Date.UTC(2021, 5, 6));
    const fresh = deepMerge({}, { startedAt: d2 });
    assert.equal(fresh.startedAt, d2);
    const replaced = deepMerge({ startedAt: d1 }, { startedAt: d2 });
    assert.equal(replaced.startedAt, d2);
    assert.equal(d1.getTime(), Date.UTC(2020, 0, 1));
    const kept = deepMerge({ startedAt: d1 }, { other: 1 });
    assert.equal(kept.startedAt, d1);
  });

  test('Map, Set and RegExp are taken by reference', () => {
    const map = new Map([['a', 1]]);
    const set = new Set([1]);
    const re = /^svc-\d+$/i;
    const result = deepMerge({ map: new Map([['b', 2]]), match: /x/ }, { map, set, match: re });
    assert.equal(result.map, map);
    assert.equal(result.set, set);
    assert.equal(result.match, re);
  });

  test('a class instance replaces an earlier one and is not merged into it', () => {
    const base = new Logger('default');
    const custom = new Logger('custom');
    custom.lines.push('hello');
    const result = deepMerge({ logger: base }, { logger: custom });
    assert.equal(result.logger, custom);
    assert.equal(base.name, 'default');
    assert.deepEqual(base.lines, []);
  });

  test('a class instance replaces a plain object and a plain object replaces an instance', () => {
    const logger = new Logger('custom');
    const result = deepMerge({ logger: { name: 'plain', level: 'info' } }, { logger });
    assert.equal(result.logger, logger);
    assert.deepEqual(Object.keys(logger), ['name', 'lines']);

    const back = deepMerge({ logger }, { logger: { level: 'warn' } });
    assert.deepEqual(back.logger, { level: 'warn' });
    assert.equal(Object.getPrototypeOf(back.logger), Object.prototype);
    assert.equal(logger.name, 'custom');
    assert.equal(Object.hasOwn(logger, 'level'), false);
  });

  test('an object with any other prototype is taken by reference', () => {
    const proto = { kind: 'base' };
    const derived = Object.create(proto);
    derived.own = 1;
    const result = deepMerge({ value: { own: 0, extra: true } }, { value: derived });
    assert.equal(result.value, derived);
  });

  test('null-prototype objects are plain: merged, and copied into ordinary objects', () => {
    const pool = Object.create(null);
    pool.max = 10;
    const source = Object.create(null);
    source.db = pool;
    const merged = deepMerge({ db: { host: 'h', max: 1 } }, source);
    assert.deepEqual(merged, { db: { host: 'h', max: 10 } });
    const copied = deepMerge({}, source);
    assert.deepEqual(copied, { db: { max: 10 } });
    assert.notEqual(copied.db, pool);
    assert.equal(Object.getPrototypeOf(copied.db), Object.prototype);
    const top = deepMerge(source);
    assert.equal(Object.getPrototypeOf(top), Object.prototype);
  });
});

describe('null and undefined values', () => {
  test('undefined in a source is skipped', () => {
    assert.deepEqual(deepMerge({ port: 8080 }, { port: undefined }), { port: 8080 });
    assert.deepEqual(deepMerge({ db: { port: 1 } }, { db: undefined }), { db: { port: 1 } });
    const created = deepMerge({ a: 1 }, { b: undefined });
    assert.equal(Object.hasOwn(created, 'b'), false);
    assert.deepEqual(created, { a: 1 });
  });

  test('undefined is skipped at every depth, including in copied objects', () => {
    const result = deepMerge({}, { log: { level: undefined }, db: { host: 'h', port: undefined } });
    assert.deepEqual(result, { log: {}, db: { host: 'h' } });
    assert.equal(Object.hasOwn(result.db, 'port'), false);
    assert.deepEqual(deepMerge({ db: { port: 1 } }, { db: { port: undefined, host: 'x' } }), {
      db: { port: 1, host: 'x' },
    });
  });

  test('null overrides, at the top level and nested', () => {
    assert.deepEqual(deepMerge({ db: { host: 'h' } }, { db: null }), { db: null });
    assert.deepEqual(deepMerge({ db: { password: 'x', host: 'h' } }, { db: { password: null } }), {
      db: { password: null, host: 'h' },
    });
    assert.deepEqual(deepMerge({ list: [1] }, { list: null }), { list: null });
    assert.deepEqual(deepMerge({}, { a: null }), { a: null });
  });

  test('a later value replaces null', () => {
    assert.deepEqual(deepMerge({ db: null }, { db: { host: 'h' } }), { db: { host: 'h' } });
    assert.deepEqual(deepMerge({ db: null }, { db: 5 }), { db: 5 });
    assert.deepEqual(deepMerge({ db: null }, { db: ['a'] }), { db: ['a'] });
  });
});

describe('arguments that are not plain objects', () => {
  test('deepMerge() returns a fresh empty ordinary object', () => {
    const a = deepMerge();
    const b = deepMerge();
    assert.deepEqual(a, {});
    assert.equal(Object.getPrototypeOf(a), Object.prototype);
    assert.notEqual(a, b);
  });

  test('null, undefined and primitives are ignored, the target included', () => {
    assert.deepEqual(deepMerge(null, { a: 1 }), { a: 1 });
    assert.deepEqual(deepMerge(undefined, { a: 1 }), { a: 1 });
    assert.deepEqual(deepMerge({ a: 1 }, undefined, null, 42, 'str', true, { b: 2 }), { a: 1, b: 2 });
    assert.deepEqual(deepMerge(undefined), {});
    assert.deepEqual(deepMerge(null, null), {});
  });

  test('arrays, Dates and class instances as arguments are ignored', () => {
    assert.deepEqual(deepMerge({ a: 1 }, ['x', 'y'], new Date(0), new Logger('l')), { a: 1 });
    const fromArray = deepMerge([1, 2], { b: 2 });
    assert.deepEqual(fromArray, { b: 2 });
    assert.equal(Array.isArray(fromArray), false);
  });
});

describe('which keys are copied', () => {
  test('symbol keys and non-enumerable properties are not copied', () => {
    const sym = Symbol('secret');
    const source = { visible: 1, [sym]: 2 };
    Object.defineProperty(source, 'hidden', { value: 3, enumerable: false });
    const result = deepMerge({}, { nested: source });
    assert.deepEqual(Object.keys(result.nested), ['visible']);
    assert.equal(Object.getOwnPropertySymbols(result.nested).length, 0);
    assert.equal(Object.hasOwn(result.nested, 'hidden'), false);
    const top = deepMerge(source);
    assert.deepEqual(Reflect.ownKeys(top), ['visible']);
  });

  test('inherited enumerable keys are not copied', () => {
    Object.prototype.benchInherited = 'from the prototype';
    let result;
    try {
      result = deepMerge({ a: 1 }, { b: { c: 2 } });
    } finally {
      delete Object.prototype.benchInherited;
    }
    assert.deepEqual(Reflect.ownKeys(result), ['a', 'b']);
    assert.deepEqual(Reflect.ownKeys(result.b), ['c']);
  });
});

describe('prototype pollution', () => {
  test('"__proto__" from JSON.parse is ignored at the top level', () => {
    const payload = JSON.parse('{"__proto__": {"polluted": "yes", "isAdmin": true}, "name": "svc"}');
    const result = deepMerge({}, payload);
    assertNotPolluted();
    assert.equal(Object.getPrototypeOf(result), Object.prototype);
    assert.equal(Object.hasOwn(result, '__proto__'), false);
    assert.equal(result.isAdmin, undefined);
    assert.deepEqual(Object.keys(result), ['name']);
  });

  test('"__proto__" is ignored when it is the first argument', () => {
    const payload = JSON.parse('{"__proto__": {"isAdmin": true}, "a": 1}');
    const result = deepMerge(payload, { b: 2 });
    assertNotPolluted();
    assert.equal(Object.getPrototypeOf(result), Object.prototype);
    assert.deepEqual(result, { a: 1, b: 2 });
  });

  test('"__proto__" is ignored at any depth', () => {
    const payload = JSON.parse('{"db": {"__proto__": {"polluted": "yes"}, "port": 1}, "x": {"y": {"__proto__": {"isAdmin": true}}}}');
    const merged = deepMerge({ db: { host: 'h' } }, payload);
    assertNotPolluted();
    assert.deepEqual(merged, { db: { host: 'h', port: 1 }, x: { y: {} } });
    assert.equal(Object.getPrototypeOf(merged.db), Object.prototype);
    assert.equal(Object.getPrototypeOf(merged.x.y), Object.prototype);
    assert.equal(merged.x.y.isAdmin, undefined);
  });

  test('"constructor" and "prototype" payloads are ignored', () => {
    const payload = JSON.parse('{"constructor": {"prototype": {"polluted": "yes"}}, "a": {"constructor": {"prototype": {"isAdmin": true}}, "b": 1}}');
    const result = deepMerge({}, payload);
    assertNotPolluted();
    assert.equal(Object.hasOwn(result, 'constructor'), false);
    assert.equal(result.constructor, Object);
    assert.deepEqual(result, { a: { b: 1 } });
    assert.equal(result.a.constructor, Object);
  });

  test('"prototype" keys are dropped at every depth', () => {
    const result = deepMerge({ a: { keep: 1 } }, { prototype: 1, a: { prototype: { x: 1 }, b: 2 } });
    assert.deepEqual(result, { a: { keep: 1, b: 2 } });
    assert.equal(Object.hasOwn(result, 'prototype'), false);
  });

  test('an own "constructor" in the target layer is not copied either', () => {
    const result = deepMerge({ constructor: 'x', name: 'svc' }, { constructor: { prototype: {} } });
    assert.deepEqual(result, { name: 'svc' });
    assert.equal(result.constructor, Object);
  });

  test('keys that only resemble the ignored ones are merged as usual', () => {
    const result = deepMerge(
      { db: { protocol: 'tcp' } },
      { db: { constructorName: 'Pool' }, proto: 1, prototypes: ['a'] },
    );
    assert.deepEqual(result, { db: { protocol: 'tcp', constructorName: 'Pool' }, proto: 1, prototypes: ['a'] });
  });
});

describe('parseEnv', () => {
  test('skips variables whose path goes through an ignored key', () => {
    const env = {
      APP__constructor__prototype__polluted: 'yes',
      APP__db__constructor: '5',
      APP__a__prototype__b: '1',
      APP__prototype: '2',
      APP__constructor: '{"x": 1}',
      APP__ok: '1',
    };
    const result = parseEnv(env);
    assertNotPolluted();
    assert.deepEqual(result, { ok: 1 });
    assert.equal(Object.hasOwn(result, 'constructor'), false);
  });

  test('a skipped variable contributes nothing, not even an empty parent', () => {
    assert.deepEqual(parseEnv({ APP__db__constructor: '5' }), {});
    assert.deepEqual(parseEnv({ APP__db__constructor: '5', APP__db__port: '1' }), { db: { port: 1 } });
  });

  test('segments that only resemble the ignored keys are kept', () => {
    assert.deepEqual(parseEnv({ APP__db__protocol: 'tcp', APP__prototypes: '["a"]' }), {
      db: { protocol: 'tcp' },
      prototypes: ['a'],
    });
  });

  test('ignored keys inside JSON values are dropped', () => {
    const env = {
      APP__db: '{"__proto__": {"polluted": "yes"}, "port": 1}',
      APP__features: '{"constructor": {"prototype": {"isAdmin": true}}, "beta": true}',
    };
    const result = parseEnv(env);
    assertNotPolluted();
    assert.deepEqual(result, { db: { port: 1 }, features: { beta: true } });
    assert.equal(Object.hasOwn(result.db, '__proto__'), false);
    assert.equal(Object.getPrototypeOf(result.db), Object.prototype);
  });
});

describe('loadConfig', () => {
  const files = filesystem({
    '/a.json': '{"db": {"host": "a-db"}, "features": {"beta": true}, "hosts": ["x"]}',
    '/b.json': '{"db": {"port": 6543}}',
    '/null.json': '{"db": {"password": null}, "cache": null}',
    '/evil.json': '{"__proto__": {"isAdmin": true}, "db": {"__proto__": {"polluted": "yes"}}, "constructor": {"prototype": {"polluted": "yes"}}}',
  });

  test('loading twice with the same defaults does not leak between loads', () => {
    const defaults = { db: { host: 'localhost', port: 5432 }, features: { beta: false }, hosts: ['h1', 'h2', 'h3'] };
    const snapshot = structuredClone(defaults);
    const first = loadConfig({ defaults, file: '/a.json', env: {}, readFile: files });
    assert.deepEqual(first, { db: { host: 'a-db', port: 5432 }, features: { beta: true }, hosts: ['x'] });
    const second = loadConfig({ defaults, file: '/b.json', env: { APP__db__user: 'svc' }, readFile: files });
    assert.deepEqual(defaults, snapshot);
    assert.deepEqual(second, {
      db: { host: 'localhost', port: 6543, user: 'svc' },
      features: { beta: false },
      hosts: ['h1', 'h2', 'h3'],
    });
    assert.notEqual(second.hosts, defaults.hosts);
    assert.notEqual(second.db, defaults.db);
  });

  test('null in the file overrides instead of crashing', () => {
    const config = loadConfig({
      defaults: { db: { host: 'h', password: 'secret' }, cache: { ttl: 60 } },
      file: '/null.json',
      env: {},
      readFile: files,
    });
    assert.deepEqual(config, { db: { host: 'h', password: null }, cache: null });
  });

  test('env arrays replace default arrays and env null overrides', () => {
    const config = loadConfig({
      defaults: { hosts: ['a', 'b', 'c'], db: { password: 'x', host: 'h' } },
      env: { APP__hosts: '["z"]', APP__db__password: 'null' },
    });
    assert.deepEqual(config, { hosts: ['z'], db: { password: null, host: 'h' } });
  });

  test('overrides keep Dates and class instances and skip undefined', () => {
    const startedAt = new Date(Date.UTC(2024, 4, 1));
    const defaultLogger = new Logger('default');
    const logger = new Logger('custom');
    const config = loadConfig({
      defaults: { logger: defaultLogger, port: 8080, log: { level: 'info' } },
      env: {},
      overrides: { startedAt, logger, port: undefined, log: { level: undefined } },
    });
    assert.equal(config.startedAt, startedAt);
    assert.equal(config.logger, logger);
    assert.equal(defaultLogger.name, 'default');
    assert.equal(config.port, 8080);
    assert.deepEqual(config.log, { level: 'info' });
  });

  test('a malicious config file cannot pollute Object.prototype', () => {
    const config = loadConfig({ defaults: { db: { host: 'h' } }, file: '/evil.json', env: {}, readFile: files });
    assertNotPolluted();
    assert.equal(Object.getPrototypeOf(config), Object.prototype);
    assert.equal(Object.hasOwn(config, 'constructor'), false);
    assert.equal(config.constructor, Object);
    assert.deepEqual(config, { db: { host: 'h' } });
  });

  test('malicious env overrides cannot pollute Object.prototype', () => {
    const config = loadConfig({
      defaults: { features: { beta: false } },
      env: {
        APP__features: '{"__proto__": {"isAdmin": true}}',
        APP__constructor__prototype__isAdmin: 'true',
        APP__x: '{"constructor": {"prototype": {"polluted": "yes"}}}',
      },
    });
    assertNotPolluted();
    assert.equal(Object.hasOwn(config, 'constructor'), false);
    assert.equal(config.constructor, Object);
    assert.deepEqual(config, { features: { beta: false }, x: {} });
  });
});
