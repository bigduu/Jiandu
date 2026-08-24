//! Store initialization, opening, authorization, and read APIs.

use crate::document::{MAX_CANONICAL_DOCUMENT_BYTES, decode_canonical_document};
use crate::layout;
use crate::{InvalidRecordReason, StoreError, StoreId, StoreMetadata};
use jiandu_core::{
    ListSort, MemoryId, MemoryListRequest, MemoryListResult, MemoryRecord, MemoryScope,
    MemorySummary, PrincipalId, ProjectId, ScopeSelector, SessionId, StoreRevision, Timestamp,
    TrustedRequestContext, Validate,
};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
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

    /// Resolve an exact scope into a capability that mutation APIs accept.
    /// No model-selected Principal identity can be introduced here.
    #[must_use]
    pub fn authorize_exact(&self, scope: &MemoryScope) -> Option<AuthorizedScope> {
        let allowed = match scope {
            MemoryScope::Principal { principal_id } => principal_id == &self.principal_id,
            MemoryScope::Project { project_id } => self.project_ids.contains(project_id),
            MemoryScope::Session { session_id } => self.session_ids.contains(session_id),
            MemoryScope::InstanceGlobal {} => self.instance_global,
        };
        allowed.then(|| AuthorizedScope(scope.clone()))
    }

    /// Authenticate and authorize one exact mutation scope before any private
    /// receipt lookup can occur.
    pub fn authorize_mutation(
        &self,
        context: &TrustedRequestContext,
        scope: &MemoryScope,
        operation: crate::MutationOperation,
    ) -> Result<AuthorizedMutation, StoreError> {
        context
            .validate()
            .map_err(|_| StoreError::Unauthenticated)?;
        if context.principal_id != self.principal_id {
            return Err(StoreError::Forbidden);
        }
        let exact = self.authorize_exact(scope).ok_or(StoreError::Forbidden)?;
        let required = operation.required_grant(scope);
        if !context
            .grants
            .iter()
            .any(|grant| grant.as_str() == required)
        {
            return Err(StoreError::Forbidden);
        }
        Ok(AuthorizedMutation {
            principal_id: context.principal_id.clone(),
            scope: exact.0,
            operation,
        })
    }

    /// Authenticate and authorize a read-only administrative lifecycle plan
    /// for one exact scope. The returned capability cannot execute a restore
    /// or purge and cannot be constructed from model-visible input.
    pub fn authorize_admin_plan(
        &self,
        context: &TrustedRequestContext,
        scope: &MemoryScope,
        action: crate::AdminAction,
    ) -> Result<AuthorizedAdmin, StoreError> {
        context
            .validate()
            .map_err(|_| StoreError::Unauthenticated)?;
        if context.principal_id != self.principal_id {
            return Err(StoreError::Forbidden);
        }
        let exact = self.authorize_exact(scope).ok_or(StoreError::Forbidden)?;
        if !context
            .grants
            .iter()
            .any(|grant| grant.as_str() == action.required_grant())
        {
            return Err(StoreError::Forbidden);
        }
        Ok(AuthorizedAdmin {
            principal_id: context.principal_id.clone(),
            scope: exact.0,
            action,
        })
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

/// Host-authorized exact mutation scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedScope(MemoryScope);

impl AuthorizedScope {
    #[must_use]
    pub fn as_scope(&self) -> &MemoryScope {
        &self.0
    }
}

/// Fresh authenticated and operation-authorized exact mutation capability.
/// Private fields prevent a model-visible command from selecting a principal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedMutation {
    pub(crate) principal_id: PrincipalId,
    pub(crate) scope: MemoryScope,
    pub(crate) operation: crate::MutationOperation,
}

impl AuthorizedMutation {
    #[must_use]
    pub fn as_scope(&self) -> &MemoryScope {
        &self.scope
    }

    #[must_use]
    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    #[must_use]
    pub const fn operation(&self) -> crate::MutationOperation {
        self.operation
    }
}

/// Fresh exact-scope administrative capability for deterministic dry-runs.
/// It carries no execution authority and has private fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedAdmin {
    principal_id: PrincipalId,
    scope: MemoryScope,
    action: crate::AdminAction,
}

impl AuthorizedAdmin {
    #[must_use]
    pub fn as_scope(&self) -> &MemoryScope {
        &self.scope
    }

    pub(crate) fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    #[must_use]
    pub const fn action(&self) -> crate::AdminAction {
        self.action
    }
}

