# RFC 0005: Extension packs

- Issue: #170 (roadmap #153, phase 3, C11)
- Related: #43 (MCP), #165 (hooks), #166 (Agent Skills), #164 (prompts and roles, RFC 0003), #10, #35, #114
- Status: schema 1 implemented; unstable until it has been used for a phase, so the manifest may still change

## Summary

Phase 2 made the extension points declarations: MCP servers and hooks in the
user config, skills and command templates as files. Each is added by hand,
and nothing records who added what, for what, or how to take it out again.

An **extension pack** is a directory, or a tar archive of one, with a
`molt-extension.toml` that bundles those declarations. `molt ext add`
installs it after showing what it would get and getting a person's yes;
`molt ext list`, `show`, `disable`, `enable` and `remove` manage it. A pack
adds no new kind of extension and no new way to run code: its servers are
the MCP bridge services of #43 and its hooks those of #165, under the same
sandbox, capability and audit rules as the user's own.

## The manifest, schema 1

```toml
schema = 1                  # required; another number is refused
name = "issue-tracker"      # 1-24 lowercase letters, digits, single hyphens, starting with a letter
version = "1.2.0"           # 1-64 characters, shown and recorded, not compared
description = "…"           # optional, at most 1024 bytes

[mcp.SERVER]                # molt_mcp::McpServer, as [mcp.NAME] in the user config
[[hooks]]                   # molt_planner::hooks::Hook, as [[hooks]] in the user config
```

and, by the layout of a Claude Code plugin, `skills/NAME/SKILL.md` and
`commands/NAME.md`. Unknown keys are refused. `service`, `environment`,
`role`, `workflow`, `prompts`, `env` and `pass_env` are refused with a
message saying packs cannot bring them yet, so that a pack written for a
later schema fails loudly rather than losing part of itself.

`${MOLT_EXTENSION_DIR}` in a server's `command`, `args` and `env` values
becomes the installed directory; the directory is added to the server's
`mounts`. A command is absolute, a bare name looked up on `PATH`, or
`${MOLT_EXTENSION_DIR}/PATH` with a plain relative path. A hook may not name
the directory: hooks run in the workspace's sandbox, which does not see it.

A pack's server `S` is `[mcp.PACK-S]` (`[mcp.PACK]` when `S` is `PACK`), so
packs do not collide with each other's names or with the user's, and a
collision that remains is refused rather than resolved by order.

## The grant

`extensions::grant(manifest, root)` is the list of what the pack would get,
one human-readable line per thing, sorted:

- per server: the command line it runs; that it has the network; each host
  path it reads; each of the user's variables it gets (`pass_env`); each
  variable it is set (`env`, with its value); how many calls a run the
  planner gets for it (the capability `mcp-PACK-S.call` with that budget);
  who is offered its tools, and which only read;
- per hook: its event, matcher, paths and command, and whether it may deny;
- each skill, and each command.

The lines are what the person approves, and approval is of lines, not of a
version: `~/.config/molt/extensions.toml` records, per pack, the version,
the source, the SHA-256 of the installed files (paths, executable bits and
contents, in path order), whether it is enabled, and the approved lines.

## Trust rules

1. **Nothing runs before approval.** Installing reads and copies files; it
   does not start a server or a hook, nor list a server's tools.
2. **A person approves.** `molt ext add` asks at a terminal; without one it
   needs `--yes`, which is as explicit as typing `y` and is how a script
   approves. An agent cannot reach either: its commands run in a sandbox
   that sees neither the user's config nor their data directory.
3. **A pack never gets more than was approved.** Each start hashes the
   installed files against the recorded hash and computes the grant again
   against the approved lines; either mismatch stops the start with what to
   do. An update whose grant has any line not approved before is installed
   only after a new approval, the new lines marked; refused, the installed
   pack stays. An update with no new line is installed with a note. The
   approved lines become the new grant either way, so a narrowing is never
   silently widened back.
4. **Only the user's layer starts packs.** `Layered::build` merges enabled
   packs right after the user config; `--config` does not. A project config
   with `extension` or `extensions` is refused; a pack installed inside the
   workspace is refused.
5. **Install is all or nothing.** Files are staged in a private directory
   beside the installed packs; a directory with a symlink or a special file,
   or an archive with an entry that is absolute, contains `..`, or is a
   link or a device, is refused before anything is moved into place.
6. **Uninstall removes what was added**: the installed directory, the record
   and approval, and the servers' cached tool lists in the default data
   dirs.

Why approval is of capabilities rather than code: a pack's code runs in the
sandbox its grant describes, with the network, paths, variables and calls
the grant names, and its hooks' commands are in the grant verbatim. New code
inside the same grant can do nothing the old could not. Pinning by code
would ask for approval on every update and teach people to say yes.

## Claude Code plugins

`extensions::import_claude_plugin` turns a directory with
`.claude-plugin/plugin.json` into a pack in its staging copy: skills as they
are; commands with the front matter Molt reads; stdio servers of `.mcp.json`
with `${CLAUDE_PLUGIN_ROOT}` mapped, `"${X}"` for a variable `X` as
`pass_env`, and no network. HTTP servers, variables built from others,
nested commands, hooks and agents are left out and said so. The written
manifest is shown and approved as any other.

## Compatibility

Schema 1 is the first; nothing earlier reads packs. A Molt that meets a
higher schema refuses the pack and says to update. `extensions.toml` denies
unknown fields, so a newer Molt that adds one must also bump how it reads
the file. The planner gets the packs' skills in `MOLT_EXTENSION_SKILLS`, a
new variable; a planner without it simply offers none.

## Acceptance

- Installing shows the grant, and nothing runs before the yes
  (`crates/molt/tests/agent.rs`,
  `an_extension_pack_is_shown_approved_offered_disabled_kept_to_its_approval_and_removed`;
  `extensions::tests::installing_lists_every_power_and_records_the_approval_only_on_yes`).
- A disabled pack's tools are not offered in a run (the same test).
- A Claude Code plugin with commands and skills is imported
  (`extensions::tests::a_claude_code_plugin_with_commands_skills_and_a_server_is_imported`).
- An update asking for more is approved again; a changed or over-reaching
  installed pack stops the start; path traversal and links install nothing
  (`extensions::tests`).

## Deferred

- Installing from a git URL or a registry, and pinning a remote by hash.
- Services with capabilities of their own, environment profiles, roles,
  prompts and workflows in a pack. Roles and prompts need RFC 0003's
  promotion to cover a pack's protected prompts.
- Importing Claude Code hooks (their tool names and `${CLAUDE_PLUGIN_ROOT}`
  scripts do not carry over to Molt's sandboxed hooks) and agents (roles).
- Comparing a server's `calls` numerically, so that fewer calls is not a
  new line.
- Evaluating a pack on the benchmark before enabling it.
