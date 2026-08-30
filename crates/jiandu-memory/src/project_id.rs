use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Opaque, path-safe, caller-provided Project identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    pub const MAX_LEN: usize = 64;

    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidProjectId> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if valid {
            Ok(Self(value))
        } else {
            Err(InvalidProjectId(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ProjectId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ProjectId {
    type Err = InvalidProjectId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ProjectId {
    type Error = InvalidProjectId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ProjectId> for String {
    fn from(value: ProjectId) -> Self {
        value.into_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProjectId(String);

impl fmt::Display for InvalidProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid project id: {}", self.0)
    }
}

impl std::error::Error for InvalidProjectId {}
