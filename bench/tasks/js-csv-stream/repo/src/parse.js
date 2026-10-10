// Reading CSV text.
//
// The input is split into lines first and each line into fields. Double
// quotes switch "quoted mode" on and off, so a quoted field may contain the
// delimiter.

/**
 * Parse CSV text into records, one array of field strings per line.
 * Blank lines are skipped.
 *
 * @param {string} text
 * @param {{ delimiter?: string }} [options]
 * @returns {string[][]}
 */
export function parse(text, { delimiter = ',' } = {}) {
  if (typeof text !== 'string') {
    throw new TypeError(`parse() expects a string, got ${typeof text}`);
  }
  const records = [];
  for (const line of text.split(/\r?\n/)) {
    if (line === '') continue;
    records.push(splitLine(line, delimiter));
  }
  return records;
}

function splitLine(line, delimiter) {
  const fields = [];
  let field = '';
  let quoted = false;
  for (const ch of line) {
    if (ch === '"') {
      quoted = !quoted;
    } else if (ch === delimiter && !quoted) {
      fields.push(field);
      field = '';
    } else {
      field += ch;
    }
  }
  fields.push(field);
  return fields;
}
