use jiandu_core::{
    ClientId, CreationActor, ErrorEnvelope, ForgetMemoryCommand, ForgetMemoryResult, Grant,
    IdempotencyKey, ListSort, MemoryGetRequest, MemoryListRequest, MemoryListResult, MemoryPatch,
    MemoryRecord, MemoryScope, MemorySearchRequest, MemorySearchResult, MemoryType,
    MutationInvocation, PageLimit, PrincipalId, ProjectId, ProvenanceInput, RememberMemoryCommand,
    RememberMemoryResult, ResultEnvelope, Revision, ScopeSelector, StoreRevision, Tag, Timestamp,
    TrustedRequestContext, UpdateMemoryCommand, UpdateMemoryResult,
};
use jiandu_index::{CursorMacKey, LexicalIndex};
use jiandu_mcp::{
    CanonicalReadBackend, ConfiguredMutationPolicy, IndexReadHealth, JianduReadServer,
    McpMutationBackend, McpReadBackend, MutationBackendCommit, MutationBackendError,
    MutationPolicy, MutationScopeKind, ReadBackendError, ReadServiceHealth, SecretContentPolicy,
    StoreReadHealth,
};
use jiandu_store::{
    AuthorizedMutationSet, AuthorizedRead, AuthorizedScopes, CanonicalStore, LockOwner,
    MutationOperation, PersistenceBoundary, PersistenceFailpointInjector, StoreError, StoreOptions,
    StoreRead,
};
use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceError, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientInfo, ClientRequest, JsonObject,
        ProtocolVersion,
    },
    service::PeerRequestOptions,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

#[derive(Debug, Clone)]
struct V2025Client;

impl ClientHandler for V2025Client {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        info
    }
}

struct MutationFixture {
    root: TempDir,
    store: Arc<RwLock<CanonicalStore>>,
    backend: Arc<CanonicalReadBackend>,
    scopes: AuthorizedScopes,
    principal: PrincipalId,
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotEntry {
    bytes: Option<Vec<u8>>,
    modified: SystemTime,
}

#[derive(Default)]
struct RecordingSecretPolicy {
    calls: AtomicUsize,
    observations: Mutex<Vec<(String, String)>>,
    denied_sentinel: Option<&'static [u8]>,
}

impl RecordingSecretPolicy {
    fn allow() -> Self {
        Self::default()
    }

    fn deny(sentinel: &'static [u8]) -> Self {
        Self {
            denied_sentinel: Some(sentinel),
            ..Self::default()
        }
    }
}

impl SecretContentPolicy for RecordingSecretPolicy {
    fn contains_secret(
        &self,
        context: &jiandu_mcp::MutationPolicyContext,
        canonical_command: &[u8],
    ) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(context.correlation_id().as_str().starts_with("req_txn_"));
        self.observations
            .lock()
            .expect("policy observations")
            .push((
                context.client_id().as_str().to_owned(),
                context.correlation_id().as_str().to_owned(),
            ));
        self.denied_sentinel.is_some_and(|sentinel| {
            canonical_command
                .windows(sentinel.len())
                .any(|w| w == sentinel)
        })
    }
}

#[derive(Default)]
struct SpyReadBackend {
    read_calls: AtomicUsize,
    revision_calls: AtomicUsize,
}

#[derive(Default)]
struct SnapshotFailureBackend {
    revision_calls: AtomicUsize,
}

impl McpReadBackend for SnapshotFailureBackend {
    fn get(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn list(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn search(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        Err(ReadBackendError::HostUnavailable)
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        self.revision_calls.fetch_add(1, Ordering::SeqCst);
        Ok(StoreRevision(99))
    }

    fn health(&self) -> ReadServiceHealth {
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing)
    }
}

impl McpMutationBackend for SnapshotFailureBackend {
    fn remember(
        &self,
        _authorization: &AuthorizedMutationSet,
        _invocation: &MutationInvocation,
        _policy: &dyn MutationPolicy,
        _creation_actor: CreationActor,
        _command: &RememberMemoryCommand,
    ) -> Result<MutationBackendCommit<RememberMemoryResult>, MutationBackendError> {
        Err(snapshot_revision_conflict())
    }

    fn update(
        &self,
        _authorization: &AuthorizedMutationSet,
        _invocation: &MutationInvocation,
        _policy: &dyn MutationPolicy,
        _command: &UpdateMemoryCommand,
    ) -> Result<MutationBackendCommit<UpdateMemoryResult>, MutationBackendError> {
        Err(snapshot_revision_conflict())
    }

    fn forget(
        &self,
        _authorization: &AuthorizedMutationSet,
        _invocation: &MutationInvocation,
        _policy: &dyn MutationPolicy,
        _command: &ForgetMemoryCommand,
    ) -> Result<MutationBackendCommit<ForgetMemoryResult>, MutationBackendError> {
        Err(snapshot_revision_conflict())
    }
}

fn snapshot_revision_conflict() -> MutationBackendError {
    MutationBackendError::new(
        ReadBackendError::Store(StoreError::RevisionConflict {
            current_revision: Revision::new(7).expect("revision"),
        }),
        StoreRevision(3),
    )
}

struct DelayedMutationBackend {
    inner: Arc<CanonicalReadBackend>,
    fresh_commits: AtomicUsize,
    completed_delays: AtomicUsize,
    post_commit_delay: Duration,
}

#[derive(Debug)]
struct FailOnce {
    boundary: PersistenceBoundary,
    fired: AtomicBool,
}

impl FailOnce {
    fn at(boundary: PersistenceBoundary) -> Arc<Self> {
        Arc::new(Self {
            boundary,
            fired: AtomicBool::new(false),
        })
    }
}

impl PersistenceFailpointInjector for FailOnce {
    fn should_fail(&self, boundary: PersistenceBoundary) -> bool {
        boundary == self.boundary && !self.fired.swap(true, Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct PauseAtBoundary {
    boundary: PersistenceBoundary,
    reached: AtomicBool,
    release: Mutex<bool>,
    release_changed: Condvar,
}

impl PauseAtBoundary {
    fn at(boundary: PersistenceBoundary) -> Arc<Self> {
        Arc::new(Self {
            boundary,
            reached: AtomicBool::new(false),
            release: Mutex::new(false),
            release_changed: Condvar::new(),
        })
    }

    fn release(&self) {
        *self.release.lock().expect("pause release lock") = true;
        self.release_changed.notify_all();
    }
}

impl PersistenceFailpointInjector for PauseAtBoundary {
    fn should_fail(&self, boundary: PersistenceBoundary) -> bool {
        if boundary != self.boundary {
            return false;
        }
        self.reached.store(true, Ordering::SeqCst);
        let mut release = self.release.lock().expect("pause release lock");
        while !*release {
            release = self
                .release_changed
                .wait(release)
                .expect("pause release wait");
        }
        false
    }
}

#[derive(Clone, Copy, Debug)]
enum BoundaryMutation {
    Remember,
    Update,
    Forget,
}

impl BoundaryMutation {
    const ALL: [Self; 3] = [Self::Remember, Self::Update, Self::Forget];

    const fn boundaries(self) -> &'static [PersistenceBoundary] {
        match self {
            Self::Remember | Self::Update => PersistenceBoundary::CREATE_UPDATE_TRANSACTION,
            Self::Forget => PersistenceBoundary::FORGET_TRANSACTION,
        }
    }

    const fn tool(self) -> &'static str {
        match self {
            Self::Remember => "memory_remember",
            Self::Update => "memory_update",
            Self::Forget => "memory_forget",
        }
    }

    const fn target_revision(self) -> u64 {
        match self {
            Self::Remember => 1,
            Self::Update | Self::Forget => 2,
        }
    }
}

impl DelayedMutationBackend {
    fn new(inner: Arc<CanonicalReadBackend>, post_commit_delay: Duration) -> Self {
        Self {
            inner,
            fresh_commits: AtomicUsize::new(0),
            completed_delays: AtomicUsize::new(0),
            post_commit_delay,
        }
    }

