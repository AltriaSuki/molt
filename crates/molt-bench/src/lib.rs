//! Molt's benchmark: runs Molt and a plain agent loop on the same coding
//! tasks, grades every run with tests the agents never see, and compares
//! success rate, time to done and cost per task. `bench/README.md` describes
//! the tasks and how to run it.

pub mod agent;
pub mod cli;
pub mod grade;
pub mod proc;
pub mod record;
pub mod report;
pub mod run;
pub mod sandbox;
pub mod stats;
pub mod task;

#[cfg(test)]
mod testing;
