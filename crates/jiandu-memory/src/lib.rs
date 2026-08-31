//! Filesystem-backed memory persistence and deterministic lexical recall.
//!
//! This crate owns the native memory model and store. MCP transport, model
//! reranking, and host-side prompt assembly remain outside the crate.

mod atomic_fs;
mod project_id;

pub mod bamboo_import;
pub mod memory_store;

pub use bamboo_import::{BambooImportReport, import_bamboo_durable_memory};
pub use project_id::{InvalidProjectId, ProjectId};
