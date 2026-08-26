#[path = "two_client_conformance/raw_http.rs"]
mod raw_http;
#[path = "two_client_conformance/resilience.rs"]
mod resilience;
#[path = "two_client_conformance/suite.rs"]
mod suite;

use axum::http::{HeaderName, HeaderValue};
use jiandu_core::{
    ClientId, CreationActor, Grant, IdempotencyKey, MemoryScope, MemoryType, PrincipalId,
    ProjectId, ProvenanceInput, RememberMemoryCommand, ScopeSelector, SessionId, Tag, Timestamp,
    TrustedRequestContext,
};
use jiandu_index::LexicalIndex;
use jiandu_service::{MCP_ROUTE, RunningDaemon, ServeConfig};
use jiandu_store::{AuthorizedScopes, CanonicalStore, LockOwner, MutationOperation};
use raw_http::RawHttpDriver;
use rmcp::model::{CallToolRequestParams, ClientInfo, ProtocolVersion, ReadResourceRequestParams};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const OFFICIAL_TOKEN: &str = "official-conformance-token-0123456789abcdef";
const RAW_TOKEN: &str = "raw-conformance-token-0123456789abcdef";
const NO_FORGET_TOKEN: &str = "no-forget-conformance-token-0123456789abcdef";
const INVALID_TOKEN: &str = "invalid-conformance-token-0123456789abcdef";
const SHARED_PROJECT: &str = "prj_conformance_shared";
const PRIVATE_A: PrivateScopeFixture = PrivateScopeFixture {
    principal_id: "prn_conformance_a",
    project_id: "prj_conformance_private_a",
    session_id: "ses_conformance_private_a",
    principal_memory: "mem_private_principal_a",
    project_memory: "mem_private_project_a",
    session_memory: "mem_private_session_a",
    principal_query: "alphaprincipaluniquesentinel",
    project_query: "alphaprojectuniquesentinel",
    session_query: "alphasessionuniquesentinel",
};
const PRIVATE_B: PrivateScopeFixture = PrivateScopeFixture {
    principal_id: "prn_conformance_b",
    project_id: "prj_conformance_private_b",
    session_id: "ses_conformance_private_b",
    principal_memory: "mem_private_principal_b",
    project_memory: "mem_private_project_b",
    session_memory: "mem_private_session_b",
    principal_query: "betaprincipaluniquesentinel",
    project_query: "betaprojectuniquesentinel",
    session_query: "betasessionuniquesentinel",
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConformanceManifest {
    fixture_version: String,
    protocol_version: String,
    api_version: String,
    harness_version: String,
    drivers: DriverVersions,
    tools: Vec<ToolFixture>,
    resources: Vec<String>,
    resource_templates: Vec<String>,
    domain_errors: Vec<String>,
    unauthorized: UnauthorizedFixture,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DriverVersions {
    official: PackageVersion,
    raw_http: PackageVersion,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackageVersion {
    package: String,
    version: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolFixture {
    name: String,
    input_schema: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnauthorizedFixture {
    status: u16,
    body: String,
    cache_control: String,
    challenge: String,
}

struct SuiteContext {
    seed_memory: &'static str,
    shared_project: &'static str,
    key_prefix: &'static str,
    own: PrivateScopeFixture,
    foreign: PrivateScopeFixture,
}

#[derive(Clone, Copy)]
struct PrivateScopeFixture {
    principal_id: &'static str,
    project_id: &'static str,
    session_id: &'static str,
    principal_memory: &'static str,
    project_memory: &'static str,
    session_memory: &'static str,
    principal_query: &'static str,
    project_query: &'static str,
    session_query: &'static str,
}

trait PublicMcpDriver: Sized {
    const NAME: &'static str;
    async fn connect(endpoint: &str, token: &str) -> Result<Self, String>;
    async fn rejects_invalid_credential(endpoint: &str) -> bool;
    fn initialize_result(&self) -> &Value;
    async fn list_tools(&mut self) -> Result<Value, String>;
    async fn list_resources(&mut self) -> Result<Value, String>;
    async fn list_resource_templates(&mut self) -> Result<Value, String>;
    async fn read_resource(&mut self, uri: &str) -> Result<Value, String>;
    async fn read_resource_error(&mut self, uri: &str) -> Result<Value, String>;
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, String>;
    async fn close(self) -> Result<(), String>;
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

struct OfficialDriver {
    client: RunningService<RoleClient, V2025Client>,
    initialize: Value,
}

impl PublicMcpDriver for OfficialDriver {
    const NAME: &'static str = "official-rmcp";

    async fn connect(endpoint: &str, token: &str) -> Result<Self, String> {
        let client = V2025Client
            .serve(official_transport(endpoint, token))
            .await
            .map_err(|_| "official initialize failed".to_owned())?;
        let initialize = serde_json::to_value(client.peer_info().ok_or("missing peer info")?)
            .map_err(|_| "official initialize normalization failed".to_owned())?;
        Ok(Self { client, initialize })
    }

    async fn rejects_invalid_credential(endpoint: &str) -> bool {
        V2025Client
            .serve(official_transport(endpoint, INVALID_TOKEN))
            .await
            .is_err()
    }

    fn initialize_result(&self) -> &Value {
        &self.initialize
    }

    async fn list_tools(&mut self) -> Result<Value, String> {
        self.client
            .list_all_tools()
            .await
            .map(|tools| json!({ "tools": tools }))
            .map_err(|_| "official tools/list failed".to_owned())
    }

    async fn list_resources(&mut self) -> Result<Value, String> {
        self.client
            .list_all_resources()
            .await
            .map(|resources| json!({ "resources": resources }))
            .map_err(|_| "official resources/list failed".to_owned())
    }

    async fn list_resource_templates(&mut self) -> Result<Value, String> {
        self.client
            .list_all_resource_templates()
            .await
            .map(|templates| json!({ "resourceTemplates": templates }))
            .map_err(|_| "official resource templates failed".to_owned())
    }

    async fn read_resource(&mut self, uri: &str) -> Result<Value, String> {
        let value = self
            .client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(|_| "official resource read failed".to_owned())?;
        serde_json::to_value(value).map_err(|_| "official resource normalization failed".to_owned())
    }

    async fn read_resource_error(&mut self, uri: &str) -> Result<Value, String> {
        match self
            .client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
        {
            Err(ServiceError::McpError(error)) => serde_json::to_value(error)
                .map_err(|_| "official resource error normalization failed".to_owned()),
            Err(_) => Err("official resource read failed outside JSON-RPC".to_owned()),
            Ok(_) => Err("official resource read unexpectedly succeeded".to_owned()),
        }
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| "tool arguments must be an object".to_owned())?;
        let value = self
            .client
            .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
            .await
            .map_err(|_| "official tool call failed".to_owned())?;
        serde_json::to_value(value).map_err(|_| "official tool normalization failed".to_owned())
    }

    async fn close(self) -> Result<(), String> {
        self.client
            .cancel()
            .await
            .map(|_| ())
            .map_err(|_| "official close failed".to_owned())
    }
}

impl PublicMcpDriver for RawHttpDriver {
    const NAME: &'static str = "raw-http";

    async fn connect(endpoint: &str, token: &str) -> Result<Self, String> {
        RawHttpDriver::connect(endpoint, token).await
    }

    async fn rejects_invalid_credential(endpoint: &str) -> bool {
        RawHttpDriver::connect(endpoint, INVALID_TOKEN)
            .await
            .is_err()
    }

    fn initialize_result(&self) -> &Value {
        RawHttpDriver::initialize_result(self)
    }

    async fn list_tools(&mut self) -> Result<Value, String> {
        RawHttpDriver::list_tools(self).await
    }

    async fn list_resources(&mut self) -> Result<Value, String> {
        RawHttpDriver::list_resources(self).await
    }

    async fn list_resource_templates(&mut self) -> Result<Value, String> {
        RawHttpDriver::list_resource_templates(self).await
    }

    async fn read_resource(&mut self, uri: &str) -> Result<Value, String> {
        RawHttpDriver::read_resource(self, uri).await
    }

    async fn read_resource_error(&mut self, uri: &str) -> Result<Value, String> {
        RawHttpDriver::read_resource_error(self, uri).await
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        RawHttpDriver::call_tool(self, name, arguments).await
    }

    async fn close(self) -> Result<(), String> {
        RawHttpDriver::close(self).await
    }
}

#[tokio::test]
async fn official_rmcp_client_passes_the_shared_public_contract() {
    run_shared::<OfficialDriver>(&SuiteContext {
        seed_memory: "mem_conformance_shared_a",
        shared_project: SHARED_PROJECT,
        key_prefix: "official",
        own: PRIVATE_A,
        foreign: PRIVATE_B,
    })
    .await;
}

#[tokio::test]
async fn raw_http_client_passes_the_shared_public_contract_with_one_response_event() {
    run_shared::<RawHttpDriver>(&SuiteContext {
        seed_memory: "mem_conformance_shared_b",
        shared_project: SHARED_PROJECT,
        key_prefix: "raw",
        own: PRIVATE_B,
        foreign: PRIVATE_A,
    })
    .await;
}

async fn run_shared<D: PublicMcpDriver>(context: &SuiteContext) {
    let fixture = Harness::new();
    let daemon = fixture.start().await;
    let endpoint = format!("http://{}{}", daemon.local_addr(), MCP_ROUTE);
    let token = if D::NAME == OfficialDriver::NAME {
        OFFICIAL_TOKEN
    } else {
        RAW_TOKEN
    };
    suite::run::<D>(&endpoint, token, context, &fixture.store_path, &manifest()).await;
    daemon.shutdown().await.expect("shutdown daemon");
    fixture.assert_lock_released();
}

#[tokio::test]
async fn independent_drivers_share_one_store_without_crossing_scope_or_forget_authority() {
    let fixture = Harness::new();
    let daemon = fixture.start().await;
    let endpoint = format!("http://{}{}", daemon.local_addr(), MCP_ROUTE);
    let manifest = manifest();
    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("official client");
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("raw client");

    let created_a = public_success(
        official
            .call_tool(
                "memory_remember",
                remember_args("interop created by A", "interop-a-create"),
            )
            .await
            .expect("A creates"),
    );
    let id_a = created_a["result"]["record"]["id"]
        .as_str()
        .expect("A record ID")
        .to_owned();
    let read_by_b = public_success(
        raw.call_tool("memory_get", json!({ "memoryId": id_a }))
            .await
            .expect("B reads A"),
    );
    assert_eq!(read_by_b["result"], created_a["result"]["record"]);
    assert_eq!(read_by_b["storeRevision"], created_a["storeRevision"]);

    let updated_by_b = public_success(
        raw.call_tool(
            "memory_update",
            json!({ "memoryId": id_a, "expectedRevision": 1,
                "patch": { "title": "interop updated by B" }, "reason": "cross-driver update",
                "idempotencyKey": "interop-b-update-a" }),
        )
        .await
        .expect("B updates A"),
    );
    let reread_by_a = public_success(
        official
            .call_tool("memory_get", json!({ "memoryId": id_a }))
            .await
            .expect("A rereads"),
    );
    assert_eq!(reread_by_a["result"], updated_by_b["result"]["record"]);
    assert_eq!(reread_by_a["storeRevision"], updated_by_b["storeRevision"]);

    let created_b = public_success(
        raw.call_tool(
            "memory_remember",
            remember_args("interop created by B", "interop-b-create"),
        )
        .await
        .expect("B creates"),
    );
    let id_b = created_b["result"]["record"]["id"]
        .as_str()
        .expect("B record ID")
        .to_owned();
    let (a_reads_b, b_reads_a) = tokio::join!(
        official.call_tool("memory_get", json!({ "memoryId": id_b })),
        raw.call_tool("memory_get", json!({ "memoryId": id_a }))
    );
    assert_eq!(
        public_success(a_reads_b.expect("A concurrent read"))["result"],
        created_b["result"]["record"]
    );
    assert_eq!(
        public_success(b_reads_a.expect("B concurrent read"))["result"]["revision"],
        2
    );

    let (write_a, write_b) = tokio::join!(
        official.call_tool(
            "memory_remember",
            remember_args("parallel A", "interop-parallel-a")
        ),
        raw.call_tool(
            "memory_remember",
            remember_args("parallel B", "interop-parallel-b")
        )
    );
    let write_a = public_success(write_a.expect("parallel A write"));
    let write_b = public_success(write_b.expect("parallel B write"));
    assert_ne!(
        write_a["result"]["record"]["id"],
        write_b["result"]["record"]["id"]
    );
    assert_ne!(write_a["storeRevision"], write_b["storeRevision"]);
    assert_eq!(write_a["result"]["record"]["revision"], 1);
    assert_eq!(write_b["result"]["record"]["revision"], 1);

    let stale = official
        .call_tool(
            "memory_update",
            json!({ "memoryId": id_a, "expectedRevision": 1,
                "patch": { "title": "stale overwrite" }, "reason": "must fail",
                "idempotencyKey": "interop-stale" }),
        )
        .await
        .expect("stale envelope");
    suite::assert_error(&stale, "REVISION_CONFLICT", &manifest);
    assert_eq!(
        public_success(
            raw.call_tool("memory_get", json!({ "memoryId": id_a }))
                .await
                .expect("unchanged")
        )["result"]["title"],
        "interop updated by B"
    );

    let mut no_forget = RawHttpDriver::connect(&endpoint, NO_FORGET_TOKEN)
        .await
        .expect("no-forget client");
    let denied = no_forget.call_tool("memory_forget", json!({ "memoryId": id_b,
        "expectedRevision": 1, "reason": "must be denied", "idempotencyKey": "interop-denied-forget" }))
        .await.expect("forbidden forget");
    suite::assert_error(&denied, "FORBIDDEN", &manifest);
    no_forget.close().await.expect("close no-forget client");

    public_success(
        official
            .call_tool(
                "memory_forget",
                json!({ "memoryId": id_b,
        "expectedRevision": 1, "reason": "authorized forget", "idempotencyKey": "interop-forget" }),
            )
            .await
            .expect("authorized forget"),
    );
    for result in [
        official
            .call_tool("memory_get", json!({ "memoryId": id_b }))
            .await
            .expect("A not found"),
        raw.call_tool("memory_get", json!({ "memoryId": id_b }))
            .await
            .expect("B not found"),
    ] {
        suite::assert_error(&result, "NOT_FOUND", &manifest);
    }

    official.close().await.expect("close official");
    raw.close().await.expect("close raw");
    daemon.shutdown().await.expect("shutdown daemon");
    fixture.assert_lock_released();
}

fn remember_args(title: &str, key: &str) -> Value {
    json!({ "scope": { "kind": "project", "projectId": SHARED_PROJECT }, "type": "decision",
        "title": title, "body": format!("{title} body"), "provenance": {}, "idempotencyKey": key })
}

fn public_success(result: Value) -> Value {
    assert_eq!(result["isError"], false);
    result["structuredContent"].clone()
}

#[test]
fn versioned_manifest_matches_locked_clients_schemas_and_raw_driver_boundary() {
    let manifest = manifest();
    assert_eq!(
        manifest.fixture_version,
        "jiandu.service/conformance/v1alpha1"
    );
    assert_eq!(manifest.harness_version, env!("CARGO_PKG_VERSION"));
    let lock = include_str!("../../../Cargo.lock");
    for driver in [&manifest.drivers.official, &manifest.drivers.raw_http] {
        assert_eq!(locked_version(lock, &driver.package), driver.version);
    }
    assert_eq!(
        suite::expected_error_set(&manifest),
        BTreeSet::from([
            "FORBIDDEN",
            "IDEMPOTENCY_CONFLICT",
            "INVALID_ARGUMENT",
            "NOT_FOUND",
            "REVISION_CONFLICT",
        ])
    );
    assert!(!include_str!("two_client_conformance/raw_http.rs").contains("rmcp"));
}

async fn assert_fixed_unauthorized(endpoint: &str, manifest: &ConformanceManifest) {
    let response = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(INVALID_TOKEN)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": manifest.protocol_version,
                "capabilities": {}, "clientInfo": { "name": "invalid", "version": "0.1.0" }
            }}),
        )
        .send()
        .await
        .expect("401 response");
    assert_eq!(response.status().as_u16(), manifest.unauthorized.status);
    assert_eq!(
        response.headers()["cache-control"],
        manifest.unauthorized.cache_control
    );
    assert_eq!(
        response.headers()["www-authenticate"],
        manifest.unauthorized.challenge
    );
    assert_eq!(
        response.text().await.expect("401 body"),
        manifest.unauthorized.body
    );
}

fn official_transport(
    endpoint: &str,
    token: &str,
) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut headers = HashMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization header"),
    );
    StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(endpoint).custom_headers(headers),
    )
}

