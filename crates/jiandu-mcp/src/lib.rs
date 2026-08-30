//! Thin MCP adapter for Jiandu's unified `memory` tool.
//!
//! The wire contract is the current Bamboo `MemoryArgs` action enum. This crate
//! maps it directly onto [`jiandu_memory::memory_store::MemoryStore`] and exposes
//! only a local stdio transport.

mod args;
mod context;
mod handler;
mod server;

pub use args::{
    MEMORY_ACTIONS, MemoryActionOptions, MemoryArgs, MemoryToolClass, QueryFilters, SplitPiece,
    WriteOptions,
};
pub use context::{MemoryError, MemoryExecutionContext};
pub use jiandu_memory::ProjectId;
pub use server::{
    MEMORY_TOOL_DESCRIPTION, MEMORY_TOOL_NAME, MemoryServer, memory_tool, serve_stdio,
};
