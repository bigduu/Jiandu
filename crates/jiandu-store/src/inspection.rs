//! Side-effect-free validation and deterministic authorized portable exports.

use crate::document::{MAX_CANONICAL_DOCUMENT_BYTES, decode_canonical_document};
use crate::layout::{self, FileIdentity, StoreDirectory};
use crate::{AuditSequence, CanonicalStore, StoreError, StoreId, StoreMetadata};
use jiandu_core::{
    AgentId, BranchId, CommittedMessageRange, Confidence, ContentDigest, CreationActor, Etag,
    ExtractionProvenance, MemoryId, MemoryRelation, MemorySchema, MemoryScope, MemoryStatus,
    MemoryType, MessageId, PrincipalId, Provenance, Revision, SessionId, SourceUri, StoreRevision,
    Tag, Timestamp, TrustedRequestContext, Validate,
};
use schemars::{JsonSchema, schema_for};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Current canonical validation-report format, independent of the store format.
pub const VALIDATION_REPORT_FORMAT_VERSION: &str = "jiandu.validation-report/v1alpha1";
/// Current canonical portable-export format, independent of the store format.
pub const PORTABLE_EXPORT_FORMAT_VERSION: &str = "jiandu.portable-export/v1alpha1";
/// Current body-free portable tombstone projection format.
pub const PORTABLE_TOMBSTONE_FORMAT_VERSION: &str = "jiandu.portable-tombstone/v1alpha1";

const MAX_FINDINGS: usize = 256;
const MAX_REQUESTED_SCOPES: usize = 64;
const MAX_DISCOVERED_SCOPES: usize = 10_000;
const MAX_EXPORT_ITEMS: usize = 10_000;
const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_SCAN_BYTES: u64 = 67_108_864;
const MAX_REPORT_BYTES: usize = 1_048_576;
const MAX_EXPORT_BYTES: usize = 67_108_864;

/// Domain-separated SHA-256 digest of a canonical report or bundle payload.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ExportDigest(
    #[schemars(length(min = 71, max = 71), regex(pattern = r"^sha256:[0-9a-f]{64}$"))] String,
);

impl ExportDigest {
    fn from_payload(domain: &[u8], payload: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        hasher.update(payload);
        let digest = hasher.finalize();
        let mut encoded = String::with_capacity(71);
        encoded.push_str("sha256:");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in digest {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(encoded)
    }

    #[must_use]
    /// Return the lowercase domain-prefixed SHA-256 representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.0.len() == 71
            && self.0.starts_with("sha256:")
            && self.0[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(())
        } else {
            Err(StoreError::InvalidRequest)
        }
    }
}

impl<'de> Deserialize<'de> for ExportDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Self(String::deserialize(deserializer)?);
        value.validate().map_err(D::Error::custom)?;
        Ok(value)
    }
}

/// Stable metadata watermark captured under one read-only coordinated snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotWatermark {
    /// Canonical store revision at the coordinated snapshot.
    pub store_revision: StoreRevision,
    /// Independent private-audit sequence at the same snapshot.
    pub audit_sequence: AuditSequence,
}

/// Whether validation was limited to explicit scopes or covered the store.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    Scoped,
    AllScopes,
}

/// Stable logical artifact category; never a canonical or ambient path.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ValidationArtifact {
    StoreMetadata,
    Layout,
    Transaction,
    Record,
    Tombstone,
    Receipt,
    Result,
    Audit,
    Witness,
    Snapshot,
}

/// Closed, stable and secret-safe validation categories.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    StoreMetadataInconsistent,
    UnsupportedStoreVersion,
    LayoutInconsistent,
    UnsafeEntry,
    ActiveTransaction,
    RecordMalformed,
    RecordIdMismatch,
    ScopePathMismatch,
    ShardMismatch,
    DuplicateMemoryId,
    RecordTombstoneConflict,
    TombstoneInconsistent,
    ReceiptInconsistent,
    ResultInconsistent,
    AuditInconsistent,
    WitnessInconsistent,
    SnapshotChanged,
    ScanLimitExceeded,
    FindingLimitReached,
}

/// One path/body/credential/key/query-free validation finding.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationFinding {
    /// Closed machine-readable finding category.
    pub code: ValidationCode,
    /// Logical artifact class affected by the finding.
    pub artifact: ValidationArtifact,
    /// Authorized or admin-visible logical scope when safely known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MemoryScope>,
    /// Opaque memory identity only when safely bound and authorized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<MemoryId>,
}

/// Strict canonical validation report. Invalid stores can omit identity and
/// watermark while still returning closed safe findings.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationReport {
    /// Validation report format identifier.
    #[schemars(
        length(min = 33, max = 33),
        regex(pattern = r"^jiandu\.validation-report/v1alpha1$")
    )]
    pub format_version: String,
    /// Scope visibility mode used by this inspection.
    pub mode: ValidationMode,
    /// Opaque source identity, omitted when metadata is not authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_store_id: Option<StoreId>,
    /// Snapshot watermark, omitted together with source identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotWatermark>,
    /// Canonically sorted scopes inspected or discovered.
    #[schemars(length(max = 10000), extend("uniqueItems" = true))]
    pub inspected_scopes: Vec<MemoryScope>,
    /// Canonically sorted, deduplicated safe findings.
    #[schemars(length(max = 257), extend("uniqueItems" = true))]
    pub findings: Vec<ValidationFinding>,
    /// Whether a bounded scan stopped before visiting the whole selection.
    pub truncated: bool,
    /// Domain-separated digest of every preceding report field.
    pub digest: ExportDigest,
}

/// Closed portable projection of every public record field. This intentionally
/// does not reuse response-extensible `MemoryRecord` deserialization.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableMemoryRecord {
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
        extend("x-jiandu-maxUtf8Bytes" = 65536)
    )]
    pub body: String,
    #[schemars(length(max = 32), extend("uniqueItems" = true))]
    pub tags: Vec<Tag>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub provenance: PortableProvenance,
    #[schemars(length(max = 128), extend("uniqueItems" = true))]
    pub relations: Vec<MemoryRelation>,
}

/// Closed portable projection of every public provenance field.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableProvenance {
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

impl From<Provenance> for PortableProvenance {
    fn from(value: Provenance) -> Self {
        Self {
            created_by: value.created_by,
            agent_id: value.agent_id,
            session_id: value.session_id,
            branch_id: value.branch_id,
            message_ids: value.message_ids,
            message_range: value.message_range,
            source_uri: value.source_uri,
            content_digest: value.content_digest,
            extraction: value.extraction,
            confidence: value.confidence,
        }
    }
}

impl From<PortableProvenance> for Provenance {
    fn from(value: PortableProvenance) -> Self {
        Self {
            created_by: value.created_by,
            agent_id: value.agent_id,
            session_id: value.session_id,
            branch_id: value.branch_id,
            message_ids: value.message_ids,
            message_range: value.message_range,
            source_uri: value.source_uri,
            content_digest: value.content_digest,
            extraction: value.extraction,
            confidence: value.confidence,
        }
    }
}

impl From<jiandu_core::MemoryRecord> for PortableMemoryRecord {
    fn from(value: jiandu_core::MemoryRecord) -> Self {
        Self {
            schema: value.schema,
            id: value.id,
            revision: value.revision,
            etag: value.etag,
            scope: value.scope,
            memory_type: value.memory_type,
            status: value.status,
            title: value.title,
            summary: value.summary,
            body: value.body,
            tags: value.tags,
            created_at: value.created_at,
            updated_at: value.updated_at,
            provenance: value.provenance.into(),
            relations: value.relations,
        }
    }
}

impl From<PortableMemoryRecord> for jiandu_core::MemoryRecord {
    fn from(value: PortableMemoryRecord) -> Self {
        Self {
            schema: value.schema,
            id: value.id,
            revision: value.revision,
            etag: value.etag,
            scope: value.scope,
            memory_type: value.memory_type,
            status: value.status,
            title: value.title,
            summary: value.summary,
            body: value.body,
            tags: value.tags,
            created_at: value.created_at,
            updated_at: value.updated_at,
            provenance: value.provenance.into(),
            relations: value.relations,
        }
    }
}

/// Body-free portable marker for one protected forgotten identity.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableTombstone {
    #[schemars(
        length(min = 34, max = 34),
        regex(pattern = r"^jiandu\.portable-tombstone/v1alpha1$")
    )]
    pub format_version: String,
    pub memory_id: MemoryId,
    pub scope: MemoryScope,
    pub revision: Revision,
    pub etag: Etag,
    pub forgotten_at: Timestamp,
    pub store_revision: StoreRevision,
    pub audit_sequence: AuditSequence,
}