/// One result observed at an authoritative store watermark.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreRead<T> {
    pub store_revision: StoreRevision,
    pub result: T,
}

pub type StoreWatermark = StoreRevision;

/// Construction options used by deterministic fault-injection tests.
/// Production callers should use `Default` or the shorter `open`/`initialize`
/// constructors.
#[derive(Clone, Default)]
pub struct StoreOptions {
    pub(crate) failpoints: crate::failpoint::Failpoints,
    pub(crate) forced_unsupported_durability: Option<&'static str>,
}

impl StoreOptions {
    #[must_use]
    pub fn with_failpoint_injector(injector: Arc<dyn crate::PersistenceFailpointInjector>) -> Self {
        Self {
            failpoints: crate::failpoint::Failpoints::new(injector),
            forced_unsupported_durability: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_forced_unsupported_durability(capability: &'static str) -> Self {
        Self {
            failpoints: crate::failpoint::Failpoints::default(),
            forced_unsupported_durability: Some(capability),
        }
    }
}

/// Path-free receipt for an explicit operator quarantine action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineReceipt {
    pub memory_id: MemoryId,
    pub quarantine_token: String,
}

/// Exclusively owned handle to one supported canonical store.
pub struct CanonicalStore {
    pub(crate) root: layout::StoreDirectory,
    pub(crate) metadata: StoreMetadata,
    pub(crate) lock: crate::lock::StoreLock,
    pub(crate) failpoints: crate::failpoint::Failpoints,
    pub(crate) poisoned: bool,
    pub(crate) quarantine_receipts: Vec<QuarantineReceipt>,
    pub(crate) forced_unsupported_durability: Option<&'static str>,
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
        Self::initialize_with_options(data_dir, owner, StoreOptions::default())
    }

    pub fn initialize_with_options(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
        options: StoreOptions,
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
        ensure_audit_genesis(&root, &metadata, &options.failpoints, false)?;
        commit_initial_metadata(&root)?;
        crate::idempotency::validate_ledger(&root, &metadata)?;
        root.validate_ambient_identity()?;
        lock.validate_ownership(&root)?;
        crate::durability::probe(
            &root,
            &options.failpoints,
            options.forced_unsupported_durability,
        )?;
        Ok(Self {
            root,
            metadata,
            lock,
            failpoints: options.failpoints,
            poisoned: false,
            quarantine_receipts: Vec::new(),
            forced_unsupported_durability: options.forced_unsupported_durability,
        })
    }

    /// Open a supported store without touching canonical records.
    ///
    /// The format is inspected before opening or updating `LOCK`, so a future
    /// store format fails closed without mutating any entry in the directory.
    pub fn open(data_dir: impl AsRef<Path>, owner: crate::LockOwner) -> Result<Self, StoreError> {
        Self::open_with_options(data_dir, owner, StoreOptions::default())
    }

    pub fn open_with_options(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        let root = layout::StoreDirectory::open(data_dir.as_ref(), false)?;
        root.validate_private_root()?;
        let (metadata, original_metadata_bytes) = read_store_metadata(&root)?;
        layout::validate_layout(&root)?;
        validate_audit_genesis(&root, &metadata)?;
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
        if root.regular_file_exists(Path::new(layout::STORE_METADATA_MIGRATION_FILE))?
            || root
                .regular_file_exists(Path::new(layout::PREVIOUS_STORE_METADATA_MIGRATION_FILE))?
        {
            return Err(StoreError::InvalidStoreMetadata);
        }
        layout::ensure_quarantine_receipt_layout(&root, &options.failpoints)?;
        let recovered = crate::recovery::recover(&root, locked_metadata, &options.failpoints)?;
        validate_audit_genesis(&root, &recovered.metadata)?;
        crate::idempotency::validate_ledger(&root, &recovered.metadata)?;
        crate::durability::probe(
            &root,
            &options.failpoints,
            options.forced_unsupported_durability,
        )?;
        Ok(Self {
            root,
            metadata: recovered.metadata,
            lock,
            failpoints: options.failpoints,
            poisoned: false,
            quarantine_receipts: recovered.quarantine_receipts,
            forced_unsupported_durability: options.forced_unsupported_durability,
        })
    }

    /// Explicitly migrate a locked v1alpha1 store through the historical
    /// receipt/audit layout into the current tombstone-gated format. An older
    /// writer rejects the final capability marker.
    pub fn migrate_v1alpha1(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
    ) -> Result<Self, StoreError> {
        Self::migrate_v1alpha1_with_options(data_dir, owner, StoreOptions::default())
    }

