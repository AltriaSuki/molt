//! Skill drafts, independent audited evaluations and human-controlled activation.
use crate::{
    error::{self, failed, invalid},
    Bus, Db, Writer,
};
use molt_api::planner::{Outcome, RunRequest, RunResponse};
use molt_api::skills::{Action, Evaluation, Request, Skill, Spec};
use molt_proto::{
    audit::{self, Logged, ReadRequest, ReadResponse},
    Budget, Kind, RemoteError, TraceId,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS skill_heads (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, name TEXT NOT NULL, active INTEGER, disabled INTEGER NOT NULL DEFAULT 0, UNIQUE(workspace,name));
CREATE TABLE IF NOT EXISTS skill_versions (id TEXT NOT NULL, rev INTEGER NOT NULL, snapshot TEXT NOT NULL, PRIMARY KEY(id,rev));
CREATE TABLE IF NOT EXISTS skill_events (seq INTEGER PRIMARY KEY, id TEXT NOT NULL, rev INTEGER, action TEXT NOT NULL, data TEXT NOT NULL, trace TEXT NOT NULL, created_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS skill_recalls (workspace TEXT NOT NULL, run TEXT NOT NULL, call TEXT NOT NULL, query TEXT NOT NULL, snapshot TEXT NOT NULL, PRIMARY KEY(workspace,run,call));
CREATE TABLE IF NOT EXISTS skill_uses (workspace TEXT NOT NULL, run TEXT NOT NULL, call TEXT NOT NULL, id TEXT NOT NULL, rev INTEGER NOT NULL, snapshot TEXT NOT NULL, PRIMARY KEY(workspace,run,call,id));
";

pub(crate) fn validate(spec: &Spec) -> Result<(), RemoteError> {
    if spec.name.trim().is_empty()
        || spec.name.len() > 100
        || spec.trigger_terms.is_empty()
        || spec.trigger_terms.len() > 16
        || spec.steps.is_empty()
        || spec.steps.len() > 16
        || spec.acceptance.trim().is_empty()
        || spec.acceptance.len() > 2000
        || spec.prerequisites.len() > 16
    {
        return Err(invalid("skill needs a name, 1–16 triggers and steps, and a bounded acceptance command"));
    }
    if spec.steps.iter().any(|s| s.trim().is_empty() || s.len() > 2000)
        || spec
            .trigger_terms
            .iter()
            .any(|s| s.is_empty() || s.len() > 100 || s.chars().any(|c| !c.is_alphanumeric() && c != '_'))
    {
        return Err(invalid("skill steps must be 1–2000 bytes and triggers single words"));
    }
    for (path, hash) in &spec.prerequisites {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("prerequisite needs SHA-256"));
        }
        if Path::new(path).is_absolute() || path.split('/').any(|p| matches!(p, "" | "." | ".." | ".git" | ".molt")) {
            return Err(invalid("prerequisite must be a plain project path"));
        }
    }
    Ok(())
}

/// Match request/reply identity and the kernel-stamped planner; text in a
/// tool result cannot stand in for a successful independently checked run.
pub(crate) fn proof(entries: &[Logged], run: &str, workspace: &str) -> Result<Evaluation, RemoteError> {
    let request = entries
        .iter()
        .find(|e| {
            e.envelope.kind == Kind::Request
                && e.envelope.to.to_string() == "planner.run"
                && e.envelope.trace_id.as_str() == run
                && e.omitted.is_none()
        })
        .ok_or_else(|| invalid("audit has no complete planner.run request"))?;
    let req: RunRequest = serde_json::from_value(request.envelope.payload.clone())
        .map_err(|_| invalid("audited run request is invalid"))?;
    if req.workspace != workspace {
        return Err(invalid("run belongs to another project"));
    }
    let reply = entries
        .iter()
        .find(|e| {
            e.envelope.kind == Kind::Reply
                && e.envelope.from.as_ref().map(|s| s.as_str()) == Some("planner")
                && e.envelope.reply_to.as_ref() == Some(&request.envelope.id)
                && e.omitted.is_none()
        })
        .ok_or_else(|| invalid("audit has no complete authenticated planner result"))?;
    let resp: RunResponse =
        serde_json::from_value(reply.envelope.payload.clone()).map_err(|_| invalid("audited run result is invalid"))?;
    Ok(Evaluation {
        run: run.into(),
        check: resp.check.as_ref().map(|c| c.command.clone()),
        passed: resp.outcome == Outcome::Passed
            && resp.check.is_some()
            && resp.review.as_ref().is_none_or(|r| r.verdict == molt_api::planner::Verdict::Approve),
        cost_usd: resp.cost_usd,
        uncounted_calls: resp.uncounted_calls,
        duration_ms: reply.ts_ms.saturating_sub(request.ts_ms),
        events: vec![request.envelope.id.to_string(), reply.envelope.id.to_string()],
    })
}

