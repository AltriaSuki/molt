# Molt

Molt is a self-improving agent built on a microkernel. The kernel is small, never
changes itself and has no model inside it. Everything that thinks, remembers or
acts is a separate service that the agent can rewrite, test and roll back, the
way a crab sheds its shell to grow.

This repository is at **milestone 3: memory**. `molt do` takes a task in
plain words, settles on a done-check first, runs parallel attempts in private
forks of the project, verifies each with the check and applies the first that
passes. Each run starts from what Molt knows about the project: a map of the
code most relevant to the task, from an index of every definition and
reference that follows the files as they change, and the notes it learned
from earlier runs there. After the run, memory reads it back from the audit
log and keeps what would help next time. The agent cannot rewrite itself yet;
that starts with milestone 5.

## Architecture

```
 mutable services (agent may rewrite)     protected (human approves)
 planner · memory · tools · improver      evaluator · promotion gate
                 \                          /
                  every message, checked and logged
                               |
 ┌───────────────────────── kernel ───────────────────────────┐
 │ message bus · capabilities · supervisor · registry · audit │
 └────────────────────────────────────────────────────────────┘
```

| Part | What it does | Where |
| --- | --- | --- |
| Message bus | Routes requests, replies and events; bounded mailbox per service; per-pair FIFO | `molt-kernel/src/kernel.rs` |
| Capabilities | Unforgeable, budgeted rights to call one target; delegation can split a budget, never grow it | `molt-kernel/src/caps.rs` |
| Supervisor | Runs services as processes with rlimits, restarts with backoff, drains on stop | `molt-kernel/src/supervisor.rs` |
| Module registry | Content-addressed versions; promotion is compare-and-swap; tier rules enforced | `molt-kernel/src/registry.rs` |
| Audit log | Append-only, hash-chained, written before delivery, batched fsync; large payload values (a model conversation's messages) written once and referenced after | `molt-kernel/src/audit.rs` |
| Transports | `Transport` trait with Unix-socket and NATS implementations | `molt-transport` |
| Service SDK | Calls, replies, events, delegation | `molt-sdk` |
| Agent payloads | The request and reply types of every agent service | `molt-api` |
| Model gateway | `model.*`: the Anthropic Messages API; the only holder of the API key | `molt-gateway` |
| File and shell services | `fs.*` and `shell.*`: files, forks, diffs, merges and commands, confined to a root | `molt-tools` |
| Planner | `planner.run`: check first, parallel attempts, verify, apply; `planner.control` steers a run that is going | `molt-planner` |
| Memory | `memory.*`: notes with provenance, learned from finished runs; the project model (definitions, references, repo map) | `molt-memory` |
| Web service | `web.fetch`: pages from the hosts the user allows, over https, as text; off unless allowed | `molt-web` |

The kernel never trusts the sender named in a message. The transport
authenticates each service's endpoint (a per-service secret on Unix sockets,
per-service users and subject permissions on NATS) and the kernel stamps the
sender from that.

## Guarantees and the tests that hold them

| Guarantee | Test |
| --- | --- |
| No service can grant itself a capability or raise its own budget | `crates/molt-kernel/tests/invariants.rs` (`a_service_cannot_use…`, `delegation_can_split…`, `budgets_are_enforced…`) |
| Topic subscriptions cannot publish or delegate broader rights; denied events are audited, and revoked subscription leases stop future events on both transports | `crates/molt-kernel/tests/topic_rights.rs` (`unix_topic_subscribers_cannot_publish_or_widen_their_rights`, `nats_topic_subscribers_cannot_publish_or_widen_their_rights`), `crates/molt-kernel/src/caps.rs` (`topic_delegation_cannot_upgrade_subscription_to_publication`) |
| Check design retains unknown and cancelled model settlement; attempts cannot receive a fabricated remaining budget | `crates/molt-planner/src/runner.rs` (`design_preserves_missing_usage_unknown_prices_and_failed_call_settlement`, `cancelled_designs_return_pending_or_late_settlement_and_drop_their_fork`), `crates/molt/src/tui/app.rs` (`incomplete_design_cost_is_shown_and_cannot_be_spent_as_free`, `attempts_use_the_remaining_effective_budget_without_a_minimum_top_up`) |
| No service can promote a kernel or protected component, change a tier, or add capabilities without a human | `only_humans_promote_protected_and_kernel_components` |
| No service can delete or rewrite audit entries; every message is on the record before delivery | `every_message_is_logged_before_it_is_delivered`, `the_audit_log_has_no_write_path_and_detects_tampering` |
| Audit verification and startup retain one bounded record; tail retains only the requested entries, independent of total log size | `audit_readers_keep_memory_bounded_as_the_log_grows`, `malformed_oversized_and_unterminated_records_are_never_discarded` |
| A failing service never takes the kernel down | `a_service_that_never_replies…`, `a_crashed_and_reconnected…`, `a_flooding_service…`, `garbage_on_the_wire_is_ignored`, `crates/molt/tests/e2e.rs` |
| Both transports behave the same: ordering, at-most-once, sender stamping, isolation, bounded inboxes | `crates/molt-transport/src/testkit.rs`, run by `conformance_unix.rs` and `conformance_nats.rs` |
| `molt do` never runs a config the project ships, and the API key never reaches the commands it runs | `crates/molt/tests/agent.rs` (`the_cli_carries_out_a_task`) |
| Only trusted external configuration installs hooks; formatting cannot bypass a protected done-check | `agent::tests::only_configuration_outside_the_project_can_install_hooks`, `tests/agent.rs::lifecycle_formatting_is_rechecked_and_cannot_weaken_the_protected_check` |
| A conversation sent whole every turn grows the log by its new messages, and reads back byte for byte; a changed or missing referenced value fails verification | `a_conversation_sent_whole_every_turn_is_logged_once_and_read_back_exactly`, `a_changed_or_missing_blob_is_found` |
| Reading the audit log takes a capability, and a read is recorded without a second copy of what it returned | `reading_the_log_takes_a_capability_and_is_recorded_without_a_second_copy` |
| Every learned note cites the messages behind it, and a run is learned from once | `crates/molt/tests/agent.rs` (`a_second_run_starts_with_what_the_first_learned…`), `crates/molt-memory/src/consolidate.rs` |
| A benchmark run is graded by tests the agent is not given, which its own edits cannot replace | `crates/molt-bench/src/grade.rs` (`grading_overwrites_the_agents_copy_of_a_hidden_file`), `crates/molt-bench/src/task.rs` (`overlay_replaces_files_links_and_directories_and_stays_inside`) |
| Every benchmark task fails its check as given and passes it with the reference solution | `crates/molt-bench/tests/tasks.rs` |
| A workspace apply persists preimages before writing, identifies actual post-crash file states, and refuses recovery over later edits | `crates/molt-tools/src/apply.rs` (`first_middle_and_last_write_failures_can_resume_or_undo_idempotently`, `restarting_after_each_durable_stage_observes_disk_truth_and_recovers`, `later_edits_and_candidate_changes_are_preserved_and_reported_by_path`), `crates/molt-tools/tests/fs.rs` (`a_durable_merge_retains_preimages_for_files_links_modes_and_dropped_forks`), `crates/molt/tests/apply.rs` |
| Attempt symbol queries verify fork ownership and content freshness, isolate parallel views, and never update the durable project model | `crates/molt-memory/tests/forks.rs` (`rename_add_move_delete_and_shell_edits_stay_inside_the_attempt`, `unverified_wrong_workspace_and_symlink_forks_are_refused_without_widening_roots`, `an_edit_during_refresh_returns_no_stale_symbols`) |
| A benchmark starts a run only while what it spent, plus the whole `--task-usd` of that run and of each run in flight, fits in `--max-usd`; it refuses models it has no price for | `crates/molt-bench/src/run.rs` (`runs_stop_before_they_could_pass_the_budget`, `a_run_of_unknown_cost_stops_the_benchmark`), `crates/molt-bench/src/cli.rs` (`a_model_without_a_price_is_refused`) |
| A model with no price stays of unknown cost and stops the run; prices come only from the user's environment or data directory, never from a project, and a download that does not validate replaces nothing | `crates/molt-gateway/src/prices.rs` (`the_data_directory_holds_the_snapshot_and_nothing_else_is_read`, `a_download_that_does_not_validate_is_refused`, `malformed_declared_prices_stop_startup_and_name_the_item`), `crates/molt-gateway/tests/gateway.rs` (`a_snapshot_prices_a_third_party_model…`) |
| `molt do --commit` commits exactly the files the run merged and leaves what the user staged alone; `molt undo` takes it back with a new commit, never touching later changes, and the undone run no longer lends any note confidence | `crates/molt/src/git.rs` (`a_run_commit_holds_only_the_merged_files…`, `undo_refuses_files_that_changed_since_the_run`), `crates/molt/tests/agent.rs` (`a_committed_run_can_be_undone_and_memory_stops_counting_it`), `crates/molt-memory/src/notes.rs` (`an_undone_run_lends_no_note_confidence…`) |
| The agent reads the web only when the user allows hosts: over https, from those hosts after every redirect, never from a loopback, private or link-local address; commands stay offline, and every page read is in the audit log | `crates/molt-web/src/allow.rs` (`only_https_urls_on_allowed_hosts_pass`, `only_public_addresses_are_public`), `crates/molt-web/tests/web.rs` (`hosts_off_the_list_are_refused_before_any_request_and_on_every_redirect`), `crates/molt/tests/agent.rs` (`allowed_web_hosts_start_the_web_service_and_offer_fetch_url_only_then`), `crates/molt-planner/tests/planner.rs` (`fetch_url_is_offered_only_to_a_planner_that_may_read_the_web`) |
| A benchmark run that fails on the model API is not counted as the agent's failure, and can be run again | `crates/molt-bench/src/agent.rs` (`a_run_that_failed_on_the_model_api…`), `crates/molt-bench/src/run.rs` (`a_benchmark_that_cannot_reach_the_model_stops`, `an_agent_that_crashes_is_charged_with_it`) |
| Issue CI rejects PR triggers and probes the sandbox before model spend | `scripts/ci/test_issue_to_pr.py::test_only_labelled_open_issues_are_eligible`, `test_sandbox_failure_posts_a_summary_and_never_starts_a_model` |
| A skill source cannot approve itself; only the CLI manages activation and withdrawal preserves evidence | `skills::tests::drafts_need_independent_evidence_and_disable_cannot_be_undone_by_learning`, `tests/service.rs::an_agent_cannot_manage_or_promote_its_own_skills` |
| Project command templates cannot set permissions or modes, and their checks are confirmed before attempts | `tui::app::tests::project_commands_set_one_request_and_confirm_their_check`, `tui::commands::tests::bad_files_conflicts_and_escaping_symlinks_do_not_block_other_commands` |
| Offline embedding models are hash-pinned and cannot reintroduce forgotten or cross-project notes | `semantic::tests::pinned_models_retrieve_synonyms_and_reject_changed_files`, `semantic::tests::synonyms_are_recalled_with_scores_and_snapshots_survive_forgetting` |

