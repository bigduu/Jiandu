use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use jiandu_memory::memory_store::{
    DurableMemoryType, MemoryQueryOptions, MemoryScope, MemoryStore,
};
use tempfile::tempdir;

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

#[tokio::test]
async fn read_only_queries_preserve_bytes_and_do_not_disable_agent_access_signals() {
    let root = tempdir().unwrap();
    let store = MemoryStore::new(root.path());
    let doc = store
        .write_memory(
            MemoryScope::Global,
            None,
            DurableMemoryType::Reference,
            "简牍控制台检索",
            "中文项目知识与只读浏览",
            &[],
            None,
            "fixture",
            false,
            None,
        )
        .await
        .unwrap();
    let before = bytes(root.path());
    for query in [None, Some("控制台")] {
        let result = store
            .query_scope_read_only(
                MemoryScope::Global,
                None,
                query,
                None,
                None,
                None,
                &MemoryQueryOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items[0].id, doc.frontmatter.id);
        assert_eq!(bytes(root.path()), before);
    }
    let agent_result = store
        .query_scope(
            MemoryScope::Global,
            None,
            Some("控制台"),
            None,
            None,
            None,
            &MemoryQueryOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(agent_result.items[0].id, doc.frontmatter.id);
    assert_ne!(
        bytes(root.path()),
        before,
        "ordinary agent recall still writes its access signal"
    );
}

#[tokio::test]
async fn session_inventory_is_sorted_validated_and_does_not_create_cold_state() {
    let root = tempdir().unwrap();
    let missing = root.path().join("missing");
    let empty = MemoryStore::new(&missing);
    assert!(empty.list_session_ids().await.unwrap().is_empty());
    assert!(!missing.exists());
    let store = MemoryStore::new(root.path());
    for id in ["z-session", "a.session"] {
        store
            .write_session_topic(id, "default", "temporary continuity")
            .await
            .unwrap();
    }
    let sessions = store.resolver().sessions_root();
    std::fs::create_dir(sessions.join("empty-directory")).unwrap();
    std::fs::create_dir(sessions.join("invalid name")).unwrap();
    std::fs::write(sessions.join("regular-file"), "not a Session").unwrap();
    let before = bytes(root.path());
    assert_eq!(
        store.list_session_ids().await.unwrap(),
        ["a.session", "z-session"]
    );
    assert_eq!(bytes(root.path()), before);
}

#[cfg(unix)]
#[tokio::test]
async fn session_inventory_does_not_follow_linked_entries_or_ancestors() {
    use std::os::unix::fs::symlink;
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let store = MemoryStore::new(root.path());
    let other = MemoryStore::new(outside.path());
    store
        .write_session_topic("real", "default", "inside")
        .await
        .unwrap();
    other
        .write_session_topic("outside", "default", "outside")
        .await
        .unwrap();
    symlink(
        other.resolver().session_root("outside"),
        store.resolver().session_root("linked"),
    )
    .unwrap();
    assert_eq!(store.list_session_ids().await.unwrap(), ["real"]);
    let linked_root = tempdir().unwrap();
    symlink(
        root.path().join("memory"),
        linked_root.path().join("memory"),
    )
    .unwrap();
    assert!(
        MemoryStore::new(linked_root.path())
            .list_session_ids()
            .await
            .unwrap()
            .is_empty()
    );
}
