use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use jiandu_memory::memory_store::{
    CreatedBy, DurableMemoryFrontmatter, DurableMemoryRelations, DurableMemoryRetrieval,
    DurableMemorySource, DurableMemoryStatus, DurableMemoryType, MemoryQueryOptions, MemoryScope,
    MemoryStore, TemporalGranularity, render_markdown_document,
};
use jiandu_memory::{ProjectId, import_bamboo_durable_memory};
use tempfile::tempdir;

fn fixture_frontmatter(
    id: &str,
    scope: MemoryScope,
    project_id: Option<&ProjectId>,
    ordinal: usize,
) -> DurableMemoryFrontmatter {
    DurableMemoryFrontmatter {
        id: id.to_string(),
        title: format!("Imported memory {ordinal}"),
        r#type: if ordinal.is_multiple_of(2) {
            DurableMemoryType::Reference
        } else {
            DurableMemoryType::Project
        },
        scope,
        project_key: project_id.map(ToString::to_string),
        granularity: Some(TemporalGranularity::Month),
        status: if ordinal.is_multiple_of(7) {
            DurableMemoryStatus::Stale
        } else {
            DurableMemoryStatus::Active
        },
        freshness: Some("review quarterly".to_string()),
        confidence: Some("confirmed".to_string()),
        created_at: "2026-08-01T00:00:00+00:00".to_string(),
        updated_at: format!("2026-08-{:02}T00:00:00+00:00", ordinal % 28 + 1),
        created_by: CreatedBy {
            kind: "model".to_string(),
            id: Some("fixture-session".to_string()),
            actor: Some("bamboo".to_string()),
        },
        updated_by: CreatedBy {
            kind: "model".to_string(),
            id: Some("fixture-session".to_string()),
            actor: Some("bamboo".to_string()),
        },
        sources: vec![DurableMemorySource {
            kind: "message".to_string(),
            id: format!("source-{ordinal}"),
            message_range: vec![format!("{ordinal}..{}", ordinal + 1)],
        }],
        relations: DurableMemoryRelations {
            supersedes: vec![format!("old-{ordinal}")],
            contradicted_by: vec![],
            related: vec![format!("related-{ordinal}")],
        },
        tags: vec!["imported".to_string(), format!("ordinal-{ordinal}")],
        retrieval: DurableMemoryRetrieval {
            keywords: vec!["bamboo".to_string(), format!("marker{ordinal}")],
            entities: vec![format!("Entity-{ordinal}")],
            embedding_ready: false,
            last_accessed_at: Some("2026-08-30T00:00:00+00:00".to_string()),
        },
    }
}

fn topic_path(
    data_dir: &Path,
    scope: MemoryScope,
    project_id: Option<&ProjectId>,
    id: &str,
) -> PathBuf {
    match scope {
        MemoryScope::Global => data_dir
            .join("memory/v1/scopes/global/topics")
            .join(format!("{id}.md")),
        MemoryScope::Project => data_dir
            .join("projects")
            .join(project_id.expect("Project topic requires id").as_str())
            .join("memory/v1/topics")
            .join(format!("{id}.md")),
        MemoryScope::Session => panic!("Session topics are not Bamboo import fixtures"),
    }
}

fn write_topic(
    data_dir: &Path,
    id: &str,
    scope: MemoryScope,
    project_id: Option<&ProjectId>,
    ordinal: usize,
    body: &str,
) -> DurableMemoryFrontmatter {
    let frontmatter = fixture_frontmatter(id, scope, project_id, ordinal);
    let rendered = render_markdown_document(&frontmatter, body).expect("render fixture topic");
    let path = topic_path(data_dir, scope, project_id, id);
    fs::create_dir_all(path.parent().expect("topic parent")).expect("create topic directory");
    fs::write(path, rendered).expect("write fixture topic");
    frontmatter
}

fn write_file(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, content).expect("write fixture file");
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64)> {
    let mut snapshot = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("read fixture tree") {
            let entry = entry.expect("fixture entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("relative fixture path")
                    .to_path_buf();
                let metadata = fs::metadata(&path).expect("fixture metadata");
                let modified = metadata
                    .modified()
                    .expect("fixture modified time")
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("fixture timestamp")
                    .as_nanos() as u64;
                snapshot.insert(relative, (fs::read(path).expect("fixture bytes"), modified));
            }
        }
    }
    snapshot
}

