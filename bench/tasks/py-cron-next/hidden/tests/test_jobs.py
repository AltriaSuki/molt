import unittest

from scheduler.jobs import DuplicateJobError, Job, JobRegistry


def job(name, **kw):
    kw.setdefault("schedule", "0 * * * *")
    kw.setdefault("command", f"run-{name}")
    return Job(name=name, **kw)


class JobTests(unittest.TestCase):
    def test_defaults(self):
        j = job("backup")
        self.assertTrue(j.enabled)
        self.assertEqual(j.tags, ())

    def test_tags_are_stored_as_tuple(self):
        j = job("backup", tags=["a", "b"])
        self.assertEqual(j.tags, ("a", "b"))
        hash(j)  # still hashable

    def test_rejects_bad_names(self):
        for name in ["", " backup", "-x", "a b", "a/b"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                job(name)

    def test_rejects_empty_schedule_or_command(self):
        with self.assertRaises(ValueError):
            job("a", schedule="  ")
        with self.assertRaises(ValueError):
            job("a", command="")


class RegistryTests(unittest.TestCase):
    def test_add_get_and_iterate_in_name_order(self):
        reg = JobRegistry([job("zeta"), job("alpha"), job("mid")])
        self.assertEqual([j.name for j in reg], ["alpha", "mid", "zeta"])
        self.assertEqual(reg.get("mid").command, "run-mid")
        self.assertIn("alpha", reg)
        self.assertNotIn("beta", reg)
        self.assertEqual(len(reg), 3)

    def test_duplicate_names_are_rejected(self):
        reg = JobRegistry([job("a")])
        with self.assertRaises(DuplicateJobError):
            reg.add(job("a", command="other"))
        self.assertTrue(issubclass(DuplicateJobError, ValueError))

    def test_remove_and_missing(self):
        reg = JobRegistry([job("a"), job("b")])
        self.assertEqual(reg.remove("a").name, "a")
        self.assertEqual([j.name for j in reg], ["b"])
        with self.assertRaises(KeyError):
            reg.remove("a")
        with self.assertRaises(KeyError):
            reg.get("nope")

    def test_enabled_and_set_enabled(self):
        reg = JobRegistry([job("a"), job("b", enabled=False), job("c")])
        self.assertEqual([j.name for j in reg.enabled()], ["a", "c"])
        reg.set_enabled("a", False)
        reg.set_enabled("b", True)
        self.assertEqual([j.name for j in reg.enabled()], ["b", "c"])

    def test_with_tag(self):
        reg = JobRegistry([job("a", tags=["x"]), job("b", tags=["x", "y"]), job("c")])
        self.assertEqual([j.name for j in reg.with_tag("x")], ["a", "b"])
        self.assertEqual([j.name for j in reg.with_tag("y")], ["b"])
        self.assertEqual(reg.with_tag("z"), [])


if __name__ == "__main__":
    unittest.main()
