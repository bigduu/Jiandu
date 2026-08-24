use super::*;
use crate::format::{IndexMetadata, build_documents};
use jiandu_core::{
    ClientId, CreationActor, Etag, ForgetMemoryCommand, Grant, IdempotencyKey, MemoryId,
    MemoryRecord, MemorySchema, MemoryScope, MemorySearchRequest, MemoryStatus, MemoryType,
    PageLimit, PrincipalId, ProjectId, Provenance, ProvenanceInput, RememberMemoryCommand,
    Revision, ScopeSelector, SessionId, StoreRevision, Tag, Timestamp, TrustedRequestContext,
};
use jiandu_store::{
    AuthorizedIndexAdmin, AuthorizedScopes, CanonicalIndexSnapshot, CanonicalStore, LockOwner,
    MutationOperation, StoreError, StoreErrorCode, StoreId, StoreWatermark,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Clone)]
struct FixtureReader {
    snapshot: CanonicalIndexSnapshot,
    current_revision: Cell<StoreRevision>,
}

impl FixtureReader {
    fn new(records: Vec<MemoryRecord>) -> Self {
        Self {
            snapshot: CanonicalIndexSnapshot {
                store_id: StoreId::new("00000000-0000-4000-8000-000000000006")
                    .expect("fixed store ID"),
                store_revision: StoreRevision(7),
                records,
            },
            current_revision: Cell::new(StoreRevision(7)),
        }
    }
}

impl CanonicalRecordReader for FixtureReader {
    fn read_index_snapshot(
        &self,
        _authorization: &AuthorizedIndexAdmin,
    ) -> Result<CanonicalIndexSnapshot, StoreError> {
        Ok(self.snapshot.clone())
    }

    fn current_store_watermark(&self) -> Result<(StoreId, StoreWatermark), StoreError> {
        Ok((self.snapshot.store_id.clone(), self.current_revision.get()))
    }
}

