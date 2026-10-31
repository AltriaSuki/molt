# Improving a prompt

`molt improve` has Molt improve one of the planner's own prompts. The
improver reads how benchmark runs failed and proposes a new version of the
prompt; a gate measures that version against the one runs use now, on the
benchmark's regression set and on its held-out set, and promotes it only
when it is better on both and breaks nothing. Every step is in an audit
log, and `molt improve rollback` undoes a promotion in one step.

```sh
molt improve run --tasks bench/tasks --max-usd 40   # one cycle for the attempt prompt
molt improve status                                  # what is promoted, and every decision
molt improve rollback                                # back to the version before it
```

`run` spends real model calls: a run of `molt do` for each task and each
version, as `molt bench run` does ([bench.md](bench.md)), plus the
improver's. It needs the same things: an API key, `--max-usd`, and a
price for the model.

## One cycle

1. **The live version on the regression set.** Every task of split `dev`
   under `--tasks` runs with the prompt as runs use it now (the built-in,
   or the version promoted last), graded by the task's hidden tests.
2. **Failure patterns.** The runs that failed are grouped by how they
   ended: said it was done but the hidden tests failed, ran out of time, or
   never had a result its check passed. Each failed run brings its ticket,
   its changes, the agent's last words from its audit log, and the hidden
   tests' output. Runs that ended in an error (an API outage, the harness)
   say nothing about the prompt and are left out. If nothing failed, the
   cycle ends there.
3. **A candidate.** The improver gets the current text and the patterns,
   and answers with a new text and why. The text is stored in the registry
   as a new version of the module `prompt.NAME`, named by the SHA-256 of
   its content, with the live version as its parent.
4. **Both versions on both sets.** The candidate runs on the regression set,
   and both versions on the held-out set (split `heldout`). Every run is a
   fresh `molt do` in its own workspace, home and data dir, with a prompt
   pack holding the version under test and nothing else of yours.
5. **The decision**, by the thresholds below. It is written to the audit
   log with the evidence (the counts, the regressions, the costs, the
   results file and its hash) before anything is promoted, whether or not
   the candidate passed.
6. **The promotion**, when it passed: a compare-and-swap on the module's
   live version, so a promotion made meanwhile is never overwritten.

Exit status: 0 when the candidate was promoted or nothing failed; 3 when
the gate did not promote it; 1 on an error, when nothing is promoted. The
results and the logs of every run stay in
`<improvement dir>/evaluations/`.

## The gate

A candidate is promoted only when all of these hold, with the values of
`[improve]` in your configuration (`~/.config/molt/config.toml`):

| Rule | Setting | Default |
| --- | --- | --- |
| Every run of both versions has a result, none ended in an error, and every cost is known | | always |
| The candidate passes no task in fewer trials than the live version, on either set | `max_regressions` | 0 |
| It passes at least as many runs of the regression set | | always |
| It passes at least this many more runs of the held-out set | `min_heldout_gain` | 0 |
| It passes at least this many more runs over both sets | `min_gain` | 1 |
| It costs at most this multiple of the live version's runs | `max_cost_ratio` | 1.5 |

`trials` runs each version that many times on each task (default 1), and
`options` gives every run of both versions the same `molt do` options,
such as `["--attempts", "1"]`; it cannot set what the harness sets
(`--no-sandbox`, `--budget-usd`, ...). `model` is the improver's model.

The prompts that can be improved are those that do not decide whether a
result is accepted: `attempt` (the default), `explorer`, `designer`,
`router`, `steps` and `compactor`. They are the low-risk tier (`mutable`),
so the gate promotes a candidate that passes by itself. The reviewer's,
the judge's and the vetter's prompts guard acceptance: the improver cannot
be pointed at them, the registry refuses a gate's promotion of anything
but a mutable version, and the planner never takes one of them from the
registry. Only you change those, with `molt prompts promote`
([prompts.md](prompts.md)).

## Who can change what

- **The improver** (`molt-improver`) is a candidate service
  ([services.md](services.md)): it runs in the Linux sandbox with no host
  files, no network and its only capability `model.complete`, with a fixed
  budget of 4 calls and 64,000 output tokens per proposal. It cannot hold
  the right to store or promote a version. It is given the regression
  set's failures and nothing of the held-out set, and its answer may hold
  only a text and a rationale: any other field is refused. It cannot reach
  the tasks, their hidden tests, the held-out set, your configuration (and
  so the thresholds), the registry, the audit log, or its own section,
  which `molt improve` writes itself.
- **The gate** is `molt improve` itself, trusted code on your machine: it
  reads the tasks and the thresholds, runs the benchmark's harness, and
  decides.
- **The registry and the audit log** are those of a kernel of their own in
  the improvement directory, `$MOLT_IMPROVE_DIR`, by default
  `~/.local/share/molt/improve` (under `$XDG_DATA_HOME` when it is set).
  It holds the prompt versions, `ModuleStored`, `GateDecision` and
  `ModulePromoted` entries (`molt audit verify` and `molt audit tail` read
  its `audit.jsonl`), and the evaluations.

## How runs use a promoted version

`molt do`, the interface and `molt serve` read the improvement registry's
live versions when they start and hand them to the planner. A file of your
prompt pack with the same name still wins, so `molt improve run` refuses
a prompt your pack replaces: its promotion would never be used. A run says
which roles speak with a promoted prompt (`attempt: speaks with the prompt
attempt (3f2a…) that molt improve promoted`), the audit log names it with
source `registry`, a `--commit` adds its `Molt-Prompt` trailer, and `molt
prompts list` shows it as `promoted by molt improve`. `MOLT_IMPROVE_DIR=none`
runs with none of them. A `--config` file of your own does not get them.

## Rolling back

`molt improve rollback [--prompt NAME]` makes the live version's parent
live again: the version promoted before it, or for the first one the
built-in prompt. It is a promotion by you, in the audit log like the
gate's, and takes effect at the next run. Running it again goes one more
step back.

## Known limits

- Measured only with a fake model so far: the cycle, the gate and the
  rollback are tested end to end against a fake Messages API, not yet on
  the real benchmark with a real model.
- One prompt per cycle and one candidate per cycle; the improver does not
  search or try again after a rejection.
- The improver sees a failed run through its changes, its last words and
  the hidden tests' output, not its whole conversation, and only the
  benchmark's runs, not your own past runs.
- The gate compares pass counts and costs; it does not test whether a
  difference is significant (`molt bench report` has the exact McNemar
  test for that), so set `trials` and the thresholds for the confidence
  you want.
- The registry versions only the planner's built-in prompts, not roles,
  workflows, the pack's own prompts or services, and the protected prompts
  cannot be proposed for a person to promote this way.
