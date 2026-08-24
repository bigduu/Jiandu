//! Store identity, format version, and canonical watermark metadata.

use jiandu_core::{StoreRevision, Timestamp};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Store format that requires every create/update to publish an idempotency
/// receipt and one sequence-addressed audit event before acknowledgement.
pub const STORE_FORMAT_VERSION: &str = "jiandu.store/v1alpha2";
pub(crate) const LEGACY_STORE_FORMAT_VERSION: &str = "jiandu.store/v1alpha1";

/// Independent monotonic address of the private mutation audit ledger.
///
/// Zero is the genesis watermark. Each committed create/update advances this
/// value exactly once; idempotent replay never advances it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AuditSequence(pub u64);

/// Opaque UUID-backed store identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct StoreId(String);

impl StoreId {
    pub fn new(value: impl Into<String>) -> Result<Self, crate::StoreError> {
        let value = value.into();
        let parsed =
            Uuid::parse_str(&value).map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
        if parsed.hyphenated().to_string() != value {
            return Err(crate::StoreError::InvalidStoreMetadata);
        }
        Ok(Self(value))
    }

    pub(crate) fn random() -> Self {
        Self(Uuid::new_v4().hyphenated().to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for StoreId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for StoreId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Strict `store.json` representation for the supported format.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreMetadata {
    pub format_version: String,
    pub store_id: StoreId,
    pub store_revision: StoreRevision,
    #[serde(default)]
    pub audit_sequence: AuditSequence,
    pub created_at: Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoreMetadata<'a> {
    format_version: &'a str,
    store_id: &'a StoreId,
    store_revision: StoreRevision,
    created_at: &'a Timestamp,
}

impl StoreMetadata {
    pub(crate) fn new() -> Result<Self, crate::StoreError> {
        Ok(Self {
            format_version: STORE_FORMAT_VERSION.to_owned(),
            store_id: StoreId::random(),
            store_revision: StoreRevision(0),
            audit_sequence: AuditSequence(0),
            created_at: timestamp_now()?,
        })
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, crate::StoreError> {
        let mut bytes = if self.format_version == LEGACY_STORE_FORMAT_VERSION {
            serde_json::to_vec_pretty(&LegacyStoreMetadata {
                format_version: &self.format_version,
                store_id: &self.store_id,
                store_revision: self.store_revision,
                created_at: &self.created_at,
            })
        } else {
            serde_json::to_vec_pretty(self)
        }
        .map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub(crate) fn upgraded_from_legacy(mut self) -> Result<Self, crate::StoreError> {
        if self.format_version != LEGACY_STORE_FORMAT_VERSION || self.audit_sequence.0 != 0 {
            return Err(crate::StoreError::InvalidStoreMetadata);
        }
        self.format_version = STORE_FORMAT_VERSION.to_owned();
        Ok(self)
    }
}

pub(crate) fn timestamp_now() -> Result<Timestamp, crate::StoreError> {
    let value = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
    Timestamp::new(value).map_err(|_| crate::StoreError::InvalidStoreMetadata)
}
