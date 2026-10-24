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
