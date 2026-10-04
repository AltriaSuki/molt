//! What memory knows about the workspace, gathered once when a run starts:
//! the project model brought up to date, a map of the code the task most
//! likely touches, and the notes learned from earlier tasks in it.
//!
//! Memory is optional. When the planner holds no capability for it, or the
//! service is down, a run goes ahead without it.

use molt_api::memory::{
    self, IndexRequest, IndexResponse, MapRequest, MapResponse, RecallRequest, RecallResponse, Recalled,
};
use molt_proto::{Budget, ErrorCode, RemoteError};

use crate::ctx::Ctx;
use crate::prompts;

/// Deadline for bringing the project model up to date. The first index of a
/// large project takes the longest; later ones parse only what changed.
const INDEX_MS: u64 = 300_000;
/// Deadline for the map and the notes.
const LOOKUP_MS: u64 = 30_000;
/// Notes shown at the start of a run.
const NOTES: u32 = 12;
/// Notes less certain than this are left out of the start of a run.
const MIN_CONFIDENCE: f64 = 0.25;

/// Memory as a run sees it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Memory {
    /// Memory answers: offer the model its tools.
    pub up: bool,
    /// The `<project_context>` block for the first message, when there is
    /// anything to put in it.
    pub context: Option<String>,
}

/// Whether an error means memory is not there at all, rather than that one
/// call failed.
fn absent(e: &RemoteError) -> bool {
    matches!(e.code, ErrorCode::Unavailable | ErrorCode::Denied)
}

/// Bring the project model up to date and gather the context for the run.
/// Failures are noted in the run's progress and never fail the run.
pub(crate) async fn prepare(ctx: &Ctx) -> Memory {
    let workspace = ctx.workspace.clone();
    let index = IndexRequest { workspace: workspace.clone(), paths: None };
    match ctx.call::<IndexResponse>(memory::INDEX, index, Budget::new(0, INDEX_MS, 0)).await {
        Ok(r) => {
            let mut line = format!(
                "project model: {} files, {} definitions ({} parsed again, {} ms)",
                r.files, r.symbols, r.parsed, r.ms
            );
            if r.truncated {
                line.push_str("; the project has more files than the model holds");
            }
            ctx.note(line).await;
        }
        Err(e) if absent(&e) => {
            tracing::debug!(run = %ctx.trace, error = %e, "running without memory");
            return Memory::default();
        }
        // A model that could not be brought up to date may still be mostly right.
        Err(e) => ctx.note(format!("project model not updated: {}", e.message)).await,
    }

    let map =
        MapRequest { workspace: workspace.clone(), query: ctx.task.clone(), max_tokens: Some(ctx.cfg.map_tokens) };
    let lookup = Budget::new(0, LOOKUP_MS, 0);
    let map = match ctx.call::<MapResponse>(memory::MAP, map, lookup).await {
        Ok(m) => Some(m),
        Err(e) => {
            ctx.note(format!("no project map: {}", e.message)).await;
            None
        }
    };
    let recall = RecallRequest {
        query: ctx.task.clone(),
        workspace: Some(workspace),
        k: Some(NOTES),
        min_confidence: Some(MIN_CONFIDENCE),
        ..Default::default()
    };
    let notes = match ctx.call::<RecallResponse>(memory::RECALL, recall, lookup).await {
        Ok(r) => r.notes,
        Err(e) => {
            ctx.note(format!("no notes recalled: {}", e.message)).await;
            Vec::new()
        }
    };
    if !notes.is_empty() {
        ctx.note(format!("recalled {} notes from earlier tasks", notes.len())).await;
    }
    let map = map.filter(|m| !m.map.trim().is_empty());
    Memory { up: true, context: prompts::project_context(&notes, map.as_ref().map(|m| m.map.as_str())) }
}

/// The notes as the model is shown them, most relevant first.
pub(crate) fn note_lines(notes: &[Recalled]) -> String {
    notes
        .iter()
        .map(|r| {
            let n = &r.note;
            let disputed = if n.conflicts.is_empty() { "" } else { ", disputed" };
            format!("- [{} {:.2}{disputed}] {}", n.kind.as_str(), n.confidence, n.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
