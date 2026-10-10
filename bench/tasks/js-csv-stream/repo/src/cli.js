// csvlite command line: CSV in, one JSON value per line out.

import { parse } from './parse.js';
import { inferRecord } from './infer.js';

export const USAGE = `usage: csvlite [--header] [--infer] [--delimiter <char>] [file]

Reads CSV from <file> (or standard input when it is missing or "-") and
writes one JSON value per record: an array of fields, or an object keyed by
the first line's names with --header. --infer turns numbers, true/false and
empty fields into JSON numbers, booleans and null. Use --delimiter '\\t' for
tab-separated input.`;

export class UsageError extends Error {
  constructor(message) {
    super(message);
    this.name = 'UsageError';
  }
}

/**
 * @param {string[]} argv arguments after the program name
 */
export function parseArgs(argv) {
  const opts = { header: false, infer: false, delimiter: ',', file: null, help: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case '-h':
      case '--help':
        opts.help = true;
        break;
      case '--header':
        opts.header = true;
        break;
      case '--infer':
        opts.infer = true;
        break;
      case '-d':
      case '--delimiter': {
        const value = argv[++i];
        if (value === undefined) throw new UsageError(`${arg} needs a value`);
        opts.delimiter = value === '\\t' ? '\t' : value;
        if (opts.delimiter.length !== 1) {
          throw new UsageError('the delimiter must be a single character');
        }
        break;
      }
      default:
        if (arg.startsWith('-') && arg !== '-') throw new UsageError(`unknown option ${arg}`);
        if (opts.file !== null) throw new UsageError('only one input file is supported');
        opts.file = arg;
    }
  }
  return opts;
}

/**
 * Convert CSV text to JSON lines.
 *
 * @param {string} text
 * @param {{ header?: boolean, infer?: boolean, delimiter?: string }} [options]
 * @returns {string}
 */
export function toJsonLines(text, { header = false, infer = false, delimiter = ',' } = {}) {
  let records = parse(text, { delimiter });
  if (header) {
    const [names = [], ...rest] = records;
    records = rest.map((fields) =>
      Object.fromEntries(names.map((name, i) => [name, fields[i] ?? ''])),
    );
  }
  if (infer) records = records.map(inferRecord);
  return records.map((record) => `${JSON.stringify(record)}\n`).join('');
}

async function readAll(stream) {
  stream.setEncoding?.('utf8');
  let text = '';
  for await (const chunk of stream) text += chunk;
  return text;
}

/**
 * Run the CLI. Returns the exit code.
 *
 * @param {string[]} argv
 * @param {{ stdin: AsyncIterable<string>, stdout: { write(s: string): unknown },
 *           stderr: { write(s: string): unknown },
 *           readFile: (path: string, encoding: string) => Promise<string> }} io
 */
export async function main(argv, { stdin, stdout, stderr, readFile }) {
  let opts;
  try {
    opts = parseArgs(argv);
  } catch (err) {
    if (!(err instanceof UsageError)) throw err;
    stderr.write(`csvlite: ${err.message}\n${USAGE}\n`);
    return 2;
  }
  if (opts.help) {
    stdout.write(`${USAGE}\n`);
    return 0;
  }
  const text =
    opts.file === null || opts.file === '-' ? await readAll(stdin) : await readFile(opts.file, 'utf8');
  stdout.write(toJsonLines(text, opts));
  return 0;
}