    fn delay_after_fresh_commit(&self, replay: bool) {
        if !replay {
            self.fresh_commits.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.post_commit_delay);
            self.completed_delays.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl McpReadBackend for DelayedMutationBackend {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        self.inner.get(authorization, request)
    }

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        self.inner.list(authorization, request)
    }

    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        self.inner.search(authorization, request)
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        self.inner.store_revision()
    }

    fn health(&self) -> ReadServiceHealth {
        self.inner.health()
    }
}

impl McpMutationBackend for DelayedMutationBackend {
    fn remember(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        creation_actor: CreationActor,
        command: &RememberMemoryCommand,
    ) -> Result<MutationBackendCommit<RememberMemoryResult>, MutationBackendError> {
        let commit =
            self.inner
                .remember(authorization, invocation, policy, creation_actor, command)?;
        self.delay_after_fresh_commit(commit.result.idempotent_replay);
        Ok(commit)
    }

    fn update(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &UpdateMemoryCommand,
    ) -> Result<MutationBackendCommit<UpdateMemoryResult>, MutationBackendError> {
        let commit = self
            .inner
            .update(authorization, invocation, policy, command)?;
        self.delay_after_fresh_commit(commit.result.idempotent_replay);
        Ok(commit)
    }

    fn forget(
        &self,
        authorization: &AuthorizedMutationSet,
        invocation: &MutationInvocation,
        policy: &dyn MutationPolicy,
        command: &ForgetMemoryCommand,
    ) -> Result<MutationBackendCommit<ForgetMemoryResult>, MutationBackendError> {
        let commit = self
            .inner
            .forget(authorization, invocation, policy, command)?;
        self.delay_after_fresh_commit(commit.result.idempotent_replay);
        Ok(commit)
    }
}

impl McpReadBackend for SpyReadBackend {
    fn get(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        Err(ReadBackendError::HostUnavailable)
    }

    fn list(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        Err(ReadBackendError::HostUnavailable)
    }

    fn search(
        &self,
        _authorization: &AuthorizedRead,
        _request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        Err(ReadBackendError::HostUnavailable)
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        self.revision_calls.fetch_add(1, Ordering::SeqCst);
        Err(ReadBackendError::HostUnavailable)
    }

    fn health(&self) -> ReadServiceHealth {
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing)
    }
}

