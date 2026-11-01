# RFC 0006: Work sessions the server keeps

- Issue: #216 (roadmap #153, phase 1)
- Related: #7 (checkpoints), #107 (questions), #74 (runs), #108, #167 (client protocol, RFC 0004)
- Status: format 1 of the journal and the `sessions.*` methods implemented in `molt serve`; the interface does not use them yet (see Deferred)

## Summary

Recovery in Molt has been about one run. A run with a checkpoint can go
on after a stop (`molt resume`, `/runs resume`), and the audit log has
every call under the run's trace. A *conversation* of many tasks had no
home of its own: the interface keeps `session.json`, but only while a task
is in flight. When the interface quits with nothing in flight it removes
the file. `RunRequest::session` only groups runs in the audit log, and the
planner does not read it. A person who comes back the next day to go on
with the same discussion starts from nothing.

This RFC adds a **stored session**, which the server owns. It has a stable
id that does not depend on any client's connection. Its journal holds every
task, reply, question, answer and decision, and what each run spent. A
client can list, load, make, archive and delete sessions through the
client protocol. Loading a session only reads its state. It never starts
a task or a model call.

## What a session is

A session is a directory `<data_dir>/sessions/<id>/` (mode 0700). Its id is
`sess_` and 16 hex digits, the form `molt runs --session` already groups
runs by. The directory holds:

- `journal.jsonl` (mode 0600): one entry a line, never rewritten.
- `lock`: an exclusive `flock` held by the one writer.

An entry is `{"v": FORMAT, "seq": N, "at_ms": T, "kind": KIND, ...}`:

| Kind | Holds | Written |
| --- | --- | --- |
| `created` | `workspace`, `title` | first, always |
| `imported` | what the interface's `session.json` held (below) | after `created`, when asked |
| `task` | `run` (the trace), `task` | before `planner.run` is asked |
| `reply` | `run`, the task's `earlier` (task, outcome, summary, changes, applied), `fork`, `checkpoint`, `cost_usd`, `uncounted_calls`, `conversation` | when the planner answers, before the client is answered |
| `failed` | `run`, `error` | when the call fails |
| `asked` | `run`, `question`, `text`, `options` | when an attempt of a task in the session asks |
| `answered` | `run`, `question`, `text` | when `question.answer` is accepted |
| `decided` | `fork`, `decision` (`applied`, `kept`, `discarded`), `paths` | when `result.apply`, `result.keep` or `result.discard` succeeds |
| `archived`, `restored` | nothing | on `sessions.archive` |

What a session holds now is its journal read from the start
(`sessions::Loaded`):

- `entries`: the whole history.
- `earlier`: the answered tasks, as the next task is told of them. A
  result applied later is marked `applied`.
- `unanswered`: tasks accepted with no reply recorded.
- `questions`: their open questions.
- `results`: results in a fork that nobody has decided on yet.
- `spent`: what the session spent.
- `notes`: what to know about what was loaded.

### Messages, tool results and model input

The full history and what the model reads are kept apart:

- **The history** is the journal. A reply keeps the winning attempt's
  conversation as the planner gave it: its tool calls and their results
  in the provider's own form, with the round trips whole. When that would
  make the entry larger than 8 MiB, the conversation is left out and
  `conversation_left_out` says so. The run's calls are still in the audit
  log under its trace, so the entry is a reference that can be read.
- **The model's input** comes from the planner, as before. A task that
  names no `earlier` of its own is told of the session's answered tasks
  (`RunRequest::earlier`). The planner's fitting and its compaction, with
  its evidence references, decide what of that reaches a model call. The
  session adds no summarizing of its own.

### Cost

Unknown is never zero.

- Each reply keeps its own `cost_usd` and `uncounted_calls`, so the cost of
  each run is on record.
- The session's `spent.known_usd` is the sum of the costs that are known.
- `spent.uncounted_calls` counts calls whose cost is not known.
- `spent.unsettled_runs` counts tasks that never said what they spent:
  every `task` until its `reply`, and every one that `failed`.
- `spent.complete()` is true only when both counts are zero.

## Persistence boundaries

- **Written before it is answered.** An entry is written and flushed with
  `fdatasync` before the request it records is answered. If the client was
  told a task started, the session has the task. If it got a reply, the
  session has the reply.
- **A crash cuts at most the last line.** The journal is only appended
  to, so a line a crash cuts off is the last one. That line was never
  confirmed. Reading leaves it out with a note, and the next writer drops
  it before it writes. Damage anywhere else, or an entry in a format newer
  than this build reads, is an error that names the line. The journal is
  left exactly as it is.
- **Nothing is run again.** A `task` with no reply is listed as
  `unanswered`: it was running when the server stopped, or still runs.
  What it did and spent is unknown, and loading never starts it again.
  When its run kept a checkpoint, `runs.list` has it, and going on with it
  is the person's choice, as before. A fork that a pending result named
  and that is gone is marked `gone`, with a note to look at the workspace.
  It may have been applied just before the crash. Molt does not guess.
- **One writer.** The `flock` on `lock` lets one connection, in this server
  or another, hold a session. Another connection's `sessions.load` is
  answered `BUSY`. Reading (`sessions.list`) needs no lock.
- **The workspace stays as it is.** Nothing about a session writes to the
  workspace. Loading one does not touch forks, checkpoints or the audit
  log.

## Lifecycle

- **A normal exit keeps the session.** The client closes the session
  (`session.close`) or the connection ends. Either way the stored session
  is released and stays. The results it still waits on are kept in their
  forks through `session.close` and `task.stop`, just as `keep` would keep
  them.
