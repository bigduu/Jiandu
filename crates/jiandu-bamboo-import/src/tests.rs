use super::*;
use jiandu_core::{
    ClientId, ForgetMemoryCommand, Grant, ProvenanceInput, RememberMemoryCommand, ScopeSelector,
    StoreRevision,
};
use jiandu_store::{AuthorizedScopes, LockOwner, MutationOperation};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

const PRIVATE_BODY: &str =
    "Prefer evidence-backed reviews that distinguish blocking defects from adjacent debt.";

#[derive(Debug, Eq, PartialEq)]
struct TreeEntry {
    kind: &'static str,
    bytes: Option<Vec<u8>>,
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    links: u64,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../jiandu-core/fixtures/migration/bamboo-memory/v1alpha1")
}

fn copied_fixture() -> TempDir {
    let destination = TempDir::new().expect("temporary snapshot copy");
    copy_tree(&fixture_root(), destination.path());
    destination
}

fn copy_tree(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).expect("fixture metadata");
        assert!(
            !metadata.file_type().is_symlink(),
            "fixture must be regular"
        );
        if metadata.is_dir() {
            fs::create_dir(&destination_path).expect("create fixture directory");
            copy_tree(&source_path, &destination_path);
        } else {
            assert!(metadata.is_file(), "fixture must be regular");
            fs::copy(&source_path, &destination_path).expect("copy fixture file");
        }
    }
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn visit(root: &Path, relative: &Path, output: &mut BTreeMap<PathBuf, TreeEntry>) {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).expect("tree metadata");
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            "special"
        };
        output.insert(
            if relative.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                relative.to_owned()
            },
            TreeEntry {
                kind,
                bytes: file_type
                    .is_file()
                    .then(|| fs::read(&path).expect("tree file")),
                len: metadata.len(),
                modified: metadata.modified().ok(),
                readonly: metadata.permissions().readonly(),
                links: link_count(&metadata),
            },
        );
        if file_type.is_dir() {
            let mut names = fs::read_dir(&path)
                .expect("tree directory")
                .map(|entry| entry.expect("tree entry").file_name())
                .collect::<Vec<_>>();
            names.sort();
            for name in names {
                visit(root, &relative.join(name), output);
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(root, Path::new(""), &mut output);
    output
}

#[cfg(unix)]
fn link_count(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(not(unix))]
fn link_count(_metadata: &fs::Metadata) -> u64 {
    1
}

fn owner() -> LockOwner {
    LockOwner::for_current_process().expect("valid test lock owner")
}

fn operator() -> (AuthorizedScopes, TrustedRequestContext) {
    let principal = PrincipalId::new("prn_fixture_owner").expect("fixture principal");
    let authority = AuthorizedScopes::new(principal.clone())
        .with_project(ProjectId::new("prj_fixture_alpha").expect("fixture project"))
        .with_session(SessionId::new("ses_fixture_1").expect("fixture session"));
    let context = TrustedRequestContext {
        principal_id: principal,
        client_id: ClientId::new("cli_bamboo_snapshot_import_tests").expect("client ID"),
        grants: [
            "memory:import:principal",
            "memory:import:project",
            "memory:admin:validate_store",
            "memory:admin:export_all",
            "memory:write:principal",
            "memory:forget:principal",
        ]
        .into_iter()
        .map(|grant| Grant::new(grant).expect("valid grant"))
        .collect(),
    };
    (authority, context)
}

fn initialize_store() -> (TempDir, CanonicalStore) {
    let directory = TempDir::new().expect("temporary Jiandu store");
    let store = CanonicalStore::initialize(directory.path(), owner()).expect("initialize store");
    (directory, store)
}

fn memory_id(value: &str) -> MemoryId {
    MemoryId::new(value).expect("fixture memory ID")
}

fn timestamp(value: &str) -> Timestamp {
    Timestamp::new(value).expect("fixture timestamp")
}

fn create_principal_record(
    store: &mut CanonicalStore,
    authority: &AuthorizedScopes,
    context: &TrustedRequestContext,
    id: &str,
) {
    let scope = MemoryScope::Principal {
        principal_id: context.principal_id.clone(),
    };
    let authorization = authority
        .authorize_mutation(context, &scope, MutationOperation::Create)
        .expect("create authorization");
    let command = RememberMemoryCommand {
        scope: ScopeSelector::Principal {},
        memory_type: MemoryType::Fact,
        title: format!("Unrelated target {id}"),
        summary: None,
        body: "unrelated target record".to_owned(),
        tags: Vec::new(),
        provenance: ProvenanceInput::default(),
        relations: Vec::new(),
        idempotency_key: IdempotencyKey::new(format!("create-{id}"))
            .expect("create idempotency key"),
    };
    store
        .create(
            &authorization,
            &command,
            memory_id(id),
            CreationActor::Host,
            timestamp("2026-08-26T00:00:00Z"),
        )
        .expect("create target record");
}

fn forget_principal_record(
    store: &mut CanonicalStore,
    authority: &AuthorizedScopes,
    context: &TrustedRequestContext,
    id: &str,
) {
    let scope = MemoryScope::Principal {
        principal_id: context.principal_id.clone(),
    };
    let authorization = authority
        .authorize_mutation(context, &scope, MutationOperation::Forget)
        .expect("forget authorization");
    store
        .forget(
            &authorization,
            &ForgetMemoryCommand {
                memory_id: memory_id(id),
                expected_revision: Revision::new(1).expect("revision one"),
                reason: "protect imported identity".to_owned(),
                idempotency_key: IdempotencyKey::new(format!("forget-{id}"))
                    .expect("forget idempotency key"),
            },
            timestamp("2026-08-26T00:01:00Z"),
        )
        .expect("forget target record");
}

fn rewrite_identity_without_projects(snapshot: &Path) {
    let identity_path = snapshot.join("expected/host-identity-map.json");
    let identity_bytes = fs::read(&identity_path).expect("identity map bytes");
    let mut identity: HostIdentityMap =
        serde_json::from_slice(&identity_bytes).expect("identity map");
    identity.projects.clear();
    let mut identity_bytes = serde_json::to_vec_pretty(&identity).expect("canonical identity map");
    identity_bytes.push(b'\n');
    fs::write(&identity_path, &identity_bytes).expect("write test identity map");

    let mapping_hex = sha256_hex(&identity_bytes);
    let manifest_path = snapshot.join("manifest.json");
    let mut manifest: CorpusManifest =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest bytes"))
            .expect("manifest");
    manifest.host_identity_mapping.bytes =
        u64::try_from(identity_bytes.len()).expect("mapping byte count");
    manifest
        .host_identity_mapping
        .sha256
        .clone_from(&mapping_hex);
    manifest.source_snapshot.mapping_contract_sha256 = format!("sha256:{mapping_hex}");
    let expected_mapping = manifest
        .expected_artifacts
        .iter_mut()
        .find(|entry| entry.relative_path == "expected/host-identity-map.json")
        .expect("host identity expected artifact");
    expected_mapping.bytes = u64::try_from(identity_bytes.len()).expect("mapping byte count");
    expected_mapping.sha256 = mapping_hex;
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("canonical manifest");
    manifest_bytes.push(b'\n');
    fs::write(manifest_path, manifest_bytes).expect("write test manifest");
}

fn assert_export_matches_expected_mapping(
    export: &PortableExportBundle,
    relative_expected_mapping: &str,
) {
    let expected: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture_root().join(relative_expected_mapping)).expect("expected mapping bytes"),
    )
    .expect("expected mapping JSON");
    let expected_record = expected
        .get("record")
        .expect("expected mapped record")
        .clone();
    let expected_id = expected_record
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("expected mapped ID");
    let actual = export
        .records
        .iter()
        .find(|record| record.id.as_str() == expected_id)
        .expect("exported mapped record");
    let mut actual = serde_json::to_value(actual).expect("portable mapped record JSON");
    let actual_object = actual.as_object_mut().expect("mapped record object");
    actual_object.remove("revision");
    actual_object.remove("etag");
    let mut expected_record = expected_record;
    let expected_object = expected_record
        .as_object_mut()
        .expect("expected mapped record object");
    assert_eq!(
        expected_object.get("summary"),
        Some(&serde_json::Value::Null)
    );
    expected_object.remove("summary");
    assert_eq!(
        actual, expected_record,
        "mapping {relative_expected_mapping}"
    );
}

