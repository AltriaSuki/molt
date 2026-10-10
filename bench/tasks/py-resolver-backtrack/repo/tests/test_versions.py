import unittest

from pkgresolve.versions import (
    Constraint,
    InvalidRequirement,
    InvalidVersion,
    Requirement,
    Version,
)


def v(text):
    return Version.parse(text)


class VersionTests(unittest.TestCase):
    def test_parse_full(self):
        self.assertEqual(v("1.2.3"), Version(1, 2, 3))

    def test_missing_parts_are_zero(self):
        self.assertEqual(v("1.4"), Version(1, 4, 0))
        self.assertEqual(v("2"), Version(2, 0, 0))

    def test_str_is_always_three_parts(self):
        self.assertEqual(str(v("1")), "1.0.0")
        self.assertEqual(str(v(" 3.10.2 ")), "3.10.2")

    def test_ordering_is_numeric(self):
        self.assertLess(v("1.2.0"), v("1.10.0"))
        self.assertLess(v("1.9.9"), v("2.0.0"))
        self.assertEqual(sorted([v("2.0"), v("0.9.1"), v("1.10"), v("1.2")]),
                         [v("0.9.1"), v("1.2"), v("1.10"), v("2.0")])

    def test_parse_accepts_a_version(self):
        version = Version(1, 2, 3)
        self.assertIs(Version.parse(version), version)

    def test_invalid_versions(self):
        for bad in ["", "a.b.c", "1.2.3.4", "1..2", "-1.0", "1.0-beta", "v1.0"]:
            with self.subTest(bad=bad):
                with self.assertRaises(InvalidVersion):
                    Version.parse(bad)

    def test_invalid_version_is_a_value_error(self):
        self.assertTrue(issubclass(InvalidVersion, ValueError))

    def test_hashable(self):
        self.assertEqual(len({v("1.0"), v("1.0.0"), v("1")}), 1)


class ConstraintTests(unittest.TestCase):
    def allowed(self, text, versions):
        return [x for x in versions if Constraint.parse(text).allows(v(x))]

    def test_any(self):
        for text in ["", "*", "  "]:
            with self.subTest(text=text):
                self.assertTrue(Constraint.parse(text).is_any())
                self.assertTrue(Constraint.parse(text).allows(v("0.0.1")))

    def test_comparisons(self):
        versions = ["0.9.0", "1.0.0", "1.5.0", "2.0.0"]
        self.assertEqual(self.allowed("==1.5", versions), ["1.5.0"])
        self.assertEqual(self.allowed("!=1.5", versions), ["0.9.0", "1.0.0", "2.0.0"])
        self.assertEqual(self.allowed(">=1.0", versions), ["1.0.0", "1.5.0", "2.0.0"])
        self.assertEqual(self.allowed(">1.0", versions), ["1.5.0", "2.0.0"])
        self.assertEqual(self.allowed("<=1.5", versions), ["0.9.0", "1.0.0", "1.5.0"])
        self.assertEqual(self.allowed("<1.5", versions), ["0.9.0", "1.0.0"])

    def test_comma_means_and(self):
        versions = ["0.9.0", "1.0.0", "1.5.0", "2.0.0"]
        self.assertEqual(self.allowed(">=1.0,<2.0", versions), ["1.0.0", "1.5.0"])
        self.assertEqual(self.allowed(" >= 1.0 , < 2.0 , != 1.5 ", versions), ["1.0.0"])

    def test_caret(self):
        versions = ["1.1.9", "1.2.0", "1.9.0", "2.0.0"]
        self.assertEqual(self.allowed("^1.2", versions), ["1.2.0", "1.9.0"])
        self.assertEqual(self.allowed("^1", versions), ["1.1.9", "1.2.0", "1.9.0"])

    def test_caret_below_one(self):
        versions = ["0.0.3", "0.0.4", "0.2.9", "0.3.0", "0.3.5", "0.4.0", "1.0.0"]
        self.assertEqual(self.allowed("^0.3", versions), ["0.3.0", "0.3.5"])
        self.assertEqual(self.allowed("^0.0.3", versions), ["0.0.3"])
        self.assertEqual(self.allowed("^0.0", versions), ["0.0.3", "0.0.4"])
        self.assertEqual(self.allowed("^0", versions), versions[:-1])

    def test_str(self):
        self.assertEqual(str(Constraint.parse("^1.2")), ">=1.2.0,<2.0.0")
        self.assertEqual(str(Constraint.parse("!=1.3")), "!=1.3.0")
        self.assertEqual(str(Constraint.parse("")), "*")

    def test_and(self):
        both = Constraint.parse(">=1.0") & Constraint.parse("<2.0")
        self.assertEqual(both, Constraint.parse(">=1.0,<2.0"))

    def test_invalid_constraints(self):
        for bad in ["1.0", "=>1.0", ">=1.0,", "~1.0", ">=x", ">=1.0 <2.0"]:
            with self.subTest(bad=bad):
                with self.assertRaises(InvalidVersion):
                    Constraint.parse(bad)


class RequirementTests(unittest.TestCase):
    def test_name_and_constraint(self):
        req = Requirement.parse("b>=1.0,<2.0")
        self.assertEqual(req.name, "b")
        self.assertEqual(req.constraint, Constraint.parse(">=1.0,<2.0"))

    def test_spaces_allowed(self):
        req = Requirement.parse("  web  ^2.1 ")
        self.assertEqual(req.name, "web")
        self.assertTrue(req.allows(v("2.5")))
        self.assertFalse(req.allows(v("3.0")))

    def test_bare_name_allows_anything(self):
        req = Requirement.parse("http-core")
        self.assertEqual(req.name, "http-core")
        self.assertTrue(req.constraint.is_any())
        self.assertEqual(str(req), "http-core")

    def test_str(self):
        self.assertEqual(str(Requirement.parse("db ==1.4")), "db==1.4.0")

    def test_invalid(self):
        for bad in ["", ">=1.0", "web 1.0", "web>=", "-web", "web-"]:
            with self.subTest(bad=bad):
                with self.assertRaises(InvalidRequirement):
                    Requirement.parse(bad)
        self.assertTrue(issubclass(InvalidRequirement, ValueError))


if __name__ == "__main__":
    unittest.main()
