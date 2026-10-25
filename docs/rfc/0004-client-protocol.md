# RFC 0004: The client protocol

- Issue: #167 (roadmap #153, phase 3, C7)
- Related: #168 (ACP adapter, built on this), #74 (runs), #107 (questions), #155 (`tui/app.rs` split), #6, #66
- Status: version 1 implemented; unstable until phase 4, so it may still change, but only by a change to the record of version 1 that review sees (see Compatibility)

## Summary

The interface in the terminal talks to its backend in its own internal
types: the `Action`s the app returns and the `Msg`s it takes, framed on a
private pipe. Nothing else can speak that, so an editor, a web page or a
script cannot drive Molt the way the interface does.

This RFC defines a **client protocol**: JSON-RPC 2.0 over a Unix socket or a
pipe, with methods for sessions, tasks, results, notes and runs, and a
stream of progress events. `molt serve` serves it for one workspace. The
interface becomes one client of it: its backend is the same server, started
on a pipe, and every request it makes is a protocol request. An adapter for
the Agent Client Protocol (#168) is meant to sit on top of it.

## Transport

- **Messages.** JSON-RPC 2.0. One message is one line: compact JSON (which
  never holds a raw newline) and `\n`. A line is at most 16 MiB; a longer
  one, or input that ends in the middle of one, ends the connection.
- **`molt serve`** listens on a Unix socket, by default `serve.sock` in the
  workspace's data dir (`--socket PATH` for another). `molt serve --stdio`
  serves one client on standard input and output instead, for a program
  that starts it as a child (an editor's adapter). Either way it takes the
  same flags as `molt ui` (`--workspace`, `--project`, `--data-dir`,
  `--pass-env`, `--deps-cache`, `--allow-web`, `--web-resolver`, and the
  global `--config` and `--profile`), and works in that one workspace.
- **The interface's server** is `molt ui-backend`, a child of the
  interface: the config the interface resolved comes first on its private
  pipe, as one line, then the protocol on the same pipe. It is the stdio
  server with a config it does not resolve again.
- What the services print goes to standard error, never into the protocol.

Unix sockets and pipes only: no TCP, so no token is needed.

## Who may connect

The server is only reachable by the user who started it:

- The socket is made with mode 0600, in a directory molt creates with mode
  0700. A directory others may write to is refused (they could put a socket
  of their own in its place); a stale socket no server answers on is
  replaced, and one a server still answers on is not.
- Each connection's peer is checked with `SO_PEERCRED`: a process of
  another user is dropped before anything is read, even if the file's mode
  was opened to it.
- With `--stdio`, the program that started the server holds both ends.

And a client can do no more than the interface could:

- The config, the workspace, the services and their capabilities come from
  the server's start, never from a client. There is no method that changes
  them.
- A task names the server's workspace or none; a request for another is
  refused. Results, notes and runs are checked against it as the
  interface's were (`fs.view` names a fork's workspace; a note change names
  its project).
- The session's capabilities on the bus are the CLI's, as in the interface
  (`session.rs`), and every call it makes is in the audit log as the CLI's.
- Confirmation stays with the client. The server applies a result only when
  the client says so: `task.submit` with `apply` true, as `molt do` does, or
  `result.apply` after the client has shown it. An attempt's question
  (`ask_user`) reaches the client as a `progress` event `asked`, and only
  the client's `question.answer` answers it. Approval policies (confirm
  mode, `[confirm]`) are the client's, as the interface's are now.

A server has at most one session open at a time (one kernel per data dir).
A second client may connect, initialize, list runs and so on, but its
`session.open` is refused with `BUSY` while another holds the session.

## Lifecycle

1. `initialize {protocol: 1, client: {name, version}}`. Every other method
   before it is refused (`NOT_INITIALIZED`), and so is a version other than
   the server's. The answer names the server, its workspace and its methods.
2. `session.open {}` starts the kernel and the services and answers once
   they are up: their versions, why memory is missing if it is, and the
   default model. From then on the server sends `progress` events.
3. Tasks, results and notes, below. Requests may be in flight together;
   answers come as each call ends, not in order.
4. `session.close {keep}` stops every call in flight, then the services,
   and removes the forks the session made, except `keep`, those named by
   `result.keep`, and those of checkpointed runs.

A connection that ends without `session.close` (the client crashed or was
killed) stops the services and removes no fork: one may hold a result the
client was waiting on. A server stopped by a signal keeps the forks of
every result it answered with that was neither applied nor discarded.

## Methods, version 1

