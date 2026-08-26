use super::{
    Harness, NO_FORGET_TOKEN, OFFICIAL_TOKEN, OfficialDriver, PRIVATE_A, PRIVATE_B,
    PublicMcpDriver, RAW_TOKEN, RawHttpDriver, SHARED_PROJECT, manifest, public_success,
    remember_args, suite,
};
use jiandu_core::{ClientId, Grant, PrincipalId, ProjectId, SessionId, TrustedRequestContext};
use jiandu_index::{IndexRebuildReport, LexicalIndex};
use jiandu_service::{DaemonError, MCP_ROUTE, READINESS_ROUTE, RunningDaemon, ServeConfig};
use jiandu_store::{AuthorizedScopes, CanonicalStore, LockOwner};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ABSENT_MEMORY: &str = "mem_resilience_absent";
const LOST_ACK_TITLE: &str = "resilience lost acknowledgement";
const INDEX_QUERY: &str = "ordinary conformance";
const INDEX_QUERY_SENTINEL: &str = "resiliencequerymustnotleak";
const UNAVAILABLE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const UNAVAILABLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn concurrent_clients_preserve_cas_idempotency_and_exact_scope() {
    let fixture = Harness::new();
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let manifest = manifest();
    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("official client");
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("raw client");
    let mut same_principal = RawHttpDriver::connect(&endpoint, NO_FORGET_TOKEN)
        .await
        .expect("same-principal raw client");

    let (official_update, raw_update) = tokio::join!(
        official.call_tool(
            "memory_update",
            update_args(
                "mem_conformance_shared_a",
                1,
                "CAS winner from official",
                "resilience-cas-official",
            ),
        ),
        raw.call_tool(
            "memory_update",
            update_args(
                "mem_conformance_shared_a",
                1,
                "CAS winner from raw",
                "resilience-cas-raw",
            ),
        )
    );
    let mut cas_success = None;
    let mut cas_conflicts = 0;
    for result in [
        official_update.expect("official CAS envelope"),
        raw_update.expect("raw CAS envelope"),
    ] {
        if result["isError"] == false {
            assert!(cas_success.replace(public_success(result)).is_none());
        } else {
            suite::assert_error(&result, "REVISION_CONFLICT", &manifest);
            assert_eq!(
                result["structuredContent"]["error"]["details"]["currentRevision"],
                2
            );
            cas_conflicts += 1;
        }
    }
    assert_eq!(cas_conflicts, 1);
    let cas_success = cas_success.expect("one CAS winner");
    assert_eq!(cas_success["result"]["record"]["revision"], 2);
    let after_cas = public_success(
        official
            .call_tool(
                "memory_get",
                json!({ "memoryId": "mem_conformance_shared_a" }),
            )
            .await
            .expect("read CAS winner"),
    );
    assert_eq!(after_cas["result"], cas_success["result"]["record"]);

    let identical = remember_args("same-principal concurrent retry", "resilience-same-key");
    let (first_attempt, second_attempt) = tokio::join!(
        official.call_tool("memory_remember", identical.clone()),
        same_principal.call_tool("memory_remember", identical.clone())
    );
    let first_attempt = public_success(first_attempt.expect("official idempotent envelope"));
    let second_attempt = public_success(second_attempt.expect("raw idempotent envelope"));
    assert_eq!(
        first_attempt["result"]["record"],
        second_attempt["result"]["record"]
    );
    assert_eq!(
        first_attempt["correlationId"],
        second_attempt["correlationId"]
    );
    assert_eq!(
        first_attempt["storeRevision"],
        second_attempt["storeRevision"]
    );
    let replay_flags = [
        first_attempt["result"]["idempotentReplay"]
            .as_bool()
            .expect("first replay flag"),
        second_attempt["result"]["idempotentReplay"]
            .as_bool()
            .expect("second replay flag"),
    ];
    assert!(replay_flags.contains(&false));
    assert!(replay_flags.contains(&true));

    let conflict_key = "resilience-concurrent-conflicting-key";
    let (left, right) = tokio::join!(
        official.call_tool(
            "memory_remember",
            remember_args("concurrent fingerprint left", conflict_key),
        ),
        same_principal.call_tool(
            "memory_remember",
            remember_args("concurrent fingerprint right", conflict_key),
        )
    );
    let mut fingerprint_successes = 0;
    let mut fingerprint_conflicts = 0;
    for result in [left.expect("left envelope"), right.expect("right envelope")] {
        if result["isError"] == false {
            public_success(result);
            fingerprint_successes += 1;
        } else {
            suite::assert_error(&result, "IDEMPOTENCY_CONFLICT", &manifest);
            fingerprint_conflicts += 1;
        }
    }
    assert_eq!((fingerprint_successes, fingerprint_conflicts), (1, 1));

    let (authorized, hidden) = tokio::join!(
        official.call_tool(
            "memory_update",
            update_args(
                PRIVATE_A.project_memory,
                1,
                "authorized private update",
                "resilience-private-authorized",
            ),
        ),
        raw.call_tool(
            "memory_update",
            update_args(
                PRIVATE_A.project_memory,
                1,
                "unauthorized private overwrite",
                "resilience-private-hidden",
            ),
        )
    );
    let authorized = public_success(authorized.expect("authorized private envelope"));
    assert_eq!(authorized["result"]["record"]["revision"], 2);
    let hidden = hidden.expect("hidden private envelope");
    suite::assert_error(&hidden, "NOT_FOUND", &manifest);
    assert_public_safe(&hidden, &fixture, &["unauthorized private overwrite"]);
    let absent = raw
        .call_tool("memory_get", json!({ "memoryId": ABSENT_MEMORY }))
        .await
        .expect("absent envelope");
    let hidden_get = raw
        .call_tool(
            "memory_get",
            json!({ "memoryId": PRIVATE_A.project_memory }),
        )
        .await
        .expect("hidden get envelope");
    assert_eq!(error_signature(&hidden_get), error_signature(&absent));
    assert_public_safe(&hidden_get, &fixture, &[PRIVATE_A.project_memory]);

    official.close().await.expect("close official");
    raw.close().await.expect("close raw");
    same_principal
        .close()
        .await
        .expect("close same-principal raw");
    daemon.shutdown().await.expect("shutdown daemon");
    fixture.assert_lock_released();
}

