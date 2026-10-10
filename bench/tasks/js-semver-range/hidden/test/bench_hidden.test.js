import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import {
  SemVer,
  parse,
  satisfies,
  maxSatisfying,
  minSatisfying,
  filterSatisfying,
} from '../src/index.js';
import { run } from '../src/cli.js';

// [version, range, expected]
const TABLE = {
  'exact versions and primitive comparators': [
    ['1.2.3', '1.2.3', true],
    ['1.2.4', '1.2.3', false],
    ['1.2.3', '=1.2.3', true],
    ['1.2.3', 'v1.2.3', true],
    ['1.2.3', '=v1.2.3', true],
    ['v1.2.3', '1.2.3', true],
    ['1.2.3+build.7', '1.2.3', true],
    ['1.2.3', '1.2.3+meta', true],
    ['1.2.3', '>1.2.2', true],
    ['1.2.2', '>1.2.2', false],
    ['1.2.2', '>=1.2.2', true],
    ['1.2.1', '>=1.2.2', false],
    ['1.2.1', '<1.2.2', true],
    ['1.2.2', '<1.2.2', false],
    ['1.2.2', '<=1.2.2', true],
    ['1.2.3', '<=1.2.2', false],
    ['0.0.0', '>=0.0.0', true],
    ['1.10.0', '>1.9.0', true],
    ['10.0.0', '<9.0.0', false],
    ['1.2.3', '>=v1.2.3', true],
    ['1.2.3', '>= 1.2.3', true],
    ['1.2.3', '= 1.2.3', true],
    ['1.2.3', '<  1.2.3', false],
  ],

  'comparator sets (all must hold)': [
    ['1.5.0', '>=1.2.3 <2.0.0', true],
    ['2.0.0', '>=1.2.3 <2.0.0', false],
    ['1.2.2', '>=1.2.3 <2.0.0', false],
    ['1.5.0', '  >=1.2.3    <2.0.0  ', true],
    ['1.5.0', '>= 1.2.3 < 2', true],
    ['2.0.0', '>= 1.2.3 < 2', false],
    ['1.5.0', '>1.0.0 <1.5.0', false],
    ['1.2.3', '>1.0.0 <2.0.0 1.2.3', true],
    ['1.2.4', '>1.0.0 <2.0.0 1.2.3', false],
    ['1.2.3', '>1.0.0 <2.0.0 <=1.2.2', false],
  ],

  'alternatives (||)': [
    ['1.2.3', '1.2.3 || 2.0.0', true],
    ['2.0.0', '1.2.3 || 2.0.0', true],
    ['1.5.0', '1.2.3 || 2.0.0', false],
    ['2.5.0', '1.x||2.x', true],
    ['3.0.0', '1.x||2.x', false],
    ['3.0.0', '<1.0.0 || >=3.0.0', true],
    ['0.5.0', '<1.0.0 || >=3.0.0', true],
    ['2.0.0', '<1.0.0 || >=3.0.0', false],
    ['1.5.0', '>=1.0.0 <1.2.0 || >=1.4.0 <1.6.0 || 2.x', true],
    ['1.3.0', '>=1.0.0 <1.2.0 || >=1.4.0 <1.6.0 || 2.x', false],
  ],

  'empty ranges and empty sets': [
    ['5.0.0', '', true],
    ['0.0.0', '   ', true],
    ['5.0.0', '1.0.0 ||', true],
    ['5.0.0', '|| 1.0.0', true],
  ],

  'x-ranges and bare partial versions': [
    ['0.0.0', '*', true],
    ['99.99.99', '*', true],
    ['1.0.0', 'x', true],
    ['1.0.0', 'X', true],
    ['2.3.4', 'x.x', true],
    ['2.3.4', '*.*.*', true],
    ['1.0.0', '1', true],
    ['1.9.9', '1', true],
    ['2.0.0', '1', false],
    ['0.9.9', '1', false],
    ['1.4.0', '1.x', true],
    ['1.4.0', '1.X.x', true],
    ['1.4.0', '1.*', true],
    ['1.4.0', '1.*.*', true],
    ['2.0.0', '1.x.x', false],
    ['1.2.0', '1.2', true],
    ['1.2.9', '1.2.x', true],
    ['1.3.0', '1.2.*', false],
    ['1.1.9', '1.2', false],
    ['0.5.0', '0', true],
    ['1.0.0', '0.x', false],
    ['1.2.0', '=1.2', true],
    ['1.3.0', '=1.2', false],
    ['1.2.3', 'v1.2', true],
    ['1.9.0', '=1', true],
  ],

  'comparisons with partial versions': [
    ['1.3.0', '>1.2', true],
    ['1.2.9', '>1.2', false],
    ['2.0.0', '>1', true],
    ['1.9.9', '>1', false],
    ['2.0.0', '>1.x', true],
    ['1.9.0', '>1.x', false],
    ['1.2.0', '>=1.2', true],
    ['1.1.9', '>=1.2', false],
    ['1.0.0', '>=1', true],
    ['0.9.9', '>=1', false],
    ['1.1.9', '<1.2', true],
    ['1.2.0', '<1.2', false],
    ['0.9.9', '<1', true],
    ['1.0.0', '<1', false],
    ['1.2.9', '<=1.2', true],
    ['1.3.0', '<=1.2', false],
    ['1.9.9', '<=1', true],
    ['2.0.0', '<=1', false],
    ['1.2.9', '<=1.2.x', true],
    ['1.0.0', '>0.x', true],
    ['0.9.0', '>0.x', false],
    ['1.3.0', '> 1.2', true],
  ],

  'operators with a wildcard major': [
    ['1.0.0', '>*', false],
    ['0.0.0', '<*', false],
    ['1.0.0', '<x', false],
    ['1.0.0', '>=*', true],
    ['1.0.0', '<=x', true],
    ['1.0.0', '=*', true],
    ['1.0.0', '>x.x', false],
    ['1.0.0', '~X.*', true],
  ],

  'hyphen ranges': [
    ['1.2.3', '1.2.3 - 2.3.4', true],
    ['2.3.4', '1.2.3 - 2.3.4', true],
    ['2.3.5', '1.2.3 - 2.3.4', false],
    ['1.2.2', '1.2.3 - 2.3.4', false],
    ['2.3.4+build', '1.2.3 - 2.3.4', true],
    ['1.2.0', '1.2 - 2.3.4', true],
    ['1.1.9', '1.2 - 2.3.4', false],
    ['1.0.0', '1 - 2.3.4', true],
    ['2.3.9', '1.2.3 - 2.3', true],
    ['2.4.0', '1.2.3 - 2.3', false],
    ['2.9.9', '1.2.3 - 2', true],
    ['3.0.0', '1.2.3 - 2', false],
    ['2.99.0', '1.2.3 - 2.x', true],
    ['1.2.0', '1.2.x - 2.x', true],
    ['3.0.0', '1.2.x - 2.x', false],
    ['0.0.1', '* - 2.0.0', true],
    ['2.0.1', '* - 2.0.0', false],
    ['99.0.0', '1.2.3 - *', true],
    ['1.2.2', '1.2.3 - *', false],
    ['5.0.0', 'x - x', true],
    ['1.5.0', 'v1.2.3 - v2.3.4', true],
    ['1.5.0', '  1.2.3   -   2.3.4  ', true],
    ['1.5.0', '1.2.3 - 2.3.4 || 3.0.0', true],
    ['3.0.0', '1.2.3 - 2.3.4 || 3.0.0', true],
    ['2.5.0', '1.2.3 - 2.3.4 || 3.0.0', false],
    ['1.2.3-2.3.4', '1.2.3-2.3.4', true],
    ['2.0.0', '1.2.3-2.3.4', false],
    ['1.2.3', '1.2.3-2.3.4', false],
  ],

  'tilde ranges': [
    ['1.2.3', '~1.2.3', true],
    ['1.2.9', '~1.2.3', true],
    ['1.3.0', '~1.2.3', false],
    ['1.2.2', '~1.2.3', false],
    ['1.2.0', '~1.2', true],
    ['1.3.0', '~1.2', false],
    ['1.0.0', '~1', true],
    ['1.9.9', '~1', true],
    ['2.0.0', '~1', false],
    ['0.2.5', '~0.2.3', true],
    ['0.3.0', '~0.2.3', false],
    ['0.0.5', '~0.0.3', true],
    ['0.1.0', '~0.0.3', false],
    ['0.9.9', '~0', true],
    ['1.0.0', '~0', false],
    ['1.2.5', '~1.2.x', true],
    ['1.5.0', '~1.x', true],
    ['2.0.0', '~1.x', false],
    ['3.0.0', '~*', true],
    ['1.2.5', '~ 1.2.3', true],
    ['1.2.5', '~v1.2.3', true],
  ],

  'caret ranges': [
    ['1.2.3', '^1.2.3', true],
    ['1.9.9', '^1.2.3', true],
    ['2.0.0', '^1.2.3', false],
    ['1.2.2', '^1.2.3', false],
    ['0.2.3', '^0.2.3', true],
    ['0.2.9', '^0.2.3', true],
    ['0.3.0', '^0.2.3', false],
    ['0.2.2', '^0.2.3', false],
    ['0.0.3', '^0.0.3', true],
    ['0.0.4', '^0.0.3', false],
    ['1.0.0', '^1.x', true],
    ['1.9.0', '^1', true],
    ['2.0.0', '^1.x', false],
    ['0.0.0', '^0.x', true],
    ['0.9.9', '^0', true],
    ['1.0.0', '^0.x', false],
    ['0.0.9', '^0.0', true],
    ['0.1.0', '^0.0', false],
    ['0.0.5', '^0.0.x', true],
    ['0.1.0', '^0.0.x', false],
    ['0.2.0', '^0.2', true],
    ['0.2.9', '^0.2.x', true],
    ['0.3.0', '^0.2', false],
    ['1.2.0', '^1.2', true],
    ['1.9.0', '^1.2.x', true],
    ['1.1.0', '^1.2', false],
    ['2.0.0', '^1.2', false],
    ['0.0.0', '^0.0.0', true],
    ['0.0.1', '^0.0.0', false],
    ['5.0.0', '^*', true],
    ['1.5.0', '^ 1.2.3', true],
    ['1.5.0', '^v1.2.3', true],
    ['1.2.3', '^1.2.3+build', true],
  ],

  'prerelease versions': [
    ['1.0.0-beta', '*', false],
    ['1.0.0-beta', '', false],
    ['1.0.0-beta', '>=0.0.0-0', false],
    ['0.0.0-beta', '>=0.0.0-0', true],
    ['1.2.3-beta', '1.x', false],
    ['1.2.3-beta', '^1.0.0', false],
    ['1.2.3-beta', '~1.2.0', false],
    ['1.2.3-beta', '>=1.0.0', false],
    ['1.2.3-beta', '<1.2.3', false],
    ['1.2.3-beta', '1.2.3 - 2.0.0', false],
    ['1.2.3-beta', '1.2.3-beta', true],
    ['1.2.3-beta+exp.sha', '1.2.3-beta', true],
    ['1.2.3-beta.2', '>1.2.3-beta.1', true],
    ['1.2.3-beta.1', '>1.2.3-beta.1', false],
    ['1.2.3-beta.11', '>1.2.3-beta.2', true],
    ['1.2.3-beta', '>1.2.3-beta.2', false],
    ['1.2.3-1', '>1.2.3-alpha', false],
    ['1.2.3-beta.2', '>1.2.3-alpha <1.2.3', true],
    ['1.2.4-beta.2', '>1.2.3-alpha <1.3.0', false],
    ['1.2.3-rc.1', '>=1.2.3-beta <1.2.3', true],
    ['1.2.3-alpha', '>=1.2.3-beta <1.2.3', false],
    ['1.2.3-beta', '>=1.2.3-alpha >=1.0.0', true],
    ['1.2.3-beta', '1.x || 1.2.3-alpha', false],
    ['2.0.0-rc.1', '>=1.0.0 || 2.0.0-rc.5', false],
    ['1.2.3-beta', '<1.2.3 || >1.2.3-rc.1', false],
    ['1.2.3-rc.2', '<1.2.3 || >1.2.3-rc.1', true],
    ['1.2.3-beta', '1.x || >=1.2.3-alpha <1.2.4', true],
    ['1.2.3-beta', '>=1.2.3-alpha <1.2.4 || 2.x', true],
    ['1.2.3-beta.4', '~1.2.3-beta.2', true],
    ['1.2.3-beta.1', '~1.2.3-beta.2', false],
    ['1.2.4-beta', '~1.2.3-beta.2', false],
    ['1.2.4', '~1.2.3-beta.2', true],
    ['1.2.3', '~1.2.3-beta.2', true],
    ['1.2.3-beta.4', '^1.2.3-beta.2', true],
    ['1.2.4-beta.4', '^1.2.3-beta.2', false],
    ['1.2.4', '^1.2.3-beta.2', true],
    ['2.0.0-beta', '^1.2.3-beta.2', false],
    ['0.0.3-pr.2', '^0.0.3-beta', true],
    ['0.0.3', '^0.0.3-beta', true],
    ['0.0.4', '^0.0.3-beta', false],
    ['2.0.0-rc.1', '1.0.0 - 2.0.0-rc.2', true],
    ['2.0.0-rc.3', '1.0.0 - 2.0.0-rc.2', false],
    ['1.5.0-beta', '1.0.0 - 2.0.0-rc.2', false],
    ['1.2.3-beta', '1.2.3-alpha - 1.2.3', true],
  ],
};

