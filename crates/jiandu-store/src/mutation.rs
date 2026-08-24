//! Canonical create/update CAS operations.

use crate::document::{decode_canonical_document, encode_canonical_document};
use crate::idempotency::{
    IdempotencyTransaction, MutationArtifacts, MutationBinding, ReceiptIdentity,
};
use crate::layout::{self, FileIdentity};
use crate::transaction::{self, RecordOperation, RecordTransaction, TransactionIntent};
use crate::{AuthorizedMutation, CanonicalStore, MutationOperation, StoreError};
use jiandu_core::{
    CreationActor, Etag, MemoryId, MemoryPatch, MemoryRecord, MemoryRelation, MemorySchema,
    MemoryScope, MemoryStatus, MemoryType, Provenance, ProvenanceInput, RememberMemoryCommand,
    Revision, ScopeSelector, StoreRevision, Tag, Timestamp, UpdateMemoryCommand, Validate,
};
use serde::Serialize;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Internal host-resolved input after receipt lookup. Keeping this type private
/// prevents callers from bypassing the idempotent public create contract.
#[derive(Clone, Debug, PartialEq)]
struct CreateMemoryInput {
    memory_id: MemoryId,
    memory_type: MemoryType,
    title: String,
    summary: Option<String>,
    body: String,
    tags: Vec<Tag>,
    provenance: Provenance,
    relations: Vec<MemoryRelation>,
    created_at: Timestamp,
}

impl CreateMemoryInput {
    fn from_remember_command(
        command: &RememberMemoryCommand,
        memory_id: MemoryId,
        authorized_scope: &AuthorizedMutation,
        created_by: CreationActor,
        created_at: Timestamp,
    ) -> Result<Self, StoreError> {
        command.validate().map_err(|_| StoreError::InvalidRequest)?;
        if !selector_resolves_to(&command.scope, authorized_scope.as_scope()) {
            return Err(StoreError::InvalidRequest);
        }
        Ok(Self {
            memory_id,
            memory_type: command.memory_type,
            title: command.title.clone(),
            summary: command.summary.clone(),
            body: command.body.clone(),
            tags: command.tags.clone(),
            provenance: command.provenance.with_created_by(created_by),
            relations: command.relations.clone(),
            created_at,
        })
    }

    fn into_record(self, scope: MemoryScope) -> Result<(MemoryRecord, Vec<u8>), StoreError> {
        let provisional_etag = Etag::new("pending").map_err(|_| StoreError::InvalidRequest)?;
        let provisional = MemoryRecord {
            schema: MemorySchema::V1Alpha1,
            id: self.memory_id,
            revision: Revision::new(1).map_err(|_| StoreError::RevisionOverflow)?,
            etag: provisional_etag,
            scope,
            memory_type: self.memory_type,
            status: MemoryStatus::Active,
            title: self.title,
            summary: self.summary,
            body: self.body,
            tags: self.tags,
            created_at: self.created_at.clone(),
            updated_at: self.created_at,
            provenance: self.provenance,
            relations: self.relations,
        };
        provisional
            .validate()
            .map_err(|_| StoreError::InvalidRequest)?;
        canonicalize_record(&provisional)
    }
}