#[tokio::test]
async fn lost_acknowledgement_restarts_to_exact_replay_and_rejects_conflicting_reuse() {
    let fixture = Harness::new();
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let manifest = manifest();
    let mut requester = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("raw requester");
    let mut observer = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("official observer");
    let before = list_shared(&mut observer).await;
    let before_revision = before["storeRevision"]
        .as_u64()
        .expect("baseline store revision");
    let command = remember_args(LOST_ACK_TITLE, "resilience-lost-ack-key");

    requester
        .call_tool_and_drop_response("memory_remember", command.clone())
        .await
        .expect("drop unread successful response");
    let observed_id = wait_for_shared_title(&mut observer, LOST_ACK_TITLE).await;
    let observed = public_success(
        observer
            .call_tool("memory_get", json!({ "memoryId": observed_id }))
            .await
            .expect("observe durable record"),
    );
    assert_eq!(observed["storeRevision"], before_revision + 1);
    requester
        .close()
        .await
        .expect("disconnect requester without reading acknowledgement");
    observer.close().await.expect("close observer");
    daemon.shutdown().await.expect("first shutdown");
    fixture.assert_lock_released();

    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let mut retry = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("first restart client");
    let replay = public_success(
        retry
            .call_tool("memory_remember", command.clone())
            .await
            .expect("restart replay envelope"),
    );
    assert_eq!(replay["result"]["idempotentReplay"], true);
    assert_eq!(replay["result"]["record"], observed["result"]);
    assert_eq!(replay["storeRevision"], observed["storeRevision"]);

    let conflicting = retry
        .call_tool(
            "memory_remember",
            remember_args("conflicting lost-ack reuse", "resilience-lost-ack-key"),
        )
        .await
        .expect("conflicting reuse envelope");
    suite::assert_error(&conflicting, "IDEMPOTENCY_CONFLICT", &manifest);
    assert_eq!(
        conflicting["structuredContent"]["storeRevision"],
        replay["storeRevision"]
    );
    assert_public_safe(&conflicting, &fixture, &[LOST_ACK_TITLE]);
    retry.close().await.expect("close first restart client");
    daemon.shutdown().await.expect("second shutdown");
    fixture.assert_lock_released();

    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let mut retry = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("second restart client");
    let second_replay = public_success(
        retry
            .call_tool("memory_remember", command)
            .await
            .expect("second restart replay envelope"),
    );
    assert_eq!(second_replay, replay);
    retry.close().await.expect("close second restart client");
    daemon.shutdown().await.expect("final shutdown");
    fixture.assert_lock_released();
}

