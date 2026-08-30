use std::collections::HashSet;

use jiandu_mcp::{
    MEMORY_ACTIONS, MEMORY_SERVER_INSTRUCTIONS, MEMORY_TOOL_NAME, MemoryArgs,
    MemoryExecutionContext, MemoryServer, MemoryToolClass, memory_tool,
};
use jiandu_memory::memory_store::{MAX_MEMORY_ID_LEN, MemoryStore};
#[cfg(unix)]
use jiandu_memory::memory_store::{MemoryScope, WRITE_AUDIT_LOG};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientInfo, JsonObject, ProtocolVersion},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _directory: TempDir,
    server: MemoryServer,
}

impl Fixture {
    fn global(session_id: &str) -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let server = MemoryServer::new(
            MemoryStore::new(directory.path()),
            MemoryExecutionContext::new(session_id).expect("context"),
        );
        Self {
            _directory: directory,
            server,
        }
    }
}

async fn write_global(server: &MemoryServer, title: &str, content: &str) -> String {
    let result = server
        .execute(json!({
            "action": "write",
            "scope": "global",
            "type": "reference",
            "title": title,
            "content": content,
        }))
        .await
        .expect("write memory");
    result["memory"]["id"]
        .as_str()
        .expect("memory id")
        .to_string()
}

