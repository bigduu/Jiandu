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
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
    data_dir: PathBuf,
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
        let data_dir = layout::normalize_data_dir(data_dir.as_ref(), true)?;
        let metadata_path = layout::safe_join(&data_dir, Path::new(layout::STORE_METADATA_FILE))?;
        if layout::file_exists(&data_dir, &metadata_path)? {
            return Err(StoreError::AlreadyInitialized);
        }
        layout::validate_initialization_state(&data_dir)?;

        let mut lock = crate::lock::StoreLock::acquire(&data_dir, true)?;
        lock.validate_initialization_marker()?;
        layout::harden_data_directory(&data_dir)?;
        lock.harden_permissions()?;
        lock.publish_owner(&owner)?;
        if layout::file_exists(&data_dir, &metadata_path)? {
            return Err(StoreError::AlreadyInitialized);
        }
        layout::validate_initialization_state(&data_dir)?;

        let metadata = prepare_initial_metadata(&data_dir)?;
        layout::create_layout(&data_dir)?;
        commit_initial_metadata(&data_dir)?;
        Ok(Self {
            data_dir,
            metadata,
            _lock: lock,
        })
    }

    /// Open a supported store without touching canonical records.
    ///
    /// The format is inspected before opening or updating `LOCK`, so a future
    /// store format fails closed without mutating any entry in the directory.
    pub fn open(data_dir: impl AsRef<Path>, owner: crate::LockOwner) -> Result<Self, StoreError> {
        let data_dir = layout::normalize_data_dir(data_dir.as_ref(), false)?;
        layout::validate_private_data_directory(&data_dir)?;
        let (metadata, original_metadata_bytes) = read_store_metadata(&data_dir)?;
        layout::validate_layout(&data_dir)?;
        let lock_path = layout::safe_join(&data_dir, Path::new(layout::STORE_LOCK_FILE))?;
        if !layout::file_exists(&data_dir, &lock_path)? {
            return Err(StoreError::InvalidLayout);
        }
        layout::ensure_private_file(&data_dir, &lock_path)?;

        let mut lock = crate::lock::StoreLock::acquire(&data_dir, false)?;
        let (locked_metadata, locked_metadata_bytes) = read_store_metadata(&data_dir)?;
        if locked_metadata != metadata || locked_metadata_bytes != original_metadata_bytes {
            return Err(StoreError::InvalidStoreMetadata);
        }
        lock.publish_owner(&owner)?;
        Ok(Self {
            data_dir,
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
        let mut candidate = None;
        for scope in authorized.all_scopes() {
            let path = layout::record_path(&self.data_dir, &scope, id)?;
            if layout::file_exists(&self.data_dir, &path)? {
                if candidate.is_some() {
                    return Err(StoreError::DuplicateMemoryId { id: id.clone() });
                }
                candidate = Some((scope, path));
            }
        }
        let (scope, path) = candidate.ok_or(StoreError::NotFound)?;
        let record = self.read_record_at(
            &path,
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
        let path = layout::record_path(&self.data_dir, scope, id)?;
        if !layout::file_exists(&self.data_dir, &path)? {
            return Err(StoreError::NotFound);
        }
        match self.read_record_at(
            &path,
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
        let destination = layout::safe_join(
            &self.data_dir,
            &PathBuf::from(layout::QUARANTINE_DIR).join(format!(
                "{}.{}.md",
                layout::record_storage_key(id),
                quarantine_token
            )),
        )?;
        layout::ensure_regular_file_or_missing(&self.data_dir, &destination)?;
        if layout::file_exists(&self.data_dir, &destination)? {
            return Err(StoreError::InvalidLayout);
        }
        let source_directory = path.parent().ok_or(StoreError::UnsafePath)?.to_path_buf();
        fs::rename(&path, &destination)
            .map_err(|source| StoreError::io("quarantine invalid record", source))?;
        sync_directory(
            &layout::safe_join(&self.data_dir, Path::new(layout::QUARANTINE_DIR))?,
            "sync quarantine directory",
        )?;
        // Persist the destination entry before the source removal. A crash
        // between directory fsyncs can then leave a duplicate, not lose the
        // only copy of an invalid operator-managed record.
        sync_directory(&source_directory, "sync source shard directory")?;
        Ok(QuarantineReceipt {
            memory_id: id.clone(),
            quarantine_token,
        })
    }

    fn scan_scope(&self, scope: &MemoryScope) -> Result<Vec<MemoryRecord>, StoreError> {
        let scope_dir = layout::scope_directory(&self.data_dir, scope)?;
        if !layout::directory_exists(&self.data_dir, &scope_dir)? {
            return Ok(Vec::new());
        }
        layout::ensure_directory(&self.data_dir, &scope_dir)?;

        let mut records = Vec::new();
        let shard_entries = fs::read_dir(&scope_dir)
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
            let shard_path = shard_entry.path();
            layout::ensure_directory(&self.data_dir, &shard_path)?;
            let record_entries = fs::read_dir(&shard_path)
                .map_err(|source| StoreError::io("list shard records", source))?;
            for record_entry in record_entries {
                let record_entry = record_entry
                    .map_err(|source| StoreError::io("read shard record entry", source))?;
                let storage_key = layout::validate_record_entry_name(&record_entry.file_name())?;
                if !storage_key.starts_with(&shard) {
                    return Err(StoreError::InvalidRecord {
                        id: None,
                        reason: InvalidRecordReason::ShardMismatch,
                    });
                }
                let record =
                    self.read_record_at(&record_entry.path(), scope, None, &storage_key, &shard)?;
                records.push(record);
            }
        }
        Ok(records)
    }

    fn read_record_at(
        &self,
        path: &Path,
        expected_scope: &MemoryScope,
        expected_id: Option<&MemoryId>,
        expected_storage_key: &str,
        expected_shard: &str,
    ) -> Result<MemoryRecord, StoreError> {
        layout::ensure_regular_file(&self.data_dir, path)?;
        let file =
            File::open(path).map_err(|source| StoreError::io("open memory record", source))?;
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
}

fn prepare_initial_metadata(root: &Path) -> Result<StoreMetadata, StoreError> {
    let path = layout::safe_join(root, Path::new(layout::STORE_METADATA_INIT_FILE))?;
    if layout::file_exists(root, &path)? {
        return match read_initial_store_metadata(root, &path) {
            Ok((metadata, _)) => {
                layout::set_private_file_permissions(&path)?;
                Ok(metadata)
            }
            Err(StoreError::InvalidStoreMetadata) => {
                fs::remove_file(&path)
                    .map_err(|source| StoreError::io("roll back partial store metadata", source))?;
                sync_directory(root, "sync metadata rollback")?;
                create_initial_metadata(root, &path)
            }
            Err(error) => Err(error),
        };
    }
    create_initial_metadata(root, &path)
}

fn create_initial_metadata(root: &Path, path: &Path) -> Result<StoreMetadata, StoreError> {
    layout::ensure_regular_file_or_missing(root, path)?;
    let metadata = StoreMetadata::new()?;
    let bytes = metadata.canonical_bytes()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::InvalidStoreMetadata
            } else {
                StoreError::io("create initial store metadata", source)
            }
        })?;
    layout::set_private_file_permissions(path)?;
    file.write_all(&bytes)
        .map_err(|source| StoreError::io("write initial store metadata", source))?;
    file.sync_all()
        .map_err(|source| StoreError::io("sync initial store metadata", source))?;
    sync_directory(root, "sync initial metadata directory")?;
    Ok(metadata)
}

