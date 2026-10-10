// Optional value typing for the CLI's --infer flag.

const NUMBER = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/;

/**
 * Turn a field string into a JSON value: '' becomes null, 'true'/'false'
 * become booleans, plain decimal numbers become numbers, and everything else
 * stays a string. Numbers that would lose precision stay strings.
 *
 * @param {string} s
 * @returns {string | number | boolean | null}
 */
export function inferValue(s) {
  if (s === '') return null;
  if (s === 'true') return true;
  if (s === 'false') return false;
  if (NUMBER.test(s)) {
    const n = Number(s);
    if (Number.isFinite(n) && !(Number.isInteger(n) && !Number.isSafeInteger(n))) {
      return n;
    }
  }
  return s;
}

/**
 * Apply inferValue to every field of a record (an array or a plain object).
 */
export function inferRecord(record) {
  if (Array.isArray(record)) return record.map(inferValue);
  const out = {};
  for (const [key, value] of Object.entries(record)) {
    out[key] = inferValue(value);
  }
  return out;
}