#[test]
fn rebuild_search_is_ranked_filtered_paginated_and_scope_safe() {
    let principal = principal_id("prn_index_a");
    let foreign = principal_id("prn_index_b");
    let project = ProjectId::new("prj_index_shared").expect("project ID");
    let records = vec![
        record(
            "mem_alpha_body",
            MemoryScope::Principal {
                principal_id: principal.clone(),
            },
            "Notes",
            "alpha appears in the body",
            MemoryType::Fact,
            &["search"],
        ),
        record(
            "mem_alpha_title",
            MemoryScope::Principal {
                principal_id: principal.clone(),
            },
            "Alpha decision",
            "ordinary body",
            MemoryType::Decision,
            &["search", "important"],
        ),
        record(
            "mem_alpha_project",
            MemoryScope::Project {
                project_id: project.clone(),
            },
            "Alpha project",
            "project body",
            MemoryType::Project,
            &["search"],
        ),
        record(
            "mem_alpha_foreign",
            MemoryScope::Principal {
                principal_id: foreign,
            },
            "Alpha foreign secret",
            "foreign-only-sentinel",
            MemoryType::Fact,
            &["search"],
        ),
    ];
    let reader = FixtureReader::new(records);
    let directory = TempDir::new().expect("temporary index directory");
    let index_directory = private_index_directory(directory.path());
    let index = LexicalIndex::new(&index_directory);
    let authority = AuthorizedScopes::new(principal.clone()).with_project(project.clone());
    let admin = index_admin(&authority, &principal);
    let report = index.rebuild(&reader, &admin).expect("rebuild succeeds");
    assert_eq!(report.watermark.document_count, 4);

    let principal_query = authority
        .authorize_index_query(&[ScopeSelector::Principal {}])
        .expect("principal query capability");
    let mut request = search_request("alpha", vec![ScopeSelector::Principal {}], 1);
    let key = CursorMacKey::new([0x42; 32]);
    let first = index
        .search(&reader, &principal_query, &request, &key)
        .expect("first page");
    assert_eq!(ids(&first), ["mem_alpha_title"]);
    assert!(first.has_more);
    assert_eq!(first.memories[0].score.get(), 1.0);

    let mut filtered = search_request("alpha", vec![ScopeSelector::Principal {}], 10);
    filtered.types = vec![MemoryType::Decision];
    filtered.statuses = vec![MemoryStatus::Active];
    filtered.tags = vec![Tag::new("important").expect("filter tag")];
    filtered.updated_after = Some(timestamp("2026-08-24T23:59:59Z"));
    assert_eq!(
        ids(&index
            .search(&reader, &principal_query, &filtered, &key)
            .expect("structured filters")),
        ["mem_alpha_title"]
    );

    let expanded_authority = authority
        .clone()
        .with_session(SessionId::new("ses_cursor_change").expect("session ID"));
    let changed_query = expanded_authority
        .authorize_index_query(&[ScopeSelector::Principal {}])
        .expect("changed query capability");
    let mut cursor_request = search_request("alpha", vec![ScopeSelector::Principal {}], 1);
    cursor_request.cursor = first.next_cursor.clone();
    assert!(matches!(
        index.search(&reader, &changed_query, &cursor_request, &key),
        Err(IndexError::InvalidCursor)
    ));

    request.cursor = first.next_cursor;
    let second = index
        .search(&reader, &principal_query, &request, &key)
        .expect("second page");
    assert_eq!(ids(&second), ["mem_alpha_body"]);
    assert!(!second.has_more);

    let project_query = authority
        .authorize_index_query(&[ScopeSelector::Project {
            project_id: project.clone(),
        }])
        .expect("project query capability");
    let project_result = index
        .search(
            &reader,
            &project_query,
            &search_request(
                "alpha",
                vec![ScopeSelector::Project {
                    project_id: project,
                }],
                10,
            ),
            &key,
        )
        .expect("project search");
    assert_eq!(ids(&project_result), ["mem_alpha_project"]);

    let no_leak = index
        .search(
            &reader,
            &principal_query,
            &search_request(
                "foreign-only-sentinel",
                vec![ScopeSelector::Principal {}],
                10,
            ),
            &key,
        )
        .expect("authorized empty search");
    assert!(no_leak.memories.is_empty());

    let wrong_scope_request = search_request("alpha", vec![ScopeSelector::Principal {}], 10);
    assert!(matches!(
        index.search(&reader, &project_query, &wrong_scope_request, &key),
        Err(IndexError::Forbidden)
    ));

    for empty_query in ["", "   ", "!!!"] {
        assert!(matches!(
            index.search(
                &reader,
                &principal_query,
                &search_request(empty_query, vec![ScopeSelector::Principal {}], 10,),
                &key,
            ),
            Err(IndexError::InvalidRequest)
        ));
    }

    let connection = Connection::open(directory.path().join("index/lexical.sqlite"))
        .expect("open derived index for corruption");
    connection
        .execute(
            "UPDATE documents SET value = x'00' WHERE memory_id = 'mem_alpha_foreign'",
            [],
        )
        .expect("corrupt foreign derived row");
    drop(connection);
    assert!(matches!(
        index.search(
            &reader,
            &principal_query,
            &search_request("alpha", vec![ScopeSelector::Principal {}], 10),
            &key,
        ),
        Err(IndexError::Degraded {
            reason: IndexDegradedReason::Corrupt
        })
    ));
}

