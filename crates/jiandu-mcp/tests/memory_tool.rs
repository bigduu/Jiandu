use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use jiandu_mcp::{
    MEMORY_ACTIONS, MEMORY_TOOL_NAME, MemoryBackend, MemoryError, MemoryExecutionContext,
    MemoryInvocation, MemoryServer, MemoryToolClass, memory_tool,
};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientInfo, JsonObject, ProtocolVersion},
};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingBackend {
    calls: Mutex<Vec<MemoryInvocation>>,
}

#[async_trait]
impl MemoryBackend for RecordingBackend {
    async fn execute(&self, invocation: MemoryInvocation) -> Result<Value, MemoryError> {
        let action = invocation.arguments.action_name();
        self.calls.lock().expect("calls lock").push(invocation);
        Ok(json!({"action": action, "success": true}))
    }
}

fn cases() -> Vec<(&'static str, Value, MemoryToolClass)> {
    use MemoryToolClass::{MutatingSerial, ReadOnlyParallel};
    vec![
        (
            "session_read",
            json!({"action":"session_read","topic":"default","options":{"max_chars":8}}),
            ReadOnlyParallel,
        ),
        (
            "session_append",
            json!({"action":"session_append","topic":"default","content":"next"}),
            MutatingSerial,
        ),
        (
            "session_replace",
            json!({"action":"session_replace","content":"replacement"}),
            MutatingSerial,
        ),
        (
            "session_clear",
            json!({"action":"session_clear","topic":"default"}),
            MutatingSerial,
        ),
        (
            "session_list_topics",
            json!({"action":"session_list_topics"}),
            ReadOnlyParallel,
        ),
        (
            "query",
            json!({"action":"query","scope":"project","query":"needle","project_key":"project_1","filters":{"type":["project"],"status":["active"],"granularity":["week"]},"options":{"limit":5,"max_chars":1000,"cursor":"5","include_related":true}}),
            ReadOnlyParallel,
        ),
        (
            "get",
            json!({"action":"get","id":"mem-1","project_key":"project_1","options":{"max_chars":500}}),
            ReadOnlyParallel,
        ),
        (
            "find_duplicates",
            json!({"action":"find_duplicates","scope":"global","title":"title","content":"body","type":"reference","tags":["one"],"options":{"limit":3}}),
            ReadOnlyParallel,
        ),
        (
            "write",
            json!({"action":"write","scope":"project","type":"project","title":"title","content":"body","tags":["one"],"project_key":"project_1","granularity":"week","options":{"allow_merge_if_similar":true}}),
            MutatingSerial,
        ),
        (
            "merge",
            json!({"action":"merge","id":"mem-1","content":"body","tags":["one"],"project_key":"project_1","source_memory_ids":["mem-2"],"mode":"merge","reason":"dedupe"}),
            MutatingSerial,
        ),
        (
            "split",
            json!({"action":"split","id":"mem-1","project_key":"project_1","pieces":[{"title":"piece","type":"reference","content":"atomic","tags":["one"]}]}),
            MutatingSerial,
        ),
        (
            "consolidate",
            json!({"action":"consolidate","ids":["mem-1","mem-2"],"title":"merged","content":"atomic","type":"project","tags":["one"],"project_key":"project_1"}),
            MutatingSerial,
        ),
        (
            "purge",
            json!({"action":"purge","id":"mem-1","reason":"obsolete","project_key":"project_1","mode":"archived"}),
            MutatingSerial,
        ),
        (
            "inspect",
            json!({"action":"inspect","scope":"project","project_key":"project_1"}),
            ReadOnlyParallel,
        ),
        (
            "rebuild",
            json!({"action":"rebuild","scope":"project","project_key":"project_1"}),
            MutatingSerial,
        ),
        (
            "scan_blobs",
            json!({"action":"scan_blobs","scope":"global","min_sections":4,"options":{"limit":10}}),
            ReadOnlyParallel,
        ),
        (
            "scan_duplicates",
            json!({"action":"scan_duplicates","scope":"global","min_score":0.75,"options":{"limit":10}}),
            ReadOnlyParallel,
        ),
    ]
}

