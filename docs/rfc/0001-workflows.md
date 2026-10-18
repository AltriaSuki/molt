# RFC 0001: Workflows

- Issue: #156 (roadmap #153, phase 1, C2)
- Related: #137 (design budget share), #47, #75, #157 (light workflow)
- Status: phase 1 implemented

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
- Version 1 has one parameter: `design.budget_share`, the share of the run's
  budget designing may spend (more than 0, at most 1). Without it the
  planner's own share applies (`MOLT_DESIGN_BUDGET_SHARE`, #137). A request's
  `design_budget_usd` still wins over both.

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
| `rules` | Only the review's fixed rules looked at the diff. Named now so readers know it; no version 1 workflow produces it. |
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

## Trust rule

A request names a workflow; it never defines one. Workflows are defined only
where executable configuration already comes from:

1. the planner's built-ins (`verify`);
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
- Loading workflows from the user's trusted configuration file, and a
  `molt do --workflow NAME` switch.
- Lightweight workflows such as #157's, built on this schema.
- Moving request fields into stage parameters, in a later schema version.
- A workflow that produces `rules`: the review's fixed rules over a result
  that has no check.
- The learn stage for workflows the caller does not know: it runs on the
  caller's side, which in phase 1 looks up only the built-ins.