pub(crate) async fn audit_proof(
    bus: &dyn Bus,
    run: &str,
    workspace: &str,
    trace: &TraceId,
) -> Result<Evaluation, RemoteError> {
    if run.is_empty() || run.len() > 128 || !run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(invalid("invalid source run id"));
    }
    let id = TraceId::from_raw(run);
    let mut request = ReadRequest::new(id);
    request.max_bytes = Some(128 * 1024);
    let mut entries = Vec::new();
    for _ in 0..128 {
        let page = bus
            .call(
                audit::READ,
                serde_json::to_value(&request).expect("request serializes"),
                Budget::new(0, 5000, 0),
                trace,
            )
            .await?;
        let page: ReadResponse = serde_json::from_value(page).map_err(|e| failed(e.to_string()))?;
        entries.extend(page.entries);
        if page.next.is_none() {
            return proof(&entries, run, workspace);
        }
        if request.cursor == page.next {
            return Err(invalid("audit cursor did not advance"));
        }
        request.cursor = page.next;
    }
    Err(invalid("run exceeds the bounded skill evidence read; not approved"))
}

pub(crate) fn draft(
    db: &Db,
    workspace: &str,
    spec: Spec,
    source: Evaluation,
    writer: &Writer,
    trace: &str,
) -> Result<Skill, RemoteError> {
    validate(&spec)?;
    if !source.passed {
        return Err(invalid("skills are drafted only from a verified successful source run"));
    }
    let id =
        format!("skill_{}", &hex::encode(Sha256::digest(format!("{workspace}\0{}", spec.name.to_lowercase())))[..32]);
    db.with(|c| -> rusqlite::Result<_> {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO skill_heads(id,workspace,name) VALUES (?1,?2,?3)",
            params![id, workspace, spec.name.to_lowercase()],
        )?;
        let revision: i64 =
            tx.query_row("SELECT COALESCE(MAX(rev),0)+1 FROM skill_versions WHERE id=?1", [&id], |r| r.get(0))?;
        let skill = Skill {
            id: id.clone(),
            revision: revision as u64,
            spec,
            source_run: source.run,
            source_events: source.events,
            writer: writer.service.clone(),
            writer_version: writer.version.clone(),
            evaluations: vec![],
        };
        tx.execute(
            "INSERT INTO skill_versions VALUES (?1,?2,?3)",
            params![id, revision, serde_json::to_string(&skill).expect("skill serializes")],
        )?;
        tx.execute(
            "INSERT INTO skill_events(id,rev,action,data,trace,created_ms) VALUES (?1,?2,'draft',?3,?4,?5)",
            params![id, revision, serde_json::to_string(&skill).unwrap(), trace, crate::now_ms() as i64],
        )?;
        tx.commit()?;
        Ok(skill)
    })
    .map_err(error::db)
}

fn version(c: &rusqlite::Connection, workspace: &str, id: &str, revision: u64) -> rusqlite::Result<Skill> {
    if revision > i64::MAX as u64 {
        return Err(rusqlite::Error::InvalidParameterName("skill revision is too large".into()));
    }
    let text: String = c.query_row("SELECT v.snapshot FROM skill_versions v JOIN skill_heads h ON h.id=v.id WHERE h.workspace=?1 AND v.id=?2 AND v.rev=?3", params![workspace,id,revision as i64], |r| r.get(0))?;
    serde_json::from_str(&text).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))
}

pub(crate) fn ready(skill: &Skill) -> bool {
    let independent: Vec<_> = skill.evaluations.iter().filter(|e| e.run != skill.source_run).collect();
    if independent.iter().any(|e| !e.passed || e.check.as_deref() != Some(&skill.spec.acceptance)) {
        return false;
    }
    independent.iter().map(|e| &e.run).collect::<HashSet<_>>().len() >= 2
}