#[test]
fn equal_scores_use_memory_id_ascending_as_the_final_tie_breaker() {
    let principal = principal_id("prn_tie_break");
    let scope = MemoryScope::Principal {
        principal_id: principal.clone(),
    };
    let reader = FixtureReader::new(vec![
        record(
            "mem_tie_z",
            scope.clone(),
            "equal alpha",
            "same body",
            MemoryType::Fact,
            &[],
        ),
        record(
            "mem_tie_a",
            scope,
            "equal alpha",
            "same body",
            MemoryType::Fact,
            &[],
        ),
    ]);
    let root = TempDir::new().expect("temporary root");
    let index_directory = private_index_directory(root.path());
    let index = LexicalIndex::new(&index_directory);
    let authority = AuthorizedScopes::new(principal.clone());
    let admin = index_admin(&authority, &principal);
    index.rebuild(&reader, &admin).expect("rebuild");
    let query = authority
        .authorize_index_query(&[ScopeSelector::Principal {}])
        .expect("query capability");
    let key = CursorMacKey::new([0x61; 32]);
    let result = index
        .search(
            &reader,
            &query,
            &search_request("alpha", vec![ScopeSelector::Principal {}], 10),
            &key,
        )
        .expect("tied search");
    assert_eq!(ids(&result), ["mem_tie_a", "mem_tie_z"]);
    assert_eq!(result.memories[0].score, result.memories[1].score);
    let before_delete = collect_principal_pages(&index, &reader, &query, &key);

    fs::remove_file(root.path().join("index/lexical.sqlite"))
        .expect("delete complete disposable index");
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("missing after delete")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Missing
        }
    );
    index
        .rebuild(&reader, &admin)
        .expect("rebuild after complete deletion");
    assert_eq!(
        collect_principal_pages(&index, &reader, &query, &key),
        before_delete
    );
}

#[test]
fn rebuild_and_query_capabilities_are_independently_authorized_and_bounded() {
    let principal = principal_id("prn_capabilities");
    let foreign = principal_id("prn_capabilities_foreign");
    let authority = AuthorizedScopes::new(principal.clone());
    let context = |principal_id: PrincipalId, grant: &str| TrustedRequestContext {
        principal_id,
        client_id: ClientId::new("cli_capability_tests").expect("client ID"),
        grants: BTreeSet::from([Grant::new(grant).expect("grant")]),
    };
    assert_eq!(
        authority
            .authorize_index_rebuild(&context(principal.clone(), "memory:admin:export_all"))
            .expect_err("export grant is not rebuild grant")
            .code(),
        StoreErrorCode::Forbidden
    );
    assert_eq!(
        authority
            .authorize_index_rebuild(&context(foreign, "memory:admin:rebuild_index"))
            .expect_err("foreign principal cannot rebuild")
            .code(),
        StoreErrorCode::Forbidden
    );
    assert_eq!(
        authority
            .authorize_index_query(&[])
            .expect_err("empty selector set")
            .code(),
        StoreErrorCode::InvalidRequest
    );
    assert_eq!(
        authority
            .authorize_index_query(&[ScopeSelector::Principal {}, ScopeSelector::Principal {},])
            .expect_err("duplicate selector set")
            .code(),
        StoreErrorCode::InvalidRequest
    );
    assert_eq!(
        authority
            .authorize_index_query(&[ScopeSelector::Project {
                project_id: ProjectId::new("prj_not_authorized").expect("project ID"),
            }])
            .expect_err("unauthorized project selector")
            .code(),
        StoreErrorCode::Forbidden
    );
}

#[test]
fn diagnostics_expose_source_unavailable_without_touching_the_index() {
    struct UnavailableReader;

    impl CanonicalRecordReader for UnavailableReader {
        fn read_index_snapshot(
            &self,
            _authorization: &AuthorizedIndexAdmin,
        ) -> Result<CanonicalIndexSnapshot, StoreError> {
            Err(StoreError::RecoveryRequired)
        }

        fn current_store_watermark(&self) -> Result<(StoreId, StoreWatermark), StoreError> {
            Err(StoreError::RecoveryRequired)
        }
    }

    let root = TempDir::new().expect("temporary root");
    let index = LexicalIndex::new(root.path().join("index"));
    let principal = principal_id("prn_unavailable_diagnostics");
    let authority = AuthorizedScopes::new(principal.clone());
    let admin = index_admin(&authority, &principal);
    assert_eq!(
        index
            .diagnose(&UnavailableReader, &admin)
            .expect("path-free source diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::SourceUnavailable
        }
    );
    assert!(!root.path().join("index").exists());
}

