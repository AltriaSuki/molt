# minirouter

A tiny HTTP router for Node, with no dependencies:

- `src/router.js`: `Router`, a route table mapping (method, path) to a handler.
- `src/dispatcher.js`: `createDispatcher(router)`, which turns a plain request
  object (`{ method, url, headers, body }`) into a plain response object
  (`{ status, headers, body }`). It handles 404, 405 (with an `allow` header),
  automatic `OPTIONS`, body-less `HEAD` responses and handler errors.
- `src/response.js`: helpers for building responses (`text`, `json`, `empty`).
- `src/url.js`: splits a request target into pathname and query.

There is no HTTP server in here: an adapter for `node:http` (or anything else)
only has to build the request object and write the response object out.

## Usage

```js
import { Router, createDispatcher, json } from './src/index.js';

const router = new Router()
  .get('/health', () => 'ok')
  .post('/notes', (ctx) => json({ created: ctx.body }, 201));

const dispatch = createDispatcher(router);
const res = await dispatch({ method: 'GET', url: '/health?verbose=1' });
// { status: 200, headers: { 'content-type': 'text/plain; charset=utf-8' }, body: 'ok' }
```

### Router

- `add(method, pattern, handler)` registers a route and returns the router.
  `get`, `post`, `put`, `patch` and `delete` are shortcuts. Methods are
  case-insensitive. Patterns must start with `/`.
- `match(method, path)` returns `{ handler, params }` or `null`. `path` is the
  pathname only (no query string). If two routes are equally good matches, the
  one added first wins.
- `allowedMethods(path)` returns the sorted list of methods that have a route
  for `path`.
- `routes()` lists `{ method, pattern }` in registration order.

### Handlers

Handlers receive `{ method, path, query, params, headers, body }` where `query`
is a `URLSearchParams` and header names are lower-case. They may return a
response object, a string (sent as `text/plain`), any other value (sent as
JSON), or nothing (`204`). They may be async.

## Tests

Requires Node 20 or newer.

```sh
npm test
# or
node --test test/router.test.js test/dispatcher.test.js
```
