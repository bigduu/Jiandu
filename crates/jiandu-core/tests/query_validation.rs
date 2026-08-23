use jiandu_core::{
    MemoryListRequest, MemoryListResult, MemorySearchRequest, MemorySearchResult, Validate,
};
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

#[test]
fn ranked_search_requires_score_while_list_summaries_do_not_expose_it() {
    let summary = json!({
        "id": "mem_01K3IDENTITY",
        "revision": 7,
        "etag": "record-7",
        "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
        "type": "decision",
        "status": "active",
        "title": "Use opaque project identity",
        "summary": "Workspace paths remain metadata, not identity.",
        "tags": ["architecture", "identity"],
        "updatedAt": "2026-08-23T10:05:00Z"
    });

    let list: MemoryListResult = serde_json::from_value(json!({
        "memories": [summary.clone()],
        "hasMore": false
    }))
    .unwrap_or_else(|error| panic!("list summary without score: {error}"));
    list.validate()
        .unwrap_or_else(|error| panic!("valid list result: {error}"));
    let list_json =
        serde_json::to_value(list).unwrap_or_else(|error| panic!("serialize list result: {error}"));
    assert!(list_json["memories"][0].get("score").is_none());

    let mut scored_list_summary = summary.clone();
    scored_list_summary["score"] = json!(0.91);
    assert!(
        serde_json::from_value::<MemoryListResult>(json!({
            "memories": [scored_list_summary],
            "hasMore": false
        }))
        .is_err()
    );

    let mut summary_without_excerpt = summary.clone();
    summary_without_excerpt
        .as_object_mut()
        .expect("summary is an object")
        .remove("summary");
    let no_excerpt: MemoryListResult = serde_json::from_value(json!({
        "memories": [summary_without_excerpt],
        "hasMore": false
    }))
    .unwrap_or_else(|error| panic!("canonical summary metadata is optional: {error}"));
    no_excerpt
        .validate()
        .unwrap_or_else(|error| panic!("summary without synthesized excerpt: {error}"));

    let unranked_search = json!({
        "memories": [summary.clone()],
        "hasMore": false,
        "diagnostics": { "indexDegraded": false }
    });
    assert!(serde_json::from_value::<MemorySearchResult>(unranked_search).is_err());

    let mut ranked = summary;
    ranked["score"] = json!(0.91);
    let search: MemorySearchResult = serde_json::from_value(json!({
        "memories": [ranked],
        "hasMore": false,
        "diagnostics": { "indexDegraded": false }
    }))
    .unwrap_or_else(|error| panic!("ranked summary with score: {error}"));
    search
        .validate()
        .unwrap_or_else(|error| panic!("valid ranked result: {error}"));
    let search_json = serde_json::to_value(search)
        .unwrap_or_else(|error| panic!("serialize search result: {error}"));
    assert_eq!(search_json["memories"][0]["score"], 0.91);
}

#[test]
fn result_validation_recurses_into_each_summary() {
    let invalid: MemoryListResult = serde_json::from_value(json!({
        "memories": [{
            "id": "mem_01K3IDENTITY",
            "revision": 7,
            "etag": "record-7",
            "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
            "type": "decision",
            "status": "active",
            "title": " ",
            "tags": ["identity", "identity"],
            "updatedAt": "2026-08-23T10:05:00Z"
        }],
        "hasMore": false
    }))
    .unwrap_or_else(|error| panic!("structurally decodable list result: {error}"));

    let errors = invalid
        .validate()
        .expect_err("nested summary invariants must be validated");
    assert!(
        errors
            .as_slice()
            .iter()
            .all(|issue| issue.field.starts_with("memories[0]."))
    );
}
