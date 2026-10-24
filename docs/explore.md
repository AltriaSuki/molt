# Explore: a sub-agent that only reads

In a large project an attempt spends many turns reading and searching
before it knows what to change, and everything it reads stays in its
conversation: every later turn pays for it again and the context fills up.
With `explore` the attempt hands such a question to a sub-agent instead.
The sub-agent reads the attempt's copy of the workspace in a conversation
of its own and returns one conclusion, with `path:line` references; only
that conclusion enters the attempt's conversation.

It is off by default. The design follows issue #42 and RFC 0002's
sub-agents as tools ([rfc/0002-tools.md](rfc/0002-tools.md)); like the
rest of phase 2 it may still change.

## Turning it on

Either for every run, in the planner's environment:

```sh
MOLT_EXPLORE=on MOLT_EXPLORE_MODEL=haiku molt do "rename Config::load to Config::read everywhere"
```

or for one workflow of your prompt pack ([prompts.md](prompts.md)), with
`explore = true` in its attempts stage:

```toml
[[workflow]]
schema = 1
name = "scouting"
[workflow.stages]
context = {}
attempts = { explore = true }
review = {}
deliver = {}
```

`MOLT_EXPLORE=on` turns it on in every workflow whose attempts change
files, the built-in `verify` and `quick` included. An `ask` run already
only reads, so it never explores; a workflow that sets both
`attempts.read_only` and `attempts.explore` is refused.

When it is on, the attempts get one more tool, `explore`, after all the
others (so the tool list before it is the one they get without it, and the
prompt cache still covers it):

```json
{ "question": "Where is the retry delay computed, and what calls it?" }
```

The designer, the vetter, the reviewer and the judge never get it.

## What the sub-agent can do

It is the run's `explorer` role, the same role that answers an `ask` run.
It is told the question and the project's context (the instruction files,
memory's notes and map), never the attempt's task or conversation, so a
question should stand on its own.

It is offered `read_file`, `list_files`, `search` and, when memory is up,
`find_symbol`, on the attempt's fork, which holds the attempt's changes so
far. A call to any other tool, `write_file`, `edit_file` and `run`
included, is refused without running, whatever the role or a pack says.
`run` is left out because a command in the fork can change it, and the
shell has no way yet to run a command that provably cannot. The user's
`tool.before` and `tool.after` hooks see its calls with the role
`explorer`.

When it stops calling tools, its final reply is the conclusion: the
attempt gets it as the result of its `explore` call, cut to 20,000 bytes.
Whatever the sub-agent read is dropped with its conversation.

## Its model, turns and budget

| Setting | Where | Default |
| --- | --- | --- |
| prompt | the explorer role's `prompt`, or `explorer.md` in your pack | the built-in `explorer` prompt |
| model | the explorer role's `model`, else `MOLT_EXPLORE_MODEL` | the run's model |
| effort | the explorer role's `effort` | the run's |
| turns | the explorer role's `max_turns` | 20 |
| budget | the explorer role's `budget_share` | 0.1 |

After the pack's explorer prompt, molt adds a paragraph a pack cannot drop:
the reader is another agent, it sees only the final reply, and every claim
needs its `path:line`. To give the sub-agent a cheaper model through the
pack, seat a role in the `explorer` slot:

```toml
[[role]]
name = "scout"
base = "explorer"
model = "haiku"
max_turns = 12
budget_share = 0.05

[[workflow]]
schema = 1
name = "scouting"
roles = { explorer = "scout" }
[workflow.stages]
context = {}
attempts = { explore = true }
deliver = {}
```

An attempt's own role can leave `explore` out of its `tools` to do
without it.

Each `explore` call gets a slice of what the attempt may still spend: the
share times what is left of the run's budget (or of a cap on this part of
the run), at the moment of the call. The slice comes out of the attempt's
budget and never adds to it. Before each of its turns the sub-agent checks
what it has spent: once that reaches the slice, or a call's cost is
unknown, it stops, and the attempt is told it ran out (`explore stopped
before it concluded: it spent its share of the budget ($0.99)`). As with
the designer's share, its last call can pass the slice by up to one call.
It stops the same way when it uses its turns, and the attempt can then ask
a narrower question or read the files itself.

What it spends counts toward the run's cost, the run's budget, and the
attempt's own `cost_usd` and `usage` in the reply; the attempt's `turns`
count only the attempt's own.

## Audit and cancellation

Its model calls carry the run's trace, as every call of the run does, the
role `explorer`, its prompt's version, and the index of the attempt it
works for (`"attempt": 0`), so the audit log ties each one to its attempt.
Its tool calls appear in the run's progress as
`attempt 1 explorer: read src/retry.rs`, and its end as
`attempt 1 explorer: concluded after 4 turns ($0.06)`.

It runs under the attempt's cancellation: when the attempt is cancelled
(another attempt won, the user stopped the run, the budget ran out), the
sub-agent's model call is cancelled too and it stops. A call it stopped
waiting for counts toward the run as any cancelled call does, and stays of
unknown cost until the gateway settles it.

## Not yet

- `run` for the sub-agent, which needs a command that cannot change the
  fork (a read-only mount, or a throwaway fork per command).
- Several `explore` calls of one turn run one after another, after the
  turn's other calls, not side by side.
- The sub-agent's turns are not streamed to the interface; the progress
  lines show its tool calls.
- How much `explore` saves per task has not been measured: the
  benchmark's before-and-after comparison of tokens per task is still to
  be run.
