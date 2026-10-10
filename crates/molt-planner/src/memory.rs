//! What memory knows about the workspace, gathered once when a run starts:
//! the project model brought up to date, a map of the code the task most
//! likely touches, and the notes learned from earlier tasks in it.
//!
//! Memory is optional. When the planner holds no capability for it, or the
//! service is down or does not answer, a run goes ahead without it.

use molt_api::memory::{
    self, IndexRequest, IndexResponse, MapRequest, MapResponse, RecallRequest, RecallResponse, Recalled,
};
use std::time::Duration;

use molt_api::progress::{Progress, RecalledNote};
use molt_proto::{Budget, ErrorCode};

use crate::ctx::Ctx;
use crate::prompts;

/// Deadline for bringing the project model up to date. The first index of a
/// large project takes the longest; later ones parse only what changed.
const INDEX_MS: u64 = 300_000;
/// Deadline for the map and the notes.
const LOOKUP_MS: u64 = 30_000;
/// Deadline for the first call, which shows whether memory answers at all.
const PROBE_MS: u64 = 10_000;
/// An update of the project model that takes longer than this is announced.
const SLOW_INDEX: Duration = Duration::from_secs(2);
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

/// Bring the project model up to date and gather the context for the run.
/// Failures are noted in the run's progress and never fail the run.
pub(crate) async fn prepare(ctx: &Ctx) -> Memory {
    let workspace = ctx.workspace.clone();
    // The notes first: a quick call, which shows whether memory answers at
    // all. One that does not would hold every later call to its deadline.
    let recall = RecallRequest {
        query: ctx.task.clone(),
        workspace: Some(workspace.clone()),
        k: Some(NOTES),
        min_confidence: Some(MIN_CONFIDENCE),
        ..Default::default()
    };
    let notes = match ctx.call::<RecallResponse>(memory::RECALL, recall, Budget::new(0, PROBE_MS, 0)).await {
        Ok(r) => r.notes,
        // Not configured, or not running: the run's starter says so if it matters.
        Err(e) if e.code == ErrorCode::Unavailable => {
            tracing::debug!(run = %ctx.trace, error = %e, "running without memory");
            return Memory::default();
        }
        // Configured, but it did not answer or refused (a capability set up wrong, say).
        Err(e) => {
            ctx.note(format!("running without memory: {}", e.message)).await;
            return Memory::default();
        }
    };

    let index = IndexRequest { workspace: workspace.clone(), paths: None };
    let indexing = ctx.call::<IndexResponse>(memory::INDEX, index, Budget::new(0, INDEX_MS, 0));
    tokio::pin!(indexing);
    let indexed = tokio::select! {
        r = &mut indexing => r,
        () = tokio::time::sleep(SLOW_INDEX) => {
            ctx.note("updating the project model; the first time, every source file is read").await;
            indexing.await
        }
    };
    match indexed {
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
        // A model that could not be brought up to date may still be mostly right.
        Err(e) => ctx.note(format!("project model not updated: {}", e.message)).await,
    }

    let map = MapRequest { workspace, query: ctx.task.clone(), max_tokens: Some(ctx.cfg.map_tokens) };
    let map = match ctx.call::<MapResponse>(memory::MAP, map, Budget::new(0, LOOKUP_MS, 0)).await {
        Ok(m) => Some(m),
        Err(e) => {
            ctx.note(format!("no project map: {}", e.message)).await;
            None
        }
    };
    let map = map.filter(|m| !m.map.trim().is_empty());
    if !notes.is_empty() || map.is_some() {
        let recalled = notes
            .iter()
            .map(|r| RecalledNote { id: r.note.id.clone(), text: r.note.text.clone(), confidence: r.note.confidence })
            .collect();
        let map_tokens = map.as_ref().map(|m| m.tokens);
        ctx.progress(Progress::Recalled { run: ctx.run_id(), notes: recalled, map_tokens }).await;
    }
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