#[tokio::test]
async fn read_only_connection_rejects_mutations_without_backend_or_revision_io() {
    let principal = PrincipalId::new("prn_mcp_read_only").expect("principal");
    let scopes = AuthorizedScopes::new(principal.clone());
    let authorization = scopes
        .authorize_read(&context(&principal, &["memory:read"], "cli_mcp_read_only"))
        .expect("read authorization");
    let backend = Arc::new(SpyReadBackend::default());
    let server = JianduReadServer::from_authorized(backend.clone(), authorization);
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let memory_id = jiandu_core::MemoryId::new("mem_read_only_target").expect("memory ID");
    let calls = [
        (
            "memory_remember",
            arguments(&remember_command("read-only-key", "read-only body")),
        ),
        (
            "memory_update",
            arguments(&UpdateMemoryCommand {
                memory_id: memory_id.clone(),
                expected_revision: Revision::new(1).expect("revision"),
                patch: MemoryPatch {
                    title: Some("read-only update".to_owned()),
                    ..MemoryPatch::default()
                },
                reason: "read-only update reason".to_owned(),
                idempotency_key: IdempotencyKey::new("read-only-update-key").expect("key"),
            }),
        ),
        (
            "memory_forget",
            arguments(&ForgetMemoryCommand {
                memory_id,
                expected_revision: Revision::new(1).expect("revision"),
                reason: "read-only forget reason".to_owned(),
                idempotency_key: IdempotencyKey::new("read-only-forget-key").expect("key"),
            }),
        ),
    ];
    for (tool, arguments) in calls {
        let denied = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
            .await
            .expect("structured denial");
        let envelope = error_envelope(&denied);
        assert_eq!(envelope.error.code, jiandu_core::DomainErrorCode::Forbidden);
        assert_eq!(envelope.store_revision, StoreRevision(0));
        assert!(envelope.correlation_id.as_str().starts_with("req_txn_"));
        assert_safe_wire(&denied, &["read-only", "mem_read_only_target"]);
    }
    assert_eq!(backend.read_calls.load(Ordering::SeqCst), 0);
    assert_eq!(backend.revision_calls.load(Ordering::SeqCst), 0);

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test]
async fn mutation_errors_use_the_same_guard_revision_snapshot_without_a_second_read() {
    let principal = PrincipalId::new("prn_mcp_revision_snapshot").expect("principal");
    let scopes = AuthorizedScopes::new(principal.clone());
    let connection = context(
        &principal,
        &["memory:read", "memory:write:principal"],
        "cli_mcp_revision_snapshot",
    );
    let backend = Arc::new(SnapshotFailureBackend::default());
    let policy: Arc<dyn MutationPolicy> = Arc::new(
        ConfiguredMutationPolicy::allow_all(Arc::new(RecordingSecretPolicy::allow()))
            .expect("allow policy"),
    );
    let server = JianduReadServer::new_with_mutations(
        backend.clone(),
        &scopes,
        &connection,
        policy,
        CreationActor::Host,
    )
    .expect("mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");
    let update = UpdateMemoryCommand {
        memory_id: jiandu_core::MemoryId::new("mem_snapshot_target").expect("memory ID"),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            title: Some("revision snapshot".to_owned()),
            ..MemoryPatch::default()
        },
        reason: "revision snapshot".to_owned(),
        idempotency_key: IdempotencyKey::new("revision-snapshot-key").expect("key"),
    };
    let result = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)))
        .await
        .expect("revision conflict envelope");
    let envelope = error_envelope(&result);
    assert_eq!(
        envelope.error.code,
        jiandu_core::DomainErrorCode::RevisionConflict
    );
    assert_eq!(envelope.store_revision, StoreRevision(3));
    assert_eq!(
        envelope.error.details,
        BTreeMap::from([("currentRevision".to_owned(), serde_json::json!(7))])
    );
    assert_eq!(backend.revision_calls.load(Ordering::SeqCst), 0);

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test]
async fn duplex_mutations_preserve_replay_cas_policy_correlation_and_body_free_forget() {
    let fixture = mutation_fixture(true);
    let detector = Arc::new(RecordingSecretPolicy::allow());
    let policy: Arc<dyn MutationPolicy> =
        Arc::new(ConfiguredMutationPolicy::allow_all(detector.clone()).expect("allow policy"));
    let connection = context(
        &fixture.principal,
        &[
            "memory:read",
            "memory:write:principal",
            "memory:forget:principal",
        ],
        "cli_mcp_mutation",
    );
    let server = JianduReadServer::new_with_mutations(
        fixture.backend.clone(),
        &fixture.scopes,
        &connection,
        policy,
        CreationActor::User,
    )
    .expect("mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let remember = remember_command("mcp-remember-key", "MCP_PRIVATE_BODY_SENTINEL");
    let remembered = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("remember result");
    let remembered: ResultEnvelope<RememberMemoryResult> = success_envelope(&remembered);
    assert_eq!(remembered.store_revision, StoreRevision(1));
    assert!(!remembered.result.idempotent_replay);
    assert_eq!(
        remembered.result.record.provenance.created_by,
        CreationActor::User
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.backend.health().index(), IndexReadHealth::Degraded);
    let degraded_search =
        client
            .call_tool(
                CallToolRequestParams::new("memory_search").with_arguments(arguments(
                    &search_request("remember", vec![ScopeSelector::Principal {}], 10),
                )),
            )
            .await
            .expect("degraded search envelope");
    assert_eq!(
        error_envelope(&degraded_search).error.code,
        jiandu_core::DomainErrorCode::IndexDegraded
    );
    let exact_get = client
        .call_tool(
            CallToolRequestParams::new("memory_get").with_arguments(arguments(&MemoryGetRequest {
                memory_id: remembered.result.record.id.clone(),
            })),
        )
        .await
        .expect("exact get remains available");
    let exact_get: ResultEnvelope<MemoryRecord> = success_envelope(&exact_get);
    assert_eq!(exact_get.result, remembered.result.record);
    let exact_list = client
        .call_tool(
            CallToolRequestParams::new("memory_list").with_arguments(arguments(&list_request(
                vec![ScopeSelector::Principal {}],
                10,
            ))),
        )
        .await
        .expect("list remains available");
    let exact_list: ResultEnvelope<MemoryListResult> = success_envelope(&exact_list);
    assert_eq!(exact_list.result.memories.len(), 1);
    let direct = fixture
        .store
        .read()
        .expect("store read lock")
        .get(&remembered.result.record.id, &fixture.scopes)
        .expect("direct get");
    assert_eq!(direct.result, remembered.result.record);

    let replay_tree = tree_snapshot(fixture.root.path());
    let replay = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("remember replay");
    let replay: ResultEnvelope<RememberMemoryResult> = success_envelope(&replay);
    assert!(replay.result.idempotent_replay);
    assert_eq!(replay.correlation_id, remembered.correlation_id);
    assert_eq!(replay.store_revision, remembered.store_revision);
    assert_eq!(replay.result.record, remembered.result.record);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tree_snapshot(fixture.root.path()), replay_tree);

    let mut conflicting_remember = remember.clone();
    conflicting_remember.body = "DIFFERENT_INPUT_SENTINEL".to_owned();
    let conflict = client
        .call_tool(
            CallToolRequestParams::new("memory_remember")
                .with_arguments(arguments(&conflicting_remember)),
        )
        .await
        .expect("idempotency conflict envelope");
    assert_eq!(
        error_envelope(&conflict).error.code,
        jiandu_core::DomainErrorCode::IdempotencyConflict
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tree_snapshot(fixture.root.path()), replay_tree);
    assert_safe_wire(&conflict, &["DIFFERENT_INPUT_SENTINEL", "mcp-remember-key"]);

    let update = UpdateMemoryCommand {
        memory_id: remembered.result.record.id.clone(),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            title: Some("Updated through MCP".to_owned()),
            body: Some("MCP_UPDATED_BODY_SENTINEL".to_owned()),
            tags: None,
            status: None,
            relations: None,
        },
        reason: "MCP_UPDATE_REASON_SENTINEL".to_owned(),
        idempotency_key: IdempotencyKey::new("mcp-update-key").expect("key"),
    };
    let updated = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)))
        .await
        .expect("update result");
    let updated: ResultEnvelope<UpdateMemoryResult> = success_envelope(&updated);
    assert_eq!(updated.store_revision, StoreRevision(2));
    assert_eq!(updated.result.previous_revision.get(), 1);
    assert_eq!(updated.result.record.revision.get(), 2);
    assert!(!updated.result.idempotent_replay);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 2);

    let update_replay_tree = tree_snapshot(fixture.root.path());
    let update_replay = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)))
        .await
        .expect("update replay");
    let update_replay: ResultEnvelope<UpdateMemoryResult> = success_envelope(&update_replay);
    assert!(update_replay.result.idempotent_replay);
    assert_eq!(update_replay.correlation_id, updated.correlation_id);
    assert_eq!(update_replay.store_revision, updated.store_revision);
    assert_eq!(update_replay.result.record, updated.result.record);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tree_snapshot(fixture.root.path()), update_replay_tree);

    let mut conflicting_update = update.clone();
    conflicting_update.patch.title = Some("CONFLICTING_UPDATE_SENTINEL".to_owned());
    let update_conflict = client
        .call_tool(
            CallToolRequestParams::new("memory_update")
                .with_arguments(arguments(&conflicting_update)),
        )
        .await
        .expect("update idempotency conflict envelope");
    assert_eq!(
        error_envelope(&update_conflict).error.code,
        jiandu_core::DomainErrorCode::IdempotencyConflict
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tree_snapshot(fixture.root.path()), update_replay_tree);
    assert_safe_wire(&update_conflict, &["CONFLICTING_UPDATE_SENTINEL"]);

    let stale = UpdateMemoryCommand {
        idempotency_key: IdempotencyKey::new("mcp-stale-key").expect("key"),
        reason: "MCP_STALE_REASON_SENTINEL".to_owned(),
        ..update.clone()
    };
    let stale_result = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&stale)))
        .await
        .expect("stale CAS envelope");
    let stale_envelope = error_envelope(&stale_result);
    assert_eq!(
        stale_envelope.error.code,
        jiandu_core::DomainErrorCode::RevisionConflict
    );
    assert_eq!(
        stale_envelope.error.details,
        BTreeMap::from([("currentRevision".to_owned(), serde_json::json!(2))])
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tree_snapshot(fixture.root.path()), update_replay_tree);
    assert_safe_wire(
        &stale_result,
        &["MCP_STALE_REASON_SENTINEL", "mcp-stale-key"],
    );

    let forget = ForgetMemoryCommand {
        memory_id: updated.result.record.id.clone(),
        expected_revision: Revision::new(2).expect("revision"),
        reason: "MCP_FORGET_REASON_SENTINEL".to_owned(),
        idempotency_key: IdempotencyKey::new("mcp-forget-key").expect("key"),
    };
    let forgotten = client
        .call_tool(CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)))
        .await
        .expect("forget result");
    let forgotten: ResultEnvelope<ForgetMemoryResult> = success_envelope(&forgotten);
    assert_eq!(forgotten.store_revision, StoreRevision(3));
    assert!(!forgotten.result.idempotent_replay);
    assert_eq!(forgotten.result.memory_id, updated.result.record.id);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 3);
    let forgotten_wire = serde_json::to_string(&forgotten).expect("forget wire");
    for forbidden in [
        "MCP_UPDATED_BODY_SENTINEL",
        "MCP_FORGET_REASON_SENTINEL",
        "mcp-forget-key",
    ] {
        assert!(!forgotten_wire.contains(forbidden));
    }

    let forget_replay_tree = tree_snapshot(fixture.root.path());
    let forget_replay = client
        .call_tool(CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)))
        .await
        .expect("forget replay");
    let forget_replay: ResultEnvelope<ForgetMemoryResult> = success_envelope(&forget_replay);
    assert!(forget_replay.result.idempotent_replay);
    assert_eq!(forget_replay.correlation_id, forgotten.correlation_id);
    assert_eq!(forget_replay.store_revision, forgotten.store_revision);
    assert_eq!(
        forget_replay.result.forgotten_at,
        forgotten.result.forgotten_at
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 3);
    assert_eq!(tree_snapshot(fixture.root.path()), forget_replay_tree);

    let mut conflicting_forget = forget.clone();
    conflicting_forget.reason = "CONFLICTING_FORGET_REASON_SENTINEL".to_owned();
    let forget_conflict = client
        .call_tool(
            CallToolRequestParams::new("memory_forget")
                .with_arguments(arguments(&conflicting_forget)),
        )
        .await
        .expect("forget idempotency conflict envelope");
    assert_eq!(
        error_envelope(&forget_conflict).error.code,
        jiandu_core::DomainErrorCode::IdempotencyConflict
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 3);
    assert_eq!(tree_snapshot(fixture.root.path()), forget_replay_tree);
    assert_safe_wire(
        &forget_conflict,
        &["CONFLICTING_FORGET_REASON_SENTINEL", "mcp-forget-key"],
    );
    assert!(
        detector
            .observations
            .lock()
            .expect("policy observations")
            .iter()
            .all(|(client_id, _)| client_id == "cli_mcp_mutation")
    );

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rmcp_cancellation_does_not_abort_durable_remember_update_or_forget() {
    let fixture = mutation_fixture(false);
    let detector = Arc::new(RecordingSecretPolicy::allow());
    let policy: Arc<dyn MutationPolicy> =
        Arc::new(ConfiguredMutationPolicy::allow_all(detector.clone()).expect("allow policy"));
    let connection = context(
        &fixture.principal,
        &[
            "memory:read",
            "memory:write:principal",
            "memory:forget:principal",
        ],
        "cli_mcp_cancelled_mutation",
    );
    let delayed = Arc::new(DelayedMutationBackend::new(
        fixture.backend.clone(),
        Duration::from_millis(200),
    ));
    let server = JianduReadServer::new_with_mutations(
        delayed.clone(),
        &fixture.scopes,
        &connection,
        policy,
        CreationActor::Host,
    )
    .expect("mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let remember = remember_command("cancel-remember-key", "cancelled remember body");
    timeout_after_durable_commit(
        client.peer(),
        delayed.as_ref(),
        1,
        CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
    )
    .await;
    let remember_replay = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("remember replay after cancellation");
    let remember_replay: ResultEnvelope<RememberMemoryResult> = success_envelope(&remember_replay);
    assert!(remember_replay.result.idempotent_replay);
    assert_eq!(remember_replay.store_revision, StoreRevision(1));

    let update = UpdateMemoryCommand {
        memory_id: remember_replay.result.record.id.clone(),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            body: Some("cancelled update body".to_owned()),
            ..MemoryPatch::default()
        },
        reason: "cancelled update reason".to_owned(),
        idempotency_key: IdempotencyKey::new("cancel-update-key").expect("key"),
    };
    timeout_after_durable_commit(
        client.peer(),
        delayed.as_ref(),
        2,
        CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)),
    )
    .await;
    let update_replay = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)))
        .await
        .expect("update replay after cancellation");
    let update_replay: ResultEnvelope<UpdateMemoryResult> = success_envelope(&update_replay);
    assert!(update_replay.result.idempotent_replay);
    assert_eq!(update_replay.store_revision, StoreRevision(2));

    let forget = ForgetMemoryCommand {
        memory_id: update_replay.result.record.id.clone(),
        expected_revision: Revision::new(2).expect("revision"),
        reason: "cancelled forget reason".to_owned(),
        idempotency_key: IdempotencyKey::new("cancel-forget-key").expect("key"),
    };
    timeout_after_durable_commit(
        client.peer(),
        delayed.as_ref(),
        3,
        CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)),
    )
    .await;
    let forget_replay = client
        .call_tool(CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)))
        .await
        .expect("forget replay after cancellation");
    let forget_replay: ResultEnvelope<ForgetMemoryResult> = success_envelope(&forget_replay);
    assert!(forget_replay.result.idempotent_replay);
    assert_eq!(forget_replay.store_revision, StoreRevision(3));
    assert_eq!(detector.calls.load(Ordering::SeqCst), 3);

    wait_for_counter(&delayed.completed_delays, 3).await;
    assert_eq!(
        fixture
            .store
            .read()
            .expect("store read lock")
            .watermark()
            .expect("watermark"),
        StoreRevision(3)
    );

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_cancelled_at_every_live_mutation_boundary_reopen_as_one_commit() {
    assert_eq!(PersistenceBoundary::CREATE_UPDATE_TRANSACTION.len(), 34);
    assert_eq!(PersistenceBoundary::FORGET_TRANSACTION.len(), 38);
    for operation in BoundaryMutation::ALL {
        for &boundary in operation.boundaries() {
            run_cancelled_boundary_case(operation, boundary).await;
        }
    }
}

