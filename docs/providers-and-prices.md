# Model providers and prices

## Model providers

The gateway keeps the existing Anthropic Messages adapter and also supports
OpenAI Chat Completions. Provider choice belongs to the gateway, not the
planner. With `OPENAI_API_KEY` set, use `molt do "task" --model openai/gpt-4.1`;
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
capabilities for text, function tools, streaming and structured output, with a
32,768 output cap and no effort parameter. Other OpenAI models require an
explicit `MOLT_OPENAI_MODELS` JSON declaration, for example
`{"my-model":{"tools":true,"streaming":true,"output_schema":false,"efforts":["low","medium","high"],"max_output":16000}}`.
A declaration must describe a model that actually implements Chat Completions;
it does not adapt other wire protocols. Unsupported requested capabilities fail
before HTTP, including effort on Haiku rather than silently dropping it.

The adapter converts unsigned text and function tool histories, preserving
call IDs, tool errors and tool results. Signed thinking, native citations,
media and other provider-native blocks cannot cross providers; start a new
conversation instead. OpenAI text/tool replies use the same planner blocks,
with provider and native finish reason in `stop_details`. Streaming buffers tool
JSON until completion, requires `[DONE]` and final usage, and never retries a
partial response. Cached prompt tokens are separated from uncached tokens for
pricing; an unknown returned model or missing usage is never charged as zero.
The spending limit remains a best-effort pre-call limit, not a guaranteed hard
remote billing cap. Cancellation stops local HTTP and leaves remote billing
unknown, as on the Anthropic path.

The implementation follows the [Chat Completions reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
and [streaming contract](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).

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
