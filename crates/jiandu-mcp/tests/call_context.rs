use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use jiandu_mcp::MEMORY_CONTEXT_META_KEY;
use serde_json::{Value, json};

/// Exercise the actual stdio executable, including rmcp's wire metadata path.
/// A bounded reader makes handshake/dispatch failures fail instead of hanging.
struct StdioClient {
    child: Child,
    input: ChildStdin,
    responses: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    pending: HashMap<u64, Value>,
    next_id: u64,
}

impl StdioClient {
    fn start(data_dir: &Path, defaults: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_jiandu"))
            .arg("--data-dir")
            .arg(data_dir)
            .args(defaults)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start stdio binary");
        let input = child.stdin.take().expect("stdin");
        let output = child.stdout.take().expect("stdout");
        let (sender, responses) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.expect("read stdio response");
                let value = serde_json::from_str(&line).expect("stdout contains only MCP JSON");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input,
            responses,
            reader: Some(reader),
            pending: HashMap::new(),
            next_id: 1,
        };
        let id = client.submit(
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"jiandu-context-test","version":"1"}
            }),
        );
        let initialized = client.receive(id);
        assert_eq!(initialized["serverInfo"]["name"], "jiandu");
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        client
    }

    fn send(&mut self, packet: Value) {
        writeln!(self.input, "{packet}").expect("send request");
        self.input.flush().expect("flush request");
    }

    fn submit(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        id
    }

    fn receive(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let response = loop {
            if let Some(response) = self.pending.remove(&id) {
                break response;
            }
            let response = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("stdio server responds before deadline");
            if let Some(response_id) = response["id"].as_u64() {
                self.pending.insert(response_id, response);
            }
        };
        assert!(
            response.get("error").is_none(),
            "protocol error: {response}"
        );
        response["result"].clone()
    }

    fn submit_memory(&mut self, arguments: Value, metadata: Option<Value>) -> u64 {
        let mut params = json!({"name":"memory","arguments":arguments});
        if let Some(metadata) = metadata {
            params["_meta"] = metadata;
        }
        self.submit("tools/call", params)
    }

    fn call(&mut self, arguments: Value, context: Option<Value>) -> Value {
        let id = self.submit_memory(arguments, context.map(host_metadata));
        self.receive(id)
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn host_metadata(context: Value) -> Value {
    json!({MEMORY_CONTEXT_META_KEY:context})
}

fn success(result: Value) -> Value {
    assert_eq!(result["isError"], false, "tool failed: {result}");
    result["structuredContent"].clone()
}

fn error(result: Value, expected: &str) {
    assert_eq!(result["isError"], true, "tool should fail: {result}");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("error text")
            .contains(expected),
        "expected {expected}: {result}"
    );
}

fn write(scope: &str, title: &str, needle: &str) -> Value {
    json!({
        "action":"write","scope":scope,"type":"reference",
        "title":title,"content":needle,"keywords":[needle],
        "options":{"allow_merge_if_similar":false}
    })
}

#[test]
fn stdio_starts_with_only_data_dir_and_global_memory_needs_no_identities() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut client = StdioClient::start(directory.path(), &[]);
    let written = success(client.call(write("global", "Global fact", "globalneedlevx"), None));
    let id = &written["memory"]["id"];
    let recalled = success(client.call(
        json!({"action":"query","scope":"global","query":"globalneedlevx"}),
        None,
    ));
    assert_eq!(recalled["data"]["items"][0]["id"], *id);
    let fetched = success(client.call(json!({"action":"get","id":id}), None));
    assert_eq!(fetched["memory"]["body"], "globalneedlevx");
    assert!(fetched["memory"]["frontmatter"]["created_by"]["id"].is_null());
    assert!(fetched["memory"]["frontmatter"].get("sources").is_none());

    for arguments in [
        json!({"action":"session_read"}),
        json!({"action":"session_append","content":"must fail"}),
        json!({"action":"session_replace","content":"must fail"}),
        json!({"action":"session_clear"}),
        json!({"action":"session_list_topics"}),
    ] {
        error(client.call(arguments, None), "require a session_id");
    }
    error(
        client.call(json!({"action":"query","scope":"project"}), None),
        "project scope requires a project_id",
    );
    let unaffected = success(client.call(json!({"action":"inspect","scope":"global"}), None));
    assert_eq!(unaffected["data"]["total_memories"], 1);
}