for (const [group, rows] of Object.entries(TABLE)) {
  describe(`satisfies: ${group}`, () => {
    for (const [version, range, expected] of rows) {
      it(`${JSON.stringify(version)} ${expected ? 'satisfies' : 'does not satisfy'} ${JSON.stringify(range)}`, () => {
        assert.equal(satisfies(version, range), expected);
      });
    }
  });
}

describe('satisfies: arguments', () => {
  it('accepts SemVer instances', () => {
    assert.equal(satisfies(new SemVer('1.2.3'), '^1.0.0'), true);
    assert.equal(satisfies(new SemVer('2.0.0'), '^1.0.0'), false);
    assert.equal(satisfies(parse('2.0.0-rc.1'), '^2.0.0-rc.0'), true);
    assert.equal(satisfies(parse('2.1.0-rc.1'), '^2.0.0-rc.0'), false);
  });

  it('returns a boolean for every answer', () => {
    assert.equal(typeof satisfies('1.2.3', '1.x'), 'boolean');
    assert.equal(typeof satisfies('3.2.3', '1.x || 2.x'), 'boolean');
  });

  it('gives the same answer when called repeatedly', () => {
    for (let i = 0; i < 3; i++) {
      assert.equal(satisfies('1.2.3-beta', '>=1.2.3-alpha <1.2.3'), true);
      assert.equal(satisfies('1.3.0-beta', '>=1.2.3-alpha <1.4.0'), false);
    }
  });
});

