# Prompts and roles

Everything the model is told first in a conversation, its system prompt,
comes from a **prompt pack**: the prompts built into molt, with whatever
your pack directory changes or adds. The pack can also declare **roles**
(a built-in role with its own prompt, tools, model, effort or turns) and
**workflows** that use them. The design is RFC 0003,
[rfc/0003-prompts-and-roles.md](rfc/0003-prompts-and-roles.md); the layout
is unstable until phase 3 of the roadmap.

Without a pack nothing changes: the model gets the same prompts, byte for
byte, as before packs existed.

## The prompts

| Name | Who speaks with it | Protected |
| --- | --- | --- |
| `attempt` | the attempts that carry out a task (`verify`, `quick`) | no |
| `explorer` | the one attempt of `ask`, which only reads, and the sub-agent of [`explore`](explore.md) | no |
| `designer` | the check designer | no |
| `vetter` | the vetter, which looks over a designed check | yes |
| `reviewer` | the reviewer of a passing result | yes |
| `judge` | the judge of a result against a rubric | yes |
| `router` | the call that picks a workflow for `auto` | no |
| `compactor` | the call that writes an attempt's notes on the turns its conversation leaves out, with `MOLT_COMPACT=model` ([do.md](do.md#long-tasks)) | no |

`molt prompts list` prints each with its version (the first digits of the
SHA-256 of its text) and whether it is built in or yours;
`molt prompts show NAME` prints its text, `--builtin` the built-in one.

The `attempt` prompt is the part every attempt shares. After it, molt adds
the paragraph that fits the run: what the done-check is and how to dispute
it, or how a rubric is judged. You cannot drop those rules by rewriting
`attempt`.

## Changing a prompt

The pack is the directory `~/.config/molt/prompts/`
(`$XDG_CONFIG_HOME/molt/prompts/` when that is set), or the one
`MOLT_PROMPTS_DIR` names; `MOLT_PROMPTS_DIR=none` runs with the built-ins.
To change a prompt, start from the built-in one:

```sh
mkdir -p ~/.config/molt/prompts
molt prompts show attempt > ~/.config/molt/prompts/attempt.md
$EDITOR ~/.config/molt/prompts/attempt.md
```

A file `NAME.md` with a built-in name replaces that prompt; any other name
(lowercase letters, digits, `-`, `_`) adds a prompt a role can use. The line
breaks that end a file are dropped. The next run uses it: `molt do` reads
the pack each time it starts, the interface when it starts.

A run says which roles speak with a prompt of yours, as a progress line
such as `attempt: speaks with your prompt attempt (3f2a9c01d4be)`. Every
model call in the audit log names its role and the full hash of its prompt
(`"prompt": { "role": "attempt", "prompt": { "name": "attempt", "sha256":
"…", "source": "user" } }`), `molt do --json` has the list as `prompts`,
and a commit `--commit` makes has a `Molt-Prompt: role=… prompt=…
sha256=…` trailer for each prompt of yours the run used.

## Protected prompts: promote them

The reviewer, the judge and the vetter decide whether a result is
accepted. A prompt of yours that would speak as one of them (their own
`reviewer.md`, `judge.md` or `vetter.md`, or the prompt of a role based on
the reviewer or the judge) is used only after you promote it:

```sh
molt prompts promote reviewer
```

It shows the text and its hash, asks you to type the name back, and records
the hash in `~/.config/molt/prompts/promoted.toml`. It needs a terminal, so
a script cannot run it, and an agent's commands run in a sandbox that does
not see your home directory. Until a prompt is promoted, and again after
every change to it, runs use the built-in prompt and say so:
`reviewer: the prompt reviewer (…) is not promoted, so the built-in reviewer
prompt is used`. `molt prompts list` shows which are promoted.

## Roles and workflows

`~/.config/molt/prompts/pack.toml` declares roles and the workflows that
seat them:

```toml
[[role]]
name = "terse"
base = "attempt"          # designer, attempt, explorer, reviewer or judge
prompt = "terse"          # terse.md in the pack; default: the base's prompt
tools = ["read_file", "search", "edit_file", "run"]
model = "sonnet"
effort = "low"
max_turns = 20

[[workflow]]
schema = 1
name = "careful"
roles = { attempt = "terse" }
[workflow.stages]
context = {}
design = {}
attempts = {}
review = {}
deliver = {}
```

```sh
molt do --workflow careful "make the parser accept trailing commas"
```

A role fills the slot of its `base` in a workflow that names it, with the
base's tools, messages and rules and these settings over the run's:

- `prompt`: a prompt of the pack, or a built-in one, by name;
- `tools`: the tools it may be offered, out of its base's; the tools that
  end its work (`submit_check`, `submit_review`, `dispute_check`, ...)
  always stay, and a call to any other is refused without running;
- `model`, `effort`, `max_turns`: for its calls only (a run's
  `judge_model` still wins for the judge);
- `budget_share`: for a role based on `designer`, the share of the run's
  budget designing may spend; for one based on `explorer`, the share of
  what an attempt has left that one `explore` call may spend
  ([explore.md](explore.md)).

A workflow's `roles` maps slots to roles. A slot it leaves out keeps the
built-in role; a role in a slot none of its stages uses, an undeclared
role, or one whose `base` is not the slot is refused. The workflow keys
are those of RFC 0001 ([rfc/0001-workflows.md](rfc/0001-workflows.md)); a
pack's workflow cannot take the name of `verify`, `ask`, `quick` or
`auto`. Unknown keys are refused, and a pack that does not read stops the
run before it starts, with the reason.

## Roles from extension packs

An [extension pack](extensions.md#roles-and-prompts) can bring roles and
prompts. They join your pack under the pack's name: its role `triage`
is `PACK-triage` here, and its prompt `strict` is `PACK-strict`, so they
never replace a prompt or role of yours or a built-in one. A workflow of
yours seats them by that name:

```toml
[[workflow]]
schema = 1
name = "tracked"
roles = { attempt = "issue-tracker-fast", reviewer = "issue-tracker-strict" }
[workflow.stages]
context = {}
attempts = {}
review = {}
deliver = {}
```

A pack's prompt is used with the text you approved when you installed
it. One that a role based on the reviewer or the judge speaks with is
protected as yours is: installing the pack does not promote it, so the
built-in prompt is used until you run `molt prompts promote
issue-tracker-strict`, which shows the pack's text and records its hash in
your `promoted.toml` (and makes `~/.config/molt/prompts/` if you have
none). `molt prompts list` shows the pack's prompts and roles with the
pack's name.

## What a project can and cannot do

Prompts, roles and workflows come only from your pack. A project's
`.molt/config.toml` that sets `prompts`, `role` or `roles` is refused, and
a `.molt/prompts/` directory in a project is never read. A pack inside the
workspace is refused too, since a run's changes are merged there. A
`--config` file inside the workspace cannot point the planner at a pack.

A project's `AGENTS.md` and `CLAUDE.md` (see
[config.md](config.md#configuration-layers)) add to the task: they reach the
model in the first message of each conversation, marked as the project's
instructions, and never replace or change the system prompt.
