import { test } from 'node:test';
import assert from 'node:assert/strict';
import { stringify, formatField, parse } from '../src/index.js';

test('formatField leaves plain values alone', () => {
  assert.equal(formatField('abc'), 'abc');
  assert.equal(formatField(12.5), '12.5');
  assert.equal(formatField(null), '');
  assert.equal(formatField(undefined), '');
});

test('formatField quotes delimiters, quotes and line breaks', () => {
  assert.equal(formatField('a,b'), '"a,b"');
  assert.equal(formatField('say "hi"'), '"say ""hi"""');
  assert.equal(formatField('two\nlines'), '"two\nlines"');
  assert.equal(formatField('cr\ronly'), '"cr\ronly"');
  assert.equal(formatField('a;b', ';'), '"a;b"');
  assert.equal(formatField('a;b'), 'a;b');
});

test('stringify writes CRLF after every record', () => {
  assert.equal(
    stringify([
      ['a', 'b'],
      [1, 2],
    ]),
    'a,b\r\n1,2\r\n',
  );
  assert.equal(stringify([]), '');
});

test('stringify options', () => {
  assert.equal(stringify([['a', 'b;c']], { delimiter: ';', eol: '\n' }), 'a;"b;c"\n');
});

test('a record with one empty field is not written as a blank line', () => {
  assert.equal(stringify([[''], ['x']]), '""\r\nx\r\n');
});

test('columns writes a header and accepts objects', () => {
  const out = stringify(
    [
      { id: 1, name: 'Ann' },
      { name: 'Bo, Jr.', id: 2 },
      ['3', 'Cy'],
      { id: 4 },
    ],
    { columns: ['id', 'name'], eol: '\n' },
  );
  assert.equal(out, 'id,name\n1,Ann\n2,"Bo, Jr."\n3,Cy\n4,\n');
});

test('objects without columns are rejected', () => {
  assert.throws(() => stringify([{ a: 1 }]), TypeError);
});

test('simple records survive a round trip', () => {
  const rows = [
    ['id', 'city', 'note'],
    ['1', 'Oslo, Norway', ''],
    ['2', 'Lima', 'ok'],
  ];
  assert.deepEqual(parse(stringify(rows)), rows);
});
