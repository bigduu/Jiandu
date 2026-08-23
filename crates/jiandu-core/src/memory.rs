//! Memory record, lifecycle, relation, tag, and provenance contracts.

use crate::MEMORY_SCHEMA;
use crate::ids::{AgentId, BranchId, Etag, MemoryId, MessageId, Revision, SessionId, Timestamp};
use crate::scope::MemoryScope;
use crate::validation::{
    MAX_BODY_BYTES, MAX_PROVENANCE_MESSAGE_IDS, MAX_RELATIONS, MAX_SUMMARY_CHARS, MAX_TAGS,
    MAX_TITLE_CHARS, Validate, ValidationCode, ValidationErrors, ValidationIssue, validate_body,
    validate_required_text,
};
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Closed memory schema identifiers understood by this crate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum MemorySchema {
    #[default]
    #[serde(rename = "jiandu.dev/memory/v1alpha1")]
    V1Alpha1,
}

impl MemorySchema {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1Alpha1 => MEMORY_SCHEMA,
        }
    }
}

/// Closed memory categories for the v1alpha1 record schema.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Preference,
    Decision,
    Project,
    Fact,
    Feedback,
    Reference,
}

/// Retrieval lifecycle of a non-forgotten memory.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Stale,
    Superseded,
    Contradicted,
    Archived,
}

impl MemoryStatus {
    /// Whether an ordinary v1alpha1 update may perform this transition.
    ///
    /// Archived is terminal. Superseded records retain provenance and may only
    /// be archived. Contradicted records may be corrected back to active/stale
    /// or archived when the contradiction is resolved.
    #[must_use]
    pub const fn can_transition_to(self, target: Self) -> bool {
        if self as u8 == target as u8 {
            return true;
        }
        match self {
            Self::Active => matches!(
                target,
                Self::Stale | Self::Superseded | Self::Contradicted | Self::Archived
            ),
            Self::Stale => matches!(
                target,
                Self::Active | Self::Superseded | Self::Contradicted | Self::Archived
            ),
            Self::Superseded => matches!(target, Self::Archived),
            Self::Contradicted => matches!(target, Self::Active | Self::Stale | Self::Archived),
            Self::Archived => false,
        }
    }
}

/// Typed relation between two opaque memory records.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Supersedes,
    Supports,
    Contradicts,
    DerivedFrom,
    RelatedTo,
}

/// One directed relation to another memory.
#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRelation {
    pub kind: RelationKind,
    pub target_memory_id: MemoryId,
}

/// Canonical lower-case tag.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Tag(
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z0-9][a-z0-9._:-]*$"))] String,
);

impl Tag {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b':' | b'-')
            })
        {
            return Err(ValidationIssue::new(
                "tags",
                ValidationCode::InvalidFormat,
                "tags must be 1 to 64 lower-case ASCII characters using letters, digits, '.', '_', ':' or '-'",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// URI identifying external provenance without making a filesystem path an identity.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SourceUri(
    #[schemars(
        length(min = 3, max = 2048),
        regex(pattern = r"^[A-Za-z][A-Za-z0-9+.-]*:[!-~]+$")
    )]
    String,
);

impl SourceUri {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        let Some((scheme, remainder)) = value.split_once(':') else {
            return Err(source_uri_issue());
        };
        let valid_scheme = scheme
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'));
        if value.len() > 2048
            || !valid_scheme
            || remainder.is_empty()
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(source_uri_issue());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn source_uri_issue() -> ValidationIssue {
    ValidationIssue::new(
        "sourceUri",
        ValidationCode::InvalidFormat,
        "must be an absolute URI of at most 2048 visible ASCII bytes",
    )
}

impl<'de> Deserialize<'de> for SourceUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Algorithm-qualified digest such as `sha256:<hex>`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ContentDigest(
    #[schemars(
        length(min = 3, max = 256),
        regex(pattern = r"^[a-z0-9_]+:[A-Fa-f0-9]+$")
    )]
    String,
);

