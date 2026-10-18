# RFC 0001: Workflows

- Issue: #156 (roadmap #153, phase 1, C2)
- Related: #137 (design budget share), #47, #75, #157 (light workflow)
- Status: phase 1 implemented; the light workflows of #157 implemented

## Summary

Every task runs through one fixed pipeline today: design a check, race
attempts against it, compare, review, apply, learn. `RunRequest` carries more
than twenty switches that turn parts of it on and off, and the planner's
`validate` rules out the combinations that make no sense one by one. Adding a
step means changing the runner.

This RFC names the pipeline's stages and makes the set of stages a run goes
through data: a **workflow**. A workflow says which stages are enabled and
with what parameters. Today's pipeline becomes the built-in workflow
`verify`, and a run that names no workflow goes through it exactly as
before. Every result also says what it was verified by: its **verification
level**.

## Stages

A workflow's stages always run in this order. Each one is either enabled,
with its parameters, or left out.

| Stage | What it does | Inputs | Outputs |
| --- | --- | --- | --- |
| `context` | Gathers what the model gets besides the task. | Workspace, `files`, the planner's instruction file names, the memory service. | Memory's notes and project map, the `<project_instructions>` block, the `<referenced_files>` block. Left out: the model gets the task, the earlier tasks and its tools only. |
| `design` | Designs a done-check, or a rubric when no command fits, when the request gives neither; then vets it and runs it once on the workspace. | Task, context, `model`, `effort`, the design budget. | A check (command, files, suite) or a rubric, or the finding that nothing automated fits. Left out: a check or rubric the request gives is still used; without one the run is unverified. |
| `attempts` | Runs `attempts` agent loops in their own forks, each checked by the check or judged by the rubric until it passes or runs out of turns, check rounds or budget. | Task, context, check or rubric, the limits. | One report per attempt; the passing attempts, in the order they passed. Required. |
| `compare` | Ranks attempts that pass on signals that can be audited (rubric criteria met, the project's suite, diff size), when `select` lets more than one pass. | The passing attempts' forks and diffs, the suite. | `selection`, with the candidates best first. Left out: `select` must be `first`. |
| `review` | Fixed rules over the diff, then an independent reviewer model, for a winner that passed a check. A result the review does not approve is kept, not applied. | Task, check, requirements, the winner's diff. | `review`. Left out: no review, whatever `review` in the request says. |
| `deliver` | Returns the winner's changes and patch and, with `apply`, merges them into the workspace; otherwise keeps the fork. | The winner's fork and diff, the review, `apply`. | `changes`, `patch`, `applied`, `fork`. Required. |
| `learn` | Has memory learn from the finished run (`memory.consolidate`). Runs on the caller's side, after the planner has answered. | The run's trace and workspace, the learning model. | What memory learned. Left out: the caller does not ask memory to learn. |

Phase 1 requires `attempts` and `deliver`: without them a run has no result,
or no one sees it.

## Schema, version 1

A workflow is a document of this shape (shown as JSON; the types are
`molt_api::workflow::Workflow` and friends):

```json
{
  "schema": 1,
  "name": "verify",
  "description": "Design or take a check, race attempts against it, compare, review, apply and learn.",
  "stages": {
    "context": {},
    "design": { "budget_share": 0.25 },
    "attempts": {},
    "compare": {},
    "review": {},
    "deliver": {},
    "learn": {}
  }
}
```

- `schema` is the version the document is written to. A planner refuses a
  workflow written to a version it does not read, rather than guess.
- A stage present in `stages` is enabled; an absent one is left out. An
  empty object enables a stage with its defaults.
- Unknown stages and unknown parameters are refused, so a typo cannot
  silently turn a stage off.
- `design.budget_share` is the share of the run's budget designing may
  spend (more than 0, at most 1). Without it the planner's own share applies
  (`MOLT_DESIGN_BUDGET_SHARE`, #137). A request's `design_budget_usd` still
  wins over both.
- `attempts.count` (1 to 8) fixes how many attempts run, whatever the
  request's `attempts`. Without a check or a rubric one runs anyway.
- `attempts.read_only: true` has the attempt work in the workspace itself,
  without a fork, offered only the tools that read (`read_file`,
  `list_files`, `search`, `find_symbol`, `recall`, `fetch_url`); a call to
  any other is refused without running. Its result changes nothing and is
  never applied or kept. Such a workflow has no `design`, `compare` or
  `review` stage, and a request with a check, a rubric or `checkpoint` is
  refused.
- `review.rules_only: true` has only the review's fixed rules look at the
  diff, with or without a check; no reviewer model is asked. A diff they
  find nothing in is approved, one they question is kept, not applied.

The last three were added for #157. They are optional, so every version 1
document written before them still reads the same; a planner that does not
know them refuses a document that uses them, as it refuses any unknown
parameter.

Stage parameters that today live on the request (`attempts`, `select`,
`max_turns`, ...) stay there in version 1; a later version can move a
parameter into the workflow when a workflow needs to fix it.

## Verification levels

Every `planner.run` reply carries `verification`, the level of evidence
behind its result, or behind the result it would have had if an attempt had
passed:

| Level | Meaning |
| --- | --- |
| `command` | A done-check command, run by the planner in the result's fork, after restoring the check's files. |
| `judged` | A rubric: each criterion decided by its own command or by an independent model judge. Weaker than `command`. |
| `rules` | Only the review's fixed rules looked at the diff: a result without a check or a rubric in a workflow whose review is `rules_only` (`quick`). A result that changed nothing had nothing to look at, and is `none`. |
| `none` | Nothing checked the result. |

The level is shown

- in the reply, as `RunResponse.verification`;
- in `molt do`'s report, as a `verification:` line under the outcome;
- in `molt do --json`, as the `verification` field;
- in the commit `molt do --commit` makes, as a `Molt-Verification:` trailer
  right after the `Molt-Run:` trailer.

The `Molt-Run:` trailer's value stays the bare run id, which `molt undo` and
`molt memory used` look commits up by; the level gets a trailer of its own
next to it.

A reply from an older planner has no `verification`; a reader treats that as
unknown, never as `none`.

## The built-in `verify` and the old request fields

`verify` enables every stage with the planner's defaults. A request without
`workflow`, or with `workflow: "verify"`, behaves exactly as it did before
workflows existed: the request's fields configure the stages as they always
have.

| `RunRequest` field | Stage it configures in `verify` |
| --- | --- |
| `files` | `context`: the referenced files. |
| `earlier` | Run-wide: given to the model whatever the stages. |
| `check`, `check_files`, `suite` | `design`: a given check skips designing. `suite` is used by `compare`. |
| `rubric`, `judge_model` | `design`: a given rubric skips designing; `attempts`: the judge. |
| `verify: false` | `design`: no check is designed, one attempt runs, level `none`. |
| `design_budget_usd` | `design`: the most designing may spend, over `design.budget_share`. |
| `attempts`, `max_turns`, `max_check_rounds`, `syntax_check`, `model`, `effort` | `attempts`. |
| `select` | `compare`. |
| `review` | `review`: false turns it off for this run. |
| `apply` | `deliver`. |
| `budget_usd`, `stream`, `checkpoint`, `resume` | Run-wide, not a stage. |

A field configures a stage only when the workflow enables it: with `review`
left out, `review: true` does nothing; with `compare` left out, a `select`
other than `first` is refused; with `design` left out, `planner.design` is
refused and a run without a check runs unverified.

A checkpoint saves the request with its `workflow`, so a resumed run goes on
through the same workflow.

## The light built-ins: `ask` and `quick` (#157)

```json
{ "schema": 1, "name": "ask",
  "stages": { "context": {}, "attempts": { "count": 1, "read_only": true }, "deliver": {} } }
{ "schema": 1, "name": "quick",
  "stages": { "context": {}, "attempts": { "count": 1 }, "review": { "rules_only": true }, "deliver": {} } }
```

- `ask` answers a question. No fork is made, no check is designed and
  `planner.design` is never called; the reply's outcome is `unverified`, its
  level `none`, with no changes. Front ends call it an unverified answer.
- `quick` makes a small change: one attempt, no designed check (a check the
  request gives is run, and the level is `command`), the review's fixed rules
  only (level `rules` without a check), then apply.
- Neither learns: a run that should leave notes is a `verify` run.

### `auto`

`auto` is not a workflow but a name a request may give in its place. The
planner then picks one of `ask`, `quick` and `verify` with one model call on
the task alone (one tool, `submit_route`; the model is the planner's
`route_model`, `MOLT_ROUTE_MODEL`, or the run's), counted in the run's cost
like any other call. Only workflows the request validates under are offered,
so with a check or a rubric `ask` is never one, and with a `select` other
than `first` only `verify` is. The pick is published as a `routed` progress
event before anything else runs, and the reply's `workflow` names it. A pick
that fails, does not come, or names a workflow that was not offered is the
heaviest offered: routing never makes a run lighter by failing. A request
that names a workflow is never routed, so naming `verify` is how a person
overrides a pick. A checkpoint saves the workflow picked, not `auto`.

`planner.design` takes `auto` too, for a front end that shows the check
before the attempts: it picks first and, for a workflow that designs
nothing, answers at once with `workflow` set and no check, for the caller to
run the task with that workflow.

### Going on from a task

`RunRequest.continues` carries the previous task's conversation with the
model, as its reply's `conversation` gave it (the winner's, fitted to a
quarter of a bus frame), and the fork its result was kept in when it was not
applied. The attempt's conversation starts with it, then the new task; the
usual fitting leaves out its oldest turns when it no longer fits the model's
input. With a kept fork, the one attempt works in that fork, which the run
never drops: a run that fails leaves it to the user, and one that applies
merges both tasks' changes. Without one, a new fork of the workspace, which
holds the earlier result when it was applied.

## Trust rule

A request names a workflow; it never defines one. Workflows are defined only
where executable configuration already comes from:

1. the planner's built-ins (`verify`, `ask`, `quick`);
2. the planner's own configuration (`molt_planner::Config::workflows`), set by
   whoever starts the planner.

A project directory can at most name one of those (in a future project
setting or command template), so a cloned repository cannot turn off the
check, the review or the comparison that protect the user, or invent stages.
A name that matches no workflow is refused with the names the planner has.
A configured workflow cannot take a built-in's name: the built-in wins.

## Phase 2 and later

Deferred, deliberately:

- **Custom stages backed by a service method.** A stage of a workflow could
  name a bus method (with its capability and budget) that the planner calls
  at that point. This needs the extension principles of #153 phase 2 and
  comes then.
- Loading workflows from the user's trusted configuration file.
- Moving more request fields into stage parameters (`attempts.count` is the
  first), in a later schema version.
- The learn stage for workflows the caller does not know: it runs on the
  caller's side, which in phase 1 looks up only the built-ins.
