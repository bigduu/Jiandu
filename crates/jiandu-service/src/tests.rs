//! Daemon boundary and Streamable HTTP integration tests.

use crate::auth::bearer_digest;
use crate::daemon::ResponseFramePause;
use crate::lifecycle::HttpAdmissionPause;
use crate::{
    DaemonError, LIVENESS_ROUTE, MCP_ROUTE, READINESS_ROUTE, RunningDaemon, ServeConfig,
    ShutdownOutcome,
};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use jiandu_core::{
    ClientId, CreationActor, Grant, IdempotencyKey, MAX_BODY_BYTES, MemoryGetRequest,
    MemoryListResult, MemoryRecord, MemoryType, PrincipalId, ProjectId, ProvenanceInput,
    RememberMemoryCommand, RememberMemoryResult, ResultEnvelope, ScopeSelector, SessionId, Tag,
};
use jiandu_store::{
    CanonicalStore, LockOwner, MutationOperation, PersistenceBoundary,
    PersistenceFailpointInjector, StoreOptions,
};
use rmcp::model::{CallToolRequestParams, ClientInfo, JsonObject, ProtocolVersion};
use rmcp::transport::{
    StreamableHttpClientTransport, common::client_side_sse::NeverRetry,
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, ServiceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VALID_TOKEN: &str = "daemon-e2e-token-0123456789abcdef";
const INVALID_TOKEN: &str = "daemon-wrong-token-0123456789abcdef";
const PRINCIPAL_SENTINEL: &str = "prn_daemon_private_identity";
const CLIENT_SENTINEL: &str = "cli_daemon_private_identity";
const PROJECT_SENTINEL: &str = "prj_daemon_exact_scope";
const SESSION_SENTINEL: &str = "ses_daemon_exact_scope";

#[derive(Debug)]
struct BoundaryPause {
    boundary: PersistenceBoundary,
    fail_after_release: bool,
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl BoundaryPause {
    fn at(boundary: PersistenceBoundary, fail_after_release: bool) -> Arc<Self> {
        Arc::new(Self {
            boundary,
            fail_after_release,
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        })
    }

    fn wait_reached(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while !state.0 && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.0
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.1 = true;
        self.changed.notify_all();
    }
}

impl PersistenceFailpointInjector for BoundaryPause {
    fn should_fail(&self, boundary: PersistenceBoundary) -> bool {
        if boundary != self.boundary {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        self.fail_after_release
    }
}

trait ReleasablePause {
    fn release_pause(&self);
}

impl ReleasablePause for BoundaryPause {
    fn release_pause(&self) {
        self.release();
    }
}

impl ReleasablePause for HttpAdmissionPause {
    fn release_pause(&self) {
        self.release();
    }
}

impl ReleasablePause for ResponseFramePause {
    fn release_pause(&self) {
        self.release();
    }
}

/// Prevent a failed assertion from stranding a blocking fixture and hanging
/// the complete test process. Both pause implementations release idempotently.
struct PauseReleaseGuard<T: ReleasablePause>(Arc<T>);

impl<T: ReleasablePause> PauseReleaseGuard<T> {
    fn new(pause: Arc<T>) -> Self {
        Self(pause)
    }
}

impl<T: ReleasablePause> Drop for PauseReleaseGuard<T> {
    fn drop(&mut self) {
        self.0.release_pause();
    }
}

/// Releases a blocking persistence fixture from an OS thread if a regression
/// parks every Tokio worker. This keeps the saturation test itself bounded even
/// when the behavior under test is broken.
struct TimedPauseReleaseGuard {
    release: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TimedPauseReleaseGuard {
    fn new(pause: Arc<BoundaryPause>, timeout: Duration) -> Self {
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = wait.recv_timeout(timeout);
            pause.release();
        });
        Self {
            release: Some(release),
            worker: Some(worker),
        }
    }
}

impl Drop for TimedPauseReleaseGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone)]
struct V2025Client;

impl ClientHandler for V2025Client {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        info
    }
}

#[test]
fn config_rejects_non_loopback_and_redacts_all_private_startup_material() {
    let root = TempDir::new().expect("temporary data parent");
    let missing = root.path().join("NONEXISTENT_PRIVATE_DATA_PATH");
    for bind in ["0.0.0.0:0", "[::]:0", "192.0.2.10:9800"] {
        let error = config(bind, &missing, VALID_TOKEN).expect_err("non-loopback rejected");
        assert_eq!(error.to_string(), "startup configuration is invalid");
        assert!(!missing.exists(), "validation must perform no store I/O");
    }

    let valid = config("127.0.0.1:0", &missing, VALID_TOKEN).expect("valid loopback config");
    let debug = format!("{valid:?}");
    for private in [
        missing.to_string_lossy().as_ref(),
        VALID_TOKEN,
        PRINCIPAL_SENTINEL,
        CLIENT_SENTINEL,
        &lower_hex(&bearer_digest(VALID_TOKEN.as_bytes())),
        "1111111111111111111111111111111111111111111111111111111111111111",
    ] {
        assert!(
            !debug.contains(private),
            "config debug leaked private material"
        );
    }

    let unavailable_path = root.path().join("CONFIG_PATH_SENTINEL.json");
    let error = ServeConfig::load(&unavailable_path).expect_err("missing config rejected");
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("CONFIG_PATH_SENTINEL"));

    let config_path = root.path().join("daemon.json");
    let config_bytes = json_bytes(config_value("127.0.0.1:0", &missing, VALID_TOKEN));
    assert!(
        !config_bytes
            .windows(VALID_TOKEN.len())
            .any(|part| part == VALID_TOKEN.as_bytes())
    );
    fs::write(&config_path, config_bytes).expect("write local config fixture");
    ServeConfig::load(&config_path).expect("load regular local config file");
    assert!(
        ServeConfig::load(root.path()).is_err(),
        "directory is not a config file"
    );

    let invalid_path = root.path().join("invalid.json");
    fs::write(&invalid_path, b"CONFIG_BODY_SENTINEL").expect("write invalid config");
    let error = ServeConfig::load(&invalid_path).expect_err("invalid JSON rejected");
    assert!(!format!("{error:?} {error}").contains("CONFIG_BODY_SENTINEL"));
}

