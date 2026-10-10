import { test, describe } from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { parse, CsvParser, CsvError } from '../src/index.js';
import { toJsonLines, main } from '../src/cli.js';

// ---------------------------------------------------------------- helpers

function feed(chunks, options) {
  const parser = new CsvParser(options);
  const out = [];
  for (const chunk of chunks) out.push(...parser.push(chunk));
  out.push(...parser.end());
  return out;
}

// Records, or the line of the CsvError that was thrown.
function outcome(fn) {
  try {
    return { records: fn() };
  } catch (err) {
    if (err instanceof CsvError) return { error: err.line };
    throw err;
  }
}

function assertCsvError(fn, line) {
  assert.throws(fn, (err) => {
    assert.ok(err instanceof CsvError, `expected a CsvError, got ${err}`);
    assert.ok(err instanceof Error);
    assert.equal(err.line, line);
    return true;
  });
}

// Feed one character at a time; note which records each push returned.
function feedChars(text, options) {
  const parser = new CsvParser(options);
  const emitted = []; // [index of the pushed char, record]
  for (let i = 0; i < text.length; i++) {
    for (const record of parser.push(text[i])) emitted.push([i, record]);
  }
  const tail = parser.end();
  return { emitted, tail };
}

function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function randomChunks(text, rand) {
  const cuts = new Set();
  const n = Math.floor(rand() * 9);
  for (let i = 0; i < n; i++) cuts.add(Math.floor(rand() * (text.length + 1)));
  const sorted = [...cuts].sort((a, b) => a - b);
  const chunks = [];
  let prev = 0;
  for (const cut of sorted) {
    chunks.push(text.slice(prev, cut));
    prev = cut;
  }
  chunks.push(text.slice(prev));
  return chunks;
}

// ------------------------------------------------------- RFC 4180 parsing

describe('parse: quoting', () => {
  test('doubled quotes inside a quoted field are a literal quote', () => {
    assert.deepEqual(parse('"a ""b"" c",d'), [['a "b" c', 'd']]);
    assert.deepEqual(parse('""""'), [['"']]);
    assert.deepEqual(parse('"""",""""""\n'), [['"', '""']]);
    assert.deepEqual(parse('"x""",y'), [['x"', 'y']]);
  });

  test('line breaks inside quotes are data', () => {
    assert.deepEqual(parse('x,"1\n2",y\nz'), [['x', '1\n2', 'y'], ['z']]);
    assert.deepEqual(parse('"a\r\nb"\r\nc\r\n'), [['a\r\nb'], ['c']]);
    assert.deepEqual(parse('"a\rb",c'), [['a\rb', 'c']]);
    assert.deepEqual(parse('"a""\nb"'), [['a"\nb']]);
    assert.deepEqual(parse('"\n\n"\n'), [['\n\n']]);
  });

  test('the delimiter inside quotes is data', () => {
    assert.deepEqual(parse('"a,b",c\n"d,"'), [['a,b', 'c'], ['d,']]);
  });

  test('quoted empty fields', () => {
    assert.deepEqual(parse('"",""\n""\n'), [['', ''], ['']]);
    assert.deepEqual(parse('"a",\n,"b"'), [['a', ''], ['', 'b']]);
  });

  test('a quote in a field that does not start with one is literal', () => {
    assert.deepEqual(parse('a"b,c""d,e"\n'), [['a"b', 'c""d', 'e"']]);
    assert.deepEqual(parse(' "x",y'), [[' "x"', 'y']]);
    assert.deepEqual(parse('x"\n"y"'), [['x"'], ['y']]);
  });
});

