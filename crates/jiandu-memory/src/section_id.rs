use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable, host-provided identity within a Project. Lowercase IDs avoid aliases
/// on case-insensitive filesystems.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SectionId(String);

impl SectionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidSectionId> {
        Self::try_from(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SectionId {
    type Error = InvalidSectionId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if (1..=64).contains(&value.len())
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b':')
            })
        {
            Ok(Self(value))
        } else {
            Err(InvalidSectionId)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSectionId;

impl fmt::Display for InvalidSectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "section_id must contain 1–64 lowercase ASCII letters, digits, '-', '_' or ':'",
        )
    }
}

impl std::error::Error for InvalidSectionId {}
