import unittest

from scheduler import cron


class FieldTableTests(unittest.TestCase):
    def test_bounds(self):
        self.assertEqual(
            [(f.name, f.low, f.high) for f in cron.FIELDS],
            [
                ("minute", 0, 59),
                ("hour", 0, 23),
                ("day-of-month", 1, 31),
                ("month", 1, 12),
                ("day-of-week", 0, 7),
            ],
        )

    def test_names(self):
        self.assertEqual(cron.MONTH.name_value("jan"), 1)
        self.assertEqual(cron.MONTH.name_value("Dec"), 12)
        self.assertEqual(cron.DAY_OF_WEEK.name_value("SUN"), 0)
        self.assertEqual(cron.DAY_OF_WEEK.name_value("sat"), 6)
        self.assertIsNone(cron.DAY_OF_WEEK.name_value("jan"))
        self.assertIsNone(cron.MINUTE.name_value("mon"))

    def test_cron_error_is_value_error(self):
        self.assertTrue(issubclass(cron.CronError, ValueError))


if __name__ == "__main__":
    unittest.main()
