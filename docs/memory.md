# Memory

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

`find_symbol` uses a disposable symbol view of its own attempt fork, including
file-tool and shell changes. Memory asks `fs.view` to verify the fork's original
workspace and content version before and after refreshing. Every source file
is re-hashed, while only changed contents are parsed again, so a script that
preserves size and mtime cannot leave stale symbols. Replies identify the
baseline and current version; a concurrent edit or failed refresh returns an
explicit instruction to use `search`/`read_file`. Each view has an independent
in-memory database, with at most eight cached views; fork symbols never enter
the persistent project/notes database. The default setup grants only `fs.view`
to memory for this purpose. Custom configs must set `molt-memory --forks` to
the same trusted absolute scratch directory used by fs, and grant the planner
`memory.fork_symbols`; ordinary memory workspace access remains confined to
`--root`. Cold views index the whole fork; eviction or service restart rebuilds
them on the next query. Oversized, binary and unsupported files remain outside
the symbol model, and responses expose skipped/truncated results.


**Notes** are one-sentence facts, conventions, decisions, preferences and
lessons. After a run, memory reads it back from the audit log, boils it down
to numbered steps, and has the model write what would help a later task, each
note citing the steps, and so the logged messages, it rests on. The model
answers by calling a `submit_notes` tool whose input schema is the answer's
shape, so learning does not depend on the endpoint enforcing a JSON schema;
an answer in plain JSON text is accepted too. An answer that does not fit is
sent back once with the error, and if the second does not fit either the run
is not learned from and the reason is shown; both calls are counted in the
learning cost. A note the run
bore out gains confidence; one it proved wrong is kept beside its correction,
both less certain, until more evidence settles it. Notes carry the version
of the service that wrote them, so rolling a version back retracts what it
learned. Recall ranks notes by keyword match (SQLite FTS5 with stemming),
confidence and recency. Chinese, Japanese and Korean, written without spaces
between words, are matched on overlapping pairs of characters, so `测试` finds
a note that says `测试命令是 cargo test`.

Every context or `recall` tool lookup the planner uses records an immutable
snapshot: note IDs and revisions, original text, source runs and audit message
IDs, the query, and keyword relevance, confidence and recency used for ranking.
Later reinforcement, correction, forgetting or version retraction does not
rewrite that run's view. A run that recalls notes prints its ID on stderr;
`memory used` inspects it, scoped to the current project. Older runs have no
snapshot and are reported as such rather than reconstructed from today's notes.

```sh
molt memory show [WORDS...]          # notes about the project, best first
molt memory forget NOTE_ID --reason "the build moved to make"
molt memory map [WORDS...]           # the map a run with that task would start with
molt memory used RUN_ID --json       # the notes actually returned for this run
molt memory review NOTE_ID --rev 2 --reason "checked the build command" --depends-on Cargo.toml
molt memory correct NOTE_ID "Tests use pnpm test." --rev 3 --reason "the command changed" --depends-on package.json
```

`forget` goes through the kernel, so the audit log records it; the note stays
as a tombstone with its reason.

`show` includes revisions, sources, recall reasons and notes awaiting review.
`review` and `correct` go through the kernel too, require a reason and the
revision printed by `show`, and reject concurrent changes. Corrections keep
the original note and evidence as a tombstone and link the replacement to it.
Forgotten, corrected, retracted and undone text cannot be automatically relearned.

Use repeatable `--depends-on` arguments to attach project-relative files,
including build configuration or a symbol's source file. These are explicit
file dependencies, not inferred semantic symbol dependencies. Each review
replaces the dependency list and records SHA-256 content fingerprints. A
change, deletion or unreadable dependency marks only its linked notes as
awaiting review; ordinary recall excludes them. Checks run before recall and
when `fs.changed` arrives, including configuration files outside the source
index. Restoring a file does not reactivate a note: confirm it with `review`
using its current revision and dependency list. Unrelated changes leave it
alone. Files are limited to 1 MiB, at most 16 per note; symlinks, paths outside
the project and Molt's own data are refused.

### Your notes, for every project

Notes about you, such as "Answer in Chinese.", belong to no project. They
live in your own layer, `user.db` next to the project registry
(`$XDG_DATA_HOME/molt`, else `~/.local/share/molt`), and every project's
recall merges them with its own, best first. Each recalled note says which
layer it came from, in `show`, on the memory screen and in a run's snapshot.

```sh
molt memory remember --global "Answer in Chinese."    # yours, everywhere
molt memory remember "Deploy with make release."      # this project's
molt memory promote NOTE_ID --reason "every project of mine builds with make"
```

Writing one goes through the kernel and is audited. Learning never writes
there: a run's record holds text the project chose. When the model thinks a
learned note holds everywhere, it only proposes it (`proposed for every
project` in `show`, and in what a run reports learning); `promote` copies it
once you say why. The project note stays, marked with where it went, and the
copy keeps its evidence, so rolling back the version that learned it retracts
the copy too. `forget` and `correct` work on your notes from any project.
Project databases from before need no migration.

`review --temporary` marks a workaround as temporary experience, and the
model is explicitly told to verify it before reuse. Review metadata does not
change the original learning evidence. All these commands accept `--workspace`
and `--data-dir`; `show` and `used` also accept `--json`. The database upgrades
from schema 1 to schema 2 without rewriting existing notes; older binaries
refuse the newer schema.

## Offline semantic notes (optional)

Build with `cargo build --workspace --features molt/semantic`. In a trusted
memory-service configuration, add `--embedding-model /outside/project/model.json`
and `--embedding-sha256 HASH` to `molt-memory`'s arguments. No model is downloaded;
its SHA-256 is required and checked before parsing. The model is a data-only JSON
export of pretrained word vectors, with `format: "mean-word-vectors-v1"`,
`dimensions` (1–1024), and a `vectors` map of lowercase words to finite arrays.
Mean pooling and cosine similarity run locally on CPU. This first backend does
not run MiniLM/ONNX, understand word order or supply a pretrained vocabulary.
Use a licensed, reviewed vocabulary suitable for the project's languages.

`--semantic-weight` (default 0.5) mixes normalized FTS5 relevance and cosine
similarity; `--semantic-min-similarity` (default 0.4) admits non-keyword matches.
Confidence and recency retain their weights. Empty/OOV queries keep lexical
behavior. Without the feature/configuration, recall is unchanged. Snapshot
reasons record lexical relevance, similarity, weight and model SHA-256, so old
snapshots retain the exact evidence even after correction or forgetting.

SQLite caches note vectors by model and text hashes. Writes/corrections refresh
the derived cache, and recall retries invalid or stale entries. Forgotten,
retracted and review-pending notes stay excluded before semantic selection.
Model input is bounded to 64 MiB, 200000 vocabulary words and 1024 dimensions;
semantic databases are limited to 10000 live notes. Changing the model hash
keeps old snapshot evidence and reindexes the new model. No additional key or
network access is used. The lightweight backend should be compared against
keyword-only recall on real tasks before claiming a success-rate improvement;
fixture synonym recall demonstrates functionality, not production quality.