#[test]
fn dry_run_fingerprints_all_cases_is_deterministic_and_zero_write() {
    let snapshot = copied_fixture();
    let source_before = tree_snapshot(snapshot.path());
    let (target, mut store) = initialize_store();
    let target_before = tree_snapshot(target.path());
    let (authority, context) = operator();

    let first = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("first deterministic dry run");
    let second = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("second deterministic dry run");
    assert_eq!(first.plan, second.plan);
    assert_eq!(first.report, second.report);
    assert_eq!(first.portable_plan, second.portable_plan);
    assert_eq!(
        first.plan_bytes().expect("plan bytes"),
        second.plan_bytes().expect("plan bytes")
    );
    assert_eq!(
        first.report_bytes().expect("report bytes"),
        second.report_bytes().expect("report bytes")
    );
    assert_eq!(first.report.cases.len(), 48);
    assert_eq!(
        first.report.source_counts,
        BambooSourceCounts {
            accepted: 1,
            transformed: 3,
            unresolved: 8,
            skipped: 30,
            quarantined: 6,
        }
    );
    assert_eq!(first.report.destination_counts.accepted, 4);
    assert_eq!(first.report.destination_counts.conflicting, 0);
    assert_eq!(first.plan.eligible_record_count, 4);
    assert_eq!(first.plan.mapped_record_count, 4);
    assert!(first.plan.destination_pristine);
    assert!(first.plan.eligible_mappings_complete);
    assert!(first.plan.committable);
    assert!(first.plan.source.native_watermark.is_none());
    assert_eq!(first.plan.source.projection_snapshot.store_revision.0, 1);
    assert_eq!(first.plan.source.projection_snapshot.audit_sequence.0, 0);

    let plan_text = String::from_utf8(first.plan_bytes().expect("plan bytes")).expect("UTF-8");
    let report_text =
        String::from_utf8(first.report_bytes().expect("report bytes")).expect("UTF-8");
    let debug = format!("{first:?}");
    for private in [
        PRIVATE_BODY,
        "memory/v1/scopes/global/topics",
        "sesfixture1",
        snapshot.path().to_string_lossy().as_ref(),
    ] {
        assert!(!plan_text.contains(private));
        assert!(!debug.contains(private));
    }
    assert!(report_text.contains("memory/v1/scopes/global/topics"));
    assert!(report_text.contains("sesfixture1"));

    let mut noncanonical = first.plan_bytes().expect("canonical reviewed plan");
    noncanonical.pop();
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &noncanonical,
            &IdempotencyKey::new("noncanonical-plan").expect("key"),
        )
        .expect_err("noncanonical plan is rejected"),
        BambooImportError::InvalidReviewedPlan
    );

    assert_eq!(tree_snapshot(snapshot.path()), source_before);
    assert_eq!(tree_snapshot(target.path()), target_before);
}

