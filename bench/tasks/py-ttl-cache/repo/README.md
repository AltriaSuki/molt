# cachelib

A small in-process cache library: `LRUCache`, a fixed-capacity mapping that
evicts the least recently used entry, a `memoize` decorator built on it, and
helpers for reading cache statistics. Standard library only.

```python
from cachelib import LRUCache, memoize, format_stats

cache = LRUCache(128)
cache.put("user:42", {"name": "Ada"})
cache.get("user:42")            # {'name': 'Ada'}
cache.get("user:7", "unknown")  # 'unknown'
print(format_stats(cache.stats()))
# hits=1 misses=1 evictions=0 hit_rate=50.0%

@memoize(maxsize=1024)
def lookup(user_id):
    ...
```

## Modules

- `cachelib/lru.py`: `LRUCache(capacity)` with `get(key, default=None)`,
  `put(key, value)`, `len()`, `in`, `keys()` (least recently used first) and
  `stats()`. `get` and `put` mark a key as most recently used; `in`, `len()`
  and `keys()` don't.
- `cachelib/memo.py`: `memoize`, usable bare (`@memoize`) or with options
  (`@memoize(maxsize=64)`). The decorated function has `cache`,
  `cache_info()` and `cache_clear()`. `make_key` builds the cache key for a
  call.
- `cachelib/stats.py`: `CacheStats`, the counters a cache keeps, and helpers
  that work on the dicts `stats()` returns: `hit_rate`, `merge_stats` and
  `format_stats`.

## Running the tests

Python 3.11 or newer, no dependencies:

```
python3 -m unittest -q tests.test_lru tests.test_memo tests.test_stats
```
