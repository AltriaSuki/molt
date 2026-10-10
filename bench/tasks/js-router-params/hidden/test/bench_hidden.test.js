import { describe, test } from 'node:test';
import assert from 'node:assert/strict';

import { Router } from '../src/router.js';
import { createDispatcher } from '../src/dispatcher.js';

// Named handlers so failures say which route won.
function h(name) {
  const fn = () => name;
  Object.defineProperty(fn, 'name', { value: name });
  return fn;
}

// Own keys of params as an ordinary object, so prototype choice doesn't matter.
function own(params) {
  const out = {};
  for (const key of Object.keys(params)) {
    Object.defineProperty(out, key, {
      value: params[key],
      enumerable: true,
      writable: true,
      configurable: true,
    });
  }
  return out;
}

function winner(router, method, path) {
  const found = router.match(method, path);
  return found === null ? null : found.handler.name;
}

function paramsOf(router, method, path) {
  const found = router.match(method, path);
  assert.notEqual(found, null, `${method} ${path} should match`);
  return own(found.params);
}

describe('named parameters', () => {
  test(':name captures one segment', () => {
    const router = new Router().get('/users/:id', h('user'));
    const found = router.match('GET', '/users/42');
    assert.equal(found.handler.name, 'user');
    assert.deepEqual(own(found.params), { id: '42' });
  });

  test('several parameters in one pattern', () => {
    const router = new Router().get('/users/:uid/posts/:pid', h('post'));
    assert.deepEqual(paramsOf(router, 'GET', '/users/7/posts/abc'), { uid: '7', pid: 'abc' });
    assert.equal(router.match('GET', '/users/7/posts'), null);
    assert.equal(router.match('GET', '/users/7/comments/abc'), null);
  });

  test(':name matches exactly one segment, never zero or two', () => {
    const router = new Router().get('/users/:id', h('user'));
    assert.equal(router.match('GET', '/users'), null);
    assert.equal(router.match('GET', '/users/'), null);
    assert.equal(router.match('GET', '/users/42/extra'), null);
  });

  test(':name does not match an empty segment', () => {
    const router = new Router().get('/users/:id/posts', h('posts'));
    assert.equal(router.match('GET', '/users//posts'), null);
    assert.deepEqual(paramsOf(router, 'GET', '/users/9/posts'), { id: '9' });
  });

  test('parameters can come first and sit between static segments', () => {
    const router = new Router().get('/:lang/docs/:page', h('docs'));
    assert.deepEqual(paramsOf(router, 'GET', '/en/docs/intro'), { lang: 'en', page: 'intro' });
    assert.equal(router.match('GET', '/en/blog/intro'), null);
  });

  test('static routes still match with empty params', () => {
    const router = new Router().get('/about/team', h('team'));
    const found = router.match('GET', '/about/team');
    assert.equal(found.handler.name, 'team');
    assert.deepEqual(Object.keys(found.params), []);
  });
});

describe('optional parameters', () => {
  test('a final :name? matches with and without the segment', () => {
    const router = new Router().get('/users/:id?', h('users'));
    const without = router.match('GET', '/users');
    assert.equal(without.handler.name, 'users');
    assert.equal(Object.hasOwn(without.params, 'id'), false);
    assert.deepEqual(Object.keys(without.params), []);
    assert.deepEqual(paramsOf(router, 'GET', '/users/5'), { id: '5' });
  });

  test('an optional parameter still matches at most one segment', () => {
    const router = new Router().get('/users/:id?', h('users'));
    assert.equal(router.match('GET', '/users/5/more'), null);
    assert.equal(router.match('GET', '/'), null);
  });

  test('a trailing slash with an absent optional parameter', () => {
    const router = new Router().get('/users/:id?', h('users'));
    const found = router.match('GET', '/users/');
    assert.equal(found.handler.name, 'users');
    assert.deepEqual(Object.keys(found.params), []);
  });
});

describe('wildcard', () => {
  test('* captures the rest of the path without a leading slash', () => {
    const router = new Router().get('/files/*', h('files'));
    assert.deepEqual(paramsOf(router, 'GET', '/files/a/b/c.txt'), { '*': 'a/b/c.txt' });
    assert.deepEqual(paramsOf(router, 'GET', '/files/readme'), { '*': 'readme' });
  });

  test('* may match nothing', () => {
    const router = new Router().get('/files/*', h('files'));
    assert.deepEqual(paramsOf(router, 'GET', '/files'), { '*': '' });
    assert.deepEqual(paramsOf(router, 'GET', '/files/'), { '*': '' });
    assert.equal(router.match('GET', '/filesystem'), null);
  });

  test('a trailing slash is not part of the captured rest', () => {
    const router = new Router().get('/files/*', h('files'));
    assert.deepEqual(paramsOf(router, 'GET', '/files/a/b/'), { '*': 'a/b' });
  });

  test('/* matches every path, including /', () => {
    const router = new Router().get('/*', h('all'));
    assert.deepEqual(paramsOf(router, 'GET', '/'), { '*': '' });
    assert.deepEqual(paramsOf(router, 'GET', '/x/y'), { '*': 'x/y' });
  });

  test('* combines with named parameters', () => {
    const router = new Router().get('/repos/:owner/blob/*', h('blob'));
    assert.deepEqual(paramsOf(router, 'GET', '/repos/ada/blob/src/main.js'), {
      owner: 'ada',
      '*': 'src/main.js',
    });
  });
});

