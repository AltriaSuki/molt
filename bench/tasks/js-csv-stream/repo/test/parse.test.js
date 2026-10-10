import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parse } from '../src/index.js';

test('splits lines and fields', () => {
  assert.deepEqual(parse('a,b,c\n1,2,3\n'), [
    ['a', 'b', 'c'],
    ['1', '2', '3'],
  ]);
});

test('last line does not need a line ending', () => {
  assert.deepEqual(parse('a,b\n1,2'), [
    ['a', 'b'],
    ['1', '2'],
  ]);
});

test('accepts CRLF line endings', () => {
  assert.deepEqual(parse('a,b\r\nc,d\r\n'), [
    ['a', 'b'],
    ['c', 'd'],
  ]);
});

test('skips blank lines', () => {
  assert.deepEqual(parse('a\n\nb\n\n'), [['a'], ['b']]);
});

test('keeps empty fields', () => {
  assert.deepEqual(parse('a,,c\n,\n'), [
    ['a', '', 'c'],
    ['', ''],
  ]);
});

test('a quoted field may contain the delimiter', () => {
  assert.deepEqual(parse('name,city\n"Smith, J",Oslo\n'), [
    ['name', 'city'],
    ['Smith, J', 'Oslo'],
  ]);
});

test('custom delimiter', () => {
  assert.deepEqual(parse('a;b\n1,5;2', { delimiter: ';' }), [
    ['a', 'b'],
    ['1,5', '2'],
  ]);
  assert.deepEqual(parse('a\tb\n', { delimiter: '\t' }), [['a', 'b']]);
});

test('empty input gives no records', () => {
  assert.deepEqual(parse(''), []);
});

test('rejects non-string input', () => {
  assert.throws(() => parse(null), TypeError);
  assert.throws(() => parse(Buffer.from('a,b')), TypeError);
});
