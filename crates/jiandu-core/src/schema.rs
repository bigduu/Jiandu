//! Deterministically generated JSON Schemas for the public `v1alpha1` contracts.

use crate::auth::TrustedRequestContext;
use crate::error::{ErrorEnvelope, ResultEnvelope};
use crate::frontmatter::MemoryFrontmatterV1Alpha1;
use crate::memory::MemoryRecord;
use crate::mutation::{
    ForgetMemoryCommand, ForgetMemoryResult, RememberMemoryCommand, RememberMemoryResult,
    UpdateMemoryCommand, UpdateMemoryResult,
};
use crate::query::{
    MemoryGetRequest, MemoryListRequest, MemoryListResult, MemorySearchRequest, MemorySearchResult,
};
use schemars::{JsonSchema, schema_for};
use serde_json::Value;
use std::collections::BTreeMap;

/// Relative file names and canonical schema documents checked into the crate.
///
/// A drift test compares this output byte-semantically after JSON parsing, so
/// contract changes cannot silently leave published schemas behind.
#[must_use]
pub fn generated_contract_schemas() -> BTreeMap<&'static str, Value> {
    BTreeMap::from([
        (
            "error-envelope.schema.json",
            schema_value::<ErrorEnvelope>(),
        ),
        (
            "forget-memory-command.schema.json",
            schema_value::<ForgetMemoryCommand>(),
        ),
        (
            "forget-memory-result-envelope.schema.json",
            schema_value::<ResultEnvelope<ForgetMemoryResult>>(),
        ),
        (
            "memory-frontmatter.schema.json",
            schema_value::<MemoryFrontmatterV1Alpha1>(),
        ),
        (
            "memory-get-request.schema.json",
            schema_value::<MemoryGetRequest>(),
        ),
        (
            "memory-get-result-envelope.schema.json",
            schema_value::<ResultEnvelope<MemoryRecord>>(),
        ),
        (
            "memory-list-request.schema.json",
            schema_value::<MemoryListRequest>(),
        ),
        (
            "memory-list-result-envelope.schema.json",
            schema_value::<ResultEnvelope<MemoryListResult>>(),
        ),
        ("memory-record.schema.json", schema_value::<MemoryRecord>()),
        (
            "memory-search-request.schema.json",
            schema_value::<MemorySearchRequest>(),
        ),
        (
            "memory-search-result-envelope.schema.json",
            schema_value::<ResultEnvelope<MemorySearchResult>>(),
        ),
        (
            "remember-memory-command.schema.json",
            schema_value::<RememberMemoryCommand>(),
        ),
        (
            "remember-memory-result-envelope.schema.json",
            schema_value::<ResultEnvelope<RememberMemoryResult>>(),
        ),
        (
            "trusted-request-context.schema.json",
            schema_value::<TrustedRequestContext>(),
        ),
        (
            "update-memory-command.schema.json",
            schema_value::<UpdateMemoryCommand>(),
        ),
        (
            "update-memory-result-envelope.schema.json",
            schema_value::<ResultEnvelope<UpdateMemoryResult>>(),
        ),
    ])
}

fn schema_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schema_for!(T)).expect("schemars root schemas are JSON serializable")
}
