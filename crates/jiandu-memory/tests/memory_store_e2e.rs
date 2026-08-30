use std::path::{Path, PathBuf};

use jiandu_memory::ProjectId;
use jiandu_memory::memory_store::{
    DurableMemoryStatus, DurableMemoryType, MemoryQueryOptions, MemoryRecallOptions, MemoryScope,
    MemoryStore, shortlist_relevant_memories,
};
use tempfile::tempdir;

fn atomic_temp_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.map_while(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
            {
                found.push(path);
            }
        }
    }

    found.sort();
    found
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
async fn durable_memory_survives_store_reopen_without_ingest() {
    let directory = tempdir().expect("temporary data directory");
    let project_id = ProjectId::parse("project-restart").expect("valid Project id");
    let store = MemoryStore::new(directory.path()).for_project(&project_id);

    let first = store
        .write_memory(
            MemoryScope::Project,
            Some(project_id.as_str()),
            DurableMemoryType::Project,
            "Orion restart checklist",
            "Orion restart requires a staged deployment and a health check.",
            &["orion".to_string(), "restart".to_string()],
            Some("session-restart"),
            "e2e",
            false,
            None,
        )
        .await
        .expect("write first durable memory");
    let second = store
        .write_memory(
            MemoryScope::Project,
            Some(project_id.as_str()),
            DurableMemoryType::Reference,
            "Orion restart reference",
            "The Orion restart runbook is the authoritative operational reference.",
            &["orion".to_string(), "runbook".to_string()],
            Some("session-restart"),
            "e2e",
            false,
            None,
        )
        .await
        .expect("write second durable memory");

    let documents_before = store
        .list_memory_documents(MemoryScope::Project, Some(project_id.as_str()))
        .await
        .expect("list records before reopen");
    assert_eq!(documents_before.len(), 2);
    let expected_documents = documents_before
        .iter()
        .map(|document| {
            (
                document.frontmatter.id.clone(),
                document.body.clone(),
                document.frontmatter.status,
            )
        })
        .collect::<Vec<_>>();
    assert!(
        expected_documents
            .iter()
            .any(|(id, _, _)| id == &first.frontmatter.id)
    );
    assert!(
        expected_documents
            .iter()
            .any(|(id, _, _)| id == &second.frontmatter.id)
    );

    let query_before = store
        .query_scope(
            MemoryScope::Project,
            Some(project_id.as_str()),
            Some("orion restart"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query before reopen");
    assert_eq!(query_before.matched_count, 2);
    assert_eq!(query_before.returned_count, 2);
    let expected_order = query_before
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();

    for (id, body, status) in &expected_documents {
        let fetched = store
            .get_memory(id, Some(project_id.as_str()))
            .await
            .expect("get before reopen")
            .expect("record exists before reopen");
        assert_eq!(&fetched.body, body);
        assert_eq!(&fetched.frontmatter.status, status);
    }

    drop(store);

    // Reopening requires no scan, ingest, or rebuild call before reads work.
    let reopened = MemoryStore::new(directory.path()).for_project(&project_id);
    let query_after = reopened
        .query_scope(
            MemoryScope::Project,
            Some(project_id.as_str()),
            Some("orion restart"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query after reopen");
    assert_eq!(query_after.matched_count, query_before.matched_count);
    assert_eq!(query_after.returned_count, query_before.returned_count);
    assert_eq!(
        query_after
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        expected_order
    );

    let documents_after = reopened
        .list_memory_documents(MemoryScope::Project, Some(project_id.as_str()))
        .await
        .expect("list records after reopen");
    assert_eq!(documents_after.len(), expected_documents.len());
    for (id, body, status) in &expected_documents {
        let fetched = reopened
            .get_memory(id, Some(project_id.as_str()))
            .await
            .expect("get after reopen")
            .expect("record exists after reopen");
        assert_eq!(&fetched.frontmatter.id, id);
        assert_eq!(&fetched.body, body);
        assert_eq!(&fetched.frontmatter.status, status);
        assert_eq!(fetched.frontmatter.status, DurableMemoryStatus::Active);
    }

    assert_eq!(atomic_temp_files(directory.path()), Vec::<PathBuf>::new());
}

#[tokio::test]
async fn project_scopes_are_isolated_and_recall_is_project_first_with_global_fallback() {
    let directory = tempdir().expect("temporary data directory");
    let root_store = MemoryStore::new(directory.path());
    let project_a_id = ProjectId::parse("project-a").expect("valid Project A id");
    let project_b_id = ProjectId::parse("project-b").expect("valid Project B id");
    let project_a = root_store.for_project(&project_a_id);
    let project_b = root_store.for_project(&project_b_id);

    let memory_a = project_a
        .write_memory(
            MemoryScope::Project,
            Some(project_a_id.as_str()),
            DurableMemoryType::Project,
            "Nebula decision for Project A",
            "Project A owns the nebula deployment decision.",
            &["nebula".to_string()],
            Some("session-a"),
            "e2e",
            false,
            None,
        )
        .await
        .expect("write Project A memory");
    let memory_b = project_b
        .write_memory(
            MemoryScope::Project,
            Some(project_b_id.as_str()),
            DurableMemoryType::Project,
            "Nebula decision for Project B",
            "Project B owns a different nebula deployment decision.",
            &["nebula".to_string()],
            Some("session-b"),
            "e2e",
            false,
            None,
        )
        .await
        .expect("write Project B memory");
    let global_memory = root_store
        .write_memory(
            MemoryScope::Global,
            None,
            DurableMemoryType::Reference,
            "Nebula global fallback beacon",
            "The fallback-beacon is global guidance for nebula work.",
            &["nebula".to_string(), "fallback-beacon".to_string()],
            Some("session-global"),
            "e2e",
            false,
            None,
        )
        .await
        .expect("write global memory");

    let project_a_query = project_a
        .query_scope(
            MemoryScope::Project,
            Some(project_a_id.as_str()),
            Some("nebula"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query Project A");
    assert_eq!(project_a_query.matched_count, 1);
    assert_eq!(project_a_query.items[0].id, memory_a.frontmatter.id);

    let project_b_query = project_b
        .query_scope(
            MemoryScope::Project,
            Some(project_b_id.as_str()),
            Some("nebula"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query Project B");
    assert_eq!(project_b_query.matched_count, 1);
    assert_eq!(project_b_query.items[0].id, memory_b.frontmatter.id);

    let global_query = root_store
        .query_scope(
            MemoryScope::Global,
            None,
            Some("nebula"),
            None,
            None,
            None,
            &query_options(),
        )
        .await
        .expect("query Global scope");
    assert_eq!(global_query.matched_count, 1);
    assert_eq!(global_query.items[0].id, global_memory.frontmatter.id);

    let project_first = shortlist_relevant_memories(
        &project_a,
        Some(project_a_id.as_str()),
        "nebula",
        &MemoryRecallOptions::default(),
    )
    .await
    .expect("project-first shortlist");
    assert_eq!(project_first.len(), 1);
    assert_eq!(project_first[0].id, memory_a.frontmatter.id);
    assert_eq!(project_first[0].scope, MemoryScope::Project);

    let global_fallback = shortlist_relevant_memories(
        &project_a,
        Some(project_a_id.as_str()),
        "fallback-beacon",
        &MemoryRecallOptions::default(),
    )
    .await
    .expect("global fallback shortlist");
    assert_eq!(global_fallback.len(), 1);
    assert_eq!(global_fallback[0].id, global_memory.frontmatter.id);
    assert_eq!(global_fallback[0].scope, MemoryScope::Global);
}

#[tokio::test]
async fn session_topics_replace_append_list_clear_and_preserve_concurrent_appends() {
    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());

    store
        .write_session_topic("session-lifecycle", "notes", "first version")
        .await
        .expect("write first version");
    store
        .write_session_topic("session-lifecycle", "notes", "replacement version")
        .await
        .expect("replace topic");
    assert_eq!(
        store
            .read_session_topic("session-lifecycle", "notes")
            .await
            .expect("read replacement")
            .as_deref(),
        Some("replacement version")
    );

    store
        .append_session_topic("session-lifecycle", "notes", "appended section")
        .await
        .expect("append topic");
    store
        .write_session_topic("session-lifecycle", "context", "context body")
        .await
        .expect("write second topic");
    assert_eq!(
        store
            .read_session_topic("session-lifecycle", "notes")
            .await
            .expect("read appended topic")
            .as_deref(),
        Some("replacement version\n\nappended section")
    );
    assert_eq!(
        store
            .list_session_topics("session-lifecycle")
            .await
            .expect("list topics"),
        vec!["context".to_string(), "notes".to_string()]
    );

    assert!(
        store
            .delete_session_topic("session-lifecycle", "notes")
            .await
            .expect("clear topic")
    );
    assert!(
        !store
            .delete_session_topic("session-lifecycle", "notes")
            .await
            .expect("clearing an absent topic is idempotent")
    );
    assert_eq!(
        store
            .read_session_topic("session-lifecycle", "notes")
            .await
            .expect("read cleared topic"),
        None
    );
    assert_eq!(
        store
            .list_session_topics("session-lifecycle")
            .await
            .expect("list after clear"),
        vec!["context".to_string()]
    );

    const WRITERS: usize = 12;
    let mut writers = Vec::with_capacity(WRITERS);
    for index in 0..WRITERS {
        let data_dir = directory.path().to_path_buf();
        writers.push(tokio::spawn(async move {
            MemoryStore::new(data_dir)
                .append_session_topic("session-concurrent", "shared", &format!("entry-{index:02}"))
                .await
        }));
    }
    for writer in writers {
        writer
            .await
            .expect("append task joins")
            .expect("append succeeds");
    }

    let concurrent = store
        .read_session_topic("session-concurrent", "shared")
        .await
        .expect("read concurrent topic")
        .expect("concurrent topic exists");
    let mut sections = concurrent
        .split("\n\n")
        .map(str::to_string)
        .collect::<Vec<_>>();
    sections.sort();
    assert_eq!(
        sections,
        (0..WRITERS)
            .map(|index| format!("entry-{index:02}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        store
            .list_session_topics("session-concurrent")
            .await
            .expect("list concurrent topics"),
        vec!["shared".to_string()]
    );
}