#[test]
fn compact_profile_derives_the_equivalent_trusted_context_and_exact_mutation_authority() {
    let root = TempDir::new().expect("temporary data parent");
    let mut document = config_value("127.0.0.1:0", root.path(), VALID_TOKEN);
    document["clients"][0]["scopes"] = json!({
        "projectIds": [PROJECT_SENTINEL],
        "sessionIds": [SESSION_SENTINEL],
        "instanceGlobal": true
    });
    document["clients"][0]["permissions"] = json!({
        "read": true,
        "write": ["principal", "project"],
        "forget": ["session", "instance_global"]
    });
    let config = ServeConfig::from_slice(&json_bytes(document)).expect("compact profile");
    let client = &config.clients[0];

    assert_eq!(
        client.context.principal_id,
        PrincipalId::new(PRINCIPAL_SENTINEL).expect("principal")
    );
    assert_eq!(
        client.context.client_id,
        ClientId::new(CLIENT_SENTINEL).expect("client")
    );
    assert_eq!(client.creation_actor, CreationActor::Host);
    assert_eq!(
        client.context.grants,
        BTreeSet::from([
            Grant::new("memory:read").expect("read grant"),
            Grant::new("memory:write:principal").expect("principal write grant"),
            Grant::new("memory:write:project").expect("project write grant"),
            Grant::new("memory:forget:session").expect("session forget grant"),
            Grant::new("memory:forget:instance_global").expect("global forget grant"),
        ])
    );
    assert!(client.mutation_policy.is_some());
    client
        .scopes
        .authorize_read(&client.context)
        .expect("read permission");

    let write = client
        .scopes
        .authorize_mutation_set(&client.context, MutationOperation::Create)
        .expect("write authority");
    assert!(
        write
            .authorize_selector(&ScopeSelector::Principal {})
            .is_ok()
    );
    assert!(
        write
            .authorize_selector(&ScopeSelector::Project {
                project_id: ProjectId::new(PROJECT_SENTINEL).expect("project"),
            })
            .is_ok()
    );
    assert!(
        write
            .authorize_selector(&ScopeSelector::Session {
                session_id: SessionId::new(SESSION_SENTINEL).expect("session"),
            })
            .is_err()
    );
    assert!(
        write
            .authorize_selector(&ScopeSelector::InstanceGlobal {})
            .is_err()
    );

    let forget = client
        .scopes
        .authorize_mutation_set(&client.context, MutationOperation::Forget)
        .expect("forget authority");
    assert!(
        forget
            .authorize_selector(&ScopeSelector::Session {
                session_id: SessionId::new(SESSION_SENTINEL).expect("session"),
            })
            .is_ok()
    );
    assert!(
        forget
            .authorize_selector(&ScopeSelector::InstanceGlobal {})
            .is_ok()
    );
    assert!(
        forget
            .authorize_selector(&ScopeSelector::Principal {})
            .is_err()
    );

    let mut cross_principal = client.context.clone();
    cross_principal.principal_id =
        PrincipalId::new("prn_cross_principal").expect("cross principal");
    assert!(client.scopes.authorize_read(&cross_principal).is_err());
    assert!(
        client
            .scopes
            .authorize_mutation_set(&cross_principal, MutationOperation::Create)
            .is_err()
    );

    let mut read_only_document = config_value("127.0.0.1:0", root.path(), VALID_TOKEN);
    read_only_document["clients"][0]["permissions"] = json!({
        "read": true,
        "write": [],
        "forget": []
    });
    let read_only =
        ServeConfig::from_slice(&json_bytes(read_only_document)).expect("read-only profile");
    let read_only_client = &read_only.clients[0];
    assert_eq!(
        read_only_client.context.grants,
        BTreeSet::from([Grant::new("memory:read").expect("read grant")])
    );
    assert!(read_only_client.mutation_policy.is_none());
    assert!(
        read_only_client
            .scopes
            .authorize_mutation_set(&read_only_client.context, MutationOperation::Create)
            .is_err()
    );
    assert!(
        read_only_client
            .scopes
            .authorize_mutation_set(&read_only_client.context, MutationOperation::Forget)
            .is_err()
    );
}

#[test]
fn config_rejects_old_unknown_duplicate_contradictory_cross_principal_or_secret_authority() {
    let root = TempDir::new().expect("temporary data parent");
    let document = config_value("127.0.0.1:0", root.path(), VALID_TOKEN);

    let mut unknown_version = document.clone();
    unknown_version["configVersion"] = Value::String("jiandu.service.config/v9".to_owned());
    assert!(ServeConfig::from_slice(&json_bytes(unknown_version)).is_err());

    let mut uppercase_digest = document.clone();
    uppercase_digest["clients"][0]["bearerTokenDigest"] =
        Value::String(format!("sha256:{}", "A".repeat(64)));
    assert!(ServeConfig::from_slice(&json_bytes(uppercase_digest)).is_err());

    let mut duplicate = document.clone();
    duplicate["clients"] = Value::Array(vec![
        document["clients"][0].clone(),
        document["clients"][0].clone(),
    ]);
    assert!(ServeConfig::from_slice(&json_bytes(duplicate)).is_err());

    let mut duplicate_scope = document.clone();
    duplicate_scope["clients"][0]["permissions"]["write"] = json!(["principal", "principal"]);
    assert!(ServeConfig::from_slice(&json_bytes(duplicate_scope)).is_err());

    let mut duplicate_exact_scope = document.clone();
    duplicate_exact_scope["clients"][0]["scopes"]["projectIds"] =
        json!([PROJECT_SENTINEL, PROJECT_SENTINEL]);
    assert!(ServeConfig::from_slice(&json_bytes(duplicate_exact_scope)).is_err());

    let mut contradictory_scope = document.clone();
    contradictory_scope["clients"][0]["permissions"]["write"] = json!(["project"]);
    assert!(ServeConfig::from_slice(&json_bytes(contradictory_scope)).is_err());

    let mut no_read = document.clone();
    no_read["clients"][0]["permissions"]["read"] = Value::Bool(false);
    assert!(ServeConfig::from_slice(&json_bytes(no_read)).is_err());

    let mut unknown_permission = document.clone();
    unknown_permission["clients"][0]["permissions"]["admin"] = Value::Bool(true);
    assert!(ServeConfig::from_slice(&json_bytes(unknown_permission)).is_err());

    let mut unknown_scope_kind = document.clone();
    unknown_scope_kind["clients"][0]["permissions"]["write"] = json!(["tenant"]);
    assert!(ServeConfig::from_slice(&json_bytes(unknown_scope_kind)).is_err());

    let mut old_schema = document.clone();
    old_schema
        .as_object_mut()
        .expect("config object")
        .remove("configVersion");
    let old_client = old_schema["clients"][0]
        .as_object_mut()
        .expect("client object");
    old_client.remove("permissions");
    old_client.insert("grants".to_owned(), json!(["memory:read"]));
    old_client.insert(
        "mutationPolicy".to_owned(),
        json!({
            "maxBodyBytes": 4096,
            "allowedTypes": ["decision"],
            "allowedScopes": ["principal"],
            "secretContentPolicy": "allow_all"
        }),
    );
    assert!(ServeConfig::from_slice(&json_bytes(old_schema)).is_err());

    let mut old_policy_sources = document.clone();
    old_policy_sources["clients"][0]["grants"] = json!(["memory:read"]);
    old_policy_sources["clients"][0]["mutationPolicy"] = json!({
        "maxBodyBytes": 1,
        "allowedTypes": ["decision"],
        "allowedScopes": ["principal"],
        "secretContentPolicy": "allow_all"
    });
    assert!(ServeConfig::from_slice(&json_bytes(old_policy_sources)).is_err());

    let cross_principal_sentinel = "prn_forbidden_config_injection";
    let mut cross_principal = document.clone();
    cross_principal["clients"][0]["scopes"]["principalId"] =
        Value::String(cross_principal_sentinel.to_owned());
    let error = ServeConfig::from_slice(&json_bytes(cross_principal))
        .expect_err("cross-principal scope injection rejected");
    assert!(!format!("{error:?} {error}").contains(cross_principal_sentinel));

    let raw_token_sentinel = "RAW_BEARER_CONFIG_SECRET_SENTINEL";
    let mut raw_token = document;
    raw_token["clients"][0]["bearerToken"] = Value::String(raw_token_sentinel.to_owned());
    let error =
        ServeConfig::from_slice(&json_bytes(raw_token)).expect_err("raw bearer field rejected");
    assert!(!format!("{error:?} {error}").contains(raw_token_sentinel));
}