describe('parse: line endings and blank lines', () => {
  test('LF, CRLF and a mix of both end records', () => {
    assert.deepEqual(parse('a\nb\r\nc'), [['a'], ['b'], ['c']]);
    assert.deepEqual(parse('a,b\r\n"c",d\r\n'), [['a', 'b'], ['c', 'd']]);
  });

  test('a CR not followed by LF is ordinary data', () => {
    assert.deepEqual(parse('a\rb,c\r\n'), [['a\rb', 'c']]);
    assert.deepEqual(parse('a\r'), [['a\r']]);
    assert.deepEqual(parse('\r'), [['\r']]);
    assert.deepEqual(parse('\r\r\n'), [['\r']]);
    assert.deepEqual(parse('a,\r'), [['a', '\r']]);
    assert.deepEqual(parse('1\r2\n"3"'), [['1\r2'], ['3']]);
    assert.deepEqual(parse('a\rb\r"c"d'), [['a\rb\r"c"d']]);
  });

  test('completely empty lines are skipped', () => {
    assert.deepEqual(parse('\n\na\r\n\r\n\nb\n\n'), [['a'], ['b']]);
    assert.deepEqual(parse('\n'), []);
    assert.deepEqual(parse('\r\n\r\n'), []);
    assert.deepEqual(parse('\n\n\n'), []);
  });

  test('no extra record after a final line ending', () => {
    assert.deepEqual(parse('a\n'), [['a']]);
    assert.deepEqual(parse('a\r\n'), [['a']]);
    assert.deepEqual(parse('"a"\r\n'), [['a']]);
  });

  test('lines with only separators or an empty quoted field are records', () => {
    assert.deepEqual(parse(','), [['', '']]);
    assert.deepEqual(parse('a,\n'), [['a', '']]);
    assert.deepEqual(parse('""\n,,\n'), [[''], ['', '', '']]);
  });

  test('records may have different numbers of fields without a header', () => {
    assert.deepEqual(parse('a\nb,c,d\n\ne'), [['a'], ['b', 'c', 'd'], ['e']]);
  });
});

describe('parse: BOM and delimiters', () => {
  test('a BOM at the very start is dropped', () => {
    assert.deepEqual(parse('\uFEFFa,b\n'), [['a', 'b']]);
    assert.deepEqual(parse('\uFEFF"q",r'), [['q', 'r']]);
    assert.deepEqual(parse('\uFEFF'), []);
    assert.deepEqual(parse('\uFEFF\n'), []);
  });

  test('a BOM anywhere else is data', () => {
    assert.deepEqual(parse('a,\uFEFFb'), [['a', '\uFEFFb']]);
    assert.deepEqual(parse('\uFEFF\uFEFFx'), [['\uFEFFx']]);
    assert.deepEqual(parse('a\n\uFEFFb'), [['a'], ['\uFEFFb']]);
  });

  test('custom delimiters', () => {
    assert.deepEqual(parse('a;"b;c";d,e\n', { delimiter: ';' }), [['a', 'b;c', 'd,e']]);
    assert.deepEqual(parse('a\t"b\tc"\t\n', { delimiter: '\t' }), [['a', 'b\tc', '']]);
    assert.deepEqual(parse('"x"|y', { delimiter: '|' }), [['x', 'y']]);
  });

  test('invalid delimiters throw TypeError', () => {
    for (const delimiter of ['', ',,', '"', '\n', '\r', 5, null]) {
      assert.throws(() => new CsvParser({ delimiter }), TypeError, `delimiter ${JSON.stringify(delimiter)}`);
    }
    assert.throws(() => parse('a;b', { delimiter: ';;' }), TypeError);
  });

  test('defaults work without options', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('a,b\n'), [['a', 'b']]);
    assert.deepEqual(parser.end(), []);
    assert.deepEqual(parse('a,b', {}), [['a', 'b']]);
  });
});

// ------------------------------------------------------------------ errors

