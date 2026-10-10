# Molt

Molt is a self-improving agent built on a microkernel. The kernel is small, never
changes itself and has no model inside it. Everything that thinks, remembers or
acts is a separate service that the agent can rewrite, test and roll back, the
way a crab sheds its shell to grow.

This repository is at **milestone 3: memory**. `molt do` takes a task in
plain words, settles on a done-check first, runs parallel attempts in private
forks of the project, verifies each with the check and applies the first that
passes. Each run starts from what Molt knows about the project: a map of the
code most relevant to the task, from an index of every definition and
reference that follows the files as they change, and the notes it learned
from earlier runs there. After the run, memory reads it back from the audit
log and keeps what would help next time. The agent cannot rewrite itself yet;
that starts with milestone 5.

## Architecture

```
 mutable services (agent may rewrite)     protected (human approves)
 planner · memory · tools · improver      evaluator · promotion gate
                 \                          /
                  every message, checked and logged
                               |
 ┌───────────────────────── kernel ───────────────────────────┐
 │ message bus · capabilities · supervisor · registry · audit │
 └────────────────────────────────────────────────────────────┘
```

| Part | What it does | Where |
| --- | --- | --- |
| Message bus | Routes requests, replies and events; bounded mailbox per service; per-pair FIFO | `molt-kernel/src/kernel.rs` |
| Capabilities | Unforgeable, budgeted rights to call one target; delegation can split a budget, never grow it | `molt-kernel/src/caps.rs` |
| Supervisor | Runs services as processes with rlimits, restarts with backoff, drains on stop | `molt-kernel/src/supervisor.rs` |
| Module registry | Content-addressed versions; promotion is compare-and-swap; tier rules enforced | `molt-kernel/src/registry.rs` |
| Audit log | Append-only, hash-chained, written before delivery, batched fsync | `molt-kernel/src/audit.rs` |
| Transports | `Transport` trait with Unix-socket and NATS implementations | `molt-transport` |
| Service SDK | Calls, replies, events, delegation | `molt-sdk` |
| Agent payloads | The request and reply types of every agent service | `molt-api` |
| Model gateway | `model.*`: the Anthropic Messages API; the only holder of the API key | `molt-gateway` |
| File and shell services | `fs.*` and `shell.*`: files, forks, diffs, merges and commands, confined to a root | `molt-tools` |
| Planner | `planner.run`: check first, parallel attempts, verify, apply | `molt-planner` |
| Memory | `memory.*`: notes with provenance, learned from finished runs; the project model (definitions, references, repo map) | `molt-memory` |

The kernel never trusts the sender named in a message. The transport
authenticates each service's endpoint (a per-service secret on Unix sockets,
per-service users and subject permissions on NATS) and the kernel stamps the
sender from that.

## Guarantees and the tests that hold them

| Guarantee | Test |
| --- | --- |
| No service can grant itself a capability or raise its own budget | `crates/molt-kernel/tests/invariants.rs` (`a_service_cannot_use…`, `delegation_can_split…`, `budgets_are_enforced…`) |
| No service can promote a kernel or protected component, change a tier, or add capabilities without a human | `only_humans_promote_protected_and_kernel_components` |
| No service can delete or rewrite audit entries; every message is on the record before delivery | `every_message_is_logged_before_it_is_delivered`, `the_audit_log_has_no_write_path_and_detects_tampering` |
| A failing service never takes the kernel down | `a_service_that_never_replies…`, `a_crashed_and_reconnected…`, `a_flooding_service…`, `garbage_on_the_wire_is_ignored`, `crates/molt/tests/e2e.rs` |
| Both transports behave the same: ordering, at-most-once, sender stamping, isolation, bounded inboxes | `crates/molt-transport/src/testkit.rs`, run by `conformance_unix.rs` and `conformance_nats.rs` |
| `molt do` never runs a config the project ships, and the API key never reaches the commands it runs | `crates/molt/tests/agent.rs` (`the_cli_carries_out_a_task`) |
| Reading the audit log takes a capability, and a read is recorded without a second copy of what it returned | `reading_the_log_takes_a_capability_and_is_recorded_without_a_second_copy` |
| Every learned note cites the messages behind it, and a run is learned from once | `crates/molt/tests/agent.rs` (`a_second_run_starts_with_what_the_first_learned…`), `crates/molt-memory/src/consolidate.rs` |