fn manifest() -> ConformanceManifest {
    serde_json::from_str(include_str!(
        "../fixtures/conformance/v1alpha1/manifest.json"
    ))
    .expect("strict conformance manifest")
}

fn locked_version(lock: &str, package: &str) -> String {
    lock.split("[[package]]")
        .find_map(|block| {
            let mut name = None;
            let mut version = None;
            for line in block.lines() {
                name = name.or_else(|| {
                    line.strip_prefix("name = \"")
                        .and_then(|value| value.strip_suffix('"'))
                });
                version = version.or_else(|| {
                    line.strip_prefix("version = \"")
                        .and_then(|value| value.strip_suffix('"'))
                });
            }
            (name == Some(package))
                .then(|| version.map(str::to_owned))
                .flatten()
        })
        .unwrap_or_else(|| panic!("missing locked package {package}"))
}

struct Harness {
    _sandbox: TempDir,
    store_path: PathBuf,
    config_path: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let sandbox = TempDir::new().expect("temporary conformance sandbox");
        let store_path = sandbox.path().join("store");
        assert!(store_path.starts_with(sandbox.path()) && store_path != sandbox.path());
        fs::create_dir(&store_path).expect("create isolated store directory");
        seed_store(&store_path);
        let config_path = sandbox.path().join("daemon.json");
        assert!(config_path.starts_with(sandbox.path()));
        fs::write(&config_path, config_bytes(&store_path)).expect("write local config");
        Self {
            _sandbox: sandbox,
            store_path,
            config_path,
        }
    }

    async fn start(&self) -> RunningDaemon {
        let daemon =
            RunningDaemon::start(ServeConfig::load(&self.config_path).expect("load config"))
                .await
                .expect("start daemon");
        assert!(daemon.local_addr().ip().is_loopback());
        assert_ne!(
            daemon.local_addr().port(),
            0,
            "fixture must use an ephemeral port"
        );
        daemon
    }

    fn assert_lock_released(&self) {
        drop(
            CanonicalStore::open(
                &self.store_path,
                LockOwner::for_current_process().expect("owner"),
            )
            .expect("daemon released temporary store lock"),
        );
    }
}

