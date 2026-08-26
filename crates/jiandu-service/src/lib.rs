//! Production loopback daemon composition for Jiandu.
//!
//! Startup configuration is read from one trusted local file. MCP requests
//! never select filesystem paths, identities, permissions, or mutation policy.

mod auth;
mod config;
mod daemon;
mod lifecycle;

pub use config::{ConfigError, ServeConfig};
pub use daemon::{DaemonError, RunningDaemon, ShutdownOutcome};

pub const MCP_ROUTE: &str = "/mcp";
pub const LIVENESS_ROUTE: &str = "/live";
pub const READINESS_ROUTE: &str = "/ready";

#[cfg(test)]
mod tests;
