# cachelib

A small in-process cache library: `LRUCache`, a fixed-capacity mapping that
evicts the least recently used entry and can expire entries after a time to
live, a `memoize` decorator built on it, and helpers for reading cache
statistics. Standard library only.

```python
from cachelib import LRUCache, memoize, format_stats

cache = LRUCache(128, ttl=60)       # entries expire after 60 seconds
cache.put("user:42", {"name": "Ada"})
cache.put("config", load_config(), ttl=None)   # this one never expires
cache.get("user:42")            # {'name': 'Ada'}
cache.get("user:7", "unknown")  # 'unknown'
print(format_stats(cache.stats()))
# hits=1 misses=1 evictions=0 expirations=0 hit_rate=50.0%

@memoize(maxsize=1024, ttl=30)
def lookup(user_id):
    ...
```

## Modules

- `cachelib/lru.py`: `LRUCache(capacity, ttl=None, clock=time.monotonic)` with
  `get(key, default=None)`, `put(key, value, ttl=...)`, `len()`, `in`,
  `keys()` (least recently used first) and `stats()`. `get` and `put` mark a
  key as most recently used; `in`, `len()` and `keys()` don't. An entry put
  at time `t` with ttl `T` expires at `t + T` and from then on behaves like a
  missing key. Leaving out `put`'s `ttl` uses the cache's ttl; `ttl=None`
  means the entry never expires. Adding a key to a full cache drops expired
  entries before it evicts a live one.
- `cachelib/memo.py`: `memoize`, usable bare (`@memoize`) or with options
  (`@memoize(maxsize=64, ttl=30)`). The decorated function has `cache`,
  `cache_info()` and `cache_clear()`. `make_key` builds the cache key for a
  call.
- `cachelib/stats.py`: `CacheStats`, the counters a cache keeps (hits,
  misses, evictions, expirations), and helpers that work on the dicts
  `stats()` returns: `hit_rate`, `merge_stats` and `format_stats`.

## Running the tests

Python 3.11 or newer, no dependencies:

```
python3 -m unittest -q tests.test_lru tests.test_memo tests.test_stats
```
