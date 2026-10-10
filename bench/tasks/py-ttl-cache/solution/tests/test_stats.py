import unittest

from cachelib import COUNTERS, CacheStats, format_stats, hit_rate, merge_stats


class CacheStatsTests(unittest.TestCase):
    def test_starts_at_zero(self):
        self.assertEqual(
            CacheStats().as_dict(),
            {"hits": 0, "misses": 0, "evictions": 0, "expirations": 0},
        )

    def test_as_dict_is_a_copy(self):
        stats = CacheStats(hits=2)
        snapshot = stats.as_dict()
        stats.hits += 1
        self.assertEqual(snapshot["hits"], 2)

    def test_counters_order(self):
        self.assertEqual(COUNTERS, ("hits", "misses", "evictions", "expirations"))


class HitRateTests(unittest.TestCase):
    def test_hit_rate(self):
        self.assertEqual(hit_rate({"hits": 3, "misses": 1, "evictions": 0}), 0.75)

    def test_no_lookups(self):
        self.assertEqual(hit_rate({"hits": 0, "misses": 0, "evictions": 5}), 0.0)


class MergeTests(unittest.TestCase):
    def test_merge_adds_counters(self):
        a = {"hits": 1, "misses": 2, "evictions": 3, "expirations": 4}
        b = {"hits": 10, "misses": 20, "evictions": 30, "expirations": 40}
        self.assertEqual(
            merge_stats(a, b),
            {"hits": 11, "misses": 22, "evictions": 33, "expirations": 44},
        )

    def test_merge_nothing(self):
        self.assertEqual(
            merge_stats(),
            {"hits": 0, "misses": 0, "evictions": 0, "expirations": 0},
        )


class FormatTests(unittest.TestCase):
    def test_format(self):
        self.assertEqual(
            format_stats({"hits": 3, "misses": 1, "evictions": 0, "expirations": 5}),
            "hits=3 misses=1 evictions=0 expirations=5 hit_rate=75.0%",
        )

    def test_format_without_lookups(self):
        self.assertEqual(
            format_stats({"hits": 0, "misses": 0, "evictions": 2, "expirations": 0}),
            "hits=0 misses=0 evictions=2 expirations=0 hit_rate=0.0%",
        )


if __name__ == "__main__":
    unittest.main()