fn seed_store(path: &Path) {
    let principal_a = PrincipalId::new(PRIVATE_A.principal_id).expect("principal A");
    let principal_b = PrincipalId::new(PRIVATE_B.principal_id).expect("principal B");
    let shared = ProjectId::new(SHARED_PROJECT).expect("shared project");
    let project_a = ProjectId::new(PRIVATE_A.project_id).expect("project A");
    let project_b = ProjectId::new(PRIVATE_B.project_id).expect("project B");
    let session_a = SessionId::new(PRIVATE_A.session_id).expect("session A");
    let session_b = SessionId::new(PRIVATE_B.session_id).expect("session B");
    let scopes_a = AuthorizedScopes::new(principal_a.clone())
        .with_project(shared.clone())
        .with_project(project_a.clone())
        .with_session(session_a.clone());
    let scopes_b = AuthorizedScopes::new(principal_b.clone())
        .with_project(shared.clone())
        .with_project(project_b.clone())
        .with_session(session_b.clone());
    let mut store =
        CanonicalStore::initialize(path, LockOwner::for_current_process().expect("owner"))
            .expect("initialize store");
    for suffix in ["a", "b", "c"] {
        seed_record(
            &mut store,
            &scopes_a,
            &principal_a,
            MemoryScope::Project {
                project_id: shared.clone(),
            },
            &format!("mem_conformance_shared_{suffix}"),
            "ordinary conformance shared body",
        );
    }
    for (scopes, principal, scope, id, body) in [
        (
            &scopes_a,
            &principal_a,
            MemoryScope::Principal {
                principal_id: principal_a.clone(),
            },
            PRIVATE_A.principal_memory,
            PRIVATE_A.principal_query,
        ),
        (
            &scopes_a,
            &principal_a,
            MemoryScope::Project {
                project_id: project_a,
            },
            PRIVATE_A.project_memory,
            PRIVATE_A.project_query,
        ),
        (
            &scopes_a,
            &principal_a,
            MemoryScope::Session {
                session_id: session_a,
            },
            PRIVATE_A.session_memory,
            PRIVATE_A.session_query,
        ),
        (
            &scopes_b,
            &principal_b,
            MemoryScope::Principal {
                principal_id: principal_b.clone(),
            },
            PRIVATE_B.principal_memory,
            PRIVATE_B.principal_query,
        ),
        (
            &scopes_b,
            &principal_b,
            MemoryScope::Project {
                project_id: project_b,
            },
            PRIVATE_B.project_memory,
            PRIVATE_B.project_query,
        ),
        (
            &scopes_b,
            &principal_b,
            MemoryScope::Session {
                session_id: session_b,
            },
            PRIVATE_B.session_memory,
            PRIVATE_B.session_query,
        ),
    ] {
        seed_record(&mut store, scopes, principal, scope, id, body);
    }
    let admin = scopes_a
        .authorize_index_rebuild(&TrustedRequestContext {
            principal_id: principal_a,
            client_id: ClientId::new("cli_conformance_index_admin").expect("client"),
            grants: BTreeSet::from([Grant::new("memory:admin:rebuild_index").expect("grant")]),
        })
        .expect("index authority");
    LexicalIndex::new(path.join("index"))
        .rebuild(&store, &admin)
        .expect("rebuild index");
}

