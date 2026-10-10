import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Router } from '../src/router.js';
import { createDispatcher } from '../src/dispatcher.js';
import { json, text } from '../src/response.js';

function app(setup, options) {
  const router = new Router();
  setup(router);
  return createDispatcher(router, options);
}

test('passes method, path, query and headers to the handler', async () => {
  let seen;
  const dispatch = app((r) =>
    r.get('/search', (ctx) => {
      seen = ctx;
      return 'done';
    }),
  );
  const res = await dispatch({
    method: 'get',
    url: '/search?q=cats&page=2#top',
    headers: { 'X-Trace': 'abc' },
  });
  assert.equal(res.status, 200);
  assert.equal(seen.method, 'GET');
  assert.equal(seen.path, '/search');
  assert.equal(seen.query.get('q'), 'cats');
  assert.equal(seen.query.get('page'), '2');
  assert.equal(seen.headers['x-trace'], 'abc');
  assert.deepEqual(Object.keys(seen.params), []);
});

test('wraps handler return values', async () => {
  const dispatch = app((r) => {
    r.get('/text', () => 'hello');
    r.get('/json', () => ({ ok: true }));
    r.get('/nothing', () => undefined);
    r.post('/explicit', () => json({ id: 1 }, 201, { Location: '/things/1' }));
  });

  const t = await dispatch({ method: 'GET', url: '/text' });
  assert.equal(t.status, 200);
  assert.equal(t.body, 'hello');
  assert.match(t.headers['content-type'], /^text\/plain/);

  const j = await dispatch({ method: 'GET', url: '/json' });
  assert.equal(j.body, '{"ok":true}');
  assert.equal(j.headers['content-type'], 'application/json');

  const n = await dispatch({ method: 'GET', url: '/nothing' });
  assert.equal(n.status, 204);
  assert.equal(n.body, '');

  const e = await dispatch({ method: 'POST', url: '/explicit' });
  assert.equal(e.status, 201);
  assert.equal(e.headers.location, '/things/1');
});

test('async handlers are awaited', async () => {
  const dispatch = app((r) => r.get('/slow', async () => text('later', 202)));
  const res = await dispatch({ method: 'GET', url: '/slow' });
  assert.equal(res.status, 202);
  assert.equal(res.body, 'later');
});

test('unknown paths get a 404', async () => {
  const dispatch = app((r) => r.get('/here', () => 'x'));
  const res = await dispatch({ method: 'GET', url: '/there' });
  assert.equal(res.status, 404);
});

test('a known path with another method gets a 405 with an allow header', async () => {
  const dispatch = app((r) => {
    r.post('/notes', () => 'created');
    r.delete('/notes', () => 'gone');
  });
  const res = await dispatch({ method: 'PUT', url: '/notes' });
  assert.equal(res.status, 405);
  assert.equal(res.headers.allow, 'DELETE, POST');
});

test('OPTIONS without a route answers with the allowed methods', async () => {
  const dispatch = app((r) => r.post('/notes', () => 'created'));
  const res = await dispatch({ method: 'OPTIONS', url: '/notes' });
  assert.equal(res.status, 204);
  assert.equal(res.headers.allow, 'POST');
});

test('HEAD responses keep their headers but drop the body', async () => {
  const dispatch = app((r) =>
    r.add('HEAD', '/file', () => text('should not be sent', 200, { 'X-Size': '42' })),
  );
  const res = await dispatch({ method: 'HEAD', url: '/file' });
  assert.equal(res.status, 200);
  assert.equal(res.headers['x-size'], '42');
  assert.equal(res.body, '');
});

test('handler errors become a 500, or whatever onError returns', async () => {
  const boom = (r) =>
    r.get('/boom', () => {
      throw new Error('kaput');
    });

  const plain = app(boom);
  assert.equal((await plain({ method: 'GET', url: '/boom' })).status, 500);

  const custom = app(boom, { onError: (err, ctx) => text(`${ctx.path}: ${err.message}`, 503) });
  const res = await custom({ method: 'GET', url: '/boom' });
  assert.equal(res.status, 503);
  assert.equal(res.body, '/boom: kaput');
});

test('malformed requests get a 400', async () => {
  const dispatch = app((r) => r.get('/x', () => 'x'));
  assert.equal((await dispatch({ method: 'GET', url: 'x' })).status, 400);
  assert.equal((await dispatch({ method: 'NOT A METHOD', url: '/x' })).status, 400);
});
