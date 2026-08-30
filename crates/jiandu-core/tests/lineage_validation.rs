use jiandu_core::{
    BRANCH_SNAPSHOT_EVENT_SCHEMA, BranchSnapshotEvent, BranchSnapshotEventSchema,
    SESSION_SNAPSHOT_MANIFEST_SCHEMA, SessionSnapshotManifest, SessionSnapshotManifestSchema,
    Validate, ValidationCode,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const VALID_EVENT: &str = include_str!("../fixtures/v1alpha1/valid/branch-snapshot-event.json");
const VALID_MANIFEST: &str =
    include_str!("../fixtures/v1alpha1/valid/session-snapshot-manifest.json");
const SAME_LINEAGE_EVENT: &str =
    include_str!("../fixtures/v1alpha1/invalid/branch-snapshot-event-same-lineage.json");
const OUT_OF_ORDER_MANIFEST: &str =
    include_str!("../fixtures/v1alpha1/invalid/session-snapshot-manifest-anchor-order.json");

fn assert_canonical_fixture<T>(fixture: &str) -> T
where
    T: DeserializeOwned + Serialize + Validate,
{
    let value: T = serde_json::from_str(fixture)
        .unwrap_or_else(|error| panic!("canonical fixture must decode: {error}"));
    value
        .validate()
        .unwrap_or_else(|error| panic!("canonical fixture must validate: {error}"));
    let encoded = format!(
        "{}\n",
        serde_json::to_string_pretty(&value)
            .unwrap_or_else(|error| panic!("canonical fixture must encode: {error}"))
    );
    assert_eq!(encoded, fixture);
    value
}

#[test]
fn valid_lineage_fixtures_round_trip_canonically() {
    let event: BranchSnapshotEvent = assert_canonical_fixture(VALID_EVENT);
    let manifest: SessionSnapshotManifest = assert_canonical_fixture(VALID_MANIFEST);

    assert_eq!(manifest.event, event);
    assert_eq!(
        BranchSnapshotEventSchema::V1Alpha1.as_str(),
        BRANCH_SNAPSHOT_EVENT_SCHEMA
    );
    assert_eq!(
        SessionSnapshotManifestSchema::V1Alpha1.as_str(),
        SESSION_SNAPSHOT_MANIFEST_SCHEMA
    );
}

#[test]
fn branch_event_rejects_same_lineage_and_strict_shape_drift() {
    let same: BranchSnapshotEvent =
        serde_json::from_str(SAME_LINEAGE_EVENT).expect("invalid semantic fixture must decode");
    let errors = same
        .validate()
        .expect_err("source and target Session identity must differ");
    assert!(errors.as_slice().iter().any(|issue| {
        issue.field == "targetSessionId" && issue.code == ValidationCode::Conflict
    }));

    let valid: Value = serde_json::from_str(VALID_EVENT).expect("valid event JSON");

    let mut host_scoped_branch_ids = valid.clone();
    host_scoped_branch_ids["targetBranchId"] = host_scoped_branch_ids["sourceBranchId"].clone();
    let host_scoped_branch_ids: BranchSnapshotEvent =
        serde_json::from_value(host_scoped_branch_ids).expect("structurally valid branch event");
    host_scoped_branch_ids
        .validate()
        .expect("branch IDs need not be globally unique across distinct Sessions");

    let mut unknown_mode = valid.clone();
    unknown_mode["mode"] = json!("live_follow");
    assert!(serde_json::from_value::<BranchSnapshotEvent>(unknown_mode).is_err());

    let mut unknown_version = valid.clone();
    unknown_version["schema"] = json!("jiandu.dev/branch-snapshot-event/v1alpha2");
    assert!(serde_json::from_value::<BranchSnapshotEvent>(unknown_version).is_err());

    let mut extra_auth_identity = valid.clone();
    extra_auth_identity["principalId"] = json!("prn_other");
    assert!(serde_json::from_value::<BranchSnapshotEvent>(extra_auth_identity).is_err());

    let mut path_identity = valid;
    path_identity["targetSessionId"] = json!("/workspace/project/session");
    assert!(serde_json::from_value::<BranchSnapshotEvent>(path_identity).is_err());
}

#[test]
fn manifest_requires_exact_sorted_unique_record_anchors() {
    let out_of_order: SessionSnapshotManifest =
        serde_json::from_str(OUT_OF_ORDER_MANIFEST).expect("invalid semantic fixture must decode");
    let errors = out_of_order
        .validate()
        .expect_err("non-canonical anchor order must fail");
    assert!(errors.as_slice().iter().any(|issue| {
        issue.field == "visibleRecords[1].memoryId" && issue.code == ValidationCode::InvalidFormat
    }));

    let mut duplicate: SessionSnapshotManifest =
        serde_json::from_str(VALID_MANIFEST).expect("valid manifest");
    duplicate.visible_records[1].memory_id = duplicate.visible_records[0].memory_id.clone();
    let errors = duplicate
        .validate()
        .expect_err("duplicate memory IDs must fail even when anchors otherwise differ");
    assert!(errors.as_slice().iter().any(|issue| {
        issue.field == "visibleRecords[1].memoryId" && issue.code == ValidationCode::Duplicate
    }));

    let mut impossible_revision: SessionSnapshotManifest =
        serde_json::from_str(VALID_MANIFEST).expect("valid manifest");
    impossible_revision.source_store_revision.0 = 2;
    let errors = impossible_revision
        .validate()
        .expect_err("a record revision cannot exceed the source store watermark");
    assert!(errors.as_slice().iter().any(|issue| {
        issue.field == "visibleRecords[0].revision" && issue.code == ValidationCode::Conflict
    }));
}

#[test]
fn manifest_contains_only_opaque_visibility_identity() {
    let manifest: Value = serde_json::from_str(VALID_MANIFEST).expect("valid manifest JSON");
    let encoded = serde_json::to_string(&manifest).expect("encode manifest");

    for forbidden in [
        "principalId",
        "clientId",
        "workspacePath",
        "messageBody",
        "prompt",
        "credential",
        "Bamboo",
    ] {
        assert!(!encoded.contains(forbidden), "manifest exposed {forbidden}");
    }
}