#[test]
fn index_format_fixture_is_canonical_and_drift_checked() {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture<'a> {
        fixture_format_version: &'static str,
        metadata: &'a IndexMetadata,
        documents: &'a [crate::format::IndexDocument],
    }

    let store_id = StoreId::new("00000000-0000-4000-8000-000000000006").expect("store ID");
    let documents = build_documents(vec![record(
        "mem_fixture",
        MemoryScope::Principal {
            principal_id: principal_id("prn_fixture"),
        },
        "项目 Alpha",
        "Deterministic CJK 记忆 body",
        MemoryType::Decision,
        &["fixture", "stable"],
    )])
    .expect("fixture document");
    let metadata =
        IndexMetadata::new(store_id, StoreRevision(7), &documents).expect("fixture metadata");
    let mut actual = serde_json::to_vec_pretty(&Fixture {
        fixture_format_version: "jiandu.index.snapshot-fixture/v1alpha1",
        metadata: &metadata,
        documents: &documents,
    })
    .expect("fixture JSON");
    actual.push(b'\n');
    assert_eq!(
        String::from_utf8(actual).expect("fixture UTF-8"),
        include_str!("../fixtures/v1alpha1/index-snapshot.json")
    );
}

#[test]
fn rebuild_is_byte_deterministic_and_missing_corrupt_incompatible_stale_are_degraded() {
    let principal = principal_id("prn_deterministic");
    let reader = FixtureReader::new(vec![record(
        "mem_deterministic",
        MemoryScope::Principal {
            principal_id: principal.clone(),
        },
        "项目 Alpha",
        "same canonical body",
        MemoryType::Reference,
        &["stable"],
    )]);
    let directory = TempDir::new().expect("temporary index directory");
    let index_directory = private_index_directory(directory.path());
    let path = index_directory.join("lexical.sqlite");
    let index = LexicalIndex::new(&index_directory);
    let authority = AuthorizedScopes::new(principal.clone());
    let admin = index_admin(&authority, &principal);

    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("missing diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Missing
        }
    );
    index.rebuild(&reader, &admin).expect("first rebuild");
    let first = fs::read(&path).expect("first index bytes");
    assert_eq!(
        test_hex(&Sha256::digest(&first)),
        "e327505abc1831939c0c13cf5796721fd3eaf0713b88b819d8f06f51d5705293"
    );
    index.rebuild(&reader, &admin).expect("replacement rebuild");
    let second = fs::read(&path).expect("second index bytes");
    assert_eq!(first, second, "fresh rebuilds must be byte-identical");
    assert!(matches!(
        index
            .diagnose(&reader, &admin)
            .expect("ready diagnostic")
            .health,
        IndexHealth::Ready(_)
    ));
    let restarted = LexicalIndex::new(&index_directory);
    assert!(matches!(
        restarted
            .diagnose(&reader, &admin)
            .expect("restart diagnostic")
            .health,
        IndexHealth::Ready(_)
    ));

    let connection = Connection::open(&path).expect("open index for schema tamper");
    connection
        .execute_batch("ALTER TABLE documents ADD COLUMN extra TEXT;")
        .expect("add unknown column");
    drop(connection);
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("extra-column diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Corrupt
        }
    );
    index
        .rebuild(&reader, &admin)
        .expect("rebuild altered schema");

    let connection = Connection::open(&path).expect("open index for constraint tamper");
    connection
        .execute_batch(
            "ALTER TABLE index_metadata RENAME TO old_index_metadata;
             CREATE TABLE index_metadata (
                 singleton INTEGER PRIMARY KEY,
                 value BLOB NOT NULL
             );
             INSERT INTO index_metadata SELECT * FROM old_index_metadata;
             DROP TABLE old_index_metadata;",
        )
        .expect("remove strict/check constraints");
    drop(connection);
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("constraint diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Corrupt
        }
    );
    index
        .rebuild(&reader, &admin)
        .expect("rebuild altered constraints");

    let connection = Connection::open(&path).expect("open index for index tamper");
    connection
        .execute_batch(
            "DROP INDEX documents_by_scope;
             CREATE INDEX documents_by_scope ON documents(memory_id, scope_key);",
        )
        .expect("alter derived lookup index");
    drop(connection);
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("altered-index diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Corrupt
        }
    );
    index
        .rebuild(&reader, &admin)
        .expect("rebuild altered index");

    let connection = Connection::open(&path).expect("open index for version tamper");
    connection
        .execute_batch("PRAGMA user_version=99;")
        .expect("tamper index version");
    drop(connection);
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("version diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::IncompatibleVersion
        }
    );
    index
        .rebuild(&reader, &admin)
        .expect("rebuild incompatible index");

    reader.current_revision.set(StoreRevision(8));
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("stale diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Stale
        }
    );
    reader.current_revision.set(StoreRevision(7));
    let mut bytes = fs::read(&path).expect("index bytes");
    bytes.truncate(bytes.len() / 2);
    fs::write(&path, bytes).expect("truncate derived index");
    assert_eq!(
        index
            .diagnose(&reader, &admin)
            .expect("corrupt diagnostic")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Corrupt
        }
    );
}