fn commit_initial_metadata(root: &Path) -> Result<(), StoreError> {
    let initial = layout::safe_join(root, Path::new(layout::STORE_METADATA_INIT_FILE))?;
    layout::ensure_private_file(root, &initial)?;
    let committed = layout::safe_join(root, Path::new(layout::STORE_METADATA_FILE))?;
    layout::ensure_regular_file_or_missing(root, &committed)?;
    if layout::file_exists(root, &committed)? {
        return Err(StoreError::AlreadyInitialized);
    }
    fs::rename(&initial, &committed)
        .map_err(|source| StoreError::io("commit store metadata", source))?;
    sync_directory(root, "sync committed store metadata")
}

fn read_store_metadata(root: &Path) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let path = layout::safe_join(root, Path::new(layout::STORE_METADATA_FILE))?;
    if !layout::file_exists(root, &path)? {
        return Err(StoreError::NotInitialized);
    }
    layout::ensure_private_file(root, &path)?;
    read_store_metadata_file(&path)
}

fn read_initial_store_metadata(
    root: &Path,
    path: &Path,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    if !layout::file_exists(root, path)? {
        return Err(StoreError::NotInitialized);
    }
    layout::ensure_regular_file(root, path)?;
    read_store_metadata_file(path)
}

fn read_store_metadata_file(path: &Path) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let file = File::open(path).map_err(|source| StoreError::io("open store metadata", source))?;
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

fn sync_directory(path: &Path, operation: &'static str) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| StoreError::io(operation, source))
}
