//! Narrow host-owned read backend seam.

use crate::{
    IndexReadHealth, MutationPolicy, MutationPolicyContext, MutationPolicyError,
    MutationPolicyRequest, ReadHealthSnapshot, ReadServiceHealth, StoreReadHealth,
};
use jiandu_core::{
    CreationActor, ForgetMemoryCommand, ForgetMemoryResult, MemoryGetRequest, MemoryId,
    MemoryListRequest, MemoryListResult, MemoryRecord, MemorySearchRequest, MemorySearchResult,
    MutationInvocation, RememberMemoryCommand, RememberMemoryResult, StoreRevision, Timestamp,
    UpdateMemoryCommand, UpdateMemoryResult, Validate,
};
use jiandu_index::{CursorMacKey, IndexError, LexicalIndex};
use jiandu_store::{
    AuthorizedMutationSet, AuthorizedRead, CanonicalStore, FreshRecordMetadata, MutationOperation,
    StoreError, StoreRead,
};
use std::fmt;
use std::sync::{Arc, RwLock};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Path-free backend error mapped to the stable public Jiandu envelope by the
/// MCP adapter.
pub enum ReadBackendError {
    Store(StoreError),
    Index(IndexError),
    Policy(MutationPolicyError),
    /// The canonical watermark changed around one successful index query.
    /// The result must be discarded rather than paired with a mixed revision.
    UnstableSearchSnapshot,
    /// The host's in-process store coordination primitive is unavailable.
    HostUnavailable,
}

impl fmt::Debug for ReadBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => formatter.debug_tuple("Store").field(&error.code()).finish(),
            Self::Index(error) => formatter.debug_tuple("Index").field(&error.code()).finish(),
            Self::Policy(error) => formatter.debug_tuple("Policy").field(error).finish(),
            Self::UnstableSearchSnapshot => formatter.write_str("UnstableSearchSnapshot"),
            Self::HostUnavailable => formatter.write_str("HostUnavailable"),
        }
    }
}

impl fmt::Display for ReadBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "canonical read failed: {:?}", error.code()),
            Self::Index(error) => write!(formatter, "lexical read failed: {:?}", error.code()),
            Self::Policy(error) => write!(formatter, "mutation policy rejected: {error:?}"),
            Self::UnstableSearchSnapshot => formatter.write_str("search snapshot changed"),
            Self::HostUnavailable => formatter.write_str("read host is unavailable"),
        }
    }
}

impl std::error::Error for ReadBackendError {}

impl From<StoreError> for ReadBackendError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<IndexError> for ReadBackendError {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

impl From<MutationPolicyError> for ReadBackendError {
    fn from(error: MutationPolicyError) -> Self {
        Self::Policy(error)
    }
}

/// One committed mutation result paired with its durable transaction-derived
/// correlation. A replay deliberately returns the original operation's
/// correlation rather than the current transport attempt's value.
#[derive(Clone, Debug, PartialEq)]
pub struct MutationBackendCommit<T> {
    pub correlation_id: jiandu_core::CorrelationId,
    pub store_revision: StoreRevision,
    pub result: T,
}

/// One mutation failure paired with the canonical revision observed while the
/// same store write guard was still held. Pre-store validation/authorization
/// failures use revision zero. A poisoned post-WAL handle also uses zero
/// rather than performing a second, racy read.
pub struct MutationBackendError {
    error: ReadBackendError,
    store_revision: StoreRevision,
}

impl MutationBackendError {
    /// Construct a trusted backend failure snapshot. Implementors must capture
    /// the exact observed revision under the same coordination guard,
    /// including a legitimate empty-store revision zero. Zero is also used
    /// when pre-store or poisoned state has no safe canonical snapshot.
    #[must_use]
    pub const fn new(error: ReadBackendError, store_revision: StoreRevision) -> Self {
        Self {
            error,
            store_revision,
        }
    }

    pub(crate) fn into_parts(self) -> (ReadBackendError, StoreRevision) {
        (self.error, self.store_revision)
    }
}

impl fmt::Debug for MutationBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MutationBackendError")
            .field("error", &self.error)
            .field("store_revision", &self.store_revision)
            .finish()
    }
}

