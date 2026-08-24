//! Strict portable-import planning, batch commit, and backup metadata.

use crate::{
    AuditSequence, CanonicalStore, PortableExportBundle, SnapshotWatermark, StoreError, StoreId,
    StoreMetadata,
};
use jiandu_core::{
    Etag, IdempotencyKey, MemoryId, MemoryScope, PrincipalId, Revision, StoreRevision, Timestamp,
    TrustedRequestContext, Validate,
};
use schemars::{JsonSchema, schema_for};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Canonical import-plan format, independent of the store and export formats.
pub const IMPORT_PLAN_FORMAT_VERSION: &str = "jiandu.import-plan/v1alpha1";
/// Canonical committed import-result format.
pub const IMPORT_RESULT_FORMAT_VERSION: &str = "jiandu.import-result/v1alpha1";
/// Strict recovery-safe backup metadata format.
pub const BACKUP_METADATA_FORMAT_VERSION: &str = "jiandu.backup-metadata/v1alpha1";

pub(crate) const MAX_IMPORT_ITEMS: usize = 100;
pub(crate) const MAX_IMPORT_SCOPES: usize = 100;
const MAX_IMPORT_PLAN_ITEMS: usize = 1_000;
const MAX_IMPORT_PLAN_BYTES: usize = 1_048_576;
const MAX_BACKUP_METADATA_BYTES: usize = 65_536;
const IMPORT_RECEIPT_FORMAT_VERSION: &str = "jiandu.store.import-receipt/v1alpha1";
const IMPORT_AUDIT_FORMAT_VERSION: &str = "jiandu.store.import-audit/v1alpha1";
const MAX_IMPORT_ARTIFACT_BYTES: usize = 262_144;

/// Domain-separated lowercase SHA-256 digest used by import contracts.
#[derive(Clone, Debug, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ImportDigest(
    #[schemars(length(min = 71, max = 71), regex(pattern = r"^sha256:[0-9a-f]{64}$"))] String,
);

impl ImportDigest {
    pub(crate) fn from_payload(domain: &[u8], payload: &[u8]) -> Self {
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

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
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

    /// Return the stable `sha256:<lowercase-hex>` representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ImportDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Self(String::deserialize(deserializer)?);
        value.validate().map_err(D::Error::custom)?;
        Ok(value)
    }
}

/// Portable entry kind classified by a dry run.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ImportItemKind {
    Record,
    Tombstone,
}

/// Closed deterministic import classification.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ImportClassification {
    Accepted,
    Conflicting,
    Unauthorized,
    TombstoneProtected,
    Invalid,
}

/// Fresh per-scope authority decision included in the dry run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportScopeDecision {
    pub scope: MemoryScope,
    pub authorized: bool,
}

/// Body-free plan entry for one opaque portable record or tombstone.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportPlanEntry {
    pub kind: ImportItemKind,
    pub memory_id: MemoryId,
    pub scope: MemoryScope,
    pub classification: ImportClassification,
}

/// Exact category totals committed into the plan digest.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportPlanCounts {
    pub accepted: u32,
    pub conflicting: u32,
    pub unauthorized: u32,
    pub tombstone_protected: u32,
    pub invalid: u32,
}

impl ImportPlanCounts {
    fn observe(&mut self, classification: ImportClassification) -> Result<(), StoreError> {
        let counter = match classification {
            ImportClassification::Accepted => &mut self.accepted,
            ImportClassification::Conflicting => &mut self.conflicting,
            ImportClassification::Unauthorized => &mut self.unauthorized,
            ImportClassification::TombstoneProtected => &mut self.tombstone_protected,
            ImportClassification::Invalid => &mut self.invalid,
        };
        *counter = counter.checked_add(1).ok_or(StoreError::InvalidRequest)?;
        Ok(())
    }

    fn total(self) -> u64 {
        u64::from(self.accepted)
            + u64::from(self.conflicting)
            + u64::from(self.unauthorized)
            + u64::from(self.tombstone_protected)
            + u64::from(self.invalid)
    }

    fn committable(self) -> bool {
        self.conflicting == 0
            && self.unauthorized == 0
            && self.tombstone_protected == 0
            && self.invalid == 0
    }
}

/// Strict deterministic zero-write import dry run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportDryRunPlan {
    #[schemars(
        length(min = 27, max = 27),
        regex(pattern = r"^jiandu\.import-plan/v1alpha1$")
    )]
    pub format_version: String,
    pub source_store_id: StoreId,
    #[schemars(length(min = 71, max = 71), regex(pattern = r"^sha256:[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    pub target_store_id: StoreId,
    pub target_snapshot: SnapshotWatermark,
    #[schemars(length(min = 1, max = 100), extend("uniqueItems" = true))]
    pub scopes: Vec<ImportScopeDecision>,
    #[schemars(length(max = 1000), extend("uniqueItems" = true))]
    pub entries: Vec<ImportPlanEntry>,
    pub counts: ImportPlanCounts,
    pub committable: bool,
    pub digest: ImportDigest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedImportPlan<'a> {
    format_version: &'a str,
    source_store_id: &'a StoreId,
    bundle_digest: &'a str,
    target_store_id: &'a StoreId,
    target_snapshot: SnapshotWatermark,
    scopes: &'a [ImportScopeDecision],
    entries: &'a [ImportPlanEntry],
    counts: ImportPlanCounts,
    committable: bool,
}

impl ImportDryRunPlan {
    fn expected_digest(&self) -> Result<ImportDigest, StoreError> {
        let payload = serde_json::to_vec(&UnsignedImportPlan {
            format_version: &self.format_version,
            source_store_id: &self.source_store_id,
            bundle_digest: &self.bundle_digest,
            target_store_id: &self.target_store_id,
            target_snapshot: self.target_snapshot,
            scopes: &self.scopes,
            entries: &self.entries,
            counts: self.counts,
            committable: self.committable,
        })
        .map_err(|_| StoreError::InvalidRequest)?;
        Ok(ImportDigest::from_payload(
            b"jiandu/import-plan/v1\0",
            &payload,
        ))
    }

    fn validate(&self) -> Result<(), StoreError> {
        let mut counts = ImportPlanCounts::default();
        for entry in &self.entries {
            counts.observe(entry.classification)?;
        }
        let scope_keys = self
            .scopes
            .iter()
            .map(|decision| scope_key(&decision.scope))
            .collect::<Vec<_>>();
        let entry_keys = self
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.memory_id.as_str().to_owned(),
                    entry.kind,
                    scope_key(&entry.scope),
                )
            })
            .collect::<Vec<_>>();
        let unique_memory_ids = self
            .entries
            .iter()
            .map(|entry| &entry.memory_id)
            .collect::<BTreeSet<_>>()
            .len()
            == self.entries.len();
        let oversized = self.entries.len() > MAX_IMPORT_ITEMS;
        let entry_scope_semantics_are_valid = self.entries.iter().all(|entry| {
            self.scopes
                .iter()
                .find(|decision| decision.scope == entry.scope)
                .is_some_and(|decision| {
                    if decision.authorized {
                        if oversized {
                            entry.classification == ImportClassification::Invalid
                        } else {
                            !matches!(
                                entry.classification,
                                ImportClassification::Unauthorized | ImportClassification::Invalid
                            )
                        }
                    } else {
                        entry.classification == ImportClassification::Unauthorized
                    }
                })
        });
        let every_scope_authorized = self.scopes.iter().all(|decision| decision.authorized);
        if self.format_version != IMPORT_PLAN_FORMAT_VERSION
            || !crate::transaction::valid_content_digest(&self.bundle_digest)
            || self.target_snapshot.audit_sequence.0 > self.target_snapshot.store_revision.0
            || self.scopes.is_empty()
            || self.scopes.len() > MAX_IMPORT_SCOPES
            || self.entries.len() > MAX_IMPORT_PLAN_ITEMS
            || !scope_keys.windows(2).all(|pair| pair[0] < pair[1])
            || !entry_keys.windows(2).all(|pair| pair[0] < pair[1])
            || !unique_memory_ids
            || !entry_scope_semantics_are_valid
            || counts != self.counts
            || counts.total() != self.entries.len() as u64
            || self.committable != (counts.committable() && every_scope_authorized)
            || self.digest != self.expected_digest()?
        {
            return Err(StoreError::InvalidRequest);
        }
        Ok(())
    }

    /// Encode strict pretty JSON with exactly one trailing LF.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_IMPORT_PLAN_BYTES)
    }

    /// Decode only the exact canonical, digest-valid representation.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        decode_canonical(bytes, MAX_IMPORT_PLAN_BYTES, Self::validate)
    }

    #[cfg(test)]
    pub(crate) fn refresh_digest_for_test(&mut self) {
        self.digest = self.expected_digest().expect("test plan digest");
    }
}

/// Strict body-free recovery metadata for one committed portable import.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupMetadata {
    #[schemars(
        length(min = 31, max = 31),
        regex(pattern = r"^jiandu\.backup-metadata/v1alpha1$")
    )]
    pub format_version: String,
    pub store_id: StoreId,
    #[schemars(
        length(min = 36, max = 36),
        regex(pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
    )]
    pub transaction_id: String,
    pub base_snapshot: SnapshotWatermark,
    pub target_snapshot: SnapshotWatermark,
    pub source_store_id: StoreId,
    pub source_snapshot: SnapshotWatermark,
    #[schemars(length(min = 71, max = 71), regex(pattern = r"^sha256:[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    pub plan_digest: ImportDigest,
    #[schemars(range(max = 100))]
    pub record_count: u32,
    #[schemars(range(max = 100))]
    pub tombstone_count: u32,
    pub digest: ImportDigest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedBackupMetadata<'a> {
    format_version: &'a str,
    store_id: &'a StoreId,
    transaction_id: &'a str,
    base_snapshot: SnapshotWatermark,
    target_snapshot: SnapshotWatermark,
    source_store_id: &'a StoreId,
    source_snapshot: SnapshotWatermark,
    bundle_digest: &'a str,
    plan_digest: &'a ImportDigest,
    record_count: u32,
    tombstone_count: u32,
}

impl BackupMetadata {
    fn expected_digest(&self) -> Result<ImportDigest, StoreError> {
        let payload = serde_json::to_vec(&UnsignedBackupMetadata {
            format_version: &self.format_version,
            store_id: &self.store_id,
            transaction_id: &self.transaction_id,
            base_snapshot: self.base_snapshot,
            target_snapshot: self.target_snapshot,
            source_store_id: &self.source_store_id,
            source_snapshot: self.source_snapshot,
            bundle_digest: &self.bundle_digest,
            plan_digest: &self.plan_digest,
            record_count: self.record_count,
            tombstone_count: self.tombstone_count,
        })
        .map_err(|_| StoreError::InvalidTransaction)?;
        Ok(ImportDigest::from_payload(
            b"jiandu/backup-metadata/v1\0",
            &payload,
        ))
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        self.plan_digest.validate()?;
        if self.format_version != BACKUP_METADATA_FORMAT_VERSION
            || !crate::transaction::valid_transaction_id(&self.transaction_id)
            || !crate::transaction::valid_content_digest(&self.bundle_digest)
            || self.base_snapshot.audit_sequence.0 > self.base_snapshot.store_revision.0
            || self.source_snapshot.audit_sequence.0 > self.source_snapshot.store_revision.0
            || self.target_snapshot.audit_sequence.0 > self.target_snapshot.store_revision.0
            || self
                .base_snapshot
                .store_revision
                .0
                .checked_add(1)
                .map(|next| next.max(self.source_snapshot.store_revision.0))
                != Some(self.target_snapshot.store_revision.0)
            || self.base_snapshot.audit_sequence.0.checked_add(1)
                != Some(self.target_snapshot.audit_sequence.0)
            || usize::try_from(self.record_count)
                .ok()
                .and_then(|records| {
                    usize::try_from(self.tombstone_count)
                        .ok()
                        .and_then(|tombstones| records.checked_add(tombstones))
                })
                .is_none_or(|total| total > MAX_IMPORT_ITEMS)
            || self.digest != self.expected_digest()?
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }

    /// Encode strict canonical backup metadata.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_BACKUP_METADATA_BYTES).map_err(|_| StoreError::InvalidTransaction)
    }

    /// Decode strict canonical backup metadata without exposing a path.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        decode_canonical(bytes, MAX_BACKUP_METADATA_BYTES, Self::validate)
            .map_err(|_| StoreError::InvalidTransaction)
    }
}

