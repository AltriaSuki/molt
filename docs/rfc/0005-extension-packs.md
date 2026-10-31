# RFC 0005: Extension packs

- Issue: #170 (roadmap #153, phase 3, C11)
- Related: #43 (MCP), #165 (hooks), #166 (Agent Skills), #164 (prompts and roles, RFC 0003), #10, #35, #114
- Status: schema 1 implemented, from paths, archives, git and registries, with roles and prompts; unstable until it has been used for a phase, so the manifest may still change

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
[role.ROLE]                 # extensions::PackRole: a role of the prompt pack, below
```

and, by the layout of a Claude Code plugin, `skills/NAME/SKILL.md` and
`commands/NAME.md`, and `prompts/NAME.md` for the roles. Unknown keys are
refused. `environment`, `workflow`, `env` and `pass_env` are refused with a
message saying packs cannot bring them yet, so that a pack written for a
later schema fails loudly rather than losing part of itself; `prompts` is
refused with where prompts go, and `[[role]]` with how to write a role.

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

## Roles and prompts

A pack's `[role.R]` is RFC 0003's `RoleDecl` without `name`
(`extensions::PackRole`), and `prompts/P.md` a prompt file as in the
user's prompt pack (at most 64 KiB, its trailing line breaks dropped).
They join the user's prompt pack, not replace any of it:

- **Names.** `R` is the role `PACK-R` and `P` the prompt `PACK-P`
  (`molt_planner::pack::extension_name`), always with the hyphen, unlike a
  server's name: no built-in prompt or role has one, so a pack named
  `reviewer` cannot bring the prompt `reviewer`. A role's `prompt` must be
  one of the pack's own; a name the user's pack or another pack has is
  refused at install (`a-b`'s `c` and `a`'s `b-c` are both `a-b-c`) and at
  start.
- **Seats.** A pack brings no workflow, so its roles speak only where a
  workflow of the user's prompt pack seats them, checked as the user's
  own roles are.
- **Hand-off.** `molt` gives the planner the enabled packs' roles in
  `MOLT_EXTENSION_ROLES_JSON` (`pack::Extension`: the pack's name, its
  installed `prompts/`, each prompt's name with the SHA-256 that was
  approved, and the roles as written). The planner reads each file, refuses
  one that does not hash to what was approved, and adds the prompts and
  roles before it checks the pack's workflows (`Pack::load_with`). The
  CLI checks the same before anything starts. A `--config` file inside the
  workspace has the variable removed, as `MOLT_PROMPTS_DIR`.
- **Promotion.** This is the part of RFC 0003's promotion a pack needed.
  A pack's prompt is the user's in all but one thing: approving the pack
  is not promoting it. A role based on the reviewer or the judge speaks
  with a pack's prompt only while the user's `promoted.toml` holds its
  hash, which only `molt prompts promote PACK-P` writes, at a terminal,
  after showing the text; it finds a prompt of an enabled pack as it finds
  a file of the user's, and makes the prompt pack's directory when there
  is none. Without it, the built-in prompt of the slot speaks and the run
  says so, as for an unpromoted file of the user's. `molt ext add --yes`
  can approve a pack from a script; nothing but a person at a terminal
  can promote what decides which results are accepted.

The grant has a line for each prompt with its full SHA-256, and one for
each role with everything it sets; a role on a gate with a prompt of the
pack says, in its line, that the prompt is used only once promoted:

```text
prompt review-kit-strict (sha256 …), for roles to speak with
role review-kit-strict, based on the reviewer, speaks with prompt review-kit-strict; it decides what results are accepted, so its prompt is used only once you run molt prompts promote review-kit-strict
role review-kit-fast, based on the attempt, speaks with prompt review-kit-fast, offered only read_file, run of its base's tools, on model haiku, at effort low
```

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
- per prompt, its name and SHA-256; per role, its base, prompt and
  settings, and for a reviewer or judge role with a prompt of the pack
  that it needs a promotion;
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

## Registries

A registry (`extensions::registry`) is a git repository or a directory
with a `molt-registry.toml`:

```toml
schema = 1                   # required; another number is refused
[pack.NAME]                  # NAME: a pack's name
description = "…"            # optional, at most 1024 bytes
git = "https://…"            # a URL as `molt ext add URL` takes, without #
rev = "v1.2.0"               # optional, with git: branch, tag or commit id
path = "packs/NAME"          # instead of git, only in a directory's registry
```

Unknown keys are refused; an index is at most 1 MiB and 4096 packs, and
each entry's URL and reference pass `GitSource`'s checks when the index is
read. The person adds registries with `molt ext registry add NAME SOURCE`,
which reads the index once and records the source in
`~/.config/molt/registries.toml` (a file of its own, so `extensions.toml`
and a Molt older than registries are unchanged); `molt ext registry list`
and `remove` manage them, and `molt ext search [TERM]` lists their packs.

`molt ext add NAME`, for a pack's name that is not a path that exists (or
with `--registry R`), reads every registry (only `R`'s), and installs the
one entry listing `NAME` through `install` as `add_git` or `add` of what it
lists would, with ` (listed by registry R)` after the source in the
approval text and the record. A registry decides only where a pack is:

- the grant is shown and approved as for any source, and an update it
  points to is approved again for anything new;
- the staged pack must be named `NAME`, or nothing is installed;
- two registries listing `NAME`, or one that cannot be read, refuse the
  install unless `--registry` chooses, so a registry that fails to answer
  cannot let another's entry win;
- a git registry is fetched by `git::fetch`, as a pack is, each time it is
  read; a `path` is taken only from a directory's registry, and refused
  when it leads out of the directory.

A pack installed from a registry is pinned as one from its URL: `update`
follows its `rev`, and `add NAME` follows the registry's current entry.

## Claude Code plugins

`extensions::import_claude_plugin` turns a directory with
`.claude-plugin/plugin.json` into a pack in its staging copy: skills as they
are; commands with the front matter Molt reads; stdio servers of `.mcp.json`
with `${CLAUDE_PLUGIN_ROOT}` mapped, `"${X}"` for a variable `X` as
`pass_env`, and no network. HTTP servers, variables built from others and
nested commands are left out and said so. The written
manifest is shown and approved as any other.

Agents (`agents/NAME.md`) are roles: each becomes `[role.NAME]` with its
text written as `prompts/NAME.md`. `tools` is mapped to Molt's (`Read`,
`Write`, `Edit`, `MultiEdit`, `Bash`, `Grep`, `Glob`, `LS`, `WebFetch`),
the rest left out and said so; an agent offered only tools that read is
based on the explorer, any other on the attempt; `model` is kept unless it
is `inherit`. A name that is not a role's, an empty text, a subdirectory
and `plugin.json`'s own `agents` paths are left out. A Claude Code agent
is chosen by the main agent at will; in Molt it speaks where a workflow of
the user's seats it, the only way a role speaks.

Hooks (`hooks/hooks.json`, and `plugin.json`'s `hooks` as files or
inline) are imported on the events that mean the same in Molt:
`PreToolUse`, `PostToolUse` and `UserPromptSubmit`. Two things kept them
out before, and each is met without widening what a hook may do:

- **Names.** A Claude Code hook matches and reads `Edit` with `file_path`,
  not `edit_file` with `path`. `molt_planner::hooks::Dialect`, a new
  optional `dialect` of a `[[hooks]]` entry (`"molt"` by default, absent
  when serialized), makes the hook see Claude Code's names: its `matcher`
  is matched against Claude Code's name of the tool, `tool_name`,
  `tool_input` and `hook_event_name` are told in its names, and an
  `updatedInput` is read back into Molt's before the tool parses it. Tools
  with no counterpart keep Molt's names. The importer sets it on every
  hook it writes.
- **Scripts.** A hook runs in the workspace's sandbox, which does not see
  the pack, so `${CLAUDE_PLUGIN_ROOT}/hooks/x.sh` cannot run. A command
  that is only one script of the plugin, run by its `#!` line or a named
  `sh`, `bash`, `zsh`, `python3`, `python` or `node`, becomes that
  interpreter with the script's text as its program (`-c`, `-e`). The
  approved grant then holds the hook's code itself, which is the rule for
  any pack's hook. A script that names the plugin's directory or locates
  files beside itself (`$0`, `BASH_SOURCE`, `__file__`, `__dirname`,
  `import.meta`) is left out, as is a command that does more.

