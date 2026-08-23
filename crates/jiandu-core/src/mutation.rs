//! Revision-aware and idempotent memory mutation contracts.

use crate::ids::{
    AgentId, BranchId, Etag, IdempotencyKey, MemoryId, MessageId, Revision, SessionId, Timestamp,
};
use crate::memory::{
    CommittedMessageRange, Confidence, ContentDigest, CreationActor, ExtractionProvenance,
    MemoryRecord, MemoryRelation, MemoryStatus, MemoryType, Provenance, SourceUri, Tag,
    validate_relation_set, validate_relations, validate_tags,
};
use crate::scope::ScopeSelector;
use crate::validation::{
    MAX_BODY_BYTES, MAX_REASON_CHARS, MAX_SUMMARY_CHARS, MAX_TITLE_CHARS, Validate, ValidationCode,
    ValidationErrors, ValidationIssue, validate_body, validate_required_text,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Caller-supplied portable provenance for a newly remembered record.
///
/// `createdBy` is deliberately absent: the service stamps that value from its
/// trusted invocation context when it constructs the authoritative record.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvenanceInput {
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

impl ProvenanceInput {
    /// Resolve input provenance into authoritative provenance using a trusted actor.
    #[must_use]
    pub fn with_created_by(&self, created_by: CreationActor) -> Provenance {
        Provenance {
            created_by,
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            branch_id: self.branch_id.clone(),
            message_ids: self.message_ids.clone(),
            message_range: self.message_range.clone(),
            source_uri: self.source_uri.clone(),
            content_digest: self.content_digest.clone(),
            extraction: self.extraction.clone(),
            confidence: self.confidence,
        }
    }
}

impl Validate for ProvenanceInput {
    fn validate(&self) -> Result<(), ValidationErrors> {
        self.with_created_by(CreationActor::Host).validate()
    }
}

/// Create one memory in an authorized scope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RememberMemoryCommand {
    pub scope: ScopeSelector,
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32), extend("uniqueItems" = true))]
    pub tags: Vec<Tag>,
    pub provenance: ProvenanceInput,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub relations: Vec<MemoryRelation>,
    pub idempotency_key: IdempotencyKey,
}

impl Validate for RememberMemoryCommand {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "title", &self.title, MAX_TITLE_CHARS);
        if let Some(summary) = &self.summary {
            validate_required_text(&mut errors, "summary", summary, MAX_SUMMARY_CHARS);
        }
        validate_body(&mut errors, &self.body);
        validate_tags(&self.tags, &mut errors, "tags");
        validate_relation_set(&self.relations, &mut errors, "relations");
        if let Err(provenance) = self.provenance.validate() {
            errors.extend(provenance, "provenance");
        }
        errors.finish()
    }
}

/// Add/remove update for a record's canonical tags.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TagPatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32), extend("uniqueItems" = true))]
    pub add: Vec<Tag>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32), extend("uniqueItems" = true))]
    pub remove: Vec<Tag>,
}

impl TagPatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

impl Validate for TagPatch {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_tags(&self.add, &mut errors, "add");
        validate_tags(&self.remove, &mut errors, "remove");
        let remove: BTreeSet<_> = self.remove.iter().collect();
        if self.add.iter().any(|tag| remove.contains(tag)) {
            errors.push(ValidationIssue::new(
                "add",
                ValidationCode::Conflict,
                "a tag cannot be added and removed in the same patch",
            ));
        }
        errors.finish()
    }
}

/// Add/remove update for typed relations.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelationPatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub add: Vec<MemoryRelation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub remove: Vec<MemoryRelation>,
}

impl RelationPatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }

    fn validate_for(&self, memory_id: &MemoryId) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_relations(memory_id, &self.add, &mut errors, "add");
        validate_relations(memory_id, &self.remove, &mut errors, "remove");
        let remove: BTreeSet<_> = self.remove.iter().collect();
        if self.add.iter().any(|relation| remove.contains(relation)) {
            errors.push(ValidationIssue::new(
                "add",
                ValidationCode::Conflict,
                "a relation cannot be added and removed in the same patch",
            ));
        }
        errors.finish()
    }
}

