use jiandu_core::{
    ContentDigest, MemoryFrontmatterV1Alpha1, MemoryRecord, generated_contract_schemas,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const SCHEMA_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schemas/v1alpha1");
const VALID_JSON: &str = include_str!("../fixtures/v1alpha1/valid/memory-record.json");
const INVALID_JSON: &[&str] = &[
    include_str!("../fixtures/v1alpha1/invalid/memory-record-unknown-type.json"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-path-project-id.json"),
    include_str!("../fixtures/v1alpha1/invalid/memory-record-missing-status.json"),
];

fn schema_accepts(schema: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .expect("generated schema compiles")
        .is_valid(instance)
}

fn assert_unique_items(schema: &Value, pointers: &[&str]) {
    for pointer in pointers {
        assert_eq!(
            schema.pointer(pointer),
            Some(&Value::Bool(true)),
            "{pointer} must declare uniqueItems"
        );
    }
}

#[test]
fn checked_in_schemas_match_rust_definitions_exactly() {
    let generated = generated_contract_schemas();
    if std::env::var_os("JIANDU_UPDATE_SCHEMAS").is_some() {
        fs::create_dir_all(SCHEMA_DIRECTORY).expect("create checked schema directory");
        for (name, schema) in &generated {
            let bytes = format!(
                "{}\n",
                serde_json::to_string_pretty(schema).expect("format generated schema")
            );
            fs::write(Path::new(SCHEMA_DIRECTORY).join(name), bytes)
                .unwrap_or_else(|error| panic!("write generated schema {name}: {error}"));
        }
    }

    let expected_names: BTreeSet<_> = generated.keys().map(ToString::to_string).collect();
    let actual_names: BTreeSet<_> = fs::read_dir(SCHEMA_DIRECTORY)
        .expect("checked schema directory exists")
        .map(|entry| {
            entry
                .expect("read checked schema entry")
                .file_name()
                .into_string()
                .expect("schema file name is UTF-8")
        })
        .collect();
    assert_eq!(actual_names, expected_names);

    for (name, expected) in generated {
        let checked = fs::read_to_string(Path::new(SCHEMA_DIRECTORY).join(name))
            .unwrap_or_else(|error| panic!("read checked schema {name}: {error}"));
        let checked: Value = serde_json::from_str(&checked)
            .unwrap_or_else(|error| panic!("parse checked schema {name}: {error}"));
        assert_eq!(checked, expected, "schema drift in {name}");
    }
}

#[test]
fn record_schema_accepts_valid_fixture_and_rejects_invalid_fixtures() {
    let schemas = generated_contract_schemas();
    let schema = &schemas["memory-record.schema.json"];
    let valid: Value = serde_json::from_str(VALID_JSON)
        .unwrap_or_else(|error| panic!("parse valid record fixture: {error}"));
    assert!(schema_accepts(schema, &valid));

    for fixture in INVALID_JSON {
        let invalid: Value = serde_json::from_str(fixture)
            .unwrap_or_else(|error| panic!("invalid semantic fixture has valid JSON: {error}"));
        assert!(!schema_accepts(schema, &invalid));
    }
}

#[test]
fn frontmatter_schema_accepts_the_canonical_header() {
    let markdown = include_str!("../fixtures/v1alpha1/valid/memory-record.md");
    let yaml = markdown
        .strip_prefix("---\n")
        .and_then(|document| document.split_once("\n---\n"))
        .map(|(header, _)| header)
        .expect("canonical Markdown delimiters");
    let frontmatter: MemoryFrontmatterV1Alpha1 = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|error| panic!("parse canonical frontmatter: {error}"));
    let value = serde_json::to_value(frontmatter)
        .unwrap_or_else(|error| panic!("serialize canonical frontmatter: {error}"));
    assert!(schema_accepts(
        &generated_contract_schemas()["memory-frontmatter.schema.json"],
        &value
    ));
}

#[test]
fn model_visible_request_schemas_cannot_accept_auth_identity_fields() {
    let schemas = generated_contract_schemas();
    for name in [
        "memory-get-request.schema.json",
        "memory-search-request.schema.json",
        "memory-list-request.schema.json",
        "remember-memory-command.schema.json",
        "update-memory-command.schema.json",
        "forget-memory-command.schema.json",
    ] {
        let encoded = serde_json::to_string(&schemas[name])
            .unwrap_or_else(|error| panic!("serialize command schema {name}: {error}"));
        assert!(
            !encoded.contains("principalId"),
            "{name} exposes principalId"
        );
        assert!(!encoded.contains("clientId"), "{name} exposes clientId");
    }
}

#[test]
fn digest_schema_and_rust_validation_reject_the_same_hyphenated_algorithm() {
    let mut value: Value = serde_json::from_str(VALID_JSON)
        .unwrap_or_else(|error| panic!("parse valid record fixture: {error}"));
    value["provenance"]["contentDigest"] = Value::String("sha-256:0123abcd".into());
    let schema = &generated_contract_schemas()["memory-record.schema.json"];
    assert!(!schema_accepts(schema, &value));
    assert!(serde_json::from_value::<MemoryRecord>(value).is_err());
    assert!(ContentDigest::new("sha-256:0123abcd").is_err());
}

#[test]
fn scalar_schema_constraints_match_runtime_canonicalization() {
    let schema = &generated_contract_schemas()["memory-record.schema.json"];
    let valid: Value = serde_json::from_str(VALID_JSON)
        .unwrap_or_else(|error| panic!("parse valid record fixture: {error}"));

    let mut redundant_fraction = valid.clone();
    redundant_fraction["updatedAt"] = Value::String("2026-08-23T10:05:00.000Z".into());
    assert!(!schema_accepts(schema, &redundant_fraction));

    let mut impossible_date = valid.clone();
    impossible_date["updatedAt"] = Value::String("2026-02-30T10:05:00Z".into());
    assert!(!schema_accepts(schema, &impossible_date));

    let mut invisible_etag = valid.clone();
    invisible_etag["etag"] = Value::String("etag with space".into());
    assert!(!schema_accepts(schema, &invisible_etag));

    let mut non_ascii_uri = valid;
    non_ascii_uri["provenance"]["sourceUri"] = Value::String("https://例子.invalid/source".into());
    assert!(!schema_accepts(schema, &non_ascii_uri));

    assert_eq!(
        schema["properties"]["body"]["x-jiandu-maxUtf8Bytes"],
        65_536
    );
}

#[test]
fn ranked_and_unranked_summary_schemas_encode_score_and_text_bounds() {
    let schemas = generated_contract_schemas();
    let search = &schemas["memory-search-result-envelope.schema.json"];
    let ranked = &search["$defs"]["RankedMemorySummary"];
    assert_eq!(ranked["properties"]["score"]["$ref"], "#/$defs/SearchScore");
    assert!(
        ranked["required"]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|field| field == "score"))
    );
    assert_eq!(ranked["additionalProperties"], false);
    assert_eq!(ranked["properties"]["tags"]["uniqueItems"], true);

    let list = &schemas["memory-list-result-envelope.schema.json"];
    let summary = &list["$defs"]["MemorySummary"];
    assert!(summary["properties"].get("score").is_none());
    assert_eq!(summary["additionalProperties"], false);
    assert_eq!(summary["properties"]["title"]["minLength"], 1);
    assert_eq!(summary["properties"]["title"]["maxLength"], 200);
    assert_eq!(summary["properties"]["summary"]["minLength"], 1);
    assert_eq!(summary["properties"]["summary"]["maxLength"], 1_000);
    assert_eq!(summary["properties"]["tags"]["maxItems"], 32);
    assert_eq!(summary["properties"]["tags"]["uniqueItems"], true);

    let summary_instance = serde_json::json!({
        "id": "mem_01K3IDENTITY",
        "revision": 7,
        "etag": "record-7",
        "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
        "type": "decision",
        "status": "active",
        "title": "Use opaque project identity",
        "tags": ["architecture", "identity"],
        "updatedAt": "2026-08-23T10:05:00Z"
    });
    let envelope = |memory: Value| {
        serde_json::json!({
            "apiVersion": "jiandu.dev/v1alpha1",
            "correlationId": "req_01K3RESULT",
            "storeRevision": 42,
            "result": {
                "memories": [memory],
                "hasMore": false
            }
        })
    };

    assert!(schema_accepts(list, &envelope(summary_instance.clone())));

    let mut scored_list = summary_instance.clone();
    scored_list["score"] = serde_json::json!(0.91);
    assert!(!schema_accepts(list, &envelope(scored_list)));

    let mut duplicate_tags = summary_instance.clone();
    duplicate_tags["tags"] = serde_json::json!(["identity", "identity"]);
    assert!(!schema_accepts(list, &envelope(duplicate_tags)));

    let mut empty_title = summary_instance.clone();
    empty_title["title"] = serde_json::json!("");
    assert!(!schema_accepts(list, &envelope(empty_title)));

    let search_envelope = |memory: Value| {
        serde_json::json!({
            "apiVersion": "jiandu.dev/v1alpha1",
            "correlationId": "req_01K3RESULT",
            "storeRevision": 42,
            "result": {
                "memories": [memory],
                "hasMore": false,
                "diagnostics": { "indexDegraded": false }
            }
        })
    };
    assert!(!schema_accepts(
        search,
        &search_envelope(summary_instance.clone())
    ));
    let mut ranked_instance = summary_instance;
    ranked_instance["score"] = serde_json::json!(0.91);
    assert!(schema_accepts(search, &search_envelope(ranked_instance)));
}