#[tokio::test]
async fn all_seventeen_actions_parse_classify_and_dispatch_unchanged() {
    let backend = Arc::new(RecordingBackend::default());
    let server = MemoryServer::new(
        backend.clone(),
        MemoryExecutionContext::new("session-1").expect("context"),
    );
    let cases = cases();
    assert_eq!(cases.len(), 17);
    assert_eq!(
        cases.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
        MEMORY_ACTIONS
    );

    for (name, arguments, class) in cases {
        let result = server.execute(arguments.clone()).await.expect("dispatch");
        assert_eq!(result["action"], name);
        let call = backend
            .calls
            .lock()
            .expect("calls lock")
            .pop()
            .expect("call");
        assert_eq!(call.arguments.action_name(), name);
        assert_eq!(call.arguments.class(), class);
        assert_json_subset(
            &arguments,
            &serde_json::to_value(&call.arguments).expect("serialize"),
        );
        assert_eq!(call.session_id, "session-1");
    }
}

fn assert_json_subset(expected: &Value, actual: &Value) {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, expected) in expected {
                assert_json_subset(
                    expected,
                    actual
                        .get(key)
                        .unwrap_or_else(|| panic!("missing key {key}")),
                );
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            assert_eq!(expected.len(), actual.len());
            for (expected, actual) in expected.iter().zip(actual) {
                assert_json_subset(expected, actual);
            }
        }
        _ => assert_eq!(expected, actual),
    }
}

#[test]
fn generated_schema_has_one_complete_branch_per_action() {
    let tool = memory_tool();
    assert_eq!(tool.name, MEMORY_TOOL_NAME);
    let schema = tool.schema_as_json_value();
    let branches = schema["oneOf"].as_array().expect("tagged enum oneOf");
    assert_eq!(branches.len(), 17);

    let contracts: [(&str, &[&str], &[&str]); 17] = [
        ("session_read", &["action", "options", "topic"], &["action"]),
        (
            "session_append",
            &["action", "content", "topic"],
            &["action", "content"],
        ),
        (
            "session_replace",
            &["action", "content", "topic"],
            &["action", "content"],
        ),
        ("session_clear", &["action", "topic"], &["action"]),
        ("session_list_topics", &["action"], &["action"]),
        (
            "query",
            &[
                "action",
                "filters",
                "options",
                "project_key",
                "query",
                "scope",
            ],
            &["action", "scope"],
        ),
        (
            "get",
            &["action", "id", "options", "project_key"],
            &["action", "id"],
        ),
        (
            "find_duplicates",
            &[
                "action",
                "content",
                "options",
                "project_key",
                "scope",
                "tags",
                "title",
                "type",
            ],
            &["action", "scope", "title"],
        ),
        (
            "write",
            &[
                "action",
                "content",
                "granularity",
                "options",
                "project_key",
                "scope",
                "tags",
                "title",
                "type",
            ],
            &["action", "content", "scope", "title", "type"],
        ),
        (
            "merge",
            &[
                "action",
                "content",
                "id",
                "mode",
                "project_key",
                "reason",
                "source_memory_ids",
                "tags",
            ],
            &["action", "content", "id"],
        ),
        (
            "split",
            &["action", "id", "pieces", "project_key"],
            &["action", "id", "pieces"],
        ),
        (
            "consolidate",
            &[
                "action",
                "content",
                "ids",
                "project_key",
                "tags",
                "title",
                "type",
            ],
            &["action", "content", "ids", "title"],
        ),
        (
            "purge",
            &[
                "action",
                "filters",
                "id",
                "mode",
                "project_key",
                "reason",
                "scope",
            ],
            &["action"],
        ),
        (
            "inspect",
            &["action", "project_key", "scope"],
            &["action", "scope"],
        ),
        (
            "rebuild",
            &["action", "project_key", "scope"],
            &["action", "scope"],
        ),
        (
            "scan_blobs",
            &["action", "min_sections", "options", "project_key", "scope"],
            &["action", "scope"],
        ),
        (
            "scan_duplicates",
            &["action", "min_score", "options", "project_key", "scope"],
            &["action", "scope"],
        ),
    ];
    for (action, properties, required) in contracts {
        let branch = branches
            .iter()
            .find(|branch| branch["properties"]["action"]["const"] == action)
            .unwrap_or_else(|| panic!("missing schema branch for {action}"));
        let mut actual_properties = branch["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        actual_properties.sort_unstable();
        assert_eq!(actual_properties, properties, "properties for {action}");
        let mut actual_required = branch["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|value| value.as_str().expect("required field"))
            .collect::<Vec<_>>();
        actual_required.sort_unstable();
        assert_eq!(actual_required, required, "required fields for {action}");
    }

    let serialized = serde_json::to_string(&schema).expect("schema JSON");
    assert!(serialized.contains("allow_merge_if_similar"));
    assert!(serialized.contains("include_related"));
}

#[tokio::test]
async fn session_actions_use_host_context_without_changing_request_schema() {
    let backend = Arc::new(RecordingBackend::default());
    let server = MemoryServer::new(
        backend.clone(),
        MemoryExecutionContext::new("session.context-1").expect("context"),
    );
    for action in [
        json!({"action":"session_read"}),
        json!({"action":"session_append","content":"a"}),
        json!({"action":"session_replace","content":"b"}),
        json!({"action":"session_clear"}),
        json!({"action":"session_list_topics"}),
    ] {
        server.execute(action).await.expect("session dispatch");
    }
    let calls = backend.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 5);
    assert!(
        calls
            .iter()
            .all(|call| call.session_id == "session.context-1")
    );
    assert!(calls.iter().all(|call| call.project_id.is_none()));
}

#[tokio::test]
async fn stable_project_identity_comes_from_context_or_matching_argument() {
    let backend = Arc::new(RecordingBackend::default());
    let context = MemoryExecutionContext::new("session-1")
        .expect("context")
        .with_project_id("project_1")
        .expect("project context");
    let server = MemoryServer::new(backend.clone(), context);

    server
        .execute(json!({"action":"query","scope":"project"}))
        .await
        .expect("context project");
    server
        .execute(json!({"action":"get","id":"mem-1","project_key":"project_1"}))
        .await
        .expect("matching project");
    let mismatch = server
        .execute(json!({"action":"inspect","scope":"project","project_key":"project_2"}))
        .await
        .expect_err("project override must fail");
    assert!(mismatch.to_string().contains("cannot override"));

    let calls = backend.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| { call.project_id.as_ref().expect("project").as_str() == "project_1" })
    );
}

