//! Trusted authentication context contracts.

use crate::ids::{ClientId, PrincipalId};
use crate::validation::{Validate, ValidationCode, ValidationErrors, ValidationIssue};
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::fmt;

const MAX_GRANTS: usize = 64;

/// An authorization capability established by the host authentication boundary.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Grant(
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[a-z][a-z0-9:_-]*$"))] String,
);

impl Grant {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.starts_with(|character: char| character.is_ascii_lowercase())
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b':' | b'_' | b'-')
            })
        {
            return Err(ValidationIssue::new(
                "grants",
                ValidationCode::InvalidFormat,
                "grant names must be lower-case ASCII capability tokens",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Grant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Authenticated identity supplied by a trusted transport adapter.
///
/// This type is deliberately separate from every model-visible command. A
/// caller must pass it beside a command after authentication; no command can
/// select a principal or impersonate a client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedRequestContext {
    pub principal_id: PrincipalId,
    pub client_id: ClientId,
    #[schemars(length(max = 64))]
    pub grants: BTreeSet<Grant>,
}

impl Validate for TrustedRequestContext {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if self.grants.len() > MAX_GRANTS {
            errors.push(ValidationIssue::new(
                "grants",
                ValidationCode::OutOfRange,
                format!("must contain at most {MAX_GRANTS} grants"),
            ));
        }
        errors.finish()
    }
}
