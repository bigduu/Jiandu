//! Strict body-free protected tombstones and non-executing admin plans.

use crate::layout::{self, StoreDirectory};
use crate::transaction;
use crate::{AuditSequence, StoreError, StoreId};
use jiandu_core::{Etag, MemoryId, MemoryScope, PrincipalId, Revision, StoreRevision, Timestamp};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;

pub(crate) const TOMBSTONE_FORMAT_VERSION: &str = "jiandu.store.tombstone/v1alpha1";
const MAX_TOMBSTONE_BYTES: usize = 65_536;

/// Operator-only lifecycle action. This slice can plan, but never execute, it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAction {
    Restore,
    HardPurge,
}

impl AdminAction {
    pub(crate) const fn required_grant(self) -> &'static str {
        match self {
            Self::Restore => "memory:admin:restore",
            Self::HardPurge => "memory:admin:hard_purge",
        }
    }
}

/// One exact opaque target in a deterministic administrative dry-run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminPlanTarget {
    pub memory_id: MemoryId,
    pub scope: MemoryScope,
    pub revision: Revision,
    pub etag: Etag,
}

/// Read-only administrative plan. Possessing it does not authorize execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminActionPlan {
    pub action: AdminAction,
    pub targets: Vec<AdminPlanTarget>,
    pub count: usize,
    pub store_revision: StoreRevision,
    pub confirmation_digest: String,
}

/// Durable protection for one forgotten opaque identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProtectedTombstone {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) transaction_id: String,
    pub(crate) memory_id: MemoryId,
    pub(crate) scope: MemoryScope,
    pub(crate) revision: Revision,
    pub(crate) etag: Etag,
    pub(crate) forgotten_at: Timestamp,
    pub(crate) store_revision: StoreRevision,
    pub(crate) audit_sequence: AuditSequence,
}