#[test]
fn canonical_store_rebuild_forget_and_degraded_index_leave_exact_reads_independent() {
    let store_directory = TempDir::new().expect("temporary store");
    let index_directory = TempDir::new().expect("temporary index");
    let principal = principal_id("prn_real_store");
    let project = ProjectId::new("prj_real_store").expect("project ID");
    let principal_scope = MemoryScope::Principal {
        principal_id: principal.clone(),
    };
    let project_scope = MemoryScope::Project {
        project_id: project.clone(),
    };
    let authority = AuthorizedScopes::new(principal.clone()).with_project(project.clone());
    let mut store = CanonicalStore::initialize(
        store_directory.path(),
        LockOwner::for_current_process().expect("lock owner"),
    )
    .expect("initialize store");
    create_store_record(
        &mut store,
        &authority,
        &principal,
        &principal_scope,
        "mem_real_principal",
        "Principal alpha body",
    );
    create_store_record(
        &mut store,
        &authority,
        &principal,
        &principal_scope,
        "mem_real_principal_b",
        "Secondary alpha body",
    );
    create_store_record(
        &mut store,
        &authority,
        &principal,
        &project_scope,
        "mem_real_project",
        "Project alpha body",
    );

    let derived_directory = private_index_directory(index_directory.path());
    let index = LexicalIndex::new(&derived_directory);
    let admin = index_admin(&authority, &principal);
    let report = index.rebuild(&store, &admin).expect("real store rebuild");
    assert_eq!(report.watermark.document_count, 3);
    let query_authority = authority
        .authorize_index_query(&[ScopeSelector::Principal {}])
        .expect("query authority");
    let key = CursorMacKey::new([0x51; 32]);
    let first_page = index
        .search(
            &store,
            &query_authority,
            &search_request("alpha", vec![ScopeSelector::Principal {}], 1),
            &key,
        )
        .expect("real store search");
    assert_eq!(ids(&first_page), ["mem_real_principal"]);
    let old_cursor = first_page.next_cursor.expect("second principal page");

    let index_path = derived_directory.join("lexical.sqlite");
    let mut damaged = fs::read(&index_path).expect("index bytes");
    damaged.truncate(damaged.len() / 2);
    fs::write(&index_path, damaged).expect("damage only derived index");
    assert!(matches!(
        index.search(
            &store,
            &query_authority,
            &search_request("alpha", vec![ScopeSelector::Principal {}], 10),
            &key,
        ),
        Err(IndexError::Degraded {
            reason: IndexDegradedReason::Corrupt
        })
    ));
    let exact = store
        .get(
            &MemoryId::new("mem_real_principal").expect("memory ID"),
            &authority,
        )
        .expect("exact store read is independent");
    assert_eq!(exact.result.body, "Principal alpha body");

    index.rebuild(&store, &admin).expect("repair derived index");
    let forget_authorization = mutation_authority(
        &authority,
        &principal,
        &principal_scope,
        MutationOperation::Forget,
        "memory:forget:principal",
    );
    store
        .forget(
            &forget_authorization,
            &ForgetMemoryCommand {
                memory_id: MemoryId::new("mem_real_principal").expect("memory ID"),
                expected_revision: Revision::new(1).expect("revision"),
                reason: "delete rebuild coverage".to_owned(),
                idempotency_key: IdempotencyKey::new("forget-real-principal")
                    .expect("idempotency key"),
            },
            timestamp("2026-08-25T00:01:00Z"),
        )
        .expect("forget record");
    assert_eq!(
        store
            .get(
                &MemoryId::new("mem_real_principal").expect("memory ID"),
                &authority,
            )
            .expect_err("forgotten exact read")
            .code(),
        StoreErrorCode::NotFound
    );
    assert_eq!(
        index
            .diagnose(&store, &admin)
            .expect("stale after forget")
            .health,
        IndexHealth::Degraded {
            reason: IndexDegradedReason::Stale
        }
    );
    index.rebuild(&store, &admin).expect("rebuild after forget");
    let mut stale_cursor_request = search_request("alpha", vec![ScopeSelector::Principal {}], 1);
    stale_cursor_request.cursor = Some(old_cursor);
    assert!(matches!(
        index.search(&store, &query_authority, &stale_cursor_request, &key),
        Err(IndexError::StaleCursor)
    ));
    assert_eq!(
        ids(&index
            .search(
                &store,
                &query_authority,
                &search_request("alpha", vec![ScopeSelector::Principal {}], 10),
                &key,
            )
            .expect("post-forget search")),
        ["mem_real_principal_b"]
    );
}

