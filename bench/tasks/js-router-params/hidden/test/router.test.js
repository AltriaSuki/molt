import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Router } from '../src/router.js';

const ok = () => 'ok';

test('matches a static route and returns its handler', () => {
  const router = new Router();
  const health = () => 'healthy';
  router.get('/health', health);
  const found = router.match('GET', '/health');
  assert.equal(found.handler, health);
  assert.deepEqual(Object.keys(found.params), []);
});

test('the root path is a route like any other', () => {
  const router = new Router();
  const home = () => 'home';
  router.get('/', home);
  assert.equal(router.match('GET', '/').handler, home);
  assert.equal(router.match('GET', '/index'), null);
});

test('returns null for an unknown path or a different method', () => {
  const router = new Router().get('/health', ok);
  assert.equal(router.match('GET', '/healthz'), null);
  assert.equal(router.match('POST', '/health'), null);
});

test('methods are case-insensitive', () => {
  const router = new Router();
  const create = () => 'created';
  router.add('post', '/notes', create);
  assert.equal(router.match('POST', '/notes').handler, create);
  assert.equal(router.match('Post', '/notes').handler, create);
});

test('static segments are case-sensitive', () => {
  const router = new Router().get('/About', ok);
  assert.equal(router.match('GET', '/about'), null);
  assert.ok(router.match('GET', '/About'));
});

test('shortcut methods register the right verb and chain', () => {
  const router = new Router()
    .get('/a', ok)
    .post('/a', ok)
    .put('/a', ok)
    .patch('/a', ok)
    .delete('/a', ok);
  assert.deepEqual(
    router.routes().map((r) => r.method),
    ['GET', 'POST', 'PUT', 'PATCH', 'DELETE'],
  );
});

test('routes() lists routes in registration order', () => {
  const router = new Router().post('/notes', ok).get('/notes/archive', ok);
  assert.deepEqual(router.routes(), [
    { method: 'POST', pattern: '/notes' },
    { method: 'GET', pattern: '/notes/archive' },
  ]);
});

test('the first of two identical routes wins', () => {
  const router = new Router();
  const first = () => 'first';
  const second = () => 'second';
  router.get('/dup', first).get('/dup', second);
  assert.equal(router.match('GET', '/dup').handler, first);
});

test('allowedMethods lists the methods registered for a path, sorted', () => {
  const router = new Router()
    .put('/notes', ok)
    .delete('/notes', ok)
    .post('/notes', ok)
    .post('/other', ok);
  assert.deepEqual(router.allowedMethods('/notes'), ['DELETE', 'POST', 'PUT']);
  assert.deepEqual(router.allowedMethods('/nothing'), []);
});

test('add rejects invalid input', () => {
  const router = new Router();
  assert.throws(() => router.add('GET', 'no-slash', ok), TypeError);
  assert.throws(() => router.add('GET', '/x', 'not a function'), TypeError);
  assert.throws(() => router.add('G E T', '/x', ok), TypeError);
  assert.throws(() => router.add(undefined, '/x', ok), TypeError);
});
