use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use jiandu_memory::{
    ProjectId,
    memory_store::{
        DurableMemoryDocument, DurableMemoryStatus, DurableMemoryType, MAX_MAX_CHARS,
        MemoryRetrievalInput, MemoryScope, MemoryStore, ProjectMemoryRead, ProjectMemoryRef,
        render_markdown_document,
    },
};
use tempfile::tempdir;

fn project(value: &str) -> ProjectId {
    ProjectId::parse(value).unwrap()
}

fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    if root.exists() {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(bytes(&path));
            } else {
                result.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    result
}

async fn write(
    store: &MemoryStore,
    project: &ProjectId,
    title: &str,
    content: &str,
) -> DurableMemoryDocument {
    store
        .write_memory(
            MemoryScope::Project,
            Some(project.as_str()),
            DurableMemoryType::Reference,
            title,
            content,
            &[],
            Some("source-session"),
            "fixture",
            false,
            None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn single_work_query_get_and_handoff_reference_survive_restart_without_writing() {
    let root = tempdir().unwrap();
    let project = project("project-one");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let document = write(
        &store,
        &project,
        "Work01 workspace recovery",
        "Verify the adopted binding and original receipt.",
    )
    .await;
    // This map stands for the host's already-adopted source map. It is neither
    // persisted by Jiandu nor inferred from a model's summary or Session range.
    let reference = ProjectMemoryRef::from_document(&document).unwrap();
    let sources = BTreeMap::from([(
        reference.source_key(),
        reference.content_version().to_string(),
    )]);
    let before = bytes(root.path());

    let candidates = store
        .query_project_sources_read_only(&project, "Work01 workspace")
        .await
        .unwrap();
    assert_eq!(candidates.items.len(), 1);
    assert_eq!(candidates.items[0].id, reference.memory_id());
    assert!(!candidates.more_available);
    let discovery = serde_json::to_value(candidates).unwrap();
    assert!(discovery.get("next_cursor").is_none());
    assert!(discovery.get("watermark").is_none());

    let handoff = serde_json::to_vec(&reference).unwrap();
    let adopted: ProjectMemoryRef = serde_json::from_slice(&handoff).unwrap();
    assert_eq!(
        sources.get(&adopted.source_key()).unwrap(),
        adopted.content_version()
    );
    let reopened = MemoryStore::new(root.path()).for_project(&project);
    match reopened
        .get_project_source_read_only(&adopted)
        .await
        .unwrap()
    {
        ProjectMemoryRead::VersionMatched {
            reference: actual,
            title,
            content,
            confidence,
        } => {
            assert_eq!(actual, reference);
            assert_eq!(title, document.frontmatter.title);
            assert_eq!(content, document.body);
            assert_eq!(confidence, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn changed_canonical_source_between_query_and_get_never_returns_the_old_body() {
    let root = tempdir().unwrap();
    let project = project("project-change");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let document = write(
        &store,
        &project,
        "Work02 validation",
        "Check the source version.",
    )
    .await;
    let pinned = ProjectMemoryRef::from_document(&document).unwrap();
    assert_eq!(
        store
            .query_project_sources_read_only(&project, "Work02")
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    store
        .merge_memory_with_retrieval(
            pinned.memory_id(),
            Some(project.as_str()),
            "The current verification command changed.",
            &[],
            &MemoryRetrievalInput::default(),
            Some("source-session"),
            "fixture",
            &[],
        )
        .await
        .unwrap()
        .unwrap();
    let before = bytes(root.path());
    let resolution = store.get_project_source_read_only(&pinned).await.unwrap();
    let ProjectMemoryRead::VersionMismatch { observed } = &resolution else {
        panic!("changed input must not be returned as current: {resolution:?}");
    };
    assert_ne!(observed.content_version(), pinned.content_version());
    assert_eq!(observed.source_key(), pinned.source_key());
    let wire = serde_json::to_value(&resolution).unwrap();
    assert!(wire.get("content").is_none());
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn bound_project_rejects_another_project_and_never_falls_back_to_global_or_session() {
    let root = tempdir().unwrap();
    let a = project("project-a");
    let b = project("project-b");
    let base = MemoryStore::new(root.path());
    let store_a = base.for_project(&a);
    let store_b = base.for_project(&b);
    let private = write(
        &store_b,
        &b,
        "Private Work03",
        "Private workspace evidence.",
    )
    .await;
    let private_ref = ProjectMemoryRef::from_document(&private).unwrap();
    let global = base
        .write_memory(
            MemoryScope::Global,
            None,
            DurableMemoryType::User,
            "Work03 global hint",
            "Global content is not Project evidence.",
            &[],
            None,
            "fixture",
            false,
            None,
        )
        .await
        .unwrap();
    base.write_session_topic("other-session", "default", "Work03 private transcript")
        .await
        .unwrap();
    let same_global_id = ProjectMemoryRef::new(
        a.clone(),
        &global.frontmatter.id,
        private_ref.content_version(),
    )
    .unwrap();
    let other_project_id = ProjectMemoryRef::new(
        a.clone(),
        private_ref.memory_id(),
        private_ref.content_version(),
    )
    .unwrap();
    let before = bytes(root.path());

    assert!(
        store_a
            .query_project_sources_read_only(&b, "Work03")
            .await
            .is_err()
    );
    assert!(
        store_a
            .get_project_source_read_only(&private_ref)
            .await
            .is_err()
    );
    assert_eq!(
        store_a
            .get_project_source_read_only(&same_global_id)
            .await
            .unwrap(),
        ProjectMemoryRead::Missing
    );
    assert_eq!(
        store_a
            .get_project_source_read_only(&other_project_id)
            .await
            .unwrap(),
        ProjectMemoryRead::Missing
    );
    assert!(ProjectMemoryRef::from_document(&global).is_err());
    // Cold Project A has no lexical index. It must report the missing index,
    // never substitute Global/Session content or rebuild it during a read.
    assert!(
        store_a
            .query_project_sources_read_only(&a, "Work03")
            .await
            .is_err()
    );
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn recall_is_top_three_active_only_and_does_not_auto_hydrate_or_expose_a_cursor() {
    let root = tempdir().unwrap();
    let project = project("project-ranked");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let mut documents = Vec::new();
    for index in 0..5 {
        documents.push(
            write(
                &store,
                &project,
                &format!("Work04 evidence {index}"),
                &format!("Work04 synthetic checkpoint reference {index}."),
            )
            .await,
        );
    }
    store
        .archive_memory(
            &documents[0].frontmatter.id,
            Some(project.as_str()),
            DurableMemoryStatus::Stale,
            Some("source no longer applicable"),
        )
        .await
        .unwrap()
        .unwrap();
    let before = bytes(root.path());
    let recalled = store
        .query_project_sources_read_only(&project, "Work04")
        .await
        .unwrap();
    assert_eq!(recalled.items.len(), 3);
    assert!(recalled.more_available);
    assert!(
        recalled
            .items
            .iter()
            .all(|item| item.status == DurableMemoryStatus::Active)
    );
    assert!(
        !recalled
            .items
            .iter()
            .any(|item| item.id == documents[0].frontmatter.id)
    );
    let wire = serde_json::to_value(&recalled).unwrap();
    assert!(wire["items"][0].get("content").is_none());
    assert!(wire.get("next_cursor").is_none());
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn inactive_sources_remain_unusable_after_explicitly_pinning_the_new_status() {
    let root = tempdir().unwrap();
    let project = project("project-inactive");
    let store = MemoryStore::new(root.path()).for_project(&project);
    for status in [
        DurableMemoryStatus::Stale,
        DurableMemoryStatus::Contradicted,
        DurableMemoryStatus::Superseded,
        DurableMemoryStatus::Archived,
    ] {
        let document = write(
            &store,
            &project,
            &format!("Inactive {status:?}"),
            &format!("Old evidence {status:?}."),
        )
        .await;
        let inactive = store
            .archive_memory(
                &document.frontmatter.id,
                Some(project.as_str()),
                status,
                Some("fixture"),
            )
            .await
            .unwrap()
            .unwrap();
        let reference = ProjectMemoryRef::from_document(&inactive).unwrap();
        let before = bytes(root.path());
        assert_eq!(
            store
                .get_project_source_read_only(&reference)
                .await
                .unwrap(),
            ProjectMemoryRead::Inactive { status }
        );
        assert_eq!(bytes(root.path()), before);
    }
}

#[tokio::test]
async fn inconsistent_derived_scope_metadata_is_rejected_without_returning_foreign_hit() {
    let root = tempdir().unwrap();
    let project = project("project-index");
    let store = MemoryStore::new(root.path()).for_project(&project);
    write(
        &store,
        &project,
        "Work08 index validation",
        "Work08 current evidence.",
    )
    .await;
    let index = store
        .resolver()
        .indexes_dir(MemoryScope::Project, Some(project.as_str()))
        .join("lexical.json");
    let original = std::fs::read(&index).unwrap();
    for (scope, key) in [("project", "private-project"), ("global", "project-index")] {
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        value["items"][0]["scope"] = serde_json::json!(scope);
        value["items"][0]["project_key"] = serde_json::json!(key);
        value["items"][0]["title"] = serde_json::json!("Work08 private title");
        std::fs::write(&index, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = bytes(root.path());
        let error = store
            .query_project_sources_read_only(&project, "Work08")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("private"));
        assert_eq!(bytes(root.path()), before);
    }
}

#[tokio::test]
async fn legacy_confidence_and_session_coverage_do_not_become_verified_claims() {
    let root = tempdir().unwrap();
    let project = project("project-legacy");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let mut document = store
        .write_memory_with_retrieval_and_source_range(
            MemoryScope::Project,
            Some(project.as_str()),
            DurableMemoryType::Feedback,
            "Work05 historical report",
            "A worker once claimed completion; check Ticket acceptance independently.",
            &[],
            &MemoryRetrievalInput::default(),
            Some("source-session"),
            "fixture",
            false,
            None,
            &["message-example".into()],
        )
        .await
        .unwrap();
    // Synthetic legacy fixture only, never a mutation to a real Jiandu root.
    document.frontmatter.confidence = Some("high".into());
    std::fs::write(
        &document.path,
        render_markdown_document(&document.frontmatter, &document.body).unwrap(),
    )
    .unwrap();
    let pinned = ProjectMemoryRef::from_document(&document).unwrap();
    let before = bytes(root.path());
    let result = store.get_project_source_read_only(&pinned).await.unwrap();
    assert!(matches!(
        result,
        ProjectMemoryRead::VersionMatched {
            confidence: None,
            ..
        }
    ));
    let wire = serde_json::to_value(&result).unwrap();
    for field in [
        "accepted",
        "work_state",
        "grant",
        "checkpoint",
        "message_range",
    ] {
        assert!(
            wire.get(field).is_none(),
            "unexpected authority field {field}"
        );
    }
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn selected_get_rejects_over_budget_body_instead_of_returning_truncated_evidence() {
    let root = tempdir().unwrap();
    let project = project("project-budget");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let document = write(
        &store,
        &project,
        "Work06 large evidence",
        &"界".repeat(MAX_MAX_CHARS + 1),
    )
    .await;
    let pinned = ProjectMemoryRef::from_document(&document).unwrap();
    let before = bytes(root.path());
    assert_eq!(
        store.get_project_source_read_only(&pinned).await.unwrap(),
        ProjectMemoryRead::OverBudget {
            required_chars: MAX_MAX_CHARS + 1,
            max_chars: MAX_MAX_CHARS
        }
    );
    assert_eq!(bytes(root.path()), before);
}

#[tokio::test]
async fn cold_reads_and_invalid_queries_create_no_root_or_scope() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("absent");
    let project = project("project-cold");
    let store = MemoryStore::new(&root).for_project(&project);
    let expected = ProjectMemoryRef::new(
        project.clone(),
        "missing-memory",
        format!("jiandu-content-v1:sha256:{}", "0".repeat(64)),
    )
    .unwrap();
    for query in ["", " \n\t", &"x".repeat(513)] {
        assert_eq!(
            store
                .query_project_sources_read_only(&project, query)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        store.get_project_source_read_only(&expected).await.unwrap(),
        ProjectMemoryRead::Missing
    );
    assert!(!root.exists());
}

#[tokio::test]
async fn reference_serialization_is_path_independent_and_validates_shape_without_authority_fields()
{
    let root = tempdir().unwrap();
    let project = project("project-reference");
    let store = MemoryStore::new(root.path()).for_project(&project);
    let mut document = write(
        &store,
        &project,
        "Work07 content reference",
        "Reference versions identify content, not event order.",
    )
    .await;
    let reference = ProjectMemoryRef::from_document(&document).unwrap();
    document.path = PathBuf::from("/different/root/is/not/identity");
    assert_eq!(
        reference,
        ProjectMemoryRef::from_document(&document).unwrap()
    );
    document.frontmatter.sources[0]
        .message_range
        .push("different-message".into());
    assert_ne!(
        reference,
        ProjectMemoryRef::from_document(&document).unwrap()
    );
    let wire = serde_json::to_value(&reference).unwrap();
    for (key, value) in [
        ("memory_id", serde_json::json!("../escape")),
        ("memory_id", serde_json::json!(" trailing-id ")),
        ("project_id", serde_json::json!("../other")),
        ("content_version", serde_json::json!("project:3")),
        (
            "content_version",
            serde_json::json!(format!("jiandu-content-v1:sha256:{}", "A".repeat(64))),
        ),
        ("grant", serde_json::json!("forged")),
    ] {
        let mut forged = wire.clone();
        forged[key] = value;
        assert!(
            serde_json::from_value::<ProjectMemoryRef>(forged).is_err(),
            "accepted forged {key}"
        );
    }
    let current: ProjectMemoryRef = serde_json::from_value(wire).unwrap();
    assert_eq!(current, reference);
    assert_eq!(current.project_id(), &project);
}

#[test]
fn v1_handoff_content_version_has_a_stable_cross_language_vector() {
    let document = DurableMemoryDocument {
        frontmatter: serde_json::from_value(serde_json::json!({
            "id": "memory-fixed",
            "title": "Fixed source",
            "type": "reference",
            "scope": "project",
            "project_key": "project-reference",
            "status": "active",
            "created_at": "2026-10-17T00:00:00Z",
            "updated_at": "2026-10-17T00:00:00Z",
            "created_by": {"kind": "session", "id": "session-fixed"},
            "updated_by": {"kind": "host"}
        }))
        .unwrap(),
        body: "Canonical Unicode 简牍.".into(),
        path: PathBuf::from("unused"),
    };
    // Independently computed using Python's UTF-8 compact JSON and SHA-256.
    // A semantic serialization change requires an intentional version decision.
    assert_eq!(
        ProjectMemoryRef::from_document(&document)
            .unwrap()
            .content_version(),
        "jiandu-content-v1:sha256:166110a6f0965aa587472a0201e50fab997de98bdd8f8b6d559a4dcf9f31caf5"
    );
}
