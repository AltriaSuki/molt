# Model providers and prices

## Model providers

The gateway speaks three protocols: Anthropic Messages, OpenAI Chat
Completions and OpenAI Responses. Provider and protocol choice belong to the
gateway, not the planner. With `OPENAI_API_KEY` set, use `molt do "task" --model openai/gpt-4.1`;
`MOLT_PROVIDER=openai` sets the default provider and default model to GPT-4.1.
`anthropic/opus`, `anthropic/sonnet` and `anthropic/haiku` explicitly choose
Anthropic. Existing unprefixed aliases and Anthropic configuration are unchanged.
Both keys may be configured together; only the model service receives them.
Shell commands and sandbox environment policies exclude `OPENAI_*` too.

`OPENAI_BASE_URL` defaults to `https://api.openai.com` and is an origin, without
`/v1`. HTTPS is required except for loopback test servers. Redirects are refused.
Changing an endpoint does not imply support for another provider's protocol.
The optional `MOLT_MODEL_ALIASES` JSON object maps a plain alias to exactly one
provider/model, for example `{"fast":"openai/gpt-4.1-mini"}`. It is trusted
configuration, never loaded from project files.

GPT-4.1, mini and nano (including their 2025-04-14 snapshots) have built-in
capabilities for text, function tools, streaming and structured output over
Chat Completions, with a 32,768 output cap, a 1,047,576-token context window
and no effort parameter. Other OpenAI models require an explicit
`MOLT_OPENAI_MODELS` JSON declaration, for example
`{"my-model":{"tools":true,"streaming":true,"output_schema":false,"efforts":["low","medium","high"],"max_output":16000,"protocol":"responses","context":400000}}`.
`protocol` is `chat_completions` (the default) or `responses`; a declaration
must name the protocol the model's endpoint actually implements, and declaring
a built-in model replaces its built-in capabilities. `context`, when given, is
the model's context window in tokens, the reply included: every reply carries
it, and the planner fits the run's conversations to it instead of guessing
from the model's name (`MOLT_CONTEXT_TOKENS` still wins). Unsupported requested
capabilities fail before HTTP, including effort on Haiku rather than silently
dropping it. A protocol is never switched because an endpoint returned an
error.

The adapter converts unsigned text and function tool histories, preserving
call IDs, tool errors and tool results. Signed thinking, native citations,
media and other provider-native blocks cannot cross providers; start a new
conversation instead. A reasoning model's own reasoning is kept ([below](#reasoning-on-chat-completions)). OpenAI text/tool replies use the same planner blocks,
with provider and native finish reason in `stop_details`. Streaming buffers tool
JSON until completion, requires `[DONE]` and final usage, and never retries a
partial response. Cached prompt tokens are separated from uncached tokens for
pricing; an unknown returned model or missing usage is never charged as zero.
The spending limit remains a best-effort pre-call limit, not a guaranteed hard
remote billing cap. Cancellation stops local HTTP and leaves remote billing
unknown, as on the Anthropic path.

### Reasoning on Chat Completions

