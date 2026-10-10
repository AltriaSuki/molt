import unittest
from datetime import datetime, timedelta

from scheduler.jobs import Job, JobRegistry
from scheduler.runner import Runner


def every_n_minutes(expr, after):
    """Stand-in for cron.next_fire: the schedule is a number of minutes."""
    n = int(expr)
    base = after.replace(second=0, microsecond=0)
    minutes = base.hour * 60 + base.minute
    return base + timedelta(minutes=n - minutes % n)


class FakeClock:
    def __init__(self, now):
        self.now = now

    def __call__(self):
        return self.now

    def advance(self, **kw):
        self.now += timedelta(**kw)


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock(datetime(2024, 5, 1, 9, 58, 30))
        self.calls = []
        self.registry = JobRegistry(
            [
                Job("fast", "5", "fast.sh"),
                Job("slow", "60", "slow.sh"),
                Job("off", "1", "off.sh", enabled=False),
            ]
        )
        self.runner = Runner(
            self.registry, self.clock, execute=self.calls.append, next_fire=every_n_minutes
        )

    def test_upcoming_is_sorted_and_skips_disabled(self):
        upcoming = self.runner.upcoming()
        self.assertEqual(
            [(when, job.name) for when, job in upcoming],
            [(datetime(2024, 5, 1, 10, 0), "fast"), (datetime(2024, 5, 1, 10, 0), "slow")],
        )
        self.assertEqual(len(self.runner.upcoming(limit=1)), 1)

    def test_tick_runs_only_due_jobs(self):
        self.assertEqual(self.runner.tick(), [])
        self.clock.advance(minutes=1, seconds=30)  # 10:00:00
        ran = self.runner.tick()
        self.assertEqual([r.job for r in ran], ["fast", "slow"])
        self.assertEqual([j.name for j in self.calls], ["fast", "slow"])
        self.assertEqual(self.runner.next_due("fast"), datetime(2024, 5, 1, 10, 5))
        self.assertEqual(self.runner.next_due("slow"), datetime(2024, 5, 1, 11, 0))

    def test_overdue_job_runs_once(self):
        self.runner.upcoming()
        self.clock.advance(minutes=30)  # 10:28:30, fast missed several runs
        ran = self.runner.tick()
        self.assertEqual([(r.job, r.due) for r in ran], [("fast", datetime(2024, 5, 1, 10, 0)), ("slow", datetime(2024, 5, 1, 10, 0))])
        self.assertEqual(self.runner.next_due("fast"), datetime(2024, 5, 1, 10, 30))

    def test_failures_are_recorded_and_do_not_stop_others(self):
        def execute(job):
            if job.name == "fast":
                raise RuntimeError("disk full")

        runner = Runner(self.registry, self.clock, execute=execute, next_fire=every_n_minutes)
        runner.upcoming()
        self.clock.advance(minutes=2)
        ran = runner.tick()
        self.assertEqual([(r.job, r.ok) for r in ran], [("fast", False), ("slow", True)])
        self.assertEqual(ran[0].error, "RuntimeError: disk full")
        self.assertEqual(runner.history, ran)

    def test_forget_recomputes_from_now(self):
        self.runner.upcoming()
        self.clock.advance(minutes=3)  # 10:01:30
        self.runner.forget("fast")
        self.assertEqual(self.runner.next_due("fast"), datetime(2024, 5, 1, 10, 5))

    def test_disabled_job_never_runs(self):
        self.clock.advance(hours=3)
        self.runner.tick()
        self.clock.advance(hours=3)
        self.runner.tick()
        self.assertNotIn("off", [j.name for j in self.calls])


if __name__ == "__main__":
    unittest.main()
