use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use jiandu_memory::{
    ProjectId,
    memory_store::{
        DurableMemoryDocument, DurableMemoryStatus, DurableMemoryType, MemoryScope, MemoryStore,
    },
};
use serde_json::Value;
use tempfile::tempdir;

struct Console {
    child: Child,
    address: SocketAddr,
}
impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Reply {
    status: u16,
    headers: String,
    body: String,
}
impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON response")
    }
}
impl Console {
    fn start(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_jiandu"))
            .args(["ui", "--data-dir"])
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut console = Self {
            child,
            address: "127.0.0.1:0".parse().unwrap(),
        };
        let stdout = console.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded readiness")
            .unwrap();
        console.address = line
            .trim()
            .strip_prefix("Jiandu console: http://")
            .unwrap()
            .trim_end_matches('/')
            .parse()
            .unwrap();
        assert!(console.address.ip().is_loopback());
        assert_ne!(console.address.port(), 0);
        console
    }
    fn request(&self, method: &str, path: &str, headers: &str) -> Reply {
        self.raw(format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nX-Jiandu-Console: 1\r\nConnection: close\r\n{headers}\r\n", self.address))
    }
    fn raw(&self, request: String) -> Reply {
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        Reply {
            status,
            headers: headers.to_ascii_lowercase(),
            body: body.to_string(),
        }
    }
    fn get(&self, path: &str) -> Value {
        let reply = self.request("GET", path, "");
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        reply.json()
    }
}

fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    if root.exists() {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(bytes(&path));
            } else {
                out.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    out
}

async fn write_memory(
    store: &MemoryStore,
    key: Option<&str>,
    title: &str,
) -> DurableMemoryDocument {
    store
        .write_memory(
            if key.is_some() {
                MemoryScope::Project
            } else {
                MemoryScope::Global
            },
            key,
            DurableMemoryType::Reference,
            title,
            "项目知识与 Jiandu console search",
            &["console".into()],
            Some("fixture-session"),
            "console-fixture",
            false,
            None,
        )
        .await
        .unwrap()
}

#[test]
fn cold_console_serves_embedded_assets_without_initializing_the_store() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("uninitialized");
    let console = Console::start(&root);
    for (path, mime) in [
        ("/", "text/html"),
        ("/console.css", "text/css"),
        ("/console.js", "text/javascript"),
    ] {
        let reply = console.request("GET", path, "");
        assert_eq!(reply.status, 200);
        assert!(reply.headers.contains(mime));
        assert!(reply.headers.contains("cache-control: no-store"));
        assert!(reply.headers.contains("frame-ancestors 'none'"));
    }
    let catalog = console.get("/api/catalog");
    assert_eq!(catalog["projects"], serde_json::json!([]));
    assert_eq!(catalog["sessions"], serde_json::json!([]));
    assert_eq!(catalog["read_only"], true);
    assert_eq!(
        console.get("/api/memories?scope=global&q=search")["matched_count"],
        0
    );
    let status = console.get("/api/status?scope=global");
    assert_eq!(status["index"]["state"], "empty");
    assert!(status["dream"]["snapshot"].is_null());
    assert!(
        !root.exists(),
        "no root, lock, index or state is created by browsing"
    );
}

