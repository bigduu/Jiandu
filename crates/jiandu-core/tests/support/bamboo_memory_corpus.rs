#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use jiandu_core::{
    Etag, MemoryId, MemoryRecord, MemoryRelation, MemorySchema, MemoryScope, MemoryStatus,
    MemoryType, Provenance, RelationKind, Revision, Tag, Timestamp, Validate,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CORPUS_SCHEMA: &str = "jiandu.dev/compatibility/bamboo-memory-fixture-corpus/v1alpha1";
pub const MAPPING_SCHEMA: &str = "jiandu.dev/compatibility/bamboo-memory-mapping/v1alpha1";
pub const IDENTITY_MAP_SCHEMA: &str = "jiandu.dev/compatibility/bamboo-host-identity-map/v1alpha1";
pub const BAMBOO_COMMIT: &str = "f362cc151a2ac097db01178449b9a733929a440f";
pub const BAMBOO_TREE: &str = "a824dfe7e7263c69304ff26ef5826b7abcb0085f";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CorpusManifest {
    pub schema: String,
    pub corpus_version: u32,
    pub source_snapshot: SourceSnapshot,
    pub format_evidence: FormatEvidence,
    pub host_identity_mapping: SnapshotEntry,
    pub expected_artifacts: Vec<SnapshotEntry>,
    pub cases: Vec<FixtureCase>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSnapshot {
    pub repository: String,
    pub commit: String,
    pub tree: String,
    pub watermark_kind: String,
    pub native_watermark: Option<String>,
    pub snapshot_evidence_kind: String,
    pub mapping_contract_sha256: String,
    pub aggregate_sha256: String,
    pub entries: Vec<SnapshotEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotEntry {
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormatEvidence {
    pub durable_path_version: String,
    pub durable_scope_state_version: u32,
    pub durable_topic_frontmatter_version_field: String,
    pub pre_granularity_frontmatter_supported: bool,
    pub project_manifest_authority: String,
    pub project_index_authority: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixtureCase {
    pub id: String,
    pub source_relative_path: String,
    pub artifact_kind: ArtifactKind,
    pub format_variant: String,
    pub encoding: FixtureEncoding,
    pub outcome: MappingOutcome,
    pub reason_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_mapping: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    DurableTopic,
    DurableIndex,
    DurableState,
    DurableView,
    DurableAuditLog,
    ProjectManifest,
    ProjectIndex,
    LegacyMigrationJournal,
    SessionTopic,
    SessionState,
    LegacyDream,
    LedgerRecord,
    LedgerIndex,
    LedgerView,
    LedgerAuditLog,
    PlanMarkdown,
    PlanState,
    PlanCursor,
    PlanSections,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureEncoding {
    Json,
    JsonLines,
    TolerantJsonLines,
    Markdown,
    MarkdownFrontmatter,
    InvalidMarkdownFrontmatter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingOutcome {
    Accepted,
    Transformed,
    Unresolved,
    Skipped,
    Quarantined,
}

#[derive(Clone, Copy)]
struct CaseSpec {
    id: &'static str,
    path: &'static str,
    kind: ArtifactKind,
    variant: &'static str,
    encoding: FixtureEncoding,
    outcome: MappingOutcome,
    reason: &'static str,
    expected: Option<&'static str>,
}

macro_rules! case {
    ($id:literal, $path:literal, $kind:ident, $variant:literal, $encoding:ident, $outcome:ident, $reason:literal) => {
        CaseSpec {
            id: $id,
            path: $path,
            kind: ArtifactKind::$kind,
            variant: $variant,
            encoding: FixtureEncoding::$encoding,
            outcome: MappingOutcome::$outcome,
            reason: $reason,
            expected: None,
        }
    };
    ($id:literal, $path:literal, $kind:ident, $variant:literal, $encoding:ident, $outcome:ident, $reason:literal, $expected:literal) => {
        CaseSpec {
            id: $id,
            path: $path,
            kind: ArtifactKind::$kind,
            variant: $variant,
            encoding: FixtureEncoding::$encoding,
            outcome: MappingOutcome::$outcome,
            reason: $reason,
            expected: Some($expected),
        }
    };
}

const CASES: &[CaseSpec] = &[
    case!(
        "ledger-index-status",
        "ledger/v1/scopes/global/indexes/by_status.json",
        LedgerIndex,
        "by_status_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "ledger-index-time",
        "ledger/v1/scopes/global/indexes/by_time.json",
        LedgerIndex,
        "by_time_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "ledger-audit",
        "ledger/v1/scopes/global/logs/audit.jsonl",
        LedgerAuditLog,
        "mutation_audit_jsonl_v1",
        JsonLines,
        Skipped,
        "audit_is_evidence_not_memory"
    ),
    case!(
        "ledger-record-current",
        "ledger/v1/scopes/global/records/rec_fixture_full.md",
        LedgerRecord,
        "prospective_record_full_v1",
        MarkdownFrontmatter,
        Unresolved,
        "jiandu_prospective_contract_absent"
    ),
    case!(
        "ledger-view-agenda",
        "ledger/v1/scopes/global/views/AGENDA.md",
        LedgerView,
        "agenda_markdown_v1",
        Markdown,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "ledger-view-todo",
        "ledger/v1/scopes/global/views/TODO.md",
        LedgerView,
        "todo_markdown_v1",
        Markdown,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "ledger-record-minimal",
        "ledger/v1/scopes/projects/fixture-workspace-deadbeef/records/rec_fixture_minimal.md",
        LedgerRecord,
        "prospective_record_minimal_legacy_v1",
        MarkdownFrontmatter,
        Unresolved,
        "jiandu_prospective_contract_absent"
    ),
    case!(
        "memory-index-graph",
        "memory/v1/scopes/global/indexes/graph.json",
        DurableIndex,
        "graph_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-index-lexical",
        "memory/v1/scopes/global/indexes/lexical.json",
        DurableIndex,
        "lexical_current_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-index-recent",
        "memory/v1/scopes/global/indexes/recent.json",
        DurableIndex,
        "recent_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-index-stale",
        "memory/v1/scopes/global/indexes/stale_candidates.json",
        DurableIndex,
        "stale_candidates_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-index-taxonomy",
        "memory/v1/scopes/global/indexes/taxonomy.json",
        DurableIndex,
        "taxonomy_v1",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-log-access",
        "memory/v1/scopes/global/logs/access_log.jsonl",
        DurableAuditLog,
        "best_effort_access_jsonl_v1",
        TolerantJsonLines,
        Skipped,
        "derived_best_effort_signal"
    ),
    case!(
        "memory-log-contradiction",
        "memory/v1/scopes/global/logs/contradiction_audit.jsonl",
        DurableAuditLog,
        "contradiction_audit_jsonl_v1",
        JsonLines,
        Skipped,
        "audit_is_evidence_not_memory"
    ),
    case!(
        "memory-log-merge",
        "memory/v1/scopes/global/logs/merge_audit.jsonl",
        DurableAuditLog,
        "merge_audit_jsonl_v1",
        JsonLines,
        Skipped,
        "audit_is_evidence_not_memory"
    ),
    case!(
        "memory-log-purge",
        "memory/v1/scopes/global/logs/purge_audit.jsonl",
        DurableAuditLog,
        "purge_audit_jsonl_v1",
        JsonLines,
        Skipped,
        "audit_is_evidence_not_memory"
    ),
    case!(
        "memory-log-write",
        "memory/v1/scopes/global/logs/write_audit.jsonl",
        DurableAuditLog,
        "write_audit_jsonl_v1",
        JsonLines,
        Skipped,
        "audit_is_evidence_not_memory"
    ),
    case!(
        "memory-state-last-dream",
        "memory/v1/scopes/global/state/last_dream.json",
        DurableState,
        "last_dream_marker_v1",
        Json,
        Skipped,
        "derived_state_marker"
    ),
    case!(
        "memory-state-last-reindex",
        "memory/v1/scopes/global/state/last_reindex.json",
        DurableState,
        "last_reindex_marker_v1",
        Json,
        Skipped,
        "derived_state_marker"
    ),
    case!(
        "memory-state-schema",
        "memory/v1/scopes/global/state/schema_version.json",
        DurableState,
        "scope_schema_version_v1",
        Json,
        Skipped,
        "format_evidence_only"
    ),
    case!(
        "durable-corrupt",
        "memory/v1/scopes/global/topics/mem_fixture_corrupt.md",
        DurableTopic,
        "invalid_frontmatter",
        InvalidMarkdownFrontmatter,
        Quarantined,
        "frontmatter_decode_failed"
    ),
    case!(
        "durable-id-mismatch",
        "memory/v1/scopes/global/topics/mem_fixture_filename.md",
        DurableTopic,
        "filename_frontmatter_mismatch",
        MarkdownFrontmatter,
        Quarantined,
        "path_frontmatter_id_mismatch"
    ),
    case!(
        "durable-global-feedback",
        "memory/v1/scopes/global/topics/mem_fixture_global_feedback.md",
        DurableTopic,
        "canonical_current_v1",
        MarkdownFrontmatter,
        Accepted,
        "explicit_principal_mapping",
        "expected/global-feedback.json"
    ),
    case!(
        "durable-global-user-transformed",
        "memory/v1/scopes/global/topics/mem_fixture_global_user.md",
        DurableTopic,
        "canonical_current_v1",
        MarkdownFrontmatter,
        Transformed,
        "type_tag_timestamp_transform",
        "expected/global-user-transformed.json"
    ),
    case!(
        "durable-same-root-a",
        "memory/v1/scopes/global/topics/mem_fixture_same_root.md",
        DurableTopic,
        "same_root_duplicate",
        MarkdownFrontmatter,
        Quarantined,
        "same_root_duplicate_nondeterministic"
    ),
    case!(
        "durable-same-root-b",
        "memory/v1/scopes/global/topics/mem_fixture_same_root_shadow.md",
        DurableTopic,
        "same_root_duplicate",
        MarkdownFrontmatter,
        Quarantined,
        "same_root_duplicate_nondeterministic"
    ),
    case!(
        "memory-view-dream",
        "memory/v1/scopes/global/views/DREAM_NOTEBOOK.md",
        DurableView,
        "dream_markdown_v1",
        Markdown,
        Unresolved,
        "possibly_user_authored_view_unsupported"
    ),
    case!(
        "memory-view-index",
        "memory/v1/scopes/global/views/MEMORY.md",
        DurableView,
        "memory_markdown_v1",
        Markdown,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-view-recent",
        "memory/v1/scopes/global/views/RECENT.md",
        DurableView,
        "recent_markdown_v1",
        Markdown,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "memory-view-stale",
        "memory/v1/scopes/global/views/STALE.md",
        DurableView,
        "stale_markdown_v1",
        Markdown,
        Skipped,
        "derived_rebuildable"
    ),
    case!(
        "durable-project-legacy-duplicate",
        "memory/v1/scopes/projects/fixture-workspace-deadbeef/topics/mem_fixture_duplicate.md",
        DurableTopic,
        "legacy_alias_duplicate",
        MarkdownFrontmatter,
        Quarantined,
        "divergent_primary_legacy_conflict_without_authority_evidence"
    ),
    case!(
        "durable-project-legacy-pre-granularity",
        "memory/v1/scopes/projects/fixture-workspace-deadbeef/topics/mem_fixture_legacy_reference.md",
        DurableTopic,
        "legacy_pre_granularity_v1",
        MarkdownFrontmatter,
        Transformed,
        "declared_legacy_project_mapping",
        "expected/project-legacy-transformed.json"
    ),
    case!(
        "durable-project-path-only",
        "memory/v1/scopes/projects/orphan-workspace-cafebabe/topics/mem_fixture_orphan.md",
        DurableTopic,
        "legacy_path_hash_orphan",
        MarkdownFrontmatter,
        Unresolved,
        "path_only_project_identity"
    ),
    case!(
        "session-topic-current",
        "memory/v1/sessions/sesfixture1/note/default.md",
        SessionTopic,
        "multi_topic_note_v1",
        Markdown,
        Unresolved,
        "session_note_split_policy_required"
    ),
    case!(
        "session-state-current",
        "memory/v1/sessions/sesfixture1/state.json",
        SessionState,
        "session_state_v1",
        Json,
        Skipped,
        "session_supporting_state_not_memory"
    ),
    case!(
        "legacy-dream",
        "notes/__dream__/global.md",
        LegacyDream,
        "legacy_global_dream",
        Markdown,
        Unresolved,
        "possibly_user_authored_legacy_prose_unsupported"
    ),
    case!(
        "session-topic-legacy-single",
        "notes/session-fixture-legacy.md",
        SessionTopic,
        "legacy_single_note",
        Markdown,
        Unresolved,
        "session_note_split_policy_required"
    ),
    case!(
        "session-topic-legacy-directory",
        "notes/session-fixture-topics/context.md",
        SessionTopic,
        "legacy_topic_directory",
        Markdown,
        Unresolved,
        "session_note_split_policy_required"
    ),
    case!(
        "plan-cursor",
        "plan/sesfixture1/cursor.json",
        PlanCursor,
        "cursor_v1",
        Json,
        Skipped,
        "host_execution_state_out_of_scope"
    ),
    case!(
        "plan-current",
        "plan/sesfixture1/plan.md",
        PlanMarkdown,
        "directory_plan_v1",
        Markdown,
        Skipped,
        "host_execution_state_out_of_scope"
    ),
    case!(
        "plan-sections",
        "plan/sesfixture1/sections.json",
        PlanSections,
        "sections_v1",
        Json,
        Skipped,
        "host_execution_state_out_of_scope"
    ),
    case!(
        "plan-state",
        "plan/sesfixture1/state.json",
        PlanState,
        "state_v1",
        Json,
        Skipped,
        "host_execution_state_out_of_scope"
    ),
    case!(
        "plan-legacy",
        "plan/seslegacy1.md",
        PlanMarkdown,
        "legacy_flat_plan",
        Markdown,
        Skipped,
        "host_execution_state_out_of_scope"
    ),
    case!(
        "durable-project-primary-duplicate",
        "projects/01JFIXTUREPROJECT0000000001/memory/v1/topics/mem_fixture_duplicate.md",
        DurableTopic,
        "canonical_current_v1",
        MarkdownFrontmatter,
        Quarantined,
        "divergent_primary_legacy_conflict_without_authority_evidence"
    ),
    case!(
        "durable-project-current",
        "projects/01JFIXTUREPROJECT0000000001/memory/v1/topics/mem_fixture_project_current.md",
        DurableTopic,
        "canonical_current_v1",
        MarkdownFrontmatter,
        Transformed,
        "metadata_and_provenance_transform",
        "expected/project-current-transformed.json"
    ),
    case!(
        "project-manifest",
        "projects/01JFIXTUREPROJECT0000000001/project.json",
        ProjectManifest,
        "project_manifest_v2",
        Json,
        Skipped,
        "authoritative_identity_evidence_not_memory"
    ),
    case!(
        "legacy-migration-journal",
        "projects/01JFIXTUREPROJECT0000000001/state/legacy-memory-migration/journals/fixture.json",
        LegacyMigrationJournal,
        "legacy_memory_journal_v1",
        Json,
        Skipped,
        "migration_evidence_not_memory"
    ),
    case!(
        "project-index",
        "projects/index.json",
        ProjectIndex,
        "project_index_v2",
        Json,
        Skipped,
        "derived_rebuildable"
    ),
];

pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/migration/bamboo-memory/v1alpha1")
}

pub fn manifest_path() -> PathBuf {
    corpus_root().join("manifest.json")
}

pub fn generated_manifest() -> Result<CorpusManifest, String> {
    let root = corpus_root();
    let source_entries = snapshot_entries(&root.join("source"), "source")?;
    let expected_artifacts = snapshot_entries(&root.join("expected"), "expected")?;
    let aggregate_sha256 = aggregate_digest(&source_entries);
    let host_identity_mapping = expected_artifacts
        .iter()
        .find(|entry| entry.relative_path == "expected/host-identity-map.json")
        .cloned()
        .ok_or_else(|| "host identity mapping fixture is missing".to_string())?;
    let cases = CASES
        .iter()
        .map(|case| FixtureCase {
            id: case.id.to_string(),
            source_relative_path: case.path.to_string(),
            artifact_kind: case.kind,
            format_variant: case.variant.to_string(),
            encoding: case.encoding,
            outcome: case.outcome,
            reason_code: case.reason.to_string(),
            expected_mapping: case.expected.map(str::to_string),
        })
        .collect();

    Ok(CorpusManifest {
        schema: CORPUS_SCHEMA.to_string(),
        corpus_version: 1,
        source_snapshot: SourceSnapshot {
            repository: "bigduu/Bamboo-agent".to_string(),
            commit: BAMBOO_COMMIT.to_string(),
            tree: BAMBOO_TREE.to_string(),
            watermark_kind: "sorted_relative_path_size_sha256".to_string(),
            native_watermark: None,
            snapshot_evidence_kind: "immutable_file_manifest".to_string(),
            mapping_contract_sha256: format!("sha256:{}", host_identity_mapping.sha256),
            aggregate_sha256,
            entries: source_entries,
        },
        format_evidence: FormatEvidence {
            durable_path_version: "memory/v1".to_string(),
            durable_scope_state_version: 1,
            durable_topic_frontmatter_version_field: "absent".to_string(),
            pre_granularity_frontmatter_supported: true,
            project_manifest_authority: "projects/<bamboo-project-id>/project.json".to_string(),
            project_index_authority: "derived_rebuildable".to_string(),
        },
        host_identity_mapping,
        expected_artifacts,
        cases,
    })
}

pub fn render_generated_manifest() -> Result<String, String> {
    let mut bytes = serde_json::to_string_pretty(&generated_manifest()?)
        .map_err(|error| format!("serialize generated manifest: {error}"))?;
    bytes.push('\n');
    Ok(bytes)
}

pub fn validate_corpus() -> Result<(), String> {
    let root = corpus_root();
    let generated = generated_manifest()?;
    validate_relative_entries(&generated.source_snapshot.entries)?;
    validate_relative_entries(&generated.expected_artifacts)?;

    if generated.source_snapshot.commit != BAMBOO_COMMIT
        || generated.source_snapshot.tree != BAMBOO_TREE
    {
        return Err("Bamboo source commit/tree drifted".to_string());
    }
    if generated.source_snapshot.entries.len() != CASES.len() {
        return Err(format!(
            "every source artifact needs one case: {} entries, {} cases",
            generated.source_snapshot.entries.len(),
            CASES.len()
        ));
    }

    let source_paths = generated
        .source_snapshot
        .entries
        .iter()
        .map(|entry| entry.relative_path.strip_prefix("source/").unwrap_or(""))
        .collect::<BTreeSet<_>>();
    let case_paths = CASES.iter().map(|case| case.path).collect::<BTreeSet<_>>();
    if source_paths != case_paths {
        return Err("source artifacts and classified cases differ".to_string());
    }

    let ids = CASES.iter().map(|case| case.id).collect::<BTreeSet<_>>();
    if ids.len() != CASES.len() {
        return Err("fixture case IDs must be unique".to_string());
    }
    let outcomes = CASES
        .iter()
        .map(|case| case.outcome)
        .collect::<BTreeSet<_>>();
    let required = BTreeSet::from([
        MappingOutcome::Accepted,
        MappingOutcome::Transformed,
        MappingOutcome::Unresolved,
        MappingOutcome::Skipped,
        MappingOutcome::Quarantined,
    ]);
    if outcomes != required {
        return Err("all five mapping outcomes must be represented".to_string());
    }

    for case in CASES {
        let path = root.join("source").join(case.path);
        validate_fixture_encoding(case, &path)?;
        let bytes = fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        validate_sanitized_bytes(case.path, &bytes)?;
        match case.outcome {
            MappingOutcome::Accepted | MappingOutcome::Transformed if case.expected.is_none() => {
                return Err(format!("{} requires an expected mapping", case.id));
            }
            MappingOutcome::Unresolved | MappingOutcome::Skipped | MappingOutcome::Quarantined
                if case.expected.is_some() =>
            {
                return Err(format!("{} must not fabricate a target mapping", case.id));
            }
            _ => {}
        }
    }

    for entry in &generated.expected_artifacts {
        let bytes = fs::read(root.join(&entry.relative_path))
            .map_err(|error| format!("read {}: {error}", entry.relative_path))?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| format!("parse {}: {error}", entry.relative_path))?;
        validate_sanitized_bytes(&entry.relative_path, &bytes)?;
    }
    validate_expected_mappings(&root, &generated)?;
    Ok(())
}

fn snapshot_entries(root: &Path, prefix: &str) -> Result<Vec<SnapshotEntry>, String> {
    let mut files = Vec::new();
    collect_regular_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
        .into_iter()
        .map(|(relative, path)| {
            let bytes = fs::read(&path)
                .map_err(|error| format!("read fixture {}: {error}", path.display()))?;
            Ok(SnapshotEntry {
                relative_path: format!("{prefix}/{relative}"),
                bytes: u64::try_from(bytes.len())
                    .map_err(|_| format!("fixture too large: {}", path.display()))?,
                sha256: sha256_hex(&bytes),
            })
        })
        .collect()
}

fn collect_regular_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(current)
        .map_err(|error| format!("read fixture directory {}: {error}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read fixture directory entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect fixture {}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "fixture symlink is forbidden: {}",
                entry.path().display()
            ));
        }
        if file_type.is_dir() {
            collect_regular_files(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| "fixture escaped corpus root".to_string())?
                .components()
                .map(|component| match component {
                    Component::Normal(value) => value
                        .to_str()
                        .map(str::to_string)
                        .ok_or_else(|| "fixture path is not UTF-8".to_string()),
                    _ => Err("fixture path is not a portable relative path".to_string()),
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            files.push((relative, entry.path()));
        } else {
            return Err(format!(
                "fixture is not a regular file: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn aggregate_digest(entries: &[SnapshotEntry]) -> String {
    let mut digest = Sha256::new();
    for entry in entries {
        digest.update(entry.relative_path.as_bytes());
        digest.update([0]);
        digest.update(entry.bytes.to_string().as_bytes());
        digest.update([0]);
        digest.update(entry.sha256.as_bytes());
        digest.update(b"\n");
    }
    let digest = digest.finalize();
    format!("sha256:{}", hex_digest(&digest))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_relative_entries(entries: &[SnapshotEntry]) -> Result<(), String> {
    let mut previous = None;
    for entry in entries {
        let path = Path::new(&entry.relative_path);
        if path.is_absolute()
            || entry.relative_path.contains('\\')
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!(
                "non-portable fixture path: {}",
                entry.relative_path
            ));
        }
        if previous.is_some_and(|value: &str| value >= entry.relative_path.as_str()) {
            return Err("snapshot entries must be strictly path-sorted".to_string());
        }
        previous = Some(entry.relative_path.as_str());
        if entry.sha256.len() != 64 || !entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("invalid fixture digest: {}", entry.relative_path));
        }
    }
    Ok(())
}

fn validate_fixture_encoding(case: &CaseSpec, path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("fixture {} is not UTF-8: {error}", case.id))?;
    match case.encoding {
        FixtureEncoding::Json => {
            serde_json::from_str::<serde_json::Value>(text)
                .map_err(|error| format!("fixture {} is invalid JSON: {error}", case.id))?;
        }
        FixtureEncoding::JsonLines => {
            let lines = nonempty_lines(text).collect::<Vec<_>>();
            if lines.is_empty() {
                return Err(format!("fixture {} has no JSONL entries", case.id));
            }
            for line in lines {
                serde_json::from_str::<serde_json::Value>(line)
                    .map_err(|error| format!("fixture {} has invalid JSONL: {error}", case.id))?;
            }
        }
        FixtureEncoding::TolerantJsonLines => {
            let mut valid = 0usize;
            let mut invalid = 0usize;
            for line in nonempty_lines(text) {
                if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                    valid += 1;
                } else {
                    invalid += 1;
                }
            }
            if valid == 0 || invalid == 0 {
                return Err(format!(
                    "fixture {} must exercise Bamboo's tolerant JSONL parser",
                    case.id
                ));
            }
        }
        FixtureEncoding::Markdown => {
            if text.trim().is_empty() {
                return Err(format!("fixture {} is empty", case.id));
            }
        }
        FixtureEncoding::MarkdownFrontmatter => {
            let (frontmatter, body) = split_frontmatter(text)?;
            if body.trim().is_empty() {
                return Err(format!("fixture {} has an empty body", case.id));
            }
            if case.kind == ArtifactKind::DurableTopic {
                serde_yaml_ng::from_str::<BambooDurableFrontmatter>(frontmatter).map_err(
                    |error| {
                        format!(
                            "fixture {} has invalid durable frontmatter: {error}",
                            case.id
                        )
                    },
                )?;
            } else {
                let value = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(frontmatter)
                    .map_err(|error| format!("fixture {} has invalid YAML: {error}", case.id))?;
                let mapping = value
                    .as_mapping()
                    .ok_or_else(|| format!("fixture {} frontmatter is not a map", case.id))?;
                for required in ["id", "title", "created_at", "updated_at"] {
                    if !mapping.contains_key(serde_yaml_ng::Value::String(required.to_string())) {
                        return Err(format!("fixture {} is missing {required}", case.id));
                    }
                }
            }
        }
        FixtureEncoding::InvalidMarkdownFrontmatter => {
            let (frontmatter, _) = split_frontmatter(text)?;
            if serde_yaml_ng::from_str::<BambooDurableFrontmatter>(frontmatter).is_ok() {
                return Err(format!("fixture {} must remain invalid", case.id));
            }
        }
    }
    Ok(())
}

fn nonempty_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn split_frontmatter(text: &str) -> Result<(&str, &str), String> {
    let text = text.trim_start_matches('\u{feff}');
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| "missing frontmatter start marker".to_string())?;
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| "missing frontmatter end marker".to_string())?;
    Ok((&rest[..end], &rest[end + 5..]))
}

