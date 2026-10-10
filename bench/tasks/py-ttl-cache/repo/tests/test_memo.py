import unittest

from cachelib import LRUCache, make_key, memoize


class MakeKeyTests(unittest.TestCase):
    def test_positional_only(self):
        self.assertEqual(make_key((1, 2), {}), (1, 2))

    def test_keyword_order_does_not_matter(self):
        self.assertEqual(
            make_key((1,), {"a": 1, "b": 2}), make_key((1,), {"b": 2, "a": 1})
        )

    def test_positional_and_keyword_differ(self):
        self.assertNotEqual(make_key((1, 2), {}), make_key((1,), {"b": 2}))


class MemoizeTests(unittest.TestCase):
    def test_caches_results(self):
        calls = []

        @memoize
        def square(x):
            calls.append(x)
            return x * x

        self.assertEqual(square(3), 9)
        self.assertEqual(square(3), 9)
        self.assertEqual(square(4), 16)
        self.assertEqual(calls, [3, 4])

    def test_keyword_arguments(self):
        calls = []

        @memoize(maxsize=8)
        def area(width, height):
            calls.append((width, height))
            return width * height

        self.assertEqual(area(width=2, height=3), 6)
        self.assertEqual(area(height=3, width=2), 6)
        self.assertEqual(len(calls), 1)

    def test_maxsize_bounds_the_cache(self):
        calls = []

        @memoize(maxsize=2)
        def ident(x):
            calls.append(x)
            return x

        for x in (1, 2, 3, 1):
            ident(x)
        self.assertEqual(calls, [1, 2, 3, 1])
        self.assertIsInstance(ident.cache, LRUCache)
        self.assertEqual(ident.cache.capacity, 2)
        self.assertEqual(len(ident.cache), 2)

    def test_none_results_are_cached(self):
        calls = []

        @memoize
        def nothing(x):
            calls.append(x)
            return None

        nothing(1)
        nothing(1)
        self.assertEqual(calls, [1])

    def test_exceptions_are_not_cached(self):
        calls = []

        @memoize
        def flaky(x):
            calls.append(x)
            if len(calls) == 1:
                raise RuntimeError("first call fails")
            return x

        with self.assertRaises(RuntimeError):
            flaky(1)
        self.assertEqual(flaky(1), 1)
        self.assertEqual(calls, [1, 1])

    def test_unhashable_argument_raises_type_error(self):
        @memoize
        def total(values):
            return sum(values)

        with self.assertRaises(TypeError):
            total([1, 2])

    def test_wraps_the_function(self):
        @memoize(maxsize=4)
        def documented(x):
            """Return x."""
            return x

        self.assertEqual(documented.__name__, "documented")
        self.assertEqual(documented.__doc__, "Return x.")

    def test_cache_info_reports_stats(self):
        @memoize(maxsize=1)
        def ident(x):
            return x

        ident(1)
        ident(1)
        ident(2)
        self.assertEqual(
            ident.cache_info(), {"hits": 1, "misses": 2, "evictions": 1}
        )

    def test_cache_clear_starts_over(self):
        calls = []

        @memoize(maxsize=4)
        def ident(x):
            calls.append(x)
            return x

        ident(1)
        ident.cache_clear()
        ident(1)
        self.assertEqual(calls, [1, 1])
        self.assertEqual(ident.cache.capacity, 4)
        self.assertEqual(ident.cache_info()["misses"], 1)

    def test_positional_options_are_rejected(self):
        with self.assertRaises(TypeError):
            memoize(64)

    def test_invalid_maxsize(self):
        with self.assertRaises(ValueError):
            memoize(maxsize=0)(lambda x: x)


if __name__ == "__main__":
    unittest.main()