#[tokio::test]
async fn merge_modes_and_both_purge_shapes_dispatch_without_a_compatibility_protocol() {
    let backend = Arc::new(RecordingBackend::default());
    let server = MemoryServer::new(
        backend.clone(),
        MemoryExecutionContext::new("session-1").expect("context"),
    );
    let cases = [
        json!({"action":"merge","id":"m1","content":"merged","mode":"merge"}),
        json!({"action":"merge","id":"m1","content":"contradiction","mode":"contradict","source_memory_ids":["m2"]}),
        json!({"action":"purge","id":"m1","mode":"archived"}),
        json!({"action":"purge","scope":"global","filters":{"status":["stale"]},"mode":"archived"}),
    ];
    for arguments in cases {
        server.execute(arguments).await.expect("dispatch");
    }
    assert_eq!(backend.calls.lock().expect("calls lock").len(), 4);
}

#[tokio::test]
async fn malformed_action_and_path_like_project_id_fail_before_backend() {
    let backend = Arc::new(RecordingBackend::default());
    let server = MemoryServer::new(
        backend.clone(),
        MemoryExecutionContext::new("session-1").expect("context"),
    );
    assert!(
        server
            .execute(json!({"action":"memory_search"}))
            .await
            .is_err()
    );
    assert!(
        server
            .execute(json!({"action":"query","scope":"project","project_key":"../workspace"}))
            .await
            .is_err()
    );
    assert!(backend.calls.lock().expect("calls lock").is_empty());
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

#[tokio::test]
async fn rmcp_duplex_lists_only_memory_and_calls_backend() {
    let backend = Arc::new(RecordingBackend::default());
    let server = MemoryServer::new(
        backend.clone(),
        MemoryExecutionContext::new("session-protocol").expect("context"),
    );
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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

    let info = client.peer_info().expect("initialize response");
    assert_eq!(
        info.server_info.as_ref().expect("server info").name,
        "jiandu"
    );
    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, MEMORY_TOOL_NAME);
    assert!(
        client
            .call_tool(CallToolRequestParams::new("memory_search"))
            .await
            .is_err(),
        "the removed six-tool surface must not remain as an alias"
    );
    assert!(backend.calls.lock().expect("calls lock").is_empty());

    let arguments: JsonObject = json!({
        "action": "write",
        "scope": "global",
        "type": "project",
        "title": "Protocol",
        "content": "Reached backend"
    })
    .as_object()
    .expect("object")
    .clone();
    let result = client
        .call_tool(CallToolRequestParams::new(MEMORY_TOOL_NAME).with_arguments(arguments))
        .await
        .expect("call tool");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content.expect("structured")["action"],
        "write"
    );
    {
        let calls = backend.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].session_id, "session-protocol");
        assert_eq!(calls[0].arguments.action_name(), "write");
    }

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}
