use super::*;
use crate::document::{decode_canonical_document, encode_canonical_document};
use crate::layout;
use jiandu_core::{
    CreationActor, FrontmatterProvenance, FrontmatterScope, ListSort, MemoryFrontmatterV1Alpha1,
    MemoryId, MemoryListRequest, MemorySchema, MemoryScope, MemoryStatus, MemoryType, PageCursor,
    PageLimit, PrincipalId, ProjectId, Revision, ScopeSelector, SessionId, StoreRevision, Tag,
    Timestamp,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

fn owner() -> LockOwner {
    LockOwner::for_current_process().expect("current process has a valid lock identity")
}

fn memory_id(value: &str) -> MemoryId {
    MemoryId::new(value).expect("test memory ID is valid")
}

fn principal_id(value: &str) -> PrincipalId {
    PrincipalId::new(value).expect("test principal ID is valid")
}

fn project_id(value: &str) -> ProjectId {
    ProjectId::new(value).expect("test project ID is valid")
}

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).expect("test session ID is valid")
}

fn timestamp(value: &str) -> Timestamp {
    Timestamp::new(value).expect("test timestamp is valid")
}

fn frontmatter(
    id: &str,
    scope: MemoryScope,
    created_at: &str,
    updated_at: &str,
) -> MemoryFrontmatterV1Alpha1 {
    MemoryFrontmatterV1Alpha1 {
        schema: MemorySchema::V1Alpha1,
        id: memory_id(id),
        revision: Revision::new(1).expect("positive revision"),
        scope: FrontmatterScope::from(&scope),
        memory_type: MemoryType::Decision,
        status: MemoryStatus::Active,
        title: format!("Title for {id}"),
        summary: Some(format!("Summary for {id}")),
        tags: vec![Tag::new("architecture").expect("valid tag")],
        created_at: timestamp(created_at),
        updated_at: timestamp(updated_at),
        provenance: FrontmatterProvenance {
            created_by: CreationActor::Host,
            agent_id: None,
            session_id: None,
            branch_id: None,
            message_ids: Vec::new(),
            message_range: None,
            source_uri: None,
            content_digest: None,
            extraction: None,
            confidence: None,
        },
        relations: Vec::new(),
    }
}

fn write_record(root: &Path, frontmatter: &MemoryFrontmatterV1Alpha1, body: &str) -> PathBuf {
    let scope: MemoryScope = frontmatter.scope.clone().into();
    let path = layout::record_path(root, &scope, &frontmatter.id).expect("safe record path");
    fs::create_dir_all(path.parent().expect("record has parent")).expect("create record shard");
    let bytes = encode_canonical_document(frontmatter, body).expect("canonical record");
    fs::write(&path, bytes).expect("write test record");
    path
}

fn write_raw_record(root: &Path, scope: &MemoryScope, id: &MemoryId, bytes: &[u8]) -> PathBuf {
    let path = layout::record_path(root, scope, id).expect("safe record path");
    fs::create_dir_all(path.parent().expect("record has parent")).expect("create record shard");
    fs::write(&path, bytes).expect("write raw test record");
    path
}

