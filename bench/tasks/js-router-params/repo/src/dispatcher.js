// Turns plain request objects into plain response objects using a Router.
// No sockets involved: an HTTP adapter (or a test) builds the request object
// and writes the response object out.
//
// request:  { method, url, headers?, body? }   url is "/path?query"
// response: { status, headers, body }

import { normalizeMethod } from './router.js';
import { parseTarget } from './url.js';
import { empty, isResponse, json, lowerCaseKeys, text } from './response.js';

function defaultOnError() {
  return text('Internal Server Error', 500);
}

/** Normalizes whatever a handler returned into a response object. */
function toResponse(result) {
  if (isResponse(result)) return result;
  if (result === undefined || result === null) return empty(204);
  if (typeof result === 'string') return text(result);
  return json(result);
}

function noRoute(router, method, pathname) {
  const allowed = router.allowedMethods(pathname);
  if (allowed.length === 0) return text('Not Found', 404);
  const allow = allowed.join(', ');
  if (method === 'OPTIONS') return empty(204, { allow });
  return text('Method Not Allowed', 405, { allow });
}

/**
 * Creates `dispatch(request) => Promise<response>`.
 *
 * Handlers are called with a context object
 * `{ method, path, query, params, headers, body }` and may return a response
 * object, a string (text/plain), any other value (JSON), or nothing (204).
 * If a handler throws, `onError(err, ctx)` builds the response (500 by default).
 */
export function createDispatcher(router, { onError = defaultOnError } = {}) {
  return async function dispatch(request) {
    let method;
    let target;
    try {
      method = normalizeMethod(request?.method ?? 'GET');
      target = parseTarget(request?.url);
    } catch {
      return text('Bad Request', 400);
    }

    const found = router.match(method, target.pathname);
    let res;
    if (found) {
      const ctx = {
        method,
        path: target.pathname,
        query: target.query,
        params: found.params,
        headers: lowerCaseKeys(request.headers),
        body: request.body,
      };
      try {
        res = toResponse(await found.handler(ctx));
      } catch (err) {
        res = toResponse(onError(err, ctx));
      }
    } else {
      res = noRoute(router, method, target.pathname);
    }

    // HEAD responses carry the headers of the response but never a body.
    return method === 'HEAD' ? { ...res, body: '' } : res;
  };
}