describe('errors', () => {
  test('CsvError is an Error subclass named CsvError', () => {
    let caught;
    try {
      parse('"a"b');
    } catch (err) {
      caught = err;
    }
    assert.ok(caught instanceof CsvError);
    assert.ok(caught instanceof Error);
    assert.equal(caught.name, 'CsvError');
    assert.equal(typeof caught.line, 'number');
    assert.equal(caught.line, 1);
  });

  test('a character after a closing quote throws', () => {
    assertCsvError(() => parse('"a"b'), 1);
    assertCsvError(() => parse('x\ny\n"a" ,b'), 3);
    assertCsvError(() => parse('a,"b"c\n'), 1);
    assertCsvError(() => parse('a,b\r\n1,""x'), 2);
  });

  test('a CR after a closing quote must be followed by LF', () => {
    assertCsvError(() => parse('"a"\rb'), 1);
    assertCsvError(() => parse('"a"\r'), 1);
    assertCsvError(() => parse('k\n"a"\r"b"'), 2);
  });

  test('the line is where the record starts, counting LFs inside quotes', () => {
    assertCsvError(() => parse('"multi\nline"x'), 1);
    assertCsvError(() => parse('a\n"p\nq"\n\n"z"z'), 5);
    assertCsvError(() => parse('a\r\nb\r\n"c"d'), 3);
    assertCsvError(() => parse('\uFEFF\n\n"x"y'), 3);
    assertCsvError(() => parse('1\r2\n"3"x'), 2);
    assertCsvError(() => parse('a,"x\r\ny",b\nc,"d\n\n"e'), 3);
  });

  test('an unterminated quote throws at the end', () => {
    assertCsvError(() => parse('"abc'), 1);
    assertCsvError(() => parse('"'), 1);
    assertCsvError(() => parse('"a""'), 1);
    assertCsvError(() => parse('a\nb\n"never\nclosed'), 3);
    assertCsvError(() => parse('a\n\n\nb,"c\r\n'), 4);
  });

  test('streaming errors come from push or end', () => {
    const p1 = new CsvParser();
    assert.deepEqual(p1.push('a\n"b'), [['a']]);
    assert.deepEqual(p1.push('\nc'), []);
    assertCsvError(() => p1.end(), 2);

    const p2 = new CsvParser();
    assert.deepEqual(p2.push('ok\n"x"'), [['ok']]);
    assertCsvError(() => p2.push('y'), 2);

    const p3 = new CsvParser();
    assert.deepEqual(p3.push('ok\n"x"\r'), [['ok']]);
    assertCsvError(() => p3.push('y'), 2);

    const p4 = new CsvParser();
    assert.deepEqual(p4.push('"a"\r'), []);
    assertCsvError(() => p4.end(), 1);
  });

  test('header errors come from the call that completes the record', () => {
    const p1 = new CsvParser({ header: true });
    assert.deepEqual(p1.push('a,b\n1,2\n3'), [{ a: '1', b: '2' }]);
    assertCsvError(() => p1.end(), 3);

    const p2 = new CsvParser({ header: true });
    assert.deepEqual(p2.push('a,b\n'), []);
    assertCsvError(() => p2.push('1,2\n\n3,4,5\n'), 4);

    const p3 = new CsvParser({ header: true });
    assertCsvError(() => p3.push('x,y,x\n'), 1);

    const p4 = new CsvParser({ header: true });
    assert.deepEqual(p4.push('k\r\nv\r'), []);
    assertCsvError(() => p4.push('\n,\r\n'), 3);
  });
});

// ------------------------------------------------------------------ header

describe('header: true', () => {
  test('later records become objects keyed by the header', () => {
    assert.deepEqual(parse('id,name\n1,Ann\n2,"Bo, Jr."\n', { header: true }), [
      { id: '1', name: 'Ann' },
      { id: '2', name: 'Bo, Jr.' },
    ]);
  });

  test('keys follow header order and records are plain objects', () => {
    const [record] = parse('b,a,c\r\n1,2,3', { header: true });
    assert.deepEqual(Object.keys(record), ['b', 'a', 'c']);
    assert.equal(Object.getPrototypeOf(record), Object.prototype);
  });

  test('a header alone, or nothing, gives no records', () => {
    assert.deepEqual(parse('id,name\n', { header: true }), []);
    assert.deepEqual(parse('id,name', { header: true }), []);
    assert.deepEqual(parse('', { header: true }), []);
    assert.deepEqual(parse('\n\n', { header: true }), []);
  });

  test('blank lines and a BOM before the header are skipped', () => {
    assert.deepEqual(parse('\uFEFF\n\nid\n\n1\n', { header: true }), [{ id: '1' }]);
  });

  test('quoted header names may contain anything', () => {
    assert.deepEqual(parse('"a,b","c\nd",""\n1,2,3', { header: true }), [
      { 'a,b': '1', 'c\nd': '2', '': '3' },
    ]);
  });

  test('names that exist on Object.prototype are ordinary names', () => {
    assert.deepEqual(parse('constructor,toString,hasOwnProperty\n1,2,3\n', { header: true }), [
      { constructor: '1', toString: '2', hasOwnProperty: '3' },
    ]);
  });

  test('duplicate header names throw on the header line', () => {
    assertCsvError(() => parse('a,b,a\n1,2,3', { header: true }), 1);
    assertCsvError(() => parse('\n\nx,"x"', { header: true }), 3);
    assertCsvError(() => parse(',', { header: true }), 1);
    // Duplicates in later records are just data.
    assert.deepEqual(parse('a,b\nx,x', { header: true }), [{ a: 'x', b: 'x' }]);
  });

  test('a record with a different field count throws on its line', () => {
    assertCsvError(() => parse('a,b\n1,2\n3\n', { header: true }), 3);
    assertCsvError(() => parse('a,b\n1,2,3', { header: true }), 2);
    assertCsvError(() => parse('a,b\n"1\n1",2\n3,4,5', { header: true }), 4);
    assertCsvError(() => parse('a,b\n""', { header: true }), 2);
  });

  test('a single column with an empty value', () => {
    assert.deepEqual(parse('name\n""\n\nx', { header: true }), [{ name: '' }, { name: 'x' }]);
  });

  test('the header record is not returned by push', () => {
    const parser = new CsvParser({ header: true, delimiter: ';' });
    assert.deepEqual(parser.push('a;b\n'), []);
    assert.deepEqual(parser.push('1;2\n3;'), [{ a: '1', b: '2' }]);
    assert.deepEqual(parser.end(), [{ a: '3', b: '' }]);
  });

  test('variable field counts are fine without a header', () => {
    assert.deepEqual(parse('a,b\n1\n', { header: false }), [['a', 'b'], ['1']]);
  });
});