/// Host-controlled synchronous read seam. A daemon may implement this over a
/// lock or blocking worker without giving the MCP handler ownership of the
/// mutable canonical store.
pub trait McpReadBackend: Send + Sync + 'static {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError>;

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError>;

    /// Return a search result paired with the exact stable canonical revision
    /// validated around that result.
    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError>;

    /// Return the canonical revision only while the store can safely serve.
    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError>;

    /// Return only the host-approved closed readiness snapshot. Implementors
    /// must not call operator-only diagnostics on behalf of this method.
    fn health(&self) -> ReadServiceHealth;
}

/// Host-controlled mutation seam. Operation-specific authority is minted from
/// trusted connection state before this trait can inspect private receipts.
pub trait McpMutationBackend: Send + Sync + 'static {
    fn remember(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        creation_actor: CreationActor,
        command: &RememberMemoryCommand,
    ) -> Result<MutationBackendCommit<RememberMemoryResult>, MutationBackendError>;

    fn update(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &UpdateMemoryCommand,
    ) -> Result<MutationBackendCommit<UpdateMemoryResult>, MutationBackendError>;

    fn forget(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &ForgetMemoryCommand,
    ) -> Result<MutationBackendCommit<ForgetMemoryResult>, MutationBackendError>;
}

/// Production backend that composes the real canonical store and lexical
/// index while preserving future mutable-store access through one host lock.
/// It intentionally does not call the operator-only index diagnostic API.
pub struct CanonicalReadBackend {
    store: Arc<RwLock<CanonicalStore>>,
    index: LexicalIndex,
    cursor_key: CursorMacKey,
    health: ReadHealthSnapshot,
}

impl fmt::Debug for CanonicalReadBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalReadBackend")
            .field("store", &"[REDACTED]")
            .field("index", &"[REDACTED]")
            .field("cursor_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl CanonicalReadBackend {
    #[must_use]
    pub fn new(
        store: Arc<RwLock<CanonicalStore>>,
        index: LexicalIndex,
        cursor_key: CursorMacKey,
        health: ReadServiceHealth,
    ) -> Self {
        Self {
            store,
            index,
            cursor_key,
            health: ReadHealthSnapshot::new(health),
        }
    }

    /// Return an observer that shares only the closed health value and cannot
    /// retain the canonical store owner.
    #[must_use]
    pub fn health_snapshot(&self) -> ReadHealthSnapshot {
        self.health.clone()
    }

    /// Replace only the pre-sanitized readiness snapshot exposed during MCP
    /// initialization. This does not inspect or mutate canonical/index data.
    pub fn update_health(&self, health: ReadServiceHealth) -> Result<(), ReadBackendError> {
        self.health
            .replace(health)
            .map_err(|()| ReadBackendError::HostUnavailable)
    }

    fn read_store(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, CanonicalStore>, ReadBackendError> {
        self.store
            .read()
            .map_err(|_| ReadBackendError::HostUnavailable)
    }

    fn write_store(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, CanonicalStore>, MutationBackendError> {
        self.store.write().map_err(|_| {
            let _ = self.update_health(ReadServiceHealth::new(
                StoreReadHealth::Degraded,
                IndexReadHealth::Degraded,
            ));
            pre_store_failure(ReadBackendError::HostUnavailable)
        })
    }

    fn mutation_failure(
        &self,
        store: &CanonicalStore,
        error: StoreError,
        policy_error: Option<MutationPolicyError>,
    ) -> MutationBackendError {
        let store_revision = self.mutation_store_revision(store);
        MutationBackendError::new(
            policy_error.map_or_else(|| ReadBackendError::Store(error), ReadBackendError::Policy),
            store_revision,
        )
    }

    fn observed_mutation_failure(
        &self,
        store: &CanonicalStore,
        error: ReadBackendError,
    ) -> MutationBackendError {
        let store_revision = self.mutation_store_revision(store);
        MutationBackendError::new(error, store_revision)
    }

    fn mutation_store_revision(&self, store: &CanonicalStore) -> StoreRevision {
        match store.watermark() {
            Ok(store_revision) => store_revision,
            Err(_) => {
                let _ = self.update_health(ReadServiceHealth::new(
                    StoreReadHealth::Degraded,
                    IndexReadHealth::Degraded,
                ));
                StoreRevision(0)
            }
        }
    }

    fn invalidate_index_after_fresh_commit(
        &self,
        idempotent_replay: bool,
    ) -> Result<(), ReadBackendError> {
        if !idempotent_replay {
            let index = match self.health().index() {
                IndexReadHealth::Ready => IndexReadHealth::Degraded,
                IndexReadHealth::Degraded => IndexReadHealth::Degraded,
                IndexReadHealth::Missing => IndexReadHealth::Missing,
            };
            self.update_health(ReadServiceHealth::new(StoreReadHealth::Ready, index))?;
        }
        Ok(())
    }
}

impl McpReadBackend for CanonicalReadBackend {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        let store = self.read_store()?;
        Ok(authorization.get(&store, &request.memory_id)?)
    }

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        let store = self.read_store()?;
        Ok(authorization.list(&store, request)?)
    }

    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        let store = self.read_store()?;
        let begin = store.watermark()?;
        let query_authorization = authorization.authorize_index_query(request)?;
        let result = self
            .index
            .search(&*store, &query_authorization, request, &self.cursor_key)?;
        let end = store.watermark()?;
        if begin != end {
            return Err(ReadBackendError::UnstableSearchSnapshot);
        }
        Ok((begin, result))
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        Ok(self.read_store()?.watermark()?)
    }

    fn health(&self) -> ReadServiceHealth {
        self.health.current()
    }
}

