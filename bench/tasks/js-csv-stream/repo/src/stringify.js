// Writing CSV text (RFC 4180 quoting).

/**
 * Format one value as a CSV field. `null` and `undefined` become an empty
 * field; anything else is converted with String(). The field is quoted when
 * it contains the delimiter, a double quote, CR or LF, and quotes inside it
 * are doubled.
 *
 * @param {unknown} value
 * @param {string} [delimiter]
 * @returns {string}
 */
export function formatField(value, delimiter = ',') {
  const s = value === null || value === undefined ? '' : String(value);
  if (s.includes(delimiter) || /["\r\n]/.test(s)) {
    return `"${s.replaceAll('"', '""')}"`;
  }
  return s;
}

function formatRow(fields, delimiter) {
  // A record holding a single empty field would otherwise come out as a
  // blank line, which readers skip.
  if (fields.length === 1 && formatField(fields[0], delimiter) === '') {
    return '""';
  }
  return fields.map((value) => formatField(value, delimiter)).join(delimiter);
}

/**
 * Serialise records to CSV text. Every record, including the last, is
 * followed by `eol`.
 *
 * Records are arrays of values. When `columns` is given, a header line with
 * those names is written first and records may also be objects, whose values
 * are written in column order (missing keys give empty fields).
 *
 * @param {Iterable<unknown[] | Record<string, unknown>>} records
 * @param {{ delimiter?: string, eol?: string, columns?: string[] }} [options]
 * @returns {string}
 */
export function stringify(records, { delimiter = ',', eol = '\r\n', columns } = {}) {
  const lines = [];
  if (columns !== undefined) {
    lines.push(formatRow(columns, delimiter));
  }
  for (const record of records) {
    if (Array.isArray(record)) {
      lines.push(formatRow(record, delimiter));
    } else if (columns !== undefined && record !== null && typeof record === 'object') {
      lines.push(formatRow(columns.map((name) => record[name]), delimiter));
    } else {
      throw new TypeError('stringify() expects arrays, or objects together with the columns option');
    }
  }
  return lines.map((line) => line + eol).join('');
}