#[tokio::test]
async fn post_metadata_boundary_failure_is_degraded_then_reopens_as_exact_replay() {
    let root = TempDir::new().expect("temporary store");
    let principal = PrincipalId::new("prn_mcp_recovery").expect("principal");
    let scopes = AuthorizedScopes::new(principal.clone());
    CanonicalStore::initialize(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("initialize store");
    let store = CanonicalStore::open_with_options(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
        StoreOptions::with_failpoint_injector(FailOnce::at(
            PersistenceBoundary::MetadataDirectorySynced,
        )),
    )
    .expect("open with failpoint");
    let store = Arc::new(RwLock::new(store));
    let index_directory = root.path().join("index");
    let backend = Arc::new(CanonicalReadBackend::new(
        store.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x44; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    ));
    let detector = Arc::new(RecordingSecretPolicy::allow());
    let policy: Arc<dyn MutationPolicy> =
        Arc::new(ConfiguredMutationPolicy::allow_all(detector.clone()).expect("allow policy"));
    let connection = context(
        &principal,
        &["memory:read", "memory:write:principal"],
        "cli_mcp_recovery",
    );
    let server = JianduReadServer::new_with_mutations(
        backend.clone(),
        &scopes,
        &connection,
        policy.clone(),
        CreationActor::Host,
    )
    .expect("mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");
    let remember = remember_command("recovery-key", "RECOVERY_BODY_SENTINEL");
    let failed = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("safe post-boundary failure");
    let failure = error_envelope(&failed);
    assert_eq!(
        failure.error.code,
        jiandu_core::DomainErrorCode::StoreUnavailable
    );
    assert_eq!(backend.health().store(), StoreReadHealth::Degraded);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_safe_wire(&failed, &["RECOVERY_BODY_SENTINEL", "recovery-key"]);

    client.cancel().await.expect("cancel failed client");
    server_task.await.expect("join failed server");
    drop(backend);
    drop(store);

    let reopened = CanonicalStore::open(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("deterministic startup recovery");
    let reopened = Arc::new(RwLock::new(reopened));
    let backend = Arc::new(CanonicalReadBackend::new(
        reopened.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x44; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    ));
    let server = JianduReadServer::new_with_mutations(
        backend,
        &scopes,
        &connection,
        policy,
        CreationActor::Host,
    )
    .expect("recovered mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client reconnects");
    let replay = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("recovered replay");
    let replay: ResultEnvelope<RememberMemoryResult> = success_envelope(&replay);
    assert!(replay.result.idempotent_replay);
    assert_eq!(replay.store_revision, StoreRevision(1));
    assert_eq!(replay.correlation_id, failure.correlation_id);
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened
            .read()
            .expect("store read lock")
            .watermark()
            .expect("watermark"),
        StoreRevision(1)
    );

    client.cancel().await.expect("cancel recovered client");
    server_task.await.expect("join recovered server");
}

#[tokio::test]
async fn grants_scope_and_configured_policy_are_independent_and_write_free_on_denial() {
    let fixture = mutation_fixture(false);
    let project_scope = MemoryScope::Project {
        project_id: ProjectId::new("prj_mcp_mutations").expect("project"),
    };
    let project_authorization = fixture
        .scopes
        .authorize_mutation(
            &context(
                &fixture.principal,
                &["memory:write:project"],
                "cli_mcp_project_fixture",
            ),
            &project_scope,
            MutationOperation::Create,
        )
        .expect("project create authority");
    let mut project_fixture = remember_command("project-fixture-key", "PROJECT_BODY_SENTINEL");
    project_fixture.scope = ScopeSelector::Project {
        project_id: ProjectId::new("prj_mcp_mutations").expect("project"),
    };
    fixture
        .store
        .write()
        .expect("store write lock")
        .create(
            &project_authorization,
            &project_fixture,
            jiandu_core::MemoryId::new("mem_mcp_project_private").expect("memory ID"),
            CreationActor::Host,
            Timestamp::new("2026-08-25T00:00:00Z").expect("timestamp"),
        )
        .expect("project fixture record");
    let project_update_authorization = fixture
        .scopes
        .authorize_mutation(
            &context(
                &fixture.principal,
                &["memory:write:project"],
                "cli_mcp_project_fixture",
            ),
            &project_scope,
            MutationOperation::Update,
        )
        .expect("project update authority");
    let project_receipt_update = UpdateMemoryCommand {
        memory_id: jiandu_core::MemoryId::new("mem_mcp_project_private").expect("memory ID"),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            title: Some("PROJECT_RECEIPT_PRIVATE_SENTINEL".to_owned()),
            ..MemoryPatch::default()
        },
        reason: "PROJECT_RECEIPT_REASON_SENTINEL".to_owned(),
        idempotency_key: IdempotencyKey::new("project-receipt-key").expect("key"),
    };
    fixture
        .store
        .write()
        .expect("store write lock")
        .update(
            &project_update_authorization,
            &project_receipt_update,
            Timestamp::new("2026-08-25T00:00:01Z").expect("timestamp"),
        )
        .expect("project update receipt");
    let detector = Arc::new(RecordingSecretPolicy::deny(b"DENY_SECRET"));
    let policy: Arc<dyn MutationPolicy> = Arc::new(
        ConfiguredMutationPolicy::new(
            32,
            BTreeSet::from([MemoryType::Decision]),
            BTreeSet::from([MutationScopeKind::Principal]),
            detector.clone(),
        )
        .expect("configured policy"),
    );
    let write_context = context(
        &fixture.principal,
        &["memory:read", "memory:write:principal"],
        "cli_mcp_write_only",
    );
    let server = JianduReadServer::new_with_mutations(
        fixture.backend.clone(),
        &fixture.scopes,
        &write_context,
        policy,
        CreationActor::Model,
    )
    .expect("write server");
    let (server_transport, client_transport) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let before = tree_snapshot(fixture.root.path());
    let mut project = remember_command("project-denial-key", "project denial");
    project.scope = ScopeSelector::Project {
        project_id: ProjectId::new("prj_mcp_mutations").expect("project"),
    };
    let project_denial = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&project)),
        )
        .await
        .expect("project denial");
    assert_eq!(
        error_envelope(&project_denial).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    assert_eq!(
        error_envelope(&project_denial).store_revision,
        StoreRevision(0)
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tree_snapshot(fixture.root.path()), before);

    let cross_scope_update = UpdateMemoryCommand {
        memory_id: jiandu_core::MemoryId::new("mem_mcp_project_private").expect("memory ID"),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            title: Some("CROSS_SCOPE_UPDATE_SENTINEL".to_owned()),
            ..MemoryPatch::default()
        },
        reason: "CROSS_SCOPE_REASON_SENTINEL".to_owned(),
        idempotency_key: IdempotencyKey::new("cross-scope-update-key").expect("key"),
    };
    let cross_scope = client
        .call_tool(
            CallToolRequestParams::new("memory_update")
                .with_arguments(arguments(&cross_scope_update)),
        )
        .await
        .expect("cross-scope envelope");
    assert_eq!(
        error_envelope(&cross_scope).error.code,
        jiandu_core::DomainErrorCode::NotFound
    );
    let ambient_path = fixture.root.path().display().to_string();
    assert_safe_wire(
        &cross_scope,
        &[
            "mem_mcp_project_private",
            "PROJECT_BODY_SENTINEL",
            "CROSS_SCOPE_UPDATE_SENTINEL",
            "CROSS_SCOPE_REASON_SENTINEL",
            "cross-scope-update-key",
            &ambient_path,
        ],
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tree_snapshot(fixture.root.path()), before);

    let out_of_set_receipt = client
        .call_tool(
            CallToolRequestParams::new("memory_update")
                .with_arguments(arguments(&project_receipt_update)),
        )
        .await
        .expect("out-of-set receipt envelope");
    assert_eq!(
        error_envelope(&out_of_set_receipt).error.code,
        jiandu_core::DomainErrorCode::NotFound
    );
    let ambient_path = fixture.root.path().display().to_string();
    assert_safe_wire(
        &out_of_set_receipt,
        &[
            "mem_mcp_project_private",
            "PROJECT_RECEIPT_PRIVATE_SENTINEL",
            "PROJECT_RECEIPT_REASON_SENTINEL",
            "project-receipt-key",
            &ambient_path,
        ],
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tree_snapshot(fixture.root.path()), before);

    let secret = remember_command("secret-denial-key", "DENY_SECRET");
    let secret_denial = client
        .call_tool(CallToolRequestParams::new("memory_remember").with_arguments(arguments(&secret)))
        .await
        .expect("secret denial");
    assert_eq!(
        error_envelope(&secret_denial).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tree_snapshot(fixture.root.path()), before);
    assert_safe_wire(&secret_denial, &["DENY_SECRET", "secret-denial-key"]);

    let oversized = remember_command(
        "size-denial-key",
        "this canonical body is longer than thirty two UTF-8 bytes",
    );
    let size_denial = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&oversized)),
        )
        .await
        .expect("size denial");
    assert_eq!(
        error_envelope(&size_denial).error.code,
        jiandu_core::DomainErrorCode::InvalidArgument
    );
    assert_eq!(tree_snapshot(fixture.root.path()), before);

    let mut wrong_type = remember_command("type-denial-key", "valid body");
    wrong_type.memory_type = MemoryType::Fact;
    let type_denial = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&wrong_type)),
        )
        .await
        .expect("type denial");
    assert_eq!(
        error_envelope(&type_denial).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    assert_eq!(tree_snapshot(fixture.root.path()), before);

    let allowed = remember_command("grant-target-key", "allowed body");
    let allowed_result = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&allowed)),
        )
        .await
        .expect("allowed remember");
    let allowed: ResultEnvelope<RememberMemoryResult> = success_envelope(&allowed_result);
    assert_eq!(fixture.backend.health().index(), IndexReadHealth::Missing);
    let forget = ForgetMemoryCommand {
        memory_id: allowed.result.record.id.clone(),
        expected_revision: Revision::new(1).expect("revision"),
        reason: "write grant is not forget".to_owned(),
        idempotency_key: IdempotencyKey::new("write-cannot-forget").expect("key"),
    };
    let no_forget = client
        .call_tool(CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)))
        .await
        .expect("forget grant denial");
    assert_eq!(
        error_envelope(&no_forget).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    assert_eq!(error_envelope(&no_forget).store_revision, StoreRevision(0));

    client.cancel().await.expect("cancel writer");
    server_task.await.expect("join writer server");

    let forget_context = context(
        &fixture.principal,
        &["memory:read", "memory:forget:principal"],
        "cli_mcp_forget_only",
    );
    let forget_policy: Arc<dyn MutationPolicy> = Arc::new(
        ConfiguredMutationPolicy::allow_all(Arc::new(RecordingSecretPolicy::allow()))
            .expect("forget policy"),
    );
    let server = JianduReadServer::new_with_mutations(
        fixture.backend.clone(),
        &fixture.scopes,
        &forget_context,
        forget_policy,
        CreationActor::Host,
    )
    .expect("forget server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");

    let no_remember = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(
                &remember_command("forget-cannot-remember", "not authorized"),
            )),
        )
        .await
        .expect("remember grant denial");
    assert_eq!(
        error_envelope(&no_remember).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    assert_eq!(
        error_envelope(&no_remember).store_revision,
        StoreRevision(0)
    );

    let update = UpdateMemoryCommand {
        memory_id: allowed.result.record.id.clone(),
        expected_revision: Revision::new(1).expect("revision"),
        patch: MemoryPatch {
            title: Some("forget-only cannot update".to_owned()),
            ..MemoryPatch::default()
        },
        reason: "forget-only".to_owned(),
        idempotency_key: IdempotencyKey::new("forget-cannot-update").expect("key"),
    };
    let no_update = client
        .call_tool(CallToolRequestParams::new("memory_update").with_arguments(arguments(&update)))
        .await
        .expect("update grant denial");
    assert_eq!(
        error_envelope(&no_update).error.code,
        jiandu_core::DomainErrorCode::Forbidden
    );
    let project_forget = ForgetMemoryCommand {
        memory_id: jiandu_core::MemoryId::new("mem_mcp_project_private").expect("memory ID"),
        expected_revision: Revision::new(1).expect("revision"),
        reason: "CROSS_SCOPE_FORGET_REASON_SENTINEL".to_owned(),
        idempotency_key: IdempotencyKey::new("cross-scope-forget-key").expect("key"),
    };
    let cross_scope_forget = client
        .call_tool(
            CallToolRequestParams::new("memory_forget").with_arguments(arguments(&project_forget)),
        )
        .await
        .expect("cross-scope forget envelope");
    assert_eq!(
        error_envelope(&cross_scope_forget).error.code,
        jiandu_core::DomainErrorCode::NotFound
    );
    let ambient_path = fixture.root.path().display().to_string();
    assert_safe_wire(
        &cross_scope_forget,
        &[
            "mem_mcp_project_private",
            "PROJECT_BODY_SENTINEL",
            "CROSS_SCOPE_FORGET_REASON_SENTINEL",
            "cross-scope-forget-key",
            &ambient_path,
        ],
    );
    let forgotten = client
        .call_tool(CallToolRequestParams::new("memory_forget").with_arguments(arguments(&forget)))
        .await
        .expect("destructive grant succeeds");
    let _: ResultEnvelope<ForgetMemoryResult> = success_envelope(&forgotten);

    client.cancel().await.expect("cancel forget client");
    server_task.await.expect("join forget server");
}