/// Durable canonical commit result. This is not an idempotency/audit receipt.
#[derive(Clone, Debug, PartialEq)]
pub struct MutationCommit {
    pub transaction_id: String,
    pub store_revision: StoreRevision,
    pub previous_revision: Option<Revision>,
    pub record: MemoryRecord,
    pub idempotent_replay: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RememberFingerprint<'a> {
    authoritative_scope: &'a MemoryScope,
    #[serde(rename = "type")]
    memory_type: MemoryType,
    title: &'a str,
    summary: Option<&'a str>,
    body: &'a str,
    tags: &'a [Tag],
    provenance: &'a ProvenanceInput,
    relations: &'a [MemoryRelation],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateFingerprint<'a> {
    authoritative_scope: &'a MemoryScope,
    memory_id: &'a MemoryId,
    expected_revision: Revision,
    patch: &'a MemoryPatch,
    reason: &'a str,
}

impl CanonicalStore {
    /// Idempotently create one canonical memory. Receipt lookup occurs after
    /// operation authorization and validation, but before generated ID/time or
    /// any existing-record check can affect the outcome.
    pub fn create(
        &mut self,
        authorization: &AuthorizedMutation,
        command: &RememberMemoryCommand,
        memory_id: MemoryId,
        created_by: CreationActor,
        created_at: Timestamp,
    ) -> Result<MutationCommit, StoreError> {
        self.validate_ownership()?;
        require_operation(authorization, MutationOperation::Create)?;
        command.validate().map_err(|_| StoreError::InvalidRequest)?;
        if !selector_resolves_to(&command.scope, authorization.as_scope()) {
            return Err(StoreError::InvalidRequest);
        }
        let request_fingerprint = crate::idempotency::request_fingerprint(&RememberFingerprint {
            authoritative_scope: authorization.as_scope(),
            memory_type: command.memory_type,
            title: &command.title,
            summary: command.summary.as_deref(),
            body: &command.body,
            tags: &command.tags,
            provenance: &command.provenance,
            relations: &command.relations,
        })?;
        let receipt_identity = ReceiptIdentity::derive(
            authorization.principal_id(),
            MutationOperation::Create,
            &command.idempotency_key,
        );
        if let Some(replay) = self.lookup_replay(
            authorization,
            MutationOperation::Create,
            &receipt_identity,
            &request_fingerprint,
        )? {
            return Ok(replay);
        }

        let input = CreateMemoryInput::from_remember_command(
            command,
            memory_id,
            authorization,
            created_by,
            created_at,
        )?;
        let (record, bytes) = input.into_record(authorization.as_scope().clone())?;
        if record_id_exists_anywhere(&self.root, &record.id)? {
            return Err(StoreError::AlreadyExists {
                id: record.id.clone(),
            });
        }
        self.ensure_metadata_current()?;
        let target_metadata = next_store_metadata(&self.metadata)?;
        let transaction_id = transaction::new_transaction_id();
        let binding = MutationBinding {
            receipt_id: receipt_identity.receipt_id,
            transaction_id: transaction_id.clone(),
            principal_digest: receipt_identity.principal_digest,
            key_digest: receipt_identity.key_digest,
            operation: MutationOperation::Create,
            scope: record.scope.clone(),
            request_fingerprint,
            memory_id: record.id.clone(),
            target_revision: record.revision,
            target_etag: record.etag.clone(),
            store_revision: target_metadata.store_revision,
            audit_sequence: target_metadata.audit_sequence,
        };
        let artifacts = MutationArtifacts::build(
            self.metadata.store_id.clone(),
            binding.clone(),
            None,
            record.clone(),
        )?;
        let manifest = transaction::TransactionManifest::for_record(
            self.metadata.store_id.clone(),
            transaction_id,
            RecordTransaction {
                operation: RecordOperation::Create,
                memory_id: record.id.clone(),
                scope: record.scope.clone(),
                base_revision: None,
                base_etag: None,
                target_revision: record.revision,
                target_etag: record.etag.clone(),
                base_store_metadata: self.metadata.clone(),
                target_store_metadata: target_metadata,
                idempotency: Some(IdempotencyTransaction {
                    binding,
                    result_digest: artifacts.result_digest.clone(),
                    receipt_digest: artifacts.receipt_digest.clone(),
                    audit_digest: artifacts.audit_digest.clone(),
                }),
            },
        )?;
        self.commit_record(manifest, record, &bytes, artifacts, None, None)
    }

