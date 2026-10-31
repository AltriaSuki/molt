# Service trust and candidate services

Capabilities limit what a service can ask for on the bus. They do not stop
a service's process from reading your files or opening a connection by
itself: a process the supervisor starts natively runs as you. So Molt runs a
service in one of two ways, and only one of them is for code an agent may
replace.

| | Native (`isolation = "native"`, the default) | Sandbox (`isolation = "sandbox"`) |
| --- | --- | --- |
| Who it is for | Code you trust: Molt's own services and services you install | A candidate service: one an agent may replace |
| What its process can reach | Whatever your user can: your files, the network, other services' databases | Only the kernel, through its bus connection, and its own private state |
| Who may promote a new version | Only you | A gate, within the registry's rules, or you |
| When the sandbox is unavailable | Not affected | Not started, and not promoted |

## Configuring a candidate

In your user config (`~/.config/molt/config.toml`) or a `--config` file, as
the `echo` service in `molt.example.toml` does:

```toml
[[service]]
name = "echo"
tier = "mutable"
isolation = "sandbox"
exec = { command = "molt-echo" }   # an absolute path, or a name found on PATH when Molt starts
provides = ["echo.*"]
requests = [{ target = "notes.add", budget = { calls = 100 } }]
memory_mb = 256
env = { GREETING = "hi" }
```

A project's `.molt/config.toml` cannot have services at all, as before.
A sandboxed service needs `exec`, and its `env` and `pass_env` cannot name
`MOLT_*`, `ANTHROPIC_*`, `OPENAI_*`, `HOME`, `PATH` or `TMPDIR`: Molt's
settings and the model credentials never reach a candidate. Anything else in
`env` and `pass_env` is passed because you wrote it there.

## What a candidate gets

Everything below is fixed by the kernel and your configuration when the
service is launched; nothing in the service's manifest, its messages or its
process can change it. The backend is the Linux sandbox the shell's commands
use ([Linux command isolation](sandbox.md)), started for each run of the
service:

- **Its bus connection and nothing else to talk to.** The supervisor opens
  a connection to the service's own endpoint for each start and hands it to
  the process as a descriptor (`MOLT_ADDRESS=fd:N`). The process sees no
  socket directory and cannot create a Unix socket (the seccomp filter
  refuses it), so it can reach no other service's endpoint, the interface's
  socket or a host daemon. It still presents its secret, and the kernel
  stamps everything it sends with its identity, as for any service.
- **Only the capabilities its manifest requests**, granted at launch with
  their budgets. It can be revoked, delegated to and budgeted like any
  holder. It can never hold `kernel.registry.propose` or
  `kernel.registry.promote` (nor `kernel.*`): a manifest that requests one is
  not launched, the kernel refuses to grant one to it, and a delegation of
  one to it is denied. So it cannot store or promote versions, its own
  included.
- **Its private state**: `<data dir>/state/<service>`, created readable by
  you only, mounted read-write at `/state` (`MOLT_STATE_DIR=/state`). It
  survives restarts. No other host path is writable.
