//! Bounded, disposable symbol caches. No fork data enters the durable database.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use molt_api::fs::{self, ViewRequest, ViewResponse};
use molt_api::memory::{ForkSymbolsRequest, ForkSymbolsResponse};
use molt_proto::{Budget, RemoteError, TraceId};

use crate::error::{failed, invalid};
use crate::{project, Db, Memory};

const MAX_VIEWS: usize = 8;

struct View {
    db: Arc<Db>,
    baseline: String,
    workspace: String,
    update: tokio::sync::Mutex<()>,
}

#[derive(Default)]
pub(crate) struct Views {
    entries: BTreeMap<PathBuf, (Arc<View>, Instant)>,
}

impl Memory {
    async fn fork_identity(&self, fork: &str, trace: &TraceId, budget: Budget) -> Result<ViewResponse, RemoteError> {
        let payload = serde_json::to_value(ViewRequest { fork: fork.into() }).map_err(|e| failed(e.to_string()))?;
        let value = self.bus.call(fs::VIEW, payload, budget, trace).await?;
        serde_json::from_value(value).map_err(|e| failed(format!("invalid fs.view response: {e}")))
    }

    pub(crate) async fn fork_symbols(
        &self,
        req: ForkSymbolsRequest,
        trace: &TraceId,
        budget: Budget,
    ) -> Result<ForkSymbolsResponse, RemoteError> {
        self.fork_symbols_inner(req, trace, budget).await.map_err(|mut e| {
            e.message.push_str("; the attempt symbol view is unavailable, use search/read_file in the fork");
            e
        })
    }

    async fn fork_symbols_inner(
        &self,
        req: ForkSymbolsRequest,
        trace: &TraceId,
        budget: Budget,
    ) -> Result<ForkSymbolsResponse, RemoteError> {
        let workspace = self.workspace_key(&req.query.workspace)?;
        let root = self.cfg.fork_root.as_ref().ok_or_else(|| invalid("memory has no trusted fork root"))?;
        let root = root.canonicalize().map_err(|e| failed(format!("fork root: {e}")))?;
        let before = self.fork_identity(&req.fork, trace, budget).await?;
        let path = Path::new(&before.fork);
        // fs checks its own metadata and base ownership. Independently bind that
        // identity to the caller's allowed workspace and our trusted scratch root.
        if before.workspace != workspace
            || path.parent() != Some(root.as_path())
            || path.canonicalize().ok().as_deref() != Some(path)
            || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        {
            return Err(invalid("the fork does not belong to this workspace and trusted scratch directory"));
        }
        let path = path.to_path_buf();
        let view = {
            let mut views = self.fork_views.lock().await;
            views.entries.retain(|p, _| p.is_dir());
            if views.entries.get(&path).is_some_and(|(v, _)| v.baseline != before.baseline || v.workspace != workspace)
            {
                views.entries.remove(&path);
            }
            if !views.entries.contains_key(&path) {
                if views.entries.len() >= MAX_VIEWS {
                    if let Some(oldest) =
                        views.entries.iter().min_by_key(|(_, (_, used))| *used).map(|(p, _)| p.clone())
                    {
                        views.entries.remove(&oldest);
                    }
                }
                let db = Arc::new(Db::in_memory().map_err(|e| failed(format!("creating fork symbol view: {e}")))?);
                let view = Arc::new(View {
                    db,
                    baseline: before.baseline.clone(),
                    workspace,
                    update: tokio::sync::Mutex::new(()),
                });
                views.entries.insert(path.clone(), (view, Instant::now()));
            }
            let (view, used) = views.entries.get_mut(&path).expect("the view was inserted");
            *used = Instant::now();
            view.clone()
        };
        let _turn = view.update.lock().await;
        let db = view.db.clone();
        let indexed_path = path.clone();
        let (symbols, index) = tokio::task::spawn_blocking(move || {
            let index = project::index_verified(&db, &indexed_path)?;
            let mut symbols = project::symbols(&db, &indexed_path, &req.query)?;
            symbols.truncated |= index.truncated;
            Ok::<_, RemoteError>((symbols, index))
        })
        .await
        .map_err(|e| failed(format!("refreshing fork symbols: {e}")))??;
        let after = self.fork_identity(&before.fork, trace, budget).await?;
        if after != before {
            return Err(failed("the fork changed while refreshing its symbols; retry after the edit finishes"));
        }
        Ok(ForkSymbolsResponse { symbols, view: after, parsed: index.parsed, skipped: index.skipped })
    }
}
