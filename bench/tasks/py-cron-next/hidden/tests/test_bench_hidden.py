import unittest
from datetime import datetime, timedelta

from scheduler import cron
from scheduler.cron import CronError, next_fire, parse
from scheduler.jobs import Job, JobRegistry
from scheduler.runner import Runner


def D(*args):
    return datetime(*args)


ALL_MINUTES = set(range(60))
ALL_HOURS = set(range(24))
ALL_DAYS = set(range(1, 32))
ALL_MONTHS = set(range(1, 13))
ALL_WEEKDAYS = set(range(7))


def fields(s):
    return (
        set(s.minutes),
        set(s.hours),
        set(s.days),
        set(s.months),
        set(s.weekdays),
        bool(s.dom_restricted),
        bool(s.dow_restricted),
    )


class ParseValuesTests(unittest.TestCase):
    def test_star_everywhere(self):
        self.assertEqual(
            fields(parse("* * * * *")),
            (ALL_MINUTES, ALL_HOURS, ALL_DAYS, ALL_MONTHS, ALL_WEEKDAYS, False, False),
        )

    def test_single_values_and_lists(self):
        s = parse("5 0,12,23 1,15,31 6 3")
        self.assertEqual(set(s.minutes), {5})
        self.assertEqual(set(s.hours), {0, 12, 23})
        self.assertEqual(set(s.days), {1, 15, 31})
        self.assertEqual(set(s.months), {6})
        self.assertEqual(set(s.weekdays), {3})

    def test_ranges(self):
        s = parse("0-4 9-17 10-12 3-5 1-5")
        self.assertEqual(set(s.minutes), {0, 1, 2, 3, 4})
        self.assertEqual(set(s.hours), set(range(9, 18)))
        self.assertEqual(set(s.days), {10, 11, 12})
        self.assertEqual(set(s.months), {3, 4, 5})
        self.assertEqual(set(s.weekdays), {1, 2, 3, 4, 5})

    def test_single_value_range(self):
        self.assertEqual(set(parse("7-7 * * * *").minutes), {7})

    def test_star_steps(self):
        s = parse("*/15 */6 */10 */4 */2")
        self.assertEqual(set(s.minutes), {0, 15, 30, 45})
        self.assertEqual(set(s.hours), {0, 6, 12, 18})
        self.assertEqual(set(s.days), {1, 11, 21, 31})
        self.assertEqual(set(s.months), {1, 5, 9})
        self.assertEqual(set(s.weekdays), {0, 2, 4, 6})

    def test_range_steps(self):
        s = parse("10-30/7 9-17/4 1-10/3 2-11/3 1-5/2")
        self.assertEqual(set(s.minutes), {10, 17, 24})
        self.assertEqual(set(s.hours), {9, 13, 17})
        self.assertEqual(set(s.days), {1, 4, 7, 10})
        self.assertEqual(set(s.months), {2, 5, 8, 11})
        self.assertEqual(set(s.weekdays), {1, 3, 5})

    def test_start_steps_run_to_field_maximum(self):
        s = parse("5/20 20/2 25/3 10/1 *")
        self.assertEqual(set(s.minutes), {5, 25, 45})
        self.assertEqual(set(s.hours), {20, 22})
        self.assertEqual(set(s.days), {25, 28, 31})
        self.assertEqual(set(s.months), {10, 11, 12})

    def test_day_of_week_start_step_reaches_seven(self):
        # The day-of-week maximum is 7 (Sunday), so 1/2 is Mon, Wed, Fri, Sun.
        self.assertEqual(set(parse("0 0 * * 1/2").weekdays), {1, 3, 5, 0})
        self.assertEqual(set(parse("0 0 * * 5/1").weekdays), {5, 6, 0})

    def test_step_larger_than_range(self):
        self.assertEqual(set(parse("*/100 * * * *").minutes), {0})
        self.assertEqual(set(parse("40-50/30 * * * *").minutes), {40})

    def test_steps_inside_lists(self):
        self.assertEqual(set(parse("1-5/2,30,50/5 * * * *").minutes), {1, 3, 5, 30, 50, 55})

    def test_overlapping_list_items(self):
        self.assertEqual(set(parse("1-10,5-12/3 * * * *").minutes), set(range(1, 11)) | {11})

    def test_sunday_seven_is_stored_as_zero(self):
        self.assertEqual(set(parse("0 0 * * 7").weekdays), {0})
        self.assertEqual(set(parse("0 0 * * 5-7").weekdays), {5, 6, 0})
        self.assertEqual(set(parse("0 0 * * 0-7").weekdays), ALL_WEEKDAYS)
        self.assertEqual(set(parse("0 0 * * 0,7").weekdays), {0})

    def test_month_names(self):
        self.assertEqual(set(parse("0 0 1 jan-Mar,DEC *").months), {1, 2, 3, 12})
        self.assertEqual(set(parse("0 0 1 feb,AUG *").months), {2, 8})
        self.assertEqual(set(parse("0 0 1 JAN/3 *").months), {1, 4, 7, 10})

    def test_weekday_names(self):
        self.assertEqual(set(parse("0 0 * * mon-FRI").weekdays), {1, 2, 3, 4, 5})
        self.assertEqual(set(parse("0 0 * * sun,Sat").weekdays), {0, 6})
        self.assertEqual(set(parse("0 0 * * TUE-thu").weekdays), {2, 3, 4})

    def test_names_and_numbers_mix(self):
        self.assertEqual(set(parse("0 0 1 3-MAY *").months), {3, 4, 5})
        self.assertEqual(set(parse("0 0 * * MON,3,fri").weekdays), {1, 3, 5})