#[tokio::test]
async fn one_console_browses_explicit_projects_and_sessions_without_mutating_bytes() {
    let directory = tempdir().unwrap();
    let store = MemoryStore::new(directory.path());
    let alpha = store.for_project(&ProjectId::parse("alpha").unwrap());
    let beta = store.for_project(&ProjectId::parse("beta").unwrap());
    let global = write_memory(&store, None, "Global reference").await;
    let a = write_memory(&alpha, Some("alpha"), "项目甲 Jiandu console").await;
    let b = write_memory(&beta, Some("beta"), "Project beta reference").await;
    write_memory(&store, Some("legacy"), "Legacy excluded from the console").await;
    store
        .write_session_topic("session-a", "default", "Alpha continuity")
        .await
        .unwrap();
    store
        .write_session_topic("session-b", "default", "Beta continuity")
        .await
        .unwrap();
    let generation = alpha
        .current_scope_generation(MemoryScope::Project, Some("alpha"))
        .await
        .unwrap();
    alpha
        .publish_dream_snapshot(
            MemoryScope::Project,
            Some("alpha"),
            &generation,
            "Project alpha orientation",
        )
        .await
        .unwrap();
    let before = bytes(directory.path());
    let console = Console::start(directory.path());
    let catalog = console.get("/api/catalog");
    assert_eq!(catalog["projects"], serde_json::json!(["alpha", "beta"]));
    assert_eq!(
        catalog["sessions"],
        serde_json::json!(["session-a", "session-b"])
    );
    for (scope, key, document) in [
        ("global", "", &global),
        ("project", "&project_id=alpha", &a),
        ("project", "&project_id=beta", &b),
    ] {
        let query = console.get(&format!("/api/memories?scope={scope}{key}&q=Jiandu"));
        assert_eq!(query["items"][0]["id"], document.frontmatter.id);
        let detail = console.get(&format!(
            "/api/memory?scope={scope}{key}&id={}",
            document.frontmatter.id
        ));
        assert_eq!(detail["body"], document.body);
        assert_eq!(detail["frontmatter"]["created_by"]["id"], "fixture-session");
    }
    let chinese = console.get("/api/memories?scope=project&project_id=alpha&q=%E9%A1%B9%E7%9B%AE");
    assert_eq!(chinese["items"][0]["id"], a.frontmatter.id);
    assert_eq!(
        console
            .request(
                "GET",
                &format!(
                    "/api/memory?scope=project&project_id=beta&id={}",
                    a.frontmatter.id
                ),
                ""
            )
            .status,
        404
    );
    assert_eq!(
        console
            .request(
                "GET",
                &format!(
                    "/api/memory?scope=project&project_id=alpha&id={}",
                    global.frontmatter.id
                ),
                ""
            )
            .status,
        404
    );
    assert_eq!(
        console
            .request(
                "GET",
                &format!("/api/memory?scope=global&id={}", b.frontmatter.id),
                ""
            )
            .status,
        404
    );
    assert_eq!(
        console.get("/api/session?session_id=session-a&q=Alpha")["topics"][0]["topic"],
        "default"
    );
    assert_eq!(
        console.get("/api/session?session_id=session-b&q=Alpha")["matched_count"],
        0
    );
    assert_eq!(
        console.get("/api/topic?session_id=session-b&topic=default")["content"],
        "Beta continuity"
    );
    let status = console.get("/api/status?scope=project&project_id=alpha");
    assert_eq!(status["index"]["state"], "ready");
    assert_eq!(status["dream"]["stale"], false);
    assert_eq!(
        status["dream"]["snapshot"]["content"],
        "Project alpha orientation"
    );
    assert_eq!(
        bytes(directory.path()),
        before,
        "including access logs and every derived artifact"
    );
}