impl ContentDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationIssue> {
        let value = value.into();
        let Some((algorithm, digest)) = value.split_once(':') else {
            return Err(content_digest_issue());
        };
        if value.len() > 256
            || algorithm.is_empty()
            || !algorithm
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || digest.is_empty()
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(content_digest_issue());
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn content_digest_issue() -> ValidationIssue {
    ValidationIssue::new(
        "contentDigest",
        ValidationCode::InvalidFormat,
        "must be an algorithm-qualified hexadecimal digest",
    )
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

macro_rules! define_unit_score {
    ($name:ident, $field:literal) => {
        #[derive(Clone, Copy, Debug, JsonSchema, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(#[schemars(range(min = 0.0, max = 1.0))] f64);

        impl $name {
            pub fn new(value: f64) -> Result<Self, ValidationIssue> {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(ValidationIssue::new(
                        $field,
                        ValidationCode::OutOfRange,
                        "must be a finite number between 0 and 1 inclusive",
                    ));
                }
                Ok(Self(value))
            }

            #[must_use]
            pub const fn get(self) -> f64 {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

define_unit_score!(Confidence, "provenance.confidence");
define_unit_score!(SearchScore, "score");

/// Actor responsible for creating a memory.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationActor {
    Model,
    User,
    Host,
    Operator,
    Import,
}

/// How memory content was selected or extracted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionMethod {
    Explicit,
    HostRule,
    Model,
    Import,
}

/// Optional extraction provenance.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractionProvenance {
    pub method: ExtractionMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub extractor_version: Option<String>,
}

/// Inclusive range of committed host messages.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommittedMessageRange {
    pub first_message_id: MessageId,
    pub last_message_id: MessageId,
}

/// Portable provenance that contains no host database or prompt-runtime type.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub created_by: CreationActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128))]
    pub message_ids: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_range: Option<CommittedMessageRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<SourceUri>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<ContentDigest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<ExtractionProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
}

impl Validate for Provenance {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if self.message_ids.len() > MAX_PROVENANCE_MESSAGE_IDS {
            errors.push(ValidationIssue::new(
                "messageIds",
                ValidationCode::OutOfRange,
                format!("must contain at most {MAX_PROVENANCE_MESSAGE_IDS} message IDs"),
            ));
        }
        let mut seen = BTreeSet::new();
        for id in &self.message_ids {
            if !seen.insert(id) {
                errors.push(ValidationIssue::new(
                    "messageIds",
                    ValidationCode::Duplicate,
                    format!("contains duplicate message ID {id}"),
                ));
            }
        }
        if !self.message_ids.is_empty() && self.message_range.is_some() {
            errors.push(ValidationIssue::new(
                "messageRange",
                ValidationCode::Conflict,
                "cannot be combined with messageIds",
            ));
        }
        if self.branch_id.is_some() && self.session_id.is_none() {
            errors.push(ValidationIssue::new(
                "branchId",
                ValidationCode::Conflict,
                "requires sessionId",
            ));
        }
        if let Some(extraction) = &self.extraction
            && let Some(version) = &extraction.extractor_version
        {
            validate_required_text(&mut errors, "extraction.extractorVersion", version, 128);
        }
        errors.finish()
    }
}

/// Complete authoritative memory record returned by record APIs.
///
/// The body is structured data from Jiandu's perspective; it is never prompt
/// policy. Optional response fields may be added within v1alpha1, so this
/// response type intentionally does not deny unknown fields on deserialization.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub schema: MemorySchema,
    pub id: MemoryId,
    pub revision: Revision,
    pub etag: Etag,
    pub scope: MemoryScope,
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    pub status: MemoryStatus,
    #[schemars(length(min = 1, max = 200))]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1000))]
    pub summary: Option<String>,
    #[schemars(
        length(min = 1, max = 65536),
        extend("x-jiandu-maxUtf8Bytes" = MAX_BODY_BYTES)
    )]
    pub body: String,
    #[schemars(length(max = 32))]
    pub tags: Vec<Tag>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub provenance: Provenance,
    #[schemars(length(max = 128))]
    pub relations: Vec<MemoryRelation>,
}