#[test]
fn ambient_bamboo_live_locator_is_ignored_in_isolated_subprocess() {
    const CHILD_MARKER: &str = "JIANDU_BAMBOO_IMPORT_LIVE_TRIPWIRE_CHILD";
    const SNAPSHOT_ENV: &str = "JIANDU_BAMBOO_IMPORT_EXPLICIT_SNAPSHOT";
    const TARGET_ENV: &str = "JIANDU_BAMBOO_IMPORT_TARGET";

    if std::env::var_os(CHILD_MARKER).is_some() {
        let snapshot = PathBuf::from(std::env::var_os(SNAPSHOT_ENV).expect("child snapshot"));
        let target = PathBuf::from(std::env::var_os(TARGET_ENV).expect("child target"));
        let store = CanonicalStore::initialize(&target, owner()).expect("child target store");
        let (authority, context) = operator();
        let dry_run = plan_bamboo_snapshot(&snapshot, &store, &authority, &context)
            .expect("explicit copied snapshot succeeds despite ambient tripwire");
        assert_eq!(dry_run.report.cases.len(), 48);
        return;
    }

    assert!(!include_str!("lib.rs").contains("std::env"));
    assert!(!include_str!("snapshot.rs").contains("std::env"));
    let snapshot = copied_fixture();
    let target = TempDir::new().expect("child target directory");
    let live_bamboo = TempDir::new().expect("live Bamboo tripwire");
    fs::write(live_bamboo.path().join("LOCK"), b"LIVE_LOCK_SENTINEL\n")
        .expect("live lock sentinel");
    fs::write(
        live_bamboo.path().join("cleanup.pending"),
        b"LIVE_CLEANUP_SENTINEL\n",
    )
    .expect("live cleanup sentinel");
    fs::create_dir(live_bamboo.path().join("memory")).expect("live memory sentinel directory");
    fs::write(
        live_bamboo.path().join("memory/LIVE_RECORD_SENTINEL"),
        b"LIVE_PRIVATE_BODY\n",
    )
    .expect("live record sentinel");
    let live_before = tree_snapshot(live_bamboo.path());

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("--exact")
        .arg("tests::ambient_bamboo_live_locator_is_ignored_in_isolated_subprocess")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .env(SNAPSHOT_ENV, snapshot.path())
        .env(TARGET_ENV, target.path())
        .env("BAMBOO_DATA_DIR", live_bamboo.path())
        .output()
        .expect("run isolated environment tripwire");
    assert!(
        output.status.success(),
        "tripwire child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(tree_snapshot(live_bamboo.path()), live_before);
}

#[test]
fn pristine_commit_imports_four_records_and_emits_deterministic_safe_evidence() {
    let snapshot = copied_fixture();
    let source_before = tree_snapshot(snapshot.path());
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let dry_run = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("committable plan");
    let reviewed = dry_run.plan_bytes().expect("reviewed plan bytes");

    let committed = commit_bamboo_snapshot(
        snapshot.path(),
        &mut store,
        &authority,
        &context,
        &reviewed,
        &IdempotencyKey::new("bamboo-import-success").expect("key"),
    )
    .expect("atomic Bamboo import");
    assert!(!committed.import.idempotent_replay);
    assert_eq!(committed.import.result.record_count, 4);
    assert_eq!(committed.import.result.tombstone_count, 0);
    assert_eq!(
        committed.import.result.base_snapshot,
        SnapshotWatermark {
            store_revision: StoreRevision(0),
            audit_sequence: AuditSequence(0),
        }
    );
    assert_eq!(committed.import.result.target_snapshot.store_revision.0, 1);
    assert_eq!(committed.import.result.target_snapshot.audit_sequence.0, 1);
    assert!(committed.validation.findings.is_empty());
    assert!(!committed.validation.truncated);
    assert_eq!(committed.export.records.len(), 4);
    assert!(committed.export.tombstones.is_empty());
    for expected in [
        "expected/global-feedback.json",
        "expected/global-user-transformed.json",
        "expected/project-legacy-transformed.json",
        "expected/project-current-transformed.json",
    ] {
        assert_export_matches_expected_mapping(&committed.export, expected);
    }

    let validation_authority = authority
        .authorize_store_validation(&context)
        .expect("validation authorization");
    let export_authority = authority
        .authorize_all_scope_export(&context)
        .expect("export authorization");
    assert_eq!(
        store
            .validate_all(&validation_authority)
            .expect("repeat validation"),
        committed.validation
    );
    assert_eq!(
        store.export_all(&export_authority).expect("repeat export"),
        committed.export
    );

    let user = store
        .get(&memory_id("mem_fixture_global_user"), &authority)
        .expect("imported preference")
        .result;
    assert_eq!(user.revision.get(), 1);
    assert!(user.etag.as_str().starts_with("sha256:"));
    assert_eq!(user.memory_type, MemoryType::Preference);
    assert_eq!(user.provenance.created_by, CreationActor::Import);
    assert_eq!(
        user.provenance.confidence,
        Some(Confidence::new(0.5).expect("medium confidence"))
    );
    assert!(user.relations.contains(&MemoryRelation {
        kind: RelationKind::Contradicts,
        target_memory_id: memory_id("mem_fixture_project_current"),
    }));
    let project = store
        .get(&memory_id("mem_fixture_project_current"), &authority)
        .expect("imported project memory")
        .result;
    assert_eq!(
        project.body,
        "The standalone memory service must expose structured records without depending on a host agent runtime."
    );
    assert!(
        project
            .tags
            .contains(&Tag::new("bamboo:freshness:high").expect("tag"))
    );
    assert!(
        project
            .tags
            .contains(&Tag::new("bamboo:granularity:quarter").expect("tag"))
    );

    let user_report = dry_run
        .report
        .cases
        .iter()
        .find(|case| case.case_id == "durable-global-user-transformed")
        .expect("global user report row");
    let actor = user_report
        .actor_evidence
        .as_ref()
        .expect("authorized raw actor evidence");
    assert_eq!(actor.created_by_kind, "session");
    assert_eq!(actor.created_by_id.as_deref(), Some("sesfixture1"));
    assert_eq!(actor.updated_by_kind, "memory_write");
    assert_eq!(actor.updated_by_actor.as_deref(), Some("fixture-agent"));
    assert_eq!(
        actor.sources,
        vec![BambooRawSourceEvidence {
            kind: "session".to_owned(),
            id: "sesfixture1".to_owned(),
            message_range: Vec::new(),
        }]
    );

    let evidence_bytes = committed
        .evidence
        .canonical_bytes()
        .expect("canonical cutover evidence");
    assert_eq!(
        BambooCutoverEvidence::decode_canonical(&evidence_bytes)
            .expect("strict cutover evidence round trip"),
        committed.evidence
    );
    let mut impossible_evidence = committed.evidence.clone();
    impossible_evidence.target_snapshot.audit_sequence = AuditSequence(0);
    impossible_evidence.digest = impossible_evidence
        .expected_digest()
        .expect("self-consistent impossible evidence digest");
    let impossible_bytes = canonical_json(&impossible_evidence, MAX_EVIDENCE_BYTES)
        .expect("canonical impossible evidence bytes");
    assert_eq!(
        BambooCutoverEvidence::decode_canonical(&impossible_bytes)
            .expect_err("semantic evidence invariants reject self-consistent tampering"),
        BambooImportError::InvalidReviewedPlan
    );
    let evidence_text = String::from_utf8(evidence_bytes).expect("UTF-8 evidence");
    let commit_debug = format!("{committed:?}");
    for private in [
        PRIVATE_BODY,
        "memory/v1/scopes/global/topics",
        "sesfixture1",
        snapshot.path().to_string_lossy().as_ref(),
        target.path().to_string_lossy().as_ref(),
    ] {
        assert!(!evidence_text.contains(private));
        assert!(!commit_debug.contains(private));
    }
    assert_eq!(tree_snapshot(snapshot.path()), source_before);
}

#[test]
fn source_drift_in_even_a_skipped_artifact_rejects_without_target_writes() {
    let snapshot = copied_fixture();
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let plan = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("initial plan")
        .plan_bytes()
        .expect("plan bytes");
    let skipped = snapshot
        .path()
        .join("source/ledger/v1/scopes/global/indexes/by_status.json");
    fs::write(&skipped, b"SOURCE_DRIFT_PRIVATE_SENTINEL\n").expect("inject source drift");
    let drifted_source = tree_snapshot(snapshot.path());
    let target_before = tree_snapshot(target.path());

    let error = commit_bamboo_snapshot(
        snapshot.path(),
        &mut store,
        &authority,
        &context,
        &plan,
        &IdempotencyKey::new("source-drift").expect("key"),
    )
    .expect_err("all source files are drift-bound");
    assert_eq!(error, BambooImportError::SourceDrift);
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("SOURCE_DRIFT_PRIVATE_SENTINEL"));
    assert!(!diagnostic.contains(snapshot.path().to_string_lossy().as_ref()));
    assert_eq!(tree_snapshot(snapshot.path()), drifted_source);
    assert_eq!(tree_snapshot(target.path()), target_before);
    assert_eq!(store.watermark().expect("watermark").0, 0);

    let reclassified = copied_fixture();
    let manifest_path = reclassified.path().join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("classification manifest bytes"))
            .expect("classification manifest");
    manifest["cases"][0]["outcome"] = serde_json::Value::String("unresolved".to_owned());
    let mut manifest_bytes =
        serde_json::to_vec_pretty(&manifest).expect("reclassified canonical manifest");
    manifest_bytes.push(b'\n');
    fs::write(manifest_path, manifest_bytes).expect("rewrite frozen classification");
    let target_before = tree_snapshot(target.path());
    assert_eq!(
        plan_bamboo_snapshot(reclassified.path(), &store, &authority, &context)
            .expect_err("frozen #36 classification cannot be relabelled"),
        BambooImportError::InvalidSnapshot
    );
    assert_eq!(tree_snapshot(target.path()), target_before);
}