## Quick start

```sh
cargo install --path crates/molt     # installs molt, molt-echo and the agent services
export ANTHROPIC_API_KEY=...
cd your-project
molt                                 # the interface: one task after another
molt do "make the parser accept trailing commas" --check "cargo test"
```

Without `--check`, the planner designs a check first (it may write test files
for it) and every attempt is held to it. The kernel on its own, with the demo
echo service:

```sh
cp molt.example.toml molt.toml        # then set its --scratch paths
molt run                              # Ctrl-C to stop
molt audit verify                     # check the hash chain
molt audit tail -n 20
```

To run across hosts on NATS, set `transport = "nats"`, run `molt nats-config`
to generate per-service credentials, put the printed `authorization` block and
`max_payload` setting in your nats-server config, and start `molt run`.

## The interface

`molt` with no subcommand (or `molt ui [--workspace DIR] [--data-dir DIR]
[--pass-env NAME]...`) opens a terminal interface on the workspace. It starts
the services once and keeps them up, so tasks follow one another without a
restart. Type a task and press Enter. A task can run over several lines:
Shift+Enter, Ctrl+J, or `\` then Enter starts a new one, and a paste keeps
its line breaks and indentation (a paste of more than 20 lines shows as
`[pasted #1: 143 lines]` and is sent in full). Ctrl+G opens the task in
`$VISUAL` or `$EDITOR`.

In the default mode, **confirm**, a task can stop twice for you, and does
only when something calls for a look; otherwise it runs through and the log
shows the check and the result as usual. The check stops when no automated
check fits, its baseline already passes or did not run, or its files would
replace files in the workspace. The result stops when it is not `passed`,
changes more than 300 lines or 10 files, deletes a file, touches a protected
path (`.github/`, `Cargo.toml`, `Cargo.lock`, `package.json`, `go.mod`,
`migrations/`), or its diff was cut short. Each stop names its reasons in one
line, such as `stopped: baseline passes · deletes 2 files`. A config given
with `--config` can change these in a `[confirm]` section (`max_lines`,
`max_files`, `ask_paths`), or set `always_ask = true` to stop every time:

1. The planner designs the done-check (`planner.design`) and runs it once on
   the workspace as it is. You see the command, its files, why it fits and
   that baseline. A check that already passes cannot tell a finished task
   from an unfinished one, and is shown in red. Enter starts the attempts
   with it; `e` edits the command, `v` shows the files, `n` runs without a
   check.
2. The attempts run side by side, one box each, with their tool calls and
   check runs as they happen (Tab shows one large). Each box's title shows
   its turn, how long it has run and what its finished model calls cost
   (`turn 7 · 2m14s · $0.18`), and the status bar shows the task's spending
   against its budget (`task $0.52 / $2.00`, in yellow from 80%). A call of
   unknown cost makes the figure a lower bound (`≥ $0.18`). When one passes,
   you see its message, the files it changed and, with `d`, the diff. Enter applies
   it; `k` keeps it in its fork, `n` discards it. `/last` opens the diff of
   the last finished task again, whatever was done with it. When no attempt
   passes, each one shows the end of why it stopped, and a failed check shows
   its last lines in its box; `v` opens the whole story and `r` runs the task
   again with the same check, without designing it anew. `/lanes` shows the
   last task's tool calls after it has ended.

Shift+Tab switches to **auto** (design the check and apply what passes) or
**plan only** (results stay in their forks). Esc stops a task: the services
are stopped, with every command the agent started, and started again. The
forks of attempts that had changed files are kept (`kept 2 · interrupted`),
and with a check known in full `r` starts the task again with it. Ctrl+C
on an empty input, or Ctrl+D, quits only when pressed twice within about a
second; the first press says what quitting keeps.
While the attempts run, you can steer them without stopping them. Whatever
you type and send is added to the task (`+ keep the short -v too`) and
becomes the next version of its requirements; each attempt takes it up at its
next step, and its box says so (`now follows v1`). `/protect PATH...` names
files or directories the result must leave as they are, and `/check CMD`
replaces the running task's check. A result found under older requirements is
never applied as it is: its attempt is told what changed, puts back any
protected file it had touched, and is checked again. Once an attempt has
passed and its result is being applied, it is too late, and the addition is
refused. `/pause` starts no new model call, command or check until `/resume`;
the status bar says what is still finishing (`pausing; waiting for attempt 1:
a model call`) and then `paused`.

The tasks you give one after another make a conversation: each new task is
told what the last five asked, how they ended, what they said and which
files they changed, and whether those changes are in the workspace. Every
task still has its own check and budget. `/new` starts a new conversation.

The log keeps everything a task reports, in full; PgUp and PgDn scroll it,
and Ctrl+Home and Ctrl+End (or Home and End with an empty input) jump to its
start and end.

`/memory [words]` lists the notes about the project, strongest first, with
where each came from: the run, the service version that wrote it and the
messages in the audit log it rests on. `f` forgets one, with a reason, as
`molt memory forget` does, and `p` promotes one to every project, as
`molt memory promote` does. The screen includes quarantined notes for inspection,
marked `[review]`, and temporary experience marked `[temporary]`. The detail
pane shows recall scores, keywords, revision, dependencies and source evidence;
`v` opens the full scrollable record. `a` reviews a note and `c` corrects it:
enter the text (for correction), a reason, and dependency paths separated by
`;`. At the confirmation step, `t` toggles temporary experience, Enter saves,
Shift+Tab returns to an earlier field, and Esc cancels. Saving uses the displayed
revision; concurrent modifications are refused and shown without retrying.
Ordinary planner recall still excludes notes awaiting review.

`/memory-used [run]` opens the immutable recall snapshots for that run in this
project, defaulting to the latest task. The task's memory log shows this command;
`u` in the memory screen opens the latest task's context. Later corrections do
not alter those recorded notes. `/remember TEXT` keeps a preference of yours
for every project. `/attempts`, `/model`, `/effort`, `/budget` and
`/check` set the next tasks' options (`/check` while a task runs changes only
that task); `/memory-model NAME` sets the model
memory learns with (`same` for the attempts' model, `default` for
`MOLT_MEMORY_MODEL` or `sonnet`), and when learning finds nothing the log
says why. `/help` lists the keys. The services'
log goes to `tui.log` in the data dir, and colors are left out under
`NO_COLOR`.

