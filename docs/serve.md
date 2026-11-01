# `molt serve`: Molt for other programs

`molt serve` keeps the services of one workspace up for programs that speak
Molt's client protocol: an editor's adapter, a web page's backend, a
script. The terminal interface is one of them: it starts a server of its
own and does everything that needs the services through the protocol. The
protocol is JSON-RPC 2.0, one message a line; its methods, events, errors
and compatibility rules are in [RFC 0004](rfc/0004-client-protocol.md).

```sh
molt serve                      # listens on serve.sock in the workspace's data dir
molt serve --socket /run/user/1000/molt.sock
molt serve --stdio              # one client, on standard input and output
```

It takes the flags `molt ui` takes (`--workspace`, `--project`,
`--data-dir`, `--pass-env`, `--deps-cache`, `--allow-web`, `--web-resolver`,
`--config`, `--profile`), resolves the config once at start, and works in
that one workspace until SIGINT, SIGTERM or SIGHUP. On the socket it says
where it listens on standard error; with `--stdio` the services' output
goes to standard error, never into the protocol.

## A client

A client says which version of the protocol it speaks, opens the session,
and then works as the interface does: it submits a task with an id of its
own, follows its `progress` events, and decides what to do with the result.

```python
import json, socket, secrets

sock = socket.socket(socket.AF_UNIX)
sock.connect("/home/me/.cache/molt/project-1a2b3c/serve.sock")
lines = sock.makefile("rw")
next_id = 0

def call(method, params):
    global next_id
    next_id += 1
    lines.write(json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params}) + "\n")
    lines.flush()
    for line in lines:
        msg = json.loads(line)
        if msg.get("method") == "progress":
            print(msg["params"]["event"], msg["params"].get("detail", ""))
        elif msg.get("id") == next_id:
            if "error" in msg:
                raise RuntimeError(msg["error"]["message"])
            return msg["result"]

call("initialize", {"protocol": 1, "client": {"name": "example"}})
call("session.open", {})
trace = "trace_" + secrets.token_hex(16)
result = call("task.submit", {"trace": trace, "request": {
    "task": "Write a greeting into greeting.txt.", "workspace": "", "apply": False, "stream": True}})
if result["outcome"] == "passed" and input(result["patch"] + "\nApply? [y/N] ") == "y":
    call("result.apply", {"fork": result["fork"]})
call("session.close", {"keep": []})
```

An empty `workspace` in a request is the server's; another workspace is
refused. A result kept in its fork (`apply` false) is removed when the
session closes unless the client applies it, keeps it (`result.keep`, or
`keep` on `session.close`), or discards it. A client that goes away without
closing its session leaves every fork its session made.

## Sessions that stay

A conversation can outlive the client and the server: a **stored session**
([RFC 0006](rfc/0006-sessions.md)). A client makes one with
`sessions.new` or picks one up with `sessions.load`. From then on, its
connection's tasks go into the session's journal:

- the task, before the planner is asked;
- the reply, with its cost, and the attempt's conversation with its tool
  results;
- an attempt's questions and the answers given;
- whether a result was applied, kept or discarded.

Each entry is on disk before the request it records is answered.

```python
session = call("sessions.new", {"title": "parser work"})["session"]
# ... tasks as above; the next day, from any client:
session = call("sessions.load", {"id": session["id"]})["session"]
print(len(session["earlier"]), "tasks,", session["spent"])
```

- A task that names no `earlier` of its own is told of the session's
  answered tasks, and its runs are grouped under the session's id in
  `molt runs --session`.
- Closing the session, a client that goes away, or a server that is killed
  leaves the stored session as it was. Results it still waits on stay in
  their forks.
- Loading it starts nothing and calls no model. A task that never got its
  reply is listed as `unanswered`, and what it spent is unknown. One that
  kept a checkpoint is in `runs.list`, to resume if you choose.
- One client holds a session at a time. Another's `sessions.load` is
  refused with `BUSY` until it lets go.
- `sessions.list` lists the workspace's sessions, and names any it cannot
  read. `sessions.archive` archives one (`restore` brings it back), and
  `sessions.delete` removes it.
- `sessions.new` with `import` starts from what the interface saved of its
  last session in the workspace, which is only its earlier tasks'
  summaries. The loaded session says so.

The sessions are in `sessions/` in the data dir. A damaged journal, or one a
newer molt wrote, is refused with the line and why, and is left as it is.

## Who can use it

- The socket has mode 0600 and the data dir molt makes is private (0700).
  `molt serve` refuses a socket directory other users may write to, and
  replaces a socket left by a server that is gone, but not one a server
  still answers on.
- A process of another user is turned away even if the socket's mode lets
  it connect: the server checks each peer's user.
- A client can do what the interface can, and no more: the config, the
  workspace, the services and their capabilities come from the server's
  start. Nothing is applied unless the client asks, and only the client
  answers an attempt's question, so the confirmation a person gives stays
  with the program they use.
- One session at a time: while one client has the session open, another's
  `session.open` is refused.

## The interface as a client

The interface starts its server as a child (`molt ui-backend`), in a process
group of its own so it keeps working while the interface is suspended, and
hands it the config it resolved on a private pipe. From then on the pipe
carries the protocol. The interface still reads the workspace's files for
`@` completion, git for `/diff`, the audit log for the transcript and its
log file itself, as any local program can.
