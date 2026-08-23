//! Store initialization, opening, authorization, and read APIs.

use crate::document::{MAX_CANONICAL_DOCUMENT_BYTES, decode_canonical_document};
use crate::layout;
use crate::{InvalidRecordReason, StoreError, StoreId, StoreMetadata};
use jiandu_core::{
    ListSort, MemoryId, MemoryListRequest, MemoryListResult, MemoryRecord, MemoryScope,
    MemorySummary, PrincipalId, ProjectId, ScopeSelector, SessionId, StoreRevision, Timestamp,
    Validate,
};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

const MAX_STORE_METADATA_BYTES: usize = 16_384;

/// Host-resolved scope authority. No path or model-selected principal is accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedScopes {
    pub(crate) principal_id: PrincipalId,
    pub(crate) project_ids: BTreeSet<ProjectId>,
    pub(crate) session_ids: BTreeSet<SessionId>,
    pub(crate) instance_global: bool,
}

impl AuthorizedScopes {
    #[must_use]
    pub fn new(principal_id: PrincipalId) -> Self {
        Self {
            principal_id,
            project_ids: BTreeSet::new(),
            session_ids: BTreeSet::new(),
            instance_global: false,
        }
    }

    #[must_use]
    pub fn with_project(mut self, project_id: ProjectId) -> Self {
        self.project_ids.insert(project_id);
        self
    }

    #[must_use]
    pub fn with_session(mut self, session_id: SessionId) -> Self {
        self.session_ids.insert(session_id);
        self
    }

    #[must_use]
    pub const fn with_instance_global(mut self) -> Self {
        self.instance_global = true;
        self
    }

    fn all_scopes(&self) -> Vec<MemoryScope> {
        let mut scopes = Vec::with_capacity(
            1 + self.project_ids.len() + self.session_ids.len() + usize::from(self.instance_global),
        );
        scopes.push(MemoryScope::Principal {
            principal_id: self.principal_id.clone(),
        });
        scopes.extend(
            self.project_ids
                .iter()
                .cloned()
                .map(|project_id| MemoryScope::Project { project_id }),
        );
        scopes.extend(
            self.session_ids
                .iter()
                .cloned()
                .map(|session_id| MemoryScope::Session { session_id }),
        );
        if self.instance_global {
            scopes.push(MemoryScope::InstanceGlobal {});
        }
        scopes
    }

    fn resolve_requested(&self, selectors: &[ScopeSelector]) -> Vec<MemoryScope> {
        selectors
            .iter()
            .filter_map(|selector| match selector {
                ScopeSelector::Principal {} => Some(MemoryScope::Principal {
                    principal_id: self.principal_id.clone(),
                }),
                ScopeSelector::Project { project_id } if self.project_ids.contains(project_id) => {
                    Some(MemoryScope::Project {
                        project_id: project_id.clone(),
                    })
                }
                ScopeSelector::Session { session_id } if self.session_ids.contains(session_id) => {
                    Some(MemoryScope::Session {
                        session_id: session_id.clone(),
                    })
                }
                ScopeSelector::InstanceGlobal {} if self.instance_global => {
                    Some(MemoryScope::InstanceGlobal {})
                }
                _ => None,
            })
            .collect()
    }
}

/// One result observed at an authoritative store watermark.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreRead<T> {
    pub store_revision: StoreRevision,
    pub result: T,
}

pub type StoreWatermark = StoreRevision;

/// Path-free receipt for an explicit operator quarantine action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineReceipt {
    pub memory_id: MemoryId,
    pub quarantine_token: String,
}

/// Exclusively owned handle to one supported canonical store.
pub struct CanonicalStore {
    root: layout::StoreDirectory,
    metadata: StoreMetadata,
    _lock: crate::lock::StoreLock,
}

impl fmt::Debug for CanonicalStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalStore")
            .field("store_id", &self.metadata.store_id)
            .field("store_revision", &self.metadata.store_revision)
            .finish_non_exhaustive()
    }
}

impl CanonicalStore {
    /// Initialize a new store and hold its owning lock.
    pub fn initialize(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
    ) -> Result<Self, StoreError> {
        let root = layout::StoreDirectory::open(data_dir.as_ref(), true)?;
        if root.regular_file_exists(Path::new(layout::STORE_METADATA_FILE))? {
            return Err(StoreError::AlreadyInitialized);
        }
        layout::validate_initialization_state(&root)?;

        let mut lock = crate::lock::StoreLock::acquire(&root, true)?;
        lock.validate_initialization_marker()?;
        root.harden_root()?;
        lock.harden_permissions()?;
        lock.publish_owner(&root, &owner)?;
        if root.regular_file_exists(Path::new(layout::STORE_METADATA_FILE))? {
            return Err(StoreError::AlreadyInitialized);
        }
        layout::validate_initialization_state(&root)?;

        let metadata = prepare_initial_metadata(&root)?;
        layout::create_layout(&root)?;
        commit_initial_metadata(&root)?;
        root.validate_ambient_identity()?;
        lock.validate_ownership(&root)?;
        Ok(Self {
            root,
            metadata,
            _lock: lock,
        })
    }