    pub fn migrate_v1alpha1_with_options(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        let root = layout::StoreDirectory::open(data_dir.as_ref(), false)?;
        root.validate_private_root()?;
        let (metadata, original_metadata_bytes) = read_legacy_store_metadata(&root)?;
        layout::validate_legacy_layout(&root)?;
        if !root.regular_file_exists(Path::new(layout::STORE_LOCK_FILE))? {
            return Err(StoreError::InvalidLayout);
        }
        let _lock_file = root.validate_private_file(Path::new(layout::STORE_LOCK_FILE))?;

        let mut lock = crate::lock::StoreLock::acquire(&root, false)?;
        let (locked_metadata, locked_metadata_bytes) = read_legacy_store_metadata(&root)?;
        if locked_metadata != metadata || locked_metadata_bytes != original_metadata_bytes {
            return Err(StoreError::InvalidStoreMetadata);
        }
        root.validate_ambient_identity()?;
        lock.publish_owner(&root, &owner)?;

        // A pre-v1alpha3 binary used this exact private staging name while
        // upgrading a v1 store. Validate and remove only a matching staged v2
        // marker before legacy WAL recovery; arbitrary or linked bytes fail
        // closed instead of being swept as migration debris.
        cleanup_previous_migration_metadata(&root, &locked_metadata, &options.failpoints)?;

        // Legacy recovery must run while the legacy format marker is still
        // authoritative. Only after it removes the old WAL do we create v2
        // namespaces or publish the capability gate.
        layout::ensure_quarantine_receipt_layout(&root, &options.failpoints)?;
        let recovered = crate::recovery::recover(&root, locked_metadata, &options.failpoints)?;
        if recovered.metadata.format_version != crate::metadata::LEGACY_STORE_FORMAT_VERSION {
            return Err(StoreError::InvalidStoreMetadata);
        }

        layout::ensure_v2_layout(&root)?;
        layout::ensure_v3_layout(&root)?;
        options
            .failpoints
            .check(crate::PersistenceBoundary::MigrationLayoutSynced)?;
        let target_metadata = recovered
            .metadata
            .clone()
            .upgraded_to_current(crate::metadata::LEGACY_STORE_FORMAT_VERSION)?;
        ensure_audit_genesis(&root, &target_metadata, &options.failpoints, true)?;
        publish_migrated_metadata(&root, &target_metadata, &options.failpoints)?;
        layout::validate_layout(&root)?;
        validate_audit_genesis(&root, &target_metadata)?;
        crate::idempotency::validate_ledger(&root, &target_metadata)?;
        crate::durability::probe(
            &root,
            &options.failpoints,
            options.forced_unsupported_durability,
        )?;
        Ok(Self {
            root,
            metadata: target_metadata,
            lock,
            failpoints: options.failpoints,
            poisoned: false,
            quarantine_receipts: recovered.quarantine_receipts,
            forced_unsupported_durability: options.forced_unsupported_durability,
        })
    }

    /// Explicitly migrate a root-locked v1alpha2 store to the v1alpha3
    /// tombstone capability. Any active v1alpha2 WAL is recovered and the
    /// complete historical ledger is validated before a v3 directory or
    /// metadata marker is published.
    pub fn migrate_v1alpha2(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
    ) -> Result<Self, StoreError> {
        Self::migrate_v1alpha2_with_options(data_dir, owner, StoreOptions::default())
    }

