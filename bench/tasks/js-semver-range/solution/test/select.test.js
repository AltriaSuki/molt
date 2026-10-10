import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

import { filterSatisfying, maxSatisfying, minSatisfying } from '../src/select.js';

describe('maxSatisfying / minSatisfying', () => {
  it('return null for an empty list', () => {
    assert.equal(maxSatisfying([], '^1.0.0'), null);
    assert.equal(minSatisfying([], '^1.0.0'), null);
    assert.deepEqual(filterSatisfying([], '^1.0.0'), []);
  });

  it('skip entries that are not versions', () => {
    const tags = ['latest', 'next', '1.0', 'nightly-2024-03-01'];
    assert.equal(maxSatisfying(tags, '*'), null);
    assert.equal(minSatisfying(tags, '*'), null);
    assert.deepEqual(filterSatisfying(tags, '*'), []);
  });

  it('pick the highest and lowest match', () => {
    const published = ['1.2.3', '1.2.4', '1.3.0', '2.0.0'];
    assert.equal(maxSatisfying(published, '~1.2.0'), '1.2.4');
    assert.equal(minSatisfying(published, '^1.2.4'), '1.2.4');
    assert.deepEqual(filterSatisfying(published, '1.x'), ['1.2.3', '1.2.4', '1.3.0']);
  });

  it('leave prereleases out unless the range names one', () => {
    const published = ['1.0.0', '1.1.0-rc.1', '1.1.0'];
    assert.deepEqual(filterSatisfying(published, '^1.0.0'), ['1.0.0', '1.1.0']);
    assert.deepEqual(filterSatisfying(published, '>=1.1.0-rc.0'), ['1.1.0-rc.1', '1.1.0']);
  });
});