    /// Open a supported store without touching canonical records.
    ///
    /// The format is inspected before opening or updating `LOCK`, so a future
    /// store format fails closed without mutating any entry in the directory.
    pub fn open(data_dir: impl AsRef<Path>, owner: crate::LockOwner) -> Result<Self, StoreError> {
        let root = layout::StoreDirectory::open(data_dir.as_ref(), false)?;
        root.validate_private_root()?;
        let (metadata, original_metadata_bytes) = read_store_metadata(&root)?;
        layout::validate_layout(&root)?;
        if !root.regular_file_exists(Path::new(layout::STORE_LOCK_FILE))? {
            return Err(StoreError::InvalidLayout);
        }
        let _lock_file = root.validate_private_file(Path::new(layout::STORE_LOCK_FILE))?;

        let mut lock = crate::lock::StoreLock::acquire(&root, false)?;
        let (locked_metadata, locked_metadata_bytes) = read_store_metadata(&root)?;
        if locked_metadata != metadata || locked_metadata_bytes != original_metadata_bytes {
            return Err(StoreError::InvalidStoreMetadata);
        }
        root.validate_ambient_identity()?;
        lock.publish_owner(&root, &owner)?;
        Ok(Self {
            root,
            metadata,
            _lock: lock,
        })
    }

    #[must_use]
    pub fn store_id(&self) -> &StoreId {
        &self.metadata.store_id
    }

    #[must_use]
    pub const fn watermark(&self) -> StoreWatermark {
        self.metadata.store_revision
    }

    /// Read exactly one record visible in `authorized`.
    ///
    /// Invisible and absent IDs deliberately produce the same `NotFound`
    /// result. Only candidate paths in authoritative allowed scopes are read.
    pub fn get(
        &self,
        id: &MemoryId,
        authorized: &AuthorizedScopes,
    ) -> Result<StoreRead<MemoryRecord>, StoreError> {
        self.validate_ownership()?;
        let mut candidate = None;
        for scope in authorized.all_scopes() {
            let relative = layout::record_relative_path(&scope, id);
            if let Some(file) = self.root.try_open_regular(&relative, false)? {
                if candidate.is_some() {
                    return Err(StoreError::DuplicateMemoryId { id: id.clone() });
                }
                candidate = Some((scope, file));
            }
        }
        let (scope, file) = candidate.ok_or(StoreError::NotFound)?;
        let record = Self::read_record_file(
            file,
            &scope,
            Some(id),
            &layout::record_storage_key(id),
            &layout::record_shard(id),
        )?;
        Ok(StoreRead {
            store_revision: self.watermark(),
            result: record,
        })
    }

    /// Deterministically list validated records from requested authorized scopes.
    pub fn list(
        &self,
        request: &MemoryListRequest,
        authorized: &AuthorizedScopes,
    ) -> Result<StoreRead<MemoryListResult>, StoreError> {
        self.validate_ownership()?;
        request.validate().map_err(|_| StoreError::InvalidRequest)?;
        let selected_scopes = authorized.resolve_requested(&request.scopes);
        let fingerprint =
            crate::cursor::binding_fingerprint(request, authorized, &selected_scopes)?;
        let offset = match &request.cursor {
            Some(cursor) => crate::cursor::decode(
                cursor,
                &self.metadata.store_id,
                self.watermark(),
                &fingerprint,
            )?,
            None => 0,
        };

        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for scope in &selected_scopes {
            for record in self.scan_scope(scope)? {
                if !seen.insert(record.id.clone()) {
                    return Err(StoreError::DuplicateMemoryId {
                        id: record.id.clone(),
                    });
                }
                if matches_filters(&record, request) {
                    records.push(record);
                }
            }
        }
        sort_records(&mut records, request.sort);

        if offset > records.len() {
            return Err(StoreError::InvalidCursor);
        }
        let limit = usize::from(request.limit.get());
        let end = offset.saturating_add(limit).min(records.len());
        let has_more = end < records.len();
        let next_cursor = if has_more {
            Some(crate::cursor::encode(
                &self.metadata.store_id,
                self.watermark(),
                &fingerprint,
                end,
            )?)
        } else {
            None
        };
        let memories = records[offset..end]
            .iter()
            .map(summary_from_record)
            .collect();
        let result = MemoryListResult {
            memories,
            next_cursor,
            has_more,
        };
        result.validate().map_err(|_| StoreError::InvalidRecord {
            id: None,
            reason: InvalidRecordReason::ValidationFailed,
        })?;
        Ok(StoreRead {
            store_revision: self.watermark(),
            result,
        })
    }