## Quick start

```sh
cargo install --path crates/molt     # installs molt, molt-echo and the agent services
export ANTHROPIC_API_KEY=...
cd your-project
molt do "make the parser accept trailing commas" --check "cargo test"
```

Without `--check`, the planner designs a check first (it may write test files
for it) and every attempt is held to it. The kernel on its own, with the demo
echo service:

```sh
cp molt.example.toml molt.toml        # then set its --scratch paths
molt run                              # Ctrl-C to stop
molt audit verify                     # check the hash chain
molt audit tail -n 20
```

To run across hosts on NATS, set `transport = "nats"`, run `molt nats-config`
to generate per-service credentials, put the printed `authorization` block and
`max_payload` setting in your nats-server config, and start `molt run`.

## `molt do`

```
molt do TASK [--check CMD] [--attempts N] [--model M] [--effort E] [--max-turns N]
        [--budget-usd X] [--no-apply] [--json] [--workspace DIR] [--data-dir DIR]
        [--pass-env NAME]... [--no-memory] [--no-learn] [--config FILE]
```

| Flag | Meaning |
| --- | --- |
| `--check CMD` | Shell command that exits 0 once the task is done, run in each attempt's fork. Default: the planner designs one. |
| `--attempts N` | Parallel attempts, 1 to 8 (default 2). The first to pass wins and the others are cancelled. |
| `--model`, `--effort` | Model for the attempts (`opus`, `sonnet`, `haiku` or an id) and how hard it thinks. |
| `--max-turns`, `--budget-usd` | Model turns per attempt, and the spending limit for the whole run. |
| `--no-apply` | Keep the result in its fork and print its path instead of merging it. |
| `--json` | Print the full result (`planner.run`'s reply in `molt-api`) as JSON. |
| `--workspace DIR` | The project (default: the current directory). |
| `--data-dir DIR` | Kernel state, the audit log and forks. Default `~/.cache/molt/<project>-<hash>` (under `$XDG_CACHE_HOME` if set). Must be outside the workspace. |
| `--pass-env NAME` | Pass a variable from your environment to the commands the agent runs, the check included. Repeatable. |
| `--no-memory` | Run without memory: no map or notes for the model, nothing learned. |
| `--no-learn` | Use memory, but do not learn from this run (learning is one more model call after it). |
| `--config FILE` | Run your own service setup (see `molt.example.toml`) instead of the default one. |

Progress goes to stderr and the result to stdout. Control characters in text
from the model or from file names are printed escaped.

| Exit status | Meaning |
| --- | --- |
| 0 | Passed (or finished unverified, when no automated check fits the task) and applied, or kept as `--no-apply` asks. |
| 1 | No attempt passed, or molt failed. |
| 2 | The command line was wrong (an unknown flag, `--attempts 9`, ...); nothing started. |
| 3 | Passed (or finished unverified), but the changes could not be merged, e.g. because the workspace changed under them; they are kept in the fork the report names. |
| 128+N | Stopped by signal N: 130 for Ctrl-C, 129 for SIGHUP, 143 for SIGTERM. The services are stopped and the run's forks removed. |

### Config and secrets

`molt do` reads a config file only when `--config` names one; it never picks
up a `molt.toml` on its own, since one shipped in a project could run any
command with your secrets. Without one it starts the default setup: the
gateway, `fs` and `shell` confined to the workspace with forks in
`<data dir>/work`, memory with its database in `<data dir>/memory.db`, and
the planner, using the service binaries installed next to `molt`. Data dirs that molt creates are private (0700), and it warns about
an existing one that others can read.

Services start with a scrubbed environment: `PATH`, `HOME`, the locale and a
few more, plus what each is configured to receive. `ANTHROPIC_API_KEY` goes to
the gateway only, and the commands the agent runs never see `MOLT_*` or
`ANTHROPIC_*` variables. Add what your builds and tests need with
`--pass-env` (or `pass_env` in a config).

A fork directory set with `--scratch` in a config must be private: the
`fs` and `shell` services create it 0700, and refuse to start on a symlink,
another user's directory or one that group or others may write to (`chmod
700` it). A service that cannot start ends `molt do` with the reason, such as
a missing binary, as soon as the supervisor gives up on it.

Defaults come from the environment:

| Variable | Read by | Default |
| --- | --- | --- |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` | gateway | required, `https://api.anthropic.com` |
| `MOLT_MODEL`, `MOLT_EFFORT`, `MOLT_MAX_TOKENS` | gateway | `opus`, unset, `32000` |
| `MOLT_MODEL_TIMEOUT_S`, `MOLT_MODEL_RETRIES`, `MOLT_MODEL_CONCURRENCY`, `MOLT_FALLBACKS` | gateway | `1200`, `4`, `16`, `1` |
| `MOLT_PLANNER_MODEL`, `MOLT_MAX_TURNS`, `MOLT_BUDGET_USD` | planner | the gateway's model, `50`, `10` |
| `MOLT_MAX_CHECK_ROUNDS`, `MOLT_CHECK_TIMEOUT_S` | planner | `3` check runs per attempt, `900` |
| `MOLT_MAP_TOKENS` | planner | `3000` (the project map a run starts with; at most `32000`) |
| `MOLT_MEMORY_MODEL`, `MOLT_MEMORY_EFFORT`, `MOLT_MEMORY_MAX_TOKENS`, `MOLT_MEMORY_MODEL_TIMEOUT_S` | memory | `sonnet`, `low`, `8000`, `300` |

The planner reads `MOLT_MAX_TOKENS` and `MOLT_MODEL_TIMEOUT_S` too: a model
call ends at that deadline, the gateway's retries included. `MOLT_FALLBACKS`
is `1` or `0` (off) and `MOLT_MODEL_RETRIES` may be `0`; every other number
must be above 0.

## Memory

Memory holds two things for each project.

**The project model** is every definition and reference in the project's
source (Rust, Python, JavaScript, TypeScript, Go, Java, C and C++, parsed with
tree-sitter), in SQLite. An index parses only files whose contents changed:
on this repository a first index takes about 170 ms and an unchanged one
2 ms; a 3,000-file project takes about 1.5 s, then 40 ms. It leaves out other
projects' code and build output (`node_modules`, Python virtualenvs, cache
directories such as Cargo's `target`) and Molt's data directory, gives each
file at most 2 seconds, and writes as it goes, so a stopped index keeps its
progress. It follows the files the `fs` service changes as they change. From it, a run starts with a map of
the code most relevant to its task, ranked the way Aider ranks it (PageRank
over references, weighted toward what the task mentions), and attempts can
ask where a name is defined and used (`find_symbol`).

**Notes** are one-sentence facts, conventions, decisions, preferences and
lessons. After a run, memory reads it back from the audit log, boils it down
to numbered steps, and has the model write what would help a later task, each
note citing the steps, and so the logged messages, it rests on. A note the run
bore out gains confidence; one it proved wrong is kept beside its correction,
both less certain, until more evidence settles it. Notes carry the version
of the service that wrote them, so rolling a version back retracts what it
learned. Recall ranks notes by keyword match (SQLite FTS5 with stemming),
confidence and recency.

```sh
molt memory show [WORDS...]          # notes about the project, best first
molt memory forget NOTE_ID --reason "the build moved to make"
molt memory map [WORDS...]           # the map a run with that task would start with
```

`forget` goes through the kernel, so the audit log records it; the note stays
as a tombstone with its reason.

## Tests

```sh
cargo test --workspace
```

The NATS conformance and end-to-end tests need a `nats-server` binary. Set
`MOLT_NATS_SERVER=/path/to/nats-server` or put it on `PATH`
(`go install github.com/nats-io/nats-server/v2@latest`). Without it they print
a notice and pass; CI always runs them.

## Roadmap

1. **Kernel**: bus with both transports, capabilities, supervisor, registry, audit log, invariant and conformance tests.
2. **A working agent**: Anthropic model gateway, planner, file and shell tools, parallel verified attempts, check-first.
3. **Memory** (this milestone): semantic store, recall, the living project model, consolidation.
4. **Evaluation**: deterministic replay from the audit log, regression and held-out suites, the promotion gate.
5. **Self-improvement, low risk**: prompts and skills only, auto-approved when the gate passes.
6. **Self-improvement, code**: Wasm services under wasmtime, sandbox builds, canary runs, hot swap.

## Known limits

- The default shell requires Linux x86_64 with bubblewrap 0.9+ and usable user namespaces. Unsupported hosts must explicitly choose --no-sandbox; namespace policy never silently falls back.
- No streaming: model replies arrive whole, so a long turn shows no progress until it ends.
- The audit log records every message, file contents and model conversations included, and is never rotated; it grows with every run in a data dir.
- Reading one trace back from the audit log scans the log from the start: there is no index yet. One read looks through at most 256 MiB and keeps to its deadline, and two run at once.
- The kernel drops a message nested more than 100 levels deep, so every logged message can be read back.
- After an interrupted run, forks are removed only from a scratch dir inside the data dir (the default); a configured scratch elsewhere may be shared with other runs and is left alone. A `molt do` killed outright (SIGKILL) removes none.
- Promotion updates the registry pointer but does not yet restart the running service; hot swap with in-flight draining lands with milestone 6.
- Call-cycle checks at the gate remain future work.
- Recall matches keywords (with stemming), not meaning: there is no vector index, since the Anthropic API has no embeddings endpoint. A note worded differently from the task can be missed; the strongest notes about the project are always shown.
- Notes learned from a run stay with that project, preferences included: a run's record holds text the project's files and commands chose. With the default setup, memory lives in the project's data dir, so nothing is shared between projects yet.
- The project model indexes the first 20,000 source files of a project, in walk order; `memory.index` reports when a project has more. A file that takes more than 2 seconds to parse keeps the symbols found by then.
- A topic capability currently allows both publishing and subscribing.
- No license has been chosen yet.

### Cancellation and model settlement

Dropping an SDK `PendingReply` requests `kernel.cancel` for that request. The kernel verifies the original caller, sends a kernel-only control message, and keeps the request open for one final cleanup or settlement reply. Cancellation is idempotent and does not affect another request. SDK services can use `serve_cancellable` for cleanup; ordinary concurrent handlers stop locally on cancellation. Cancelled queued work does not start.

Link loss ends concurrent admission immediately, even if every slot is occupied. Cooperative handlers get up to two seconds to clean up before they are aborted; queued work does not start after disconnection. Dropping a server cancels and removes its active request registrations. Shell shutdown closes command admission before sending TERM, and dropped shell request futures also abort their output readers.

Dropping the SDK service itself closes every pending call's waiter and requests cancellation without awaiting a reply. Background cancellation after a drop is best effort and bounded to one second, so a stalled transport cannot retain the link forever. Remote cancellation and unsettled usage remain unconfirmed.

The planner gives each attempt its own cancellation token. A winner stops competing attempts; budget exhaustion or unknown model cost stops new actions throughout the run. Shell cancellation and timeout send TERM to the process group, allow 500 ms for cleanup, then send KILL and reap the leader before replying. Service shutdown uses the same grace. Process groups alone cannot contain a program that deliberately creates a new session; OS isolation is tracked by #9.

The gateway closes its local HTTP request on cancellation and audits a correlated settlement record with `local_stopped`, `remote_cancel_confirmed`, `usage` and `cost_usd`. Anthropic Messages has no remote cancellation acknowledgement here, so remote cancellation and missing usage remain unknown. A completion already received wins the race and is counted once. After a kernel timeout, one authenticated late model reply can still be audited for up to one hour, without delivering a second run result; the late-settlement registry is bounded and temporary, while accepted records persist in the audit log. `molt audit --help` describes querying the log.

Run `cost_usd` is the known subtotal. `uncounted_calls` now includes pending calls, errors without usage, and replies without a known price. The terminal explicitly labels the known cost and incomplete settlement; automatic memory learning is skipped when settlement is incomplete. This is an estimated spending limit, not a provider-enforced dollar ceiling.

### Linux command isolation

The read-only system runtime includes `/etc/alternatives` so distribution-provided links such as `/usr/bin/cc` and `/usr/bin/awk` resolve to their tools. Other host `/etc` contents are absent except the loader cache, certificate directory and, when networking is enabled, resolver configuration.

The default `molt do` shell and done-check use the same Linux x86_64 backend, requiring `/usr/bin/bwrap` (bubblewrap 0.9+) and working unprivileged user namespaces. Startup probes the backend and fails if required capabilities are missing. `molt do --no-sandbox` and `molt-tools shell --no-sandbox` are explicit host choices; the library equivalent is `Shell::with_policy(..., ExecutionPolicy::Unconfined)`. Other architectures and macOS currently need that explicit choice.

Each command starts from an empty mount namespace with read-only system runtime paths, a read-only original project, its writable `fs.fork` directory, private tmp/home, and isolated PID, user, IPC, UTS and network namespaces. Direct isolated shell calls on the original project are refused. Ignored entries linked back to the project remain read-only; shared build directories are not writable. `/proc` shows only sandbox processes. User namespaces cannot be nested, capabilities are dropped, and a seccomp filter blocks AF_UNIX sockets (including access to host service sockets through mounted paths), io_uring (which could bypass syscall socket filtering), and alternate syscall ABIs. Programs must use ordinary file/socket I/O rather than requiring io_uring. The default denies network access. With explicit host networking enabled, IP connections, including localhost, are allowed; AF_UNIX stays denied.

`molt do --sandbox-policy /trusted/path/policy.json` supplies a JSON policy. It must be owned by the current user, not writable by others, and outside the canonical project and scratch roots. Each canonical path component is opened without following replacement symlinks; file permissions and content are checked through the same descriptor, and reads are bounded to 64 KiB. The shell reads it at startup; no request payload or repository file selects a policy. The default shell service is protected from service-driven replacement. A custom service configuration is still an explicit trusted user choice, and custom shell implementations are responsible for their own isolation.

```json
{
  "network": false,
  "read_only": ["/opt/toolchains", "/opt/dependency-cache"],
  "environment": ["RUSTUP_HOME", "CARGO_HOME", "CARGO_TARGET_DIR"],
  "memory_mb": 4096,
  "cpu_secs": 600,
  "file_mb": 128,
  "open_files": 256,
  "processes": 2048
}
```

Dependencies mount at their canonical absolute paths. `--pass-env NAME` explicitly allows another variable; HOME always points to private `/home/molt`, and service bus / Anthropic credentials cannot be passed. Toolchain paths outside `/usr` need declared read-only mounts. Builds that normally write into an ignored host-linked `target` or dependency cache must put their output in the writable fork or private `/tmp`. For example, declare a Rust toolchain mount and RUSTUP_HOME, set CARGO_HOME and CARGO_TARGET_DIR to private `/tmp` locations, and explicitly enable network when downloading dependencies; a prepopulated read-only dependency cache can instead be copied into private cache space for offline builds. The private cache disappears after each command. A C build using system `cc` needs no extra mounts.

Limits are inherited per process: address space, CPU time, file size, open files and process count, plus the existing command wall timeout. These are not aggregate cgroup quotas; RLIMIT_NPROC has the operating system's per-user semantics and does not limit root. TERM reaches the sandbox session group with 500 ms grace; KILL then tears down its PID namespace, including descendants that called setsid. Normal completion, cancelled handlers and service exit also tear down the namespace. Temporary mounts disappear with the last process. Future executable skills must use this shell path to share the policy.

CI installs bubblewrap and requires namespace tests to run. A constrained local executor may skip only those integration tests after a failed capability probe; production startup still fails closed. Tests cover host paths, symlinks, read-only dependencies, network policy, host sockets, limits, C builds, cancellation and descendants with new sessions.