    /// Apply one optimistic patch to the exact authoritative scope. A stale
    /// revision returns only the current revision, never a body or path.
    pub fn update(
        &mut self,
        authorization: &AuthorizedMutation,
        command: &UpdateMemoryCommand,
        updated_at: Timestamp,
    ) -> Result<MutationCommit, StoreError> {
        self.validate_ownership()?;
        require_operation(authorization, MutationOperation::Update)?;
        command.validate().map_err(|_| StoreError::InvalidRequest)?;
        let scope = authorization.as_scope();
        let request_fingerprint = crate::idempotency::request_fingerprint(&UpdateFingerprint {
            authoritative_scope: scope,
            memory_id: &command.memory_id,
            expected_revision: command.expected_revision,
            patch: &command.patch,
            reason: &command.reason,
        })?;
        let receipt_identity = ReceiptIdentity::derive(
            authorization.principal_id(),
            MutationOperation::Update,
            &command.idempotency_key,
        );
        if let Some(replay) = self.lookup_replay(
            authorization,
            MutationOperation::Update,
            &receipt_identity,
            &request_fingerprint,
        )? {
            return Ok(replay);
        }
        let relative = layout::record_relative_path(scope, &command.memory_id);
        let file = self
            .root
            .try_open_regular(&relative, false)?
            .ok_or(StoreError::NotFound)?;
        let current_identity = FileIdentity::from_file(&file)?;
        let read_file = file
            .try_clone()
            .map_err(|source| StoreError::io("clone current record handle", source))?;
        let current = Self::read_record_file(
            read_file,
            scope,
            Some(&command.memory_id),
            &layout::record_storage_key(&command.memory_id),
            &layout::record_shard(&command.memory_id),
        )?;
        if command.expected_revision != current.revision {
            return Err(StoreError::RevisionConflict {
                current_revision: current.revision,
            });
        }
        command
            .validate_against(&current)
            .map_err(|_| StoreError::InvalidRequest)?;
        if timestamp_nanos(&updated_at)? < timestamp_nanos(&current.updated_at)? {
            return Err(StoreError::InvalidRequest);
        }
        let target_record = apply_patch(current.clone(), &command.patch, updated_at)?;
        let (target_record, bytes) = canonicalize_record(&target_record)?;
        self.ensure_metadata_current()?;
        let target_metadata = next_store_metadata(&self.metadata)?;
        let transaction_id = transaction::new_transaction_id();
        let binding = MutationBinding {
            receipt_id: receipt_identity.receipt_id,
            transaction_id: transaction_id.clone(),
            principal_digest: receipt_identity.principal_digest,
            key_digest: receipt_identity.key_digest,
            operation: MutationOperation::Update,
            scope: current.scope.clone(),
            request_fingerprint,
            memory_id: current.id.clone(),
            target_revision: target_record.revision,
            target_etag: target_record.etag.clone(),
            store_revision: target_metadata.store_revision,
            audit_sequence: target_metadata.audit_sequence,
        };
        let artifacts = MutationArtifacts::build(
            self.metadata.store_id.clone(),
            binding.clone(),
            Some(current.revision),
            target_record.clone(),
        )?;
        let manifest = transaction::TransactionManifest::for_record(
            self.metadata.store_id.clone(),
            transaction_id,
            RecordTransaction {
                operation: RecordOperation::Update,
                memory_id: current.id.clone(),
                scope: current.scope.clone(),
                base_revision: Some(current.revision),
                base_etag: Some(current.etag.clone()),
                target_revision: target_record.revision,
                target_etag: target_record.etag.clone(),
                base_store_metadata: self.metadata.clone(),
                target_store_metadata: target_metadata,
                idempotency: Some(IdempotencyTransaction {
                    binding,
                    result_digest: artifacts.result_digest.clone(),
                    receipt_digest: artifacts.receipt_digest.clone(),
                    audit_digest: artifacts.audit_digest.clone(),
                }),
            },
        )?;
        self.commit_record(
            manifest,
            target_record,
            &bytes,
            artifacts,
            Some(current_identity),
            Some(current.revision),
        )
    }

