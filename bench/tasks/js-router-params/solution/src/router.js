// Route table: maps (method, path) to a handler.
//
// A pattern is a "/"-separated list of segments:
//   static   matches the identical raw path segment (case-sensitive)
//   :name    matches one non-empty path segment, captured as params.name
//   :name?   (last segment only) like :name, but the segment may be absent
//   *        (last segment only) matches the rest of the path, possibly empty,
//            captured as params["*"] without a leading slash
//
// Captured values are percent-decoded after the path has been split; a value
// with a malformed escape means the route does not match. A trailing slash on
// the request path is ignored ("/" itself stays "/").
//
// When several routes match, the most specific wins: patterns are compared
// segment by segment from the left, static > parameter > "*", and a pattern
// that has already ended beats one that continues there. Ties go to the route
// added first. HEAD requests fall back to GET routes.
//
// Methods are case-insensitive and stored upper-cased.

const METHOD_RE = /^[A-Za-z][A-Za-z-]*$/;

const STATIC = 0;
const PARAM = 1;
const WILDCARD = 2;
// Rank of "no segment here": a pattern that has ended can only be compared at
// that position with an optional parameter or "*" that matched nothing.
const END = -1;

/** Validates an HTTP method name and returns it upper-cased. */
export function normalizeMethod(method) {
  if (typeof method !== 'string' || !METHOD_RE.test(method)) {
    throw new TypeError(`invalid HTTP method: ${String(method)}`);
  }
  return method.toUpperCase();
}

/** Splits an absolute path into raw segments, ignoring one trailing slash. */
function splitPath(path) {
  const trimmed = path.length > 1 && path.endsWith('/') ? path.slice(0, -1) : path;
  return trimmed === '/' ? [] : trimmed.slice(1).split('/');
}

function compilePattern(pattern) {
  if (typeof pattern !== 'string' || !pattern.startsWith('/')) {
    throw new TypeError(
      `route pattern must be a string starting with "/", got ${JSON.stringify(pattern)}`,
    );
  }
  const parts = splitPath(pattern);
  const names = new Set();
  return parts.map((part, i) => {
    const last = i === parts.length - 1;
    if (part === '*') {
      if (!last) throw new Error(`"*" must be the last segment of ${pattern}`);
      return { kind: WILDCARD };
    }
    if (part.startsWith(':')) {
      const optional = part.endsWith('?');
      const name = optional ? part.slice(1, -1) : part.slice(1);
      if (name === '') throw new Error(`empty parameter name in ${pattern}`);
      if (optional && !last) {
        throw new Error(`optional parameter :${name}? must be the last segment of ${pattern}`);
      }
      if (names.has(name)) throw new Error(`duplicate parameter :${name} in ${pattern}`);
      names.add(name);
      return { kind: PARAM, name, optional };
    }
    return { kind: STATIC, value: part };
  });
}

function decode(raw) {
  try {
    return decodeURIComponent(raw);
  } catch {
    return null;
  }
}

/** Matches compiled segments against raw path segments; returns params or null. */
function matchSegments(segments, parts) {
  const params = Object.create(null);
  for (let i = 0; i < segments.length; i++) {
    const segment = segments[i];
    if (segment.kind === WILDCARD) {
      const value = decode(parts.slice(i).join('/'));
      if (value === null) return null;
      params['*'] = value;
      return params;
    }
    if (i >= parts.length) {
      // Only a trailing optional parameter may be left over.
      return segment.kind === PARAM && segment.optional ? params : null;
    }
    const part = parts[i];
    if (segment.kind === STATIC) {
      if (part !== segment.value) return null;
      continue;
    }
    if (part === '') return null;
    const value = decode(part);
    if (value === null) return null;
    params[segment.name] = value;
  }
  return segments.length === parts.length ? params : null;
}

/** Negative when `a` is more specific than `b`, 0 when they rank the same. */
function compareSpecificity(a, b) {
  const n = Math.max(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const ka = i < a.length ? a[i].kind : END;
    const kb = i < b.length ? b[i].kind : END;
    if (ka !== kb) return ka - kb;
  }
  return 0;
}

function toParts(path) {
  return typeof path === 'string' && path.startsWith('/') ? splitPath(path) : null;
}

export class Router {
  #routes = [];

  /**
   * Registers `handler` for `method` requests to `pattern`.
   * Returns the router so calls can be chained.
   */
  add(method, pattern, handler) {
    const verb = normalizeMethod(method);
    const segments = compilePattern(pattern);
    if (typeof handler !== 'function') {
      throw new TypeError(`handler for ${verb} ${pattern} must be a function`);
    }
    this.#routes.push({ method: verb, pattern, segments, handler });
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
    const parts = toParts(path);
    if (parts === null) return null;
    let found = this.#best(verb, parts);
    if (!found && verb === 'HEAD') found = this.#best('GET', parts);
    return found && { handler: found.route.handler, params: found.params };
  }

  /**
   * The methods that have a route matching `path`, sorted alphabetically,
   * including HEAD whenever GET is there. The dispatcher uses this for 405
   * responses and OPTIONS.
   */
  allowedMethods(path) {
    const parts = toParts(path);
    if (parts === null) return [];
    const methods = new Set();
    for (const route of this.#routes) {
      if (!methods.has(route.method) && matchSegments(route.segments, parts)) {
        methods.add(route.method);
      }
    }
    if (methods.has('GET')) methods.add('HEAD');
    return [...methods].sort();
  }

  /** The registered routes, in the order they were added. */
  routes() {
    return this.#routes.map(({ method, pattern }) => ({ method, pattern }));
  }

  #best(verb, parts) {
    let best = null;
    for (const route of this.#routes) {
      if (route.method !== verb) continue;
      const params = matchSegments(route.segments, parts);
      if (params === null) continue;
      // Strictly more specific only: on a tie the earlier route stays.
      if (best === null || compareSpecificity(route.segments, best.route.segments) < 0) {
        best = { route, params };
      }
    }
    return best;
  }
}