    pub fn migrate_v1alpha2_with_options(
        data_dir: impl AsRef<Path>,
        owner: crate::LockOwner,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        let root = layout::StoreDirectory::open(data_dir.as_ref(), false)?;
        root.validate_private_root()?;
        let (metadata, original_metadata_bytes) = read_previous_store_metadata(&root)?;
        layout::validate_v2_layout(&root)?;
        if !root.regular_file_exists(Path::new(layout::STORE_LOCK_FILE))? {
            return Err(StoreError::InvalidLayout);
        }
        let _lock_file = root.validate_private_file(Path::new(layout::STORE_LOCK_FILE))?;

        let mut lock = crate::lock::StoreLock::acquire(&root, false)?;
        let (locked_metadata, locked_metadata_bytes) = read_previous_store_metadata(&root)?;
        if locked_metadata != metadata || locked_metadata_bytes != original_metadata_bytes {
            return Err(StoreError::InvalidStoreMetadata);
        }
        root.validate_ambient_identity()?;
        lock.publish_owner(&root, &owner)?;

        layout::ensure_quarantine_receipt_layout(&root, &options.failpoints)?;
        let recovered = crate::recovery::recover(&root, locked_metadata, &options.failpoints)?;
        if recovered.metadata.format_version != crate::metadata::PREVIOUS_STORE_FORMAT_VERSION {
            return Err(StoreError::InvalidStoreMetadata);
        }
        validate_audit_genesis(&root, &recovered.metadata)?;
        crate::idempotency::validate_ledger(&root, &recovered.metadata)?;

        layout::ensure_v3_layout(&root)?;
        options
            .failpoints
            .check(crate::PersistenceBoundary::MigrationLayoutSynced)?;
        let target_metadata = recovered
            .metadata
            .clone()
            .upgraded_to_current(crate::metadata::PREVIOUS_STORE_FORMAT_VERSION)?;
        publish_migrated_metadata(&root, &target_metadata, &options.failpoints)?;
        layout::validate_layout(&root)?;
        validate_audit_genesis(&root, &target_metadata)?;
        crate::idempotency::validate_ledger(&root, &target_metadata)?;
        crate::durability::probe(
            &root,
            &options.failpoints,
            options.forced_unsupported_durability,
        )?;
        Ok(Self {
            root,
            metadata: target_metadata,
            lock,
            failpoints: options.failpoints,
            poisoned: false,
            quarantine_receipts: recovered.quarantine_receipts,
            forced_unsupported_durability: options.forced_unsupported_durability,
        })
    }

    #[must_use]
    pub fn store_id(&self) -> &StoreId {
        &self.metadata.store_id
    }

    /// Return the authoritative in-memory watermark only while this handle is
    /// still safe to serve. A post-boundary failure requires reopen/recovery
    /// instead of exposing a potentially stale value.
    pub fn watermark(&self) -> Result<StoreWatermark, StoreError> {
        self.validate_ownership()?;
        Ok(self.metadata.store_revision)
    }

    /// Re-run the same path-free filesystem capability probe required before
    /// startup readiness.
    pub fn doctor(&self) -> Result<crate::StoreDoctorReport, StoreError> {
        self.validate_ownership()?;
        crate::durability::probe(
            &self.root,
            &self.failpoints,
            self.forced_unsupported_durability,
        )
    }

