# MCP servers

Molt can offer the tools of a [Model Context Protocol](https://modelcontextprotocol.io)
server to its attempts: an issue tracker, a database schema, design
documents, an internal API. Each server you configure becomes a Molt
service of its own, so what holds for every service holds for it: the
supervisor starts it, the kernel routes every call to it and writes the call
and its answer to the audit log, and the planner may call it only as many
times as its capability allows. Its tools are declarations of the kind
[Tools](tools.md) describes.

## Configuring a server

MCP servers are read only from your user config,
`~/.config/molt/config.toml`, or from a `--config` file outside the project:

```toml
[mcp.issues]
command = "/usr/local/bin/issues-mcp"   # serves MCP on stdin and stdout
args = ["--stdio"]
env = { TRACKER_URL = "https://tracker.example.com" }  # set for the server
pass_env = ["TRACKER_TOKEN"]            # copied from your environment
network = true                          # it needs the tracker; off by default
mounts = ["/opt/issues-mcp"]            # host paths it may read
calls = 200                             # calls a run may make (default 1000)
timeout_ms = 30000                      # deadline of one call (default 60000)

[mcp.issues.tools.get_issue]
read_only = true
roles = ["attempt", "designer"]

[mcp.issues.tools.delete_issue]
enabled = false
```

The server is bridged as service `mcp-issues` (a service name cannot hold a
dot), and the planner is given the capability to call `mcp-issues.call`
`calls` times per run; `calls = 0` offers none of its tools. Its tools
reach the model as `mcp__issues__TOOL`.

Per tool, under `[mcp.NAME.tools.TOOL]` (the server's name for the tool):

- `read_only` (default `false`): the tool changes nothing. Only you can say
  so; the server's own `readOnlyHint` is not taken on its word. A read-only
  tool runs alongside the other reads of a turn.
- `roles` (default `["attempt"]`): who may be offered it. Any role but
  `attempt` (`designer`, `vetter`, `reviewer`, `judge`) needs
  `read_only = true`: only a read-only tool can be given to the check
  designer.
- `enabled` (default `true`): `false` never offers it.
- `timeout_ms`, `max_result_bytes`: as in [Tools](tools.md).

Unknown keys stop Molt before it starts, with the reason.

## What a project cannot do

A project's `.molt/config.toml` that has `[mcp]` is refused, as one with
services or hooks is, and MCP servers in a `--config` file inside the
project are ignored with a warning. A project's `.mcp.json` is never read.
Whoever wrote the project cannot start a program on your machine through
MCP.

## The sandbox

A server runs in the sandbox the shell's commands get
([Linux command isolation](sandbox.md)), with the same resource limits and
the shell's `--sandbox-policy` if you gave one, except:

- it has no workspace: it sees the system's read-only directories, the
  paths in `mounts`, its own command file, a private `/tmp` and an empty
  home, `/home/molt`, which it starts in;
- the network is off unless the server says `network = true`, whatever the
  shell's policy says;
- its environment is `PATH`, the locale and the variables of its `env` and
  `pass_env`, never Molt's own settings, the model API keys or `HOME`.

A command found on `PATH` outside `/usr` and `/bin` (a Node or Python
installation in `/opt` or your home) needs its installation in `mounts`.
With `--no-sandbox` the servers run unconfined too, as the shell's commands
do; nothing falls back to that by itself.

## Results, schemas and the tool list

- Every result reaches the model as data from outside the workspace, in a
  block it cannot close, with a line telling it not to follow instructions
  in it. Text content is passed on; images and other content are named but
  not shown.
- Before a call reaches the server, the bridge checks its input against the
  tool's input schema (types, required and unknown fields, enums) and
  answers an input that does not fit with an error result instead. A tool
  whose schema the model API can hold calls to exactly is declared strict.
- A server that crashes or fails gives that call an error result; the run
  goes on, and the next call starts the server again (up to four starts a
  run).
- The tool list is read when Molt starts and cached in the data dir
  (`mcp/NAME.json`), so a run does not start the server twice. When the
  server now describes a tool differently than the run was told, or no
  longer lists it, the bridge refuses calls to that tool for the rest of
  the run, since what the model knows of it may no longer be true, and
  replaces the cached list, so the next run is offered the tool as the
  server describes it now. The same holds when the server announces a
  change (`notifications/tools/list_changed`) while a run is going.
- A server that cannot list its tools at startup is started without any;
  the run goes on with a warning.

## Not yet

- Servers reached over HTTP (streamable HTTP or SSE): only stdio servers
  are bridged.
- MCP resources, prompts and sampling: only tools.