/// Deterministic portable state for an explicitly authorized scope set.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableExportBundle {
    #[schemars(
        length(min = 31, max = 31),
        regex(pattern = r"^jiandu\.portable-export/v1alpha1$")
    )]
    pub format_version: String,
    #[schemars(
        length(min = 21, max = 21),
        regex(pattern = r"^jiandu\.store/v1alpha3$")
    )]
    pub source_store_format: String,
    pub source_store_id: StoreId,
    pub snapshot: SnapshotWatermark,
    #[schemars(length(max = 10000), extend("uniqueItems" = true))]
    pub scopes: Vec<MemoryScope>,
    #[schemars(length(max = 10000), extend("uniqueItems" = true))]
    pub records: Vec<PortableMemoryRecord>,
    #[schemars(length(max = 10000), extend("uniqueItems" = true))]
    pub tombstones: Vec<PortableTombstone>,
    pub digest: ExportDigest,
}

/// Private-field all-scope export capability, independently grantable from
/// ordinary read/write/destructive permissions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedExportAdmin {
    principal_id: PrincipalId,
}

/// Private-field full-store validation capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedValidationAdmin {
    principal_id: PrincipalId,
}

impl crate::AuthorizedScopes {
    /// Authorize an operator-only export across every canonical scope.
    pub fn authorize_all_scope_export(
        &self,
        context: &TrustedRequestContext,
    ) -> Result<AuthorizedExportAdmin, StoreError> {
        authorize_admin(context, &self.principal_id, "memory:admin:export_all")?;
        Ok(AuthorizedExportAdmin {
            principal_id: context.principal_id.clone(),
        })
    }

    /// Authorize a full-store validation report, separately from export.
    pub fn authorize_store_validation(
        &self,
        context: &TrustedRequestContext,
    ) -> Result<AuthorizedValidationAdmin, StoreError> {
        authorize_admin(context, &self.principal_id, "memory:admin:validate_store")?;
        Ok(AuthorizedValidationAdmin {
            principal_id: context.principal_id.clone(),
        })
    }
}

fn authorize_admin(
    context: &TrustedRequestContext,
    expected_principal: &PrincipalId,
    required_grant: &str,
) -> Result<(), StoreError> {
    context
        .validate()
        .map_err(|_| StoreError::Unauthenticated)?;
    if &context.principal_id != expected_principal
        || !context
            .grants
            .iter()
            .any(|grant| grant.as_str() == required_grant)
    {
        return Err(StoreError::Forbidden);
    }
    Ok(())
}

/// Offline, coordinated inspector. It opens only an existing root/LOCK and
/// acquires the same kernel ownership lock as the writer without publishing a
/// new owner or modifying the lock bytes.
pub struct ReadOnlyStoreInspector {
    root: StoreDirectory,
    lock: crate::lock::StoreLock,
}

impl ReadOnlyStoreInspector {
    /// Open an existing store for coordinated inspection without publishing a
    /// lock owner, initialization, migration, recovery, or repair.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = StoreDirectory::open(data_dir.as_ref(), false)?;
        root.validate_private_root()?;
        let lock = crate::lock::StoreLock::acquire(&root, false)?;
        root.validate_ambient_identity()?;
        lock.validate_ownership(&root)?;
        Ok(Self { root, lock })
    }

    /// Validate only explicitly authorized scopes while the writer is stopped.
    pub fn validate_scopes(
        &self,
        authorized: &crate::AuthorizedScopes,
        scopes: &[MemoryScope],
    ) -> Result<ValidationReport, StoreError> {
        let scopes = resolve_scopes(authorized, scopes)?;
        self.validate_source()?;
        let report = inspect(&self.root, Selection::Scoped(scopes), None)?.report;
        self.validate_source()?;
        Ok(report)
    }

    /// Validate the complete store under the distinct admin capability.
    pub fn validate_all(
        &self,
        authorization: &AuthorizedValidationAdmin,
    ) -> Result<ValidationReport, StoreError> {
        let _ = &authorization.principal_id;
        self.validate_source()?;
        let report = inspect(&self.root, Selection::All, None)?.report;
        self.validate_source()?;
        Ok(report)
    }

    /// Export only explicitly authorized scopes while the writer is stopped.
    pub fn export_scopes(
        &self,
        authorized: &crate::AuthorizedScopes,
        scopes: &[MemoryScope],
    ) -> Result<PortableExportBundle, StoreError> {
        let scopes = resolve_scopes(authorized, scopes)?;
        self.validate_source()?;
        let bundle = export(&self.root, Selection::Scoped(scopes), None)?;
        self.validate_source()?;
        Ok(bundle)
    }

    /// Export all canonical scopes under the distinct admin capability.
    pub fn export_all(
        &self,
        authorization: &AuthorizedExportAdmin,
    ) -> Result<PortableExportBundle, StoreError> {
        let _ = &authorization.principal_id;
        self.validate_source()?;
        let bundle = export(&self.root, Selection::All, None)?;
        self.validate_source()?;
        Ok(bundle)
    }

    fn validate_source(&self) -> Result<(), StoreError> {
        self.root.validate_ambient_identity()?;
        self.lock.validate_ownership(&self.root)
    }
}

impl CanonicalStore {
    /// Validate explicit scopes using the live owner's already-held lock.
    pub fn validate_scopes(
        &self,
        authorized: &crate::AuthorizedScopes,
        scopes: &[MemoryScope],
    ) -> Result<ValidationReport, StoreError> {
        let scopes = resolve_scopes(authorized, scopes)?;
        self.validate_ownership()?;
        let output = inspect(&self.root, Selection::Scoped(scopes), Some(&self.metadata))?;
        self.validate_ownership()?;
        Ok(output.report)
    }

    /// Validate the complete live store under an admin capability.
    pub fn validate_all(
        &self,
        authorization: &AuthorizedValidationAdmin,
    ) -> Result<ValidationReport, StoreError> {
        let _ = &authorization.principal_id;
        self.validate_ownership()?;
        let output = inspect(&self.root, Selection::All, Some(&self.metadata))?;
        self.validate_ownership()?;
        Ok(output.report)
    }

    /// Export explicit scopes using the live owner's already-held lock.
    pub fn export_scopes(
        &self,
        authorized: &crate::AuthorizedScopes,
        scopes: &[MemoryScope],
    ) -> Result<PortableExportBundle, StoreError> {
        let scopes = resolve_scopes(authorized, scopes)?;
        self.validate_ownership()?;
        let bundle = export(&self.root, Selection::Scoped(scopes), Some(&self.metadata))?;
        self.validate_ownership()?;
        Ok(bundle)
    }

    /// Export the complete live store under an admin capability.
    pub fn export_all(
        &self,
        authorization: &AuthorizedExportAdmin,
    ) -> Result<PortableExportBundle, StoreError> {
        let _ = &authorization.principal_id;
        self.validate_ownership()?;
        let bundle = export(&self.root, Selection::All, Some(&self.metadata))?;
        self.validate_ownership()?;
        Ok(bundle)
    }
}

fn resolve_scopes(
    authorized: &crate::AuthorizedScopes,
    scopes: &[MemoryScope],
) -> Result<Vec<MemoryScope>, StoreError> {
    if scopes.is_empty() || scopes.len() > MAX_REQUESTED_SCOPES {
        return Err(StoreError::InvalidRequest);
    }
    let mut resolved = scopes.to_vec();
    resolved.sort_by_key(scope_key);
    if resolved.windows(2).any(|pair| pair[0] == pair[1])
        || resolved
            .iter()
            .any(|scope| authorized.authorize_exact(scope).is_none())
    {
        return Err(StoreError::Forbidden);
    }
    Ok(resolved)
}

#[derive(Clone)]
enum Selection {
    Scoped(Vec<MemoryScope>),
    All,
}

struct InspectionOutput {
    report: ValidationReport,
    metadata: Option<StoreMetadata>,
    records: Vec<PortableMemoryRecord>,
    tombstones: Vec<PortableTombstone>,
    scopes: Vec<MemoryScope>,
}

