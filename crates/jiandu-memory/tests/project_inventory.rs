use jiandu_memory::ProjectId;
use jiandu_memory::memory_store::{
    DurableMemoryDocument, DurableMemoryType, MemoryScope, MemoryStore,
};
use tempfile::tempdir;

async fn write_memory(
    store: &MemoryStore,
    scope: MemoryScope,
    project_key: Option<&str>,
    title: &str,
) -> DurableMemoryDocument {
    store
        .write_memory(
            scope,
            project_key,
            DurableMemoryType::Reference,
            title,
            "Project inventory must preserve the existing memory layouts.",
            &[],
            None,
            "inventory-test",
            false,
            None,
        )
        .await
        .expect("write inventory fixture through MemoryStore")
}

#[tokio::test]
async fn project_inventory_is_empty_without_creating_a_store() {
    let directory = tempdir().expect("temporary data directory");
    for data_dir in [
        directory.path().to_path_buf(),
        directory.path().join("absent"),
    ] {
        let store = MemoryStore::new(&data_dir);
        assert!(store.list_project_ids().await.unwrap().is_empty());
        assert!(store.list_project_keys().await.unwrap().is_empty());
        assert_eq!(store.count_all_memories().await.unwrap(), 0);
        assert!(!data_dir.join("projects").exists());
        assert!(!data_dir.join("memory").exists());
    }
}

#[tokio::test]
async fn project_inventory_discovers_a_typed_write_after_reopen() {
    let directory = tempdir().expect("temporary data directory");
    let project_id = ProjectId::parse("Project_42").unwrap();
    let project = MemoryStore::new(directory.path()).for_project(&project_id);
    write_memory(
        &project,
        MemoryScope::Project,
        Some(project_id.as_str()),
        "Typed inventory record",
    )
    .await;

    let reopened = MemoryStore::new(directory.path());
    assert_eq!(
        reopened.list_project_ids().await.unwrap(),
        vec![project_id.clone()]
    );
    assert_eq!(
        reopened.list_project_keys().await.unwrap(),
        vec![project_id.to_string()]
    );
    assert_eq!(reopened.count_all_memories().await.unwrap(), 1);
}

#[tokio::test]
async fn project_inventory_includes_initialized_projects_without_topics() {
    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());
    let project_id = ProjectId::parse("empty-project").unwrap();
    store
        .for_project(&project_id)
        .rebuild_scope(MemoryScope::Project, Some(project_id.as_str()))
        .await
        .expect("initialize an empty Project through MemoryStore");

    assert_eq!(store.list_project_ids().await.unwrap(), vec![project_id]);
    assert_eq!(
        store.list_project_keys().await.unwrap(),
        vec!["empty-project".to_string()]
    );
    assert_eq!(store.count_all_memories().await.unwrap(), 0);
}

#[tokio::test]
async fn project_inventory_sorts_and_deduplicates_both_layouts_without_migration() {
    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());
    for key in ["zeta", "shared", "alpha"] {
        let project_id = ProjectId::parse(key).unwrap();
        write_memory(
            &store.for_project(&project_id),
            MemoryScope::Project,
            Some(key),
            "First-class inventory record",
        )
        .await;
    }
    let legacy = write_memory(
        &store,
        MemoryScope::Project,
        Some("legacy-only"),
        "Legacy-only inventory record",
    )
    .await;
    let duplicate_legacy = write_memory(
        &store,
        MemoryScope::Project,
        Some("shared"),
        "Separate legacy record for a shared id",
    )
    .await;
    write_memory(&store, MemoryScope::Global, None, "Global inventory record").await;

    for _ in 0..3 {
        assert_eq!(
            store.list_project_ids().await.unwrap(),
            ["alpha", "shared", "zeta"]
                .map(|key| ProjectId::parse(key).unwrap())
                .to_vec()
        );
        assert_eq!(
            store.list_project_keys().await.unwrap(),
            ["alpha", "legacy-only", "shared", "zeta"].map(str::to_string)
        );
    }
    // A deduplicated catalogue still counts both distinct physical scopes.
    assert_eq!(store.count_all_memories().await.unwrap(), 6);
    for document in [legacy, duplicate_legacy] {
        let unchanged = store
            .get_memory(
                &document.frontmatter.id,
                document.frontmatter.project_key.as_deref(),
            )
            .await
            .unwrap()
            .expect("legacy record remains in its original scope");
        assert_eq!(unchanged.path, document.path);
        assert_eq!(unchanged.body, document.body);
    }
}

#[tokio::test]
async fn project_inventory_and_counts_remain_bound_to_the_explicit_project() {
    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());
    write_memory(&store, MemoryScope::Global, None, "Global inventory record").await;
    write_memory(
        &store,
        MemoryScope::Project,
        Some("bound-project"),
        "Legacy record with the bound id",
    )
    .await;
    let unrelated_id = ProjectId::parse("unrelated-project").unwrap();
    write_memory(
        &store.for_project(&unrelated_id),
        MemoryScope::Project,
        Some(unrelated_id.as_str()),
        "Unrelated typed record",
    )
    .await;

    let bound_id = ProjectId::parse("bound-project").unwrap();
    let bound = store.for_project(&bound_id);
    assert_eq!(
        bound.list_project_ids().await.unwrap(),
        vec![bound_id.clone()]
    );
    assert_eq!(
        bound.list_project_keys().await.unwrap(),
        vec![bound_id.to_string()]
    );
    assert_eq!(bound.count_all_memories().await.unwrap(), 1);

    write_memory(
        &bound,
        MemoryScope::Project,
        Some(bound_id.as_str()),
        "Bound typed record",
    )
    .await;
    assert_eq!(
        bound.list_project_ids().await.unwrap(),
        vec![bound_id.clone()]
    );
    assert_eq!(
        bound.list_project_keys().await.unwrap(),
        vec![bound_id.to_string()]
    );
    assert_eq!(bound.count_all_memories().await.unwrap(), 2);
    assert_eq!(store.count_all_memories().await.unwrap(), 4);
}

