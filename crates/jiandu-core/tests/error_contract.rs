use jiandu_core::{
    ApiVersion, DomainError, DomainErrorCode, ErrorEnvelope, ResultEnvelope, Validate,
};
use serde_json::json;

#[test]
fn error_codes_have_committed_names_and_retryability() {
    let cases = [
        (DomainErrorCode::InvalidArgument, "INVALID_ARGUMENT", false),
        (DomainErrorCode::Unauthenticated, "UNAUTHENTICATED", false),
        (DomainErrorCode::Forbidden, "FORBIDDEN", false),
        (DomainErrorCode::NotFound, "NOT_FOUND", false),
        (DomainErrorCode::RevisionConflict, "REVISION_CONFLICT", true),
        (
            DomainErrorCode::IdempotencyConflict,
            "IDEMPOTENCY_CONFLICT",
            false,
        ),
        (DomainErrorCode::StoreUnavailable, "STORE_UNAVAILABLE", true),
        (DomainErrorCode::IndexDegraded, "INDEX_DEGRADED", true),
        (DomainErrorCode::RateLimited, "RATE_LIMITED", true),
        (DomainErrorCode::Internal, "INTERNAL", true),
    ];

    for (code, name, retryable) in cases {
        assert_eq!(
            serde_json::to_value(code)
                .unwrap_or_else(|error| panic!("serialize error code: {error}")),
            json!(name)
        );
        assert_eq!(code.is_retryable(), retryable);
        let error = DomainError::new(code, "Safe public failure");
        assert_eq!(error.retryable, retryable);
        error
            .validate()
            .unwrap_or_else(|error| panic!("generated error must validate: {error}"));
    }
}

#[test]
fn inconsistent_retryability_and_unsafe_detail_keys_fail_validation() {
    let mut error = DomainError::new(DomainErrorCode::NotFound, "No visible memory exists")
        .with_detail("currentRevision", json!(7));
    error.retryable = true;
    error.details.insert("host path".into(), json!("redacted"));
    assert!(error.validate().is_err());
}

#[test]
fn success_and_error_envelopes_share_committed_metadata_names() {
    let success: ResultEnvelope<serde_json::Value> = serde_json::from_value(json!({
        "apiVersion": "jiandu.dev/v1alpha1",
        "correlationId": "req_01K3SUCCESS",
        "storeRevision": 42,
        "result": { "ok": true }
    }))
    .unwrap_or_else(|error| panic!("valid success envelope: {error}"));
    assert_eq!(success.api_version, ApiVersion::V1Alpha1);

    let failure: ErrorEnvelope = serde_json::from_value(json!({
        "apiVersion": "jiandu.dev/v1alpha1",
        "correlationId": "req_01K3FAILURE",
        "storeRevision": 42,
        "error": {
            "code": "REVISION_CONFLICT",
            "message": "Expected revision is stale",
            "retryable": true,
            "details": { "currentRevision": 8 }
        }
    }))
    .unwrap_or_else(|error| panic!("valid error envelope: {error}"));
    failure
        .error
        .validate()
        .unwrap_or_else(|error| panic!("valid error contract: {error}"));

    let unknown_version = json!({
        "apiVersion": "jiandu.dev/v1beta1",
        "correlationId": "req_01K3FAILURE",
        "storeRevision": 42,
        "error": {
            "code": "INTERNAL",
            "message": "Failure",
            "retryable": true
        }
    });
    assert!(serde_json::from_value::<ErrorEnvelope>(unknown_version).is_err());
}