fn assert_no_staging_siblings(parent: &Path) {
    assert!(
        fs::read_dir(parent)
            .expect("read parent")
            .map_while(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".jiandu-bamboo-import-")),
        "one-shot import must clean sibling staging directories"
    );
}

fn query_options() -> MemoryQueryOptions {
    MemoryQueryOptions {
        limit: Some(20),
        max_chars: Some(6_000),
        cursor: None,
        include_related: true,
    }
}

#[tokio::test]
async fn empty_source_publishes_an_empty_jiandu_root() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    fs::create_dir(&source).expect("empty source");

    let report = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect("import empty source");
    assert_eq!(report.scanned, 0);
    assert_eq!(report.imported, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.rebuilt_scopes, 0);
    assert!(destination.is_dir());
    assert!(
        fs::read_dir(&destination)
            .expect("read empty destination")
            .next()
            .is_none()
    );
    assert_no_staging_siblings(root.path());
}

#[tokio::test]
async fn global_only_import_accepts_an_existing_empty_destination() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    fs::create_dir(&source).expect("source");
    fs::create_dir(&destination).expect("empty destination");
    write_topic(
        &source,
        "mem_global_only",
        MemoryScope::Global,
        None,
        1,
        "global-only marker",
    );

    let report = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect("global import");
    assert_eq!(report.global_topics, 1);
    assert_eq!(report.project_topics, 0);
    assert_eq!(report.rebuilt_scopes, 1);
    assert!(
        MemoryStore::new(&destination)
            .get_memory("mem_global_only", None)
            .await
            .expect("get global")
            .is_some()
    );
}

#[tokio::test]
async fn project_only_import_keeps_the_opaque_project_id() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    let project_id = ProjectId::parse("project-only").expect("Project id");
    fs::create_dir(&source).expect("source");
    write_topic(
        &source,
        "mem_project_only",
        MemoryScope::Project,
        Some(&project_id),
        2,
        "project-only marker",
    );

    let report = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect("Project import");
    assert_eq!(report.global_topics, 0);
    assert_eq!(report.project_topics, 1);
    assert_eq!(report.project_scopes, 1);
    assert_eq!(report.rebuilt_scopes, 1);
    let doc = MemoryStore::new(&destination)
        .for_project(&project_id)
        .get_memory("mem_project_only", Some(project_id.as_str()))
        .await
        .expect("get Project topic")
        .expect("Project topic exists");
    assert_eq!(doc.frontmatter.project_key.as_deref(), Some("project-only"));
}

