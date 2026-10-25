# Molt in your editor: `molt acp`

`molt acp` lets an editor that speaks the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP) use Molt as its
agent: Zed and JetBrains IDEs do natively, and Neovim, Emacs and VS Code
through plugins. The editor starts `molt acp` and talks to it on standard
input and output. You type a task in the editor's agent panel, follow the
run there, and confirm the apply there: nothing is written to your project
until you pick **Apply**.

It speaks ACP version 1.

## Set it up

`molt` must be on the editor's `PATH`, or give its full path. The model's
key comes from the environment the editor starts it in (`ANTHROPIC_API_KEY`,
or what your [config](config.md) names), or from the `env` of the setting.

### Zed

In Zed's `settings.json`:

```json
{
  "agent_servers": {
    "Molt": {
      "type": "custom",
      "command": "molt",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Then open the agent panel, start a new thread with **Molt**, and type a task.
If your version of Zed does not take `type`, leave it out.

### JetBrains IDEs and others

An editor that takes an ACP agent as a command takes `molt` with the
arguments `acp`. JetBrains IDEs with ACP support read agents from
`~/.jetbrains/acp.json` (see JetBrains' documentation for your version):

```json
{
  "agent_servers": {
    "Molt": { "command": "/path/to/molt", "args": ["acp"] }
  }
}
```

### Flags

`molt acp` takes the global `--config` and `--profile`, and the flags of
`molt serve` that are not about one folder: `--pass-env`, `--deps-cache`,
`--allow-web` and `--web-resolver`, for instance
`"args": ["--profile", "fast", "acp", "--deps-cache", "cargo"]`. Each folder
gets its data dir as `molt` would give it (`~/.cache/molt/<name>-<hash>`).

## What happens

- **A session** is a thread in the editor, in the folder the editor names
  (ACP's `cwd`, which must be an absolute path). The folder is the
  workspace, and the only one: nothing the editor sends can name another.
  The first session in a folder starts Molt's services for it, as `molt`
  does; later sessions in the same folder share them.
- **A prompt** is a task, run through the `auto` workflow as the interface
  runs one: the planner picks `ask`, `quick` or `verify`. A file you mention
  (`@file` in Zed) is given to the task as `@path` is in the interface; a
  selection, or a file outside the folder, goes into the task as text.
  Images are not taken. The tasks of a thread before it go with it as
  earlier tasks, as in the interface. The defaults of `[tui]` (and of
  `--profile`) apply: `attempts`, `model`, `effort`, `budget`, `check`,
  `memory_model` and `stream`.
- **While it runs**, the editor shows:
  - the workflow's steps as a plan (design the check, make the change,
    review, apply), each marked as it starts and ends;
  - each tool call of the attempts (`read`, `write`, `run`, ...), with the
    file it touches, marked done when the next one starts;
  - each run of the check, passed or failed with the end of its output;
  - the models' text and Molt's notes as the agent's thoughts.
- **At the end** the agent's message is the run's summary and its outcome
  (passed its check, judged, unverified, or failed) with what it cost. A
  result with changes comes as a pending edit with a diff of each file, and
  the editor asks:
  - **Apply** merges it into the folder.
  - **Keep it aside** applies nothing and keeps the result in its fork;
    `molt apply FORK` in the folder applies it later.
  - **Discard** applies nothing and removes it.

  Nothing else applies a result: not a cancel, not a closed editor, not an
  answer the adapter does not know. A run that failed applies nothing and
  keeps what its best attempt did.
- **A question** an attempt asks (`ask_user`) comes as a permission prompt
  with its options, and **Let it decide**, which tells the attempt to decide
  and say what it assumed. The answer goes to that question alone.
- **Stop** in the editor (ACP's `session/cancel`) stops the run as Esc does
  in the interface: every command the attempts started ends, and what they
  had written is kept in their forks, which the message names. Nothing is
  applied. The next prompt starts the services again.
- Memory learns from a finished run, as it does in the interface, unless the
  budget is spent or the cost is not all known.

One task runs in a folder at a time: a prompt in another thread of the
same folder while one runs is refused until it ends or is stopped.

## What it does not do

`molt acp` can do what any client of [`molt serve`](serve.md) can, and no
more: the config, the services and their capabilities come from your config
when the folder's services start, never from the editor.

- It does not read or write files through the editor (ACP's `fs`
  methods), or run commands in the editor's terminal: the attempts work in
  their forks, in Molt's sandbox, and only an apply you allow writes to the
  folder.
- The MCP servers the editor offers are not used: tools come only from your
  own config ([MCP servers](mcp.md)).
- It has no login of its own (`authenticate` is a no-op) and cannot load a
  past session (`session/load`).

When the editor closes its side (it quits, or the thread goes), a task
still running is stopped as by a cancel, keeping its work, and every
folder's services stop; results you did not apply or discard are kept.
