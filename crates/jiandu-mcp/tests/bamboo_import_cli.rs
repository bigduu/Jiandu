use std::process::Command;

use jiandu_memory::ProjectId;
use jiandu_memory::memory_store::{
    DurableMemoryType, MemoryQueryOptions, MemoryScope, MemoryStore,
};
use tempfile::tempdir;

#[tokio::test]
async fn existing_jiandu_binary_imports_once_and_a_fresh_reader_uses_the_store() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    let project_id = ProjectId::parse("cli-project").expect("Project id");

    let source_store = MemoryStore::new(&source);
    let global = source_store
        .write_memory(
            MemoryScope::Global,
            None,
            DurableMemoryType::Reference,
            "CLI Global source",
            "A normal reader finds the cli-global-needle after import.",
            &["cli".to_string()],
            Some("source-session"),
            "test",
            false,
            None,
        )
        .await
        .expect("write Global source");
    let project = source_store
        .for_project(&project_id)
        .write_memory(
            MemoryScope::Project,
            Some(project_id.as_str()),
            DurableMemoryType::Project,
            "CLI Project source",
            "A normal reader finds the cli-project-needle after import.",
            &["cli".to_string()],
            Some("source-session"),
            "test",
            false,
            None,
        )
        .await
        .expect("write Project source");

    let output = Command::new(env!("CARGO_BIN_EXE_jiandu"))
        .arg("import-bamboo")
        .arg("--source-data-dir")
        .arg(&source)
        .arg("--data-dir")
        .arg(&destination)
        .output()
        .expect("run one-shot import");
    assert!(
        output.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON import report");
    assert_eq!(report["scanned"], 2);
    assert_eq!(report["imported"], 2);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["rebuilt_scopes"], 2);
    assert_eq!(
        report["content_identity_sha256"]
            .as_str()
            .expect("content identity")
            .len(),
        64
    );

    // This integration-test process is independent from the completed importer
    // process and opens the published root exactly like a native Jiandu host.
    let reader = MemoryStore::new(&destination);
    assert_eq!(
        reader
            .get_memory(&global.frontmatter.id, None)
            .await
            .expect("read imported Global")
            .expect("Global exists")
            .body,
        global.body
    );
    let project_reader = reader.for_project(&project_id);
    assert_eq!(
        project_reader
            .get_memory(&project.frontmatter.id, Some(project_id.as_str()))
            .await
            .expect("read imported Project")
            .expect("Project exists")
            .body,
        project.body
    );
    let query = project_reader
        .query_scope(
            MemoryScope::Project,
            Some(project_id.as_str()),
            Some("cli project needle"),
            None,
            None,
            None,
            &MemoryQueryOptions {
                limit: Some(5),
                max_chars: Some(1_000),
                cursor: None,
                include_related: false,
            },
        )
        .await
        .expect("query imported Project");
    assert_eq!(query.matched_count, 1);

    let second = Command::new(env!("CARGO_BIN_EXE_jiandu"))
        .arg("import-bamboo")
        .arg("--source-data-dir")
        .arg(&source)
        .arg("--data-dir")
        .arg(&destination)
        .output()
        .expect("run rejected second import");
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("absent or empty"),
        "second import should explain the destination gate: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}
