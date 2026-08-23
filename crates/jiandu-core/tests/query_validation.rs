use jiandu_core::{MemoryListRequest, MemorySearchRequest, Validate};
use serde_json::json;

#[test]
fn search_contract_uses_top_level_deterministic_pagination_fields() {
    let request: MemorySearchRequest = serde_json::from_value(json!({
        "query": "project identity decision",
        "scopes": [
            { "kind": "project", "projectId": "prj_01K3PROJECT" },
            { "kind": "principal" }
        ],
        "types": ["decision", "preference"],
        "statuses": ["active"],
        "tags": ["identity"],
        "limit": 10,
        "cursor": "eyJvZmZzZXQiOjEwfQ"
    }))
    .unwrap_or_else(|error| panic!("valid search request: {error}"));
    request
        .validate()
        .unwrap_or_else(|error| panic!("search request validates: {error}"));
    assert_eq!(request.limit.get(), 10);
}

#[test]
fn search_rejects_empty_query_duplicate_filters_and_out_of_range_limit() {
    let invalid_limit = serde_json::from_value::<MemorySearchRequest>(json!({
        "query": "identity",
        "scopes": [{ "kind": "principal" }],
        "limit": 0
    }));
    assert!(invalid_limit.is_err());

    let request: MemorySearchRequest = serde_json::from_value(json!({
        "query": " ",
        "scopes": [{ "kind": "principal" }, { "kind": "principal" }],
        "types": ["decision", "decision"],
        "limit": 10
    }))
    .unwrap_or_else(|error| panic!("structurally valid search request: {error}"));
    let errors = request
        .validate()
        .expect_err("semantic query/filter validation must fail");
    assert!(errors.len() >= 3);
}

#[test]
fn list_sort_values_are_closed_and_unknown_input_fields_fail() {
    let request: MemoryListRequest = serde_json::from_value(json!({
        "scopes": [{ "kind": "session", "sessionId": "ses_01K3SESSION" }],
        "statuses": ["active", "stale"],
        "sort": "updated_at_desc",
        "limit": 25
    }))
    .unwrap_or_else(|error| panic!("valid list request: {error}"));
    request
        .validate()
        .unwrap_or_else(|error| panic!("list request validates: {error}"));

    assert!(
        serde_json::from_value::<MemoryListRequest>(json!({
            "scopes": [{ "kind": "principal" }],
            "sort": "relevance",
            "limit": 10
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<MemoryListRequest>(json!({
            "scopes": [{ "kind": "principal" }],
            "limit": 10,
            "principalId": "prn_other"
        }))
        .is_err()
    );
}