#[tokio::test]
async fn missing_and_corrupt_index_preserve_exact_reads_and_rebuild_deterministically() {
    let fixture = Harness::new();
    let index_file = disposable_index_file(&fixture);
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("official baseline client");
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("raw baseline client");
    let baseline_search = search_shared(&mut official, INDEX_QUERY).await;
    let raw_search = search_shared(&mut raw, INDEX_QUERY).await;
    assert_same_public_snapshot(&baseline_search, &raw_search);
    let baseline_get = get_shared(&mut official).await;
    let baseline_list = list_shared(&mut official).await;
    official.close().await.expect("close official baseline");
    raw.close().await.expect("close raw baseline");
    daemon.shutdown().await.expect("baseline shutdown");
    fixture.assert_lock_released();
    let baseline_index_bytes = fs::read(&index_file).expect("baseline derived index bytes");

    assert_disposable_target(&fixture, &index_file);
    fs::remove_file(&index_file).expect("remove only disposable fixture index");
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    assert_readiness(&endpoint, "missing", &fixture).await;
    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("missing-index official client");
    assert_initialize_index(&official, "missing");
    assert_exact_snapshot(&mut official, &baseline_get, &baseline_list).await;
    let missing_search = official
        .call_tool("memory_search", search_args(INDEX_QUERY_SENTINEL))
        .await
        .expect("missing-index search envelope");
    assert_index_degraded(&missing_search, &fixture);
    official.close().await.expect("close missing-index client");
    daemon.shutdown().await.expect("missing-index shutdown");
    fixture.assert_lock_released();

    let report = rebuild_index(&fixture);
    assert!(!report.replaced_existing);
    assert_eq!(
        fs::read(&index_file).expect("rebuilt missing index bytes"),
        baseline_index_bytes
    );
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    assert_readiness(&endpoint, "ready", &fixture).await;
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("rebuilt raw client");
    assert_initialize_index(&raw, "ready");
    let rebuilt_search = search_shared(&mut raw, INDEX_QUERY).await;
    assert_same_public_snapshot(&baseline_search, &rebuilt_search);
    assert_exact_snapshot(&mut raw, &baseline_get, &baseline_list).await;
    raw.close().await.expect("close rebuilt raw client");
    daemon.shutdown().await.expect("rebuilt shutdown");
    fixture.assert_lock_released();

    assert_disposable_target(&fixture, &index_file);
    fs::write(&index_file, b"corrupt disposable fixture index")
        .expect("corrupt only disposable fixture index");
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    assert_readiness(&endpoint, "degraded", &fixture).await;
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("corrupt-index raw client");
    assert_initialize_index(&raw, "degraded");
    assert_exact_snapshot(&mut raw, &baseline_get, &baseline_list).await;
    let corrupt_search = raw
        .call_tool("memory_search", search_args(INDEX_QUERY_SENTINEL))
        .await
        .expect("corrupt-index search envelope");
    assert_index_degraded(&corrupt_search, &fixture);
    assert_eq!(
        error_signature(&corrupt_search),
        error_signature(&missing_search)
    );
    raw.close().await.expect("close corrupt-index client");
    daemon.shutdown().await.expect("corrupt-index shutdown");
    fixture.assert_lock_released();

    let report = rebuild_index(&fixture);
    assert!(report.replaced_existing);
    assert_eq!(
        fs::read(&index_file).expect("rebuilt corrupt index bytes"),
        baseline_index_bytes
    );
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("final official client");
    let final_search = search_shared(&mut official, INDEX_QUERY).await;
    assert_same_public_snapshot(&baseline_search, &final_search);
    assert_exact_snapshot(&mut official, &baseline_get, &baseline_list).await;
    official.close().await.expect("close final official client");
    daemon.shutdown().await.expect("final index shutdown");
    fixture.assert_lock_released();
}