fn inspect(
    root: &StoreDirectory,
    selection: Selection,
    expected_metadata: Option<&StoreMetadata>,
) -> Result<InspectionOutput, StoreError> {
    let mode = match selection {
        Selection::Scoped(_) => ValidationMode::Scoped,
        Selection::All => ValidationMode::AllScopes,
    };
    let mut collector = FindingCollector::default();
    let mut budget = ScanBudget::new();
    let begin = inspect_metadata(root, &mut collector, &mut budget)?;
    if expected_metadata.is_some_and(|expected| begin.metadata.as_ref() != Some(expected)) {
        collector.push(
            ValidationCode::SnapshotChanged,
            ValidationArtifact::Snapshot,
            None,
            None,
        );
    }
    if layout::validate_layout(root).is_err() {
        collector.push(
            ValidationCode::LayoutInconsistent,
            ValidationArtifact::Layout,
            None,
            None,
        );
    }
    inspect_active_transactions(root, &mut collector, &mut budget);
    if let Some(metadata) = &begin.metadata
        && crate::store::validate_audit_genesis_bounded(root, metadata, &mut budget).is_err()
    {
        collector.push(
            ValidationCode::AuditInconsistent,
            ValidationArtifact::Audit,
            None,
            None,
        );
    }

    let requested_scopes = match &selection {
        Selection::Scoped(scopes) => scopes.clone(),
        Selection::All => Vec::new(),
    };
    let mut records = Vec::new();
    let mut tombstones = Vec::new();
    let mut observed_scopes = BTreeMap::<String, MemoryScope>::new();
    let tombstoned_keys =
        begin.metadata.as_ref().and_then(|_| {
            match bounded_tombstone_storage_keys(root, &mut budget, &mut collector) {
                Ok(keys) => Some(keys),
                Err(_) => {
                    collector.push(
                        ValidationCode::TombstoneInconsistent,
                        ValidationArtifact::Tombstone,
                        None,
                        None,
                    );
                    None
                }
            }
        });

    match &selection {
        Selection::Scoped(scopes) => {
            for scope in scopes {
                observed_scopes.insert(scope_key(scope), scope.clone());
                if let (Some(metadata), Some(tombstoned_keys)) = (&begin.metadata, &tombstoned_keys)
                {
                    scan_record_scope(
                        root,
                        metadata,
                        scope,
                        tombstoned_keys,
                        &mut records,
                        &mut collector,
                        &mut budget,
                    );
                }
                if let Some(metadata) = &begin.metadata {
                    scan_tombstone_scope(
                        root,
                        metadata,
                        scope,
                        &mut tombstones,
                        &mut collector,
                        &mut budget,
                    );
                }
            }
        }
        Selection::All => {
            if let (Some(metadata), Some(tombstoned_keys)) = (&begin.metadata, &tombstoned_keys) {
                scan_all_records(
                    root,
                    metadata,
                    tombstoned_keys,
                    &mut records,
                    &mut observed_scopes,
                    &mut collector,
                    &mut budget,
                );
            }
            if let Some(metadata) = &begin.metadata {
                scan_all_tombstones(
                    root,
                    metadata,
                    &mut tombstones,
                    &mut observed_scopes,
                    &mut collector,
                    &mut budget,
                );
                for issue in crate::idempotency::inspect_ledger(root, metadata, &mut budget) {
                    let (code, artifact) = match issue {
                        crate::idempotency::LedgerIssue::Receipt => (
                            ValidationCode::ReceiptInconsistent,
                            ValidationArtifact::Receipt,
                        ),
                        crate::idempotency::LedgerIssue::Result => (
                            ValidationCode::ResultInconsistent,
                            ValidationArtifact::Result,
                        ),
                        crate::idempotency::LedgerIssue::Audit => {
                            (ValidationCode::AuditInconsistent, ValidationArtifact::Audit)
                        }
                        crate::idempotency::LedgerIssue::Tombstone => (
                            ValidationCode::TombstoneInconsistent,
                            ValidationArtifact::Tombstone,
                        ),
                        crate::idempotency::LedgerIssue::Witness => (
                            ValidationCode::WitnessInconsistent,
                            ValidationArtifact::Witness,
                        ),
                        crate::idempotency::LedgerIssue::Limit => (
                            ValidationCode::ScanLimitExceeded,
                            ValidationArtifact::Snapshot,
                        ),
                        crate::idempotency::LedgerIssue::Unsafe => {
                            (ValidationCode::UnsafeEntry, ValidationArtifact::Snapshot)
                        }
                    };
                    collector.push(code, artifact, None, None);
                }
                if budget.exceeded {
                    record_scan_limit(&mut collector, &mut budget);
                }
                inspect_quarantine_receipts(root, metadata, &mut collector, &mut budget);
            }
        }
    }

    detect_duplicates(&records, &tombstones, &mut collector);
    let end = inspect_metadata(root, &mut FindingCollector::discarding(), &mut budget)?;
    if begin.raw_bytes != end.raw_bytes || begin.metadata != end.metadata {
        collector.push(
            ValidationCode::SnapshotChanged,
            ValidationArtifact::Snapshot,
            None,
            None,
        );
    }
    #[cfg(test)]
    layout::run_test_hook(
        layout::TestHookPoint::InspectionTombstoneRescan,
        OsStr::new(layout::TOMBSTONES_DIR),
    );
    if begin.metadata.is_some() {
        match (
            tombstoned_keys,
            bounded_tombstone_storage_keys(root, &mut budget, &mut collector),
        ) {
            (Some(begin), Ok(end)) if begin == end => {}
            (_, Ok(_)) => collector.push(
                ValidationCode::SnapshotChanged,
                ValidationArtifact::Snapshot,
                None,
                None,
            ),
            (_, Err(_)) => {
                collector.push(
                    ValidationCode::TombstoneInconsistent,
                    ValidationArtifact::Tombstone,
                    None,
                    None,
                );
                collector.push(
                    ValidationCode::SnapshotChanged,
                    ValidationArtifact::Snapshot,
                    None,
                    None,
                );
            }
        }
    }

    if budget.exceeded {
        record_scan_limit(&mut collector, &mut budget);
    }

    records.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| scope_key(&left.scope).cmp(&scope_key(&right.scope)))
    });
    tombstones.sort_by(|left, right| {
        left.memory_id
            .cmp(&right.memory_id)
            .then_with(|| scope_key(&left.scope).cmp(&scope_key(&right.scope)))
    });
    let scopes = if mode == ValidationMode::Scoped {
        requested_scopes
    } else {
        observed_scopes.into_values().collect()
    };
    let mut report = ValidationReport {
        format_version: VALIDATION_REPORT_FORMAT_VERSION.to_owned(),
        mode,
        source_store_id: begin
            .metadata
            .as_ref()
            .map(|metadata| metadata.store_id.clone()),
        snapshot: begin.metadata.as_ref().map(|metadata| SnapshotWatermark {
            store_revision: metadata.store_revision,
            audit_sequence: metadata.audit_sequence,
        }),
        inspected_scopes: scopes.clone(),
        findings: collector.finish(),
        truncated: collector.truncated || budget.exceeded,
        digest: ExportDigest(String::new()),
    };
    report.digest = report.expected_digest()?;
    Ok(InspectionOutput {
        report,
        metadata: begin.metadata,
        records,
        tombstones,
        scopes,
    })
}

fn export(
    root: &StoreDirectory,
    selection: Selection,
    expected_metadata: Option<&StoreMetadata>,
) -> Result<PortableExportBundle, StoreError> {
    let inspected = inspect(root, selection, expected_metadata)?;
    if !inspected.report.findings.is_empty() || inspected.report.truncated {
        return Err(StoreError::ValidationFailed);
    }
    let metadata = inspected.metadata.ok_or(StoreError::InvalidStoreMetadata)?;
    if inspected
        .records
        .len()
        .saturating_add(inspected.tombstones.len())
        > MAX_EXPORT_ITEMS
    {
        return Err(StoreError::InvalidRequest);
    }
    let mut bundle = PortableExportBundle {
        format_version: PORTABLE_EXPORT_FORMAT_VERSION.to_owned(),
        source_store_format: metadata.format_version,
        source_store_id: metadata.store_id,
        snapshot: SnapshotWatermark {
            store_revision: metadata.store_revision,
            audit_sequence: metadata.audit_sequence,
        },
        scopes: inspected.scopes,
        records: inspected.records,
        tombstones: inspected.tombstones,
        digest: ExportDigest(String::new()),
    };
    bundle.digest = bundle.expected_digest()?;
    if bundle.canonical_bytes()?.len() > MAX_EXPORT_BYTES {
        return Err(StoreError::InvalidRequest);
    }
    Ok(bundle)
}

#[derive(Default)]
struct FindingCollector {
    findings: Vec<ValidationFinding>,
    truncated: bool,
    discard: bool,
}

struct ScanBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
    remaining_items: usize,
    exceeded: bool,
}

impl ScanBudget {
    const fn new() -> Self {
        Self {
            remaining_entries: MAX_SCAN_ENTRIES,
            remaining_bytes: MAX_SCAN_BYTES,
            remaining_items: MAX_EXPORT_ITEMS,
            exceeded: false,
        }
    }

    fn consume_entry(&mut self) -> bool {
        if self.exceeded || self.remaining_entries == 0 {
            self.exceeded = true;
            return false;
        }
        self.remaining_entries -= 1;
        true
    }

    fn consume_bytes(&mut self, bytes: u64) -> bool {
        if self.exceeded || bytes > self.remaining_bytes {
            self.exceeded = true;
            return false;
        }
        self.remaining_bytes -= bytes;
        true
    }