When the interface stops for you (a check or a result to confirm) or the
services go down, it rings the terminal bell and, in terminals that show
them (iTerm2, WezTerm, Ghostty with OSC 9; VTE terminals, foot and urxvt
with OSC 777), sends a desktop notification with a short label such as
`molt: check ready`, never the task's text. It stays quiet while the
terminal has focus or a key was pressed in the last five seconds, and calls
once per stop. `/notify all` also calls when a task ends
(`molt: passed · 3 files`), and `/notify off` turns calling off. The window
title follows what it is doing (`molt · running 2/4`, `molt · waiting:
apply`, `molt · idle`), and the terminal's own title is put back on exit.

Typing `@` starts a file path: the workspace's files (as `.gitignore` leaves
them) are suggested as you type, ↑/↓ picks one and Tab puts it in. The files
a task names this way go with it: the check designer and every attempt get
their text at the start, next to the project's instruction files, and a file
too large is named with its size for the model to read itself. A path outside
the workspace, or one that is not a file in it, is refused before the task
starts.

Ctrl+O opens the run's transcript: each model turn's text, the tools it
called with their arguments, and what they returned, read from the audit log
as the run goes and still there after it ends. With one attempt drawn large
(Tab), `o` opens that attempt's turns alone. `j`/`k` scroll, `{`/`}` jump
between turns, `e` unfolds long text and `v` opens the whole transcript in
`$VISUAL` or `$EDITOR`. Where the audit log cannot be read, the tool calls the
run published show instead.

The interface shows model text previews by default, including while designing
the check. `/stream off` disables previews for subsequent tasks; `/stream on`
enables them again. Each attempt keeps only the latest 2,048 characters, with
terminal controls removed, and folds the text into a brief line when the call
ends. Missing events are labeled as gaps; the interface never reconstructs
them. Previews are provisional: the complete reply and check determine the
result, and cancelled or late events cannot make a result appear.

`/commit on` commits each result once it is applied, as `molt do --commit`
does (see "Commits and `molt undo`" below): the work tree is looked at when
the task starts, and the commit is made after the run applies the result, or
after you accept it in confirm mode. A result kept in its fork, or one that
fails to apply, is not committed. `/commit off` (the default) stops it.

The interface holds the data dir's lock while it is open, so a `molt do` in
the same project is refused until it closes.

## `molt do`

```
molt do TASK [--check CMD | --no-check] [--attempts N] [--select MODE] [--model M] [--effort E] [--max-turns N]
        [--budget-usd X] [--no-apply | --commit [--branch NAME]] [--no-review] [--json] [--stream]
        [--workspace DIR] [--data-dir DIR] [--pass-env NAME]... [--no-memory] [--no-learn]
        [--memory-model MODEL] [--judge-model MODEL] [--checkpoint] [--config FILE]
molt resume [RUN] [--retry] [--list] [--budget-usd X] [--no-apply] [--commit] [--json] [--workspace DIR] ...
molt undo [RUN] [--workspace DIR] [--data-dir DIR]
```

| Flag | Meaning |
| --- | --- |
| `--check CMD` | Shell command that exits 0 once the task is done, run in each attempt's fork. Default: the planner designs one. |
| `--no-check` | Design no check: one attempt does the task and its result is applied unverified. This is the plain agent loop `molt bench` compares Molt with. |
| `--attempts N` | Parallel attempts, 1 to 8 (default 2). The first to pass wins and the others are cancelled, unless `--select` says otherwise. |
| `--select MODE` | How to pick among attempts that pass: `first` (default) takes the first; `wait:<seconds>` gives the others that long after the first passes; `all` lets every attempt finish. Then the passing ones are compared (see below). |
| `--model`, `--effort` | Model for the attempts (`opus`, `sonnet`, `haiku` or an id) and how hard it thinks. |
| `--max-turns`, `--budget-usd` | Model turns per attempt, and the spending limit for the whole run. |
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
| `--memory-model MODEL` | The model memory learns from this run with; `same` uses the attempts' model. Default: `MOLT_MEMORY_MODEL`, else `sonnet`. Useful when that model is not on your endpoint. |
| `--judge-model MODEL` | The model that judges a task no command can check against its criteria (below). Default: the attempts' model. |
| `--checkpoint` | Save a checkpoint as the run goes, so a run that stops before it finishes can go on with `molt resume` (below). |
| `--config FILE` | Run your own service setup (see `molt.example.toml`) instead of the default one. |

Progress goes to stderr and the result to stdout. Control characters in text
from the model or from file names are printed escaped.

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
designs nothing.

### Commits and `molt undo`

With `--commit`, an applied result is committed to git. The commit holds
exactly the files the run merged, as the workspace has them: it is built in a
scratch index from the current commit, so what you staged stays staged and
out of it. Its subject is the first line of the winning attempt's summary, the
rest of the summary is its body, and it ends with trailers:

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

With `--stream`, previews identify the attempt (or check designer), turn and
model call. These are provisional text, not verified results. Thinking and
partial tool arguments are not displayed; tools execute only after the entire
message, including its JSON inputs, has been received and validated. A broken
or malformed stream fails the call without retrying its partial answer. HTTP
errors before a stream begins still use the gateway's normal retry policy.

Preview events are coalesced, capped at 4 KiB and queued with a fixed bound.
A slow subscriber can miss previews; sequence gaps are reported, and the
complete model reply and final task result remain authoritative. The display
shows cost pending until the call ends, and cost unknown when pricing or a
complete response is unavailable. Cancelled attempts' late previews are hidden;
their remote calls can still run and incur charges (see the limits below).
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
| 4 | Passed and applied, but `--commit` could not commit the changes; the reason is printed. |
| 128+N | Stopped by signal N: 130 for Ctrl-C, 129 for SIGHUP, 143 for SIGTERM. The services are stopped and the run's forks removed. |

### Config and secrets

`molt do` reads a config file only when `--config` names one; it never picks
up a `molt.toml` on its own, since one shipped in a project could run any
command with your secrets. Without one it starts the default setup: the
gateway, `fs` and `shell` confined to the workspace with forks in
`<data dir>/work`, memory with its database in `<data dir>/memory.db`, and
the planner, using the service binaries installed next to `molt`. Data dirs that molt creates are private (0700), and it warns about
an existing one that others can read.

Services start with a scrubbed environment: `PATH`, `HOME`, the locale and a
few more, plus what each is configured to receive. `ANTHROPIC_API_KEY` goes to
the gateway only, and the commands the agent runs never see `MOLT_*` or
`ANTHROPIC_*` variables. Add what your builds and tests need with
`--pass-env` (or `pass_env` in a config).