#[tokio::test]
async fn writer_contention_and_service_unavailability_fail_closed_without_disclosure() {
    let fixture = Harness::new();
    let daemon = fixture.start().await;
    let endpoint = daemon_endpoint(&daemon);
    let before = snapshot_tree(&fixture.store_path);
    let second =
        RunningDaemon::start(ServeConfig::load(&fixture.config_path).expect("load config"))
            .await
            .expect_err("second writer must fail");
    let diagnostic = second.to_string();
    let owner = match &second {
        DaemonError::StoreLocked { owner: Some(owner) } => owner,
        _ => panic!("second writer returned the wrong closed error"),
    };
    assert_eq!(
        diagnostic,
        format!(
            "canonical store is owned by instance {} (pid {}, started {})",
            owner.instance_id, owner.process_id, owner.started_at
        )
    );
    assert_eq!(owner.process_id, std::process::id());
    assert_public_safe(&Value::String(diagnostic), &fixture, &[]);
    assert_eq!(snapshot_tree(&fixture.store_path), before);

    let mut official = OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN)
        .await
        .expect("official client before outage");
    let mut raw = RawHttpDriver::connect(&endpoint, RAW_TOKEN)
        .await
        .expect("raw client before outage");
    assert_eq!(
        get_shared(&mut official).await["result"]["id"],
        "mem_conformance_shared_a"
    );
    assert_eq!(
        get_shared(&mut raw).await["result"]["id"],
        "mem_conformance_shared_a"
    );

    official
        .close()
        .await
        .expect("close official before outage");
    raw.close().await.expect("close raw before outage");
    daemon.shutdown().await.expect("shutdown service");
    let ready_error = reqwest::get(format!(
        "http://{}{}",
        endpoint_addr(&endpoint),
        READINESS_ROUTE
    ))
    .await
    .expect_err("readiness must not fabricate a response after shutdown");
    assert!(!ready_error.is_status());

    let official_error = match OfficialDriver::connect(&endpoint, OFFICIAL_TOKEN).await {
        Err(error) => error,
        Ok(_) => panic!("unavailable service fabricated an official session"),
    };
    let raw_error = match RawHttpDriver::connect(&endpoint, RAW_TOKEN).await {
        Err(error) => error,
        Ok(_) => panic!("unavailable service fabricated a raw session"),
    };
    assert_transport_error_safe(&official_error, &fixture);
    assert_transport_error_safe(&raw_error, &fixture);

    let known = unavailable_raw_get(&endpoint, PRIVATE_B.project_memory).await;
    let absent = unavailable_raw_get(&endpoint, ABSENT_MEMORY).await;
    assert!(known.is_connect());
    assert!(absent.is_connect());
    fixture.assert_lock_released();
}

fn daemon_endpoint(daemon: &RunningDaemon) -> String {
    format!("http://{}{}", daemon.local_addr(), MCP_ROUTE)
}

fn endpoint_addr(endpoint: &str) -> &str {
    endpoint
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix(MCP_ROUTE))
        .expect("fixture endpoint shape")
}

fn update_args(memory_id: &str, revision: u64, title: &str, key: &str) -> Value {
    json!({
        "memoryId": memory_id,
        "expectedRevision": revision,
        "patch": { "title": title },
        "reason": "resilience conformance update",
        "idempotencyKey": key
    })
}

fn search_args(query: &str) -> Value {
    json!({
        "query": query,
        "scopes": [{ "kind": "project", "projectId": SHARED_PROJECT }],
        "limit": 100
    })
}

async fn get_shared<D: PublicMcpDriver>(driver: &mut D) -> Value {
    public_success(
        driver
            .call_tool(
                "memory_get",
                json!({ "memoryId": "mem_conformance_shared_a" }),
            )
            .await
            .expect("shared get envelope"),
    )
}