    fn consume_item(&mut self) -> bool {
        if self.exceeded || self.remaining_items == 0 {
            self.exceeded = true;
            return false;
        }
        self.remaining_items -= 1;
        true
    }
}

impl crate::idempotency::LedgerScanBudget for ScanBudget {
    fn consume_entry(&mut self) -> bool {
        Self::consume_entry(self)
    }

    fn consume_bytes(&mut self, bytes: u64) -> bool {
        Self::consume_bytes(self, bytes)
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }
}

fn record_scan_limit(findings: &mut FindingCollector, budget: &mut ScanBudget) {
    budget.exceeded = true;
    findings.push(
        ValidationCode::ScanLimitExceeded,
        ValidationArtifact::Snapshot,
        None,
        None,
    );
    findings.push(
        ValidationCode::FindingLimitReached,
        ValidationArtifact::Snapshot,
        None,
        None,
    );
}

impl FindingCollector {
    fn discarding() -> Self {
        Self {
            discard: true,
            ..Self::default()
        }
    }

    fn push(
        &mut self,
        code: ValidationCode,
        artifact: ValidationArtifact,
        scope: Option<MemoryScope>,
        memory_id: Option<MemoryId>,
    ) {
        if self.discard {
            return;
        }
        let finding = ValidationFinding {
            code,
            artifact,
            scope,
            memory_id,
        };
        if self.findings.iter().any(|existing| existing == &finding) {
            return;
        }
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(finding);
        } else {
            self.truncated = true;
        }
    }

    fn finish(&self) -> Vec<ValidationFinding> {
        let mut findings = self.findings.clone();
        if self.truncated
            && !findings
                .iter()
                .any(|finding| finding.code == ValidationCode::FindingLimitReached)
        {
            findings.push(ValidationFinding {
                code: ValidationCode::FindingLimitReached,
                artifact: ValidationArtifact::Snapshot,
                scope: None,
                memory_id: None,
            });
        }
        findings.sort_by_key(finding_key);
        findings
    }
}

struct MetadataInspection {
    metadata: Option<StoreMetadata>,
    raw_bytes: Option<Vec<u8>>,
}

fn inspect_metadata(
    root: &StoreDirectory,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) -> Result<MetadataInspection, StoreError> {
    let Some(file) = root.try_open_regular(Path::new(layout::STORE_METADATA_FILE), false)? else {
        findings.push(
            ValidationCode::StoreMetadataInconsistent,
            ValidationArtifact::StoreMetadata,
            None,
            None,
        );
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: None,
        });
    };
    if StoreDirectory::validate_private_open_file(&file).is_err() {
        findings.push(
            ValidationCode::UnsafeEntry,
            ValidationArtifact::StoreMetadata,
            None,
            None,
        );
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: None,
        });
    }
    let identity = FileIdentity::from_file(&file)?;
    let metadata_len = file
        .metadata()
        .map_err(|source| StoreError::io("inspect validation metadata", source))?
        .len();
    if !budget.consume_bytes(metadata_len) {
        record_scan_limit(findings, budget);
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: None,
        });
    }
    if metadata_len > 16_384 {
        findings.push(
            ValidationCode::StoreMetadataInconsistent,
            ValidationArtifact::StoreMetadata,
            None,
            None,
        );
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: None,
        });
    }
    let mut bytes = Vec::new();
    file.take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read validation metadata", source))?;
    if !root.file_identity_matches(Path::new(layout::STORE_METADATA_FILE), identity)? {
        findings.push(
            ValidationCode::SnapshotChanged,
            ValidationArtifact::Snapshot,
            None,
            None,
        );
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: Some(bytes),
        });
    }
    let value = serde_json::from_slice::<serde_json::Value>(&bytes);
    let format = value.as_ref().ok().and_then(|value| {
        value
            .as_object()
            .and_then(|object| object.get("formatVersion"))
            .and_then(serde_json::Value::as_str)
    });
    if format.is_some_and(|format| format != crate::STORE_FORMAT_VERSION) {
        findings.push(
            ValidationCode::UnsupportedStoreVersion,
            ValidationArtifact::StoreMetadata,
            None,
            None,
        );
        return Ok(MetadataInspection {
            metadata: None,
            raw_bytes: Some(bytes),
        });
    }
    match serde_json::from_slice::<StoreMetadata>(&bytes) {
        Ok(metadata)
            if metadata.format_version == crate::STORE_FORMAT_VERSION
                && metadata.audit_sequence.0 <= metadata.store_revision.0
                && metadata
                    .canonical_bytes()
                    .is_ok_and(|canonical| canonical == bytes) =>
        {
            Ok(MetadataInspection {
                metadata: Some(metadata),
                raw_bytes: Some(bytes),
            })
        }
        _ => {
            findings.push(
                ValidationCode::StoreMetadataInconsistent,
                ValidationArtifact::StoreMetadata,
                None,
                None,
            );
            Ok(MetadataInspection {
                metadata: None,
                raw_bytes: Some(bytes),
            })
        }
    }
}

fn inspect_active_transactions(
    root: &StoreDirectory,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let Ok(directory) = root.open_directory(Path::new("transactions")) else {
        findings.push(
            ValidationCode::LayoutInconsistent,
            ValidationArtifact::Transaction,
            None,
            None,
        );
        return;
    };
    match bounded_sorted_names(&directory, "inspect active transactions", budget) {
        Ok(names) if names.is_empty() => {}
        Ok(_) => findings.push(
            ValidationCode::ActiveTransaction,
            ValidationArtifact::Transaction,
            None,
            None,
        ),
        Err(_) if budget.exceeded => record_scan_limit(findings, budget),
        Err(_) => findings.push(
            ValidationCode::UnsafeEntry,
            ValidationArtifact::Transaction,
            None,
            None,
        ),
    }
    for relative in [
        layout::STORE_METADATA_INIT_FILE,
        layout::STORE_METADATA_MIGRATION_FILE,
        layout::PREVIOUS_STORE_METADATA_MIGRATION_FILE,
    ] {
        match root.try_open_regular(Path::new(relative), false) {
            Ok(Some(_)) => findings.push(
                ValidationCode::ActiveTransaction,
                ValidationArtifact::Transaction,
                None,
                None,
            ),
            Ok(None) => {}
            Err(error) => {
                push_filesystem_finding(findings, ValidationArtifact::Transaction, None, error)
            }
        }
    }
}

/// Bounded non-decoding global resurrection guard used only to filter a
/// candidate record before its body is opened. It retains hashed storage keys
/// in memory, never owner scope metadata or tombstone contents.
fn bounded_tombstone_storage_keys(
    root: &StoreDirectory,
    budget: &mut ScanBudget,
    findings: &mut FindingCollector,
) -> Result<BTreeSet<String>, StoreError> {
    let mut keys = BTreeSet::new();
    for kind in ["principal", "project", "session"] {
        let kind_directory =
            root.open_directory(Path::new(layout::TOMBSTONES_DIR).join(kind).as_path())?;
        let owners = initial_names(
            &kind_directory,
            "inspect tombstone protection owners",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        )
        .ok_or(StoreError::InvalidRequest)?;
        for owner_name in &owners {
            layout::validate_owner_entry_name(owner_name)?;
            let owner_directory =
                StoreDirectory::open_child_directory(&kind_directory, owner_name)?;
            collect_bounded_tombstone_keys(&owner_directory, &mut keys, budget, findings)?;
            if budget.exceeded {
                return Err(StoreError::InvalidRequest);
            }
        }
        ensure_names_unchanged(
            &kind_directory,
            &owners,
            "recheck tombstone protection owners",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        );
        if budget.exceeded {
            return Err(StoreError::InvalidRequest);
        }
    }
    let global = root.open_directory(
        Path::new(layout::TOMBSTONES_DIR)
            .join("instance_global")
            .as_path(),
    )?;
    collect_bounded_tombstone_keys(&global, &mut keys, budget, findings)?;
    Ok(keys)
}

fn collect_bounded_tombstone_keys(
    owner_directory: &cap_std::fs::Dir,
    keys: &mut BTreeSet<String>,
    budget: &mut ScanBudget,
    findings: &mut FindingCollector,
) -> Result<(), StoreError> {
    let shards = initial_names(
        owner_directory,
        "inspect tombstone protection shards",
        findings,
        ValidationArtifact::Tombstone,
        None,
        budget,
    )
    .ok_or(StoreError::InvalidRequest)?;
    for shard_name in &shards {
        let shard = shard_name
            .to_str()
            .filter(|value| valid_shard(value))
            .ok_or(StoreError::InvalidLayout)?;
        let shard_directory = StoreDirectory::open_child_directory(owner_directory, shard_name)?;
        let names = initial_names(
            &shard_directory,
            "inspect tombstone protection entries",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        )
        .ok_or(StoreError::InvalidRequest)?;
        for name in &names {
            let storage_key = layout::validate_tombstone_entry_name(name)?;
            if !storage_key.starts_with(shard) {
                return Err(StoreError::InvalidLayout);
            }
            // Only directory-entry and filesystem safety metadata is observed
            // here. Unauthorized tombstone files are never opened or decoded.
            StoreDirectory::validate_private_regular_entry_in(&shard_directory, name)?;
            if !keys.insert(storage_key) {
                return Err(StoreError::InvalidTransaction);
            }
        }
        ensure_names_unchanged(
            &shard_directory,
            &names,
            "recheck tombstone protection entries",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        );
    }
    ensure_names_unchanged(
        owner_directory,
        &shards,
        "recheck tombstone protection shards",
        findings,
        ValidationArtifact::Tombstone,
        None,
        budget,
    );
    Ok(())
}