describe('percent-decoding', () => {
  test('parameter values are decoded', () => {
    const router = new Router().get('/users/:name', h('user'));
    assert.deepEqual(paramsOf(router, 'GET', '/users/caf%C3%A9'), { name: 'café' });
    assert.deepEqual(paramsOf(router, 'GET', '/users/a%20b'), { name: 'a b' });
  });

  test('an encoded slash stays inside one parameter', () => {
    const router = new Router().get('/users/:id', h('user'));
    assert.deepEqual(paramsOf(router, 'GET', '/users/a%2Fb'), { id: 'a/b' });
  });

  test('an encoded slash is not a separator for the route either', () => {
    const router = new Router().get('/a/:x/:y', h('two'));
    assert.equal(router.match('GET', '/a/b%2Fc'), null);
  });

  test('the wildcard value is decoded as a whole', () => {
    const router = new Router().get('/files/*', h('files'));
    assert.deepEqual(paramsOf(router, 'GET', '/files/my%20docs/x%2Fy.txt'), {
      '*': 'my docs/x/y.txt',
    });
  });

  test('a malformed escape means no match, without throwing', () => {
    const router = new Router().get('/users/:id', h('user')).get('/files/*', h('files'));
    assert.equal(router.match('GET', '/users/%zz'), null);
    assert.equal(router.match('GET', '/users/%E0%A4%A'), null);
    assert.equal(router.match('GET', '/files/ok/%E0%A4%A'), null);
  });

  test('a malformed escape lets another matching route win', () => {
    const router = new Router().get('/files/:name', h('byName')).get('/:dir/%zz', h('raw'));
    const found = router.match('GET', '/files/%zz');
    assert.equal(found.handler.name, 'raw');
    assert.deepEqual(own(found.params), { dir: 'files' });
  });
});

describe('trailing slash', () => {
  test('is ignored on the request path', () => {
    const router = new Router().get('/about', h('about')).get('/users/:id', h('user'));
    assert.equal(winner(router, 'GET', '/about/'), 'about');
    assert.deepEqual(paramsOf(router, 'GET', '/users/42/'), { id: '42' });
  });

  test('/ stays /', () => {
    const router = new Router().get('/', h('home')).get('/about', h('about'));
    assert.equal(winner(router, 'GET', '/'), 'home');
    assert.equal(winner(router, 'GET', '/about/'), 'about');
  });
});

describe('params object', () => {
  test('has only the captured values as own properties', () => {
    const router = new Router().get('/u/:id', h('u'));
    const { params } = router.match('GET', '/u/1');
    assert.deepEqual(Object.keys(params), ['id']);
    assert.equal(params.id, '1');
  });

  test('parameters named like built-ins are ordinary own properties', () => {
    const router = new Router()
      .get('/obj/:constructor/:toString', h('builtins'))
      .get('/proto/:__proto__', h('proto'));
    const a = router.match('GET', '/obj/x/y').params;
    assert.equal(Object.hasOwn(a, 'constructor'), true);
    assert.equal(a.constructor, 'x');
    assert.equal(Object.hasOwn(a, 'toString'), true);
    assert.equal(a.toString, 'y');

    const b = router.match('GET', '/proto/evil').params;
    assert.equal(Object.hasOwn(b, '__proto__'), true);
    assert.equal(b['__proto__'], 'evil');
    assert.deepEqual(Object.keys(b), ['__proto__']);
  });

  test('each match gets its own params', () => {
    const router = new Router().get('/u/:id', h('u'));
    const first = router.match('GET', '/u/1').params;
    const second = router.match('GET', '/u/2').params;
    assert.equal(first.id, '1');
    assert.equal(second.id, '2');
  });
});