/// Explicit set of mutable record fields in `v1alpha1`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 200))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        length(min = 1, max = 65536),
        extend("x-jiandu-maxUtf8Bytes" = MAX_BODY_BYTES)
    )]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<TagPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MemoryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relations: Option<RelationPatch>,
}

impl MemoryPatch {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.status.is_none()
            && self.tags.as_ref().is_none_or(TagPatch::is_empty)
            && self.relations.as_ref().is_none_or(RelationPatch::is_empty)
    }

    fn validate_for(
        &self,
        memory_id: &MemoryId,
        current_status: Option<MemoryStatus>,
    ) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if self.is_empty() {
            errors.push(ValidationIssue::new(
                "patch",
                ValidationCode::Required,
                "must change at least one supported field",
            ));
        }
        if let Some(title) = &self.title {
            validate_required_text(&mut errors, "title", title, MAX_TITLE_CHARS);
        }
        if let Some(body) = &self.body {
            validate_body(&mut errors, body);
        }
        if let Some(tags) = &self.tags
            && let Err(tag_errors) = tags.validate()
        {
            errors.extend(tag_errors, "tags");
        }
        if let Some(relations) = &self.relations
            && let Err(relation_errors) = relations.validate_for(memory_id)
        {
            errors.extend(relation_errors, "relations");
        }
        if let (Some(current), Some(target)) = (current_status, self.status)
            && !current.can_transition_to(target)
        {
            errors.push(ValidationIssue::new(
                "status",
                ValidationCode::InvalidTransition,
                format!("cannot transition from {current:?} to {target:?}"),
            ));
        }
        errors.finish()
    }
}

/// Optimistic, idempotent patch of exactly one record.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateMemoryCommand {
    pub memory_id: MemoryId,
    pub expected_revision: Revision,
    pub patch: MemoryPatch,
    #[schemars(length(min = 1, max = 1000))]
    pub reason: String,
    pub idempotency_key: IdempotencyKey,
}

impl UpdateMemoryCommand {
    /// Validate invariants requiring the current authoritative record.
    pub fn validate_against(&self, current: &MemoryRecord) -> Result<(), ValidationErrors> {
        let mut errors = match self.validate() {
            Ok(()) => ValidationErrors::new(),
            Err(errors) => errors,
        };
        if self.memory_id != current.id {
            errors.push(ValidationIssue::new(
                "memoryId",
                ValidationCode::Conflict,
                "does not match the current record",
            ));
        }
        if self.expected_revision != current.revision {
            errors.push(ValidationIssue::new(
                "expectedRevision",
                ValidationCode::Conflict,
                "does not match the current revision",
            ));
        }
        if let Some(target) = self.patch.status
            && !current.status.can_transition_to(target)
        {
            errors.push(ValidationIssue::new(
                "patch.status",
                ValidationCode::InvalidTransition,
                format!("cannot transition from {:?} to {target:?}", current.status),
            ));
        }
        errors.finish()
    }
}

impl Validate for UpdateMemoryCommand {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "reason", &self.reason, MAX_REASON_CHARS);
        if let Err(patch) = self.patch.validate_for(&self.memory_id, None) {
            errors.extend(patch, "patch");
        }
        errors.finish()
    }
}

/// Optimistic, idempotent forgetting of exactly one record.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForgetMemoryCommand {
    pub memory_id: MemoryId,
    pub expected_revision: Revision,
    #[schemars(length(min = 1, max = 1000))]
    pub reason: String,
    pub idempotency_key: IdempotencyKey,
}

impl Validate for ForgetMemoryCommand {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        validate_required_text(&mut errors, "reason", &self.reason, MAX_REASON_CHARS);
        errors.finish()
    }
}

/// Successful remember receipt. Replays return the same record.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberMemoryResult {
    pub record: MemoryRecord,
    pub idempotent_replay: bool,
}

/// Successful update receipt.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMemoryResult {
    pub record: MemoryRecord,
    pub previous_revision: Revision,
    pub idempotent_replay: bool,
}

/// Successful forget receipt containing no forgotten body.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetMemoryResult {
    pub memory_id: MemoryId,
    pub revision: Revision,
    pub etag: Etag,
    pub forgotten_at: Timestamp,
    pub idempotent_replay: bool,
}
