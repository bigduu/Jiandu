//! Versioned result envelopes and stable, secret-safe domain errors.

use crate::API_VERSION;
use crate::ids::{CorrelationId, StoreRevision};
use crate::validation::{
    Validate, ValidationCode, ValidationErrors, ValidationIssue, validate_required_text,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Closed API version carried on every public response envelope.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum ApiVersion {
    #[default]
    #[serde(rename = "jiandu.dev/v1alpha1")]
    V1Alpha1,
}

impl ApiVersion {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1Alpha1 => API_VERSION,
        }
    }
}

/// Successful operation result with correlation and store-watermark metadata.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultEnvelope<T> {
    pub api_version: ApiVersion,
    pub correlation_id: CorrelationId,
    pub store_revision: StoreRevision,
    pub result: T,
}

/// Stable machine-readable failure categories for `v1alpha1`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DomainErrorCode {
    InvalidArgument,
    Unauthenticated,
    Forbidden,
    NotFound,
    RevisionConflict,
    IdempotencyConflict,
    StoreUnavailable,
    IndexDegraded,
    RateLimited,
    Internal,
}

impl DomainErrorCode {
    /// Deterministic default retry policy associated with the stable code.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RevisionConflict
                | Self::StoreUnavailable
                | Self::IndexDegraded
                | Self::RateLimited
                | Self::Internal
        )
    }
}

/// Safe public error payload. Secret diagnostic context belongs in host logs.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainError {
    pub code: DomainErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

impl DomainError {
    #[must_use]
    pub fn new(code: DomainErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.is_retryable(),
            details: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

impl Validate for DomainError {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "message", &self.message, 1_000);
        if self.retryable != self.code.is_retryable() {
            errors.push(ValidationIssue::new(
                "retryable",
                ValidationCode::Conflict,
                "must match the retry policy for code",
            ));
        }
        if self.details.len() > 32 {
            errors.push(ValidationIssue::new(
                "details",
                ValidationCode::OutOfRange,
                "must contain at most 32 entries",
            ));
        }
        for key in self.details.keys() {
            if key.is_empty()
                || key.len() > 64
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                errors.push(ValidationIssue::new(
                    "details",
                    ValidationCode::InvalidFormat,
                    "keys must be 1 to 64 ASCII letters, digits, '_' or '-'",
                ));
                break;
            }
        }
        errors.finish()
    }
}

/// Failed operation result with the same correlation metadata as success.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    pub api_version: ApiVersion,
    pub correlation_id: CorrelationId,
    pub store_revision: StoreRevision,
    pub error: DomainError,
}
