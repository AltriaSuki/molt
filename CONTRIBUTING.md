# Contributing to Molt

Thanks for helping. Molt is a self-improving agent on a microkernel, and its
value rests on a few guarantees holding no matter which service changes. This
guide says how to propose work, what a change has to keep true, and how it
gets checked and merged.

## Start from an issue

Every change beyond a typo starts from an issue, so the scope is agreed
before the code is written. Comment on the issue to say you are taking it;
if none exists, open one first.

Issues follow the format of the existing ones:

- **Title:** `[area] what should change`, for example
  `[审计] 流式校验与有界 tail，避免历史日志撑满启动内存`. Areas in use:
  执行, 交互, 会话, 记忆, 模型, 调度, 评估, 审计, 上下文, 合并, 缺陷, 内核,
  网关, 规划, 工程, 自我改进.
- **Body:** priority and impact (P0 to P3), background or the code that shows
  the problem (link lines on a fixed commit), what to build, acceptance
  conditions that a test can check, and dependencies on other issues.
- Say what you checked and what you only inferred. "Not measured yet" is
  fine; an unsupported claim is not.

Issues may be written in Chinese or English. Code, comments, commit messages
and the README are in English.

## Set up

You need:

- Rust stable with `rustfmt` and `clippy` (`rustup component add rustfmt clippy`).
- `nats-server` for the NATS conformance and end-to-end tests:
  `go install github.com/nats-io/nats-server/v2@latest`, then
  `export MOLT_NATS_SERVER=$(go env GOPATH)/bin/nats-server`. Without it those
  tests print a notice and pass; CI always runs them.
- On Linux, `bubblewrap` 0.9+ and working unprivileged user namespaces for
  the shell isolation tests (`bwrap --unshare-all --ro-bind / / true` should
  succeed). Set `MOLT_REQUIRE_SANDBOX_TESTS=1` to make a missing backend fail
  instead of skip, as CI does.
- To validate benchmark tasks: `python3`, `node` 20+, `go` and `cargo`.

## Before you open a pull request

Run what CI runs, from the repository root:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touched `bench/tasks`, also run `molt bench validate` (or
`cargo test -p molt-bench --test tasks`).

A change that fixes a bug comes with a test that fails without the fix. A
CI-only failure is still yours to root-cause: "flaky" is not a cause. Never
skip, disable or loosen a test to get green.

## What every change must keep true

These are the project's invariants. Each has tests listed in the README's
"Guarantees and the tests that hold them" table; a change that weakens one
needs an issue and the maintainer's agreement first.

- **The kernel stays small and has no model in it.** Anything that thinks,
  remembers or acts is a service on the bus.
- **The kernel never trusts a sender's claim.** The transport authenticates
  the endpoint and the kernel stamps the sender. No service can grant itself
  a capability or raise its own budget; delegation can split a budget, never
  grow it.
- **The audit log is append-only and written before delivery.** There is no
  write path that edits or deletes entries.
- **Protected components need a human.** Only a human promotes the kernel,
  the evaluator or the promotion gate, changes a tier, or adds capabilities.
  An agent-generated change must never be able to edit its own acceptance
  check, held-out data or gate thresholds.
- **Secrets stay where they are.** Only the model gateway holds the API key;
  it never reaches commands, the check, logs meant for the user, or the bus
  in clear.
- **Commands run isolated by default.** Shell commands and the done-check go
  through the Linux sandbox. `--no-sandbox` is an explicit user choice and
  nothing may fall back to it silently.
- **Unknown is never zero.** A cost, token count or cancellation that is not
  confirmed stays unknown in replies and totals; it is never recorded as free
  or as done.
- **Wire types stay compatible.** New fields in `molt-api` and `molt-proto`
  payloads get `#[serde(default)]` and, where an old peer would choke,
  `skip_serializing_if`, so older callers keep working. A breaking change to
  a message type says so in its PR.

When you add a guarantee, add its test and a row to that README table. When
you add or remove a limitation, update "Known limits".

## Code style

- `rustfmt.toml` sets the format (120 columns); `cargo fmt` decides, not
  taste.
- Clippy runs with `-D warnings`. Fix the cause rather than adding `allow`;
  an `allow` that stays carries a comment saying why.
- Comments explain why, not what. Match the density of the code around you.
- Error messages say what went wrong and what to do, in plain words, for
  example `check is empty: leave it out to have one designed`.
- Test names are sentences about behavior:
  `a_service_cannot_use_a_capability_it_does_not_hold`.
- Keep a change to one crate's internals out of other crates' public APIs
  unless the issue calls for it.

## Commits

- One logical change per commit; it should build and pass tests on its own.
- Subject in the imperative, sentence case, no trailing period, no type
  prefix, about 72 characters at most: `Propagate request cancellation and
  preserve unknown model settlement`.
- The body explains why and anything a reviewer would not see from the diff.
- Never commit secrets, API keys, local paths or audit logs.

## Pull requests

- Branch from `main`: `feat/<topic>` for features, `fix/<topic>` for fixes.
- Keep a pull request to one issue. If work depends on an unmerged pull
  request, base the new one on that branch and say so; it is retargeted to
  `main` when the parent lands.
- Put `Closes #N` in the description, say what changes for a user (before
  and after), how it works, and what you ran. Draft pull requests are welcome
  for early feedback.
- To bring a branch up to date, merge `main` into it. Do not rebase or
  force-push a branch someone else is working on.
- CI must be green, and review comments answered or addressed, before a
  merge.
- Pull requests are squash-merged. The commit title is the pull request
  title followed by its number, as in `Milestone 3: memory service with
  notes and a living project model (#2)`.

## Benchmark tasks

New tasks under `bench/tasks` follow the rules in `bench/README.md`: the
prompt is the whole spec, hidden tests check only what it states, standard
toolchains only, no network, and a reference solution that passes. Never
change a task that results were already recorded for; add a new one, since
results record each task's content hash.

## Security

Report a sandbox escape, a way to forge a sender or a capability, a secret
leak, or a way around the audit log privately to the maintainer
(@AltriaSuki) instead of in a public issue.

## License

No license has been chosen yet (#33). Until one is, contributions are
accepted on the understanding that they will be released under the license
the project adopts.
