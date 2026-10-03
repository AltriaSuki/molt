//! Capabilities: unforgeable, budgeted rights to send to one target.
//!
//! The table is an immutable snapshot behind [`ArcSwap`], so the hot path
//! (verify and charge on every message) never takes a lock. Budget counters
//! are atomics charged with compare-and-swap. Grants and revocations publish a
//! new snapshot under a writer lock.
//!
//! Only the kernel can grant from nothing ([`CapTable::grant`] is not reachable
//! over the bus). Services can only [`CapTable::delegate`], which carves the
//! child's budget out of the parent, so the total can never grow.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use molt_proto::{Budget, CapId, ServiceId, Target};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapError {
    #[error("no capability presented")]
    Missing,
    #[error("unknown capability")]
    Unknown,
    #[error("capability is held by another service")]
    NotHolder,
    #[error("capability does not cover {0}")]
    WrongTarget(Target),
    #[error("capability has expired")]
    Expired,
    #[error("capability was revoked")]
    Revoked,
    #[error("capability budget exhausted")]
    OverBudget,
    #[error("delegation would widen the capability: {0}")]
    Widening(&'static str),
}

#[derive(Debug)]
pub struct Capability {
    pub id: CapId,
    pub holder: ServiceId,
    pub target: Target,
    pub parent: Option<CapId>,
    pub expires_at: Option<SystemTime>,
    /// Longest reply deadline a message under this capability may ask for (0 = no limit).
    pub max_ms: u64,
    tokens: AtomicU64,
    calls: AtomicU64,
    revoked: AtomicBool,
}

impl Capability {
    /// What is left of the budget. `ms` is the per-message deadline limit.
    pub fn remaining(&self) -> Budget {
        Budget::new(self.tokens.load(Ordering::Acquire), self.max_ms, self.calls.load(Ordering::Acquire))
    }

    fn live(&self, now: SystemTime) -> Result<(), CapError> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(CapError::Revoked);
        }
        if self.expires_at.is_some_and(|t| t <= now) {
            return Err(CapError::Expired);
        }
        Ok(())
    }
}

/// Atomically take `amount` from `counter`, failing if it would go below zero.
fn take(counter: &AtomicU64, amount: u64) -> bool {
    counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| left.checked_sub(amount)).is_ok()
}

#[derive(Default)]
pub struct CapTable {
    snapshot: ArcSwap<HashMap<CapId, Arc<Capability>>>,
    writer: Mutex<()>,
}

