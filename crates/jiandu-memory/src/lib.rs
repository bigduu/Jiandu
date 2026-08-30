//! Filesystem-backed memory persistence and deterministic lexical recall.
//!
//! This crate owns the native memory model and store. MCP transport, model
//! reranking, and host-side prompt assembly remain outside the crate.

mod atomic_fs;
mod project_id;

pub mod memory_store;

pub use project_id::{InvalidProjectId, ProjectId};
