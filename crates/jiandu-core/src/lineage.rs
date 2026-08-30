//! Agent-neutral Session snapshot event and immutable visibility contracts.
//!
//! These types perform no I/O and do not claim that a host-declared message is
//! committed. A later authoritative resolver must verify that evidence before
//! it can mint a [`SessionSnapshotManifest`].

use crate::ids::{
    BranchId, Etag, EventId, MemoryId, MessageId, Revision, SessionId, StoreRevision, Timestamp,
};
use crate::validation::{Validate, ValidationCode, ValidationErrors, ValidationIssue};
use crate::{BRANCH_SNAPSHOT_EVENT_SCHEMA, SESSION_SNAPSHOT_MANIFEST_SCHEMA};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Closed schema identifier for a host-declared branch snapshot event.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum BranchSnapshotEventSchema {
    #[default]
    #[serde(rename = "jiandu.dev/branch-snapshot-event/v1alpha1")]
    V1Alpha1,
}

impl BranchSnapshotEventSchema {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1Alpha1 => BRANCH_SNAPSHOT_EVENT_SCHEMA,
        }
    }
}

/// Closed snapshot behavior for the first lineage contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchSnapshotMode {
    Snapshot,
}

/// Strict host intent to branch one committed Session lineage into another.
///
/// This event is not itself proof that `through_message_id` was committed or
/// belongs to the source lineage. That proof remains trusted service state.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BranchSnapshotEvent {
    pub schema: BranchSnapshotEventSchema,
    pub event_id: EventId,
    pub source_session_id: SessionId,
    pub source_branch_id: BranchId,
    pub through_message_id: MessageId,
    pub target_session_id: SessionId,
    pub target_branch_id: BranchId,
    pub mode: BranchSnapshotMode,
    pub occurred_at: Timestamp,
}

impl Validate for BranchSnapshotEvent {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if self.source_session_id == self.target_session_id {
            errors.push(ValidationIssue::new(
                "targetSessionId",
                ValidationCode::Conflict,
                "must differ from sourceSessionId so Session scope remains isolated",
            ));
        }
        errors.finish()
    }
}

/// Exact identity of one source Session record visible in a snapshot.
///
/// Consumers must load the exact revision and ETag or fail closed; a later
/// source revision is not a substitute for this anchor.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotRecordAnchor {
    pub memory_id: MemoryId,
    pub revision: Revision,
    pub etag: Etag,
}

/// Closed schema identifier for Jiandu's resolved immutable snapshot manifest.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum SessionSnapshotManifestSchema {
    #[default]
    #[serde(rename = "jiandu.dev/session-snapshot-manifest/v1alpha1")]
    V1Alpha1,
}

impl SessionSnapshotManifestSchema {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1Alpha1 => SESSION_SNAPSHOT_MANIFEST_SCHEMA,
        }
    }
}

/// Immutable, deterministic source-record view resolved for one branch event.
///
/// The manifest contains only Session-scoped source records. Principal and
/// Project records remain shared through their existing scopes and are never
/// copied into `visible_records`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSnapshotManifest {
    pub schema: SessionSnapshotManifestSchema,
    pub event: BranchSnapshotEvent,
    pub source_store_revision: StoreRevision,
    #[schemars(extend("uniqueItems" = true))]
    pub visible_records: Vec<SnapshotRecordAnchor>,
}

impl Validate for SessionSnapshotManifest {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if let Err(event_errors) = self.event.validate() {
            errors.extend(event_errors, "event");
        }

        for (index, anchor) in self.visible_records.iter().enumerate() {
            if anchor.revision.get() > self.source_store_revision.0 {
                errors.push(ValidationIssue::new(
                    format!("visibleRecords[{index}].revision"),
                    ValidationCode::Conflict,
                    "cannot exceed sourceStoreRevision",
                ));
            }
        }

        for (index, pair) in self.visible_records.windows(2).enumerate() {
            match pair[0].memory_id.cmp(&pair[1].memory_id) {
                Ordering::Less => {}
                Ordering::Equal => errors.push(ValidationIssue::new(
                    format!("visibleRecords[{}].memoryId", index + 1),
                    ValidationCode::Duplicate,
                    "contains a duplicate memory ID",
                )),
                Ordering::Greater => errors.push(ValidationIssue::new(
                    format!("visibleRecords[{}].memoryId", index + 1),
                    ValidationCode::InvalidFormat,
                    "must be in strictly ascending memoryId order",
                )),
            }
        }

        errors.finish()
    }
}
