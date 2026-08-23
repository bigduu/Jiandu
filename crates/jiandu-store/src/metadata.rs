//! Store identity, format version, and canonical watermark metadata.

use jiandu_core::{StoreRevision, Timestamp};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

pub const STORE_FORMAT_VERSION: &str = "jiandu.store/v1alpha1";

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
    pub created_at: Timestamp,
}

impl StoreMetadata {
    pub(crate) fn new() -> Result<Self, crate::StoreError> {
        Ok(Self {
            format_version: STORE_FORMAT_VERSION.to_owned(),
            store_id: StoreId::random(),
            store_revision: StoreRevision(0),
            created_at: timestamp_now()?,
        })
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, crate::StoreError> {
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub(crate) fn timestamp_now() -> Result<Timestamp, crate::StoreError> {
    let value = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
    Timestamp::new(value).map_err(|_| crate::StoreError::InvalidStoreMetadata)
}