A reasoning model on a Chat Completions endpoint, such as DeepSeek's
`deepseek-reasoner` on a named endpoint ([models.md](models.md#named-endpoints)),
sends its reasoning beside the answer as `reasoning_content` (some
endpoints call it `reasoning`), streamed or whole. It comes back first in
the reply, as a `thinking` block with no signature that names its protocol
and the field it came in:

```json
{"type":"thinking","thinking":"I should read a.rs first.","protocol":"openai_chat","field":"reasoning_content"}
```

It is never previewed. Passed back unchanged, as the planner does with
every assistant turn, it goes back verbatim, in that field of its
assistant message, for as long as the user's turn it was given in lasts:
through that turn's tool calls and their results, as these endpoints
require of a model that reasons between calls. A user message that holds
no tool result starts a new turn, and the reasoning of every turn before
it is left out, as they ask; so is reasoning in turns a long task left out
or put in its record ([do.md](do.md#long-tasks)), which never holds
thinking. Another protocol never gets it: the Messages and Responses
adapters leave it out (and an assistant turn that held nothing else), since
it was that endpoint's state. Signed Anthropic thinking is still refused
on Chat Completions. Reasoning that is not text is refused as an unreadable
reply.

When the endpoint counts reasoning tokens apart
(`completion_tokens_details.reasoning_tokens`), the reply's usage carries
them as `reasoning_tokens`: a breakdown of `output_tokens`, which already
holds them and is what they are billed as. A reply without the breakdown,
and any Messages API reply, leaves it out: unknown, never zero, and a sum
holds it only while every call in it counted them.

### OpenAI Responses

A model declared with `"protocol":"responses"` is called at
`OPENAI_BASE_URL/v1/responses`, with the same key, aliases, capability checks,
retries, pricing and cancellation as Chat Completions. Every call sends
`store: false` and the whole conversation as `input`: no
`previous_response_id` or conversation object, so nothing depends on what the
endpoint keeps, and a run resumes and audits from its own messages. Function
calls and their outputs keep their `call_id`, and a failed tool's output says
so. Tools are sent with `strict: false` unless a tool asks for strict, since
Responses makes them strict by default; `output_schema` goes in
`text.format`, effort in `reasoning.effort`.

The native state a stateless conversation needs is a reasoning model's
reasoning. For a model whose declaration names efforts, the call asks for
`reasoning.encrypted_content`, and each reasoning item comes back as an
`openai_reasoning` block with its id, summary and encrypted content. Passed
back unchanged, as the planner does with every assistant turn, it goes to the
next call in its place before the calls it led to. A reasoning item the
endpoint sent without encrypted content is kept in the reply but cannot be
sent back without server-side state, so it is left out of later calls, as the
API allows. These blocks never cross protocols: a conversation holding one is
refused before HTTP by the Messages and Chat Completions adapters, and signed
Anthropic thinking is refused by the Responses adapter; start a new
conversation instead.

A reply maps to the shared blocks with `stop_details` naming
`"protocol":"responses"`, its `status` and any `incomplete_reason`. A
response cut at `max_output_tokens` stops with `max_tokens`, and a function
call it cut is kept only as the last block, with empty input, which the
planner never runs; an unfinished call anywhere else, a `failed` response,
annotations and other native output items (web search, files, images) are
refused, and their usage and cost stay unknown. A content-filtered response
is a refusal. Streaming previews only output text; function arguments are
taken from finished items, the reply and usage from the final
`response.completed` (or `incomplete`) event, so a stream cut before it, or
with an `error` event, has no answer and is never retried. Cached input
tokens are separated from uncached ones for pricing. Only text and function
tools are supported; built-in tools, media input and background responses
are not. The adapter is checked against a scripted endpoint, not certified
against any live model.

The implementation follows the [Responses reference](https://developers.openai.com/api/reference/resources/responses/methods/create).
The Chat Completions adapter follows the [Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
and [streaming contract](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).

## When a model call fails

The gateway retries what may pass on a second try (408, 409, 429, 5xx,
timeouts, connections that fail) up to `MOLT_MODEL_RETRIES` times, waiting
what `retry-after` says (at most 60 s) or backing off from one second. Each
retry is announced before its wait: the TUI shows it in the attempt's box
and in the status bar, as `model API 502, retrying in 60s (2/4)`, counting
down, and `molt do` prints the same line.

An error says what to fix where the status points at a setting:

- a 404 from an `ANTHROPIC_BASE_URL` (or `OPENAI_BASE_URL`) that ends in
  `/v1` says to remove it, since molt adds `/v1/messages` itself;
- a 401 names the credential variables to check;
- a model the API does not know, named one or two typos from an alias
  (`--model sonet`), gets the alias suggested.

A call the API refused with an error status (401, 403, 404 and the like,
which come with no usage), or that the gateway refused before sending, was
not billed: it settles at no cost and does not count among the calls of
unknown cost. Any other failure, and a refusal that follows an attempt that
timed out or was cut off, keeps the call's cost unknown. Memory does not
learn from a run that failed before any model call was answered, and says
so; it says when it skips learning because the budget is spent or because
some calls' cost is unknown.

## Model prices

The run budget (`--budget-usd`) holds only when molt knows what each model
call cost, so a call to a model with no price stops the run after it: an
unknown cost is never counted as free. Molt knows the prices of a few
thousand models, so third-party APIs reached through `ANTHROPIC_BASE_URL`
work too. It looks a model up, in order, in:

1. `MOLT_MODEL_PRICES`, prices you declare in US dollars per million tokens:
   `glm-4.6=0.6,2.2;deepseek-chat=0.28,0.42` (input, output), or four
   numbers to give cache write and cache read prices as well.
2. The snapshot `molt prices update` downloaded, in
   `~/.local/share/molt/prices.json` (`$XDG_DATA_HOME/molt` when set;
   `MOLT_PRICES_FILE` names another file), or, until there is one, the
   snapshot built into this release.
3. The Anthropic prices built into the gateway.

```sh
molt prices show deepseek-chat         # what a model costs and where that price came from
molt prices update                     # download LiteLLM's price list, check it, and use it
molt prices update --source openrouter # or OpenRouter's own prices
```

`update` refuses a download that is not a price list (not JSON, the wrong
shape, a negative or non-finite price) and leaves the current snapshot as
it is; it points out models whose price moved more than twofold. Snapshots
record their source, URL, SHA-256 and download time, and every model call's
reply names the entry and snapshot its cost came from. A dated id
(`-YYYYMMDD`) and a provider prefix (`openrouter/`) fall back to the bare
model id; a bare id that only several providers list gets the highest of
their prices. When a source leaves out cache prices they are estimated high:
a cache write at 1.25 times input and a cache read at the full input price.

Prices are read once when the gateway starts, never from the project
directory, and never downloaded during a run. The sandbox hides home
directories from the commands an agent runs, so they cannot change the
snapshot; with `--no-sandbox` they run as you and could.