Parameters and results are JSON objects. The types are in
`crates/molt/src/protocol.rs`; most results are the services' own `molt-api`
replies.

| Method | Parameters | Result |
| --- | --- | --- |
| `initialize` | `protocol`, `client` | `protocol`, `server`, `workspace`, `methods` |
| `session.open` | none | `versions`, `memory_problem`, `default_model` |
| `session.close` | `keep` | `{}` |
| `task.design` | `trace`, `request` (`RunRequest`) | `design` (`DesignResponse`), `existing`: check files already in the workspace |
| `task.submit` | `trace`, `request` (`RunRequest`, with its `workflow`) | `RunResponse` |
| `task.plan` | `trace`, `request` (`PlanRequest`) | `PlanResponse` |
| `task.steer` | `trace`, and a `Constraint`'s `text`, `protect`, `check`, `answers` | `ControlResponse` |
| `task.pause`, `task.resume` | `trace` | `ControlResponse` |
| `question.answer` | `trace`, `question`, `text` | `ControlResponse` |
| `task.stop` | `keep`, `attempts`, `check_files`, `drop_run` | `kept`, `notes` |
| `task.learn` | `trace`, `model` | `ConsolidateResponse` |
| `result.diff` | `fork` | `DiffResponse` |
| `result.apply` | `fork`, `paths` (empty: all of it) | `MergeResponse` |
| `result.recheck` | `fork`, `only`, `check` | `Rechecked` |
| `result.keep` | `fork` | `{}` |
| `result.discard` | `fork` | `{}` |
| `commit.prepare` | `trace` | `{}` |
| `commit.make` | `trace`, `task`, `response` | `Committed`, or `null` when there was nothing to commit |
| `runs.list` | none | the stopped runs: `run`, `saved` (`{"Ok": checkpoint}` or `{"Err": why}`), `gone` |
| `runs.drop` | `run` | `{}` |
| `runs.undo` | `run` | `Undone` |
| `notes.list` | `query` | `Recalled` notes, quarantined ones included |
| `notes.forget` | `id`, `reason` | `forgotten` |
| `notes.remember` | `text` | `id` |
| `notes.promote` | `id`, `reason` | `id` of the copy |
| `notes.change` | `review` (`ReviewRequest`), `text` | `ReviewResponse` |
| `notes.used` | `run` | `RecallSnapshot`s |
| `command.run` | `trace`, `command` | the shell's `RunResponse` |
| `command.stop` | `trace` | `{}` |

- **`trace`** is the run's id, chosen by the client (`trace_` and random
  hex, as `molt_proto::TraceId::random` makes them). Every progress event of
  the run carries it as `run`, and the steering methods name the run by it.
- **`task.stop`** is the interface's Esc: it stops every call in flight and
  the services, with every command the agent started, and closes the
  session. Nothing done is lost: each fork in `attempts` that holds changes
  beyond `check_files` is kept and named in `kept`, as is one whose changes
  cannot be listed (with a note), and the forks in `keep` stay. Open
  another session to go on. A call it cut off answers `CANCELLED`, which
  never means it did nothing.
- **`commit.prepare`** looks at the work tree before run `trace` starts;
  `commit.make` then commits exactly what the run merged, as `/commit on`
  does.
- **`command.run`** is the interface's `!cmd`: a command in a fork of the
  workspace that is dropped after, in the attempts' sandbox, its output
  streamed as `progress` events `output` of `trace`.

A request without an `id` is a notification: it is carried out and gets no
answer.

### Errors

| Code | Meaning |
| --- | --- |
| -32700 | Not JSON. |
| -32600 | Not a JSON-RPC 2.0 request. |
| -32601 | No such method. |
| -32602 | Parameters that do not read, a protocol version not served, or a request for another workspace. |
| -32000 | The call was made and failed; `message` says why, as the interface shows it. |
| -32001 | No session is open: `session.open` first. |
| -32002 | `initialize` first. |
| -32003 | Stopped by `task.stop` or `session.close` before it answered; what it did is unknown. |
| -32004 | Another client has the session open. |
| -32005 | The session is open or starting already (`session.open`), or still starting (`task.stop`). |

### Events

The server sends each progress event of the session as a notification
`progress`, whose parameters are a `molt_api::progress::Progress`, tagged by
`event`: the model calls (`model_started`, `model_text`, `model_finished`
with `role`, `usage` and `cost_usd`, `model_retry`), the check
(`check_ready`, `check_ran`), the attempts (`attempt_started` with its
`fork`, `tool_call` with the tool and a one-line description of its input,
`attempt_finished`), the requirements (`required`, `adopted`, `state`),
questions (`asked`), what memory gave the run (`recalled`), the workflow
picked (`routed`), plans (`planned`, `step`), notes, and a command's
`output`. Events are best effort: a client that does not read falls
behind, and past 64 events waiting, more are dropped. Model events have
sequence numbers that show the gap, and every answer still comes, with the
whole result.

