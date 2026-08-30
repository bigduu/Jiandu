//! Opaque public identities, revisions, ETags, and canonical timestamps.

use crate::validation::{ValidationCode, ValidationIssue};
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MAX_ID_LENGTH: usize = 128;

fn validate_prefixed_id(field: &str, value: &str, prefix: &str) -> Result<(), ValidationIssue> {
    if value.len() <= prefix.len() || value.len() > MAX_ID_LENGTH || !value.starts_with(prefix) {
        return Err(ValidationIssue::new(
            field,
            ValidationCode::InvalidFormat,
            format!("must be an opaque {prefix} identifier of at most {MAX_ID_LENGTH} bytes"),
        ));
    }
    let suffix = &value[prefix.len()..];
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ValidationIssue::new(
            field,
            ValidationCode::InvalidFormat,
            "must contain only ASCII letters, digits, '_' or '-' after its prefix",
        ));
    }
    Ok(())
}

macro_rules! define_prefixed_id {
    ($name:ident, $field:literal, $prefix:literal, $pattern:literal, $min:literal) => {
        #[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(
            #[schemars(length(min = $min, max = 128), regex(pattern = $pattern))] String,
        );

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
                let value = value.into();
                validate_prefixed_id($field, &value, $prefix)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = ValidationIssue;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

define_prefixed_id!(MemoryId, "memoryId", "mem_", r"^mem_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(
    PrincipalId,
    "principalId",
    "prn_",
    r"^prn_[A-Za-z0-9_-]+$",
    5
);
define_prefixed_id!(ProjectId, "projectId", "prj_", r"^prj_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(SessionId, "sessionId", "ses_", r"^ses_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(ClientId, "clientId", "cli_", r"^cli_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(EventId, "eventId", "evt_", r"^evt_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(BranchId, "branchId", "br_", r"^br_[A-Za-z0-9_-]+$", 4);
define_prefixed_id!(MessageId, "messageId", "msg_", r"^msg_[A-Za-z0-9_-]+$", 5);
define_prefixed_id!(
    CorrelationId,
    "correlationId",
    "req_",
    r"^req_[A-Za-z0-9_-]+$",
    5
);

fn validate_token(
    field: &str,
    value: &str,
    max_length: usize,
    predicate: impl Fn(u8) -> bool,
) -> Result<(), ValidationIssue> {
    if value.is_empty() || value.len() > max_length || !value.bytes().all(predicate) {
        return Err(ValidationIssue::new(
            field,
            ValidationCode::InvalidFormat,
            format!("must be a non-empty opaque token of at most {max_length} ASCII bytes"),
        ));
    }
    Ok(())
}

macro_rules! define_token {
    ($name:ident, $field:literal, $max:literal, $pattern:literal, $predicate:expr) => {
        #[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(
            #[schemars(length(min = 1, max = $max), regex(pattern = $pattern))] String,
        );

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
                let value = value.into();
                validate_token($field, &value, $max, $predicate)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

define_token!(
    AgentId,
    "agentId",
    128,
    r"^[A-Za-z0-9._:-]+$",
    |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
);
define_token!(
    IdempotencyKey,
    "idempotencyKey",
    128,
    r"^[A-Za-z0-9._:-]+$",
    |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
);
define_token!(
    PageCursor,
    "cursor",
    1024,
    r"^[A-Za-z0-9_-]+$",
    |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
);

/// Positive, monotonically increasing per-record revision.
#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Revision(#[schemars(range(min = 1))] u64);

impl Revision {
    pub fn new(value: u64) -> Result<Self, ValidationIssue> {
        if value == 0 {
            return Err(ValidationIssue::new(
                "revision",
                ValidationCode::OutOfRange,
                "must be a positive integer",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Monotonic revision of the canonical store; zero represents an empty store.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct StoreRevision(pub u64);

/// Opaque entity tag returned by the authoritative store.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Etag(#[schemars(length(min = 1, max = 256), regex(pattern = r"^[!-~]+$"))] String);

impl Etag {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(ValidationIssue::new(
                "etag",
                ValidationCode::InvalidFormat,
                "must be 1 to 256 visible ASCII bytes",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Etag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Canonical RFC 3339 timestamp in UTC using the `Z` designator.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Timestamp(
    #[schemars(
        length(min = 20, max = 35),
        regex(pattern = r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{0,8}[1-9])?Z$"),
        extend("format" = "date-time")
    )]
    String,
);

impl Timestamp {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        if value.len() < 20 || value.len() > 35 || !value.ends_with('Z') {
            return Err(timestamp_issue());
        }
        let parsed = OffsetDateTime::parse(&value, &Rfc3339).map_err(|_| timestamp_issue())?;
        let canonical = parsed.format(&Rfc3339).map_err(|_| timestamp_issue())?;
        if canonical != value {
            return Err(timestamp_issue());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn unix_timestamp_nanos(&self) -> i128 {
        OffsetDateTime::parse(&self.0, &Rfc3339)
            .map_or(i128::MIN, |value| value.unix_timestamp_nanos())
    }
}

fn timestamp_issue() -> ValidationIssue {
    ValidationIssue::new(
        "timestamp",
        ValidationCode::InvalidFormat,
        "must be canonical RFC 3339 UTC using the 'Z' designator",
    )
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