class RestrictedTests(unittest.TestCase):
    def test_only_literal_star_is_unrestricted(self):
        cases = {
            "* * * * *": (False, False),
            "* * 1 * *": (True, False),
            "* * * * 1": (False, True),
            "* * */1 * *": (True, False),
            "* * 1-31 * *": (True, False),
            "* * * * */1": (False, True),
            "* * * * 0-7": (False, True),
            "* * * * 0-6": (False, True),
            "* * 1 * MON": (True, True),
        }
        for expr, expected in cases.items():
            with self.subTest(expr=expr):
                s = parse(expr)
                self.assertEqual((bool(s.dom_restricted), bool(s.dow_restricted)), expected)


class WhitespaceAndAliasTests(unittest.TestCase):
    def test_tabs_and_runs_of_spaces(self):
        a = parse("0\t\t12  1 \t *\t*")
        self.assertEqual(fields(a), fields(parse("0 12 1 * *")))

    def test_leading_and_trailing_whitespace(self):
        self.assertEqual(fields(parse("  */5 * * * *  ")), fields(parse("*/5 * * * *")))
        self.assertEqual(fields(parse("\t@hourly ")), fields(parse("0 * * * *")))

    def test_aliases(self):
        cases = {
            "@yearly": "0 0 1 1 *",
            "@annually": "0 0 1 1 *",
            "@monthly": "0 0 1 * *",
            "@weekly": "0 0 * * 0",
            "@daily": "0 0 * * *",
            "@midnight": "0 0 * * *",
            "@hourly": "0 * * * *",
        }
        for alias, expansion in cases.items():
            with self.subTest(alias=alias):
                self.assertEqual(fields(parse(alias)), fields(parse(expansion)))

    def test_aliases_are_case_insensitive(self):
        self.assertEqual(fields(parse("@DAILY")), fields(parse("0 0 * * *")))
        self.assertEqual(fields(parse("@Weekly")), fields(parse("0 0 * * 0")))

    def test_weekly_alias_is_sunday_only(self):
        s = parse("@weekly")
        self.assertEqual(set(s.weekdays), {0})
        self.assertFalse(s.dom_restricted)
        self.assertTrue(s.dow_restricted)


class ErrorTests(unittest.TestCase):
    BAD = [
        "",
        "   ",
        "* * * *",
        "* * * * * *",
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * 32 * *",
        "* * * 0 *",
        "* * * 13 *",
        "* * * * 8",
        "1-60 * * * *",
        "* * * * 0-8",
        "*/0 * * * *",
        "1-10/0 * * * *",
        "5/0 * * * *",
        "10-5 * * * *",
        "* * * DEC-JAN *",
        "* * * FOO *",
        "* * * * MONDAY",
        "* * * SEPT *",
        "* * * * JAN",
        "* * * MON *",
        "MON * * * *",
        "1,,2 * * * *",
        "1, * * * *",
        ",1 * * * *",
        "a * * * *",
        "*/ * * * *",
        "*/x * * * *",
        "*/MON * * * 1",
        "1-2-3 * * * *",
        "1- * * * *",
        "-5 * * * *",
        "1/2/3 * * * *",
        "** * * * *",
        "@reboot",
        "@",
        "@daily *",
        "@ daily",
    ]

    def test_parse_rejects(self):
        for expr in self.BAD:
            with self.subTest(expr=expr):
                with self.assertRaises(CronError):
                    parse(expr)

    def test_next_fire_rejects_invalid_expressions(self):
        for expr in ["61 * * * *", "* * * *", "@never", "*/0 * * * *"]:
            with self.subTest(expr=expr):
                with self.assertRaises(CronError):
                    next_fire(expr, D(2024, 1, 1))

    def test_cron_error_is_a_value_error(self):
        self.assertTrue(issubclass(CronError, ValueError))
        with self.assertRaises(ValueError):
            parse("99 * * * *")


