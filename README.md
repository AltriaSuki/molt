# Molt

Molt is a self-improving agent built on a microkernel. The kernel is small, never
changes itself and has no model inside it. Everything that thinks, remembers or
acts is a separate service that the agent can rewrite, test and roll back, the
way a crab sheds its shell to grow.

This repository is at **milestone 1: the kernel**. There is no LLM yet; the
point of this milestone is a kernel you can trust before anything is allowed
to rewrite the services around it.

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

## Quick start

```sh
cargo install --path crates/molt     # installs `molt` and `molt-echo`
cp molt.example.toml molt.toml
molt run                              # Ctrl-C to stop
molt audit verify                     # check the hash chain
molt audit tail -n 20
```

To run across hosts on NATS, set `transport = "nats"`, run `molt nats-config`
to generate per-service credentials, put the printed `authorization` block and
`max_payload` setting in your nats-server config, and start `molt run`.

## Tests

```sh
cargo test --workspace
```

The NATS conformance and end-to-end tests need a `nats-server` binary. Set
`MOLT_NATS_SERVER=/path/to/nats-server` or put it on `PATH`
(`go install github.com/nats-io/nats-server/v2@latest`). Without it they print
a notice and pass; CI always runs them.

## Roadmap

1. **Kernel** (this milestone): bus with both transports, capabilities, supervisor, registry, audit log, invariant and conformance tests.
2. **A working agent**: Anthropic model gateway, planner, file and shell tools, parallel verified attempts, check-first.
3. **Memory**: semantic store, recall, the living project model, consolidation.
4. **Evaluation**: deterministic replay from the audit log, regression and held-out suites, the promotion gate.
5. **Self-improvement, low risk**: prompts and skills only, auto-approved when the gate passes.
6. **Self-improvement, code**: Wasm services under wasmtime, sandbox builds, canary runs, hot swap.

## Not in this milestone

- Promotion updates the registry pointer but does not yet restart the running service; hot swap with in-flight draining lands with milestone 6.
- Cancellation tokens, call-cycle checks at the gate and an `audit.read` endpoint for the evaluator arrive with the services that need them.
- A topic capability currently allows both publishing and subscribing.
- No license has been chosen yet.
