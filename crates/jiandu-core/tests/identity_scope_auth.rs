use jiandu_core::{
    AgentId, ClientId, Etag, Grant, IdempotencyKey, MemoryId, MemoryScope, PrincipalId, ProjectId,
    Revision, ScopeKind, ScopeSelector, SessionId, Timestamp, TrustedRequestContext, Validate,
};
use serde_json::json;
use std::collections::BTreeSet;

#[test]
fn opaque_id_newtypes_round_trip_without_exposing_path_identity() {
    let id: MemoryId = serde_json::from_value(json!("mem_01K3ABC_xyz-9"))
        .unwrap_or_else(|error| panic!("valid memory id: {error}"));
    assert_eq!(id.as_str(), "mem_01K3ABC_xyz-9");
    assert_eq!(
        serde_json::to_value(id).unwrap_or_else(|error| panic!("serialize memory id: {error}")),
        json!("mem_01K3ABC_xyz-9")
    );

    let path_identity = serde_json::from_value::<ProjectId>(json!("/tmp/project"));
    assert!(path_identity.is_err());
    assert!(ProjectId::new("prj_../../workspace").is_err());
}

#[test]
fn revision_and_timestamp_reject_noncanonical_values() {
    assert!(serde_json::from_value::<Revision>(json!(0)).is_err());
    assert!(Timestamp::new("2026-08-23T10:00:00+00:00").is_err());
    assert!(Timestamp::new("2026-08-23T10:00:00.000Z").is_err());
    assert!(Timestamp::new("2026-08-23T10:00:00Z").is_ok());
    assert!(Timestamp::new("2026-08-23T10:00:00.001Z").is_ok());
    assert!(Etag::new("contains a space").is_err());
    assert!(Etag::new("W/\"opaque:1\"").is_ok());
    assert!(AgentId::new("_host-agent").is_ok());
    assert!(IdempotencyKey::new("-retry-key").is_ok());
}

#[test]
fn scope_selector_uses_committed_json_names_and_never_accepts_principal_impersonation() {
    let project: ScopeSelector = serde_json::from_value(json!({
        "kind": "project",
        "projectId": "prj_01K3PROJECT"
    }))
    .unwrap_or_else(|error| panic!("valid project selector: {error}"));
    assert_eq!(project.kind(), ScopeKind::Project);

    let principal = serde_json::from_value::<ScopeSelector>(json!({
        "kind": "principal",
        "principalId": "prn_other"
    }));
    assert!(principal.is_err());

    let session = ScopeSelector::Session {
        session_id: SessionId::new("ses_01K3SESSION")
            .unwrap_or_else(|error| panic!("valid session id: {error}")),
    };
    assert_eq!(
        serde_json::to_value(session)
            .unwrap_or_else(|error| panic!("serialize session selector: {error}")),
        json!({ "kind": "session", "sessionId": "ses_01K3SESSION" })
    );
}

#[test]
fn resolved_principal_scope_is_distinct_from_model_visible_selector() {
    let scope = MemoryScope::Principal {
        principal_id: PrincipalId::new("prn_01K3OWNER")
            .unwrap_or_else(|error| panic!("valid principal id: {error}")),
    };
    assert_eq!(scope.kind(), ScopeKind::Principal);
    assert_eq!(
        serde_json::to_value(scope)
            .unwrap_or_else(|error| panic!("serialize resolved scope: {error}")),
        json!({ "kind": "principal", "principalId": "prn_01K3OWNER" })
    );

    let forged_global = serde_json::from_value::<MemoryScope>(json!({
        "kind": "instance_global",
        "projectId": "prj_01K3PROJECT"
    }));
    assert!(forged_global.is_err());
}

#[test]
fn trusted_auth_context_is_separate_and_validated() {
    let context = TrustedRequestContext {
        principal_id: PrincipalId::new("prn_01K3OWNER")
            .unwrap_or_else(|error| panic!("valid principal id: {error}")),
        client_id: ClientId::new("cli_01K3CLIENT")
            .unwrap_or_else(|error| panic!("valid client id: {error}")),
        grants: BTreeSet::from([
            Grant::new("memory:read").unwrap_or_else(|error| panic!("valid grant: {error}")),
            Grant::new("memory:write:project")
                .unwrap_or_else(|error| panic!("valid grant: {error}")),
        ]),
    };
    context
        .validate()
        .unwrap_or_else(|error| panic!("valid trusted context: {error}"));
    assert_eq!(
        serde_json::to_value(context)
            .unwrap_or_else(|error| panic!("serialize auth context: {error}")),
        json!({
            "principalId": "prn_01K3OWNER",
            "clientId": "cli_01K3CLIENT",
            "grants": ["memory:read", "memory:write:project"]
        })
    );
}