class NextFireTableTests(unittest.TestCase):
    CASES = [
        # (expression, after, expected)
        # every minute; the result is strictly after and on a whole minute
        ("* * * * *", D(2024, 5, 1, 10, 0), D(2024, 5, 1, 10, 1)),
        ("* * * * *", D(2024, 5, 1, 10, 0, 30), D(2024, 5, 1, 10, 1)),
        ("* * * * *", D(2024, 5, 1, 10, 0, 59, 999999), D(2024, 5, 1, 10, 1)),
        ("* * * * *", D(2024, 5, 1, 23, 59, 1), D(2024, 5, 2, 0, 0)),
        ("0 * * * *", D(2024, 5, 1, 10, 0), D(2024, 5, 1, 11, 0)),
        ("30 * * * *", D(2024, 5, 1, 10, 15), D(2024, 5, 1, 10, 30)),
        ("30 10 * * *", D(2024, 5, 1, 10, 30, 15), D(2024, 5, 2, 10, 30)),
        ("30 10 * * *", D(2024, 5, 1, 10, 29, 59), D(2024, 5, 1, 10, 30)),
        ("15,45 * * * *", D(2024, 5, 1, 10, 20), D(2024, 5, 1, 10, 45)),
        ("15,45 * * * *", D(2024, 5, 1, 10, 50), D(2024, 5, 1, 11, 15)),
        # day, month and year rollover
        ("0 0 * * *", D(2024, 12, 31, 23, 59), D(2025, 1, 1, 0, 0)),
        ("0 0 * * *", D(2024, 2, 28, 12, 0), D(2024, 2, 29, 0, 0)),
        ("0 0 * * *", D(2023, 2, 28, 12, 0), D(2023, 3, 1, 0, 0)),
        ("0 0 1 * *", D(2024, 1, 31, 12, 0), D(2024, 2, 1, 0, 0)),
        ("0 0 31 * *", D(2024, 4, 15), D(2024, 5, 31, 0, 0)),
        ("0 0 31 * *", D(2024, 7, 31, 0, 0), D(2024, 8, 31, 0, 0)),
        ("0 0 31 * *", D(2024, 8, 31, 0, 0), D(2024, 10, 31, 0, 0)),
        ("0 0 1 1 *", D(2024, 6, 1), D(2025, 1, 1, 0, 0)),
        ("30 23 31 12 *", D(2024, 12, 31, 23, 30), D(2025, 12, 31, 23, 30)),
        ("59 23 * * *", D(2024, 12, 31, 23, 58, 59), D(2024, 12, 31, 23, 59)),
        ("*/5 * * 3 *", D(2024, 1, 15, 8, 7), D(2024, 3, 1, 0, 0)),
        ("0 9 * 3 *", D(2024, 3, 31, 9, 0), D(2025, 3, 1, 9, 0)),
        ("0 0 15 6 *", D(2024, 6, 15, 0, 0), D(2025, 6, 15, 0, 0)),
        # leap years
        ("0 12 29 2 *", D(2024, 1, 1), D(2024, 2, 29, 12, 0)),
        ("0 0 29 2 *", D(2024, 3, 1), D(2028, 2, 29, 0, 0)),
        ("0 0 29 2 *", D(2024, 2, 29, 0, 0), D(2028, 2, 29, 0, 0)),
        ("0 0 29 2 *", D(2096, 3, 1), D(2104, 2, 29, 0, 0)),
        ("59 23 29 2 *", D(2097, 1, 1), D(2104, 2, 29, 23, 59)),
        ("59 23 28 2 *", D(2023, 2, 28, 23, 59), D(2024, 2, 28, 23, 59)),
        ("0 0 * 2 *", D(2100, 2, 28, 0, 0), D(2101, 2, 1, 0, 0)),
        # steps on ranges
        ("*/20 9-10 * * *", D(2024, 5, 1, 10, 45), D(2024, 5, 2, 9, 0)),
        ("10-30/10 * * * *", D(2024, 5, 1, 12, 30), D(2024, 5, 1, 13, 10)),
        ("0 */6 * * *", D(2024, 5, 1, 23, 0), D(2024, 5, 2, 0, 0)),
        ("0 9-17/4 * * *", D(2024, 5, 1, 13, 0), D(2024, 5, 1, 17, 0)),
        ("0 0 */10 * *", D(2024, 1, 31, 0, 0), D(2024, 2, 1, 0, 0)),
        ("0 0 25/3 * *", D(2024, 2, 28, 0, 0), D(2024, 3, 25, 0, 0)),
        ("0 0 1 */4 *", D(2024, 5, 2), D(2024, 9, 1, 0, 0)),
        ("0 0 1 10/1 *", D(2024, 1, 1, 0, 0), D(2024, 10, 1, 0, 0)),
        # day of week (2024-05-01 is a Wednesday)
        ("0 0 * * 0", D(2024, 5, 1), D(2024, 5, 5, 0, 0)),
        ("0 0 * * 7", D(2024, 5, 1), D(2024, 5, 5, 0, 0)),
        ("0 0 * * SUN", D(2024, 5, 1), D(2024, 5, 5, 0, 0)),
        ("0 0 * * 6", D(2024, 5, 1), D(2024, 5, 4, 0, 0)),
        ("0 0 * * 1", D(2024, 5, 1), D(2024, 5, 6, 0, 0)),
        ("0 0 * * 3", D(2024, 5, 1, 0, 0), D(2024, 5, 8, 0, 0)),
        ("0 9 * * mon-fri", D(2024, 5, 3, 10, 0), D(2024, 5, 6, 9, 0)),
        ("0 9 * * 1-5", D(2024, 5, 4, 8, 0), D(2024, 5, 6, 9, 0)),
        ("0 0 * * 1/2", D(2024, 5, 3, 12, 0), D(2024, 5, 5, 0, 0)),
        ("0 0 * * 5-7", D(2024, 5, 1), D(2024, 5, 3, 0, 0)),
        ("0 0 * * 5-7", D(2024, 5, 4, 0, 0), D(2024, 5, 5, 0, 0)),
        ("0 0 * * 5-7", D(2024, 5, 5, 0, 0), D(2024, 5, 10, 0, 0)),
        ("0 0 * 2 MON", D(2024, 3, 1), D(2025, 2, 3, 0, 0)),
        ("0 12 * jan sat", D(2024, 12, 1), D(2025, 1, 4, 12, 0)),
        # day-of-month OR day-of-week when both are restricted
        ("0 0 13 * FRI", D(2024, 9, 1), D(2024, 9, 6, 0, 0)),
        ("0 0 13 * FRI", D(2024, 11, 11), D(2024, 11, 13, 0, 0)),
        ("0 0 13 * FRI", D(2024, 11, 13, 0, 0), D(2024, 11, 15, 0, 0)),
        ("0 0 1 * 1-5", D(2024, 5, 31, 12, 0), D(2024, 6, 1, 0, 0)),
        ("0 0 1 * 1-5", D(2024, 6, 1, 0, 0), D(2024, 6, 3, 0, 0)),
        ("0 0 */1 * MON", D(2024, 5, 1, 0, 0), D(2024, 5, 2, 0, 0)),
        ("0 0 1-31 * 1", D(2024, 5, 1, 0, 0), D(2024, 5, 2, 0, 0)),
        ("0 0 * * */1", D(2024, 5, 1, 0, 0), D(2024, 5, 2, 0, 0)),
        ("0 0 15 * 0-7", D(2024, 5, 1, 0, 0), D(2024, 5, 2, 0, 0)),
        ("0 0 30 2 MON", D(2024, 3, 1), D(2025, 2, 3, 0, 0)),
        ("0 0 29 2 sun", D(2025, 1, 1), D(2025, 2, 2, 0, 0)),
        ("0 0 31 * 6", D(2024, 6, 23), D(2024, 6, 29, 0, 0)),
        # only one of the day fields restricted: both must match
        ("0 0 1 * *", D(2024, 5, 1, 0, 0), D(2024, 6, 1, 0, 0)),
        ("0 0 * * 1", D(2024, 5, 6, 0, 0), D(2024, 5, 13, 0, 0)),
        # names
        ("0 0 1 jan,jul *", D(2024, 2, 1), D(2024, 7, 1, 0, 0)),
        ("0 0 1 DEC *", D(2024, 12, 1, 0, 0), D(2025, 12, 1, 0, 0)),
        # aliases
        ("@hourly", D(2024, 5, 1, 10, 59, 59), D(2024, 5, 1, 11, 0)),
        ("@daily", D(2024, 5, 1, 0, 0), D(2024, 5, 2, 0, 0)),
        ("@midnight", D(2024, 2, 28, 23, 0), D(2024, 2, 29, 0, 0)),
        ("@weekly", D(2024, 5, 4, 12, 0), D(2024, 5, 5, 0, 0)),
        ("@weekly", D(2024, 5, 5, 0, 0), D(2024, 5, 12, 0, 0)),
        ("@monthly", D(2024, 12, 15), D(2025, 1, 1, 0, 0)),
        ("@yearly", D(2024, 1, 1, 0, 0), D(2025, 1, 1, 0, 0)),
        ("@annually", D(2023, 12, 31, 23, 59, 59), D(2024, 1, 1, 0, 0)),
        ("@HOURLY", D(2024, 5, 1, 10, 0), D(2024, 5, 1, 11, 0)),
        # whitespace
        ("\t30\t9  * *   1  ", D(2024, 5, 1), D(2024, 5, 6, 9, 30)),
    ]

    def test_table(self):
        for expr, after, expected in self.CASES:
            with self.subTest(expr=expr, after=after):
                got = next_fire(expr, after)
                self.assertEqual(got, expected)
                self.assertEqual((got.second, got.microsecond), (0, 0))
                self.assertIsNone(got.tzinfo)

    def test_accepts_a_parsed_schedule(self):
        for expr, after, expected in self.CASES:
            with self.subTest(expr=expr, after=after):
                self.assertEqual(next_fire(parse(expr), after), expected)

    def test_chained_calls_walk_forward(self):
        t = D(2024, 1, 30, 22, 0)
        seen = []
        for _ in range(5):
            t = next_fire("0 0,12 31,1 * *", t)
            seen.append(t)
        self.assertEqual(
            seen,
            [
                D(2024, 1, 31, 0, 0),
                D(2024, 1, 31, 12, 0),
                D(2024, 2, 1, 0, 0),
                D(2024, 2, 1, 12, 0),
                D(2024, 3, 1, 0, 0),
            ],
        )