    /// Produce an all-or-error, non-executing administrative plan for a
    /// bounded explicit set of tombstoned IDs in one exact authorized scope.
    /// Input duplicates are invalid and accepted targets are sorted by opaque
    /// ID.
    pub fn plan_admin_action(
        &self,
        authorization: &AuthorizedAdmin,
        memory_ids: &[MemoryId],
    ) -> Result<crate::AdminActionPlan, StoreError> {
        const MAX_TARGETS: usize = 100;

        self.validate_ownership()?;
        if memory_ids.is_empty() || memory_ids.len() > MAX_TARGETS {
            return Err(StoreError::InvalidRequest);
        }
        let unique: BTreeSet<_> = memory_ids.iter().cloned().collect();
        if unique.len() != memory_ids.len() {
            return Err(StoreError::InvalidRequest);
        }
        let mut targets = Vec::with_capacity(unique.len());
        let mut tombstones = Vec::with_capacity(unique.len());
        for memory_id in unique {
            let tombstone = crate::tombstone::read_exact(
                &self.root,
                &self.metadata.store_id,
                authorization.as_scope(),
                &memory_id,
            )?
            .ok_or(StoreError::NotFound)?;
            targets.push(crate::AdminPlanTarget {
                memory_id: tombstone.memory_id.clone(),
                scope: tombstone.scope.clone(),
                revision: tombstone.revision,
                etag: tombstone.etag.clone(),
            });
            tombstones.push(tombstone);
        }
        let store_revision = self.watermark()?;
        let confirmation_digest = crate::tombstone::confirmation_digest(
            &self.metadata.store_id,
            authorization.principal_id(),
            authorization.action(),
            authorization.as_scope(),
            store_revision,
            &tombstones,
        )?;
        Ok(crate::AdminActionPlan {
            action: authorization.action(),
            count: targets.len(),
            targets,
            store_revision,
            confirmation_digest,
        })
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
        if crate::tombstone::id_exists_anywhere(&self.root, id)? {
            return Err(StoreError::NotFound);
        }
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
            store_revision: self.watermark()?,
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
                self.watermark()?,
                &fingerprint,
            )?,
            None => 0,
        };

        let mut seen = HashSet::new();
        let mut records = Vec::new();
        let tombstoned_storage_keys = crate::tombstone::storage_keys(&self.root)?;
        for scope in &selected_scopes {
            for record in self.scan_scope(scope, &tombstoned_storage_keys)? {
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
                self.watermark()?,
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
            store_revision: self.watermark()?,
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
        let source_digest = crate::transaction::raw_file_digest(
            file.try_clone()
                .map_err(|source| StoreError::io("clone quarantine source handle", source))?,
        )?;
        let manifest = crate::transaction::TransactionManifest::for_quarantine(
            self.metadata.store_id.clone(),
            crate::transaction::QuarantineTransaction {
                memory_id: id.clone(),
                scope: scope.clone(),
                quarantine_token: quarantine_token.clone(),
                source_digest,
            },
        )?;
        let destination = match &manifest.intent {
            crate::transaction::TransactionIntent::Quarantine(quarantine) => {
                crate::transaction::quarantine_relative(quarantine)?
            }
            crate::transaction::TransactionIntent::Record(_)
            | crate::transaction::TransactionIntent::Forget(_) => {
                return Err(StoreError::InvalidTransaction);
            }
        };
        let destination_name = destination
            .file_name()
            .ok_or(StoreError::InvalidTransaction)?;
        let quarantine_directory = self
            .root
            .open_directory(Path::new(layout::QUARANTINE_DIR))?;
        if layout::StoreDirectory::try_open_regular_in(&quarantine_directory, destination_name)?
            .is_some()
        {
            return Err(StoreError::InvalidLayout);
        }

        self.poisoned = true;
        crate::transaction::persist_manifest(&self.root, &manifest, &self.failpoints)?;
        layout::StoreDirectory::rename_between(
            &source_directory,
            source_name,
            &quarantine_directory,
            destination_name,
        )
        .map_err(|error| match error {
            StoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                StoreError::UnsafePath
            }
            other => other,
        })?;
        self.failpoints
            .check(crate::PersistenceBoundary::QuarantineRenamed)?;
        let moved =
            layout::StoreDirectory::try_open_regular_in(&quarantine_directory, destination_name);
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
                layout::StoreDirectory::rename_between(
                    &quarantine_directory,
                    destination_name,
                    &source_directory,
                    source_name,
                )?;
            }
            let source_restored =
                layout::StoreDirectory::try_open_regular_in(&source_directory, source_name)?
                    .is_some();
            let destination_removed = layout::StoreDirectory::try_open_regular_in(
                &quarantine_directory,
                destination_name,
            )?
            .is_none();
            if !source_restored || !destination_removed {
                return Err(StoreError::UnsafePath);
            }
            layout::StoreDirectory::sync_open_directory(
                &quarantine_directory,
                "sync quarantine race rollback",
            )?;
            layout::StoreDirectory::sync_open_directory(
                &source_directory,
                "sync quarantine source race rollback",
            )?;
            crate::transaction::remove_manifest(
                &self.root,
                &manifest,
                &crate::failpoint::Failpoints::default(),
            )?;
            self.poisoned = false;
            return Err(StoreError::UnsafePath);
        }
        // Keep the verified source handle alive until identity has been
        // checked at the destination.
        drop(file);
        layout::StoreDirectory::sync_open_directory(
            &quarantine_directory,
            "sync quarantine directory",
        )?;
        self.failpoints
            .check(crate::PersistenceBoundary::QuarantineDirectorySynced)?;
        // Persist the destination entry before the source removal. A crash
        // between directory fsyncs can then leave a duplicate, not lose the
        // only copy of an invalid operator-managed record.
        layout::StoreDirectory::sync_open_directory(
            &source_directory,
            "sync source shard directory",
        )?;
        self.failpoints
            .check(crate::PersistenceBoundary::QuarantineSourceDirectorySynced)?;
        let durable = crate::transaction::DurableQuarantineReceipt::from_manifest(&manifest)?;
        crate::transaction::persist_quarantine_receipt(&self.root, &durable, &self.failpoints)?;
        crate::transaction::remove_manifest(&self.root, &manifest, &self.failpoints)?;
        let receipt = QuarantineReceipt {
            memory_id: id.clone(),
            quarantine_token,
        };
        self.quarantine_receipts.push(receipt.clone());
        self.quarantine_receipts.sort_by(|left, right| {
            left.memory_id
                .cmp(&right.memory_id)
                .then_with(|| left.quarantine_token.cmp(&right.quarantine_token))
        });
        self.poisoned = false;
        Ok(receipt)
    }

    /// Durable, path-free quarantine receipts awaiting operator acknowledgement.
    pub fn pending_quarantine_receipts(&self) -> Result<&[QuarantineReceipt], StoreError> {
        self.validate_ownership()?;
        Ok(&self.quarantine_receipts)
    }

    /// Acknowledge a durable quarantine receipt without deleting the
    /// quarantined bytes. This is an operator receipt lifecycle, not the
    /// idempotent mutation receipt contract owned by Issue #5.
    pub fn acknowledge_quarantine_receipt(
        &mut self,
        memory_id: &MemoryId,
        quarantine_token: &str,
    ) -> Result<(), StoreError> {
        self.validate_ownership()?;
        let directory = self
            .root
            .open_directory(Path::new(layout::QUARANTINE_RECEIPTS_DIR))?;
        let entries = directory
            .entries()
            .map_err(|source| StoreError::io("list quarantine receipts", source))?;
        let mut match_path = None;
        for entry in entries {
            let entry =
                entry.map_err(|source| StoreError::io("read quarantine receipt", source))?;
            let name = entry.file_name();
            let Some(transaction_id) = crate::transaction::transaction_id_from_receipt_name(&name)
            else {
                return Err(StoreError::InvalidTransaction);
            };
            let file = layout::StoreDirectory::try_open_regular_in(&directory, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            layout::StoreDirectory::validate_private_open_file(&file)?;
            let receipt = crate::transaction::DurableQuarantineReceipt::decode(
                file,
                &transaction_id,
                &self.metadata.store_id,
            )?;
            if &receipt.memory_id == memory_id && receipt.quarantine_token == quarantine_token {
                if match_path.is_some() {
                    return Err(StoreError::InvalidTransaction);
                }
                match_path = Some(Path::new(layout::QUARANTINE_RECEIPTS_DIR).join(name));
            }
        }
        let path = match_path.ok_or(StoreError::NotFound)?;
        self.poisoned = true;
        self.root.remove_regular_file(&path)?;
        self.failpoints
            .check(crate::PersistenceBoundary::QuarantineReceiptAcknowledgementRemoved)?;
        self.root.sync_directory(
            Path::new(layout::QUARANTINE_RECEIPTS_DIR),
            "sync quarantine receipt acknowledgement",
        )?;
        self.failpoints
            .check(crate::PersistenceBoundary::QuarantineReceiptAcknowledgementDirectorySynced)?;
        self.quarantine_receipts.retain(|receipt| {
            &receipt.memory_id != memory_id || receipt.quarantine_token != quarantine_token
        });
        self.poisoned = false;
        Ok(())
    }

    fn scan_scope(
        &self,
        scope: &MemoryScope,
        tombstoned_storage_keys: &BTreeSet<String>,
    ) -> Result<Vec<MemoryRecord>, StoreError> {
        let scope_relative = layout::scope_relative_directory(scope);
        let Some(scope_directory) = self.root.try_open_directory(&scope_relative)? else {
            return Ok(Vec::new());
        };
        #[cfg(test)]
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
                if crate::transaction::transaction_id_from_erasure_witness_name(&file_name)
                    .is_some()
                {
                    let witness =
                        layout::StoreDirectory::try_open_regular_in(&shard_directory, &file_name)?
                            .ok_or(StoreError::InvalidLayout)?;
                    layout::StoreDirectory::validate_private_open_file(&witness)?;
                    if witness
                        .metadata()
                        .map_err(|source| StoreError::io("inspect forget erasure witness", source))?
                        .len()
                        != 0
                    {
                        return Err(StoreError::InvalidTransaction);
                    }
                    continue;
                }
                let storage_key = layout::validate_record_entry_name(&file_name)?;
                if tombstoned_storage_keys.contains(&storage_key) {
                    continue;
                }
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

    pub(crate) fn read_record_file(
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

    pub(crate) fn validate_ownership(&self) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::RecoveryRequired);
        }
        self.root.validate_ambient_identity()?;
        self.lock.validate_ownership(&self.root)
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

pub(crate) fn read_store_metadata(
    root: &layout::StoreDirectory,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let file = root
        .try_open_regular(Path::new(layout::STORE_METADATA_FILE), false)?
        .ok_or(StoreError::NotInitialized)?;
    layout::StoreDirectory::validate_private_open_file(&file)?;
    read_store_metadata_file_for(file, crate::STORE_FORMAT_VERSION)
}

fn read_legacy_store_metadata(
    root: &layout::StoreDirectory,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let file = root
        .try_open_regular(Path::new(layout::STORE_METADATA_FILE), false)?
        .ok_or(StoreError::NotInitialized)?;
    layout::StoreDirectory::validate_private_open_file(&file)?;
    read_store_metadata_file_for(file, crate::metadata::LEGACY_STORE_FORMAT_VERSION)
}

fn read_previous_store_metadata(
    root: &layout::StoreDirectory,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    let file = root
        .try_open_regular(Path::new(layout::STORE_METADATA_FILE), false)?
        .ok_or(StoreError::NotInitialized)?;
    layout::StoreDirectory::validate_private_open_file(&file)?;
    read_store_metadata_file_for(file, crate::metadata::PREVIOUS_STORE_FORMAT_VERSION)
}

fn read_store_metadata_file(file: File) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
    read_store_metadata_file_for(file, crate::STORE_FORMAT_VERSION)
}