#[cfg(unix)]
#[test]
fn rebuild_never_chmods_shared_parent_follows_symlink_parent_or_overwrites_other_names() {
    use crate::directory::{TestHookPoint, install_test_hook};
    use std::os::unix::fs::{PermissionsExt, symlink};

    let principal = principal_id("prn_path_safety");
    let reader = FixtureReader::new(Vec::new());
    let authority = AuthorizedScopes::new(principal.clone());
    let admin = index_admin(&authority, &principal);
    let root = TempDir::new().expect("temporary root");

    let missing = root.path().join("missing-index");
    assert!(matches!(
        LexicalIndex::new(&missing).rebuild(&reader, &admin),
        Err(IndexError::InvalidRequest)
    ));
    assert!(
        !missing.exists(),
        "index never creates an ambient directory"
    );

    let shared = root.path().join("shared");
    fs::create_dir(&shared).expect("shared directory");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("shared permissions");
    let shared_index = LexicalIndex::new(&shared);
    assert!(matches!(
        shared_index.rebuild(&reader, &admin),
        Err(IndexError::InvalidRequest)
    ));
    assert_eq!(
        fs::metadata(&shared)
            .expect("shared metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );

    let private = root.path().join("private-index");
    fs::create_dir(&private).expect("private directory");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("private permissions");
    let link = root.path().join("index-link");
    symlink(&private, &link).expect("directory symlink");
    assert!(matches!(
        LexicalIndex::new(&link).rebuild(&reader, &admin),
        Err(IndexError::InvalidRequest)
    ));

    let unrelated = private.join("important-private-file");
    fs::write(&unrelated, b"must remain byte-identical").expect("unrelated file");
    fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o600))
        .expect("unrelated permissions");
    LexicalIndex::new(&private)
        .rebuild(&reader, &admin)
        .expect("fixed-name rebuild");
    assert_eq!(
        fs::read(&unrelated).expect("unrelated bytes"),
        b"must remain byte-identical"
    );

    let fixed_target = private.join("lexical.sqlite");
    let external = root.path().join("external-private-file");
    fs::write(&external, b"external bytes must survive").expect("external file");
    fs::set_permissions(&external, fs::Permissions::from_mode(0o600))
        .expect("external permissions");
    fs::remove_file(&fixed_target).expect("remove disposable target");
    symlink(&external, &fixed_target).expect("index-file symlink");
    assert!(matches!(
        LexicalIndex::new(&private).rebuild(&reader, &admin),
        Err(IndexError::Degraded {
            reason: IndexDegradedReason::Corrupt
        })
    ));
    assert_eq!(
        fs::read(&external).expect("external bytes"),
        b"external bytes must survive"
    );

    fs::remove_file(&fixed_target).expect("remove symlink only");
    fs::hard_link(&external, &fixed_target).expect("index-file hardlink");
    assert!(matches!(
        LexicalIndex::new(&private).rebuild(&reader, &admin),
        Err(IndexError::Degraded {
            reason: IndexDegradedReason::Corrupt
        })
    ));
    assert_eq!(
        fs::read(&external).expect("hardlinked external bytes"),
        b"external bytes must survive"
    );

    let raced = root.path().join("raced-index");
    fs::create_dir(&raced).expect("race directory");
    fs::set_permissions(&raced, fs::Permissions::from_mode(0o700))
        .expect("race directory permissions");
    let displaced = root.path().join("displaced-index");
    let external_directory = root.path().join("external-index-directory");
    fs::create_dir(&external_directory).expect("external directory");
    fs::set_permissions(&external_directory, fs::Permissions::from_mode(0o700))
        .expect("external directory permissions");
    let external_index = external_directory.join("lexical.sqlite");
    fs::write(&external_index, b"external index bytes must survive").expect("external index bytes");
    fs::set_permissions(&external_index, fs::Permissions::from_mode(0o600))
        .expect("external index permissions");
    let raced_for_hook = raced.clone();
    let displaced_for_hook = displaced.clone();
    let external_directory_for_hook = external_directory.clone();
    install_test_hook(TestHookPoint::AfterDirectoryOpen, move || {
        fs::rename(&raced_for_hook, &displaced_for_hook).expect("displace opened directory");
        symlink(&external_directory_for_hook, &raced_for_hook)
            .expect("replace ambient directory with symlink");
    });
    assert!(matches!(
        LexicalIndex::new(&raced).rebuild(&reader, &admin),
        Err(IndexError::InvalidRequest)
    ));
    assert_eq!(
        fs::read(&external_index).expect("external index remains readable"),
        b"external index bytes must survive"
    );

    let target_race = root.path().join("target-race-index");
    fs::create_dir(&target_race).expect("target race directory");
    fs::set_permissions(&target_race, fs::Permissions::from_mode(0o700))
        .expect("target race directory permissions");
    let target_race_index = LexicalIndex::new(&target_race);
    target_race_index
        .rebuild(&reader, &admin)
        .expect("initial target race image");
    let target_race_path = target_race.join("lexical.sqlite");
    let target_race_path_for_hook = target_race_path.clone();
    let external_for_hook = external.clone();
    install_test_hook(TestHookPoint::BeforePublish, move || {
        fs::remove_file(&target_race_path_for_hook).expect("remove original target");
        symlink(&external_for_hook, &target_race_path_for_hook).expect("race target symlink");
    });
    assert!(matches!(
        target_race_index.rebuild(&reader, &admin),
        Err(IndexError::Degraded {
            reason: IndexDegradedReason::Corrupt
        })
    ));
    assert_eq!(
        fs::read(&external).expect("raced external bytes"),
        b"external bytes must survive"
    );
    assert!(
        fs::read_dir(&target_race)
            .expect("target race entries")
            .all(|entry| !entry
                .expect("target race entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".lexical-"))
    );
}

fn private_index_directory(root: &Path) -> PathBuf {
    let directory = root.join("index");
    fs::create_dir(&directory).expect("create private index directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("set private index directory permissions");
    }
    directory
}

fn record(
    id: &str,
    scope: MemoryScope,
    title: &str,
    body: &str,
    memory_type: MemoryType,
    tags: &[&str],
) -> MemoryRecord {
    MemoryRecord {
        schema: MemorySchema::V1Alpha1,
        id: MemoryId::new(id).expect("memory ID"),
        revision: Revision::new(1).expect("revision"),
        etag: Etag::new(format!("etag-{id}")).expect("etag"),
        scope,
        memory_type,
        status: MemoryStatus::Active,
        title: title.to_owned(),
        summary: Some(format!("summary for {id}")),
        body: body.to_owned(),
        tags: tags
            .iter()
            .map(|tag| Tag::new(*tag).expect("tag"))
            .collect(),
        created_at: timestamp("2026-08-25T00:00:00Z"),
        updated_at: timestamp("2026-08-25T00:00:00Z"),
        provenance: Provenance {
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

fn search_request(query: &str, scopes: Vec<ScopeSelector>, limit: u16) -> MemorySearchRequest {
    MemorySearchRequest {
        query: query.to_owned(),
        scopes,
        types: Vec::new(),
        statuses: Vec::new(),
        tags: Vec::new(),
        updated_after: None,
        limit: PageLimit::new(limit).expect("page limit"),
        cursor: None,
    }
}

fn index_admin(authority: &AuthorizedScopes, principal: &PrincipalId) -> AuthorizedIndexAdmin {
    authority
        .authorize_index_rebuild(&TrustedRequestContext {
            principal_id: principal.clone(),
            client_id: ClientId::new("cli_index_tests").expect("client ID"),
            grants: BTreeSet::from(
                [Grant::new("memory:admin:rebuild_index").expect("index grant")],
            ),
        })
        .expect("index admin capability")
}

fn create_store_record(
    store: &mut CanonicalStore,
    authority: &AuthorizedScopes,
    principal: &PrincipalId,
    scope: &MemoryScope,
    id: &str,
    body: &str,
) {
    let grant = match scope {
        MemoryScope::Principal { .. } => "memory:write:principal",
        MemoryScope::Project { .. } => "memory:write:project",
        MemoryScope::Session { .. } => "memory:write:session",
        MemoryScope::InstanceGlobal {} => "memory:write:instance_global",
    };
    let authorization = mutation_authority(
        authority,
        principal,
        scope,
        MutationOperation::Create,
        grant,
    );
    store
        .create(
            &authorization,
            &RememberMemoryCommand {
                scope: selector(scope),
                memory_type: MemoryType::Decision,
                title: format!("Alpha title for {id}"),
                summary: Some(format!("Summary for {id}")),
                body: body.to_owned(),
                tags: vec![Tag::new("real-store").expect("tag")],
                provenance: ProvenanceInput::default(),
                relations: Vec::new(),
                idempotency_key: IdempotencyKey::new(format!("create-{id}"))
                    .expect("idempotency key"),
            },
            MemoryId::new(id).expect("memory ID"),
            CreationActor::Host,
            timestamp("2026-08-25T00:00:00Z"),
        )
        .expect("create canonical record");
}

fn mutation_authority(
    authority: &AuthorizedScopes,
    principal: &PrincipalId,
    scope: &MemoryScope,
    operation: MutationOperation,
    grant: &str,
) -> jiandu_store::AuthorizedMutation {
    authority
        .authorize_mutation(
            &TrustedRequestContext {
                principal_id: principal.clone(),
                client_id: ClientId::new("cli_index_mutations").expect("client ID"),
                grants: BTreeSet::from([Grant::new(grant).expect("mutation grant")]),
            },
            scope,
            operation,
        )
        .expect("mutation authorization")
}

fn selector(scope: &MemoryScope) -> ScopeSelector {
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

fn principal_id(value: &str) -> PrincipalId {
    PrincipalId::new(value).expect("principal ID")
}

fn timestamp(value: &str) -> Timestamp {
    Timestamp::new(value).expect("timestamp")
}

fn ids(result: &jiandu_core::MemorySearchResult) -> Vec<&str> {
    result
        .memories
        .iter()
        .map(|memory| memory.id.as_str())
        .collect()
}

fn collect_principal_pages<R: CanonicalRecordReader>(
    index: &LexicalIndex,
    reader: &R,
    authorization: &jiandu_store::AuthorizedIndexQuery,
    key: &CursorMacKey,
) -> Vec<String> {
    let mut request = search_request("alpha", vec![ScopeSelector::Principal {}], 1);
    let mut output = Vec::new();
    loop {
        let page = index
            .search(reader, authorization, &request, key)
            .expect("deterministic page");
        output.extend(
            page.memories
                .iter()
                .map(|memory| memory.id.as_str().to_owned()),
        );
        if !page.has_more {
            break;
        }
        request.cursor = page.next_cursor;
    }
    output
}

fn test_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