#[test]
fn config_versions_keep_v01_closed_and_require_a_bounded_v02_shutdown_object() {
    let root = TempDir::new().expect("temporary data parent");
    let legacy = config_value("127.0.0.1:0", root.path(), VALID_TOKEN);
    let legacy_config =
        ServeConfig::from_slice(&json_bytes(legacy.clone())).expect("strict v0.1 config");
    assert_eq!(
        legacy_config.drain_timeout,
        std::time::Duration::from_secs(5)
    );

    let mut legacy_with_new_field = legacy.clone();
    legacy_with_new_field["shutdown"] = json!({ "drainTimeoutMs": 10 });
    assert!(
        ServeConfig::from_slice(&json_bytes(legacy_with_new_field)).is_err(),
        "v0.1 must remain a closed schema"
    );

    let mut current = legacy;
    current["configVersion"] = Value::String("jiandu.service.config/v0.2".to_owned());
    assert!(
        ServeConfig::from_slice(&json_bytes(current.clone())).is_err(),
        "v0.2 requires an explicit shutdown bound"
    );

    for invalid in [0_u64, 9, 60_001, u64::MAX] {
        let mut candidate = current.clone();
        candidate["shutdown"] = json!({ "drainTimeoutMs": invalid });
        assert!(ServeConfig::from_slice(&json_bytes(candidate)).is_err());
    }

    for valid in [10_u64, 50, 5_000, 60_000] {
        let mut candidate = current.clone();
        candidate["shutdown"] = json!({ "drainTimeoutMs": valid });
        let parsed = ServeConfig::from_slice(&json_bytes(candidate)).expect("bounded v0.2");
        assert_eq!(
            parsed.drain_timeout,
            std::time::Duration::from_millis(valid)
        );
    }

    let mut unknown = current;
    unknown["shutdown"] = json!({ "drainTimeoutMs": 50, "reason": "SECRET_SENTINEL" });
    let error = ServeConfig::from_slice(&json_bytes(unknown)).expect_err("unknown field rejected");
    assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
}

#[tokio::test]
async fn startup_never_initializes_a_missing_store_or_binds_before_readiness() {
    let root = TempDir::new().expect("temporary data parent");
    let missing = root.path().join("missing-store");
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let bind = reserved.local_addr().expect("reserved address");
    drop(reserved);

    let error =
        RunningDaemon::start(config(&bind.to_string(), &missing, VALID_TOKEN).expect("config"))
            .await
            .expect_err("missing store must fail");
    assert!(matches!(error, DaemonError::StoreFailure { .. }));
    assert!(!missing.exists(), "serve must never initialize a store");
    let listener = std::net::TcpListener::bind(bind).expect("listener was never bound");
    drop(listener);
}

