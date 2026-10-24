# The interface

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
   it; `s` lists its files to choose which to apply, and `e` opens it in
   `$EDITOR`. Either way its check and the review's fixed rules run again on
   just what would be applied before Enter takes it, and a choice or an edit
   that fails is not applied. `k` keeps it in its fork, `n` discards it. `/last` opens the diff of
   the last finished task again, whatever was done with it. When no attempt
   passes, each one shows the end of why it stopped, and a failed check shows
   its last lines in its box; `v` opens the whole story and `r` runs the task
   again with the same check, without designing it anew. `/lanes` shows the
   last task's tool calls after it has ended.

Shift+Tab switches to **auto** (design the check and apply what passes) or
**plan only** (results stay in their forks). Esc pressed twice within about
a second stops a task (the first press says `esc again to stop`, since Esc
also closes every overlay); Ctrl+C stops it at once. The services are
stopped, with every command the agent started, and started again. The forks
of attempts that had changed files are kept (`kept 2 · interrupted`), and so
is a fork whose changes could not be listed, with the reason. What the
stopped task had spent by its finished model calls goes into the session
total; a call that was cut off or had no price is counted as uncounted, and
without previews (`/stream off`) the whole task is (`1 stopped task
uncounted`). With a check known in full `r` starts the task again with it,
on a budget of its own. Ctrl+C
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
An attempt can also ask you something only you can decide
(`? attempt 1 asks  Tabs or spaces?`, with numbered options when it has
some). It waits for the answer while the other attempts go on; what you type
next answers it, an option by its number or in your own words, and becomes
the next version of the requirements, which every attempt follows. A run asks
at most three questions, and an attempt left unanswered for ten minutes
decides itself and says so in its reply. `molt do` has no one to ask, and its
attempts are not offered the question at all.

The tasks you give one after another make a conversation: each new task is
told what the last five asked, how they ended, what they said and which
files they changed, and whether those changes are in the workspace. Every
task still has its own check and budget. `/new` starts a new conversation.
`/continue TASK` goes further: the new task gets the last one's whole
conversation with the model (its oldest turns left out when it does not fit),
and works in its result's fork while that result is kept.

Not every task needs the whole pipeline. Before a task starts, one small
model call picks how much it gets, and the log says so first (`◆ handled as
quick (a comment) · /verify for the full workflow`): `ask` answers with tools
that only read, in the workspace itself, without a fork or a check, and its
answer is marked unverified; `quick` is one attempt with no designed check
(a `/check` you set is still run) whose diff only the review's fixed rules
look at; `verify` is everything above. With a check set the pick is never
`ask`. `/ask`, `/quick` and `/verify` run a task through that workflow without
a pick, and `/verify` alone runs the last task again through the full one.
The pick's model is the planner's `MOLT_ROUTE_MODEL`, by default the task's.

The log keeps everything a task reports, in full; PgUp and PgDn scroll it,
and Ctrl+Home and Ctrl+End (or Home and End with an empty input) jump to its
start and end.

A line that starts with `!` is a shell command, not a task: `!cargo test`
runs through the shell service in a fresh fork of the workspace as it is
now, in the same sandbox as the attempts' commands, and its output streams
into the log (stderr dimmed) as it comes. No model is called and nothing is
spent; the request and its reply are in the audit log like every other
message. The fork is dropped afterwards, so whatever the command writes is
not kept in the workspace. The log says how it ran (`in the sandbox`, or
`unconfined (--no-sandbox)` when you chose that), and its last line gives
the exit code and how long it took. Esc stops it. One command runs at a
time, for at most an hour; live output stops after 1 MiB, and the end of
the output then comes from the reply.

