use std::collections::HashSet;

use jiandu_mcp::{
    MEMORY_ACTIONS, MEMORY_SERVER_INSTRUCTIONS, MEMORY_TOOL_DESCRIPTION, MEMORY_TOOL_NAME,
    MemoryArgs, MemoryExecutionContext, MemoryServer, MemoryToolClass, memory_tool,
};
#[cfg(unix)]
use jiandu_memory::memory_store::WRITE_AUDIT_LOG;
use jiandu_memory::memory_store::{
    MAX_MEMORY_ENTITIES, MAX_MEMORY_ID_LEN, MAX_MEMORY_KEYWORDS, MAX_MEMORY_TAG_CHARS,
    MAX_MEMORY_TAGS, MAX_RETRIEVAL_TERM_CHARS, MemoryScope, MemoryStore, render_markdown_document,
};
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

    let drain_server = std::sync::Arc::clone(&server);
    let drain_waiter = tokio::spawn(async move {
        drain_server.wait_for_in_flight_mutations().await;
    });

    let second_server = std::sync::Arc::clone(&server);
    let second_waiter = tokio::spawn(async move {
        second_server
            .execute(json!({"action": "rebuild", "scope": "global"}))
            .await
    });

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
async fn all_nineteen_actions_parse_classify_and_dispatch_to_the_real_store() {
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
    let dream_generation = server
        .execute(json!({"action":"dream_read","scope":"global"}))
        .await
        .expect("read cold Dream generation")["current_generation"]
        .as_str()
        .expect("Dream generation")
        .to_string();

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
            "dream_read",
            json!({"action":"dream_read","scope":"global"}),
            ReadOnlyParallel,
        ),
        (
            "dream_publish",
            json!({"action":"dream_publish","scope":"global","source_generation":dream_generation,"content":"## Current orientation\n\n- Compact host-generated context."}),
            MutatingSerial,
        ),
        (
            "find_duplicates",
            json!({"action":"find_duplicates","scope":"global","title":"Dispatch get","content":"needle get body","type":"reference","tags":["one"],"keywords":["dispatch-alias"],"entities":["Dispatch API"],"options":{"limit":3}}),
            ReadOnlyParallel,
        ),
        (
            "write",
            json!({"action":"write","scope":"global","type":"project","title":"Dispatch explicit write","content":"explicit write body","tags":["one"],"keywords":["write-alias"],"entities":["Write API"],"granularity":"week","options":{"allow_merge_if_similar":false}}),
            MutatingSerial,
        ),
        (
            "merge",
            json!({"action":"merge","id":merge_id,"content":"new merged section","tags":["one"],"keywords":["merge-alias"],"entities":["Merge API"],"source_memory_ids":[merge_source_id],"mode":"merge","reason":"dedupe"}),
            MutatingSerial,
        ),
        (
            "split",
            json!({"action":"split","id":split_id,"pieces":[{"title":"Dispatch atomic piece","type":"reference","content":"atomic split body","tags":["one"],"keywords":["split-alias"],"entities":["Split API"]}]}),
            MutatingSerial,
        ),
        (
            "consolidate",
            json!({"action":"consolidate","ids":[consolidate_a,consolidate_b],"title":"Dispatch canonical","content":"canonical body","type":"project","tags":["one"],"keywords":["canonical-alias"],"entities":["Canonical API"]}),
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

    assert_eq!(cases.len(), 19);
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

#[tokio::test]
async fn dream_is_generation_stamped_atomic_and_project_isolated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let global_store = MemoryStore::new(directory.path());
    let global = MemoryServer::new(
        global_store.clone(),
        MemoryExecutionContext::new("dream_global").expect("context"),
    );
    write_global(&global, "Dream source one", "First canonical Dream source.").await;

    std::fs::remove_file(
        global_store
            .resolver()
            .scope_generation_path(MemoryScope::Global, None),
    )
    .expect("simulate an existing pre-generation store");
    let upgrade_error = global
        .execute(json!({"action":"dream_read","scope":"global"}))
        .await
        .expect_err("non-empty old store requires rebuild");
    assert!(upgrade_error.to_string().contains("run action=rebuild"));
    global
        .execute(json!({"action":"rebuild","scope":"global"}))
        .await
        .expect("upgrade rebuild");

    let cold = global
        .execute(json!({"action":"dream_read","scope":"global"}))
        .await
        .expect("cold Dream read");
    assert_eq!(cold["found"], false);
    assert_eq!(cold["stale"], false);
    assert_eq!(cold["content"], Value::Null);
    let first_generation = cold["current_generation"]
        .as_str()
        .expect("first generation")
        .to_string();
    assert_eq!(first_generation.len(), 64);

    let published = global
        .execute(json!({
            "action":"dream_publish",
            "scope":"global",
            "source_generation":first_generation,
            "content":"## Current orientation\n\n- First stable signal."
        }))
        .await
        .expect("publish Dream");
    assert_eq!(published["published"], true);
    assert_eq!(published["stale"], false);
    assert_eq!(
        global_store
            .count_scope_memories(MemoryScope::Global, None)
            .await
            .expect("canonical count"),
        1,
        "Dream is not a canonical topic"
    );
    assert_eq!(
        global_store
            .read_lexical_index(MemoryScope::Global, None)
            .await
            .expect("lexical index")
            .expect("lexical index exists")
            .items
            .len(),
        1,
        "Dream is absent from lexical recall"
    );

    let fresh = global
        .execute(json!({"action":"dream_read","scope":"global"}))
        .await
        .expect("fresh Dream read");
    assert_eq!(fresh["found"], true);
    assert_eq!(fresh["stale"], false);
    assert_eq!(
        fresh["content"],
        "## Current orientation\n\n- First stable signal."
    );
    let prior_bytes = std::fs::read(
        global_store
            .resolver()
            .dream_path(MemoryScope::Global, None),
    )
    .expect("Dream bytes");

    write_global(
        &global,
        "Dream source two",
        "A later canonical mutation changes the source generation.",
    )
    .await;
    let stale = global
        .execute(json!({"action":"dream_read","scope":"global"}))
        .await
        .expect("stale Dream read");
    assert_eq!(stale["stale"], true);
    assert_ne!(stale["current_generation"], stale["source_generation"]);
    let current_generation = stale["current_generation"]
        .as_str()
        .expect("current generation")
        .to_string();
    let rejected = global
        .execute(json!({
            "action":"dream_publish",
            "scope":"global",
            "source_generation":first_generation,
            "content":"This synthesis raced with canonical memory."
        }))
        .await
        .expect_err("stale publish rejected");
    assert!(
        rejected
            .to_string()
            .contains("stale Dream source_generation")
    );
    assert_eq!(
        std::fs::read(
            global_store
                .resolver()
                .dream_path(MemoryScope::Global, None)
        )
        .expect("prior Dream remains"),
        prior_bytes
    );

    global
        .execute(json!({"action":"rebuild","scope":"global"}))
        .await
        .expect("rebuild");
    assert_eq!(
        global_store
            .current_scope_generation(MemoryScope::Global, None)
            .await
            .expect("generation after rebuild"),
        current_generation,
        "rebuild does not invent a new canonical generation"
    );
    assert_eq!(
        std::fs::read(
            global_store
                .resolver()
                .dream_path(MemoryScope::Global, None)
        )
        .expect("rebuild preserves Dream"),
        prior_bytes
    );
    let rejected = global
        .execute(json!({
            "action":"dream_publish",
            "scope":"global",
            "source_generation":&current_generation,
            "content":"   "
        }))
        .await
        .expect_err("empty Dream rejected");
    assert!(rejected.to_string().contains("1..=12000"));
    let oversized = "界".repeat(12_001);
    let rejected = global
        .execute(json!({
            "action":"dream_publish",
            "scope":"global",
            "source_generation":&current_generation,
            "content":oversized
        }))
        .await
        .expect_err("oversized Dream rejected");
    assert!(rejected.to_string().contains("1..=12000"));
    assert_eq!(
        std::fs::read(
            global_store
                .resolver()
                .dream_path(MemoryScope::Global, None)
        )
        .expect("failed publish preserves Dream"),
        prior_bytes
    );

    let context_a = MemoryExecutionContext::new("dream_project_a")
        .expect("context")
        .with_project_id("project_a")
        .expect("project A");
    let context_b = MemoryExecutionContext::new("dream_project_b")
        .expect("context")
        .with_project_id("project_b")
        .expect("project B");
    let project_a = MemoryServer::new(MemoryStore::new(directory.path()), context_a);
    let project_b = MemoryServer::new(MemoryStore::new(directory.path()), context_b);
    let empty_project = project_a
        .execute(json!({"action":"dream_read","scope":"project"}))
        .await
        .expect("fresh empty Project Dream is a cold state");
    assert_eq!(empty_project["found"], false);
    assert_eq!(empty_project["stale"], false);
    assert_eq!(
        empty_project["current_generation"]
            .as_str()
            .expect("empty Project generation")
            .len(),
        64
    );
    let write_a = project_a
        .execute(json!({
            "action":"write",
            "scope":"project",
            "type":"project",
            "title":"Project A Dream source",
            "content":"Only Project A may orient from this fact."
        }))
        .await
        .expect("write Project A");
    assert_eq!(write_a["memory"]["scope"], "project");
    project_b
        .execute(json!({
            "action":"write",
            "scope":"project",
            "type":"project",
            "title":"Project B Dream source",
            "content":"Only Project B may orient from this fact."
        }))
        .await
        .expect("write Project B");
    let cold_a = project_a
        .execute(json!({"action":"dream_read","scope":"project"}))
        .await
        .expect("Project A cold Dream");
    project_a
        .execute(json!({
            "action":"dream_publish",
            "scope":"project",
            "source_generation":cold_a["current_generation"],
            "content":"## Project A orientation\n\n- A only."
        }))
        .await
        .expect("publish Project A Dream");
    let read_a = project_a
        .execute(json!({"action":"dream_read","scope":"project"}))
        .await
        .expect("read Project A Dream");
    let read_b = project_b
        .execute(json!({"action":"dream_read","scope":"project"}))
        .await
        .expect("read Project B Dream");
    assert_eq!(read_a["project_key"], "project_a");
    assert_eq!(read_a["found"], true);
    assert_eq!(read_b["project_key"], "project_b");
    assert_eq!(read_b["found"], false);

    let unbound = MemoryServer::new(
        MemoryStore::new(directory.path()),
        MemoryExecutionContext::new("dream_unbound").expect("context"),
    );
    assert!(
        unbound
            .execute(json!({"action":"dream_read","scope":"project"}))
            .await
            .expect_err("Project Dream requires host authority")
            .to_string()
            .contains("project scope requires a project_id")
    );
    assert!(
        global
            .execute(json!({"action":"dream_read","scope":"session"}))
            .await
            .expect_err("Session Dream unsupported")
            .to_string()
            .contains("supports durable scopes only")
    );
}