fn mutation_fixture(index_ready: bool) -> MutationFixture {
    let root = TempDir::new().expect("temporary store");
    let principal = PrincipalId::new("prn_mcp_mutations").expect("principal");
    let scopes = AuthorizedScopes::new(principal.clone())
        .with_project(ProjectId::new("prj_mcp_mutations").expect("project"));
    let store = CanonicalStore::initialize(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("initialize store");
    let index_directory = root.path().join("index");
    let index = LexicalIndex::new(&index_directory);
    if index_ready {
        let admin = scopes
            .authorize_index_rebuild(&context(
                &principal,
                &["memory:admin:rebuild_index"],
                "cli_mcp_index_admin",
            ))
            .expect("index admin");
        index.rebuild(&store, &admin).expect("empty index rebuild");
    }
    let store = Arc::new(RwLock::new(store));
    let backend = Arc::new(CanonicalReadBackend::new(
        store.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x88; 32]),
        ReadServiceHealth::new(
            StoreReadHealth::Ready,
            if index_ready {
                IndexReadHealth::Ready
            } else {
                IndexReadHealth::Missing
            },
        ),
    ));
    MutationFixture {
        root,
        store,
        backend,
        scopes,
        principal,
    }
}

fn context(principal: &PrincipalId, grants: &[&str], client_id: &str) -> TrustedRequestContext {
    TrustedRequestContext {
        principal_id: principal.clone(),
        client_id: ClientId::new(client_id).expect("client ID"),
        grants: grants
            .iter()
            .map(|grant| Grant::new(*grant).expect("grant"))
            .collect(),
    }
}

