// Request-target parsing. We only ever see origin-form targets
// ("/path?query"), so this avoids `new URL()` and its need for a base.

/**
 * Splits a request target into its pathname and query.
 * The fragment, if any, is dropped. The pathname is returned raw
 * (still percent-encoded).
 */
export function parseTarget(target) {
  if (typeof target !== 'string' || !target.startsWith('/')) {
    throw new TypeError(`request target must be an absolute path, got ${JSON.stringify(target)}`);
  }
  const hashAt = target.indexOf('#');
  const withoutHash = hashAt === -1 ? target : target.slice(0, hashAt);
  const queryAt = withoutHash.indexOf('?');
  const pathname = queryAt === -1 ? withoutHash : withoutHash.slice(0, queryAt);
  const search = queryAt === -1 ? '' : withoutHash.slice(queryAt + 1);
  return { pathname, query: new URLSearchParams(search) };
}
