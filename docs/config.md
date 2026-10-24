# Configuration

## Config and secrets

`molt do` reads a whole config file only when `--config` names one; it never
picks up a `molt.toml` on its own, since one shipped in a project could run any
command with your secrets. Without one it starts the default setup, with your
user config and the project's data config over it (below): the
gateway, `fs` and `shell` confined to the workspace with forks in
`<data dir>/work`, memory with its database in `<data dir>/memory.db`, and
the planner, using the service binaries installed next to `molt`. Data dirs that molt creates are private (0700), and it warns about
an existing one that others can read.

## Configuration layers

Without `--config`, each layer goes over the ones before it:

1. the built-in setup above;
2. your user config, `~/.config/molt/config.toml` (`$XDG_CONFIG_HOME/molt/config.toml`
   when that is set): trusted, so it may add services, change them, add
   hooks, set `[tui]` and `[confirm]` defaults and define profiles. A
   `[[service]]` with the name of a built-in one replaces the keys it gives
   and keeps the rest; one with a new name is added. `[kernel]` is refused
   there, since each workspace has its own data dir;
3. the profile `--profile NAME` picks: a `[profile.NAME]` table with the keys
   of `[tui]`, over them;
4. the project's `.molt/config.toml` in the workspace, which may hold only data:
   `check` (the default done-check), `ask_paths` (added to the paths confirm
   mode stops for) and `instruction_files` (in place of `AGENTS.md` and
   `CLAUDE.md`). A project config with `[[service]]`, `hooks`, `env` or
   `pass_env` is refused with the reason, since whoever wrote the project
   would choose what runs with your environment;
5. environment variables: `MOLT_PLANNER_MODEL` or `MOLT_MODEL` over `model`,
   `MOLT_EFFORT` over `effort`, `MOLT_BUDGET_USD` over `budget`,
   `MOLT_MEMORY_MODEL` over `memory_model`, and `MOLT_INSTRUCTION_FILES` over
   `instruction_files`;
6. the command line, then the interface's saved settings and what you set in
   the session.

```toml
# ~/.config/molt/config.toml
[tui]
model = "sonnet"
attempts = 2

[profile.cheap]
model = "haiku"
attempts = 1
budget = 0.5

# Added to the default services; the planner may call it, as with --allow-web.
[[service]]
name = "web"
tier = "protected"
provides = ["web.*"]
exec = { command = "molt-web", args = ["--allow", "docs.rs"] }
```

`model`, `effort`, `attempts`, `budget` and `check` of `[tui]` are `molt do`'s
defaults too where its flag is not given (`molt ask` takes `model`, `effort`
and `budget`). `molt config show` prints the configuration in effect, and
`--origin` prints one `key = value` line per value with the layer it comes
from; secret-looking variables are redacted. With `--config FILE`, that file
is the whole configuration, as before, and `--profile` picks one of its own
`[profile.*]` tables.

Services start with a scrubbed environment: `PATH`, `HOME`, the locale and a
few more, plus what each is configured to receive. `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` go to the gateway only, and the commands the agent runs
never see `MOLT_*` or `ANTHROPIC_*` variables. Add what your builds and tests need with
`--pass-env` (or `pass_env` in a config).