class NeverFiresTests(unittest.TestCase):
    def test_impossible_dates(self):
        for expr in [
            "0 0 30 2 *",
            "0 0 31 2 *",
            "0 0 30,31 2 *",
            "0 0 31 4,6,9,11 *",
            "0 0 31 apr *",
        ]:
            with self.subTest(expr=expr):
                with self.assertRaises(CronError):
                    next_fire(expr, D(2024, 1, 1))

    def test_impossible_from_a_parsed_schedule(self):
        with self.assertRaises(CronError):
            next_fire(parse("0 0 31 11 *"), D(2024, 1, 1))

    def test_possible_rare_dates_are_not_errors(self):
        self.assertEqual(next_fire("0 0 29 2 *", D(2101, 1, 1)), D(2104, 2, 29, 0, 0))
        self.assertEqual(next_fire("0 0 29 feb *", D(2096, 2, 29, 0, 1)), D(2104, 2, 29, 0, 0))


class RunnerIntegrationTests(unittest.TestCase):
    def test_runner_uses_cron(self):
        now = [D(2024, 5, 3, 8, 59, 30)]  # a Friday
        ran = []
        registry = JobRegistry(
            [
                Job("standup", "0 9 * * mon-fri", "standup.sh"),
                Job("weekly", "@weekly", "weekly.sh"),
                Job("leap", "0 0 29 2 *", "leap.sh"),
            ]
        )
        runner = Runner(registry, clock=lambda: now[0], execute=ran.append)
        self.assertEqual(
            [(when, job.name) for when, job in runner.upcoming()],
            [
                (D(2024, 5, 3, 9, 0), "standup"),
                (D(2024, 5, 5, 0, 0), "weekly"),
                (D(2028, 2, 29, 0, 0), "leap"),
            ],
        )
        now[0] = D(2024, 5, 3, 9, 0)
        self.assertEqual([r.job for r in runner.tick()], ["standup"])
        self.assertEqual(runner.next_due("standup"), D(2024, 5, 6, 9, 0))


if __name__ == "__main__":
    unittest.main()
