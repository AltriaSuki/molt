"""Cache statistics: the counters a cache keeps, and helpers for reading them.

A cache hands its counters out as a plain dict (``LRUCache.stats()``), so they
can be logged, merged across caches or compared in tests without holding on
to the cache itself. The helpers in this module all work on those dicts.
"""

from dataclasses import dataclass

# The counters every stats dict carries, in display order.
COUNTERS = ("hits", "misses", "evictions")


@dataclass
class CacheStats:
    """Running counters for one cache.

    ``hits`` and ``misses`` count lookups that found / did not find a value.
    ``evictions`` counts entries removed to make room for new ones.
    """

    hits: int = 0
    misses: int = 0
    evictions: int = 0

    def as_dict(self):
        """Return the counters as a new ``{name: value}`` dict."""
        return {name: getattr(self, name) for name in COUNTERS}


def hit_rate(stats):
    """Return the fraction of lookups that were hits, between 0.0 and 1.0.

    A cache that has seen no lookups has a hit rate of 0.0.
    """
    lookups = stats["hits"] + stats["misses"]
    if not lookups:
        return 0.0
    return stats["hits"] / lookups


def merge_stats(*snapshots):
    """Add up several stats dicts, e.g. the caches of several workers.

    With no arguments, returns every counter at zero.
    """
    total = dict.fromkeys(COUNTERS, 0)
    for snapshot in snapshots:
        for name in COUNTERS:
            total[name] += snapshot[name]
    return total


def format_stats(stats):
    """Render a stats dict as one log-friendly line.

        >>> format_stats({"hits": 3, "misses": 1, "evictions": 0})
        'hits=3 misses=1 evictions=0 hit_rate=75.0%'
    """
    return (
        f"hits={stats['hits']} misses={stats['misses']} "
        f"evictions={stats['evictions']} hit_rate={hit_rate(stats):.1%}"
    )
