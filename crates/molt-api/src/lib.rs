//! Payload shapes for the agent services on the Molt bus.
//!
//! The kernel never looks inside a payload; these types are the contract
//! between the services that send and serve them:
//!
//! | Service   | Methods                                    | Module        |
//! |-----------|--------------------------------------------|---------------|
//! | `model`   | `complete`                                 | [`model`]     |
//! | `fs`      | `read write edit list search fork diff merge drop` | [`fs`] |
//! | `shell`   | `run`                                      | [`shell`]     |
//! | `memory`  | `remember recall forget retract consolidate index map symbols` | [`memory`] |
//! | `planner` | `run`                                      | [`planner`]   |
//!
//! The planner also publishes [`progress::Progress`] events on
//! `topic:progress`, and the `fs` service [`fs::FilesChanged`] events on
//! `topic:fs.changed`.

pub mod fs;
pub mod memory;
pub mod model;
pub mod planner;
pub mod progress;
pub mod shell;