A fork directory set with `--scratch` in a config must be private: the
`fs` and `shell` services create it 0700, and refuse to start on a symlink,
another user's directory or one that group or others may write to (`chmod
700` it). A service that cannot start ends `molt do` with the reason, such as
a missing binary, as soon as the supervisor gives up on it.

Topic permissions use `topic:publish:<name>` or `topic:subscribe:<name>` in a
service's `requests`. The kernel checks publication separately from subscription;
the SDK chooses the corresponding capability. Event destinations remain
`topic:<name>`, so event payloads and subscriptions' topic names do not change.
A subscription's recorded capability and its ancestors must still be live at
delivery; revocation and expiry end access to subsequent events. A denied
publication is recorded in the audit log before the denial is delivered.

Existing `topic:<name>` grants remain compatible and authorize both operations.
They can be delegated as a narrower publish or subscribe grant, but a narrow
grant cannot become the combined grant or the other right. To migrate, upgrade
the kernel and SDK-backed services together, then replace each combined grant
with the operations that service actually uses. The default and example setups
give model/planner publication on progress, fs publication on fs.changed, memory
subscription on fs.changed, and CLI/TUI subscription on progress. Older kernels
and SDKs do not recognize the new permission spellings.

Defaults come from the environment:

| Variable | Read by | Default |
| --- | --- | --- |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` | gateway | required, `https://api.anthropic.com` |
| `MOLT_MODEL`, `MOLT_EFFORT`, `MOLT_MAX_TOKENS` | gateway | `opus`, unset, `32000` |
| `MOLT_MODEL_TIMEOUT_S`, `MOLT_MODEL_RETRIES`, `MOLT_MODEL_CONCURRENCY`, `MOLT_FALLBACKS` | gateway | `1200`, `4`, `16`, `1` |
| `MOLT_MODEL_PRICES`, `MOLT_PRICES_FILE` | gateway | unset, `~/.local/share/molt/prices.json` (see [Model prices](#model-prices)) |
| `MOLT_PLANNER_MODEL`, `MOLT_MAX_TURNS`, `MOLT_BUDGET_USD` | planner | the gateway's model, `50`, `10` |
| `MOLT_MAX_CHECK_ROUNDS`, `MOLT_CHECK_TIMEOUT_S` | planner | `3` check runs per attempt, `900` |
| `MOLT_MAP_TOKENS` | planner | `3000` (the project map a run starts with; at most `32000`) |
| `MOLT_CONTEXT_TOKENS` | planner | the model's context window: `200000` for Claude models, `128000` for others |
| `MOLT_INSTRUCTION_FILES` | planner | `AGENTS.md,CLAUDE.md` (the project's instruction files; `none` reads none) |
| `MOLT_SYNTAX_CHECK` | planner | `report` (see [Syntax errors after an edit](#syntax-errors-after-an-edit)); `reject` or `off` |
| `MOLT_MEMORY_MODEL`, `MOLT_MEMORY_EFFORT`, `MOLT_MEMORY_MAX_TOKENS`, `MOLT_MEMORY_MODEL_TIMEOUT_S` | memory | `sonnet`, `low`, `8000`, `300` |

The planner reads `MOLT_MAX_TOKENS` and `MOLT_MODEL_TIMEOUT_S` too: a model
call ends at that deadline, the gateway's retries included. `MOLT_FALLBACKS`
is `1` or `0` (off) and `MOLT_MODEL_RETRIES` may be `0`; every other number
must be above 0.

### Long tasks

Before each model call, the planner checks that the conversation fits: the
model's input (`MOLT_CONTEXT_TOKENS` less `MOLT_MAX_TOKENS` for the reply,
counted from what the model measured on the last call) and the bus frame
(8 MiB for the whole request). When it would not, the oldest turns are left
out, always a whole round trip, so no tool call loses its result and no
assistant turn is changed. Molt adds a short record of what those turns did
to the first message, after the task and the check, which are never cut. A
single result too big to fit even alone is cut down, and says so. Progress
notes each time this happens, and the full conversation stays in the audit
log. When even that cannot fit, the attempt ends with an error that names
the limit, the model's input or the transport frame.

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

### Syntax errors after an edit

When an attempt writes or edits a file in a language the project model
parses (below), the `fs` service parses the file before and after the change
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

## Trusted lifecycle hooks

An explicitly selected `--config /outside/project/molt.toml` may contain
`[[hooks]]` entries. Files inside the workspace never supply hooks, even with
`--config`; no project file is discovered automatically.

```toml
[[hooks]]
event = "before_apply"
command = "cargo fmt"
blocking = true
timeout_ms = 30000

[[hooks]]
event = "run_finished"
command = "printf 'Molt finished\\n'"
```

Supported events are `before_apply`, `after_apply`, `run_finished`, and
`attempt_failed`. Only `before_apply` may block. Its commands run sequentially
in each passing candidate's isolated fork after acceptance, before ranking, diff, review
and application. The authoritative check files are restored, protected paths
are checked, and acceptance runs again even if a nonblocking formatter failed.
A failing check or blocking hook keeps the result in its fork and applies
nothing. Retained results (including the TUI's confirm mode) are prepared the
same way before they are shown, so the diff/review covers the formatted result.

Notification hooks run in a separate disposable fork and cannot change the
applied workspace. `after_apply` is also sent when the TUI user applies a kept
result; `run_finished` means the planner finished, not that the user accepted a
kept result. All commands use the existing shell sandbox, environment scrubbing,
process-tree cleanup and bounded output. They do not gain host network or
notification access. Shell requests/results record output, exit status and
elapsed time in the audit; planner notes identify the lifecycle event. Timeouts
are 1–600000 ms, commands at most 8192 bytes, and at most 32 hooks are accepted.
An interrupted or resumed run may deliver notifications again; use idempotent
notification commands. Hooks are not part of the model-editable RunRequest.

## Projects

A project is what molt keeps memory for. Each has a stable id and a name in
the user's registry (`$XDG_DATA_HOME/molt/projects.json`, else
`~/.local/share/molt/projects.json`), and any number of directories: one
usually, several for clones, or none. The first run in a directory registers
it under the directory's name, keeping the data dir it had before, so nothing
is lost on upgrade. Notes about any of a project's directories are kept under
its id, so clones share them and a moved directory keeps them once molt is
told where it went; the data dir, with the audit log and stopped runs, stays
with the project too. Nothing in a project's directory decides which project
it is.

```sh
molt project list                         # ids, names, directories, output and data dirs
molt project path "Japan trip"            # absolute working directory, also accepts an id
molt project new "Japan trip"             # a project with no folder
molt project new site --path ~/src/site   # a directory under a name of your own
molt project add site ~/src/site-2        # another clone, with the same notes
molt project move site ~/code/site        # the directory moved (--from DIR for one of several)
molt do "plan five days in Kyoto" --project "Japan trip" --no-check
```

`--project ID|NAME` stands in for `--workspace` in `molt do`, `molt resume`,
`molt ui` and `molt memory`. `molt project path ID|NAME` prints its absolute
working directory for finding files or `cd "$(molt project path NAME)"`. Text
results from `do` and `resume` name the applied directory, and JSON results
include `workspace`. A project with no folder works in a private one
of its own, `<registry dir>/projects/<id>/files`, which is where its
research and writing end up. With `--config`, the registry is left out and
notes are kept by the workspace's path, as before.
### Reading the web

Commands have no network, so an attempt cannot look up an unfamiliar API.
`--allow-web HOST` (repeatable, for `molt do`, `molt resume` and the
interface) lets it read pages from those hosts: `docs.rs` is that host,
`*.python.org` any host below python.org. It starts a `web` service of its
own and gives the planner the capability to call it; attempts and the check
designer are then offered `fetch_url`, and without the flag they are not.
The service fetches over https on the default port only, checks every
redirect against the list, resolves names itself and never connects to a
loopback, private, link-local or other non-public address, and uses no
proxy. It reads at most 2 MiB of a page and returns at most 100 KB of text,
with HTML turned into plain text and links kept; other content that is not
text is refused. The model is told the page is data, not instructions. Each
URL and the text read from it go through the kernel, so the audit log has
them for replaying a run without the network. In a config file, the
same is a protected service:

```toml
[[service]]
name = "web"
tier = "protected"
exec = { command = "molt-web", args = ["--allow", "docs.rs", "--allow", "*.python.org"] }
provides = ["web.*"]
```

with `{ target = "web.fetch", budget = { calls = 1000000 } }` in the
planner's `requests`.

## Memory

Memory holds two things for each project.

**The project model** is every definition and reference in the project's
source (Rust, Python, JavaScript, TypeScript, Go, Java, C and C++, parsed with
tree-sitter), in SQLite. An index parses only files whose contents changed:
on this repository a first index takes about 170 ms and an unchanged one
2 ms; a 3,000-file project takes about 1.5 s, then 40 ms. It leaves out other
projects' code and build output (`node_modules`, Python virtualenvs, cache
directories such as Cargo's `target`) and Molt's data directory, gives each
file at most 2 seconds, and writes as it goes, so a stopped index keeps its
progress. It follows the files the `fs` service changes as they change. From it, a run starts with a map of
the code most relevant to its task, ranked the way Aider ranks it (PageRank
over references, weighted toward what the task mentions), and attempts can
ask where a name is defined and used (`find_symbol`).

`find_symbol` uses a disposable symbol view of its own attempt fork, including
file-tool and shell changes. Memory asks `fs.view` to verify the fork's original
workspace and content version before and after refreshing. Every source file
is re-hashed, while only changed contents are parsed again, so a script that
preserves size and mtime cannot leave stale symbols. Replies identify the
baseline and current version; a concurrent edit or failed refresh returns an
explicit instruction to use `search`/`read_file`. Each view has an independent
in-memory database, with at most eight cached views; fork symbols never enter
the persistent project/notes database. The default setup grants only `fs.view`
to memory for this purpose. Custom configs must set `molt-memory --forks` to
the same trusted absolute scratch directory used by fs, and grant the planner
`memory.fork_symbols`; ordinary memory workspace access remains confined to
`--root`. Cold views index the whole fork; eviction or service restart rebuilds
them on the next query. Oversized, binary and unsupported files remain outside
the symbol model, and responses expose skipped/truncated results.


**Notes** are one-sentence facts, conventions, decisions, preferences and
lessons. After a run, memory reads it back from the audit log, boils it down
to numbered steps, and has the model write what would help a later task, each
note citing the steps, and so the logged messages, it rests on. A note the run
bore out gains confidence; one it proved wrong is kept beside its correction,
both less certain, until more evidence settles it. Notes carry the version
of the service that wrote them, so rolling a version back retracts what it
learned. Recall ranks notes by keyword match (SQLite FTS5 with stemming),
confidence and recency.

Every context or `recall` tool lookup the planner uses records an immutable
snapshot: note IDs and revisions, original text, source runs and audit message
IDs, the query, and keyword relevance, confidence and recency used for ranking.
Later reinforcement, correction, forgetting or version retraction does not
rewrite that run's view. A run that recalls notes prints its ID on stderr;
`memory used` inspects it, scoped to the current project. Older runs have no
snapshot and are reported as such rather than reconstructed from today's notes.

```sh
molt memory show [WORDS...]          # notes about the project, best first
molt memory forget NOTE_ID --reason "the build moved to make"
molt memory map [WORDS...]           # the map a run with that task would start with
molt memory used RUN_ID --json       # the notes actually returned for this run
molt memory review NOTE_ID --rev 2 --reason "checked the build command" --depends-on Cargo.toml
molt memory correct NOTE_ID "Tests use pnpm test." --rev 3 --reason "the command changed" --depends-on package.json
```

`forget` goes through the kernel, so the audit log records it; the note stays
as a tombstone with its reason.

`show` includes revisions, sources, recall reasons and notes awaiting review.
`review` and `correct` go through the kernel too, require a reason and the
revision printed by `show`, and reject concurrent changes. Corrections keep
the original note and evidence as a tombstone and link the replacement to it.
Forgotten, corrected, retracted and undone text cannot be automatically relearned.

Use repeatable `--depends-on` arguments to attach project-relative files,
including build configuration or a symbol's source file. These are explicit
file dependencies, not inferred semantic symbol dependencies. Each review
replaces the dependency list and records SHA-256 content fingerprints. A
change, deletion or unreadable dependency marks only its linked notes as
awaiting review; ordinary recall excludes them. Checks run before recall and
when `fs.changed` arrives, including configuration files outside the source
index. Restoring a file does not reactivate a note: confirm it with `review`
using its current revision and dependency list. Unrelated changes leave it
alone. Files are limited to 1 MiB, at most 16 per note; symlinks, paths outside
the project and Molt's own data are refused.

### Your notes, for every project

Notes about you, such as "Answer in Chinese.", belong to no project. They
live in your own layer, `user.db` next to the project registry
(`$XDG_DATA_HOME/molt`, else `~/.local/share/molt`), and every project's
recall merges them with its own, best first. Each recalled note says which
layer it came from, in `show`, on the memory screen and in a run's snapshot.

```sh
molt memory remember --global "Answer in Chinese."    # yours, everywhere
molt memory remember "Deploy with make release."      # this project's
molt memory promote NOTE_ID --reason "every project of mine builds with make"
```

Writing one goes through the kernel and is audited. Learning never writes
there: a run's record holds text the project chose. When the model thinks a
learned note holds everywhere, it only proposes it (`proposed for every
project` in `show`, and in what a run reports learning); `promote` copies it
once you say why. The project note stays, marked with where it went, and the
copy keeps its evidence, so rolling back the version that learned it retracts
the copy too. `forget` and `correct` work on your notes from any project.
Project databases from before need no migration.

`review --temporary` marks a workaround as temporary experience, and the
model is explicitly told to verify it before reuse. Review metadata does not
change the original learning evidence. All these commands accept `--workspace`
and `--data-dir`; `show` and `used` also accept `--json`. The database upgrades
from schema 1 to schema 2 without rewriting existing notes; older binaries
refuse the newer schema.

### Model providers

The gateway keeps the existing Anthropic Messages adapter and also supports
OpenAI Chat Completions. Provider choice belongs to the gateway, not the
planner. With `OPENAI_API_KEY` set, use `molt do "task" --model openai/gpt-4.1`;
`MOLT_PROVIDER=openai` sets the default provider and default model to GPT-4.1.
`anthropic/opus`, `anthropic/sonnet` and `anthropic/haiku` explicitly choose
Anthropic. Existing unprefixed aliases and Anthropic configuration are unchanged.
Both keys may be configured together; only the model service receives them.
Shell commands and sandbox environment policies exclude `OPENAI_*` too.

`OPENAI_BASE_URL` defaults to `https://api.openai.com` and is an origin, without
`/v1`. HTTPS is required except for loopback test servers. Redirects are refused.
Changing an endpoint does not imply support for another provider's protocol.
The optional `MOLT_MODEL_ALIASES` JSON object maps a plain alias to exactly one
provider/model, for example `{"fast":"openai/gpt-4.1-mini"}`. It is trusted
configuration, never loaded from project files.

GPT-4.1, mini and nano (including their 2025-04-14 snapshots) have built-in
capabilities for text, function tools, streaming and structured output, with a
32,768 output cap and no effort parameter. Other OpenAI models require an
explicit `MOLT_OPENAI_MODELS` JSON declaration, for example
`{"my-model":{"tools":true,"streaming":true,"output_schema":false,"efforts":["low","medium","high"],"max_output":16000}}`.
A declaration must describe a model that actually implements Chat Completions;
it does not adapt other wire protocols. Unsupported requested capabilities fail
before HTTP, including effort on Haiku rather than silently dropping it.

The adapter converts unsigned text and function tool histories, preserving
call IDs, tool errors and tool results. Signed thinking, native citations,
media and other provider-native blocks cannot cross providers; start a new
conversation instead. OpenAI text/tool replies use the same planner blocks,
with provider and native finish reason in `stop_details`. Streaming buffers tool
JSON until completion, requires `[DONE]` and final usage, and never retries a
partial response. Cached prompt tokens are separated from uncached tokens for
pricing; an unknown returned model or missing usage is never charged as zero.
The spending limit remains a best-effort pre-call limit, not a guaranteed hard
remote billing cap. Cancellation stops local HTTP and leaves remote billing
unknown, as on the Anthropic path.

The implementation follows the [Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
and [streaming contract](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).

## Model prices

The run budget (`--budget-usd`) holds only when molt knows what each model
call cost, so a call to a model with no price stops the run after it: an
unknown cost is never counted as free. Molt knows the prices of a few
thousand models, so third-party APIs reached through `ANTHROPIC_BASE_URL`
work too. It looks a model up, in order, in:

1. `MOLT_MODEL_PRICES`, prices you declare in US dollars per million tokens:
   `glm-4.6=0.6,2.2;deepseek-chat=0.28,0.42` (input, output), or four
   numbers to give cache write and cache read prices as well.
2. The snapshot `molt prices update` downloaded, in
   `~/.local/share/molt/prices.json` (`$XDG_DATA_HOME/molt` when set;
   `MOLT_PRICES_FILE` names another file), or, until there is one, the
   snapshot built into this release.
3. The Anthropic prices built into the gateway.

```sh
molt prices show deepseek-chat         # what a model costs and where that price came from
molt prices update                     # download LiteLLM's price list, check it, and use it
molt prices update --source openrouter # or OpenRouter's own prices
```

`update` refuses a download that is not a price list (not JSON, the wrong
shape, a negative or non-finite price) and leaves the current snapshot as
it is; it points out models whose price moved more than twofold. Snapshots
record their source, URL, SHA-256 and download time, and every model call's
reply names the entry and snapshot its cost came from. A dated id
(`-YYYYMMDD`) and a provider prefix (`openrouter/`) fall back to the bare
model id; a bare id that only several providers list gets the highest of
their prices. When a source leaves out cache prices they are estimated high:
a cache write at 1.25 times input and a cache read at the full input price.

Prices are read once when the gateway starts, never from the project
directory, and never downloaded during a run. The sandbox hides home
directories from the commands an agent runs, so they cannot change the
snapshot; with `--no-sandbox` they run as you and could.

## Benchmark

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

`run` needs `ANTHROPIC_API_KEY`, a spending limit, and a [price](#model-prices) for
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

## Tests

```sh
cargo test --workspace
```

The NATS conformance and end-to-end tests need a `nats-server` binary. Set
`MOLT_NATS_SERVER=/path/to/nats-server` or put it on `PATH`
(`go install github.com/nats-io/nats-server/v2@v2.15.1`, using the Go version
in `.github/ci-versions.toml`). Without it they print
a notice and pass; CI always runs them.

The Rust release and components are pinned in `rust-toolchain.toml`, and CI
reads that file too. Go and nats-server are pinned in `.github/ci-versions.toml`;
Go cannot fetch a newer toolchain automatically. NATS binaries use an exact
version/OS/architecture/Go cache key and are version-checked on every run,
including cache hits. Actions are pinned to commit SHAs. Toolchain upgrades
belong in a separate PR so ordinary changes keep the same tool versions when
rerun. The runner image and apt's isolation backend still receive OS updates;
these pins do not claim a bit-for-bit identical machine image.

`crates/molt-bench/tests/tasks.rs` validates every benchmark task, which takes
`python3`, `node` (20 or newer), `go` and `cargo`. A missing toolchain skips
its tasks with a notice, except on CI.

## Roadmap

1. **Kernel**: bus with both transports, capabilities, supervisor, registry, audit log, invariant and conformance tests.
2. **A working agent**: Anthropic model gateway, planner, file and shell tools, parallel verified attempts, check-first.
3. **Memory** (this milestone): semantic store, recall, the living project model, consolidation.
4. **Evaluation**: deterministic replay from the audit log, regression and held-out suites, the promotion gate.
5. **Self-improvement, low risk**: prompts and skills only, auto-approved when the gate passes.
6. **Self-improvement, code**: Wasm services under wasmtime, sandbox builds, canary runs, hot swap.

## Recovering a workspace apply

Every merge that writes files stages private, fsynced preimages and candidate
contents under `.molt/applies/apply-<id>` in the trusted scratch area before
its first workspace write. Its journal records preparing, staged, applying,
applied, undoing, undone or needs-recovery phases. `fs.merge` adds an optional
`operation` ID to its reply; failures name the same ID, and the planner keeps
the candidate and its existing exit status 3. `fs.applies` reports both the
last durable phase and each path's observed pending/applied/conflict state,
so a crash between a file rename and a journal update cannot hide a write.

With the default setup, use the project's data directory followed by `/work`
as `SCRATCH`. These host commands need neither a model key nor a running
kernel. Ordinary fs reads/writes cannot access this recovery data; service calls to `fs.applies` / `fs.recover` still go through the
kernel's normal capability checks and audit. Offline commands update the
same durable journal, without creating a bus audit record.

```sh
molt-tools apply --root /path/to/project --scratch /path/to/data/work list
molt-tools apply --root /path/to/project --scratch /path/to/data/work inspect apply-ID
molt-tools apply --root /path/to/project --scratch /path/to/data/work resume apply-ID
molt-tools apply --root /path/to/project --scratch /path/to/data/work undo apply-ID
```

Resume finishes the originally staged candidate. A source fork that still
exists must have the same fingerprint; changes are refused. If run cleanup
removed it, recovery uses the immutable, checksum-verified postimages instead.
Undo restores the exact old file contents,
symlink targets and permissions, including files deleted by the apply, and
removes added files. Undo remains available after a successful merge drops
its fork. Both actions are idempotent and refuse a path whose current bytes,
link target or permissions differ from both recorded states, preserving later
user edits. Incomplete staging has made no workspace writes: undo abandons
it; resume asks for a new merge. Corrupt staging blobs never replace a file.

A process lock serializes apply/recovery commands sharing a scratch directory.
Exclude other editors and commands that write the workspace or candidate
while applying or recovering: path checks detect edits between operations,
but cannot make check-and-rename atomic against arbitrary external writers.
The operation is a sequence of durable single-file renames/deletions, not an
atomic directory swap. Files/symlinks and their existing merge permission
semantics are supported; empty directory metadata and file-to-directory
replacements, ownership, hardlink identity and extended attributes are not tracked. Recovery materials are retained after success
for explicit undo; they consume disk space until the user removes a completed
operation directory after inspecting it and deciding that undo is no longer
needed. Deleting an unfinished record loses its recovery material.

## Known limits

- The default shell requires Linux x86_64 with bubblewrap 0.9+ and usable user namespaces. Unsupported hosts must explicitly choose --no-sandbox; namespace policy never silently falls back.
- Model text streams with `--stream` in `molt do` and by default in the TUI (`/stream off` disables it); thinking is not shown and tool calls wait for the complete message. Preview delivery is best effort, and the final reply remains complete.
- The audit log records every message, file contents and model conversations included, and is never rotated; it grows with every run in a data dir.
- Audit startup, verification and nonempty tail reads still scan the whole log, but memory is bounded by one record plus the requested tail. A record is limited to 16 MiB including its newline; oversized, malformed or unterminated records are refused, including on restart. The writer stops before persisting an oversized entry. `tail -n 0` opens the file without scanning; `read_all` intentionally retains every returned entry. Tail validates record syntax throughout the file, but only verification and startup check the hash chain.
- Reading one trace back from the audit log scans the log from the start: there is no index yet. One read looks through at most 256 MiB and keeps to its deadline, and two run at once.
- The kernel drops a message nested more than 100 levels deep, so every logged message can be read back.
- After an interrupted run, forks are removed only from a scratch dir inside the data dir (the default); a configured scratch elsewhere may be shared with other runs and is left alone. A `molt do` killed outright (SIGKILL) removes none. A run with `--checkpoint` that stops keeps its forks for `molt resume`.
- A rubric's model-judged criteria are a model's reading, not a proof: the judge can be wrong, and nothing but the criteria with a command checks the result. In the interface, a constraint that adds a check is refused while a run is being judged by a rubric.
- Only `molt do --commit`, `molt resume --commit` and the interface's `/commit on` commit a result, always on the current branch (`--branch` is for `molt do` only). `molt undo` takes back only those commits, and refuses a file changed after the run rather than merging the revert into it (use `git revert` then).
- A checkpoint is saved whole at each boundary, so a long conversation makes each save bigger. The interface (`molt ui`) does not save checkpoints yet.
- Promotion updates the registry pointer but does not yet restart the running service; hot swap with in-flight draining lands with milestone 6.
- Call-cycle checks at the gate remain future work.
- Default recall matches keywords with stemming, so differently worded notes can be missed. Optional offline word-vector recall needs a reviewed hash-pinned local model; its mean-pooling backend does not understand word order or supply a pretrained vocabulary.
- Notes learned from a run stay with that project, preferences included: a run's record holds text the project's files and commands chose. Only notes you write or promote yourself are shared between projects. Clones of one project share its data dir, so molt runs in one of them at a time.
- The project model indexes the first 20,000 source files of a project, in walk order; `memory.index` reports when a project has more. A file that takes more than 2 seconds to parse keeps the symbols found by then.
- Legacy `topic:<name>` capabilities still permit both publication and subscription; use the explicit rights to reduce them. Events already accepted for delivery may arrive after revocation.
- Model prices are as current as the last `molt prices update` (or the release's snapshot) and as right as the community list they came from; a price that changes mid-run is not picked up until the next run. Models that LiteLLM lists only with tiered or regional prices, such as some Qwen models, have no price until declared in `MOLT_MODEL_PRICES`.
- The web service has no search: an attempt reads pages whose URL it knows or finds in pages it read. It does not use a proxy, so a network that only reaches the web through one cannot use it, and it reads pages as served, without running their scripts.

### Cancellation and model settlement

Dropping an SDK `PendingReply` requests `kernel.cancel` for that request. The kernel verifies the original caller, sends a kernel-only control message, and keeps the request open for one final cleanup or settlement reply. Cancellation is idempotent and does not affect another request. SDK services can use `serve_cancellable` for cleanup; ordinary concurrent handlers stop locally on cancellation. Cancelled queued work does not start.

Link loss ends concurrent admission immediately, even if every slot is occupied. Cooperative handlers get up to two seconds to clean up before they are aborted; queued work does not start after disconnection. Dropping a server cancels and removes its active request registrations. Shell shutdown closes command admission before sending TERM, and dropped shell request futures also abort their output readers.

Dropping the SDK service itself closes every pending call's waiter and requests cancellation without awaiting a reply. Background cancellation after a drop is best effort and bounded to one second, so a stalled transport cannot retain the link forever. Remote cancellation and unsettled usage remain unconfirmed.

The planner gives each attempt its own cancellation token. A winner stops competing attempts; budget exhaustion or unknown model cost stops new actions throughout the run. The one exception is a call cancelled because its attempt was stopped while the run went on, such as one that lost the race: its cost is unknown too, and is reported in `uncounted_calls`, but it does not stop the winner's review and apply, so a run can pass its budget by up to one call for each attempt it stopped. Shell cancellation and timeout send TERM to the process group, allow 500 ms for cleanup, then send KILL and reap the leader before replying. Service shutdown uses the same grace. Process groups alone cannot contain a program that deliberately creates a new session; OS isolation is tracked by #9.

The gateway closes its local HTTP request on cancellation and audits a correlated settlement record with `local_stopped`, `remote_cancel_confirmed`, `usage` and `cost_usd`. Anthropic Messages has no remote cancellation acknowledgement here, so remote cancellation and missing usage remain unknown. A completion already received wins the race and is counted once. After a kernel timeout, one authenticated late model reply can still be audited for up to one hour, without delivering a second run result; the late-settlement registry is bounded and temporary, while accepted records persist in the audit log. `molt audit --help` describes querying the log.

Run `cost_usd` is the known subtotal. `uncounted_calls` now includes pending calls, errors without usage, and replies without a known price. The terminal explicitly labels the known cost and incomplete settlement; automatic memory learning is skipped when settlement is incomplete. This is an estimated spending limit, not a provider-enforced dollar ceiling.

`planner.design` uses the same settlement rule, including failed and cancelled
designs. A missing or incomplete canonical usage record is refused before
serde defaults can turn it into zero tokens. `DesignResponse.cost_usd` is a
known subtotal; `uncounted_calls` marks the unknown part. Its optional `error`
reports a failed design while preserving settlement, and `cancelled` means
local work stopped, without claiming the provider stopped billing. Invalid
requests still return protocol errors. Consumers must inspect `error` and
`cancelled` before treating a missing command as an unverified task.

With complete settlement, `remaining_budget_usd` carries the task's effective
limit minus the design cost, including the planner's configured default. The
TUI forwards exactly that remainder to attempts, shows incomplete costs on
the confirmation screen and session total, and starts no attempts when design
cost is unknown or the remainder is exhausted. The new fields have serde
defaults for reading older replies; older consumers should be updated to
handle failed design replies explicitly and honor the remaining limit.

### Linux command isolation

The read-only system runtime includes `/etc/alternatives` so distribution-provided links such as `/usr/bin/cc` and `/usr/bin/awk` resolve to their tools. Other host `/etc` contents are absent except the loader cache, certificate directory and, when networking is enabled, resolver configuration.

The default `molt do` shell and done-check use the same Linux x86_64 backend, requiring `/usr/bin/bwrap` (bubblewrap 0.9+) and working unprivileged user namespaces. Startup probes the backend and fails if required capabilities are missing. `molt do --no-sandbox` and `molt-tools shell --no-sandbox` are explicit host choices; the library equivalent is `Shell::with_policy(..., ExecutionPolicy::Unconfined)`. Other architectures and macOS currently need that explicit choice.

Each command starts from an empty mount namespace with read-only system runtime paths, a read-only original project, its writable `fs.fork` directory, private tmp/home, and isolated PID, user, IPC, UTS and network namespaces. Direct isolated shell calls on the original project are refused. Ignored entries linked back to the project remain read-only; shared build directories are not writable. `/proc` shows only sandbox processes. User namespaces cannot be nested, capabilities are dropped, and a seccomp filter blocks creating AF_UNIX sockets (so no host service socket is reachable through a mounted path or, with host networking, an abstract name) and AF_UNIX datagram socket pairs (which could send to such sockets by address), io_uring (which could bypass syscall socket filtering), and alternate syscall ABIs. Programs must use ordinary file/socket I/O rather than requiring io_uring. The default denies network access. With explicit host networking enabled, IP connections, including localhost, are allowed; AF_UNIX stays denied. Stream and seqpacket socket pairs, which reach only each other, are allowed: Rust's standard library and Node's libuv start child processes with them, so `cargo test` and `node --test` need them.

`molt do --sandbox-policy /trusted/path/policy.json` supplies a JSON policy. It must be owned by the current user, not writable by others, and outside the canonical project and scratch roots. Each canonical path component is opened without following replacement symlinks; file permissions and content are checked through the same descriptor, and reads are bounded to 64 KiB. The shell reads it at startup; no request payload or repository file selects a policy. The default shell service is protected from service-driven replacement. A custom service configuration is still an explicit trusted user choice, and custom shell implementations are responsible for their own isolation.

```json
{
  "network": false,
  "read_only": ["/opt/toolchains", "/opt/dependency-cache"],
  "environment": ["RUSTUP_HOME", "CARGO_HOME", "CARGO_TARGET_DIR"],
  "memory_mb": 4096,
  "cpu_secs": 600,
  "file_mb": 128,
  "open_files": 256,
  "processes": 2048
}
```

Dependencies mount at their canonical absolute paths. `--pass-env NAME` explicitly allows another variable; HOME always points to private `/home/molt`, and service bus / Anthropic credentials cannot be passed. Toolchain paths outside `/usr` need declared read-only mounts. Builds that normally write into an ignored host-linked `target` or dependency cache must put their output in the writable fork or private `/tmp`. For example, declare a Rust toolchain mount and RUSTUP_HOME, set CARGO_HOME and CARGO_TARGET_DIR to private `/tmp` locations, and explicitly enable network when downloading dependencies; a prepopulated read-only dependency cache can instead be copied into private cache space for offline builds. The private cache disappears after each command. A C build using system `cc` needs no extra mounts.

Limits are inherited per process: address space, CPU time, file size, open files and process count, plus the existing command wall timeout. These are not aggregate cgroup quotas; RLIMIT_NPROC has the operating system's per-user semantics and does not limit root. TERM reaches the sandbox session group with 500 ms grace; KILL then tears down its PID namespace, including descendants that called setsid. Normal completion, cancelled handlers and service exit also tear down the namespace. Temporary mounts disappear with the last process. Future executable skills must use this shell path to share the policy.

CI installs bubblewrap and requires namespace tests to run. A constrained local executor may skip only those integration tests after a failed capability probe; production startup still fails closed. Tests cover host paths, symlinks, read-only dependencies, network policy, host sockets, limits, C builds, cancellation and descendants with new sessions.

## From an issue to a PR in CI

An opt-in composite GitHub Action runs a maintainer-labelled issue with a fixed
budget, then creates a PR containing check and baseline evidence, known cost
and run ID. It requires a trusted installed Molt build and a working sandbox;
fork PR events cannot start a secret-bearing run. See
[scripts/ci/README.md](scripts/ci/README.md) for setup, permissions and limits.

## Declarative project skills

Memory may propose a small declarative workflow while learning from a verified
successful task. It saves a **draft**, with its source run, audit message IDs,
writer/version, trigger words, prerequisite file hashes, steps and acceptance
command. Drafts never enter a later task's context. A successful source task
cannot approve its own workflow, and no skill text is executed as a script.

Inspect and manage them through the kernel-identified CLI:

```sh
molt skill --workspace . list
molt skill --workspace . draft skill.json --run SOURCE_RUN
molt skill --workspace . evaluate SKILL_ID --revision 1 --run HELD_OUT_RUN_A
molt skill --workspace . evaluate SKILL_ID --revision 1 --run HELD_OUT_RUN_B
molt skill --workspace . promote SKILL_ID --revision 1
molt skill --workspace . used RUN
molt skill --workspace . disable SKILL_ID --reason 'No longer suitable'
molt skill --workspace . rollback SKILL_ID --revision 1 --reason 'Revision 2 regressed'
```

A manual draft's JSON fields are `name`, `trigger_terms` (single words),
`prerequisites` (map of relative paths to SHA-256), `steps` and `acceptance`
(the check command). Evaluation reads authenticated request/reply pairs from
the audit; a forged note, tool output or an evaluation payload cannot assert
success. Promotion needs two distinct independent successful runs in the same
project, with the stored acceptance command, besides the source. Failed
independent evaluations remain attached to the draft. The human must select
independent cases and their protected checks: this registry does not establish
that arbitrary chosen runs constitute an adequate held-out task set.

Recall selects at most four active skills for the current project, only when
all trigger words and prerequisite hashes match. The planner treats their
steps as data below the task, check and permissions. Captured offerings retain
full revision snapshots and their evidence, accessible with `skill used` even
after disabling or rollback. These are records of offered context, not proof
that the model followed every step. New drafts cannot re-enable a disabled
skill. Rollback selects an evaluated older revision but keeps it disabled until
an explicit human promotion. Disabling or rollback voids learned-note evidence
from runs that were offered the withdrawn active revision, re-settles confidence
and leaves tombstones and original audit/snapshot evidence intact. Learning
cannot automatically recreate those withdrawn notes.

The first implementation does not run executable skill scripts or perform
automatic promotion. Real task success, cost and latency comparisons with a
no-skill baseline remain to be run under an explicit evaluation budget. Stored
independent evidence records each run's wall duration, known cost subtotal and
unknown calls; these alone do not prove a workflow's causal benefit.

## Offline semantic notes (optional)

Build with `cargo build --workspace --features molt/semantic`. In a trusted
memory-service configuration, add `--embedding-model /outside/project/model.json`
and `--embedding-sha256 HASH` to `molt-memory`'s arguments. No model is downloaded;
its SHA-256 is required and checked before parsing. The model is a data-only JSON
export of pretrained word vectors, with `format: "mean-word-vectors-v1"`,
`dimensions` (1–1024), and a `vectors` map of lowercase words to finite arrays.
Mean pooling and cosine similarity run locally on CPU. This first backend does
not run MiniLM/ONNX, understand word order or supply a pretrained vocabulary.
Use a licensed, reviewed vocabulary suitable for the project's languages.

`--semantic-weight` (default 0.5) mixes normalized FTS5 relevance and cosine
similarity; `--semantic-min-similarity` (default 0.4) admits non-keyword matches.
Confidence and recency retain their weights. Empty/OOV queries keep lexical
behavior. Without the feature/configuration, recall is unchanged. Snapshot
reasons record lexical relevance, similarity, weight and model SHA-256, so old
snapshots retain the exact evidence even after correction or forgetting.

SQLite caches note vectors by model and text hashes. Writes/corrections refresh
the derived cache, and recall retries invalid or stale entries. Forgotten,
retracted and review-pending notes stay excluded before semantic selection.
Model input is bounded to 64 MiB, 200000 vocabulary words and 1024 dimensions;
semantic databases are limited to 10000 live notes. Changing the model hash
keeps old snapshot evidence and reindexes the new model. No additional key or
network access is used. The lightweight backend should be compared against
keyword-only recall on real tasks before claiming a success-rate improvement;
fixture synonym recall demonstrates functionality, not production quality.

## Project task commands

Place Markdown templates in `.molt/commands/NAME.md`, then type `/NAME arguments`
in the interface. `$ARGUMENTS` inserts the complete argument text; `$1`, `$2`,
and later positions insert whitespace-separated words (missing words are empty).
Expansion is one pass and does not execute a shell. Optional frontmatter uses
flat, single-line YAML scalars:

```markdown
---
description: Fix an issue and add a regression test
argument-hint: ISSUE_NUMBER
check: cargo test
attempts: 2
budget: 2.5
---
Fix issue $ARGUMENTS and add a regression test.
```

These defaults affect only that task. Explicit interface settings take precedence,
and a template budget cannot raise an existing limit. Confirm mode shows a
template's check before starting; auto and plan modes retain their existing rules.
Templates cannot set modes, permissions or protected paths. `/help` lists project
commands and Tab completes unique command prefixes (or lists matches). Built-ins
win name conflicts. Invalid, oversized and out-of-workspace files are skipped
with a startup warning; restart the interface after editing templates. Discovery
is bounded to 128 directory entries and 64 KiB per file. Nested YAML structures
and multiline frontmatter scalars are not supported.

## Terminal colors and controls

`/theme dark`, `/theme light`, and `/theme ansi` change all interface colors,
including earlier log entries. Dark and light set their own background; ANSI
uses only the terminal's 16 colors and follows its background. Without a saved
choice, `COLORTERM=truecolor` or `24bit` selects dark and other terminals select
ANSI. Light's eight text colors meet WCAG AA (4.5:1) against its white background.
`NO_COLOR` keeps colors disabled. The choice is saved privately in
`<data dir>/tui-theme.json`; other interface settings are not saved yet.

Ctrl+L clears and redraws the interface. On Unix, Ctrl+Z restores the terminal
and suspends just the interface process; use the shell's `fg` to return. Its
kernel and services run in an independent backend process group, so bus
dispatch and later tool/model turns continue while the interface is stopped.
Private pipes carry bounded frames; service stdout goes to the log, and output
backpressure can drop progress previews while preserving final RPC settlement.
Queued results are shown after resuming. This requires a shell with job control and a foreground
process group (an orphaned process group can ignore SIGTSTP). Background color
probing with OSC 11 is not implemented.

## License

Molt is dual-licensed under the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option. Contributions are accepted under the same terms; see [CONTRIBUTING.md](CONTRIBUTING.md#license).
