//! Agent-neutral domain contracts for Jiandu.
//!
//! This crate contains only ordinary Rust data types and validation policy. It
//! deliberately has no storage, transport, agent-runtime, prompt, model, or
//! filesystem-path identity dependency.

/// Public Jiandu API version carried by v1alpha1 envelopes.
pub const API_VERSION: &str = "jiandu.dev/v1alpha1";

/// Canonical memory record schema identifier.
pub const MEMORY_SCHEMA: &str = "jiandu.dev/memory/v1alpha1";

pub mod auth;
pub mod error;
pub mod frontmatter;
pub mod ids;
pub mod memory;
pub mod mutation;
pub mod query;
pub mod schema;
pub mod scope;
pub mod validation;

pub use auth::*;
pub use error::*;
pub use frontmatter::*;
pub use ids::*;
pub use memory::*;
pub use mutation::*;
pub use query::*;
pub use schema::*;
pub use scope::*;
pub use validation::*;