fn seed_record(
    store: &mut CanonicalStore,
    scopes: &AuthorizedScopes,
    principal: &PrincipalId,
    scope: MemoryScope,
    id: &str,
    body: &str,
) {
    let grant = match scope {
        MemoryScope::Principal { .. } => "memory:write:principal",
        MemoryScope::Project { .. } => "memory:write:project",
        MemoryScope::Session { .. } => "memory:write:session",
        MemoryScope::InstanceGlobal {} => "memory:write:instance_global",
    };
    let authority = scopes
        .authorize_mutation(
            &TrustedRequestContext {
                principal_id: principal.clone(),
                client_id: ClientId::new("cli_conformance_seed").expect("client"),
                grants: BTreeSet::from([Grant::new(grant).expect("grant")]),
            },
            &scope,
            MutationOperation::Create,
        )
        .expect("seed authority");
    let selector = match scope {
        MemoryScope::Principal { .. } => ScopeSelector::Principal {},
        MemoryScope::Project { project_id } => ScopeSelector::Project { project_id },
        MemoryScope::Session { session_id } => ScopeSelector::Session { session_id },
        MemoryScope::InstanceGlobal {} => ScopeSelector::InstanceGlobal {},
    };
    store
        .create(
            &authority,
            &RememberMemoryCommand {
                scope: selector,
                memory_type: MemoryType::Decision,
                title: format!("ordinary conformance {id}"),
                summary: None,
                body: body.to_owned(),
                tags: vec![Tag::new("conformance").expect("tag")],
                provenance: ProvenanceInput::default(),
                relations: Vec::new(),
                idempotency_key: IdempotencyKey::new(format!("seed-{id}")).expect("key"),
            },
            jiandu_core::MemoryId::new(id).expect("ID"),
            CreationActor::Host,
            Timestamp::new("2026-08-26T00:00:00Z").expect("timestamp"),
        )
        .expect("seed record");
}

