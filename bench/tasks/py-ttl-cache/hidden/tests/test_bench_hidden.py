import unittest

from cachelib import CacheStats, LRUCache, format_stats, memoize, merge_stats


class FakeClock:
    """A clock the test moves by hand. Times are exact binary fractions."""

    def __init__(self, start=1000.0):
        self.now = start

    def __call__(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds


def counters(hits=0, misses=0, evictions=0, expirations=0):
    return {
        "hits": hits,
        "misses": misses,
        "evictions": evictions,
        "expirations": expirations,
    }


class TtlConstructionTests(unittest.TestCase):
    def test_ttl_defaults_to_none(self):
        self.assertIsNone(LRUCache(2).ttl)

    def test_ttl_property(self):
        self.assertEqual(LRUCache(2, ttl=5).ttl, 5)
        self.assertEqual(LRUCache(2, ttl=0.5).ttl, 0.5)
        self.assertIsNone(LRUCache(2, ttl=None).ttl)

    def test_ttl_must_be_positive(self):
        for bad in (0, 0.0, -1, -0.5):
            with self.subTest(ttl=bad):
                with self.assertRaises(ValueError):
                    LRUCache(2, ttl=bad)

    def test_positional_ttl_and_clock(self):
        clock = FakeClock()
        cache = LRUCache(2, 10, clock)
        cache.put("a", 1)
        clock.advance(10)
        self.assertNotIn("a", cache)

    def test_default_clock_works(self):
        cache = LRUCache(2, ttl=3600)
        cache.put("a", 1)
        self.assertEqual(cache.get("a"), 1)
        self.assertIn("a", cache)
        self.assertEqual(len(cache), 1)

    def test_no_ttl_means_never_expire(self):
        clock = FakeClock()
        cache = LRUCache(2, clock=clock)
        cache.put("a", 1)
        clock.advance(10**9)
        self.assertEqual(cache.get("a"), 1)
        self.assertEqual(cache.keys(), ["a"])
        self.assertEqual(cache.stats(), counters(hits=1))


class ExpiryTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.cache = LRUCache(4, ttl=10, clock=self.clock)

    def test_live_until_exactly_ttl(self):
        self.cache.put("a", 1)
        self.clock.advance(9.5)
        self.assertEqual(self.cache.get("a"), 1)
        self.clock.advance(0.5)
        self.assertIsNone(self.cache.get("a"))

    def test_expired_get_returns_given_default(self):
        self.cache.put("a", 1)
        self.clock.advance(10)
        self.assertEqual(self.cache.get("a", "fallback"), "fallback")

    def test_expired_entry_not_in_cache(self):
        self.cache.put("a", 1)
        self.clock.advance(9.75)
        self.assertIn("a", self.cache)
        self.clock.advance(0.25)
        self.assertNotIn("a", self.cache)

    def test_len_and_keys_skip_expired_entries(self):
        self.cache.put("a", 1)
        self.clock.advance(4)
        self.cache.put("b", 2)
        self.cache.put("c", 3, ttl=None)
        self.clock.advance(6)  # "a" expires now; "b" at +14
        self.assertEqual(len(self.cache), 2)
        self.assertEqual(self.cache.keys(), ["b", "c"])
        self.clock.advance(4)
        self.assertEqual(len(self.cache), 1)
        self.assertEqual(self.cache.keys(), ["c"])

    def test_expired_entry_stays_gone(self):
        self.cache.put("a", 1)
        self.clock.advance(10)
        self.assertIsNone(self.cache.get("a"))
        self.assertNotIn("a", self.cache)
        self.assertEqual(len(self.cache), 0)
        self.assertEqual(self.cache.keys(), [])
        self.assertIsNone(self.cache.get("a"))

    def test_get_refreshes_recency_not_expiry(self):
        self.cache.put("a", 1)
        self.cache.put("b", 2)
        self.clock.advance(5)
        self.assertEqual(self.cache.get("a"), 1)
        self.assertEqual(self.cache.keys(), ["b", "a"])
        self.clock.advance(5)
        self.assertIsNone(self.cache.get("a"))
        self.assertEqual(self.cache.keys(), [])


class PerEntryTtlTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()

    def test_shorter_ttl_than_cache(self):
        cache = LRUCache(4, ttl=10, clock=self.clock)
        cache.put("a", 1, ttl=2)
        self.clock.advance(2)
        self.assertNotIn("a", cache)

    def test_longer_ttl_than_cache(self):
        cache = LRUCache(4, ttl=10, clock=self.clock)
        cache.put("a", 1, ttl=30)
        self.clock.advance(29)
        self.assertEqual(cache.get("a"), 1)
        self.clock.advance(1)
        self.assertIsNone(cache.get("a"))

    def test_ttl_none_never_expires_in_a_ttl_cache(self):
        cache = LRUCache(4, ttl=10, clock=self.clock)
        cache.put("a", 1, ttl=None)
        cache.put("b", 2)
        self.clock.advance(10**6)
        self.assertEqual(cache.keys(), ["a"])
        self.assertEqual(cache.get("a"), 1)

    def test_entry_ttl_in_a_cache_without_ttl(self):
        cache = LRUCache(4, clock=self.clock)
        cache.put("a", 1, ttl=3)
        cache.put("b", 2)
        self.clock.advance(3)
        self.assertEqual(cache.keys(), ["b"])
        self.assertIsNone(cache.get("a"))

    def test_put_rejects_non_positive_ttl(self):
        cache = LRUCache(4, ttl=10, clock=self.clock)
        for bad in (0, -1, -2.5):
            with self.subTest(ttl=bad):
                with self.assertRaises(ValueError):
                    cache.put("new", 1, ttl=bad)
                self.assertNotIn("new", cache)
        self.assertEqual(len(cache), 0)
        self.assertEqual(cache.stats(), counters())

    def test_rejected_put_keeps_existing_value(self):
        cache = LRUCache(4, ttl=10, clock=self.clock)
        cache.put("a", 1)
        with self.assertRaises(ValueError):
            cache.put("a", 2, ttl=0)
        self.assertEqual(cache.get("a"), 1)


class OverwriteTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.cache = LRUCache(4, ttl=10, clock=self.clock)

    def test_put_restarts_expiry(self):
        self.cache.put("a", 1)
        self.clock.advance(8)
        self.cache.put("a", 2)
        self.clock.advance(9)  # 17s after the first put, 9s after the second
        self.assertEqual(self.cache.get("a"), 2)
        self.clock.advance(1)
        self.assertNotIn("a", self.cache)

    def test_put_makes_key_most_recent(self):
        self.cache.put("a", 1)
        self.cache.put("b", 2)
        self.clock.advance(3)
        self.cache.put("a", 3)
        self.assertEqual(self.cache.keys(), ["b", "a"])

    def test_omitted_ttl_on_overwrite_uses_cache_ttl(self):
        self.cache.put("a", 1, ttl=None)
        self.cache.put("a", 2)
        self.clock.advance(10)
        self.assertNotIn("a", self.cache)

    def test_omitted_ttl_on_overwrite_drops_entry_ttl(self):
        self.cache.put("a", 1, ttl=100)
        self.cache.put("a", 2)
        self.clock.advance(10)
        self.assertIsNone(self.cache.get("a"))

    def test_omitted_ttl_on_overwrite_in_cache_without_ttl(self):
        cache = LRUCache(4, clock=self.clock)
        cache.put("a", 1, ttl=5)
        cache.put("a", 2)
        self.clock.advance(1000)
        self.assertEqual(cache.get("a"), 2)

    def test_overwrite_with_none_removes_expiry(self):
        self.cache.put("a", 1)
        self.clock.advance(5)
        self.cache.put("a", 2, ttl=None)
        self.clock.advance(1000)
        self.assertEqual(self.cache.get("a"), 2)

    def test_put_on_expired_key_starts_fresh(self):
        self.cache.put("a", 1)
        self.cache.put("b", 2, ttl=None)
        self.clock.advance(12)
        self.cache.put("a", 3)
        self.assertEqual(self.cache.keys(), ["b", "a"])
        self.clock.advance(9)
        self.assertEqual(self.cache.get("a"), 3)
        self.assertEqual(self.cache.stats(), counters(hits=1, expirations=1))


class FullCacheTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()

    def test_expired_lru_entry_is_dropped_not_evicted(self):
        cache = LRUCache(3, clock=self.clock)
        cache.put("a", 1, ttl=5)
        cache.put("b", 2)
        cache.put("c", 3)
        self.clock.advance(6)
        cache.put("d", 4)
        self.assertEqual(cache.keys(), ["b", "c", "d"])
        self.assertEqual(cache.stats(), counters(expirations=1))

    def test_expired_entry_makes_room_before_live_lru(self):
        cache = LRUCache(3, clock=self.clock)
        cache.put("a", 1)
        cache.put("b", 2, ttl=5)
        cache.put("c", 3)
        self.clock.advance(5)
        cache.put("d", 4)
        self.assertEqual(cache.keys(), ["a", "c", "d"])
        self.assertEqual(cache.get("a"), 1)
        self.assertEqual(cache.stats(), counters(hits=1, expirations=1))

    def test_most_recent_entry_expired(self):
        cache = LRUCache(3, clock=self.clock)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("c", 3, ttl=1)
        self.clock.advance(1)
        cache.put("d", 4)
        self.assertEqual(cache.keys(), ["a", "b", "d"])
        self.assertEqual(cache.stats()["evictions"], 0)

    def test_all_expired_entries_are_dropped(self):
        cache = LRUCache(3, ttl=5, clock=self.clock)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("c", 3)
        self.clock.advance(5)
        cache.put("d", 4)
        self.assertEqual(cache.keys(), ["d"])
        self.assertEqual(len(cache), 1)
        self.assertEqual(cache.stats(), counters(expirations=3))

    def test_live_lru_evicted_when_nothing_expired(self):
        cache = LRUCache(2, ttl=10, clock=self.clock)
        cache.put("a", 1)
        self.clock.advance(1)
        cache.put("b", 2)
        self.clock.advance(1)
        cache.put("c", 3)
        self.assertEqual(cache.keys(), ["b", "c"])
        self.assertEqual(cache.stats(), counters(evictions=1))

    def test_recency_and_expiry_together(self):
        cache = LRUCache(3, clock=self.clock)
        cache.put("a", 1)
        cache.put("b", 2, ttl=5)
        cache.put("c", 3)
        cache.get("a")  # order: b, c, a
        self.clock.advance(6)
        cache.put("d", 4)  # drops expired b, nothing evicted
        self.assertEqual(cache.keys(), ["c", "a", "d"])
        cache.put("e", 5)  # full of live entries: evicts c
        self.assertEqual(cache.keys(), ["a", "d", "e"])
        self.assertEqual(cache.stats(), counters(hits=1, evictions=1, expirations=1))

    def test_capacity_one_with_expired_entry(self):
        cache = LRUCache(1, ttl=2, clock=self.clock)
        cache.put("a", 1)
        self.clock.advance(2)
        cache.put("b", 2)
        self.assertEqual(cache.keys(), ["b"])
        self.assertEqual(cache.stats(), counters(expirations=1))


class TtlStatsTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.cache = LRUCache(4, ttl=10, clock=self.clock)

    def test_stats_has_exactly_four_counters(self):
        self.assertEqual(self.cache.stats(), counters())

    def test_expired_get_is_a_miss(self):
        self.cache.put("a", 1)
        self.assertEqual(self.cache.get("a"), 1)
        self.clock.advance(10)
        self.assertIsNone(self.cache.get("a"))
        self.assertEqual(self.cache.stats(), counters(hits=1, misses=1, expirations=1))

    def test_untouched_expired_entries_are_counted(self):
        self.cache.put("a", 1)
        self.cache.put("b", 2, ttl=3)
        self.cache.put("c", 3, ttl=None)
        self.clock.advance(3)
        self.assertEqual(self.cache.stats(), counters(expirations=1))
        self.clock.advance(7)
        self.assertEqual(self.cache.stats(), counters(expirations=2))

    def test_each_expired_entry_counts_once(self):
        self.cache.put("a", 1)
        self.clock.advance(10)
        self.assertEqual(self.cache.stats()["expirations"], 1)
        self.assertNotIn("a", self.cache)
        self.assertIsNone(self.cache.get("a"))
        self.assertEqual(len(self.cache), 0)
        self.assertEqual(self.cache.keys(), [])
        self.cache.put("b", 2)
        self.assertEqual(self.cache.stats(), counters(misses=1, expirations=1))

    def test_contains_len_keys_do_not_count_lookups(self):
        self.cache.put("a", 1)
        self.clock.advance(10)
        "a" in self.cache
        len(self.cache)
        self.cache.keys()
        stats = self.cache.stats()
        self.assertEqual((stats["hits"], stats["misses"]), (0, 0))
        self.assertEqual(stats["expirations"], 1)

    def test_overwrite_before_expiry_is_not_an_expiration(self):
        self.cache.put("a", 1)
        self.clock.advance(9)
        self.cache.put("a", 2)
        self.clock.advance(9)
        self.assertEqual(self.cache.stats(), counters())
        self.clock.advance(1)
        self.assertEqual(self.cache.stats(), counters(expirations=1))

    def test_overwrite_after_expiry_counts_old_entry(self):
        self.cache.put("a", 1)
        self.clock.advance(15)
        self.cache.put("a", 2)
        self.assertEqual(self.cache.stats(), counters(expirations=1))
        self.clock.advance(10)
        self.assertEqual(self.cache.stats(), counters(expirations=2))

    def test_evicted_entry_never_counts_as_expired(self):
        cache = LRUCache(1, ttl=5, clock=self.clock)
        cache.put("a", 1)
        self.clock.advance(1)
        cache.put("b", 2, ttl=None)
        self.clock.advance(100)
        self.assertEqual(cache.stats(), counters(evictions=1))

    def test_stats_dict_works_with_stats_helpers(self):
        self.cache.put("a", 1)
        self.cache.get("a")
        self.cache.get("b")
        self.clock.advance(10)
        self.assertEqual(
            format_stats(self.cache.stats()),
            "hits=1 misses=1 evictions=0 expirations=1 hit_rate=50.0%",
        )


class MemoizeTtlTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.calls = []

    def make(self, **options):
        @memoize(clock=self.clock, **options)
        def double(x):
            self.calls.append(x)
            return 2 * x

        return double

    def test_results_expire_after_ttl(self):
        double = self.make(ttl=10)
        self.assertEqual(double(1), 2)
        self.clock.advance(9)
        self.assertEqual(double(1), 2)
        self.assertEqual(self.calls, [1])
        self.clock.advance(1)
        self.assertEqual(double(1), 2)
        self.assertEqual(self.calls, [1, 1])
        self.clock.advance(9)
        double(1)
        self.assertEqual(self.calls, [1, 1])

    def test_cache_hits_do_not_extend_expiry(self):
        double = self.make(ttl=10)
        double(1)
        for _ in range(4):
            self.clock.advance(2)
            double(1)
        self.assertEqual(self.calls, [1])
        self.clock.advance(2)
        double(1)
        self.assertEqual(self.calls, [1, 1])

    def test_without_ttl_results_never_expire(self):
        double = self.make()
        double(1)
        self.clock.advance(10**9)
        double(1)
        self.assertEqual(self.calls, [1])
        self.assertIsNone(double.cache.ttl)

    def test_cache_uses_ttl(self):
        double = self.make(ttl=10, maxsize=3)
        self.assertIsInstance(double.cache, LRUCache)
        self.assertEqual(double.cache.ttl, 10)
        self.assertEqual(double.cache.capacity, 3)

    def test_cache_info_counts_expirations(self):
        double = self.make(ttl=10)
        double(1)
        double(1)
        self.clock.advance(10)
        double(1)
        self.assertEqual(
            double.cache_info(), counters(hits=1, misses=2, expirations=1)
        )

    def test_cache_clear_keeps_ttl_and_clock(self):
        double = self.make(ttl=10, maxsize=4)
        double(1)
        double.cache_clear()
        self.assertEqual(double.cache.ttl, 10)
        self.assertEqual(double.cache.capacity, 4)
        double(1)
        self.clock.advance(9)
        double(1)
        self.assertEqual(self.calls, [1, 1])
        self.clock.advance(1)
        double(1)
        self.assertEqual(self.calls, [1, 1, 1])

    def test_expired_results_make_room_without_evicting(self):
        double = self.make(ttl=10, maxsize=2)
        double(1)
        double(2)
        self.clock.advance(10)
        double(3)
        self.assertEqual(
            double.cache_info(), counters(misses=3, expirations=2)
        )

    def test_invalid_ttl(self):
        for bad in (0, -5):
            with self.subTest(ttl=bad):
                with self.assertRaises(ValueError):
                    memoize(ttl=bad)(lambda x: x)

    def test_bare_decorator_still_works(self):
        @memoize
        def ident(x):
            self.calls.append(x)
            return x

        ident(1)
        ident(1)
        self.assertEqual(self.calls, [1])
        self.assertIsNone(ident.cache.ttl)
        self.assertEqual(ident.cache_info(), counters(hits=1, misses=1))


class StatsModuleTests(unittest.TestCase):
    def test_cache_stats_has_expirations(self):
        stats = CacheStats()
        self.assertEqual(stats.expirations, 0)
        self.assertEqual(stats.as_dict(), counters())

    def test_cache_stats_keyword_expirations(self):
        self.assertEqual(
            CacheStats(hits=1, expirations=3).as_dict(),
            counters(hits=1, expirations=3),
        )

    def test_merge_sums_expirations(self):
        self.assertEqual(
            merge_stats(counters(1, 2, 3, 4), counters(5, 6, 7, 8), counters(expirations=1)),
            counters(6, 8, 10, 13),
        )

    def test_merge_nothing_has_four_counters(self):
        self.assertEqual(merge_stats(), counters())

    def test_format_includes_expirations(self):
        self.assertEqual(
            format_stats(counters(3, 1, 2, 4)),
            "hits=3 misses=1 evictions=2 expirations=4 hit_rate=75.0%",
        )


if __name__ == "__main__":
    unittest.main()