`type = "prompt"` hooks, matchers on MCP tools (Claude Code names a
plugin's servers `mcp__plugin_…`), events with no counterpart (`Stop`,
`SubagentStop`, `SessionStart`, `SessionEnd`, `Notification`,
`PreCompact`) and hooks beyond Molt's 32 are left out and said so.
Imported hooks are not `blocking`, as in Claude Code a failing hook other
than exit 2 is passed over; a missing `timeout` is Claude Code's 60 s.

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
user config, absent by default. Registries are kept in `registries.toml`,
which a Molt before them does not read; their index is schema 1 of its
own.

## Acceptance

- Installing shows the grant, and nothing runs before the yes
  (`crates/molt/tests/agent.rs`,
  `an_extension_pack_is_shown_approved_offered_disabled_kept_to_its_approval_and_removed`;
  `extensions::tests::installing_lists_every_power_and_records_the_approval_only_on_yes`).
- A disabled pack's tools are not offered in a run (the same test).
- A Claude Code plugin with commands and skills is imported
  (`extensions::tests::a_claude_code_plugin_with_commands_skills_and_a_server_is_imported`),
  and so are its hooks that Molt can run, their scripts in the approved
  grant, in Claude Code's names
  (`extensions::tests::a_claude_code_plugins_hooks_are_imported_with_their_scripts_and_the_rest_left_out_with_why`;
  `hooks::tests::a_claude_code_hook_sees_and_answers_in_claude_codes_tool_names_and_inputs`).
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