async fn list_shared<D: PublicMcpDriver>(driver: &mut D) -> Value {
    public_success(
        driver
            .call_tool(
                "memory_list",
                json!({
                    "scopes": [{ "kind": "project", "projectId": SHARED_PROJECT }],
                    "sort": "id_asc",
                    "limit": 100
                }),
            )
            .await
            .expect("shared list envelope"),
    )
}

async fn search_shared<D: PublicMcpDriver>(driver: &mut D, query: &str) -> Value {
    public_success(
        driver
            .call_tool("memory_search", search_args(query))
            .await
            .expect("shared search envelope"),
    )
}

async fn wait_for_shared_title<D: PublicMcpDriver>(driver: &mut D, title: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let listed = list_shared(driver).await;
            if let Some(id) = listed["result"]["memories"]
                .as_array()
                .expect("list memories")
                .iter()
                .find(|memory| memory["title"] == title)
                .and_then(|memory| memory["id"].as_str())
            {
                break id.to_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lost-ack mutation becomes publicly durable")
}

async fn unavailable_raw_get(endpoint: &str, memory_id: &str) -> reqwest::Error {
    let client = reqwest::Client::builder()
        .connect_timeout(UNAVAILABLE_CONNECT_TIMEOUT)
        .build()
        .expect("bounded unavailable client");
    tokio::time::timeout(
        UNAVAILABLE_REQUEST_TIMEOUT,
        client
            .post(endpoint)
            .bearer_auth(RAW_TOKEN)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-session-id", "unavailable-session")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "memory_get",
                    "arguments": { "memoryId": memory_id }
                }
            }))
            .send(),
    )
    .await
    .expect("unavailable raw request must terminate")
    .expect_err("unavailable service must not fabricate a tool response")
}

fn assert_transport_error_safe(error: &str, fixture: &Harness) {
    assert!(!error.is_empty());
    assert_public_safe(&Value::String(error.to_owned()), fixture, &[]);
    for sentinel in [
        PRIVATE_A.project_memory,
        PRIVATE_B.project_memory,
        ABSENT_MEMORY,
        "NOT_FOUND",
    ] {
        assert!(!error.contains(sentinel));
    }
}

async fn assert_exact_snapshot<D: PublicMcpDriver>(
    driver: &mut D,
    expected_get: &Value,
    expected_list: &Value,
) {
    let actual_get = get_shared(driver).await;
    let actual_list = list_shared(driver).await;
    assert_same_public_snapshot(expected_get, &actual_get);
    assert_same_public_snapshot(expected_list, &actual_list);
}

fn assert_same_public_snapshot(expected: &Value, actual: &Value) {
    assert_eq!(actual["storeRevision"], expected["storeRevision"]);
    assert_eq!(actual["result"], expected["result"]);
}

fn assert_initialize_index<D: PublicMcpDriver>(driver: &D, expected: &str) {
    assert_eq!(
        driver.initialize_result()["capabilities"]["experimental"]["jiandu"]["health"]["index"],
        expected
    );
}

async fn assert_readiness(endpoint: &str, index: &str, fixture: &Harness) {
    let base = endpoint
        .strip_suffix(MCP_ROUTE)
        .expect("fixture endpoint route");
    let response = reqwest::get(format!("{base}{READINESS_ROUTE}"))
        .await
        .expect("readiness response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("readiness JSON");
    assert_eq!(
        body,
        json!({
            "status": "ready",
            "health": {
                "store": "ready",
                "index": index,
                "exactRead": true,
                "list": true,
                "search": index == "ready"
            }
        })
    );
    assert_public_safe(&body, fixture, &[]);
}

fn assert_index_degraded(result: &Value, fixture: &Harness) {
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "INDEX_DEGRADED"
    );
    assert_eq!(result["structuredContent"]["error"]["retryable"], true);
    assert_eq!(
        result["structuredContent"]["error"]["message"],
        "Lexical memory search is temporarily unavailable."
    );
    assert_public_safe(result, fixture, &[INDEX_QUERY_SENTINEL]);
}

fn error_signature(result: &Value) -> Value {
    json!({
        "apiVersion": result["structuredContent"]["apiVersion"],
        "storeRevision": result["structuredContent"]["storeRevision"],
        "error": result["structuredContent"]["error"],
        "text": result["content"]
    })
}