impl Validate for MemoryRecord {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "title", &self.title, MAX_TITLE_CHARS);
        if let Some(summary) = &self.summary {
            validate_required_text(&mut errors, "summary", summary, MAX_SUMMARY_CHARS);
        }
        validate_body(&mut errors, &self.body);
        validate_tags(&self.tags, &mut errors, "tags");
        validate_relations(&self.id, &self.relations, &mut errors, "relations");
        if self.created_at.unix_timestamp_nanos() > self.updated_at.unix_timestamp_nanos() {
            errors.push(ValidationIssue::new(
                "updatedAt",
                ValidationCode::InvalidFormat,
                "must be at or after createdAt",
            ));
        }
        if let Err(provenance) = self.provenance.validate() {
            errors.extend(provenance, "provenance");
        }
        errors.finish()
    }
}

/// Compact record projection used by search and list responses.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySummary {
    pub id: MemoryId,
    pub revision: Revision,
    pub etag: Etag,
    pub scope: MemoryScope,
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    pub status: MemoryStatus,
    pub title: String,
    pub summary: String,
    pub tags: Vec<Tag>,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<SearchScore>,
}

impl Validate for MemorySummary {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "title", &self.title, MAX_TITLE_CHARS);
        validate_required_text(&mut errors, "summary", &self.summary, MAX_SUMMARY_CHARS);
        validate_tags(&self.tags, &mut errors, "tags");
        errors.finish()
    }
}

pub(crate) fn validate_tags(tags: &[Tag], errors: &mut ValidationErrors, field: &str) {
    if tags.len() > MAX_TAGS {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::OutOfRange,
            format!("must contain at most {MAX_TAGS} tags"),
        ));
    }
    let mut seen = BTreeSet::new();
    for tag in tags {
        if !seen.insert(tag) {
            errors.push(ValidationIssue::new(
                field,
                ValidationCode::Duplicate,
                format!("contains duplicate tag {tag}"),
            ));
        }
    }
}

pub(crate) fn validate_relations(
    source_id: &MemoryId,
    relations: &[MemoryRelation],
    errors: &mut ValidationErrors,
    field: &str,
) {
    validate_relation_set(relations, errors, field);
    for relation in relations {
        if relation.target_memory_id == *source_id {
            errors.push(ValidationIssue::new(
                field,
                ValidationCode::Conflict,
                "must not contain a self-relation",
            ));
        }
    }
}

pub(crate) fn validate_relation_set(
    relations: &[MemoryRelation],
    errors: &mut ValidationErrors,
    field: &str,
) {
    if relations.len() > MAX_RELATIONS {
        errors.push(ValidationIssue::new(
            field,
            ValidationCode::OutOfRange,
            format!("must contain at most {MAX_RELATIONS} relations"),
        ));
    }
    let mut seen = BTreeSet::new();
    for relation in relations {
        if !seen.insert(relation) {
            errors.push(ValidationIssue::new(
                field,
                ValidationCode::Duplicate,
                format!(
                    "contains duplicate {:?} relation to {}",
                    relation.kind, relation.target_memory_id
                ),
            ));
        }
    }
}

/// Contract bounds exported for documentation and conformance clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryValidationBounds {
    pub max_title_chars: usize,
    pub max_summary_chars: usize,
    pub max_body_bytes: usize,
    pub max_tags: usize,
    pub max_relations: usize,
}

impl Default for MemoryValidationBounds {
    fn default() -> Self {
        Self {
            max_title_chars: MAX_TITLE_CHARS,
            max_summary_chars: MAX_SUMMARY_CHARS,
            max_body_bytes: MAX_BODY_BYTES,
            max_tags: MAX_TAGS,
            max_relations: MAX_RELATIONS,
        }
    }
}