impl CapTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a capability from nothing. Kernel-internal: driven by human
    /// configuration and approved manifests, never by a bus message.
    pub fn grant(&self, holder: ServiceId, target: Target, budget: Budget, ttl: Option<Duration>) -> Arc<Capability> {
        self.insert(Capability {
            id: CapId::random(),
            holder,
            target,
            parent: None,
            expires_at: ttl.map(|d| SystemTime::now() + d),
            max_ms: budget.ms,
            tokens: AtomicU64::new(budget.tokens),
            calls: AtomicU64::new(budget.calls),
            revoked: AtomicBool::new(false),
        })
    }

    fn insert(&self, cap: Capability) -> Arc<Capability> {
        let cap = Arc::new(cap);
        let _w = self.writer.lock().unwrap();
        let mut next = HashMap::clone(&self.snapshot.load());
        next.insert(cap.id.clone(), cap.clone());
        self.snapshot.store(Arc::new(next));
        cap
    }

    pub fn get(&self, id: &CapId) -> Option<Arc<Capability>> {
        self.snapshot.load().get(id).cloned()
    }

    /// Check that `sender` may send to `target` under `cap`. The capability
    /// and every ancestor must be live.
    pub fn verify(
        &self,
        cap: Option<&CapId>,
        sender: &ServiceId,
        target: &Target,
    ) -> Result<Arc<Capability>, CapError> {
        let snap = self.snapshot.load();
        let cap = snap.get(cap.ok_or(CapError::Missing)?).ok_or(CapError::Unknown)?;
        if &cap.holder != sender {
            return Err(CapError::NotHolder);
        }
        if !cap.target.covers(target) {
            return Err(CapError::WrongTarget(target.clone()));
        }
        let now = SystemTime::now();
        let mut cur = Some(cap);
        while let Some(c) = cur {
            c.live(now)?;
            cur = c.parent.as_ref().and_then(|p| snap.get(p));
        }
        Ok(cap.clone())
    }

    /// Charge `tokens` and `calls` against `cap`, all or nothing.
    pub fn charge(&self, cap: &Capability, tokens: u64, calls: u64) -> Result<(), CapError> {
        if !take(&cap.calls, calls) {
            return Err(CapError::OverBudget);
        }
        if !take(&cap.tokens, tokens) {
            cap.calls.fetch_add(calls, Ordering::AcqRel);
            return Err(CapError::OverBudget);
        }
        Ok(())
    }

    /// Return a charge for a message the kernel accepted but could not deliver.
    pub fn refund(&self, cap: &Capability, tokens: u64, calls: u64) {
        cap.tokens.fetch_add(tokens, Ordering::AcqRel);
        cap.calls.fetch_add(calls, Ordering::AcqRel);
    }

    /// Give `to` a narrower slice of a capability `sender` holds. The child's
    /// tokens and calls are moved out of the parent, so delegation can split
    /// a budget but never grow it.
    pub fn delegate(
        &self,
        parent: &CapId,
        sender: &ServiceId,
        to: ServiceId,
        target: Target,
        budget: Budget,
        ttl: Option<Duration>,
    ) -> Result<Arc<Capability>, CapError> {
        let p = self.verify(Some(parent), sender, &target)?;
        if p.max_ms != 0 && (budget.ms == 0 || budget.ms > p.max_ms) {
            return Err(CapError::Widening("deadline longer than the parent's"));
        }
        let expires_at = match (p.expires_at, ttl.map(|d| SystemTime::now() + d)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.charge(&p, budget.tokens, budget.calls)?;
        Ok(self.insert(Capability {
            id: CapId::random(),
            holder: to,
            target,
            parent: Some(p.id.clone()),
            expires_at,
            max_ms: budget.ms,
            tokens: AtomicU64::new(budget.tokens),
            calls: AtomicU64::new(budget.calls),
            revoked: AtomicBool::new(false),
        }))
    }

    /// Revoke a capability. Everything delegated from it stops working too.
    pub fn revoke(&self, id: &CapId) -> bool {
        match self.get(id) {
            Some(c) => {
                c.revoked.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// Revoke every capability held by `holder` (used when a service version is replaced).
    pub fn revoke_held_by(&self, holder: &ServiceId) -> Vec<CapId> {
        self.snapshot
            .load()
            .values()
            .filter(|c| &c.holder == holder && !c.revoked.swap(true, Ordering::AcqRel))
            .map(|c| c.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> ServiceId {
        ServiceId::new(s).unwrap()
    }

    fn t(s: &str) -> Target {
        s.parse().unwrap()
    }

    #[test]
    fn charge_is_all_or_nothing() {
        let table = CapTable::new();
        let cap = table.grant(sid("p"), t("m.x"), Budget::new(10, 0, 2), None);
        assert_eq!(table.charge(&cap, 20, 1), Err(CapError::OverBudget));
        assert_eq!(cap.remaining(), Budget::new(10, 0, 2), "a failed charge must not leak");
        table.charge(&cap, 5, 1).unwrap();
        table.charge(&cap, 5, 1).unwrap();
        assert_eq!(table.charge(&cap, 0, 1), Err(CapError::OverBudget));
    }

    #[test]
    fn concurrent_charges_never_overspend() {
        let table = Arc::new(CapTable::new());
        let cap = table.grant(sid("p"), t("m.x"), Budget::new(1000, 0, 1000), None);
        let ok = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..500 {
                        if table.charge(&cap, 1, 1).is_ok() {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(ok.load(Ordering::Relaxed), 1000);
        assert_eq!(cap.remaining(), Budget::new(0, 0, 0));
    }

    #[test]
    fn revoking_a_parent_revokes_children() {
        let table = CapTable::new();
        let parent = table.grant(sid("p"), t("m.*"), Budget::new(100, 1000, 10), None);
        let child = table.delegate(&parent.id, &sid("p"), sid("c"), t("m.x"), Budget::new(10, 500, 1), None).unwrap();
        assert!(table.verify(Some(&child.id), &sid("c"), &t("m.x")).is_ok());
        table.revoke(&parent.id);
        assert_eq!(table.verify(Some(&child.id), &sid("c"), &t("m.x")).unwrap_err(), CapError::Revoked);
    }
}