// --------------------------------------------------------------- streaming

describe('streaming', () => {
  test('push returns the records each chunk completed', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('id,note\r\n1,"multi'), [['id', 'note']]);
    assert.deepEqual(parser.push('\nline ""quoted"""\r'), []);
    assert.deepEqual(parser.push('\n2,x'), [['1', 'multi\nline "quoted"']]);
    assert.deepEqual(parser.end(), [['2', 'x']]);
  });

  test('several records in one chunk come back in order', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('a\nb\n\nc\nd'), [['a'], ['b'], ['c']]);
    assert.deepEqual(parser.end(), [['d']]);
  });

  test('a record is only complete once its LF arrives', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('a,b'), []);
    assert.deepEqual(parser.push('\r'), []);
    assert.deepEqual(parser.push('\n'), [['a', 'b']]);
    assert.deepEqual(parser.push('c'), []);
    assert.deepEqual(parser.end(), [['c']]);
  });

  test('a CR at the end of a chunk followed by data is data', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('a\r'), []);
    assert.deepEqual(parser.push('b\n'), [['a\rb']]);
    assert.deepEqual(parser.push('c\r'), []);
    assert.deepEqual(parser.end(), [['c\r']]);
  });

  test('a doubled quote split across chunks', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('"x"'), []);
    assert.deepEqual(parser.push('"y"\n'), [['x"y']]);
    assert.deepEqual(parser.push('"p"'), []);
    assert.deepEqual(parser.push(',q\n'), [['p', 'q']]);
    assert.deepEqual(parser.end(), []);
  });

  test('a CRLF after a closing quote split across chunks', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('"a"\r'), []);
    assert.deepEqual(parser.push('\n'), [['a']]);
    assert.deepEqual(parser.end(), []);
  });

  test('empty chunks change nothing, including before a BOM', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push(''), []);
    assert.deepEqual(parser.push('\uFEFF'), []);
    assert.deepEqual(parser.push(''), []);
    assert.deepEqual(parser.push('\uFEFFb\n'), [['\uFEFFb']]);
    assert.deepEqual(parser.push(''), []);
    assert.deepEqual(parser.end(), []);
  });

  test('end returns nothing after a final line ending', () => {
    const parser = new CsvParser();
    assert.deepEqual(parser.push('a\r\n\r\n'), [['a']]);
    assert.deepEqual(parser.end(), []);
  });
});

// Inputs that exercise every state; each entry is [text, options, expected].
// `expected` is a list of records, or { error: line }.
const TRICKY = [
  ['id,note\r\n1,"multi\nline ""quoted"""\r\n2,x\r\n', {}, [
    ['id', 'note'],
    ['1', 'multi\nline "quoted"'],
    ['2', 'x'],
  ]],
  ['\uFEFF"a""b",c\r\n\r\n"",\r\nd\re,"f\r\ng"', {}, [
    ['a"b', 'c'],
    ['', ''],
    ['d\re', 'f\r\ng'],
  ]],
  ['a\r\r\n"x""""y"\n\n,\n"\r"\r\n', {}, [['a\r'], ['x""y'], ['', ''], ['\r']]],
  ['\uFEFF\uFEFF;"y;z"\r\n1;"";2\r', { delimiter: ';' }, [
    ['\uFEFF', 'y;z'],
    ['1', '', '2\r'],
  ]],
  ['\uFEFFk1,"k\r\n2"\r\n"v""1",v2\n\r\nw1,"w\n2"', { header: true }, [
    { k1: 'v"1', 'k\r\n2': 'v2' },
    { k1: 'w1', 'k\r\n2': 'w\n2' },
  ]],
  ['a"b,""""\n"c\r"\r\n', {}, [['a"b', '"'], ['c\r']]],
  ['x\n"a"b\nc', {}, { error: 2 }],
  ['a\n"open\r\n""', {}, { error: 2 }],
  ['ok\r\n"x"\r"', {}, { error: 2 }],
  ['h1,h2\r\n1,2\r\n\r\n3\r\n', { header: true }, { error: 4 }],
  ['h,"h"\r\n1,2', { header: true }, { error: 1 }],
];