- A pack is found by its name in a registry and installed only as its
  listed source would be, under that name; an index or registry that is
  not one is refused before it is recorded
  (`extensions::registry::tests`,
  `a_pack_is_found_by_name_in_a_registry_and_installed_only_once_its_grant_is_approved`,
  `a_directory_registry_lists_packs_beside_its_index_and_a_listed_name_must_be_the_packs`,
  `a_registry_or_index_that_is_not_one_is_refused_before_it_is_recorded`;
  `crates/molt/tests/agent.rs`,
  `a_pack_is_found_by_name_in_a_registry_and_installed_as_its_path_would_be`).

- A pack's roles and prompts join the user's prompt pack under its name,
  replace none of it, reach the planner only with the text approved, and a
  reviewer or judge role speaks with a pack's prompt only once a person has
  promoted it; a Claude Code plugin's agents are imported as roles
  (`extensions::tests::a_packs_roles_and_prompts_are_in_its_grant_and_a_gate_role_still_needs_a_promotion`,
  `a_claude_code_plugins_agents_are_imported_as_roles_with_their_text_as_prompts`;
  `pack::tests::an_extensions_roles_join_under_its_name_and_its_protected_prompt_needs_a_promotion`,
  `an_extension_replaces_nothing_and_brings_only_the_text_that_was_approved`;
  `prompts::tests::an_extensions_prompt_is_promoted_as_the_users_own_by_a_person_typing_its_name`;
  `layers::tests::an_enabled_packs_roles_reach_the_planner_for_the_users_workflows_to_seat`;
  `crates/molt-planner/tests/planner.rs`,
  `an_extension_packs_reviewer_role_speaks_with_its_prompt_only_once_a_person_has_promoted_it`).

## Deferred

- Signing registries or packs, and caching a registry's index. Fetching git submodules and LFS files, and
  running the git fetch in a sandbox of its own.
- Environment profiles and workflows in a pack. A workflow of a pack would
  seat roles without the user writing one; with roles on a gate it also
  needs the promotion above to cover the whole declaration (RFC 0003
  defers protecting a role's model, tools and turns).
- A pack's prompt that replaces a built-in one, such as `attempt`: today
  a pack only adds. Storing a pack's prompts as registry modules for
  `molt improve`.
- A pack's service with the network, or with mounts beyond the pack's
  directory: a candidate has neither today.
- Claude Code agents chosen by the model at will (a `Task` tool), rather
  than seated by a workflow; hooks on events Molt has no counterpart of, and hook scripts that need files beside them
  (that would take a hook sandbox that sees the pack).
- Comparing a server's `calls` numerically, so that fewer calls is not a
  new line.
- Evaluating a pack on the benchmark before enabling it.
