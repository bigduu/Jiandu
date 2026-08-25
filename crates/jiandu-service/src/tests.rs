//! Daemon boundary and Streamable HTTP integration tests.

use crate::auth::bearer_digest;
use crate::{DaemonError, LIVENESS_ROUTE, MCP_ROUTE, READINESS_ROUTE, RunningDaemon, ServeConfig};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use jiandu_core::{
    IdempotencyKey, MemoryGetRequest, MemoryRecord, MemoryType, ProvenanceInput,
    RememberMemoryCommand, RememberMemoryResult, ResultEnvelope, ScopeSelector, Tag,
};
use jiandu_store::{CanonicalStore, LockOwner};
use rmcp::model::{CallToolRequestParams, ClientInfo, JsonObject, ProtocolVersion};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, ServiceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const VALID_TOKEN: &str = "daemon-e2e-token-0123456789abcdef";
const INVALID_TOKEN: &str = "daemon-wrong-token-0123456789abcdef";
const PRINCIPAL_SENTINEL: &str = "prn_daemon_private_identity";
const CLIENT_SENTINEL: &str = "cli_daemon_private_identity";

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
fn config_rejects_malformed_or_duplicate_credentials_and_unknown_authority() {
    let root = TempDir::new().expect("temporary data parent");
    let document = config_value("127.0.0.1:0", root.path(), VALID_TOKEN);

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

    let mut unknown_grant = document;
    unknown_grant["clients"][0]["grants"] = json!(["memory:read", "daemon:admin"]);
    assert!(ServeConfig::from_slice(&json_bytes(unknown_grant)).is_err());
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

    let remember = RememberMemoryCommand {
        scope: ScopeSelector::Principal {},
        memory_type: MemoryType::Decision,
        title: "Daemon E2E decision".to_owned(),
        summary: Some("Authenticated singleton transport".to_owned()),
        body: "DAEMON_PRIVATE_BODY_SENTINEL".to_owned(),
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
    assert_eq!(fetched.result.body, "DAEMON_PRIVATE_BODY_SENTINEL");
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

fn config_value(bind: &str, data_dir: &Path, token: &str) -> Value {
    json!({
        "bind": bind,
        "dataDir": data_dir,
        "cursorMacKey": format!("hmac-sha256:{}", "11".repeat(32)),
        "clients": [{
            "bearerTokenDigest": format!("sha256:{}", lower_hex(&bearer_digest(token.as_bytes()))),
            "principalId": PRINCIPAL_SENTINEL,
            "clientId": CLIENT_SENTINEL,
            "grants": ["memory:read", "memory:write:principal", "memory:forget:principal"],
            "scopes": {
                "projectIds": [],
                "sessionIds": [],
                "instanceGlobal": false
            },
            "creationActor": "host",
            "mutationPolicy": {
                "maxBodyBytes": 4096,
                "allowedTypes": ["decision"],
                "allowedScopes": ["principal"],
                "secretContentPolicy": "allow_all"
            }
        }]
    })
}

fn json_bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("config JSON")
}

fn client_transport(base: &str, token: &str) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut headers = HashMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization header"),
    );
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{base}{MCP_ROUTE}"))
        .custom_headers(headers);
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