function expectedOutcome(expected) {
  return Array.isArray(expected) ? { records: expected } : expected;
}

describe('chunking does not matter', () => {
  test('whole-input results of the tricky inputs', () => {
    for (const [text, options, expected] of TRICKY) {
      assert.deepEqual(outcome(() => parse(text, options)), expectedOutcome(expected), JSON.stringify(text));
    }
  });

  test('every split into two chunks', () => {
    for (const [text, options, expected] of TRICKY) {
      for (let i = 0; i <= text.length; i++) {
        const chunks = [text.slice(0, i), text.slice(i)];
        assert.deepEqual(
          outcome(() => feed(chunks, options)),
          expectedOutcome(expected),
          `${JSON.stringify(text)} split at ${i}`,
        );
      }
    }
  });

  test('every split into three chunks', () => {
    for (const [text, options, expected] of TRICKY.slice(0, 6)) {
      for (let i = 0; i <= text.length; i++) {
        for (let j = i; j <= text.length; j++) {
          const chunks = [text.slice(0, i), text.slice(i, j), text.slice(j)];
          assert.deepEqual(
            outcome(() => feed(chunks, options)),
            expectedOutcome(expected),
            `${JSON.stringify(text)} split at ${i} and ${j}`,
          );
        }
      }
    }
  });

  test('one character at a time: each record arrives with its LF', () => {
    for (const [text, options, expected] of TRICKY) {
      if (!Array.isArray(expected)) {
        assert.deepEqual(
          outcome(() => feed([...text], options)),
          expectedOutcome(expected),
          JSON.stringify(text),
        );
        continue;
      }
      const { emitted, tail } = feedChars(text, options);
      for (const [index] of emitted) {
        assert.equal(text[index], '\n', `${JSON.stringify(text)}: record returned at index ${index}`);
      }
      const lfEnds = emitted.map(([index]) => index);
      assert.equal(new Set(lfEnds).size, lfEnds.length, 'at most one record per LF');
      assert.deepEqual([...emitted.map(([, record]) => record), ...tail], expected);
      assert.ok(tail.length <= 1, 'end() returns at most the last record');
      if (text.endsWith('\n')) assert.deepEqual(tail, []);
    }
  });

  test('two-chunk pushes return exactly the records whose LF they contain', () => {
    for (const [text, options, expected] of TRICKY) {
      if (!Array.isArray(expected)) continue;
      const { emitted } = feedChars(text, options);
      for (let i = 0; i <= text.length; i++) {
        const before = emitted.filter(([index]) => index < i).length;
        const parser = new CsvParser(options);
        assert.deepEqual(parser.push(text.slice(0, i)), expected.slice(0, before), `split at ${i}`);
        const rest = [...parser.push(text.slice(i)), ...parser.end()];
        assert.deepEqual(rest, expected.slice(before), `split at ${i}`);
      }
    }
  });
});

// --------------------------------------------------- seeded random inputs

const FIELD_CHARS = ['a', 'b', 'z', ' ', ',', ';', '"', '\r', '\n', '\uFEFF', 'é', '\u{1F600}'];

function randomField(rand) {
  const len = Math.floor(rand() * 6);
  let s = '';
  for (let i = 0; i < len; i++) s += FIELD_CHARS[Math.floor(rand() * FIELD_CHARS.length)];
  return s;
}