impl McpMutationBackend for CanonicalReadBackend {
    fn remember(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        creation_actor: CreationActor,
        command: &RememberMemoryCommand,
    ) -> Result<MutationBackendCommit<RememberMemoryResult>, MutationBackendError> {
        command
            .validate()
            .map_err(|_| pre_store_failure(StoreError::InvalidRequest))?;
        if authorization.operation() != MutationOperation::Create {
            return Err(pre_store_failure(StoreError::Forbidden));
        }
        let exact = authorization
            .authorize_selector(&command.scope)
            .map_err(pre_store_failure)?;
        let memory_id = MemoryId::new(format!("mem_{}", Uuid::new_v4().simple()))
            .map_err(|_| pre_store_failure(StoreError::InvalidRequest))?;
        let mut store = self.write_store()?;
        let created_at =
            current_timestamp().map_err(|error| self.observed_mutation_failure(&store, error))?;
        let context = MutationPolicyContext::new(&exact, invocation.correlation_id().clone());
        let mut policy_error = None;
        let commit = store.create_with_invocation_and_admission(
            &exact,
            command,
            FreshRecordMetadata {
                memory_id,
                created_by: creation_actor,
                created_at,
            },
            invocation,
            |target| {
                policy
                    .evaluate(
                        &context,
                        MutationPolicyRequest::Remember { command, target },
                    )
                    .map_err(|error| {
                        policy_error = Some(error);
                        policy_store_error(error)
                    })
            },
        );
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => return Err(self.mutation_failure(&store, error, policy_error)),
        };
        self.invalidate_index_after_fresh_commit(commit.idempotent_replay)
            .map_err(|error| self.observed_mutation_failure(&store, error))?;
        let correlation_id = commit
            .correlation_id()
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        Ok(MutationBackendCommit {
            correlation_id,
            store_revision: commit.store_revision,
            result: RememberMemoryResult {
                record: commit.record,
                idempotent_replay: commit.idempotent_replay,
            },
        })
    }

    fn update(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &UpdateMemoryCommand,
    ) -> Result<MutationBackendCommit<UpdateMemoryResult>, MutationBackendError> {
        command
            .validate()
            .map_err(|_| pre_store_failure(StoreError::InvalidRequest))?;
        if authorization.operation() != MutationOperation::Update {
            return Err(pre_store_failure(StoreError::Forbidden));
        }
        let mut store = self.write_store()?;
        let exact = store
            .resolve_existing_mutation(authorization, &command.memory_id, &command.idempotency_key)
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        let updated_at =
            current_timestamp().map_err(|error| self.observed_mutation_failure(&store, error))?;
        let context = MutationPolicyContext::new(&exact, invocation.correlation_id().clone());
        let mut policy_error = None;
        let commit = store.update_with_invocation_and_admission(
            &exact,
            command,
            updated_at,
            invocation,
            |target| {
                policy
                    .evaluate(&context, MutationPolicyRequest::Update { command, target })
                    .map_err(|error| {
                        policy_error = Some(error);
                        policy_store_error(error)
                    })
            },
        );
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => return Err(self.mutation_failure(&store, error, policy_error)),
        };
        self.invalidate_index_after_fresh_commit(commit.idempotent_replay)
            .map_err(|error| self.observed_mutation_failure(&store, error))?;
        let correlation_id = commit
            .correlation_id()
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        let previous_revision = commit
            .previous_revision
            .ok_or(StoreError::InvalidTransaction)
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        Ok(MutationBackendCommit {
            correlation_id,
            store_revision: commit.store_revision,
            result: UpdateMemoryResult {
                record: commit.record,
                previous_revision,
                idempotent_replay: commit.idempotent_replay,
            },
        })
    }

    fn forget(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &ForgetMemoryCommand,
    ) -> Result<MutationBackendCommit<ForgetMemoryResult>, MutationBackendError> {
        command
            .validate()
            .map_err(|_| pre_store_failure(StoreError::InvalidRequest))?;
        if authorization.operation() != MutationOperation::Forget {
            return Err(pre_store_failure(StoreError::Forbidden));
        }
        let mut store = self.write_store()?;
        let exact = store
            .resolve_existing_mutation(authorization, &command.memory_id, &command.idempotency_key)
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        let forgotten_at =
            current_timestamp().map_err(|error| self.observed_mutation_failure(&store, error))?;
        let context = MutationPolicyContext::new(&exact, invocation.correlation_id().clone());
        let mut policy_error = None;
        let commit = store.forget_with_invocation_and_admission(
            &exact,
            command,
            forgotten_at,
            invocation,
            || {
                policy
                    .evaluate(&context, MutationPolicyRequest::Forget(command))
                    .map_err(|error| {
                        policy_error = Some(error);
                        policy_store_error(error)
                    })
            },
        );
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => return Err(self.mutation_failure(&store, error, policy_error)),
        };
        self.invalidate_index_after_fresh_commit(commit.idempotent_replay)
            .map_err(|error| self.observed_mutation_failure(&store, error))?;
        let correlation_id = commit
            .correlation_id()
            .map_err(|error| self.mutation_failure(&store, error, None))?;
        Ok(MutationBackendCommit {
            correlation_id,
            store_revision: commit.store_revision,
            result: ForgetMemoryResult {
                memory_id: commit.memory_id,
                revision: commit.revision,
                etag: commit.etag,
                forgotten_at: commit.forgotten_at,
                idempotent_replay: commit.idempotent_replay,
            },
        })
    }
}

fn pre_store_failure(error: impl Into<ReadBackendError>) -> MutationBackendError {
    MutationBackendError::new(error.into(), StoreRevision(0))
}

const fn policy_store_error(error: MutationPolicyError) -> StoreError {
    match error {
        MutationPolicyError::InvalidRequest => StoreError::InvalidRequest,
        MutationPolicyError::Forbidden => StoreError::Forbidden,
        MutationPolicyError::Unavailable => StoreError::InvalidTransaction,
    }
}

fn current_timestamp() -> Result<Timestamp, ReadBackendError> {
    let value = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ReadBackendError::HostUnavailable)?;
    Timestamp::new(value).map_err(|_| ReadBackendError::HostUnavailable)
}
