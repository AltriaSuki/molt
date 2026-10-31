# `molt do`

```
molt do TASK [--check CMD | --no-check | --criterion TEXT...] [--workflow NAME] [--attempts N] [--select MODE]
        [--schedule fixed|adaptive [--escalate MODELS]] [--model M] [--effort E] [--max-turns N]
        [--budget-usd X] [--design-budget-usd X] [--no-apply | --commit [--branch NAME]] [--no-review] [--json] [--stream]
        [--workspace DIR] [--data-dir DIR] [--pass-env NAME]... [--no-memory] [--no-learn]
        [--memory-model MODEL] [--judge-model MODEL] [--checkpoint] [--config FILE]
molt resume [RUN] [--retry] [--list] [--budget-usd X] [--design-budget-usd X] [--no-apply] [--commit] [--json] [--workspace DIR] ...
molt undo [RUN] [--workspace DIR] [--data-dir DIR]
molt ask QUESTION [--model M] [--effort E] [--max-turns N] [--budget-usd X] [--json] [--workspace DIR] ...
```

`molt ask` answers a question about the workspace with one attempt whose tools
only read, in the workspace itself: no fork, no check, nothing changed, and
the report says `outcome: unverified answer`.

| Flag | Meaning |
| --- | --- |
| `--workflow NAME` | `verify` (the default: design or take a check, race the attempts, compare, review, learn), `quick` (one attempt, the given check or none, only the review's fixed rules, no learning), `ask` (answer with read-only tools; nothing changes), `auto` (one model call picks among those three, never `ask` with `--check`; stderr says which), or one the planner's configuration defines. |
| `--check CMD` | Shell command that exits 0 once the task is done, run in each attempt's fork. Default: the planner designs one. |
| `--no-check` | Design no check: one attempt does the task and its result is applied unverified. This is the plain agent loop `molt bench` compares Molt with. |
| `--criterion TEXT` | A criterion the result must meet, for a task no command can check (repeatable, at most 12): the criteria are the run's rubric, an independent judge decides each, and no check is designed (see [Tasks no command can check](#tasks-no-command-can-check)). Not with `--check` or `--no-check`. |
| `--attempts N` | Parallel attempts, 1 to 8 (default 2). The first to pass wins and the others are cancelled, unless `--select` says otherwise. |
| `--select MODE` | How to pick among attempts that pass: `first` (default) takes the first; `wait:<seconds>` gives the others that long after the first passes; `all` lets every attempt finish. Then the passing ones are compared (see below). |
| `--schedule MODE` | `fixed` (the default, or the planner's `MOLT_SCHEDULE`) starts `--attempts` at once; `adaptive` starts one and adds more, up to `--attempts`, on evidence (see [Adaptive schedule](#adaptive-schedule)). |
| `--escalate MODELS` | With `--schedule adaptive`: the models to escalate through, cheapest first, such as `haiku,sonnet,opus`. Default: the planner's `MOLT_ESCALATION_MODELS`, else none. |
| `--model`, `--effort` | Model for the attempts (`opus`, `sonnet`, `haiku` or an id) and how hard it thinks. |
| `--max-turns`, `--budget-usd` | Model turns per attempt, and the spending limit for the whole run. |
| `--design-budget-usd X` | The most designing the check may spend, out of the run's budget and at most all of it (see [Designing has a budget](#designing-has-a-budget)). Default: a quarter of the run's budget, or the share `MOLT_DESIGN_BUDGET_SHARE` sets. |
| `--no-apply` | Keep the result in its fork and print its path instead of merging it. |
| `--commit` | Commit the applied result to git (see below). |
| `--branch NAME` | With `--commit`, commit on a new branch made from the current commit and switched to. |
| `--no-review` | Skip the review of a passing result (see below). |
| `--json` | Print the full result (`planner.run`'s reply in `molt-api`) as JSON. |
| `--stream` | Show model text previews on stderr while the designer and attempts work. The final result still goes to stdout, including with `--json`. |
| `--workspace DIR` | The project (default: the current directory). |
| `--data-dir DIR` | Kernel state, the audit log and forks. Default `~/.cache/molt/<project>-<hash>` (under `$XDG_CACHE_HOME` if set). Must be outside the workspace. |
| `--pass-env NAME` | Pass a variable from your environment to the commands the agent runs, the check included. Repeatable. |
| `--no-memory` | Run without memory: no map or notes for the model, nothing learned. |
| `--no-learn` | Use memory, but do not learn from this run (learning is one more model call after it). |
| `--memory-model MODEL` | The model memory learns from this run with; `same` uses the attempts' model. Default: `MOLT_MEMORY_MODEL`, else the attempts' model. |
| `--judge-model MODEL` | The model that judges a task no command can check against its criteria (below). Default: the attempts' model. |
| `--checkpoint` | Save a checkpoint as the run goes, so a run that stops before it finishes can go on with `molt resume` (below). |
| `--config FILE` | Run your own service setup (see `molt.example.toml`) instead of the default one. |

Progress goes to stderr and the result to stdout. Control characters in text
from the model or from file names are printed escaped.

Build output stays out of the result. Commands build into a private
directory outside the fork (see "Linux command isolation"), and a file a
command still leaves in the fork that looks like build output, in a build or
cache directory (`target-*/` at the top, `__pycache__/`, `node_modules/`,
`.pytest_cache/`, a directory holding a `CACHEDIR.TAG`, ...) or compiled
(`.pyc`, `.o`, `.rlib`, `.so`, a binary executable, ...), is left out unless
the original has it or the attempt wrote it with its file tools. The report
says how many were left out and names a few (`left_out` in `--json`), and in
the interface's confirm mode they are a reason to stop.

A result that passed its check is reviewed before it is applied. Fixed rules
look for the usual ways of gaming a check: deleted test files, new skip or
`only` markers, and changed test configuration. A reviewer model then reads
the task, the check and the diff with read-only tools and answers approve,
concern or reject. Only an approved result is merged; otherwise it is kept in
its fork and the findings are printed. Unverified results (`--no-check`) and
empty changes are not reviewed.

With `--select wait:<seconds>` or `--select all`, attempts go on after the
first one passes, and when more than one passes they are compared on what can
be audited, never on a model's opinion of its own work: first whether the
project's own test suite still passes in each one's fork (the designer names
the suite when it finds one), then fewer changed lines, then fewer changed
files, then which passed first. With review on, the candidates are reviewed in
that order and the first one approved wins, so a result that games the check
gives way to the next. The report and `--json` (`selection`) list every
candidate with its scores, and the choice is noted in the audit log. When
less budget is left than the first passing attempt cost, the run takes it
without waiting, as `first` would. To compare the modes on the benchmark, add
an arm: `molt bench --arm molt --arm select-all="--select all"`.

### Adaptive schedule

By default a run starts all its attempts at once, on its model: an easy task
pays for every one. `--schedule adaptive` (or `MOLT_SCHEDULE=adaptive` for the
planner) starts one attempt, on the cheapest model it may use, and starts
another only when the evidence asks for it and the budget allows it:

- An attempt that **fails or errs**, with none other going, is followed by a
  new attempt on the next model of the escalation list (an escalation), or,
  at the top of the list or at the escalation cap, by one on the same model
  with another approach. The new attempt is told what the failed ones did.
- An attempt that **makes no progress** (its check fails twice in a row with
  the same output) or is **stuck** (every tool call fails three turns in a
  row) gets one attempt alongside it, on the next model up when there is one
  (an escalation). A first failed check is fed back to the attempt as always.
- An attempt that ended on a failure **only the user can fix** holds back new
  attempts (see [Failures that another attempt cannot fix](#failures-that-another-attempt-cannot-fix)):
  something missing from the environment, or out of disk or memory, holds
  back every new attempt; a model endpoint that refused the credential or the
  model holds back new attempts on that model, and the run escalates to the
  next model up when there is one. A compiler error or a failing test goes on
  as above.
- **Nothing new starts** once an attempt passed, the run was stopped or its
  budget spent, its `--attempts` are all started, or the budget left is less
  than a new attempt is assumed to cost. That assumption is the most an
  attempt of the run cost so far (or what its attempts spent per attempt
  started); the planner knows no price list, so an attempt on a model that has
  not run yet may cost more, and the decision says so. Budget is held back for
  the model calls in flight, at the most one call has cost. While any call's
  cost is unknown (an unknown price, a reply without usage, a call stopped
  locally that the provider may still bill, a call a stopped run left
  unanswered), what is left of the budget is unknown, and nothing new starts.

Which settings win: under `fixed` nothing here applies, and it stays the
control to compare with. Under `adaptive`, `--attempts` (or `[tui] attempts`,
or the workflow's count) is the most attempts the run starts in all; a run with
room for one attempt only (`quick`, `ask`, `--no-check`, a kept result to go
on from) runs as fixed. The first attempt's model is `--model` when it is on
the escalation list, which then goes up from there; a `--model` not on the list
keeps every attempt on it; without `--model`, the list's first model takes the
place of `MOLT_PLANNER_MODEL`. A prompt pack whose attempt role names a model
keeps it. `--escalate` replaces `MOLT_ESCALATION_MODELS`; without either, every
attempt has the run's model and the schedule only decides how many start.
`MOLT_MAX_ESCALATIONS` (default 2, `0` for none) caps the escalations of a run.
The other roles (designer, reviewer, judge, ...) keep their own models.

Every decision is a `scheduled` progress event, so the audit log keeps it:
the action (`start`, `parallel`, `escalate`, `retry`, or `hold` with what held
it back), the attempt it starts and the one whose evidence led to it, the
model, the reason, the price assumed and where it comes from, the budget seen
(`limit_usd`, `spent_usd`, `reserved_usd`, `in_flight_calls`, `unknown_calls`,
and `remaining_usd` only when it is known), the escalations against the cap,
and the policy version (`adaptive/1`). `molt do` prints each as a
`schedule:` line. The budget is checked before each model call and the costs
are the gateway's estimates, so this is not an exact cost ceiling. An attempt
that ends after one passed, or reports after it ended, changes nothing: the
first to pass stays the winner. A resumed run keeps each attempt's model and
the escalations made so far.

To compare the schedules on the benchmark, add an arm, such as
`molt bench --arm molt --arm adaptive="--schedule adaptive --escalate haiku,sonnet,opus"`.
How they compare on success, cost and time has not been measured yet.

### Failures that another attempt cannot fix

A done-check that fails because the code is wrong is what attempts are for.
One that fails because `pytest` is not installed, or a model call refused for
a bad API key, would fail the same way in every attempt, on every model, and
each one costs. Molt classifies each failure from the exit code and the
output (or the model call's error), and acts only on what is clear:

| Class | Example | What happens |
| --- | --- | --- |
| `environment` | `sh: 1: pytest: not found` (exit 127), `/usr/bin/python3: No module named pytest`, `error: no such command: \`nextest\``, `Error: Cannot find module 'lodash'`, a dependency that needed the network | The attempt is told it is not the code's fault. The same failure at the next check run ends the attempt; no new attempt starts. |
| `exhausted` | `No space left on device`, `Cannot allocate memory` | As for `environment`. |
| `endpoint` | the gateway's 401 or 403 (`the credential was rejected: check ANTHROPIC_API_KEY`), a 404 for the model | No new attempt on that model; an adaptive run escalates to the next model up once, if there is one. A 429, 5xx or timeout the gateway already retried is not held back. |
| `code` | `error[E0308]: mismatched types`, `FAILED tests/test_a.py::test_b`, `--- FAIL: TestAdd`, `main.c:3:5: error:`, `error TS2322` | As before: the output goes back to the attempt, which keeps all its check rounds, and new attempts start as the schedule decides. |
| `policy` | `Read-only file system`, a capability the kernel refused | The model is told to do it inside what is allowed. |
| `outcome_unknown` | a tool that changes things (one a service provides, not marked read-only) stopped by its deadline | The model is told the call may have taken effect, to check before calling it again and not to simply repeat it. Molt never makes the call again itself. |
| `unknown` | anything else, such as a test that prints only `expected hi, got bye` | Nothing changes. |

A project file that is missing is never the environment's: `sh: 1:
./build.sh: not found`, `Cannot find module './parser'` and Python's
`ModuleNotFoundError` (it may name the module the task asks for) are left to
the attempt.

The rules are bounded on both sides. An attempt stops on the environment only
when two check runs in a row fail with the same line; a different failure
(the attempt wrote the missing script, or the next missing program shows up)
starts the count again. A class never gives a command more access, opens the
network, or resets what the run may spend: it can only stop an attempt early
or hold back a new one. Once you have fixed the cause, run the task again; a
run saved with `--checkpoint` can instead go on with `molt resume RUN --retry`,
which starts a new attempt despite the saved failure.

When no attempt passed, the run's summary says each such failure once, with
every attempt it stopped and what to do:

```
The environment, not the code, stopped attempt 1, attempt 2: `sh: 1: pytest: not found`. Provide it where commands run (`molt env doctor` shows what they get), then run the task again: another attempt without it would fail the same way.
```

Each attempt's report (`--json`, `attempts[].failure`) has the `class`, its
`source` (`check`, `tool` or `model`), the suggested `action` (`fix_code`,
`fix_environment`, `fix_config`, `change_approach`, `retry_later`,
`verify_first`, `none`), the `evidence` line verbatim and the `exit_code`. The
whole output stays in the attempt's `note`. Independently of the class, a
tool call that fails with the same input and the same error as before is
told how many times it has now, and that repeating it unchanged will not help.

### Tasks no command can check

Some tasks have no command that can tell when they are done: write a design
note, reword the error messages, explain a module in its docs. For those the
designer sets a rubric instead of a check: a few concrete criteria the result
must meet, each with a command where one can check that part (a file exists,
a word is gone) and without one where only reading can tell. When the attempt
finishes, the criteria with a command are run in its fork, and a separate
judge model, which never saw the attempt work, reads the task, the criteria,
the attempt's final reply and the diff, with read-only tools, and answers each
remaining criterion met or not with a reason. Unmet criteria go back to the
attempt, as a failed check would, up to the same number of rounds; a result
that still misses one fails the run and is not applied. With more than one
passing attempt, the one that meets more criteria ranks first (see above).

A result accepted this way is reported as passed (judged by a model), not
passed, and the report and `--json` (`rubric`, `judgement`) list each
criterion with what the judge said and whether a command decided it. A rubric
works without any network access beyond the model API. Memory learns less
from a judged run than from a checked one. `planner.design` returns the
rubric in place of a check when it designs one, and a run given a rubric
designs nothing. To give the criteria yourself and skip designing, pass
`--criterion TEXT` once for each:

```sh
molt do 'Write docs/errors.md listing every error message with an input that triggers it' \
  --criterion 'docs/errors.md lists every error message the lexer and parser can report' \
  --criterion 'each message has an example input and the line and column it reports'
```

### Designing has a budget

The designer, and the vetter that checks its work, may spend only a share of
the run's budget: a quarter by default, `MOLT_DESIGN_BUDGET_SHARE` for another
share (such as `0.4`), or `--design-budget-usd X` for an amount, which can be
at most the run's budget (with `molt resume`, it replaces the stopped run's).
It is checked before each of their model calls, as the run's budget is, so the
last call can take designing past it. The attempts keep what is left of the
run's budget, as before. When the share runs out before the designer has
submitted a check, the run stops before any attempt with a failure that says
so, what designing spent, how much of the run's budget is left, and what to
do next: give the check with `--check CMD` or the criteria with `--criterion
TEXT`, run unverified with `--no-check`, or allow more with
`--design-budget-usd`. `molt do` prints it and exits with status 1; confirm
mode in the TUI stops with it and names `/check` and `/design-budget` instead.

Designing does not do the task: only the check files the designer lists reach
the attempts. When it changes other files in its fork anyway, such as writing
the task's own document to try its check on, the run's progress says which,
since that work is thrown away and only spent the budget.

### When the check is wrong

A designed check can expect something the task rules out: a column counted
wrong, a message without the quotes the task asks for. When the designer's
check comes with test files of its own, a separate vetter model, which only
reads, compares those files and the command with the task (and the files the
task points to) and lists each assertion the task contradicts or gives no
basis for, quoting the task. A conflict goes back to the designer as the
result of its `submit_check`, to revise; when its second check still
conflicts, the design fails with the conflicts listed and no attempt starts
(`planner.design` returns the same error, which confirm mode shows). The
vetter's calls count toward the run's cost and budget. A vetter that does not
finish holds nothing back: the check is used, and the design's rationale says
it was not vetted.

An attempt that finds the check contradicting the task can call
`dispute_check` with the assertion, the task's words it contradicts and a
command that shows it, instead of bending the code to the check. That ends the
attempt, which is reported as cancelled with its `dispute`, not as failed; the
other attempts go on. When no attempt passes and one disputed, the reply
carries the `dispute`, its summary leads with it, `molt do` prints it under
`outcome: failed, the check was disputed` and exits with status 1, and the
TUI stops at the check card with the dispute, where `e` edits the check and
Enter runs the task again with it. (The card needs the check in full: a check
designed in auto mode with files of its own stays in the planner, so there the
dispute is only shown.) Attempts judged by a rubric or run without
a check have no `dispute_check`.

### Commits and `molt undo`

With `--commit`, an applied result is committed to git. The commit holds
exactly the files the run merged, as the workspace has them: it is built in a
scratch index from the current commit, so what you staged stays staged and
out of it. Its subject is the first line of the winning attempt's summary, the
rest of the summary is its body. Subjects stay within 72 characters, using a
word boundary when possible and no ellipsis; emphasis is removed while literal
code is kept.
The message ends with trailers:

```
Molt-Run: trace_…
Molt-Check: cargo test
```

`Molt-Run` is the run id, so `git log` leads back to the run's audit trace and
to `molt memory used RUN` for the notes it recalled. `Molt-Check` is the check
the result passed, or `none (unverified)`. Git runs on your side, after the
run; attempts never run it. A file git ignores is left out and named. Nothing
is committed when a file the run changed had changes of yours not committed
before it (the commit would take them in), or when the workspace is not in a
git work tree; the result is still applied, the reason is printed, and the
exit status is 4. A `--branch` name git refuses, or one that exists, stops the
run before it starts.

CLI undo checks the data-directory lock before changing Git; when the interface
holds it, use `/undo` there. A later memory notification failure exits with status
4 and prints the run id to retry.

`molt undo [RUN]` takes back the commit of a run, by default the newest on the
current branch not taken back yet. It makes a new commit that puts the run's
files back as they were before it, with a `Molt-Undo: RUN` trailer; history is
not rewritten, and what you have staged elsewhere stays staged. It refuses
when one of those files changed after the run, or has changes not committed,
and says so rather than overwriting them. Memory is then told: the evidence
the run gave is voided, so it no longer lends any note confidence. Notes only
it showed are forgotten, the notes it bore out or disputed are worked out
again without it, and it is not learned from again. If memory cannot be
reached, `molt undo RUN` again tells it without a second revert.

### Applying a kept result: `molt apply`

```
molt apply FORK [--only PATH...] [--check CMD [--check-file PATH...] | --no-check] [--workspace DIR] [--data-dir DIR]
```

applies a result kept in its fork, such as one `molt do --no-apply` printed,
and drops the fork. `--only` applies just those of its files; since that is not
what was checked, it needs `--check CMD` to run the check again first, in a
fork of the workspace with only the chosen files (and the `--check-file`s the
check needs, which are not applied), or `--no-check` to apply them unchecked.
`--check` without `--only` checks the whole result again, as after you edited
the fork by hand. When the check fails, nothing is applied, the fork stays,
and the exit status is 1. What the review's fixed rules find (a deleted test,
a new skip marker) is printed. The merge request names the chosen files, so
the audit log has them.

### Checkpoints and `molt resume`

With `--checkpoint`, the planner saves the run in the data dir
(`runs/<run>.json`, readable by you only) at every boundary: a model call
starting or answered, an attempt's tool calls about to run or done, a check
run, an attempt ending. The file holds the task, the check, each attempt's
fork and conversation, and the spend so far.

A run that finishes removes its checkpoint. One that stops first keeps it,
along with its forks: interrupted (Ctrl-C, a signal, the process killed) or
out of budget. `molt resume --list` shows the stopped runs of the workspace.
`molt resume [RUN]` goes on with one (the latest by default), under the same
run id:

- Attempts go on from their last checkpoint, in their forks.
- The cost so far counts against the budget; `--budget-usd` gives the run
  more.
- A model call that was unanswered when the run stopped is reported as
  uncounted, never as free.
- Tool calls that a stop cut short are not run again. The model is told
  their effect is unknown and to look before redoing them.
- A check that was running runs again.
- `--retry` leaves the stopped attempts as they are and starts new ones with
  the same check; the earlier record and cost stay.

The result is applied as any run's is, so changes made to the workspace in
the meantime are never silently overwritten: a conflicting path keeps the
result in its fork, with exit status 3. A damaged checkpoint, one in another
format, or one for another workspace is refused with the reason. The ten
newest checkpoints are kept; older ones are removed with their forks when a
new checkpointed run starts.
With `--workflow auto`, a checkpoint is kept unless the planner picks a
workflow that only reads (`ask`), which answers in one go and keeps none.

With `--stream`, previews identify whose call it is, its turn and the model
call: `attempt 1` (attempts are counted from 1, as in the TUI), `designer`,
`vetter`, `reviewer`, `judge` or `router`, as in
`reviewer, turn 3, call msg_…`. The tool lines of each say the same name. These are provisional text, not verified results. Thinking and
partial tool arguments are not displayed; tools execute only after the entire
message, including its JSON inputs, has been received and validated. A broken
or malformed stream fails the call without retrying its partial answer. HTTP
errors before a stream begins still use the gateway's normal retry policy.

Preview events are coalesced, capped at 4 KiB and queued with a fixed bound.
A slow subscriber can miss previews; sequence gaps are reported, and the
complete model reply and final task result remain authoritative. The display
shows cost pending until the call ends, and cost unknown when pricing or a
complete response is unavailable. Cancelled attempts' late previews are hidden;
their remote calls can still run and incur charges (see [the limits](limits.md#cancellation-and-model-settlement)).
This line-oriented mode uses no alternate screen or cursor controls, so it
also works in a narrow terminal or when stderr is redirected.

For `--config`, give the model service a `topic:publish:progress` request capability,
as in `molt.example.toml`; `--stream` refuses a setup without it. The new
gateway events use the request's audit trace and are accepted by the CLI only
from the kernel-authenticated model service. The TUI consumes the same events
through its progress callback, with previews enabled by default.

| Exit status | Meaning |
| --- | --- |
| 0 | Passed (or finished unverified, when no automated check fits the task) and applied, or kept as `--no-apply` asks. |
| 1 | No attempt passed, or molt failed. |
| 2 | The command line was wrong (an unknown flag, `--attempts 9`, ...); nothing started. |
| 3 | Passed (or finished unverified), but the changes could not be merged, e.g. because the workspace changed under them; they are kept in the fork the report names. |
| 4 | A commit or undo could not fully complete. A partial undo may leave Git reverted with memory notification pending; retry the named run. The reason is printed. |
| 128+N | Stopped by signal N: 130 for Ctrl-C, 129 for SIGHUP, 143 for SIGTERM. The services are stopped and the run's forks removed. |

Status 3 also covers a passing result withheld by the review. The report explains
the review findings and names the kept fork for inspection.

### Long tasks

Before each model call, the planner checks that the conversation fits: the
input of the model that call goes to (its context window less the room kept
for the reply, counted from what the model measured on the last call, tool
definitions included) and the bus frame (8 MiB for the whole request). Each
model has its own: a reviewer or explorer routed to a model with a smaller
window ([Role models](models.md#role-models)) is fitted to that window, from
its first call, while the attempts keep the run's. Before the first call to a
model the planner asks the gateway what it declares for it (`model.limits`,
which sends nothing to the provider and costs nothing): the context window
(`context` of a [named endpoint](models.md#named-endpoints)'s model or of
`MOLT_OPENAI_MODELS`) and the most it writes (`max_output`, or the built-in
cap of a Claude model). The room kept for the reply is `MOLT_MAX_TOKENS`,
lowered to that cap, and the call asks for no more. `MOLT_CONTEXT_TOKENS`,
when set, is every model's window. A model whose window nobody declares is
fitted to a guess from its name, `200000` tokens for Claude models and
`128000` for others, and progress says so once a run for any other model,
with how to declare it. When a reply reports a window other than the one its
call was fitted to, progress says so once, and the reply's window is used
from then on (`MOLT_CONTEXT_TOKENS` still wins). Routing a role to another
model never changes the run's budget: every call counts against the one
budget, whichever model made it. When it would not, the oldest turns are left
out, always a whole round trip, so no tool call loses its result and no
assistant turn is changed. A single result too big to fit even alone is cut
down, and says so. Progress notes each time this happens, and the full
conversation stays in the audit log. When even that cannot fit, the attempt
ends with an error that names the limit, the model's input or the transport
frame.

What the left-out turns did goes into a record of its own, a checkpoint
right after the first message: an assistant turn that says the record
follows, then a user turn that holds it. The first message (the task, the
check and the project's context) is never cut or changed, so a provider's
prompt cache, which reads the system prompt, the tools and the messages in
order, keeps it through every fit. The gateway asks the Messages API to
cache up to the first message and up to the checkpoint. A fit starts only
when the next call would not fit, and then leaves turns out until the
conversation is down to half of what the model takes, so the calls after it
send the same checkpoint and turns and read them from the cache until the
next fit. The record is rebuilt at each fit, at most 24 KiB besides its
first section, in this order:

- **What holds now**, from the run's own state rather than from the turns:
  the user's requirements as of the version the attempt works to, the
  protected paths, the done-check, and what the run had spent when the
  record was written. A requirement told to the attempt in a message that
  is later left out is still here. Nothing the turns or a model wrote can
  change this section.
- The files the left-out turns wrote or edited, with the turns that did it,
  and what failed in them: a tool call that failed, a command that did not
  exit 0, a done-check that failed.
- The attempt's notes, when a model writes them (below), marked as written
  from the turns: where they and what holds now disagree, what holds now
  wins.
- A line for each left-out call with its result, and each message,
  shortened, with an id such as `E12`, newest kept first. A read of a file
  that a later turn wrote says so: it is not what the file holds now.

Once turns are left out, an attempt is offered `earlier_result`, last in its
tools: given an id, it returns that call's result or that message whole, as
it was then, 16 KiB at a time. It never runs the call again: a write or a
command read back says it already ran. A file read back is read once more,
by Molt and without the user's tool hooks, only to say whether the file
still reads the same; the attempt is told to read it again for what it
holds now. A role whose `tools` leave `earlier_result` out is not offered
it. Each conversation keeps its items' full text up to 1 MiB, oldest let go
first (their record lines stay), and they are saved with the attempt's
checkpoint, so `molt resume` goes on with the same ids. Thinking blocks are
never kept.

With `MOLT_COMPACT=model`, each fit that leaves turns out also has the
`compactor` ([Prompts and roles](prompts.md#the-prompts)) write the
attempt's notes on them: the changes that stand, the decisions and the
evidence they rest on, what failed and why, the open questions and the next
step, merged with its notes so far. It is one model call per fit, with the
attempt's model unless the compactor's role names one; it is shown the task
and up to 48 KiB of the left-out items as data (less when the model it
calls takes less: the call stays within a quarter of that model's input),
and is not previewed. What
it costs counts toward the run and the attempt, as every call does, and it
is not made once the run's budget is spent. When it fails, declines or does
not submit notes, the notes stay as they were and Molt's own record stands,
which progress says; a failure whose cost is unknown stops the run, as any
model call of unknown cost does. The default, `MOLT_COMPACT=record`, makes
no extra call.

### Project instructions

A run starts by reading the project's instruction files, `AGENTS.md` and
`CLAUDE.md`, from the workspace root and from every directory on the way to a
path the task names (`crates/molt/src/x.rs` brings in `crates/AGENTS.md` and
`crates/molt/AGENTS.md` too). They go into the first message of the check's
designer and of every attempt, as a `<project_instructions>` block before the
task, at most 16 KiB of them; a file under the same text by another name is
shown once. They are data, like memory's notes: they never go into the
system prompt, nothing in them runs, and they change neither the sandbox
nor the done-check. The block says the task wins where they disagree, and
that they win over notes. They are read with `fs.read`, so the audit log
records what each run saw, and the run's progress lists the files.
`--no-memory` does not turn them off; `MOLT_INSTRUCTION_FILES` names other
files, or `none`.

A task often names no path: the attempt finds the code by searching. When an
attempt's `read_file`, `list_files` or `search` reaches a directory the run
has not looked in (a file's own directory and those above it, or every
directory a search matched in), Molt reads that directory's instruction
files and adds them to the call's result, as a `<directory_instructions>`
block that names each file, the directory it applies to (`applies_to`) and
its version (the start of its SHA-256). A rule there applies under that
directory only. The first `write_file` or `edit_file` in a directory whose
files the attempt has not been given is not made: its result gives them,
and the attempt makes the change again if it still fits them. A file is
given once; a file with the same text as one already given is named with
`same_text_as`, and CLAUDE.md beside an identical AGENTS.md not at all.

These files follow the same rules as the first ones: read through `fs.read`
from the workspace (never from the attempt's fork, so an attempt that writes
an instruction file is told that its text is its own change and is still
given the project's), only inside the workspace and never under `.molt` or
`.git`, at most 16 KiB of them in an attempt's conversation at a time, and
only text that grants nothing. A file over that is cut or, with little room
left, named with the reason, as is one that is a link out of the workspace
or cannot be read, and progress says so. At most 8 new directories are
looked in per call; the rest when a later call reaches them. What an attempt
was given is saved with its checkpoint: after `molt resume`, a file that
changed meanwhile is given again with `replaces` naming the version before.
A file whose result a fit has left out of the conversation is given again
the next time its directory is reached. Commands are not covered, since what
a `run` will touch is not known before it runs: to get a directory's files
before changing it with a command, list it (`list_files` with its path). The
designer, the reviewer and an attempt's explorer get none of these
files.

### Syntax errors after an edit

When an attempt writes or edits a file in a language the project model
parses ([Memory](memory.md#memory)), the `fs` service parses the file before and after the change
with the same tree-sitter grammars and returns the syntax errors the change
added, with the lines around each, in the tool's result. An error the file
already had is not reported again, wherever the edit moved it, so a project
that was broken before, or a construct a grammar does not know, does not get
in the way. Nothing outside the service runs and nothing is fetched; a
version that takes over 2 seconds to parse is not checked. By default the
change is still made (`report`); `--syntax-check reject` (or
`MOLT_SYNTAX_CHECK=reject`) leaves the file as it was and returns the errors
as a failed call, as SWE-agent's edit guard does, and `off` does not look.
The check files a run writes into forks are never checked. To compare the
modes, run them as benchmark arms, such as
`--arm reject="--syntax-check reject" --arm unchecked="--syntax-check off"`.
