# Extension packs

An extension pack bundles what you would otherwise add to your
configuration by hand: [MCP servers](mcp.md), [lifecycle
hooks](config.md#trusted-lifecycle-hooks), [Agent Skills](skills.md) and
[command templates](tui.md#project-task-commands). It is a directory, or a
`.tar`, `.tar.gz` or `.tgz` archive of one, with a `molt-extension.toml` at
the top. The design is RFC 0005,
[rfc/0005-extension-packs.md](rfc/0005-extension-packs.md); the format is
unstable until it has been used for a phase of the roadmap.

```sh
molt ext add ./issue-tracker         # shows what it would get, then asks
molt ext add issue-tracker.tgz --yes # the same, approved from a script
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

A manifest with services, environment profiles, roles, workflows, prompts or
`env` is refused with the reason: packs cannot bring those yet. So is an
unknown key, a schema other than 1, and anything the user config would
refuse in `[mcp]` or `[[hooks]]` (a server given `ANTHROPIC_API_KEY`, say).

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
  the agent may call MCP server issue-tracker-issues 200 times a run (mcp__issue-tracker-issues__*)
Install it? [y/N]
```

These lines are the pack's **grant**: the programs it runs, their network,
the host paths they read, your environment variables they get and the
variables they are set, how often the agent may call them and who is
offered their tools, each hook's command, and its skills and commands.
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
- **Names do not shadow.** A pack whose server would be bridged under a name
  your config's `[mcp]` or another pack already has is refused, at install
  and at start. Your skills win over a pack's of the same name, and a
  pack's command over a project's.
- **The pack runs where yours would.** Its servers run in the MCP sandbox
  ([mcp.md](mcp.md#the-sandbox)), with the network only when the grant says
  so; its hooks in the shell's sandbox. Its skills are offered as yours, in
  the same `<skills>` block and under the same limits.

## Paths and archives

A directory is copied file by file, never through a symlink: a pack that
holds a symlink, a device or a pipe is refused, wherever it points, and a
`.git` at its top is left out. An archive is read entry by entry: an entry
with `..` or an absolute path, a symlink, a hard link or a device refuses
the whole archive, before anything is installed. An archive of one
directory is read as the pack in it. A pack holds at most 2048 files and
directories, 64 MiB and 16 levels of directories.

## Disabling and removing

`molt ext disable NAME` keeps the pack and its approval, and the next start
runs none of its servers, hooks, skills or commands. `molt ext enable NAME`
checks it as a start does and turns it back on.

`molt ext remove NAME` deletes the installed files, the record and the
approval, and the tool lists its servers left in the default data dirs
(`~/.cache/molt/*/mcp/`). A data dir you chose with `--data-dir` keeps its
cached list, which nothing reads once the server is gone.

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
- hooks and agents are not imported yet.

The manifest it writes is in the installed pack, and the grant is shown and
approved as any pack's.

## Not yet

Installing from a git URL or a registry, services, environment profiles,
roles, prompts and workflows in a pack, importing a Claude Code plugin's
hooks and agents, and evaluating a pack on the benchmark before it is
enabled.
