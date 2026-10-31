# Tools

The model works through tools: reading and changing files, running commands,
asking memory, reading the web, asking you. Each tool is a declaration of a
bus method, and a service of your own can add tools the same way (RFC 0002,
[rfc/0002-tools.md](rfc/0002-tools.md)). The declaration format is unstable
until phase 3 of the roadmap.

## The built-in tools

| Tool | Calls | Who gets it |
| --- | --- | --- |
| `read_file`, `list_files`, `search` | `fs.read`, `fs.list`, `fs.search` | everyone |
| `write_file`, `edit_file`, `run` | `fs.write`, `fs.edit`, `shell.run` | attempts and the check designer |
| `find_symbol`, `recall` | `memory.symbols`, `memory.recall` | attempts and the designer, when memory is up |
| `fetch_url` | `web.fetch` | attempts and the designer, when you allow web hosts |
| `ask_user` | answered by you, through the interface | attempts of an interactive run |
| `load_skill` | answered by the planner itself | attempts, `ask` runs and the check designer, when the run has [skills](skills.md) |

An `ask` run gets only the ones that read. The vetter, the reviewer and the
judge get `read_file`, `list_files` and `search`.

With `MOLT_EXPLORE=on`, or a workflow that sets `attempts.explore`, the
attempts also get `explore`, last: a question to a sub-agent that reads
their fork and returns only its conclusion (see [explore.md](explore.md)).

## Background processes

With `MOLT_PROCESSES=on` in the planner's environment, attempts also get
four tools to run a command that keeps going while they work, such as a dev
server, a file watcher or an interactive program (#110). They come after
the other built-ins and before `explore`; off, the tool lists are as they
were.

| Tool | Calls | What it does |
| --- | --- | --- |
| `start_process` | `shell.start` | Starts `command` and answers with its id and what it printed so far. Given `ready`, text the program prints once it is up, it waits for it (`timeout_s`, default 30, at most 120) and says whether it appeared or the command exited first. |
| `read_process` | `shell.read` | What the process printed since the last read, standard output and error together, and whether it is still running or how it ended. `wait_s` (at most 30) waits for output when there is none yet. |
| `write_process` | `shell.write` | Writes `input` to its standard input, at most 64 KiB; `close` closes it after. Fails once the process has ended. |
| `stop_process` | `shell.stop` | Stops it with everything it started. Stopping it again answers the same. |

A process runs as `run`'s commands do: in the attempt's fork, in the same
sandbox, with the same environment, limits and network policy, in a process
group of its own; with `--no-sandbox` it is unconfined as they are. Its id
is random and good only in the fork and for the caller that started it: an
id from another attempt, an earlier run, or one already forgotten finds
nothing. Only attempts that change files get the tools; the designer, the
reviewer, the judge, an explorer and an `ask` run do not.

Every process of an attempt is stopped when the model gives its final reply,
before its work is checked, so the check sees the fork with nothing else
changing it, and again when the attempt ends, whether it passed, failed,
lost the race or was cancelled. The shell service also stops a process when
its fork is removed, when it has run 30 minutes, and when the service stops.

The shell service's bounds: 4 processes running at once per workspace and
caller and 8 in all, the latest 1 MiB of each one's output kept (a read
past what was kept says how many bytes it missed), at most 32 KiB of output
in one answer (its start and end, the middle counted as omitted), 64 KiB of
input per write. An ended process is remembered for 10 minutes, or until
the attempt ends. Its input and output are pipes, not a terminal.

The planner holds `shell.start`, `shell.read`, `shell.write` and
`shell.stop` in the default setup, and offers the tools only when it holds
`shell.start` and `MOLT_PROCESSES` is on. A process grants nothing a `run`
command does not have: a command may already run for an hour in the same
sandbox.

## Saved outputs