- **Only an explicit request changes the lifecycle.** `sessions.new` makes
  a session. `sessions.archive` archives one, and with `restore` takes it
  out of the archive. `sessions.delete` removes its directory, and refuses
  while another client holds it. An archived session can be loaded to read
  it, and is not held until it is restored.
- **The connection and the session are separate.** A connection holds at
  most one stored session at a time: the one it last made or loaded. A
  connection that ends releases it.

## Protocol

These are methods added to version 1 of the client protocol (RFC 0004).
They need no `session.open`, because they do not start the services.
`initialize` lists them after version 1's methods.

| Method | Parameters | Result |
| --- | --- | --- |
| `sessions.list` | `archived` (list archived ones too) | `Listed` for each of this workspace's sessions, newest first, with `problem` for one that cannot be loaded |
| `sessions.new` | `title`, `import` | `session` (`Loaded`), `held` |
| `sessions.load` | `id` | `session` (`Loaded`), `held` (false for an archived session) |
| `sessions.archive` | `id`, `restore` | `{}` |
| `sessions.delete` | `id` | `{}` |

While a connection holds a session:

- `task.submit` records the task, its reply or its failure.
- The request's `session` defaults to the stored session's id, so the
  audit log groups the runs under the same id.
- `earlier` defaults to the session's answered tasks.
- `asked` events of its tasks, accepted `question.answer`s and successful
  `result.apply`, `result.keep` and `result.discard` on its results are
  recorded too.

A task that cannot be recorded is refused before it starts. A reply that
cannot be recorded is logged, and its answer still goes to the client,
since the work is done.

A session of another workspace is refused with `INVALID_PARAMS`. Its
first line names its workspace.

`testdata/protocol-v1-sessions.jsonl` records these methods' messages, and
its SHA-256 is pinned in a test, as for version 1's record. The added types
follow the wire rule: new fields get `#[serde(default)]`.

## Relation to what exists

- **Run checkpoints** (`molt_planner::checkpoint`) stay the record of one
  run's attempts. A session names the run, and its `reply` carries the
  `checkpoint` id when there is one. Nothing is copied.
- **The audit log** stays append-only and complete. The session is a
  separate, smaller index of a conversation, and it never replaces the
  log or reads it back.
- **The interface's `session.json`** (`tui/saved.rs`, format 1) stays as it
  is: the interface still writes it and still removes it when it quits
  with nothing in flight.
- **Migration from `session.json`.** `sessions.new` with `import` takes
  the file over, and leaves it in place. The file only ever saved the
  earlier tasks as summaries, the spend and its unknown parts, a task in
  flight and a result that waited. The `imported` entry holds exactly
  that, and the loaded session's notes say that the earlier messages and
  tool results were never saved. The import never claims history that was
  never kept. A task in flight becomes `unanswered`, with its checkpoint
  flag, and its spend stays unknown.
- **Format versions.** Every line carries its format. This build writes
  format 1, and refuses a line of a newer format with a message saying
  which. A later format that changes an entry reads format 1's entries as
  they are. Old entries are never rewritten in place.

## Acceptance

- Ten rounds of tasks, questions, answers, replies and decisions, then a
  normal exit. Loading the same session again gives the whole history, the
  decisions, the result still waiting and the total cost, with one unknown
  call and the task in flight counted as unknown
  (`sessions::tests::ten_rounds_of_tasks_questions_and_decisions_are_all_there_after_a_normal_exit`).
- Through `molt serve`, a stored session keeps two tasks through a server
  killed with SIGKILL, and a third through a normal exit. Loading it makes
  no model call. The third task's model calls are told of the first two,
  and the applied result is marked applied
  (`crates/molt/tests/agent.rs`:
  `a_stored_session_survives_a_killed_server_and_a_normal_exit_and_loading_it_calls_no_model`).
- Killed while it wrote an entry: the cut line is left out and every
  confirmed entry stays. A task with no reply is unanswered, and its cost
  stays unknown
  (`sessions::tests::a_line_a_crash_cut_off_is_left_out_and_everything_confirmed_before_it_stays`).
- A damaged journal, one of a newer format and one never created whole are
  refused with the line and why, and are left exactly as they were. A
  result whose fork is gone is named
  (`sessions::tests::a_damaged_journal_or_one_of_a_newer_format_is_refused_and_left_as_it_is`,
  and the ten-round test).
- Migration from the interface's `session.json` brings what it held and
  says what it never held
  (`sessions::tests::the_interfaces_saved_session_is_imported_without_claiming_what_it_never_saved`,
  `serve::tests::a_stored_session_is_held_by_one_client_outlives_its_connection_and_only_its_word_ends_it`).
- Existing checkpoint recovery is unchanged (`runs.list`, `/runs resume`
  and `molt resume`, and their tests).

## Deferred

- **The interface as a client of stored sessions.** It still keeps its own
  `session.json` and starts a fresh conversation after a quit with nothing
  in flight. Moving it to `sessions.*` touches `tui/app/runs.rs`, the hot
  spot of #155, and comes in its own change.
- **`task.plan` and `task.design`** are not recorded yet. A plan's result
  and spend are not part of the session's totals.
- **Going on from a task's conversation** (`RunRequest::continues`) is the
  client's to ask, as before. The session keeps the conversation, and does
  not fill `continues` on its own.
- **Several clients writing one session**, sessions across hosts, and
  replaying a side effect whose outcome is unknown are out of scope for
  this format.