fn list_request(scopes: Vec<ScopeSelector>, limit: u16) -> MemoryListRequest {
    MemoryListRequest {
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        sort: ListSort::UpdatedAtDesc,
        limit: PageLimit::new(limit).expect("valid page limit"),
        cursor: None,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotEntry {
    bytes: Option<Vec<u8>>,
    modified: SystemTime,
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .expect("read snapshot directory")
            .map(|entry| entry.expect("read snapshot entry"))
            .collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path).expect("snapshot metadata");
            let relative = entry_path
                .strip_prefix(root)
                .expect("snapshot entry is contained")
                .to_path_buf();
            let bytes = metadata
                .is_file()
                .then(|| fs::read(&entry_path).expect("snapshot file"));
            output.insert(
                relative,
                SnapshotEntry {
                    bytes,
                    modified: metadata.modified().expect("snapshot modification time"),
                },
            );
            if metadata.is_dir() {
                visit(root, &entry_path, output);
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}

#[test]
fn initialization_is_deterministic_and_lock_diagnostics_are_secret_safe() {
    let directory = TempDir::new().expect("temporary directory");
    let first_owner = owner();
    let store = CanonicalStore::initialize(directory.path(), first_owner.clone())
        .expect("initialize store");
    let store_debug = format!("{store:?}");
    assert!(!store_debug.contains(&directory.path().display().to_string()));
    assert!(!store_debug.contains("data_dir"));

    let metadata_bytes = fs::read(directory.path().join("store.json")).expect("read metadata");
    assert!(metadata_bytes.ends_with(b"\n"));
    let metadata: StoreMetadata = serde_json::from_slice(&metadata_bytes).expect("strict metadata");
    assert_eq!(
        metadata.canonical_bytes().expect("canonical metadata"),
        metadata_bytes
    );
    assert_eq!(metadata.format_version, STORE_FORMAT_VERSION);
    assert_eq!(metadata.store_revision, StoreRevision(0));
    assert_eq!(metadata.store_id, *store.store_id());
    for expected in [
        "records/principal",
        "records/project",
        "records/session",
        "records/instance_global",
        "lineages",
        "tombstones",
        "transactions",
        "receipts",
        "audit",
        "index",
        "quarantine",
        "backups",
    ] {
        assert!(
            directory.path().join(expected).is_dir(),
            "missing {expected}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(directory.path())
                .expect("data-directory metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        for private_file in ["store.json", "LOCK"] {
            assert_eq!(
                fs::metadata(directory.path().join(private_file))
                    .expect("private control-file metadata")
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(directory.path().join("records/project"))
                .expect("private layout metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
    }

    let error =
        CanonicalStore::open(directory.path(), owner()).expect_err("lock must be exclusive");
    match error {
        StoreError::StoreLocked { owner: diagnostics } => {
            assert_eq!(diagnostics, Some(first_owner));
        }
        other => panic!("unexpected error: {other}"),
    }

    #[cfg(unix)]
    let lock_inode = {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(directory.path().join("LOCK"))
            .expect("lock metadata before reopen")
            .ino()
    };

    drop(store);
    assert!(directory.path().join("LOCK").is_file());
    let reopened =
        CanonicalStore::open(directory.path(), owner()).expect("lock is released on drop");
    drop(reopened);
    assert!(directory.path().join("LOCK").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            fs::metadata(directory.path().join("LOCK"))
                .expect("lock metadata after reopen")
                .ino(),
            lock_inode
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("make data directory insecure");
        let before = tree_snapshot(directory.path());
        assert_eq!(
            CanonicalStore::open(directory.path(), owner())
                .expect_err("insecure data-directory permissions fail closed")
                .code(),
            StoreErrorCode::InvalidDataDirectory
        );
        assert_eq!(tree_snapshot(directory.path()), before);
    }
}

#[test]
fn initialization_rejects_foreign_data_and_recovers_or_rolls_back_its_own_marker() {
    let foreign = TempDir::new().expect("temporary directory");
    fs::write(
        foreign.path().join("unrelated.txt"),
        b"belongs to the operator\n",
    )
    .expect("write unrelated file");
    let before = tree_snapshot(foreign.path());
    assert_eq!(
        CanonicalStore::initialize(foreign.path(), owner())
            .expect_err("a non-empty foreign directory must not be claimed")
            .code(),
        StoreErrorCode::InvalidDataDirectory
    );
    assert_eq!(tree_snapshot(foreign.path()), before);

    let foreign_lock = TempDir::new().expect("temporary directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(foreign_lock.path(), fs::Permissions::from_mode(0o755))
            .expect("set foreign directory permissions");
    }
    fs::write(
        foreign_lock.path().join("LOCK"),
        b"not a Jiandu initialization marker\n",
    )
    .expect("write unrelated lock file");
    let before = tree_snapshot(foreign_lock.path());
    assert_eq!(
        CanonicalStore::initialize(foreign_lock.path(), owner())
            .expect_err("an unrelated LOCK file must not be overwritten")
            .code(),
        StoreErrorCode::InvalidDataDirectory
    );
    assert_eq!(tree_snapshot(foreign_lock.path()), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(foreign_lock.path())
                .expect("foreign root metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }

    let resumable = TempDir::new().expect("temporary directory");
    fs::write(resumable.path().join("LOCK"), b"").expect("write initialization marker");
    fs::create_dir(resumable.path().join("records")).expect("create partial layout");
    let pending = StoreMetadata::new().expect("create pending metadata");
    fs::write(
        resumable.path().join(layout::STORE_METADATA_INIT_FILE),
        pending
            .canonical_bytes()
            .expect("canonical pending metadata"),
    )
    .expect("write pending metadata");
    let recovered = CanonicalStore::initialize(resumable.path(), owner())
        .expect("resume unambiguous initialization");
    assert_eq!(recovered.store_id(), &pending.store_id);
    assert!(
        !resumable
            .path()
            .join(layout::STORE_METADATA_INIT_FILE)
            .exists()
    );
    assert!(resumable.path().join("records/project").is_dir());

    let rolled_back = TempDir::new().expect("temporary directory");
    fs::write(rolled_back.path().join("LOCK"), b"").expect("write initialization marker");
    fs::write(
        rolled_back.path().join(layout::STORE_METADATA_INIT_FILE),
        b"{\"formatVersion\":",
    )
    .expect("write truncated pending metadata");
    let initialized = CanonicalStore::initialize(rolled_back.path(), owner())
        .expect("roll back an uncommitted truncated metadata file");
    assert_eq!(initialized.watermark(), StoreRevision(0));
    assert!(
        !rolled_back
            .path()
            .join(layout::STORE_METADATA_INIT_FILE)
            .exists()
    );
    assert!(rolled_back.path().join("store.json").is_file());
}

#[test]
fn supported_open_preserves_canonical_record_bytes_and_mtime() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_preserved"),
    };
    let header = frontmatter(
        "mem_preserved",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:05:00Z",
    );
    let path = write_record(directory.path(), &header, " exact body \n");
    let before_bytes = fs::read(&path).expect("read before open");
    let before_mtime = fs::metadata(&path)
        .expect("metadata before open")
        .modified()
        .expect("mtime before open");
    drop(store);

    let reopened = CanonicalStore::open(directory.path(), owner()).expect("open supported store");
    assert_eq!(fs::read(&path).expect("read after open"), before_bytes);
    assert_eq!(
        fs::metadata(&path)
            .expect("metadata after open")
            .modified()
            .expect("mtime after open"),
        before_mtime
    );
    let read = reopened
        .get(
            &header.id,
            &AuthorizedScopes::new(principal_id("prn_reader"))
                .with_project(project_id("prj_preserved")),
        )
        .expect("read preserved record");
    assert_eq!(read.result.body, " exact body \n");
}

#[test]
fn future_store_format_fails_before_any_directory_mutation() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    drop(store);
    let metadata_path = directory.path().join("store.json");
    let metadata = fs::read_to_string(&metadata_path)
        .expect("metadata")
        .replace(STORE_FORMAT_VERSION, "jiandu.store/v99");
    fs::write(&metadata_path, metadata).expect("write future metadata");
    let before = tree_snapshot(directory.path());

    let error = CanonicalStore::open(directory.path(), owner()).expect_err("future format fails");
    assert_eq!(error.code(), StoreErrorCode::UnsupportedStoreFormat);
    assert_eq!(tree_snapshot(directory.path()), before);
}

#[test]
fn canonical_document_fixture_round_trips_body_and_stable_etag() {
    let bytes = include_bytes!("../fixtures/v1alpha1/valid/project-memory.md");
    let decoded = decode_canonical_document(bytes, None).expect("valid canonical fixture");
    assert_eq!(
        decoded.record.body,
        "Workspace paths remain mutable metadata and never become identity.\n"
    );
    let header = MemoryFrontmatterV1Alpha1::from_record(&decoded.record);
    let encoded = encode_canonical_document(&header, &decoded.record.body).expect("re-encode");
    assert_eq!(encoded, bytes);
    let second = decode_canonical_document(&encoded, None).expect("decode again");
    assert_eq!(decoded.record.etag, second.record.etag);

    let changed = encode_canonical_document(&header, "different body").expect("changed record");
    let changed = decode_canonical_document(&changed, None).expect("decode changed record");
    assert_ne!(decoded.record.etag, changed.record.etag);

    let mut new_revision = header.clone();
    new_revision.revision = Revision::new(8).expect("positive revision");
    let new_revision = encode_canonical_document(&new_revision, &decoded.record.body)
        .expect("new revision record");
    let new_revision =
        decode_canonical_document(&new_revision, None).expect("decode new revision record");
    assert_ne!(decoded.record.etag, new_revision.record.etag);

    let mut new_id = header;
    new_id.id = memory_id("mem_01K3OTHERIDENTITY");
    let new_id = encode_canonical_document(&new_id, &decoded.record.body).expect("new ID record");
    let new_id = decode_canonical_document(&new_id, None).expect("decode new ID record");
    assert_ne!(decoded.record.etag, new_id.record.etag);
}

#[test]
fn canonical_document_preserves_markdown_and_rejects_unsafe_encodings() {
    let header = frontmatter(
        "mem_exact_body",
        MemoryScope::Principal {
            principal_id: principal_id("prn_exact"),
        },
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let body = "  leading and trailing  \n";
    let bytes = encode_canonical_document(&header, body).expect("canonical body");
    assert!(bytes.ends_with(b"\n\n"));
    assert_eq!(
        decode_canonical_document(&bytes, None)
            .expect("decode exact body")
            .record
            .body,
        body
    );

    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(&bytes);
    assert_invalid_reason(&bom, InvalidRecordReason::NonCanonicalEncoding);

    let crlf = String::from_utf8(bytes.clone())
        .expect("utf8 fixture")
        .replace('\n', "\r\n");
    assert_invalid_reason(crlf.as_bytes(), InvalidRecordReason::NonCanonicalEncoding);

    let no_body_newline = encode_canonical_document(&header, "body").expect("canonical record");
    assert_invalid_reason(
        &no_body_newline[..no_body_newline.len() - 1],
        InvalidRecordReason::Truncated,
    );
    assert_invalid_reason(
        include_bytes!("../fixtures/v1alpha1/invalid/malformed-frontmatter.md"),
        InvalidRecordReason::MalformedFrontmatter,
    );
    assert_invalid_reason(
        include_bytes!("../fixtures/v1alpha1/invalid/truncated.md"),
        InvalidRecordReason::Truncated,
    );
    assert_invalid_reason(&[0xff, b'\n'], InvalidRecordReason::InvalidUtf8);

    let markdown_body = "first section\n---\nsecond section";
    let markdown = encode_canonical_document(&header, markdown_body)
        .expect("Markdown horizontal rules are valid body content");
    assert_eq!(
        decode_canonical_document(&markdown, None)
            .expect("decode Markdown body")
            .record
            .body,
        markdown_body
    );

    let unknown_etag = encode_canonical_document(&header, "body")
        .expect("canonical record")
        .split(|byte| *byte == b'\n')
        .enumerate()
        .flat_map(|(index, line)| {
            let mut output = line.to_vec();
            output.push(b'\n');
            if index == 2 {
                output.extend_from_slice(b"etag: injected\n");
            }
            output
        })
        .collect::<Vec<_>>();
    assert_invalid_reason(&unknown_etag, InvalidRecordReason::MalformedFrontmatter);

    let canonical =
        String::from_utf8(encode_canonical_document(&header, "body").expect("canonical record"))
            .expect("UTF-8 record");
    let noncanonical_yaml = canonical.replace(
        "title: Title for mem_exact_body",
        "title: 'Title for mem_exact_body'",
    );
    assert_invalid_reason(
        noncanonical_yaml.as_bytes(),
        InvalidRecordReason::NonCanonicalEncoding,
    );

    let oversized = "x".repeat(jiandu_core::MAX_BODY_BYTES + 1);
    assert_eq!(
        encode_canonical_document(&header, &oversized)
            .expect_err("body bound is enforced")
            .code(),
        StoreErrorCode::InvalidRecord
    );
    let oversized_file = vec![b'x'; crate::document::MAX_CANONICAL_DOCUMENT_BYTES + 1];
    assert_invalid_reason(&oversized_file, InvalidRecordReason::ValidationFailed);
}

fn assert_invalid_reason(bytes: &[u8], expected: InvalidRecordReason) {
    match decode_canonical_document(bytes, None).expect_err("fixture must be invalid") {
        StoreError::InvalidRecord { reason, .. } => assert_eq!(reason, expected),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn paths_reject_traversal_absolute_inputs_and_symlinked_store_roots() {
    let directory = TempDir::new().expect("temporary directory");
    let root = fs::canonicalize(directory.path()).expect("canonical temporary directory");
    assert_eq!(
        layout::safe_join(&root, Path::new("../escape"))
            .expect_err("parent traversal is rejected")
            .code(),
        StoreErrorCode::UnsafePath
    );
    assert_eq!(
        layout::safe_join(&root, Path::new("/absolute/escape"))
            .expect_err("absolute record path is rejected")
            .code(),
        StoreErrorCode::UnsafePath
    );
    assert_eq!(
        CanonicalStore::initialize(root.join("missing").join("..").join("store"), owner())
            .expect_err("parent traversal in data directory is rejected")
            .code(),
        StoreErrorCode::InvalidDataDirectory
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let real = root.join("real-store");
        fs::create_dir(&real).expect("create real directory");
        let linked = root.join("linked-store");
        symlink(&real, &linked).expect("create data-directory symlink");
        assert_eq!(
            CanonicalStore::initialize(&linked, owner())
                .expect_err("symlinked data directory is rejected")
                .code(),
            StoreErrorCode::InvalidDataDirectory
        );
    }
}

#[cfg(unix)]
#[test]
fn initialization_rejects_intermediate_symlinks_without_touching_the_target() {
    use std::os::unix::fs::symlink;

    let parent = TempDir::new().expect("temporary parent");
    let outside = TempDir::new().expect("outside directory");
    fs::write(
        outside.path().join("operator.txt"),
        b"outside remains unchanged\n",
    )
    .expect("write outside marker");
    let before = tree_snapshot(outside.path());
    let linked_parent = parent.path().join("linked-parent");
    symlink(outside.path(), &linked_parent).expect("create intermediate directory symlink");

    let error = CanonicalStore::initialize(linked_parent.join("new-store"), owner())
        .expect_err("an intermediate data-directory symlink is rejected");
    assert_eq!(error.code(), StoreErrorCode::InvalidDataDirectory);
    assert_eq!(tree_snapshot(outside.path()), before);
    assert!(!outside.path().join("new-store").exists());
}

#[cfg(unix)]
#[test]
fn initialization_rejects_a_hard_linked_lock_without_mutating_the_external_inode() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempDir::new().expect("temporary store directory");
    let outside = TempDir::new().expect("outside directory");
    let external_lock = outside.path().join("external-lock");
    fs::write(&external_lock, b"").expect("create external lock inode");
    fs::set_permissions(&external_lock, fs::Permissions::from_mode(0o640))
        .expect("set distinctive external mode");
    fs::hard_link(&external_lock, directory.path().join("LOCK"))
        .expect("install hard-linked initialization marker");
    let before = fs::metadata(&external_lock).expect("external metadata");

    let error = CanonicalStore::initialize(directory.path(), owner())
        .expect_err("a multi-link LOCK inode is rejected");
    assert_eq!(error.code(), StoreErrorCode::UnsafePath);
    assert_eq!(fs::read(&external_lock).expect("external bytes"), b"");
    let after = fs::metadata(&external_lock).expect("external metadata after rejection");
    assert_eq!(after.permissions().mode() & 0o7777, 0o640);
    assert_eq!(
        after.modified().expect("external mtime"),
        before.modified().expect("original mtime")
    );
    assert_eq!(after.len(), 0);
}

#[cfg(unix)]
#[test]
fn lock_check_open_race_rejects_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let directory = TempDir::new().expect("temporary store directory");
    drop(CanonicalStore::initialize(directory.path(), owner()).expect("initialize store"));
    let outside = TempDir::new().expect("outside directory");
    let outside_file = outside.path().join("outside-lock");
    fs::write(&outside_file, b"outside lock bytes\n").expect("write outside lock");
    fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o640))
        .expect("set outside permissions");
    let before = fs::read(&outside_file).expect("outside bytes before race");
    let lock_path = directory.path().join("LOCK");
    let saved_lock = directory.path().join("LOCK.saved");
    let hook_lock_path = lock_path.clone();
    let hook_saved_lock = saved_lock.clone();
    let hook_outside = outside_file.clone();
    layout::install_test_hook(layout::TestHookPoint::RegularOpen, "LOCK", move || {
        fs::rename(&hook_lock_path, &hook_saved_lock).expect("move checked lock inode");
        symlink(&hook_outside, &hook_lock_path).expect("replace LOCK with symlink");
    });

    let error = CanonicalStore::open(directory.path(), owner())
        .expect_err("no-follow LOCK open rejects the replacement");
    assert_eq!(error.code(), StoreErrorCode::UnsafePath);
    assert_eq!(
        fs::read(&outside_file).expect("outside bytes after race"),
        before
    );
    assert_eq!(
        fs::metadata(&outside_file)
            .expect("outside metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert!(saved_lock.is_file());
}

#[cfg(unix)]
#[test]
fn replacing_lock_cannot_create_a_second_owner_in_the_same_root() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempDir::new().expect("temporary store directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    fs::rename(
        directory.path().join("LOCK"),
        directory.path().join("LOCK.original"),
    )
    .expect("move held lock name");
    fs::write(directory.path().join("LOCK"), b"").expect("create replacement lock");
    fs::set_permissions(
        directory.path().join("LOCK"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("set replacement lock mode");

    let second = CanonicalStore::open(directory.path(), owner())
        .expect_err("the held root inode lock blocks a replacement LOCK owner");
    assert_eq!(second.code(), StoreErrorCode::StoreLocked);
    let first = store
        .get(
            &memory_id("mem_absent"),
            &AuthorizedScopes::new(principal_id("prn_reader")),
        )
        .expect_err("the original owner fails closed after LOCK replacement");
    assert_eq!(first.code(), StoreErrorCode::UnsafePath);
}

#[cfg(unix)]
#[test]
fn root_replacement_invalidates_the_held_store_and_cannot_be_followed_as_a_link() {
    use std::os::unix::fs::symlink;

    let parent = TempDir::new().expect("temporary parent");
    let root = parent.path().join("store");
    let moved = parent.path().join("store.moved");
    let store = CanonicalStore::initialize(&root, owner()).expect("initialize store");
    fs::rename(&root, &moved).expect("move the opened root inode");
    symlink(&moved, &root).expect("replace configured root with symlink");

    let existing = store
        .get(
            &memory_id("mem_absent"),
            &AuthorizedScopes::new(principal_id("prn_reader")),
        )
        .expect_err("existing handle notices ambient root replacement");
    assert_eq!(existing.code(), StoreErrorCode::UnsafePath);
    let second = CanonicalStore::open(&root, owner())
        .expect_err("a second owner cannot follow the replacement root link");
    assert_eq!(second.code(), StoreErrorCode::InvalidDataDirectory);
}

#[cfg(unix)]
#[test]
fn record_open_race_never_follows_a_symlink_or_blocks_on_a_fifo() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("temporary store directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_record_race"),
    };
    let header = frontmatter(
        "mem_record_race",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let record_path = write_record(directory.path(), &header, "inside body");
    let outside = TempDir::new().expect("outside directory");
    let outside_file = outside.path().join("outside.md");
    fs::write(&outside_file, b"outside must never be read\n").expect("write outside file");
    let outside_before = fs::read(&outside_file).expect("outside bytes before race");
    let saved = record_path.with_extension("saved");
    let hook_record = record_path.clone();
    let hook_saved = saved.clone();
    let hook_outside = outside_file.clone();
    layout::install_test_hook(
        layout::TestHookPoint::RegularOpen,
        layout::record_file_name(&header.id),
        move || {
            fs::rename(&hook_record, &hook_saved).expect("move checked record");
            symlink(&hook_outside, &hook_record).expect("replace record with symlink");
        },
    );
    let authorized = AuthorizedScopes::new(principal_id("prn_reader"))
        .with_project(project_id("prj_record_race"));
    let error = store
        .get(&header.id, &authorized)
        .expect_err("record no-follow open rejects a raced symlink");
    assert_eq!(error.code(), StoreErrorCode::UnsafePath);
    assert_eq!(
        fs::read(&outside_file).expect("outside bytes after race"),
        outside_before
    );

    fs::remove_file(&record_path).expect("remove raced symlink");
    fs::rename(&saved, &record_path).expect("restore record for FIFO race");
    let fifo_record = record_path.clone();
    let fifo_saved = saved.clone();
    layout::install_test_hook(
        layout::TestHookPoint::RegularOpen,
        layout::record_file_name(&header.id),
        move || {
            fs::rename(&fifo_record, &fifo_saved).expect("move checked record for FIFO race");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_record)
                .status()
                .expect("run mkfifo");
            assert!(status.success(), "mkfifo failed");
        },
    );
    let error = store
        .get(&header.id, &authorized)
        .expect_err("nonblocking open rejects a raced FIFO");
    assert_eq!(error.code(), StoreErrorCode::InvalidLayout);
}

#[cfg(unix)]
#[test]
fn list_scope_read_dir_race_stays_on_the_opened_directory_inode() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("temporary store directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_scope_race"),
    };
    let header = frontmatter(
        "mem_scope_race",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    write_record(directory.path(), &header, "inside body");
    let scope_path = layout::scope_directory(directory.path(), &scope).expect("scope path");
    let scope_name = scope_path
        .file_name()
        .expect("scope storage key")
        .to_os_string();
    let moved_scope = scope_path.with_extension("moved");
    let outside = TempDir::new().expect("outside directory");
    fs::write(
        outside.path().join("sentinel"),
        b"outside remains unchanged\n",
    )
    .expect("write outside sentinel");
    let outside_before = tree_snapshot(outside.path());
    let hook_scope = scope_path.clone();
    let hook_moved = moved_scope.clone();
    let hook_outside = outside.path().to_path_buf();
    layout::install_test_hook(
        layout::TestHookPoint::DirectoryEntries,
        scope_name,
        move || {
            fs::rename(&hook_scope, &hook_moved).expect("move checked scope directory");
            symlink(&hook_outside, &hook_scope).expect("replace scope with outside symlink");
        },
    );
    let authorized = AuthorizedScopes::new(principal_id("prn_reader"))
        .with_project(project_id("prj_scope_race"));
    let request = list_request(
        vec![ScopeSelector::Project {
            project_id: project_id("prj_scope_race"),
        }],
        10,
    );
    let page = store
        .list(&request, &authorized)
        .expect("read_dir stays on the opened original scope directory");
    assert_eq!(page.result.memories.len(), 1);
    assert_eq!(page.result.memories[0].id, header.id);
    assert_eq!(tree_snapshot(outside.path()), outside_before);
}

#[cfg(unix)]
#[test]
fn quarantine_move_is_bound_to_the_invalid_inode_that_was_validated() {
    let directory = TempDir::new().expect("temporary store directory");
    let mut store =
        CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Session {
        session_id: session_id("ses_quarantine_race"),
    };
    let id = memory_id("mem_quarantine_race");
    let original = include_bytes!("../fixtures/v1alpha1/invalid/malformed-frontmatter.md");
    let replacement = include_bytes!("../fixtures/v1alpha1/invalid/truncated.md");
    let record_path = write_raw_record(directory.path(), &scope, &id, original);
    let saved = record_path.with_extension("validated");
    let hook_record = record_path.clone();
    let hook_saved = saved.clone();
    layout::install_test_hook(
        layout::TestHookPoint::Rename,
        layout::record_file_name(&id),
        move || {
            fs::rename(&hook_record, &hook_saved).expect("move validated source inode");
            fs::write(&hook_record, replacement).expect("install replacement source inode");
        },
    );

    let error = store
        .quarantine_invalid(&scope, &id)
        .expect_err("a replacement inode is not accepted as the validated source");
    assert_eq!(error.code(), StoreErrorCode::UnsafePath);
    assert_eq!(
        fs::read(&record_path).expect("replacement restored"),
        replacement
    );
    assert_eq!(
        fs::read(&saved).expect("validated inode retained"),
        original
    );
    assert_eq!(
        fs::read_dir(directory.path().join("quarantine"))
            .expect("quarantine directory")
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn exact_read_rejects_a_symlinked_record_without_following_it() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_symlink"),
    };
    let header = frontmatter(
        "mem_symlink",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let outside = directory.path().join("outside.md");
    fs::write(
        &outside,
        encode_canonical_document(&header, "outside body").expect("canonical outside record"),
    )
    .expect("write outside record");
    let record_path =
        layout::record_path(directory.path(), &scope, &header.id).expect("record path");
    fs::create_dir_all(record_path.parent().expect("record parent")).expect("record directory");
    symlink(&outside, &record_path).expect("create record symlink");

    let error = store
        .get(
            &header.id,
            &AuthorizedScopes::new(principal_id("prn_reader"))
                .with_project(project_id("prj_symlink")),
        )
        .expect_err("record symlink is rejected");
    assert_eq!(error.code(), StoreErrorCode::UnsafePath);
}

#[test]
fn exact_and_list_reads_validate_filename_scope_and_shard() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let expected_scope = MemoryScope::Project {
        project_id: project_id("prj_expected"),
    };
    let authorized =
        AuthorizedScopes::new(principal_id("prn_reader")).with_project(project_id("prj_expected"));

    let expected_id = memory_id("mem_filename");
    let wrong_id_header = frontmatter(
        "mem_header",
        expected_scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let bytes = encode_canonical_document(&wrong_id_header, "wrong ID").expect("canonical bytes");
    let mismatch_path = write_raw_record(directory.path(), &expected_scope, &expected_id, &bytes);
    assert_invalid_store_reason(
        store
            .get(&expected_id, &authorized)
            .expect_err("filename mismatch is rejected"),
        InvalidRecordReason::IdFilenameMismatch,
    );
    fs::remove_file(mismatch_path).expect("remove mismatch fixture");

    let scope_id = memory_id("mem_scope");
    let wrong_scope_header = frontmatter(
        scope_id.as_str(),
        MemoryScope::Project {
            project_id: project_id("prj_other"),
        },
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let bytes =
        encode_canonical_document(&wrong_scope_header, "wrong scope").expect("canonical bytes");
    let mismatch_path = write_raw_record(directory.path(), &expected_scope, &scope_id, &bytes);
    assert_invalid_store_reason(
        store
            .get(&scope_id, &authorized)
            .expect_err("scope mismatch is rejected"),
        InvalidRecordReason::ScopePathMismatch,
    );
    fs::remove_file(mismatch_path).expect("remove scope fixture");

    let shard_header = frontmatter(
        "mem_shard",
        expected_scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let canonical_shard = layout::record_shard(&shard_header.id);
    let wrong_shard_name = if canonical_shard == "00" { "ff" } else { "00" };
    let wrong_shard = layout::scope_directory(directory.path(), &expected_scope)
        .expect("scope directory")
        .join(wrong_shard_name);
    fs::create_dir_all(&wrong_shard).expect("wrong shard directory");
    fs::write(
        wrong_shard.join(layout::record_file_name(&shard_header.id)),
        encode_canonical_document(&shard_header, "wrong shard").expect("canonical bytes"),
    )
    .expect("write wrong-shard record");
    let request = list_request(
        vec![ScopeSelector::Project {
            project_id: project_id("prj_expected"),
        }],
        10,
    );
    assert_invalid_store_reason(
        store
            .list(&request, &authorized)
            .expect_err("wrong shard is rejected"),
        InvalidRecordReason::ShardMismatch,
    );
}

#[test]
fn storage_keys_are_casefold_safe_and_keep_principal_scopes_distinct() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let upper_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_CaseFold"),
    };
    let lower_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_casefold"),
    };
    let upper_owner = layout::scope_directory(directory.path(), &upper_scope)
        .expect("upper-case owner storage key");
    let lower_owner = layout::scope_directory(directory.path(), &lower_scope)
        .expect("lower-case owner storage key");
    let upper_owner_key = upper_owner
        .file_name()
        .and_then(|name| name.to_str())
        .expect("ASCII owner key");
    let lower_owner_key = lower_owner
        .file_name()
        .and_then(|name| name.to_str())
        .expect("ASCII owner key");
    assert!(
        upper_owner_key
            .bytes()
            .all(|byte| !byte.is_ascii_uppercase())
    );
    assert!(
        lower_owner_key
            .bytes()
            .all(|byte| !byte.is_ascii_uppercase())
    );
    assert_ne!(upper_owner_key, lower_owner_key);
    assert_eq!(
        upper_owner_key,
        "64969df88972d6ecec626f38ff7e3ccc7c80db295fcf153f3e0b6e6e5480c8cf"
    );

    let upper_memory = memory_id("mem_CaseFold");
    let lower_memory = memory_id("mem_casefold");
    let upper_file = layout::record_file_name(&upper_memory);
    let lower_file = layout::record_file_name(&lower_memory);
    assert!(upper_file.bytes().all(|byte| !byte.is_ascii_uppercase()));
    assert!(lower_file.bytes().all(|byte| !byte.is_ascii_uppercase()));
    assert_ne!(upper_file, lower_file);
    assert_eq!(
        upper_file,
        "c41c2439779d75e0f42786fb0599a15e675669ba7cbdbfc596f059cd06baa3e6.md"
    );
    assert_eq!(layout::record_shard(&upper_memory), "c4");

    write_raw_record(
        directory.path(),
        &upper_scope,
        &upper_memory,
        include_bytes!("../fixtures/v1alpha1/invalid/malformed-frontmatter.md"),
    );
    let lower_authority = AuthorizedScopes::new(principal_id("prn_casefold"));
    assert_eq!(
        store
            .get(&upper_memory, &lower_authority)
            .expect_err("case-folded principal aliases remain invisible")
            .code(),
        StoreErrorCode::NotFound
    );
    let page = store
        .list(
            &list_request(vec![ScopeSelector::Principal {}], 10),
            &lower_authority,
        )
        .expect("another case variant is not scanned");
    assert!(page.result.memories.is_empty());
}

#[test]
fn inaccessible_records_match_not_found_and_are_never_scanned() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let hidden_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_hidden"),
    };
    let hidden_id = memory_id("mem_hidden");
    write_raw_record(
        directory.path(),
        &hidden_scope,
        &hidden_id,
        include_bytes!("../fixtures/v1alpha1/invalid/malformed-frontmatter.md"),
    );

    let own_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_visible"),
    };
    let visible = frontmatter(
        "mem_visible",
        own_scope,
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    write_record(directory.path(), &visible, "visible body");
    let authorized = AuthorizedScopes::new(principal_id("prn_visible"));

    let hidden_error = store
        .get(&hidden_id, &authorized)
        .expect_err("invisible record is hidden");
    let absent_error = store
        .get(&memory_id("mem_absent"), &authorized)
        .expect_err("absent record is missing");
    assert_eq!(hidden_error.code(), StoreErrorCode::NotFound);
    assert_eq!(absent_error.code(), StoreErrorCode::NotFound);
    assert_eq!(hidden_error.to_string(), absent_error.to_string());

    let visible_page = store
        .list(
            &list_request(vec![ScopeSelector::Principal {}], 10),
            &authorized,
        )
        .expect("hidden invalid sibling is not scanned");
    assert_eq!(visible_page.result.memories.len(), 1);
    assert_eq!(visible_page.result.memories[0].id, visible.id);

    let hidden_project = MemoryScope::Project {
        project_id: project_id("prj_hidden"),
    };
    write_raw_record(
        directory.path(),
        &hidden_project,
        &memory_id("mem_hidden_project"),
        b"not canonical\n",
    );
    let unauthorized_request = list_request(
        vec![ScopeSelector::Project {
            project_id: project_id("prj_hidden"),
        }],
        10,
    );
    let empty = store
        .list(&unauthorized_request, &authorized)
        .expect("unauthorized scope is filtered before scanning");
    assert!(empty.result.memories.is_empty());
}

#[test]
fn duplicate_memory_ids_are_explicit_across_authorized_scopes() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let id = "mem_duplicate";
    let principal_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_duplicate"),
    };
    let project_scope = MemoryScope::Project {
        project_id: project_id("prj_duplicate"),
    };
    write_record(
        directory.path(),
        &frontmatter(
            id,
            principal_scope,
            "2026-08-23T10:00:00Z",
            "2026-08-23T10:00:00Z",
        ),
        "principal copy",
    );
    write_record(
        directory.path(),
        &frontmatter(
            id,
            project_scope,
            "2026-08-23T10:00:00Z",
            "2026-08-23T10:00:00Z",
        ),
        "project copy",
    );
    let authorized = AuthorizedScopes::new(principal_id("prn_duplicate"))
        .with_project(project_id("prj_duplicate"));
    assert_eq!(
        store
            .get(&memory_id(id), &authorized)
            .expect_err("duplicate exact read fails")
            .code(),
        StoreErrorCode::DuplicateMemoryId
    );
    let request = list_request(
        vec![
            ScopeSelector::Principal {},
            ScopeSelector::Project {
                project_id: project_id("prj_duplicate"),
            },
        ],
        10,
    );
    assert_eq!(
        store
            .list(&request, &authorized)
            .expect_err("duplicate list fails")
            .code(),
        StoreErrorCode::DuplicateMemoryId
    );
}

