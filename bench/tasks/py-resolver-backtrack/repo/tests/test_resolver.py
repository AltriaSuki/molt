import unittest

from pkgresolve import PackageIndex, ResolutionError, Version, resolve, verify_lock

STACK = {
    "web": {"1.0.0": ["http^1"], "1.5.0": ["http^1", "tls>=1.2"], "2.0.0": ["http^2", "tls>=1.2"]},
    "http": {"1.0.0": [], "1.4.2": ["log"], "2.0.0": ["tls"], "2.1.0": ["tls>=1.3", "log"]},
    "tls": {"1.2.0": [], "1.3.0": []},
    "log": {"1.0.0": [], "1.2.3": []},
    "unused": {"0.1.0": ["web"]},
}


def versions(resolution):
    return {name: str(version) for name, version in resolution.items()}


class ResolveTests(unittest.TestCase):
    def setUp(self):
        self.index = PackageIndex(STACK)

    def test_newest_versions(self):
        result = resolve(self.index, ["web"])
        self.assertEqual(versions(result),
                         {"web": "2.0.0", "http": "2.1.0", "tls": "1.3.0", "log": "1.2.3"})

    def test_values_are_versions(self):
        result = resolve(self.index, ["tls"])
        self.assertEqual(result, {"tls": Version(1, 3)})

    def test_root_constraint_is_respected(self):
        result = resolve(self.index, ["web<2"])
        self.assertEqual(versions(result),
                         {"web": "1.5.0", "http": "1.4.2", "tls": "1.3.0", "log": "1.2.3"})

    def test_only_needed_packages(self):
        result = resolve(self.index, ["http^1"])
        self.assertEqual(versions(result), {"http": "1.4.2", "log": "1.2.3"})
        self.assertNotIn("unused", resolve(self.index, ["web"]))

    def test_no_requirements(self):
        self.assertEqual(resolve(self.index, []), {})

    def test_result_is_a_valid_lock(self):
        for reqs in (["web"], ["web^1", "log<1.2"], ["unused", "tls==1.3"]):
            with self.subTest(reqs=reqs):
                self.assertEqual(verify_lock(self.index, reqs, resolve(self.index, reqs)), [])

    def test_cycle_terminates(self):
        index = PackageIndex({
            "a": {"1.0.0": ["b"], "1.1.0": ["b>=1"]},
            "b": {"1.0.0": ["a"], "2.0.0": ["a^1"]},
        })
        self.assertEqual(versions(resolve(index, ["a"])), {"a": "1.1.0", "b": "2.0.0"})

    def test_unknown_package(self):
        with self.assertRaises(ResolutionError) as ctx:
            resolve(self.index, ["web", "ghost>=1"])
        self.assertIn("ghost", str(ctx.exception))

    def test_no_matching_version(self):
        with self.assertRaises(ResolutionError) as ctx:
            resolve(self.index, ["tls>=2"])
        self.assertIn("tls", str(ctx.exception))

    def test_resolution_error_is_an_exception(self):
        self.assertTrue(issubclass(ResolutionError, Exception))


if __name__ == "__main__":
    unittest.main()
