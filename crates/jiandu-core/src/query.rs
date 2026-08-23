//! Deterministic search, list, filter, and pagination contracts.

use crate::ids::{MemoryId, PageCursor, Timestamp};
use crate::memory::{MemoryStatus, MemorySummary, MemoryType, Tag};
use crate::scope::ScopeSelector;
use crate::validation::{
    MAX_FILTER_VALUES, MAX_PAGE_LIMIT, MAX_QUERY_CHARS, MAX_SCOPES, Validate, ValidationCode,
    ValidationErrors, ValidationIssue,
};
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;

/// Read exactly one visible record by opaque ID.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryGetRequest {
    pub memory_id: MemoryId,
}

/// Bounded page size shared by search and list operations.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PageLimit(#[schemars(range(min = 1, max = 100))] u16);

impl PageLimit {
    pub fn new(value: u16) -> Result<Self, ValidationIssue> {
        if value == 0 || value > MAX_PAGE_LIMIT {
            return Err(ValidationIssue::new(
                "limit",
                ValidationCode::OutOfRange,
                format!("must be between 1 and {MAX_PAGE_LIMIT}"),
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for PageLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u16::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Stable list order. Implementations use memory ID as the final tie-breaker.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListSort {
    #[default]
    UpdatedAtDesc,
    UpdatedAtAsc,
    CreatedAtDesc,
    CreatedAtAsc,
    IdAsc,
}

/// Ranked full-text query over authorized scopes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemorySearchRequest {
    #[schemars(length(min = 1, max = 4096))]
    pub query: String,
    #[schemars(length(min = 1, max = 16))]
    pub scopes: Vec<ScopeSelector>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub types: Vec<MemoryType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub statuses: Vec<MemoryStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub tags: Vec<Tag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<Timestamp>,
    pub limit: PageLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PageCursor>,
}

impl Validate for MemorySearchRequest {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_query(&self.query, &mut errors);
        validate_filters(
            &self.scopes,
            &self.types,
            &self.statuses,
            &self.tags,
            &mut errors,
        );
        errors.finish()
    }
}

/// Structured deterministic listing without relevance ranking.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryListRequest {
    #[schemars(length(min = 1, max = 16))]
    pub scopes: Vec<ScopeSelector>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub types: Vec<MemoryType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub statuses: Vec<MemoryStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub tags: Vec<Tag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<Timestamp>,
    #[serde(default)]
    pub sort: ListSort,
    pub limit: PageLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PageCursor>,
}

impl Validate for MemoryListRequest {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_filters(
            &self.scopes,
            &self.types,
            &self.statuses,
            &self.tags,
            &mut errors,
        );
        errors.finish()
    }
}

/// Secret-safe diagnostics for a ranked query.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchDiagnostics {
    pub index_degraded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Ranked result ordered by score descending, then memory ID ascending.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchResult {
    pub memories: Vec<MemorySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<PageCursor>,
    pub has_more: bool,
    pub diagnostics: SearchDiagnostics,
}

/// Deterministically sorted list result.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryListResult {
    pub memories: Vec<MemorySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<PageCursor>,
    pub has_more: bool,
}

fn validate_query(query: &str, errors: &mut ValidationErrors) {
    if query.trim().is_empty() {
        errors.push(ValidationIssue::new(
            "query",
            ValidationCode::Required,
            "must not be empty or whitespace",
        ));
    }
    if query != query.trim() || query.chars().any(|character| character.is_control()) {
        errors.push(ValidationIssue::new(
            "query",
            ValidationCode::InvalidFormat,
            "must be trimmed and contain no control characters",
        ));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        errors.push(ValidationIssue::new(
            "query",
            ValidationCode::OutOfRange,
            format!("must contain at most {MAX_QUERY_CHARS} characters"),
        ));
    }
}

fn validate_filters(
    scopes: &[ScopeSelector],
    types: &[MemoryType],
    statuses: &[MemoryStatus],
    tags: &[Tag],
    errors: &mut ValidationErrors,
) {
    if scopes.is_empty() || scopes.len() > MAX_SCOPES {
        errors.push(ValidationIssue::new(
            "scopes",
            ValidationCode::OutOfRange,
            format!("must contain between 1 and {MAX_SCOPES} authorized scopes"),
        ));
    }
    validate_unique_filter(scopes, "scopes", errors);
    validate_filter_count(types.len(), "types", errors);
    validate_unique_filter(types, "types", errors);
    validate_filter_count(statuses.len(), "statuses", errors);
    validate_unique_filter(statuses, "statuses", errors);
    validate_filter_count(tags.len(), "tags", errors);
    validate_unique_filter(tags, "tags", errors);
}

fn validate_filter_count(count: usize, field: &str, errors: &mut ValidationErrors) {
    if count > MAX_FILTER_VALUES {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::OutOfRange,
            format!("must contain at most {MAX_FILTER_VALUES} values"),
        ));
    }
}

fn validate_unique_filter<T>(values: &[T], field: &str, errors: &mut ValidationErrors)
where
    T: Eq + std::hash::Hash,
{
    let mut seen = HashSet::with_capacity(values.len());
    if values.iter().any(|value| !seen.insert(value)) {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::Duplicate,
            "must not contain duplicate filter values",
        ));
    }
}