#[test]
fn instance_global_records_use_the_ownerless_layout_and_require_an_explicit_grant() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::InstanceGlobal {};
    let header = frontmatter(
        "mem_global",
        scope,
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    let path = write_record(directory.path(), &header, "operator-managed reference");
    assert!(path.starts_with(directory.path().join("records/instance_global")));
    let ungranted = AuthorizedScopes::new(principal_id("prn_global_reader"));
    assert_eq!(
        store
            .get(&header.id, &ungranted)
            .expect_err("global records require an explicit grant")
            .code(),
        StoreErrorCode::NotFound
    );

    let granted = ungranted.with_instance_global();
    let exact = store
        .get(&header.id, &granted)
        .expect("granted global exact read");
    assert_eq!(exact.result.id, header.id);
    let page = store
        .list(
            &list_request(vec![ScopeSelector::InstanceGlobal {}], 10),
            &granted,
        )
        .expect("granted global list");
    assert_eq!(page.result.memories.len(), 1);
    assert_eq!(page.result.memories[0].id, header.id);
}

#[test]
fn normal_reads_never_rewrite_or_quarantine_invalid_records() {
    let directory = TempDir::new().expect("temporary directory");
    let mut store =
        CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Session {
        session_id: session_id("ses_invalid"),
    };
    let id = memory_id("mem_invalid");
    let path = write_raw_record(
        directory.path(),
        &scope,
        &id,
        include_bytes!("../fixtures/v1alpha1/invalid/malformed-frontmatter.md"),
    );
    let before_bytes = fs::read(&path).expect("invalid bytes before read");
    let before_mtime = fs::metadata(&path)
        .expect("invalid metadata before read")
        .modified()
        .expect("invalid mtime before read");
    let authorized =
        AuthorizedScopes::new(principal_id("prn_operator")).with_session(session_id("ses_invalid"));

    assert_eq!(
        store
            .get(&id, &authorized)
            .expect_err("invalid get fails")
            .code(),
        StoreErrorCode::InvalidRecord
    );
    assert_eq!(
        store
            .list(
                &list_request(
                    vec![ScopeSelector::Session {
                        session_id: session_id("ses_invalid"),
                    }],
                    10,
                ),
                &authorized,
            )
            .expect_err("invalid list fails")
            .code(),
        StoreErrorCode::InvalidRecord
    );
    assert_eq!(
        fs::read(&path).expect("invalid bytes after read"),
        before_bytes
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("invalid metadata after read")
            .modified()
            .expect("invalid mtime after read"),
        before_mtime
    );
    assert_eq!(
        fs::read_dir(directory.path().join("quarantine"))
            .expect("quarantine directory")
            .count(),
        0
    );

    let receipt = store
        .quarantine_invalid(&scope, &id)
        .expect("explicit operator quarantine succeeds");
    assert_eq!(receipt.memory_id, id);
    assert_eq!(receipt.quarantine_token.len(), 32);
    assert!(!path.exists());
    assert_eq!(
        fs::read_dir(directory.path().join("quarantine"))
            .expect("quarantine directory")
            .count(),
        1
    );

    let truncated_id = memory_id("mem_truncated");
    write_raw_record(
        directory.path(),
        &scope,
        &truncated_id,
        include_bytes!("../fixtures/v1alpha1/invalid/truncated.md"),
    );
    assert_invalid_store_reason(
        store
            .get(&truncated_id, &authorized)
            .expect_err("truncated canonical file is rejected"),
        InvalidRecordReason::Truncated,
    );

    let valid = frontmatter(
        "mem_valid",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:00:00Z",
    );
    write_record(directory.path(), &valid, "valid body");
    assert_eq!(
        store
            .quarantine_invalid(&scope, &valid.id)
            .expect_err("valid record is retained")
            .code(),
        StoreErrorCode::RecordIsValid
    );
}