#[test]
fn source_inventory_binds_directories_and_bounds_total_entries_and_depth_without_writes() {
    let (target, store) = initialize_store();
    let (authority, context) = operator();
    let target_before = tree_snapshot(target.path());

    let extra_directory_snapshot = copied_fixture();
    fs::create_dir(
        extra_directory_snapshot
            .path()
            .join("source/UNREVIEWED_EMPTY_PRIVATE_DIRECTORY"),
    )
    .expect("inject unreviewed empty directory");
    let extra_directory_before = tree_snapshot(extra_directory_snapshot.path());
    let error = plan_bamboo_snapshot(
        extra_directory_snapshot.path(),
        &store,
        &authority,
        &context,
    )
    .expect_err("an empty directory is still source-inventory drift");
    assert_eq!(error, BambooImportError::SourceDrift);
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("UNREVIEWED_EMPTY_PRIVATE_DIRECTORY"));
    assert!(!diagnostic.contains(extra_directory_snapshot.path().to_string_lossy().as_ref()));
    assert_eq!(
        tree_snapshot(extra_directory_snapshot.path()),
        extra_directory_before
    );
    assert_eq!(tree_snapshot(target.path()), target_before);

    let wide_snapshot = copied_fixture();
    for entry in 0..=MAX_SOURCE_ENTRIES {
        fs::create_dir(
            wide_snapshot
                .path()
                .join("source")
                .join(format!("private-entry-{entry:04}")),
        )
        .expect("inject total-entry-budget witness");
    }
    let wide_before = tree_snapshot(wide_snapshot.path());
    let error = plan_bamboo_snapshot(wide_snapshot.path(), &store, &authority, &context)
        .expect_err("total files plus directories are globally bounded");
    assert_eq!(error, BambooImportError::InvalidSnapshot);
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("private-entry"));
    assert!(!diagnostic.contains(wide_snapshot.path().to_string_lossy().as_ref()));
    assert_eq!(tree_snapshot(wide_snapshot.path()), wide_before);
    assert_eq!(tree_snapshot(target.path()), target_before);

    let deep_snapshot = copied_fixture();
    let mut deep = deep_snapshot.path().join("source");
    for depth in 0..=MAX_SOURCE_DEPTH {
        deep.push(format!("private-depth-{depth:02}"));
        fs::create_dir(&deep).expect("inject bounded-depth witness");
    }
    let deep_before = tree_snapshot(deep_snapshot.path());
    let error = plan_bamboo_snapshot(deep_snapshot.path(), &store, &authority, &context)
        .expect_err("over-depth source traversal fails before unbounded recursion");
    assert_eq!(error, BambooImportError::InvalidSnapshot);
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains("private-depth"));
    assert!(!diagnostic.contains(deep_snapshot.path().to_string_lossy().as_ref()));
    assert_eq!(tree_snapshot(deep_snapshot.path()), deep_before);
    assert_eq!(tree_snapshot(target.path()), target_before);
    assert_eq!(store.watermark().expect("pristine watermark").0, 0);
}

