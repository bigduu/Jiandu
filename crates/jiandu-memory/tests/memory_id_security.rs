use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use jiandu_memory::ProjectId;
use jiandu_memory::memory_store::{
    CreatedBy, DurableMemoryFrontmatter, DurableMemoryRelations, DurableMemoryRetrieval,
    DurableMemoryStatus, DurableMemoryType, MAX_MEMORY_ID_LEN, MemoryScope, MemorySplitPiece,
    MemoryStore, parse_markdown_document, render_markdown_document, validate_memory_id,
};
use tempfile::tempdir;

fn fixture_frontmatter(
    id: &str,
    scope: MemoryScope,
    project_key: Option<&str>,
    title: &str,
) -> DurableMemoryFrontmatter {
    DurableMemoryFrontmatter {
        id: id.to_string(),
        title: title.to_string(),
        r#type: DurableMemoryType::Project,
        scope,
        project_key: project_key.map(ToString::to_string),
        granularity: None,
        status: DurableMemoryStatus::Active,
        freshness: Some("high".to_string()),
        confidence: Some("high".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        created_by: CreatedBy {
            kind: "security_test".to_string(),
            id: None,
            actor: Some("test".to_string()),
        },
        updated_by: CreatedBy {
            kind: "security_test".to_string(),
            id: None,
            actor: Some("test".to_string()),
        },
        sources: Vec::new(),
        relations: DurableMemoryRelations::default(),
        tags: vec!["security-test".to_string()],
        retrieval: DurableMemoryRetrieval {
            keywords: vec!["shared".to_string()],
            entities: Vec::new(),
            embedding_ready: true,
            last_accessed_at: None,
        },
    }
}

async fn write_fixture_at(
    path: &Path,
    frontmatter: &DurableMemoryFrontmatter,
    body: &str,
) -> Vec<u8> {
    let rendered = render_markdown_document(frontmatter, body).expect("render fixture");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("create fixture directory");
    }
    tokio::fs::write(path, rendered.as_bytes())
        .await
        .expect("write fixture");
    rendered.into_bytes()
}