describe('most specific route wins', () => {
  test('static beats a parameter, whatever the order', () => {
    const r1 = new Router().get('/users/:id', h('param')).get('/users/me', h('me'));
    const r2 = new Router().get('/users/me', h('me')).get('/users/:id', h('param'));
    for (const router of [r1, r2]) {
      assert.equal(winner(router, 'GET', '/users/me'), 'me');
      assert.equal(winner(router, 'GET', '/users/you'), 'param');
    }
  });

  test('a parameter beats a wildcard, whatever the order', () => {
    const r1 = new Router().get('/files/*', h('rest')).get('/files/:name', h('name'));
    const r2 = new Router().get('/files/:name', h('name')).get('/files/*', h('rest'));
    for (const router of [r1, r2]) {
      assert.equal(winner(router, 'GET', '/files/a'), 'name');
      assert.equal(winner(router, 'GET', '/files/a/b'), 'rest');
    }
  });

  test('static beats a wildcard', () => {
    const router = new Router().get('/files/*', h('rest')).get('/files/index', h('index'));
    assert.equal(winner(router, 'GET', '/files/index'), 'index');
  });

  test('the leftmost differing segment decides', () => {
    const router = new Router().get('/:y/b', h('paramFirst')).get('/a/:x', h('staticFirst'));
    assert.equal(winner(router, 'GET', '/a/b'), 'staticFirst');
    assert.deepEqual(paramsOf(router, 'GET', '/a/b'), { x: 'b' });
  });

  test('a later static segment does not outweigh an earlier one', () => {
    const router = new Router()
      .get('/:a/:b/c/d', h('lateStatics'))
      .get('/x/:b/:c/:d', h('earlyStatic'));
    assert.equal(winner(router, 'GET', '/x/y/c/d'), 'earlyStatic');
  });

  test('a deeper static prefix beats a wildcard higher up', () => {
    const router = new Router().get('/*', h('catchAll')).get('/api/*', h('api'));
    assert.equal(winner(router, 'GET', '/api/v1/users'), 'api');
    assert.deepEqual(paramsOf(router, 'GET', '/api/v1/users'), { '*': 'v1/users' });
    assert.equal(winner(router, 'GET', '/web/home'), 'catchAll');
  });

  test('a shorter pattern beats an empty wildcard or optional parameter', () => {
    const router = new Router()
      .get('/files/*', h('rest'))
      .get('/files/:name?', h('optional'))
      .get('/files', h('exact'));
    assert.equal(winner(router, 'GET', '/files'), 'exact');
    assert.equal(winner(router, 'GET', '/files/'), 'exact');
    assert.equal(winner(router, 'GET', '/files/a'), 'optional');
    assert.equal(winner(router, 'GET', '/files/a/b'), 'rest');
  });

  test('an empty optional parameter beats an empty wildcard', () => {
    const router = new Router().get('/docs/*', h('rest')).get('/docs/:page?', h('optional'));
    assert.equal(winner(router, 'GET', '/docs'), 'optional');
  });

  test('a required and an optional parameter rank the same', () => {
    const r1 = new Router().get('/p/:a', h('required')).get('/p/:b?', h('optional'));
    assert.equal(winner(r1, 'GET', '/p/1'), 'required');
    const r2 = new Router().get('/p/:b?', h('optional')).get('/p/:a', h('required'));
    assert.equal(winner(r2, 'GET', '/p/1'), 'optional');
    assert.deepEqual(paramsOf(r2, 'GET', '/p/1'), { b: '1' });
  });

  test('equally specific routes: the one added first wins', () => {
    const router = new Router().get('/x/:a', h('first')).get('/x/:b', h('second'));
    assert.equal(winner(router, 'GET', '/x/1'), 'first');
    assert.deepEqual(paramsOf(router, 'GET', '/x/1'), { a: '1' });
  });

  test('only routes for the requested method compete', () => {
    const router = new Router().post('/users/me', h('postMe')).get('/users/:id', h('getUser'));
    assert.equal(winner(router, 'GET', '/users/me'), 'getUser');
    assert.equal(winner(router, 'POST', '/users/me'), 'postMe');
    assert.equal(router.match('POST', '/users/you'), null);
  });
});

describe('HEAD', () => {
  test('falls back to the GET route', () => {
    const router = new Router().get('/users/:id', h('getUser'));
    const found = router.match('HEAD', '/users/3');
    assert.equal(found.handler.name, 'getUser');
    assert.deepEqual(own(found.params), { id: '3' });
    assert.equal(winner(router, 'head', '/users/3/'), 'getUser');
  });

  test('a matching HEAD route wins over any GET route', () => {
    const router = new Router().get('/files/:name', h('getFile')).add('HEAD', '/files/*', h('headAny'));
    const found = router.match('HEAD', '/files/a');
    assert.equal(found.handler.name, 'headAny');
    assert.deepEqual(own(found.params), { '*': 'a' });
    assert.equal(winner(router, 'GET', '/files/a'), 'getFile');
  });

  test('falls back to GET when the HEAD routes do not match that path', () => {
    const router = new Router().add('HEAD', '/files/:name', h('headFile')).get('/files/*', h('getAny'));
    assert.equal(winner(router, 'HEAD', '/files/a'), 'headFile');
    assert.equal(winner(router, 'HEAD', '/files/a/b'), 'getAny');
  });

  test('GET does not fall back to HEAD, and HEAD needs a GET or HEAD route', () => {
    const router = new Router().add('HEAD', '/ping', h('headPing')).post('/submit', h('submit'));
    assert.equal(router.match('GET', '/ping'), null);
    assert.equal(router.match('HEAD', '/submit'), null);
  });
});