#[test]
fn mapping_bytes_are_bound_and_only_unresolved_eligible_records_block_commit() {
    let snapshot = copied_fixture();
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let complete = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("complete baseline plan");
    rewrite_identity_without_projects(snapshot.path());
    let target_before_drift = tree_snapshot(target.path());
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &complete.plan_bytes().expect("original reviewed plan"),
            &IdempotencyKey::new("identity-map-drift").expect("key"),
        )
        .expect_err("identity-map drift invalidates an earlier reviewed plan"),
        BambooImportError::SourceDrift
    );
    assert_eq!(tree_snapshot(target.path()), target_before_drift);
    let incomplete = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("full report remains available with unresolved eligible mappings");

    assert_eq!(incomplete.report.cases.len(), 48);
    assert_eq!(
        incomplete.report.source_counts,
        complete.report.source_counts
    );
    assert_ne!(
        incomplete.plan.source.mapping_contract_sha256,
        complete.plan.source.mapping_contract_sha256
    );
    assert_ne!(incomplete.plan.digest, complete.plan.digest);
    assert_eq!(incomplete.plan.eligible_record_count, 4);
    assert_eq!(incomplete.plan.mapped_record_count, 2);
    assert!(!incomplete.plan.eligible_mappings_complete);
    assert!(!incomplete.plan.committable);
    assert_eq!(
        incomplete
            .report
            .cases
            .iter()
            .filter(|case| {
                case.mapping_disposition == BambooMappingDisposition::UnresolvedIdentity
            })
            .count(),
        2
    );
    assert_eq!(incomplete.report.source_counts.unresolved, 8);
    let target_before = tree_snapshot(target.path());
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &incomplete.plan_bytes().expect("incomplete plan bytes"),
            &IdempotencyKey::new("unresolved-mapping").expect("key"),
        )
        .expect_err("unresolved eligible identity is noncommittable"),
        BambooImportError::UnresolvedEligibleIdentity
    );
    assert_eq!(tree_snapshot(target.path()), target_before);
    assert_eq!(store.watermark().expect("watermark").0, 0);
}

