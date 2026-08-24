//! Versioned, body-free transaction intents for canonical store recovery.

use crate::failpoint::Failpoints;
use crate::idempotency::IdempotencyTransaction;
use crate::layout::{self, FileIdentity, StoreDirectory};
use crate::{PersistenceBoundary, StoreError, StoreId, StoreMetadata};
use jiandu_core::{Etag, MemoryId, MemoryScope, Revision};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) const TRANSACTION_FORMAT_VERSION: &str = "jiandu.store.transaction/v1alpha2";
pub(crate) const LEGACY_TRANSACTION_FORMAT_VERSION: &str = "jiandu.store.transaction/v1alpha1";
pub(crate) const QUARANTINE_RECEIPT_FORMAT_VERSION: &str =
    "jiandu.store.quarantine-receipt/v1alpha1";
const MAX_TRANSACTION_BYTES: usize = 65_536;
const SHA256_ETAG_LENGTH: usize = 71;
const SHA256_DIGEST_LENGTH: usize = 71;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordOperation {
    Create,
    Update,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecordTransaction {
    pub(crate) operation: RecordOperation,
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_etag: Option<Etag>,
    pub(crate) target_revision: Revision,
    pub(crate) target_etag: Etag,
    pub(crate) base_store_metadata: StoreMetadata,
    pub(crate) target_store_metadata: StoreMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idempotency: Option<IdempotencyTransaction>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QuarantineTransaction {
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    pub(crate) quarantine_token: String,
    pub(crate) source_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TransactionIntent {
    Record(Box<RecordTransaction>),
    Quarantine(QuarantineTransaction),
}

/// Immutable write-ahead intent. It contains opaque IDs and hashes, but never
/// a record body, canonical/ambient path, credential, or model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TransactionManifest {
    pub(crate) format_version: String,
    pub(crate) transaction_id: String,
    pub(crate) store_id: StoreId,
    pub(crate) intent: TransactionIntent,
}

impl TransactionManifest {
    pub(crate) fn for_record(
        store_id: StoreId,
        transaction_id: String,
        transaction: RecordTransaction,
    ) -> Result<Self, StoreError> {
        let manifest = Self {
            format_version: TRANSACTION_FORMAT_VERSION.to_owned(),
            transaction_id,
            store_id,
            intent: TransactionIntent::Record(Box::new(transaction)),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn for_quarantine(
        store_id: StoreId,
        transaction: QuarantineTransaction,
    ) -> Result<Self, StoreError> {
        let manifest = Self {
            format_version: TRANSACTION_FORMAT_VERSION.to_owned(),
            transaction_id: new_transaction_id(),
            store_id,
            intent: TransactionIntent::Quarantine(transaction),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self)
    }

    pub(crate) fn decode(
        file: File,
        expected_transaction_id: &str,
        expected_store_id: &StoreId,
    ) -> Result<Self, StoreError> {
        let bytes = read_bounded(file)?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidTransaction)?;
        manifest.validate()?;
        if manifest.transaction_id != expected_transaction_id
            || &manifest.store_id != expected_store_id
            || manifest.canonical_bytes()? != bytes
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), StoreError> {
        if !matches!(
            self.format_version.as_str(),
            TRANSACTION_FORMAT_VERSION | LEGACY_TRANSACTION_FORMAT_VERSION
        ) || !valid_transaction_id(&self.transaction_id)
        {
            return Err(StoreError::InvalidTransaction);
        }
        match &self.intent {
            TransactionIntent::Record(record) => {
                record.validate(&self.store_id, &self.format_version, &self.transaction_id)
            }
            TransactionIntent::Quarantine(quarantine) => quarantine.validate(),
        }
    }
}

impl RecordTransaction {
    fn validate(
        &self,
        store_id: &StoreId,
        manifest_format: &str,
        transaction_id: &str,
    ) -> Result<(), StoreError> {
        let valid_base = match self.operation {
            RecordOperation::Create => {
                self.base_revision.is_none()
                    && self.base_etag.is_none()
                    && self.target_revision.get() == 1
            }
            RecordOperation::Update => match (&self.base_revision, &self.base_etag) {
                (Some(base_revision), Some(base_etag)) => {
                    base_revision
                        .get()
                        .checked_add(1)
                        .is_some_and(|next| next == self.target_revision.get())
                        && valid_content_etag(base_etag.as_str())
                }
                _ => false,
            },
        };
        let next_store_revision = self
            .base_store_metadata
            .store_revision
            .0
            .checked_add(1)
            .ok_or(StoreError::InvalidTransaction)?;
        let is_legacy = manifest_format == LEGACY_TRANSACTION_FORMAT_VERSION;
        let expected_store_format = if is_legacy {
            crate::metadata::LEGACY_STORE_FORMAT_VERSION
        } else {
            crate::STORE_FORMAT_VERSION
        };
        let audit_advances = if is_legacy {
            self.base_store_metadata.audit_sequence.0 == 0
                && self.target_store_metadata.audit_sequence.0 == 0
                && self.idempotency.is_none()
        } else {
            self.base_store_metadata
                .audit_sequence
                .0
                .checked_add(1)
                .is_some_and(|next| next == self.target_store_metadata.audit_sequence.0)
                && self.idempotency.as_ref().is_some_and(|idempotency| {
                    idempotency.validate().is_ok()
                        && idempotency.binding.transaction_id == transaction_id
                        && idempotency.binding.operation == self.operation.into()
                        && idempotency.binding.scope == self.scope
                        && idempotency.binding.memory_id == self.memory_id
                        && idempotency.binding.target_revision == self.target_revision
                        && idempotency.binding.target_etag == self.target_etag
                        && idempotency.binding.store_revision
                            == self.target_store_metadata.store_revision
                        && idempotency.binding.audit_sequence
                            == self.target_store_metadata.audit_sequence
                })
        };
        if !valid_base
            || !valid_content_etag(self.target_etag.as_str())
            || &self.base_store_metadata.store_id != store_id
            || &self.target_store_metadata.store_id != store_id
            || self.base_store_metadata.format_version != expected_store_format
            || self.target_store_metadata.format_version != expected_store_format
            || self.base_store_metadata.created_at != self.target_store_metadata.created_at
            || self.target_store_metadata.store_revision.0 != next_store_revision
            || !audit_advances
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

impl From<RecordOperation> for crate::MutationOperation {
    fn from(operation: RecordOperation) -> Self {
        match operation {
            RecordOperation::Create => Self::Create,
            RecordOperation::Update => Self::Update,
        }
    }
}

impl QuarantineTransaction {
    fn validate(&self) -> Result<(), StoreError> {
        if !valid_quarantine_token(&self.quarantine_token)
            || !valid_content_digest(&self.source_digest)
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableQuarantineReceipt {
    pub(crate) format_version: String,
    pub(crate) transaction_id: String,
    pub(crate) store_id: StoreId,
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    pub(crate) quarantine_token: String,
    pub(crate) source_digest: String,
}

impl DurableQuarantineReceipt {
    pub(crate) fn from_manifest(manifest: &TransactionManifest) -> Result<Self, StoreError> {
        let TransactionIntent::Quarantine(quarantine) = &manifest.intent else {
            return Err(StoreError::InvalidTransaction);
        };
        let receipt = Self {
            format_version: QUARANTINE_RECEIPT_FORMAT_VERSION.to_owned(),
            transaction_id: manifest.transaction_id.clone(),
            store_id: manifest.store_id.clone(),
            memory_id: quarantine.memory_id.clone(),
            scope: quarantine.scope.clone(),
            quarantine_token: quarantine.quarantine_token.clone(),
            source_digest: quarantine.source_digest.clone(),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self)
    }

    pub(crate) fn decode(
        file: File,
        expected_transaction_id: &str,
        expected_store_id: &StoreId,
    ) -> Result<Self, StoreError> {
        let bytes = read_bounded(file)?;
        let receipt: Self =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidTransaction)?;
        receipt.validate()?;
        if receipt.transaction_id != expected_transaction_id
            || &receipt.store_id != expected_store_id
            || receipt.canonical_bytes()? != bytes
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(receipt)
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.format_version != QUARANTINE_RECEIPT_FORMAT_VERSION
            || !valid_transaction_id(&self.transaction_id)
            || !valid_quarantine_token(&self.quarantine_token)
            || !valid_content_digest(&self.source_digest)
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

pub(crate) fn new_transaction_id() -> String {
    Uuid::new_v4().hyphenated().to_string()
}

pub(crate) fn valid_transaction_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|uuid| uuid.hyphenated().to_string() == value)
}

pub(crate) fn valid_quarantine_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_content_etag(value: &str) -> bool {
    value.len() == SHA256_ETAG_LENGTH && valid_content_digest(value)
}

pub(crate) fn valid_content_digest(value: &str) -> bool {
    value.len() == SHA256_DIGEST_LENGTH
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| StoreError::InvalidTransaction)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_TRANSACTION_BYTES {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(bytes)
}

fn read_bounded(file: File) -> Result<Vec<u8>, StoreError> {
    if file
        .metadata()
        .map_err(|source| StoreError::io("inspect transaction file", source))?
        .len()
        > MAX_TRANSACTION_BYTES as u64
    {
        return Err(StoreError::InvalidTransaction);
    }
    let mut bytes = Vec::new();
    file.take((MAX_TRANSACTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read transaction file", source))?;
    Ok(bytes)
}

pub(crate) fn manifest_relative(transaction_id: &str) -> Result<PathBuf, StoreError> {
    validate_transaction_id(transaction_id)?;
    Ok(PathBuf::from("transactions").join(format!("{transaction_id}.json")))
}

pub(crate) fn manifest_temp_relative(transaction_id: &str) -> Result<PathBuf, StoreError> {
    validate_transaction_id(transaction_id)?;
    Ok(PathBuf::from("transactions").join(format!(".manifest-{transaction_id}.tmp")))
}

pub(crate) fn record_relative(manifest: &TransactionManifest) -> Result<PathBuf, StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    Ok(layout::record_relative_path(
        &record.scope,
        &record.memory_id,
    ))
}

pub(crate) fn record_temp_relative(manifest: &TransactionManifest) -> Result<PathBuf, StoreError> {
    let target = record_relative(manifest)?;
    let parent = target.parent().ok_or(StoreError::InvalidTransaction)?;
    Ok(parent.join(format!(".record-{}.tmp", manifest.transaction_id)))
}

pub(crate) fn metadata_temp_relative(
    manifest: &TransactionManifest,
) -> Result<PathBuf, StoreError> {
    validate_transaction_id(&manifest.transaction_id)?;
    Ok(PathBuf::from(format!(
        ".store-{}.tmp",
        manifest.transaction_id
    )))
}

pub(crate) fn quarantine_relative(
    quarantine: &QuarantineTransaction,
) -> Result<PathBuf, StoreError> {
    quarantine.validate()?;
    Ok(PathBuf::from(layout::QUARANTINE_DIR).join(format!(
        "{}.{}.md",
        layout::record_storage_key(&quarantine.memory_id),
        quarantine.quarantine_token
    )))
}

pub(crate) fn receipt_relative(transaction_id: &str) -> Result<PathBuf, StoreError> {
    validate_transaction_id(transaction_id)?;
    Ok(PathBuf::from(layout::QUARANTINE_RECEIPTS_DIR)
        .join(format!("quarantine-{transaction_id}.json")))
}

pub(crate) fn receipt_temp_relative(transaction_id: &str) -> Result<PathBuf, StoreError> {
    validate_transaction_id(transaction_id)?;
    Ok(PathBuf::from(layout::QUARANTINE_RECEIPTS_DIR)
        .join(format!(".quarantine-{transaction_id}.tmp")))
}

pub(crate) fn persist_manifest(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let bytes = manifest.canonical_bytes()?;
    let staged = manifest_temp_relative(&manifest.transaction_id)?;
    let published = manifest_relative(&manifest.transaction_id)?;
    write_new_file(
        root,
        &staged,
        &bytes,
        FilePersistence {
            write_operation: "write transaction manifest",
            sync_operation: "sync transaction manifest",
            directory_operation: "sync staged transaction directory",
            written: PersistenceBoundary::ManifestTempWritten,
            synced: PersistenceBoundary::ManifestTempSynced,
            directory_synced: PersistenceBoundary::ManifestTempDirectorySynced,
        },
        failpoints,
    )?;
    if root.regular_file_exists(&published)? {
        return Err(StoreError::InvalidTransaction);
    }
    root.rename(&staged, &published)?;
    failpoints.check(PersistenceBoundary::ManifestPublished)?;
    root.sync_directory(
        Path::new("transactions"),
        "sync published transaction manifest",
    )?;
    failpoints.check(PersistenceBoundary::ManifestDirectorySynced)
}

pub(crate) fn stage_record(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    bytes: &[u8],
    failpoints: &Failpoints,
) -> Result<FileIdentity, StoreError> {
    let staged = record_temp_relative(manifest)?;
    let parent = staged.parent().ok_or(StoreError::InvalidTransaction)?;
    root.create_directory_all(parent)?;
    sync_directory_chain(root, parent, "sync prepared record namespace")?;
    failpoints.check(PersistenceBoundary::RecordNamespacePrepared)?;
    write_new_file(
        root,
        &staged,
        bytes,
        FilePersistence {
            write_operation: "write staged memory record",
            sync_operation: "sync staged memory record",
            directory_operation: "sync staged record directory",
            written: PersistenceBoundary::RecordTempWritten,
            synced: PersistenceBoundary::RecordTempSynced,
            directory_synced: PersistenceBoundary::RecordTempDirectorySynced,
        },
        failpoints,
    )?;
    FileIdentity::from_file(&root.open_existing_regular(&staged, false)?)
}

pub(crate) fn stage_metadata(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<FileIdentity, StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let staged = metadata_temp_relative(manifest)?;
    write_new_file(
        root,
        &staged,
        &record.target_store_metadata.canonical_bytes()?,
        FilePersistence {
            write_operation: "write staged store metadata",
            sync_operation: "sync staged store metadata",
            directory_operation: "sync staged metadata directory",
            written: PersistenceBoundary::MetadataTempWritten,
            synced: PersistenceBoundary::MetadataTempSynced,
            directory_synced: PersistenceBoundary::MetadataTempDirectorySynced,
        },
        failpoints,
    )?;
    FileIdentity::from_file(&root.open_existing_regular(&staged, false)?)
}

pub(crate) fn prepare_idempotency_namespaces(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let idempotency = record
        .idempotency
        .as_ref()
        .ok_or(StoreError::InvalidTransaction)?;
    for relative in [
        crate::idempotency::result_relative(&idempotency.binding.receipt_id)?,
        crate::idempotency::receipt_relative_for_binding(&idempotency.binding)?,
        crate::idempotency::audit_relative(idempotency.binding.audit_sequence)?,
    ] {
        let parent = relative.parent().ok_or(StoreError::InvalidTransaction)?;
        root.create_directory_all(parent)?;
        sync_directory_chain(root, parent, "sync mutation artifact namespace")?;
    }
    failpoints.check(PersistenceBoundary::IdempotencyNamespacePrepared)
}

pub(crate) fn stage_mutation_result(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    bytes: &[u8],
    failpoints: &Failpoints,
) -> Result<FileIdentity, StoreError> {
    let binding = record_idempotency(manifest)?.binding.clone();
    let staged = crate::idempotency::result_temp_relative(&binding)?;
    write_new_file(
        root,
        &staged,
        bytes,
        FilePersistence {
            write_operation: "write staged mutation result",
            sync_operation: "sync staged mutation result",
            directory_operation: "sync staged mutation result directory",
            written: PersistenceBoundary::MutationResultTempWritten,
            synced: PersistenceBoundary::MutationResultTempSynced,
            directory_synced: PersistenceBoundary::MutationResultTempDirectorySynced,
        },
        failpoints,
    )?;
    FileIdentity::from_file(&root.open_existing_regular(&staged, false)?)
}

pub(crate) fn stage_idempotency_receipt(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    bytes: &[u8],
    failpoints: &Failpoints,
) -> Result<FileIdentity, StoreError> {
    let binding = record_idempotency(manifest)?.binding.clone();
    let staged = crate::idempotency::receipt_temp_relative(&binding)?;
    write_new_file(
        root,
        &staged,
        bytes,
        FilePersistence {
            write_operation: "write staged idempotency receipt",
            sync_operation: "sync staged idempotency receipt",
            directory_operation: "sync staged idempotency receipt directory",
            written: PersistenceBoundary::MutationReceiptTempWritten,
            synced: PersistenceBoundary::MutationReceiptTempSynced,
            directory_synced: PersistenceBoundary::MutationReceiptTempDirectorySynced,
        },
        failpoints,
    )?;
    FileIdentity::from_file(&root.open_existing_regular(&staged, false)?)
}

pub(crate) fn stage_mutation_audit(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    bytes: &[u8],
    failpoints: &Failpoints,
) -> Result<FileIdentity, StoreError> {
    let binding = record_idempotency(manifest)?.binding.clone();
    let staged = crate::idempotency::audit_temp_relative(&binding)?;
    write_new_file(
        root,
        &staged,
        bytes,
        FilePersistence {
            write_operation: "write staged mutation audit event",
            sync_operation: "sync staged mutation audit event",
            directory_operation: "sync staged mutation audit directory",
            written: PersistenceBoundary::MutationAuditTempWritten,
            synced: PersistenceBoundary::MutationAuditTempSynced,
            directory_synced: PersistenceBoundary::MutationAuditTempDirectorySynced,
        },
        failpoints,
    )?;
    FileIdentity::from_file(&root.open_existing_regular(&staged, false)?)
}

pub(crate) fn publish_record(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    staged_identity: FileIdentity,
    expected_current: Option<FileIdentity>,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let staged = record_temp_relative(manifest)?;
    let target = record_relative(manifest)?;
    match expected_current {
        Some(expected) if !root.file_identity_matches(&target, expected)? => {
            return Err(StoreError::UnsafePath);
        }
        None if root.regular_file_exists(&target)? => {
            let TransactionIntent::Record(record) = &manifest.intent else {
                return Err(StoreError::InvalidTransaction);
            };
            return Err(StoreError::AlreadyExists {
                id: record.memory_id.clone(),
            });
        }
        _ => {}
    }
    root.rename(&staged, &target)?;
    if !root.file_identity_matches(&target, staged_identity)? {
        return Err(StoreError::UnsafePath);
    }
    failpoints.check(PersistenceBoundary::RecordRenamed)?;
    root.sync_directory(
        target.parent().ok_or(StoreError::InvalidTransaction)?,
        "sync canonical record directory",
    )?;
    failpoints.check(PersistenceBoundary::RecordDirectorySynced)
}

pub(crate) fn publish_mutation_result(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    staged_identity: FileIdentity,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let binding = &record_idempotency(manifest)?.binding;
    publish_new_artifact(
        root,
        &crate::idempotency::result_temp_relative(binding)?,
        &crate::idempotency::result_relative(&binding.receipt_id)?,
        staged_identity,
        ArtifactPublication {
            published_boundary: PersistenceBoundary::MutationResultPublished,
            synced_boundary: PersistenceBoundary::MutationResultDirectorySynced,
            sync_operation: "sync committed mutation result",
        },
        failpoints,
    )
}

pub(crate) fn publish_idempotency_receipt(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    staged_identity: FileIdentity,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let binding = &record_idempotency(manifest)?.binding;
    publish_new_artifact(
        root,
        &crate::idempotency::receipt_temp_relative(binding)?,
        &crate::idempotency::receipt_relative_for_binding(binding)?,
        staged_identity,
        ArtifactPublication {
            published_boundary: PersistenceBoundary::MutationReceiptPublished,
            synced_boundary: PersistenceBoundary::MutationReceiptDirectorySynced,
            sync_operation: "sync committed idempotency receipt",
        },
        failpoints,
    )
}

pub(crate) fn publish_mutation_audit(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    staged_identity: FileIdentity,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let binding = &record_idempotency(manifest)?.binding;
    publish_new_artifact(
        root,
        &crate::idempotency::audit_temp_relative(binding)?,
        &crate::idempotency::audit_relative(binding.audit_sequence)?,
        staged_identity,
        ArtifactPublication {
            published_boundary: PersistenceBoundary::MutationAuditPublished,
            synced_boundary: PersistenceBoundary::MutationAuditDirectorySynced,
            sync_operation: "sync committed mutation audit event",
        },
        failpoints,
    )
}

pub(crate) fn publish_metadata(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    staged_identity: FileIdentity,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let staged = metadata_temp_relative(manifest)?;
    let target = Path::new(layout::STORE_METADATA_FILE);
    root.rename(&staged, target)?;
    if !root.file_identity_matches(target, staged_identity)? {
        return Err(StoreError::UnsafePath);
    }
    failpoints.check(PersistenceBoundary::MetadataRenamed)?;
    root.sync_root("sync committed store metadata")?;
    failpoints.check(PersistenceBoundary::MetadataDirectorySynced)
}

#[derive(Clone, Copy)]
struct ArtifactPublication {
    published_boundary: PersistenceBoundary,
    synced_boundary: PersistenceBoundary,
    sync_operation: &'static str,
}

fn publish_new_artifact(
    root: &StoreDirectory,
    staged: &Path,
    target: &Path,
    staged_identity: FileIdentity,
    publication: ArtifactPublication,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    if root.regular_file_exists(target)? {
        return Err(StoreError::InvalidTransaction);
    }
    root.rename(staged, target)?;
    if !root.file_identity_matches(target, staged_identity)? {
        return Err(StoreError::UnsafePath);
    }
    failpoints.check(publication.published_boundary)?;
    sync_parent(root, target, publication.sync_operation)?;
    failpoints.check(publication.synced_boundary)
}

fn record_idempotency(
    manifest: &TransactionManifest,
) -> Result<&IdempotencyTransaction, StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    record
        .idempotency
        .as_ref()
        .ok_or(StoreError::InvalidTransaction)
}

pub(crate) fn persist_quarantine_receipt(
    root: &StoreDirectory,
    receipt: &DurableQuarantineReceipt,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let staged = receipt_temp_relative(&receipt.transaction_id)?;
    let published = receipt_relative(&receipt.transaction_id)?;
    if let Some(file) = root.try_open_regular(&published, false)? {
        StoreDirectory::validate_private_open_file(&file)?;
        let existing =
            DurableQuarantineReceipt::decode(file, &receipt.transaction_id, &receipt.store_id)?;
        return if existing == *receipt {
            Ok(())
        } else {
            Err(StoreError::InvalidTransaction)
        };
    }
    let _ = root.remove_regular_file_if_exists(&staged)?;
    write_new_file(
        root,
        &staged,
        &receipt.canonical_bytes()?,
        FilePersistence {
            write_operation: "write staged quarantine receipt",
            sync_operation: "sync staged quarantine receipt",
            directory_operation: "sync staged quarantine receipt directory",
            written: PersistenceBoundary::QuarantineReceiptTempWritten,
            synced: PersistenceBoundary::QuarantineReceiptTempSynced,
            directory_synced: PersistenceBoundary::QuarantineReceiptTempDirectorySynced,
        },
        failpoints,
    )?;
    root.rename(&staged, &published)?;
    failpoints.check(PersistenceBoundary::QuarantineReceiptPublished)?;
    root.sync_directory(
        Path::new(layout::QUARANTINE_RECEIPTS_DIR),
        "sync quarantine receipt",
    )?;
    failpoints.check(PersistenceBoundary::QuarantineReceiptDirectorySynced)
}

pub(crate) fn remove_manifest(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let manifest_path = manifest_relative(&manifest.transaction_id)?;
    let _removed = root.remove_regular_file_if_exists(&manifest_path)?;
    failpoints.check(PersistenceBoundary::ManifestRemoved)?;
    root.sync_directory(Path::new("transactions"), "sync transaction cleanup")?;
    failpoints.check(PersistenceBoundary::ManifestRemovalDirectorySynced)
}

pub(crate) fn raw_file_digest(mut file: File) -> Result<String, StoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| StoreError::io("seek store file for hashing", source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16_384];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| StoreError::io("hash store file", source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    // `File::try_clone` may share the underlying cursor with the handle that
    // will subsequently be decoded. Rewind before returning so hashing never
    // makes that authoritative handle appear empty.
    file.seek(SeekFrom::Start(0))
        .map_err(|source| StoreError::io("rewind hashed store file", source))?;
    Ok(format_digest(hasher.finalize().as_slice()))
}

#[derive(Clone, Copy)]
struct FilePersistence {
    write_operation: &'static str,
    sync_operation: &'static str,
    directory_operation: &'static str,
    written: PersistenceBoundary,
    synced: PersistenceBoundary,
    directory_synced: PersistenceBoundary,
}

fn write_new_file(
    root: &StoreDirectory,
    relative: &Path,
    bytes: &[u8],
    persistence: FilePersistence,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let mut file = root
        .create_new_regular(relative)
        .map_err(|error| match error {
            StoreError::InvalidStoreMetadata => StoreError::InvalidTransaction,
            other => other,
        })?;
    StoreDirectory::set_private_file(&file)?;
    file.write_all(bytes)
        .map_err(|source| StoreError::io(persistence.write_operation, source))?;
    failpoints.check(persistence.written)?;
    file.sync_all()
        .map_err(|source| StoreError::io(persistence.sync_operation, source))?;
    failpoints.check(persistence.synced)?;
    sync_parent(root, relative, persistence.directory_operation)?;
    failpoints.check(persistence.directory_synced)
}

pub(crate) fn sync_parent(
    root: &StoreDirectory,
    relative: &Path,
    operation: &'static str,
) -> Result<(), StoreError> {
    let parent = relative.parent().ok_or(StoreError::InvalidTransaction)?;
    if parent.as_os_str().is_empty() {
        root.sync_root(operation)
    } else {
        root.sync_directory(parent, operation)
    }
}

pub(crate) fn sync_directory_chain(
    root: &StoreDirectory,
    directory: &Path,
    operation: &'static str,
) -> Result<(), StoreError> {
    for ancestor in directory.ancestors() {
        if ancestor.as_os_str().is_empty() {
            root.sync_root(operation)?;
        } else {
            root.sync_directory(ancestor, operation)?;
        }
    }
    Ok(())
}

fn validate_transaction_id(value: &str) -> Result<(), StoreError> {
    if valid_transaction_id(value) {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn format_digest(digest: &[u8]) -> String {
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(crate) fn transaction_id_from_manifest_name(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let transaction_id = name.strip_suffix(".json")?;
    valid_transaction_id(transaction_id).then(|| transaction_id.to_owned())
}

pub(crate) fn transaction_id_from_manifest_temp_name(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let transaction_id = name.strip_prefix(".manifest-")?.strip_suffix(".tmp")?;
    valid_transaction_id(transaction_id).then(|| transaction_id.to_owned())
}

pub(crate) fn transaction_id_from_receipt_name(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let transaction_id = name.strip_prefix("quarantine-")?.strip_suffix(".json")?;
    valid_transaction_id(transaction_id).then(|| transaction_id.to_owned())
}

pub(crate) fn transaction_id_from_receipt_temp_name(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let transaction_id = name.strip_prefix(".quarantine-")?.strip_suffix(".tmp")?;
    valid_transaction_id(transaction_id).then(|| transaction_id.to_owned())
}