const INVALID_RANGES = [
  '>=',
  '>=1.2.3 <',
  '^',
  '~',
  '-',
  'v',
  'latest',
  '1.2.3.4',
  'a.b.c',
  '1.2.3 || foo',
  '>=1.2.3<2.0.0',
  '>>1.2.3',
  '=>1.2.3',
  '~^1.2.3',
  '< 1.2.3 >',
  '1.2.3 - 2.3.4 - 3.0.0',
  '1.2.3 - 2.3.4 >1.5.0',
  '>=1.0.0 - 2.0.0',
  '1.2.3 -2.3.4',
  '1.2.3 - ',
  '01.2.3',
  '1.02',
  '1.2.3-01',
  '1.2.3-',
  '1.2.3+',
  '1.2-beta',
  '1.2.x-beta',
  '1.x.3',
  '*.1',
  'x.1.x',
];

describe('satisfies: invalid ranges', () => {
  for (const range of INVALID_RANGES) {
    it(`throws TypeError for ${JSON.stringify(range)}`, () => {
      assert.throws(() => satisfies('1.2.3', range), {
        name: 'TypeError',
        message: `Invalid range: ${range}`,
      });
    });
  }

  it('throws TypeError for a range that is not a string', () => {
    for (const range of [null, undefined, 123, ['1.x']]) {
      assert.throws(() => satisfies('1.2.3', range), { name: 'TypeError', message: /^Invalid range: / });
    }
  });
});