// An independent RFC 4180 writer: quotes when needed, and sometimes when not.
function encodeField(s, delimiter, first, rand) {
  const needs =
    s.includes(delimiter) || /["\r\n]/.test(s) || (first && s.startsWith('\uFEFF'));
  if (needs || rand() < 0.3) return `"${s.replaceAll('"', '""')}"`;
  return s;
}

function encode(rows, delimiter, rand, withBom) {
  let text = withBom ? '\uFEFF' : '';
  rows.forEach((row, r) => {
    if (rand() < 0.2) text += rand() < 0.5 ? '\n' : '\r\n'; // a blank line
    const parts = row.map((field, i) => {
      const atStart = !withBom && text.length === 0 && i === 0;
      let encoded = encodeField(field, delimiter, atStart, rand);
      if (row.length === 1 && encoded === '') encoded = '""';
      return encoded;
    });
    text += parts.join(delimiter);
    if (r < rows.length - 1 || rand() < 0.5) text += rand() < 0.5 ? '\n' : '\r\n';
  });
  return text;
}

describe('seeded random inputs', () => {
  test('random valid CSV parses back to its rows under random chunking', () => {
    const rand = mulberry32(0xc5f1);
    for (let iter = 0; iter < 400; iter++) {
      const delimiter = rand() < 0.75 ? ',' : ';';
      const width = 1 + Math.floor(rand() * 4);
      const rows = [];
      const count = Math.floor(rand() * 5);
      for (let r = 0; r < count; r++) {
        const n = rand() < 0.7 ? width : 1 + Math.floor(rand() * 4);
        rows.push(Array.from({ length: n }, () => randomField(rand)));
      }
      const text = encode(rows, delimiter, rand, rand() < 0.3);
      const label = `iteration ${iter}: ${JSON.stringify(text)}`;
      assert.deepEqual(parse(text, { delimiter }), rows, label);
      assert.deepEqual(feed(randomChunks(text, rand), { delimiter }), rows, label);
    }
  });

  test('random valid CSV with a header gives objects under random chunking', () => {
    const rand = mulberry32(0x5eed);
    for (let iter = 0; iter < 200; iter++) {
      const width = 1 + Math.floor(rand() * 4);
      const names = new Set();
      while (names.size < width) names.add(randomField(rand));
      const header = [...names];
      const rows = [];
      const count = Math.floor(rand() * 4);
      for (let r = 0; r < count; r++) rows.push(header.map(() => randomField(rand)));
      const text = encode([header, ...rows], ',', rand, rand() < 0.3);
      const expected = rows.map((row) => Object.fromEntries(header.map((name, i) => [name, row[i]])));
      const label = `iteration ${iter}: ${JSON.stringify(text)}`;
      assert.deepEqual(parse(text, { header: true }), expected, label);
      assert.deepEqual(feed(randomChunks(text, rand), { header: true }), expected, label);
    }
  });

  test('arbitrary input gives the same records or error however it is chunked', () => {
    const rand = mulberry32(42);
    const alphabet = ['a', ',', '"', '"', '\r', '\n', '\n', '\uFEFF', ' '];
    for (let iter = 0; iter < 600; iter++) {
      const len = Math.floor(rand() * 25);
      let text = '';
      for (let i = 0; i < len; i++) text += alphabet[Math.floor(rand() * alphabet.length)];
      const options = { header: iter % 3 === 0 };
      const whole = outcome(() => parse(text, options));
      const label = `iteration ${iter}: ${JSON.stringify(text)}`;
      assert.deepEqual(outcome(() => feed(randomChunks(text, rand), options)), whole, label);
      assert.deepEqual(outcome(() => feed([...text], options)), whole, label);
    }
  });
});

// --------------------------------------------------------------------- CLI

describe('the CLI picks up the new parsing rules', () => {
  test('toJsonLines keeps quoted line breaks and quotes in one field', () => {
    assert.equal(
      toJsonLines('a,"b\nc"\n"say ""hi""",x\n'),
      `${JSON.stringify(['a', 'b\nc'])}\n${JSON.stringify(['say "hi"', 'x'])}\n`,
    );
  });

  test('main reads standard input split inside a quoted field', async () => {
    let out = '';
    const code = await main(['--header'], {
      stdin: Readable.from(['﻿k,v\r\n"a\r', '\nb",""""\r\n']),
      stdout: {
        write(s) {
          out += s;
          return true;
        },
      },
      stderr: { write: () => true },
      readFile: async () => assert.fail('no file was given'),
    });
    assert.equal(code, 0);
    assert.equal(out, `${JSON.stringify({ k: 'a\r\nb', v: '"' })}\n`);
  });
});
