use jiandu_core::{
    ForgetMemoryCommand, MemoryRecord, RememberMemoryCommand, UpdateMemoryCommand, Validate,
    ValidationCode,
};
use serde_json::json;

fn valid_record() -> MemoryRecord {
    serde_json::from_value(json!({
        "schema": "jiandu.dev/memory/v1alpha1",
        "id": "mem_01K3IDENTITY",
        "revision": 7,
        "etag": "record-7",
        "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
        "type": "decision",
        "status": "active",
        "title": "Use opaque project identity",
        "body": "Workspace paths remain mutable metadata and never become identity.",
        "tags": ["architecture", "identity"],
        "createdAt": "2026-08-23T10:00:00Z",
        "updatedAt": "2026-08-23T10:05:00Z",
        "provenance": { "createdBy": "host" },
        "relations": []
    }))
    .unwrap_or_else(|error| panic!("valid record: {error}"))
}

#[test]
fn remember_requires_idempotency_without_accepting_auth_identity() {
    let command: RememberMemoryCommand = serde_json::from_value(json!({
        "scope": { "kind": "project", "projectId": "prj_01K3PROJECT" },
        "type": "decision",
        "title": "Use opaque project identity",
        "body": "Workspace paths remain mutable metadata and never become identity.",
        "tags": ["architecture", "identity"],
        "provenance": {
            "agentId": "bamboo",
            "sessionId": "ses_01K3SESSION",
            "messageIds": ["msg_41"]
        },
        "idempotencyKey": "evt_01K3REMEMBER"
    }))
    .unwrap_or_else(|error| panic!("valid remember command: {error}"));
    command
        .validate()
        .unwrap_or_else(|error| panic!("remember command must validate: {error}"));

    let value = serde_json::to_value(command)
        .unwrap_or_else(|error| panic!("serialize remember command: {error}"));
    assert_eq!(value["idempotencyKey"], "evt_01K3REMEMBER");
    assert!(value.get("principalId").is_none());
    assert!(value.get("clientId").is_none());

    let mut impersonation = value.clone();
    impersonation["principalId"] = json!("prn_other");
    assert!(serde_json::from_value::<RememberMemoryCommand>(impersonation).is_err());

    let mut missing_key = value;
    missing_key
        .as_object_mut()
        .expect("command is an object")
        .remove("idempotencyKey");
    assert!(serde_json::from_value::<RememberMemoryCommand>(missing_key).is_err());
}

#[test]
fn update_requires_expected_revision_and_rejects_empty_or_ambiguous_patches() {
    let command: UpdateMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 7,
        "patch": {
            "status": "stale",
            "tags": { "add": ["reviewed"], "remove": ["identity"] },
            "relations": {
                "add": [{ "kind": "supports", "targetMemoryId": "mem_01K3TARGET" }]
            }
        },
        "reason": "Decision needs revalidation",
        "idempotencyKey": "evt_01K3UPDATE"
    }))
    .unwrap_or_else(|error| panic!("valid update command: {error}"));
    command
        .validate_against(&valid_record())
        .unwrap_or_else(|error| panic!("update command must validate: {error}"));

    let value = serde_json::to_value(&command)
        .unwrap_or_else(|error| panic!("serialize update command: {error}"));
    assert_eq!(value["expectedRevision"], 7);
    assert_eq!(value["idempotencyKey"], "evt_01K3UPDATE");
    assert!(value.get("principalId").is_none());
    assert!(value.get("clientId").is_none());

    let empty: UpdateMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 7,
        "patch": {},
        "reason": "No actual change",
        "idempotencyKey": "evt_01K3EMPTY"
    }))
    .expect("empty patch is structurally valid before domain validation");
    assert!(empty.validate().is_err());

    let ambiguous: UpdateMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 7,
        "patch": { "tags": { "add": ["same"], "remove": ["same"] } },
        "reason": "Ambiguous tag edit",
        "idempotencyKey": "evt_01K3AMBIGUOUS"
    }))
    .expect("ambiguous patch is structurally valid before domain validation");
    assert!(ambiguous.validate().is_err());
}

#[test]
fn update_checks_revision_identity_self_relations_and_status_transition() {
    let command: UpdateMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 6,
        "patch": {
            "status": "active",
            "relations": {
                "add": [{ "kind": "related_to", "targetMemoryId": "mem_01K3IDENTITY" }]
            }
        },
        "reason": "Try a stale unsafe update",
        "idempotencyKey": "evt_01K3STALE"
    }))
    .unwrap_or_else(|error| panic!("structural update command: {error}"));
    let errors = command
        .validate_against(&valid_record())
        .expect_err("stale self-relation update must fail");
    assert_eq!(
        errors
            .as_slice()
            .iter()
            .filter(|issue| {
                issue.field == "expectedRevision" && issue.code == ValidationCode::Conflict
            })
            .count(),
        1
    );
    assert_eq!(
        errors
            .as_slice()
            .iter()
            .filter(|issue| {
                issue.field == "patch.relations.add" && issue.code == ValidationCode::Conflict
            })
            .count(),
        1
    );

    let mut archived = valid_record();
    archived.status = jiandu_core::MemoryStatus::Archived;
    let reactivate: UpdateMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 7,
        "patch": { "status": "active" },
        "reason": "Try to reactivate terminal record",
        "idempotencyKey": "evt_01K3REACTIVATE"
    }))
    .unwrap_or_else(|error| panic!("structural update command: {error}"));
    let errors = reactivate
        .validate_against(&archived)
        .expect_err("archived records are terminal");
    assert!(errors.as_slice().iter().any(|issue| {
        issue.field == "patch.status" && issue.code == ValidationCode::InvalidTransition
    }));
}

#[test]
fn forget_requires_revision_reason_and_idempotency_key() {
    let command: ForgetMemoryCommand = serde_json::from_value(json!({
        "memoryId": "mem_01K3IDENTITY",
        "expectedRevision": 7,
        "reason": "User requested deletion",
        "idempotencyKey": "delete_01K3IDENTITY"
    }))
    .unwrap_or_else(|error| panic!("valid forget command: {error}"));
    command
        .validate()
        .unwrap_or_else(|error| panic!("forget command must validate: {error}"));

    let value = serde_json::to_value(command)
        .unwrap_or_else(|error| panic!("serialize forget command: {error}"));
    assert_eq!(value["expectedRevision"], 7);
    assert_eq!(value["idempotencyKey"], "delete_01K3IDENTITY");
    assert!(value.get("principalId").is_none());
    assert!(value.get("clientId").is_none());

    let missing_revision = json!({
        "memoryId": "mem_01K3IDENTITY",
        "reason": "User requested deletion",
        "idempotencyKey": "delete_01K3IDENTITY"
    });
    assert!(serde_json::from_value::<ForgetMemoryCommand>(missing_revision).is_err());
}
