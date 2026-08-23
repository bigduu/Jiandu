//! Pure DTOs for the committed snake_case Markdown frontmatter contract.
//!
//! Parsing files, normalizing line endings, hashing content, and all other I/O
//! remain outside `jiandu-core`.

use crate::ids::{
    AgentId, BranchId, Etag, MemoryId, MessageId, PrincipalId, ProjectId, Revision, SessionId,
    Timestamp,
};
use crate::memory::{
    CommittedMessageRange, Confidence, ContentDigest, CreationActor, ExtractionMethod,
    ExtractionProvenance, MemoryRecord, MemoryRelation, MemorySchema, MemoryStatus, MemoryType,
    Provenance, RelationKind, SourceUri, Tag,
};
use crate::scope::MemoryScope;
use crate::validation::{Validate, ValidationErrors};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Resolved authoritative scope serialized in canonical frontmatter.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontmatterScope {
    Principal { principal_id: PrincipalId },
    Project { project_id: ProjectId },
    Session { session_id: SessionId },
    InstanceGlobal {},
}

impl From<&MemoryScope> for FrontmatterScope {
    fn from(scope: &MemoryScope) -> Self {
        match scope {
            MemoryScope::Principal { principal_id } => Self::Principal {
                principal_id: principal_id.clone(),
            },
            MemoryScope::Project { project_id } => Self::Project {
                project_id: project_id.clone(),
            },
            MemoryScope::Session { session_id } => Self::Session {
                session_id: session_id.clone(),
            },
            MemoryScope::InstanceGlobal {} => Self::InstanceGlobal {},
        }
    }
}

impl From<FrontmatterScope> for MemoryScope {
    fn from(scope: FrontmatterScope) -> Self {
        match scope {
            FrontmatterScope::Principal { principal_id } => Self::Principal { principal_id },
            FrontmatterScope::Project { project_id } => Self::Project { project_id },
            FrontmatterScope::Session { session_id } => Self::Session { session_id },
            FrontmatterScope::InstanceGlobal {} => Self::InstanceGlobal {},
        }
    }
}

/// Snake_case form of a typed memory relation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrontmatterRelation {
    pub kind: RelationKind,
    pub target_memory_id: MemoryId,
}

impl From<&MemoryRelation> for FrontmatterRelation {
    fn from(relation: &MemoryRelation) -> Self {
        Self {
            kind: relation.kind,
            target_memory_id: relation.target_memory_id.clone(),
        }
    }
}

impl From<FrontmatterRelation> for MemoryRelation {
    fn from(relation: FrontmatterRelation) -> Self {
        Self {
            kind: relation.kind,
            target_memory_id: relation.target_memory_id,
        }
    }
}

/// Snake_case form of extraction provenance.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrontmatterExtraction {
    pub method: ExtractionMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub extractor_version: Option<String>,
}

impl From<&ExtractionProvenance> for FrontmatterExtraction {
    fn from(extraction: &ExtractionProvenance) -> Self {
        Self {
            method: extraction.method,
            extractor_version: extraction.extractor_version.clone(),
        }
    }
}

impl From<FrontmatterExtraction> for ExtractionProvenance {
    fn from(extraction: FrontmatterExtraction) -> Self {
        Self {
            method: extraction.method,
            extractor_version: extraction.extractor_version,
        }
    }
}

/// Snake_case form of an inclusive committed-message range.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrontmatterMessageRange {
    pub first_message_id: MessageId,
    pub last_message_id: MessageId,
}

impl From<&CommittedMessageRange> for FrontmatterMessageRange {
    fn from(range: &CommittedMessageRange) -> Self {
        Self {
            first_message_id: range.first_message_id.clone(),
            last_message_id: range.last_message_id.clone(),
        }
    }
}

impl From<FrontmatterMessageRange> for CommittedMessageRange {
    fn from(range: FrontmatterMessageRange) -> Self {
        Self {
            first_message_id: range.first_message_id,
            last_message_id: range.last_message_id,
        }
    }
}

/// Portable provenance in canonical snake_case form.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrontmatterProvenance {
    pub created_by: CreationActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub message_ids: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_range: Option<FrontmatterMessageRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<SourceUri>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<ContentDigest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<FrontmatterExtraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
}

impl From<&Provenance> for FrontmatterProvenance {
    fn from(provenance: &Provenance) -> Self {
        Self {
            created_by: provenance.created_by,
            agent_id: provenance.agent_id.clone(),
            session_id: provenance.session_id.clone(),
            branch_id: provenance.branch_id.clone(),
            message_ids: provenance.message_ids.clone(),
            message_range: provenance.message_range.as_ref().map(Into::into),
            source_uri: provenance.source_uri.clone(),
            content_digest: provenance.content_digest.clone(),
            extraction: provenance.extraction.as_ref().map(Into::into),
            confidence: provenance.confidence,
        }
    }
}

impl From<FrontmatterProvenance> for Provenance {
    fn from(provenance: FrontmatterProvenance) -> Self {
        Self {
            created_by: provenance.created_by,
            agent_id: provenance.agent_id,
            session_id: provenance.session_id,
            branch_id: provenance.branch_id,
            message_ids: provenance.message_ids,
            message_range: provenance.message_range.map(Into::into),
            source_uri: provenance.source_uri,
            content_digest: provenance.content_digest,
            extraction: provenance.extraction.map(Into::into),
            confidence: provenance.confidence,
        }
    }
}

/// Header of one canonical `v1alpha1` Markdown memory document.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryFrontmatterV1Alpha1 {
    pub schema: MemorySchema,
    pub id: MemoryId,
    pub revision: Revision,
    pub scope: FrontmatterScope,
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    pub status: MemoryStatus,
    #[schemars(length(min = 1, max = 200))]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1000))]
    pub summary: Option<String>,
    #[schemars(length(max = 32), extend("uniqueItems" = true))]
    pub tags: Vec<Tag>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub provenance: FrontmatterProvenance,
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub relations: Vec<FrontmatterRelation>,
}

impl MemoryFrontmatterV1Alpha1 {
    /// Project an API record into canonical frontmatter; ETag and body are not header fields.
    #[must_use]
    pub fn from_record(record: &MemoryRecord) -> Self {
        Self {
            schema: record.schema,
            id: record.id.clone(),
            revision: record.revision,
            scope: (&record.scope).into(),
            memory_type: record.memory_type,
            status: record.status,
            title: record.title.clone(),
            summary: record.summary.clone(),
            tags: record.tags.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            provenance: (&record.provenance).into(),
            relations: record.relations.iter().map(Into::into).collect(),
        }
    }

    /// Reconstruct the API record using the ETag and Markdown body supplied by the caller.
    #[must_use]
    pub fn into_record(self, etag: Etag, body: String) -> MemoryRecord {
        MemoryRecord {
            schema: self.schema,
            id: self.id,
            revision: self.revision,
            etag,
            scope: self.scope.into(),
            memory_type: self.memory_type,
            status: self.status,
            title: self.title,
            summary: self.summary,
            body,
            tags: self.tags,
            created_at: self.created_at,
            updated_at: self.updated_at,
            provenance: self.provenance.into(),
            relations: self.relations.into_iter().map(Into::into).collect(),
        }
    }

    /// Validate frontmatter and body through the single authoritative record policy.
    pub fn validate_document(&self, body: &str) -> Result<(), ValidationErrors> {
        let etag = Etag::new("frontmatter-validation")?;
        self.clone().into_record(etag, body.into()).validate()
    }
}