#[tokio::test]
async fn unauthenticated_http_is_fixed_redacted_and_never_constructs_an_mcp_handler() {
    let root = initialized_store();
    let daemon =
        RunningDaemon::start(config("127.0.0.1:0", root.path(), VALID_TOKEN).expect("config"))
            .await
            .expect("start daemon");
    let client = reqwest::Client::new();
    let base = format!("http://{}", daemon.local_addr());

    for route in [LIVENESS_ROUTE, READINESS_ROUTE] {
        let response = client
            .get(format!("{base}{route}"))
            .send()
            .await
            .expect("probe response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("probe body");
        for private in [
            root.path().to_string_lossy().as_ref(),
            VALID_TOKEN,
            PRINCIPAL_SENTINEL,
            CLIENT_SENTINEL,
        ] {
            assert!(!body.contains(private), "probe leaked private material");
        }
    }
    assert_eq!(daemon.handler_construction_count(), 0);

    let cases = [None, Some("Basic Zm9vOmJhcg=="), Some("Bearer  malformed")];
    for authorization in cases {
        let mut request = client.post(format!("{base}{MCP_ROUTE}")).body("{}");
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        assert_fixed_unauthorized(request.send().await.expect("401 response"), root.path()).await;
    }
    let response = client
        .post(format!("{base}{MCP_ROUTE}"))
        .header("authorization", format!("Bearer {VALID_TOKEN}"))
        .header("authorization", format!("Bearer {VALID_TOKEN}"))
        .body("{}")
        .send()
        .await
        .expect("multiple authorization response");
    assert_fixed_unauthorized(response, root.path()).await;

    let invalid_transport = client_transport(&base, INVALID_TOKEN);
    assert!(
        V2025Client.serve(invalid_transport).await.is_err(),
        "invalid rmcp credential must fail initialize"
    );
    assert_eq!(daemon.handler_construction_count(), 0);
    daemon.shutdown().await.expect("shutdown daemon");
}

#[tokio::test]
async fn authenticated_streamable_http_discovers_reads_and_durably_mutates_one_store() {
    let root = initialized_store();
    let daemon =
        RunningDaemon::start(config("127.0.0.1:0", root.path(), VALID_TOKEN).expect("config"))
            .await
            .expect("start daemon");
    let base = format!("http://{}", daemon.local_addr());
    let client = V2025Client
        .serve(client_transport(&base, VALID_TOKEN))
        .await
        .expect("authenticated initialize");
    let tools = client.list_all_tools().await.expect("discover tools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        [
            "memory_forget",
            "memory_get",
            "memory_list",
            "memory_remember",
            "memory_search",
            "memory_update",
        ]
    );

    let maximum_body = "x".repeat(MAX_BODY_BYTES);
    let remember = RememberMemoryCommand {
        scope: ScopeSelector::Principal {},
        memory_type: MemoryType::Reference,
        title: "Daemon E2E decision".to_owned(),
        summary: Some("Authenticated singleton transport".to_owned()),
        body: maximum_body.clone(),
        tags: vec![Tag::new("daemon").expect("tag")],
        provenance: ProvenanceInput::default(),
        relations: Vec::new(),
        idempotency_key: IdempotencyKey::new("daemon-e2e-remember-key").expect("key"),
    };
    let remembered = client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&remember)),
        )
        .await
        .expect("remember tool");
    let remembered: ResultEnvelope<RememberMemoryResult> = success_envelope(&remembered);
    assert!(!remembered.result.idempotent_replay);

    let get = MemoryGetRequest {
        memory_id: remembered.result.record.id.clone(),
    };
    let fetched = client
        .call_tool(CallToolRequestParams::new("memory_get").with_arguments(arguments(&get)))
        .await
        .expect("get tool");
    let fetched: ResultEnvelope<MemoryRecord> = success_envelope(&fetched);
    assert_eq!(fetched.result, remembered.result.record);
    assert_eq!(fetched.result.body, maximum_body);
    assert!(daemon.handler_construction_count() > 0);

    let readiness = reqwest::get(format!("{base}{READINESS_ROUTE}"))
        .await
        .expect("readiness")
        .text()
        .await
        .expect("readiness body");
    let readiness: Value = serde_json::from_str(&readiness).expect("readiness JSON");
    assert_eq!(readiness["status"], "ready");
    assert_eq!(readiness["health"]["store"], "ready");
    assert_eq!(readiness["health"]["index"], "missing");
    assert_eq!(readiness["health"]["exactRead"], true);
    assert_eq!(readiness["health"]["list"], true);
    assert_eq!(readiness["health"]["search"], false);

    client.cancel().await.expect("cancel client");
    daemon.shutdown().await.expect("shutdown daemon");
    let reopened = CanonicalStore::open(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("durable store reopens after daemon lock release");
    assert_eq!(
        reopened.watermark().expect("durable watermark"),
        remembered.store_revision
    );
}

#[tokio::test]
async fn corrupt_disposable_index_keeps_process_and_canonical_reads_ready() {
    let root = initialized_store();
    fs::write(
        root.path().join("index/lexical.sqlite"),
        b"not a valid lexical index",
    )
    .expect("write corrupt disposable index");
    let daemon =
        RunningDaemon::start(config("127.0.0.1:0", root.path(), VALID_TOKEN).expect("config"))
            .await
            .expect("degraded index must not stop daemon");
    let response = reqwest::get(format!("http://{}{}", daemon.local_addr(), READINESS_ROUTE))
        .await
        .expect("readiness response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.expect("readiness body"))
        .expect("readiness JSON");
    assert_eq!(body["status"], "ready");
    assert_eq!(body["health"]["store"], "ready");
    assert_eq!(body["health"]["index"], "degraded");
    assert_eq!(body["health"]["exactRead"], true);
    assert_eq!(body["health"]["list"], true);
    assert_eq!(body["health"]["search"], false);
    assert_eq!(daemon.handler_construction_count(), 0);
    daemon.shutdown().await.expect("shutdown daemon");
}

#[tokio::test]
async fn second_writer_reports_safe_owner_without_changing_the_owned_store() {
    let root = initialized_store();
    let first = RunningDaemon::start(
        config("127.0.0.1:0", root.path(), VALID_TOKEN).expect("first config"),
    )
    .await
    .expect("first daemon");
    let before = snapshot_tree(root.path());
    let second = RunningDaemon::start(
        config("127.0.0.1:0", root.path(), INVALID_TOKEN).expect("second config"),
    )
    .await
    .expect_err("second writer rejected");
    let diagnostic = format!("{second:?} {second}");
    assert!(matches!(second, DaemonError::StoreLocked { .. }));
    assert!(!diagnostic.contains(root.path().to_string_lossy().as_ref()));
    assert!(!diagnostic.contains(VALID_TOKEN));
    assert!(!diagnostic.contains(INVALID_TOKEN));
    assert!(!diagnostic.contains(PRINCIPAL_SENTINEL));
    assert_eq!(snapshot_tree(root.path()), before);

    first.shutdown().await.expect("shutdown first daemon");
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("lock released after shutdown"),
    );
}