- **Read-only**: the system directories (`/usr`, `/bin`, `/sbin`, `/lib`,
  `/lib64`), the loader cache, the certificate directory, `/etc/alternatives`,
  and its own program file when it is outside them. Not the rest of `/etc`,
  your home, the project, the data dir (the audit log, the registry, the
  sockets, other services' databases and state), or Molt's configuration.
- **No network**: a network namespace of its own with only a loopback
  device, and no resolver configuration. There is no setting that gives a candidate the
  network.
- **Exactly this environment**: the bus variables (`MOLT_ADDRESS`,
  `MOLT_SERVICE_ID`, `MOLT_SECRET`, `MOLT_CAPS`, `MOLT_VERSION`), its `env`
  and `pass_env`, `PATH=/usr/bin:/bin`, `HOME=/home/molt` (empty),
  `TMPDIR=/tmp` (private), `MOLT_STATE_DIR` and `PWD`. None of Molt's own
  environment. The variables reach bubblewrap through a descriptor, not its
  command line, so other processes cannot read the bus secret there.
- **Fixed limits**: the sandbox's address-space, CPU-time, file-size,
  open-file and process limits, with `memory_mb` and `cpu_secs` from the
  service's section. Its own processes only, in their own PID namespace.
  User namespaces cannot be nested, capabilities are dropped, and it cannot
  mount, set up io_uring or use another syscall ABI.

The supervisor restarts it as it restarts any service, each time confined
and with a new connection; stopping it sends SIGTERM to the sandbox's
processes, then tears the sandbox down after the drain time.

## Promotion

A version reaches a service only through the registry. A gate (a service
holding `kernel.registry.promote`) may promote a version only when all of
these hold; otherwise the request is denied and logged:

- the service runs confined now (it was last launched in the sandbox); a
  native service's versions are yours alone to change, whatever its tier;
- the backend confining it can confine now: the kernel probes it for each
  promotion, so a version is never approved for a host whose sandbox is gone;
- the gate is not promoting itself;
- the registry's rules: not a kernel- or protected-tier service, no change
  of tier, no capability the live version did not have (unless it is a
  rollback to a version that was live before).

A candidate cannot reach the evaluation or the decision: whatever a gate
measured and recorded lives outside the sandbox, the registry and the audit
log are not mounted, and it cannot hold a right to promote. Molt has no
automatic gate yet ([#10](https://github.com/AltriaSuki/molt/issues/10));
these are the rules any gate runs under. A promoted version does not yet
replace the running process, and `molt` installs the versions your
configuration names when it starts (see [Known limits](limits.md)).

## When the sandbox is unavailable

Molt probes the sandbox before it starts a candidate. Without bubblewrap
0.9+, usable unprivileged user namespaces, or on a platform other than Linux
x86_64, `molt` (and `molt do --config`) stops at startup with the reason, for
example `echo is not started: the linux-sandbox backend is unavailable: ...`.
It does not run the candidate natively, and `--no-sandbox` does not change
that: it is about the shell's commands. The only way to run a candidate's
program unconfined is to set `isolation = "native"` yourself, which makes it
a trusted host service: then a gate can no longer promote it. On the NATS
transport a candidate is refused too, since it would need the network to
reach the bus.

## Trusted services

These run natively and are trusted with what their process can reach; only
a person changes them:

- the kernel itself (bus, capabilities, registry, audit log, supervisor)
  and the `molt` binary that starts it from your configuration;
- `model`, the only holder of the API key; `shell`, which runs commands in
  the sandbox; `fs`, `memory` and `planner`, which need the project, the
  forks and their databases; `web`; and each MCP bridge (`mcp-NAME`), which
  runs its server in the sandbox ([MCP servers](mcp.md));
- every service you configure without `isolation = "sandbox"`.

`fs`, `memory` and `planner` are `mutable` tier, which marks what may
become agent-replaceable; while they run natively, a gate cannot promote them.

## What this protects, and what it does not

Protected from a candidate: your files and home, the project and its forks,
other services' databases and state, the capability table, the registry and
the audit log (reachable only as bus calls it holds capabilities for), the
API key and Molt's settings, the network, and the sandbox's own settings
(environment, mounts, syscall filter and limits).

Trusted, so outside what is protected: the Linux kernel, bubblewrap and the
system libraries mounted into the sandbox; the Molt kernel and the native
services above; your configuration; any value you pass a candidate in `env`
or `pass_env`; and any capability you grant it, which it can use within its
budget.

Covered: Linux x86_64 with bubblewrap 0.9+ and unprivileged user
namespaces, on the Unix transport. The tests in
`crates/molt/tests/candidate.rs` run a hostile candidate there that tries to
read your files, your home, `/etc/passwd`, another service's state, the
registry and the audit log, rewrite a gate's evaluation, reach the sockets, a
host port and DNS, see the kernel's process, make a user namespace, mount,
set up io_uring, write its own program and promote itself over the bus; each
is refused, while its granted call, its private state, its budget, a restart
and the revocation of its capability work. CI requires these tests; a local
executor without the sandbox skips them.

Not claimed:

- Escapes through a kernel, bubblewrap or system library bug, and side
  channels (timing, CPU caches) are not covered by these tests.
- The seccomp filter blocks the syscalls named above, not everything else:
  the rest of the kernel's interface is reachable from inside.
- Limits are per process, not an aggregate quota: a candidate can fill its
  state directory up to the disk, a file at a time, and use its CPU and
  memory limits in each of its processes.
- It can read what the system directories hold, such as which programs are
  installed.
- Its process sees the bus secret of its own endpoint, which lets it speak
  only as itself.
- macOS, other architectures, NATS deployments and a Wasm backend have no
  candidate backend yet; there a candidate does not run.