describe('satisfies: invalid versions', () => {
  for (const version of ['1.2', '1.x', '', 'not.a.version', '1.2.3.4', '01.2.3', '1.2.3-01']) {
    it(`throws TypeError for version ${JSON.stringify(version)}`, () => {
      assert.throws(() => satisfies(version, '*'), { name: 'TypeError', message: /^Invalid version/ });
      assert.throws(() => satisfies(version, '>=0.0.0'), { name: 'TypeError', message: /^Invalid version/ });
    });
  }

  it('throws TypeError for a version that is neither a string nor a SemVer', () => {
    assert.throws(() => satisfies(null, '*'), { name: 'TypeError', message: /^Invalid version/ });
    assert.throws(() => satisfies(123, ''), { name: 'TypeError', message: /^Invalid version/ });
  });
});

describe('maxSatisfying / minSatisfying', () => {
  const published = ['0.9.0', '1.0.0', '1.2.3', '1.2.4', '1.3.0-beta.1', '1.3.0', '2.0.0-rc.1', '2.0.0', '2.1.0'];

  it('pick the highest and lowest match', () => {
    assert.equal(maxSatisfying(published, '~1.2.0'), '1.2.4');
    assert.equal(minSatisfying(published, '~1.2.0'), '1.2.3');
    assert.equal(maxSatisfying(published, '^1.0.0'), '1.3.0');
    assert.equal(minSatisfying(published, '^1.0.0'), '1.0.0');
    assert.equal(maxSatisfying(published, '*'), '2.1.0');
    assert.equal(minSatisfying(published, '*'), '0.9.0');
  });

  it('handle partial and hyphen ranges', () => {
    assert.equal(maxSatisfying(published, '>1.2 <2'), '1.3.0');
    assert.equal(minSatisfying(published, '>1.2 <2'), '1.3.0');
    assert.equal(maxSatisfying(published, '1.0 - 1.2'), '1.2.4');
    assert.equal(maxSatisfying(published, '<=1.2'), '1.2.4');
    assert.equal(minSatisfying(published, '>=1'), '1.0.0');
  });

  it('only pick prereleases the range asks for', () => {
    assert.equal(maxSatisfying(published, '>=1.3.0-beta.0 <1.3.0'), '1.3.0-beta.1');
    assert.equal(minSatisfying(published, '>=1.3.0-beta.0'), '1.3.0-beta.1');
    assert.equal(minSatisfying(published, '>=1.2.5'), '1.3.0');
    assert.equal(minSatisfying(published, '^2.0.0-rc.0'), '2.0.0-rc.1');
    assert.equal(minSatisfying(published, '^2.0.0'), '2.0.0');
    assert.equal(maxSatisfying(published, '<2.0.0'), '1.3.0');
  });

  it('use every alternative of the range', () => {
    assert.equal(maxSatisfying(published, '0.x || 1.2.x'), '1.2.4');
    assert.equal(minSatisfying(published, '1.2.x || >=2.0.0'), '1.2.3');
    assert.equal(maxSatisfying(['3.0.0', '1.5.0', '2.5.0', '0.9.0'], '1.x || >=3.0.0 <3.1.0'), '3.0.0');
  });

  it('return null when nothing matches', () => {
    assert.equal(maxSatisfying(published, '^3.0.0'), null);
    assert.equal(minSatisfying(published, '>2.1.0'), null);
    assert.equal(maxSatisfying(['1.0.0-alpha', '1.0.0-beta'], '1.x'), null);
  });

  it('work on unsorted input with junk entries', () => {
    const messy = ['latest', '1.4.0', 'v1.10.0', '1.9.1', 'nightly', '1.2', '0.1.0'];
    assert.equal(maxSatisfying(messy, '^1.4.0'), 'v1.10.0');
    assert.equal(minSatisfying(messy, '^1.4.0'), '1.4.0');
    assert.equal(maxSatisfying(messy, '~1.9'), '1.9.1');
  });

  it('throw TypeError for an invalid range', () => {
    assert.throws(() => maxSatisfying(['1.0.0'], '>=1.0.0 <'), TypeError);
    assert.throws(() => minSatisfying(['1.0.0'], 'banana'), TypeError);
  });
});