#[tokio::test]
async fn unexpected_http_task_exit_is_observed_and_releases_the_store_lock() {
    let root = initialized_store();
    let mut daemon =
        RunningDaemon::start(config("127.0.0.1:0", root.path(), VALID_TOKEN).expect("config"))
            .await
            .expect("start daemon");

    daemon.abort_transport_for_test();
    assert!(matches!(
        daemon.wait().await,
        Err(DaemonError::RuntimeUnavailable)
    ));
    drop(daemon);

    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("aborted HTTP task released the singleton lock"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_linearizes_with_session_initialize_and_preserves_auth_precedence() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 1_000).expect("config"),
    )
    .await
    .expect("start daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let pause = Arc::new(HttpAdmissionPause::default());
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    daemon.install_http_admission_pause(pause.clone());
    let base = format!("http://{}", daemon.local_addr());
    let http = reqwest::Client::new();

    let initialize = tokio::spawn({
        let http = http.clone();
        let base = base.clone();
        async move {
            authorized_mcp_post(&http, &base, initialize_request())
                .send()
                .await
        }
    });
    wait_http_pause(pause.clone()).await;
    assert_eq!(lifecycle.active_operations(), 1);

    // `shutdown()` closes the gate synchronously before returning its future.
    let shutdown = tokio::spawn(daemon.shutdown());
    assert!(!lifecycle.is_accepting());

    let invalid = http
        .post(format!("{base}{MCP_ROUTE}"))
        .body("{}")
        .send()
        .await
        .expect("invalid credential response");
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        invalid.text().await.expect("401 body"),
        r#"{"error":"unauthorized"}"#
    );
    assert_fixed_unavailable(
        authorized_mcp_post(&http, &base, json!({}))
            .send()
            .await
            .expect("draining response"),
        root.path(),
    )
    .await;

    pause.release();
    let initialize = initialize
        .await
        .expect("initialize task")
        .expect("initialize response");
    assert_eq!(initialize.status(), StatusCode::OK);
    let _ = initialize.bytes().await.expect("initialize response body");
    assert_eq!(
        shutdown.await.expect("shutdown task").expect("shutdown"),
        ShutdownOutcome::Drained
    );
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("normal drain releases the store lock"),
    );
}

