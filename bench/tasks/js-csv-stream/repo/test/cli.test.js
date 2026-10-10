import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { parseArgs, toJsonLines, main, USAGE } from '../src/cli.js';

function sink() {
  return {
    text: '',
    write(s) {
      this.text += s;
      return true;
    },
  };
}

test('parseArgs defaults', () => {
  assert.deepEqual(parseArgs([]), {
    header: false,
    infer: false,
    delimiter: ',',
    file: null,
    help: false,
  });
});

test('parseArgs flags and file', () => {
  const opts = parseArgs(['--header', '-d', ';', '--infer', 'data.csv']);
  assert.equal(opts.header, true);
  assert.equal(opts.infer, true);
  assert.equal(opts.delimiter, ';');
  assert.equal(opts.file, 'data.csv');
  assert.equal(parseArgs(['--delimiter', '\\t']).delimiter, '\t');
});

test('parseArgs rejects bad usage', () => {
  assert.throws(() => parseArgs(['--nope']), { name: 'UsageError' });
  assert.throws(() => parseArgs(['-d']), { name: 'UsageError' });
  assert.throws(() => parseArgs(['-d', ';;']), { name: 'UsageError' });
  assert.throws(() => parseArgs(['a.csv', 'b.csv']), { name: 'UsageError' });
});

test('toJsonLines writes arrays by default', () => {
  assert.equal(toJsonLines('a,b\n1,2\n'), '["a","b"]\n["1","2"]\n');
});

test('toJsonLines with header and infer', () => {
  const out = toJsonLines('id,name,active\n1,Ann,true\n2,,false\n', { header: true, infer: true });
  assert.equal(
    out,
    '{"id":1,"name":"Ann","active":true}\n{"id":2,"name":null,"active":false}\n',
  );
});

test('main reads standard input in chunks', async () => {
  const stdout = sink();
  const stderr = sink();
  const code = await main(['--header'], {
    stdin: Readable.from(['k,v\n', 'x,1\ny,', '2\n']),
    stdout,
    stderr,
    readFile: async () => assert.fail('no file was given'),
  });
  assert.equal(code, 0);
  assert.equal(stdout.text, '{"k":"x","v":"1"}\n{"k":"y","v":"2"}\n');
  assert.equal(stderr.text, '');
});

test('main reads a file', async () => {
  const stdout = sink();
  const code = await main(['in.csv'], {
    stdin: Readable.from([]),
    stdout,
    stderr: sink(),
    readFile: async (path, encoding) => {
      assert.equal(path, 'in.csv');
      assert.equal(encoding, 'utf8');
      return 'a;b\n';
    },
  });
  assert.equal(code, 0);
  assert.equal(stdout.text, '["a;b"]\n');
});

test('main reports usage errors with exit code 2', async () => {
  const stdout = sink();
  const stderr = sink();
  const code = await main(['--bogus'], { stdin: Readable.from([]), stdout, stderr, readFile: null });
  assert.equal(code, 2);
  assert.equal(stdout.text, '');
  assert.match(stderr.text, /unknown option --bogus/);
  assert.ok(stderr.text.includes(USAGE));
});