fn validate_sanitized_bytes(label: &str, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("fixture {label} is not UTF-8: {error}"))?;
    for forbidden in [
        "/Users/",
        "/home/",
        "\\Users\\",
        "Authorization:",
        "Bearer ",
        "api_key",
        "apiKey",
        "credential_ref",
        "sk-live-",
        "sk-proj-",
    ] {
        if text.contains(forbidden) {
            return Err(format!(
                "fixture {label} contains forbidden data marker {forbidden}"
            ));
        }
    }
    Ok(())
}

fn validate_expected_mappings(root: &Path, manifest: &CorpusManifest) -> Result<(), String> {
    let identity_bytes = fs::read(root.join("expected/host-identity-map.json"))
        .map_err(|error| format!("read host identity mapping: {error}"))?;
    let identity: HostIdentityMap = serde_json::from_slice(&identity_bytes)
        .map_err(|error| format!("decode host identity mapping: {error}"))?;
    identity.validate()?;

    let expected_paths = manifest
        .cases
        .iter()
        .filter_map(|case| case.expected_mapping.as_deref())
        .collect::<BTreeSet<_>>();
    let actual_paths = manifest
        .expected_artifacts
        .iter()
        .map(|entry| entry.relative_path.as_str())
        .filter(|path| *path != "expected/host-identity-map.json")
        .collect::<BTreeSet<_>>();
    if expected_paths != actual_paths {
        return Err("expected mapping files and case references differ".to_string());
    }

    let cases_by_id = manifest
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let contradiction_source = cases_by_id
        .get("durable-project-current")
        .ok_or_else(|| "contradiction source case is missing".to_string())?;
    let contradiction_text = fs::read_to_string(
        root.join("source")
            .join(&contradiction_source.source_relative_path),
    )
    .map_err(|error| format!("read contradiction source: {error}"))?;
    let (contradiction_frontmatter, _) = split_frontmatter(&contradiction_text)?;
    let contradiction: BambooDurableFrontmatter =
        serde_yaml_ng::from_str(contradiction_frontmatter)
            .map_err(|error| format!("parse contradiction source: {error}"))?;
    if contradiction.relations.contradicted_by != ["mem_fixture_global_user"] {
        return Err("fixture must preserve Bamboo contradicted_by source direction".to_string());
    }
    for relative in expected_paths {
        let bytes = fs::read(root.join(relative))
            .map_err(|error| format!("read expected mapping {relative}: {error}"))?;
        let expected: ExpectedMapping = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode expected mapping {relative}: {error}"))?;
        if expected.mapping_schema != MAPPING_SCHEMA {
            return Err(format!("unexpected mapping schema in {relative}"));
        }
        let case = cases_by_id
            .get(expected.source_case.as_str())
            .ok_or_else(|| format!("mapping {relative} names an unknown source case"))?;
        if case.expected_mapping.as_deref() != Some(relative) {
            return Err(format!(
                "mapping {relative} is bound to the wrong source case"
            ));
        }
        if expected.generated_fields != ["revision", "etag"] {
            return Err(format!("mapping {relative} must defer revision and etag"));
        }
        if expected.source_case == "durable-global-user-transformed"
            && expected.mapping_policy.as_deref() != Some("explicit_user_type:preference")
        {
            return Err("Bamboo user conversion requires an explicit mapping policy".to_string());
        }
        let source_entry = manifest
            .source_snapshot
            .entries
            .iter()
            .find(|entry| entry.relative_path == format!("source/{}", case.source_relative_path))
            .ok_or_else(|| format!("mapping {relative} has no source snapshot entry"))?;
        let expected_uri = format!("bamboo-memory://snapshot-v1/{}", case.source_relative_path);
        let actual_uri = expected
            .record
            .provenance
            .source_uri
            .as_ref()
            .ok_or_else(|| format!("mapping {relative} lacks sourceUri"))?;
        if actual_uri.as_str() != expected_uri {
            return Err(format!(
                "mapping {relative} sourceUri is not bound to its source case"
            ));
        }
        let actual_digest = expected
            .record
            .provenance
            .content_digest
            .as_ref()
            .ok_or_else(|| format!("mapping {relative} lacks contentDigest"))?;
        if actual_digest.as_str() != format!("sha256:{}", source_entry.sha256) {
            return Err(format!(
                "mapping {relative} contentDigest does not match source bytes"
            ));
        }
        let record = expected.record.into_validated_record()?;
        if record.summary.is_some() {
            return Err(format!(
                "mapping {relative} must not fabricate a Bamboo summary"
            ));
        }
        let expected_confidence = match expected.source_case.as_str() {
            "durable-project-current" => Some(1.0),
            "durable-global-user-transformed" => Some(0.5),
            "durable-global-feedback" | "durable-project-legacy-pre-granularity" => None,
            source_case => {
                return Err(format!(
                    "mapping {relative} has no frozen confidence policy for {source_case}"
                ));
            }
        };
        if record.provenance.confidence.map(|value| value.get()) != expected_confidence {
            return Err(format!(
                "mapping {relative} does not match its frozen confidence policy"
            ));
        }
        if expected.source_case == "durable-global-user-transformed"
            && !record.relations.iter().any(|relation| {
                relation.kind == RelationKind::Contradicts
                    && relation.target_memory_id.as_str() == "mem_fixture_project_current"
            })
        {
            return Err("contradicted_by must reverse into a Jiandu contradicts edge".to_string());
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExpectedMapping {
    mapping_schema: String,
    source_case: String,
    #[serde(default)]
    mapping_policy: Option<String>,
    generated_fields: Vec<String>,
    record: MappedRecord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MappedRecord {
    schema: MemorySchema,
    id: MemoryId,
    scope: MemoryScope,
    #[serde(rename = "type")]
    memory_type: MemoryType,
    status: MemoryStatus,
    title: String,
    summary: Option<String>,
    body: String,
    tags: Vec<Tag>,
    created_at: Timestamp,
    updated_at: Timestamp,
    provenance: Provenance,
    relations: Vec<MemoryRelation>,
}

impl MappedRecord {
    fn into_validated_record(self) -> Result<MemoryRecord, String> {
        let record = MemoryRecord {
            schema: self.schema,
            id: self.id,
            revision: Revision::new(1).map_err(|error| error.to_string())?,
            etag: Etag::new("fixture-etag").map_err(|error| error.to_string())?,
            scope: self.scope,
            memory_type: self.memory_type,
            status: self.status,
            title: self.title,
            summary: self.summary,
            body: self.body,
            tags: self.tags,
            created_at: self.created_at,
            updated_at: self.updated_at,
            provenance: self.provenance,
            relations: self.relations,
        };
        record
            .validate()
            .map_err(|error| format!("mapped Jiandu record is invalid: {error}"))?;
        Ok(record)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostIdentityMap {
    mapping_schema: String,
    principal_slots: Vec<PrincipalSlot>,
    projects: Vec<ProjectBinding>,
    sessions: Vec<SessionBinding>,
    record_type_overrides: Vec<RecordTypeOverride>,
    created_by_mappings: Vec<CreatedByMapping>,
}

impl HostIdentityMap {
    fn validate(&self) -> Result<(), String> {
        if self.mapping_schema != IDENTITY_MAP_SCHEMA {
            return Err("unexpected host identity-map schema".to_string());
        }
        if self.principal_slots.len() != 1
            || self.projects.len() != 1
            || self.sessions.len() != 1
            || self.record_type_overrides.len() != 1
            || self.created_by_mappings.len() != 2
        {
            return Err(
                "fixture host mapping must keep one explicit identity of each kind".to_string(),
            );
        }
        if self.principal_slots[0].source != "bamboo-single-user-global"
            || self.principal_slots[0].target_principal_id != "prn_fixture_owner"
            || self.projects[0].bamboo_project_id != "01JFIXTUREPROJECT0000000001"
            || self.projects[0].declared_legacy_project_keys != ["fixture-workspace-deadbeef"]
            || self.projects[0].target_project_id != "prj_fixture_alpha"
            || self.sessions[0].bamboo_session_id != "sesfixture1"
            || self.sessions[0].target_session_id != "ses_fixture_1"
            || self.record_type_overrides[0].source_memory_id != "mem_fixture_global_user"
            || self.record_type_overrides[0].target_type != "preference"
            || self.record_type_overrides[0].reason != "operator-classified fixture preference"
            || self.created_by_mappings[0].bamboo_kind != "user"
            || self.created_by_mappings[0].target_actor != "import"
            || self.created_by_mappings[0].requires_session_binding
            || !self.created_by_mappings[0].preserve_as_migration_evidence
            || self.created_by_mappings[1].bamboo_kind != "session"
            || self.created_by_mappings[1].target_actor != "import"
            || !self.created_by_mappings[1].requires_session_binding
            || !self.created_by_mappings[1].preserve_as_migration_evidence
        {
            return Err("fixture host identity mapping drifted".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrincipalSlot {
    source: String,
    target_principal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectBinding {
    bamboo_project_id: String,
    declared_legacy_project_keys: Vec<String>,
    target_project_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionBinding {
    bamboo_session_id: String,
    target_session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordTypeOverride {
    source_memory_id: String,
    target_type: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatedByMapping {
    bamboo_kind: String,
    target_actor: String,
    #[serde(default)]
    requires_session_binding: bool,
    preserve_as_migration_evidence: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BambooDurableFrontmatter {
    id: String,
    title: String,
    #[serde(rename = "type")]
    memory_type: BambooMemoryType,
    scope: BambooMemoryScope,
    #[serde(default)]
    project_key: Option<String>,
    #[serde(default)]
    granularity: Option<BambooGranularity>,
    status: BambooMemoryStatus,
    #[serde(default)]
    freshness: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    created_at: String,
    updated_at: String,
    created_by: BambooActor,
    updated_by: BambooActor,
    #[serde(default)]
    sources: Vec<BambooSource>,
    #[serde(default)]
    relations: BambooRelations,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    retrieval: BambooRetrieval,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BambooMemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BambooMemoryScope {
    Session,
    Project,
    Global,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BambooGranularity {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BambooMemoryStatus {
    Active,
    Stale,
    Superseded,
    Contradicted,
    Archived,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct BambooActor {
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    actor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct BambooSource {
    kind: String,
    id: String,
    #[serde(default)]
    message_range: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct BambooRelations {
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    contradicted_by: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct BambooRetrieval {
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    embedding_ready: bool,
    #[serde(default)]
    last_accessed_at: Option<String>,
}