#[tokio::test]
async fn detached_readiness_snapshot_never_retains_the_canonical_store_owner() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 1_000).expect("config"),
    )
    .await
    .expect("start daemon");
    let health = daemon.health_snapshot_for_test();
    assert_eq!(health.current().store(), jiandu_mcp::StoreReadHealth::Ready);

    assert_eq!(
        daemon.shutdown().await.expect("shutdown"),
        ShutdownOutcome::Drained
    );
    // Keep the exact observer alive while proving that it owns only the
    // sanitized health value, not CanonicalReadBackend or CanonicalStore.
    assert_eq!(health.current().store(), jiandu_mcp::StoreReadHealth::Ready);
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("health snapshot cannot retain the singleton lock"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_drain_flushes_a_produced_final_frame_before_reporting_drained() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 1_000).expect("config"),
    )
    .await
    .expect("start daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let pause = Arc::new(ResponseFramePause::default());
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    daemon.install_final_frame_pause(pause.clone());
    let base = format!("http://{}", daemon.local_addr());
    let response = tokio::spawn(async move {
        let response = authorized_mcp_post(&reqwest::Client::new(), &base, initialize_request())
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, body))
    });
    wait_response_frame_pause(pause.clone()).await;
    assert_eq!(
        lifecycle.active_operations(),
        0,
        "the final frame has left PermitBody but is not yet available to Hyper"
    );

    let mut shutdown = tokio::spawn(daemon.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "Drained must wait for Axum to flush the produced final frame"
    );
    pause.release();
    let (status, body) = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .expect("response timeout")
        .expect("response task")
        .expect("complete response body");
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.is_empty(),
        "normal drain delivered the complete response"
    );
    assert_eq!(
        shutdown.await.expect("shutdown task").expect("shutdown"),
        ShutdownOutcome::Drained
    );
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("normal final-frame drain releases the store lock"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_timeout_cancels_incomplete_authenticated_upload_and_releases_lock() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 30).expect("config"),
    )
    .await
    .expect("start daemon");
    let before = snapshot_tree(root.path());
    let lifecycle = daemon.lifecycle_for_test();
    let address = daemon.local_addr();
    let mut socket = TcpStream::connect(address)
        .await
        .expect("connect raw authenticated client");
    let request = format!(
        "POST {MCP_ROUTE} HTTP/1.1\r\n\
         Host: {address}\r\n\
         Authorization: Bearer {VALID_TOKEN}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Length: 1048576\r\n\
         Connection: keep-alive\r\n\
         \r\n{{"
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("send incomplete authenticated body");
    wait_for_active_operations(&lifecycle, 1).await;

    let started = tokio::time::Instant::now();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), daemon.shutdown())
            .await
            .expect("incomplete upload cannot strand shutdown")
            .expect("forced shutdown"),
        ShutdownOutcome::ForcedAfterTimeout
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "connection-level force cancellation must remain bounded"
    );
    assert_eq!(lifecycle.active_operations(), 0);
    assert_eq!(lifecycle.active_backend_operations(), 0);
    assert_eq!(
        snapshot_tree(root.path()),
        before,
        "partial body is zero-write"
    );

    // Keep the peer socket alive while proving no detached Router/connection
    // clone retains the canonical owner.
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("forced partial upload releases the singleton lock"),
    );
    let mut byte = [0_u8; 1];
    match tokio::time::timeout(Duration::from_millis(200), socket.read(&mut byte)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("forced request must not receive an MCP acknowledgement"),
        Err(_) => panic!("server did not close the forced raw connection"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn normal_drain_waits_for_concurrent_mutation_and_read_before_acknowledged_shutdown() {
    let root = initialized_store();
    let pause = BoundaryPause::at(PersistenceBoundary::MetadataDirectorySynced, false);
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    let daemon = RunningDaemon::start_with_store_options_for_test(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 2_000).expect("config"),
        StoreOptions::with_failpoint_injector(pause.clone()),
    )
    .await
    .expect("start paused daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let base = format!("http://{}", daemon.local_addr());
    let client = Arc::new(
        V2025Client
            .serve(client_transport(&base, VALID_TOKEN))
            .await
            .expect("authenticated initialize"),
    );
    let read_client = Arc::new(
        V2025Client
            .serve(client_transport(&base, VALID_TOKEN))
            .await
            .expect("second authenticated initialize"),
    );
    let command = remember_command("shutdown-normal-key", "normal drain body");
    let remember = tokio::spawn({
        let client = client.clone();
        let command = command.clone();
        async move {
            client
                .call_tool(
                    CallToolRequestParams::new("memory_remember")
                        .with_arguments(arguments(&command)),
                )
                .await
        }
    });
    wait_boundary_pause(pause.clone()).await;

    let list = tokio::spawn({
        let client = read_client.clone();
        async move {
            client
                .call_tool(
                    CallToolRequestParams::new("memory_list").with_arguments(
                        json!({
                            "scopes": [{ "kind": "principal" }],
                            "sort": "id_asc",
                            "limit": 100
                        })
                        .as_object()
                        .expect("list arguments")
                        .clone(),
                    ),
                )
                .await
        }
    });
    // Each independent call owns one HTTP response permit and one backend
    // worker permit. Waiting for all four proves both requests linearized
    // before shutdown closes admission; the list worker is then blocked on
    // the canonical writer rather than merely queued in the client transport.
    wait_for_active_operations(&lifecycle, 4).await;

    let shutdown = tokio::spawn(daemon.shutdown());
    assert_fixed_unavailable(
        authorized_mcp_post(&reqwest::Client::new(), &base, json!({}))
            .send()
            .await
            .expect("new request rejected during drain"),
        root.path(),
    )
    .await;
    pause.release();

    let remembered = tokio::time::timeout(Duration::from_secs(5), remember)
        .await
        .expect("remember response timeout")
        .expect("remember task")
        .expect("remember response");
    let remembered: ResultEnvelope<RememberMemoryResult> = success_envelope(&remembered);
    let listed = tokio::time::timeout(Duration::from_secs(5), list)
        .await
        .expect("list response timeout")
        .expect("list task")
        .expect("list response");
    let listed: ResultEnvelope<MemoryListResult> = success_envelope(&listed);
    assert!(
        listed
            .result
            .memories
            .iter()
            .any(|item| item.id == remembered.result.record.id)
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown timeout")
            .expect("shutdown task")
            .expect("shutdown"),
        ShutdownOutcome::Drained
    );
    drop(client);
    drop(read_client);
    let reopened = CanonicalStore::open(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("normal concurrent drain releases lock");
    assert_eq!(
        reopened.watermark().expect("watermark"),
        remembered.store_revision
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_read_contention_does_not_starve_the_shutdown_deadline() {
    let root = initialized_store();
    let pause = BoundaryPause::at(PersistenceBoundary::MetadataDirectorySynced, false);
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    let daemon = RunningDaemon::start_with_store_options_for_test(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 30).expect("config"),
        StoreOptions::with_failpoint_injector(pause.clone()),
    )
    .await
    .expect("start paused daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let base = format!("http://{}", daemon.local_addr());
    let writer = Arc::new(
        V2025Client
            .serve(client_transport_no_retry(&base, VALID_TOKEN))
            .await
            .expect("writer initialize"),
    );
    let reader = Arc::new(
        V2025Client
            .serve(client_transport_no_retry(&base, VALID_TOKEN))
            .await
            .expect("reader initialize"),
    );
    let remember = tokio::spawn({
        let writer = writer.clone();
        async move {
            writer
                .call_tool(
                    CallToolRequestParams::new("memory_remember").with_arguments(arguments(
                        &remember_command("single-worker-write", "single worker body"),
                    )),
                )
                .await
        }
    });
    wait_boundary_pause(pause.clone()).await;
    let _timed_release = TimedPauseReleaseGuard::new(pause.clone(), Duration::from_millis(750));
    let list = tokio::spawn({
        let reader = reader.clone();
        async move {
            reader
                .call_tool(
                    CallToolRequestParams::new("memory_list").with_arguments(
                        json!({
                            "scopes": [{ "kind": "principal" }],
                            "sort": "id_asc",
                            "limit": 100
                        })
                        .as_object()
                        .expect("list arguments")
                        .clone(),
                    ),
                )
                .await
        }
    });
    wait_for_active_operations(&lifecycle, 4).await;

    let started = tokio::time::Instant::now();
    let shutdown = tokio::spawn(daemon.shutdown());
    wait_until_forced(&lifecycle).await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "canonical reads must not park the only Tokio runtime worker"
    );
    assert_eq!(lifecycle.active_backend_operations(), 2);
    pause.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown timeout")
            .expect("shutdown task")
            .expect("shutdown"),
        ShutdownOutcome::ForcedAfterTimeout
    );
    remember.abort();
    list.abort();
    drop(writer);
    drop(reader);
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("single-worker forced drain releases the lock"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_timeout_rechecks_pre_wal_admission_and_never_writes_or_acknowledges() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 30).expect("config"),
    )
    .await
    .expect("start daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let session_managers = daemon.session_managers_for_test();
    let pause = Arc::new(HttpAdmissionPause::default());
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    daemon.install_commit_admission_pause(pause.clone());
    let before = snapshot_tree(root.path());
    let base = format!("http://{}", daemon.local_addr());
    let client = Arc::new(
        V2025Client
            .serve(client_transport_no_retry(&base, VALID_TOKEN))
            .await
            .expect("authenticated initialize"),
    );
    let command = remember_command("shutdown-pre-wal-key", "PRE_WAL_BODY_SENTINEL");
    let mut call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool(
                    CallToolRequestParams::new("memory_remember")
                        .with_arguments(arguments(&command)),
                )
                .await
        }
    });
    wait_http_pause(pause.clone()).await;
    assert_eq!(snapshot_tree(root.path()), before, "WAL must not exist yet");

    let shutdown = tokio::spawn(daemon.shutdown());
    wait_until_forced(&lifecycle).await;
    wait_for_forced_transport_cleanup(&lifecycle, &session_managers).await;
    match tokio::time::timeout(Duration::from_millis(100), &mut call).await {
        Ok(Ok(Ok(_))) => panic!("unpersisted mutation must not be acknowledged"),
        Ok(Ok(Err(_))) | Ok(Err(_)) => {}
        Err(_) => {
            // rmcp's caller waiter may outlive its already-closed server-side
            // session. The server proof above is authoritative; abandon the
            // local waiter after proving it received no acknowledgement.
            call.abort();
        }
    }
    pause.release();
    assert_eq!(
        shutdown.await.expect("shutdown task").expect("shutdown"),
        ShutdownOutcome::ForcedAfterTimeout
    );
    drop(client);
    assert_eq!(
        snapshot_tree(root.path()),
        before,
        "forced pre-WAL path is zero-write"
    );
    let reopened = CanonicalStore::open(
        root.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("forced timeout releases lock");
    assert_eq!(
        reopened.watermark().expect("watermark"),
        jiandu_core::StoreRevision(0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_grace_deadline_covers_post_idle_session_and_transport_cleanup() {
    let root = initialized_store();
    let mut daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 50).expect("config"),
    )
    .await
    .expect("start daemon");
    daemon.install_graceful_cleanup_delay_for_test(Duration::from_secs(2));
    let managers = daemon.session_managers_for_test();
    let base = format!("http://{}", daemon.local_addr());
    let client = V2025Client
        .serve(client_transport_no_retry(&base, VALID_TOKEN))
        .await
        .expect("authenticated idle session");

    let started = tokio::time::Instant::now();
    assert_eq!(
        daemon.shutdown().await.expect("forced shutdown"),
        ShutdownOutcome::ForcedAfterTimeout
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the post-idle cleanup delay must not extend or reclassify response grace"
    );
    for manager in managers {
        assert!(
            manager.sessions.read().await.is_empty(),
            "forced cleanup closes every tracked session"
        );
    }
    drop(client);
    drop(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("forced post-idle cleanup releases the store lock"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn forced_post_wal_disconnect_releases_lock_and_restart_replays_exact_commit() {
    let root = initialized_store();
    let pause = BoundaryPause::at(PersistenceBoundary::MetadataRenamed, true);
    let _pause_release = PauseReleaseGuard::new(pause.clone());
    let daemon = RunningDaemon::start_with_store_options_for_test(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 30).expect("config"),
        StoreOptions::with_failpoint_injector(pause.clone()),
    )
    .await
    .expect("start paused daemon");
    let lifecycle = daemon.lifecycle_for_test();
    let session_managers = daemon.session_managers_for_test();
    let base = format!("http://{}", daemon.local_addr());
    let client = Arc::new(
        V2025Client
            .serve(client_transport_no_retry(&base, VALID_TOKEN))
            .await
            .expect("authenticated initialize"),
    );
    let command = remember_command("shutdown-post-wal-key", "POST_WAL_BODY_SENTINEL");
    let mut call = tokio::spawn({
        let client = client.clone();
        let command = command.clone();
        async move {
            client
                .call_tool(
                    CallToolRequestParams::new("memory_remember")
                        .with_arguments(arguments(&command)),
                )
                .await
        }
    });
    wait_boundary_pause(pause.clone()).await;

    let shutdown = tokio::spawn(daemon.shutdown());
    wait_until_forced(&lifecycle).await;
    wait_for_forced_transport_cleanup(&lifecycle, &session_managers).await;
    match tokio::time::timeout(Duration::from_millis(100), &mut call).await {
        Ok(Ok(Ok(_))) => panic!("late commit must not receive an acknowledgement"),
        Ok(Ok(Err(_))) | Ok(Err(_)) => {}
        Err(_) => call.abort(),
    }
    assert!(matches!(
        CanonicalStore::open(
            root.path(),
            LockOwner::for_current_process().expect("competing lock owner"),
        ),
        Err(jiandu_store::StoreError::StoreLocked { .. })
    ));
    pause.release();
    assert_eq!(
        shutdown.await.expect("shutdown task").expect("shutdown"),
        ShutdownOutcome::ForcedAfterTimeout
    );
    drop(client);

    let restarted = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 1_000).expect("restart config"),
    )
    .await
    .expect("immediate restart recovers interrupted transaction");
    let restarted_base = format!("http://{}", restarted.local_addr());
    let replay_client = V2025Client
        .serve(client_transport(&restarted_base, VALID_TOKEN))
        .await
        .expect("restarted client");
    let replay = replay_client
        .call_tool(
            CallToolRequestParams::new("memory_remember").with_arguments(arguments(&command)),
        )
        .await
        .expect("same-key retry");
    let replay: ResultEnvelope<RememberMemoryResult> = success_envelope(&replay);
    assert!(replay.result.idempotent_replay);
    assert_eq!(replay.result.record.body, "POST_WAL_BODY_SENTINEL");
    replay_client.cancel().await.expect("cancel replay client");
    assert_eq!(
        restarted.shutdown().await.expect("restart shutdown"),
        ShutdownOutcome::Drained
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_shutdown_waiter_does_not_drop_the_cleanup_supervisor() {
    let root = initialized_store();
    let daemon = RunningDaemon::start(
        config_v02("127.0.0.1:0", root.path(), VALID_TOKEN, 100).expect("config"),
    )
    .await
    .expect("start daemon");
    let waiter = tokio::spawn(daemon.shutdown());
    waiter.abort();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match CanonicalStore::open(
                root.path(),
                LockOwner::for_current_process().expect("lock owner"),
            ) {
                Ok(store) => break drop(store),
                Err(jiandu_store::StoreError::StoreLocked { .. }) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("unexpected reopen error: {error:?}"),
            }
        }
    })
    .await
    .expect("detached supervisor releases the store lock");
}

fn initialized_store() -> TempDir {
    let root = TempDir::new().expect("temporary store");
    drop(
        CanonicalStore::initialize(
            root.path(),
            LockOwner::for_current_process().expect("lock owner"),
        )
        .expect("initialize fixture store"),
    );
    root
}

fn config(bind: &str, data_dir: &Path, token: &str) -> Result<ServeConfig, crate::ConfigError> {
    ServeConfig::from_slice(&json_bytes(config_value(bind, data_dir, token)))
}

fn config_v02(
    bind: &str,
    data_dir: &Path,
    token: &str,
    drain_timeout_ms: u64,
) -> Result<ServeConfig, crate::ConfigError> {
    let mut value = config_value(bind, data_dir, token);
    value["configVersion"] = Value::String("jiandu.service.config/v0.2".to_owned());
    value["shutdown"] = json!({ "drainTimeoutMs": drain_timeout_ms });
    ServeConfig::from_slice(&json_bytes(value))
}

fn config_value(bind: &str, data_dir: &Path, token: &str) -> Value {
    json!({
        "configVersion": "jiandu.service.config/v0.1",
        "bind": bind,
        "dataDir": data_dir,
        "cursorMacKey": format!("hmac-sha256:{}", "11".repeat(32)),
        "clients": [{
            "bearerTokenDigest": format!("sha256:{}", lower_hex(&bearer_digest(token.as_bytes()))),
            "principalId": PRINCIPAL_SENTINEL,
            "clientId": CLIENT_SENTINEL,
            "scopes": {
                "projectIds": [],
                "sessionIds": [],
                "instanceGlobal": false
            },
            "permissions": {
                "read": true,
                "write": ["principal"],
                "forget": ["principal"]
            },
            "creationActor": "host"
        }]
    })
}

fn json_bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("config JSON")
}