/// A cancelled MCP request must detach, not abort, a mutation whose Tokio
/// filesystem open is still blocked while holding the durable scope guard.
/// Otherwise a second writer could enter while that abandoned blocking open is
/// still live, or a same-server recovery read could observe the mutation before
/// its derived artifacts settle.
#[cfg(unix)]
#[tokio::test]
async fn cancelled_mcp_waiter_keeps_scope_guard_until_owned_mutation_finishes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = MemoryStore::new(directory.path());
    let audit_path = store
        .resolver()
        .logs_dir(MemoryScope::Global, None)
        .join(WRITE_AUDIT_LOG);
    std::fs::create_dir_all(audit_path.parent().expect("audit parent")).expect("logs dir");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&audit_path)
            .status()
            .expect("run mkfifo")
            .success(),
        "create a controllably blocking audit sink"
    );

    let server = std::sync::Arc::new(MemoryServer::new(
        store.clone(),
        MemoryExecutionContext::new("session_cancel").expect("context"),
    ));
    let first_server = std::sync::Arc::clone(&server);
    let first_waiter = tokio::spawn(async move {
        first_server
            .execute(json!({
                "action": "write",
                "scope": "global",
                "type": "reference",
                "title": "Owned mutation survives waiter cancellation",
                "content": "The canonical document commits before the audit FIFO is released.",
                "options": {"allow_merge_if_similar": false}
            }))
            .await
    });

    let mut canonical_committed = false;
    for _ in 0..500 {
        if store
            .list_memory_documents(MemoryScope::Global, None)
            .await
            .expect("list canonical docs")
            .len()
            == 1
        {
            canonical_committed = true;
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        canonical_committed,
        "first mutation reached its canonical write"
    );
    // Let the owned mutation advance from the canonical rename to the FIFO open,
    // where it remains blocked while still owning the scope guard.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        !first_waiter.is_finished(),
        "audit FIFO still blocks the call"
    );

    first_waiter.abort();
    assert!(
        first_waiter
            .await
            .expect_err("waiter was cancelled")
            .is_cancelled(),
        "outer MCP waiter cancellation is observed"
    );

    let read_server = std::sync::Arc::clone(&server);
    let read_waiter = tokio::spawn(async move {
        read_server
            .execute(json!({"action": "inspect", "scope": "global"}))
            .await
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let read_finished_before_first_io = read_waiter.is_finished();

    let drain_server = std::sync::Arc::clone(&server);
    let drain_waiter = tokio::spawn(async move {
        drain_server.wait_for_in_flight_mutations().await;
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let drain_finished_before_first_io = drain_waiter.is_finished();

    let second_server = std::sync::Arc::clone(&server);
    let second_waiter = tokio::spawn(async move {
        second_server
            .execute(json!({"action": "rebuild", "scope": "global"}))
            .await
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let second_finished_before_first_io = second_waiter.is_finished();

    // Always release the blocking syscall before asserting, so even a broken
    // implementation cannot strand Tokio's blocking pool during test teardown.
    let reader_path = audit_path.clone();
    let audit_reader = std::thread::spawn(move || {
        use std::io::Read;

        let mut raw = String::new();
        std::fs::File::open(reader_path)
            .expect("open audit FIFO reader")
            .read_to_string(&mut raw)
            .expect("read audit FIFO");
        raw
    });
    let rebuild = second_waiter
        .await
        .expect("second waiter joins")
        .expect("rebuild succeeds after first mutation releases the guard");
    drain_waiter.await.expect("mutation drain joins");
    let inspect = read_waiter
        .await
        .expect("read waiter joins")
        .expect("inspect succeeds after accepted mutations settle");
    let audit = audit_reader.join().expect("audit reader joins");

    assert!(
        !read_finished_before_first_io,
        "same-server recovery read completed before the cancelled waiter's mutation settled"
    );
    assert!(
        !drain_finished_before_first_io,
        "in-flight drain must wait for the cancelled waiter's owned mutation"
    );
    assert!(
        !second_finished_before_first_io,
        "second same-scope writer entered before the cancelled waiter's underlying I/O ended"
    );
    assert_eq!(rebuild["action"], "rebuild");
    assert_eq!(inspect["action"], "inspect");
    assert_eq!(inspect["data"]["total_memories"], 1);
    assert!(
        inspect["data"]["view_files"]
            .as_array()
            .expect("view files")
            .iter()
            .any(|name| name == "MEMORY.md"),
        "inspect observes the rebuilt memory view"
    );
    assert!(
        inspect["data"]["index_files"]
            .as_array()
            .expect("index files")
            .iter()
            .any(|name| name == "lexical.json"),
        "inspect observes the rebuilt lexical index"
    );
    assert!(
        audit.contains("Owned mutation survives waiter cancellation"),
        "detached owned mutation completed its audit write"
    );
}

#[tokio::test]
async fn all_seventeen_actions_parse_classify_and_dispatch_to_the_real_store() {
    use MemoryToolClass::{MutatingSerial, ReadOnlyParallel};

    let fixture = Fixture::global("session_dispatch");
    let server = &fixture.server;
    let get_id = write_global(server, "Dispatch get", "needle get body").await;
    let merge_id = write_global(server, "Dispatch merge", "merge target body").await;
    let merge_source_id = write_global(server, "Dispatch merge source", "merge source body").await;
    let split_id = write_global(server, "Dispatch split", "split source body").await;
    let consolidate_a = write_global(server, "Dispatch consolidate A", "first source body").await;
    let consolidate_b = write_global(server, "Dispatch consolidate B", "second source body").await;
    let purge_id = write_global(server, "Dispatch purge", "purge source body").await;

    let cases = vec![
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
            json!({"action":"query","scope":"global","query":"needle","filters":{"type":["reference"],"status":["active"],"granularity":[]},"options":{"limit":5,"max_chars":1000,"include_related":true}}),
            ReadOnlyParallel,
        ),
        (
            "get",
            json!({"action":"get","id":get_id,"options":{"max_chars":500}}),
            ReadOnlyParallel,
        ),
        (
            "find_duplicates",
            json!({"action":"find_duplicates","scope":"global","title":"Dispatch get","content":"needle get body","type":"reference","tags":["one"],"options":{"limit":3}}),
            ReadOnlyParallel,
        ),
        (
            "write",
            json!({"action":"write","scope":"global","type":"project","title":"Dispatch explicit write","content":"explicit write body","tags":["one"],"granularity":"week","options":{"allow_merge_if_similar":false}}),
            MutatingSerial,
        ),
        (
            "merge",
            json!({"action":"merge","id":merge_id,"content":"new merged section","tags":["one"],"source_memory_ids":[merge_source_id],"mode":"merge","reason":"dedupe"}),
            MutatingSerial,
        ),
        (
            "split",
            json!({"action":"split","id":split_id,"pieces":[{"title":"Dispatch atomic piece","type":"reference","content":"atomic split body","tags":["one"]}]}),
            MutatingSerial,
        ),
        (
            "consolidate",
            json!({"action":"consolidate","ids":[consolidate_a,consolidate_b],"title":"Dispatch canonical","content":"canonical body","type":"project","tags":["one"]}),
            MutatingSerial,
        ),
        (
            "purge",
            json!({"action":"purge","id":purge_id,"reason":"obsolete","mode":"archived"}),
            MutatingSerial,
        ),
        (
            "inspect",
            json!({"action":"inspect","scope":"global"}),
            ReadOnlyParallel,
        ),
        (
            "rebuild",
            json!({"action":"rebuild","scope":"global"}),
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
    ];

    assert_eq!(cases.len(), 17);
    assert_eq!(
        cases.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
        MEMORY_ACTIONS
    );

    for (name, arguments, expected_class) in cases {
        let parsed: MemoryArgs = serde_json::from_value(arguments.clone()).expect("parse args");
        assert_eq!(parsed.action_name(), name);
        assert_eq!(parsed.class(), expected_class);
        let result = server.execute(arguments).await.expect("real dispatch");
        assert_eq!(result["action"], name);
    }
}

#[test]
fn generated_schema_has_complete_actions_value_domains_and_safe_ids() {
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
        let branch = branch(&schema, action);
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

    for action in [
        "query",
        "write",
        "find_duplicates",
        "scan_blobs",
        "scan_duplicates",
        "inspect",
        "rebuild",
    ] {
        assert_enum(
            &branch(&schema, action)["properties"]["scope"],
            &["project", "global"],
        );
    }
    assert_enum(
        &branch(&schema, "write")["properties"]["type"],
        &["user", "feedback", "project", "reference"],
    );
    assert_enum(
        &branch(&schema, "write")["properties"]["granularity"],
        &["day", "week", "month", "quarter", "year"],
    );
    assert_enum(
        &branch(&schema, "merge")["properties"]["mode"],
        &["merge", "semantic_merge", "contradict"],
    );
    assert_enum(
        &branch(&schema, "purge")["properties"]["mode"],
        &["active", "stale", "superseded", "contradicted", "archived"],
    );
    let filter_schema =
        referenced_schema(&schema, &branch(&schema, "query")["properties"]["filters"]);
    assert_enum(
        &filter_schema["properties"]["type"]["items"],
        &["user", "feedback", "project", "reference"],
    );
    assert_enum(
        &filter_schema["properties"]["status"]["items"],
        &["active", "stale", "superseded", "contradicted", "archived"],
    );
    assert_enum(
        &filter_schema["properties"]["granularity"]["items"],
        &["day", "week", "month", "quarter", "year"],
    );

    for (action, field) in [
        ("get", "id"),
        ("merge", "id"),
        ("split", "id"),
        ("purge", "id"),
    ] {
        assert_id_schema(&branch(&schema, action)["properties"][field]);
    }
    assert_id_schema(&branch(&schema, "merge")["properties"]["source_memory_ids"]["items"]);
    assert_id_schema(&branch(&schema, "consolidate")["properties"]["ids"]["items"]);
    let project_key = &branch(&schema, "write")["properties"]["project_key"];
    assert_eq!(
        find_key(project_key, "maxLength").and_then(Value::as_u64),
        Some(64)
    );
    assert_eq!(
        find_key(project_key, "pattern").and_then(Value::as_str),
        Some("^[A-Za-z0-9_-]+$")
    );
    assert!(
        find_key(project_key, "description")
            .and_then(Value::as_str)
            .expect("project authority description")
            .contains("cannot grant Project access")
    );

    let serialized = serde_json::to_string(&schema).expect("schema JSON");
    assert!(serialized.contains("allow_merge_if_similar"));
    assert!(serialized.contains("include_related"));
}

fn branch<'a>(schema: &'a Value, action: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .expect("oneOf")
        .iter()
        .find(|branch| branch["properties"]["action"]["const"] == action)
        .unwrap_or_else(|| panic!("missing schema branch for {action}"))
}

fn referenced_schema<'a>(root: &'a Value, schema: &Value) -> &'a Value {
    let reference = find_key(schema, "$ref")
        .and_then(Value::as_str)
        .expect("schema reference");
    root.pointer(reference.strip_prefix('#').expect("local reference"))
        .expect("referenced schema")
}

fn assert_enum(schema: &Value, expected: &[&str]) {
    let values = find_key(schema, "enum")
        .and_then(Value::as_array)
        .expect("enum values")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(values, expected);
}

fn assert_id_schema(schema: &Value) {
    assert_eq!(
        find_key(schema, "maxLength").and_then(Value::as_u64),
        Some(MAX_MEMORY_ID_LEN as u64)
    );
    assert_eq!(
        find_key(schema, "pattern").and_then(Value::as_str),
        Some("^[A-Za-z0-9_-]+$")
    );
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => object
            .get(key)
            .or_else(|| object.values().find_map(|value| find_key(value, key))),
        Value::Array(array) => array.iter().find_map(|value| find_key(value, key)),
        _ => None,
    }
}

#[tokio::test]
async fn session_actions_use_the_host_context_and_real_session_files() {
    let fixture = Fixture::global("session.context-1");
    let server = &fixture.server;
    server
        .execute(json!({"action":"session_append","content":"first"}))
        .await
        .expect("append");
    server
        .execute(json!({"action":"session_append","content":"second"}))
        .await
        .expect("append");
    let read = server
        .execute(json!({"action":"session_read","options":{"max_chars":8}}))
        .await
        .expect("read");
    assert_eq!(read["session_id"], "session.context-1");
    assert_eq!(read["length_chars"], 13);
    assert_eq!(read["content"], "first\n\ns");
    assert_eq!(read["body_truncated"], true);
    let topics = server
        .execute(json!({"action":"session_list_topics"}))
        .await
        .expect("topics");
    assert_eq!(topics["topics"], json!(["default"]));
    server
        .execute(json!({"action":"session_clear"}))
        .await
        .expect("clear");
}

#[tokio::test]
async fn project_identity_is_stable_scoped_and_cannot_be_overridden() {
    let directory = tempfile::tempdir().expect("tempdir");
    let project_a = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_a")
            .expect("context")
            .with_project_id("project_a")
            .expect("project"),
    );
    let project_b = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_b")
            .expect("context")
            .with_project_id("project_b")
            .expect("project"),
    );
    let no_project = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_global").expect("context"),
    );

    let written = project_a
        .execute(json!({
            "action":"write",
            "scope":"project",
            "type":"project",
            "title":"Project A only",
            "content":"isolated content"
        }))
        .await
        .expect("project write");
    let id = written["memory"]["id"].as_str().expect("id");
    assert_eq!(written["memory"]["project_key"], "project_a");

    let query = project_a
        .execute(json!({"action":"query","scope":"project"}))
        .await
        .expect("project query");
    assert_eq!(query["data"]["matched_count"], 1);
    project_a
        .execute(json!({"action":"get","id":format!(" {id} "),"project_key":"project_a"}))
        .await
        .expect("matching project with a trimmed id");

    let mismatch = project_a
        .execute(json!({"action":"inspect","scope":"project","project_key":"project_b"}))
        .await
        .expect_err("override must fail");
    assert!(mismatch.to_string().contains("cannot override"));
    assert!(
        project_b
            .execute(json!({"action":"get","id":id}))
            .await
            .is_err(),
        "another project context must not discover the memory"
    );
    assert!(
        no_project
            .execute(json!({"action":"get","id":id}))
            .await
            .is_err(),
        "an unscoped context must not enumerate project memories"
    );
}

