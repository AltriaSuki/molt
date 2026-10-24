# Declarative project skills

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
```

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