fn read_store_metadata_file_for(
    file: File,
    expected_format: &str,
) -> Result<(StoreMetadata, Vec<u8>), StoreError> {
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
    if format != expected_format {
        return Err(StoreError::UnsupportedStoreFormat {
            found: safe_format_diagnostic(format),
        });
    }
    let metadata: StoreMetadata =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidStoreMetadata)?;
    if metadata.format_version != expected_format
        || (expected_format == crate::metadata::LEGACY_STORE_FORMAT_VERSION
            && metadata.audit_sequence.0 != 0)
        || metadata.canonical_bytes()? != bytes
    {
        return Err(StoreError::InvalidStoreMetadata);
    }
    Ok((metadata, bytes))
}

fn ensure_audit_genesis(
    root: &layout::StoreDirectory,
    metadata: &StoreMetadata,
    failpoints: &crate::failpoint::Failpoints,
    replace_stale: bool,
) -> Result<(), StoreError> {
    let expected =
        crate::idempotency::AuditGenesis::new(metadata.store_id.clone(), metadata.store_revision);
    let target = Path::new(layout::AUDIT_GENESIS_FILE);
    let staged = Path::new(layout::AUDIT_GENESIS_TEMP_FILE);
    if let Some(file) = root.try_open_regular(target, false)? {
        layout::StoreDirectory::validate_private_open_file(&file)?;
        let existing = crate::idempotency::AuditGenesis::decode(file, &metadata.store_id)?;
        if existing == expected {
            if root.remove_regular_file_if_exists(staged)? {
                root.sync_directory(Path::new("audit"), "sync stale genesis temp cleanup")?;
            }
            root.sync_directory(Path::new("audit"), "sync existing audit genesis")?;
            failpoints.check(crate::PersistenceBoundary::MigrationGenesisDirectorySynced)?;
            return Ok(());
        }
        if !replace_stale {
            return Err(StoreError::InvalidStoreMetadata);
        }
        root.remove_regular_file(target)?;
        root.sync_directory(Path::new("audit"), "sync stale audit genesis removal")?;
    }
    if root.remove_regular_file_if_exists(staged)? {
        root.sync_directory(Path::new("audit"), "sync genesis temp rollback")?;
    }
    let bytes = expected.canonical_bytes()?;
    let mut file = root.create_new_regular(staged)?;
    layout::StoreDirectory::set_private_file(&file)?;
    file.write_all(&bytes)
        .map_err(|source| StoreError::io("write staged audit genesis", source))?;
    failpoints.check(crate::PersistenceBoundary::MigrationGenesisTempWritten)?;
    file.sync_all()
        .map_err(|source| StoreError::io("sync staged audit genesis", source))?;
    failpoints.check(crate::PersistenceBoundary::MigrationGenesisTempSynced)?;
    root.sync_directory(Path::new("audit"), "sync staged audit genesis directory")?;
    failpoints.check(crate::PersistenceBoundary::MigrationGenesisTempDirectorySynced)?;
    let identity = layout::FileIdentity::from_file(&file)?;
    drop(file);
    root.rename(staged, target)?;
    if !root.file_identity_matches(target, identity)? {
        return Err(StoreError::UnsafePath);
    }
    failpoints.check(crate::PersistenceBoundary::MigrationGenesisPublished)?;
    root.sync_directory(Path::new("audit"), "sync published audit genesis")?;
    failpoints.check(crate::PersistenceBoundary::MigrationGenesisDirectorySynced)
}

