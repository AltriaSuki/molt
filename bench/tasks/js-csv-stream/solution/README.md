# csvlite

A small, dependency-free CSV library for Node.js (20+), plus a command line
tool that turns CSV into JSON lines.

```js
import { parse, stringify, CsvParser } from './src/index.js';

parse('name,city\n"Smith, J",Oslo\n');
// [['name', 'city'], ['Smith, J', 'Oslo']]

parse('id,note\r\n1,"two\nlines"\r\n', { header: true });
// [{ id: '1', note: 'two\nlines' }]

stringify([['id', 'note'], [1, 'say "hi"']]);
// 'id,note\r\n1,"say ""hi"""\r\n'

// Incremental parsing: chunks may be cut anywhere.
const parser = new CsvParser({ header: true });
parser.push('id,na');   // []
parser.push('me\n1,A'); // []
parser.push('nn\n2,B'); // [{ id: '1', name: 'Ann' }]
parser.push('o');       // []
parser.end();           // [{ id: '2', name: 'Bo' }]
```

## Parsing rules

The reader follows RFC 4180:

- records end at LF or CRLF; a CR that is not followed by LF is data;
- a field that starts with `"` is quoted: inside it `""` is a literal quote
  and the delimiter, CR and LF are data; only the delimiter or a line ending
  may follow the closing quote;
- a `"` inside a field that does not start with one is a literal character;
- completely empty lines are skipped, and a BOM at the very start is dropped;
- with `header: true` the first record names the fields and later records
  become objects; duplicate names and records with a different number of
  fields are errors.

Malformed input throws a `CsvError` whose `line` property is the 1-based line
(counting every LF, including those inside quoted fields) on which the bad
record starts.

## Layout

- `src/parse.js` - `CsvParser` (`push(chunk)` / `end()`), `parse(text,
  { delimiter, header })` and `CsvError`.
- `src/stringify.js` - `stringify(records, { delimiter, eol, columns })` and
  `formatField(value, delimiter)`: records back to CSV with RFC 4180 quoting.
- `src/infer.js` - value typing used by the CLI's `--infer` flag.
- `src/cli.js` - argument parsing and the CLI's `main`; `bin/csvlite.js` is
  the executable.
- `src/index.js` - the package entry; re-exports the library functions.

## CLI

```sh
node bin/csvlite.js --header --infer data.csv
cat data.csv | node bin/csvlite.js --delimiter ';'
```

## Tests

```sh
npm test
```

which runs `node --test` on the files in `test/`.
