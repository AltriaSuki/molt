import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { run, USAGE } from '../src/cli.js';

function exec(...argv) {
  let out = '';
  let err = '';
  const code = run(argv, { out: (s) => (out += s), err: (s) => (err += s) });
  return { code, out, err };
}

describe('pkgver cli', () => {
  it('prints usage', () => {
    assert.deepEqual(exec('help'), { code: 0, out: USAGE, err: '' });
    assert.equal(exec().code, 2);
  });

  it('rejects unknown commands', () => {
    const r = exec('frobnicate');
    assert.equal(r.code, 2);
    assert.match(r.err, /unknown command: frobnicate/);
  });

  it('valid', () => {
    assert.deepEqual(exec('valid', 'v1.2.3+abc'), { code: 0, out: '1.2.3\n', err: '' });
    const r = exec('valid', '1.2');
    assert.equal(r.code, 1);
    assert.equal(r.out, '');
  });

  it('compare', () => {
    assert.equal(exec('compare', '1.0.0-rc.1', '1.0.0').out, '-1\n');
    assert.equal(exec('compare', '1.0.0', '1.0.0+x').out, '0\n');
    const r = exec('compare', '1.0.0', 'nope');
    assert.equal(r.code, 2);
    assert.match(r.err, /Invalid version: nope/);
  });

  it('sort', () => {
    assert.equal(exec('sort', '1.10.0', '1.9.0', '1.9.0-rc.1').out, '1.9.0-rc.1\n1.9.0\n1.10.0\n');
    assert.equal(exec('sort', '-r', '1.10.0', '1.9.0').out, '1.10.0\n1.9.0\n');
  });

  it('inc', () => {
    assert.equal(exec('inc', '1.2.3', 'minor').out, '1.3.0\n');
    assert.equal(exec('inc', '1.2.3', 'prerelease', 'beta').out, '1.2.4-beta.0\n');
    assert.equal(exec('inc', '1.2.3', 'micro').code, 2);
    assert.equal(exec('inc', '1.2').code, 2);
  });

  it('max and min print nothing and exit 1 when nothing matches', () => {
    assert.deepEqual(exec('max', '*'), { code: 1, out: '', err: '' });
    assert.deepEqual(exec('min', '*', 'latest'), { code: 1, out: '', err: '' });
  });
});