    fn commit_record(
        &mut self,
        manifest: transaction::TransactionManifest,
        record: MemoryRecord,
        bytes: &[u8],
        artifacts: MutationArtifacts,
        expected_current: Option<FileIdentity>,
        previous_revision: Option<Revision>,
    ) -> Result<MutationCommit, StoreError> {
        let TransactionIntent::Record(intent) = &manifest.intent else {
            return Err(StoreError::InvalidTransaction);
        };
        let target_metadata = intent.target_store_metadata.clone();
        let transaction_id = manifest.transaction_id.clone();

        // From the first write-ahead byte onward, any error requires the same
        // startup reconciliation used after a process crash. This prevents a
        // live handle from serving a stale in-memory watermark after rename.
        self.poisoned = true;
        transaction::persist_manifest(&self.root, &manifest, &self.failpoints)?;
        let record_identity =
            transaction::stage_record(&self.root, &manifest, bytes, &self.failpoints)?;
        let metadata_identity =
            transaction::stage_metadata(&self.root, &manifest, &self.failpoints)?;
        transaction::prepare_idempotency_namespaces(&self.root, &manifest, &self.failpoints)?;
        let result_identity = transaction::stage_mutation_result(
            &self.root,
            &manifest,
            &artifacts.result_bytes,
            &self.failpoints,
        )?;
        let receipt_identity = transaction::stage_idempotency_receipt(
            &self.root,
            &manifest,
            &artifacts.receipt_bytes,
            &self.failpoints,
        )?;
        let audit_identity = transaction::stage_mutation_audit(
            &self.root,
            &manifest,
            &artifacts.audit_bytes,
            &self.failpoints,
        )?;
        transaction::publish_record(
            &self.root,
            &manifest,
            record_identity,
            expected_current,
            &self.failpoints,
        )?;
        transaction::publish_mutation_result(
            &self.root,
            &manifest,
            result_identity,
            &self.failpoints,
        )?;
        transaction::publish_idempotency_receipt(
            &self.root,
            &manifest,
            receipt_identity,
            &self.failpoints,
        )?;
        transaction::publish_mutation_audit(
            &self.root,
            &manifest,
            audit_identity,
            &self.failpoints,
        )?;
        transaction::publish_metadata(&self.root, &manifest, metadata_identity, &self.failpoints)?;
        self.metadata = target_metadata;
        transaction::remove_manifest(&self.root, &manifest, &self.failpoints)?;
        self.poisoned = false;
        Ok(MutationCommit {
            transaction_id,
            store_revision: self.metadata.store_revision,
            previous_revision,
            record,
            idempotent_replay: false,
        })
    }

    fn lookup_replay(
        &self,
        authorization: &AuthorizedMutation,
        operation: MutationOperation,
        identity: &ReceiptIdentity,
        request_fingerprint: &str,
    ) -> Result<Option<MutationCommit>, StoreError> {
        let Some(receipt) = crate::idempotency::read_receipt(
            &self.root,
            &self.metadata.store_id,
            identity,
            operation,
        )?
        else {
            return Ok(None);
        };
        let binding = &receipt.binding;
        if binding.receipt_id != identity.receipt_id
            || binding.principal_digest != identity.principal_digest
            || binding.key_digest != identity.key_digest
            || binding.operation != operation
        {
            return Err(StoreError::InvalidTransaction);
        }
        // The freshly minted capability must still authorize the exact scope
        // stored in the receipt. Scope/fingerprint differences are conflicting
        // key reuse, never a reason to disclose the historical result.
        if binding.scope != *authorization.as_scope()
            || binding.request_fingerprint != request_fingerprint
        {
            return Err(StoreError::IdempotencyConflict);
        }
        let result = crate::idempotency::read_result(
            &self.root,
            &self.metadata.store_id,
            binding,
            &receipt.result_digest,
        )?;
        crate::idempotency::verify_audit(
            &self.root,
            &self.metadata.store_id,
            binding,
            &receipt.result_digest,
        )?;
        Ok(Some(MutationCommit {
            transaction_id: binding.transaction_id.clone(),
            store_revision: binding.store_revision,
            previous_revision: result.previous_revision,
            record: result.record,
            idempotent_replay: true,
        }))
    }

