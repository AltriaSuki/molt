import unittest

from pkgresolve import PackageIndex, Version, format_lock, parse_lock, verify_lock

INDEX = {
    "web": {"1.0.0": ["http^1"], "2.0.0": ["http^2", "tls>=1.2"]},
    "http": {"1.0.0": [], "2.0.0": ["tls"]},
    "tls": {"1.2.0": [], "1.3.0": []},
}


class FormatAndParseTests(unittest.TestCase):
    def test_format_sorted_by_name(self):
        text = format_lock({"web": Version(2), "http": Version(2), "tls": Version(1, 3)})
        self.assertEqual(text, "http==2.0.0\ntls==1.3.0\nweb==2.0.0\n")

    def test_format_empty(self):
        self.assertEqual(format_lock({}), "")

    def test_round_trip(self):
        lock = {"web": Version(1), "http": Version(1, 0, 4)}
        self.assertEqual(parse_lock(format_lock(lock)), lock)

    def test_parse_skips_blank_lines_and_comments(self):
        text = "# generated\n\n  web == 2.0  \n# trailing\ntls==1.3.0\n"
        self.assertEqual(parse_lock(text), {"web": Version(2), "tls": Version(1, 3)})

    def test_parse_errors(self):
        for bad in ["web", "web>=1.0", "==1.0", "web==1.0\nweb==2.0", "web==x"]:
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    parse_lock(bad)


class VerifyLockTests(unittest.TestCase):
    def setUp(self):
        self.index = PackageIndex(INDEX)

    def lock(self, **versions):
        return {name: Version.parse(text) for name, text in versions.items()}

    def test_valid(self):
        lock = self.lock(web="2.0", http="2.0", tls="1.2")
        self.assertEqual(verify_lock(self.index, ["web"], lock), [])

    def test_root_requirement_not_met(self):
        lock = self.lock(web="2.0", http="2.0", tls="1.3")
        problems = verify_lock(self.index, ["web<2"], lock)
        self.assertEqual(problems, ["root requires web<2.0.0, but web 2.0.0 is locked"])

    def test_dependency_not_met(self):
        lock = self.lock(web="2.0", http="1.0", tls="1.3")
        problems = verify_lock(self.index, ["web"], lock)
        self.assertEqual(problems,
                         ["web 2.0.0 requires http>=2.0.0,<3.0.0, but http 1.0.0 is locked"])

    def test_missing_package(self):
        lock = self.lock(web="2.0", http="2.0")
        problems = verify_lock(self.index, ["web"], lock)
        self.assertIn("web 2.0.0 requires tls>=1.2.0, but tls is not locked", problems)

    def test_version_not_in_index(self):
        lock = self.lock(web="1.0", http="1.5")
        self.assertEqual(verify_lock(self.index, ["web"], lock), ["http 1.5.0 is not in the index"])

    def test_unneeded_package(self):
        lock = self.lock(web="1.0", http="1.0", tls="1.3")
        self.assertEqual(verify_lock(self.index, ["web"], lock),
                         ["tls is locked but nothing requires it"])

    def test_cycle(self):
        index = PackageIndex({"a": {"1.0.0": ["b"]}, "b": {"1.0.0": ["a^1"]}})
        self.assertEqual(verify_lock(index, ["a"], self.lock(a="1.0", b="1.0")), [])


if __name__ == "__main__":
    unittest.main()