/// Strict body-free result of one committed portable batch import.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableImportResult {
    #[schemars(
        length(min = 29, max = 29),
        regex(pattern = r"^jiandu\.import-result/v1alpha1$")
    )]
    pub format_version: String,
    #[schemars(
        length(min = 36, max = 36),
        regex(pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
    )]
    pub transaction_id: String,
    pub source_store_id: StoreId,
    pub source_snapshot: SnapshotWatermark,
    pub target_store_id: StoreId,
    pub base_snapshot: SnapshotWatermark,
    pub target_snapshot: SnapshotWatermark,
    #[schemars(length(min = 71, max = 71), regex(pattern = r"^sha256:[0-9a-f]{64}$"))]
    pub bundle_digest: String,
    pub plan_digest: ImportDigest,
    pub backup_digest: ImportDigest,
    #[schemars(range(max = 100))]
    pub record_count: u32,
    #[schemars(range(max = 100))]
    pub tombstone_count: u32,
    pub digest: ImportDigest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedImportResult<'a> {
    format_version: &'a str,
    transaction_id: &'a str,
    source_store_id: &'a StoreId,
    source_snapshot: SnapshotWatermark,
    target_store_id: &'a StoreId,
    base_snapshot: SnapshotWatermark,
    target_snapshot: SnapshotWatermark,
    bundle_digest: &'a str,
    plan_digest: &'a ImportDigest,
    backup_digest: &'a ImportDigest,
    record_count: u32,
    tombstone_count: u32,
}

impl PortableImportResult {
    fn expected_digest(&self) -> Result<ImportDigest, StoreError> {
        let bytes = serde_json::to_vec(&UnsignedImportResult {
            format_version: &self.format_version,
            transaction_id: &self.transaction_id,
            source_store_id: &self.source_store_id,
            source_snapshot: self.source_snapshot,
            target_store_id: &self.target_store_id,
            base_snapshot: self.base_snapshot,
            target_snapshot: self.target_snapshot,
            bundle_digest: &self.bundle_digest,
            plan_digest: &self.plan_digest,
            backup_digest: &self.backup_digest,
            record_count: self.record_count,
            tombstone_count: self.tombstone_count,
        })
        .map_err(|_| StoreError::InvalidTransaction)?;
        Ok(ImportDigest::from_payload(
            b"jiandu/import-result/v1\0",
            &bytes,
        ))
    }

    fn validate(&self) -> Result<(), StoreError> {
        self.plan_digest.validate()?;
        self.backup_digest.validate()?;
        let total = usize::try_from(self.record_count).ok().and_then(|records| {
            usize::try_from(self.tombstone_count)
                .ok()
                .and_then(|tombstones| records.checked_add(tombstones))
        });
        if self.format_version != IMPORT_RESULT_FORMAT_VERSION
            || !crate::transaction::valid_transaction_id(&self.transaction_id)
            || !crate::transaction::valid_content_digest(&self.bundle_digest)
            || self.source_snapshot.audit_sequence.0 > self.source_snapshot.store_revision.0
            || self.base_snapshot.audit_sequence.0 > self.base_snapshot.store_revision.0
            || self.target_snapshot.audit_sequence.0 > self.target_snapshot.store_revision.0
            || self
                .base_snapshot
                .store_revision
                .0
                .checked_add(1)
                .map(|next| next.max(self.source_snapshot.store_revision.0))
                != Some(self.target_snapshot.store_revision.0)
            || self.base_snapshot.audit_sequence.0.checked_add(1)
                != Some(self.target_snapshot.audit_sequence.0)
            || total.is_none_or(|total| total > MAX_IMPORT_ITEMS)
            || self.digest != self.expected_digest()?
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }

    /// Encode strict canonical result bytes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_IMPORT_ARTIFACT_BYTES).map_err(|_| StoreError::InvalidTransaction)
    }

    /// Decode exact canonical, digest-bound result bytes.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        decode_canonical(bytes, MAX_IMPORT_ARTIFACT_BYTES, Self::validate)
            .map_err(|_| StoreError::InvalidTransaction)
    }
}

/// Host-facing import outcome. The replay bit is intentionally not persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportCommit {
    pub result: PortableImportResult,
    pub backup_metadata: BackupMetadata,
    pub idempotent_replay: bool,
}