    fn ensure_metadata_current(&self) -> Result<(), StoreError> {
        let (metadata, _) = crate::store::read_store_metadata(&self.root)?;
        if metadata == self.metadata {
            Ok(())
        } else {
            Err(StoreError::RecoveryRequired)
        }
    }
}

fn canonicalize_record(record: &MemoryRecord) -> Result<(MemoryRecord, Vec<u8>), StoreError> {
    record.validate().map_err(|_| StoreError::InvalidRequest)?;
    let frontmatter = jiandu_core::MemoryFrontmatterV1Alpha1::from_record(record);
    let bytes = encode_canonical_document(&frontmatter, &record.body)?;
    let decoded = decode_canonical_document(&bytes, Some(&record.id))?.record;
    Ok((decoded, bytes))
}

fn next_store_metadata(current: &crate::StoreMetadata) -> Result<crate::StoreMetadata, StoreError> {
    let mut target = current.clone();
    target.store_revision = StoreRevision(
        current
            .store_revision
            .0
            .checked_add(1)
            .ok_or(StoreError::RevisionOverflow)?,
    );
    target.audit_sequence = crate::AuditSequence(
        current
            .audit_sequence
            .0
            .checked_add(1)
            .ok_or(StoreError::RevisionOverflow)?,
    );
    Ok(target)
}

fn require_operation(
    authorization: &AuthorizedMutation,
    expected: MutationOperation,
) -> Result<(), StoreError> {
    if authorization.operation() == expected {
        Ok(())
    } else {
        Err(StoreError::Forbidden)
    }
}

fn apply_patch(
    mut current: MemoryRecord,
    patch: &MemoryPatch,
    updated_at: Timestamp,
) -> Result<MemoryRecord, StoreError> {
    current.revision = Revision::new(
        current
            .revision
            .get()
            .checked_add(1)
            .ok_or(StoreError::RevisionOverflow)?,
    )
    .map_err(|_| StoreError::RevisionOverflow)?;
    current.updated_at = updated_at;
    if let Some(title) = &patch.title {
        current.title.clone_from(title);
    }
    if let Some(body) = &patch.body {
        current.body.clone_from(body);
    }
    if let Some(status) = patch.status {
        current.status = status;
    }
    if let Some(tags) = &patch.tags {
        let mut values: BTreeSet<_> = current.tags.into_iter().collect();
        for removed in &tags.remove {
            values.remove(removed);
        }
        values.extend(tags.add.iter().cloned());
        current.tags = values.into_iter().collect();
    }
    if let Some(relations) = &patch.relations {
        let mut values: BTreeSet<_> = current.relations.into_iter().collect();
        for removed in &relations.remove {
            values.remove(removed);
        }
        values.extend(relations.add.iter().cloned());
        current.relations = values.into_iter().collect();
    }
    current.validate().map_err(|_| StoreError::InvalidRequest)?;
    Ok(current)
}

fn selector_resolves_to(selector: &ScopeSelector, scope: &MemoryScope) -> bool {
    match (selector, scope) {
        (ScopeSelector::Principal {}, MemoryScope::Principal { .. })
        | (ScopeSelector::InstanceGlobal {}, MemoryScope::InstanceGlobal {}) => true,
        (
            ScopeSelector::Project { project_id: left },
            MemoryScope::Project { project_id: right },
        ) => left == right,
        (
            ScopeSelector::Session { session_id: left },
            MemoryScope::Session { session_id: right },
        ) => left == right,
        _ => false,
    }
}

fn timestamp_nanos(timestamp: &Timestamp) -> Result<i128, StoreError> {
    OffsetDateTime::parse(timestamp.as_str(), &Rfc3339)
        .map(OffsetDateTime::unix_timestamp_nanos)
        .map_err(|_| StoreError::InvalidRequest)
}

/// Enforce the global MemoryId invariant without parsing another tenant's
/// record body. Only private owner directory keys and the exact hashed target
/// filename are inspected; the resulting error remains path/body-free.
fn record_id_exists_anywhere(
    root: &layout::StoreDirectory,
    id: &MemoryId,
) -> Result<bool, StoreError> {
    let shard_name = layout::record_shard(id);
    let file_name = layout::record_file_name(id);
    for kind in ["principal", "project", "session"] {
        let kind_directory =
            root.open_directory(std::path::Path::new("records").join(kind).as_path())?;
        let owners = kind_directory
            .entries()
            .map_err(|source| StoreError::io("scan memory owner keys", source))?;
        for owner in owners {
            let owner = owner.map_err(|source| StoreError::io("read memory owner key", source))?;
            let owner_name = owner.file_name();
            layout::validate_owner_entry_name(&owner_name)?;
            let owner_directory =
                layout::StoreDirectory::open_child_directory(&kind_directory, &owner_name)?;
            let Some(shard_directory) = layout::StoreDirectory::try_open_child_directory(
                &owner_directory,
                OsStr::new(&shard_name),
            )?
            else {
                continue;
            };
            if layout::StoreDirectory::try_open_regular_in(
                &shard_directory,
                OsStr::new(&file_name),
            )?
            .is_some()
            {
                return Ok(true);
            }
        }
    }
    let global = std::path::Path::new("records")
        .join("instance_global")
        .join(shard_name)
        .join(file_name);
    root.regular_file_exists(&global)
}