fn assert_invalid_store_reason(error: StoreError, expected: InvalidRecordReason) {
    match error {
        StoreError::InvalidRecord { reason, .. } => assert_eq!(reason, expected),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn list_filters_and_sorts_by_real_timestamp_with_id_tie_breakers() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_filters"),
    };

    let whole_second = frontmatter(
        "mem_whole",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:05:00Z",
    );
    write_record(directory.path(), &whole_second, "whole second");
    let mut fractional = frontmatter(
        "mem_fractional",
        scope.clone(),
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:05:00.1Z",
    );
    fractional.memory_type = MemoryType::Reference;
    fractional.status = MemoryStatus::Stale;
    fractional
        .tags
        .push(Tag::new("identity").expect("valid tag"));
    write_record(directory.path(), &fractional, "fractional second");
    let tied = frontmatter(
        "mem_aaa_tied",
        scope,
        "2026-08-23T10:00:00Z",
        "2026-08-23T10:05:00Z",
    );
    write_record(directory.path(), &tied, "timestamp tie");
    let authorized = AuthorizedScopes::new(principal_id("prn_filters"));

    let all = store
        .list(
            &list_request(vec![ScopeSelector::Principal {}], 10),
            &authorized,
        )
        .expect("list all records");
    let ids: Vec<_> = all
        .result
        .memories
        .iter()
        .map(|memory| memory.id.as_str())
        .collect();
    assert_eq!(ids, ["mem_fractional", "mem_aaa_tied", "mem_whole"]);
    assert_eq!(all.store_revision, StoreRevision(0));
    let serialized = serde_json::to_value(&all.result.memories).expect("serialize summaries");
    let serialized = serialized.to_string();
    for forbidden in ["body", "provenance", "relations", "path"] {
        assert!(!serialized.contains(forbidden));
    }

    let mut filtered = list_request(vec![ScopeSelector::Principal {}], 10);
    filtered.types = vec![MemoryType::Reference];
    filtered.statuses = vec![MemoryStatus::Stale];
    filtered.tags = vec![Tag::new("identity").expect("valid tag")];
    filtered.updated_after = Some(timestamp("2026-08-23T10:05:00Z"));
    let page = store
        .list(&filtered, &authorized)
        .expect("apply all structured filters");
    assert_eq!(page.result.memories.len(), 1);
    assert_eq!(page.result.memories[0].id, fractional.id);

    let mut invalid = list_request(
        vec![ScopeSelector::Principal {}, ScopeSelector::Principal {}],
        10,
    );
    invalid.sort = ListSort::IdAsc;
    assert_eq!(
        store
            .list(&invalid, &authorized)
            .expect_err("duplicate scopes are invalid")
            .code(),
        StoreErrorCode::InvalidRequest
    );
}