A `run` result keeps the first 8 KiB and the last 24 KiB of each stream,
and a process the latest 1 MiB of what it printed. A failure in the middle
of a long test log used to be lost with them. Now the shell service saves
an output whole once it outgrows that, and the result names it by an id
such as `out_1a2b…` (#238). Attempts get one more tool, last after the
other built-ins, when the planner holds `shell.output`, as it does in the
default setup:

| Tool | Calls | What it does |
| --- | --- | --- |
| `read_output` | `shell.output` | Given `search`, the lines that hold the text (at most 40), each with its line number and byte offset. Given `offset`, about 16 KB from that byte, and where the next page starts. `wait_s` (at most 30) waits for a running process's output to grow. |

The `run` result, the done-check's feedback and `read_process` say when
part of an output was cut and under which id it is saved. Reading runs
nothing: the command does not run again, so its side effects do not
happen twice.

An output belongs to the caller and the workspace whose command printed
it. A read from another fork, or by another service, is refused (`denied`).
Each saved output records the command, how it ended (exit code, signal,
timeout, cancelled, or not known when the request was dropped), the bytes
printed and the bytes saved, the SHA-256 of what was saved, and the trace
and request id of the call that ran it. The reply that names it is in the
audit log, so the log ties the output to its call.

The service's bounds: 64 MiB per output, 512 MiB for all of them, 256
outputs, and 256 MiB left free on the disk. When the quota is reached,
the outputs whose commands ended longest ago are deleted first. When
nothing can be deleted, the output stops growing, and the result says
how much was saved and why the rest was not: the per-output limit, the
quota, a nearly full disk, or a failed write. An output is kept for 2
hours after its command ended, and never past the service's own run. A
read of one that expired or was deleted says so, and an id the service
never had, or had before it restarted, finds nothing. The files are in
`<scratch>/.molt/output`, which no workspace can name and the sandbox
does not mount.

## Git history

A fork has no `.git`, so an attempt cannot run git. With `MOLT_GIT=on` in
the planner's environment, attempts (and `ask` runs) get one more tool,
`git`, after the other built-ins and before `explore`, which reads the
history of the user's repository through `fs.git` (#111). It only reads.

| `query` | What it answers |
| --- | --- |
| `status` | The baseline, the changes not committed that the user had when the attempt's copy was made (staged or not), and the attempt's own changes since, apart. |
| `log` | Commits, newest first, with the files each changed. `rev` (a commit or a range `A..B`, default the baseline), `path`, `search` (only commits that added or removed that exact text, as `git log -S`) and `limit` (default 20, at most 100) narrow it. |
| `show` | One commit (`rev`): its message and patch, `path` to narrow it. |
| `diff` | From `from` (default the baseline) to `to`, another commit, or without `to` to the files as they are now. |
| `blame` | Who last changed each line of `path` as of `rev` (default the baseline), `start_line` to `end_line`. |

The baseline is the commit the user's `HEAD` named when the attempt's
fork was made, recorded with the fork. `HEAD`, `HEAD~2` and `HEAD^` in a
query mean it, even when the user has since committed or switched
branches; every answer then starts with a note naming where `HEAD` is now.
An `ask` run, which has no fork, reads `HEAD` as it is.

What it never does: the fs service runs git with the repository's git
directory as its work tree, so git never reads or refreshes the user's
work tree or index, and with the user's and the system's configuration
left out and every setting that could run a program, page, use the network
or write overridden on the command line (hooks, fsmonitor, external diff
and textconv drivers, signature checks, pagers, transports, lazy fetches of
a partial clone, automatic gc). What differs between a commit and files on
disk is worked out from content hashes, as `fs.diff` does, so no clean
filter runs either. A revision is resolved to a commit id first, so it can
be neither an option nor `REV:PATH`; paths are relative to the workspace,
and when the workspace is a directory of a larger repository, only its
part is shown, with paths relative to it. A linked work tree reads its own
`HEAD`; a submodule is named, not entered; a repository whose configuration
puts its work tree somewhere that does not hold the workspace is refused,
and a workspace outside any repository gets an error saying it has no
history.

An answer is at most 40 KiB and a query takes at most 30 s. The planner
holds `fs.*` in the default setup, so it holds `fs.git` too, and it offers
the tool only when it does and `MOLT_GIT` is on.

## How `edit_file` finds the text

`edit_file` replaces `old_string` in a file (`fs.edit`). It looks for it in
three steps, and the first that finds it decides (#261):

1. exactly;
2. with `\r\n` and `\n` taken as the same, so text copied with LF line
   endings finds its place in a CRLF file;
3. also ignoring spaces and tabs at the ends of lines.

Indentation is never ignored: spaces where the file has tabs do not match.
Whatever the step, `new_string` gets the line ending of the line the match
starts on, so a CRLF file stays CRLF. The result says the line the change
starts on and, when it did not match exactly, what was ignored.

Without `replace_all`, the text must occur once in the step that finds it.
When it occurs more often, the error lists the line each occurrence starts
on with its anchor, `LINE:HASH`, where the hash is the first four hex digits
of the SHA-256 of the line without its line ending and trailing whitespace.
Passing one as `anchor` picks that occurrence; an anchor whose line has
changed since is refused, with what the line reads now.

When the text is found nowhere, the error names the file's version (the
first twelve hex digits of its SHA-256) and shows up to two windows of the
file most like `old_string`, with their line numbers, anchors and text as
it is now, and the first line that differs (or that they differ only in
whitespace, such as tabs against spaces). The model can correct its call
from that alone, so the planner does not tell it to read the file again;
when nothing is like it, it does. On a file too large to compare every
window (over 400,000 line comparisons), only windows that share a line with
`old_string` are scored.

## Adding a tool of your own

A tool is served by a service you run: a program that connects to the bus
(see `molt-sdk`) and provides the method the tool calls. Declare the tool on
the service in your user config or in a `--config` file outside the project,
and give the planner a capability for the method:

```toml
[[service]]
name = "lint"
exec = { command = "/usr/local/bin/molt-lint" }
provides = ["lint.*"]

[[service.tools]]
name = "lint"
description = "Lint one file of the workspace and list what it finds."
target = "lint.check"
read_only = true
roles = ["attempt", "reviewer"]
trust = "workspace"
timeout_ms = 30000
input_schema = { type = "object", properties = { path = { type = "string" } }, required = ["path"] }
```

The planner offers the tool only when it holds a capability for
`lint.check`: add `{ target = "lint.check", budget = { calls = 1000 } }` to
the planner's `requests`. In the user config a `[[service]] name = "planner"`
replaces the planner's `requests` as a whole, so copy the list
`molt config show` prints and add yours to it. Declaring a tool grants
nothing by itself.

The keys: `name` (letters, digits, `_`, `-`), `description`, `input_schema`
(a JSON schema of an object) and `target` (a method the service provides)
are required. `read_only` (default `false`) lets the call run alongside
other reads and is required for any role but `attempt`. `roles` defaults to
`["attempt"]`; the others are `designer`, `vetter`, `reviewer` and `judge`.
`trust` is `external` (default) or `workspace`. `timeout_ms` defaults to
60000, `max_result_bytes` to 50000 (the most). `strict = true` asks the
model API to hold calls to the schema exactly. An unknown key, a name one of
Molt's tools or another service already has, or a target the service does
not provide stops Molt before it starts, with the reason.

The service receives, at the tool's method,

```json
{ "tool": "lint", "workspace": "/path/to/the/attempts/fork", "input": { "path": "src/a.rs" } }
```

and answers `{ "content": "text for the model", "is_error": false }`. The
call has the tool's deadline and nothing to spend on further calls. The
model gets the content, cut to `max_result_bytes`, marked as data it must
not take instructions from. Every call and its answer is in the audit log.

For the tools of an MCP server, configure the server in `[mcp.NAME]`
instead: see [MCP servers](mcp.md).

A project cannot add tools: its `.molt/config.toml` may not have
`[[service]]`, and tool declarations in a `--config` file inside the
project are ignored, as its hooks are.