#[test]
fn generated_schemas_encode_all_collection_uniqueness_invariants() {
    let schemas = generated_contract_schemas();

    assert_unique_items(
        &schemas["memory-record.schema.json"],
        &[
            "/properties/tags/uniqueItems",
            "/properties/relations/uniqueItems",
            "/$defs/Provenance/properties/messageIds/uniqueItems",
        ],
    );
    assert_unique_items(
        &schemas["memory-frontmatter.schema.json"],
        &[
            "/properties/tags/uniqueItems",
            "/properties/relations/uniqueItems",
            "/$defs/FrontmatterProvenance/properties/message_ids/uniqueItems",
        ],
    );
    assert_unique_items(
        &schemas["remember-memory-command.schema.json"],
        &[
            "/properties/tags/uniqueItems",
            "/properties/relations/uniqueItems",
            "/$defs/ProvenanceInput/properties/messageIds/uniqueItems",
        ],
    );
    assert_unique_items(
        &schemas["memory-search-request.schema.json"],
        &[
            "/properties/scopes/uniqueItems",
            "/properties/types/uniqueItems",
            "/properties/statuses/uniqueItems",
            "/properties/tags/uniqueItems",
        ],
    );
    assert_unique_items(
        &schemas["memory-list-request.schema.json"],
        &[
            "/properties/scopes/uniqueItems",
            "/properties/types/uniqueItems",
            "/properties/statuses/uniqueItems",
            "/properties/tags/uniqueItems",
        ],
    );
    assert_unique_items(
        &schemas["update-memory-command.schema.json"],
        &[
            "/$defs/TagPatch/properties/add/uniqueItems",
            "/$defs/TagPatch/properties/remove/uniqueItems",
            "/$defs/RelationPatch/properties/add/uniqueItems",
            "/$defs/RelationPatch/properties/remove/uniqueItems",
        ],
    );
}
