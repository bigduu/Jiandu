use jiandu_core::{
    BranchId, CommittedMessageRange, ContentDigest, MemoryId, MemoryRecord, MemoryRelation,
    MemoryStatus, MessageId, Provenance, RelationKind, SessionId, SourceUri, Tag, Validate,
    ValidationCode,
};
use serde_json::json;

fn valid_record() -> MemoryRecord {
    serde_json::from_value(json!({
        "schema": "jiandu.dev/memory/v1alpha1",
        "id": "mem_01K3IDENTITY",
        "revision": 7,
        "etag": "W/\"mem_01K3IDENTITY:7:abc123\"",
        "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
        "type": "decision",
        "status": "active",
        "title": "Use opaque project identity",
        "summary": "Workspace paths are metadata, not Project identity.",
        "body": "Workspace paths remain mutable metadata and never become identity.",
        "tags": ["architecture", "identity"],
        "createdAt": "2026-08-23T10:00:00Z",
        "updatedAt": "2026-08-23T10:05:00Z",
        "provenance": {
            "createdBy": "host",
            "agentId": "bamboo",
            "sessionId": "ses_01K3SESSION",
            "branchId": "br_01K3BRANCH",
            "messageIds": ["msg_41", "msg_42"],
            "sourceUri": "https://example.invalid/decisions/identity",
            "contentDigest": "sha256:0123456789abcdef",
            "extraction": { "method": "explicit", "extractorVersion": "1.0.0" },
            "confidence": 0.98
        },
        "relations": [{
            "kind": "supports",
            "targetMemoryId": "mem_01K3ARCHITECTURE"
        }]
    }))
    .unwrap_or_else(|error| panic!("valid record fixture value: {error}"))
}

#[test]
fn complete_record_validates_and_round_trips_committed_names() {
    let record = valid_record();
    record
        .validate()
        .unwrap_or_else(|error| panic!("record must validate: {error}"));
    let value = serde_json::to_value(&record)
        .unwrap_or_else(|error| panic!("serialize valid record: {error}"));
    assert_eq!(value["type"], "decision");
    assert_eq!(value["scope"]["projectId"], "prj_01K3PROJECT");
    assert_eq!(value["provenance"]["createdBy"], "host");
    assert!(value.get("memoryType").is_none());
}

#[test]
fn unknown_closed_enum_values_fail_explicitly() {
    let mut unknown_type = serde_json::to_value(valid_record())
        .unwrap_or_else(|error| panic!("serialize record: {error}"));
    unknown_type["type"] = json!("strategy");
    assert!(serde_json::from_value::<MemoryRecord>(unknown_type).is_err());

    let mut unknown_status = serde_json::to_value(valid_record())
        .unwrap_or_else(|error| panic!("serialize record: {error}"));
    unknown_status["status"] = json!("deleted");
    assert!(serde_json::from_value::<MemoryRecord>(unknown_status).is_err());
}

#[test]
fn lifecycle_transition_policy_is_explicit_and_archived_is_terminal() {
    assert!(MemoryStatus::Active.can_transition_to(MemoryStatus::Stale));
    assert!(MemoryStatus::Stale.can_transition_to(MemoryStatus::Active));
    assert!(MemoryStatus::Contradicted.can_transition_to(MemoryStatus::Active));
    assert!(MemoryStatus::Superseded.can_transition_to(MemoryStatus::Archived));
    assert!(!MemoryStatus::Superseded.can_transition_to(MemoryStatus::Active));
    assert!(!MemoryStatus::Archived.can_transition_to(MemoryStatus::Active));
    assert!(MemoryStatus::Archived.can_transition_to(MemoryStatus::Archived));
}

#[test]
fn tag_relation_timestamp_and_body_bounds_are_enforced() {
    assert!(Tag::new("Architecture").is_err());
    assert!(Tag::new("../identity").is_err());

    let mut duplicate_tags = valid_record();
    duplicate_tags.tags.push(
        Tag::new("identity").unwrap_or_else(|error| panic!("valid duplicate tag value: {error}")),
    );
    let errors = duplicate_tags
        .validate()
        .expect_err("duplicate tags must fail validation");
    assert!(
        errors
            .as_slice()
            .iter()
            .any(|issue| issue.field == "tags" && issue.code == ValidationCode::Duplicate)
    );

    let mut self_relation = valid_record();
    self_relation.relations.push(MemoryRelation {
        kind: RelationKind::RelatedTo,
        target_memory_id: self_relation.id.clone(),
    });
    assert!(self_relation.validate().is_err());

    let mut backwards_time = valid_record();
    std::mem::swap(
        &mut backwards_time.created_at,
        &mut backwards_time.updated_at,
    );
    assert!(backwards_time.validate().is_err());

    let mut too_large = valid_record();
    too_large.body = "x".repeat(65_537);
    assert!(too_large.validate().is_err());
}

#[test]
fn provenance_rejects_ambiguous_ranges_duplicate_messages_and_orphan_branches() {
    let message =
        MessageId::new("msg_41").unwrap_or_else(|error| panic!("valid message id: {error}"));
    let mut provenance = Provenance {
        created_by: jiandu_core::CreationActor::Host,
        agent_id: None,
        session_id: None,
        branch_id: Some(
            BranchId::new("br_01K3BRANCH")
                .unwrap_or_else(|error| panic!("valid branch id: {error}")),
        ),
        message_ids: vec![message.clone(), message.clone()],
        message_range: Some(CommittedMessageRange {
            first_message_id: message.clone(),
            last_message_id: message,
        }),
        source_uri: None,
        content_digest: None,
        extraction: None,
        confidence: None,
    };
    let errors = provenance
        .validate()
        .expect_err("ambiguous provenance must fail");
    assert!(errors.len() >= 3);

    provenance.session_id = Some(
        SessionId::new("ses_01K3SESSION")
            .unwrap_or_else(|error| panic!("valid session id: {error}")),
    );
    provenance.message_ids.clear();
    provenance
        .validate()
        .unwrap_or_else(|error| panic!("range provenance must validate: {error}"));
}

#[test]
fn relation_targets_remain_opaque_ids() {
    let target = MemoryId::new("mem_01K3TARGET")
        .unwrap_or_else(|error| panic!("valid target memory id: {error}"));
    let relation = MemoryRelation {
        kind: RelationKind::DerivedFrom,
        target_memory_id: target,
    };
    assert_eq!(
        serde_json::to_value(relation)
            .unwrap_or_else(|error| panic!("serialize relation: {error}")),
        json!({ "kind": "derived_from", "targetMemoryId": "mem_01K3TARGET" })
    );
}

#[test]
fn content_digest_runtime_uses_the_committed_schema_grammar() {
    assert!(ContentDigest::new("sha256:0123456789abcdef").is_ok());
    assert!(ContentDigest::new("sha_256:0123456789ABCDEF").is_ok());
    assert!(ContentDigest::new("sha-256:0123456789abcdef").is_err());
    assert!(ContentDigest::new("SHA256:0123456789abcdef").is_err());
}

#[test]
fn source_uri_uses_the_committed_ascii_uri_grammar() {
    assert!(SourceUri::new("https://example.invalid/source?id=42").is_ok());
    assert!(SourceUri::new("https://example.invalid/a path").is_err());
    assert!(SourceUri::new("https://例子.invalid/source").is_err());
}
