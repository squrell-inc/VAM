//! E2E scenarios: typed formats, the catalog check, and the seams the runner engine builds on.
//!
//! This crate never depends on `vam-orchestrator`, `vam-core` or `vam-runtime` (S5).

pub mod api;
pub mod bench;
pub mod catalog;
pub mod check;
pub mod docs;
pub mod driver;
pub mod engine;
pub mod events;
pub mod forms;
pub mod lint;
pub mod mock;
pub mod report;
pub mod run_cmd;
pub mod runner;
pub mod scenario;
pub mod usage;
pub mod vamd;
pub mod vmm_probe;
pub mod world;
