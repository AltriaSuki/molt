"""A fixed-capacity cache that evicts the least recently used entry, with
optional expiry."""

import time
from collections import OrderedDict

from .stats import CacheStats

# put()'s default for ``ttl``: use the cache's ttl. (None means "never".)
_CACHE_TTL = object()


def _check_ttl(ttl):
    if ttl is not None and ttl <= 0:
        raise ValueError(f"ttl must be a number of seconds > 0, or None; got {ttl!r}")
    return ttl


class _Entry:
    __slots__ = ("value", "expires_at")

    def __init__(self, value, expires_at):
        self.value = value
        self.expires_at = expires_at  # None: never expires

    def expired(self, now):
        return self.expires_at is not None and now >= self.expires_at


class LRUCache:
    """Map keys to values, holding at most ``capacity`` entries.

    ``get`` and ``put`` count as a use of a key. When ``put`` adds a new key
    to a full cache, expired entries are dropped first, and only if the cache
    is still full is the least recently used entry evicted. ``in``, ``len()``
    and ``keys()`` only look: they change neither recency nor the hit and
    miss counters.

    An entry put at time ``t`` with a ttl of ``T`` seconds expires at
    ``t + T``; from then on it behaves exactly like a missing key. ``ttl``
    is the default for every ``put``; ``None`` means entries never expire.
    ``clock`` returns the current time in seconds.

    Keys must be hashable. The cache is not thread-safe.
    """

    def __init__(self, capacity, ttl=None, clock=time.monotonic):
        if isinstance(capacity, bool) or not isinstance(capacity, int):
            raise TypeError(
                f"capacity must be an int, not {type(capacity).__name__}"
            )
        if capacity < 1:
            raise ValueError(f"capacity must be at least 1, got {capacity}")
        self._capacity = capacity
        self._ttl = _check_ttl(ttl)
        self._clock = clock
        # Ordered from least to most recently used. Expired entries may
        # linger here until something notices them.
        self._entries = OrderedDict()
        self._stats = CacheStats()

    @property
    def capacity(self):
        """The most entries the cache will hold."""
        return self._capacity

    @property
    def ttl(self):
        """The default time to live of an entry in seconds, or None."""
        return self._ttl

    def get(self, key, default=None):
        """Return the value cached for ``key`` and mark it most recently used.

        Returns ``default`` when the key is not in the cache or has expired.
        Every call counts as either a hit or a miss. A hit does not extend
        the entry's expiry.
        """
        entry = self._live_entry(key)
        if entry is None:
            self._stats.misses += 1
            return default
        self._entries.move_to_end(key)
        self._stats.hits += 1
        return entry.value

    def put(self, key, value, ttl=_CACHE_TTL):
        """Cache ``value`` under ``key`` as the most recently used entry.

        ``ttl`` is this entry's time to live in seconds: left out, it is the
        cache's ttl; ``None`` means the entry never expires. Putting a key
        that is already cached replaces its value and restarts its expiry.
        """
        ttl = self._ttl if ttl is _CACHE_TTL else _check_ttl(ttl)
        now = self._clock()
        entry = _Entry(value, None if ttl is None else now + ttl)
        old = self._entries.get(key)
        if old is not None:
            if old.expired(now):
                self._stats.expirations += 1
            self._entries[key] = entry
            self._entries.move_to_end(key)
            return
        if len(self._entries) >= self._capacity:
            self._drop_expired(now)
            if len(self._entries) >= self._capacity:
                self._evict_lru()
        self._entries[key] = entry

    def keys(self):
        """Return the live keys as a list, least recently used first."""
        self._drop_expired(self._clock())
        return list(self._entries)

    def stats(self):
        """Return a snapshot of the counters: hits, misses, evictions and
        expirations."""
        self._drop_expired(self._clock())
        return self._stats.as_dict()

    def __len__(self):
        self._drop_expired(self._clock())
        return len(self._entries)

    def __contains__(self, key):
        return self._live_entry(key) is not None

    def _live_entry(self, key):
        """Return the entry for ``key``, or None if it is absent or expired.

        An expired entry is dropped on the way.
        """
        entry = self._entries.get(key)
        if entry is not None and entry.expired(self._clock()):
            del self._entries[key]
            self._stats.expirations += 1
            return None
        return entry

    def _drop_expired(self, now):
        expired = [key for key, entry in self._entries.items() if entry.expired(now)]
        for key in expired:
            del self._entries[key]
        self._stats.expirations += len(expired)

    def _evict_lru(self):
        self._entries.popitem(last=False)
        self._stats.evictions += 1
