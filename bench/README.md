# Benchmark tasks

`molt bench` runs Molt and a plain agent loop on the same coding tasks and
compares success rate, time to done and cost per task (the main README's
Benchmark section shows how to run it). The tasks in
`tasks/` are also the seed of the evaluator's replay set: every task has an
id, a split, and a content hash that results record, so later runs can tell
whether they measured the same thing.

## A task

```
tasks/<id>/
  task.toml   what the agent is asked, and how the result is graded
  repo/       the starting workspace the agent gets
  hidden/     grading files, copied over the workspace after the agent finishes
  solution/   a reference solution: files laid over repo/ (never shown to an agent)
```

The directory name is the task's id: lowercase letters, digits and dashes,
starting with the language (`py-`, `js-`, `rs-`, `go-`).

`task.toml`:

```toml
title = "Round invoice totals to the cent with Decimal"
language = "python"     # python | javascript | rust | go
kind = "bugfix"         # bugfix | feature | refactor | build
difficulty = "medium"   # easy | medium | hard
split = "dev"           # dev | heldout
tests = "python3 -m unittest -q tests.test_invoice"  # the repo's own tests
check = "python3 -m unittest -q tests.test_invoice tests.test_bench_hidden"
timeout_s = 120         # for one run of `tests` or `check`; default 120
prompt = """
What the agent is asked to do, written like a ticket from a teammate.
"""
```

### How a task is graded

After an agent finishes, the files under `hidden/` are copied into its
workspace, replacing whatever is at those paths, and `check` runs in the
workspace root with `bash -c`. The task passed when the check exits 0
within `timeout_s`. The agent never sees `hidden/` or `solution/`, and is
told nothing about the check.

The check runs with a clean environment: `PATH`, `HOME`, the locale, the
toolchain locations (`CARGO_HOME`, `GOPATH`, `GOCACHE` and the like) and
`CI=1`, nothing else. It has no network access it can count on.

### Rules for a task

- **The prompt is the spec.** It states every behavior the hidden tests
  check: names, signatures, return values, error types and messages, edge
  cases. A correct implementation of the prompt passes; nothing in the
  hidden tests depends on a detail the prompt leaves open. The prompt never
  mentions the hidden tests, and never gives away the implementation.
- **The repo is a real, small project**: a few source files, a `README.md`
  that says what it is and how to run its tests, and its own tests, which
  pass as given.
- **Hidden files are tests**, named with `bench_hidden` in the file name
  (`tests/test_bench_hidden.py`, `test/bench_hidden.test.js`,
  `tests/bench_hidden.rs`, `benchhidden/bench_hidden_test.go`), plus copies
  of any visible test files the grade relies on, so an agent's edits to them
  are undone.
- **The check names its test targets** (modules, files or test binaries)
  instead of discovering every test, so test files an agent added cannot
  break the grade. It also runs the repo's own tests, so a change that breaks
  them fails.
- **Standard toolchains only, no dependencies, no network**: `python3` with
  the standard library and `unittest`; `node` with `node:test`, ES modules,
  and test files listed explicitly (no globs); Rust with `std` only and
  `cargo test --offline`; Go with the standard library, a `go.mod` declaring
  `go 1.21`, and `GOTOOLCHAIN=local` in the check.
- **Deterministic and quick**: no clocks, randomness or timing in the
  grade unless injected; the check takes under a minute on the solution.
- **Validated**: `molt bench validate` checks that every task's starting
  repo fails its check, that the repo with the solution passes it, and that
  the repo's own tests (`tests`) pass on both. CI runs it on every task.

The `dev` split is what an improver may learn from later; the `heldout`
split is for measuring whether what it learned generalizes.

## Writing a task

1. Make `tasks/<id>/` with the four parts above. Write `repo/` first and
   make its own tests pass, then the prompt, then `solution/`, then the
   hidden tests.
2. `molt bench validate --task <id>` until it says the task is valid. It
   works in fresh copies outside the repository, so it never leaves build
   output in the task.
3. Try a plausible but incomplete solution, the kind a hurried engineer
   would write, and make sure the hidden tests catch it.
4. Read the prompt and the repo as the agent will, and check that each
   hidden assertion follows from them.

Changing a task's `task.toml`, `repo/` or `hidden/` changes its hash, so
results recorded before the change are not mixed with results after it.

## What a run records

One JSON line per run in the results file, with the task, its hash, split
and kind, the arm and its options, the trial, the setup (model, effort,
turns, budget, time limit), whether the hidden tests passed, whether the
agent said it was done, wall time, cost and tokens, model calls, Molt's
verdict, and the end of the check's output. Beside the results file, a
`.logs` directory keeps each run's stderr, Molt's JSON result, its changes
as a diff, and the check's output (`--keep-audit` adds the audit log, which
has every model call).