    /// Move one invalid canonical candidate into quarantine by explicit
    /// operator action. The exclusive borrow prevents safe concurrent reads
    /// from observing the namespace move. Ordinary `get` and `list` calls
    /// never invoke this.
    pub fn quarantine_invalid(
        &mut self,
        scope: &MemoryScope,
        id: &MemoryId,
    ) -> Result<QuarantineReceipt, StoreError> {
        self.validate_ownership()?;
        let relative = layout::record_relative_path(scope, id);
        let source_directory_path = relative.parent().ok_or(StoreError::UnsafePath)?;
        let source_name = relative.file_name().ok_or(StoreError::UnsafePath)?;
        let source_directory = self.root.open_directory(source_directory_path)?;
        let file = layout::StoreDirectory::try_open_regular_in(&source_directory, source_name)?
            .ok_or(StoreError::NotFound)?;
        let source_identity = layout::FileIdentity::from_file(&file)?;
        let read_file = file
            .try_clone()
            .map_err(|source| StoreError::io("clone invalid record handle", source))?;
        match Self::read_record_file(
            read_file,
            scope,
            Some(id),
            &layout::record_storage_key(id),
            &layout::record_shard(id),
        ) {
            Ok(_) => return Err(StoreError::RecordIsValid { id: id.clone() }),
            Err(StoreError::InvalidRecord { .. }) => {}
            Err(error) => return Err(error),
        }

        let quarantine_token = Uuid::new_v4().simple().to_string();
        let destination_name =
            format!("{}.{}.md", layout::record_storage_key(id), quarantine_token);
        let quarantine_directory = self
            .root
            .open_directory(Path::new(layout::QUARANTINE_DIR))?;
        if layout::StoreDirectory::try_open_regular_in(
            &quarantine_directory,
            OsStr::new(&destination_name),
        )?
        .is_some()
        {
            return Err(StoreError::InvalidLayout);
        }
        layout::StoreDirectory::rename_between(
            &source_directory,
            source_name,
            &quarantine_directory,
            OsStr::new(&destination_name),
        )
        .map_err(|error| match error {
            StoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                StoreError::UnsafePath
            }
            other => other,
        })?;
        let moved = layout::StoreDirectory::try_open_regular_in(
            &quarantine_directory,
            OsStr::new(&destination_name),
        );
        let moved_matches = match moved.as_ref() {
            Ok(Some(moved)) => layout::FileIdentity::from_file(moved)? == source_identity,
            Ok(None) | Err(_) => false,
        };
        if !moved_matches {
            // The source name changed after validation. Recover the moved
            // replacement when possible, but never treat it as the verified
            // invalid inode.
            if layout::StoreDirectory::try_open_regular_in(&source_directory, source_name)?
                .is_none()
            {
                let _ = layout::StoreDirectory::rename_between(
                    &quarantine_directory,
                    OsStr::new(&destination_name),
                    &source_directory,
                    source_name,
                );
            }
            return Err(StoreError::UnsafePath);
        }
        // Keep the verified source handle alive until identity has been
        // checked at the destination.
        drop(file);
        layout::StoreDirectory::sync_open_directory(
            &quarantine_directory,
            "sync quarantine directory",
        )?;
        // Persist the destination entry before the source removal. A crash
        // between directory fsyncs can then leave a duplicate, not lose the
        // only copy of an invalid operator-managed record.
        layout::StoreDirectory::sync_open_directory(
            &source_directory,
            "sync source shard directory",
        )?;
        Ok(QuarantineReceipt {
            memory_id: id.clone(),
            quarantine_token,
        })
    }

    fn scan_scope(&self, scope: &MemoryScope) -> Result<Vec<MemoryRecord>, StoreError> {
        let scope_relative = layout::scope_relative_directory(scope);
        let Some(scope_directory) = self.root.try_open_directory(&scope_relative)? else {
            return Ok(Vec::new());
        };
        #[cfg(all(test, unix))]
        layout::run_test_hook(
            layout::TestHookPoint::DirectoryEntries,
            scope_relative.file_name().ok_or(StoreError::UnsafePath)?,
        );

        let mut records = Vec::new();
        let shard_entries = scope_directory
            .entries()
            .map_err(|source| StoreError::io("list scope shards", source))?;
        for shard_entry in shard_entries {
            let shard_entry =
                shard_entry.map_err(|source| StoreError::io("read scope shard entry", source))?;
            let shard = shard_entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::InvalidLayout)?;
            if !valid_shard_name(&shard) {
                return Err(StoreError::InvalidLayout);
            }
            let shard_directory =
                layout::StoreDirectory::open_child_directory(&scope_directory, OsStr::new(&shard))?;
            let record_entries = shard_directory
                .entries()
                .map_err(|source| StoreError::io("list shard records", source))?;
            for record_entry in record_entries {
                let record_entry = record_entry
                    .map_err(|source| StoreError::io("read shard record entry", source))?;
                let file_name = record_entry.file_name();
                let storage_key = layout::validate_record_entry_name(&file_name)?;
                if !storage_key.starts_with(&shard) {
                    return Err(StoreError::InvalidRecord {
                        id: None,
                        reason: InvalidRecordReason::ShardMismatch,
                    });
                }
                let file =
                    layout::StoreDirectory::try_open_regular_in(&shard_directory, &file_name)?
                        .ok_or(StoreError::InvalidLayout)?;
                let record = Self::read_record_file(file, scope, None, &storage_key, &shard)?;
                records.push(record);
            }
        }
        Ok(records)
    }

    fn read_record_file(
        file: File,
        expected_scope: &MemoryScope,
        expected_id: Option<&MemoryId>,
        expected_storage_key: &str,
        expected_shard: &str,
    ) -> Result<MemoryRecord, StoreError> {
        if file
            .metadata()
            .map_err(|source| StoreError::io("inspect memory record", source))?
            .len()
            > MAX_CANONICAL_DOCUMENT_BYTES as u64
        {
            return Err(StoreError::InvalidRecord {
                id: expected_id.cloned(),
                reason: InvalidRecordReason::ValidationFailed,
            });
        }
        let mut bytes = Vec::new();
        file.take((MAX_CANONICAL_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| StoreError::io("read memory record", source))?;
        let record = decode_canonical_document(&bytes, expected_id)?.record;
        if expected_id.is_some_and(|id| id != &record.id)
            || layout::record_storage_key(&record.id) != expected_storage_key
        {
            return Err(StoreError::InvalidRecord {
                id: expected_id.cloned().or_else(|| Some(record.id.clone())),
                reason: InvalidRecordReason::IdFilenameMismatch,
            });
        }
        if &record.scope != expected_scope {
            return Err(StoreError::InvalidRecord {
                id: expected_id.cloned().or_else(|| Some(record.id.clone())),
                reason: InvalidRecordReason::ScopePathMismatch,
            });
        }
        if layout::record_shard(&record.id) != expected_shard {
            return Err(StoreError::InvalidRecord {
                id: expected_id.cloned().or_else(|| Some(record.id.clone())),
                reason: InvalidRecordReason::ShardMismatch,
            });
        }
        Ok(record)
    }

    fn validate_ownership(&self) -> Result<(), StoreError> {
        self.root.validate_ambient_identity()?;
        self._lock.validate_ownership(&self.root)
    }
}

