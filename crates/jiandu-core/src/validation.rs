//! Shared, side-effect-free domain validation primitives.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_SUMMARY_CHARS: usize = 1_000;
pub const MAX_BODY_BYTES: usize = 65_536;
pub const MAX_REASON_CHARS: usize = 1_000;
pub const MAX_QUERY_CHARS: usize = 4_096;
pub const MAX_TAGS: usize = 32;
pub const MAX_RELATIONS: usize = 128;
pub const MAX_PROVENANCE_MESSAGE_IDS: usize = 128;
pub const MAX_SCOPES: usize = 16;
pub const MAX_FILTER_VALUES: usize = 32;
pub const MAX_PAGE_LIMIT: u16 = 100;

/// Stable categories for field-level contract validation failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    Required,
    InvalidFormat,
    OutOfRange,
    Duplicate,
    Conflict,
    InvalidTransition,
}

/// One safe, field-addressable validation diagnostic.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub field: String,
    pub code: ValidationCode,
    pub message: String,
}

impl ValidationIssue {
    pub fn new(field: impl Into<String>, code: ValidationCode, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            code,
            message: message.into(),
        }
    }

    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.field = if self.field.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}.{}", self.field)
        };
        self
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

/// All validation failures discovered in one side-effect-free validation pass.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ValidationErrors(Vec<ValidationIssue>);

impl ValidationErrors {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, issue: ValidationIssue) {
        self.0.push(issue);
    }

    pub fn extend(&mut self, other: Self, prefix: &str) {
        self.0
            .extend(other.0.into_iter().map(|issue| issue.with_prefix(prefix)));
    }

    #[must_use]
    pub fn as_slice(&self) -> &[ValidationIssue] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn finish(self) -> Result<(), Self> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }
}

impl From<ValidationIssue> for ValidationErrors {
    fn from(issue: ValidationIssue) -> Self {
        Self(vec![issue])
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// Implemented by contract values with cross-field or aggregate invariants.
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationErrors>;
}

pub(crate) fn validate_required_text(
    errors: &mut ValidationErrors,
    field: &str,
    value: &str,
    max_chars: usize,
) {
    let count = value.chars().count();
    if value.trim().is_empty() {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::Required,
            "must not be empty or whitespace",
        ));
    } else if value != value.trim() {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::InvalidFormat,
            "must not have leading or trailing whitespace",
        ));
    }
    if count > max_chars {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::OutOfRange,
            format!("must contain at most {max_chars} characters"),
        ));
    }
    if value.chars().any(|character| character.is_control()) {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::InvalidFormat,
            "must not contain control characters",
        ));
    }
}

pub(crate) fn validate_body(errors: &mut ValidationErrors, body: &str) {
    if body.trim().is_empty() {
        errors.push(ValidationIssue::new(
            "body",
            ValidationCode::Required,
            "must not be empty or whitespace",
        ));
    }
    if body.len() > MAX_BODY_BYTES {
        errors.push(ValidationIssue::new(
            "body",
            ValidationCode::OutOfRange,
            format!("must contain at most {MAX_BODY_BYTES} UTF-8 bytes"),
        ));
    }
    if body.contains('\0') {
        errors.push(ValidationIssue::new(
            "body",
            ValidationCode::InvalidFormat,
            "must not contain NUL",
        ));
    }
}
