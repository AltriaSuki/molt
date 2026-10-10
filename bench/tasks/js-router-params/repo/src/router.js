// Route table: maps (method, path) to a handler.
//
// Paths are matched exactly against the pattern a route was registered with.
// Methods are case-insensitive and stored upper-cased. When two routes are
// registered for the same method and pattern, the one added first wins.

const METHOD_RE = /^[A-Za-z][A-Za-z-]*$/;

/** Validates an HTTP method name and returns it upper-cased. */
export function normalizeMethod(method) {
  if (typeof method !== 'string' || !METHOD_RE.test(method)) {
    throw new TypeError(`invalid HTTP method: ${String(method)}`);
  }
  return method.toUpperCase();
}

function checkPattern(pattern) {
  if (typeof pattern !== 'string' || !pattern.startsWith('/')) {
    throw new TypeError(
      `route pattern must be a string starting with "/", got ${JSON.stringify(pattern)}`,
    );
  }
}

export class Router {
  #routes = [];

  /**
   * Registers `handler` for `method` requests to `pattern`.
   * Returns the router so calls can be chained.
   */
  add(method, pattern, handler) {
    const verb = normalizeMethod(method);
    checkPattern(pattern);
    if (typeof handler !== 'function') {
      throw new TypeError(`handler for ${verb} ${pattern} must be a function`);
    }
    this.#routes.push({ method: verb, pattern, handler });
    return this;
  }

  get(pattern, handler) {
    return this.add('GET', pattern, handler);
  }

  post(pattern, handler) {
    return this.add('POST', pattern, handler);
  }

  put(pattern, handler) {
    return this.add('PUT', pattern, handler);
  }

  patch(pattern, handler) {
    return this.add('PATCH', pattern, handler);
  }

  delete(pattern, handler) {
    return this.add('DELETE', pattern, handler);
  }

  /**
   * Finds the route for a request. `path` is the URL path without the query
   * string. Returns `{ handler, params }`, or null when nothing matches.
   */
  match(method, path) {
    const verb = normalizeMethod(method);
    for (const route of this.#routes) {
      if (route.method === verb && route.pattern === path) {
        return { handler: route.handler, params: {} };
      }
    }
    return null;
  }

  /**
   * The methods that have a route for `path`, sorted alphabetically.
   * The dispatcher uses this for 405 responses and OPTIONS.
   */
  allowedMethods(path) {
    const methods = new Set();
    for (const route of this.#routes) {
      if (route.pattern === path) methods.add(route.method);
    }
    return [...methods].sort();
  }

  /** The registered routes, in the order they were added. */
  routes() {
    return this.#routes.map(({ method, pattern }) => ({ method, pattern }));
  }
}