pub(crate) fn validate_audit_genesis(
    root: &layout::StoreDirectory,
    metadata: &StoreMetadata,
) -> Result<(), StoreError> {
    validate_audit_genesis_inner(root, metadata, &mut |_| Ok(()))
}

pub(crate) fn validate_audit_genesis_bounded(
    root: &layout::StoreDirectory,
    metadata: &StoreMetadata,
    budget: &mut impl crate::idempotency::LedgerScanBudget,
) -> Result<(), StoreError> {
    validate_audit_genesis_inner(root, metadata, &mut |file| {
        let length = file
            .metadata()
            .map_err(|source| StoreError::io("inspect bounded audit genesis", source))?
            .len();
        if budget.consume_bytes(length) {
            Ok(())
        } else {
            Err(StoreError::InvalidRequest)
        }
    })
}

fn validate_audit_genesis_inner(
    root: &layout::StoreDirectory,
    metadata: &StoreMetadata,
    before_decode: &mut impl FnMut(&std::fs::File) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let file = root
        .try_open_regular(Path::new(layout::AUDIT_GENESIS_FILE), false)?
        .ok_or(StoreError::InvalidStoreMetadata)?;
    before_decode(&file)?;
    layout::StoreDirectory::validate_private_open_file(&file)?;
    let genesis = crate::idempotency::AuditGenesis::decode(file, &metadata.store_id)?;
    if genesis.base_store_revision.0 > metadata.store_revision.0 {
        return Err(StoreError::InvalidStoreMetadata);
    }
    Ok(())
}

