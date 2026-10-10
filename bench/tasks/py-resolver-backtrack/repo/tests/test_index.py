import os
import tempfile
import unittest

from pkgresolve import PackageIndex, Requirement, Version

EXAMPLE = {
    "web": {"1.0.0": ["http^1"], "2.0.0": ["http^2", "tls>=1.2"]},
    "http": {"1.0.0": [], "2.0.0": ["tls"]},
    "tls": {"1.2.0": [], "1.3.0": []},
}


class PackageIndexTests(unittest.TestCase):
    def test_versions_newest_first(self):
        index = PackageIndex()
        for text in ["1.2.0", "1.10.0", "0.9.0", "1.9.3"]:
            index.add("lib", text)
        self.assertEqual([str(x) for x in index.versions("lib")],
                         ["1.10.0", "1.9.3", "1.2.0", "0.9.0"])

    def test_versions_returns_a_copy(self):
        index = PackageIndex(EXAMPLE)
        index.versions("tls").clear()
        self.assertEqual(len(index.versions("tls")), 2)

    def test_dependencies_are_parsed(self):
        index = PackageIndex(EXAMPLE)
        deps = index.dependencies("web", Version(2))
        self.assertEqual(deps, (Requirement.parse("http^2"), Requirement.parse("tls>=1.2")))
        self.assertEqual(index.dependencies("http", Version(1)), ())

    def test_unknown_package(self):
        index = PackageIndex(EXAMPLE)
        self.assertNotIn("nope", index)
        with self.assertRaises(KeyError):
            index.versions("nope")
        with self.assertRaises(KeyError):
            index.dependencies("web", Version(3))

    def test_names_and_len(self):
        index = PackageIndex(EXAMPLE)
        self.assertEqual(index.names(), ["http", "tls", "web"])
        self.assertEqual(len(index), 3)
        self.assertIn("web", index)

    def test_matching(self):
        index = PackageIndex(EXAMPLE)
        self.assertEqual(index.matching(Requirement.parse("tls>=1.2")), [Version(1, 3), Version(1, 2)])
        self.assertEqual(index.matching(Requirement.parse("tls>=2")), [])
        self.assertEqual(index.matching(Requirement.parse("nope")), [])

    def test_duplicate_version_rejected(self):
        index = PackageIndex(EXAMPLE)
        with self.assertRaises(ValueError):
            index.add("tls", "1.2")

    def test_bad_input_rejected(self):
        index = PackageIndex()
        with self.assertRaises(ValueError):
            index.add("bad name", "1.0")
        with self.assertRaises(ValueError):
            index.add("lib", "one")
        with self.assertRaises(ValueError):
            index.add("lib", "1.0", ["other >> 1"])
        with self.assertRaises(TypeError):
            index.add("lib", "1.0", "other")

    def test_add_after_versions_is_seen(self):
        index = PackageIndex(EXAMPLE)
        self.assertEqual(index.versions("tls")[0], Version(1, 3))
        index.add("tls", "1.4.0")
        self.assertEqual(index.versions("tls")[0], Version(1, 4))

    def test_json_round_trip(self):
        index = PackageIndex(EXAMPLE)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "index.json")
            with open(path, "w", encoding="utf-8") as handle:
                import json
                json.dump(index.to_dict(), handle)
            loaded = PackageIndex.load(path)
        self.assertEqual(loaded.to_dict(), index.to_dict())
        self.assertEqual(loaded.to_dict()["web"]["2.0.0"], ["http>=2.0.0,<3.0.0", "tls>=1.2.0"])

    def test_from_json_requires_object(self):
        with self.assertRaises(ValueError):
            PackageIndex.from_json("[]")

    def test_staging_mirror_snapshot_loads(self):
        path = os.path.join(os.path.dirname(__file__), "data", "mirror.json")
        index = PackageIndex.load(path)
        self.assertEqual(len(index), 30)
        for name in index.names():
            versions = index.versions(name)
            self.assertEqual(len(versions), 15, name)
            for version in versions:
                for req in index.dependencies(name, version):
                    self.assertIn(req.name, index, f"{name} {version} requires {req}")


if __name__ == "__main__":
    unittest.main()
