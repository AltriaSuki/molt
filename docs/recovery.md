# Recovering a workspace apply

Every merge that writes files stages private, fsynced preimages and candidate
contents under `.molt/applies/apply-<id>` in the trusted scratch area before
its first workspace write. Its journal records preparing, staged, applying,
applied, undoing, undone or needs-recovery phases. `fs.merge` adds an optional
`operation` ID to its reply; failures name the same ID, and the planner keeps
the candidate and its existing exit status 3. `fs.applies` reports both the
last durable phase and each path's observed pending/applied/conflict state,
so a crash between a file rename and a journal update cannot hide a write.

With the default setup, use the project's data directory followed by `/work`
as `SCRATCH`. These host commands need neither a model key nor a running
kernel. Ordinary fs reads/writes cannot access this recovery data; service calls to `fs.applies` / `fs.recover` still go through the
kernel's normal capability checks and audit. Offline commands update the
same durable journal, without creating a bus audit record.

```sh
molt-tools apply --root /path/to/project --scratch /path/to/data/work list
molt-tools apply --root /path/to/project --scratch /path/to/data/work inspect apply-ID
molt-tools apply --root /path/to/project --scratch /path/to/data/work resume apply-ID
molt-tools apply --root /path/to/project --scratch /path/to/data/work undo apply-ID
```

Resume finishes the originally staged candidate. A source fork that still
exists must have the same fingerprint; changes are refused. If run cleanup
removed it, recovery uses the immutable, checksum-verified postimages instead.
Undo restores the exact old file contents,
symlink targets and permissions, including files deleted by the apply, and
removes added files. Undo remains available after a successful merge drops
its fork. Both actions are idempotent and refuse a path whose current bytes,
link target or permissions differ from both recorded states, preserving later
user edits. Incomplete staging has made no workspace writes: undo abandons
it; resume asks for a new merge. Corrupt staging blobs never replace a file.

A process lock serializes apply/recovery commands sharing a scratch directory.
Exclude other editors and commands that write the workspace or candidate
while applying or recovering: path checks detect edits between operations,
but cannot make check-and-rename atomic against arbitrary external writers.
The operation is a sequence of durable single-file renames/deletions, not an
atomic directory swap. Files/symlinks and their existing merge permission
semantics are supported; empty directory metadata and file-to-directory
replacements, ownership, hardlink identity and extended attributes are not tracked. Recovery materials are retained after success
for explicit undo; they consume disk space until the user removes a completed
operation directory after inspecting it and deciding that undo is no longer
needed. Deleting an unfinished record loses its recovery material.