#[tokio::test]
async fn split_and_consolidate_metadata_are_visible_through_get_and_query() {
    let fixture = Fixture::global("session_write_family_metadata");
    let server = &fixture.server;

    let split_source = write_global(server, "Split metadata source", "source body").await;
    let split = server
        .execute(json!({
            "action": "split",
            "id": split_source,
            "pieces": [{
                "title": "Split metadata target",
                "content": "Atomic split fact without its alias in prose.",
                "keywords": ["split-metadata-only-alias"],
                "entities": ["Split Entity"],
                "tags": ["拆分 标签"]
            }]
        }))
        .await
        .expect("split");
    let split_id = split["data"]["new_ids"][0].as_str().expect("split id");
    let split_get = server
        .execute(json!({"action": "get", "id": split_id}))
        .await
        .expect("get split target");
    assert_eq!(
        split_get["memory"]["frontmatter"]["retrieval"]["keywords"][0],
        "split-metadata-only-alias"
    );
    assert_eq!(split_get["memory"]["retrieval_metadata_truncated"], false);
    let split_query = server
        .execute(json!({
            "action": "query",
            "scope": "global",
            "query": "split-metadata-only-alias"
        }))
        .await
        .expect("query split target");
    assert_eq!(split_query["data"]["items"][0]["id"], split_id);

    let source_a = write_global(server, "Consolidate metadata A", "source A").await;
    let source_b = write_global(server, "Consolidate metadata B", "source B").await;
    let consolidate = server
        .execute(json!({
            "action": "consolidate",
            "ids": [source_a, source_b],
            "title": "Consolidated metadata target",
            "content": "One canonical fact without its model alias in prose.",
            "type": "reference",
            "keywords": ["consolidated-metadata-only-alias"],
            "entities": ["Canonical Entity"],
            "tags": ["合并 标签"]
        }))
        .await
        .expect("consolidate");
    let consolidated_id = consolidate["data"]["new_id"]
        .as_str()
        .expect("consolidated id");
    let consolidated_get = server
        .execute(json!({"action": "get", "id": consolidated_id}))
        .await
        .expect("get consolidated target");
    assert_eq!(
        consolidated_get["memory"]["frontmatter"]["retrieval"]["keywords"][0],
        "consolidated-metadata-only-alias"
    );
    assert_eq!(
        consolidated_get["memory"]["retrieval_metadata_truncated"],
        false
    );
    let consolidated_query = server
        .execute(json!({
            "action": "query",
            "scope": "global",
            "query": "consolidated-metadata-only-alias"
        }))
        .await
        .expect("query consolidated target");
    assert_eq!(
        consolidated_query["data"]["items"][0]["id"],
        consolidated_id
    );
}

