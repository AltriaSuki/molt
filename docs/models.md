# Models: probes and role models

The gateway knows what the Claude models and GPT-4.1 accept, and takes
your word for other OpenAI models in `MOLT_OPENAI_MODELS` (see
[Model providers and prices](providers-and-prices.md)). A third-party
endpoint can still differ from what a model's name suggests: one ignores
`output_schema` and answers in prose, another refuses `effort` with a 400.
`molt models probe` finds that out once, so the gateway shapes calls for
what the endpoint does instead of failing in the middle of a run. And
`[models.roles]` sends each role's calls to its own model: the reviewer and
the explorer on a cheaper one, the attempts on the strong one.

## What molt knows: `molt models show`

```sh
molt models show            # the default model, the aliases, and what probes found
molt models show glm-4.6    # where its calls go, what the gateway knows, what a probe found, its price
```

It reads the gateway's variables from your environment, as `molt prices`
does, and calls nothing.

## Probing a model: `molt models probe`

```sh
molt models probe glm-4.6
```

```
Probing anthropic/glm-4.6 at https://open.bigmodel.cn/api/anthropic makes up to 4 calls, one at a time,
with no retries and at most 512 output tokens each. They are billed as usual:
  - a tool call
  - a streamed reply
  - an answer in a JSON schema (output_schema)
  - a request with effort "low"
Make these calls? [y/N]
```

A probe calls the real model and costs money, so only this command ever
probes: never the gateway when it starts, nor a run. It says what it will
call and asks first; from a script, `--yes` answers for you, and without a
terminal and without `--yes` it calls nothing. Each check is one small call:

| Check | Works when |
| --- | --- |
| tools | the reply calls the tool it was asked to call |
| streaming | a streamed request is answered with a stream |
| output_schema | asked for prose with a schema, the answer is JSON that matches the schema |
| effort | a request with an effort is taken |

A 400 or 422 is the endpoint's answer about the feature: it does not work
there. A 401, 403 or 404, or an endpoint out of reach, says nothing about
the feature, so the probe stops at its first such answer, records nothing
and says what to fix. Any other failure (a 429, a timeout) leaves that
feature unknown. Haiku takes no effort and an OpenAI model only the
efforts declared for it, so the probe does not ask for what the gateway
never sends. It prints what each call cost; a call with no known price
makes the total unknown, never zero.

What it finds is kept in `~/.local/share/molt/models.json`
(`$XDG_DATA_HOME/molt` when set; `MOLT_MODELS_FILE` names another file),
beside the prices, for the model on that endpoint: once `ANTHROPIC_BASE_URL`
or `OPENAI_BASE_URL` changes, the record no longer applies. Probing again
replaces it. The gateway reads the file when it starts, so a run started
after the probe goes by it, and one already running keeps what it started
with.

## What the gateway does with it

For a model a probe found:

- **ignores `output_schema`**: a call with a schema is asked for through a
  tool instead. The request gets one tool, `molt_output`, whose input is the
  schema, and must call it (a forced tool call, so without thinking); the
  tool's input comes back as the reply's JSON text, as from a model that
  keeps to the schema. A call with tools of its own and a schema is refused
  before anything is sent, since forcing the schema's tool would keep the
  model from its own. Memory has learned through a tool on every endpoint
  since #140, so it needs no probe for this.
- **refuses effort**: no default effort is sent, and a call that asks for
  one is refused before anything is sent, as on Haiku.
- **does not stream**: a call that asks for previews is made whole; the
  reply is the same and only the previews are lost.
- **does not call tools**: a call with tools is refused before anything is
  sent, saying the probe found it.

A feature the probe found working, or could not tell, is used as before.

## Role models

```toml
# ~/.config/molt/config.toml
[models.roles]
reviewer = "haiku"
explorer = "haiku"
memory = "haiku"
```

Each role's calls go to its model when its declaration (RFC 0003) names
none. The roles are `designer`, `vetter`, `reviewer`, `judge`, `router`,
`explorer` (the sub-agent of `explore`), `compactor` and `memory`. The
attempts are not a role here: their model is the run's (`--model`,
`[tui] model`), and `attempt` is refused with that.

What wins, for a role's calls: a request's `judge_model` for the judge
(as it wins over a judge role's model); then the role's own declaration in
the prompt pack; then `MOLT_EXPLORE_MODEL` for the explorer and
`MOLT_ROUTE_MODEL` for the router; then `[models.roles]`; then the run's
model. For memory, a model the learning request names (`molt do
--memory-model`, `[tui] memory_model`) and `MOLT_MEMORY_MODEL` win over it.

The planner and memory get the table as `MOLT_ROLE_MODELS`, a JSON object
such as `{"reviewer":"haiku"}`; set in the environment, that is used
instead of `[models.roles]`. Only your own config has it: a project's
`.molt/config.toml` cannot choose models. A routed model needs a price
like any other, or a run stops after its first call.
