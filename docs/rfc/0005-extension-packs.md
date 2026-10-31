# RFC 0005: Extension packs

- Issue: #170 (roadmap #153, phase 3, C11)
- Related: #43 (MCP), #165 (hooks), #166 (Agent Skills), #164 (prompts and roles, RFC 0003), #10, #35, #114
- Status: schema 1 implemented, from paths, archives and git; unstable until it has been used for a phase, so the manifest may still change

## Summary

Phase 2 made the extension points declarations: MCP servers and hooks in the
user config, skills and command templates as files. Each is added by hand,
and nothing records who added what, for what, or how to take it out again.

An **extension pack** is a directory, a tar archive of one, or a commit of
a git repository, with a `molt-extension.toml` that bundles those
declarations, and bus services run as candidates. `molt ext add` installs it after showing what it would get
and getting a person's yes; `molt ext list`, `show`, `update`, `disable`,
`enable` and `remove` manage it. A pack
adds no new kind of extension and no new way to run code: its servers are
the MCP bridge services of #43, its services the candidate services of
`docs/services.md` and its hooks those of #165, under the same sandbox,
capability and audit rules as the user's own.

## The manifest, schema 1

```toml
schema = 1                  # required; another number is refused
name = "issue-tracker"      # 1-24 lowercase letters, digits, single hyphens, starting with a letter
version = "1.2.0"           # 1-64 characters, shown and recorded, not compared
description = "…"           # optional, at most 1024 bytes

[mcp.SERVER]                # molt_mcp::McpServer, as [mcp.NAME] in the user config
[[hooks]]                   # molt_planner::hooks::Hook, as [[hooks]] in the user config
[service.SERVICE]           # extensions::PackService: a candidate service, below
```

and, by the layout of a Claude Code plugin, `skills/NAME/SKILL.md` and
`commands/NAME.md`. Unknown keys are refused. `environment`, `role`,
`workflow`, `prompts`, `env` and `pass_env` are refused with a message
saying packs cannot bring them yet, so that a pack written for a
later schema fails loudly rather than losing part of itself.

`${MOLT_EXTENSION_DIR}` in a server's `command`, `args` and `env` values
becomes the installed directory; the directory is added to the server's
`mounts`. A command is absolute, a bare name looked up on `PATH`, or
`${MOLT_EXTENSION_DIR}/PATH` with a plain relative path. A hook may not name
the directory: hooks run in the workspace's sandbox, which does not see it.

A pack's server `S` is `[mcp.PACK-S]` (`[mcp.PACK]` when `S` is `PACK`), so
packs do not collide with each other's names or with the user's, and a
collision that remains is refused rather than resolved by order.

## Services

A pack's service `S` runs as `ext-PACK-S` (`ext-PACK` when `S` is `PACK`);
no service of Molt's and no MCP bridge (`mcp-NAME`) has the prefix, and a
collision with the user's `[[service]]` or another pack's is refused.
`PackService::section` turns it into the `[[service]]` the user config
would hold, with only what a pack may not choose fixed:

- `tier = "mutable"`, `isolation = "sandbox"`, `provides = ["ext-PACK-S.*"]`;
  the keys are not in `PackService`, so a manifest naming them is refused;
- `exec` from `command` and `args`, with `${MOLT_EXTENSION_DIR}` expanded,
  under the same rule as a server's command;
- `mounts = [the installed directory]`: `ServiceSection::mounts`, new, is
  the host paths a sandboxed service reads, bound read-only at their own
  paths by `SandboxPolicy::service_command`. The user config may set it
  too; it is refused on a native service, and a mount that is the data
  dir, holds it or lies in it stops the start;
- `requests`, `env`, `pass_env`, `tools`, `memory_mb` and `cpu_secs` as
  given, under the checks of a sandboxed `[[service]]` (no `MOLT_*`,
  `ANTHROPIC_*`, `OPENAI_*`, `HOME`, `PATH`, `TMPDIR`) and of RFC 0002's
  tools (targets on its own methods, no planner tool's name, no user
  trust). A request for a kernel method is refused at install, before the
  kernel would refuse the registry's at launch.

