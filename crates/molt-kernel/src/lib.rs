//! The Molt microkernel.
//!
//! Five parts and no judgment: a message bus ([`Kernel`]), capabilities
//! ([`caps`]), a process supervisor ([`supervisor`]), a module registry
//! ([`registry`]) and an append-only audit log ([`audit`]). There is no model
//! inside; everything that thinks is a service on the bus.
//!
//! Invariants, each covered by `tests/invariants.rs`:
//! 1. No service can grant itself a capability or raise its own budget.
//! 2. No service can promote a kernel or protected component.
//! 3. No service can delete or rewrite audit log entries.
//! 4. A failing service never takes the kernel down.

pub mod audit;
pub mod caps;
mod kernel;
pub mod registry;
pub mod supervisor;

pub use kernel::{Config, Kernel, KernelError, Launched, ServiceStatus, ENV_CAPS};