fn remember_command(key: &str, body: &str) -> RememberMemoryCommand {
    RememberMemoryCommand {
        scope: ScopeSelector::Principal {},
        memory_type: MemoryType::Reference,
        title: "Shutdown lifecycle fixture".to_owned(),
        summary: Some("Bounded daemon drain".to_owned()),
        body: body.to_owned(),
        tags: vec![Tag::new("shutdown").expect("tag")],
        provenance: ProvenanceInput::default(),
        relations: Vec::new(),
        idempotency_key: IdempotencyKey::new(key).expect("idempotency key"),
    }
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "shutdown-race-fixture", "version": "1.0.0" }
        }
    })
}

fn authorized_mcp_post(
    client: &reqwest::Client,
    base: &str,
    body: Value,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}{MCP_ROUTE}"))
        .header("authorization", format!("Bearer {VALID_TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body)
}

async fn assert_fixed_unavailable(response: reqwest::Response, data_dir: &Path) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get("cache-control").expect("no-store"),
        "no-store"
    );
    let body = response.text().await.expect("503 body");
    assert_eq!(body, r#"{"error":"service_unavailable"}"#);
    for private in [
        data_dir.to_string_lossy().as_ref(),
        VALID_TOKEN,
        INVALID_TOKEN,
        PRINCIPAL_SENTINEL,
        CLIENT_SENTINEL,
        "PRE_WAL_BODY_SENTINEL",
        "POST_WAL_BODY_SENTINEL",
    ] {
        assert!(!body.contains(private), "503 leaked private material");
    }
}