fn remember_command(key: &str, body: &str) -> RememberMemoryCommand {
    RememberMemoryCommand {
        scope: ScopeSelector::Principal {},
        memory_type: MemoryType::Decision,
        title: "Remember through MCP".to_owned(),
        summary: Some("Trusted adapter mutation".to_owned()),
        body: body.to_owned(),
        tags: vec![Tag::new("mcp").expect("tag")],
        provenance: ProvenanceInput::default(),
        relations: Vec::new(),
        idempotency_key: IdempotencyKey::new(key).expect("idempotency key"),
    }
}

fn list_request(scopes: Vec<ScopeSelector>, limit: u16) -> MemoryListRequest {
    MemoryListRequest {
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        sort: ListSort::IdAsc,
        limit: PageLimit::new(limit).expect("limit"),
        cursor: None,
    }
}

fn search_request(query: &str, scopes: Vec<ScopeSelector>, limit: u16) -> MemorySearchRequest {
    MemorySearchRequest {
        query: query.to_owned(),
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        limit: PageLimit::new(limit).expect("limit"),
        cursor: None,
    }
}

fn arguments(value: &impl Serialize) -> JsonObject {
    serde_json::to_value(value)
        .expect("request JSON")
        .as_object()
        .expect("request object")
        .clone()
}

