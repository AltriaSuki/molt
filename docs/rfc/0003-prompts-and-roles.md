# RFC 0003: Prompts and roles

- Issue: #164 (roadmap #153, phase 2, C3)
- Related: #35 (improver), #10, #148 (role labels), #156 (workflows, RFC 0001), #163 (tools, RFC 0002)
- Status: implemented; unstable until phase 3, so the pack's layout may still change

## Summary

The system prompts the planner speaks with are constants compiled into it,
and who speaks (the designer, the attempts, the reviewer, ...) is fixed by
the code. A user cannot tune a prompt for a weaker model, the improver
(#35) has nothing it can version, and a workflow cannot say "this attempt,
but terser and on a cheaper model".

This RFC makes the prompts a **prompt pack** and the speakers **roles**.
The pack is the built-in prompts plus what a directory of the user's adds:
a file per prompt it replaces or adds, and declarations of roles and of
the workflows that use them. Every prompt version is named by the SHA-256
of its text, and every model call records the role it speaks as and that
version in the audit log. The prompts of the roles that guard the gate a
result passes (the reviewer, the judge and the vetter) take effect only
once a person has promoted them. Without a pack, the model gets byte for
byte what it got before.

## The built-in prompts and roles

The issue names five built-in roles. Each is a slot a workflow can seat a
role in, mapped to a part of the pipeline that exists today:

| Role (slot) | Built-in prompt | Where it speaks | Protected |
| --- | --- | --- | --- |
| `designer` | `designer` | the `design` stage: designs the done-check | no |
| `attempt` | `attempt`, followed by the paragraph about the done-check or the criteria | the `attempts` stage of `verify`, `quick` and any workflow that changes files | no |
| `explorer` | `explorer` | the one attempt of a workflow whose `attempts.read_only` is set, such as `ask`: it reads and answers | no |
| `reviewer` | `reviewer` | the `review` stage's model, when the review is not `rules_only` | yes |
| `judge` | `judge` | judging a result against the run's rubric | yes |

Two more prompts are not slots, since nothing should replace their role
with another: `vetter` (it looks over a designed check before the attempts
start; protected, as it guards the check) and `router` (it picks a workflow
for `auto`).

`explorer` is the issue's name for what the code calls the read-only `ask`
attempt; it is the only role that did not have a name of its own before.
An attempt's system prompt was one of three constants that share their
first part; the pack's `attempt` prompt is that first part, and the planner
adds the paragraph that fits the run (the done-check, with
`dispute_check`; the rubric; nothing for an unverified run), so that a user
who rewrites `attempt` cannot drop the rules about the check by accident.
The nudges and the user turns the planner adds (`You have not called
submit_review ...`, a failed check's output) stay compiled in.

## The pack

The pack is a directory: `~/.config/molt/prompts/`, or under
`$XDG_CONFIG_HOME` when that is set, as for the user config; or the
directory `MOLT_PROMPTS_DIR` names (`none` for the built-ins only).

| File | Holds |
| --- | --- |
| `NAME.md` | The prompt `NAME`: one of the built-in names above to replace it, any other name (lowercase letters, digits, `-`, `_`) to add one. At most 64 KiB. The line breaks that end the file are dropped, so `molt prompts show NAME > NAME.md` gives the same text and the same hash. |
| `pack.toml` | `[[role]]` and `[[workflow]]` declarations, below. |
| `promoted.toml` | Written by `molt prompts promote`: `[NAME] sha256 = "…", promoted_ms = …`. |

A prompt's **version** is the lowercase hex SHA-256 of its text as used
(`molt_planner::pack::sha256`), whether it is built in or the user's. The
planner reads the pack when it starts and refuses to start on one that does
not read: a prompt that is empty or misnamed, a role that names a prompt
there is not, a workflow that seats a role of the wrong kind. The CLI reads
it first too, so `molt do` stops with the reason before anything runs.

## Roles

A role is a declaration in `pack.toml` (the type is
`molt_api::role::RoleDecl`):

```toml
[[role]]
name = "terse"
base = "attempt"
prompt = "terse"                  # prompts/terse.md
tools = ["read_file", "search", "edit_file", "run"]
model = "sonnet"
effort = "low"
max_turns = 20
```

| Field | Meaning | Default |
| --- | --- | --- |
| `name` | 1 to 64 lowercase letters, digits, `-`, `_`; not a built-in role's name. | required |
| `base` | The built-in role it is a kind of: `designer`, `attempt`, `explorer`, `reviewer` or `judge`. It fills that slot and gets that role's tools, first message and rules. | required |
| `prompt` | The prompt it speaks with, by name: a file of the pack or a built-in prompt. | its base's |
| `tools` | The tools it may be offered, by name, out of those its base would be. Its base's own `submit_*` and `dispute_check` always stay. | all of its base's |
| `model` | The model for its calls. | the run's |
| `effort` | `low` … `max`. | the run's |
| `max_turns` | The most model turns one of its conversations takes, 1 to 1000. | its base's |
| `budget_share` | The share of the run's budget it may spend, more than 0, at most 1. | its base's |

A role's settings win over the run's for that role: a run with `--effort
high` through a workflow that seats `terse` sends `effort: low` on terse's
calls. The one exception is a request's `judge_model`, which still wins
over a judge role's `model`, as it wins over the run's model today. A
call to a tool the role is not offered is refused without running, with
the message a call to a tool that does not exist gets.

`budget_share` is read for designer roles only in this version, where it
replaces the workflow's `design.budget_share` and the planner's share (a
request's `design_budget_usd` still wins). For the other roles a share has
no single meaning yet (parallel attempts share the run's budget), so a
pack that gives one is refused rather than ignored; see Deferred.

## Workflows that seat roles

A workflow gets a `roles` table, slot to role name (RFC 0001's schema 1,
with an optional field, so every document written before it reads the
same). The pack's `pack.toml` can define workflows, which runs name like
any other (`molt do --workflow careful`):

```toml
[[workflow]]
schema = 1
name = "careful"
roles = { attempt = "terse", reviewer = "strict-reviewer" }
[workflow.stages]
context = {}
design = {}
attempts = {}
review = {}
deliver = {}
```

A slot a workflow does not name keeps its built-in role. A workflow is
refused when it seats a role in a slot none of its stages uses (a
`designer` without `design`, an `attempt` in a read-only workflow, an
`explorer` in one that is not, a `reviewer` with no review model), when
the role is not declared, or when the role's `base` is not that slot. A
pack's workflow cannot take a built-in's name. The planner's own
configuration (`Config::workflows`) can seat roles too, and is checked the
same way when a run names it.

Workflows live in the pack, next to the roles they seat, rather than in
`config.toml`: RFC 0001 deferred loading workflows from the user's
configuration, and the pack is already the place that is the user's alone.

## Where the versions are recorded

- **The audit log.** Every `model.complete` request the planner makes has a
  `prompt` field, `{ "role": "terse", "prompt": { "name": "terse",
  "sha256": "…", "source": "user" } }` (`source` is `builtin` or `user`).
  The kernel writes each request to the log before delivering it, so the
  log has the role and the version of every call, and the system prompt
  itself. The gateway does not send the field to the model API.
- **The reply.** `RunResponse.prompts` lists each role that spoke and its
  prompt's version, in the order they first spoke, built-in or not.
- **Progress.** At the start of a run, a `note` event for each role of the
  workflow's stages that speaks with a prompt of the pack, by name and the
  first 12 digits of its hash (`attempt: speaks with your prompt attempt
  (3f2a…)`), and for each pack prompt left unused for want of a promotion.
- **Commits.** `molt do --commit` and the interface's commit add a
  `Molt-Prompt: role=NAME prompt=NAME sha256=HEX` trailer for each role
  that spoke with a prompt of the user's pack. The built-in prompts get no
  trailer: they are the planner's release's, and a commit without
  `Molt-Prompt` used only built-in prompts.

## Protected prompts and promotion

The reviewer and the judge decide whether a result is accepted, and the
vetter whether the check is fair; CONTRIBUTING's rule is that an agent's
change must never be able to edit its own acceptance check. So a prompt of
the pack that would speak as one of them takes effect only while
`promoted.toml` holds its current hash:

- the pack's `reviewer.md`, `judge.md` or `vetter.md`;
- any prompt a role based on `reviewer` or `judge` speaks with, a built-in
  one other than its base's own included.

Otherwise the built-in prompt of that slot is used, the run says so in a
progress note, and `RunResponse.prompts` shows the built-in version. An
edit after a promotion changes the hash, so the built-in is back until
the new text is promoted.

`molt prompts promote NAME` is the promotion: it shows the file's text, its
size and hash and what it replaces, asks the person to type the name back,
and records the hash with the time in `promoted.toml`. It refuses to run
without a terminal on stdin and stderr, so a script or an agent's `run`
tool cannot use it. An agent cannot reach the pack in the first place: the
sandbox its commands run in mounts the system read-only and the workspace,
not the user's home, and `HOME` there is a directory of its own. `molt
prompts list` shows each prompt's version and whether a protected one is
promoted; `molt prompts show NAME [--builtin]` prints a text.

Only the prompt is protected. A reviewer role's `model`, `tools` and
`max_turns` are the user's ordinary settings, as `--model` and
`judge_model` are today; protecting the whole declaration is deferred.

## Trust rules

- The pack comes only from the user: the planner reads the directory its
  starter names, and `molt` names it only from the user layer of the
  configuration. A project's `.molt/config.toml` that sets `prompts`,
  `role` or `roles` is refused with why, as one with `service` or `hooks`
  is; a project's own `.molt/prompts/` is never read. A `--config` file
  inside the workspace has a `MOLT_PROMPTS_DIR` in the planner's
  environment removed, as its hooks are.
- A pack inside the workspace is refused (`molt` stops with why), since the
  agent's changes are merged there: with the home directory as the
  workspace, a result could otherwise carry its own `promoted.toml`.
- A project's `AGENTS.md` (and `CLAUDE.md`, or the files
  `instruction_files` names) can only add to what the model is told: it is
  given in the first user message, in a `<project_instructions>` block,
  as data, and never reaches the system prompt, which is the same with or
  without it.
- A request never carries a prompt, a role or a workflow definition; it
  names a workflow.

## Compatibility

- `CompleteRequest.prompt`, `RunResponse.prompts` and `Workflow.roles` are
  optional (`#[serde(default)]`) and left out when empty, so older callers
  and readers keep working; a reply from an older planner has no
  `prompts`, which says nothing about what it used.
- Without a pack every system prompt is byte for byte what it was:
  `prompts::tests::the_built_in_system_prompts_the_model_gets_are_byte_for_byte_what_they_were`
  compares each one (the three attempt variants, the explorer, the
  designer, the vetter, the reviewer, the judge and the router) with a copy
  taken before the change (`crates/molt-planner/src/testdata/system_prompts.txt`).
  The tool lists stay pinned by RFC 0002's test.

## Choices where the issue left room

- **Names.** The prompt names are the role names, plus `vetter` and
  `router`; the attempt's three prompts become one overridable part plus
  the paragraph the run needs.
- **The vetter is protected too.** The issue names the reviewer and the
  judge; the vetter decides whether a designed check is fair to the task,
  so a lenient vetter weakens the same gate.
- **Promotion is a file of hashes**, written by a command that needs a
  terminal, rather than a kernel registry entry: it works before any
  kernel is running, it is readable, and an edit undoes it by itself.
- **An unpromoted protected prompt falls back** to the built-in, with a
  note, rather than failing the run: the user's run still has a reviewer,
  just not the new one.
- **Roles and workflows live in the pack's `pack.toml`**, not in
  `config.toml`, so that everything that changes what the model is told is
  in one directory of the user's.

## Acceptance

- Default prompts are byte for byte the same: the test above, and the
  whole planner suite.
- An override changes the system prompt and its hash is in the audit log:
  `crates/molt-planner/tests/planner.rs`
  (`an_override_in_the_pack_changes_the_system_prompt_and_every_call_names_its_version`),
  `crates/molt/tests/agent.rs`
  (`an_override_in_the_prompt_pack_reaches_the_model_and_its_hash_the_audit_log`,
  with the real kernel, reading the log back), `git::tests::a_run_commit_names_each_prompt_of_the_users_pack_by_its_hash`.
- A workflow can seat a custom role:
  `a_workflow_of_the_pack_seats_a_custom_role_with_its_prompt_model_effort_turns_and_tools`,
  `a_workflow_that_seats_a_role_no_pack_declares_is_refused`,
  `pack::tests`, `workflow::tests::a_workflow_seats_roles_only_in_slots_its_stages_use`.
- A protected prompt needs a person's promotion:
  `a_reviewer_override_reaches_the_model_only_once_a_person_has_promoted_it`,
  `pack::tests::a_protected_override_takes_effect_only_while_its_promotion_matches`,
  `prompts::tests::a_promotion_records_the_hash_a_person_typed_the_name_for`.
- `AGENTS.md` only adds, and a project cannot give prompts or roles:
  `the_projects_instruction_files_add_to_the_system_prompt_and_never_replace_it`,
  `layers::tests::the_planner_reads_the_users_prompt_pack_and_a_project_cannot_give_one`,
  `agent::tests::only_configuration_outside_the_project_can_name_a_prompt_pack`,
  `prompts::tests::the_pack_is_under_the_user_config_and_never_inside_the_workspace`.

## Deferred

- Storing each prompt version in the kernel's registry as a data module,
  content-addressed like manifests, so the improver (#35) can propose a
  version and a person promote it through the kernel's own promotion path.
  The hash recorded today is the key such an entry would have.
- A `budget_share` for roles other than the designer.
- Protecting a reviewer or judge role's whole declaration (its model,
  tools and turns), not only its prompt.
- The nudges and other user turns the planner adds, as prompts of the pack.
- Reading a changed pack while the planner runs; today it is read when the
  planner starts (each `molt do`, each start of the interface).
- Showing the role and prompt version on every progress event, not only in
  the notes at a run's start (#148).
- `molt prompts diff NAME` and an `edit` that opens the built-in text.
