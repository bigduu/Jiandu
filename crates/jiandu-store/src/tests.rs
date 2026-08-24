use super::*;
use crate::document::{decode_canonical_document, encode_canonical_document};
use crate::layout;
use jiandu_core::{
    ClientId, CreationActor, FrontmatterProvenance, FrontmatterScope, Grant, IdempotencyKey,
    ListSort, MemoryFrontmatterV1Alpha1, MemoryId, MemoryListRequest, MemoryPatch, MemorySchema,
    MemoryScope, MemoryStatus, MemoryType, PageCursor, PageLimit, PrincipalId, ProjectId,
    ProvenanceInput, RememberMemoryCommand, Revision, ScopeSelector, SessionId, StoreRevision, Tag,
    TagPatch, Timestamp, TrustedRequestContext, UpdateMemoryCommand,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
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

#[cfg(windows)]
fn raw_windows_path(root: &Path, components: &[&std::ffi::OsStr]) -> PathBuf {
    let mut raw = root.as_os_str().to_os_string();
    for component in components {
        raw.push(std::path::MAIN_SEPARATOR_STR);
        raw.push(component);
    }
    PathBuf::from(raw)
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

fn scope_selector(scope: &MemoryScope) -> ScopeSelector {
    match scope {
        MemoryScope::Principal { .. } => ScopeSelector::Principal {},
        MemoryScope::Project { project_id } => ScopeSelector::Project {
            project_id: project_id.clone(),
        },
        MemoryScope::Session { session_id } => ScopeSelector::Session {
            session_id: session_id.clone(),
        },
        MemoryScope::InstanceGlobal {} => ScopeSelector::InstanceGlobal {},
    }
}

fn remember_command(scope: &MemoryScope, id: &str, body: &str) -> RememberMemoryCommand {
    RememberMemoryCommand {
        scope: scope_selector(scope),
        memory_type: MemoryType::Decision,
        title: format!("Title for {id}"),
        summary: Some(format!("Summary for {id}")),
        body: body.to_owned(),
        tags: vec![Tag::new("architecture").expect("valid tag")],
        provenance: ProvenanceInput::default(),
        relations: Vec::new(),
        idempotency_key: IdempotencyKey::new(format!("create-{id}"))
            .expect("valid idempotency key"),
    }
}

fn authorized_mutation(
    authority: &AuthorizedScopes,
    scope: &MemoryScope,
    operation: MutationOperation,
) -> AuthorizedMutation {
    let context = TrustedRequestContext {
        principal_id: authority.principal_id.clone(),
        client_id: ClientId::new("cli_store_tests").expect("valid client ID"),
        grants: BTreeSet::from([
            Grant::new(operation.required_grant(scope)).expect("valid mutation grant")
        ]),
    };
    authority
        .authorize_mutation(&context, scope, operation)
        .expect("trusted context authorizes exact mutation")
}

fn create_memory(
    store: &mut CanonicalStore,
    authorization: &AuthorizedMutation,
    id: &str,
    body: &str,
) -> Result<MutationCommit, StoreError> {
    store.create(
        authorization,
        &remember_command(authorization.as_scope(), id, body),
        memory_id(id),
        CreationActor::Host,
        timestamp("2026-08-24T02:00:00Z"),
    )
}

fn update_command(id: &MemoryId, revision: u64, title: &str) -> UpdateMemoryCommand {
    UpdateMemoryCommand {
        memory_id: id.clone(),
        expected_revision: Revision::new(revision).expect("positive revision"),
        patch: MemoryPatch {
            title: Some(title.to_owned()),
            body: None,
            tags: Some(TagPatch {
                add: vec![Tag::new("updated").expect("valid tag")],
                remove: Vec::new(),
            }),
            status: None,
            relations: None,
        },
        reason: "authoritative source changed".to_owned(),
        idempotency_key: IdempotencyKey::new(format!(
            "update-{revision}-{}",
            title.replace(' ', "-")
        ))
        .expect("valid idempotency key"),
    }
}

#[derive(Debug)]
struct FailOnce {
    boundary: PersistenceBoundary,
    fired: AtomicBool,
}

impl FailOnce {
    fn at(boundary: PersistenceBoundary) -> Arc<Self> {
        Arc::new(Self {
            boundary,
            fired: AtomicBool::new(false),
        })
    }
}

impl PersistenceFailpointInjector for FailOnce {
    fn should_fail(&self, boundary: PersistenceBoundary) -> bool {
        boundary == self.boundary && !self.fired.swap(true, Ordering::SeqCst)
    }
}

const MUTATION_PERSISTENCE_BOUNDARIES: &[PersistenceBoundary] = &[
    PersistenceBoundary::ManifestTempWritten,
    PersistenceBoundary::ManifestTempSynced,
    PersistenceBoundary::ManifestTempDirectorySynced,
    PersistenceBoundary::ManifestPublished,
    PersistenceBoundary::ManifestDirectorySynced,
    PersistenceBoundary::RecordNamespacePrepared,
    PersistenceBoundary::RecordTempWritten,
    PersistenceBoundary::RecordTempSynced,
    PersistenceBoundary::RecordTempDirectorySynced,
    PersistenceBoundary::MetadataTempWritten,
    PersistenceBoundary::MetadataTempSynced,
    PersistenceBoundary::MetadataTempDirectorySynced,
    PersistenceBoundary::IdempotencyNamespacePrepared,
    PersistenceBoundary::MutationResultTempWritten,
    PersistenceBoundary::MutationResultTempSynced,
    PersistenceBoundary::MutationResultTempDirectorySynced,
    PersistenceBoundary::MutationReceiptTempWritten,
    PersistenceBoundary::MutationReceiptTempSynced,
    PersistenceBoundary::MutationReceiptTempDirectorySynced,
    PersistenceBoundary::MutationAuditTempWritten,
    PersistenceBoundary::MutationAuditTempSynced,
    PersistenceBoundary::MutationAuditTempDirectorySynced,
    PersistenceBoundary::RecordRenamed,
    PersistenceBoundary::RecordDirectorySynced,
    PersistenceBoundary::MutationResultPublished,
    PersistenceBoundary::MutationResultDirectorySynced,
    PersistenceBoundary::MutationReceiptPublished,
    PersistenceBoundary::MutationReceiptDirectorySynced,
    PersistenceBoundary::MutationAuditPublished,
    PersistenceBoundary::MutationAuditDirectorySynced,
    PersistenceBoundary::MetadataRenamed,
    PersistenceBoundary::MetadataDirectorySynced,
    PersistenceBoundary::ManifestRemoved,
    PersistenceBoundary::ManifestRemovalDirectorySynced,
];

const RECOVERY_IDEMPOTENCY_BOUNDARIES: &[PersistenceBoundary] = &[
    PersistenceBoundary::RecoveryIdempotencyNamespacePrepared,
    PersistenceBoundary::RecoveryMutationResultDirectorySynced,
    PersistenceBoundary::RecoveryMutationReceiptDirectorySynced,
    PersistenceBoundary::RecoveryMutationAuditDirectorySynced,
];

const MIGRATION_PERSISTENCE_BOUNDARIES: &[PersistenceBoundary] = &[
    PersistenceBoundary::MigrationLayoutSynced,
    PersistenceBoundary::MigrationGenesisTempWritten,
    PersistenceBoundary::MigrationGenesisTempSynced,
    PersistenceBoundary::MigrationGenesisTempDirectorySynced,
    PersistenceBoundary::MigrationGenesisPublished,
    PersistenceBoundary::MigrationGenesisDirectorySynced,
    PersistenceBoundary::MigrationMetadataTempWritten,
    PersistenceBoundary::MigrationMetadataTempSynced,
    PersistenceBoundary::MigrationMetadataTempDirectorySynced,
    PersistenceBoundary::MigrationMetadataPublished,
    PersistenceBoundary::MigrationMetadataDirectorySynced,
];

fn mutation_target_was_published(boundary: PersistenceBoundary) -> bool {
    matches!(
        boundary,
        PersistenceBoundary::RecordRenamed
            | PersistenceBoundary::RecordDirectorySynced
            | PersistenceBoundary::MutationResultPublished
            | PersistenceBoundary::MutationResultDirectorySynced
            | PersistenceBoundary::MutationReceiptPublished
            | PersistenceBoundary::MutationReceiptDirectorySynced
            | PersistenceBoundary::MutationAuditPublished
            | PersistenceBoundary::MutationAuditDirectorySynced
            | PersistenceBoundary::MetadataRenamed
            | PersistenceBoundary::MetadataDirectorySynced
            | PersistenceBoundary::ManifestRemoved
            | PersistenceBoundary::ManifestRemovalDirectorySynced
    )
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

fn store_metadata(root: &Path) -> StoreMetadata {
    serde_json::from_slice(&fs::read(root.join("store.json")).expect("read store metadata"))
        .expect("decode store metadata")
}

fn make_legacy_store(root: &Path) -> StoreMetadata {
    let store = CanonicalStore::initialize(root, owner()).expect("initialize migration fixture");
    let mut metadata = store.metadata.clone();
    drop(store);
    metadata.format_version = crate::metadata::LEGACY_STORE_FORMAT_VERSION.to_owned();
    metadata.audit_sequence = AuditSequence(0);
    fs::remove_file(root.join(layout::AUDIT_GENESIS_FILE)).expect("remove v2 audit genesis");
    fs::remove_dir_all(root.join("receipts/idempotency")).expect("remove v2 idempotency layout");
    fs::remove_dir_all(root.join(layout::MUTATION_AUDIT_DIR))
        .expect("remove v2 mutation audit layout");
    fs::write(
        root.join("store.json"),
        metadata.canonical_bytes().expect("legacy metadata bytes"),
    )
    .expect("write legacy store metadata");
    metadata
}

fn regular_files_below(root: &Path, relative: &str) -> Vec<PathBuf> {
    tree_snapshot(&root.join(relative))
        .into_iter()
        .filter_map(|(path, entry)| entry.bytes.map(|_| Path::new(relative).join(path)))
        .collect()
}

fn only_regular_file_below(root: &Path, relative: &str) -> PathBuf {
    let files = regular_files_below(root, relative);
    assert_eq!(files.len(), 1, "expected one file below {relative}");
    root.join(&files[0])
}

fn committed_idempotency_fixture() -> TempDir {
    let directory = TempDir::new().expect("temporary directory");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_artifact_fixture"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_artifact_fixture"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let mut store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize");
    create_memory(
        &mut store,
        &authorization,
        "mem_artifact_fixture",
        "private result body",
    )
    .expect("commit artifact fixture");
    drop(store);
    directory
}

fn assert_invalid_ledger_after(mutate: impl FnOnce(&Path)) {
    let directory = committed_idempotency_fixture();
    mutate(directory.path());
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("invalid private ledger fails before readiness")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
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
        "receipts/quarantine",
        "receipts/idempotency",
        "receipts/idempotency/metadata",
        "receipts/idempotency/results",
        "audit",
        "audit/mutations",
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

    #[cfg(windows)]
    {
        let lock_path = directory.path().join("LOCK");
        let replaced_lock = directory.path().join("LOCK.replaced");
        assert!(fs::rename(&lock_path, &replaced_lock).is_err());
        assert!(fs::remove_file(&lock_path).is_err());
        assert!(lock_path.is_file());
        assert!(!replaced_lock.exists());

        let replaced_root = directory.path().with_extension("replaced");
        assert!(!replaced_root.exists());
        assert!(fs::rename(directory.path(), &replaced_root).is_err());
        assert!(directory.path().is_dir());
        assert!(!replaced_root.exists());
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
    assert_eq!(
        initialized.watermark().expect("initial watermark"),
        StoreRevision(0)
    );
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
    #[cfg(windows)]
    let parent_traversal = raw_windows_path(
        &root,
        &[
            std::ffi::OsStr::new("missing"),
            std::ffi::OsStr::new(".."),
            std::ffi::OsStr::new("store"),
        ],
    );
    #[cfg(not(windows))]
    let parent_traversal = root.join("missing").join("..").join("store");
    assert_eq!(
        CanonicalStore::initialize(parent_traversal, owner())
            .expect_err("parent traversal in data directory is rejected")
            .code(),
        StoreErrorCode::InvalidDataDirectory
    );
    assert!(!root.join("missing").exists());
    assert!(!root.join("store").exists());

    let outside = TempDir::new().expect("outside temporary directory");
    let outside_root = fs::canonicalize(outside.path()).expect("canonical outside directory");
    assert_eq!(
        root.parent(),
        outside_root.parent(),
        "temporary fixtures must share a parent for the escape regression"
    );
    let outside_before = tree_snapshot(&outside_root);
    let outside_name = outside_root.file_name().expect("outside directory name");
    #[cfg(windows)]
    let sibling_traversal = raw_windows_path(&root, &[std::ffi::OsStr::new(".."), outside_name]);
    #[cfg(not(windows))]
    let sibling_traversal = root.join("..").join(outside_name);
    assert_eq!(
        CanonicalStore::initialize(sibling_traversal, owner())
            .expect_err("parent traversal cannot claim a sibling directory")
            .code(),
        StoreErrorCode::InvalidDataDirectory
    );
    assert_eq!(tree_snapshot(&outside_root), outside_before);

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

#[test]
fn create_and_update_enforce_global_identity_cas_and_monotonic_time() {
    let directory = TempDir::new().expect("temporary directory");
    let mut store =
        CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_mutation"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_mutation"))
        .with_project(project_id("prj_mutation"));
    let create_authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let update_authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    assert!(
        authority
            .authorize_exact(&MemoryScope::Project {
                project_id: project_id("prj_other")
            })
            .is_none()
    );
    assert!(
        authority
            .authorize_exact(&MemoryScope::Principal {
                principal_id: principal_id("prn_other")
            })
            .is_none()
    );
    assert!(
        authority
            .authorize_exact(&MemoryScope::Session {
                session_id: session_id("ses_unauthorized")
            })
            .is_none()
    );
    assert!(
        authority
            .authorize_exact(&MemoryScope::InstanceGlobal {})
            .is_none()
    );

    let created = create_memory(
        &mut store,
        &create_authorization,
        "mem_mutation",
        "secret body",
    )
    .expect("create memory");
    assert_eq!(created.record.revision.get(), 1);
    assert_eq!(created.previous_revision, None);
    assert_eq!(created.store_revision, StoreRevision(1));
    assert_eq!(
        store.watermark().expect("created watermark"),
        StoreRevision(1)
    );
    assert!(created.record.etag.as_str().starts_with("sha256:"));

    let other_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_other"),
    };
    let other_authority = AuthorizedScopes::new(principal_id("prn_other"));
    let other_authorization =
        authorized_mutation(&other_authority, &other_scope, MutationOperation::Create);
    let duplicate_error = create_memory(
        &mut store,
        &other_authorization,
        "mem_mutation",
        "different tenant body",
    )
    .expect_err("MemoryId is globally unique across canonical scopes");
    assert_eq!(duplicate_error.code(), StoreErrorCode::AlreadyExists);
    let duplicate_diagnostic = duplicate_error.to_string();
    assert!(!duplicate_diagnostic.contains("secret body"));
    assert!(!duplicate_diagnostic.contains(&directory.path().display().to_string()));

    let earlier = store
        .update(
            &update_authorization,
            &update_command(&created.record.id, 1, "too early"),
            timestamp("2026-08-24T01:59:59Z"),
        )
        .expect_err("updatedAt is monotonic per record");
    assert_eq!(earlier.code(), StoreErrorCode::InvalidRequest);
    assert_eq!(
        store.watermark().expect("unchanged watermark"),
        StoreRevision(1)
    );

    let updated = store
        .update(
            &update_authorization,
            &update_command(&created.record.id, 1, "Updated title"),
            timestamp("2026-08-24T02:01:00Z"),
        )
        .expect("current CAS update succeeds");
    assert_eq!(updated.previous_revision, Some(created.record.revision));
    assert_eq!(updated.record.revision.get(), 2);
    assert_eq!(updated.store_revision, StoreRevision(2));
    assert_ne!(updated.record.etag, created.record.etag);

    let stale = store
        .update(
            &update_authorization,
            &update_command(&created.record.id, 1, "stale overwrite"),
            timestamp("2026-08-24T02:02:00Z"),
        )
        .expect_err("stale CAS cannot overwrite");
    match stale {
        StoreError::RevisionConflict { current_revision } => {
            assert_eq!(current_revision, updated.record.revision);
        }
        other => panic!("unexpected error: {other}"),
    }
    let stale_diagnostic = stale_error_text(&StoreError::RevisionConflict {
        current_revision: updated.record.revision,
    });
    assert!(!stale_diagnostic.contains("secret body"));
    assert!(!stale_diagnostic.contains(&directory.path().display().to_string()));

    let read = store
        .get(&updated.record.id, &authority)
        .expect("read updated record");
    assert_eq!(read.result.title, "Updated title");
    assert_eq!(read.result.body, "secret body");
    drop(store);
    let reopened = CanonicalStore::open(directory.path(), owner()).expect("reopen committed store");
    assert_eq!(
        reopened.watermark().expect("reopened watermark"),
        StoreRevision(2)
    );
    assert_eq!(
        reopened
            .get(&updated.record.id, &authority)
            .expect("read after restart")
            .result
            .revision
            .get(),
        2
    );
}

#[test]
fn create_replays_exact_result_across_generated_values_and_restart_without_writes() {
    let directory = TempDir::new().expect("temporary directory");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_create_replay"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_create_replay"))
        .with_project(project_id("prj_create_replay"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let command = remember_command(&scope, "caller-input", "exact replay body");
    let mut store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize");
    let committed = store
        .create(
            &authorization,
            &command,
            memory_id("mem_create_replay_original"),
            CreationActor::Host,
            timestamp("2026-08-24T02:00:00Z"),
        )
        .expect("first create commits");
    assert!(!committed.idempotent_replay);
    assert_eq!(store.metadata.audit_sequence, AuditSequence(1));
    assert_eq!(
        regular_files_below(directory.path(), layout::IDEMPOTENCY_RECEIPTS_DIR).len(),
        1
    );
    assert_eq!(
        regular_files_below(directory.path(), layout::IDEMPOTENCY_RESULTS_DIR).len(),
        1
    );
    assert_eq!(
        regular_files_below(directory.path(), layout::MUTATION_AUDIT_DIR).len(),
        1
    );

    let before_replay = tree_snapshot(directory.path());
    let replay = store
        .create(
            &authorization,
            &command,
            memory_id("mem_generated_value_must_be_ignored"),
            CreationActor::Import,
            timestamp("2026-08-24T03:00:00Z"),
        )
        .expect("identical retry replays");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.transaction_id, committed.transaction_id);
    assert_eq!(replay.store_revision, committed.store_revision);
    assert_eq!(replay.previous_revision, committed.previous_revision);
    assert_eq!(replay.record, committed.record);
    assert_eq!(tree_snapshot(directory.path()), before_replay);
    drop(store);

    let mut reopened = CanonicalStore::open(directory.path(), owner()).expect("reopen store");
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let before_restart_replay = tree_snapshot(directory.path());
    let replay = reopened
        .create(
            &authorization,
            &command,
            memory_id("mem_another_generated_value"),
            CreationActor::Model,
            timestamp("2026-08-24T04:00:00Z"),
        )
        .expect("restart retry replays");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.record, committed.record);
    assert_eq!(replay.store_revision, StoreRevision(1));
    assert_eq!(reopened.metadata.audit_sequence, AuditSequence(1));
    assert_eq!(tree_snapshot(directory.path()), before_restart_replay);
}