#[test]
fn cursor_is_restart_deterministic_and_bound_to_store_query_and_authority() {
    let directory = TempDir::new().expect("temporary directory");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let principal_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_cursor"),
    };
    let project_scope = MemoryScope::Project {
        project_id: project_id("prj_cursor"),
    };
    let session_scope = MemoryScope::Session {
        session_id: session_id("ses_cursor"),
    };
    for (id, scope, updated_at) in [
        ("mem_a_cursor", principal_scope, "2026-08-23T10:05:00Z"),
        ("mem_b_cursor", project_scope, "2026-08-23T10:05:00Z"),
        ("mem_c_cursor", session_scope, "2026-08-23T10:06:00Z"),
    ] {
        write_record(
            directory.path(),
            &frontmatter(id, scope, "2026-08-23T10:00:00Z", updated_at),
            "cursor body",
        );
    }
    let authorized = AuthorizedScopes::new(principal_id("prn_cursor"))
        .with_project(project_id("prj_cursor"))
        .with_session(session_id("ses_cursor"));
    let base_request = list_request(
        vec![
            ScopeSelector::Principal {},
            ScopeSelector::Project {
                project_id: project_id("prj_cursor"),
            },
            ScopeSelector::Session {
                session_id: session_id("ses_cursor"),
            },
        ],
        2,
    );
    let first = store
        .list(&base_request, &authorized)
        .expect("first cursor page");
    assert_eq!(
        first
            .result
            .memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<Vec<_>>(),
        ["mem_c_cursor", "mem_a_cursor"]
    );
    assert!(first.result.has_more);
    let cursor = first.result.next_cursor.expect("next cursor");

    let mut second_request = base_request.clone();
    second_request.cursor = Some(cursor.clone());
    drop(store);
    let reopened = CanonicalStore::open(directory.path(), owner()).expect("restart store");
    let second = reopened
        .list(&second_request, &authorized)
        .expect("cursor survives restart");
    assert_eq!(second.result.memories.len(), 1);
    assert_eq!(second.result.memories[0].id.as_str(), "mem_b_cursor");
    assert!(!second.result.has_more);
    assert!(second.result.next_cursor.is_none());

    let mut different_sort = second_request.clone();
    different_sort.sort = ListSort::IdAsc;
    assert_eq!(
        reopened
            .list(&different_sort, &authorized)
            .expect_err("cursor is bound to sort")
            .code(),
        StoreErrorCode::InvalidCursor
    );
    let mut different_filter = second_request.clone();
    different_filter.tags = vec![Tag::new("architecture").expect("valid tag")];
    assert_eq!(
        reopened
            .list(&different_filter, &authorized)
            .expect_err("cursor is bound to filters")
            .code(),
        StoreErrorCode::InvalidCursor
    );
    let broader_authority = authorized
        .clone()
        .with_project(project_id("prj_additional"));
    assert_eq!(
        reopened
            .list(&second_request, &broader_authority)
            .expect_err("cursor is bound to authoritative scope fingerprint")
            .code(),
        StoreErrorCode::InvalidCursor
    );
    let mut malformed = base_request.clone();
    malformed.cursor = Some(PageCursor::new("j1_malformed").expect("base64url-shaped cursor"));
    assert_eq!(
        reopened
            .list(&malformed, &authorized)
            .expect_err("malformed cursor fails closed")
            .code(),
        StoreErrorCode::InvalidCursor
    );
    let mut tampered_parts: Vec<String> = cursor.as_str().split('_').map(str::to_owned).collect();
    tampered_parts[4] = "0".to_owned();
    let mut tampered_offset = base_request.clone();
    tampered_offset.cursor = Some(
        PageCursor::new(tampered_parts.join("_"))
            .expect("tampered token still matches the public cursor alphabet"),
    );
    assert_eq!(
        reopened
            .list(&tampered_offset, &authorized)
            .expect_err("offset tampering fails its integrity check")
            .code(),
        StoreErrorCode::InvalidCursor
    );

    drop(reopened);
    let metadata_path = directory.path().join("store.json");
    let mut metadata: StoreMetadata =
        serde_json::from_slice(&fs::read(&metadata_path).expect("metadata bytes"))
            .expect("store metadata");
    metadata.store_revision = StoreRevision(1);
    fs::write(
        &metadata_path,
        metadata.canonical_bytes().expect("canonical metadata"),
    )
    .expect("advance external test watermark");
    let advanced = CanonicalStore::open(directory.path(), owner()).expect("open advanced store");
    assert_eq!(
        advanced
            .list(&second_request, &authorized)
            .expect_err("old cursor is stale after watermark advance")
            .code(),
        StoreErrorCode::StaleCursor
    );
}