fn assert_public_safe(value: &Value, fixture: &Harness, extra: &[&str]) {
    for sentinel in [
        OFFICIAL_TOKEN,
        RAW_TOKEN,
        NO_FORGET_TOKEN,
        PRIVATE_A.principal_id,
        PRIVATE_A.project_id,
        PRIVATE_A.session_id,
        PRIVATE_A.principal_memory,
        PRIVATE_A.project_memory,
        PRIVATE_A.session_memory,
        PRIVATE_B.principal_id,
        PRIVATE_B.project_id,
        PRIVATE_B.session_id,
        PRIVATE_B.principal_memory,
        PRIVATE_B.project_memory,
        PRIVATE_B.session_memory,
        "mem_conformance_shared_a",
        "mem_conformance_shared_b",
        "mem_conformance_shared_c",
        ABSENT_MEMORY,
        "ordinary conformance shared body",
        PRIVATE_A.principal_query,
        PRIVATE_A.project_query,
        PRIVATE_A.session_query,
        PRIVATE_B.principal_query,
        PRIVATE_B.project_query,
        PRIVATE_B.session_query,
    ]
    .into_iter()
    .chain(extra.iter().copied())
    {
        assert!(
            !value_contains(value, sentinel),
            "public value leaked {sentinel}"
        );
    }
    assert!(!value_contains(
        value,
        fixture.store_path.to_string_lossy().as_ref()
    ));
    assert!(!value_contains(
        value,
        fixture.config_path.to_string_lossy().as_ref()
    ));
}

fn value_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values.iter().any(|value| value_contains(value, needle)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(needle) || value_contains(value, needle)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn disposable_index_file(fixture: &Harness) -> PathBuf {
    let index_file = fixture.store_path.join("index").join("lexical.sqlite");
    assert_disposable_target(fixture, &index_file);
    index_file
}

fn assert_disposable_target(fixture: &Harness, path: &Path) {
    assert!(fixture.store_path.starts_with(fixture._sandbox.path()));
    assert_ne!(fixture.store_path, fixture._sandbox.path());
    assert_eq!(path, fixture.store_path.join("index/lexical.sqlite"));
    assert!(path.starts_with(&fixture.store_path));
    assert!(path.starts_with(fixture._sandbox.path()));
}

fn rebuild_index(fixture: &Harness) -> IndexRebuildReport {
    let principal = PrincipalId::new(PRIVATE_A.principal_id).expect("rebuild principal");
    let scopes = AuthorizedScopes::new(principal.clone())
        .with_project(ProjectId::new(SHARED_PROJECT).expect("shared project"))
        .with_project(ProjectId::new(PRIVATE_A.project_id).expect("private project"))
        .with_session(SessionId::new(PRIVATE_A.session_id).expect("private session"));
    let admin = scopes
        .authorize_index_rebuild(&TrustedRequestContext {
            principal_id: principal,
            client_id: ClientId::new("cli_resilience_rebuild").expect("rebuild client"),
            grants: BTreeSet::from([
                Grant::new("memory:admin:rebuild_index").expect("rebuild grant")
            ]),
        })
        .expect("rebuild authority");
    let store = CanonicalStore::open(
        &fixture.store_path,
        LockOwner::for_current_process().expect("rebuild owner"),
    )
    .expect("open canonical store for public rebuild");
    let canonical_revision = store.watermark().expect("canonical watermark");
    let report = LexicalIndex::new(fixture.store_path.join("index"))
        .rebuild(&store, &admin)
        .expect("public deterministic rebuild");
    assert_eq!(report.watermark.source_store_revision, canonical_revision);
    drop(store);
    report
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = fs::read_dir(current)
            .expect("read fixture tree")
            .map(|entry| entry.expect("fixture entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("fixture-relative path")
                .to_path_buf();
            let file_type = entry.file_type().expect("fixture file type");
            if file_type.is_dir() {
                output.push((relative, b"directory".to_vec()));
                visit(root, &path, output);
            } else if file_type.is_file() {
                output.push((relative, fs::read(path).expect("fixture file bytes")));
            } else {
                panic!("fixture store contains a non-file entry");
            }
        }
    }

    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}
