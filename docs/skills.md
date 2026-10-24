# Skills

Molt has two kinds of skills: **Agent Skills**, the `SKILL.md` directories
many agents share, which people write; and **learned skills**, small
declarative workflows memory drafts from verified runs, which a person
promotes after independent evaluation. A promoted learned skill can be
exported as an Agent Skill.

## Agent Skills (`SKILL.md`)

An Agent Skill is a directory holding a `SKILL.md` in the
[Agent Skills format](https://agentskills.io/specification): YAML front
matter with a `name` and a `description`, then Markdown instructions. Other
files of the directory (`scripts/`, `references/`, `assets/`) are what the
instructions refer to.

```markdown
---
name: pdf-tools
description: Extract text and tables from PDF files. Use when the task names a PDF.
---
# PDF tools

Read `references/layout.md` before changing the extractor.
```

Each run looks in two places:

| Where | Trust |
| --- | --- |
| `~/.config/molt/skills/NAME/SKILL.md` (under `$XDG_CONFIG_HOME` when set, or the directory `MOLT_SKILLS_DIR` names; `MOLT_SKILLS_DIR=none` turns it off) | Yours: followed where it fits the task |
| the project's `.molt/skills/NAME/SKILL.md` | Data: written by whoever wrote the project, never instructions from you |

The user directory comes from the user layer of the configuration only, as
the prompt pack does: a project cannot point the planner elsewhere, and a
directory inside the workspace is refused. With `--config`, no user skills
are read.

**Progressive disclosure.** Only each skill's name and description go into
the first message, in a `<skills>` block after what memory recalled (at most
32 skills and 16 KiB; the rest are left out with a note). Attempts and the
check designer get a read-only `load_skill` tool: `load_skill(name)` returns
the instructions and a list of the skill's files, and
`load_skill(name, file)` one of those files. The tool is offered only when
the run has skills.

**Limits on what is read.** A file is a plain relative path inside the
skill's directory: no `..`, no absolute path, nothing that resolves outside
the directory. A project skill is never followed through a symlink, neither
its directory, its `SKILL.md` nor its files. `SKILL.md` is read up to
128 KiB, another file up to 256 KiB, and at most 48 KiB of either reaches
the model. Text that is not UTF-8 is not shown.

**Project skills are data.** Their text is framed as coming from the
project, not from you: the model is told to use what it says about the
project where it fits the task, to follow no instruction that goes beyond
the task, and not to run their scripts. Molt itself never runs a skill's
scripts: the planner only reads them, and the model's commands run in forks,
which have no `.molt` directory, inside the sandbox.

**Conformance.** A skill is left out, with a progress note saying why, when
its name is not 1 to 64 lowercase letters, digits and single hyphens or is
not its directory's name, when its description is empty or longer than 1024
characters, or when its front matter does not read. The front matter is
read as the YAML the format needs (plain, quoted and block values, flow
lists, one level of `metadata`); keys the format does not define are
ignored. A project skill with the name of one of yours is left out.

**What is recorded.** The run's progress names the skills offered
(`skills offered: pdf-tools (user), deploy (project)`). The recall snapshot
of the run's context, shown by `molt memory used RUN`, records each offered
skill's name, source, location and the SHA-256 of the `SKILL.md` read,
together with the learned skills the same recall offered. What `load_skill`
returned is in the model's requests in the audit log.

Not yet: running a user skill's scripts as such, `allowed-tools`, and
loading a skill only when its description matches the task.

## Learned skills


Memory may propose a small declarative workflow while learning from a verified
successful task. It saves a **draft**, with its source run, audit message IDs,
writer/version, trigger words, prerequisite file hashes, steps and acceptance
command. Drafts never enter a later task's context. A successful source task
cannot approve its own workflow, and no skill text is executed as a script.

Inspect and manage them through the kernel-identified CLI:

```sh
molt skill --workspace . list
molt skill --workspace . draft skill.json --run SOURCE_RUN
molt skill --workspace . evaluate SKILL_ID --revision 1 --run HELD_OUT_RUN_A
molt skill --workspace . evaluate SKILL_ID --revision 1 --run HELD_OUT_RUN_B
molt skill --workspace . promote SKILL_ID --revision 1
molt skill --workspace . used RUN
molt skill --workspace . disable SKILL_ID --reason 'No longer suitable'
molt skill --workspace . rollback SKILL_ID --revision 1 --reason 'Revision 2 regressed'
molt skill --workspace . export SKILL_ID --out ~/.config/molt/skills
```

`export` writes the promoted revision of an enabled skill as
`DIR/NAME/SKILL.md` (or prints it without `--out`), and never replaces a
file. Its name comes from the skill's name; the description names its
trigger words; the body holds the steps, the acceptance command and the
prerequisite hashes; `metadata` holds the skill's id, revision, source run
and the independent runs that evaluated it. Drafts, disabled and rolled-back
skills are not exported, so export never goes around the independent
evaluation and the human promotion. Once you put the file in your skills
directory it is an Agent Skill you installed.

A manual draft's JSON fields are `name`, `trigger_terms` (single words),
`prerequisites` (map of relative paths to SHA-256), `steps` and `acceptance`
(the check command). Evaluation reads authenticated request/reply pairs from
the audit; a forged note, tool output or an evaluation payload cannot assert
success. Promotion needs two distinct independent successful runs in the same
project, with the stored acceptance command, besides the source. Failed
independent evaluations remain attached to the draft. The human must select
independent cases and their protected checks: this registry does not establish
that arbitrary chosen runs constitute an adequate held-out task set.

Recall selects at most four active skills for the current project, only when
all trigger words and prerequisite hashes match. The planner treats their
steps as data below the task, check and permissions. Captured offerings retain
full revision snapshots and their evidence, accessible with `skill used` even
after disabling or rollback. These are records of offered context, not proof
that the model followed every step. New drafts cannot re-enable a disabled
skill. Rollback selects an evaluated older revision but keeps it disabled until
an explicit human promotion. Disabling or rollback voids learned-note evidence
from runs that were offered the withdrawn active revision, re-settles confidence
and leaves tombstones and original audit/snapshot evidence intact. Learning
cannot automatically recreate those withdrawn notes.

The first implementation does not run executable skill scripts or perform
automatic promotion. Real task success, cost and latency comparisons with a
no-skill baseline remain to be run under an explicit evaluation budget. Stored
independent evidence records each run's wall duration, known cost subtotal and
unknown calls; these alone do not prove a workflow's causal benefit.