#[test]
fn one_stdio_connection_switches_projects_without_a_session_or_retained_authority() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut client = StdioClient::start(directory.path(), &[]);
    let a = json!({"project_id":"project-a"});
    let b = json!({"project_id":"project-b"});
    let written_a =
        success(client.call(write("project", "A fact", "onlyavxneedle"), Some(a.clone())));
    let written_b =
        success(client.call(write("project", "B fact", "onlybvxneedle"), Some(b.clone())));
    assert_eq!(written_a["memory"]["project_key"], "project-a");
    assert_eq!(written_b["memory"]["project_key"], "project-b");
    let id_a = &written_a["memory"]["id"];
    let get_a = success(client.call(json!({"action":"get","id":id_a}), Some(a.clone())));
    assert!(get_a["memory"]["frontmatter"]["created_by"]["id"].is_null());
    assert!(get_a["memory"]["frontmatter"].get("sources").is_none());

    for (context, count) in [(a.clone(), 1), (b.clone(), 0)] {
        let recalled = success(client.call(
            json!({"action":"query","scope":"project","query":"onlyavxneedle"}),
            Some(context),
        ));
        assert_eq!(recalled["data"]["matched_count"], count);
    }
    error(
        client.call(json!({"action":"get","id":id_a}), Some(b)),
        "memory not found",
    );
    error(
        client.call(
            json!({"action":"inspect","scope":"project","project_key":"project-b"}),
            Some(a),
        ),
        "cannot override",
    );
    error(
        client.call(json!({"action":"query","scope":"project"}), None),
        "project scope requires a project_id",
    );
    error(
        client.call(
            json!({"action":"get","id":id_a,"project_key":"project-a"}),
            None,
        ),
        "cannot grant Project access",
    );
    let global = success(client.call(json!({"action":"inspect","scope":"global"}), None));
    assert_eq!(global["data"]["total_memories"], 0);
}

#[test]
fn concurrent_stdio_calls_keep_their_project_session_and_provenance_contexts() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut client = StdioClient::start(directory.path(), &[]);
    let a = json!({"project_id":"project-a","session_id":"session-a"});
    let b = json!({"project_id":"project-b","session_id":"session-b"});
    let first = client.submit_memory(
        write("project", "A fact", "avxconcurrent"),
        Some(host_metadata(a.clone())),
    );
    let second = client.submit_memory(
        write("project", "B fact", "bvxconcurrent"),
        Some(host_metadata(b.clone())),
    );
    for (id, context, project, session) in [
        (first, a, "project-a", "session-a"),
        (second, b, "project-b", "session-b"),
    ] {
        let written = success(client.receive(id));
        assert_eq!(written["memory"]["project_key"], project);
        let fetched = success(client.call(
            json!({"action":"get","id":written["memory"]["id"]}),
            Some(context),
        ));
        assert_eq!(
            fetched["memory"]["frontmatter"]["created_by"]["id"],
            session
        );
        assert_eq!(
            fetched["memory"]["frontmatter"]["sources"][0]["id"],
            session
        );
    }

    let first = client.submit_memory(
        json!({"action":"session_append","content":"note A"}),
        Some(host_metadata(json!({"session_id":"session-a"}))),
    );
    let second = client.submit_memory(
        json!({"action":"session_append","content":"note B"}),
        Some(host_metadata(json!({"session_id":"session-b"}))),
    );
    assert_eq!(success(client.receive(first))["session_id"], "session-a");
    assert_eq!(success(client.receive(second))["session_id"], "session-b");
    for (session, note) in [("session-a", "note A"), ("session-b", "note B")] {
        let read = success(client.call(
            json!({"action":"session_read"}),
            Some(json!({"session_id":session})),
        ));
        assert_eq!(read["content"], note);
    }
    error(
        client.call(json!({"action":"session_read"}), None),
        "require a session_id",
    );
}

