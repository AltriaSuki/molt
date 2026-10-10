import unittest

from cachelib import LRUCache


class ConstructionTests(unittest.TestCase):
    def test_capacity_is_exposed(self):
        self.assertEqual(LRUCache(3).capacity, 3)

    def test_capacity_must_be_positive(self):
        for bad in (0, -1):
            with self.subTest(capacity=bad):
                with self.assertRaises(ValueError):
                    LRUCache(bad)

    def test_capacity_must_be_an_int(self):
        for bad in (2.5, "3", None, True):
            with self.subTest(capacity=bad):
                with self.assertRaises(TypeError):
                    LRUCache(bad)

    def test_new_cache_is_empty(self):
        cache = LRUCache(2)
        self.assertEqual(len(cache), 0)
        self.assertEqual(cache.keys(), [])
        self.assertNotIn("a", cache)


class GetPutTests(unittest.TestCase):
    def test_get_returns_cached_value(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        self.assertEqual(cache.get("a"), 1)
        self.assertIn("a", cache)
        self.assertEqual(len(cache), 1)

    def test_get_missing_returns_default(self):
        cache = LRUCache(2)
        self.assertIsNone(cache.get("nope"))
        self.assertEqual(cache.get("nope", "fallback"), "fallback")

    def test_cached_none_is_not_a_miss(self):
        cache = LRUCache(2)
        cache.put("a", None)
        self.assertIsNone(cache.get("a", "fallback"))
        self.assertEqual(cache.stats()["hits"], 1)

    def test_put_existing_key_replaces_value(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("a", 2)
        self.assertEqual(cache.get("a"), 2)
        self.assertEqual(len(cache), 1)

    def test_unhashable_key_raises_type_error(self):
        cache = LRUCache(2)
        with self.assertRaises(TypeError):
            cache.put(["a"], 1)


class RecencyTests(unittest.TestCase):
    def test_keys_are_least_recent_first(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key.upper())
        self.assertEqual(cache.keys(), ["a", "b", "c"])

    def test_get_marks_key_most_recent(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key.upper())
        cache.get("a")
        self.assertEqual(cache.keys(), ["b", "c", "a"])

    def test_put_existing_key_marks_it_most_recent(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key.upper())
        cache.put("b", "B2")
        self.assertEqual(cache.keys(), ["a", "c", "b"])

    def test_contains_and_len_do_not_touch_recency(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertIn("a", cache)
        self.assertEqual(len(cache), 2)
        cache.put("c", 3)
        self.assertEqual(cache.keys(), ["b", "c"])


class EvictionTests(unittest.TestCase):
    def test_full_cache_evicts_least_recently_used(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("c", 3)
        self.assertNotIn("a", cache)
        self.assertEqual(cache.keys(), ["b", "c"])
        self.assertEqual(len(cache), 2)

    def test_eviction_respects_gets(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.get("a")
        cache.put("c", 3)
        self.assertEqual(cache.keys(), ["a", "c"])

    def test_replacing_a_key_never_evicts(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("a", 10)
        self.assertEqual(cache.keys(), ["b", "a"])
        self.assertEqual(cache.stats()["evictions"], 0)

    def test_capacity_one(self):
        cache = LRUCache(1)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertEqual(cache.keys(), ["b"])
        self.assertEqual(cache.get("b"), 2)


class StatsTests(unittest.TestCase):
    def test_new_cache_has_zero_counters(self):
        self.assertEqual(
            LRUCache(2).stats(),
            {"hits": 0, "misses": 0, "evictions": 0, "expirations": 0},
        )

    def test_counts_hits_misses_and_evictions(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.get("a")
        cache.get("a")
        cache.get("zzz")
        cache.put("b", 2)
        cache.put("c", 3)
        self.assertEqual(
            cache.stats(),
            {"hits": 2, "misses": 1, "evictions": 1, "expirations": 0},
        )

    def test_contains_does_not_count(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        "a" in cache
        "b" in cache
        self.assertEqual(cache.stats()["hits"], 0)
        self.assertEqual(cache.stats()["misses"], 0)

    def test_stats_is_a_snapshot(self):
        cache = LRUCache(2)
        snapshot = cache.stats()
        snapshot["hits"] = 99
        cache.get("a")
        self.assertEqual(cache.stats()["hits"], 0)
        self.assertEqual(cache.stats()["misses"], 1)


if __name__ == "__main__":
    unittest.main()