`/diff` lists what the workspace has changed against git's `HEAD`: each
file with its added and removed lines, untracked files git does not ignore
marked `new`. Enter opens the selected file's diff in the diff viewer, `r`
lists again, and Esc goes back. Outside a git work tree, or before the
first commit, it says it cannot compare. Git runs on your side, as it does
for `/commit`, with the repository's external diff programs, filters and
hooks turned off.

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
for every project. `/attempts`, `/model`, `/effort`, `/budget`,
`/design-budget` and `/check` set the next tasks' options (`/check` while a task runs changes only
that task); `/memory-model NAME` sets the model
memory learns with (`same` for the attempts' model, `default` for
`MOLT_MEMORY_MODEL`, else the attempts' model), and when learning finds nothing the log
says why. Typing `/` shows matching commands and argument hints above the input
(up to six at once). ↑/↓ selects a candidate and Tab puts it in; commands with
fixed values, such as `/effort` and `/model`, suggest those values next. Esc
hides the list; editing brings it back. Tab otherwise still enlarges attempts.
With an empty input, `?` opens shortcuts for the current screen, including check
and apply confirmations and memory; `?` again or Esc closes them. In text, `?`
is typed normally. `/help` lists all commands and keys. The services'
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

Interface settings are saved automatically for each workspace: mode, model,
attempts, effort, budget, design budget, check, memory model, previews, notifications and commit.
The next launch restores them, and `/reset` removes saved settings and restores
the defaults from the explicitly selected config, or built-in defaults. The auto
mode label stays yellow. A malformed or unsupported saved file is ignored with a
single-line notice. Files use mode 0600 in the same private workspace state directory
as history; values containing known credentials or common key patterns are not saved.

The user config (see [Configuration layers](config.md#configuration-layers)), or the
config passed with `--config`, can set defaults, for example:

```toml
[tui]
mode = "confirm"
check = "cargo test"
attempts = 2
budget = 3.0
model = "sonnet"
```

Commands entered this session override saved values, which override these config
values (and a `--profile`'s), which override built-in defaults. Project command templates still affect
only their own task and do not overwrite saved preferences. Changing defaults does not rewrite a run request already submitted for a task.

If service startup fails transiently, the interface retries up to three times,
after approximately 2, 5 and 15 seconds, and shows the countdown. Recognized
configuration, credential, executable and permission errors stop immediately with
an action to take. A backend disconnect leaves pending settlement unknown and is
not retried automatically. `/restart` restarts idle services while preserving the
log, settings, history and kept forks; finish or stop a task and wait for learning
and commits first. Restart uses the configuration loaded at launch; relaunch to change that
configuration or environment. `/log` opens up to 200 lines from the final 64 KiB of
`tui.log`, sanitizes terminal controls, redacts known credentials and omits lines
with common credential patterns. A manual restart resets the automatic retry limit.

`/undo [RUN]` takes back the latest (or named) run commit through the idle TUI
session and tells its existing memory service to stop counting the run. It
refuses while a task, learning, apply/discard or another Git operation is active.
A failed memory notice preserves the Git result and can be retried with the same
run id without another revert. Quitting waits for Git and memory settlement;
undo clears the earlier-task context so later tasks do not assume the reverted
result is still present. Retained forks and queued task settings stay available.

While a check or result waits for confirmation, PgUp/PgDn and Ctrl+Home/Ctrl+End
still scroll the conversation log. At apply confirmation, `v` opens the full
review with fixed-rule findings, the independent reviewer verdict/findings and
any review error. Stop reasons distinguish rules from the reviewer; older
planner replies retain their combined findings without claiming a source.
In diff and transcript viewers, `G`/End shows the final page with the last row
at the bottom, including wrapped text.

`/queue add TASK` submits a separate task while another runs. Plain input still
adds a constraint to the current task. `/queue` opens the task list: `a` adds a
multiline task, arrows select, `J`/`K` move it, `n` removes it, and Enter inspects
its complete text. `/queue remove N` and `/queue move FROM TO` use one-based
positions. Each task captures its mode, model, effort, attempts, check, budget,
stream, memory-model and commit setting when submitted. Later setting changes
apply to later submissions. Its workspace and earlier-task context are read when
it starts, after the preceding apply/discard and optional commit have settled.
Check and apply confirmations hold the queue; failure or keeping a result lets
the next task start. A failed task's optional retry offer yields to the queue.

`/queue pause` leaves the current task running; `/queue resume` continues queued
work. Stopping the current task pauses its queue, preserving every queued
task. `/queue clear` removes only queued tasks. Limits are 100 tasks, 64 KiB per
task and 4 MiB total; rejected drafts are retained. Invalid `@paths` pause the
queue without losing the task; the planner reads files when the queued task starts. Queued tasks live in this interface session:
quitting shows their full text for confirmation and prints it after the terminal
is restored, with known credentials redacted. A crash does not save the queue.

If the interface stops before a task is done (quit while it runs, a crash,
the process killed), the next `molt` in the workspace picks up where it
stopped, without calling a model: the earlier tasks' summaries later tasks
are told about, what the session spent, and the task that was in flight. A
task that changes things keeps a planner checkpoint as it runs (see
"Checkpoints and `molt resume`"), and quitting or a crash keeps it and its
forks: `/runs resume [RUN]` goes on with it under the same run id, with its
check, protected paths, requirements and spend; `/runs discard [RUN]` drops it
and its forks and counts what it spent. `/runs` lists the workspace's stopped
runs, `molt do --checkpoint` ones included, and says why a damaged or
incompatible checkpoint, or one whose fork is gone, cannot go on. Stopping the
task with Esc is cancelling it: its checkpoint goes, and its work is kept as
an interrupted result. `/pause` keeps the run in the interface. A check being
designed or waiting for you, or an answer under `/ask`, is not checkpointed:
its task is put back in the input to start again, and a design cut off counts
as uncounted spend. A result that waited for your decision is kept (`/kept`).
The session is saved privately per workspace whenever it changes, up to 1 MiB;
a quit with nothing in flight removes it, so the next interface starts afresh.
Only one kernel runs per data directory, so `molt resume` and the interface
never go on with the same run at once.

`/kept` lists this workspace's retained results with their task, outcome, age and
change counts. Select with arrows, `d` inspects the current diff, Enter applies,
`n` discards, and Esc returns. Apply and discard wait for idle services; a failed
operation leaves the result in the list. Apply uses the original fork baseline
and refuses conflicting workspace edits, naming the files. Kept results survive
interface restarts; missing fork directories are pruned. Private metadata is
bounded to 1,000 entries / 4 MiB and filters task text like input history. Saved
paths are checked by the fs service against the current workspace before use.
Applying an older kept result does not create a git commit automatically.

Input history is private to the workspace and survives restarts, even when two
workspaces use the same data directory. ↑/↓ browses it; returning past the newest
entry restores the unfinished draft. Ctrl+R searches newest first without duplicate
matches, Ctrl+R again selects an older match, Enter submits it, Tab or Esc puts it
back in the input for editing, and Ctrl+C cancels the search and restores the draft.
`/clear` clears only the log; `/history-clear` removes this workspace's input history.
History retains up to 1000 entries and 4 MiB; entries over 16 KiB, known environment
credentials, common token prefixes, credential assignments and private keys are
excluded. This filtering is heuristic. History files use mode 0600 under a private
workspace directory; malformed records are ignored with a startup notice.

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
(Tab), `o` opens that attempt's turns alone. Each turn is headed by whose it
was: `attempt 2`, `designer`, `vetter`, `reviewer`, `judge` or `router`
(`planner` for a run from a planner that did not say). Attempts are counted
from 1 here, in their boxes and in `molt do`, alike. `j`/`k` scroll, `{`/`}` jump
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
does (see [Commits and `molt undo`](do.md#commits-and-molt-undo)): the work tree is looked at when
the task starts, and the commit is made after the run applies the result, or
after you accept it in confirm mode. A result kept in its fork, or one that
fails to apply, is not committed. `/commit off` (the default) stops it.

The interface holds the data dir's lock while it is open, so a `molt do` in
the same project is refused until it closes.

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
commands, and their argument hints and descriptions appear in the same live
completion list as built-ins. Built-ins
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
`<data dir>/tui-theme.json`; workspace settings are saved separately as described below.

Ctrl+L clears and redraws the interface. On Unix, Ctrl+Z restores the terminal
and suspends just the interface process; use the shell's `fg` to return. Its
kernel and services run in an independent backend process group, so bus
dispatch and later tool/model turns continue while the interface is stopped.
Private pipes carry bounded frames; service stdout goes to the log, and output
backpressure can drop progress previews while preserving final RPC settlement.
Queued results are shown after resuming. This requires a shell with job control and a foreground
process group (an orphaned process group can ignore SIGTSTP). Background color
probing with OSC 11 is not implemented.
