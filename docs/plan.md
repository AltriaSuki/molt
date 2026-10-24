# Plans: a task in checked steps

A task too large to do and check in one go can run as a plan: a few ordered
steps, one goal sentence each, where each step is a run of its own with its
own done-check, and the next step starts from the last one's result. Once
every step has passed, an overall check runs on the whole result, which is
then applied, or kept, as a run's result is.

```
molt do TASK --plan [--check CMD] [--budget-usd X] ...
molt do TASK --step GOAL [--step GOAL]... [--check CMD] ...
```

In the interface: `/plan TASK`.

## How a plan goes

1. **Steps.** Without `--step`, the planner's step designer (a model seated
   as the internal `steps` role, with its own system prompt) reads the task
   and the project's map and notes, and submits two to eight steps with
   `submit_plan`. Each is one goal sentence that leaves the project in a
   state that can be checked. With `--step` the steps are yours, in order,
   and nothing designs them.
2. **The overall check.** `--check` is the plan's overall check. Without one
   the planner designs a check for the whole task, as `molt do` would, before
   the first step starts; with `--no-check` there is none, and the steps
   run without checks too. The plan's overall check is always a command:
   `--criterion` is refused.
3. **Each step.** The plan makes one fork of the workspace for its result.
   Each step is a `planner.run` under the plan's trace whose task is the
   whole task with the plan around it ("Do step 2 now: ..."), so the model
   knows what is done and what is left for later steps. It designs its own
   check, races its attempts in forks made from the plan's result, has its
   winner reviewed, and merges it into the plan's result. The next step's
   attempts fork from there.
4. **The end.** Once every step has passed, the overall check runs on the
   plan's result, with its files written into a throwaway fork of it. When
   it passes and the run applies, the result is merged into the workspace,
   through the same `apply.before` hooks as a run's; otherwise it is kept,
   and the report says where.

A step that does not pass stops the plan. The steps after it never start,
the overall check does not run, and the result of the steps that passed is
kept in the plan's fork. It is an ordinary kept result made from the
workspace: `molt apply FORK` (or the interface's `/kept`) applies it, and
dropping it throws it away. The reply's `remaining` lists the steps from the
one that did not pass on; asking `planner.plan` for those steps with the kept
result as `continues.fork` goes on from there, designing nothing again.

## Confirming the steps

In the interface's confirm and plan modes, `/plan TASK` first asks only for
the steps (`propose` in the request: nothing runs, no fork is made) and shows
them as a card, as it shows a designed check:

| Key | Does |
| --- | --- |
| `enter` | run the steps |
| `e` | edit them in the input box, one step per line (`shift+enter` or `ctrl+j` for a new line, `enter` to save, `esc` back) |
| `esc` | cancel |

What proposing cost comes out of the task's budget. In auto mode, and from
`molt do --plan`, the steps run as the step designer proposed them, without
stopping.

## The budget

One budget, `--budget-usd` (or `/budget`), covers the whole plan: the step
designer, the overall check's designer and every step. Each step's run gets
what the plan has left when it starts, so a step that spends a lot leaves
less for the ones after it. A step that runs out of its share stops as a run
that runs out does, and so stops the plan. Unknown is never zero: when a
model call of the plan has an unknown cost, what is left is not known either,
and no further step starts.

## On the record

The plan is one episode in the audit log, under its trace:

- the `planner.plan` request, with its steps, and with `proposed` beside
  them when a person edited the step designer's proposal;
- a `planned` progress event with the steps the plan goes by (`edited` when
  they are a person's edit), and then a note saying how many steps were
  proposed;
- a `step` event as each step starts, naming the plan's fork;
- every step's model calls, check design, check runs, review and merge, as a
  run's;
- the reply: the steps, each step's run (its check, attempts, changes and
  cost) in `ran`, `remaining`, the plan's result and the totals.

`molt do --json` prints the plan's result as a run's reply, with the plan
under `plan`.

## Limits

- A plan keeps no checkpoint: `--checkpoint` is refused. A plan stopped part
  way (interrupted, killed) is not resumed from the step it was in; the
  result of the steps that passed is kept, and the remaining steps can run
  in it.
- `molt do` and the interface have no way yet to go on from a stopped plan's
  kept result; the API does (`continues.fork`).
- The steps run one after another, never in parallel, and the TUI confirms
  the steps but not each step's designed check.
- The overall check's own files are written only to check the result; they
  are not part of what is applied.
- A workflow that only reads (`ask`) is refused. `auto` picks a workflow for
  each step.