fn prepare_initial_metadata(root: &layout::StoreDirectory) -> Result<StoreMetadata, StoreError> {
    let path = Path::new(layout::STORE_METADATA_INIT_FILE);
    if let Some(file) = root.try_open_regular(path, false)? {
        return match read_store_metadata_file(file) {
            Ok((metadata, _)) => {
                let file = root.open_existing_regular(path, true)?;
                layout::StoreDirectory::set_private_file(&file)?;
                Ok(metadata)
            }
            Err(StoreError::InvalidStoreMetadata) => {
                root.remove_regular_file(path)?;
                root.sync_root("sync metadata rollback")?;
                create_initial_metadata(root, path)
            }
            Err(error) => Err(error),
        };
    }
    create_initial_metadata(root, path)
}

fn create_initial_metadata(
    root: &layout::StoreDirectory,
    path: &Path,
) -> Result<StoreMetadata, StoreError> {
    let metadata = StoreMetadata::new()?;
    let bytes = metadata.canonical_bytes()?;
    let mut file = root.create_new_regular(path)?;
    layout::StoreDirectory::set_private_file(&file)?;
    file.write_all(&bytes)
        .map_err(|source| StoreError::io("write initial store metadata", source))?;
    file.sync_all()
        .map_err(|source| StoreError::io("sync initial store metadata", source))?;
    root.sync_root("sync initial metadata directory")?;
    Ok(metadata)
}