/// Private-field host capability for reading WAL-persisted backup metadata.
/// It is independently grantable from import, export, mutation, and lifecycle
/// permissions and cannot create an unaudited artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedBackupMetadata {
    principal_id: PrincipalId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportRecordIntent {
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    pub(crate) revision: Revision,
    pub(crate) etag: Etag,
    pub(crate) content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportTombstoneIntent {
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    pub(crate) revision: Revision,
    pub(crate) etag: Etag,
    pub(crate) forgotten_at: Timestamp,
    pub(crate) content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportBinding {
    pub(crate) receipt_id: String,
    pub(crate) transaction_id: String,
    pub(crate) principal_digest: String,
    pub(crate) key_digest: String,
    pub(crate) request_fingerprint: String,
    pub(crate) scopes: Vec<MemoryScope>,
    pub(crate) items: Vec<ImportLedgerItem>,
    pub(crate) source_store_id: StoreId,
    pub(crate) source_snapshot: SnapshotWatermark,
    pub(crate) base_snapshot: SnapshotWatermark,
    pub(crate) bundle_digest: String,
    pub(crate) plan_digest: ImportDigest,
    pub(crate) record_count: u32,
    pub(crate) tombstone_count: u32,
    pub(crate) store_revision: StoreRevision,
    pub(crate) audit_sequence: AuditSequence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ImportLedgerItem {
    Record {
        memory_id: MemoryId,
        scope: MemoryScope,
        revision: Revision,
        etag: Etag,
        content_digest: String,
    },
    Tombstone {
        memory_id: MemoryId,
        scope: MemoryScope,
        revision: Revision,
        etag: Etag,
        forgotten_at: Timestamp,
        content_digest: String,
    },
}

impl ImportLedgerItem {
    fn memory_id(&self) -> &MemoryId {
        match self {
            Self::Record { memory_id, .. } | Self::Tombstone { memory_id, .. } => memory_id,
        }
    }

    fn scope(&self) -> &MemoryScope {
        match self {
            Self::Record { scope, .. } | Self::Tombstone { scope, .. } => scope,
        }
    }

    const fn kind(&self) -> ImportItemKind {
        match self {
            Self::Record { .. } => ImportItemKind::Record,
            Self::Tombstone { .. } => ImportItemKind::Tombstone,
        }
    }

    fn as_tombstone_intent(&self) -> Option<ImportTombstoneIntent> {
        match self {
            Self::Tombstone {
                memory_id,
                scope,
                revision,
                etag,
                forgotten_at,
                content_digest,
            } => Some(ImportTombstoneIntent {
                memory_id: memory_id.clone(),
                scope: scope.clone(),
                revision: *revision,
                etag: etag.clone(),
                forgotten_at: forgotten_at.clone(),
                content_digest: content_digest.clone(),
            }),
            Self::Record { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImportTransaction {
    pub(crate) base_store_metadata: StoreMetadata,
    pub(crate) target_store_metadata: StoreMetadata,
    pub(crate) binding: ImportBinding,
    pub(crate) records: Vec<ImportRecordIntent>,
    pub(crate) tombstones: Vec<ImportTombstoneIntent>,
    pub(crate) result_digest: String,
    pub(crate) receipt_digest: String,
    pub(crate) audit_digest: String,
    pub(crate) backup_digest: String,
}

impl ImportBinding {
    fn validate(&self) -> Result<(), StoreError> {
        self.plan_digest.validate()?;
        let total = usize::try_from(self.record_count).ok().and_then(|records| {
            usize::try_from(self.tombstone_count)
                .ok()
                .and_then(|tombstones| records.checked_add(tombstones))
        });
        let expected_store_revision = self
            .base_snapshot
            .store_revision
            .0
            .checked_add(1)
            .map(|next| next.max(self.source_snapshot.store_revision.0));
        let expected_audit_sequence = self.base_snapshot.audit_sequence.0.checked_add(1);
        if !valid_hex(&self.receipt_id, 64)
            || !crate::transaction::valid_transaction_id(&self.transaction_id)
            || !crate::transaction::valid_content_digest(&self.principal_digest)
            || !crate::transaction::valid_content_digest(&self.key_digest)
            || !crate::transaction::valid_content_digest(&self.request_fingerprint)
            || !crate::transaction::valid_content_digest(&self.bundle_digest)
            || self.scopes.is_empty()
            || self.scopes.len() > MAX_IMPORT_SCOPES
            || self.items.len() > MAX_IMPORT_ITEMS
            || !self
                .scopes
                .windows(2)
                .all(|pair| scope_key(&pair[0]) < scope_key(&pair[1]))
            || self.source_snapshot.audit_sequence.0 > self.source_snapshot.store_revision.0
            || self.base_snapshot.audit_sequence.0 > self.base_snapshot.store_revision.0
            || self.store_revision.0 == 0
            || self.audit_sequence.0 == 0
            || self.audit_sequence.0 > self.store_revision.0
            || expected_store_revision != Some(self.store_revision.0)
            || expected_audit_sequence != Some(self.audit_sequence.0)
            || total.is_none_or(|total| total > MAX_IMPORT_ITEMS)
        {
            return Err(StoreError::InvalidTransaction);
        }
        let mut record_count = 0_u32;
        let mut tombstone_count = 0_u32;
        let mut ids = BTreeSet::new();
        let mut previous = None;
        for item in &self.items {
            let key = (
                item.memory_id().clone(),
                item.kind(),
                scope_key(item.scope()),
            );
            if previous.as_ref().is_some_and(|previous| previous >= &key)
                || !ids.insert(item.memory_id().clone())
                || !self.scopes.iter().any(|scope| scope == item.scope())
            {
                return Err(StoreError::InvalidTransaction);
            }
            previous = Some(key);
            match item {
                ImportLedgerItem::Record {
                    revision,
                    etag,
                    content_digest,
                    ..
                } => {
                    if revision.get() > self.source_snapshot.store_revision.0
                        || revision.get() > self.store_revision.0
                        || !crate::transaction::valid_content_digest(etag.as_str())
                        || !crate::transaction::valid_content_digest(content_digest)
                    {
                        return Err(StoreError::InvalidTransaction);
                    }
                    record_count = record_count
                        .checked_add(1)
                        .ok_or(StoreError::InvalidTransaction)?;
                }
                ImportLedgerItem::Tombstone {
                    revision,
                    etag,
                    content_digest,
                    ..
                } => {
                    if revision.get() > self.source_snapshot.store_revision.0
                        || revision.get() > self.store_revision.0
                        || !crate::transaction::valid_content_digest(etag.as_str())
                        || !crate::transaction::valid_content_digest(content_digest)
                    {
                        return Err(StoreError::InvalidTransaction);
                    }
                    tombstone_count = tombstone_count
                        .checked_add(1)
                        .ok_or(StoreError::InvalidTransaction)?;
                }
            }
        }
        if record_count != self.record_count || tombstone_count != self.tombstone_count {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

pub(crate) fn validate_import_transaction(
    transaction: &ImportTransaction,
    store_id: &StoreId,
    transaction_id: &str,
    manifest_format: &str,
) -> Result<(), StoreError> {
    if manifest_format != crate::transaction::TRANSACTION_FORMAT_VERSION
        || transaction.base_store_metadata.format_version != crate::STORE_FORMAT_VERSION
        || transaction.target_store_metadata.format_version != crate::STORE_FORMAT_VERSION
        || &transaction.base_store_metadata.store_id != store_id
        || &transaction.target_store_metadata.store_id != store_id
        || transaction.base_store_metadata.created_at
            != transaction.target_store_metadata.created_at
        || transaction.binding.transaction_id != transaction_id
        || transaction.binding.store_revision != transaction.target_store_metadata.store_revision
        || transaction.binding.audit_sequence != transaction.target_store_metadata.audit_sequence
        || transaction.binding.base_snapshot.store_revision
            != transaction.base_store_metadata.store_revision
        || transaction.binding.base_snapshot.audit_sequence
            != transaction.base_store_metadata.audit_sequence
    {
        return Err(StoreError::InvalidTransaction);
    }
    transaction.binding.validate()?;
    let next_revision = transaction
        .base_store_metadata
        .store_revision
        .0
        .checked_add(1)
        .ok_or(StoreError::InvalidTransaction)?;
    let next_audit = transaction
        .base_store_metadata
        .audit_sequence
        .0
        .checked_add(1)
        .ok_or(StoreError::InvalidTransaction)?;
    if transaction.target_store_metadata.store_revision.0
        != next_revision.max(transaction.binding.source_snapshot.store_revision.0)
        || transaction.target_store_metadata.audit_sequence.0 != next_audit
        || transaction.records.len() != transaction.binding.record_count as usize
        || transaction.tombstones.len() != transaction.binding.tombstone_count as usize
        || [
            &transaction.result_digest,
            &transaction.receipt_digest,
            &transaction.audit_digest,
            &transaction.backup_digest,
        ]
        .into_iter()
        .any(|digest| !crate::transaction::valid_content_digest(digest))
    {
        return Err(StoreError::InvalidTransaction);
    }
    let mut ids = BTreeSet::new();
    let mut record_keys = Vec::with_capacity(transaction.records.len());
    for record in &transaction.records {
        if !ids.insert(record.memory_id.clone())
            || !crate::transaction::valid_content_digest(record.etag.as_str())
            || !crate::transaction::valid_content_digest(&record.content_digest)
            || record.revision.get() > transaction.target_store_metadata.store_revision.0
            || !transaction
                .binding
                .scopes
                .iter()
                .any(|scope| scope == &record.scope)
        {
            return Err(StoreError::InvalidTransaction);
        }
        record_keys.push((record.memory_id.clone(), scope_key(&record.scope)));
    }
    let mut tombstone_keys = Vec::with_capacity(transaction.tombstones.len());
    for tombstone in &transaction.tombstones {
        if !ids.insert(tombstone.memory_id.clone())
            || !crate::transaction::valid_content_digest(tombstone.etag.as_str())
            || !crate::transaction::valid_content_digest(&tombstone.content_digest)
            || tombstone.revision.get() > transaction.target_store_metadata.store_revision.0
            || !transaction
                .binding
                .scopes
                .iter()
                .any(|scope| scope == &tombstone.scope)
        {
            return Err(StoreError::InvalidTransaction);
        }
        tombstone_keys.push((tombstone.memory_id.clone(), scope_key(&tombstone.scope)));
    }
    if !record_keys.windows(2).all(|pair| pair[0] < pair[1])
        || !tombstone_keys.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(StoreError::InvalidTransaction);
    }
    let expected_items = transaction
        .records
        .iter()
        .map(|record| ImportLedgerItem::Record {
            memory_id: record.memory_id.clone(),
            scope: record.scope.clone(),
            revision: record.revision,
            etag: record.etag.clone(),
            content_digest: record.content_digest.clone(),
        })
        .chain(
            transaction
                .tombstones
                .iter()
                .map(|tombstone| ImportLedgerItem::Tombstone {
                    memory_id: tombstone.memory_id.clone(),
                    scope: tombstone.scope.clone(),
                    revision: tombstone.revision,
                    etag: tombstone.etag.clone(),
                    forgotten_at: tombstone.forgotten_at.clone(),
                    content_digest: tombstone.content_digest.clone(),
                }),
        )
        .collect::<Vec<_>>();
    let mut expected_items = expected_items;
    expected_items.sort_by_key(|item| {
        (
            item.memory_id().clone(),
            item.kind(),
            scope_key(item.scope()),
        )
    });
    if transaction.binding.items != expected_items {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableImportReceipt {
    format_version: String,
    store_id: StoreId,
    binding: ImportBinding,
    result_digest: String,
    backup_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableImportAudit {
    format_version: String,
    store_id: StoreId,
    binding: ImportBinding,
    result_digest: String,
    backup_digest: String,
}

struct ImportArtifacts {
    result: PortableImportResult,
    backup: BackupMetadata,
    result_bytes: Vec<u8>,
    result_digest: String,
    receipt_bytes: Vec<u8>,
    receipt_digest: String,
    audit_bytes: Vec<u8>,
    audit_digest: String,
    backup_bytes: Vec<u8>,
    backup_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportReceiptIdentity {
    receipt_id: String,
    principal_digest: String,
    key_digest: String,
}

impl ImportReceiptIdentity {
    fn derive(principal_id: &PrincipalId, key: &IdempotencyKey) -> Self {
        let principal_digest = digest_text(b"jiandu/import-principal/v1\0", principal_id.as_str());
        let key_digest = digest_text(b"jiandu/import-idempotency-key/v1\0", key.as_str());
        let mut hasher = Sha256::new();
        hasher.update(b"jiandu/import-receipt-identity/v1\0");
        hasher.update(principal_id.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(key_digest.as_bytes());
        let receipt_id = hex_digest(&hasher.finalize());
        Self {
            receipt_id,
            principal_digest,
            key_digest,
        }
    }
}

impl DurableImportReceipt {
    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        if self.format_version != IMPORT_RECEIPT_FORMAT_VERSION
            || !crate::transaction::valid_content_digest(&self.result_digest)
            || !crate::transaction::valid_content_digest(&self.backup_digest)
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        crate::idempotency::canonical_json(self, MAX_IMPORT_ARTIFACT_BYTES)
    }

    fn decode(file: File, store_id: &StoreId, receipt_id: &str) -> Result<Self, StoreError> {
        crate::idempotency::decode_canonical(file, MAX_IMPORT_ARTIFACT_BYTES, |receipt: &Self| {
            receipt.validate()?;
            if &receipt.store_id != store_id || receipt.binding.receipt_id != receipt_id {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }
}

impl DurableImportAudit {
    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        if self.format_version != IMPORT_AUDIT_FORMAT_VERSION
            || !crate::transaction::valid_content_digest(&self.result_digest)
            || !crate::transaction::valid_content_digest(&self.backup_digest)
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        crate::idempotency::canonical_json(self, MAX_IMPORT_ARTIFACT_BYTES)
    }

    fn decode(file: File, store_id: &StoreId, sequence: AuditSequence) -> Result<Self, StoreError> {
        crate::idempotency::decode_canonical(file, MAX_IMPORT_ARTIFACT_BYTES, |audit: &Self| {
            audit.validate()?;
            if &audit.store_id != store_id || audit.binding.audit_sequence != sequence {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }
}

impl ImportArtifacts {
    fn build_with_base(
        store_id: StoreId,
        binding: ImportBinding,
        base_snapshot: SnapshotWatermark,
    ) -> Result<Self, StoreError> {
        binding.validate()?;
        if base_snapshot != binding.base_snapshot {
            return Err(StoreError::InvalidTransaction);
        }
        let target_snapshot = SnapshotWatermark {
            store_revision: binding.store_revision,
            audit_sequence: binding.audit_sequence,
        };
        let mut backup = BackupMetadata {
            format_version: BACKUP_METADATA_FORMAT_VERSION.to_owned(),
            store_id: store_id.clone(),
            transaction_id: binding.transaction_id.clone(),
            base_snapshot,
            target_snapshot,
            source_store_id: binding.source_store_id.clone(),
            source_snapshot: binding.source_snapshot,
            bundle_digest: binding.bundle_digest.clone(),
            plan_digest: binding.plan_digest.clone(),
            record_count: binding.record_count,
            tombstone_count: binding.tombstone_count,
            digest: ImportDigest(String::new()),
        };
        backup.digest = backup.expected_digest()?;
        let backup_bytes = backup.canonical_bytes()?;
        let backup_digest = crate::idempotency::content_digest(&backup_bytes);
        let mut result = PortableImportResult {
            format_version: IMPORT_RESULT_FORMAT_VERSION.to_owned(),
            transaction_id: binding.transaction_id.clone(),
            source_store_id: binding.source_store_id.clone(),
            source_snapshot: binding.source_snapshot,
            target_store_id: store_id.clone(),
            base_snapshot,
            target_snapshot,
            bundle_digest: binding.bundle_digest.clone(),
            plan_digest: binding.plan_digest.clone(),
            backup_digest: ImportDigest(backup_digest.clone()),
            record_count: binding.record_count,
            tombstone_count: binding.tombstone_count,
            digest: ImportDigest(String::new()),
        };
        result.digest = result.expected_digest()?;
        let result_bytes = result.canonical_bytes()?;
        let result_digest = crate::idempotency::content_digest(&result_bytes);
        let receipt = DurableImportReceipt {
            format_version: IMPORT_RECEIPT_FORMAT_VERSION.to_owned(),
            store_id: store_id.clone(),
            binding: binding.clone(),
            result_digest: result_digest.clone(),
            backup_digest: backup_digest.clone(),
        };
        let receipt_bytes = receipt.canonical_bytes()?;
        let receipt_digest = crate::idempotency::content_digest(&receipt_bytes);
        let audit = DurableImportAudit {
            format_version: IMPORT_AUDIT_FORMAT_VERSION.to_owned(),
            store_id,
            binding,
            result_digest: result_digest.clone(),
            backup_digest: backup_digest.clone(),
        };
        let audit_bytes = audit.canonical_bytes()?;
        let audit_digest = crate::idempotency::content_digest(&audit_bytes);
        Ok(Self {
            result,
            backup,
            result_bytes,
            result_digest,
            receipt_bytes,
            receipt_digest,
            audit_bytes,
            audit_digest,
            backup_bytes,
            backup_digest,
        })
    }

    fn from_intent(store_id: StoreId, transaction: &ImportTransaction) -> Result<Self, StoreError> {
        let artifacts = Self::build_with_base(
            store_id,
            transaction.binding.clone(),
            SnapshotWatermark {
                store_revision: transaction.base_store_metadata.store_revision,
                audit_sequence: transaction.base_store_metadata.audit_sequence,
            },
        )?;
        if artifacts.result_digest != transaction.result_digest
            || artifacts.receipt_digest != transaction.receipt_digest
            || artifacts.audit_digest != transaction.audit_digest
            || artifacts.backup_digest != transaction.backup_digest
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(artifacts)
    }
}

fn import_receipt_relative(binding: &ImportBinding) -> Result<PathBuf, StoreError> {
    binding.validate()?;
    Ok(PathBuf::from(crate::layout::IMPORT_RECEIPTS_DIR)
        .join(&binding.principal_digest[7..])
        .join(&binding.receipt_id[..2])
        .join(format!("{}.json", binding.receipt_id)))
}

fn import_receipt_relative_for_identity(
    identity: &ImportReceiptIdentity,
) -> Result<PathBuf, StoreError> {
    if !valid_hex(&identity.receipt_id, 64)
        || !crate::transaction::valid_content_digest(&identity.principal_digest)
        || !crate::transaction::valid_content_digest(&identity.key_digest)
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(crate::layout::IMPORT_RECEIPTS_DIR)
        .join(&identity.principal_digest[7..])
        .join(&identity.receipt_id[..2])
        .join(format!("{}.json", identity.receipt_id)))
}

fn import_result_relative(receipt_id: &str) -> Result<PathBuf, StoreError> {
    if !valid_hex(receipt_id, 64) {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(crate::layout::IMPORT_RESULTS_DIR)
        .join(&receipt_id[..2])
        .join(format!("{receipt_id}.json")))
}

fn import_audit_relative(sequence: AuditSequence) -> Result<PathBuf, StoreError> {
    if sequence.0 == 0 {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(crate::layout::IMPORT_AUDIT_DIR).join(format!("{:020}.json", sequence.0)))
}

fn import_backup_relative(transaction_id: &str) -> Result<PathBuf, StoreError> {
    if !crate::transaction::valid_transaction_id(transaction_id) {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(crate::layout::IMPORT_BACKUPS_DIR).join(format!("{transaction_id}.json")))
}

fn temp_sibling(target: &Path, kind: &str, transaction_id: &str) -> Result<PathBuf, StoreError> {
    if !crate::transaction::valid_transaction_id(transaction_id) {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(target
        .parent()
        .ok_or(StoreError::InvalidTransaction)?
        .join(format!(".{kind}-{transaction_id}.tmp")))
}

fn read_import_result(
    root: &crate::layout::StoreDirectory,
    store_id: &StoreId,
    binding: &ImportBinding,
    expected_digest: &str,
) -> Result<PortableImportResult, StoreError> {
    if !crate::transaction::valid_content_digest(expected_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    let file = root
        .try_open_regular(&import_result_relative(&binding.receipt_id)?, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    validate_import_artifact_file(&file)?;
    if crate::transaction::raw_file_digest(
        file.try_clone()
            .map_err(|source| StoreError::io("clone import result", source))?,
    )? != expected_digest
    {
        return Err(StoreError::InvalidTransaction);
    }
    crate::idempotency::decode_canonical(
        file,
        MAX_IMPORT_ARTIFACT_BYTES,
        |result: &PortableImportResult| {
            result.validate()?;
            if &result.target_store_id != store_id
                || result.transaction_id != binding.transaction_id
                || result.source_store_id != binding.source_store_id
                || result.source_snapshot != binding.source_snapshot
                || result.base_snapshot != binding.base_snapshot
                || result.target_snapshot.store_revision != binding.store_revision
                || result.target_snapshot.audit_sequence != binding.audit_sequence
                || result.bundle_digest != binding.bundle_digest
                || result.plan_digest != binding.plan_digest
                || result.record_count != binding.record_count
                || result.tombstone_count != binding.tombstone_count
            {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        },
    )
}

fn verify_import_backup(
    root: &crate::layout::StoreDirectory,
    store_id: &StoreId,
    binding: &ImportBinding,
    expected_digest: &str,
) -> Result<BackupMetadata, StoreError> {
    if !crate::transaction::valid_content_digest(expected_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    let file = root
        .try_open_regular(&import_backup_relative(&binding.transaction_id)?, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    validate_import_artifact_file(&file)?;
    if crate::transaction::raw_file_digest(
        file.try_clone()
            .map_err(|source| StoreError::io("clone import backup metadata", source))?,
    )? != expected_digest
    {
        return Err(StoreError::InvalidTransaction);
    }
    crate::idempotency::decode_canonical(
        file,
        MAX_BACKUP_METADATA_BYTES,
        |backup: &BackupMetadata| {
            backup.validate()?;
            if &backup.store_id != store_id
                || backup.transaction_id != binding.transaction_id
                || backup.source_store_id != binding.source_store_id
                || backup.source_snapshot != binding.source_snapshot
                || backup.base_snapshot != binding.base_snapshot
                || backup.target_snapshot.store_revision != binding.store_revision
                || backup.target_snapshot.audit_sequence != binding.audit_sequence
                || backup.bundle_digest != binding.bundle_digest
                || backup.plan_digest != binding.plan_digest
                || backup.record_count != binding.record_count
                || backup.tombstone_count != binding.tombstone_count
            {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        },
    )
}

fn verify_import_audit(
    root: &crate::layout::StoreDirectory,
    store_id: &StoreId,
    binding: &ImportBinding,
    result_digest: &str,
    backup_digest: &str,
) -> Result<(), StoreError> {
    let file = root
        .try_open_regular(&import_audit_relative(binding.audit_sequence)?, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    validate_import_artifact_file(&file)?;
    let audit = DurableImportAudit::decode(file, store_id, binding.audit_sequence)?;
    if audit.binding != *binding
        || audit.result_digest != result_digest
        || audit.backup_digest != backup_digest
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

pub(crate) struct ImportLedgerInspection {
    pub(crate) sequences: BTreeSet<AuditSequence>,
    pub(crate) tombstone_paths: BTreeSet<PathBuf>,
}

pub(crate) fn inspect_import_ledger(
    root: &crate::layout::StoreDirectory,
    metadata: &StoreMetadata,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<ImportLedgerInspection, (crate::idempotency::LedgerIssue, StoreError)> {
    use crate::idempotency::LedgerIssue;

    if metadata.format_version != crate::STORE_FORMAT_VERSION {
        return Ok(ImportLedgerInspection {
            sequences: BTreeSet::new(),
            tombstone_paths: BTreeSet::new(),
        });
    }

    let receipt_root = Path::new(crate::layout::IMPORT_RECEIPTS_DIR);
    let principal_directories = root
        .open_directory(receipt_root)
        .map_err(|error| (LedgerIssue::Receipt, error))?;
    let mut sequences = BTreeSet::new();
    let mut tombstone_paths = BTreeSet::new();
    let mut expected_results = BTreeSet::new();
    let mut expected_audits = BTreeSet::new();
    let mut expected_backups = BTreeSet::new();

    for principal_name in crate::idempotency::entry_names(
        &principal_directories,
        "list import receipt principals",
        budget,
    )
    .map_err(|error| (LedgerIssue::Receipt, error))?
    {
        let principal = principal_name
            .to_str()
            .filter(|value| valid_hex(value, 64))
            .ok_or((LedgerIssue::Receipt, StoreError::InvalidTransaction))?;
        let principal_directory = crate::layout::StoreDirectory::open_child_directory(
            &principal_directories,
            &principal_name,
        )
        .map_err(|error| (LedgerIssue::Receipt, error))?;
        for shard_name in crate::idempotency::entry_names(
            &principal_directory,
            "list import receipt shards",
            budget,
        )
        .map_err(|error| (LedgerIssue::Receipt, error))?
        {
            let shard = shard_name
                .to_str()
                .filter(|value| valid_hex(value, 2))
                .ok_or((LedgerIssue::Receipt, StoreError::InvalidTransaction))?;
            let shard_directory = crate::layout::StoreDirectory::open_child_directory(
                &principal_directory,
                &shard_name,
            )
            .map_err(|error| (LedgerIssue::Receipt, error))?;
            for file_name in crate::idempotency::entry_names(
                &shard_directory,
                "list import receipt artifacts",
                budget,
            )
            .map_err(|error| (LedgerIssue::Receipt, error))?
            {
                let receipt_id = canonical_json_hex_name(&file_name, 64)
                    .map_err(|error| (LedgerIssue::Receipt, error))?;
                let relative = receipt_root.join(principal).join(shard).join(&file_name);
                let file = crate::layout::StoreDirectory::try_open_regular_in(
                    &shard_directory,
                    &file_name,
                )
                .map_err(|error| (LedgerIssue::Receipt, error))?
                .ok_or((LedgerIssue::Receipt, StoreError::InvalidTransaction))?;
                crate::idempotency::charge_file(&file, budget)
                    .map_err(|error| (LedgerIssue::Receipt, error))?;
                crate::layout::StoreDirectory::validate_private_open_file(&file)
                    .map_err(|error| (LedgerIssue::Receipt, error))?;
                if !crate::layout::StoreDirectory::has_single_link(&file)
                    .map_err(|error| (LedgerIssue::Receipt, error))?
                {
                    return Err((LedgerIssue::Receipt, StoreError::UnsafePath));
                }
                let receipt = DurableImportReceipt::decode(file, &metadata.store_id, receipt_id)
                    .map_err(|error| (LedgerIssue::Receipt, error))?;
                let binding = &receipt.binding;
                if import_receipt_relative(binding).ok().as_deref() != Some(relative.as_path())
                    || &binding.principal_digest[7..] != principal
                    || &binding.receipt_id[..2] != shard
                    || binding.store_revision.0 > metadata.store_revision.0
                    || binding.audit_sequence.0 > metadata.audit_sequence.0
                    || !sequences.insert(binding.audit_sequence)
                {
                    return Err((LedgerIssue::Receipt, StoreError::InvalidTransaction));
                }
                let artifacts = ImportArtifacts::build_with_base(
                    metadata.store_id.clone(),
                    binding.clone(),
                    binding.base_snapshot,
                )
                .map_err(|error| (LedgerIssue::Receipt, error))?;
                if receipt.result_digest != artifacts.result_digest
                    || receipt.backup_digest != artifacts.backup_digest
                {
                    return Err((LedgerIssue::Receipt, StoreError::InvalidTransaction));
                }

                let result_path = import_result_relative(&binding.receipt_id)
                    .map_err(|error| (LedgerIssue::Result, error))?;
                if !expected_results.insert(result_path.clone()) {
                    return Err((LedgerIssue::Result, StoreError::InvalidTransaction));
                }
                let result_file = open_bounded_import_artifact(
                    root,
                    &result_path,
                    &artifacts.result_digest,
                    budget,
                )
                .map_err(|error| (LedgerIssue::Result, error))?;
                let result = crate::idempotency::decode_canonical(
                    result_file,
                    MAX_IMPORT_ARTIFACT_BYTES,
                    PortableImportResult::validate,
                )
                .map_err(|error| (LedgerIssue::Result, error))?;
                if result != artifacts.result {
                    return Err((LedgerIssue::Result, StoreError::InvalidTransaction));
                }

                let backup_path = import_backup_relative(&binding.transaction_id)
                    .map_err(|error| (LedgerIssue::Backup, error))?;
                if !expected_backups.insert(backup_path.clone()) {
                    return Err((LedgerIssue::Backup, StoreError::InvalidTransaction));
                }
                let backup_file = open_bounded_import_artifact(
                    root,
                    &backup_path,
                    &artifacts.backup_digest,
                    budget,
                )
                .map_err(|error| (LedgerIssue::Backup, error))?;
                let backup = crate::idempotency::decode_canonical(
                    backup_file,
                    MAX_BACKUP_METADATA_BYTES,
                    BackupMetadata::validate,
                )
                .map_err(|error| (LedgerIssue::Backup, error))?;
                if backup
                    != artifacts
                        .result_backup()
                        .map_err(|error| (LedgerIssue::Backup, error))?
                {
                    return Err((LedgerIssue::Backup, StoreError::InvalidTransaction));
                }

                let audit_path = import_audit_relative(binding.audit_sequence)
                    .map_err(|error| (LedgerIssue::Audit, error))?;
                if !expected_audits.insert(audit_path.clone()) {
                    return Err((LedgerIssue::Audit, StoreError::InvalidTransaction));
                }
                let audit_file = open_bounded_import_artifact(
                    root,
                    &audit_path,
                    &artifacts.audit_digest,
                    budget,
                )
                .map_err(|error| (LedgerIssue::Audit, error))?;
                let audit = DurableImportAudit::decode(
                    audit_file,
                    &metadata.store_id,
                    binding.audit_sequence,
                )
                .map_err(|error| (LedgerIssue::Audit, error))?;
                if audit.binding != *binding
                    || audit.result_digest != artifacts.result_digest
                    || audit.backup_digest != artifacts.backup_digest
                {
                    return Err((LedgerIssue::Audit, StoreError::InvalidTransaction));
                }

                for item in &binding.items {
                    match item {
                        // Imported records enter the ordinary mutation lifecycle after
                        // commit. Their historical bytes stay digest-bound in the
                        // receipt/result/audit/backup, but a later update or forget must
                        // not make that historical receipt invalidate startup.
                        ImportLedgerItem::Record { .. } => {}
                        ImportLedgerItem::Tombstone { .. } => {
                            let path =
                                validate_imported_tombstone(root, metadata, binding, item, budget)
                                    .map_err(|error| (LedgerIssue::Tombstone, error))?;
                            if !tombstone_paths.insert(path) {
                                return Err((
                                    LedgerIssue::Tombstone,
                                    StoreError::InvalidTransaction,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    validate_import_result_namespace(root, &expected_results, budget)
        .map_err(|error| (LedgerIssue::Result, error))?;
    validate_flat_import_namespace(
        root,
        Path::new(crate::layout::IMPORT_AUDIT_DIR),
        &expected_audits,
        budget,
    )
    .map_err(|error| (LedgerIssue::Audit, error))?;
    validate_flat_import_namespace(
        root,
        Path::new(crate::layout::IMPORT_BACKUPS_DIR),
        &expected_backups,
        budget,
    )
    .map_err(|error| (LedgerIssue::Backup, error))?;

    Ok(ImportLedgerInspection {
        sequences,
        tombstone_paths,
    })
}

impl ImportArtifacts {
    fn result_backup(&self) -> Result<BackupMetadata, StoreError> {
        Ok(self.backup.clone())
    }
}

fn open_bounded_import_artifact(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected_digest: &str,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<File, StoreError> {
    let file = root
        .try_open_regular(relative, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    crate::idempotency::charge_file(&file, budget)?;
    validate_import_artifact_file(&file)?;
    if crate::transaction::raw_file_digest(
        file.try_clone()
            .map_err(|source| StoreError::io("clone import ledger artifact", source))?,
    )? != expected_digest
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(file)
}

fn validate_import_artifact_file(file: &File) -> Result<(), StoreError> {
    crate::layout::StoreDirectory::validate_private_open_file(file)?;
    if !crate::layout::StoreDirectory::has_single_link(file)? {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn import_file_identity_matches(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected: crate::layout::FileIdentity,
) -> Result<bool, StoreError> {
    let Some(file) = root.try_open_regular(relative, false)? else {
        return Ok(false);
    };
    validate_import_artifact_file(&file)?;
    Ok(crate::layout::FileIdentity::from_file(&file)? == expected)
}

fn validate_imported_tombstone(
    root: &crate::layout::StoreDirectory,
    metadata: &StoreMetadata,
    binding: &ImportBinding,
    item: &ImportLedgerItem,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<PathBuf, StoreError> {
    let intent = item
        .as_tombstone_intent()
        .ok_or(StoreError::InvalidTransaction)?;
    let relative = import_tombstone_relative(&intent);
    let file = open_bounded_import_artifact(root, &relative, &intent.content_digest, budget)?;
    let tombstone = crate::tombstone::ProtectedTombstone::decode(file, &metadata.store_id)?;
    if tombstone.transaction_id != binding.transaction_id
        || tombstone.memory_id != intent.memory_id
        || tombstone.scope != intent.scope
        || tombstone.revision != intent.revision
        || tombstone.etag != intent.etag
        || tombstone.forgotten_at != intent.forgotten_at
        || tombstone.store_revision != binding.store_revision
        || tombstone.audit_sequence != binding.audit_sequence
        || crate::mutation::record_id_exists_anywhere_bounded(root, &intent.memory_id, budget)?
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(relative)
}

fn validate_import_result_namespace(
    root: &crate::layout::StoreDirectory,
    expected: &BTreeSet<PathBuf>,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<(), StoreError> {
    let namespace = Path::new(crate::layout::IMPORT_RESULTS_DIR);
    let directory = root.open_directory(namespace)?;
    let mut observed = BTreeSet::new();
    for shard_name in
        crate::idempotency::entry_names(&directory, "list import result shards", budget)?
    {
        let shard = shard_name
            .to_str()
            .filter(|value| valid_hex(value, 2))
            .ok_or(StoreError::InvalidTransaction)?;
        let shard_directory =
            crate::layout::StoreDirectory::open_child_directory(&directory, &shard_name)?;
        for file_name in crate::idempotency::entry_names(
            &shard_directory,
            "list import result artifacts",
            budget,
        )? {
            let receipt_id = canonical_json_hex_name(&file_name, 64)?;
            if &receipt_id[..2] != shard {
                return Err(StoreError::InvalidTransaction);
            }
            let relative = namespace.join(shard).join(&file_name);
            validate_expected_import_entry(
                &shard_directory,
                &file_name,
                &relative,
                expected,
                &mut observed,
                budget,
            )?;
        }
    }
    if &observed == expected {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn validate_flat_import_namespace(
    root: &crate::layout::StoreDirectory,
    namespace: &Path,
    expected: &BTreeSet<PathBuf>,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<(), StoreError> {
    let directory = root.open_directory(namespace)?;
    let mut observed = BTreeSet::new();
    for file_name in
        crate::idempotency::entry_names(&directory, "list import ledger artifacts", budget)?
    {
        let relative = namespace.join(&file_name);
        validate_expected_import_entry(
            &directory,
            &file_name,
            &relative,
            expected,
            &mut observed,
            budget,
        )?;
    }
    if &observed == expected {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn validate_expected_import_entry(
    directory: &cap_std::fs::Dir,
    file_name: &OsStr,
    relative: &Path,
    expected: &BTreeSet<PathBuf>,
    observed: &mut BTreeSet<PathBuf>,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<(), StoreError> {
    if !expected.contains(relative) || !observed.insert(relative.to_owned()) {
        return Err(StoreError::InvalidTransaction);
    }
    let file = crate::layout::StoreDirectory::try_open_regular_in(directory, file_name)?
        .ok_or(StoreError::InvalidTransaction)?;
    crate::idempotency::charge_file(&file, budget)?;
    crate::layout::StoreDirectory::validate_private_open_file(&file)?;
    if !crate::layout::StoreDirectory::has_single_link(&file)? {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn canonical_json_hex_name(name: &OsStr, length: usize) -> Result<&str, StoreError> {
    name.to_str()
        .and_then(|value| value.strip_suffix(".json"))
        .filter(|value| valid_hex(value, length))
        .ok_or(StoreError::InvalidTransaction)
}

pub(crate) fn recover_import(
    root: &crate::layout::StoreDirectory,
    metadata: StoreMetadata,
    manifest: &crate::transaction::TransactionManifest,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<StoreMetadata, StoreError> {
    let crate::transaction::TransactionIntent::Import(transaction) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    validate_import_transaction(
        transaction,
        &manifest.store_id,
        &manifest.transaction_id,
        &manifest.format_version,
    )?;
    let artifacts = ImportArtifacts::from_intent(manifest.store_id.clone(), transaction)?;
    let metadata_is_base = metadata == transaction.base_store_metadata;
    let metadata_is_target = metadata == transaction.target_store_metadata;
    if !metadata_is_base && !metadata_is_target {
        return Err(StoreError::InvalidTransaction);
    }

    let mut record_states = Vec::with_capacity(transaction.records.len());
    for intent in &transaction.records {
        record_states.push(inspect_import_file_pair(
            root,
            &import_record_relative(intent),
            &import_record_temp_relative(intent, &manifest.transaction_id)?,
            &intent.content_digest,
        )?);
    }
    let mut tombstone_states = Vec::with_capacity(transaction.tombstones.len());
    for intent in &transaction.tombstones {
        tombstone_states.push(inspect_import_file_pair(
            root,
            &import_tombstone_relative(intent),
            &import_tombstone_temp_relative(intent, &manifest.transaction_id)?,
            &intent.content_digest,
        )?);
    }
    let backup_target = import_backup_relative(&manifest.transaction_id)?;
    let backup_temp = temp_sibling(&backup_target, "backup", &manifest.transaction_id)?;
    let backup_state = inspect_import_file_pair(
        root,
        &backup_target,
        &backup_temp,
        &transaction.backup_digest,
    )?;
    let result_target = import_result_relative(&transaction.binding.receipt_id)?;
    let result_temp = temp_sibling(&result_target, "result", &manifest.transaction_id)?;
    let result_state = inspect_import_file_pair(
        root,
        &result_target,
        &result_temp,
        &transaction.result_digest,
    )?;
    let receipt_target = import_receipt_relative(&transaction.binding)?;
    let receipt_temp = temp_sibling(&receipt_target, "receipt", &manifest.transaction_id)?;
    let receipt_state = inspect_import_file_pair(
        root,
        &receipt_target,
        &receipt_temp,
        &transaction.receipt_digest,
    )?;
    let audit_target = import_audit_relative(transaction.binding.audit_sequence)?;
    let audit_temp = temp_sibling(&audit_target, "audit", &manifest.transaction_id)?;
    let audit_state =
        inspect_import_file_pair(root, &audit_target, &audit_temp, &transaction.audit_digest)?;
    let metadata_temp = crate::transaction::metadata_temp_relative(manifest)?;
    let metadata_temp_digest =
        crate::idempotency::content_digest(&transaction.target_store_metadata.canonical_bytes()?);
    let metadata_temp_state =
        inspect_import_staged_file(root, &metadata_temp, &metadata_temp_digest)?;

    let targets_complete = record_states.iter().all(ImportFileState::is_target)
        && tombstone_states.iter().all(ImportFileState::is_target);
    let any_target = record_states.iter().any(ImportFileState::is_target)
        || tombstone_states.iter().any(ImportFileState::is_target);
    let artifact_states = [backup_state, result_state, receipt_state, audit_state];
    let any_artifact = artifact_states.iter().any(ImportFileState::is_target);
    let artifacts_complete = artifact_states.iter().all(ImportFileState::is_target);
    if (result_state.is_target() && !backup_state.is_target())
        || (receipt_state.is_target() && !result_state.is_target())
        || (audit_state.is_target() && !receipt_state.is_target())
        || (any_artifact && !targets_complete)
    {
        return Err(StoreError::InvalidTransaction);
    }

    if metadata_is_target {
        if !targets_complete || !artifacts_complete || metadata_temp_state.is_some() {
            return Err(StoreError::InvalidTransaction);
        }
        revalidate_complete_import_targets(
            root,
            manifest,
            transaction,
            &record_states,
            &tombstone_states,
            backup_state,
            result_state,
            receipt_state,
            audit_state,
        )?;
        crate::transaction::remove_manifest(root, manifest, failpoints)?;
        failpoints.check(crate::PersistenceBoundary::RecoveryManifestDirectorySynced)?;
        return Ok(transaction.target_store_metadata.clone());
    }

    if !any_target && !any_artifact {
        rollback_import_staging(
            root,
            manifest,
            transaction,
            &record_states,
            &tombstone_states,
            backup_state,
            result_state,
            receipt_state,
            audit_state,
            &backup_temp,
            &result_temp,
            &receipt_temp,
            &audit_temp,
            &metadata_temp,
            metadata_temp_state,
            failpoints,
        )?;
        failpoints.check(crate::PersistenceBoundary::RecoveryManifestDirectorySynced)?;
        return Ok(transaction.base_store_metadata.clone());
    }

    for (intent, state) in transaction
        .records
        .iter()
        .zip(record_states.iter().copied())
    {
        recover_publish_import_pair(
            root,
            state,
            &import_record_temp_relative(intent, &manifest.transaction_id)?,
            &import_record_relative(intent),
            &intent.content_digest,
            crate::PersistenceBoundary::RecoveryRecordDirectorySynced,
            "recover imported record",
            failpoints,
        )?;
    }
    for (intent, state) in transaction
        .tombstones
        .iter()
        .zip(tombstone_states.iter().copied())
    {
        recover_publish_import_pair(
            root,
            state,
            &import_tombstone_temp_relative(intent, &manifest.transaction_id)?,
            &import_tombstone_relative(intent),
            &intent.content_digest,
            crate::PersistenceBoundary::RecoveryTombstoneSynced,
            "recover imported tombstone",
            failpoints,
        )?;
    }
    recover_publish_import_pair(
        root,
        backup_state,
        &backup_temp,
        &backup_target,
        &transaction.backup_digest,
        crate::PersistenceBoundary::RecoveryBackupMetadataDirectorySynced,
        "recover import backup metadata",
        failpoints,
    )?;
    recover_publish_import_pair(
        root,
        result_state,
        &result_temp,
        &result_target,
        &transaction.result_digest,
        crate::PersistenceBoundary::RecoveryMutationResultDirectorySynced,
        "recover import result",
        failpoints,
    )?;
    recover_publish_import_pair(
        root,
        receipt_state,
        &receipt_temp,
        &receipt_target,
        &transaction.receipt_digest,
        crate::PersistenceBoundary::RecoveryMutationReceiptDirectorySynced,
        "recover import receipt",
        failpoints,
    )?;
    recover_publish_import_pair(
        root,
        audit_state,
        &audit_temp,
        &audit_target,
        &transaction.audit_digest,
        crate::PersistenceBoundary::RecoveryMutationAuditDirectorySynced,
        "recover import audit",
        failpoints,
    )?;
    revalidate_complete_import_targets(
        root,
        manifest,
        transaction,
        &record_states,
        &tombstone_states,
        backup_state,
        result_state,
        receipt_state,
        audit_state,
    )?;
    if let Some(StagedImportFile::Exact(metadata_identity)) = metadata_temp_state {
        revalidate_exact_import_file(
            root,
            &metadata_temp,
            &metadata_temp_digest,
            metadata_identity,
        )?;
        root.rename(
            &metadata_temp,
            Path::new(crate::layout::STORE_METADATA_FILE),
        )?;
        revalidate_exact_import_file(
            root,
            Path::new(crate::layout::STORE_METADATA_FILE),
            &metadata_temp_digest,
            metadata_identity,
        )?;
        root.sync_root("recover imported store metadata")?;
        failpoints.check(crate::PersistenceBoundary::RecoveryMetadataDirectorySynced)?;
    } else if metadata_temp_state.is_none() {
        // The live writer may have renamed store.json after all batch targets
        // were durable but crashed before the root-directory sync. A reboot is
        // then allowed to expose the old metadata and no staging name. The
        // strict manifest plus exact target artifacts prove the target bytes,
        // so rebuild and republish canonical metadata just like the ordinary
        // single-record recovery protocol.
        crate::recovery::recover_target_metadata(root, manifest, failpoints)?;
    } else {
        return Err(StoreError::InvalidTransaction);
    }
    crate::transaction::remove_manifest(root, manifest, failpoints)?;
    failpoints.check(crate::PersistenceBoundary::RecoveryManifestDirectorySynced)?;
    // Reconstructing the strict artifacts here proves that the manifest alone
    // never needed a body to identify every final byte.
    let _ = artifacts;
    Ok(transaction.target_store_metadata.clone())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImportFileState {
    Absent,
    Staged(StagedImportFile),
    Target(crate::layout::FileIdentity),
}

impl ImportFileState {
    const fn is_target(&self) -> bool {
        matches!(self, Self::Target(_))
    }

    const fn staged_identity(self) -> Option<crate::layout::FileIdentity> {
        match self {
            Self::Staged(staged) => Some(staged.identity()),
            Self::Absent | Self::Target(_) => None,
        }
    }

    const fn published_identity(self) -> Option<crate::layout::FileIdentity> {
        match self {
            Self::Target(identity) | Self::Staged(StagedImportFile::Exact(identity)) => {
                Some(identity)
            }
            Self::Absent | Self::Staged(StagedImportFile::Incomplete(_)) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagedImportFile {
    Exact(crate::layout::FileIdentity),
    Incomplete(crate::layout::FileIdentity),
}

impl StagedImportFile {
    const fn identity(self) -> crate::layout::FileIdentity {
        match self {
            Self::Exact(identity) | Self::Incomplete(identity) => identity,
        }
    }
}

fn inspect_import_file_pair(
    root: &crate::layout::StoreDirectory,
    target: &Path,
    staged: &Path,
    expected_digest: &str,
) -> Result<ImportFileState, StoreError> {
    let target_identity = inspect_exact_file(root, target, expected_digest)?;
    let staged_state = inspect_import_staged_file(root, staged, expected_digest)?;
    match (target_identity, staged_state) {
        (None, None) => Ok(ImportFileState::Absent),
        (None, Some(state)) => Ok(ImportFileState::Staged(state)),
        (Some(identity), None) => Ok(ImportFileState::Target(identity)),
        (Some(_), Some(_)) => Err(StoreError::InvalidTransaction),
    }
}

fn inspect_import_staged_file(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected_digest: &str,
) -> Result<Option<StagedImportFile>, StoreError> {
    if !crate::transaction::valid_content_digest(expected_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    root.try_open_regular(relative, false)?
        .map(|file| {
            crate::layout::StoreDirectory::validate_private_open_file(&file)?;
            if !crate::layout::StoreDirectory::has_single_link(&file)? {
                return Err(StoreError::UnsafePath);
            }
            let identity = crate::layout::FileIdentity::from_file(&file)?;
            if crate::transaction::raw_file_digest(file)? == expected_digest {
                Ok(StagedImportFile::Exact(identity))
            } else {
                Ok(StagedImportFile::Incomplete(identity))
            }
        })
        .transpose()
}

fn inspect_exact_file(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected_digest: &str,
) -> Result<Option<crate::layout::FileIdentity>, StoreError> {
    if !crate::transaction::valid_content_digest(expected_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    root.try_open_regular(relative, false)?
        .map(|file| {
            crate::layout::StoreDirectory::validate_private_open_file(&file)?;
            if !crate::layout::StoreDirectory::has_single_link(&file)? {
                return Err(StoreError::UnsafePath);
            }
            let identity = crate::layout::FileIdentity::from_file(&file)?;
            if crate::transaction::raw_file_digest(file)? != expected_digest {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(identity)
        })
        .transpose()
}

fn revalidate_exact_import_file(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected_digest: &str,
    expected_identity: crate::layout::FileIdentity,
) -> Result<(), StoreError> {
    match inspect_exact_file(root, relative, expected_digest)? {
        Some(identity) if identity == expected_identity => Ok(()),
        Some(_) | None => Err(StoreError::UnsafePath),
    }
}

#[allow(clippy::too_many_arguments)]
fn revalidate_complete_import_targets(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    transaction: &ImportTransaction,
    record_states: &[ImportFileState],
    tombstone_states: &[ImportFileState],
    backup_state: ImportFileState,
    result_state: ImportFileState,
    receipt_state: ImportFileState,
    audit_state: ImportFileState,
) -> Result<(), StoreError> {
    for (intent, state) in transaction.records.iter().zip(record_states) {
        revalidate_exact_import_file(
            root,
            &import_record_relative(intent),
            &intent.content_digest,
            state
                .published_identity()
                .ok_or(StoreError::InvalidTransaction)?,
        )?;
    }
    for (intent, state) in transaction.tombstones.iter().zip(tombstone_states) {
        revalidate_exact_import_file(
            root,
            &import_tombstone_relative(intent),
            &intent.content_digest,
            state
                .published_identity()
                .ok_or(StoreError::InvalidTransaction)?,
        )?;
    }
    for (relative, digest, state) in [
        (
            import_backup_relative(&manifest.transaction_id)?,
            &transaction.backup_digest,
            backup_state,
        ),
        (
            import_result_relative(&transaction.binding.receipt_id)?,
            &transaction.result_digest,
            result_state,
        ),
        (
            import_receipt_relative(&transaction.binding)?,
            &transaction.receipt_digest,
            receipt_state,
        ),
        (
            import_audit_relative(transaction.binding.audit_sequence)?,
            &transaction.audit_digest,
            audit_state,
        ),
    ] {
        revalidate_exact_import_file(
            root,
            &relative,
            digest,
            state
                .published_identity()
                .ok_or(StoreError::InvalidTransaction)?,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rollback_import_staging(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    transaction: &ImportTransaction,
    record_states: &[ImportFileState],
    tombstone_states: &[ImportFileState],
    backup_state: ImportFileState,
    result_state: ImportFileState,
    receipt_state: ImportFileState,
    audit_state: ImportFileState,
    backup_temp: &Path,
    result_temp: &Path,
    receipt_temp: &Path,
    audit_temp: &Path,
    metadata_temp: &Path,
    metadata_temp_state: Option<StagedImportFile>,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    for (intent, state) in transaction.records.iter().zip(record_states) {
        remove_import_temp(
            root,
            &import_record_temp_relative(intent, &manifest.transaction_id)?,
            state.staged_identity(),
            crate::PersistenceBoundary::RecoveryRecordDirectorySynced,
            failpoints,
        )?;
    }
    for (intent, state) in transaction.tombstones.iter().zip(tombstone_states) {
        remove_import_temp(
            root,
            &import_tombstone_temp_relative(intent, &manifest.transaction_id)?,
            state.staged_identity(),
            crate::PersistenceBoundary::RecoveryTombstoneSynced,
            failpoints,
        )?;
    }
    for (relative, state, boundary) in [
        (
            backup_temp,
            backup_state,
            crate::PersistenceBoundary::RecoveryBackupMetadataDirectorySynced,
        ),
        (
            result_temp,
            result_state,
            crate::PersistenceBoundary::RecoveryMutationResultDirectorySynced,
        ),
        (
            receipt_temp,
            receipt_state,
            crate::PersistenceBoundary::RecoveryMutationReceiptDirectorySynced,
        ),
        (
            audit_temp,
            audit_state,
            crate::PersistenceBoundary::RecoveryMutationAuditDirectorySynced,
        ),
    ] {
        remove_import_temp(
            root,
            relative,
            state.staged_identity(),
            boundary,
            failpoints,
        )?;
    }
    remove_import_temp(
        root,
        metadata_temp,
        metadata_temp_state.map(StagedImportFile::identity),
        crate::PersistenceBoundary::RecoveryMetadataDirectorySynced,
        failpoints,
    )?;
    crate::transaction::remove_manifest(root, manifest, failpoints)
}

fn remove_import_temp(
    root: &crate::layout::StoreDirectory,
    relative: &Path,
    expected_identity: Option<crate::layout::FileIdentity>,
    boundary: crate::PersistenceBoundary,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let Some(expected_identity) = expected_identity else {
        return Ok(());
    };
    if !import_file_identity_matches(root, relative, expected_identity)? {
        return Err(StoreError::UnsafePath);
    }
    if !root.remove_regular_file_if_exists(relative)? {
        return Err(StoreError::UnsafePath);
    }
    crate::transaction::sync_parent(root, relative, "rollback staged import artifact")?;
    failpoints.check(boundary)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_publish_import_pair(
    root: &crate::layout::StoreDirectory,
    state: ImportFileState,
    staged: &Path,
    target: &Path,
    expected_digest: &str,
    boundary: crate::PersistenceBoundary,
    operation: &'static str,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    match state {
        ImportFileState::Target(identity) => {
            revalidate_exact_import_file(root, target, expected_digest, identity)
        }
        ImportFileState::Staged(StagedImportFile::Exact(identity)) => {
            revalidate_exact_import_file(root, staged, expected_digest, identity)?;
            root.rename(staged, target)?;
            revalidate_exact_import_file(root, target, expected_digest, identity)?;
            crate::transaction::sync_parent(root, target, operation)?;
            failpoints.check(boundary)
        }
        ImportFileState::Staged(StagedImportFile::Incomplete(_)) => {
            Err(StoreError::InvalidTransaction)
        }
        ImportFileState::Absent => Err(StoreError::InvalidTransaction),
    }
}

impl CanonicalStore {
    /// Produce a deterministic, side-effect-free import plan from strict
    /// canonical portable-export bytes and fresh per-scope authority.
    pub fn plan_import(
        &self,
        authority: &crate::AuthorizedScopes,
        context: &TrustedRequestContext,
        bundle_bytes: &[u8],
    ) -> Result<ImportDryRunPlan, StoreError> {
        self.validate_ownership()?;
        context
            .validate()
            .map_err(|_| StoreError::Unauthenticated)?;
        if context.principal_id != authority.principal_id {
            return Err(StoreError::Forbidden);
        }
        let bundle = PortableExportBundle::decode_canonical(bundle_bytes)?;
        let over_limit =
            bundle.records.len().saturating_add(bundle.tombstones.len()) > MAX_IMPORT_ITEMS;
        let scopes = bundle
            .scopes
            .iter()
            .map(|scope| ImportScopeDecision {
                scope: scope.clone(),
                authorized: import_scope_authorized(authority, context, scope),
            })
            .collect::<Vec<_>>();
        let authorized_scope_keys = scopes
            .iter()
            .filter(|decision| decision.authorized)
            .map(|decision| scope_key(&decision.scope))
            .collect::<BTreeSet<_>>();
        let mut entries =
            Vec::with_capacity(bundle.records.len().saturating_add(bundle.tombstones.len()));
        for record in &bundle.records {
            let authorized = authorized_scope_keys.contains(&scope_key(&record.scope));
            let classification = classify_target(
                self,
                &record.id,
                authorized,
                over_limit,
                ImportItemKind::Record,
            )?;
            entries.push(ImportPlanEntry {
                kind: ImportItemKind::Record,
                memory_id: record.id.clone(),
                scope: record.scope.clone(),
                classification,
            });
        }
        for tombstone in &bundle.tombstones {
            let authorized = authorized_scope_keys.contains(&scope_key(&tombstone.scope));
            let classification = classify_target(
                self,
                &tombstone.memory_id,
                authorized,
                over_limit,
                ImportItemKind::Tombstone,
            )?;
            entries.push(ImportPlanEntry {
                kind: ImportItemKind::Tombstone,
                memory_id: tombstone.memory_id.clone(),
                scope: tombstone.scope.clone(),
                classification,
            });
        }
        entries.sort_by_key(|entry| {
            (
                entry.memory_id.as_str().to_owned(),
                entry.kind,
                scope_key(&entry.scope),
            )
        });
        let mut counts = ImportPlanCounts::default();
        for entry in &entries {
            counts.observe(entry.classification)?;
        }
        let committable = counts.committable() && scopes.iter().all(|decision| decision.authorized);
        let mut plan = ImportDryRunPlan {
            format_version: IMPORT_PLAN_FORMAT_VERSION.to_owned(),
            source_store_id: bundle.source_store_id,
            bundle_digest: bundle.digest.as_str().to_owned(),
            target_store_id: self.metadata.store_id.clone(),
            target_snapshot: SnapshotWatermark {
                store_revision: self.metadata.store_revision,
                audit_sequence: self.metadata.audit_sequence,
            },
            scopes,
            entries,
            counts,
            committable,
            digest: ImportDigest(String::new()),
        };
        plan.digest = plan.expected_digest()?;
        plan.validate()?;
        let _ = plan.canonical_bytes()?;
        Ok(plan)
    }

    /// Commit one strict portable bundle as a single metadata-last batch.
    /// Fresh exact-scope authority and receipt replay are resolved before any
    /// target-state/CAS inspection or write.
    pub fn import_portable(
        &mut self,
        authority: &crate::AuthorizedScopes,
        context: &TrustedRequestContext,
        bundle_bytes: &[u8],
        expected_plan_digest: &ImportDigest,
        idempotency_key: &IdempotencyKey,
    ) -> Result<ImportCommit, StoreError> {
        self.validate_ownership()?;
        context
            .validate()
            .map_err(|_| StoreError::Unauthenticated)?;
        expected_plan_digest.validate()?;
        if context.principal_id != authority.principal_id {
            return Err(StoreError::Forbidden);
        }
        let bundle = PortableExportBundle::decode_canonical(bundle_bytes)?;
        require_import_authority(authority, context, &bundle.scopes)?;
        let identity = ImportReceiptIdentity::derive(&context.principal_id, idempotency_key);
        let request_fingerprint = import_request_fingerprint(&bundle, expected_plan_digest)?;
        if let Some(replay) = self.lookup_import_replay(
            &identity,
            &request_fingerprint,
            &bundle,
            expected_plan_digest,
        )? {
            return Ok(replay);
        }

        // Receipt lookup above deliberately precedes this fresh target-state
        // plan, so committed retries survive later conflicts or tombstones.
        let plan = self.plan_import(authority, context, bundle_bytes)?;
        if &plan.digest != expected_plan_digest || !plan.committable {
            return Err(StoreError::ValidationFailed);
        }
        let (disk_metadata, _) = crate::store::read_store_metadata(&self.root)?;
        if disk_metadata != self.metadata
            || plan.target_snapshot.store_revision != self.metadata.store_revision
        {
            return Err(StoreError::RecoveryRequired);
        }
        let mut target_metadata = self.metadata.clone();
        target_metadata.store_revision = StoreRevision(
            self.metadata
                .store_revision
                .0
                .checked_add(1)
                .ok_or(StoreError::RevisionOverflow)?
                .max(bundle.snapshot.store_revision.0),
        );
        target_metadata.audit_sequence = AuditSequence(
            self.metadata
                .audit_sequence
                .0
                .checked_add(1)
                .ok_or(StoreError::RevisionOverflow)?,
        );
        let transaction_id = crate::transaction::new_transaction_id();
        let mut record_stages = Vec::with_capacity(bundle.records.len());
        let mut record_intents = Vec::with_capacity(bundle.records.len());
        for portable in &bundle.records {
            let record: jiandu_core::MemoryRecord = portable.clone().into();
            let bytes = crate::document::encode_canonical_document(
                &jiandu_core::MemoryFrontmatterV1Alpha1::from_record(&record),
                &record.body,
            )?;
            let decoded = crate::document::decode_canonical_document(&bytes, Some(&record.id))?;
            if decoded.record != record {
                return Err(StoreError::InvalidRequest);
            }
            let intent = ImportRecordIntent {
                memory_id: record.id.clone(),
                scope: record.scope.clone(),
                revision: record.revision,
                etag: record.etag.clone(),
                content_digest: crate::idempotency::content_digest(&bytes),
            };
            record_stages.push((intent.clone(), bytes));
            record_intents.push(intent);
        }
        let mut tombstone_stages = Vec::with_capacity(bundle.tombstones.len());
        let mut tombstone_intents = Vec::with_capacity(bundle.tombstones.len());
        for portable in &bundle.tombstones {
            let tombstone = crate::tombstone::ProtectedTombstone::new(
                self.metadata.store_id.clone(),
                transaction_id.clone(),
                portable.memory_id.clone(),
                portable.scope.clone(),
                portable.revision,
                portable.etag.clone(),
                portable.forgotten_at.clone(),
                target_metadata.store_revision,
                target_metadata.audit_sequence,
            )?;
            let bytes = tombstone.canonical_bytes()?;
            let intent = ImportTombstoneIntent {
                memory_id: portable.memory_id.clone(),
                scope: portable.scope.clone(),
                revision: portable.revision,
                etag: portable.etag.clone(),
                forgotten_at: portable.forgotten_at.clone(),
                content_digest: crate::idempotency::content_digest(&bytes),
            };
            tombstone_stages.push((intent.clone(), bytes));
            tombstone_intents.push(intent);
        }
        let mut ledger_items = record_intents
            .iter()
            .map(|record| ImportLedgerItem::Record {
                memory_id: record.memory_id.clone(),
                scope: record.scope.clone(),
                revision: record.revision,
                etag: record.etag.clone(),
                content_digest: record.content_digest.clone(),
            })
            .chain(
                tombstone_intents
                    .iter()
                    .map(|tombstone| ImportLedgerItem::Tombstone {
                        memory_id: tombstone.memory_id.clone(),
                        scope: tombstone.scope.clone(),
                        revision: tombstone.revision,
                        etag: tombstone.etag.clone(),
                        forgotten_at: tombstone.forgotten_at.clone(),
                        content_digest: tombstone.content_digest.clone(),
                    }),
            )
            .collect::<Vec<_>>();
        ledger_items.sort_by_key(|item| {
            (
                item.memory_id().clone(),
                item.kind(),
                scope_key(item.scope()),
            )
        });
        let binding = ImportBinding {
            receipt_id: identity.receipt_id,
            transaction_id: transaction_id.clone(),
            principal_digest: identity.principal_digest,
            key_digest: identity.key_digest,
            request_fingerprint,
            scopes: bundle.scopes.clone(),
            items: ledger_items,
            source_store_id: bundle.source_store_id.clone(),
            source_snapshot: bundle.snapshot,
            base_snapshot: SnapshotWatermark {
                store_revision: self.metadata.store_revision,
                audit_sequence: self.metadata.audit_sequence,
            },
            bundle_digest: bundle.digest.as_str().to_owned(),
            plan_digest: plan.digest,
            record_count: u32::try_from(record_intents.len())
                .map_err(|_| StoreError::InvalidRequest)?,
            tombstone_count: u32::try_from(tombstone_intents.len())
                .map_err(|_| StoreError::InvalidRequest)?,
            store_revision: target_metadata.store_revision,
            audit_sequence: target_metadata.audit_sequence,
        };
        let artifacts = ImportArtifacts::build_with_base(
            self.metadata.store_id.clone(),
            binding.clone(),
            SnapshotWatermark {
                store_revision: self.metadata.store_revision,
                audit_sequence: self.metadata.audit_sequence,
            },
        )?;
        let manifest = crate::transaction::TransactionManifest::for_import(
            self.metadata.store_id.clone(),
            transaction_id,
            ImportTransaction {
                base_store_metadata: self.metadata.clone(),
                target_store_metadata: target_metadata,
                binding,
                records: record_intents,
                tombstones: tombstone_intents,
                result_digest: artifacts.result_digest.clone(),
                receipt_digest: artifacts.receipt_digest.clone(),
                audit_digest: artifacts.audit_digest.clone(),
                backup_digest: artifacts.backup_digest.clone(),
            },
        )?;
        // Bound the complete body-free v4 WAL before the commit path poisons
        // the live handle or writes a manifest staging byte.
        let _ = manifest.canonical_bytes()?;
        self.commit_import_batch(manifest, record_stages, tombstone_stages, artifacts)
    }

    fn lookup_import_replay(
        &self,
        identity: &ImportReceiptIdentity,
        request_fingerprint: &str,
        bundle: &PortableExportBundle,
        expected_plan_digest: &ImportDigest,
    ) -> Result<Option<ImportCommit>, StoreError> {
        let relative = import_receipt_relative_for_identity(identity)?;
        let Some(file) = self.root.try_open_regular(&relative, false)? else {
            return Ok(None);
        };
        validate_import_artifact_file(&file)?;
        let receipt =
            DurableImportReceipt::decode(file, &self.metadata.store_id, &identity.receipt_id)?;
        let binding = &receipt.binding;
        if binding.principal_digest != identity.principal_digest
            || binding.key_digest != identity.key_digest
            || binding.request_fingerprint != request_fingerprint
            || binding.scopes != bundle.scopes
            || binding.source_store_id != bundle.source_store_id
            || binding.source_snapshot != bundle.snapshot
            || binding.bundle_digest != bundle.digest.as_str()
            || &binding.plan_digest != expected_plan_digest
        {
            return Err(StoreError::IdempotencyConflict);
        }
        let result = read_import_result(
            &self.root,
            &self.metadata.store_id,
            binding,
            &receipt.result_digest,
        )?;
        let backup_metadata = verify_import_backup(
            &self.root,
            &self.metadata.store_id,
            binding,
            &receipt.backup_digest,
        )?;
        verify_import_audit(
            &self.root,
            &self.metadata.store_id,
            binding,
            &receipt.result_digest,
            &receipt.backup_digest,
        )?;
        Ok(Some(ImportCommit {
            result,
            backup_metadata,
            idempotent_replay: true,
        }))
    }

    fn commit_import_batch(
        &mut self,
        manifest: crate::transaction::TransactionManifest,
        records: Vec<(ImportRecordIntent, Vec<u8>)>,
        tombstones: Vec<(ImportTombstoneIntent, Vec<u8>)>,
        artifacts: ImportArtifacts,
    ) -> Result<ImportCommit, StoreError> {
        let crate::transaction::TransactionIntent::Import(intent) = &manifest.intent else {
            return Err(StoreError::InvalidTransaction);
        };
        let target_metadata = intent.target_store_metadata.clone();
        self.poisoned = true;
        crate::transaction::persist_manifest(&self.root, &manifest, &self.failpoints)?;
        prepare_import_namespaces(&self.root, intent, &self.failpoints)?;
        let record_identities =
            stage_import_records(&self.root, &manifest, &records, &self.failpoints)?;
        let tombstone_identities =
            stage_import_tombstones(&self.root, &manifest, &tombstones, &self.failpoints)?;
        let backup_identity = stage_import_artifact(
            &self.root,
            &temp_sibling(
                &import_backup_relative(&manifest.transaction_id)?,
                "backup",
                &manifest.transaction_id,
            )?,
            &artifacts.backup_bytes,
            crate::transaction::FilePersistence {
                write_operation: "write staged import backup metadata",
                sync_operation: "sync staged import backup metadata",
                directory_operation: "sync staged import backup metadata directory",
                written: crate::PersistenceBoundary::BackupMetadataTempWritten,
                synced: crate::PersistenceBoundary::BackupMetadataTempSynced,
                directory_synced: crate::PersistenceBoundary::BackupMetadataTempDirectorySynced,
            },
            &self.failpoints,
        )?;
        let result_identity = stage_import_artifact(
            &self.root,
            &temp_sibling(
                &import_result_relative(&intent.binding.receipt_id)?,
                "result",
                &manifest.transaction_id,
            )?,
            &artifacts.result_bytes,
            mutation_result_persistence(),
            &self.failpoints,
        )?;
        let receipt_identity = stage_import_artifact(
            &self.root,
            &temp_sibling(
                &import_receipt_relative(&intent.binding)?,
                "receipt",
                &manifest.transaction_id,
            )?,
            &artifacts.receipt_bytes,
            mutation_receipt_persistence(),
            &self.failpoints,
        )?;
        let audit_identity = stage_import_artifact(
            &self.root,
            &temp_sibling(
                &import_audit_relative(intent.binding.audit_sequence)?,
                "audit",
                &manifest.transaction_id,
            )?,
            &artifacts.audit_bytes,
            mutation_audit_persistence(),
            &self.failpoints,
        )?;
        let metadata_identity =
            crate::transaction::stage_metadata(&self.root, &manifest, &self.failpoints)?;

        publish_import_records(&self.root, &manifest, &record_identities, &self.failpoints)?;
        publish_import_tombstones(
            &self.root,
            &manifest,
            &tombstone_identities,
            &self.failpoints,
        )?;
        publish_import_artifact(
            &self.root,
            &temp_sibling(
                &import_backup_relative(&manifest.transaction_id)?,
                "backup",
                &manifest.transaction_id,
            )?,
            &import_backup_relative(&manifest.transaction_id)?,
            backup_identity,
            crate::transaction::ArtifactPublication {
                published_boundary: crate::PersistenceBoundary::BackupMetadataPublished,
                synced_boundary: crate::PersistenceBoundary::BackupMetadataDirectorySynced,
                sync_operation: "sync committed import backup metadata",
            },
            &self.failpoints,
        )?;
        publish_import_result_artifacts(
            &self.root,
            &manifest,
            result_identity,
            receipt_identity,
            audit_identity,
            &self.failpoints,
        )?;
        crate::transaction::publish_metadata(
            &self.root,
            &manifest,
            metadata_identity,
            &self.failpoints,
        )?;
        self.metadata = target_metadata;
        crate::transaction::remove_manifest(&self.root, &manifest, &self.failpoints)?;
        self.poisoned = false;
        Ok(ImportCommit {
            result: artifacts.result,
            backup_metadata: artifacts.backup,
            idempotent_replay: false,
        })
    }

    /// Read strict recovery-safe backup metadata that was atomically committed
    /// by the import WAL. The capability cannot create a standalone artifact;
    /// startup ledger validation proves the metadata remains receipt/audit
    /// exact-set bound before it is returned to a future host adapter.
    pub fn read_backup_metadata(
        &self,
        _authorization: &AuthorizedBackupMetadata,
        transaction_id: &str,
    ) -> Result<BackupMetadata, StoreError> {
        self.validate_ownership()?;
        if !crate::transaction::valid_transaction_id(transaction_id) {
            return Err(StoreError::InvalidRequest);
        }
        crate::idempotency::validate_ledger(&self.root, &self.metadata)?;
        let relative = import_backup_relative(transaction_id)?;
        let file = self
            .root
            .try_open_regular(&relative, false)?
            .ok_or(StoreError::NotFound)?;
        validate_import_artifact_file(&file)?;
        let identity = crate::layout::FileIdentity::from_file(&file)?;
        let backup = crate::idempotency::decode_canonical(
            file,
            MAX_BACKUP_METADATA_BYTES,
            BackupMetadata::validate,
        )?;
        if backup.store_id != self.metadata.store_id
            || backup.transaction_id != transaction_id
            || backup.target_snapshot.store_revision.0 > self.metadata.store_revision.0
            || backup.target_snapshot.audit_sequence.0 > self.metadata.audit_sequence.0
        {
            return Err(StoreError::InvalidTransaction);
        }
        crate::idempotency::validate_ledger(&self.root, &self.metadata)?;
        self.validate_ownership()?;
        if !import_file_identity_matches(&self.root, &relative, identity)? {
            return Err(StoreError::UnsafePath);
        }
        Ok(backup)
    }
}

impl crate::AuthorizedScopes {
    /// Authenticate an independently grantable host capability that can read
    /// WAL-persisted backup metadata but cannot export, import, mutate, or
    /// purge.
    pub fn authorize_backup_metadata(
        &self,
        context: &TrustedRequestContext,
    ) -> Result<AuthorizedBackupMetadata, StoreError> {
        context
            .validate()
            .map_err(|_| StoreError::Unauthenticated)?;
        if context.principal_id != self.principal_id
            || !context
                .grants
                .iter()
                .any(|grant| grant.as_str() == "memory:admin:backup_metadata")
        {
            return Err(StoreError::Forbidden);
        }
        Ok(AuthorizedBackupMetadata {
            principal_id: context.principal_id.clone(),
        })
    }
}

fn classify_target(
    store: &CanonicalStore,
    id: &MemoryId,
    authorized: bool,
    over_limit: bool,
    _kind: ImportItemKind,
) -> Result<ImportClassification, StoreError> {
    if !authorized {
        return Ok(ImportClassification::Unauthorized);
    }
    if over_limit {
        return Ok(ImportClassification::Invalid);
    }
    if crate::tombstone::id_exists_anywhere(&store.root, id)? {
        return Ok(ImportClassification::TombstoneProtected);
    }
    if crate::mutation::record_id_exists_anywhere(&store.root, id)? {
        return Ok(ImportClassification::Conflicting);
    }
    Ok(ImportClassification::Accepted)
}

fn require_import_authority(
    authority: &crate::AuthorizedScopes,
    context: &TrustedRequestContext,
    scopes: &[MemoryScope],
) -> Result<(), StoreError> {
    if scopes.is_empty() || scopes.len() > MAX_IMPORT_SCOPES {
        return Err(StoreError::InvalidRequest);
    }
    for scope in scopes {
        if !import_scope_authorized(authority, context, scope) {
            return Err(StoreError::Forbidden);
        }
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequestFingerprint<'a> {
    scopes: &'a [MemoryScope],
    source_store_id: &'a StoreId,
    source_snapshot: SnapshotWatermark,
    bundle_digest: &'a str,
    expected_plan_digest: &'a ImportDigest,
}

fn import_request_fingerprint(
    bundle: &PortableExportBundle,
    expected_plan_digest: &ImportDigest,
) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(&ImportRequestFingerprint {
        scopes: &bundle.scopes,
        source_store_id: &bundle.source_store_id,
        source_snapshot: bundle.snapshot,
        bundle_digest: bundle.digest.as_str(),
        expected_plan_digest,
    })
    .map_err(|_| StoreError::InvalidRequest)?;
    Ok(
        ImportDigest::from_payload(b"jiandu/canonical-import-input/v1\0", &bytes)
            .as_str()
            .to_owned(),
    )
}

fn prepare_import_namespaces(
    root: &crate::layout::StoreDirectory,
    transaction: &ImportTransaction,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let paths = [
        import_result_relative(&transaction.binding.receipt_id)?,
        import_receipt_relative(&transaction.binding)?,
        import_audit_relative(transaction.binding.audit_sequence)?,
        import_backup_relative(&transaction.binding.transaction_id)?,
    ];
    for relative in paths {
        let parent = relative.parent().ok_or(StoreError::InvalidTransaction)?;
        root.create_directory_all(parent)?;
        crate::transaction::sync_directory_chain(
            root,
            parent,
            "sync prepared import artifact namespace",
        )?;
    }
    failpoints.check(crate::PersistenceBoundary::IdempotencyNamespacePrepared)
}

fn import_record_relative(intent: &ImportRecordIntent) -> PathBuf {
    crate::layout::record_relative_path(&intent.scope, &intent.memory_id)
}

fn import_record_temp_relative(
    intent: &ImportRecordIntent,
    transaction_id: &str,
) -> Result<PathBuf, StoreError> {
    if !crate::transaction::valid_transaction_id(transaction_id) {
        return Err(StoreError::InvalidTransaction);
    }
    let target = import_record_relative(intent);
    Ok(target
        .parent()
        .ok_or(StoreError::InvalidTransaction)?
        .join(format!(
            ".import-record-{}-{transaction_id}.tmp",
            crate::layout::record_storage_key(&intent.memory_id)
        )))
}

fn import_tombstone_relative(intent: &ImportTombstoneIntent) -> PathBuf {
    crate::layout::tombstone_relative_path(&intent.scope, &intent.memory_id)
}

fn import_tombstone_temp_relative(
    intent: &ImportTombstoneIntent,
    transaction_id: &str,
) -> Result<PathBuf, StoreError> {
    if !crate::transaction::valid_transaction_id(transaction_id) {
        return Err(StoreError::InvalidTransaction);
    }
    let target = import_tombstone_relative(intent);
    Ok(target
        .parent()
        .ok_or(StoreError::InvalidTransaction)?
        .join(format!(
            ".import-tombstone-{}-{transaction_id}.tmp",
            crate::layout::record_storage_key(&intent.memory_id)
        )))
}

fn stage_import_records(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    records: &[(ImportRecordIntent, Vec<u8>)],
    failpoints: &crate::failpoint::Failpoints,
) -> Result<Vec<crate::layout::FileIdentity>, StoreError> {
    let mut identities = Vec::with_capacity(records.len());
    for (intent, bytes) in records {
        if crate::idempotency::content_digest(bytes) != intent.content_digest {
            return Err(StoreError::InvalidTransaction);
        }
        let staged = import_record_temp_relative(intent, &manifest.transaction_id)?;
        let parent = staged.parent().ok_or(StoreError::InvalidTransaction)?;
        root.create_directory_all(parent)?;
        crate::transaction::sync_directory_chain(root, parent, "sync import record namespace")?;
        failpoints.check(crate::PersistenceBoundary::RecordNamespacePrepared)?;
        crate::transaction::write_new_file(
            root,
            &staged,
            bytes,
            crate::transaction::FilePersistence {
                write_operation: "write staged imported record",
                sync_operation: "sync staged imported record",
                directory_operation: "sync staged imported record directory",
                written: crate::PersistenceBoundary::RecordTempWritten,
                synced: crate::PersistenceBoundary::RecordTempSynced,
                directory_synced: crate::PersistenceBoundary::RecordTempDirectorySynced,
            },
            failpoints,
        )?;
        identities.push(crate::layout::FileIdentity::from_file(
            &root.open_existing_regular(&staged, false)?,
        )?);
    }
    Ok(identities)
}

fn stage_import_tombstones(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    tombstones: &[(ImportTombstoneIntent, Vec<u8>)],
    failpoints: &crate::failpoint::Failpoints,
) -> Result<Vec<crate::layout::FileIdentity>, StoreError> {
    let mut identities = Vec::with_capacity(tombstones.len());
    for (intent, bytes) in tombstones {
        if crate::idempotency::content_digest(bytes) != intent.content_digest {
            return Err(StoreError::InvalidTransaction);
        }
        let staged = import_tombstone_temp_relative(intent, &manifest.transaction_id)?;
        let parent = staged.parent().ok_or(StoreError::InvalidTransaction)?;
        root.create_directory_all(parent)?;
        crate::transaction::sync_directory_chain(root, parent, "sync import tombstone namespace")?;
        failpoints.check(crate::PersistenceBoundary::TombstoneNamespacePrepared)?;
        crate::transaction::write_new_file(
            root,
            &staged,
            bytes,
            crate::transaction::FilePersistence {
                write_operation: "write staged imported tombstone",
                sync_operation: "sync staged imported tombstone",
                directory_operation: "sync staged imported tombstone directory",
                written: crate::PersistenceBoundary::TombstoneTempWritten,
                synced: crate::PersistenceBoundary::TombstoneTempSynced,
                directory_synced: crate::PersistenceBoundary::TombstoneTempDirectorySynced,
            },
            failpoints,
        )?;
        identities.push(crate::layout::FileIdentity::from_file(
            &root.open_existing_regular(&staged, false)?,
        )?);
    }
    Ok(identities)
}

fn stage_import_artifact(
    root: &crate::layout::StoreDirectory,
    staged: &Path,
    bytes: &[u8],
    persistence: crate::transaction::FilePersistence,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<crate::layout::FileIdentity, StoreError> {
    crate::transaction::write_new_file(root, staged, bytes, persistence, failpoints)?;
    crate::layout::FileIdentity::from_file(&root.open_existing_regular(staged, false)?)
}

fn publish_import_records(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    identities: &[crate::layout::FileIdentity],
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let crate::transaction::TransactionIntent::Import(transaction) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    if identities.len() != transaction.records.len() {
        return Err(StoreError::InvalidTransaction);
    }
    for (intent, identity) in transaction.records.iter().zip(identities) {
        if crate::tombstone::id_exists_anywhere(root, &intent.memory_id)?
            || crate::mutation::record_id_exists_anywhere(root, &intent.memory_id)?
        {
            return Err(StoreError::InvalidTransaction);
        }
        crate::transaction::publish_new_artifact(
            root,
            &import_record_temp_relative(intent, &manifest.transaction_id)?,
            &import_record_relative(intent),
            *identity,
            crate::transaction::ArtifactPublication {
                published_boundary: crate::PersistenceBoundary::RecordRenamed,
                synced_boundary: crate::PersistenceBoundary::RecordDirectorySynced,
                sync_operation: "sync imported canonical record",
            },
            failpoints,
        )?;
    }
    Ok(())
}

fn publish_import_tombstones(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    identities: &[crate::layout::FileIdentity],
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let crate::transaction::TransactionIntent::Import(transaction) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    if identities.len() != transaction.tombstones.len() {
        return Err(StoreError::InvalidTransaction);
    }
    for (intent, identity) in transaction.tombstones.iter().zip(identities) {
        if crate::tombstone::id_exists_anywhere(root, &intent.memory_id)?
            || crate::mutation::record_id_exists_anywhere(root, &intent.memory_id)?
        {
            return Err(StoreError::InvalidTransaction);
        }
        crate::transaction::publish_new_artifact(
            root,
            &import_tombstone_temp_relative(intent, &manifest.transaction_id)?,
            &import_tombstone_relative(intent),
            *identity,
            crate::transaction::ArtifactPublication {
                published_boundary: crate::PersistenceBoundary::TombstonePublished,
                synced_boundary: crate::PersistenceBoundary::TombstoneDirectorySynced,
                sync_operation: "sync imported protected tombstone",
            },
            failpoints,
        )?;
    }
    Ok(())
}

fn publish_import_artifact(
    root: &crate::layout::StoreDirectory,
    staged: &Path,
    target: &Path,
    identity: crate::layout::FileIdentity,
    publication: crate::transaction::ArtifactPublication,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    crate::transaction::publish_new_artifact(
        root,
        staged,
        target,
        identity,
        publication,
        failpoints,
    )
}

fn publish_import_result_artifacts(
    root: &crate::layout::StoreDirectory,
    manifest: &crate::transaction::TransactionManifest,
    result_identity: crate::layout::FileIdentity,
    receipt_identity: crate::layout::FileIdentity,
    audit_identity: crate::layout::FileIdentity,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let crate::transaction::TransactionIntent::Import(transaction) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let result = import_result_relative(&transaction.binding.receipt_id)?;
    publish_import_artifact(
        root,
        &temp_sibling(&result, "result", &manifest.transaction_id)?,
        &result,
        result_identity,
        crate::transaction::ArtifactPublication {
            published_boundary: crate::PersistenceBoundary::MutationResultPublished,
            synced_boundary: crate::PersistenceBoundary::MutationResultDirectorySynced,
            sync_operation: "sync committed import result",
        },
        failpoints,
    )?;
    let receipt = import_receipt_relative(&transaction.binding)?;
    publish_import_artifact(
        root,
        &temp_sibling(&receipt, "receipt", &manifest.transaction_id)?,
        &receipt,
        receipt_identity,
        crate::transaction::ArtifactPublication {
            published_boundary: crate::PersistenceBoundary::MutationReceiptPublished,
            synced_boundary: crate::PersistenceBoundary::MutationReceiptDirectorySynced,
            sync_operation: "sync committed import receipt",
        },
        failpoints,
    )?;
    let audit = import_audit_relative(transaction.binding.audit_sequence)?;
    publish_import_artifact(
        root,
        &temp_sibling(&audit, "audit", &manifest.transaction_id)?,
        &audit,
        audit_identity,
        crate::transaction::ArtifactPublication {
            published_boundary: crate::PersistenceBoundary::MutationAuditPublished,
            synced_boundary: crate::PersistenceBoundary::MutationAuditDirectorySynced,
            sync_operation: "sync committed import audit",
        },
        failpoints,
    )
}

fn mutation_result_persistence() -> crate::transaction::FilePersistence {
    crate::transaction::FilePersistence {
        write_operation: "write staged import result",
        sync_operation: "sync staged import result",
        directory_operation: "sync staged import result directory",
        written: crate::PersistenceBoundary::MutationResultTempWritten,
        synced: crate::PersistenceBoundary::MutationResultTempSynced,
        directory_synced: crate::PersistenceBoundary::MutationResultTempDirectorySynced,
    }
}

fn mutation_receipt_persistence() -> crate::transaction::FilePersistence {
    crate::transaction::FilePersistence {
        write_operation: "write staged import receipt",
        sync_operation: "sync staged import receipt",
        directory_operation: "sync staged import receipt directory",
        written: crate::PersistenceBoundary::MutationReceiptTempWritten,
        synced: crate::PersistenceBoundary::MutationReceiptTempSynced,
        directory_synced: crate::PersistenceBoundary::MutationReceiptTempDirectorySynced,
    }
}

fn mutation_audit_persistence() -> crate::transaction::FilePersistence {
    crate::transaction::FilePersistence {
        write_operation: "write staged import audit",
        sync_operation: "sync staged import audit",
        directory_operation: "sync staged import audit directory",
        written: crate::PersistenceBoundary::MutationAuditTempWritten,
        synced: crate::PersistenceBoundary::MutationAuditTempSynced,
        directory_synced: crate::PersistenceBoundary::MutationAuditTempDirectorySynced,
    }
}

fn import_scope_authorized(
    authority: &crate::AuthorizedScopes,
    context: &TrustedRequestContext,
    scope: &MemoryScope,
) -> bool {
    authority.authorize_exact(scope).is_some()
        && context
            .grants
            .iter()
            .any(|grant| grant.as_str() == import_grant(scope))
}

fn import_grant(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Principal { .. } => "memory:import:principal",
        MemoryScope::Project { .. } => "memory:import:project",
        MemoryScope::Session { .. } => "memory:import:session",
        MemoryScope::InstanceGlobal {} => "memory:import:instance_global",
    }
}

fn scope_key(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Principal { principal_id } => format!("0:{}", principal_id.as_str()),
        MemoryScope::Project { project_id } => format!("1:{}", project_id.as_str()),
        MemoryScope::Session { session_id } => format!("2:{}", session_id.as_str()),
        MemoryScope::InstanceGlobal {} => "3:".to_owned(),
    }
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest_text(domain: &[u8], value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(value.as_bytes());
    format!("sha256:{}", hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn canonical_json(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| StoreError::InvalidRequest)?;
    bytes.push(b'\n');
    if bytes.len() > maximum {
        return Err(StoreError::InvalidRequest);
    }
    Ok(bytes)
}

fn decode_canonical<T>(
    bytes: &[u8],
    maximum: usize,
    validate: impl FnOnce(&T) -> Result<(), StoreError>,
) -> Result<T, StoreError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
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

/// Generate authoritative import plan, commit-result, and backup-metadata schemas.
pub fn generated_import_schemas() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "import-dry-run-plan.schema.json",
            serde_json::to_value(schema_for!(ImportDryRunPlan))
                .expect("import plan schema serializes"),
        ),
        (
            "portable-import-result.schema.json",
            serde_json::to_value(schema_for!(PortableImportResult))
                .expect("portable import result schema serializes"),
        ),
        (
            "backup-metadata.schema.json",
            serde_json::to_value(schema_for!(BackupMetadata))
                .expect("backup metadata schema serializes"),
        ),
    ]
}