#[test]
fn unrelated_nonempty_destination_is_rejected_before_wal_even_without_id_conflicts() {
    let snapshot = copied_fixture();
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let dry_run =
        plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context).expect("pristine plan");
    let plan = dry_run.plan_bytes().expect("reviewed plan");
    create_principal_record(&mut store, &authority, &context, "mem_unrelated_target");
    let before_attempt = tree_snapshot(target.path());
    let watermark = store.watermark().expect("nonempty watermark");

    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &plan,
            &IdempotencyKey::new("nonempty-target").expect("key"),
        )
        .expect_err("nonempty target is rejected"),
        BambooImportError::DestinationNotPristine
    );
    assert_eq!(tree_snapshot(target.path()), before_attempt);
    assert_eq!(store.watermark().expect("unchanged watermark"), watermark);

    // Even a self-consistent outer plan that substitutes the real rev-1
    // portable-plan digest cannot turn the changed-watermark recovery branch
    // into a fresh import. A new key has no receipt, so the adapter performs
    // read-only diagnosis and rejects the non-pristine destination.
    let current_portable_plan = store
        .plan_import(&authority, &context, &dry_run.bundle_bytes)
        .expect("read-only current-target portable plan");
    let mut forged = dry_run.plan.clone();
    forged.portable_plan_digest = current_portable_plan.digest;
    forged.digest = forged
        .expected_digest()
        .expect("self-consistent outer digest");
    let forged_bytes = forged.canonical_bytes().expect("canonical forged plan");
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &forged_bytes,
            &IdempotencyKey::new("forged-current-plan-new-key").expect("key"),
        )
        .expect_err("changed watermark never starts a fresh import"),
        BambooImportError::DestinationNotPristine
    );
    assert_eq!(tree_snapshot(target.path()), before_attempt);
    assert_eq!(store.watermark().expect("unchanged watermark"), watermark);
}

