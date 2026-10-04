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
//! | `planner` | `run`                                      | [`planner`]   |
//!
//! The planner also publishes [`progress::Progress`] events on
//! `topic:progress`.

pub mod fs;
pub mod model;
pub mod planner;
pub mod progress;
pub mod shell;
