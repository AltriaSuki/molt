// Plain response objects: { status, headers, body }.
// Header names are always lower-case; bodies are strings.

export function lowerCaseKeys(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers ?? {})) {
    out[name.toLowerCase()] = value;
  }
  return out;
}

export function response(status, body = '', headers = {}) {
  return { status, headers: lowerCaseKeys(headers), body };
}

export function text(body, status = 200, headers = {}) {
  return response(status, String(body), {
    'content-type': 'text/plain; charset=utf-8',
    ...lowerCaseKeys(headers),
  });
}

export function json(data, status = 200, headers = {}) {
  return response(status, JSON.stringify(data), {
    'content-type': 'application/json',
    ...lowerCaseKeys(headers),
  });
}

export function empty(status = 204, headers = {}) {
  return response(status, '', headers);
}

export function isResponse(value) {
  return (
    value !== null &&
    typeof value === 'object' &&
    Number.isInteger(value.status) &&
    typeof value.headers === 'object' &&
    'body' in value
  );
}