#[test]
fn update_replay_precedes_cas_not_found_and_conflicting_reuse_is_write_free() {
    let directory = TempDir::new().expect("temporary directory");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_update_replay"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_update_replay"));
    let create_authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let update_authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    let mut store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize");
    let created = create_memory(
        &mut store,
        &create_authorization,
        "mem_update_replay",
        "stable replay body",
    )
    .expect("create update target");
    let command = update_command(&created.record.id, 1, "replayed update");
    let committed = store
        .update(
            &update_authorization,
            &command,
            timestamp("2026-08-24T02:01:00Z"),
        )
        .expect("update commits");
    assert!(!committed.idempotent_replay);
    assert_eq!(store.metadata.audit_sequence, AuditSequence(2));

    let before_replay = tree_snapshot(directory.path());
    let replay = store
        .update(
            &update_authorization,
            &command,
            timestamp("2026-08-24T09:00:00Z"),
        )
        .expect("receipt lookup precedes now-stale CAS");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.record, committed.record);
    assert_eq!(tree_snapshot(directory.path()), before_replay);

    let mut conflicting = command.clone();
    conflicting.patch.title = Some("different caller input".to_owned());
    let diagnostic = store
        .update(
            &update_authorization,
            &conflicting,
            timestamp("2026-08-24T10:00:00Z"),
        )
        .expect_err("same key with different input conflicts");
    assert_eq!(diagnostic.code(), StoreErrorCode::IdempotencyConflict);
    assert_eq!(tree_snapshot(directory.path()), before_replay);

    let mut missing = command;
    missing.memory_id = memory_id("mem_missing_but_receipt_exists");
    assert_eq!(
        store
            .update(
                &update_authorization,
                &missing,
                timestamp("2026-08-24T10:01:00Z"),
            )
            .expect_err("receipt conflict precedes not-found lookup")
            .code(),
        StoreErrorCode::IdempotencyConflict
    );
    assert_eq!(tree_snapshot(directory.path()), before_replay);

    drop(store);
    let mut reopened = CanonicalStore::open(directory.path(), owner()).expect("restart");
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    let retry = update_command(&created.record.id, 1, "replayed update");
    let before_restart_replay = tree_snapshot(directory.path());
    let replay = reopened
        .update(&authorization, &retry, timestamp("2026-08-24T11:00:00Z"))
        .expect("restart update retry replays");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.record, committed.record);
    assert_eq!(tree_snapshot(directory.path()), before_restart_replay);
}