#[tokio::test]
async fn console_pages_and_filters_the_shared_store_and_reports_stale_dreams() {
    let directory = tempdir().unwrap();
    let store = MemoryStore::new(directory.path());
    for number in 0..22 {
        let doc = write_memory(&store, None, &format!("Reference {number}")).await;
        if number < 2 {
            store
                .archive_memory(
                    &doc.frontmatter.id,
                    None,
                    DurableMemoryStatus::Archived,
                    None,
                )
                .await
                .unwrap();
        }
    }
    let generation = store
        .current_scope_generation(MemoryScope::Global, None)
        .await
        .unwrap();
    store
        .publish_dream_snapshot(
            MemoryScope::Global,
            None,
            &generation,
            "Earlier orientation",
        )
        .await
        .unwrap();
    write_memory(&store, None, "New reference makes Dream stale").await;
    for number in 0..25 {
        store
            .write_session_topic(
                "paged-session",
                &format!("topic_{number:02}"),
                "Session pagination fixture",
            )
            .await
            .unwrap();
    }
    let before = bytes(directory.path());
    let console = Console::start(directory.path());
    let first = console.get("/api/memories?scope=global");
    assert_eq!(first["items"].as_array().unwrap().len(), 20);
    let next = first["next_cursor"].as_str().unwrap();
    let last = console.get(&format!("/api/memories?scope=global&cursor={next}"));
    assert_eq!(last["items"].as_array().unwrap().len(), 3);
    assert!(last["next_cursor"].is_null());
    let archived = console.get("/api/memories?scope=global&status=archived&q=Reference");
    assert_eq!(archived["matched_count"], 2);
    assert!(
        archived["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "archived")
    );
    assert_eq!(
        console.get("/api/status?scope=global")["dream"]["stale"],
        true
    );
    let topics = console.get("/api/session?session_id=paged-session");
    assert_eq!(topics["topics"].as_array().unwrap().len(), 20);
    assert_eq!(topics["next_cursor"], "20");
    let last_topics = console.get("/api/session?session_id=paged-session&cursor=20");
    assert_eq!(last_topics["topics"].as_array().unwrap().len(), 5);
    assert!(last_topics["next_cursor"].is_null());
    assert_eq!(
        console
            .request("GET", "/api/topic?session_id=paged-session&topic=..", "")
            .status,
        400
    );
    assert_eq!(bytes(directory.path()), before);
}

#[tokio::test]
async fn missing_indexes_remain_browsable_and_invalid_or_foreign_requests_fail_closed() {
    let directory = tempdir().unwrap();
    let store = MemoryStore::new(directory.path());
    let doc = write_memory(&store, None, "Missing index reference").await;
    // Deliberately damage only an isolated fixture's derived index; no personal
    // data and no canonical topic is modified by this failure-state test.
    std::fs::remove_file(
        store
            .resolver()
            .indexes_dir(MemoryScope::Global, None)
            .join("lexical.json"),
    )
    .unwrap();
    let before = bytes(directory.path());
    let console = Console::start(directory.path());
    assert_eq!(
        console.get("/api/memories?scope=global")["items"][0]["id"],
        doc.frontmatter.id
    );
    assert_eq!(
        console.get("/api/status?scope=global")["index"]["state"],
        "missing"
    );
    assert_eq!(
        console
            .request("GET", "/api/memories?scope=global&q=reference", "")
            .status,
        404
    );
    for path in [
        "/api/memories?scope=project&project_id=..",
        "/api/memories?scope=global&project_id=alpha",
        "/api/memories?scope=session",
        "/api/memories?scope=global&data_dir=outside",
    ] {
        assert_eq!(console.request("GET", path, "").status, 400, "{path}");
    }
    assert_eq!(
        console
            .request(
                "GET",
                "/api/memories?scope=global",
                "Origin: https://foreign.example\r\n"
            )
            .status,
        403
    );
    assert_eq!(
        console
            .request("GET", "/api/catalog", "Sec-Fetch-Site: cross-site\r\n")
            .status,
        403
    );
    assert_eq!(console.raw("GET /api/catalog HTTP/1.1\r\nHost: foreign.example\r\nX-Jiandu-Console: 1\r\nConnection: close\r\n\r\n".into()).status, 403);
    assert_eq!(
        console
            .raw(format!(
                "GET /api/catalog HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                console.address
            ))
            .status,
        403
    );
    assert_eq!(
        console
            .request(
                "POST",
                "/api/memories?scope=global",
                "Content-Length: 0\r\n"
            )
            .status,
        405
    );
    assert_eq!(
        bytes(directory.path()),
        before,
        "no implicit rebuild on failures"
    );
    std::fs::write(
        store
            .resolver()
            .indexes_dir(MemoryScope::Global, None)
            .join("lexical.json"),
        "invalid fixture JSON",
    )
    .unwrap();
    std::fs::remove_file(
        store
            .resolver()
            .scope_generation_path(MemoryScope::Global, None),
    )
    .unwrap();
    let invalid_bytes = bytes(directory.path());
    let invalid_status = console.get("/api/status?scope=global");
    assert_eq!(invalid_status["index"]["state"], "invalid");
    assert!(
        invalid_status["dream_error"]
            .as_str()
            .unwrap()
            .contains("rebuild")
    );
    assert_eq!(
        console
            .request("GET", "/api/memories?scope=global&q=reference", "")
            .status,
        409
    );
    assert_eq!(
        console.get("/api/memories?scope=global")["matched_count"],
        1
    );
    assert_eq!(bytes(directory.path()), invalid_bytes);
}
