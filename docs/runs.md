# Runs and sessions

Every task Molt carries out is a run with an id of its own (`trace_…`): the
one `molt do --json` prints as `run`, `molt resume` goes on with, and memory
indexes what it learned under. `molt runs` lists the runs of a project, and
a session groups the runs of one piece of work.

```sh
molt runs                      # every run of this project, oldest first
molt runs -n 5                 # the newest five
molt runs --session            # grouped by session, with each session's cost
molt runs --session release    # only the runs of the session `release`
molt runs show trace_3f2a      # one run in full (a unique start of its id will do)
molt runs --json               # the list as JSON; `show --json` gives a run's request and reply too
```

Like `molt memory`, it works on the project of the current directory (or
`--workspace`, `--project`), in the data dir `molt do` uses there; `--data-dir`
reads another. It starts no service and makes no model call.

## What a run shows

| Column | |
|---|---|
| `RUN` | the run's id |
| `STARTED (UTC)` | when it was first asked for |
| `RESULT` | `passed`, `judged`, `unverified` or `failed` as the run's reply said; `interrupted` when it was stopped (Ctrl-C, `task.stop`, the editor's cancel) or its planner went before it finished; `error` when the planner answered with one; `running` while it has no answer and the services are up. A `*` after it: the run stopped with a checkpoint, and `molt resume RUN` goes on with it |
| `COST` | what the run spent, a check designed for it on its own included, as the audit log settles it ([How a cost is settled](#how-a-cost-is-settled)): its reply's, plus any model call answered late. A run with no reply is counted from its model calls in the log. `+?` after it: some calls' cost is unknown, so this is a subtotal. `?`: nothing in the log says what the run spent, so it is not known; it is never shown as free |
| `SETTLED (UTC)` | when what is known of the cost last changed: the reply, or the latest late answer after it; `-` while it is not known |
| `TIME` | from the request to the answer, added up over resumes |
| `LEARNED` | what memory learned from it: a number of new notes, `nothing` (it found nothing new, or only bore notes out), `failed`, `?` (asked and never answered), or `-` (not asked: `--no-learn`, no memory, a skipped learning) |
| `APPLIED` | `workspace` when the result was merged; `kept` when it was left in its fork (`--no-apply`, the interface's confirm mode); `-` with no result or no changes |
| `TASK` | the task's first line |

`--json` gives the same for each run with every field: the whole first
2000 bytes of the task, the session, the workflow, the verification level,
`uncounted_calls`, the fork a kept result is in, how many files it changed,
the notes memory learned (id, kind and text), the run's summary or error,
and its `settlement`.

`molt runs show RUN` adds what only the run's own messages hold: the whole
task, how its cost was settled (what its reply said, the calls answered
late and when it was settled), its check or criteria, each attempt with its
result, turns, check runs and cost, the files changed, the notes memory gave it (as `molt memory used`
shows them) and the notes it learned.

## Sessions

A run belongs to a session when it is given one:

- `molt do`, `molt ask` and `molt resume` take `--session NAME`. A resumed run
  keeps its session unless another is given.
- The interface puts every run of one opening into one session: a new one
  each time `molt` is opened, or the one `molt ui --session NAME` names.
- `molt serve` gives each client connection a session of its own, unless
  `molt serve --session NAME` names one for all, or a client names one in
  its request (`RunRequest.session`).
- With `molt acp`, each editor session is a session.

A session name is up to 128 bytes, with no control characters. The
session is on the run's request in the audit log; nothing else keeps it.
Sessions only group runs for now: a session cannot be resumed as a whole.

`molt runs --session` lists each session with its runs, the session whose
first run is oldest first and the runs of no session last. A session's cost
is the sum of its runs' known costs, says how many runs' cost is not
known, and when the latest of them was settled. A stored session of
`molt serve` ([serve.md](serve.md#sessions-that-stay)) adds up its runs'
costs the same way when it is loaded or listed.

In the interface, `/runs` lists the project's latest ten runs, with a `•`
by those of the current session, and then the runs that stopped with a
checkpoint, to resume or drop (`/runs resume RUN`, `/runs discard RUN`).

## Where it comes from

Everything `molt runs` says comes from the audit log of the data dir: each
`planner.run` or `planner.plan` request is a run (a plan only proposed is
not), the planner's reply gives its result and cost, a cancel before the
reply marks it interrupted, the replies to `planner.design` on the same run
add the cost of a check designed on its own, memory's reply to
`memory.consolidate` for the run says what it learned, and the planner's
starts and exits end whatever it had not answered.

## How a cost is settled

A run's cost is settled from its model calls in the audit log as well as
from its reply. Every `model.complete` asked under the run's id is one
call, known by its message id, and each call is settled once:

- The gateway's answer settles it: at its cost when the answer has its
  usage and a price, at no cost when it was refused before anything could
  be billed, and as unknown otherwise (no price, a failure, a stop with no
  usage).
- When the kernel stops waiting for a call (its deadline passed, or a
  cancel was not answered in time), it answers the caller itself with the
  cost unknown, and the planner counts the call as uncounted. The gateway's
  answer may still come later, for up to an hour; the kernel then writes it
  to the log without delivering it. Such a late answer is added to the
  run's cost, and the call is taken off its uncounted ones.
- A repeated answer, an index read again or built again, and a restart
  settle nothing twice: the call's first settling answer is the one that
  counts.

A run whose reply says what it spent costs that, plus what late answers
settled since. A run that never said (its planner was killed, it errored,
or it was cut off without an answer) costs what its calls in the log
settled, and every call with no known cost (one never answered, one
answered without a price) is counted as unknown. A run with no model call
in the log and no reply stays `?`. A run still going is not costed from its
calls until it ends. The run's reply in the log is never changed: `molt
runs show --json` gives it as it was, with `settlement.replied` the cost it
said, beside `cost_usd` as settled now.

## The index

So that a listing does not read the whole log every time, what it found is
kept in `runs.jsonl` in the data dir, with the place in the log it read up
to, and the next listing reads on from there. The index holds nothing the
log does not: delete it, or replace the log under it, and the next listing
builds it again from the log, to the same list. Two listings at once take
turns on `runs.lock`. A data dir that cannot be written to is read without
an index. A model call's request holds its whole conversation, so the index
reads only its ids from the log's line and never puts the conversation back
together.
