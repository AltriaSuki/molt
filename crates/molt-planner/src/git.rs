//! `git` (#111): an attempt reads the git history of the user's repository,
//! which its fork does not have, through `fs.git`.
//!
//! The fs service reads the repository the attempt's workspace was copied
//! from, takes the commit its `HEAD` named when the fork was made as the
//! baseline, and changes nothing in it. The planner only shapes the
//! request and shows the answer. Off unless the planner's starter turns it
//! on (`MOLT_GIT=on`), so the built-in tool lists stay as they were.

use molt_api::fs::{self, GitQuery, GitRequest, GitResponse};
use molt_api::model::ToolUse;
use molt_api::planner::tools as t;
use molt_proto::{Budget, RemoteError};
use serde_json::{json, Value};

use crate::ctx::Ctx;

/// Deadline of one query: the service stops its own at 30 s.
const CALL_MS: u64 = 45_000;

/// The tool's definition, offered to attempts after the other built-ins.
pub(crate) fn definition() -> Value {
    let rev = |what: &str| json!({ "type": "string", "description": what });
    json!({
        "name": t::GIT,
        "description": "Read the git history of the user's repository, which your workspace does not have (it is a \
                        copy without .git). Use it to find out when and why code changed, such as the commit that \
                        introduced a regression, and what the user had changed before you started. It only reads: \
                        nothing in the repository changes. query is one of: status (the baseline commit your copy \
                        was made from, the user's changes not committed that your copy started with, and your own \
                        changes since); log (commits, newest first, with the files each changed; narrow it with \
                        rev, path and search); show (one commit's message and patch); diff (from a commit, by \
                        default the baseline, to another commit given as to, or without to to your files as they \
                        are now); blame (who last changed each line of a file as of a commit, by default the \
                        baseline). HEAD means the baseline, even when the user's HEAD has moved since your copy was \
                        made. Paths are relative to your workspace, and history outside it is not shown. An answer \
                        is cut at about 40 KB: narrow it with path, limit or lines.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "enum": ["status", "log", "show", "diff", "blame"] },
                "rev": rev("A commit: an id, a branch, a tag or HEAD~2. log also takes a range A..B, and starts \
                            from the baseline without one; show needs it."),
                "path": rev("A file or directory, relative to your workspace. blame needs a file."),
                "search": rev("log: only commits that added or removed this exact text."),
                "limit": { "type": "integer", "description": "log: most commits. Default 20, at most 100." },
                "from": rev("diff: the commit to compare from. Default the baseline."),
                "to": rev("diff: the commit to compare to. Without it, your files as they are now."),
                "start_line": { "type": "integer", "description": "blame: the first line, from 1." },
                "end_line": { "type": "integer", "description": "blame: the last line." },
            },
            "required": ["query"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// `tools` with `git` after them.
pub(crate) fn with_git(mut tools: Vec<Value>) -> Vec<Value> {
    tools.push(definition());
    tools
}

/// The query a call of the tool makes.
pub(crate) fn parse(call: &ToolUse) -> Result<GitQuery, String> {
    serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for {}: {e}. Check the tool's input schema and try again.", call.name))
}

/// What a progress line says the call does.
pub(crate) fn detail(query: &GitQuery) -> String {
    let about = |s: &[Option<&String>]| s.iter().flatten().map(|s| format!(" {s}")).collect::<String>();
    match query {
        GitQuery::Status => "git status".into(),
        GitQuery::Log { rev, path, search, .. } => {
            let search = search.as_ref().map(|s| format!("-S {s:?}"));
            format!("git log{}", about(&[rev.as_ref(), search.as_ref(), path.as_ref()]))
        }
        GitQuery::Show { rev, path } => format!("git show {rev}{}", about(&[path.as_ref()])),
        GitQuery::Diff { from, to, path } => format!("git diff{}", about(&[from.as_ref(), to.as_ref(), path.as_ref()])),
        GitQuery::Blame { path, rev, .. } => format!("git blame{} {path}", about(&[rev.as_ref()])),
    }
}

/// Make the query in `fork`, and the text for the model.
pub(crate) async fn run(ctx: &Ctx, fork: &str, query: GitQuery) -> Result<String, RemoteError> {
    let req = GitRequest { workspace: fork.to_owned(), query };
    let resp: GitResponse = ctx.call(fs::GIT, req, Budget::new(0, CALL_MS, 0)).await?;
    Ok(show(resp))
}

fn show(resp: GitResponse) -> String {
    let mut out = resp.output;
    if resp.truncated {
        out.push_str("\n[cut at the size limit: narrow the query with path, limit or lines]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool's definition as the model API gets it, one line of JSON.
    const PINNED: &str = include_str!("testdata/git_tool.txt");

    #[test]
    fn the_git_tool_is_pinned_strict_and_closed() {
        let def = definition();
        assert_eq!(serde_json::to_string(&def).unwrap(), PINNED.trim_end());
        assert_eq!(def["strict"], true);
        assert_eq!(def["input_schema"]["additionalProperties"], false);
        let before = vec![json!({ "name": "read_file" })];
        assert_eq!(with_git(before.clone())[..1], before[..]);
    }

    #[test]
    fn a_call_becomes_its_query_and_a_wrong_one_says_what_is_missing() {
        let call = |input: Value| ToolUse { id: "t".into(), name: t::GIT.into(), input };
        assert_eq!(parse(&call(json!({ "query": "status", "path": "x" }))).unwrap(), GitQuery::Status);
        let log = parse(&call(json!({ "query": "log", "search": "a + b", "limit": 5, "start_line": 3 }))).unwrap();
        assert_eq!(
            log,
            GitQuery::Log { rev: None, path: None, search: Some("a + b".into()), limit: Some(5) },
            "a field of another query is ignored"
        );
        assert_eq!(detail(&log), "git log -S \"a + b\"");
        let why = parse(&call(json!({ "query": "show" }))).unwrap_err();
        assert!(why.contains("missing field `rev`"), "{why}");
        assert!(parse(&call(json!({ "query": "push" }))).is_err());
        let cut = show(GitResponse { output: "a\n".into(), truncated: true, baseline: None });
        assert!(cut.ends_with("narrow the query with path, limit or lines]"), "{cut}");
    }
}
