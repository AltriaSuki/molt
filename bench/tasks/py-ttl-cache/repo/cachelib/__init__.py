"""cachelib: a small in-process LRU cache and a memoize decorator built on it.

    >>> from cachelib import LRUCache
    >>> cache = LRUCache(2)
    >>> cache.put("a", 1)
    >>> cache.get("a")
    1
"""

from .lru import LRUCache
from .memo import make_key, memoize
from .stats import COUNTERS, CacheStats, format_stats, hit_rate, merge_stats

__all__ = [
    "COUNTERS",
    "CacheStats",
    "LRUCache",
    "format_stats",
    "hit_rate",
    "make_key",
    "memoize",
    "merge_stats",
]

__version__ = "0.4.1"
