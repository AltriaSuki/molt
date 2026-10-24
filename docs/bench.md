# Benchmark

`molt bench` measures what Molt's design buys. It runs Molt as it ships and
a plain agent loop on the same coding tasks, grades every run with tests the
agent is not given, and reports success rate, time to done and cost per task
for each. Both arms are `molt do`, so they share the model, the tools, the
prompts and the cost accounting; the plain loop (`--no-check --attempts 1
--no-memory`) only turns off the done-check, the parallel attempts and
memory.

```sh
molt bench list                                        # the tasks
molt bench run --results runs/base.jsonl --dry-run     # what would run, and the most it could cost
molt bench run --results runs/base.jsonl --max-usd 40  # run it; again to resume
molt bench report runs/base.jsonl                      # the comparison, in Markdown (--json for data)
molt bench validate                                    # check every task grades correctly
```

`run` needs `ANTHROPIC_API_KEY`, a spending limit, and a [price](providers-and-prices.md#model-prices) for
every model it uses; runs get the same prices. Each run gets its own
copy of the task's repo, data dir and empty home directory, `--task-usd`
(default $5) as its budget and `--timeout-s` (default 30 minutes); a run
starts only if what was spent, plus that budget for it and for each run in
flight, stays within `--max-usd`. Results are appended one JSON line per run
as runs finish, with each run's diff, output and grade kept beside them, so
an interrupted benchmark resumes where it stopped. A run that failed on the
model API (an outage, a refused key, no credit) is recorded as an error,
not a failure, and `--retry-errors` runs it again; when several in a row
fail that way the benchmark stops. `--arm NAME=OPTIONS` adds an arm of your
own, such as `--arm one-attempt="--attempts 1"`, and `--trials N` repeats
each run. The report gives each arm's success rate with a 95% interval, the
runs where it said it was done and was not, and compares two arms on the
same runs with an exact McNemar test.

The agent's commands, its check included, run in Molt's sandbox, as
`molt do` runs them by default. The tasks' toolchains are found on `PATH`
and mounted into it read-only (a rustup install with its home, a Node in
`/opt`); the tasks, the runs and your home directory never are, so the
agent's commands cannot see the hidden tests. Before it starts, `run`
checks that the sandbox works here and that each toolchain runs in it.
`--no-sandbox` runs the commands as you instead; results record which.

The tasks are in `bench/tasks`, in Python, JavaScript, Rust and Go, each a
small project with a ticket, hidden tests and a reference solution, split
into `dev` and `heldout` for the evaluator to come. `bench/README.md` has
the format.

Known limits:

- **An agent could game the grade on purpose**, such as with a module that
  shadows the test framework, since the hidden tests run in its workspace.
  With `--no-sandbox` it could also read a task's hidden tests or solution
  by their absolute path. Nothing points it there, and each run's diff is
  kept so such a run can be spotted, but nothing prevents it.
- **Spending can pass the limits a little.** Molt checks its budget before
  each model call, so a run can pass `--task-usd` by up to one call for
  each of its parallel attempts, and the benchmark can pass `--max-usd` by
  as much. `--dry-run` shows the most it plans to spend.
- **Times are only comparable at `--jobs 1`**: parallel runs share the
  machine and slow each other down.