fn publish_migrated_metadata(
    root: &layout::StoreDirectory,
    metadata: &StoreMetadata,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let staged = Path::new(layout::STORE_METADATA_MIGRATION_FILE);
    if root.remove_regular_file_if_exists(staged)? {
        root.sync_root("sync stale migration metadata rollback")?;
    }
    let bytes = metadata.canonical_bytes()?;
    let mut file = root.create_new_regular(staged)?;
    layout::StoreDirectory::set_private_file(&file)?;
    file.write_all(&bytes)
        .map_err(|source| StoreError::io("write staged migrated store metadata", source))?;
    failpoints.check(crate::PersistenceBoundary::MigrationMetadataTempWritten)?;
    file.sync_all()
        .map_err(|source| StoreError::io("sync staged migrated store metadata", source))?;
    failpoints.check(crate::PersistenceBoundary::MigrationMetadataTempSynced)?;
    root.sync_root("sync staged migrated metadata directory")?;
    failpoints.check(crate::PersistenceBoundary::MigrationMetadataTempDirectorySynced)?;
    let identity = layout::FileIdentity::from_file(&file)?;
    drop(file);
    root.rename(staged, Path::new(layout::STORE_METADATA_FILE))?;
    if !root.file_identity_matches(Path::new(layout::STORE_METADATA_FILE), identity)? {
        return Err(StoreError::UnsafePath);
    }
    failpoints.check(crate::PersistenceBoundary::MigrationMetadataPublished)?;
    root.sync_root("sync published migrated store metadata")?;
    failpoints.check(crate::PersistenceBoundary::MigrationMetadataDirectorySynced)
}

fn cleanup_previous_migration_metadata(
    root: &layout::StoreDirectory,
    legacy_metadata: &StoreMetadata,
    failpoints: &crate::failpoint::Failpoints,
) -> Result<(), StoreError> {
    let relative = Path::new(layout::PREVIOUS_STORE_METADATA_MIGRATION_FILE);
    let Some(file) = root.try_open_regular(relative, false)? else {
        return Ok(());
    };
    layout::StoreDirectory::validate_private_open_file(&file)?;
    if !layout::StoreDirectory::has_single_link(&file)? {
        return Err(StoreError::UnsafePath);
    }
    let (staged, _) =
        read_store_metadata_file_for(file, crate::metadata::PREVIOUS_STORE_FORMAT_VERSION)?;
    if staged.store_id != legacy_metadata.store_id
        || staged.store_revision != legacy_metadata.store_revision
        || staged.audit_sequence != legacy_metadata.audit_sequence
        || staged.created_at != legacy_metadata.created_at
    {
        return Err(StoreError::InvalidStoreMetadata);
    }
    root.remove_regular_file(relative)?;
    failpoints.check(crate::PersistenceBoundary::MigrationPreviousMetadataRemoved)?;
    root.sync_root("sync previous migration metadata rollback")?;
    failpoints.check(crate::PersistenceBoundary::MigrationPreviousMetadataDirectorySynced)
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
