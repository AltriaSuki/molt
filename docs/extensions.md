# Extension packs

An extension pack bundles what you would otherwise add to your
configuration by hand: [MCP servers](mcp.md), bus
[services](services.md) run as sandboxed candidates, [lifecycle
hooks](config.md#trusted-lifecycle-hooks), [Agent Skills](skills.md) and
[command templates](tui.md#project-task-commands). It is a directory, a
`.tar`, `.tar.gz` or `.tgz` archive of one, or a git repository, with a
`molt-extension.toml` at the top, and can be found by its name in a
[registry](#registries). The design is RFC 0005,
[rfc/0005-extension-packs.md](rfc/0005-extension-packs.md); the format is
unstable until it has been used for a phase of the roadmap.

```sh
molt ext add ./issue-tracker         # shows what it would get, then asks
molt ext add issue-tracker.tgz --yes # the same, approved from a script
molt ext add https://github.com/team/issue-tracker#v1.2.0   # one commit of a repository
molt ext update issue-tracker        # that branch or tag again, if it moved
molt ext registry add team https://github.com/team/molt-packs   # where names are looked up
molt ext search issues               # what the registries list
molt ext add issue-tracker           # by its name, from the registry that lists it
molt ext list
molt ext show issue-tracker          # where it is and what you approved
molt ext disable issue-tracker       # off, still installed
molt ext enable issue-tracker
molt ext remove issue-tracker        # gone, with everything it added
```

## The manifest

```toml
schema = 1
name = "issue-tracker"        # 1 to 24 lowercase letters, digits and single hyphens
version = "1.2.0"
description = "Read and file issues on the team tracker."

# MCP servers, with the keys of [mcp.NAME] in your config (mcp.md).
[mcp.issues]
command = "${MOLT_EXTENSION_DIR}/bin/issues-mcp"   # a program in the pack
args = ["--stdio"]
pass_env = ["TRACKER_TOKEN"]
network = true
calls = 200

[mcp.issues.tools.get_issue]
read_only = true
roles = ["attempt", "designer"]

# Bus services, each a candidate in the sandbox (see "Services" below).
[service.notes]
command = "${MOLT_EXTENSION_DIR}/bin/notes"
requests = [{ target = "memory.search", budget = { calls = 30 } }]
calls = 40                    # how often the agent may call it in a run

[[service.notes.tools]]
name = "find_note"
description = "Find a team note by its title."
target = "ext-issue-tracker-notes.find"
read_only = true
input_schema = { type = "object", properties = { title = { type = "string" } } }

# Lifecycle hooks, with the keys of [[hooks]] in your config.
[[hooks]]
event = "tool.before"
matcher = "edit_file|write_file"
paths = ["migrations/**"]
command = "echo 'migrations are frozen' >&2; exit 2"
blocking = true
```

Beside it, by convention as in a Claude Code plugin:

- `skills/NAME/SKILL.md`: Agent Skills, offered as yours are;
- `commands/NAME.md`: command templates for the interface, in the format of
  a project's `.molt/commands`.

`${MOLT_EXTENSION_DIR}` in a server's `command`, `args` and `env` is the
directory the pack is installed in. A server's command is an absolute path,
a name looked up on `PATH`, or `${MOLT_EXTENSION_DIR}/PATH` for a program in
the pack. Each server of the pack may read the pack's files, besides its
own `mounts`. A server `NAME` of pack `PACK` is bridged as `PACK-NAME`, or
`PACK` when the two are the same, so its tools reach the model as
`mcp__PACK-NAME__TOOL`. Hooks run in the workspace's sandbox, which cannot
see the pack, so a hook's command is the whole of it.

A manifest with environment profiles, roles, workflows, prompts or `env` is
refused with the reason: packs cannot bring those yet. So is an
unknown key, a schema other than 1, and anything the user config would
refuse in `[mcp]` or `[[hooks]]` (a server given `ANTHROPIC_API_KEY`, say).

## Services

A pack's `[service.NAME]` is a bus service, as a `[[service]]` of your
config is ([Service trust](services.md)), but always a **candidate**: one
an agent may replace, run in the Linux sandbox and never natively. The
pack chooses less than your config would:

- It runs as `ext-PACK-NAME` (`ext-PACK` when `NAME` is the pack's), and
  provides that service's methods, `ext-PACK-NAME.*`, and no others. Its
  tier is `mutable`, its isolation `sandbox`. `tier`, `isolation`,
  `provides`, `exec` and `mounts` are not keys a pack has: they are refused
  as unknown.
- `command` and `args` are as an MCP server's, with `${MOLT_EXTENSION_DIR}`
  for the installed directory, which it reads (and nothing else of your
  machine) through its one mount. It has no network, its private state at
  `<data dir>/state/ext-PACK-NAME`, and its `env`, `pass_env`, `memory_mb`
  and `cpu_secs` as yours would; `MOLT_*`, `ANTHROPIC_*`, `OPENAI_*`,
  `HOME`, `PATH` and `TMPDIR` are refused.
- `requests` are the capabilities it holds, each with its budget, as a
  `[[service]]`'s. A kernel method (`kernel.*`, the registry's included) is
  refused.
- `[[service.NAME.tools]]` are tools on its own methods, in the format of a
  service's ([Tools](tools.md)), offered to the roles they name; `calls`
  (1000 by default, 0 for none) is how often the agent may call the service
  in a run, and without it its tools are not offered. A tool with the name
  of one of the planner's own, of a tool of another pack, or with the
  user's trust is refused.

As any candidate, it does not start where the sandbox is unavailable, and
then neither does `molt`: `molt ext disable PACK` to run without it.

## What installing shows and asks

`molt ext add` copies the pack into a staging directory, checks it, and
lists what it would get, one line per thing:

```text
extension issue-tracker 1.2.0 (sha256 3f2a9c01d4be) from /home/me/issue-tracker
  Read and file issues on the team tracker.
It would get:
  MCP server issue-tracker-issues gets your environment variable TRACKER_TOKEN
  MCP server issue-tracker-issues has the network
  MCP server issue-tracker-issues runs ${MOLT_EXTENSION_DIR}/bin/issues-mcp --stdio in a sandbox that sees the pack's files
  MCP server issue-tracker-issues's tool get_issue is offered to attempt, designer, as a tool that only reads
  MCP server issue-tracker-issues's tools are offered to attempts unless listed here
  hook tool.before for tools matching "edit_file|write_file" for paths migrations/** runs `echo 'migrations are frozen' >&2; exit 2` in the workspace's sandbox, and denies when it fails
  service ext-issue-tracker-notes may use memory.search within 30 calls, 0 tokens and 0 ms
  service ext-issue-tracker-notes runs ${MOLT_EXTENSION_DIR}/bin/notes in a candidate's sandbox that sees the pack's files, without the network
  service ext-issue-tracker-notes's tool find_note (ext-issue-tracker-notes.find) is offered to attempt, as a tool that only reads
  the agent may call MCP server issue-tracker-issues 200 times a run (mcp__issue-tracker-issues__*)
  the agent may call service ext-issue-tracker-notes 40 times a run (ext-issue-tracker-notes.*)
Install it? [y/N]
```

These lines are the pack's **grant**: the programs it runs, their network,
the host paths they read, your environment variables they get and the
variables they are set, the capabilities its services hold and their
budgets, how often the agent may call them and who is offered their tools,
each hook's command, and its skills and commands.
Nothing the pack holds is run to install it.

It asks at a terminal. Without one it installs only with `--yes`; otherwise
it prints the list, installs nothing and exits 1. What you approve is
recorded, line by line, with the pack's content hash, in
`~/.config/molt/extensions.toml` (under `$XDG_CONFIG_HOME` when that is
set); the pack is installed in `~/.local/share/molt/extensions/NAME/`
(under `$XDG_DATA_HOME`). The next `molt do`, `molt ask`, interface, `molt
serve` or `molt acp` starts it.

## A pack never gets more than you approved

- **At every start** the installed files are hashed and compared with the
  hash they were installed with, and the pack's grant is worked out again
  and compared with what you approved. A pack that fails either stops the
  start with what to do: install it again, or disable it.
- **An update** is `molt ext add` of a pack with the name of an installed
  one. One whose grant has a line you have not approved (the network, one
  more variable, another command, more calls, a new hook) lists the new
  lines with `+` and is installed only once you approve again; refused,
  the installed pack stays as it was. A line that changes is a new line,
  even to fewer calls. One that asks for nothing more, such as new code for
  the same server, or a server or hook fewer (listed with `-`), is
  installed with a note and without asking. The approval then becomes the
  new grant, so what an update gave up is not given back silently.
- **A project cannot install or enable a pack.** A `.molt/config.toml` with
  `extension` or `extensions` is refused, nothing in a workspace is read as
  a pack unless you run `molt ext add` on it, and a pack installed inside
  the workspace is refused. `--config FILE` starts no packs, as it reads no
  user skills.
- **Names do not shadow.** A pack whose server would be bridged, or whose
  service would run, under a name your config's `[mcp]` or `[[service]]`
  or another pack already has is refused, at install and at start; so is
  a service's tool with the name of another pack's. Your skills win over a pack's of the same name, and a
  pack's command over a project's.
- **The pack runs where yours would.** Its servers run in the MCP sandbox
  ([mcp.md](mcp.md#the-sandbox)), with the network only when the grant says
  so; its services in the candidates' sandbox, never with the network; its
  hooks in the shell's sandbox. Its skills are offered as yours, in
  the same `<skills>` block and under the same limits.

## Paths and archives

A directory is copied file by file, never through a symlink: a pack that
holds a symlink, a device or a pipe is refused, wherever it points, and a
`.git` at its top is left out. An archive is read entry by entry: an entry
with `..` or an absolute path, a symlink, a hard link or a device refuses
the whole archive, before anything is installed. An archive of one
directory is read as the pack in it. A pack holds at most 2048 files and
directories, 64 MiB and 16 levels of directories.

## From a git repository

`molt ext add URL[#REF]` fetches one commit of a repository and installs
its files as it would a directory's. `REF` is a branch, a tag or a full
commit id; without one it is the repository's default branch (its `HEAD`).

```text
extension issue-tracker 1.2.0 (sha256 3f2a9c01d4be) from https://github.com/team/issue-tracker#v1.2.0, commit 8c41d07e9a2b
It would get:
  ...
```

- **Over https or ssh only.** `https://HOST/PATH`, `ssh://[USER@]HOST/PATH`
  and `USER@HOST:PATH` are fetched. `http://` and `git://` are refused, as
  anyone on the way could change what arrives; `file://` is refused (give
  the directory instead: it is copied without its `.git`); and git's
  `TRANSPORT::ADDRESS` forms, such as `ext::`, which run a program to reach
  a repository, are never fetched. An https URL with a user or a token in
  it is refused, as it would be recorded; a private repository is reached
  with your ssh keys and agent, or a token in `~/.netrc`.
- **Nothing of the repository runs.** Git runs with neither your nor the
  system's git configuration, an environment emptied but for `PATH`,
  `HOME`, `SSH_AUTH_SOCK` and the proxy and CA variables, no credential
  helper or prompt (ssh runs with `BatchMode=yes`), no hooks, only the URL's
  own protocol allowed, every object checked as it arrives, and no
  submodule fetched. The commit is fetched into a new bare repository and
  never checked out: its files are written from the commit's tree by Molt,
  so no `.gitattributes` filter, smudge or LFS step applies, and a
  `.gitmodules` is only a file.
- **Checked as a directory.** A symlink in the tree, or a submodule,
  refuses the pack. The files then go through the same checks, limits,
  hash and grant as a directory's. The fetch stops after 120 seconds, or
  once the fetched objects pass 128 MiB.
- **Pinned to the commit you approved.** `extensions.toml` records the URL,
  the reference and the full commit id beside the approval, and `molt ext
  show` prints them. The installed files are checked against their hash at
  each start, as any pack's.

`molt ext update NAME` fetches the recorded branch or tag again. When it
still names the installed commit it says so and changes nothing. When it
moved, it says from which commit to which, and the new commit is installed
as an update by `molt ext add` would be: an update whose grant has a line
you have not approved lists it with `+` and needs your yes (at a terminal,
or `--yes`), and refused, the installed commit stays; one that asks for
nothing more goes in with a note. A pack installed at a commit id is not
moved: `molt ext add URL#REF` installs another. A pack installed from a
path is updated with `molt ext add` of the path.

## Registries

A registry is an index of packs by name: a git repository or a directory
with a `molt-registry.toml` at its top.

```toml
schema = 1

[pack.issue-tracker]
description = "Read and file issues on the team tracker."
git = "https://github.com/team/issue-tracker"
rev = "v1.2.0"                # a branch, a tag or a whole commit id; HEAD without one

[pack.notes]                  # only in a registry that is a directory:
path = "packs/notes"          # a pack's directory or archive beside the index
```

```sh
molt ext registry add team https://github.com/team/molt-packs#main
molt ext registry add local /srv/molt-packs    # a directory, or its molt-registry.toml
molt ext registry list
molt ext search notes        # name or description, any case; marks what is installed
molt ext add issue-tracker   # the pack a registry lists under that name
molt ext add notes --registry local
molt ext registry remove team
```

- **A registry says where; you still approve what.** `molt ext add NAME`
  fetches what the registry lists and installs it as `molt ext add` of
  that URL and reference, or of that path, would: the grant is shown, with
  `(listed by registry team)` after the source, and nothing is installed
  or run before your yes. An update the registry points to is approved as
  any update: a line you have not approved is shown with `+`.
- **The name is the pack's.** The pack fetched must have the name it was
  listed under, or nothing is installed. A name two of your registries
  list is refused until you choose one with `--registry`, and so is any
  `molt ext add NAME` while one of your registries cannot be read, since it
  might list the name too.
- **A path wins.** `molt ext add notes` installs `./notes` when that
  exists, as before registries; `--registry` looks the name up even then.
- **Fetched as a pack is.** A registry in git is fetched as a pack's
  repository is ([From a git repository](#from-a-git-repository)): over
  https or ssh only, running nothing of it, each time it is read. Its
  packs are git URLs under the same rules; a `path` is refused in it, and
  in a directory's registry a `path` that leads out of the directory is.
  An index is at most 1 MiB and 4096 packs, and unknown keys or a schema
  other than 1 refuse it.
- **Where they are kept.** `molt ext registry add` reads the index once to
  check it, and records the URL or the directory in
  `~/.config/molt/registries.toml`; a project's configuration cannot name
  one. A pack installed from a registry is pinned as one installed from its
  URL: `molt ext update NAME` follows its branch or tag, and `molt ext add
  NAME` again follows what the registry lists now. Removing a registry
  keeps the packs installed from it.

## Disabling and removing

`molt ext disable NAME` keeps the pack and its approval, and the next start
runs none of its servers, hooks, skills or commands. `molt ext enable NAME`
checks it as a start does and turns it back on.

`molt ext remove NAME` deletes the installed files, the record and the
approval, and what its servers and services left in the default data dirs:
the servers' tool lists (`~/.cache/molt/*/mcp/`) and the services' private
state (`~/.cache/molt/*/state/ext-NAME…`). A data dir you chose with
`--data-dir` is swept too when you name it: `molt ext remove NAME
--data-dir DIR` (repeatable). Otherwise it keeps them, and nothing reads
them once the pack is gone.

## Claude Code plugins

`molt ext add` of a directory with `.claude-plugin/plugin.json` and no
`molt-extension.toml` imports it, and says what it left out:

- `skills/` is kept as it is;
- `commands/NAME.md` is kept with its `description` and `argument-hint`;
  `allowed-tools`, `model` and the rest are left out, since Molt's commands
  set none, and a command Molt cannot read, or one in a subdirectory, is
  left out;
- the servers of `.mcp.json` (and of `mcpServers` in `plugin.json`) that run
  on stdio become MCP servers without the network, `${CLAUDE_PLUGIN_ROOT}`
  becoming `${MOLT_EXTENSION_DIR}`; a variable set to `${NAME}` of its own
  name is passed from your environment, and a server reached by URL, or with
  a variable made from others, is left out;
- hooks of `hooks/hooks.json` (and of `hooks` in `plugin.json`, files or
  inline) on `PreToolUse`, `PostToolUse` and `UserPromptSubmit` become the
  pack's `tool.before`, `tool.after` and `prompt.submit` hooks, with
  `dialect = "claude-code"` (see below) and their `matcher` and `timeout`;
- agents are not imported: they are roles, which packs cannot bring yet.

A hook runs in the workspace's sandbox, which cannot see the pack, so a
hook whose command runs one of the plugin's scripts
(`${CLAUDE_PLUGIN_ROOT}/hooks/check.sh`, or `python3
"${CLAUDE_PLUGIN_ROOT}/hooks/check.py"`) gets the script written into its
command (`bash -c '…'`, by the script's `#!` line or the interpreter
named: `sh`, `bash`, `zsh`, `python3`, `python` or `node`), and the grant
shows it whole. A script that uses the plugin's directory or files beside
itself (`$0`, `BASH_SOURCE`, `__file__`, `__dirname`, `import.meta`), a
command that does more than run one script, a hook of another type than
`command`, one matching MCP tools (Claude Code names a plugin's servers
otherwise), and the events Molt has none of (`Stop`, `SessionStart`,
`Notification`, …) are left out, each with why. `$CLAUDE_PROJECT_DIR` is
the directory the hook runs in. At most 32 hooks are taken.

A hook with `dialect = "claude-code"` is told of a tool call in Claude
Code's names: `tool_name` is `Read`, `Write`, `Edit`, `Bash`, `Grep` or
`WebFetch` for `read_file`, `write_file`, `edit_file`, `run`, `search` and
`fetch_url`, its `matcher` is matched against that name, `tool_input` has
`file_path` for `path` (and `timeout` in ms for `timeout_s`, `-i` for
`case_insensitive`), and `hook_event_name` is `PreToolUse`, `PostToolUse`
or `UserPromptSubmit`. An `updatedInput` it answers is read back into
Molt's. Molt's other tools keep their names. The hook is told nothing of a
Claude Code session (`session_id`, `transcript_path`).

The manifest it writes is in the installed pack, and the grant is shown and
approved as any pack's.

## Not yet

Signed registries or packs, fetching git submodules or LFS files, git
fetches in a sandbox of their own, environment profiles, roles, prompts
and workflows in a pack, a pack's service with the network or a mount
beyond the pack, importing a Claude Code plugin's
agents, or its hooks on events Molt does not have, and evaluating a pack on the benchmark before it is
enabled.
