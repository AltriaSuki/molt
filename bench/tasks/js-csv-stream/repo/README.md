# csvlite

A small, dependency-free CSV library for Node.js (20+), plus a command line
tool that turns CSV into JSON lines.

```js
import { parse, stringify } from './src/index.js';

parse('name,city\n"Smith, J",Oslo\n');
// [['name', 'city'], ['Smith, J', 'Oslo']]

stringify([['id', 'note'], [1, 'say "hi"']]);
// 'id,note\r\n1,"say ""hi"""\r\n'
```

## Layout

- `src/parse.js` - `parse(text, { delimiter })`: CSV text to an array of
  records (arrays of strings).
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