describe('filterSatisfying', () => {
  it('keeps the matching entries in their original order', () => {
    const list = ['2.0.0', '1.2.3-rc.1', 'v1.2.3', 'nightly', '1.2.10', '1.3.0', '1.2.3-rc.2'];
    assert.deepEqual(filterSatisfying(list, '~1.2'), ['v1.2.3', '1.2.10']);
    assert.deepEqual(filterSatisfying(list, '~1.2.3-rc.2'), ['v1.2.3', '1.2.10', '1.2.3-rc.2']);
    assert.deepEqual(filterSatisfying(list, '>=1.2.3-rc.0 <1.2.3 || >=2'), ['2.0.0', '1.2.3-rc.1', '1.2.3-rc.2']);
  });
});

function exec(...argv) {
  let out = '';
  let err = '';
  const code = run(argv, { out: (s) => (out += s), err: (s) => (err += s) });
  return { code, out, err };
}

describe('pkgver cli with ranges', () => {
  it('satisfies prints the answer and exits 0 or 1', () => {
    assert.deepEqual(exec('satisfies', '1.4.0', '^1.2'), { code: 0, out: 'true\n', err: '' });
    assert.deepEqual(exec('satisfies', '1.4.0-rc.1', '^1.2'), { code: 1, out: 'false\n', err: '' });
  });

  it('satisfies reports an invalid range or version and exits 2', () => {
    assert.deepEqual(exec('satisfies', '1.2.3', '>=1.2.3 <'), {
      code: 2,
      out: '',
      err: 'pkgver: Invalid range: >=1.2.3 <\n',
    });
    const r = exec('satisfies', '1.2', '*');
    assert.equal(r.code, 2);
    assert.equal(r.out, '');
    assert.match(r.err, /^pkgver: Invalid version/);
  });

  it('max and min pick from the listed versions', () => {
    const list = ['1.2.3', '1.2.4', '1.3.0-beta.1', '1.3.0', '2.0.0'];
    assert.deepEqual(exec('max', '1.2.3 - 1.3', ...list), { code: 0, out: '1.3.0\n', err: '' });
    assert.deepEqual(exec('min', '>1.2', ...list), { code: 0, out: '1.3.0\n', err: '' });
    assert.deepEqual(exec('max', '~1.2.3', ...list), { code: 0, out: '1.2.4\n', err: '' });
    assert.deepEqual(exec('max', '^3', ...list), { code: 1, out: '', err: '' });
  });
});
