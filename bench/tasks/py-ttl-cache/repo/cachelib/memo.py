"""``memoize``: remember a function's results in an LRUCache."""

import functools

from .lru import LRUCache

_MISSING = object()
# Separates positional from keyword arguments in a cache key, so that
# f(1, 2) and f(1, b=2) never share an entry.
_KWARGS_MARK = ("<kwargs>",)


def make_key(args, kwargs):
    """Build the cache key for one call from its arguments.

    Keyword arguments are sorted by name, so ``f(a=1, b=2)`` and
    ``f(b=2, a=1)`` share an entry. Every argument must be hashable; the
    cache raises ``TypeError`` for a key that is not.
    """
    if not kwargs:
        return args
    return args + _KWARGS_MARK + tuple(sorted(kwargs.items()))


def memoize(func=None, *, maxsize=128):
    """Cache the results of ``func``, keeping the ``maxsize`` most recent.

    Use it bare or with options::

        @memoize
        def load(path): ...

        @memoize(maxsize=1024)
        def lookup(user_id): ...

    The decorated function gets three extras: ``cache`` (the LRUCache in
    use), ``cache_info()`` (its stats dict) and ``cache_clear()``, which
    starts over with an empty cache and fresh counters. Calls that raise are
    not cached.
    """
    if func is not None and not callable(func):
        raise TypeError(
            "memoize() takes options as keywords, e.g. @memoize(maxsize=64)"
        )

    def decorate(fn):
        @functools.wraps(fn)
        def wrapper(*args, **kwargs):
            key = make_key(args, kwargs)
            result = wrapper.cache.get(key, _MISSING)
            if result is _MISSING:
                result = fn(*args, **kwargs)
                wrapper.cache.put(key, result)
            return result

        def cache_info():
            return wrapper.cache.stats()

        def cache_clear():
            wrapper.cache = LRUCache(maxsize)

        wrapper.cache = LRUCache(maxsize)
        wrapper.cache_info = cache_info
        wrapper.cache_clear = cache_clear
        return wrapper

    if func is not None:
        return decorate(func)
    return decorate
