use jiandu_core::{Etag, MemoryFrontmatterV1Alpha1, MemoryRecord, Validate};
use serde_json::Value;

const VALID_JSON: &str = include_str!("../fixtures/v1alpha1/valid/memory-record.json");
const VALID_MARKDOWN: &str = include_str!("../fixtures/v1alpha1/valid/memory-record.md");
const INVALID_JSON: &[&str] = &[
    include_str!("../fixtures/v1alpha1/invalid/memory-record-unknown-type.json"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-path-project-id.json"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-missing-status.json"),
];
const INVALID_MARKDOWN: &[&str] = &[
    include_str!("../fixtures/v1alpha1/invalid/memory-record-unknown-status.md"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-path-project-id.md"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-instance-extra.md"),
];

fn split_markdown(document: &str) -> (&str, &str) {
    let document = document
        .strip_prefix("---\n")
        .expect("canonical Markdown starts with a frontmatter delimiter");
    document
        .split_once("\n---\n")
        .expect("canonical Markdown has a closing frontmatter delimiter")
}

#[test]
fn canonical_json_fixture_round_trips_without_semantic_drift() {
    let source: Value = serde_json::from_str(VALID_JSON)
        .unwrap_or_else(|error| panic!("valid JSON fixture syntax: {error}"));
    let record: MemoryRecord = serde_json::from_value(source.clone())
        .unwrap_or_else(|error| panic!("valid JSON record fixture: {error}"));
    record
        .validate()
        .unwrap_or_else(|error| panic!("valid JSON record semantics: {error}"));
    let round_trip = serde_json::to_value(record)
        .unwrap_or_else(|error| panic!("serialize JSON record fixture: {error}"));
    assert_eq!(round_trip, source);
}

#[test]
fn canonical_markdown_frontmatter_round_trips_through_api_record() {
    assert!(VALID_MARKDOWN.ends_with('\n'));
    assert!(!VALID_MARKDOWN.contains("\r\n"));
    let (yaml, body) = split_markdown(VALID_MARKDOWN);
    let header: MemoryFrontmatterV1Alpha1 = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|error| panic!("valid Markdown frontmatter: {error}"));
    header
        .validate_document(body)
        .unwrap_or_else(|error| panic!("valid Markdown document semantics: {error}"));

    let record = header.clone().into_record(
        Etag::new("fixture-etag").unwrap_or_else(|error| panic!("valid fixture ETag: {error}")),
        body.into(),
    );
    let round_trip = MemoryFrontmatterV1Alpha1::from_record(&record);
    assert_eq!(round_trip, header);
    assert_eq!(record.body, body);

    let source_yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).unwrap_or_else(|error| panic!("valid source YAML: {error}"));
    let encoded_yaml = serde_yaml_ng::to_value(round_trip)
        .unwrap_or_else(|error| panic!("serialize frontmatter DTO: {error}"));
    assert_eq!(encoded_yaml, source_yaml);
}

#[test]
fn malformed_or_unknown_json_fixtures_fail_explicitly() {
    for fixture in INVALID_JSON {
        assert!(
            serde_json::from_str::<MemoryRecord>(fixture).is_err(),
            "invalid JSON fixture unexpectedly decoded"
        );
    }
}

#[test]
fn malformed_or_unknown_markdown_frontmatter_fails_explicitly() {
    for fixture in INVALID_MARKDOWN {
        let (yaml, _) = split_markdown(fixture);
        assert!(
            serde_yaml_ng::from_str::<MemoryFrontmatterV1Alpha1>(yaml).is_err(),
            "invalid Markdown fixture unexpectedly decoded"
        );
    }
}