fn inspect_quarantine_receipts(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let directory = match root.open_directory(Path::new(layout::QUARANTINE_RECEIPTS_DIR)) {
        Ok(directory) => directory,
        Err(error) => {
            push_filesystem_finding(findings, ValidationArtifact::Receipt, None, error);
            return;
        }
    };
    let names = match initial_names(
        &directory,
        "inspect quarantine receipts",
        findings,
        ValidationArtifact::Receipt,
        None,
        budget,
    ) {
        Some(names) => names,
        None => return,
    };
    for name in &names {
        let Some(transaction_id) = crate::transaction::transaction_id_from_receipt_name(name)
        else {
            findings.push(
                ValidationCode::ReceiptInconsistent,
                ValidationArtifact::Receipt,
                None,
                None,
            );
            continue;
        };
        let file = match StoreDirectory::try_open_regular_in(&directory, name) {
            Ok(Some(file)) => file,
            Ok(None) => {
                findings.push(
                    ValidationCode::SnapshotChanged,
                    ValidationArtifact::Snapshot,
                    None,
                    None,
                );
                continue;
            }
            Err(error) => {
                push_filesystem_finding(findings, ValidationArtifact::Receipt, None, error);
                continue;
            }
        };
        let length = file.metadata().map_or(u64::MAX, |metadata| metadata.len());
        if !budget.consume_bytes(length) {
            record_scan_limit(findings, budget);
            break;
        }
        if StoreDirectory::validate_private_open_file(&file).is_err()
            || crate::transaction::DurableQuarantineReceipt::decode(
                file,
                &transaction_id,
                &metadata.store_id,
            )
            .is_err()
        {
            findings.push(
                ValidationCode::ReceiptInconsistent,
                ValidationArtifact::Receipt,
                None,
                None,
            );
        }
    }
    ensure_names_unchanged(
        &directory,
        &names,
        "recheck quarantine receipts",
        findings,
        ValidationArtifact::Receipt,
        None,
        budget,
    );
}

fn scan_record_scope(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    scope: &MemoryScope,
    tombstoned_keys: &BTreeSet<String>,
    records: &mut Vec<PortableMemoryRecord>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let relative = layout::scope_relative_directory(scope);
    let directory = match root.try_open_directory(&relative) {
        Ok(Some(directory)) => directory,
        Ok(None) => return,
        Err(error) => {
            push_filesystem_finding(
                findings,
                ValidationArtifact::Record,
                Some(scope.clone()),
                error,
            );
            return;
        }
    };
    scan_record_owner(
        &directory,
        &relative,
        metadata,
        Some(scope),
        tombstoned_keys,
        false,
        records,
        findings,
        budget,
    );
}

fn scan_tombstone_scope(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    scope: &MemoryScope,
    tombstones: &mut Vec<PortableTombstone>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let relative = layout::tombstone_scope_relative_directory(scope);
    let directory = match root.try_open_directory(&relative) {
        Ok(Some(directory)) => directory,
        Ok(None) => return,
        Err(error) => {
            push_filesystem_finding(
                findings,
                ValidationArtifact::Tombstone,
                Some(scope.clone()),
                error,
            );
            return;
        }
    };
    scan_tombstone_owner(
        &directory,
        &relative,
        Some(scope),
        metadata,
        false,
        tombstones,
        findings,
        budget,
    );
}

fn scan_all_records(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    tombstoned_keys: &BTreeSet<String>,
    records: &mut Vec<PortableMemoryRecord>,
    scopes: &mut BTreeMap<String, MemoryScope>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    for kind in ["principal", "project", "session"] {
        let kind_relative = Path::new("records").join(kind);
        let kind_directory = match root.open_directory(&kind_relative) {
            Ok(directory) => directory,
            Err(error) => {
                push_filesystem_finding(findings, ValidationArtifact::Record, None, error);
                continue;
            }
        };
        let owner_names = match initial_names(
            &kind_directory,
            "inspect record scope owners",
            findings,
            ValidationArtifact::Record,
            None,
            budget,
        ) {
            Some(names) => names,
            None => continue,
        };
        for owner_name in &owner_names {
            if layout::validate_owner_entry_name(owner_name).is_err() {
                findings.push(
                    ValidationCode::LayoutInconsistent,
                    ValidationArtifact::Record,
                    None,
                    None,
                );
                continue;
            }
            let owner_directory =
                match StoreDirectory::open_child_directory(&kind_directory, owner_name) {
                    Ok(directory) => directory,
                    Err(error) => {
                        push_filesystem_finding(findings, ValidationArtifact::Record, None, error);
                        continue;
                    }
                };
            scan_record_owner(
                &owner_directory,
                &kind_relative.join(owner_name),
                metadata,
                None,
                tombstoned_keys,
                true,
                records,
                findings,
                budget,
            );
            if budget.exceeded {
                break;
            }
        }
        ensure_names_unchanged(
            &kind_directory,
            &owner_names,
            "recheck record scope owners",
            findings,
            ValidationArtifact::Record,
            None,
            budget,
        );
        if budget.exceeded {
            break;
        }
    }
    let global_relative = PathBuf::from("records/instance_global");
    match root.open_directory(&global_relative) {
        Ok(directory) => scan_record_owner(
            &directory,
            &global_relative,
            metadata,
            Some(&MemoryScope::InstanceGlobal {}),
            tombstoned_keys,
            true,
            records,
            findings,
            budget,
        ),
        Err(error) => push_filesystem_finding(findings, ValidationArtifact::Record, None, error),
    }
    for record in records.iter() {
        scopes.insert(scope_key(&record.scope), record.scope.clone());
    }
}