async fn write_store_fixture(
    store: &MemoryStore,
    scope: MemoryScope,
    project_key: Option<&str>,
    id: &str,
    title: &str,
    body: &str,
) -> PathBuf {
    let path = store
        .resolver()
        .topic_path(scope, project_key, id)
        .expect("valid fixture id");
    let frontmatter = fixture_frontmatter(id, scope, project_key, title);
    write_fixture_at(&path, &frontmatter, body).await;
    path
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, current: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        for entry in entries.map_while(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, snapshot);
            } else {
                let relative = path.strip_prefix(root).expect("path below snapshot root");
                snapshot.insert(
                    relative.to_path_buf(),
                    std::fs::read(&path).expect("read snapshot file"),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn assert_invalid_input(error: std::io::Error) {
    assert_eq!(error.kind(), ErrorKind::InvalidInput, "{error}");
}

#[tokio::test]
async fn invalid_ids_are_rejected_before_filesystem_reads_or_writes() {
    let data = tempdir().expect("temporary data root");
    let outside = tempdir().expect("temporary outside root");
    let store = MemoryStore::new(data.path());

    // This file is outside Jiandu's data root and is a valid, parseable memory.
    // Before validation, an absolute raw id would resolve directly to it.
    let outside_stem = outside.path().join("parseable-memory");
    let absolute_id = outside_stem.to_string_lossy().into_owned();
    let outside_path = PathBuf::from(format!("{absolute_id}.md"));
    let outside_frontmatter = fixture_frontmatter(
        "outside_fixture",
        MemoryScope::Global,
        None,
        "Outside fixture",
    );
    let outside_bytes =
        write_fixture_at(&outside_path, &outside_frontmatter, "must remain unchanged").await;
    assert!(parse_markdown_document(std::str::from_utf8(&outside_bytes).unwrap()).is_ok());

    let invalid_ids = vec![
        absolute_id.clone(),
        "../escape".to_string(),
        "nested/id".to_string(),
        r"nested\id".to_string(),
        String::new(),
        " ".to_string(),
        "x".repeat(MAX_MEMORY_ID_LEN + 1),
    ];
    for invalid_id in &invalid_ids {
        assert!(
            validate_memory_id(invalid_id).is_err(),
            "accepted invalid id {invalid_id:?}"
        );
        assert!(
            store
                .resolver()
                .topic_path(MemoryScope::Global, None, invalid_id)
                .is_err(),
            "built a path for invalid id {invalid_id:?}"
        );
        let error = store
            .get_memory(invalid_id, None)
            .await
            .expect_err("invalid get must fail before lookup");
        assert_invalid_input(error);
    }

    let data_before = snapshot_files(data.path());
    let error = store
        .archive_memory(
            &absolute_id,
            None,
            DurableMemoryStatus::Archived,
            Some("must not escape"),
        )
        .await
        .expect_err("invalid mutation must fail before lookup");
    assert_invalid_input(error);
    assert_eq!(snapshot_files(data.path()), data_before);
    assert_eq!(std::fs::read(&outside_path).unwrap(), outside_bytes);

    for session_id in [".", ".."] {
        let error = store
            .write_session_topic(session_id, "default", "must not be written")
            .await
            .expect_err("reserved session id must fail");
        assert_invalid_input(error);
    }
    assert_eq!(snapshot_files(data.path()), data_before);
}

#[tokio::test]
async fn preferred_project_lookup_is_project_then_global_and_never_another_project() {
    let data = tempdir().expect("temporary data root");
    let root_store = MemoryStore::new(data.path());
    let project_a_id = ProjectId::parse("project-a").expect("Project A id");
    let project_b_id = ProjectId::parse("project-b").expect("Project B id");
    let project_a = root_store.for_project(&project_a_id);
    let project_b = root_store.for_project(&project_b_id);

    let shared_id = "shared_id";
    let a_path = write_store_fixture(
        &project_a,
        MemoryScope::Project,
        Some(project_a_id.as_str()),
        shared_id,
        "Project A copy",
        "body from Project A",
    )
    .await;
    let b_path = write_store_fixture(
        &project_b,
        MemoryScope::Project,
        Some(project_b_id.as_str()),
        shared_id,
        "Project B copy",
        "body from Project B",
    )
    .await;
    let global_path = write_store_fixture(
        &root_store,
        MemoryScope::Global,
        None,
        shared_id,
        "Global copy",
        "body from global",
    )
    .await;
    write_store_fixture(
        &root_store,
        MemoryScope::Global,
        None,
        "global_only",
        "Global fallback",
        "global fallback body",
    )
    .await;
    let b_only_path = write_store_fixture(
        &project_b,
        MemoryScope::Project,
        Some(project_b_id.as_str()),
        "project_b_only",
        "Project B private",
        "private body from Project B",
    )
    .await;

    let from_a = project_a
        .get_memory(" shared_id ", Some(project_a_id.as_str()))
        .await
        .expect("lookup A")
        .expect("A copy");
    assert_eq!(from_a.path, a_path);
    assert_eq!(from_a.body, "body from Project A");

    let from_b = project_b
        .get_memory(shared_id, Some(project_b_id.as_str()))
        .await
        .expect("lookup B")
        .expect("B copy");
    assert_eq!(from_b.path, b_path);
    assert_eq!(from_b.body, "body from Project B");

    let from_global = root_store
        .get_memory(shared_id, None)
        .await
        .expect("lookup global")
        .expect("global copy");
    assert_eq!(from_global.path, global_path);
    assert_eq!(from_global.body, "body from global");

    let fallback = project_a
        .get_memory("global_only", Some(project_a_id.as_str()))
        .await
        .expect("lookup global fallback")
        .expect("global fallback");
    assert_eq!(fallback.frontmatter.scope, MemoryScope::Global);

    assert!(
        project_a
            .get_memory("project_b_only", Some(project_a_id.as_str()))
            .await
            .expect("isolated lookup")
            .is_none()
    );
    assert!(
        root_store
            .get_memory("project_b_only", None)
            .await
            .expect("global-only lookup")
            .is_none()
    );

    let b_only_before = std::fs::read(&b_only_path).unwrap();
    assert!(
        project_a
            .archive_memory(
                "project_b_only",
                Some(project_a_id.as_str()),
                DurableMemoryStatus::Archived,
                Some("must stay isolated"),
            )
            .await
            .expect("isolated mutation")
            .is_none()
    );
    assert_eq!(std::fs::read(&b_only_path).unwrap(), b_only_before);

    let b_shared_before = std::fs::read(&b_path).unwrap();
    let global_shared_before = std::fs::read(&global_path).unwrap();
    let archived_a = project_a
        .archive_memory(
            shared_id,
            Some(project_a_id.as_str()),
            DurableMemoryStatus::Archived,
            Some("archive A only"),
        )
        .await
        .expect("archive A")
        .expect("A target");
    assert_eq!(archived_a.path, a_path);
    assert_eq!(archived_a.frontmatter.status, DurableMemoryStatus::Archived);
    assert_eq!(std::fs::read(&b_path).unwrap(), b_shared_before);
    assert_eq!(std::fs::read(&global_path).unwrap(), global_shared_before);
    assert_eq!(
        project_b
            .get_memory(shared_id, Some(project_b_id.as_str()))
            .await
            .unwrap()
            .unwrap()
            .frontmatter
            .status,
        DurableMemoryStatus::Active
    );
    assert_eq!(
        root_store
            .get_memory(shared_id, None)
            .await
            .unwrap()
            .unwrap()
            .frontmatter
            .status,
        DurableMemoryStatus::Active
    );
}

#[tokio::test]
async fn invalid_source_id_lists_fail_before_any_target_mutation() {
    let data = tempdir().expect("temporary data root");
    let project_id = ProjectId::parse("project-sources").expect("Project id");
    let store = MemoryStore::new(data.path()).for_project(&project_id);

    let target = store
        .write_memory(
            MemoryScope::Project,
            Some(project_id.as_str()),
            DurableMemoryType::Project,
            "Target memory",
            "original target body",
            &[],
            None,
            "security-test",
            false,
            None,
        )
        .await
        .expect("write target");
    store
        .write_memory(
            MemoryScope::Project,
            Some(project_id.as_str()),
            DurableMemoryType::Project,
            "Source memory",
            "original source body",
            &[],
            None,
            "security-test",
            false,
            None,
        )
        .await
        .expect("write source");

    let before = snapshot_files(data.path());
    let error = store
        .merge_memory(
            &target.frontmatter.id,
            Some(project_id.as_str()),
            "forbidden append",
            &[],
            None,
            "security-test",
            &["../escape".to_string()],
        )
        .await
        .expect_err("invalid merge source must fail");
    assert_invalid_input(error);
    assert_eq!(snapshot_files(data.path()), before);

    let error = store
        .mark_memory_contradicted(
            &target.frontmatter.id,
            Some(project_id.as_str()),
            &[String::new()],
            Some("must not mutate"),
            None,
            "security-test",
        )
        .await
        .expect_err("empty contradiction source must fail");
    assert_invalid_input(error);
    assert_eq!(snapshot_files(data.path()), before);

    let merged = MemorySplitPiece {
        title: "Consolidated memory".to_string(),
        r#type: None,
        content: "must not be persisted".to_string(),
        tags: Vec::new(),
    };
    let error = store
        .consolidate_memories(
            &[target.frontmatter.id.clone(), r"nested\source".to_string()],
            Some(project_id.as_str()),
            &merged,
            None,
            "security-test",
        )
        .await
        .expect_err("invalid consolidate source must fail");
    assert_invalid_input(error);
    assert_eq!(snapshot_files(data.path()), before);
}
