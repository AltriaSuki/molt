//! Wire types shared by the Molt kernel and its services.
//!
//! Everything that crosses a process boundary is defined here: the message
//! [`Envelope`], identifiers, budgets and service manifests. The kernel never
//! looks inside [`Envelope::payload`]; services own their payload shapes.

pub mod audit;
mod envelope;
mod ids;
mod manifest;
pub mod tool;

pub use envelope::{depth, nesting, Budget, Envelope, ErrorCode, Kind, RemoteError, MAX_DEPTH, MAX_ID};
pub use ids::{CapId, IdError, MsgId, ServiceId, Target, TraceId};
pub use manifest::{CapRequest, Exec, Manifest, Module, Tier, VersionId, MAX_MODULE_TEXT};

/// The reserved service id the kernel uses for its own endpoints and replies.
pub const KERNEL: &str = "kernel";
