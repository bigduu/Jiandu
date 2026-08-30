//! Unreleased, compatibility-free port of Bamboo's current `memory/v1` store.
//!
//! The implementation deliberately contains only deterministic filesystem
//! memory behavior. Model reranking, Dream, MCP, and historical formats remain
//! outside this crate.

mod atomic_fs;
mod project_id;

pub mod memory_store;

pub use project_id::{InvalidProjectId, ProjectId};