pub(crate) fn discover(
    db: &Db,
    workspace: &str,
    query: &str,
    identity: Option<(&str, &str)>,
) -> Result<Vec<Skill>, RemoteError> {
    let terms: HashSet<_> = query.split(|c: char| !c.is_alphanumeric() && c != '_').map(str::to_lowercase).collect();
    db.with(|c| -> rusqlite::Result<_> {
        let tx = c.transaction()?;
        if let Some((run,call)) = identity {
            let previous: Option<(String,String)> = tx.query_row("SELECT query,snapshot FROM skill_recalls WHERE workspace=?1 AND run=?2 AND call=?3",params![workspace,run,call],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((old_query,text)) = previous {
                if old_query != query { return Err(rusqlite::Error::InvalidParameterName("skill capture identity belongs to another query".into())); }
                let previous = serde_json::from_str(&text).map_err(|e|rusqlite::Error::InvalidParameterName(e.to_string()))?;
                tx.commit()?; return Ok(previous);
            }
        }
        let mut stmt = tx.prepare("SELECT v.snapshot FROM skill_heads h JOIN skill_versions v ON v.id=h.id AND v.rev=h.active WHERE h.workspace=?1 AND h.disabled=0 ORDER BY h.id LIMIT 1001")?;
        let texts: Vec<String> = stmt.query_map([workspace], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        if texts.len() > 1000 { return Err(rusqlite::Error::InvalidParameterName("more than 1000 active skills; disable unused skills".into())); }
        let mut found = Vec::new();
        for text in texts {
            let skill: Skill = serde_json::from_str(&text).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
            if ready(&skill) && skill.spec.trigger_terms.iter().all(|t| terms.contains(&t.to_lowercase())) && skill.spec.prerequisites.iter().all(|(path,hash)| crate::review::fingerprint(Path::new(workspace), path).is_ok_and(|actual| actual.eq_ignore_ascii_case(hash))) {
                if let Some((run,call)) = identity {
                    tx.execute("INSERT OR IGNORE INTO skill_uses VALUES (?1,?2,?3,?4,?5,?6)", params![workspace,run,call,skill.id,skill.revision as i64,text])?;
                }
                found.push(skill);
                if found.len() == 4 { break; }
            }
        }
        drop(stmt);
        if let Some((run,call)) = identity {
            tx.execute("INSERT INTO skill_recalls VALUES (?1,?2,?3,?4,?5)",params![workspace,run,call,query,serde_json::to_string(&found).unwrap()])?;
        }
        tx.commit()?; Ok(found)
    }).map_err(error::db)
}

pub(crate) async fn manage(
    db: &Db,
    bus: &dyn Bus,
    request: Request,
    writer: &Writer,
    trace: &TraceId,
) -> Result<Value, RemoteError> {
    let ws = request.workspace;
    match request.action {
        Action::Draft { spec, run } => {
            let source = audit_proof(bus, &run, &ws, trace).await?;
            serde_json::to_value(draft(db, &ws, spec, source, writer, trace.as_str())?).map_err(|e| failed(e.to_string()))
        }
        Action::Evaluate { id, revision, run } => {
            let skill = db.with(|c| version(c,&ws,&id,revision)).map_err(error::db)?;
            if run == skill.source_run { return Err(invalid("the source run cannot independently evaluate its own skill")); }
            let mut proof = audit_proof(bus, &run, &ws, trace).await?;
            proof.passed &= proof.check.as_deref() == Some(&skill.spec.acceptance);
            db.with(|c| -> rusqlite::Result<Value> {
                let tx = c.transaction()?;
                let mut skill = version(&tx,&ws,&id,revision)?;
                if !skill.evaluations.iter().any(|e| e.run == proof.run) { skill.evaluations.push(proof.clone()); }
                if skill.evaluations.len() > 32 { return Err(rusqlite::Error::InvalidParameterName("at most 32 evaluations per skill version".into())); }
                tx.execute("UPDATE skill_versions SET snapshot=?3 WHERE id=?1 AND rev=?2",params![id,revision as i64,serde_json::to_string(&skill).unwrap()])?;
                tx.execute("INSERT INTO skill_events(id,rev,action,data,trace,created_ms) VALUES (?1,?2,'evaluate',?3,?4,?5)",params![id,revision as i64,serde_json::to_string(&proof).unwrap(),trace.as_str(),crate::now_ms() as i64])?;
                tx.commit()?; Ok(json!(skill))
            }).map_err(error::db)
        }
        Action::Promote { id, revision } => change(db,&ws,&id,Some(revision),"promote","human promotion",trace.as_str()),
        Action::Disable { id, reason } => change(db,&ws,&id,None,"disable",&reason,trace.as_str()),
        Action::Rollback { id, revision, reason } => change(db,&ws,&id,Some(revision),"rollback",&reason,trace.as_str()),
        Action::List => db.read(|c| {
            let mut stmt = c.prepare("SELECT h.id,h.active,h.disabled,v.rev,v.snapshot FROM skill_heads h JOIN skill_versions v ON v.id=h.id WHERE h.workspace=?1 ORDER BY h.id,v.rev LIMIT 1000")?;
            let rows = stmt.query_map([ws], |r| Ok(json!({"id":r.get::<_,String>(0)?,"active":r.get::<_,Option<i64>>(1)?,"disabled":r.get::<_,bool>(2)?,"revision":r.get::<_,i64>(3)?,"skill":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?})))?.collect::<rusqlite::Result<Vec<_>>>().map(|v| json!(v));
            rows
        }).map_err(error::db),
        Action::Export { id } => db.read(|c| export(c, &ws, &id)).map_err(error::db),
        Action::Used { run } => db.read(|c| {
            let mut stmt = c.prepare("SELECT snapshot FROM skill_uses WHERE workspace=?1 AND run=?2 ORDER BY call,id")?;
            let rows = stmt.query_map(params![ws,run], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>().map(|v| json!(v.into_iter().filter_map(|s| serde_json::from_str::<Value>(&s).ok()).collect::<Vec<_>>()));
            rows
        }).map_err(error::db),
    }
}

/// The promoted revision of the enabled skill `id`, for writing it out as
/// a `SKILL.md`. Only what a person promoted after two independent
/// evaluations leaves memory this way: a draft, a disabled or rolled-back
/// skill, or a revision that no longer counts as evaluated is refused.
fn export(c: &rusqlite::Connection, ws: &str, id: &str) -> rusqlite::Result<Value> {
    let refuse = |why: &str| Err(rusqlite::Error::InvalidParameterName(why.into()));
    let head: Option<(Option<i64>, bool)> = c
        .query_row("SELECT active,disabled FROM skill_heads WHERE workspace=?1 AND id=?2", params![ws, id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((active, disabled)) = head else { return refuse("this project has no skill with that id") };
    let Some(active) = active.filter(|_| !disabled) else {
        return refuse(
            "only a promoted, enabled skill is exported: evaluate it on two independent runs and promote it first",
        );
    };
    let skill = version(c, ws, id, active as u64)?;
    if !ready(&skill) {
        return refuse("the promoted revision lacks two independent successful evaluations");
    }
    Ok(json!(skill))
}

fn change(
    db: &Db,
    ws: &str,
    id: &str,
    revision: Option<u64>,
    action: &str,
    reason: &str,
    trace: &str,
) -> Result<Value, RemoteError> {
    if reason.trim().is_empty() || reason.len() > 2000 {
        return Err(invalid("skill state change needs a reason of 1–2000 bytes"));
    }
    db.with(|c| -> rusqlite::Result<Value> {
        let tx = c.transaction()?;
        let current: Option<i64> =
            tx.query_row("SELECT active FROM skill_heads WHERE workspace=?1 AND id=?2", params![ws, id], |r| r.get(0))?;
        if let Some(rev) = revision {
            let skill = version(&tx, ws, id, rev)?;
            if !ready(&skill) {
                return Err(rusqlite::Error::InvalidParameterName(
                    "promotion/rollback requires two independent successful audited evaluations".into(),
                ));
            }
            tx.execute(
                "UPDATE skill_heads SET active=?3,disabled=1 WHERE workspace=?1 AND id=?2",
                params![ws, id, rev as i64],
            )?;
            // Only an explicit human promotion enables a version; rollback is sticky-disabled.
            if action == "promote" {
                tx.execute("UPDATE skill_heads SET disabled=0 WHERE id=?1", [id])?;
            }
        } else {
            tx.execute("UPDATE skill_heads SET disabled=1 WHERE workspace=?1 AND id=?2", params![ws, id])?;
        }
        if action == "rollback" || action == "disable" {
            let runs: Vec<String> = if let Some(current) = current {
                let mut stmt =
                    tx.prepare("SELECT DISTINCT run FROM skill_uses WHERE workspace=?1 AND id=?2 AND rev=?3")?;
                let rows = stmt.query_map(params![ws, id, current], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                rows
            } else {
                Vec::new()
            };
            crate::notes::void_runs(&tx, &runs, trace, crate::now_ms())?;
        }
        tx.execute(
            "INSERT INTO skill_events(id,rev,action,data,trace,created_ms) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, revision.map(|r| r as i64), action, reason, trace, crate::now_ms() as i64],
        )?;
        tx.commit()?;
        Ok(json!({"id":id,"revision":revision,"action":action,"disabled":action != "promote"}))
    })
    .map_err(error::db)
}

pub(crate) fn retract_in(
    tx: &rusqlite::Transaction,
    service: &str,
    version: &str,
    trace: &str,
    now: u64,
) -> rusqlite::Result<()> {
    let mut stmt=tx.prepare("SELECT h.id,h.workspace,h.active FROM skill_heads h JOIN skill_versions v ON v.id=h.id AND v.rev=h.active WHERE h.disabled=0 AND json_extract(v.snapshot,'$.writer')=?1 AND json_extract(v.snapshot,'$.writer_version')=?2")?;
    let heads: Vec<(String, String, i64)> = stmt
        .query_map(params![service, version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, ws, rev) in heads {
        let mut uses = tx.prepare("SELECT DISTINCT run FROM skill_uses WHERE workspace=?1 AND id=?2 AND rev=?3")?;
        let runs: Vec<String> = uses.query_map(params![ws, id, rev], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        crate::notes::void_runs(tx, &runs, trace, now)?;
        tx.execute("UPDATE skill_heads SET disabled=1 WHERE id=?1", [&id])?;
        tx.execute(
            "INSERT INTO skill_events(id,rev,action,data,trace,created_ms) VALUES (?1,?2,'retract_writer',?3,?4,?5)",
            params![id, rev, format!("{service}@{version}"), trace, now as i64],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evaluation(run: &str, passed: bool) -> Evaluation {
        Evaluation {
            run: run.into(),
            check: Some("cargo test".into()),
            passed,
            cost_usd: 0.2,
            uncounted_calls: 0,
            duration_ms: 10,
            events: vec!["r".into(), "s".into()],
        }
    }
    fn spec() -> Spec {
        Spec {
            name: "Fix parser".into(),
            trigger_terms: vec!["parser".into()],
            prerequisites: Default::default(),
            steps: vec!["Locate the parser".into(), "Add a test and repair it".into()],
            acceptance: "cargo test".into(),
        }
    }
    #[test]
    fn only_an_authenticated_matching_planner_result_is_evaluation_evidence() {
        use molt_proto::{CapId, Envelope, ServiceId};
        let trace = TraceId::random();
        let req = Envelope::request(
            trace.clone(),
            "planner.run".parse().unwrap(),
            CapId::random(),
            serde_json::to_value(RunRequest::new("repair parser", "/w")).unwrap(),
        );
        let mut result=req.reply(json!({"outcome":"passed","check":{"command":"cargo test","files":[],"designed":true},"summary":"done","changes":[],"patch":"","applied":true,"attempts":[],"usage":{},"cost_usd":0.2}));
        result.from = Some(ServiceId::new("planner").unwrap());
        let entries = vec![
            Logged { seq: 1, ts_ms: 1, envelope: req, omitted: None },
            Logged { seq: 2, ts_ms: 11, envelope: result, omitted: None },
        ];
        let found = proof(&entries, trace.as_str(), "/w").unwrap();
        assert!(found.passed);
        assert_eq!(found.duration_ms, 10);
        assert!(proof(&entries, trace.as_str(), "/other").is_err());
        let mut forged = entries.clone();
        forged[1].envelope.from = Some(ServiceId::new("model").unwrap());
        assert!(proof(&forged, trace.as_str(), "/w").is_err());
        forged[1] = entries[1].clone();
        forged[1].omitted = Some(512);
        assert!(proof(&forged, trace.as_str(), "/w").is_err());
    }

    #[test]
    fn drafts_need_independent_evidence_and_disable_cannot_be_undone_by_learning() {
        let db = Db::in_memory().unwrap();
        let writer = Writer { service: "memory".into(), version: "v1".into() };
        let mut s = draft(&db, "/w", spec(), evaluation("source", true), &writer, "draft").unwrap();
        assert!(discover(&db, "/w", "parser", Some(("before", "empty"))).unwrap().is_empty());
        assert!(change(&db, "/w", &s.id, Some(1), "promote", "manual", "t").is_err());
        s.evaluations = vec![evaluation("source", true), evaluation("one", true), evaluation("two", false)];
        assert!(!ready(&s));
        s.evaluations.push(evaluation("three", true));
        assert!(!ready(&s), "a failed independent evaluation keeps this revision a draft");
        s.evaluations = vec![evaluation("one", true), evaluation("three", true)];
        assert!(ready(&s));
        db.with(|c| {
            c.execute(
                "UPDATE skill_versions SET snapshot=?2 WHERE id=?1",
                params![s.id, serde_json::to_string(&s).unwrap()],
            )
        })
        .unwrap();
        let exported = |id: &str| db.read(|c| export(c, "/w", id));
        assert!(exported(&s.id).is_err(), "an evaluated draft is not exported before a person promotes it");
        change(&db, "/w", &s.id, Some(1), "promote", "manual", "t").unwrap();
        assert_eq!(serde_json::from_value::<Skill>(exported(&s.id).unwrap()).unwrap(), s);
        assert!(exported("skill_none").is_err());
        assert_eq!(discover(&db, "/w", "repair parser", Some(("use", "call"))).unwrap().len(), 1);
        assert!(discover(&db, "/w", "parser", Some(("before", "empty"))).unwrap().is_empty());
        assert!(discover(&db, "/other", "parser", None).unwrap().is_empty());
        assert!(discover(&db, "/w", "repair database", None).unwrap().is_empty());
        let learned = crate::notes::NewNote {
            kind: molt_api::memory::NoteKind::Lesson,
            text: "Use parser technique.".into(),
            workspace: Some("/w".into()),
            confidence: 0.8,
            provenance: molt_api::memory::Provenance {
                trace: "use".into(),
                events: vec![],
                service: "memory".into(),
                version: "v1".into(),
            },
        };
        let (learned, _) = crate::notes::remember(&db, learned, 1).unwrap();
        change(&db, "/w", &s.id, None, "disable", "unsafe", "t").unwrap();
        assert!(exported(&s.id).is_err(), "a disabled skill is not exported");
        let gone = crate::recall(
            &db,
            &molt_api::memory::RecallRequest { workspace: Some("/w".into()), ..Default::default() },
            None,
            None,
        )
        .unwrap();
        assert!(gone.iter().all(|n| n.note.id != learned.id));
        let evidence: i64 = db
            .with(|c| c.query_row("SELECT COUNT(*) FROM evidence WHERE note=?1", [learned.id], |r| r.get(0)))
            .unwrap();
        assert!(evidence > 0);
        draft(&db, "/w", spec(), evaluation("newsource", true), &writer, "draft2").unwrap();
        assert!(discover(&db, "/w", "parser", None).unwrap().is_empty());
        let old: String =
            db.with(|c| c.query_row("SELECT snapshot FROM skill_uses WHERE run='use'", [], |r| r.get(0))).unwrap();
        assert_eq!(serde_json::from_str::<Skill>(&old).unwrap(), s);
    }
    #[test]
    fn prerequisites_are_checked_before_reusing_a_workflow() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().to_str().unwrap();
        std::fs::write(root.path().join("config"), "old").unwrap();
        let db = Db::in_memory().unwrap();
        let writer = Writer { service: "memory".into(), version: "v1".into() };
        let mut specification = spec();
        specification.prerequisites.insert("config".into(), hex::encode(Sha256::digest(b"old")));
        let mut s = draft(&db, ws, specification, evaluation("source", true), &writer, "t").unwrap();
        s.evaluations = vec![evaluation("one", true), evaluation("two", true)];
        db.with(|c| {
            c.execute(
                "UPDATE skill_versions SET snapshot=?2 WHERE id=?1",
                params![s.id, serde_json::to_string(&s).unwrap()],
            )
        })
        .unwrap();
        change(&db, ws, &s.id, Some(1), "promote", "manual", "t").unwrap();
        assert_eq!(discover(&db, ws, "parser", None).unwrap().len(), 1);
        std::fs::write(root.path().join("config"), "new").unwrap();
        assert!(discover(&db, ws, "parser", None).unwrap().is_empty());
    }
}
