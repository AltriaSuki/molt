"""A fixed-capacity cache that evicts the least recently used entry."""

from collections import OrderedDict

from .stats import CacheStats


class LRUCache:
    """Map keys to values, holding at most ``capacity`` entries.

    ``get`` and ``put`` count as a use of a key. When ``put`` adds a new key
    to a full cache, the least recently used entry is evicted to make room.
    ``in``, ``len()`` and ``keys()`` only look: they change neither recency
    nor the hit and miss counters.

    Keys must be hashable. The cache is not thread-safe.
    """

    def __init__(self, capacity):
        if isinstance(capacity, bool) or not isinstance(capacity, int):
            raise TypeError(
                f"capacity must be an int, not {type(capacity).__name__}"
            )
        if capacity < 1:
            raise ValueError(f"capacity must be at least 1, got {capacity}")
        self._capacity = capacity
        # Ordered from least to most recently used.
        self._entries = OrderedDict()
        self._stats = CacheStats()

    @property
    def capacity(self):
        """The most entries the cache will hold."""
        return self._capacity

    def get(self, key, default=None):
        """Return the value cached for ``key`` and mark it most recently used.

        Returns ``default`` when the key is not in the cache. Every call
        counts as either a hit or a miss.
        """
        try:
            value = self._entries[key]
        except KeyError:
            self._stats.misses += 1
            return default
        self._entries.move_to_end(key)
        self._stats.hits += 1
        return value

    def put(self, key, value):
        """Cache ``value`` under ``key`` as the most recently used entry.

        Putting a key that is already cached replaces its value. Putting a
        new key into a full cache evicts the least recently used entry.
        """
        if key in self._entries:
            self._entries[key] = value
            self._entries.move_to_end(key)
            return
        if len(self._entries) >= self._capacity:
            self._evict_lru()
        self._entries[key] = value

    def keys(self):
        """Return the cached keys as a list, least recently used first."""
        return list(self._entries)

    def stats(self):
        """Return a snapshot of the counters: hits, misses and evictions."""
        return self._stats.as_dict()

    def __len__(self):
        return len(self._entries)

    def __contains__(self, key):
        return key in self._entries

    def _evict_lru(self):
        self._entries.popitem(last=False)
        self._stats.evictions += 1