#[test]
fn generated_schema_has_complete_actions_value_domains_and_safe_ids() {
    let tool = memory_tool();
    assert_eq!(tool.name, MEMORY_TOOL_NAME);
    let schema = tool.schema_as_json_value();
    let branches = schema["oneOf"].as_array().expect("tagged enum oneOf");
    assert_eq!(branches.len(), 19);

    let contracts: [(&str, &[&str], &[&str]); 19] = [
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
            "dream_read",
            &["action", "project_key", "scope"],
            &["action", "scope"],
        ),
        (
            "dream_publish",
            &[
                "action",
                "content",
                "project_key",
                "scope",
                "source_generation",
            ],
            &["action", "content", "scope", "source_generation"],
        ),
        (
            "find_duplicates",
            &[
                "action",
                "content",
                "entities",
                "keywords",
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
                "entities",
                "granularity",
                "keywords",
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
                "entities",
                "id",
                "keywords",
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
                "entities",
                "ids",
                "keywords",
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
        "dream_read",
        "dream_publish",
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
    let dream_publish = branch(&schema, "dream_publish");
    assert_eq!(
        find_key(
            &dream_publish["properties"]["source_generation"],
            "minLength"
        )
        .and_then(Value::as_u64),
        Some(64)
    );
    assert_eq!(
        find_key(
            &dream_publish["properties"]["source_generation"],
            "maxLength"
        )
        .and_then(Value::as_u64),
        Some(64)
    );
    assert_eq!(
        find_key(&dream_publish["properties"]["content"], "maxLength").and_then(Value::as_u64),
        Some(12_000)
    );
    for action in ["query", "get", "dream_read", "dream_publish", "write"] {
        assert!(
            branch(&schema, action)["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty()),
            "{action} branch must explain its model-facing contract"
        );
    }
    for action in ["write", "merge", "find_duplicates", "consolidate"] {
        for (field, max_items) in [("keywords", 32_u64), ("entities", 16)] {
            let field_schema = &branch(&schema, action)["properties"][field];
            assert_eq!(
                find_key(field_schema, "maxItems").and_then(Value::as_u64),
                Some(max_items),
                "{action}.{field} maxItems"
            );
            assert_eq!(
                find_key(field_schema, "maxLength").and_then(Value::as_u64),
                Some(96),
                "{action}.{field} item maxLength"
            );
        }
    }
    let split_piece = referenced_schema(
        &schema,
        &branch(&schema, "split")["properties"]["pieces"]["items"],
    );
    for field in ["keywords", "entities", "tags"] {
        assert!(
            split_piece["properties"].get(field).is_some(),
            "split piece must expose {field}"
        );
    }

    let description = tool.description.as_deref().expect("tool description");
    assert_eq!(description, MEMORY_TOOL_DESCRIPTION);
    for guidance in [
        "short set of discriminative keywords",
        "compact top-3 hits",
        "get(id)",
        "retrieval_metadata_truncated",
        "run rebuild for the same authorized scope",
        "Before write, query",
        "keywords/entities/tags",
        "omitted/blank query",
        "dream_read",
        "current_generation",
        "dream_publish",
        "rejects stale synthesis",
        "never makes the model call",
    ] {
        assert!(
            description.contains(guidance),
            "tool description must be self-sufficient: {guidance}"
        );
    }
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
            "content":"isolated content",
            "keywords":["quasarvx927"]
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
    project_b
        .execute(json!({
            "action":"write",
            "scope":"project",
            "type":"project",
            "title":"Project B seed",
            "content":"creates B's independent lexical index"
        }))
        .await
        .expect("project B seed");
    let a_recall = project_a
        .execute(json!({"action":"query","scope":"project","query":"quasarvx927"}))
        .await
        .expect("project A indexed recall");
    let b_recall = project_b
        .execute(json!({"action":"query","scope":"project","query":"quasarvx927"}))
        .await
        .expect("project B isolated indexed recall");
    assert_eq!(a_recall["data"]["matched_count"], 1);
    assert_eq!(b_recall["data"]["matched_count"], 0);
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
        "retrieval_metadata_truncated",
        "inspect",
        "session_append",
        "write",
        "dream_read",
        "dream_publish",
        "lower-trust",
        "current_generation",
        "rejects a stale generation",
        "never chooses a model",
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
        "same topic with `session_read`",
        "`session_list_topics` only when the topic itself is uncertain",
        "`inspect` the known affected scope",
        "do not guess the scope",
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
    assert!(
        !instructions.contains("run `inspect` first"),
        "Session recovery must not be routed through durable-only inspect"
    );
    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, MEMORY_TOOL_NAME);
    assert_eq!(
        tools[0].description.as_deref(),
        Some(MEMORY_TOOL_DESCRIPTION)
    );
    assert!(
        client
            .call_tool(CallToolRequestParams::new("memory_search"))
            .await
            .is_err(),
        "the removed multi-tool surface must not remain as an alias"
    );

    let cold_dream = call_memory(&client, json!({"action":"dream_read","scope":"global"})).await;
    assert_eq!(cold_dream["found"], false);
    assert_eq!(cold_dream["stale"], false);
    assert_eq!(
        cold_dream["current_generation"]
            .as_str()
            .expect("empty Global generation")
            .len(),
        64
    );

    let write = call_memory(
        &client,
        json!({
            "action": "write",
            "scope": "global",
            "type": "project",
            "title": "Protocol persistence",
            "content": "written through the MCP transport",
            "keywords": ["朱雀别名", "transport-alias"],
            "entities": ["ＡＰＩ 网关"],
            "tags": ["MCP 认证"]
        }),
    )
    .await;
    let id = write["memory"]["id"].as_str().expect("id").to_string();
    let query = call_memory(
        &client,
        json!({"action":"query","scope":"global","query":"朱雀别名"}),
    )
    .await;
    assert_eq!(query["data"]["items"][0]["id"], id);
    let hit = query["data"]["items"][0]
        .as_object()
        .expect("compact query hit");
    for forbidden in ["body", "path", "frontmatter", "keywords", "entities"] {
        assert!(!hit.contains_key(forbidden), "query leaked {forbidden}");
    }
    let get = call_memory(&client, json!({"action":"get","id":id})).await;
    assert_eq!(get["memory"]["body"], "written through the MCP transport");
    assert_eq!(get["memory"]["body_truncated"], false);
    assert_eq!(get["memory"]["retrieval_metadata_truncated"], false);
    assert_eq!(
        get["memory"]["frontmatter"]["retrieval"]["keywords"][0],
        "朱雀别名"
    );
    assert!(
        get["memory"]["frontmatter"]["retrieval"]["entities"]
            .as_array()
            .expect("entities")
            .contains(&json!("API 网关"))
    );
    assert!(
        get["memory"]["frontmatter"]["tags"]
            .as_array()
            .expect("tags")
            .contains(&json!("mcp-认证"))
    );
    assert!(
        !serde_json::to_string(&get)
            .expect("serialize get")
            .contains("embedding_ready")
    );

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

#[tokio::test]
async fn direct_write_enforces_metadata_bounds_and_get_bounds_legacy_metadata() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = MemoryStore::new(directory.path());
    let server = MemoryServer::new(
        store.clone(),
        MemoryExecutionContext::new("metadata_bounds").expect("context"),
    );
    let oversized_query = "界".repeat(513);
    let query_error = server
        .execute(json!({
            "action": "query",
            "scope": "global",
            "query": oversized_query
        }))
        .await
        .expect_err("runtime query bound must not depend on JSON Schema enforcement");
    assert!(query_error.to_string().contains("exceeds 512 characters"));

    let keywords = (0..40)
        .map(|index| {
            if index == 0 {
                "Ｋ".repeat(MAX_RETRIEVAL_TERM_CHARS + 24)
            } else {
                format!("explicit-keyword-{index:02}")
            }
        })
        .collect::<Vec<_>>();
    let entities = (0..20)
        .map(|index| {
            if index == 0 {
                "Ｅ".repeat(MAX_RETRIEVAL_TERM_CHARS + 24)
            } else {
                format!("explicit-entity-{index:02}")
            }
        })
        .collect::<Vec<_>>();
    let tags = (0..40)
        .map(|index| {
            if index == 0 {
                "标签".repeat(MAX_MEMORY_TAG_CHARS)
            } else {
                format!("explicit-tag-{index:02}")
            }
        })
        .collect::<Vec<_>>();
    let write = server
        .execute(json!({
            "action": "write",
            "scope": "global",
            "type": "reference",
            "title": "Runtime metadata bounds",
            "content": "The MCP handler delegates to store-enforced metadata limits.",
            "keywords": keywords,
            "entities": entities,
            "tags": tags,
        }))
        .await
        .expect("direct MCP execution accepts and bounds metadata");
    let id = write["memory"]["id"].as_str().expect("id");
    let mut doc = store
        .get_memory(id, None)
        .await
        .expect("read stored memory")
        .expect("stored memory exists");
    assert!(
        doc.frontmatter
            .retrieval
            .keywords
            .contains(&"explicit-keyword-31".to_string())
    );
    assert!(
        !doc.frontmatter
            .retrieval
            .keywords
            .contains(&"explicit-keyword-32".to_string())
    );
    assert!(
        doc.frontmatter
            .retrieval
            .entities
            .contains(&"explicit-entity-15".to_string())
    );
    assert!(
        !doc.frontmatter
            .retrieval
            .entities
            .contains(&"explicit-entity-16".to_string())
    );
    assert_eq!(doc.frontmatter.tags.len(), MAX_MEMORY_TAGS);
    assert!(
        doc.frontmatter
            .tags
            .iter()
            .all(|value| value.chars().count() <= MAX_MEMORY_TAG_CHARS)
    );
    assert!(
        doc.frontmatter
            .retrieval
            .keywords
            .iter()
            .all(|value| value.chars().count() <= MAX_RETRIEVAL_TERM_CHARS)
    );
    assert!(
        doc.frontmatter
            .retrieval
            .entities
            .iter()
            .all(|value| value.chars().count() <= MAX_RETRIEVAL_TERM_CHARS)
    );

    // Simulate canonical bytes copied from a legacy store: typed `get` must
    // remain bounded without requiring an eager migration or rewriting the file.
    doc.frontmatter.tags = (0..40)
        .map(|index| format!("legacy-tag-{index:03}"))
        .collect();
    doc.frontmatter.retrieval.keywords = (0..140)
        .map(|index| {
            if index == 0 {
                "旧".repeat(MAX_RETRIEVAL_TERM_CHARS + 24)
            } else {
                format!("legacy-keyword-{index:03}")
            }
        })
        .collect();
    doc.frontmatter.retrieval.entities = (0..70)
        .map(|index| format!("legacy-entity-{index:03}"))
        .collect();
    let rendered = render_markdown_document(&doc.frontmatter, &doc.body).expect("render legacy");
    tokio::fs::write(&doc.path, rendered)
        .await
        .expect("replace canonical bytes");

    let get = server
        .execute(json!({"action": "get", "id": id}))
        .await
        .expect("bounded legacy get");
    assert_eq!(get["memory"]["retrieval_metadata_truncated"], true);
    assert_eq!(
        get["memory"]["frontmatter"]["tags"]
            .as_array()
            .expect("tags")
            .len(),
        MAX_MEMORY_TAGS
    );
    assert_eq!(
        get["memory"]["frontmatter"]["retrieval"]["keywords"]
            .as_array()
            .expect("keywords")
            .len(),
        MAX_MEMORY_KEYWORDS
    );
    assert_eq!(
        get["memory"]["frontmatter"]["retrieval"]["entities"]
            .as_array()
            .expect("entities")
            .len(),
        MAX_MEMORY_ENTITIES
    );
    assert!(
        get["memory"]["frontmatter"]["retrieval"]["keywords"]
            .as_array()
            .expect("keywords")
            .iter()
            .all(|value| value.as_str().expect("keyword").chars().count()
                <= MAX_RETRIEVAL_TERM_CHARS)
    );
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