## For an ACP adapter (#168)

The Agent Client Protocol is JSON-RPC 2.0 on stdio, so an adapter is one
process speaking ACP on one side and this protocol on the other, to a
`molt serve --stdio` it starts for the session's `cwd`:

| ACP | This protocol |
| --- | --- |
| `initialize` | `initialize` |
| `session/new` | `session.open` (one server per ACP session, started in its `cwd`) |
| `session/prompt` | `task.submit` with `apply` false and the workflow `auto`, answered when it ends |
| `session/update` | the `progress` events of the run: `tool_call` and `attempt_finished` as tool calls, `model_text` as message chunks, and the result's `patch` as a diff |
| `session/request_permission` | asked before `result.apply`, and for each `asked` event, whose answer goes to `question.answer` |
| `session/cancel` | `task.stop` with the run's attempt forks, which keeps their work |

## Compatibility

- The parameters and results here follow the repository's rule for wire
  types: a field added later gets `#[serde(default)]`, and where an older
  peer would choke, `skip_serializing_if`. Unknown fields are ignored on
  both sides, and so are events a client does not know (the interface
  leaves them out).
- `crates/molt/src/testdata/protocol-v1.jsonl` is the record of version 1:
  for every method, parameters and a result as a version 1 peer sends them,
  and a set of events. Its SHA-256 is pinned in a test, so a pull request
  that edits it shows that in its diff and its review. A test
  (`protocol::tests::every_message_of_version_1_still_reads_and_keeps_every_field`)
  reads every line with today's types and checks that writing it again
  keeps every field it had, with the same value. Another
  (`serve::tests::the_server_takes_every_request_of_version_1_as_written`)
  sends every request of the file to a server and checks that it is read
  and dispatched. A change that would break a line, by removing or renaming
  a method or a field or changing what one holds, is a breaking change:
  version 2, a new file next to this one, and `initialize` refusing what it
  no longer serves.
- `Result` values in a few results (`runs.list`'s `saved`, `runs.undo`'s
  `memory`) are Rust's serde form, `{"Ok": ...}` or `{"Err": "..."}`.

## Acceptance

- The interface talks to its backend only through this protocol:
  `tui/backend.rs` turns each action into a request and each answer into
  the message the app took before, and the server is the one `molt serve`
  runs (`tui::backend::tests`, and `crates/molt/tests/agent.rs`:
  `the_ui_backend_finishes_without_terminal_reads_and_exits_when_the_pipe_closes`).
- A scripted client runs a task to its end: over the socket of `molt serve`,
  it opens a session, submits a task, sees its progress (the attempt, its
  tool calls, the check, the model calls with their role and cost), reads
  the result's diff, applies it and closes
  (`a_script_runs_a_task_through_molt_serve_and_applies_the_result`).
- `task.stop` loses no work: the run's call answers that it was stopped,
  the command its attempt started ends, and the attempt's fork is kept with
  what it wrote
  (`a_task_stopped_through_the_protocol_keeps_the_attempts_work_and_its_commands_end`).
- Only the server's user is served on its socket
  (`serve::tests::only_the_servers_own_user_is_served_on_its_socket`,
  `the_socket_is_private_a_live_one_is_not_taken_over_and_a_stale_one_is_replaced`),
  and a request cannot name another workspace
  (`a_request_names_the_servers_workspace_or_none_and_never_another`).
- CI checks version 1 for breaking changes (the two tests above).

## Deferred

- **Generated schemas and SDKs.** The schema is the Rust types and the
  record of version 1; a JSON Schema generated from them, and TypeScript and
  Python clients, come later.
- **Richer events.** A tool call's result, a summary of a tool call's input
  beyond its one line, and an attempt's diff as it goes are not events yet.
  A client can ask `result.diff` for any fork of the workspace, an attempt's
  included.
- **More than one session per server**, and several clients sharing one
  session (an editor and the terminal on the same run).
- **The interface attaching to a running `molt serve`** instead of starting
  its own; it still reads the workspace's files, git, the audit log and the
  service log itself, as any local program can.
- **TCP with a token**, for a client on another host.
- **`notes.*` beyond what the memory screen does**, and `skills.*`.