fn success_envelope<T: DeserializeOwned>(
    result: &rmcp::model::CallToolResult,
) -> ResultEnvelope<T> {
    assert_eq!(result.is_error, Some(false));
    serde_json::from_value(
        result
            .structured_content
            .clone()
            .expect("authoritative structured result"),
    )
    .expect("success envelope")
}

fn error_envelope(result: &rmcp::model::CallToolResult) -> ErrorEnvelope {
    assert_eq!(result.is_error, Some(true));
    serde_json::from_value(
        result
            .structured_content
            .clone()
            .expect("authoritative structured error"),
    )
    .expect("error envelope")
}

fn assert_safe_wire(result: &rmcp::model::CallToolResult, forbidden: &[&str]) {
    let wire = serde_json::to_string(result).expect("tool wire");
    for sentinel in forbidden {
        assert!(!wire.contains(sentinel), "wire leaked {sentinel}");
    }
    let text = result.content[0].as_text().expect("safe summary");
    assert!(text.text.len() < 100);
}

async fn timeout_after_durable_commit(
    peer: &Peer<RoleClient>,
    backend: &DelayedMutationBackend,
    expected_fresh_commits: usize,
    request: CallToolRequestParams,
) {
    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(request)),
            PeerRequestOptions::with_timeout(Duration::from_millis(20)),
        )
        .await
        .expect("send cancellable mutation");
    wait_for_counter(&backend.fresh_commits, expected_fresh_commits).await;
    let error = handle
        .await_response()
        .await
        .expect_err("client timeout cancels the protocol request");
    assert!(matches!(error, ServiceError::Timeout { .. }));
}

async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("blocking mutation reached the expected durability state");
}

