# scheduler

A small cron-style job scheduler, standard library only (Python 3.11+).

- `scheduler/jobs.py`: `Job` (name, cron schedule, command, enabled, tags)
  and `JobRegistry`.
- `scheduler/config.py`: loads jobs from a TOML file (`[[jobs]]` tables,
  optional `[defaults]`), see the module docstring for the format.
- `scheduler/runner.py`: `Runner` asks `cron.next_fire` when each job is next
  due and runs the jobs that are due on each `tick()`. It takes an injected
  clock, so nothing in the package reads the system time on its own.
- `scheduler/cron.py`: cron expressions (`parse`, `next_fire`, `CronError`).
- `python3 -m scheduler jobs.toml [--after 2024-05-01T09:30] [-n 10]` lists
  when the jobs run next.

## Tests

```
python3 -m unittest -q tests.test_jobs tests.test_config tests.test_runner tests.test_cron tests.test_cli
```

The runner tests use a stand-in for `cron.next_fire`, so they don't depend on
the cron module.
