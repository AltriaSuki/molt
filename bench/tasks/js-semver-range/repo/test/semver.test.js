import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import {
  SemVer,
  parse,
  valid,
  compare,
  rcompare,
  eq,
  neq,
  gt,
  gte,
  lt,
  lte,
  sort,
  rsort,
  inc,
} from '../src/semver.js';

describe('SemVer', () => {
  it('parses the parts of a full version', () => {
    const v = new SemVer('1.22.333-beta.7.x-y+build.5.sha-1');
    assert.equal(v.major, 1);
    assert.equal(v.minor, 22);
    assert.equal(v.patch, 333);
    assert.deepEqual(v.prerelease, ['beta', 7, 'x-y']);
    assert.deepEqual(v.build, ['build', '5', 'sha-1']);
    assert.equal(v.version, '1.22.333-beta.7.x-y');
    assert.equal(String(v), '1.22.333-beta.7.x-y');
  });

  it('accepts a leading v and surrounding whitespace', () => {
    assert.equal(new SemVer('v1.2.3').version, '1.2.3');
    assert.equal(new SemVer('  1.2.3\n').version, '1.2.3');
  });

  it('copies another SemVer', () => {
    const a = new SemVer('1.2.3-rc.1');
    const b = new SemVer(a);
    assert.notEqual(a, b);
    assert.equal(b.version, '1.2.3-rc.1');
    b.prerelease.push('x');
    assert.deepEqual(a.prerelease, ['rc', 1]);
  });

  it('rejects invalid versions with a TypeError', () => {
    for (const bad of [
      '',
      '1',
      '1.2',
      '1.2.3.4',
      '01.2.3',
      '1.02.3',
      '1.2.03',
      '1.2.3-',
      '1.2.3-01',
      '1.2.3-beta..1',
      '1.2.3+',
      '1.2.3+a..b',
      'a.b.c',
      '1.2.x',
      '=1.2.3',
      'V1.2.3',
      '99999999999999999999.0.0',
    ]) {
      assert.throws(() => new SemVer(bad), { name: 'TypeError', message: /^Invalid version/ }, bad);
    }
    assert.throws(() => new SemVer(null), TypeError);
    assert.throws(() => new SemVer(123), TypeError);
  });
});

describe('parse and valid', () => {
  it('parse returns null instead of throwing', () => {
    assert.equal(parse('nope'), null);
    assert.equal(parse(undefined), null);
    assert.equal(parse('1.2.3').patch, 3);
  });

  it('parse returns a SemVer instance unchanged', () => {
    const v = new SemVer('2.0.0');
    assert.equal(parse(v), v);
  });

  it('valid normalizes', () => {
    assert.equal(valid('v1.2.3+build'), '1.2.3');
    assert.equal(valid('1.2.3-alpha.1'), '1.2.3-alpha.1');
    assert.equal(valid('1.2'), null);
  });
});

describe('precedence', () => {
  it('compares major, minor and patch numerically', () => {
    assert.equal(compare('1.2.3', '1.2.3'), 0);
    assert.equal(compare('1.2.3', '1.2.4'), -1);
    assert.equal(compare('1.10.0', '1.9.0'), 1);
    assert.equal(compare('2.0.0', '10.0.0'), -1);
  });

  it('orders prereleases as in the SemVer 2.0.0 spec', () => {
    const ordered = [
      '1.0.0-alpha',
      '1.0.0-alpha.1',
      '1.0.0-alpha.beta',
      '1.0.0-beta',
      '1.0.0-beta.2',
      '1.0.0-beta.11',
      '1.0.0-rc.1',
      '1.0.0',
    ];
    for (let i = 0; i < ordered.length - 1; i++) {
      assert.equal(compare(ordered[i], ordered[i + 1]), -1, `${ordered[i]} < ${ordered[i + 1]}`);
      assert.equal(compare(ordered[i + 1], ordered[i]), 1, `${ordered[i + 1]} > ${ordered[i]}`);
    }
  });

  it('ranks numeric identifiers below alphanumeric ones', () => {
    assert.equal(compare('1.0.0-1', '1.0.0-a'), -1);
    assert.equal(compare('1.0.0-99', '1.0.0-0a'), -1);
  });

  it('ignores build metadata', () => {
    assert.equal(compare('1.0.0+a', '1.0.0+b'), 0);
    assert.ok(eq('1.0.0-rc.1+x', '1.0.0-rc.1'));
  });

  it('has the usual comparison helpers', () => {
    assert.ok(gt('1.0.1', '1.0.0'));
    assert.ok(gte('1.0.0', '1.0.0'));
    assert.ok(lt('1.0.0-rc.1', '1.0.0'));
    assert.ok(lte('1.0.0', '1.0.0'));
    assert.ok(neq('1.0.0', '1.0.1'));
    assert.equal(rcompare('1.0.0', '2.0.0'), 1);
  });

  it('throws on invalid input', () => {
    assert.throws(() => compare('1.0', '1.0.0'), TypeError);
  });
});

describe('sort', () => {
  it('sorts ascending and descending without mutating the input', () => {
    const input = ['1.10.0', '1.2.0', '1.2.0-rc.1', '0.9.9', '1.2.0+b', '1.2.0+a'];
    assert.deepEqual(sort(input), ['0.9.9', '1.2.0-rc.1', '1.2.0', '1.2.0+a', '1.2.0+b', '1.10.0']);
    assert.deepEqual(rsort(input), ['1.10.0', '1.2.0+b', '1.2.0+a', '1.2.0', '1.2.0-rc.1', '0.9.9']);
    assert.deepEqual(input, ['1.10.0', '1.2.0', '1.2.0-rc.1', '0.9.9', '1.2.0+b', '1.2.0+a']);
  });
});

describe('inc', () => {
  it('bumps major, minor and patch', () => {
    assert.equal(inc('1.2.3', 'major'), '2.0.0');
    assert.equal(inc('1.2.3', 'minor'), '1.3.0');
    assert.equal(inc('1.2.3', 'patch'), '1.2.4');
    assert.equal(inc('1.2.3+build', 'patch'), '1.2.4');
  });

  it('finishes a prerelease instead of skipping a version', () => {
    assert.equal(inc('2.0.0-rc.1', 'major'), '2.0.0');
    assert.equal(inc('1.3.0-rc.1', 'minor'), '1.3.0');
    assert.equal(inc('1.2.4-rc.1', 'patch'), '1.2.4');
    assert.equal(inc('1.2.4-rc.1', 'major'), '2.0.0');
  });

  it('bumps prereleases', () => {
    assert.equal(inc('1.2.3', 'prerelease'), '1.2.4-0');
    assert.equal(inc('1.2.3', 'prerelease', 'rc'), '1.2.4-rc.0');
    assert.equal(inc('1.2.4-rc.0', 'prerelease'), '1.2.4-rc.1');
    assert.equal(inc('1.2.4-rc.0', 'prerelease', 'rc'), '1.2.4-rc.1');
    assert.equal(inc('1.2.4-beta.3', 'prerelease', 'rc'), '1.2.4-rc.0');
    assert.equal(inc('1.2.4-beta', 'prerelease'), '1.2.4-beta.0');
  });

  it('returns null for an invalid version and throws on an unknown release', () => {
    assert.equal(inc('1.2', 'patch'), null);
    assert.throws(() => inc('1.2.3', 'micro'), TypeError);
  });
});