async fn run_cancelled_boundary_case(operation: BoundaryMutation, boundary: PersistenceBoundary) {
    let root = TempDir::new().expect("temporary store");
    let principal = PrincipalId::new("prn_mcp_boundary").expect("principal");
    let scopes = AuthorizedScopes::new(principal.clone());
    let target_id = jiandu_core::MemoryId::new("mem_mcp_boundary_target").expect("memory ID");
    let mut initial = CanonicalStore::initialize(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("initialize store");
    if !matches!(operation, BoundaryMutation::Remember) {
        let exact_scope = MemoryScope::Principal {
            principal_id: principal.clone(),
        };
        let seed_context = context(
            &principal,
            &["memory:write:principal"],
            "cli_mcp_boundary_seed",
        );
        let authorization = scopes
            .authorize_mutation(&seed_context, &exact_scope, MutationOperation::Create)
            .expect("seed authority");
        initial
            .create(
                &authorization,
                &remember_command("boundary-seed-key", "boundary seed body"),
                target_id.clone(),
                CreationActor::Host,
                Timestamp::new("2020-01-01T00:00:00Z").expect("timestamp"),
            )
            .expect("seed target");
    }
    drop(initial);

    let pause = PauseAtBoundary::at(boundary);
    let store = CanonicalStore::open_with_options(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
        StoreOptions::with_failpoint_injector(pause.clone()),
    )
    .expect("open paused store");
    let store = Arc::new(RwLock::new(store));
    let index_directory = root.path().join("index");
    let backend = Arc::new(CanonicalReadBackend::new(
        store.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x66; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    ));
    let detector = Arc::new(RecordingSecretPolicy::allow());
    let policy: Arc<dyn MutationPolicy> =
        Arc::new(ConfiguredMutationPolicy::allow_all(detector.clone()).expect("allow policy"));
    let grants = match operation {
        BoundaryMutation::Remember | BoundaryMutation::Update => {
            ["memory:read", "memory:write:principal"]
        }
        BoundaryMutation::Forget => ["memory:read", "memory:forget:principal"],
    };
    let connection = context(&principal, &grants, "cli_mcp_boundary");
    let request_arguments = boundary_mutation_arguments(operation, &target_id);
    let server = JianduReadServer::new_with_mutations(
        backend.clone(),
        &scopes,
        &connection,
        policy.clone(),
        CreationActor::Host,
    )
    .expect("mutation server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client connects");
    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(
                CallToolRequestParams::new(operation.tool())
                    .with_arguments(request_arguments.clone()),
            )),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("send cancellable mutation");
    wait_for_flag(&pause.reached).await;
    handle
        .cancel(Some("boundary cancellation test".to_owned()))
        .await
        .expect("send notifications/cancelled");
    pause.release();
    wait_for_store_revision(&store, operation.target_revision()).await;
    assert_eq!(
        detector.calls.load(Ordering::SeqCst),
        1,
        "fresh policy count for {operation:?} at {boundary:?}"
    );
    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
    drop(backend);
    drop(store);

    let reopened = Arc::new(RwLock::new(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("reopen after cancelled mutation"),
    ));
    let backend = Arc::new(CanonicalReadBackend::new(
        reopened.clone(),
        LexicalIndex::new(&index_directory),
        CursorMacKey::new([0x66; 32]),
        ReadServiceHealth::new(StoreReadHealth::Ready, IndexReadHealth::Missing),
    ));
    let server = JianduReadServer::new_with_mutations(
        backend.clone(),
        &scopes,
        &connection,
        policy,
        CreationActor::Host,
    )
    .expect("replay server");
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server_task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .expect("server starts")
            .waiting()
            .await
            .expect("server stops");
    });
    let client = V2025Client
        .serve(client_transport)
        .await
        .expect("client reconnects");
    let replay = client
        .call_tool(CallToolRequestParams::new(operation.tool()).with_arguments(request_arguments))
        .await
        .expect("same-key retry");
    assert_boundary_replay(&replay, operation.target_revision(), operation, boundary);
    assert_eq!(
        detector.calls.load(Ordering::SeqCst),
        1,
        "replay policy count for {operation:?} at {boundary:?}"
    );
    assert!(
        detector
            .observations
            .lock()
            .expect("policy observations")
            .iter()
            .all(|(client_id, _)| client_id == "cli_mcp_boundary")
    );
    client.cancel().await.expect("cancel replay client");
    server_task.await.expect("join replay server");
    drop(backend);
    drop(reopened);
    assert_single_commit_watermarks(root.path(), operation.target_revision());
}

fn boundary_mutation_arguments(
    operation: BoundaryMutation,
    target_id: &jiandu_core::MemoryId,
) -> JsonObject {
    match operation {
        BoundaryMutation::Remember => arguments(&remember_command(
            "boundary-remember-key",
            "boundary remember body",
        )),
        BoundaryMutation::Update => arguments(&UpdateMemoryCommand {
            memory_id: target_id.clone(),
            expected_revision: Revision::new(1).expect("revision"),
            patch: MemoryPatch {
                title: Some("boundary updated title".to_owned()),
                ..MemoryPatch::default()
            },
            reason: "boundary update reason".to_owned(),
            idempotency_key: IdempotencyKey::new("boundary-update-key").expect("key"),
        }),
        BoundaryMutation::Forget => arguments(&ForgetMemoryCommand {
            memory_id: target_id.clone(),
            expected_revision: Revision::new(1).expect("revision"),
            reason: "boundary forget reason".to_owned(),
            idempotency_key: IdempotencyKey::new("boundary-forget-key").expect("key"),
        }),
    }
}

async fn wait_for_flag(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("blocking mutation reached the selected persistence boundary");
}

async fn wait_for_store_revision(store: &RwLock<CanonicalStore>, expected: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = store.try_read()
                && store
                    .watermark()
                    .is_ok_and(|revision| revision == StoreRevision(expected))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("cancelled blocking worker completed its durable transaction");
}

fn assert_boundary_replay(
    result: &rmcp::model::CallToolResult,
    expected_revision: u64,
    operation: BoundaryMutation,
    boundary: PersistenceBoundary,
) {
    assert_eq!(
        result.is_error,
        Some(false),
        "{operation:?} at {boundary:?}"
    );
    let envelope = result
        .structured_content
        .as_ref()
        .expect("authoritative structured result");
    assert_eq!(
        envelope["storeRevision"],
        serde_json::json!(expected_revision),
        "{operation:?} at {boundary:?}"
    );
    assert_eq!(
        envelope["result"]["idempotentReplay"],
        serde_json::json!(true),
        "{operation:?} at {boundary:?}"
    );
    assert!(
        envelope["correlationId"]
            .as_str()
            .is_some_and(|value| value.starts_with("req_txn_")),
        "{operation:?} at {boundary:?}"
    );
}

fn assert_single_commit_watermarks(root: &Path, expected: u64) {
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("store.json")).expect("store metadata"))
            .expect("store metadata JSON");
    assert_eq!(metadata["storeRevision"], serde_json::json!(expected));
    assert_eq!(metadata["auditSequence"], serde_json::json!(expected));
    let audit_count = fs::read_dir(root.join("audit/mutations"))
        .expect("mutation audit directory")
        .count();
    assert_eq!(audit_count as u64, expected);
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let mut entries = fs::read_dir(path)
            .expect("read snapshot directory")
            .map(|entry| entry.expect("snapshot entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            output.insert(
                path.strip_prefix(root)
                    .expect("entry below root")
                    .to_path_buf(),
                SnapshotEntry {
                    bytes: metadata
                        .is_file()
                        .then(|| fs::read(&path).expect("snapshot file")),
                    modified: metadata.modified().expect("modified time"),
                },
            );
            if metadata.is_dir() {
                visit(root, &path, output);
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}