fn config_bytes(store_path: &Path) -> Vec<u8> {
    let client = |token: &str,
                  principal: &str,
                  client: &str,
                  project: &str,
                  session: Option<&str>,
                  forget: bool| {
        let project_ids = if project == SHARED_PROJECT {
            vec![SHARED_PROJECT]
        } else {
            vec![SHARED_PROJECT, project]
        };
        json!({
            "bearerTokenDigest": format!("sha256:{}", lower_hex(&Sha256::digest(token))),
            "principalId": principal,
            "clientId": client,
            "scopes": {
                "projectIds": project_ids,
                "sessionIds": session.into_iter().collect::<Vec<_>>(),
                "instanceGlobal": false
            },
            "permissions": {
                "read": true,
                "write": ["project"],
                "forget": if forget { json!(["project"]) } else { json!([]) }
            },
            "creationActor": "host"
        })
    };
    serde_json::to_vec(&json!({
        "configVersion": "jiandu.service.config/v0.1",
        "bind": "127.0.0.1:0",
        "dataDir": store_path,
        "cursorMacKey": format!("hmac-sha256:{}", "11".repeat(32)),
        "clients": [
            client(OFFICIAL_TOKEN, PRIVATE_A.principal_id, "cli_conformance_official", PRIVATE_A.project_id, Some(PRIVATE_A.session_id), true),
            client(RAW_TOKEN, PRIVATE_B.principal_id, "cli_conformance_raw", PRIVATE_B.project_id, Some(PRIVATE_B.session_id), true),
            client(NO_FORGET_TOKEN, PRIVATE_A.principal_id, "cli_conformance_no_forget", SHARED_PROJECT, None, false)
        ]
    }))
    .expect("config JSON")
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("write digest");
    }
    encoded
}