async fn wait_http_pause(pause: Arc<HttpAdmissionPause>) {
    let waiter_pause = pause.clone();
    let waiter = tokio::task::spawn_blocking(move || waiter_pause.wait_reached());
    match tokio::time::timeout(Duration::from_secs(2), waiter).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => panic!("HTTP admission pause released before it was reached"),
        Ok(Err(_)) => panic!("HTTP admission pause waiter failed"),
        Err(_) => {
            pause.release();
            panic!("HTTP admission pause was not reached within the test bound");
        }
    }
}

async fn wait_response_frame_pause(pause: Arc<ResponseFramePause>) {
    let waiter_pause = pause.clone();
    let waiter = tokio::task::spawn_blocking(move || waiter_pause.wait_reached());
    match tokio::time::timeout(Duration::from_secs(2), waiter).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => panic!("response-frame pause released before it was reached"),
        Ok(Err(_)) => panic!("response-frame pause waiter failed"),
        Err(_) => {
            pause.release();
            panic!("response-frame pause was not reached within the test bound");
        }
    }
}

async fn wait_boundary_pause(pause: Arc<BoundaryPause>) {
    let waiter_pause = pause.clone();
    let waiter = tokio::task::spawn_blocking(move || waiter_pause.wait_reached());
    match tokio::time::timeout(Duration::from_secs(2), waiter).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => panic!("persistence pause released before it was reached"),
        Ok(Err(_)) => panic!("persistence pause waiter failed"),
        Err(_) => {
            pause.release();
            panic!("persistence pause was not reached within the test bound");
        }
    }
}

async fn wait_for_active_operations(lifecycle: &crate::lifecycle::LifecycleGate, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while lifecycle.active_operations() < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected lifecycle operations entered");
}

async fn wait_until_forced(lifecycle: &crate::lifecycle::LifecycleGate) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !lifecycle.is_forced() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown reached forced phase");
}

async fn wait_for_forced_transport_cleanup(
    lifecycle: &crate::lifecycle::LifecycleGate,
    session_managers: &[Arc<
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
    >],
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let mut session_count = 0;
            for manager in session_managers {
                session_count += manager.sessions.read().await.len();
            }
            if lifecycle.active_operations() == 1 && session_count == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("forced transport cleanup is bounded and leaves only the canonical worker");
}

fn client_transport(base: &str, token: &str) -> StreamableHttpClientTransport<reqwest::Client> {
    client_transport_config(base, token, false)
}

fn client_transport_no_retry(
    base: &str,
    token: &str,
) -> StreamableHttpClientTransport<reqwest::Client> {
    client_transport_config(base, token, true)
}

fn client_transport_config(
    base: &str,
    token: &str,
    disable_sse_retry: bool,
) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut headers = HashMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization header"),
    );
    let mut config = StreamableHttpClientTransportConfig::with_uri(format!("{base}{MCP_ROUTE}"))
        .custom_headers(headers);
    if disable_sse_retry {
        config.retry_config = Arc::new(NeverRetry::default());
    }
    StreamableHttpClientTransport::from_config(config)
}

async fn assert_fixed_unauthorized(response: reqwest::Response, data_dir: &Path) {
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .expect("challenge"),
        "Bearer"
    );
    assert_eq!(
        response.headers().get("cache-control").expect("no-store"),
        "no-store"
    );
    let body = response.text().await.expect("401 body");
    assert_eq!(body, r#"{"error":"unauthorized"}"#);
    for private in [
        data_dir.to_string_lossy().as_ref(),
        VALID_TOKEN,
        INVALID_TOKEN,
        PRINCIPAL_SENTINEL,
        CLIENT_SENTINEL,
    ] {
        assert!(!body.contains(private), "401 leaked private material");
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
            .expect("structured result"),
    )
    .expect("success envelope")
}

fn snapshot_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .expect("read store directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("directory entries");
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("relative store path")
                .to_string_lossy()
                .replace('\\', "/");
            let file_type = entry.file_type().expect("entry type");
            if file_type.is_dir() {
                snapshot.insert(format!("{relative}/"), Vec::new());
                visit(root, &path, snapshot);
            } else if file_type.is_file() {
                snapshot.insert(relative, fs::read(path).expect("read store file"));
            } else {
                panic!("unexpected store entry type");
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("write digest");
    }
    encoded
}
