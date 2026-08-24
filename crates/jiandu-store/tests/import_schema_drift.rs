use jiandu_store::{
    BackupMetadata, ImportDryRunPlan, PortableImportResult, generated_import_schemas,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const SCHEMA_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schemas/import/v1alpha1");
const VALID_PLAN: &str = include_str!("../fixtures/import/v1alpha1/valid/dry-run-plan.json");
const VALID_RESULT: &str = include_str!("../fixtures/import/v1alpha1/valid/import-result.json");
const VALID_BACKUP: &str = include_str!("../fixtures/import/v1alpha1/valid/backup-metadata.json");

fn schema_accepts(schema: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .expect("generated import schema compiles")
        .is_valid(instance)
}

fn generated_schema<'a>(generated: &'a [(&'static str, Value)], name: &str) -> &'a Value {
    &generated
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing generated schema {name}"))
        .1
}

#[test]
fn checked_in_import_schemas_match_rust_definitions_exactly() {
    let generated = generated_import_schemas();
    if std::env::var_os("JIANDU_UPDATE_SCHEMAS").is_some() {
        fs::create_dir_all(SCHEMA_DIRECTORY).expect("create checked import schema directory");
        for (name, schema) in &generated {
            let bytes = format!(
                "{}\n",
                serde_json::to_string_pretty(schema).expect("format generated schema")
            );
            fs::write(Path::new(SCHEMA_DIRECTORY).join(name), bytes)
                .unwrap_or_else(|error| panic!("write generated import schema {name}: {error}"));
        }
    }

    let expected_names: BTreeSet<_> = generated
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    let actual_names: BTreeSet<_> = fs::read_dir(SCHEMA_DIRECTORY)
        .expect("checked import schema directory exists")
        .map(|entry| {
            entry
                .expect("read import schema entry")
                .file_name()
                .into_string()
                .expect("import schema file name is UTF-8")
        })
        .collect();
    assert_eq!(actual_names, expected_names);

    for (name, expected) in generated {
        let checked = fs::read_to_string(Path::new(SCHEMA_DIRECTORY).join(name))
            .unwrap_or_else(|error| panic!("read checked import schema {name}: {error}"));
        let checked: Value = serde_json::from_str(&checked)
            .unwrap_or_else(|error| panic!("parse checked import schema {name}: {error}"));
        assert_eq!(checked, expected, "schema drift in {name}");
    }
}

#[test]
fn canonical_import_fixtures_round_trip_and_match_generated_schemas() {
    let generated = generated_import_schemas();
    let plan_json: Value = serde_json::from_str(VALID_PLAN).expect("valid import plan fixture");
    let result_json: Value =
        serde_json::from_str(VALID_RESULT).expect("valid import result fixture");
    let backup_json: Value =
        serde_json::from_str(VALID_BACKUP).expect("valid backup metadata fixture");
    assert!(schema_accepts(
        generated_schema(&generated, "import-dry-run-plan.schema.json"),
        &plan_json,
    ));
    assert!(schema_accepts(
        generated_schema(&generated, "portable-import-result.schema.json"),
        &result_json,
    ));
    assert!(schema_accepts(
        generated_schema(&generated, "backup-metadata.schema.json"),
        &backup_json,
    ));

    assert_eq!(
        ImportDryRunPlan::decode_canonical(VALID_PLAN.as_bytes())
            .expect("strict import plan fixture")
            .canonical_bytes()
            .expect("canonical import plan bytes"),
        VALID_PLAN.as_bytes(),
    );
    assert_eq!(
        PortableImportResult::decode_canonical(VALID_RESULT.as_bytes())
            .expect("strict import result fixture")
            .canonical_bytes()
            .expect("canonical import result bytes"),
        VALID_RESULT.as_bytes(),
    );
    assert_eq!(
        BackupMetadata::decode_canonical(VALID_BACKUP.as_bytes())
            .expect("strict backup metadata fixture")
            .canonical_bytes()
            .expect("canonical backup metadata bytes"),
        VALID_BACKUP.as_bytes(),
    );
}

#[test]
fn import_schemas_reject_versions_digests_transaction_ids_and_batch_bounds() {
    let generated = generated_import_schemas();
    let plan_schema = generated_schema(&generated, "import-dry-run-plan.schema.json");
    let result_schema = generated_schema(&generated, "portable-import-result.schema.json");
    let backup_schema = generated_schema(&generated, "backup-metadata.schema.json");

    let mut plan: Value = serde_json::from_str(VALID_PLAN).expect("valid plan fixture");
    plan["bundleDigest"] = Value::String("sha256:not-hex".to_owned());
    assert!(!schema_accepts(plan_schema, &plan));
    let mut empty_scopes: Value = serde_json::from_str(VALID_PLAN).expect("valid plan fixture");
    empty_scopes["scopes"] = Value::Array(Vec::new());
    assert!(!schema_accepts(plan_schema, &empty_scopes));

    let valid_result: Value = serde_json::from_str(VALID_RESULT).expect("valid result fixture");
    let mut wrong_result_version = valid_result.clone();
    wrong_result_version["formatVersion"] =
        Value::String("jiandu.import-result/v1alpha2".to_owned());
    assert!(!schema_accepts(result_schema, &wrong_result_version));
    let mut invalid_transaction = valid_result.clone();
    invalid_transaction["transactionId"] = Value::String("not-a-uuid".to_owned());
    assert!(!schema_accepts(result_schema, &invalid_transaction));
    let mut too_many_records = valid_result;
    too_many_records["recordCount"] = Value::from(101_u64);
    assert!(!schema_accepts(result_schema, &too_many_records));

    let mut backup: Value = serde_json::from_str(VALID_BACKUP).expect("valid backup fixture");
    backup["bundleDigest"] = Value::String(format!("sha256:{}", "A".repeat(64)));
    assert!(!schema_accepts(backup_schema, &backup));
}