`calls` (default 1000, as a server's) becomes the planner's capability for
`ext-PACK-S.*` with that budget, only when the service declares tools.
The service is then a candidate like any other: the supervisor starts it
only in the sandbox, without the network, with its private state, and a
gate may promote a version of it under the registry's rules, which never
widen its capabilities. A pack with a service does not start where the
sandbox is unavailable.

## The grant

`extensions::grant(manifest, root)` is the list of what the pack would get,
one human-readable line per thing, sorted:

- per server: the command line it runs; that it has the network; each host
  path it reads; each of the user's variables it gets (`pass_env`); each
  variable it is set (`env`, with its value); how many calls a run the
  planner gets for it (the capability `mcp-PACK-S.call` with that budget);
  who is offered its tools, and which only read;
- per service: the command line it runs in the candidates' sandbox, each
  capability it requests with its budget (calls, tokens, ms), each of the
  user's variables it gets and each variable it is set, and, when it has
  tools, how many calls a run the planner gets for it and each tool with
  its target, roles, whether it only reads and whether its results are
  believed as the workspace's;
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
   and approval, and the servers' cached tool lists and the services'
   private state in the default data dirs and in those named with
   `--data-dir`.
7. **A pack's code is never trusted.** Its servers run in the MCP sandbox
   and its services as candidates; neither runs as a native host process,
   and a service of a pack can hold no kernel capability.

Why approval is of capabilities rather than code: a pack's code runs in the
sandbox its grant describes, with the network, paths, variables and calls
the grant names, and its hooks' commands are in the grant verbatim. New code
inside the same grant can do nothing the old could not. Pinning by code
would ask for approval on every update and teach people to say yes.

## Git sources

`molt ext add URL[#REF]` (`extensions::GitSource`, `extensions::git`)
takes `https://`, `ssh://` and `USER@HOST:PATH` URLs, and a branch, tag or
full commit id after `#` (the remote's `HEAD` without one). It refuses
`http://` and `git://` (unencrypted), `file://` (a directory is copied
instead), git's `TRANSPORT::` forms such as `ext::`, an https URL with a
user or token in it (it would be recorded), a host or user starting with
`-`, and a reference that is not a plain ref name.

Fetching runs nothing of the repository's:

1. `git init --bare` with an empty template directory, in the staging
   directory.
2. `git fetch --depth=1 --no-tags --no-recurse-submodules URL +REF:refs/molt/fetched`,
   with an environment emptied but for `PATH`, `HOME`, `SSH_AUTH_SOCK` and
   the proxy and CA variables; `GIT_CONFIG_NOSYSTEM=1`,
   `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`,
   `GIT_SSH_COMMAND="ssh -o BatchMode=yes"`, `GIT_ALLOW_PROTOCOL` and
   `protocol.allow=never` with only the URL's own protocol allowed; and on
   the command line `core.hooksPath=/dev/null`, `core.fsmonitor=false`,
   `core.askPass=`, `credential.helper=`, `transfer.fsckObjects=true`,
   `fetch.recurseSubmodules=false`, `gc.auto=0`. It is stopped after 120 s,
   or once the scratch repository passes 128 MiB.
3. The commit's tree is listed (`ls-tree -r -l -z`) and its blobs read
   (`cat-file --batch`) and written by Molt: nothing is checked out, so
   no attribute, filter, smudge or LFS process applies. A symlink (mode
   120000), a submodule (160000), a path that is not plain or names `.git`
   refuses the pack; sizes are checked against the pack's limits before
   any blob is read.

The files then go through the same staging as a directory's: the importer,
the manifest, the hash and the grant. The record gains a `git` table with
the URL, the reference and the full commit id, and the approval is
otherwise the same. `molt ext update NAME` fetches the reference again (the
recorded URL is checked again as above); the same commit changes nothing,
a moved one is reported from commit to commit and installed under rule 3
below, so a grant with a new line is shown with `+` and needs a yes. A pack
installed at a commit id does not move, and one from a path is not fetched.

Why not a sandbox for git: the fetch needs the network and the user's ssh
keys, and git's own switches already turn off every program it would run.
The scrubbed invocation is the floor; running it under the MCP sandbox with
the network is deferred.

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
the file. The `git` table of a pack installed from git is optional and
written only for such a pack: a file without one reads as before, and a
Molt older than it refuses a file with one, and so starts nothing, rather
than lose the pin. The planner gets the packs' skills in
`MOLT_EXTENSION_SKILLS`, a new variable; a planner without it simply
offers none. `[service]` is new in schema 1 without a bump: a Molt before
it refused the key by name, so no pack with services was ever read
without them. `mounts` on a `[[service]]` is a new optional key of the
user config, absent by default.

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
- A pack's service is shown with what it may use, runs only as a sandboxed
  candidate under its pack's name and is removed with its state; it cannot
  choose its tier, isolation or methods, or request a kernel method
  (`extensions::tests::a_packs_service_is_shown_with_what_it_may_use_and_runs_only_as_a_sandboxed_candidate`,
  `a_packs_service_cannot_choose_its_tier_isolation_methods_or_kernel_rights`;
  `layers::tests::an_enabled_packs_service_joins_as_a_sandboxed_candidate_the_planner_may_call`;
  `crates/molt/tests/candidate.rs`,
  `an_installed_packs_service_runs_as_a_sandboxed_candidate_on_the_bus`,
  `a_candidate_is_not_started_with_a_mount_that_shows_it_the_data_dir`).
- A pack from git is pinned to its commit, nothing of the repository runs,
  a symlink or submodule refuses it, only https and ssh are fetched, and an
  update reports the moved ref and asks again only for more
  (`extensions::tests::a_pack_from_a_git_repository_is_installed_at_its_ref_and_pinned_to_the_commit`,
  `nothing_in_a_fetched_repository_runs`,
  `a_fetched_tree_with_a_symlink_is_refused_as_a_directory_is`,
  `an_update_from_git_reports_the_ref_that_moved_and_asks_again_only_for_more`;
  `extensions::git::tests::only_https_and_ssh_urls_are_fetched_and_never_through_a_transport_program`).

## Deferred

- Installing from a registry. Fetching git submodules and LFS files, and
  running the git fetch in a sandbox of its own.
- Environment profiles, roles, prompts and workflows in a pack. Roles and
  prompts need RFC 0003's promotion to cover a pack's protected prompts.
- A pack's service with the network, or with mounts beyond the pack's
  directory: a candidate has neither today.
- Importing Claude Code hooks (their tool names and `${CLAUDE_PLUGIN_ROOT}`
  scripts do not carry over to Molt's sandboxed hooks) and agents (roles).
- Comparing a server's `calls` numerically, so that fewer calls is not a
  new line.
- Evaluating a pack on the benchmark before enabling it.