#[test]
fn authorization_precedes_private_receipt_lookup_and_principal_operation_keys_are_isolated() {
    let directory = TempDir::new().expect("temporary directory");
    let shared_scope = MemoryScope::Project {
        project_id: project_id("prj_receipt_isolation"),
    };
    let authority_a = AuthorizedScopes::new(principal_id("prn_receipt_a"))
        .with_project(project_id("prj_receipt_isolation"));
    let create_a = authorized_mutation(&authority_a, &shared_scope, MutationOperation::Create);
    let mut store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize");
    let mut command_a = remember_command(&shared_scope, "first", "principal A body");
    command_a.idempotency_key = IdempotencyKey::new("shared-operation-key").expect("key");
    store
        .create(
            &create_a,
            &command_a,
            memory_id("mem_receipt_a"),
            CreationActor::Host,
            timestamp("2026-08-24T02:00:00Z"),
        )
        .expect("principal A create");

    let unauthorized_context = TrustedRequestContext {
        principal_id: principal_id("prn_receipt_a"),
        client_id: ClientId::new("cli_unauthorized").expect("client ID"),
        grants: BTreeSet::new(),
    };
    let before_denial = tree_snapshot(directory.path());
    assert_eq!(
        authority_a
            .authorize_mutation(
                &unauthorized_context,
                &shared_scope,
                MutationOperation::Create,
            )
            .expect_err("grant denial occurs before a receipt capability exists")
            .code(),
        StoreErrorCode::Forbidden
    );
    assert_eq!(tree_snapshot(directory.path()), before_denial);

    let authority_b = AuthorizedScopes::new(principal_id("prn_receipt_b"))
        .with_project(project_id("prj_receipt_isolation"));
    let create_b = authorized_mutation(&authority_b, &shared_scope, MutationOperation::Create);
    let mut command_b = remember_command(&shared_scope, "second", "principal B body");
    command_b.idempotency_key = IdempotencyKey::new("shared-operation-key").expect("key");
    store
        .create(
            &create_b,
            &command_b,
            memory_id("mem_receipt_b"),
            CreationActor::Host,
            timestamp("2026-08-24T02:01:00Z"),
        )
        .expect("same raw key is isolated by principal");

    let update_a = authorized_mutation(&authority_a, &shared_scope, MutationOperation::Update);
    let mut update = update_command(&memory_id("mem_receipt_a"), 1, "operation isolation");
    update.idempotency_key = IdempotencyKey::new("shared-operation-key").expect("key");
    store
        .update(&update_a, &update, timestamp("2026-08-24T02:02:00Z"))
        .expect("same raw key is isolated by operation");
    assert_eq!(store.watermark().expect("watermark"), StoreRevision(3));
    assert_eq!(store.metadata.audit_sequence, AuditSequence(3));
    assert_eq!(
        regular_files_below(directory.path(), layout::IDEMPOTENCY_RECEIPTS_DIR).len(),
        3
    );
}

#[test]
fn lost_acknowledgement_after_commit_replays_success_without_a_second_audit() {
    for boundary in [
        PersistenceBoundary::ManifestRemoved,
        PersistenceBoundary::ManifestRemovalDirectorySynced,
    ] {
        let directory = TempDir::new().expect("temporary directory");
        let scope = MemoryScope::Session {
            session_id: session_id("ses_lost_ack"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_lost_ack"))
            .with_session(session_id("ses_lost_ack"));
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        let command = remember_command(&scope, "lost-ack", "lost acknowledgement body");
        let injector = FailOnce::at(boundary);
        let mut store = CanonicalStore::initialize_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(injector.clone()),
        )
        .expect("initialize");
        assert_eq!(
            store
                .create(
                    &authorization,
                    &command,
                    memory_id("mem_lost_ack"),
                    CreationActor::Host,
                    timestamp("2026-08-24T02:00:00Z"),
                )
                .expect_err("simulate acknowledgement loss")
                .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert!(injector.fired.load(Ordering::SeqCst), "{boundary:?}");
        drop(store);

        let mut reopened = CanonicalStore::open(directory.path(), owner()).expect("recover commit");
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        let replay = reopened
            .create(
                &authorization,
                &command,
                memory_id("mem_retry_generated_id"),
                CreationActor::Operator,
                timestamp("2026-08-24T03:00:00Z"),
            )
            .expect("retry observes durable success");
        assert!(replay.idempotent_replay, "{boundary:?}");
        assert_eq!(replay.record.id, memory_id("mem_lost_ack"), "{boundary:?}");
        assert_eq!(replay.store_revision, StoreRevision(1), "{boundary:?}");
        assert_eq!(reopened.metadata.audit_sequence, AuditSequence(1));
        assert_eq!(
            regular_files_below(directory.path(), layout::MUTATION_AUDIT_DIR).len(),
            1
        );
    }
}

#[test]
fn artifact_recovery_boundaries_are_restartable_and_converge_to_replayable_success() {
    for &recovery_boundary in RECOVERY_IDEMPOTENCY_BOUNDARIES {
        let directory = TempDir::new().expect("temporary directory");
        let scope = MemoryScope::Principal {
            principal_id: principal_id("prn_artifact_recovery"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_artifact_recovery"));
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        let command = remember_command(&scope, "recovery", "artifact recovery body");
        let mut store = CanonicalStore::initialize_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(FailOnce::at(PersistenceBoundary::RecordRenamed)),
        )
        .expect("initialize");
        store
            .create(
                &authorization,
                &command,
                memory_id("mem_artifact_recovery"),
                CreationActor::Host,
                timestamp("2026-08-24T02:00:00Z"),
            )
            .expect_err("leave target record with base metadata");
        drop(store);

        assert_eq!(
            CanonicalStore::open_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(FailOnce::at(recovery_boundary)),
            )
            .expect_err("recovery boundary prevents readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{recovery_boundary:?}"
        );
        let mut reopened = CanonicalStore::open(directory.path(), owner())
            .unwrap_or_else(|error| panic!("retry {recovery_boundary:?}: {error:?}"));
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        let replay = reopened
            .create(
                &authorization,
                &command,
                memory_id("mem_recovery_retry_generated"),
                CreationActor::Host,
                timestamp("2026-08-24T03:00:00Z"),
            )
            .expect("recovered receipt replays");
        assert!(replay.idempotent_replay, "{recovery_boundary:?}");
        assert_eq!(reopened.watermark().expect("watermark"), StoreRevision(1));
        assert_eq!(reopened.metadata.audit_sequence, AuditSequence(1));
        assert_eq!(
            fs::read_dir(directory.path().join("transactions"))
                .expect("transaction directory")
                .count(),
            0
        );
    }
}

#[test]
fn malformed_foreign_missing_oversized_and_orphaned_private_artifacts_fail_closed() {
    assert_invalid_ledger_after(|root| {
        let receipt = only_regular_file_below(root, layout::IDEMPOTENCY_RECEIPTS_DIR);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).expect("receipt bytes"))
                .expect("receipt JSON");
        value
            .as_object_mut()
            .expect("receipt object")
            .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
        let mut bytes = serde_json::to_vec_pretty(&value).expect("tampered receipt");
        bytes.push(b'\n');
        fs::write(receipt, bytes).expect("write unknown receipt field");
    });

    assert_invalid_ledger_after(|root| {
        let receipt = only_regular_file_below(root, layout::IDEMPOTENCY_RECEIPTS_DIR);
        fs::write(receipt, vec![b'x'; 65_537]).expect("write oversized receipt");
    });

    assert_invalid_ledger_after(|root| {
        let result = only_regular_file_below(root, layout::IDEMPOTENCY_RESULTS_DIR);
        fs::write(result, vec![b'x'; 1_048_577]).expect("write oversized private result");
    });

    assert_invalid_ledger_after(|root| {
        let audit = only_regular_file_below(root, layout::MUTATION_AUDIT_DIR);
        fs::remove_file(audit).expect("remove committed audit event");
    });

    assert_invalid_ledger_after(|root| {
        let result = only_regular_file_below(root, layout::IDEMPOTENCY_RESULTS_DIR);
        let parent = result.parent().expect("result shard");
        let shard = parent
            .file_name()
            .and_then(|value| value.to_str())
            .expect("result shard name");
        let mut foreign_id = format!("{shard}{}", "0".repeat(62));
        if result
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value == foreign_id)
        {
            foreign_id.replace_range(2..3, "1");
        }
        fs::copy(&result, parent.join(format!("{foreign_id}.json")))
            .expect("copy foreign result artifact");
    });

    assert_invalid_ledger_after(|root| {
        let result = only_regular_file_below(root, layout::IDEMPOTENCY_RESULTS_DIR);
        let orphan = result
            .parent()
            .expect("result shard")
            .join(".result-00000000-0000-4000-8000-000000000001.tmp");
        fs::write(orphan, b"orphan private transaction artifact\n")
            .expect("write orphan result temp");
    });

    #[cfg(unix)]
    {
        let directory = committed_idempotency_fixture();
        let receipt = only_regular_file_below(directory.path(), layout::IDEMPOTENCY_RECEIPTS_DIR);
        fs::hard_link(&receipt, receipt.with_extension("linked"))
            .expect("create hard-linked receipt");
        assert_eq!(
            CanonicalStore::open(directory.path(), owner())
                .expect_err("hard-linked artifact fails before readiness")
                .code(),
            StoreErrorCode::UnsafePath
        );
    }
}

