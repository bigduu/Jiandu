use jiandu_store::{PortableExportBundle, ValidationReport, generated_inspection_schemas};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const SCHEMA_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schemas/inspection/v1alpha1");
const VALID_REPORT: &str =
    include_str!("../fixtures/inspection/v1alpha1/valid/validation-report.json");
const VALID_EXPORT: &str =
    include_str!("../fixtures/inspection/v1alpha1/valid/portable-export.json");

fn schema_accepts(schema: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .expect("generated inspection schema compiles")
        .is_valid(instance)
}

#[test]
fn checked_in_inspection_schemas_match_rust_definitions_exactly() {
    let generated = generated_inspection_schemas();
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
        .expect("checked inspection schema directory exists")
        .map(|entry| {
            entry
                .expect("read inspection schema entry")
                .file_name()
                .into_string()
                .expect("inspection schema file name is UTF-8")
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
fn canonical_fixtures_round_trip_and_match_generated_schemas() {
    let schemas = generated_inspection_schemas();
    let report_json: Value = serde_json::from_str(VALID_REPORT).expect("valid report fixture JSON");
    let export_json: Value = serde_json::from_str(VALID_EXPORT).expect("valid export fixture JSON");
    assert!(schema_accepts(
        &schemas["validation-report.schema.json"],
        &report_json
    ));
    assert!(schema_accepts(
        &schemas["portable-export-bundle.schema.json"],
        &export_json
    ));
    let tombstone_schema = &schemas["portable-tombstone.schema.json"];
    assert!(
        export_json["tombstones"]
            .as_array()
            .expect("fixture tombstones")
            .iter()
            .all(|tombstone| schema_accepts(tombstone_schema, tombstone))
    );

    assert_eq!(
        ValidationReport::decode_canonical(VALID_REPORT.as_bytes())
            .expect("strict report fixture")
            .canonical_bytes()
            .expect("canonical report bytes"),
        VALID_REPORT.as_bytes()
    );
    assert_eq!(
        PortableExportBundle::decode_canonical(VALID_EXPORT.as_bytes())
            .expect("strict export fixture")
            .canonical_bytes()
            .expect("canonical export bytes"),
        VALID_EXPORT.as_bytes()
    );
}

#[test]
fn schemas_reject_version_collection_and_record_bound_violations() {
    let schemas = generated_inspection_schemas();
    let report_schema = &schemas["validation-report.schema.json"];
    let export_schema = &schemas["portable-export-bundle.schema.json"];

    let mut report: Value = serde_json::from_str(VALID_REPORT).expect("valid report fixture JSON");
    report["formatVersion"] = Value::String("jiandu.validation-report/v1alpha2".to_owned());
    assert!(!schema_accepts(report_schema, &report));

    let valid_export: Value =
        serde_json::from_str(VALID_EXPORT).expect("valid export fixture JSON");
    let mut wrong_version = valid_export.clone();
    wrong_version["formatVersion"] = Value::String("jiandu.portable-export/v1alpha2".to_owned());
    assert!(!schema_accepts(export_schema, &wrong_version));

    let mut duplicate_tags = valid_export.clone();
    let tag = duplicate_tags["records"][0]["tags"][0].clone();
    duplicate_tags["records"][0]["tags"] = Value::Array(vec![tag.clone(), tag]);
    assert!(!schema_accepts(export_schema, &duplicate_tags));

    let mut empty_title = valid_export.clone();
    empty_title["records"][0]["title"] = Value::String(String::new());
    assert!(!schema_accepts(export_schema, &empty_title));

    let mut oversized_body = valid_export;
    oversized_body["records"][0]["body"] = Value::String("x".repeat(65_537));
    assert!(!schema_accepts(export_schema, &oversized_body));
}
