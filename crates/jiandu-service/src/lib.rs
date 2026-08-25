//! Production loopback daemon composition for Jiandu.
//!
//! Startup configuration is read from one trusted local file. MCP requests
//! never select filesystem paths, identities, grants, or mutation policy.

mod auth;
mod config;
mod daemon;

pub use config::{ConfigError, ServeConfig};
pub use daemon::{DaemonError, RunningDaemon};

pub const MCP_ROUTE: &str = "/mcp";
pub const LIVENESS_ROUTE: &str = "/live";
pub const READINESS_ROUTE: &str = "/ready";

#[cfg(test)]
mod tests;