A fork directory set with `--scratch` in a config must be private: the
`fs` and `shell` services create it 0700, and refuse to start on a symlink,
another user's directory or one that group or others may write to (`chmod
700` it). A service that cannot start ends `molt do` with the reason, such as
a missing binary, as soon as the supervisor gives up on it.

Topic permissions use `topic:publish:<name>` or `topic:subscribe:<name>` in a
service's `requests`. The kernel checks publication separately from subscription;
the SDK chooses the corresponding capability. Event destinations remain
`topic:<name>`, so event payloads and subscriptions' topic names do not change.
A subscription's recorded capability and its ancestors must still be live at
delivery; revocation and expiry end access to subsequent events. A denied
publication is recorded in the audit log before the denial is delivered.

Existing `topic:<name>` grants remain compatible and authorize both operations.
They can be delegated as a narrower publish or subscribe grant, but a narrow
grant cannot become the combined grant or the other right. To migrate, upgrade
the kernel and SDK-backed services together, then replace each combined grant
with the operations that service actually uses. The default and example setups
give model/planner publication on progress, fs publication on fs.changed, memory
subscription on fs.changed, and CLI/TUI subscription on progress. Older kernels
and SDKs do not recognize the new permission spellings.

For Claude Code or a compatible proxy, the same `ANTHROPIC_AUTH_TOKEN` and
`ANTHROPIC_BASE_URL` work here. Without `ANTHROPIC_API_KEY`, the gateway sends
`Authorization: Bearer <token>`; with an API key it sends only `x-api-key`.
If both are set, a startup warning explains that the API key takes precedence.
Empty or whitespace-only values count as unset. These credentials never reach
sandbox commands, including when `--pass-env` is requested.

Defaults come from the environment:

| Variable | Read by | Default |
| --- | --- | --- |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` | gateway | API key preferred; Bearer token fallback (or use `OPENAI_API_KEY`) |
| `ANTHROPIC_BASE_URL` | gateway | `https://api.anthropic.com` |
| `MOLT_MODEL`, `MOLT_EFFORT`, `MOLT_MAX_TOKENS` | gateway | `opus`, unset, `32000` |
| `MOLT_MODEL_TIMEOUT_S`, `MOLT_MODEL_RETRIES`, `MOLT_MODEL_CONCURRENCY`, `MOLT_FALLBACKS` | gateway | `1200`, `4`, `16`, `1` |
| `MOLT_MODEL_PRICES`, `MOLT_PRICES_FILE` | gateway | unset, `~/.local/share/molt/prices.json` (see [Model prices](providers-and-prices.md#model-prices)) |
| `MOLT_PLANNER_MODEL`, `MOLT_MAX_TURNS`, `MOLT_BUDGET_USD` | planner | the gateway's model, `50`, `10` |
| `MOLT_DESIGN_BUDGET_SHARE` | planner | `0.25`: the share of a run's budget designing its check may spend, more than 0 and at most 1 (see [Designing has a budget](do.md#designing-has-a-budget)) |
| `MOLT_MAX_CHECK_ROUNDS`, `MOLT_CHECK_TIMEOUT_S` | planner | `3` check runs per attempt, `900` |
| `MOLT_MAP_TOKENS` | planner | `3000` (the project map a run starts with; at most `32000`) |
| `MOLT_CONTEXT_TOKENS` | planner | the model's context window: `200000` for Claude models, `128000` for others |
| `MOLT_INSTRUCTION_FILES` | planner | `AGENTS.md,CLAUDE.md` (the project's instruction files; `none` reads none) |
| `MOLT_SYNTAX_CHECK` | planner | `report` (see [Syntax errors after an edit](do.md#syntax-errors-after-an-edit)); `reject` or `off` |
| `MOLT_MEMORY_MODEL`, `MOLT_MEMORY_EFFORT`, `MOLT_MEMORY_MAX_TOKENS`, `MOLT_MEMORY_MODEL_TIMEOUT_S` | memory | the attempts' model (the run's `--model`, else `MOLT_PLANNER_MODEL`, else the gateway's `MOLT_MODEL`), `low`, `8000`, `300` |

The planner reads `MOLT_MAX_TOKENS` and `MOLT_MODEL_TIMEOUT_S` too: a model
call ends at that deadline, the gateway's retries included. `MOLT_FALLBACKS`
is `1` or `0` (off) and `MOLT_MODEL_RETRIES` may be `0`; every other number
must be above 0.

## Trusted lifecycle hooks

An explicitly selected `--config /outside/project/molt.toml`, or the user
config, may contain `[[hooks]]` entries. Files inside the workspace never
supply hooks, even with `--config`, and a project's `.molt/config.toml` with
hooks is refused.

```toml
[[hooks]]
event = "before_apply"
command = "cargo fmt"
blocking = true
timeout_ms = 30000

[[hooks]]
event = "run_finished"
command = "printf 'Molt finished\\n'"
```

Supported events are `before_apply`, `after_apply`, `run_finished`, and
`attempt_failed`. Only `before_apply` may block. Its commands run sequentially
in each passing candidate's isolated fork after acceptance, before ranking, diff, review
and application. The authoritative check files are restored, protected paths
are checked, and acceptance runs again even if a nonblocking formatter failed.
A failing check or blocking hook keeps the result in its fork and applies
nothing. Retained results (including the TUI's confirm mode) are prepared the
same way before they are shown, so the diff/review covers the formatted result.

Notification hooks run in a separate disposable fork and cannot change the
applied workspace. `after_apply` is also sent when the TUI user applies a kept
result; `run_finished` means the planner finished, not that the user accepted a
kept result. All commands use the existing shell sandbox, environment scrubbing,
process-tree cleanup and bounded output. They do not gain host network or
notification access. Shell requests/results record output, exit status and
elapsed time in the audit; planner notes identify the lifecycle event. Timeouts
are 1–600000 ms, commands at most 8192 bytes, and at most 32 hooks are accepted.
An interrupted or resumed run may deliver notifications again; use idempotent
notification commands. Hooks are not part of the model-editable RunRequest.