#[tokio::test]
async fn request_project_key_never_self_grants_project_authority() {
    let directory = tempfile::tempdir().expect("tempdir");
    let project_a = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_authority_a")
            .expect("context")
            .with_project_id("project_a")
            .expect("project"),
    );
    let project_b = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_authority_b")
            .expect("context")
            .with_project_id("project_b")
            .expect("project"),
    );
    let unassigned = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_unassigned").expect("context"),
    );
    let written = project_a
        .execute(json!({
            "action":"write",
            "scope":"project",
            "type":"project",
            "title":"Authority boundary",
            "content":"owned by A"
        }))
        .await
        .expect("seed A");
    let id = written["memory"]["id"].as_str().expect("id");

    for request in [
        json!({"action":"get","id":id,"project_key":"project_a"}),
        json!({"action":"write","scope":"project","project_key":"project_a","type":"project","title":"unauthorized","content":"must fail"}),
        json!({"action":"merge","id":id,"project_key":"project_a","content":"must fail"}),
        json!({"action":"purge","id":id,"project_key":"project_a"}),
        json!({"action":"rebuild","scope":"project","project_key":"project_a"}),
    ] {
        let error = unassigned
            .execute(request)
            .await
            .expect_err("request data cannot grant authority");
        assert!(error.to_string().contains("cannot grant Project access"));
    }

    let project_b_state = project_b
        .execute(json!({"action":"inspect","scope":"project"}))
        .await
        .expect("inspect B");
    assert_eq!(project_b_state["data"]["total_memories"], 0);
    let project_a_memory = project_a
        .execute(json!({"action":"get","id":id}))
        .await
        .expect("A remains readable");
    assert_eq!(project_a_memory["memory"]["body"], "owned by A");
    assert_eq!(
        project_a_memory["memory"]["frontmatter"]["status"],
        "active"
    );
}