#[test]
fn manifests_receipts_audits_and_diagnostics_are_secret_safe_while_results_stay_private() {
    const BODY: &str = "BODY_SENTINEL_7f3cf1d431804a28";
    const REASON: &str = "REASON_SENTINEL_e1afcf5f00384cad";
    const CREATE_KEY: &str = "raw-create-key-9719f05ca7be4d9e";
    const UPDATE_KEY: &str = "raw-update-key-428be55b4a7e4cb0";
    const QUERY: &str = "QUERY_SENTINEL_select_password_5bc91ff5";
    const CREDENTIAL: &str = "CREDENTIAL_SENTINEL_sk_4ece1ac8";
    const CANONICAL_PATH: &str = "/private/jiandu/CANONICAL_PATH_SENTINEL_77cd0e9d";

    let directory = TempDir::new().expect("temporary directory");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_secret_safety"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_secret_safety"));
    let create_authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let mut create = remember_command(
        &scope,
        "secret-safety",
        &format!("{BODY}\n{QUERY}\n{CREDENTIAL}\n{CANONICAL_PATH}"),
    );
    create.idempotency_key = IdempotencyKey::new(CREATE_KEY).expect("create key");
    let mut store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize");
    let created = store
        .create(
            &create_authorization,
            &create,
            memory_id("mem_secret_safety"),
            CreationActor::Host,
            timestamp("2026-08-24T02:00:00Z"),
        )
        .expect("create secret fixture");
    drop(store);

    let injector = FailOnce::at(PersistenceBoundary::ManifestDirectorySynced);
    let mut store = CanonicalStore::open_with_options(
        directory.path(),
        owner(),
        StoreOptions::with_failpoint_injector(injector),
    )
    .expect("open with manifest failpoint");
    let update_authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    let mut update = update_command(&created.record.id, 1, "secret-safe update");
    update.reason = REASON.to_owned();
    update.idempotency_key = IdempotencyKey::new(UPDATE_KEY).expect("update key");
    store
        .update(
            &update_authorization,
            &update,
            timestamp("2026-08-24T02:01:00Z"),
        )
        .expect_err("leave body-free update manifest");
    let manifest = fs::read_to_string(only_regular_file_below(directory.path(), "transactions"))
        .expect("read transaction manifest");
    for forbidden in [
        BODY,
        REASON,
        CREATE_KEY,
        UPDATE_KEY,
        QUERY,
        CREDENTIAL,
        CANONICAL_PATH,
    ] {
        assert!(
            !manifest.contains(forbidden),
            "manifest leaked sentinel {forbidden}"
        );
    }
    drop(store);

    let mut store = CanonicalStore::open(directory.path(), owner()).expect("roll back manifest");
    let update_authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    store
        .update(
            &update_authorization,
            &update,
            timestamp("2026-08-24T02:01:00Z"),
        )
        .expect("commit update after rollback");

    let mut conflict = update;
    conflict.patch.title = Some("conflicting secret-safe update".to_owned());
    let error = store
        .update(
            &update_authorization,
            &conflict,
            timestamp("2026-08-24T03:00:00Z"),
        )
        .expect_err("conflicting key reuse");
    assert_eq!(error.code(), StoreErrorCode::IdempotencyConflict);
    let diagnostic = format!("{error:?} {error}");
    for forbidden in [
        BODY,
        REASON,
        CREATE_KEY,
        UPDATE_KEY,
        QUERY,
        CREDENTIAL,
        CANONICAL_PATH,
    ] {
        assert!(!diagnostic.contains(forbidden));
    }
    drop(store);

    let snapshot = tree_snapshot(directory.path());
    let body_bearing = [BODY, QUERY, CREDENTIAL, CANONICAL_PATH];
    for (relative, entry) in snapshot {
        let Some(bytes) = entry.bytes else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let private_body_location = relative.starts_with("records")
            || relative.starts_with(layout::IDEMPOTENCY_RESULTS_DIR);
        for sentinel in body_bearing {
            if text.contains(sentinel) {
                assert!(
                    private_body_location,
                    "body-like sentinel leaked into {}",
                    relative.display()
                );
            }
        }
        for forbidden in [REASON, CREATE_KEY, UPDATE_KEY] {
            assert!(
                !text.contains(forbidden),
                "secret metadata leaked into {}",
                relative.display()
            );
        }
    }
}

fn stale_error_text(error: &StoreError) -> String {
    error.to_string()
}

fn assert_recovery_required<T: std::fmt::Debug>(result: Result<T, StoreError>, context: &str) {
    assert_eq!(
        result.expect_err(context).code(),
        StoreErrorCode::RecoveryRequired,
        "{context}"
    );
}

#[test]
fn concurrent_current_updates_serialize_to_one_commit_and_one_conflict() {
    let directory = TempDir::new().expect("temporary directory");
    let mut store =
        CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_concurrent"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_concurrent"));
    let create_authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let update_authorization = authorized_mutation(&authority, &scope, MutationOperation::Update);
    let created = create_memory(
        &mut store,
        &create_authorization,
        "mem_concurrent",
        "concurrent body",
    )
    .expect("create memory");
    let shared = Arc::new(Mutex::new(store));
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for title in ["winner-a", "winner-b"] {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        let update_authorization = update_authorization.clone();
        let id = created.record.id.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            shared.lock().expect("store mutex").update(
                &update_authorization,
                &update_command(&id, 1, title),
                timestamp("2026-08-24T02:01:00Z"),
            )
        }));
    }
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("mutation thread"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::RevisionConflict { .. })))
            .count(),
        1
    );
    assert_eq!(
        shared
            .lock()
            .expect("store mutex")
            .watermark()
            .expect("serialized watermark"),
        StoreRevision(2)
    );
}