fn scan_all_tombstones(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    tombstones: &mut Vec<PortableTombstone>,
    scopes: &mut BTreeMap<String, MemoryScope>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    for kind in ["principal", "project", "session"] {
        let kind_relative = Path::new(layout::TOMBSTONES_DIR).join(kind);
        let kind_directory = match root.open_directory(&kind_relative) {
            Ok(directory) => directory,
            Err(error) => {
                push_filesystem_finding(findings, ValidationArtifact::Tombstone, None, error);
                continue;
            }
        };
        let owner_names = match initial_names(
            &kind_directory,
            "inspect tombstone scope owners",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        ) {
            Some(names) => names,
            None => continue,
        };
        for owner_name in &owner_names {
            if layout::validate_owner_entry_name(owner_name).is_err() {
                findings.push(
                    ValidationCode::TombstoneInconsistent,
                    ValidationArtifact::Tombstone,
                    None,
                    None,
                );
                continue;
            }
            let owner_directory =
                match StoreDirectory::open_child_directory(&kind_directory, owner_name) {
                    Ok(directory) => directory,
                    Err(error) => {
                        push_filesystem_finding(
                            findings,
                            ValidationArtifact::Tombstone,
                            None,
                            error,
                        );
                        continue;
                    }
                };
            scan_tombstone_owner(
                &owner_directory,
                &kind_relative.join(owner_name),
                None,
                metadata,
                true,
                tombstones,
                findings,
                budget,
            );
            if budget.exceeded {
                break;
            }
        }
        ensure_names_unchanged(
            &kind_directory,
            &owner_names,
            "recheck tombstone scope owners",
            findings,
            ValidationArtifact::Tombstone,
            None,
            budget,
        );
        if budget.exceeded {
            break;
        }
    }
    let global_relative = PathBuf::from("tombstones/instance_global");
    match root.open_directory(&global_relative) {
        Ok(directory) => scan_tombstone_owner(
            &directory,
            &global_relative,
            Some(&MemoryScope::InstanceGlobal {}),
            metadata,
            true,
            tombstones,
            findings,
            budget,
        ),
        Err(error) => {
            push_filesystem_finding(findings, ValidationArtifact::Tombstone, None, error);
        }
    }
    for tombstone in tombstones.iter() {
        scopes.insert(scope_key(&tombstone.scope), tombstone.scope.clone());
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_record_owner(
    owner_directory: &cap_std::fs::Dir,
    owner_relative: &Path,
    metadata: &StoreMetadata,
    expected_scope: Option<&MemoryScope>,
    tombstoned_keys: &BTreeSet<String>,
    reveal_decoded_identity: bool,
    records: &mut Vec<PortableMemoryRecord>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let shard_names = match initial_names(
        owner_directory,
        "inspect record shards",
        findings,
        ValidationArtifact::Record,
        expected_scope.cloned(),
        budget,
    ) {
        Some(names) => names,
        None => return,
    };
    for shard_name in &shard_names {
        let Some(shard) = shard_name.to_str().filter(|value| valid_shard(value)) else {
            findings.push(
                ValidationCode::ShardMismatch,
                ValidationArtifact::Record,
                expected_scope.cloned(),
                None,
            );
            continue;
        };
        let shard_directory =
            match StoreDirectory::open_child_directory(owner_directory, shard_name) {
                Ok(directory) => directory,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Record,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
        let file_names = match initial_names(
            &shard_directory,
            "inspect record candidates",
            findings,
            ValidationArtifact::Record,
            expected_scope.cloned(),
            budget,
        ) {
            Some(names) => names,
            None => continue,
        };
        for file_name in &file_names {
            if crate::transaction::transaction_id_from_erasure_witness_name(file_name).is_some() {
                inspect_witness(&shard_directory, file_name, expected_scope, findings);
                continue;
            }
            let storage_key = match layout::validate_record_entry_name(file_name) {
                Ok(storage_key) => storage_key,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Record,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            if !storage_key.starts_with(shard) {
                findings.push(
                    ValidationCode::ShardMismatch,
                    ValidationArtifact::Record,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            // Global hashed protection happens before opening or decoding this
            // candidate body. Validation records only the authorized candidate
            // conflict, and export fails closed rather than normalizing it away.
            if tombstoned_keys.contains(&storage_key) {
                findings.push(
                    ValidationCode::RecordTombstoneConflict,
                    ValidationArtifact::Record,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let file = match StoreDirectory::try_open_regular_in(&shard_directory, file_name) {
                Ok(Some(file)) => file,
                Ok(None) => {
                    findings.push(
                        ValidationCode::SnapshotChanged,
                        ValidationArtifact::Snapshot,
                        expected_scope.cloned(),
                        None,
                    );
                    continue;
                }
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Record,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            if StoreDirectory::validate_private_open_file(&file).is_err() {
                findings.push(
                    ValidationCode::UnsafeEntry,
                    ValidationArtifact::Record,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let identity = match FileIdentity::from_file(&file) {
                Ok(identity) => identity,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Record,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            let length = file.metadata().map_or(u64::MAX, |metadata| metadata.len());
            if !budget.consume_bytes(length) {
                record_scan_limit(findings, budget);
                break;
            }
            let decoded = read_record(file);
            #[cfg(test)]
            layout::run_test_hook(layout::TestHookPoint::InspectionRecordRecheck, file_name);
            if !StoreDirectory::file_identity_matches_in(&shard_directory, file_name, identity)
                .unwrap_or(false)
            {
                findings.push(
                    ValidationCode::SnapshotChanged,
                    ValidationArtifact::Snapshot,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let record = match decoded {
                Ok(record) => record,
                Err(error) => {
                    push_record_error(
                        findings,
                        expected_scope.cloned(),
                        reveal_decoded_identity,
                        error,
                    );
                    continue;
                }
            };
            let safe_scope = if reveal_decoded_identity {
                Some(record.scope.clone())
            } else {
                expected_scope.cloned()
            };
            if layout::record_storage_key(&record.id) != storage_key {
                findings.push(
                    ValidationCode::RecordIdMismatch,
                    ValidationArtifact::Record,
                    safe_scope,
                    reveal_decoded_identity.then_some(record.id),
                );
                continue;
            }
            if layout::record_shard(&record.id) != shard {
                findings.push(
                    ValidationCode::ShardMismatch,
                    ValidationArtifact::Record,
                    if reveal_decoded_identity {
                        Some(record.scope)
                    } else {
                        expected_scope.cloned()
                    },
                    reveal_decoded_identity.then_some(record.id),
                );
                continue;
            }
            let expected_relative = layout::scope_relative_directory(&record.scope);
            if expected_scope.is_some_and(|scope| scope != &record.scope)
                || expected_relative != owner_relative
            {
                findings.push(
                    ValidationCode::ScopePathMismatch,
                    ValidationArtifact::Record,
                    if reveal_decoded_identity {
                        Some(record.scope)
                    } else {
                        expected_scope.cloned()
                    },
                    reveal_decoded_identity.then_some(record.id),
                );
                continue;
            }
            if record.revision.get() > metadata.store_revision.0 {
                findings.push(
                    ValidationCode::RecordMalformed,
                    ValidationArtifact::Record,
                    if reveal_decoded_identity {
                        Some(record.scope)
                    } else {
                        expected_scope.cloned()
                    },
                    reveal_decoded_identity.then_some(record.id),
                );
                continue;
            }
            if !budget.consume_item() {
                record_scan_limit(findings, budget);
                break;
            }
            records.push(record.into());
        }
        ensure_names_unchanged(
            &shard_directory,
            &file_names,
            "recheck record candidates",
            findings,
            ValidationArtifact::Record,
            expected_scope.cloned(),
            budget,
        );
        if budget.exceeded {
            break;
        }
    }
    ensure_names_unchanged(
        owner_directory,
        &shard_names,
        "recheck record shards",
        findings,
        ValidationArtifact::Record,
        expected_scope.cloned(),
        budget,
    );
}

fn read_record(file: File) -> Result<jiandu_core::MemoryRecord, StoreError> {
    if file
        .metadata()
        .map_err(|source| StoreError::io("inspect portable record", source))?
        .len()
        > MAX_CANONICAL_DOCUMENT_BYTES as u64
    {
        return Err(StoreError::InvalidRecord {
            id: None,
            reason: crate::InvalidRecordReason::ValidationFailed,
        });
    }
    let mut bytes = Vec::new();
    file.take((MAX_CANONICAL_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read portable record", source))?;
    Ok(decode_canonical_document(&bytes, None)?.record)
}

fn inspect_witness(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    scope: Option<&MemoryScope>,
    findings: &mut FindingCollector,
) {
    let valid = StoreDirectory::try_open_regular_in(directory, name)
        .and_then(|file| file.ok_or(StoreError::InvalidTransaction))
        .and_then(|file| {
            StoreDirectory::validate_private_open_file(&file)?;
            let identity = FileIdentity::from_file(&file)?;
            let metadata = file
                .metadata()
                .map_err(|source| StoreError::io("inspect erasure witness", source))?;
            if metadata.len() == 0
                && StoreDirectory::file_identity_matches_in(directory, name, identity)?
            {
                Ok(())
            } else {
                Err(StoreError::InvalidTransaction)
            }
        });
    if valid.is_err() {
        findings.push(
            ValidationCode::WitnessInconsistent,
            ValidationArtifact::Witness,
            scope.cloned(),
            None,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_tombstone_owner(
    owner_directory: &cap_std::fs::Dir,
    owner_relative: &Path,
    expected_scope: Option<&MemoryScope>,
    metadata: &StoreMetadata,
    reveal_decoded_identity: bool,
    tombstones: &mut Vec<PortableTombstone>,
    findings: &mut FindingCollector,
    budget: &mut ScanBudget,
) {
    let shard_names = match initial_names(
        owner_directory,
        "inspect tombstone shards",
        findings,
        ValidationArtifact::Tombstone,
        expected_scope.cloned(),
        budget,
    ) {
        Some(names) => names,
        None => return,
    };
    for shard_name in &shard_names {
        let Some(shard) = shard_name.to_str().filter(|value| valid_shard(value)) else {
            findings.push(
                ValidationCode::ShardMismatch,
                ValidationArtifact::Tombstone,
                expected_scope.cloned(),
                None,
            );
            continue;
        };
        let shard_directory =
            match StoreDirectory::open_child_directory(owner_directory, shard_name) {
                Ok(directory) => directory,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Tombstone,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
        let file_names = match initial_names(
            &shard_directory,
            "inspect protected tombstones",
            findings,
            ValidationArtifact::Tombstone,
            expected_scope.cloned(),
            budget,
        ) {
            Some(names) => names,
            None => continue,
        };
        for file_name in &file_names {
            let storage_key = match layout::validate_tombstone_entry_name(file_name) {
                Ok(storage_key) => storage_key,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Tombstone,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            if !storage_key.starts_with(shard) {
                findings.push(
                    ValidationCode::ShardMismatch,
                    ValidationArtifact::Tombstone,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let file = match StoreDirectory::try_open_regular_in(&shard_directory, file_name) {
                Ok(Some(file)) => file,
                Ok(None) => {
                    findings.push(
                        ValidationCode::SnapshotChanged,
                        ValidationArtifact::Snapshot,
                        expected_scope.cloned(),
                        None,
                    );
                    continue;
                }
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Tombstone,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            if StoreDirectory::validate_private_open_file(&file).is_err() {
                findings.push(
                    ValidationCode::UnsafeEntry,
                    ValidationArtifact::Tombstone,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let identity = match FileIdentity::from_file(&file) {
                Ok(identity) => identity,
                Err(error) => {
                    push_filesystem_finding(
                        findings,
                        ValidationArtifact::Tombstone,
                        expected_scope.cloned(),
                        error,
                    );
                    continue;
                }
            };
            let length = file.metadata().map_or(u64::MAX, |metadata| metadata.len());
            if !budget.consume_bytes(length) {
                record_scan_limit(findings, budget);
                break;
            }
            let decoded = crate::tombstone::ProtectedTombstone::decode(file, &metadata.store_id);
            if !StoreDirectory::file_identity_matches_in(&shard_directory, file_name, identity)
                .unwrap_or(false)
            {
                findings.push(
                    ValidationCode::SnapshotChanged,
                    ValidationArtifact::Snapshot,
                    expected_scope.cloned(),
                    None,
                );
                continue;
            }
            let tombstone = match decoded {
                Ok(tombstone) => tombstone,
                Err(_) => {
                    findings.push(
                        ValidationCode::TombstoneInconsistent,
                        ValidationArtifact::Tombstone,
                        expected_scope.cloned(),
                        None,
                    );
                    continue;
                }
            };
            let relative = owner_relative.join(shard).join(file_name);
            if layout::record_storage_key(&tombstone.memory_id) != storage_key
                || layout::record_shard(&tombstone.memory_id) != shard
                || tombstone.relative_path() != relative
                || expected_scope.is_some_and(|scope| scope != &tombstone.scope)
                || tombstone.revision.get() > tombstone.store_revision.0
                || tombstone.audit_sequence.0 > tombstone.store_revision.0
                || tombstone.store_revision.0 > metadata.store_revision.0
                || tombstone.audit_sequence.0 > metadata.audit_sequence.0
            {
                findings.push(
                    ValidationCode::TombstoneInconsistent,
                    ValidationArtifact::Tombstone,
                    if reveal_decoded_identity {
                        Some(tombstone.scope)
                    } else {
                        expected_scope.cloned()
                    },
                    reveal_decoded_identity.then_some(tombstone.memory_id),
                );
                continue;
            }
            if !budget.consume_item() {
                record_scan_limit(findings, budget);
                break;
            }
            tombstones.push(PortableTombstone {
                format_version: PORTABLE_TOMBSTONE_FORMAT_VERSION.to_owned(),
                memory_id: tombstone.memory_id,
                scope: tombstone.scope,
                revision: tombstone.revision,
                etag: tombstone.etag,
                forgotten_at: tombstone.forgotten_at,
                store_revision: tombstone.store_revision,
                audit_sequence: tombstone.audit_sequence,
            });
        }
        ensure_names_unchanged(
            &shard_directory,
            &file_names,
            "recheck protected tombstones",
            findings,
            ValidationArtifact::Tombstone,
            expected_scope.cloned(),
            budget,
        );
        if budget.exceeded {
            break;
        }
    }
    ensure_names_unchanged(
        owner_directory,
        &shard_names,
        "recheck tombstone shards",
        findings,
        ValidationArtifact::Tombstone,
        expected_scope.cloned(),
        budget,
    );
}

fn initial_names(
    directory: &cap_std::fs::Dir,
    operation: &'static str,
    findings: &mut FindingCollector,
    artifact: ValidationArtifact,
    scope: Option<MemoryScope>,
    budget: &mut ScanBudget,
) -> Option<Vec<OsString>> {
    match bounded_sorted_names(directory, operation, budget) {
        Ok(names) => Some(names),
        Err(_) if budget.exceeded => {
            record_scan_limit(findings, budget);
            None
        }
        Err(error) => {
            push_filesystem_finding(findings, artifact, scope, error);
            None
        }
    }
}

fn ensure_names_unchanged(
    directory: &cap_std::fs::Dir,
    begin: &[OsString],
    operation: &'static str,
    findings: &mut FindingCollector,
    artifact: ValidationArtifact,
    scope: Option<MemoryScope>,
    budget: &mut ScanBudget,
) {
    match bounded_sorted_names(directory, operation, budget) {
        Ok(end) if end == begin => {}
        Ok(_) => findings.push(
            ValidationCode::SnapshotChanged,
            ValidationArtifact::Snapshot,
            scope,
            None,
        ),
        Err(_) if budget.exceeded => record_scan_limit(findings, budget),
        Err(error) => push_filesystem_finding(findings, artifact, scope, error),
    }
}

fn push_record_error(
    findings: &mut FindingCollector,
    scope: Option<MemoryScope>,
    reveal_decoded_identity: bool,
    error: StoreError,
) {
    match error {
        StoreError::InvalidRecord { id, reason } => {
            let code = match reason {
                crate::InvalidRecordReason::IdFilenameMismatch => ValidationCode::RecordIdMismatch,
                crate::InvalidRecordReason::ScopePathMismatch => ValidationCode::ScopePathMismatch,
                crate::InvalidRecordReason::ShardMismatch => ValidationCode::ShardMismatch,
                crate::InvalidRecordReason::InvalidUtf8
                | crate::InvalidRecordReason::Truncated
                | crate::InvalidRecordReason::MalformedFrontmatter
                | crate::InvalidRecordReason::NonCanonicalEncoding
                | crate::InvalidRecordReason::ValidationFailed => ValidationCode::RecordMalformed,
            };
            findings.push(
                code,
                ValidationArtifact::Record,
                scope,
                if reveal_decoded_identity { id } else { None },
            );
        }
        other => push_filesystem_finding(findings, ValidationArtifact::Record, scope, other),
    }
}

fn push_filesystem_finding(
    findings: &mut FindingCollector,
    artifact: ValidationArtifact,
    scope: Option<MemoryScope>,
    error: StoreError,
) {
    let code = match error {
        StoreError::UnsafePath => ValidationCode::UnsafeEntry,
        _ => ValidationCode::LayoutInconsistent,
    };
    findings.push(code, artifact, scope, None);
}

fn valid_shard(value: &str) -> bool {
    value.len() == 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn detect_duplicates(
    records: &[PortableMemoryRecord],
    tombstones: &[PortableTombstone],
    findings: &mut FindingCollector,
) {
    let mut observed = BTreeMap::<MemoryId, MemoryScope>::new();
    for record in records {
        if observed
            .insert(record.id.clone(), record.scope.clone())
            .is_some()
        {
            findings.push(
                ValidationCode::DuplicateMemoryId,
                ValidationArtifact::Record,
                Some(record.scope.clone()),
                Some(record.id.clone()),
            );
        }
    }
    for tombstone in tombstones {
        if observed
            .insert(tombstone.memory_id.clone(), tombstone.scope.clone())
            .is_some()
        {
            findings.push(
                ValidationCode::DuplicateMemoryId,
                ValidationArtifact::Tombstone,
                Some(tombstone.scope.clone()),
                Some(tombstone.memory_id.clone()),
            );
        }
    }
}

fn bounded_sorted_names(
    directory: &cap_std::fs::Dir,
    operation: &'static str,
    budget: &mut ScanBudget,
) -> Result<Vec<OsString>, StoreError> {
    StoreDirectory::validate_private_open_directory(directory)?;
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io(operation, source))?;
    let mut names = Vec::new();
    for entry in entries {
        if !budget.consume_entry() {
            return Err(StoreError::InvalidRequest);
        }
        names.push(
            entry
                .map_err(|source| StoreError::io(operation, source))?
                .file_name(),
        );
    }
    names.sort();
    Ok(names)
}

fn scope_key(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Principal { principal_id } => format!("0:{}", principal_id.as_str()),
        MemoryScope::Project { project_id } => format!("1:{}", project_id.as_str()),
        MemoryScope::Session { session_id } => format!("2:{}", session_id.as_str()),
        MemoryScope::InstanceGlobal {} => "3:".to_owned(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedValidationReport<'a> {
    format_version: &'a str,
    mode: ValidationMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_store_id: Option<&'a StoreId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<SnapshotWatermark>,
    inspected_scopes: &'a [MemoryScope],
    findings: &'a [ValidationFinding],
    truncated: bool,
}

impl ValidationReport {
    fn expected_digest(&self) -> Result<ExportDigest, StoreError> {
        let payload = serde_json::to_vec(&UnsignedValidationReport {
            format_version: &self.format_version,
            mode: self.mode,
            source_store_id: self.source_store_id.as_ref(),
            snapshot: self.snapshot,
            inspected_scopes: &self.inspected_scopes,
            findings: &self.findings,
            truncated: self.truncated,
        })
        .map_err(|_| StoreError::InvalidRequest)?;
        Ok(ExportDigest::from_payload(
            b"jiandu/validation-report/v1\0",
            &payload,
        ))
    }

    fn validate(&self) -> Result<(), StoreError> {
        let finding_limit_count = self
            .findings
            .iter()
            .filter(|finding| finding.code == ValidationCode::FindingLimitReached)
            .count();
        let finding_limit_is_canonical = self.findings.iter().all(|finding| {
            finding.code != ValidationCode::FindingLimitReached
                || (finding.artifact == ValidationArtifact::Snapshot
                    && finding.scope.is_none()
                    && finding.memory_id.is_none())
        });
        let scope_limit = match self.mode {
            ValidationMode::Scoped => MAX_REQUESTED_SCOPES,
            ValidationMode::AllScopes => MAX_DISCOVERED_SCOPES,
        };
        let inspected_scope_keys = self
            .inspected_scopes
            .iter()
            .map(scope_key)
            .collect::<BTreeSet<_>>();
        if self.format_version != VALIDATION_REPORT_FORMAT_VERSION
            || self.inspected_scopes.len() > scope_limit
            || (self.mode == ValidationMode::Scoped && self.inspected_scopes.is_empty())
            || self.findings.len() > MAX_FINDINGS + 1
            || !is_sorted_scopes(&self.inspected_scopes)
            || self.source_store_id.is_some() != self.snapshot.is_some()
            || self
                .snapshot
                .is_some_and(|snapshot| snapshot.audit_sequence.0 > snapshot.store_revision.0)
            || (self.truncated && finding_limit_count != 1)
            || (!self.truncated && finding_limit_count != 0)
            || !finding_limit_is_canonical
            || (self.mode == ValidationMode::Scoped
                && self.findings.iter().any(|finding| {
                    finding
                        .scope
                        .as_ref()
                        .is_some_and(|scope| !inspected_scope_keys.contains(&scope_key(scope)))
                }))
            || !self
                .findings
                .windows(2)
                .all(|pair| finding_key(&pair[0]) < finding_key(&pair[1]))
            || self.digest != self.expected_digest()?
        {
            return Err(StoreError::InvalidRequest);
        }
        Ok(())
    }

    /// Encode this report as strict pretty JSON with one trailing LF.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_REPORT_BYTES)
    }

    /// Decode only an exact canonical representation that satisfies all
    /// cross-field report invariants and its digest.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        decode_canonical(bytes, MAX_REPORT_BYTES, Self::validate)
    }

    #[cfg(test)]
    pub(crate) fn refresh_digest_for_test(&mut self) {
        self.digest = self.expected_digest().expect("test report digest");
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedPortableExport<'a> {
    format_version: &'a str,
    source_store_format: &'a str,
    source_store_id: &'a StoreId,
    snapshot: SnapshotWatermark,
    scopes: &'a [MemoryScope],
    records: &'a [PortableMemoryRecord],
    tombstones: &'a [PortableTombstone],
}

impl PortableExportBundle {
    fn expected_digest(&self) -> Result<ExportDigest, StoreError> {
        let payload = serde_json::to_vec(&UnsignedPortableExport {
            format_version: &self.format_version,
            source_store_format: &self.source_store_format,
            source_store_id: &self.source_store_id,
            snapshot: self.snapshot,
            scopes: &self.scopes,
            records: &self.records,
            tombstones: &self.tombstones,
        })
        .map_err(|_| StoreError::InvalidRequest)?;
        Ok(ExportDigest::from_payload(
            b"jiandu/portable-export/v1\0",
            &payload,
        ))
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.format_version != PORTABLE_EXPORT_FORMAT_VERSION
            || self.source_store_format != crate::STORE_FORMAT_VERSION
            || self.scopes.len() > MAX_DISCOVERED_SCOPES
            || !is_sorted_scopes(&self.scopes)
            || self.snapshot.audit_sequence.0 > self.snapshot.store_revision.0
            || self.records.len().saturating_add(self.tombstones.len()) > MAX_EXPORT_ITEMS
            || !self.records.windows(2).all(|pair| {
                pair[0].id < pair[1].id
                    || (pair[0].id == pair[1].id
                        && scope_key(&pair[0].scope) < scope_key(&pair[1].scope))
            })
            || !self.tombstones.windows(2).all(|pair| {
                pair[0].memory_id < pair[1].memory_id
                    || (pair[0].memory_id == pair[1].memory_id
                        && scope_key(&pair[0].scope) < scope_key(&pair[1].scope))
            })
            || self.digest != self.expected_digest()?
        {
            return Err(StoreError::InvalidRequest);
        }
        let scopes = self.scopes.iter().map(scope_key).collect::<BTreeSet<_>>();
        let mut ids = BTreeSet::new();
        for record in &self.records {
            let record_value: jiandu_core::MemoryRecord = record.clone().into();
            let canonical = crate::document::encode_canonical_document(
                &jiandu_core::MemoryFrontmatterV1Alpha1::from_record(&record_value),
                &record_value.body,
            )
            .and_then(|bytes| decode_canonical_document(&bytes, Some(&record_value.id)));
            if !scopes.contains(&scope_key(&record.scope))
                || !ids.insert(record.id.clone())
                || record.revision.get() > self.snapshot.store_revision.0
                || record_value.validate().is_err()
                || canonical.is_err()
                || canonical.is_ok_and(|document| document.record != record_value)
            {
                return Err(StoreError::InvalidRequest);
            }
        }
        for tombstone in &self.tombstones {
            if tombstone.format_version != PORTABLE_TOMBSTONE_FORMAT_VERSION
                || !scopes.contains(&scope_key(&tombstone.scope))
                || !ids.insert(tombstone.memory_id.clone())
                || !crate::transaction::valid_content_digest(tombstone.etag.as_str())
                || tombstone.store_revision.0 == 0
                || tombstone.audit_sequence.0 == 0
                || tombstone.revision.get() > tombstone.store_revision.0
                || tombstone.audit_sequence.0 > tombstone.store_revision.0
                || tombstone.store_revision.0 > self.snapshot.store_revision.0
                || tombstone.audit_sequence.0 > self.snapshot.audit_sequence.0
            {
                return Err(StoreError::InvalidRequest);
            }
        }
        Ok(())
    }

    /// Encode this bundle as strict pretty JSON with one trailing LF.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_EXPORT_BYTES)
    }

    /// Decode only an exact canonical representation that satisfies all
    /// record/tombstone, ordering, watermark, and digest invariants.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        decode_canonical(bytes, MAX_EXPORT_BYTES, Self::validate)
    }

    #[cfg(test)]
    pub(crate) fn refresh_digest_for_test(&mut self) {
        self.digest = self.expected_digest().expect("test export digest");
    }
}

fn is_sorted_scopes(scopes: &[MemoryScope]) -> bool {
    scopes
        .windows(2)
        .all(|pair| scope_key(&pair[0]) < scope_key(&pair[1]))
}

fn finding_key(
    finding: &ValidationFinding,
) -> (ValidationCode, ValidationArtifact, String, String) {
    (
        finding.code,
        finding.artifact,
        finding.scope.as_ref().map_or_else(String::new, scope_key),
        finding
            .memory_id
            .as_ref()
            .map_or_else(String::new, |memory_id| memory_id.as_str().to_owned()),
    )
}

fn canonical_json(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| StoreError::InvalidRequest)?;
    bytes.push(b'\n');
    if bytes.len() > maximum {
        return Err(StoreError::InvalidRequest);
    }
    Ok(bytes)
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    maximum: usize,
    validate: impl FnOnce(&T) -> Result<(), StoreError>,
) -> Result<T, StoreError> {
    if bytes.len() > maximum {
        return Err(StoreError::InvalidRequest);
    }
    let value: T = serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidRequest)?;
    validate(&value)?;
    if canonical_json(&value, maximum)? != bytes {
        return Err(StoreError::InvalidRequest);
    }
    Ok(value)
}

/// Generated schemas checked into `crates/jiandu-store/schemas` by drift tests.
#[must_use]
pub fn generated_inspection_schemas() -> BTreeMap<&'static str, serde_json::Value> {
    BTreeMap::from([
        (
            "portable-export-bundle.schema.json",
            serde_json::to_value(schema_for!(PortableExportBundle))
                .expect("portable export schema is JSON serializable"),
        ),
        (
            "portable-tombstone.schema.json",
            serde_json::to_value(schema_for!(PortableTombstone))
                .expect("portable tombstone schema is JSON serializable"),
        ),
        (
            "validation-report.schema.json",
            serde_json::to_value(schema_for!(ValidationReport))
                .expect("validation report schema is JSON serializable"),
        ),
    ])
}
