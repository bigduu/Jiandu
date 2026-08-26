//! Transport-independent authenticated MCP adapter for Jiandu.
//!
//! Authentication and scope resolution happen at the trusted host boundary.
//! This crate never accepts a principal, client identity, filesystem path, or
//! prompt-placement policy from an MCP request.

mod backend;
mod health;
mod policy;
mod resource;
mod server;

pub use backend::{
    CanonicalReadBackend, McpMutationBackend, McpReadBackend, MutationBackendCommit,
    MutationBackendError, ReadBackendError,
};
pub use health::{
    IndexReadHealth, OptionalCapability, ReadHealthSnapshot, ReadServiceHealth, StoreReadHealth,
};
pub use policy::{
    AllowAllSecretContent, ConfiguredMutationPolicy, MutationPolicy, MutationPolicyContext,
    MutationPolicyError, MutationPolicyRequest, MutationScopeKind, SecretContentPolicy,
};
pub use resource::{
    INSTANCE_GLOBAL_LIST_RESOURCE_URI, MEMORY_RESOURCE_TEMPLATE, PRINCIPAL_LIST_RESOURCE_URI,
    PROJECT_LIST_RESOURCE_TEMPLATE, SESSION_LIST_RESOURCE_TEMPLATE,
};
pub use server::{JIANDU_MCP_PROTOCOL_REVISION, JianduReadServer};