#[test]
fn incompatible_destination_metadata_fails_closed_without_adapter_writes() {
    let snapshot = copied_fixture();
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let plan = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("compatible pristine target")
        .plan_bytes()
        .expect("reviewed plan");
    let metadata_path = target.path().join("store.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).expect("target metadata bytes"))
            .expect("target metadata");
    metadata["formatVersion"] = serde_json::Value::String("jiandu.store/v9".to_owned());
    let mut incompatible_bytes =
        serde_json::to_vec_pretty(&metadata).expect("incompatible metadata bytes");
    incompatible_bytes.push(b'\n');
    fs::write(&metadata_path, incompatible_bytes).expect("inject incompatible target metadata");
    let before_attempt = tree_snapshot(target.path());

    let error = commit_bamboo_snapshot(
        snapshot.path(),
        &mut store,
        &authority,
        &context,
        &plan,
        &IdempotencyKey::new("incompatible-target").expect("key"),
    )
    .expect_err("incompatible target must fail closed");
    assert!(matches!(
        error,
        BambooImportError::Store(
            StoreErrorCode::ValidationFailed
                | StoreErrorCode::InvalidStoreMetadata
                | StoreErrorCode::RecoveryRequired
        )
    ));
    assert_eq!(tree_snapshot(target.path()), before_attempt);
}

#[test]
fn protected_target_tombstone_rejects_resurrection_without_partial_apply() {
    let snapshot = copied_fixture();
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let pristine =
        plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context).expect("pristine plan");
    let plan = pristine.plan_bytes().expect("reviewed plan");
    let protected_id = "mem_fixture_global_feedback";
    create_principal_record(&mut store, &authority, &context, protected_id);
    let conflicting_before = tree_snapshot(target.path());
    let conflicting = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("conflicting destination overlay");
    assert_eq!(tree_snapshot(target.path()), conflicting_before);
    assert_eq!(
        conflicting.report.source_counts,
        pristine.report.source_counts
    );
    assert_eq!(conflicting.report.destination_counts.accepted, 3);
    assert_eq!(conflicting.report.destination_counts.conflicting, 1);
    assert!(!conflicting.plan.destination_pristine);
    assert!(!conflicting.plan.committable);
    assert_eq!(
        conflicting
            .report
            .cases
            .iter()
            .find(|case| case
                .target_memory_id
                .as_ref()
                .is_some_and(|id| id.as_str() == protected_id))
            .and_then(|case| case.destination_classification),
        Some(ImportClassification::Conflicting)
    );
    forget_principal_record(&mut store, &authority, &context, protected_id);
    let before_attempt = tree_snapshot(target.path());
    let watermark = store.watermark().expect("tombstone watermark");
    let tombstoned = plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context)
        .expect("tombstone-protected destination overlay");
    assert_eq!(tree_snapshot(target.path()), before_attempt);
    assert_eq!(
        tombstoned.report.source_counts,
        pristine.report.source_counts
    );
    assert_eq!(tombstoned.report.destination_counts.accepted, 3);
    assert_eq!(tombstoned.report.destination_counts.tombstone_protected, 1);
    assert!(!tombstoned.plan.destination_pristine);
    assert!(!tombstoned.plan.committable);
    assert_eq!(
        tombstoned
            .report
            .cases
            .iter()
            .find(|case| case
                .target_memory_id
                .as_ref()
                .is_some_and(|id| id.as_str() == protected_id))
            .and_then(|case| case.destination_classification),
        Some(ImportClassification::TombstoneProtected)
    );

    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &plan,
            &IdempotencyKey::new("protected-tombstone").expect("key"),
        )
        .expect_err("protected tombstone rejects resurrection"),
        BambooImportError::ProtectedTombstoneResurrection
    );
    assert_eq!(tree_snapshot(target.path()), before_attempt);
    assert_eq!(store.watermark().expect("unchanged watermark"), watermark);
}