#[tokio::test]
async fn project_inventory_ignores_invalid_names_files_and_incomplete_layouts() {
    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());
    let valid_id = ProjectId::parse("valid-typed").unwrap();
    write_memory(
        &store.for_project(&valid_id),
        MemoryScope::Project,
        Some(valid_id.as_str()),
        "Valid typed record",
    )
    .await;
    write_memory(
        &store,
        MemoryScope::Project,
        Some("valid-legacy"),
        "Valid legacy record",
    )
    .await;

    let projects = directory.path().join("projects");
    let legacy = store.resolver().scopes_root().join("projects");
    for invalid in ["with space", "invalid.id", "项目", &"x".repeat(65)] {
        std::fs::create_dir_all(projects.join(invalid).join("memory/v1")).unwrap();
        std::fs::create_dir_all(legacy.join(invalid)).unwrap();
    }
    std::fs::write(projects.join("regular-file"), "not a Project directory").unwrap();
    std::fs::write(legacy.join("regular-file"), "not a legacy scope").unwrap();
    std::fs::create_dir(projects.join("without-memory")).unwrap();
    std::fs::create_dir_all(projects.join("without-v1/memory")).unwrap();
    std::fs::create_dir(projects.join("memory-is-file")).unwrap();
    std::fs::write(projects.join("memory-is-file/memory"), "not a directory").unwrap();
    std::fs::create_dir_all(projects.join("v1-is-file/memory")).unwrap();
    std::fs::write(projects.join("v1-is-file/memory/v1"), "not a directory").unwrap();

    assert_eq!(store.list_project_ids().await.unwrap(), vec![valid_id]);
    assert_eq!(
        store.list_project_keys().await.unwrap(),
        ["valid-legacy", "valid-typed"].map(str::to_string)
    );
    assert_eq!(store.count_all_memories().await.unwrap(), 2);
}

// macOS filesystems reject this fixture before directory discovery can run.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn project_inventory_ignores_non_utf8_directory_names() {
    use std::os::unix::ffi::OsStringExt;

    let directory = tempdir().expect("temporary data directory");
    let store = MemoryStore::new(directory.path());
    let invalid = std::ffi::OsString::from_vec(vec![0xff]);
    std::fs::create_dir_all(
        directory
            .path()
            .join("projects")
            .join(&invalid)
            .join("memory/v1"),
    )
    .unwrap();
    std::fs::create_dir_all(
        store
            .resolver()
            .scopes_root()
            .join("projects")
            .join(invalid),
    )
    .unwrap();

    assert!(store.list_project_ids().await.unwrap().is_empty());
    assert!(store.list_project_keys().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn project_inventory_ignores_symlinked_entries_and_nested_memory_directories() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary data directory");
    let outside = tempdir().expect("isolated symlink target");
    let store = MemoryStore::new(directory.path());
    let target_id = ProjectId::parse("target").unwrap();
    let target = MemoryStore::new(outside.path()).for_project(&target_id);
    write_memory(
        &target,
        MemoryScope::Project,
        Some("target"),
        "Target record",
    )
    .await;
    let target_home = outside.path().join("projects/target");
    let projects = directory.path().join("projects");
    let legacy = store.resolver().scopes_root().join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::create_dir_all(&legacy).unwrap();
    symlink(&target_home, projects.join("linked-project")).unwrap();
    symlink(outside.path().join("absent"), projects.join("broken-link")).unwrap();
    std::fs::create_dir(projects.join("linked-memory")).unwrap();
    symlink(
        target_home.join("memory"),
        projects.join("linked-memory/memory"),
    )
    .unwrap();
    std::fs::create_dir_all(projects.join("linked-v1/memory")).unwrap();
    symlink(
        target_home.join("memory/v1"),
        projects.join("linked-v1/memory/v1"),
    )
    .unwrap();
    symlink(target_home.join("memory/v1"), legacy.join("linked-legacy")).unwrap();

    assert!(store.list_project_ids().await.unwrap().is_empty());
    assert!(store.list_project_keys().await.unwrap().is_empty());
    assert_eq!(store.count_all_memories().await.unwrap(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn project_inventory_does_not_traverse_symlinked_catalogue_ancestors() {
    use std::os::unix::fs::symlink;

    let outside = tempdir().expect("isolated symlink target");
    let target = MemoryStore::new(outside.path());
    let target_id = ProjectId::parse("target").unwrap();
    write_memory(
        &target.for_project(&target_id),
        MemoryScope::Project,
        Some("target"),
        "Typed target record",
    )
    .await;
    write_memory(
        &target,
        MemoryScope::Project,
        Some("target"),
        "Legacy target record",
    )
    .await;

    for relative in [
        "projects",
        "memory",
        "memory/v1",
        "memory/v1/scopes",
        "memory/v1/scopes/projects",
    ] {
        let directory = tempdir().expect("temporary data directory");
        let link = directory.path().join(relative);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(outside.path().join(relative), &link).unwrap();
        let store = MemoryStore::new(directory.path());
        assert!(
            store.list_project_ids().await.unwrap().is_empty(),
            "{relative}"
        );
        assert!(
            store.list_project_keys().await.unwrap().is_empty(),
            "{relative}"
        );
    }
}