#[tokio::test]
async fn session_append_lock_is_shared_between_server_instances() {
    let directory = tempfile::tempdir().expect("tempdir");
    let first = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("shared_session").expect("context"),
    );
    let second = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("shared_session").expect("context"),
    );

    for index in 0..50 {
        let alpha = format!("alpha_{index}");
        let beta = format!("beta_{index}");
        let (first_result, second_result) = tokio::join!(
            first.execute(json!({"action":"session_append","content":alpha})),
            second.execute(json!({"action":"session_append","content":beta})),
        );
        first_result.expect("first append");
        second_result.expect("second append");
    }

    let read = first
        .execute(json!({"action":"session_read"}))
        .await
        .expect("read all appends");
    let content = read["content"].as_str().expect("content");
    let fragments = content.split("\n\n").collect::<HashSet<_>>();
    assert_eq!(fragments.len(), 100);
    for index in 0..50 {
        assert!(fragments.contains(format!("alpha_{index}").as_str()));
        assert!(fragments.contains(format!("beta_{index}").as_str()));
    }
}

#[tokio::test]
async fn merge_modes_and_single_and_batch_purge_map_to_the_real_store() {
    let fixture = Fixture::global("session_lifecycle");
    let server = &fixture.server;

    let merge_target = write_global(server, "Merge target", "first fact").await;
    let merge_source = write_global(server, "Merge source", "second fact").await;
    let merged = server
        .execute(json!({
            "action":"merge",
            "id":format!(" {merge_target} "),
            "content":"coherent addition",
            "mode":"semantic_merge",
            "source_memory_ids":[format!(" {merge_source} ")]
        }))
        .await
        .expect("semantic merge");
    assert_eq!(merged["mode"], "semantic_merge");

    let contradiction_target = write_global(server, "Old fact", "old value").await;
    let contradiction_source = write_global(server, "New fact", "new value").await;
    let contradicted = server
        .execute(json!({
            "action":"merge",
            "id":contradiction_target,
            "content":"superseded by newer evidence",
            "mode":"contradict",
            "source_memory_ids":[contradiction_source]
        }))
        .await
        .expect("contradict");
    assert_eq!(contradicted["mode"], "contradict");

    let single_id = write_global(server, "Single purge", "archive this").await;
    let single = server
        .execute(json!({"action":"purge","id":single_id,"mode":"archived"}))
        .await
        .expect("single purge");
    assert_eq!(single["status"], "archived");

    write_global(server, "Batch purge", "archive active entries").await;
    let batch = server
        .execute(json!({
            "action":"purge",
            "scope":"global",
            "filters":{"status":["active"]},
            "mode":"archived"
        }))
        .await
        .expect("batch purge");
    assert!(batch["data"]["matched_count"].as_u64().expect("count") >= 1);
}