#[test]
fn per_call_context_replaces_all_startup_defaults_and_never_falls_back_when_invalid() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut client = StdioClient::start(
        directory.path(),
        &[
            "--session-id",
            "startup-session",
            "--project-id",
            "startup-project",
        ],
    );
    let default_note = success(client.call(
        json!({"action":"session_append","content":"default note"}),
        None,
    ));
    assert_eq!(default_note["session_id"], "startup-session");
    let default_project =
        success(client.call(write("project", "Default fact", "defaultneedle"), None));
    assert_eq!(default_project["memory"]["project_key"], "startup-project");

    let project_only = json!({"project_id":"call-project"});
    let written = success(client.call(
        write("project", "Per-call fact", "callneedle"),
        Some(project_only.clone()),
    ));
    let fetched = success(client.call(
        json!({"action":"get","id":written["memory"]["id"]}),
        Some(project_only.clone()),
    ));
    assert!(fetched["memory"]["frontmatter"]["created_by"]["id"].is_null());
    assert!(fetched["memory"]["frontmatter"].get("sources").is_none());
    error(
        client.call(json!({"action":"session_read"}), Some(project_only)),
        "require a session_id",
    );
    error(
        client.call(
            json!({"action":"inspect","scope":"project"}),
            Some(json!({"session_id":"call-session"})),
        ),
        "project scope requires a project_id",
    );
    for empty in [json!({}), json!({"session_id":null,"project_id":null})] {
        error(
            client.call(json!({"action":"session_read"}), Some(empty.clone())),
            "require a session_id",
        );
        error(
            client.call(
                json!({"action":"inspect","scope":"project"}),
                Some(empty.clone()),
            ),
            "project scope requires a project_id",
        );
        success(client.call(json!({"action":"inspect","scope":"global"}), Some(empty)));
    }

    for malformed in [
        json!(null),
        json!("startup-project"),
        json!([]),
        json!({"project_id":"../escape"}),
        json!({"project_id":""}),
        json!({"session_id":".."}),
        json!({"session_id":""}),
        json!({"session_id":5}),
        json!({"project_id":true}),
        json!({"projectId":"typo"}),
    ] {
        let result = client.call(
            write("global", "Must not commit", "invalidneedle"),
            Some(malformed),
        );
        assert_eq!(
            result["isError"], true,
            "malformed context must not fall back: {result}"
        );
    }
    let global = success(client.call(json!({"action":"inspect","scope":"global"}), None));
    assert_eq!(global["data"]["total_memories"], 0);
    let original = success(client.call(json!({"action":"session_read"}), None));
    assert_eq!(original["session_id"], "startup-session");
    assert_eq!(original["content"], "default note");
    let original_project = success(client.call(
        json!({"action":"query","scope":"project","query":"defaultneedle"}),
        None,
    ));
    assert_eq!(original_project["data"]["matched_count"], 1);
    assert_eq!(
        original_project["data"]["items"][0]["id"],
        default_project["memory"]["id"]
    );
}

#[test]
fn model_arguments_cannot_supply_host_identity_and_unrelated_metadata_is_ignored() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut client = StdioClient::start(directory.path(), &[]);
    for arguments in [
        json!({"action":"session_append","content":"forged","session_id":"forged"}),
        json!({"action":"session_list_topics","session_id":"forged"}),
        json!({"action":"inspect","scope":"project","project_id":"forged"}),
        json!({"action":"inspect","scope":"project","_meta":host_metadata(json!({"project_id":"forged"}))}),
    ] {
        error(client.call(arguments, None), "unknown field");
    }
    error(
        client.call(
            json!({"action":"inspect","scope":"project","project_key":"forged"}),
            None,
        ),
        "cannot grant Project access",
    );
    let id = client.submit_memory(
        json!({"action":"inspect","scope":"global"}),
        Some(json!({"progressToken":"read", "com.example/trace":"opaque"})),
    );
    success(client.receive(id));
}