fn commit_initial_metadata(root: &layout::StoreDirectory) -> Result<(), StoreError> {
    let initial = Path::new(layout::STORE_METADATA_INIT_FILE);
    let initial_file = root.validate_private_file(initial)?;
    let initial_identity = layout::FileIdentity::from_file(&initial_file)?;
    let committed = Path::new(layout::STORE_METADATA_FILE);
    if root.regular_file_exists(committed)? {
        return Err(StoreError::AlreadyInitialized);
    }
    root.rename(initial, committed)?;
    if !root.file_identity_matches(committed, initial_identity)? {
        if !root.regular_file_exists(initial)? {
            let _ = root.rename(committed, initial);
        }
        return Err(StoreError::UnsafePath);
    }
    root.sync_root("sync committed store metadata")
}

fn read_store_metadata(
    root: &layout::StoreDirectory,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let file = root
        .try_open_regular(Path::new(layout::STORE_METADATA_FILE), false)?
        .ok_or(StoreError::NotInitialized)?;
    layout::StoreDirectory::validate_private_open_file(&file)?;
    read_store_metadata_file(file)
}

fn read_store_metadata_file(file: File) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    if file
        .metadata()
        .map_err(|source| StoreError::io("inspect store metadata", source))?
        .len()
        > MAX_STORE_METADATA_BYTES as u64
    {
        return Err(StoreError::InvalidStoreMetadata);
    }
    let mut bytes = Vec::new();
    file.take((MAX_STORE_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read store metadata", source))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidStoreMetadata)?;
    let format = value
        .as_object()
        .and_then(|object| object.get("formatVersion"))
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::InvalidStoreMetadata)?;
    if format != crate::STORE_FORMAT_VERSION {
        return Err(StoreError::UnsupportedStoreFormat {
            found: safe_format_diagnostic(format),
        });
    }
    let metadata: StoreMetadata =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidStoreMetadata)?;
    if metadata.format_version != crate::STORE_FORMAT_VERSION
        || metadata.canonical_bytes()? != bytes
    {
        return Err(StoreError::InvalidStoreMetadata);
    }
    Ok((metadata, bytes))
}

fn safe_format_diagnostic(format: &str) -> String {
    if format.starts_with("jiandu.store/")
        && format.len() <= 64
        && format.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        format.to_owned()
    } else {
        "<invalid>".to_owned()
    }
}

fn valid_shard_name(shard: &str) -> bool {
    shard.len() == 2
        && shard
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn matches_filters(record: &MemoryRecord, request: &MemoryListRequest) -> bool {
    (request.types.is_empty() || request.types.contains(&record.memory_type))
        && (request.statuses.is_empty() || request.statuses.contains(&record.status))
        && request
            .tags
            .iter()
            .all(|requested| record.tags.contains(requested))
        && request.updated_after.as_ref().is_none_or(|watermark| {
            timestamp_nanos(&record.updated_at) > timestamp_nanos(watermark)
        })
}

fn sort_records(records: &mut [MemoryRecord], sort: ListSort) {
    records.sort_by(|left, right| {
        let primary = match sort {
            ListSort::UpdatedAtDesc => {
                timestamp_nanos(&right.updated_at).cmp(&timestamp_nanos(&left.updated_at))
            }
            ListSort::UpdatedAtAsc => {
                timestamp_nanos(&left.updated_at).cmp(&timestamp_nanos(&right.updated_at))
            }
            ListSort::CreatedAtDesc => {
                timestamp_nanos(&right.created_at).cmp(&timestamp_nanos(&left.created_at))
            }
            ListSort::CreatedAtAsc => {
                timestamp_nanos(&left.created_at).cmp(&timestamp_nanos(&right.created_at))
            }
            ListSort::IdAsc => Ordering::Equal,
        };
        primary.then_with(|| left.id.cmp(&right.id))
    });
}

fn timestamp_nanos(timestamp: &Timestamp) -> i128 {
    OffsetDateTime::parse(timestamp.as_str(), &Rfc3339)
        .map_or(i128::MIN, |value| value.unix_timestamp_nanos())
}

fn summary_from_record(record: &MemoryRecord) -> MemorySummary {
    MemorySummary {
        id: record.id.clone(),
        revision: record.revision,
        etag: record.etag.clone(),
        scope: record.scope.clone(),
        memory_type: record.memory_type,
        status: record.status,
        title: record.title.clone(),
        summary: record.summary.clone(),
        tags: record.tags.clone(),
        updated_at: record.updated_at.clone(),
    }
}