#[tokio::test]
async fn malformed_actions_project_ids_and_all_memory_id_shapes_fail_closed() {
    let fixture = Fixture::global("session_validation");
    let server = &fixture.server;
    assert!(
        server
            .execute(json!({"action":"memory_search"}))
            .await
            .is_err()
    );
    assert!(
        server
            .execute(json!({
                "action":"query",
                "scope":"project",
                "project_key":"../workspace"
            }))
            .await
            .is_err()
    );

    for arguments in [
        json!({"action":"get","id":"../../secret"}),
        json!({"action":"merge","id":"/tmp/secret","content":"x"}),
        json!({"action":"split","id":"..","pieces":[{"title":"x","content":"x"}]}),
        json!({"action":"purge","id":"folder/file"}),
        json!({"action":"consolidate","ids":["safe_id","../unsafe"],"title":"x","content":"x"}),
        json!({"action":"merge","id":"safe_id","content":"x","source_memory_ids":["../unsafe"]}),
    ] {
        let error = server
            .execute(arguments)
            .await
            .expect_err("unsafe id must fail");
        assert!(error.to_string().contains("memory id"));
    }

    for arguments in [
        json!({"action":"query","scope":"session"}),
        json!({"action":"write","scope":"global","type":"unknown","title":"x","content":"x"}),
        json!({"action":"write","scope":"global","type":"project","title":"x","content":"x","granularity":"decade"}),
        json!({"action":"query","scope":"global","filters":{"type":["unknown"]}}),
        json!({"action":"query","scope":"global","filters":{"status":["unknown"]}}),
        json!({"action":"query","scope":"global","filters":{"granularity":["decade"]}}),
        json!({"action":"merge","id":"safe_id","content":"x","mode":"unknown"}),
        json!({"action":"purge","id":"safe_id","mode":"unknown"}),
        json!({"action":"split","id":"safe_id","pieces":[]}),
        json!({"action":"consolidate","ids":["safe_id"],"title":"x","content":"x"}),
    ] {
        server
            .execute(arguments)
            .await
            .expect_err("invalid value domain must fail");
    }

    assert!(MemoryExecutionContext::new(".").is_err());
    assert!(
        MemoryExecutionContext::new("valid_session")
            .expect("session")
            .with_project_id("../project")
            .is_err()
    );

    let blank_purge = server
        .execute(json!({"action":"purge","id":"   "}))
        .await
        .expect_err("blank id follows the bulk purge branch");
    assert!(
        blank_purge
            .to_string()
            .contains("purge supports durable scopes only")
    );
    assert!(!blank_purge.to_string().contains("path-safe"));
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
async fn rmcp_duplex_lists_only_memory_and_runs_real_write_query_get() {
    let directory = tempfile::tempdir().expect("tempdir");
    let server = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("session_protocol").expect("context"),
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
    assert_eq!(
        info.instructions.as_deref(),
        Some(MEMORY_SERVER_INSTRUCTIONS)
    );
    let instructions = info.instructions.as_deref().expect("instructions");
    for required_guidance in [
        "query",
        "get",
        "inspect",
        "session_append",
        "write",
        "Project scope",
        "Global",
        "project_key",
        "Never store secrets",
        "supporting evidence",
        "not an all-or-nothing transaction",
        "committed canonical memory",
        "mutation error",
        "cancellation or disconnect",
        "same server",
        "subsequent read-only calls wait",
        "run `inspect` first",
        "run `rebuild`",
        "never blindly retry",
    ] {
        assert!(
            instructions.contains(required_guidance),
            "missing runtime guidance: {required_guidance}"
        );
    }
    assert!(
        !instructions.contains("failed tool call did not recall or persist anything"),
        "instructions must not promise transactional failure semantics"
    );
    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, MEMORY_TOOL_NAME);
    assert!(
        client
            .call_tool(CallToolRequestParams::new("memory_search"))
            .await
            .is_err(),
        "the removed multi-tool surface must not remain as an alias"
    );

    let write = call_memory(
        &client,
        json!({
            "action": "write",
            "scope": "global",
            "type": "project",
            "title": "Protocol persistence",
            "content": "written through the MCP transport"
        }),
    )
    .await;
    let id = write["memory"]["id"].as_str().expect("id").to_string();
    let query = call_memory(
        &client,
        json!({"action":"query","scope":"global","query":"transport"}),
    )
    .await;
    assert_eq!(query["data"]["items"][0]["id"], id);
    let get = call_memory(&client, json!({"action":"get","id":id})).await;
    assert_eq!(get["memory"]["body"], "written through the MCP transport");

    let invalid_arguments: JsonObject = json!({"action":"get","id":"../escape"})
        .as_object()
        .expect("object")
        .clone();
    let invalid = client
        .call_tool(CallToolRequestParams::new(MEMORY_TOOL_NAME).with_arguments(invalid_arguments))
        .await
        .expect("tool errors are returned as call results");
    assert_eq!(invalid.is_error, Some(true));
    assert!(invalid.structured_content.is_none());

    client.cancel().await.expect("cancel client");
    server_task.await.expect("join server");
}

async fn call_memory(
    client: &rmcp::service::RunningService<rmcp::RoleClient, V2025Client>,
    arguments: Value,
) -> Value {
    let arguments: JsonObject = arguments.as_object().expect("object").clone();
    let result = client
        .call_tool(CallToolRequestParams::new(MEMORY_TOOL_NAME).with_arguments(arguments))
        .await
        .expect("call tool");
    assert_eq!(result.is_error, Some(false));
    let structured = result.structured_content.expect("structured result");
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .expect("text result");
    let text_value: Value = serde_json::from_str(&text.text).expect("text result is JSON");
    assert_eq!(text_value, structured);
    structured
}
