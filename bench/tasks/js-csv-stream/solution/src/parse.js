// Reading CSV text (RFC 4180), all at once or incrementally.
//
// CsvParser is a character-level state machine that keeps its state between
// push() calls, so the input may be cut into chunks anywhere. parse() is one
// push() of the whole text followed by end().

export class CsvError extends Error {
  /**
   * @param {string} message
   * @param {number} line 1-based line on which the offending record starts
   */
  constructor(message, line) {
    super(`${message} (line ${line})`);
    this.name = 'CsvError';
    this.line = line;
  }
}

const BOM = '\uFEFF';

// Parser states.
const FIELD_START = 0; // at the start of a field
const UNQUOTED = 1; // inside an unquoted field
const QUOTED = 2; // inside a quoted field
const QUOTE_SEEN = 3; // a quote inside a quoted field: the closing one, or half of ""
const CR_SEEN = 4; // a CR outside quotes; only a following LF makes it a line ending
const CR_AFTER_QUOTE = 5; // a CR right after a closing quote; must be followed by LF

export class CsvParser {
  #delimiter;
  #header;
  #keys = null; // field names, once the header record has been read
  #state = FIELD_START;
  #field = '';
  #fields = [];
  #blank = true; // nothing but (possibly) a CR seen since the record started
  #started = false; // any character consumed yet (the BOM check)
  #line = 1; // 1 + LFs consumed so far
  #recordLine = 1; // line on which the current record starts
  #done = [];

  /**
   * @param {{ delimiter?: string, header?: boolean }} [options]
   */
  constructor({ delimiter = ',', header = false } = {}) {
    if (
      typeof delimiter !== 'string' ||
      delimiter.length !== 1 ||
      delimiter === '"' ||
      delimiter === '\r' ||
      delimiter === '\n'
    ) {
      throw new TypeError('delimiter must be one character other than a double quote, CR or LF');
    }
    this.#delimiter = delimiter;
    this.#header = Boolean(header);
  }

  /**
   * Feed the next piece of input.
   *
   * @param {string} chunk
   * @returns {Array<string[] | Record<string, string>>} records completed by this chunk
   */
  push(chunk) {
    if (typeof chunk !== 'string') {
      throw new TypeError(`push() expects a string, got ${typeof chunk}`);
    }
    for (let i = 0; i < chunk.length; i++) {
      this.#consume(chunk[i]);
    }
    return this.#take();
  }

  /**
   * Signal the end of the input.
   *
   * @returns {Array<string[] | Record<string, string>>} the remaining records
   */
  end() {
    switch (this.#state) {
      case QUOTED:
        throw new CsvError('unterminated quoted field', this.#recordLine);
      case CR_AFTER_QUOTE:
        throw new CsvError('unexpected CR after closing quote', this.#recordLine);
      case CR_SEEN:
        // A trailing CR is data.
        this.#field += '\r';
        this.#endRecord();
        break;
      default:
        if (!this.#blank) this.#endRecord();
    }
    return this.#take();
  }

  #take() {
    const records = this.#done;
    this.#done = [];
    return records;
  }

  #consume(ch) {
    if (!this.#started) {
      this.#started = true;
      if (ch === BOM) return;
    }
    switch (this.#state) {
      case FIELD_START:
        if (ch === '"') {
          this.#blank = false;
          this.#state = QUOTED;
        } else {
          this.#unquoted(ch);
        }
        return;
      case UNQUOTED:
        this.#unquoted(ch);
        return;
      case QUOTED:
        if (ch === '"') {
          this.#state = QUOTE_SEEN;
        } else {
          if (ch === '\n') this.#line++;
          this.#field += ch;
        }
        return;
      case QUOTE_SEEN:
        if (ch === '"') {
          this.#field += '"';
          this.#state = QUOTED;
        } else if (ch === this.#delimiter) {
          this.#endField();
        } else if (ch === '\n') {
          this.#endLine();
        } else if (ch === '\r') {
          this.#state = CR_AFTER_QUOTE;
        } else {
          throw this.#afterQuote(ch);
        }
        return;
      case CR_AFTER_QUOTE:
        if (ch !== '\n') throw this.#afterQuote('\r');
        this.#endLine();
        return;
      case CR_SEEN:
        if (ch === '\n') {
          this.#endLine();
        } else {
          // The CR was data after all.
          this.#field += '\r';
          this.#blank = false;
          this.#state = UNQUOTED;
          this.#unquoted(ch);
        }
        return;
      default:
        throw new Error(`unreachable parser state ${this.#state}`);
    }
  }

  #unquoted(ch) {
    if (ch === '\r') {
      this.#state = CR_SEEN;
      return;
    }
    if (ch === '\n') {
      this.#endLine();
      return;
    }
    this.#blank = false;
    if (ch === this.#delimiter) {
      this.#endField();
    } else {
      this.#field += ch;
      this.#state = UNQUOTED;
    }
  }

  #afterQuote(ch) {
    return new CsvError(
      `unexpected ${JSON.stringify(ch)} after closing quote`,
      this.#recordLine,
    );
  }

  #endField() {
    this.#fields.push(this.#field);
    this.#field = '';
    this.#state = FIELD_START;
  }

  // An LF that ends a line outside quotes.
  #endLine() {
    if (!this.#blank) this.#endRecord();
    this.#line++;
    this.#recordLine = this.#line;
    this.#blank = true;
    this.#state = FIELD_START;
  }

  #endRecord() {
    this.#fields.push(this.#field);
    const fields = this.#fields;
    this.#fields = [];
    this.#field = '';
    this.#state = FIELD_START;
    if (!this.#header) {
      this.#done.push(fields);
    } else if (this.#keys === null) {
      const seen = new Set();
      for (const name of fields) {
        if (seen.has(name)) {
          throw new CsvError(`duplicate header name ${JSON.stringify(name)}`, this.#recordLine);
        }
        seen.add(name);
      }
      this.#keys = fields;
    } else if (fields.length !== this.#keys.length) {
      throw new CsvError(
        `expected ${this.#keys.length} fields but found ${fields.length}`,
        this.#recordLine,
      );
    } else {
      this.#done.push(Object.fromEntries(this.#keys.map((key, i) => [key, fields[i]])));
    }
  }
}

/**
 * Parse CSV text. Takes the same options as CsvParser and returns what one
 * push(text) followed by end() returns.
 *
 * @param {string} text
 * @param {{ delimiter?: string, header?: boolean }} [options]
 * @returns {Array<string[] | Record<string, string>>}
 */
export function parse(text, options) {
  if (typeof text !== 'string') {
    throw new TypeError(`parse() expects a string, got ${typeof text}`);
  }
  const parser = new CsvParser(options);
  const records = parser.push(text);
  return records.concat(parser.end());
}