#[tokio::test]
async fn combined_import_preserves_topics_and_ignores_non_topic_bamboo_state() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    let destination_two = root.path().join("jiandu-two");
    let project_id = ProjectId::parse("project-combined").expect("Project id");
    fs::create_dir(&source).expect("source");
    let global_frontmatter = write_topic(
        &source,
        "mem_combined_global",
        MemoryScope::Global,
        None,
        3,
        "Global exact body with a combined-global-needle.",
    );
    let project_frontmatter = write_topic(
        &source,
        "mem_combined_project",
        MemoryScope::Project,
        Some(&project_id),
        4,
        "Project exact body with a combined-project-needle.",
    );

    write_file(
        source.join("memory/v1/sessions/session-1/note/default.md"),
        "do not import Session notes",
    );
    write_file(
        source.join("memory/v1/scopes/global/indexes/source-only.json"),
        "do not copy indexes",
    );
    write_file(
        source.join("memory/v1/scopes/global/views/DREAM_NOTEBOOK.md"),
        "do not copy Dream views",
    );
    write_file(
        source.join("memory/v1/scopes/global/logs/access_log.jsonl"),
        "do not copy logs",
    );
    write_file(
        source.join("projects/project-combined/project.json"),
        "do not copy Project administration",
    );
    write_file(
        source.join("projects/project-combined/state/legacy-memory-migration/journal.json"),
        "do not copy migration journals",
    );
    write_file(source.join("plan/session-1/plan.md"), "do not copy plans");

    let source_before = tree_snapshot(&source);
    let report = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect("combined import");
    assert_eq!(report.scanned, 2);
    assert_eq!(report.imported, 2);
    assert_eq!(report.failed, 0);
    assert_eq!(report.global_topics, 1);
    assert_eq!(report.project_topics, 1);
    assert_eq!(report.rebuilt_scopes, 2);
    assert_eq!(report.content_identity_sha256.len(), 64);
    assert_eq!(tree_snapshot(&source), source_before);

    for (scope, id, project) in [
        (MemoryScope::Global, "mem_combined_global", None),
        (
            MemoryScope::Project,
            "mem_combined_project",
            Some(&project_id),
        ),
    ] {
        assert_eq!(
            fs::read(topic_path(&destination, scope, project, id)).expect("destination topic"),
            fs::read(topic_path(&source, scope, project, id)).expect("source topic")
        );
    }

    for excluded in [
        "memory/v1/sessions",
        "memory/v1/scopes/global/indexes/source-only.json",
        "memory/v1/scopes/global/views/DREAM_NOTEBOOK.md",
        "memory/v1/scopes/global/logs/access_log.jsonl",
        "projects/project-combined/project.json",
        "projects/project-combined/state/legacy-memory-migration",
        "plan",
    ] {
        assert!(
            !destination.join(excluded).exists(),
            "excluded Bamboo state was copied: {excluded}"
        );
    }

    let reopened = MemoryStore::new(&destination);
    let global = reopened
        .get_memory("mem_combined_global", None)
        .await
        .expect("get Global")
        .expect("Global exists");
    assert_eq!(global.frontmatter, global_frontmatter);
    assert_eq!(
        global.body,
        "Global exact body with a combined-global-needle."
    );
    let project_store = reopened.for_project(&project_id);
    let project = project_store
        .get_memory("mem_combined_project", Some(project_id.as_str()))
        .await
        .expect("get Project")
        .expect("Project exists");
    assert_eq!(project.frontmatter, project_frontmatter);
    assert_eq!(
        project.body,
        "Project exact body with a combined-project-needle."
    );
    let query = project_store
        .query_scope(
            MemoryScope::Project,
            Some(project_id.as_str()),
            Some("combined project needle"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query imported Project");
    assert_eq!(query.matched_count, 1);
    let inspect = project_store
        .inspect_scope(MemoryScope::Project, Some(project_id.as_str()))
        .await
        .expect("inspect imported Project");
    assert_eq!(inspect.total_memories, 1);
    assert!(
        inspect
            .index_files
            .iter()
            .any(|name| name == "lexical.json")
    );
    assert!(inspect.view_files.iter().any(|name| name == "MEMORY.md"));

    let second_report = import_bamboo_durable_memory(&source, &destination_two)
        .await
        .expect("repeat import into another root");
    assert_eq!(
        second_report.content_identity_sha256,
        report.content_identity_sha256
    );
    assert_eq!(
        fs::read_to_string(destination.join("memory/v1/scopes/global/views/MEMORY.md"))
            .expect("first deterministic view"),
        fs::read_to_string(destination_two.join("memory/v1/scopes/global/views/MEMORY.md"))
            .expect("second deterministic view")
    );
    let mut first_lexical: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(destination.join("memory/v1/scopes/global/indexes/lexical.json"))
            .expect("first lexical index"),
    )
    .expect("parse first lexical index");
    let mut second_lexical: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(destination_two.join("memory/v1/scopes/global/indexes/lexical.json"))
            .expect("second lexical index"),
    )
    .expect("parse second lexical index");
    first_lexical["generated_at"] = serde_json::Value::Null;
    second_lexical["generated_at"] = serde_json::Value::Null;
    assert_eq!(first_lexical, second_lexical);
    assert_no_staging_siblings(root.path());
}