impl ProtectedTombstone {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store_id: StoreId,
        transaction_id: String,
        memory_id: MemoryId,
        scope: MemoryScope,
        revision: Revision,
        etag: Etag,
        forgotten_at: Timestamp,
        store_revision: StoreRevision,
        audit_sequence: AuditSequence,
    ) -> Result<Self, StoreError> {
        let tombstone = Self {
            format_version: TOMBSTONE_FORMAT_VERSION.to_owned(),
            store_id,
            transaction_id,
            memory_id,
            scope,
            revision,
            etag,
            forgotten_at,
            store_revision,
            audit_sequence,
        };
        tombstone.validate()?;
        Ok(tombstone)
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        crate::idempotency::canonical_json(self, MAX_TOMBSTONE_BYTES)
    }

    pub(crate) fn decode(file: File, expected_store_id: &StoreId) -> Result<Self, StoreError> {
        crate::idempotency::decode_canonical(file, MAX_TOMBSTONE_BYTES, |tombstone: &Self| {
            tombstone.validate()?;
            if &tombstone.store_id != expected_store_id {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        if self.format_version != TOMBSTONE_FORMAT_VERSION
            || !transaction::valid_transaction_id(&self.transaction_id)
            || !transaction::valid_content_digest(self.etag.as_str())
            || self.store_revision.0 == 0
            || self.audit_sequence.0 == 0
            || self.audit_sequence.0 > self.store_revision.0
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }

    pub(crate) fn relative_path(&self) -> std::path::PathBuf {
        layout::tombstone_relative_path(&self.scope, &self.memory_id)
    }
}

/// Enforce global non-resurrection without parsing or disclosing another
/// tenant's tombstone. Only strict owner keys and the hashed target filename
/// are inspected.
pub(crate) fn id_exists_anywhere(root: &StoreDirectory, id: &MemoryId) -> Result<bool, StoreError> {
    let shard_name = layout::record_shard(id);
    let file_name = layout::tombstone_file_name(id);
    for kind in ["principal", "project", "session"] {
        let kind_directory = root.open_directory(
            std::path::Path::new(layout::TOMBSTONES_DIR)
                .join(kind)
                .as_path(),
        )?;
        let owners = kind_directory
            .entries()
            .map_err(|source| StoreError::io("scan tombstone owner keys", source))?;
        for owner in owners {
            let owner =
                owner.map_err(|source| StoreError::io("read tombstone owner key", source))?;
            let owner_name = owner.file_name();
            layout::validate_owner_entry_name(&owner_name)?;
            let owner_directory =
                StoreDirectory::open_child_directory(&kind_directory, &owner_name)?;
            let Some(shard_directory) = StoreDirectory::try_open_child_directory(
                &owner_directory,
                OsStr::new(&shard_name),
            )?
            else {
                continue;
            };
            if StoreDirectory::try_open_regular_in(&shard_directory, OsStr::new(&file_name))?
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    root.regular_file_exists(
        &std::path::Path::new(layout::TOMBSTONES_DIR)
            .join("instance_global")
            .join(shard_name)
            .join(file_name),
    )
}

/// Collect every canonical tombstone storage key without decoding or exposing
/// another scope's protected metadata. Callers use this once per list scan so
/// a tombstoned ID is filtered before any ambient record body is opened.
pub(crate) fn storage_keys(root: &StoreDirectory) -> Result<BTreeSet<String>, StoreError> {
    let mut keys = BTreeSet::new();
    for kind in ["principal", "project", "session"] {
        let kind_directory =
            root.open_directory(Path::new(layout::TOMBSTONES_DIR).join(kind).as_path())?;
        let owners = kind_directory
            .entries()
            .map_err(|source| StoreError::io("scan tombstone owner keys", source))?;
        for owner in owners {
            let owner =
                owner.map_err(|source| StoreError::io("read tombstone owner key", source))?;
            let owner_name = owner.file_name();
            layout::validate_owner_entry_name(&owner_name)?;
            let owner_directory =
                StoreDirectory::open_child_directory(&kind_directory, &owner_name)?;
            collect_storage_keys(&owner_directory, &mut keys)?;
        }
    }
    let global = root.open_directory(
        Path::new(layout::TOMBSTONES_DIR)
            .join("instance_global")
            .as_path(),
    )?;
    collect_storage_keys(&global, &mut keys)?;
    Ok(keys)
}

fn collect_storage_keys(
    owner_directory: &cap_std::fs::Dir,
    keys: &mut BTreeSet<String>,
) -> Result<(), StoreError> {
    let shards = owner_directory
        .entries()
        .map_err(|source| StoreError::io("scan tombstone shards", source))?;
    for shard in shards {
        let shard = shard.map_err(|source| StoreError::io("read tombstone shard", source))?;
        let shard_name = shard
            .file_name()
            .into_string()
            .map_err(|_| StoreError::UnsafePath)?;
        if shard_name.len() != 2
            || !shard_name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StoreError::InvalidLayout);
        }
        let shard_directory =
            StoreDirectory::open_child_directory(owner_directory, OsStr::new(&shard_name))?;
        let entries = shard_directory
            .entries()
            .map_err(|source| StoreError::io("scan protected tombstones", source))?;
        for entry in entries {
            let entry =
                entry.map_err(|source| StoreError::io("read protected tombstone", source))?;
            let name = entry.file_name();
            let storage_key = layout::validate_tombstone_entry_name(&name)?;
            if !storage_key.starts_with(&shard_name) {
                return Err(StoreError::InvalidLayout);
            }
            let file = StoreDirectory::try_open_regular_in(&shard_directory, &name)?
                .ok_or(StoreError::InvalidLayout)?;
            StoreDirectory::validate_private_open_file(&file)?;
            keys.insert(storage_key);
        }
    }
    Ok(())
}

pub(crate) fn read_exact(
    root: &StoreDirectory,
    store_id: &StoreId,
    scope: &MemoryScope,
    memory_id: &MemoryId,
) -> Result<Option<ProtectedTombstone>, StoreError> {
    read_exact_inner(root, store_id, scope, memory_id, &mut |_| Ok(()))
}

pub(crate) fn read_exact_bounded(
    root: &StoreDirectory,
    store_id: &StoreId,
    scope: &MemoryScope,
    memory_id: &MemoryId,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<Option<ProtectedTombstone>, StoreError> {
    read_exact_inner(root, store_id, scope, memory_id, &mut |file| {
        let length = file
            .metadata()
            .map_err(|source| StoreError::io("inspect bounded tombstone", source))?
            .len();
        if budget.consume_bytes(length) {
            Ok(())
        } else {
            Err(StoreError::InvalidRequest)
        }
    })
}

fn read_exact_inner(
    root: &StoreDirectory,
    store_id: &StoreId,
    scope: &MemoryScope,
    memory_id: &MemoryId,
    before_decode: &mut impl FnMut(&File) -> Result<(), StoreError>,
) -> Result<Option<ProtectedTombstone>, StoreError> {
    root.try_open_regular(&layout::tombstone_relative_path(scope, memory_id), false)?
        .map(|file| {
            before_decode(&file)?;
            StoreDirectory::validate_private_open_file(&file)?;
            let tombstone = ProtectedTombstone::decode(file, store_id)?;
            if &tombstone.scope != scope
                || &tombstone.memory_id != memory_id
                || tombstone.relative_path() != layout::tombstone_relative_path(scope, memory_id)
            {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(tombstone)
        })
        .transpose()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationInput<'a> {
    format_version: &'static str,
    store_id: &'a StoreId,
    principal_id: &'a PrincipalId,
    action: AdminAction,
    scope: &'a MemoryScope,
    store_revision: StoreRevision,
    targets: Vec<ConfirmationTarget<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationTarget<'a> {
    transaction_id: &'a str,
    memory_id: &'a MemoryId,
    scope: &'a MemoryScope,
    revision: Revision,
    etag: &'a Etag,
    forgotten_at: &'a Timestamp,
    store_revision: StoreRevision,
    audit_sequence: AuditSequence,
}

pub(crate) fn confirmation_digest(
    store_id: &StoreId,
    principal_id: &PrincipalId,
    action: AdminAction,
    scope: &MemoryScope,
    store_revision: StoreRevision,
    tombstones: &[ProtectedTombstone],
) -> Result<String, StoreError> {
    let targets = tombstones
        .iter()
        .map(|tombstone| ConfirmationTarget {
            transaction_id: &tombstone.transaction_id,
            memory_id: &tombstone.memory_id,
            scope: &tombstone.scope,
            revision: tombstone.revision,
            etag: &tombstone.etag,
            forgotten_at: &tombstone.forgotten_at,
            store_revision: tombstone.store_revision,
            audit_sequence: tombstone.audit_sequence,
        })
        .collect();
    let bytes = serde_json::to_vec(&ConfirmationInput {
        format_version: "jiandu.admin-plan/v1alpha1",
        store_id,
        principal_id,
        action,
        scope,
        store_revision,
        targets,
    })
    .map_err(|_| StoreError::InvalidRequest)?;
    let mut hasher = Sha256::new();
    hasher.update(b"jiandu/admin-confirmation/v1\0");
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}