#[test]
fn post_commit_evidence_failure_is_explicit_and_exact_key_retry_recovers_receipt() {
    let snapshot = copied_fixture();
    let source_before = tree_snapshot(snapshot.path());
    let (target, mut store) = initialize_store();
    let (authority, context) = operator();
    let dry_run =
        plan_bamboo_snapshot(snapshot.path(), &store, &authority, &context).expect("pristine plan");
    let reviewed_bytes = dry_run.plan_bytes().expect("reviewed plan bytes");
    let key = IdempotencyKey::new("lost-post-commit-evidence").expect("key");

    let error = commit_bamboo_snapshot_with_hook(
        snapshot.path(),
        &mut store,
        &authority,
        &context,
        &reviewed_bytes,
        &key,
        |_| Err(StoreErrorCode::Io),
    )
    .expect_err("simulate validation/export acknowledgement loss");
    let (transaction_id, result_digest, backup_digest) = match error {
        BambooImportError::CommittedEvidenceUnavailable {
            transaction_id,
            import_result_digest,
            backup_metadata_digest,
            failure: StoreErrorCode::Io,
        } => (transaction_id, import_result_digest, backup_metadata_digest),
        other => panic!("unexpected committed outcome: {other:?}"),
    };
    assert!(valid_content_digest(&result_digest));
    assert!(valid_content_digest(&backup_digest));
    assert_eq!(store.watermark().expect("committed watermark").0, 1);
    let committed_tree = tree_snapshot(target.path());

    let mut tampered = dry_run.plan.clone();
    tampered.target_validation_digest = serde_json::from_value(serde_json::Value::String(format!(
        "sha256:{}",
        "0".repeat(64)
    )))
    .expect("syntactically valid alternate digest");
    tampered.digest = tampered.expected_digest().expect("tampered plan digest");
    let tampered_bytes = tampered.canonical_bytes().expect("canonical tampered plan");
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &tampered_bytes,
            &key,
        )
        .expect_err("tampered target digest is stale"),
        BambooImportError::StaleReviewedPlan
    );
    assert_eq!(tree_snapshot(target.path()), committed_tree);

    let mut portable_tampered = dry_run.plan.clone();
    portable_tampered.portable_plan_digest = serde_json::from_value(serde_json::Value::String(
        format!("sha256:{}", "1".repeat(64)),
    ))
    .expect("syntactically valid alternate portable-plan digest");
    portable_tampered.digest = portable_tampered
        .expected_digest()
        .expect("tampered outer plan digest");
    let portable_tampered_bytes = portable_tampered
        .canonical_bytes()
        .expect("canonical portable-plan tamper");
    assert_eq!(
        commit_bamboo_snapshot(
            snapshot.path(),
            &mut store,
            &authority,
            &context,
            &portable_tampered_bytes,
            &key,
        )
        .expect_err("changed portable-plan digest conflicts with exact receipt"),
        BambooImportError::StaleReviewedPlan
    );
    assert_eq!(tree_snapshot(target.path()), committed_tree);

    let recovered = commit_bamboo_snapshot(
        snapshot.path(),
        &mut store,
        &authority,
        &context,
        &reviewed_bytes,
        &key,
    )
    .expect("exact receipt replay recovers final evidence");
    assert!(recovered.import.idempotent_replay);
    assert_eq!(recovered.import.result.transaction_id, transaction_id);
    assert_eq!(recovered.import.result.digest.as_str(), result_digest);
    assert_eq!(
        recovered.import.backup_metadata.digest.as_str(),
        backup_digest
    );
    assert_eq!(recovered.evidence.plan_digest, dry_run.plan.digest);
    assert_eq!(store.watermark().expect("single commit watermark").0, 1);
    assert_eq!(tree_snapshot(target.path()), committed_tree);
    assert_eq!(tree_snapshot(snapshot.path()), source_before);
}

#[cfg(unix)]
#[test]
fn symlink_and_hardlink_source_entries_fail_closed_with_safe_diagnostics() {
    use std::os::unix::fs::symlink;

    let (target, store) = initialize_store();
    let (authority, context) = operator();
    let target_before = tree_snapshot(target.path());

    let symlink_snapshot = copied_fixture();
    let external = TempDir::new().expect("external source witness");
    let external_file = external.path().join("PRIVATE_EXTERNAL_SOURCE");
    fs::write(&external_file, b"PRIVATE_EXTERNAL_BODY\n").expect("external body");
    let source_file = symlink_snapshot
        .path()
        .join("source/ledger/v1/scopes/global/indexes/by_status.json");
    fs::remove_file(&source_file).expect("replace copied fixture file");
    symlink(&external_file, &source_file).expect("inject source symlink");
    let symlink_error = plan_bamboo_snapshot(symlink_snapshot.path(), &store, &authority, &context)
        .expect_err("symlink input must fail closed");
    assert_eq!(symlink_error, BambooImportError::UnsafeSnapshot);

    let hardlink_snapshot = copied_fixture();
    let hardlink_source = hardlink_snapshot
        .path()
        .join("source/ledger/v1/scopes/global/indexes/by_status.json");
    let hardlink_witness = external.path().join("hardlink-witness");
    fs::hard_link(&hardlink_source, &hardlink_witness).expect("inject source hardlink");
    let hardlink_error =
        plan_bamboo_snapshot(hardlink_snapshot.path(), &store, &authority, &context)
            .expect_err("hardlink input must fail closed");
    assert_eq!(hardlink_error, BambooImportError::UnsafeSnapshot);

    for diagnostic in [
        format!("{symlink_error:?} {symlink_error}"),
        format!("{hardlink_error:?} {hardlink_error}"),
    ] {
        assert!(!diagnostic.contains("PRIVATE_EXTERNAL_BODY"));
        assert!(!diagnostic.contains(external.path().to_string_lossy().as_ref()));
        assert!(!diagnostic.contains(symlink_snapshot.path().to_string_lossy().as_ref()));
        assert!(!diagnostic.contains(hardlink_snapshot.path().to_string_lossy().as_ref()));
    }
    assert_eq!(
        fs::read(&external_file).expect("external witness"),
        b"PRIVATE_EXTERNAL_BODY\n"
    );
    assert_eq!(tree_snapshot(target.path()), target_before);
}