#[tokio::test]
async fn malformed_and_misplaced_topics_fail_before_destination_publication() {
    let root = tempdir().expect("root");

    let malformed_source = root.path().join("malformed-source");
    let malformed_destination = root.path().join("malformed-destination");
    write_file(
        malformed_source.join("memory/v1/scopes/global/topics/mem_bad.md"),
        "not frontmatter",
    );
    let error = import_bamboo_durable_memory(&malformed_source, &malformed_destination)
        .await
        .expect_err("malformed topic must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!malformed_destination.exists());

    let mismatch_source = root.path().join("mismatch-source");
    let mismatch_destination = root.path().join("mismatch-destination");
    fs::create_dir(&mismatch_source).expect("mismatch source");
    let project_id = ProjectId::parse("misplaced-project").expect("Project id");
    let misplaced =
        fixture_frontmatter("mem_misplaced", MemoryScope::Project, Some(&project_id), 5);
    write_file(
        mismatch_source.join("memory/v1/scopes/global/topics/mem_misplaced.md"),
        render_markdown_document(&misplaced, "wrong scope path").expect("render misplaced"),
    );
    let error = import_bamboo_durable_memory(&mismatch_source, &mismatch_destination)
        .await
        .expect_err("scope/path mismatch must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!mismatch_destination.exists());
    assert_no_staging_siblings(root.path());
}

#[tokio::test]
async fn invalid_project_and_duplicate_id_fail_before_destination_publication() {
    let root = tempdir().expect("root");

    let invalid_source = root.path().join("invalid-project-source");
    let invalid_destination = root.path().join("invalid-project-destination");
    let invalid_frontmatter =
        fixture_frontmatter("mem_invalid_project", MemoryScope::Project, None, 6);
    write_file(
        invalid_source.join("projects/bad project/memory/v1/topics/mem_invalid_project.md"),
        render_markdown_document(&invalid_frontmatter, "invalid Project path")
            .expect("render invalid Project fixture"),
    );
    let error = import_bamboo_durable_memory(&invalid_source, &invalid_destination)
        .await
        .expect_err("invalid ProjectId must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!invalid_destination.exists());

    let duplicate_source = root.path().join("duplicate-source");
    let duplicate_destination = root.path().join("duplicate-destination");
    let project_id = ProjectId::parse("duplicate-project").expect("Project id");
    fs::create_dir(&duplicate_source).expect("duplicate source");
    write_topic(
        &duplicate_source,
        "mem_duplicate",
        MemoryScope::Global,
        None,
        7,
        "Global duplicate",
    );
    write_topic(
        &duplicate_source,
        "mem_duplicate",
        MemoryScope::Project,
        Some(&project_id),
        8,
        "Project duplicate",
    );
    let error = import_bamboo_durable_memory(&duplicate_source, &duplicate_destination)
        .await
        .expect_err("duplicate id must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!duplicate_destination.exists());
    assert_no_staging_siblings(root.path());
}

#[tokio::test]
async fn source_equals_destination_and_nonempty_destination_are_rejected_unchanged() {
    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    fs::create_dir(&source).expect("source");
    write_topic(
        &source,
        "mem_same_root",
        MemoryScope::Global,
        None,
        9,
        "same root",
    );
    let source_before = tree_snapshot(&source);
    let error = import_bamboo_durable_memory(&source, &source)
        .await
        .expect_err("source=destination must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(tree_snapshot(&source), source_before);

    let destination = root.path().join("nonempty-destination");
    write_file(destination.join("keep.txt"), "keep me");
    let error = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect_err("nonempty destination must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read_to_string(destination.join("keep.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(tree_snapshot(&source), source_before);
    assert_no_staging_siblings(root.path());
}

#[cfg(unix)]
#[tokio::test]
async fn global_topic_ancestor_symlink_cannot_escape_the_bamboo_source() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let external = root.path().join("external");
    let destination = root.path().join("jiandu");
    fs::create_dir(&source).expect("source");
    write_topic(
        &external,
        "mem_external_global",
        MemoryScope::Global,
        None,
        10,
        "must not follow a Global ancestor symlink",
    );
    symlink(external.join("memory"), source.join("memory")).expect("Global ancestor symlink");

    let error = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect_err("Global ancestor symlink outside the source must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("escapes the source data root"));
    assert!(!destination.exists());
    assert_no_staging_siblings(root.path());
}

#[cfg(unix)]
#[tokio::test]
async fn project_topic_ancestor_symlink_cannot_escape_the_bamboo_source() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let external = root.path().join("external");
    let destination = root.path().join("jiandu");
    let project_id = ProjectId::parse("symlink-project").expect("Project id");
    fs::create_dir_all(source.join("projects").join(project_id.as_str()))
        .expect("source Project home");
    write_topic(
        &external,
        "mem_external_project",
        MemoryScope::Project,
        Some(&project_id),
        11,
        "must not follow a Project ancestor symlink",
    );
    symlink(
        external
            .join("projects")
            .join(project_id.as_str())
            .join("memory"),
        source
            .join("projects")
            .join(project_id.as_str())
            .join("memory"),
    )
    .expect("Project ancestor symlink");

    let error = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect_err("Project ancestor symlink outside the source must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("escapes the source data root"));
    assert!(!destination.exists());
    assert_no_staging_siblings(root.path());
}

#[cfg(unix)]
#[tokio::test]
async fn final_topics_symlink_remains_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let external = root.path().join("external");
    let destination = root.path().join("jiandu");
    write_topic(
        &external,
        "mem_external_topics",
        MemoryScope::Global,
        None,
        12,
        "must not follow the final topics symlink",
    );
    let global_root = source.join("memory/v1/scopes/global");
    fs::create_dir_all(&global_root).expect("Global scope root");
    symlink(
        external.join("memory/v1/scopes/global/topics"),
        global_root.join("topics"),
    )
    .expect("final topics symlink");

    let error = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect_err("final topics symlink must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("not a real directory"));
    assert!(!destination.exists());
    assert_no_staging_siblings(root.path());
}

#[tokio::test]
async fn live_shaped_577_global_and_865_project_topics_are_fully_readable() {
    const GLOBAL_COUNT: usize = 577;
    const PROJECT_COUNT: usize = 865;

    let root = tempdir().expect("root");
    let source = root.path().join("bamboo");
    let destination = root.path().join("jiandu");
    let project_id = ProjectId::parse("live-shaped-project").expect("Project id");
    fs::create_dir(&source).expect("source");

    for ordinal in 0..GLOBAL_COUNT {
        write_topic(
            &source,
            &format!("mem_global_{ordinal:04}"),
            MemoryScope::Global,
            None,
            ordinal,
            &format!("Global fixture marker globalmarker{ordinal}"),
        );
    }
    for ordinal in 0..PROJECT_COUNT {
        write_topic(
            &source,
            &format!("mem_project_{ordinal:04}"),
            MemoryScope::Project,
            Some(&project_id),
            ordinal,
            &format!("Project fixture marker projectmarker{ordinal}"),
        );
    }

    let report = import_bamboo_durable_memory(&source, &destination)
        .await
        .expect("live-shaped import");
    assert_eq!(report.scanned, GLOBAL_COUNT + PROJECT_COUNT);
    assert_eq!(report.imported, GLOBAL_COUNT + PROJECT_COUNT);
    assert_eq!(report.failed, 0);
    assert_eq!(report.global_topics, GLOBAL_COUNT);
    assert_eq!(report.project_topics, PROJECT_COUNT);
    assert_eq!(report.project_scopes, 1);
    assert_eq!(report.rebuilt_scopes, 2);

    let store = MemoryStore::new(&destination);
    let global_inspect = store
        .inspect_scope(MemoryScope::Global, None)
        .await
        .expect("inspect live Global");
    assert_eq!(global_inspect.total_memories, GLOBAL_COUNT);
    let project_store = store.for_project(&project_id);
    let project_inspect = project_store
        .inspect_scope(MemoryScope::Project, Some(project_id.as_str()))
        .await
        .expect("inspect live Project");
    assert_eq!(project_inspect.total_memories, PROJECT_COUNT);

    let exact_global = store
        .get_memory("mem_global_0576", None)
        .await
        .expect("get representative Global")
        .expect("representative Global exists");
    assert_eq!(exact_global.body, "Global fixture marker globalmarker576");
    let exact_project = project_store
        .get_memory("mem_project_0864", Some(project_id.as_str()))
        .await
        .expect("get representative Project")
        .expect("representative Project exists");
    assert_eq!(
        exact_project.body,
        "Project fixture marker projectmarker864"
    );
    let query = project_store
        .query_scope(
            MemoryScope::Project,
            Some(project_id.as_str()),
            Some("projectmarker864"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query live Project");
    assert!(query.items.iter().any(|item| item.id == "mem_project_0864"));
    for inspect in [global_inspect, project_inspect] {
        assert!(
            inspect
                .index_files
                .iter()
                .any(|name| name == "lexical.json")
        );
        assert!(inspect.index_files.iter().any(|name| name == "graph.json"));
        assert!(inspect.view_files.iter().any(|name| name == "MEMORY.md"));
        assert!(
            inspect
                .state_files
                .iter()
                .any(|name| name == "schema_version.json")
        );
    }
    assert_no_staging_siblings(root.path());
}
