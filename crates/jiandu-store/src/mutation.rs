//! Canonical create/update CAS operations.

use crate::document::{decode_canonical_document, encode_canonical_document};
use crate::layout::{self, FileIdentity};
use crate::transaction::{self, RecordOperation, RecordTransaction, TransactionIntent};
use crate::{AuthorizedScope, CanonicalStore, StoreError};
use jiandu_core::{
    CreationActor, Etag, MemoryId, MemoryPatch, MemoryRecord, MemoryRelation, MemorySchema,
    MemoryScope, MemoryStatus, MemoryType, Provenance, RememberMemoryCommand, Revision,
    ScopeSelector, StoreRevision, Tag, Timestamp, UpdateMemoryCommand, Validate,
};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Host-resolved canonical input for one new memory.
///
/// ID, scope, actor, and timestamp are authoritative host values. The store
/// stamps revision 1 and derives the ETag from canonical bytes. Idempotency is
/// intentionally not claimed by this type; durable replay receipts belong to
/// Issue #5.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateMemoryInput {
    pub memory_id: MemoryId,
    pub memory_type: MemoryType,
    pub title: String,
    pub summary: Option<String>,
    pub body: String,
    pub tags: Vec<Tag>,
    pub provenance: Provenance,
    pub relations: Vec<MemoryRelation>,
    pub created_at: Timestamp,
}

impl CreateMemoryInput {
    /// Resolve the model-visible remember command without treating its
    /// idempotency key as durable. The future receipt layer must extend the
    /// transaction before acknowledgment rather than invoking a post-commit
    /// callback that could falsely imply atomicity.
    pub fn from_remember_command(
        command: &RememberMemoryCommand,
        memory_id: MemoryId,
        authorized_scope: &AuthorizedScope,
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
}

impl CanonicalStore {
    /// Create one canonical memory atomically, failing if the target already
    /// exists. The current owner serializes this operation with all other
    /// mutations through the exclusive `&mut self` borrow.
    pub fn create(
        &mut self,
        authorized_scope: &AuthorizedScope,
        input: CreateMemoryInput,
    ) -> Result<MutationCommit, StoreError> {
        self.validate_ownership()?;
        let (record, bytes) = input.into_record(authorized_scope.as_scope().clone())?;
        if record_id_exists_anywhere(&self.root, &record.id)? {
            return Err(StoreError::AlreadyExists {
                id: record.id.clone(),
            });
        }
        self.ensure_metadata_current()?;
        let target_metadata = next_store_metadata(&self.metadata)?;
        let manifest = transaction::TransactionManifest::for_record(
            self.metadata.store_id.clone(),
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
            },
        )?;
        self.commit_record(manifest, record, &bytes, None, None)
    }

    /// Apply one optimistic patch to the exact authoritative scope. A stale
    /// revision returns only the current revision, never a body or path.
    pub fn update(
        &mut self,
        authorized_scope: &AuthorizedScope,
        command: &UpdateMemoryCommand,
        updated_at: Timestamp,
    ) -> Result<MutationCommit, StoreError> {
        self.validate_ownership()?;
        command.validate().map_err(|_| StoreError::InvalidRequest)?;
        let scope = authorized_scope.as_scope();
        let relative = layout::record_relative_path(scope, &command.memory_id);
        let file = self
            .root
            .try_open_regular(&relative, false)?
            .ok_or(StoreError::NotFound)?;
        let identity = FileIdentity::from_file(&file)?;
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
        let manifest = transaction::TransactionManifest::for_record(
            self.metadata.store_id.clone(),
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
            },
        )?;
        self.commit_record(
            manifest,
            target_record,
            &bytes,
            Some(identity),
            Some(current.revision),
        )
    }

    fn commit_record(
        &mut self,
        manifest: transaction::TransactionManifest,
        record: MemoryRecord,
        bytes: &[u8],
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
        transaction::publish_record(
            &self.root,
            &manifest,
            record_identity,
            expected_current,
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
        })
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
    Ok(target)
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
