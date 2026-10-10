/**
 * A copy of a config that is safe to log: values under secret-looking keys
 * are replaced with "[redacted]". Keys are compared case-insensitively.
 */
export const REDACTED = '[redacted]';

export const SECRET_KEYS = ['password', 'secret', 'token', 'apiKey', 'privateKey'];

function isRecord(value) {
  return value !== null && typeof value === 'object' && Object.getPrototypeOf(value) === Object.prototype;
}

export function redact(config, { keys = SECRET_KEYS } = {}) {
  const secret = new Set(keys.map((key) => key.toLowerCase()));
  const walk = (value) => {
    if (Array.isArray(value)) return value.map(walk);
    if (!isRecord(value)) return value;
    return Object.fromEntries(
      Object.entries(value).map(([key, inner]) => [
        key,
        secret.has(key.toLowerCase()) && inner !== null && inner !== undefined ? REDACTED : walk(inner),
      ]),
    );
  };
  return walk(config);
}
