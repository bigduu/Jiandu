//! Thin MCP adapter for Jiandu's unified `memory` tool.
//!
//! The wire contract is the current Bamboo `MemoryArgs` action enum. Storage,
//! retrieval, and lifecycle policy remain behind one host-provided backend.

mod args;
mod backend;
mod server;

pub use args::{
    MEMORY_ACTIONS, MemoryActionOptions, MemoryArgs, MemoryToolClass, QueryFilters, SplitPiece,
    WriteOptions,
};
pub use backend::{
    MemoryBackend, MemoryError, MemoryExecutionContext, MemoryInvocation, ProjectId,
};
pub use server::{
    MEMORY_TOOL_DESCRIPTION, MEMORY_TOOL_NAME, MemoryServer, memory_tool, serve_stdio,
};