describe('allowedMethods', () => {
  function api() {
    return new Router()
      .get('/users/:id', h('get'))
      .put('/users/:id', h('put'))
      .delete('/users/:id', h('del'))
      .post('/users', h('create'))
      .patch('/files/*', h('patchFile'));
  }

  test('lists methods whose routes match, sorted, with HEAD for GET', () => {
    const router = api();
    assert.deepEqual(router.allowedMethods('/users/5'), ['DELETE', 'GET', 'HEAD', 'PUT']);
    assert.deepEqual(router.allowedMethods('/users'), ['POST']);
    assert.deepEqual(router.allowedMethods('/files/a/b'), ['PATCH']);
    assert.deepEqual(router.allowedMethods('/files'), ['PATCH']);
  });

  test('applies the trailing-slash and decoding rules', () => {
    const router = api();
    assert.deepEqual(router.allowedMethods('/users/5/'), ['DELETE', 'GET', 'HEAD', 'PUT']);
    assert.deepEqual(router.allowedMethods('/users/%zz'), []);
    assert.deepEqual(router.allowedMethods('/nothing/here'), []);
  });

  test('lists each method once', () => {
    const router = new Router()
      .get('/a/:x', h('one'))
      .get('/a/*', h('two'))
      .add('HEAD', '/a/:y', h('head'))
      .post('/a/:z', h('post'))
      .post('/a/b', h('postB'));
    assert.deepEqual(router.allowedMethods('/a/b'), ['GET', 'HEAD', 'POST']);
  });

  test('an explicit HEAD route without GET', () => {
    const router = new Router().add('HEAD', '/ping', h('headPing'));
    assert.deepEqual(router.allowedMethods('/ping'), ['HEAD']);
  });
});

describe('invalid patterns', () => {
  test('two parameters with the same name', () => {
    const router = new Router();
    assert.throws(() => router.get('/a/:id/b/:id', h('x')), Error);
    assert.throws(() => router.get('/a/:id/:id?', h('x')), Error);
  });

  test('* that is not the last segment', () => {
    const router = new Router();
    assert.throws(() => router.get('/*/a', h('x')), Error);
    assert.throws(() => router.get('/files/*/raw', h('x')), Error);
  });

  test(':name? that is not the last segment', () => {
    const router = new Router();
    assert.throws(() => router.get('/a/:id?/b', h('x')), Error);
  });

  test('valid patterns are accepted', () => {
    const router = new Router();
    assert.doesNotThrow(() => router.get('/a/:id/b/:other', h('x')));
    assert.doesNotThrow(() => router.get('/a/:id/*', h('x')));
    assert.doesNotThrow(() => router.get('/a/:id/:more?', h('x')));
    assert.doesNotThrow(() => router.get('/*', h('x')));
  });
});

describe('dispatcher with parameters', () => {
  function app() {
    const router = new Router()
      .get('/users/:id', (ctx) => ({ id: ctx.params.id, q: ctx.query.get('q') }))
      .put('/users/:id', () => 'updated')
      .get('/files/*', (ctx) => `file:${ctx.params['*']}`);
    return createDispatcher(router);
  }

  test('passes params to the handler', async () => {
    const dispatch = app();
    const res = await dispatch({ method: 'GET', url: '/users/a%20b?q=1' });
    assert.equal(res.status, 200);
    assert.deepEqual(JSON.parse(res.body), { id: 'a b', q: '1' });
    const file = await dispatch({ method: 'GET', url: '/files/x/y.txt' });
    assert.equal(file.body, 'file:x/y.txt');
  });

  test('a trailing slash reaches the same handler', async () => {
    const res = await app()({ method: 'GET', url: '/users/7/' });
    assert.equal(res.status, 200);
    assert.equal(JSON.parse(res.body).id, '7');
  });

  test('HEAD runs the GET handler and sends no body', async () => {
    const res = await app()({ method: 'HEAD', url: '/users/7' });
    assert.equal(res.status, 200);
    assert.equal(res.headers['content-type'], 'application/json');
    assert.equal(res.body, '');
  });

  test('405 lists the allowed methods, HEAD included', async () => {
    const res = await app()({ method: 'DELETE', url: '/users/7' });
    assert.equal(res.status, 405);
    assert.equal(res.headers.allow, 'GET, HEAD, PUT');
  });

  test('a malformed escape is a 404', async () => {
    const res = await app()({ method: 'GET', url: '/users/%zz' });
    assert.equal(res.status, 404);
  });
});