#[test]
fn create_failpoints_recover_old_or_new_without_partial_state() {
    for &boundary in MUTATION_PERSISTENCE_BOUNDARIES {
        let directory = TempDir::new().expect("temporary directory");
        let injector = FailOnce::at(boundary);
        let mut store = CanonicalStore::initialize_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(injector.clone()),
        )
        .expect("initialize fault-injected store");
        let scope = MemoryScope::Project {
            project_id: project_id("prj_fail_create"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_fail_create"))
            .with_project(project_id("prj_fail_create"));
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        let id = memory_id("mem_fail_create");
        let error = create_memory(
            &mut store,
            &authorization,
            id.as_str(),
            "body-not-in-manifest",
        )
        .expect_err("boundary injects failure");
        assert_eq!(
            error.code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert!(injector.fired.load(Ordering::SeqCst), "{boundary:?}");
        assert_eq!(
            store
                .get(&id, &authority,)
                .expect_err("failed transaction poisons current handle")
                .code(),
            StoreErrorCode::RecoveryRequired,
            "{boundary:?}"
        );
        assert_eq!(
            store
                .watermark()
                .expect_err("failed transaction cannot expose a stale watermark")
                .code(),
            StoreErrorCode::RecoveryRequired,
            "{boundary:?}"
        );
        drop(store);

        let reopened = CanonicalStore::open(directory.path(), owner())
            .unwrap_or_else(|error| panic!("startup recovery succeeds at {boundary:?}: {error:?}"));
        let committed = mutation_target_was_published(boundary);
        if committed {
            let record = reopened.get(&id, &authority).expect("new state recovered");
            assert_eq!(record.result.body, "body-not-in-manifest", "{boundary:?}");
            assert_eq!(
                reopened.watermark().expect("recovered watermark"),
                StoreRevision(1),
                "{boundary:?}"
            );
        } else {
            assert_eq!(
                reopened
                    .get(&id, &authority)
                    .expect_err("old state recovered")
                    .code(),
                StoreErrorCode::NotFound,
                "{boundary:?}"
            );
            assert_eq!(
                reopened.watermark().expect("rolled-back watermark"),
                StoreRevision(0),
                "{boundary:?}"
            );
        }
        assert_eq!(
            fs::read_dir(directory.path().join("transactions"))
                .expect("transaction directory")
                .count(),
            0,
            "{boundary:?}"
        );
        assert!(
            fs::read_dir(directory.path())
                .expect("store root")
                .all(|entry| !entry
                    .expect("root entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".store-")),
            "{boundary:?}"
        );
    }
}

#[test]
fn update_failpoints_recover_exactly_one_revision() {
    for &boundary in MUTATION_PERSISTENCE_BOUNDARIES {
        let directory = TempDir::new().expect("temporary directory");
        let mut initial =
            CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
        let scope = MemoryScope::Principal {
            principal_id: principal_id("prn_fail_update"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_fail_update"));
        let create_authorization =
            authorized_mutation(&authority, &scope, MutationOperation::Create);
        let created = create_memory(
            &mut initial,
            &create_authorization,
            "mem_fail_update",
            "stable body",
        )
        .expect("create initial record");
        drop(initial);
        let injector = FailOnce::at(boundary);
        let mut store = CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(injector),
        )
        .expect("open fault-injected store");
        let update_authorization =
            authorized_mutation(&authority, &scope, MutationOperation::Update);
        assert_eq!(
            store
                .update(
                    &update_authorization,
                    &update_command(&created.record.id, 1, "target title"),
                    timestamp("2026-08-24T02:01:00Z"),
                )
                .expect_err("boundary injects update failure")
                .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        drop(store);
        let reopened = CanonicalStore::open(directory.path(), owner()).unwrap_or_else(|error| {
            panic!("recover update transaction at {boundary:?}: {error:?}")
        });
        let record = reopened
            .get(&created.record.id, &authority)
            .expect("record is never lost")
            .result;
        let committed = mutation_target_was_published(boundary);
        assert_eq!(record.revision.get(), if committed { 2 } else { 1 });
        assert_eq!(
            reopened.watermark().expect("recovered update watermark"),
            StoreRevision(if committed { 2 } else { 1 })
        );
        assert_eq!(
            record.title,
            if committed {
                "target title"
            } else {
                "Title for mem_fail_update"
            }
        );
    }
}

#[test]
fn manifest_is_strict_bounded_body_free_and_multiple_intents_fail_closed() {
    let directory = TempDir::new().expect("temporary directory");
    let injector = FailOnce::at(PersistenceBoundary::ManifestDirectorySynced);
    let mut store = CanonicalStore::initialize_with_options(
        directory.path(),
        owner(),
        StoreOptions::with_failpoint_injector(injector),
    )
    .expect("initialize store");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_manifest"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_manifest"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    create_memory(
        &mut store,
        &authorization,
        "mem_manifest",
        "uniquely-secret-body",
    )
    .expect_err("leave durable manifest");
    drop(store);
    let transactions = directory.path().join("transactions");
    let manifest_path = fs::read_dir(&transactions)
        .expect("transaction directory")
        .map(|entry| entry.expect("manifest entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("published manifest");
    let manifest = fs::read_to_string(&manifest_path).expect("read manifest");
    for forbidden in ["uniquely-secret-body", "Title for", "records/", "body"] {
        assert!(!manifest.contains(forbidden), "manifest leaked {forbidden}");
    }

    let second = transactions.join("00000000-0000-4000-8000-000000000001.json");
    fs::copy(&manifest_path, &second).expect("install contradictory second intent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&second, fs::Permissions::from_mode(0o600))
            .expect("private manifest permissions");
    }
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("multiple active manifests fail closed")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
    fs::remove_file(second).expect("remove contradictory test manifest");

    let mut value: serde_json::Value = serde_json::from_str(&manifest).expect("manifest JSON");
    value
        .as_object_mut()
        .expect("manifest object")
        .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
    let mut bytes = serde_json::to_vec_pretty(&value).expect("serialize noncanonical manifest");
    bytes.push(b'\n');
    fs::write(&manifest_path, bytes).expect("write unknown manifest field");
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("unknown manifest fields fail closed")
            .code(),
        StoreErrorCode::InvalidTransaction
    );

    fs::write(&manifest_path, manifest.as_bytes()).expect("restore canonical manifest");
    let foreign_temp = transactions.join("foreign.tmp");
    fs::write(&foreign_temp, b"not a Jiandu transaction\n").expect("write foreign temp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&foreign_temp, fs::Permissions::from_mode(0o600))
            .expect("private foreign temp permissions");
    }
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("foreign transaction temp fails closed")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
    fs::remove_file(&foreign_temp).expect("remove foreign temp");

    fs::write(&manifest_path, vec![b'x'; 65_537]).expect("write oversized manifest");
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("oversized transaction manifest fails closed")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
}

#[test]
fn orphan_record_temp_never_reaches_list_scanning() {
    let directory = TempDir::new().expect("temporary directory");
    drop(CanonicalStore::initialize(directory.path(), owner()).expect("initialize store"));
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_orphan_temp"),
    };
    let id = memory_id("mem_orphan_temp");
    let record = layout::record_path(directory.path(), &scope, &id).expect("record path");
    let shard = record.parent().expect("record shard");
    fs::create_dir_all(shard).expect("create shard");
    let orphan = shard.join(".record-00000000-0000-4000-8000-000000000001.tmp");
    fs::write(&orphan, b"orphan transaction bytes\n").expect("write orphan temp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o600))
            .expect("private orphan temp permissions");
    }
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("orphan record temp fails before readiness")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
}

#[test]
fn quarantine_failpoints_recover_a_durable_receipt_ledger() {
    let boundaries = [
        PersistenceBoundary::ManifestDirectorySynced,
        PersistenceBoundary::QuarantineRenamed,
        PersistenceBoundary::QuarantineDirectorySynced,
        PersistenceBoundary::QuarantineSourceDirectorySynced,
        PersistenceBoundary::QuarantineReceiptTempWritten,
        PersistenceBoundary::QuarantineReceiptTempSynced,
        PersistenceBoundary::QuarantineReceiptTempDirectorySynced,
        PersistenceBoundary::QuarantineReceiptPublished,
        PersistenceBoundary::QuarantineReceiptDirectorySynced,
        PersistenceBoundary::ManifestRemoved,
    ];
    for boundary in boundaries {
        let directory = TempDir::new().expect("temporary directory");
        let initial =
            CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
        let scope = MemoryScope::Session {
            session_id: session_id("ses_quarantine_recovery"),
        };
        let id = memory_id("mem_quarantine_recovery");
        let source = write_raw_record(
            directory.path(),
            &scope,
            &id,
            include_bytes!("../fixtures/v1alpha1/invalid/truncated.md"),
        );
        drop(initial);
        let injector = FailOnce::at(boundary);
        let mut store = CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(injector),
        )
        .expect("open fault-injected store");
        assert_eq!(
            store
                .quarantine_invalid(&scope, &id)
                .expect_err("boundary injects quarantine failure")
                .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert_eq!(
            store
                .pending_quarantine_receipts()
                .expect_err("failed quarantine poisons ledger reads")
                .code(),
            StoreErrorCode::RecoveryRequired,
            "{boundary:?}"
        );
        drop(store);
        let mut reopened =
            CanonicalStore::open(directory.path(), owner()).expect("recover quarantine");
        let committed = !matches!(boundary, PersistenceBoundary::ManifestDirectorySynced);
        if committed {
            assert!(!source.exists(), "{boundary:?}");
            assert_eq!(
                reopened
                    .pending_quarantine_receipts()
                    .expect("pending receipts")
                    .len(),
                1
            );
            let receipt = reopened
                .pending_quarantine_receipts()
                .expect("pending receipts")[0]
                .clone();
            assert_eq!(receipt.memory_id, id);
            assert_eq!(
                fs::read_dir(directory.path().join("quarantine"))
                    .expect("quarantine directory")
                    .count(),
                1
            );
            reopened
                .acknowledge_quarantine_receipt(&receipt.memory_id, &receipt.quarantine_token)
                .expect("acknowledge durable operator receipt");
            assert!(
                reopened
                    .pending_quarantine_receipts()
                    .expect("pending receipts after acknowledgement")
                    .is_empty()
            );
            drop(reopened);
            let after_ack =
                CanonicalStore::open(directory.path(), owner()).expect("reopen after ack");
            assert!(
                after_ack
                    .pending_quarantine_receipts()
                    .expect("pending receipts after restart")
                    .is_empty()
            );
        } else {
            assert!(source.is_file(), "{boundary:?}");
            assert!(
                reopened
                    .pending_quarantine_receipts()
                    .expect("rolled-back receipt ledger")
                    .is_empty()
            );
        }
    }
}

#[test]
fn recovery_itself_is_restartable_and_never_serves_an_ambiguous_handle() {
    let directory = TempDir::new().expect("temporary directory");
    let first = FailOnce::at(PersistenceBoundary::RecordRenamed);
    let mut store = CanonicalStore::initialize_with_options(
        directory.path(),
        owner(),
        StoreOptions::with_failpoint_injector(first),
    )
    .expect("initialize store");
    let recovery_scope = MemoryScope::Principal {
        principal_id: principal_id("prn_recovery_restart"),
    };
    let recovery_authority = AuthorizedScopes::new(principal_id("prn_recovery_restart"));
    let authorization = authorized_mutation(
        &recovery_authority,
        &recovery_scope,
        MutationOperation::Create,
    );
    create_memory(
        &mut store,
        &authorization,
        "mem_recovery_restart",
        "recovery body",
    )
    .expect_err("stop after record rename");
    drop(store);

    let recovery_failure = FailOnce::at(PersistenceBoundary::RecoveryMetadataDirectorySynced);
    assert_eq!(
        CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(recovery_failure),
        )
        .expect_err("recovery boundary failure prevents readiness")
        .code(),
        StoreErrorCode::InjectedFailure
    );
    let reopened = CanonicalStore::open(directory.path(), owner()).expect("retry recovery");
    assert_eq!(
        reopened.watermark().expect("recovery watermark"),
        StoreRevision(1)
    );
    assert_eq!(
        reopened
            .get(
                &memory_id("mem_recovery_restart"),
                &AuthorizedScopes::new(principal_id("prn_recovery_restart")),
            )
            .expect("committed record survives repeated recovery")
            .result
            .revision
            .get(),
        1
    );
}

#[test]
fn rollback_and_quarantine_recovery_boundaries_are_restartable() {
    for recovery_boundary in [
        PersistenceBoundary::RecoveryRecordDirectorySynced,
        PersistenceBoundary::RecoveryManifestDirectorySynced,
    ] {
        let directory = TempDir::new().expect("temporary directory");
        let primary_boundary =
            if recovery_boundary == PersistenceBoundary::RecoveryRecordDirectorySynced {
                PersistenceBoundary::RecordTempDirectorySynced
            } else {
                PersistenceBoundary::ManifestDirectorySynced
            };
        let mut store = CanonicalStore::initialize_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(FailOnce::at(primary_boundary)),
        )
        .expect("initialize store");
        let scope = MemoryScope::Principal {
            principal_id: principal_id("prn_rollback_restart"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_rollback_restart"));
        let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
        create_memory(
            &mut store,
            &authorization,
            "mem_rollback_restart",
            "rollback body",
        )
        .expect_err("leave rollback transaction");
        drop(store);
        assert_eq!(
            CanonicalStore::open_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(FailOnce::at(recovery_boundary)),
            )
            .expect_err("recovery failpoint blocks readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{recovery_boundary:?}"
        );
        let reopened = CanonicalStore::open(directory.path(), owner()).expect("retry rollback");
        assert_eq!(
            reopened.watermark().expect("rollback watermark"),
            StoreRevision(0)
        );
        assert_eq!(
            reopened
                .get(&memory_id("mem_rollback_restart"), &authority)
                .expect_err("rollback never invents create success")
                .code(),
            StoreErrorCode::NotFound
        );
    }

    for recovery_boundary in [
        PersistenceBoundary::RecoveryQuarantineDirectorySynced,
        PersistenceBoundary::RecoveryQuarantineSourceDirectorySynced,
        PersistenceBoundary::RecoveryReceiptDirectorySynced,
    ] {
        let directory = TempDir::new().expect("quarantine recovery directory");
        let initial =
            CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
        let scope = MemoryScope::Session {
            session_id: session_id("ses_recovery_boundary"),
        };
        let id = memory_id("mem_recovery_boundary");
        write_raw_record(
            directory.path(),
            &scope,
            &id,
            include_bytes!("../fixtures/v1alpha1/invalid/truncated.md"),
        );
        drop(initial);
        let mut store = CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(FailOnce::at(
                PersistenceBoundary::QuarantineRenamed,
            )),
        )
        .expect("open store");
        store
            .quarantine_invalid(&scope, &id)
            .expect_err("leave renamed quarantine transaction");
        drop(store);
        assert_eq!(
            CanonicalStore::open_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(FailOnce::at(recovery_boundary)),
            )
            .expect_err("quarantine recovery failure blocks readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{recovery_boundary:?}"
        );
        let reopened = CanonicalStore::open(directory.path(), owner()).expect("retry quarantine");
        assert_eq!(
            reopened
                .pending_quarantine_receipts()
                .expect("recovered receipts")
                .len(),
            1
        );
        assert_eq!(
            reopened
                .pending_quarantine_receipts()
                .expect("recovered receipts")[0]
                .memory_id,
            id
        );
        assert_eq!(
            fs::read_dir(directory.path().join("transactions"))
                .expect("transaction directory")
                .count(),
            0,
            "{recovery_boundary:?}"
        );
    }
}

#[test]
fn quarantine_receipt_acknowledgement_is_restartable_and_poisons_every_service_entry() {
    let scenarios = [
        (
            PersistenceBoundary::QuarantineReceiptAcknowledgementRemoved,
            false,
        ),
        (
            PersistenceBoundary::QuarantineReceiptAcknowledgementRemoved,
            true,
        ),
        (
            PersistenceBoundary::QuarantineReceiptAcknowledgementDirectorySynced,
            false,
        ),
    ];
    for (boundary, restore_unflushed_receipt) in scenarios {
        let directory = TempDir::new().expect("temporary directory");
        let scope = MemoryScope::Principal {
            principal_id: principal_id("prn_ack_recovery"),
        };
        let authority = AuthorizedScopes::new(principal_id("prn_ack_recovery"));
        let create_authorization =
            authorized_mutation(&authority, &scope, MutationOperation::Create);
        let update_authorization =
            authorized_mutation(&authority, &scope, MutationOperation::Update);
        let mut initial =
            CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
        let valid = create_memory(
            &mut initial,
            &create_authorization,
            "mem_ack_valid",
            "stable canonical body",
        )
        .expect("create valid record");
        drop(initial);

        let invalid_id = memory_id("mem_ack_invalid");
        write_raw_record(
            directory.path(),
            &scope,
            &invalid_id,
            include_bytes!("../fixtures/v1alpha1/invalid/truncated.md"),
        );
        let mut quarantine_store =
            CanonicalStore::open(directory.path(), owner()).expect("open quarantine store");
        let receipt = quarantine_store
            .quarantine_invalid(&scope, &invalid_id)
            .expect("quarantine invalid record");
        drop(quarantine_store);

        let receipt_directory = directory.path().join(layout::QUARANTINE_RECEIPTS_DIR);
        let receipt_path = fs::read_dir(&receipt_directory)
            .expect("receipt directory")
            .next()
            .expect("one receipt")
            .expect("receipt entry")
            .path();
        let receipt_bytes = fs::read(&receipt_path).expect("durable receipt bytes");
        let quarantine_before = tree_snapshot(&directory.path().join("quarantine"));

        let injector = FailOnce::at(boundary);
        let mut store = CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_failpoint_injector(injector.clone()),
        )
        .expect("open acknowledgement store");
        assert_eq!(
            store
                .acknowledge_quarantine_receipt(&receipt.memory_id, &receipt.quarantine_token)
                .expect_err("acknowledgement boundary injects failure")
                .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert!(injector.fired.load(Ordering::SeqCst), "{boundary:?}");

        assert_recovery_required(
            store.pending_quarantine_receipts(),
            "poisoned receipt ledger read",
        );
        assert_recovery_required(store.watermark(), "poisoned watermark read");
        assert_recovery_required(store.doctor(), "poisoned doctor");
        assert_recovery_required(
            store.get(&valid.record.id, &authority),
            "poisoned exact read",
        );
        assert_recovery_required(
            store.list(
                &list_request(vec![ScopeSelector::Principal {}], 10),
                &authority,
            ),
            "poisoned list read",
        );
        assert_recovery_required(
            create_memory(
                &mut store,
                &create_authorization,
                "mem_ack_poisoned_create",
                "must not commit",
            ),
            "poisoned create",
        );
        assert_recovery_required(
            store.update(
                &update_authorization,
                &update_command(&valid.record.id, 1, "must not update"),
                timestamp("2026-08-24T02:01:00Z"),
            ),
            "poisoned update",
        );
        assert_recovery_required(
            store.quarantine_invalid(&scope, &invalid_id),
            "poisoned quarantine",
        );
        assert_recovery_required(
            store.acknowledge_quarantine_receipt(&receipt.memory_id, &receipt.quarantine_token),
            "poisoned acknowledgement retry",
        );
        drop(store);

        if restore_unflushed_receipt {
            assert_eq!(
                boundary,
                PersistenceBoundary::QuarantineReceiptAcknowledgementRemoved
            );
            fs::write(&receipt_path, &receipt_bytes)
                .expect("simulate rolled-back unflushed receipt deletion");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600))
                    .expect("restore private receipt permissions");
            }
        }

        let reopened =
            CanonicalStore::open(directory.path(), owner()).expect("reopen acknowledgement state");
        let expected_pending = usize::from(restore_unflushed_receipt);
        assert_eq!(
            reopened
                .pending_quarantine_receipts()
                .expect("reconstructed receipt ledger")
                .len(),
            expected_pending,
            "{boundary:?}, restore={restore_unflushed_receipt}"
        );
        assert_eq!(
            tree_snapshot(&directory.path().join("quarantine")),
            quarantine_before,
            "acknowledgement never deletes quarantined bytes"
        );
        drop(reopened);

        let reopened_again = CanonicalStore::open(directory.path(), owner())
            .expect("repeated acknowledgement recovery converges");
        assert_eq!(
            reopened_again
                .pending_quarantine_receipts()
                .expect("stable reconstructed receipt ledger")
                .len(),
            expected_pending
        );
        assert_eq!(
            tree_snapshot(&directory.path().join("quarantine")),
            quarantine_before
        );
    }
}

#[test]
fn recovery_fails_closed_for_impossible_record_metadata_combinations() {
    let directory = TempDir::new().expect("temporary directory");
    let injector = FailOnce::at(PersistenceBoundary::ManifestDirectorySynced);
    let mut store = CanonicalStore::initialize_with_options(
        directory.path(),
        owner(),
        StoreOptions::with_failpoint_injector(injector),
    )
    .expect("initialize store");
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_impossible_watermark"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_impossible_watermark"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    create_memory(
        &mut store,
        &authorization,
        "mem_impossible_watermark",
        "uncommitted body",
    )
    .expect_err("stop before staging record");
    drop(store);
    let manifest_path = fs::read_dir(directory.path().join("transactions"))
        .expect("transactions")
        .map(|entry| entry.expect("manifest entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("manifest");
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest bytes"))
            .expect("manifest JSON");
    let target_metadata: StoreMetadata =
        serde_json::from_value(manifest_value["intent"]["targetStoreMetadata"].clone())
            .expect("target metadata in manifest");
    fs::write(
        directory.path().join("store.json"),
        target_metadata
            .canonical_bytes()
            .expect("canonical target metadata"),
    )
    .expect("install impossible target watermark");
    assert_eq!(
        CanonicalStore::open(directory.path(), owner())
            .expect_err("target watermark with old record state is ambiguous")
            .code(),
        StoreErrorCode::InvalidTransaction
    );

    let unrelated = TempDir::new().expect("unrelated-state directory");
    let injector = FailOnce::at(PersistenceBoundary::RecordRenamed);
    let mut store = CanonicalStore::initialize_with_options(
        unrelated.path(),
        owner(),
        StoreOptions::with_failpoint_injector(injector),
    )
    .expect("initialize unrelated-state store");
    let scope = MemoryScope::Project {
        project_id: project_id("prj_unrelated_recovery"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_unrelated_recovery"))
        .with_project(project_id("prj_unrelated_recovery"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let id = memory_id("mem_unrelated_recovery");
    create_memory(
        &mut store,
        &authorization,
        id.as_str(),
        "transaction target",
    )
    .expect_err("stop after target rename");
    drop(store);
    let mut unrelated_header = frontmatter(
        id.as_str(),
        scope.clone(),
        "2026-08-24T02:00:00Z",
        "2026-08-24T02:03:00Z",
    );
    unrelated_header.revision = Revision::new(9).expect("positive unrelated revision");
    let path = layout::record_path(unrelated.path(), &scope, &id).expect("record path");
    fs::write(
        path,
        encode_canonical_document(&unrelated_header, "unrelated canonical bytes")
            .expect("unrelated canonical record"),
    )
    .expect("replace target with unrelated state");
    assert_eq!(
        CanonicalStore::open(unrelated.path(), owner())
            .expect_err("neither base nor target record fails closed")
            .code(),
        StoreErrorCode::InvalidTransaction
    );
}

#[test]
fn startup_and_doctor_verify_durability_or_fail_closed() {
    let directory = TempDir::new().expect("temporary directory");
    let mut store =
        CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    let report = store.doctor().expect("durability doctor passes");
    assert!(report.file_sync);
    assert!(report.same_filesystem_atomic_replace);
    assert_eq!(
        report.directory_durability,
        if cfg!(windows) {
            DirectoryDurability::PlatformDocumentedBestEffort
        } else {
            DirectoryDurability::ExplicitSync
        }
    );
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_doctor"),
    };
    let authority = AuthorizedScopes::new(principal_id("prn_doctor"));
    let authorization = authorized_mutation(&authority, &scope, MutationOperation::Create);
    let created = create_memory(
        &mut store,
        &authorization,
        "mem_doctor",
        "doctor preserves canonical bytes",
    )
    .expect("create doctor fixture");
    let record_path =
        layout::record_path(directory.path(), &scope, &created.record.id).expect("record path");
    let record_before = fs::read(&record_path).expect("record bytes before failed startup");
    let mtime_before = fs::metadata(&record_path)
        .expect("record metadata")
        .modified()
        .expect("record mtime");
    drop(store);
    let metadata_before = fs::read(directory.path().join("store.json")).expect("metadata before");
    assert_eq!(
        CanonicalStore::open_with_options(
            directory.path(),
            owner(),
            StoreOptions::with_forced_unsupported_durability("atomic replacement"),
        )
        .expect_err("unsupported startup capability fails before readiness")
        .code(),
        StoreErrorCode::UnsupportedDurability
    );
    assert_eq!(
        fs::read(directory.path().join("store.json")).expect("metadata after"),
        metadata_before
    );
    assert_eq!(
        fs::read(&record_path).expect("record bytes after failed startup"),
        record_before
    );
    assert_eq!(
        fs::metadata(&record_path)
            .expect("record metadata after")
            .modified()
            .expect("record mtime after"),
        mtime_before
    );
    assert_eq!(
        fs::read_dir(directory.path().join("transactions"))
            .expect("transaction directory")
            .count(),
        0
    );
}

#[test]
fn legacy_receipt_layout_migrates_idempotently_across_create_and_sync_failures() {
    for boundary in [
        PersistenceBoundary::QuarantineReceiptLayoutCreated,
        PersistenceBoundary::QuarantineReceiptLayoutDirectorySynced,
    ] {
        let directory = TempDir::new().expect("temporary directory");
        drop(CanonicalStore::initialize(directory.path(), owner()).expect("initialize store"));
        let receipt_layout = directory.path().join(layout::QUARANTINE_RECEIPTS_DIR);
        fs::remove_dir(&receipt_layout).expect("restore original v1alpha1 receipt layout");

        let injector = FailOnce::at(boundary);
        assert_eq!(
            CanonicalStore::open_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(injector.clone()),
            )
            .expect_err("layout migration boundary blocks readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert!(injector.fired.load(Ordering::SeqCst), "{boundary:?}");

        if boundary == PersistenceBoundary::QuarantineReceiptLayoutCreated {
            // Model the other legal power-loss outcome: the unsynced mkdir
            // rolls back instead of remaining visible after process restart.
            fs::remove_dir(&receipt_layout).expect("simulate rolled-back directory entry");
        }
        let reopened = CanonicalStore::open(directory.path(), owner())
            .expect("interrupted compatible migration converges");
        assert_eq!(
            reopened.watermark().expect("migrated watermark"),
            StoreRevision(0)
        );
        assert!(receipt_layout.is_dir(), "{boundary:?}");
        assert!(
            reopened
                .pending_quarantine_receipts()
                .expect("empty migrated receipt ledger")
                .is_empty()
        );
    }
}

#[test]
fn v1alpha1_to_v1alpha2_migration_is_restartable_at_every_boundary_and_gates_old_writers() {
    for &boundary in MIGRATION_PERSISTENCE_BOUNDARIES {
        let directory = TempDir::new().expect("temporary directory");
        let legacy = make_legacy_store(directory.path());
        let scope = MemoryScope::Project {
            project_id: project_id("prj_migration_preserved"),
        };
        let header = frontmatter(
            "mem_migration_preserved",
            scope,
            "2026-08-24T01:00:00Z",
            "2026-08-24T01:00:00Z",
        );
        let record_path = write_record(directory.path(), &header, " migration body \n");
        let record_bytes = fs::read(&record_path).expect("record before migration");
        let record_mtime = fs::metadata(&record_path)
            .expect("record metadata")
            .modified()
            .expect("record mtime");

        let injector = FailOnce::at(boundary);
        assert_eq!(
            CanonicalStore::migrate_v1alpha1_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(injector.clone()),
            )
            .expect_err("migration failpoint interrupts readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        assert!(injector.fired.load(Ordering::SeqCst), "{boundary:?}");

        let published = store_metadata(directory.path()).format_version == STORE_FORMAT_VERSION;
        let migrated = if published {
            assert_eq!(
                CanonicalStore::migrate_v1alpha1(directory.path(), owner())
                    .expect_err("old writer rejects the v1alpha2 capability gate")
                    .code(),
                StoreErrorCode::UnsupportedStoreFormat,
                "{boundary:?}"
            );
            CanonicalStore::open(directory.path(), owner()).expect("open published migration")
        } else {
            CanonicalStore::migrate_v1alpha1(directory.path(), owner())
                .expect("retry pre-publish migration")
        };
        assert_eq!(migrated.store_id(), &legacy.store_id, "{boundary:?}");
        assert_eq!(migrated.watermark().expect("watermark"), StoreRevision(0));
        assert_eq!(migrated.metadata.audit_sequence, AuditSequence(0));
        assert_eq!(
            fs::read(&record_path).expect("record after migration"),
            record_bytes,
            "{boundary:?}"
        );
        assert_eq!(
            fs::metadata(&record_path)
                .expect("record metadata after migration")
                .modified()
                .expect("record mtime after migration"),
            record_mtime,
            "{boundary:?}"
        );
        assert!(directory.path().join(layout::AUDIT_GENESIS_FILE).is_file());
        assert!(
            !directory
                .path()
                .join(layout::STORE_METADATA_MIGRATION_FILE)
                .exists()
        );
        assert!(regular_files_below(directory.path(), layout::MUTATION_AUDIT_DIR).is_empty());
        drop(migrated);
        assert_eq!(
            CanonicalStore::migrate_v1alpha1(directory.path(), owner())
                .expect_err("old writer stays gated after restart")
                .code(),
            StoreErrorCode::UnsupportedStoreFormat
        );
    }
}

#[test]
fn migration_recovers_a_legacy_record_transaction_before_publishing_the_v2_gate() {
    let directory = TempDir::new().expect("temporary directory");
    let legacy = make_legacy_store(directory.path());
    let scope = MemoryScope::Principal {
        principal_id: principal_id("prn_legacy_wal"),
    };
    let header = frontmatter(
        "mem_legacy_wal",
        scope.clone(),
        "2026-08-24T01:00:00Z",
        "2026-08-24T01:00:00Z",
    );
    let bytes =
        encode_canonical_document(&header, "legacy recovered body").expect("legacy target record");
    let record = decode_canonical_document(&bytes, Some(&header.id))
        .expect("decode target")
        .record;
    let mut target_metadata = legacy.clone();
    target_metadata.store_revision = StoreRevision(1);
    let transaction_id = crate::transaction::new_transaction_id();
    let manifest = crate::transaction::TransactionManifest {
        format_version: crate::transaction::LEGACY_TRANSACTION_FORMAT_VERSION.to_owned(),
        transaction_id,
        store_id: legacy.store_id.clone(),
        intent: crate::transaction::TransactionIntent::Record(Box::new(
            crate::transaction::RecordTransaction {
                operation: crate::transaction::RecordOperation::Create,
                memory_id: record.id.clone(),
                scope: scope.clone(),
                base_revision: None,
                base_etag: None,
                target_revision: record.revision,
                target_etag: record.etag.clone(),
                base_store_metadata: legacy,
                target_store_metadata: target_metadata,
                idempotency: None,
            },
        )),
    };
    let root = layout::StoreDirectory::open(directory.path(), false).expect("open legacy root");
    let failpoints =
        crate::failpoint::Failpoints::new(FailOnce::at(PersistenceBoundary::RecordRenamed));
    crate::transaction::persist_manifest(&root, &manifest, &failpoints)
        .expect("persist legacy manifest");
    let record_identity = crate::transaction::stage_record(&root, &manifest, &bytes, &failpoints)
        .expect("stage legacy record");
    let _metadata_identity = crate::transaction::stage_metadata(&root, &manifest, &failpoints)
        .expect("stage legacy metadata");
    assert_eq!(
        crate::transaction::publish_record(&root, &manifest, record_identity, None, &failpoints,)
            .expect_err("simulate legacy crash after record rename")
            .code(),
        StoreErrorCode::InjectedFailure
    );
    drop(root);

    let migrated = CanonicalStore::migrate_v1alpha1(directory.path(), owner())
        .expect("migration first recovers legacy transaction");
    assert_eq!(migrated.watermark().expect("watermark"), StoreRevision(1));
    assert_eq!(migrated.metadata.audit_sequence, AuditSequence(0));
    assert_eq!(
        migrated
            .get(
                &record.id,
                &AuthorizedScopes::new(principal_id("prn_legacy_wal")),
            )
            .expect("legacy target recovered")
            .result
            .body,
        "legacy recovered body"
    );
    assert!(regular_files_below(directory.path(), layout::IDEMPOTENCY_RECEIPTS_DIR).is_empty());
    assert!(regular_files_below(directory.path(), layout::IDEMPOTENCY_RESULTS_DIR).is_empty());
    assert!(regular_files_below(directory.path(), layout::MUTATION_AUDIT_DIR).is_empty());
    assert_eq!(
        fs::read_dir(directory.path().join("transactions"))
            .expect("transactions")
            .count(),
        0
    );
    let genesis_file =
        fs::File::open(directory.path().join(layout::AUDIT_GENESIS_FILE)).expect("audit genesis");
    let genesis = crate::idempotency::AuditGenesis::decode(genesis_file, migrated.store_id())
        .expect("decode audit genesis");
    assert_eq!(genesis.base_store_revision, StoreRevision(1));
}

#[test]
fn durability_probe_failpoints_cleanup_before_the_next_readiness() {
    for boundary in [
        PersistenceBoundary::DurabilityProbeFilesSynced,
        PersistenceBoundary::DurabilityProbeRenamed,
        PersistenceBoundary::DurabilityProbeDirectorySynced,
    ] {
        let directory = TempDir::new().expect("temporary directory");
        drop(CanonicalStore::initialize(directory.path(), owner()).expect("initialize store"));
        assert_eq!(
            CanonicalStore::open_with_options(
                directory.path(),
                owner(),
                StoreOptions::with_failpoint_injector(FailOnce::at(boundary)),
            )
            .expect_err("durability probe failpoint blocks readiness")
            .code(),
            StoreErrorCode::InjectedFailure,
            "{boundary:?}"
        );
        let reopened = CanonicalStore::open(directory.path(), owner())
            .expect("recognized doctor artifacts are cleaned and probe retries");
        assert!(reopened.doctor().expect("doctor succeeds").file_sync);
        assert_eq!(
            fs::read_dir(directory.path().join("transactions"))
                .expect("transaction directory")
                .count(),
            0,
            "{boundary:?}"
        );
    }
}

#[test]
fn every_persistence_boundary_has_a_crash_recovery_scenario() {
    use std::collections::BTreeSet;

    let mut covered: BTreeSet<_> = [
        PersistenceBoundary::ManifestTempWritten,
        PersistenceBoundary::ManifestTempSynced,
        PersistenceBoundary::ManifestTempDirectorySynced,
        PersistenceBoundary::ManifestPublished,
        PersistenceBoundary::ManifestDirectorySynced,
        PersistenceBoundary::RecordNamespacePrepared,
        PersistenceBoundary::RecordTempWritten,
        PersistenceBoundary::RecordTempSynced,
        PersistenceBoundary::RecordTempDirectorySynced,
        PersistenceBoundary::MetadataTempWritten,
        PersistenceBoundary::MetadataTempSynced,
        PersistenceBoundary::MetadataTempDirectorySynced,
        PersistenceBoundary::RecordRenamed,
        PersistenceBoundary::RecordDirectorySynced,
        PersistenceBoundary::MetadataRenamed,
        PersistenceBoundary::MetadataDirectorySynced,
        PersistenceBoundary::QuarantineRenamed,
        PersistenceBoundary::QuarantineDirectorySynced,
        PersistenceBoundary::QuarantineSourceDirectorySynced,
        PersistenceBoundary::QuarantineReceiptTempWritten,
        PersistenceBoundary::QuarantineReceiptTempSynced,
        PersistenceBoundary::QuarantineReceiptTempDirectorySynced,
        PersistenceBoundary::QuarantineReceiptPublished,
        PersistenceBoundary::QuarantineReceiptDirectorySynced,
        PersistenceBoundary::QuarantineReceiptAcknowledgementRemoved,
        PersistenceBoundary::QuarantineReceiptAcknowledgementDirectorySynced,
        PersistenceBoundary::QuarantineReceiptLayoutCreated,
        PersistenceBoundary::QuarantineReceiptLayoutDirectorySynced,
        PersistenceBoundary::ManifestRemoved,
        PersistenceBoundary::ManifestRemovalDirectorySynced,
        PersistenceBoundary::RecoveryRecordDirectorySynced,
        PersistenceBoundary::RecoveryMetadataDirectorySynced,
        PersistenceBoundary::RecoveryQuarantineDirectorySynced,
        PersistenceBoundary::RecoveryQuarantineSourceDirectorySynced,
        PersistenceBoundary::RecoveryReceiptDirectorySynced,
        PersistenceBoundary::RecoveryManifestDirectorySynced,
        PersistenceBoundary::DurabilityProbeFilesSynced,
        PersistenceBoundary::DurabilityProbeRenamed,
        PersistenceBoundary::DurabilityProbeDirectorySynced,
    ]
    .into_iter()
    .collect();
    covered.extend(MUTATION_PERSISTENCE_BOUNDARIES.iter().copied());
    covered.extend(RECOVERY_IDEMPOTENCY_BOUNDARIES.iter().copied());
    covered.extend(MIGRATION_PERSISTENCE_BOUNDARIES.iter().copied());
    let declared: BTreeSet<_> = PersistenceBoundary::ALL.iter().copied().collect();
    assert_eq!(covered, declared);
}

#[test]
fn v1alpha2_private_fixtures_match_strict_rust_codecs() {
    let store_id = StoreId::new("00000000-0000-4000-8000-000000000017").expect("store ID");
    let created_at = timestamp("2026-08-24T00:00:00Z");
    let base_metadata = StoreMetadata {
        format_version: STORE_FORMAT_VERSION.to_owned(),
        store_id: store_id.clone(),
        store_revision: StoreRevision(41),
        audit_sequence: AuditSequence(6),
        created_at: created_at.clone(),
    };
    let target_metadata = StoreMetadata {
        store_revision: StoreRevision(42),
        audit_sequence: AuditSequence(7),
        ..base_metadata.clone()
    };
    let scope = MemoryScope::Project {
        project_id: project_id("prj_fixture"),
    };
    let header = frontmatter(
        "mem_fixture",
        scope.clone(),
        "2026-08-24T01:00:00Z",
        "2026-08-24T01:00:00Z",
    );
    let record = decode_canonical_document(
        &encode_canonical_document(&header, "fixture body").expect("record bytes"),
        Some(&header.id),
    )
    .expect("record")
    .record;
    let binding = crate::idempotency::MutationBinding {
        receipt_id: "11".repeat(32),
        transaction_id: "00000000-0000-4000-8000-000000000017".to_owned(),
        principal_digest: format!("sha256:{}", "2".repeat(64)),
        key_digest: format!("sha256:{}", "3".repeat(64)),
        operation: MutationOperation::Create,
        scope: scope.clone(),
        request_fingerprint: format!("sha256:{}", "4".repeat(64)),
        memory_id: record.id.clone(),
        target_revision: record.revision,
        target_etag: record.etag.clone(),
        store_revision: StoreRevision(42),
        audit_sequence: AuditSequence(7),
    };
    let artifacts = crate::idempotency::MutationArtifacts::build(
        store_id.clone(),
        binding.clone(),
        None,
        record.clone(),
    )
    .expect("artifacts");
    let manifest = crate::transaction::TransactionManifest::for_record(
        store_id.clone(),
        binding.transaction_id.clone(),
        crate::transaction::RecordTransaction {
            operation: crate::transaction::RecordOperation::Create,
            memory_id: record.id.clone(),
            scope,
            base_revision: None,
            base_etag: None,
            target_revision: record.revision,
            target_etag: record.etag.clone(),
            base_store_metadata: base_metadata,
            target_store_metadata: target_metadata.clone(),
            idempotency: Some(crate::idempotency::IdempotencyTransaction {
                binding,
                result_digest: artifacts.result_digest.clone(),
                receipt_digest: artifacts.receipt_digest.clone(),
                audit_digest: artifacts.audit_digest.clone(),
            }),
        },
    )
    .expect("manifest");
    let genesis = crate::idempotency::AuditGenesis::new(store_id, StoreRevision(41));
    let decoded_metadata: StoreMetadata =
        serde_json::from_slice(include_bytes!("../fixtures/v1alpha2/store-metadata.json"))
            .expect("decode metadata fixture");
    assert_eq!(decoded_metadata, target_metadata);
    let decode_directory = TempDir::new().expect("fixture decode directory");
    let decode = |name: &str, bytes: &[u8]| {
        let path = decode_directory.path().join(name);
        fs::write(&path, bytes).expect("write fixture for strict decode");
        fs::File::open(path).expect("open fixture for strict decode")
    };
    assert_eq!(
        crate::idempotency::AuditGenesis::decode(
            decode(
                "audit-genesis.json",
                include_bytes!("../fixtures/v1alpha2/audit-genesis.json"),
            ),
            &target_metadata.store_id,
        )
        .expect("decode genesis fixture"),
        genesis
    );
    assert_eq!(
        crate::idempotency::DurableMutationResult::decode(
            decode(
                "mutation-result.json",
                include_bytes!("../fixtures/v1alpha2/mutation-result.json"),
            ),
            &target_metadata.store_id,
            &artifacts.result.binding,
        )
        .expect("decode result fixture"),
        artifacts.result
    );
    assert_eq!(
        crate::idempotency::DurableIdempotencyReceipt::decode(
            decode(
                "idempotency-receipt.json",
                include_bytes!("../fixtures/v1alpha2/idempotency-receipt.json"),
            ),
            &target_metadata.store_id,
            &artifacts.receipt.binding.receipt_id,
        )
        .expect("decode receipt fixture"),
        artifacts.receipt
    );
    assert_eq!(
        crate::idempotency::DurableAuditEvent::decode(
            decode(
                "mutation-audit.json",
                include_bytes!("../fixtures/v1alpha2/mutation-audit.json"),
            ),
            &target_metadata.store_id,
            AuditSequence(7),
        )
        .expect("decode audit fixture"),
        artifacts.audit
    );
    assert_eq!(
        crate::transaction::TransactionManifest::decode(
            decode(
                "record-transaction.json",
                include_bytes!("../fixtures/v1alpha2/record-transaction.json"),
            ),
            "00000000-0000-4000-8000-000000000017",
            &target_metadata.store_id,
        )
        .expect("decode manifest fixture"),
        manifest
    );
    for (name, bytes, fixture) in [
        (
            "store-metadata.json",
            target_metadata.canonical_bytes().expect("metadata"),
            include_bytes!("../fixtures/v1alpha2/store-metadata.json").as_slice(),
        ),
        (
            "audit-genesis.json",
            genesis.canonical_bytes().expect("genesis"),
            include_bytes!("../fixtures/v1alpha2/audit-genesis.json").as_slice(),
        ),
        (
            "mutation-result.json",
            artifacts.result_bytes,
            include_bytes!("../fixtures/v1alpha2/mutation-result.json").as_slice(),
        ),
        (
            "idempotency-receipt.json",
            artifacts.receipt_bytes,
            include_bytes!("../fixtures/v1alpha2/idempotency-receipt.json").as_slice(),
        ),
        (
            "mutation-audit.json",
            artifacts.audit_bytes,
            include_bytes!("../fixtures/v1alpha2/mutation-audit.json").as_slice(),
        ),
        (
            "record-transaction.json",
            manifest.canonical_bytes().expect("manifest"),
            include_bytes!("../fixtures/v1alpha2/record-transaction.json").as_slice(),
        ),
    ] {
        assert_eq!(bytes, fixture, "fixture drift: {name}");
    }
}
